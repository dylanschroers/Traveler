//! Phase 2: turn a tree into positions.
//!
//! A layout is a pure function. It takes a [`Tree`], it returns one block per node, and it knows
//! nothing about rendering or about where the tree came from. That is what makes the city/graph
//! switch in phase 5 cheap: node ids are stable across layouts, so switching views is
//! interpolating two `Placements`.
//!
//! Nothing here depends on bevy, only on `glam`, so the whole thing is testable headlessly.

mod city;
pub mod satellite;
mod space;
pub mod svg;

pub use city::{CityLayout, CityParams, HeightMode};
pub use space::{SpaceLayout, SpaceParams};

use glam::Vec3;
use traveler_core::{NodeId, Tree};

/// Everything a layout is allowed to look at.
///
/// A struct rather than bare arguments so phase 4 can add `edges` without touching the trait.
pub struct LayoutCtx<'a, T> {
    pub tree: &'a T,
}

impl<'a, T: Tree> LayoutCtx<'a, T> {
    pub fn new(tree: &'a T) -> Self {
        Self { tree }
    }
}

pub trait Layout {
    fn name(&self) -> &'static str;
    /// Generic rather than `dyn`: the treemap touches the tree once per node per layout, and a
    /// vtable hop in that loop is measurable at a million nodes.
    fn place<T: Tree>(&self, ctx: &LayoutCtx<'_, T>, out: &mut Placements);
}

/// One axis-aligned block per node, parallel to the index.
///
/// `center` and `size` are what a renderer wants directly: a unit cube scaled by `size` and
/// translated to `center`. A node that could not be placed, because its slice of the parent
/// collapsed to nothing, has `visible[i] == false` and zeroes for the rest.
#[derive(Clone, Debug, Default)]
pub struct Placements {
    pub center: Vec<Vec3>,
    pub size: Vec<Vec3>,
    pub visible: Vec<bool>,
    pub min: Vec3,
    pub max: Vec3,
    /// Nodes with `visible == true`.
    pub placed: usize,
}

impl Placements {
    pub fn with_capacity(n: usize) -> Self {
        Self {
            center: vec![Vec3::ZERO; n],
            size: vec![Vec3::ZERO; n],
            visible: vec![false; n],
            min: Vec3::ZERO,
            max: Vec3::ZERO,
            placed: 0,
        }
    }

    pub fn len(&self) -> usize {
        self.center.len()
    }

    pub fn is_empty(&self) -> bool {
        self.center.is_empty()
    }

    /// Resets in place, keeping the allocations. Re-laying out the same tree should not allocate.
    pub fn reset(&mut self, n: usize) {
        self.center.clear();
        self.center.resize(n, Vec3::ZERO);
        self.size.clear();
        self.size.resize(n, Vec3::ZERO);
        self.visible.clear();
        self.visible.resize(n, false);
        self.min = Vec3::ZERO;
        self.max = Vec3::ZERO;
        self.placed = 0;
    }

    #[inline]
    pub fn is_visible(&self, id: NodeId) -> bool {
        self.visible[id.index()]
    }

    /// Footprint of a node in the ground plane as `(min_x, min_z, max_x, max_z)`.
    #[inline]
    pub fn footprint(&self, id: NodeId) -> (f32, f32, f32, f32) {
        let c = self.center[id.index()];
        let s = self.size[id.index()];
        (
            c.x - s.x * 0.5,
            c.z - s.z * 0.5,
            c.x + s.x * 0.5,
            c.z + s.z * 0.5,
        )
    }
}

/// Rectangle in the ground plane. `h` is the extent along z, not a height.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub z: f64,
    pub w: f64,
    pub h: f64,
}

impl Rect {
    #[inline]
    pub fn area(&self) -> f64 {
        self.w * self.h
    }

    #[inline]
    pub fn is_degenerate(&self) -> bool {
        self.w <= f64::EPSILON || self.h <= f64::EPSILON
    }

    /// Shrinks by `m` on every side, never past collapsing: a rect thinner than `2m` keeps a
    /// sliver rather than inverting.
    #[inline]
    pub fn inset(&self, m: f64) -> Rect {
        let mx = m.min(self.w * 0.4);
        let mz = m.min(self.h * 0.4);
        Rect {
            x: self.x + mx,
            z: self.z + mz,
            w: self.w - 2.0 * mx,
            h: self.h - 2.0 * mz,
        }
    }
}

/// Per-node layout weight: bytes, with a floor so that empty files and empty directories still
/// get a visible sliver instead of a zero-area rect.
///
/// The floor is applied to leaves only and summed upward, never applied to a directory directly.
/// Nested treemaps need a directory's weight to equal the sum of its children's exactly, and any
/// per-node transform of an already-summed value breaks that.
pub fn layout_weights<T: Tree>(tree: &T, min_leaf: f64) -> Vec<f64> {
    let n = tree.len();
    let mut w = vec![0.0f64; n];
    for (i, weight) in w.iter_mut().enumerate() {
        let id = NodeId(i as u32);
        if tree.child_count(id) == 0 {
            *weight = (tree.weight(id) as f64).max(min_leaf);
        }
    }
    // Children always have a higher id than their parent, so one reverse pass sums every subtree.
    for i in (1..n).rev() {
        let p = tree.parent(NodeId(i as u32)).index();
        w[p] += w[i];
    }
    w
}

/// Spacing as a fraction of the parent's shorter side, shrinking one step per level of depth.
///
/// The falloff gives top-level folders boulevards and deep ones seams, and it stops the spacing
/// compounding: without it every level would surrender the same fraction and a node ten levels
/// down would keep only a fraction of its true area.
#[inline]
pub(crate) fn spacing_ratio(base: f64, falloff: f64, floor: f64, depth: u16) -> f64 {
    if base <= 0.0 {
        return 0.0;
    }
    (base * falloff.powi(depth as i32)).max(floor)
}
