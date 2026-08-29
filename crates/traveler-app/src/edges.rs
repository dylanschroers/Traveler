//! Groups in the city: pick one and see where its members are.
//!
//! The two providers that cost nothing run before the window opens. The duplicate finder reads
//! file contents, which on a spinning disk is one seek per candidate and can take a minute on a
//! build tree, so it runs on a worker thread and drops its result in when it is ready. Nothing
//! blocks.
//!
//! A selected group is drawn two ways at once: its members change color in place, which shows
//! *where* they are, and the satellite cluster lifts them out of the city arranged by folder,
//! which shows how they relate to each other. Neither draws a line between two members, because a
//! group of n members implies n^2 of them.

use std::sync::{Arc, Mutex};
use std::time::Instant;

use bevy::prelude::*;
use traveler_core::NodeId;
use traveler_edges::{EdgeProvider, EdgeSet, ExactDuplicates};

use crate::palette::Category;
use crate::pin::Pinned;
use crate::{BlockMaterials, City, FileNode};

/// Recoloring is one component write per member, so a group holding every `.rs` file on a drive
/// is capped rather than allowed to stall a frame.
const MAX_HIGHLIGHT: usize = 20_000;

/// A held selection, distinct from every category hue and from the group highlight. Not part of
/// the categorical encoding: it means "the thing you just clicked", so it only has to stand apart
/// from its neighbours, not carry an identity of its own.
pub const PINNED: Color = Color::srgb(0.82, 0.42, 0.95);

pub enum DupeState {
    Idle,
    Running {
        slot: Arc<Mutex<Option<EdgeSet>>>,
        since: Instant,
    },
    Done,
}

#[derive(Resource)]
pub struct Edges {
    pub sets: Vec<EdgeSet>,
    pub active: usize,
    pub selected: Option<usize>,
    pub dupes: DupeState,
    /// What is currently recolored, so it can be put back.
    painted: Vec<NodeId>,
    applied: (Option<(usize, usize)>, u32),
}

impl Edges {
    pub fn new(sets: Vec<EdgeSet>) -> Self {
        Self {
            sets,
            active: 0,
            selected: None,
            dupes: DupeState::Idle,
            painted: Vec::new(),
            applied: (None, u32::MAX),
        }
    }

    /// Hands the duplicate finder to a worker thread. Nothing about the app waits on it: the
    /// result is picked up by `poll_dupes` whenever it lands.
    pub fn start_dupe_scan(&mut self, index: Arc<traveler_scan::FileIndex>) {
        if !matches!(self.dupes, DupeState::Idle) {
            return;
        }
        let slot: Arc<Mutex<Option<EdgeSet>>> = Arc::new(Mutex::new(None));
        let sink = slot.clone();
        std::thread::spawn(move || {
            let set = ExactDuplicates::default().compute(&index);
            if let Ok(mut guard) = sink.lock() {
                *guard = Some(set);
            }
        });
        self.dupes = DupeState::Running {
            slot,
            since: Instant::now(),
        };
    }

    pub fn active_set(&self) -> Option<&EdgeSet> {
        self.sets.get(self.active)
    }

    pub fn selected_group(&self) -> Option<&traveler_edges::Group> {
        let set = self.active_set()?;
        set.groups.get(self.selected?)
    }
}

pub fn edge_input(keys: Res<ButtonInput<KeyCode>>, mut edges: ResMut<Edges>, city: Res<City>) {
    if edges.sets.is_empty() {
        return;
    }

    if keys.just_pressed(KeyCode::Tab) {
        edges.active = (edges.active + 1) % edges.sets.len();
        edges.selected = None;
    }
    if keys.just_pressed(KeyCode::Escape) {
        edges.selected = None;
    }

    let count = edges.active_set().map(|s| s.groups.len()).unwrap_or(0);
    if count > 0 {
        let step = if keys.pressed(KeyCode::ShiftLeft) {
            10
        } else {
            1
        };
        if keys.just_pressed(KeyCode::ArrowDown) {
            edges.selected = Some(match edges.selected {
                Some(i) => (i + step).min(count - 1),
                None => 0,
            });
        }
        if keys.just_pressed(KeyCode::ArrowUp) {
            edges.selected = match edges.selected {
                Some(i) if i >= step => Some(i - step),
                Some(_) => Some(0),
                None => Some(0),
            };
        }
    }

    // The duplicate finder reads the disk, so it starts only when asked.
    if keys.just_pressed(KeyCode::KeyD) {
        let index = city.index.clone();
        edges.start_dupe_scan(index);
    }
}

pub fn poll_dupes(mut edges: ResMut<Edges>) {
    let arrived = match &edges.dupes {
        DupeState::Running { slot, .. } => slot.lock().ok().and_then(|mut g| g.take()),
        _ => None,
    };
    if let Some(set) = arrived {
        edges.sets.push(set);
        edges.active = edges.sets.len() - 1;
        edges.selected = None;
        edges.dupes = DupeState::Done;
    }
}

/// Repaints only when something actually changed.
///
/// One system owns both overlays because they overlap: a pinned folder can contain members of the
/// selected group. Precedence is pin over group over category, which matches what the click meant.
pub fn paint_selection(
    mut edges: ResMut<Edges>,
    pinned: Res<Pinned>,
    city: Res<City>,
    materials: Res<BlockMaterials>,
    mut blocks: Query<&mut MeshMaterial3d<StandardMaterial>, With<FileNode>>,
) {
    let want = (edges.selected.map(|s| (edges.active, s)), pinned.generation);
    if want == edges.applied {
        return;
    }
    edges.applied = want;

    // Put everything back first, then repaint. Simpler than reasoning about which of the two
    // overlays a block should fall back to.
    let previous = std::mem::take(&mut edges.painted);
    for id in previous {
        let cat = category_of(&city, id);
        if let Ok(mut mat) = blocks.get_mut(city.entities[id.index()]) {
            *mat = MeshMaterial3d(materials.by_slot[cat as usize].clone());
        }
    }

    let mut painted = Vec::new();
    let mut paint = |id: NodeId, handle: &Handle<StandardMaterial>, painted: &mut Vec<NodeId>| {
        if let Ok(mut mat) = blocks.get_mut(city.entities[id.index()]) {
            *mat = MeshMaterial3d(handle.clone());
            painted.push(id);
        }
    };

    if let Some(group) = edges
        .sets
        .get(edges.active)
        .and_then(|s| edges.selected.and_then(|i| s.groups.get(i)))
    {
        for id in group.members.iter().take(MAX_HIGHLIGHT) {
            paint(*id, &materials.highlight, &mut painted);
        }
    }
    // Second, so it wins wherever the two overlap.
    for id in &pinned.subtree {
        paint(*id, &materials.pinned, &mut painted);
    }

    edges.painted = painted;
}

/// Distinct from every category hue, since it has to read as "these, right now" against whatever
/// it is drawn over.
pub const HIGHLIGHT: Color = Color::srgb(0.98, 0.82, 0.25);

fn category_of(city: &City, id: NodeId) -> Category {
    if city.index.flags(id).is_dir() {
        Category::Directory
    } else {
        city.categories.of(city.index.ext(id))
    }
}
