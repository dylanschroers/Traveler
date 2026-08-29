//! The selected group, lifted out of the city and arranged by folder.
//!
//! The city stays exactly as it was underneath. This floats above it, one block per member,
//! clumped by the folder each member came from, with a thread dropping from every block down to
//! the real file. Reading it takes one glance: a single tight clump means the group came from one
//! place, several clumps mean several.
//!
//! Blocks live in a fixed pool spawned once and hidden. Selection changes move them rather than
//! spawning and despawning, so switching groups costs no allocation.

use bevy::prelude::*;
use traveler_layout::satellite::{Satellite, SatelliteParams, lay_out};
use traveler_scan::NodeId;

use crate::edges::{Edges, HIGHLIGHT};
use crate::lod::Hover;
use crate::{City, ray_box};

/// Pool size, and therefore the hard cap on blocks the cluster can show.
pub const POOL: usize = 2000;

/// Marks a pooled block. The slot is the entity's position in `SatelliteView::pool`, so the
/// component itself carries nothing.
#[derive(Component)]
pub struct SatelliteBlock;

#[derive(Resource, Default)]
pub struct SatelliteView {
    pub pool: Vec<Entity>,
    pub data: Satellite,
    pub origin: Vec3,
    /// Which (kind, group) is currently laid out, so an unchanged selection costs nothing.
    pub applied: Option<(usize, usize)>,
    pub shown: usize,
    /// Uniform shrink applied so a cluster of any size fits a readable footprint.
    pub scale: f32,
    /// Block edge after scaling, which is what the pick has to test against.
    pub block: f32,
}

impl SatelliteView {
    /// The member a pooled block currently stands for.
    fn member(&self, slot: usize) -> Option<NodeId> {
        self.data.members.get(slot).copied()
    }
}

pub fn update_satellite(
    mut view: ResMut<SatelliteView>,
    edges: Res<Edges>,
    city: Res<City>,
    mut blocks: Query<(&mut Transform, &mut Visibility), With<SatelliteBlock>>,
) {
    let want = edges.selected.map(|s| (edges.active, s));
    if want == view.applied {
        return;
    }
    view.applied = want;

    let params = SatelliteParams {
        max_members: POOL,
        ..Default::default()
    };
    view.data = match edges.selected_group() {
        Some(group) => lay_out(
            city.index.as_ref(),
            &group.members,
            &city.placements,
            &params,
        ),
        None => Satellite::default(),
    };

    // Hang the cluster over exactly the point the layout measured its bearings from. Any other
    // centre, however reasonable it looks on its own, tilts every thread by the offset between
    // the two and the arrangement stops lining up with the city.
    let world = city.params.world_size as f32;
    let ceiling = city.placements.max.y + world * 0.30;
    let anchor = view.data.anchor;
    view.origin = Vec3::new(anchor.x, ceiling, anchor.y);

    // A group spread over hundreds of folders packs into a wide disc. Shrink the whole thing to
    // one readable footprint rather than letting it sprawl to the size of the city.
    let target = world * 0.18;
    view.scale = if view.data.radius > target && view.data.radius > 0.0 {
        target / view.data.radius
    } else {
        1.0
    };
    view.block = view.data.block * view.scale;

    let block = view.block;
    let scale = view.scale;
    let origin = view.origin;
    let count = view.data.offsets.len().min(view.pool.len());
    for slot in 0..view.pool.len() {
        let Ok((mut transform, mut vis)) = blocks.get_mut(view.pool[slot]) else {
            continue;
        };
        if slot < count {
            transform.translation = origin + view.data.offsets[slot] * scale;
            transform.scale = Vec3::splat(block);
            *vis = Visibility::Visible;
        } else if view.shown > slot {
            *vis = Visibility::Hidden;
        }
    }
    view.shown = count;
}

/// One straight line per member, from the file to its block.
///
/// This had an elbow for a while: straight up out of the file, then a bend into the block. That
/// was a workaround for threads crossing the city, and it stopped being worth its cost once the
/// cluster started keeping the city's bearings, because the lines no longer cross. Worse, the
/// bend plane sat just under the cluster, so a thread from a distant file had to cover its whole
/// horizontal run in a short vertical drop and came out nearly horizontal, streaking across
/// everything. A straight line has neither problem.
pub fn draw_threads(view: Res<SatelliteView>, city: Res<City>, mut gizmos: Gizmos) {
    if view.data.is_empty() {
        return;
    }
    let half = view.block * 0.5;
    for (slot, member) in view.data.members.iter().enumerate() {
        if !city.placements.is_visible(*member) {
            continue;
        }
        let file = city.placements.center[member.index()];
        let block = view.origin + view.data.offsets[slot] * view.scale - Vec3::Y * half;
        gizmos.line(file, block, HIGHLIGHT.with_alpha(0.3));
    }
}

/// Lets the crosshair name a file by pointing at its block in the cluster, not just in the city.
/// Runs after the city pick and only wins if it is nearer.
pub fn pick_satellite(
    view: Res<SatelliteView>,
    mut hover: ResMut<Hover>,
    camera: Query<&GlobalTransform, With<Camera3d>>,
) {
    if view.data.is_empty() {
        return;
    }
    let Ok(cam) = camera.single() else {
        return;
    };
    let eye = cam.translation();
    let dir = *cam.forward();
    let size = Vec3::splat(view.block);

    let mut best = if hover.node.is_some() {
        hover.distance
    } else {
        f32::INFINITY
    };
    let mut hit = None;
    for (slot, offset) in view.data.offsets.iter().enumerate() {
        if let Some(t) = ray_box(eye, dir, view.origin + *offset * view.scale, size)
            && t < best
        {
            best = t;
            hit = Some(slot);
        }
    }
    if let Some(slot) = hit
        && let Some(member) = view.member(slot)
    {
        hover.node = Some(member);
        hover.distance = best;
    }
}

/// Spawns the pool once, hidden. Called from the city setup, which already holds the mesh.
pub fn spawn_pool(
    commands: &mut Commands,
    mesh: Handle<Mesh>,
    material: Handle<StandardMaterial>,
) -> Vec<Entity> {
    (0..POOL)
        .map(|_| {
            commands
                .spawn((
                    Mesh3d(mesh.clone()),
                    MeshMaterial3d(material.clone()),
                    Transform::from_scale(Vec3::ZERO),
                    Visibility::Hidden,
                    SatelliteBlock,
                ))
                .id()
        })
        .collect()
}
