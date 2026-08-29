//! The pointer graph, in the city.
//!
//! It costs a full read of the resident set, so it follows the duplicate finder's precedent
//! exactly: an explicit action on a key, run on a worker thread, result dropped in when ready,
//! nothing blocks. `MemoryReader` is what makes that possible — `process_vm_readv` is checked
//! against the calling process rather than the tracer thread, so the worker can read while the
//! main thread keeps the target.
//!
//! Edges are drawn per selection, never all at once: an aggregate of every mapping-to-mapping
//! edge is still hundreds of lines, and PLAN.md §6's rule is that a picture nobody can read is
//! not information.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use bevy::prelude::*;
use traveler_core::{NodeId, Tree, thousands};
use traveler_proc::{MemoryReader, Pointers, ScanLimits, pointers};

use crate::pin::Pinned;
use crate::scene::SceneView;
use crate::scene::Target;
use crate::space::{Space, Traced};

/// How many edges are drawn when nothing is pinned. Enough to read the shape, few enough to read.
const OVERVIEW_EDGES: usize = 24;

pub const POINTER_INK: Color = Color::srgb(0.98, 0.82, 0.25);

#[derive(Resource, Default)]
pub enum Scan {
    #[default]
    Idle,
    Running {
        slot: Arc<Mutex<Option<Pointers>>>,
        since: Instant,
    },
    Done(Box<Pointers>),
}

impl Scan {
    pub fn summary(&self) -> String {
        match self {
            Scan::Idle => "G scans for pointers".into(),
            Scan::Running { since, .. } => {
                format!(
                    "scanning for pointers... {:.1}s",
                    since.elapsed().as_secs_f64()
                )
            }
            Scan::Done(found) => format!(
                "{} pointer edges, {} candidates, {:.0}x chance",
                thousands(found.edges.len() as u64),
                thousands(found.found),
                found.hit_rate() / found.chance_rate().max(f64::MIN_POSITIVE)
            ),
        }
    }
}

pub fn input(
    keys: Res<ButtonInput<KeyCode>>,
    traced: Res<Traced>,
    space: Res<Space>,
    mut scan: ResMut<Scan>,
) {
    if !keys.just_pressed(KeyCode::KeyG) || matches!(*scan, Scan::Running { .. }) {
        return;
    }
    // A diff renders a union frame, where an extent is not any one capture's mapping, so there is
    // nothing coherent to scan against.
    let Some(capture) = space.pages() else {
        return;
    };
    let reader = MemoryReader::new(traced.0.pid());
    let capture = capture.clone();
    let slot: Arc<Mutex<Option<Pointers>>> = Arc::new(Mutex::new(None));
    let sink = slot.clone();
    std::thread::spawn(move || {
        let found = pointers::scan(&reader, &capture, &ScanLimits::default());
        if let Ok(mut guard) = sink.lock() {
            *guard = Some(found);
        }
    });
    *scan = Scan::Running {
        slot,
        since: Instant::now(),
    };
}

pub fn poll(mut scan: ResMut<Scan>) {
    let arrived = match &*scan {
        Scan::Running { slot, .. } => slot.lock().ok().and_then(|mut g| g.take()),
        _ => None,
    };
    if let Some(found) = arrived {
        println!(
            "  {} pointer edges over {} words in {:.0} ms, {:.0}x chance",
            found.edges.len(),
            thousands(found.words),
            found.elapsed.as_secs_f64() * 1000.0,
            found.hit_rate() / found.chance_rate().max(f64::MIN_POSITIVE)
        );
        *scan = Scan::Done(Box::new(found));
    }
}

/// One line per edge, from the source mapping to the target. Pinning a mapping drills into it and
/// shows only what it points at, which is where the count is bounded by the selection.
pub fn draw(scan: Res<Scan>, space: Res<Space>, pinned: Res<Pinned>, mut gizmos: Gizmos) {
    let Scan::Done(found) = &*scan else {
        return;
    };
    let places = &space.placements;
    let at = |id: NodeId| places.center[id.index()];

    let selected = pinned.target.map(Target::node);
    let edges: Vec<_> = match selected {
        Some(from) if space.frame.child_count(from) == 0 => found.out_of(from).collect(),
        _ => found.edges.iter().take(OVERVIEW_EDGES).collect(),
    };
    let Some(heaviest) = edges.iter().map(|e| e.count).max().filter(|c| *c > 0) else {
        return;
    };

    for edge in edges {
        if !places.is_visible(edge.from) || !places.is_visible(edge.to) {
            continue;
        }
        // Weight rides the alpha channel, so the heavy edges read through the light ones rather
        // than every edge shouting equally.
        let weight = (edge.count as f32 / heaviest as f32).powf(0.4);
        let lift = space.world_size() * 0.04 * weight;
        let (from, to) = (at(edge.from), at(edge.to));
        let arc = (from + to) * 0.5 + Vec3::Y * lift;
        // Two segments through a lifted midpoint, so an edge between neighbours is still visible
        // above the blocks rather than buried in them.
        gizmos.line(from, arc, POINTER_INK.with_alpha(0.15 + 0.5 * weight));
        gizmos.line(arc, to, POINTER_INK.with_alpha(0.15 + 0.5 * weight));
    }
}
