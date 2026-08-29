//! Level of detail, and picking, in one traversal.
//!
//! Phase 0 measured the overview camera at roughly half the framerate of the street camera at
//! every object count, and a hard wall past two million blocks. Drawing every file of a big drive
//! at once is therefore not an option, and it would be unreadable anyway: a file that covers a
//! third of a pixel is noise.
//!
//! The rule is a screen-size test evaluated top-down. Walk from the root; a node that is on
//! screen at a reasonable size gets its children walked too, and one that is small does not, so
//! its whole subtree stays hidden and the folder is drawn as a single solid block instead. Cost is
//! proportional to what ends up drawn, not to what was scanned, because an early stop skips the
//! entire subtree rather than testing it.
//!
//! Picking rides along on the same walk. The visible set is already bounded by the test above, so
//! a ray-box test against every node reached costs a fraction of a millisecond and needs no
//! spatial index at all.

use bevy::prelude::*;
use bevy::window::PrimaryWindow;
use traveler_scan::{NodeId, Tree};

use crate::{City, FileNode};

/// Vertical field of view assumed for the screen-size test. Bevy's perspective default; being
/// slightly off only nudges a heuristic threshold, it cannot make the view wrong.
const ASSUMED_FOV_Y: f32 = std::f32::consts::FRAC_PI_4;

#[derive(Resource)]
pub struct Lod {
    pub enabled: bool,
    /// A node whose largest footprint edge covers fewer pixels than this does not get expanded.
    pub px_threshold: f32,
    /// Visibility currently applied to the entity.
    visible: Vec<bool>,
    /// True when the node is currently drawn as its own thin slab rather than as a solid block
    /// standing in for its whole subtree.
    expanded: Vec<bool>,
    seen_stamp: Vec<u32>,
    expand_stamp: Vec<u32>,
    reached: Vec<u32>,
    prev_reached: Vec<u32>,
    stack: Vec<u32>,
    frame: u32,
    pub drawn: usize,
}

impl Lod {
    pub fn new(n: usize) -> Self {
        Self {
            enabled: true,
            px_threshold: 28.0,
            visible: vec![false; n],
            expanded: vec![true; n],
            seen_stamp: vec![u32::MAX; n],
            expand_stamp: vec![u32::MAX; n],
            reached: Vec::with_capacity(4096),
            prev_reached: Vec::with_capacity(4096),
            stack: Vec::with_capacity(1024),
            frame: 0,
            drawn: 0,
        }
    }

    /// Called after a relayout, which rewrites every transform to the slab form.
    pub fn mark_all_expanded(&mut self) {
        self.expanded.fill(true);
    }
}

#[derive(Resource, Default)]
pub struct Hover {
    pub node: Option<NodeId>,
    /// Distance to the hit, so a nearer pick from another source can win.
    pub distance: f32,
}

pub fn update_lod(
    city: Res<City>,
    mut lod: ResMut<Lod>,
    mut hover: ResMut<Hover>,
    camera: Query<&GlobalTransform, With<Camera3d>>,
    windows: Query<&Window, With<PrimaryWindow>>,
    mut blocks: Query<(&mut Visibility, &mut Transform), With<FileNode>>,
) {
    let Ok(cam) = camera.single() else {
        return;
    };
    let Ok(window) = windows.single() else {
        return;
    };

    let eye = cam.translation();
    let forward = cam.forward();
    let viewport_h = window.resolution.physical_height().max(1) as f32;
    // Pixels per world unit at one unit of distance.
    let focal = viewport_h * 0.5 / (ASSUMED_FOV_Y * 0.5).tan();

    let index = &city.index;
    let places = &city.placements;

    lod.frame = lod.frame.wrapping_add(1);
    let frame = lod.frame;
    {
        // Split the borrow: two fields of one resource cannot both be reached through DerefMut.
        let lod = &mut *lod;
        std::mem::swap(&mut lod.reached, &mut lod.prev_reached);
    }
    lod.reached.clear();
    lod.stack.clear();
    lod.stack.push(index.root().0);

    let mut best_hit = f32::INFINITY;
    let mut best_node: Option<NodeId> = None;

    while let Some(raw) = lod.stack.pop() {
        let id = NodeId(raw);
        if !places.is_visible(id) {
            continue;
        }

        let center = places.center[id.index()];
        let size = places.size[id.index()];
        let kids = index.child_count(id);

        let footprint = size.x.max(size.z);
        let distance = (center - eye).length().max(0.001);
        let px = footprint * focal / distance;
        let expand = kids > 0 && (!lod.enabled || px >= lod.px_threshold);

        lod.seen_stamp[id.index()] = frame;
        if expand {
            lod.expand_stamp[id.index()] = frame;
            for c in index.child_ids(id) {
                lod.stack.push(c.0);
            }
        }
        lod.reached.push(raw);

        // Box actually drawn for this node: the thin slab when its contents are showing,
        // otherwise a solid block spanning everything it contains.
        let (draw_center, draw_size) = if kids > 0 && !expand {
            collapsed_box(&city, id)
        } else {
            (center, size)
        };

        if let Some(t) = ray_box(eye, *forward, draw_center, draw_size)
            && t < best_hit
        {
            best_hit = t;
            best_node = Some(id);
        }
    }

    // Hide what fell out of the set, then show and re-shape what is in it. Both loops are over
    // the visible set, never over every node in the tree.
    for i in 0..lod.prev_reached.len() {
        let raw = lod.prev_reached[i];
        let idx = raw as usize;
        if lod.seen_stamp[idx] != frame && lod.visible[idx] {
            lod.visible[idx] = false;
            if let Ok((mut vis, _)) = blocks.get_mut(city.entities[idx]) {
                *vis = Visibility::Hidden;
            }
        }
    }

    for i in 0..lod.reached.len() {
        let raw = lod.reached[i];
        let idx = raw as usize;
        let id = NodeId(raw);
        let want_expanded = lod.expand_stamp[idx] == frame || index.child_count(id) == 0;
        let show = !lod.visible[idx];
        let reshape = lod.expanded[idx] != want_expanded;
        // Nothing to write means no ECS access at all. With a still camera that makes this loop
        // free, and while moving it costs only the nodes that actually changed.
        if !show && !reshape {
            continue;
        }
        let Ok((mut vis, mut transform)) = blocks.get_mut(city.entities[idx]) else {
            continue;
        };
        if show {
            lod.visible[idx] = true;
            *vis = Visibility::Visible;
        }
        if reshape {
            lod.expanded[idx] = want_expanded;
            let (c, s) = if want_expanded {
                (places.center[idx], places.size[idx])
            } else {
                collapsed_box(&city, id)
            };
            transform.translation = c;
            transform.scale = s;
        }
    }

    lod.drawn = lod.reached.len();
    hover.node = best_node;
    hover.distance = best_hit;
}

/// A folder standing in for its contents: same footprint, but tall enough to cover everything
/// inside it, so an unexpanded region reads as solid mass rather than an empty plate.
fn collapsed_box(city: &City, id: NodeId) -> (Vec3, Vec3) {
    let center = city.placements.center[id.index()];
    let size = city.placements.size[id.index()];
    let base = center.y - size.y * 0.5;
    let top = city.subtree_top[id.index()].max(base + size.y);
    let height = top - base;
    (
        Vec3::new(center.x, base + height * 0.5, center.z),
        Vec3::new(size.x, height, size.z),
    )
}

/// Slab method. Returns the near intersection distance, or None.
pub fn ray_box(origin: Vec3, dir: Vec3, center: Vec3, size: Vec3) -> Option<f32> {
    let half = size * 0.5;
    let min = center - half;
    let max = center + half;
    let mut t_near = f32::NEG_INFINITY;
    let mut t_far = f32::INFINITY;

    for axis in 0..3 {
        let d = dir[axis];
        let o = origin[axis];
        if d.abs() < 1e-8 {
            if o < min[axis] || o > max[axis] {
                return None;
            }
        } else {
            let inv = 1.0 / d;
            let mut t0 = (min[axis] - o) * inv;
            let mut t1 = (max[axis] - o) * inv;
            if t0 > t1 {
                std::mem::swap(&mut t0, &mut t1);
            }
            t_near = t_near.max(t0);
            t_far = t_far.min(t1);
            if t_near > t_far {
                return None;
            }
        }
    }
    if t_far < 0.0 {
        return None;
    }
    Some(t_near.max(0.0))
}

#[cfg(test)]
mod tests {
    use super::ray_box;
    use bevy::prelude::Vec3;

    const C: Vec3 = Vec3::new(0.0, 0.0, 0.0);
    const S: Vec3 = Vec3::new(2.0, 2.0, 2.0);

    #[test]
    fn hits_a_box_straight_ahead_at_the_near_face() {
        let t = ray_box(Vec3::new(0.0, 0.0, 10.0), Vec3::new(0.0, 0.0, -1.0), C, S);
        assert!((t.expect("hit") - 9.0).abs() < 1e-4);
    }

    #[test]
    fn misses_a_box_off_to_the_side() {
        assert!(ray_box(Vec3::new(5.0, 0.0, 10.0), Vec3::new(0.0, 0.0, -1.0), C, S).is_none());
    }

    #[test]
    fn ignores_a_box_behind_the_camera() {
        assert!(ray_box(Vec3::new(0.0, 0.0, 10.0), Vec3::new(0.0, 0.0, 1.0), C, S).is_none());
    }

    #[test]
    fn a_camera_inside_a_box_hits_it_at_zero() {
        let t = ray_box(Vec3::ZERO, Vec3::new(0.0, 0.0, -1.0), C, S);
        assert_eq!(t, Some(0.0));
    }

    #[test]
    fn a_ray_parallel_to_a_face_still_resolves() {
        // Travelling along x at a height inside the box: hits. Outside it: misses.
        assert!(ray_box(Vec3::new(-10.0, 0.5, 0.0), Vec3::X, C, S).is_some());
        assert!(ray_box(Vec3::new(-10.0, 5.0, 0.0), Vec3::X, C, S).is_none());
    }
}
