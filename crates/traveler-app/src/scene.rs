//! What the level-of-detail pass and the crosshair need, whichever city they are looking at.
//!
//! The disk city and the address space share the renderer and the detail pass, not the scene.
//! This is the seam: a scene is a tree, its placements, and one entity per node. Everything above
//! it — categories, permissions, captures — belongs to one mode and stays there.

use bevy::prelude::*;
use traveler_core::{NodeId, Tree};
use traveler_layout::Placements;

/// What the crosshair can be on. Pages are not nodes — a gigabyte of working set is 262k of them
/// and §9 keeps them as bitsets — so they have no `NodeId` and everything that can hold a
/// selection has to be able to hold one of these instead.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Target {
    Node(NodeId),
    /// Page `page` of `mapping`, resolved against that mapping's bitsets on demand.
    Page {
        mapping: NodeId,
        page: u32,
    },
}

impl Target {
    /// The node a selection belongs to, which for a page is the mapping holding it.
    #[inline]
    pub fn node(self) -> NodeId {
        match self {
            Target::Node(id) => id,
            Target::Page { mapping, .. } => mapping,
        }
    }
}

pub trait SceneView: Resource {
    type Tree: Tree;

    fn tree(&self) -> &Self::Tree;
    fn placements(&self) -> &Placements;
    /// Highest point of everything inside a node, for drawing it as one solid block when its
    /// contents are not being shown.
    fn subtree_top(&self) -> &[f32];
    fn entities(&self) -> &[Entity];
    fn world_size(&self) -> f32;
}

/// One reverse pass, since a child always has a higher id than its parent.
pub fn subtree_tops<T: Tree>(tree: &T, places: &Placements) -> Vec<f32> {
    let n = tree.len();
    let mut top = vec![0.0f32; n];
    for (i, t) in top.iter_mut().enumerate() {
        if places.visible[i] {
            *t = places.center[i].y + places.size[i].y * 0.5;
        }
    }
    for i in (1..n).rev() {
        let p = tree.parent(NodeId(i as u32)).index();
        top[p] = top[p].max(top[i]);
    }
    top
}
