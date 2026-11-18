//! Which (level, tile) meshes are on screen.
//!
//! Refines from the coarsest level down, like a map: a tile splits into its two halves at the
//! next finer level while one of its slices would still cover more than `px` pixels. If the
//! result is over budget, `px` is raised until it fits, so far-away time gets coarser first.
//!
//! The budget follows the frame time. Frame cost on the M1 Air tracked spans drawn almost
//! linearly (33k spans 7 ms, 75k spans 14 ms with spikes past 40) and not tile count or bloom, so
//! a fixed budget is wrong on every machine but one and wrong again when the window is resized.
//!
//! Tile geometry is built on the async compute pool, once, and cached. A single coarse tile of a
//! long recording took 13.6 ms on the main thread, which is a dropped frame on every first sight.
//! Until a wanted tile arrives, whatever was on screen over the same stretch of time stays up.

use std::collections::{HashMap, HashSet};

use bevy::asset::RenderAssetUsages;
use bevy::mesh::PrimitiveTopology;
use bevy::prelude::*;
use bevy::tasks::{AsyncComputeTaskPool, Task, futures::check_ready};

use crate::scene::SceneRef;

const CACHE: usize = 900;
const MAX_IN_FLIGHT: usize = 6;
/// Vertices uploaded per frame. Unbounded, finished tiles land together and the upload is the hitch.
const UPLOAD_PER_FRAME: usize = 60_000;
const FOV_Y: f32 = std::f32::consts::FRAC_PI_4;
const MIN_BUDGET: usize = 8_000;
/// Shrink above this smoothed frame time, grow below the lower one. Both leave room under 16.7 ms.
const SLOW_MS: f32 = 10.5;
const FAST_MS: f32 = 7.0;
const SETTLE_FRAMES: u32 = 20;
const HOLD_FRAMES: u32 = 600;

type Key = (u8, u32);

struct Entry {
    entity: Entity,
    mesh: Handle<Mesh>,
    used: u64,
    spans: usize,
    /// Kept for picking: line endpoints, and the span each line belongs to.
    pos: Vec<[f32; 3]>,
    segs: Vec<u32>,
}

struct TileData {
    pos: Vec<[f32; 3]>,
    col: Vec<[f32; 4]>,
    segs: Vec<u32>,
    spans: usize,
}

/// A span at a zoom level: what the mouse is on.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Pick {
    pub level: u8,
    pub span: u32,
}

#[derive(Resource)]
pub struct Tiles {
    pub budget: usize,
    pub max_budget: usize,
    frame_ms: f32,
    since_change: u32,
    cut_from: Option<(usize, f32)>,
    floor: usize,
    hold: u32,
    pub px: f32,
    pub used_px: f32,
    pub drawn: usize,
    pub shown: usize,
    pub built: usize,
    pub in_flight: usize,
    /// Bumped whenever the set of tiles on screen changes, so overlays know to rebuild.
    pub version: u64,
    map: HashMap<Key, Entry>,
    building: HashMap<Key, Task<TileData>>,
    ready: Vec<(Key, TileData)>,
    visible: HashSet<Key>,
    material: Option<Handle<StandardMaterial>>,
    frame: u64,
    last_view: Option<(Vec3, Vec3, f32)>,
    want: Vec<Key>,
}

impl Tiles {
    pub fn new(max_budget: usize, px: f32) -> Self {
        Self {
            budget: max_budget.min(40_000).max(MIN_BUDGET),
            max_budget: max_budget.max(MIN_BUDGET),
            frame_ms: 0.0,
            since_change: 0,
            cut_from: None,
            floor: MIN_BUDGET,
            hold: 0,
            px,
            used_px: px,
            drawn: 0,
            shown: 0,
            built: 0,
            in_flight: 0,
            version: 0,
            map: HashMap::new(),
            building: HashMap::new(),
            ready: Vec::new(),
            visible: HashSet::new(),
            material: None,
            frame: 0,
            last_view: None,
            want: Vec::new(),
        }
    }

    fn change_budget(&mut self, budget: usize) {
        if budget != self.budget {
            self.budget = budget.clamp(MIN_BUDGET, self.max_budget);
            self.since_change = 0;
            self.last_view = None;
        }
    }

    pub fn cached(&self) -> usize {
        self.map.len()
    }

    /// The line nearest `cursor` (logical pixels) within `radius`, among tiles on screen.
    pub fn pick(&self, view_proj: Mat4, viewport: Vec2, cursor: Vec2, radius: f32) -> Option<(Pick, f32)> {
        let to_screen = |p: &[f32; 3]| {
            let c = view_proj * Vec3::from_array(*p).extend(1.0);
            (c.w > 1e-4).then(|| Vec2::new((c.x / c.w + 1.0) * 0.5 * viewport.x, (1.0 - c.y / c.w) * 0.5 * viewport.y))
        };
        let mut best: Option<(f32, Pick)> = None;
        for key in &self.visible {
            let entry = &self.map[key];
            for (i, &span) in entry.segs.iter().enumerate() {
                let (Some(a), Some(b)) = (to_screen(&entry.pos[2 * i]), to_screen(&entry.pos[2 * i + 1])) else { continue };
                let ab = b - a;
                let t = ((cursor - a).dot(ab) / ab.length_squared().max(1e-6)).clamp(0.0, 1.0);
                let d = (a + ab * t).distance(cursor);
                if d <= radius && best.is_none_or(|(bd, _)| d < bd) {
                    best = Some((d, Pick { level: key.0, span }));
                }
            }
        }
        best.map(|(d, p)| (p, d))
    }

    /// Line endpoints on screen whose span passes `keep`, for highlighting.
    pub fn visible_lines(&self, mut keep: impl FnMut(u8, u32) -> bool) -> Vec<[f32; 3]> {
        let mut out = Vec::new();
        for key in &self.visible {
            let entry = &self.map[key];
            for (i, &span) in entry.segs.iter().enumerate() {
                if keep(key.0, span) {
                    out.extend_from_slice(&entry.pos[2 * i..2 * i + 2]);
                }
            }
        }
        out
    }
}

pub fn adapt_budget(time: Res<Time<Real>>, mut tiles: ResMut<Tiles>) {
    let ms = time.delta_secs() * 1000.0;
    tiles.frame_ms = if tiles.frame_ms == 0.0 { ms } else { tiles.frame_ms * 0.9 + ms * 0.1 };
    tiles.since_change += 1;
    tiles.hold = tiles.hold.saturating_sub(1);
    if tiles.since_change < SETTLE_FRAMES {
        return;
    }
    // A cut that did not make frames faster means the time goes somewhere else (another app, the
    // display, the window size): put the detail back and stop cutting for a while.
    if let Some((budget, ms)) = tiles.cut_from.take()
        && tiles.frame_ms > ms * 0.93
    {
        tiles.floor = budget;
        tiles.hold = HOLD_FRAMES;
        tiles.change_budget(budget);
        return;
    }
    if tiles.hold == 0 {
        tiles.floor = MIN_BUDGET;
    }
    let limited = tiles.used_px > tiles.px;
    if tiles.frame_ms > SLOW_MS && tiles.budget > tiles.floor {
        tiles.cut_from = Some((tiles.budget, tiles.frame_ms));
        let next = ((tiles.budget as f32 * 0.8) as usize).max(tiles.floor);
        tiles.change_budget(next);
    } else if tiles.frame_ms < FAST_MS && limited {
        let next = ((tiles.budget as f32 * 1.15) as usize).min(tiles.max_budget);
        tiles.change_budget(next);
    }
}

#[allow(clippy::too_many_arguments)]
pub fn update_tiles(
    mut commands: Commands,
    scene: Res<SceneRef>,
    mut tiles: ResMut<Tiles>,
    camera: Query<(&GlobalTransform, &Camera), With<Camera3d>>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    let Ok((cam_tf, camera)) = camera.single() else { return };
    let Some(size) = camera.physical_target_size() else { return };
    let tiles = &mut *tiles;
    let material = tiles
        .material
        .get_or_insert_with(|| {
            materials.add(StandardMaterial { base_color: Color::WHITE, unlit: true, alpha_mode: AlphaMode::Add, ..default() })
        })
        .clone();
    tiles.frame += 1;
    let frame = tiles.frame;

    let done: Vec<(Key, TileData)> = tiles.building.iter_mut().filter_map(|(k, t)| check_ready(t).map(|d| (*k, d))).collect();
    for (k, _) in &done {
        tiles.building.remove(k);
    }
    tiles.ready.extend(done);
    let mut uploaded = 0;
    let mut finished = Vec::new();
    while uploaded < UPLOAD_PER_FRAME
        && let Some((k, d)) = tiles.ready.pop()
    {
        uploaded += d.pos.len();
        let mesh = meshes.add(
            Mesh::new(PrimitiveTopology::LineList, RenderAssetUsages::default())
                .with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, d.pos.clone())
                .with_inserted_attribute(Mesh::ATTRIBUTE_COLOR, d.col),
        );
        let entity = commands.spawn((Mesh3d(mesh.clone()), MeshMaterial3d(material.clone()), Visibility::Hidden)).id();
        tiles.map.insert(k, Entry { entity, mesh, used: frame, spans: d.spans, pos: d.pos, segs: d.segs });
        tiles.built += 1;
        finished.push(k);
    }

    let eye = cam_tf.translation();
    let fwd = cam_tf.forward().as_vec3();
    let height = size.y.max(1) as f32;
    let moved = match tiles.last_view {
        Some((e, f, h)) => e.distance(eye) > 0.002 * (1.0 + eye.length()) || f.dot(fwd) < 0.99995 || h != height,
        None => true,
    };
    if moved {
        tiles.last_view = Some((eye, fwd, height));
        let focal = height * 0.5 / (FOV_Y * 0.5).tan();
        let view_proj = camera.clip_from_view() * cam_tf.to_matrix().inverse();
        let mut px = tiles.px;
        let mut want = std::mem::take(&mut tiles.want);
        for _ in 0..12 {
            want.clear();
            let spans = select(&scene, eye, view_proj, focal, px, &mut want);
            if spans <= tiles.budget {
                break;
            }
            px *= 1.5;
        }
        tiles.used_px = px;
        tiles.want = want;
    }
    if !moved && finished.is_empty() && tiles.building.is_empty() && tiles.ready.is_empty() {
        return;
    }

    let pool = AsyncComputeTaskPool::get();
    let mut now_visible = HashSet::with_capacity(tiles.want.len());
    let mut missing: Vec<(f64, f64)> = Vec::new();
    for i in 0..tiles.want.len() {
        let key = tiles.want[i];
        if let Some(entry) = tiles.map.get_mut(&key) {
            entry.used = frame;
            now_visible.insert(key);
            continue;
        }
        missing.push(span_of(&scene, key));
        if !tiles.building.contains_key(&key)
            && !tiles.ready.iter().any(|(k, _)| *k == key)
            && tiles.building.len() < MAX_IN_FLIGHT
        {
            let scene = scene.clone();
            tiles.building.insert(key, pool.spawn(async move { build_tile(&scene, key) }));
        }
    }
    if !missing.is_empty() {
        for &key in &tiles.visible {
            let (a, b) = span_of(&scene, key);
            if !now_visible.contains(&key) && missing.iter().any(|&(c, d)| a < d && c < b) {
                now_visible.insert(key);
            }
        }
    }

    // Through commands rather than a Visibility query: a tile finished this frame was spawned by
    // a command too, so a query cannot see it yet and it would stay hidden for good.
    let mut drawn = 0;
    for key in &now_visible {
        let entry = &tiles.map[key];
        drawn += entry.spans;
        if !tiles.visible.contains(key) {
            commands.entity(entry.entity).insert(Visibility::Visible);
        }
    }
    for key in tiles.visible.difference(&now_visible) {
        if let Some(entry) = tiles.map.get(key) {
            commands.entity(entry.entity).insert(Visibility::Hidden);
        }
    }
    if tiles.visible != now_visible {
        tiles.version += 1;
    }
    tiles.visible = now_visible;
    tiles.drawn = drawn;
    tiles.shown = tiles.visible.len();
    tiles.in_flight = tiles.building.len();

    if tiles.map.len() > CACHE {
        let mut old: Vec<(u64, Key)> =
            tiles.map.iter().filter(|(k, _)| !tiles.visible.contains(k)).map(|(k, e)| (e.used, *k)).collect();
        old.sort_unstable();
        for (_, key) in old.into_iter().take(tiles.map.len() - CACHE) {
            if let Some(entry) = tiles.map.remove(&key) {
                commands.entity(entry.entity).despawn();
                meshes.remove(&entry.mesh);
            }
        }
    }
}

fn span_of(scene: &SceneRef, (l, j): Key) -> (f64, f64) {
    let c = scene.levels.levels[l as usize].chunk;
    (j as f64 * c, (j + 1) as f64 * c)
}

fn select(scene: &SceneRef, eye: Vec3, view_proj: Mat4, focal: f32, px: f32, out: &mut Vec<Key>) -> usize {
    let top = scene.levels.levels.len() - 1;
    let ext = scene.extent();
    let mut spans = 0;
    let mut stack: Vec<(usize, usize)> = (0..scene.levels.levels[top].buckets.len()).map(|j| (top, j)).collect();
    while let Some((l, j)) = stack.pop() {
        let level = &scene.levels.levels[l];
        let x0 = scene.x_of((j as f64 * level.chunk) as f32);
        let x1 = scene.x_of(((j + 1) as f64 * level.chunk).min(scene.trace.duration) as f32);
        let min = Vec3::new(x0, -ext, -ext);
        let max = Vec3::new(x1, ext, ext);
        if !in_frustum(view_proj, min, max) {
            continue;
        }
        let dist = eye.clamp(min, max).distance(eye).max(0.05);
        let slice_px = scene.slice_world(level.slice) * focal / dist;
        if slice_px > px && l > 0 {
            let finer = &scene.levels.levels[l - 1];
            for c in [2 * j, 2 * j + 1] {
                if c < finer.buckets.len() {
                    stack.push((l - 1, c));
                }
            }
        } else if !level.buckets[j].is_empty() {
            out.push((l as u8, j as u32));
            spans += level.buckets[j].len();
        }
    }
    spans
}

fn in_frustum(view_proj: Mat4, min: Vec3, max: Vec3) -> bool {
    let mut out = [true; 5];
    for i in 0..8 {
        let p = Vec3::new(
            if i & 1 == 0 { min.x } else { max.x },
            if i & 2 == 0 { min.y } else { max.y },
            if i & 4 == 0 { min.z } else { max.z },
        );
        let c = view_proj * p.extend(1.0);
        out[0] &= c.x < -c.w;
        out[1] &= c.x > c.w;
        out[2] &= c.y < -c.w;
        out[3] &= c.y > c.w;
        out[4] &= c.w <= 0.0;
    }
    !out.iter().any(|&o| o)
}

fn build_tile(scene: &SceneRef, (l, j): Key) -> TileData {
    let level = &scene.levels.levels[l as usize];
    let a = (j as f64 * level.chunk) as f32;
    let b = ((j + 1) as f64 * level.chunk) as f32;
    let bucket = &level.buckets[j as usize];
    let mut pos: Vec<[f32; 3]> = Vec::with_capacity(bucket.len() * 4);
    let mut col: Vec<[f32; 4]> = Vec::with_capacity(bucket.len() * 4);
    let mut segs: Vec<u32> = Vec::with_capacity(bucket.len() * 2);
    let slice = level.slice as f32;
    for &si in bucket {
        let sp = level.spans[si as usize];
        let t0 = sp.t0.max(a);
        let t1 = sp.t1.min(b);
        if t1 <= t0 {
            continue;
        }
        let th = sp.thread as usize;
        let own = scene.offset(th, sp.node);
        let c = scene.color(sp.node, sp.weight);
        if sp.t0 >= a {
            let parent = scene.trace.parent[sp.node as usize];
            let from = scene.lane_at(th, sp.t0) + scene.offset(th, parent);
            let ramp = ((sp.t1 - sp.t0) * 0.3).min(slice * 1.5);
            let knee = scene.lane_at(th, sp.t0 + ramp) + own;
            pos.extend_from_slice(&[from.to_array(), knee.to_array(), knee.to_array(), (scene.lane_at(th, t1) + own).to_array()]);
            col.extend_from_slice(&[c, c, c, c]);
            segs.extend_from_slice(&[si, si]);
        } else {
            pos.extend_from_slice(&[(scene.lane_at(th, t0) + own).to_array(), (scene.lane_at(th, t1) + own).to_array()]);
            col.extend_from_slice(&[c, c]);
            segs.push(si);
        }
    }
    TileData { pos, col, segs, spans: bucket.len() }
}
