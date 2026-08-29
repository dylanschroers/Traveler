//! Click to hold on to something.
//!
//! The crosshair readout changes the instant you turn your head, which is useless the moment you
//! want to actually look at what you found. Clicking pins it. Clicking a folder also lights up
//! everything inside it, which is the only way to see how much of a region belongs to one folder
//! rather than to its neighbours.

use bevy::prelude::*;
use traveler_core::{NodeId, Tree};

use crate::lod::Hover;
use crate::scene::{SceneView, Target};

/// Painting is one component write per node, so a click on the root cannot be allowed to touch
/// every block in the tree at once. Counting is uncapped: the summary tells the truth even when
/// the highlight is showing only part of it.
pub const MAX_PAINTED: usize = 20_000;

#[derive(Resource, Default)]
pub struct Pinned {
    pub target: Option<Target>,
    /// Descendants of the pinned node, capped for painting.
    pub subtree: Vec<NodeId>,
    /// True totals underneath it, whatever the cap. A leaf is a file or a mapping, a group is a
    /// folder or a region, depending on which city is being looked at.
    pub leaves: u64,
    pub groups: u64,
    pub painted_all: bool,
    /// Bumped whenever the subtree is rebuilt. The painter keys off this rather than off the node
    /// id, so it cannot mark itself up to date after running before the subtree existed.
    pub generation: u32,
    /// Largest single child, which is nearly always the thing you actually wanted.
    pub biggest: Option<NodeId>,
}

pub fn pin_input(
    mouse: Res<ButtonInput<MouseButton>>,
    keys: Res<ButtonInput<KeyCode>>,
    hover: Res<Hover>,
    mut pinned: ResMut<Pinned>,
) {
    if keys.just_pressed(KeyCode::Escape) {
        pinned.target = None;
        return;
    }
    if !mouse.just_pressed(MouseButton::Left) {
        return;
    }
    // Clicking the same block again, or clicking the sky, lets go.
    pinned.target = match (hover.target, pinned.target) {
        (Some(hit), Some(held)) if hit == held => None,
        (hit, _) => hit,
    };
}

/// Recomputes the subtree and its totals, only when the pinned node changes.
pub fn update_pin<S: SceneView>(
    mut pinned: ResMut<Pinned>,
    scene: Res<S>,
    mut last: Local<Option<Target>>,
) {
    if pinned.target == *last {
        return;
    }
    *last = pinned.target;
    pinned.generation = pinned.generation.wrapping_add(1);

    pinned.subtree.clear();
    pinned.leaves = 0;
    pinned.groups = 0;
    pinned.biggest = None;
    pinned.painted_all = true;

    let Some(root) = pinned.target.map(Target::node) else {
        return;
    };
    let index = scene.tree();

    pinned.biggest = index.child_ids(root).max_by_key(|c| index.weight(*c));

    // Children are contiguous but grandchildren are not, so the subtree needs a walk.
    let mut stack = vec![root];
    while let Some(id) = stack.pop() {
        if id != root {
            if index.is_group(id) {
                pinned.groups += 1;
            } else {
                pinned.leaves += 1;
            }
        }
        if pinned.subtree.len() < MAX_PAINTED {
            pinned.subtree.push(id);
        } else {
            pinned.painted_all = false;
        }
        for c in index.child_ids(id) {
            stack.push(c);
        }
    }
}
