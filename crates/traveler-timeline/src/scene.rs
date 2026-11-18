use bevy::asset::RenderAssetUsages;
use bevy::mesh::PrimitiveTopology;
use bevy::prelude::*;
use traveler_trace::describe::{Describer, thread_names};
use traveler_trace::story::{Phase, phases};
use traveler_trace::{Levels, ROOT, Trace};

/// World units for the whole run, whatever its length. Zoom does the rest.
const LENGTH: f32 = 1600.0;
const DEPTH_STEP: f32 = 3.2;
/// Depth grows linearly up to here, then by its square root. A hard cap at 16 stacked every
/// deeper call (a thread pool's recursion runs 30+ deep) at one radius into a solid wall.
const LINEAR_DEPTH: f32 = 10.0;
const LANE_STEP: f32 = 40.0;

fn radius(d: f32) -> f32 {
    let d = if d <= LINEAR_DEPTH { d } else { LINEAR_DEPTH + 1.5 * (d - LINEAR_DEPTH).sqrt() };
    DEPTH_STEP * d
}
/// How far a call turns from its caller toward its library's direction.
const LIBRARY_PULL: f32 = 0.3;
/// Angle between neighbouring calls with the same caller, and the most a fan may open.
const SIBLING_STEP: f32 = 0.16;
const MAX_FAN: f32 = 1.1;

fn wrap(a: f32) -> f32 {
    let t = std::f32::consts::TAU;
    (a + std::f32::consts::PI).rem_euclid(t) - std::f32::consts::PI
}

const PALETTE: [[f32; 3]; 7] = [
    [0.12, 0.75, 1.0],
    [0.55, 0.3, 1.0],
    [1.0, 0.22, 0.7],
    [0.2, 1.0, 0.7],
    [1.0, 0.55, 0.15],
    [0.95, 0.9, 0.3],
    [0.35, 0.45, 1.0],
];

/// Shared with the tile builders on the async compute pool.
#[derive(Resource, Clone, Deref)]
pub struct SceneRef(pub std::sync::Arc<Scene>);

pub struct Scene {
    pub trace: Trace,
    pub levels: Levels,
    lanes: Vec<Lane>,
    dirs: Vec<Vec2>,
    module_color: Vec<Vec3>,
    /// Libraries by share of samples, most first, with the colour each is drawn in.
    pub libraries: Vec<(String, Vec3, f32)>,
    pub describe: Describer,
    /// The run told in a few phases, for the story panel.
    pub phases: Vec<Phase>,
    /// What to call each thread on screen.
    pub names: Vec<String>,
    reach: f32,
    xs: f32,
}

/// Libraries past the palette share one dim colour, so a colour always names one library.
const OTHER: Vec3 = Vec3::new(0.42, 0.46, 0.56);

struct Lane {
    center: Vec2,
    start: f32,
    ease: f32,
}

impl Scene {
    pub fn new(trace: Trace, levels: Levels) -> Self {
        let xs = LENGTH / trace.duration as f32;

        let mut module_weight = vec![0u64; trace.modules.len()];
        for th in &trace.threads {
            for &leaf in &th.leaves {
                module_weight[trace.module_of(leaf) as usize] += 1;
            }
        }
        let mut order: Vec<usize> = (0..trace.modules.len()).collect();
        order.sort_by_key(|&m| std::cmp::Reverse(module_weight[m]));
        let n = order.len().max(1) as f32;
        let mut module_angle = vec![0.0f32; trace.modules.len()];
        let mut module_color = vec![Vec3::ONE; trace.modules.len()];
        for (rank, &m) in order.iter().enumerate() {
            module_angle[m] = std::f32::consts::FRAC_PI_2 + std::f32::consts::TAU * rank as f32 / n;
            module_color[m] = PALETTE.get(rank).map_or(OTHER, |&c| Vec3::from(c));
        }
        let total = module_weight.iter().sum::<u64>().max(1) as f32;
        let libraries = order
            .iter()
            .map(|&m| (trace.modules[m].clone(), module_color[m], module_weight[m] as f32 / total))
            .collect();
        // A call grows the way its caller grew, leaning a little toward its library's side, so
        // a chain of calls reads as one branch. Calls with the same caller fan out side by side
        // in a fixed order instead of landing on each other.
        let mut kids: Vec<Vec<u32>> = vec![Vec::new(); trace.parent.len()];
        for node in 1..trace.parent.len() {
            kids[trace.parent[node] as usize].push(node as u32);
        }
        let mut angle = vec![std::f32::consts::FRAC_PI_2; trace.parent.len()];
        let mut stack = vec![ROOT];
        while let Some(p) = stack.pop() {
            let mut ks = std::mem::take(&mut kids[p as usize]);
            ks.sort_by_key(|&c| trace.func[c as usize]);
            let fan = (SIBLING_STEP * (ks.len().saturating_sub(1)) as f32).min(MAX_FAN);
            for (i, &c) in ks.iter().enumerate() {
                let lib = module_angle[trace.module_of(c) as usize];
                let base = if p == ROOT { lib } else { angle[p as usize] + wrap(lib - angle[p as usize]) * LIBRARY_PULL };
                let off = if ks.len() > 1 { (i as f32 / (ks.len() - 1) as f32 - 0.5) * fan } else { 0.0 };
                angle[c as usize] = base + off;
                stack.push(c);
            }
        }
        let dirs = angle.iter().map(|a| Vec2::new(a.cos(), a.sin())).collect();

        // Lanes far enough apart that two threads' deepest calls do not run into each other.
        let deepest = trace
            .threads
            .iter()
            .flat_map(|th| {
                let depth = &trace.depth;
                th.leaves.iter().map(move |&l| depth[l as usize].saturating_sub(th.trunk))
            })
            .max()
            .unwrap_or(0) as f32;
        let reach = radius(deepest);
        let lane_step = LANE_STEP.max(reach * 1.1);
        let mut lanes = Vec::with_capacity(trace.threads.len());
        for (i, th) in trace.threads.iter().enumerate() {
            let center = if i == 0 {
                Vec2::ZERO
            } else {
                let a = i as f32 * 2.399_963;
                Vec2::new(a.cos(), a.sin()) * lane_step * (1.0 + (i as f32).sqrt())
            };
            let ease = (center.length() * 2.5 / xs).max(trace.duration as f32 * 0.004);
            lanes.push(Lane { center, start: th.start as f32, ease });
        }
        let describe = Describer::new(&trace);
        let phases = phases(&trace, &describe);
        let names = thread_names(&trace);
        Scene { trace, levels, lanes, dirs, module_color, libraries, describe, phases, names, reach, xs }
    }

    pub fn length(&self) -> f32 {
        LENGTH
    }

    pub fn x_of(&self, t: f32) -> f32 {
        t * self.xs
    }

    pub fn time_at_x(&self, x: f32) -> f32 {
        x / self.xs
    }

    pub fn strand_radius(&self) -> f32 {
        0.45
    }

    /// Furthest any tendril reaches from the time axis.
    pub fn extent(&self) -> f32 {
        let lanes = self.lanes.iter().map(|l| l.center.length()).fold(0.0, f32::max);
        lanes + self.reach + 2.0
    }

    pub fn slice_world(&self, slice: f64) -> f32 {
        slice as f32 * self.xs
    }

    pub fn lane_at(&self, thread: usize, t: f32) -> Vec3 {
        let lane = &self.lanes[thread];
        let k = ((t - lane.start) / lane.ease).clamp(0.0, 1.0);
        let k = k * k * (3.0 - 2.0 * k);
        let yz = lane.center * k;
        Vec3::new(t * self.xs, yz.x, yz.y)
    }

    /// Where a thread's label sits: just after its strand has branched out to its own lane.
    pub fn label_anchor(&self, thread: usize) -> Vec3 {
        let lane = &self.lanes[thread];
        self.lane_at(thread, lane.start + lane.ease)
    }

    pub fn offset(&self, thread: usize, node: u32) -> Vec3 {
        let trunk = self.trace.threads[thread].trunk;
        let d = self.trace.depth[node as usize].saturating_sub(trunk) as f32;
        if d == 0.0 {
            return Vec3::ZERO;
        }
        let r = self.dirs[node as usize] * radius(d);
        Vec3::new(0.0, r.x, r.y)
    }

    pub fn color(&self, node: u32, weight: f32) -> [f32; 4] {
        let c = self.module_color[self.trace.module_of(node) as usize] * (0.06 + 1.25 * weight.sqrt());
        [c.x, c.y, c.z, 1.0]
    }

    /// The strand's centre line: dense through the branch-off curve, then straight.
    pub fn strand_points(&self, thread: usize) -> Vec<Vec3> {
        let lane = &self.lanes[thread];
        let th = &self.trace.threads[thread];
        let (s, e) = (th.start as f32, th.end as f32);
        let mut pts = Vec::new();
        for i in 0..=24 {
            let t = s + lane.ease * i as f32 / 24.0;
            if t < e {
                pts.push(self.lane_at(thread, t));
            }
        }
        pts.push(self.lane_at(thread, e));
        pts
    }

    pub fn strand_mesh(&self, thread: usize) -> Mesh {
        tube(&self.strand_points(thread), if thread == 0 { self.strand_radius() } else { self.strand_radius() * 0.7 })
    }

    pub fn ring_radius(&self) -> f32 {
        self.extent() + 8.0
    }
}

fn tube(pts: &[Vec3], r: f32) -> Mesh {
    const SIDES: usize = 6;
    let mut pos = Vec::with_capacity(pts.len() * SIDES);
    let mut idx = Vec::new();
    for (i, &p) in pts.iter().enumerate() {
        let next = pts.get(i + 1).copied().unwrap_or(p);
        let prev = if i > 0 { pts[i - 1] } else { p };
        let dir = (next - prev).normalize_or(Vec3::X);
        let a = dir.any_orthonormal_vector();
        let b = dir.cross(a);
        for k in 0..SIDES {
            let ang = std::f32::consts::TAU * k as f32 / SIDES as f32;
            pos.push((p + (a * ang.cos() + b * ang.sin()) * r).to_array());
        }
        if i + 1 < pts.len() {
            let base = (i * SIDES) as u32;
            for k in 0..SIDES as u32 {
                let k2 = (k + 1) % SIDES as u32;
                idx.extend_from_slice(&[base + k, base + SIDES as u32 + k, base + k2]);
                idx.extend_from_slice(&[base + k2, base + SIDES as u32 + k, base + SIDES as u32 + k2]);
            }
        }
    }
    Mesh::new(PrimitiveTopology::TriangleList, RenderAssetUsages::default())
        .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, pos)
        .with_inserted_indices(bevy::mesh::Indices::U32(idx))
}
