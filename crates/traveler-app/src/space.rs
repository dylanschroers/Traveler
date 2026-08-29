//! The address space on screen.
//!
//! Same renderer, same detail pass, same picking as the disk city. What differs is the tree it
//! walks and what a colour means: hue carries permissions here, which is the most informative
//! three-way split for an unfamiliar program, and it reuses the three validated hues rather than
//! picking new ones.

use std::sync::Arc;

use bevy::prelude::*;
use bevy::state::state_scoped::DespawnOnExit;
use traveler_core::{Tree, human_bytes, thousands};
use traveler_layout::{Layout, LayoutCtx, Placements, SpaceLayout, SpaceParams};
use traveler_proc::{AddressSpace, AttachedTarget, Frame, NodeKind, pages::PAGE};

use crate::hud::{INK_MUTED, INK_SECONDARY, PANEL, font, panel};
use crate::lod::{Hover, Lod};
use crate::palette::Category;
use crate::pin::{MAX_PAINTED, Pinned};
use crate::scene::{SceneView, Target, subtree_tops};
use crate::{BlockMaterials, FileNode, Mode, block_materials, camera_start};
use bevy::window::PrimaryWindow;
use traveler_core::NodeId;

use crate::ray_box;

/// Thickness is a free channel here, as it is in the city. Resident fraction is the default,
/// so a mapping that is four gigabytes reserved and forty megabytes resident reads as a wide flat
/// plate rather than a tower.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SpaceHeight {
    Uniform,
    Resident,
}

#[derive(Resource)]
pub struct Space {
    pub capture: AddressSpace,
    pub frame: Arc<Frame>,
    pub placements: Placements,
    pub subtree_top: Vec<f32>,
    pub entities: Vec<Entity>,
    pub params: SpaceParams,
    pub height: SpaceHeight,
    pub layout_ms: f64,
}

/// Held apart from the snapshot so a capture can borrow the space immutably while the traced
/// process, which every later capture and poke needs mutably, stays reachable.
#[derive(Resource)]
pub struct Traced(pub AttachedTarget);

impl Space {
    pub fn new(capture: AddressSpace, params: SpaceParams) -> Self {
        let frame = capture.frame().clone();
        let mut placements = Placements::with_capacity(frame.len());
        let started = std::time::Instant::now();
        SpaceLayout::new(params).place(&LayoutCtx::new(frame.as_ref()), &mut placements);
        let layout_ms = started.elapsed().as_secs_f64() * 1000.0;
        let entities = vec![Entity::PLACEHOLDER; frame.len()];
        let mut space = Self {
            capture,
            frame,
            placements,
            subtree_top: Vec::new(),
            entities,
            params,
            height: SpaceHeight::Resident,
            layout_ms,
        };
        space.apply_height();
        space
    }

    /// Rewrites the thickness channel in place. Layout stays a pure function of the tree: how tall
    /// a block is drawn is a rendering choice and belongs here, not in the packer.
    pub fn apply_height(&mut self) {
        let base = |id: NodeId| self.frame.depth(id) as f32 * self.params.level_step;
        let thin = self.params.leaf_thickness;
        let thick = thin * 6.0;
        for raw in self.frame.mappings() {
            let id = NodeId(raw);
            if !self.placements.is_visible(id) {
                continue;
            }
            let t = match self.height {
                SpaceHeight::Uniform => thin,
                // Unknown reads as flat rather than as full, since a mapping nobody looked at is
                // not a mapping that is entirely resident.
                SpaceHeight::Resident => {
                    thin + self.capture.resident_fraction(id).unwrap_or(0.0) * (thick - thin)
                }
            };
            let i = id.index();
            self.placements.size[i].y = t;
            self.placements.center[i].y = base(id) + t * 0.5;
        }
        self.subtree_top = subtree_tops(self.frame.as_ref(), &self.placements);
    }

    /// Which of the five materials a node wears. The slots are the same ones the disk city uses,
    /// so the palette stays the validated three hues plus two neutrals.
    fn slot(&self, id: NodeId) -> Category {
        if self.frame.kind(id) != NodeKind::Mapping {
            return Category::Directory;
        }
        let prot = self.capture.prot(id);
        if prot.is_exec() {
            Category::Code
        } else if prot.is_write() {
            Category::Media
        } else if prot.is_read() {
            Category::Binary
        } else {
            Category::Other
        }
    }
}

impl SceneView for Space {
    type Tree = Frame;

    fn tree(&self) -> &Frame {
        &self.frame
    }
    fn placements(&self) -> &Placements {
        &self.placements
    }
    fn subtree_top(&self) -> &[f32] {
        &self.subtree_top
    }
    fn entities(&self) -> &[Entity] {
        &self.entities
    }
    fn world_size(&self) -> f32 {
        self.params.world_size as f32
    }
}

#[derive(Component)]
pub struct SpaceHover;

#[derive(Component)]
pub struct SpaceStats;

pub fn spawn(
    mut commands: Commands,
    mut space: ResMut<Space>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    camera: Query<Entity, With<Camera3d>>,
) {
    let cube = meshes.add(Cuboid::from_size(Vec3::ONE));
    let block = block_materials(&mut materials);
    let pages = PageView::spawn_pool(
        &mut commands,
        cube.clone(),
        block.by_slot[Category::Other as usize].clone(),
    );
    commands.insert_resource(pages);

    let mut spawned = Vec::with_capacity(space.placements.placed);
    for id in space.frame.nodes() {
        if !space.placements.is_visible(id) {
            continue;
        }
        let slot = space.slot(id);
        let entity = commands
            .spawn((
                Mesh3d(cube.clone()),
                MeshMaterial3d(block.by_slot[slot as usize].clone()),
                Transform::from_translation(space.placements.center[id.index()])
                    .with_scale(space.placements.size[id.index()]),
                Visibility::Hidden,
                FileNode(id),
                DespawnOnExit(Mode::Process),
            ))
            .id();
        spawned.push((id, entity));
    }
    for (id, entity) in spawned {
        space.entities[id.index()] = entity;
    }
    commands.insert_resource(block);

    for (illuminance, at) in [
        (4_000.0, Vec3::new(-0.35, -1.0, -0.5)),
        (1_500.0, Vec3::new(0.8, -0.35, 0.6)),
        (1_100.0, Vec3::new(0.2, -0.2, -0.9)),
    ] {
        commands.spawn((
            DirectionalLight {
                illuminance,
                shadow_maps_enabled: false,
                ..default()
            },
            Transform::IDENTITY.looking_at(at, Vec3::Y),
            DespawnOnExit(Mode::Process),
        ));
    }

    let world = space.params.world_size as f32;
    for entity in camera.iter() {
        commands
            .entity(entity)
            .insert((camera_start(world), crate::free_camera(world)));
    }
}

pub fn setup_hud(mut commands: Commands, space: Res<Space>, traced: Res<Traced>) {
    let frame = &space.frame;
    let meta = space.capture.meta();
    commands.spawn((
        panel(Some(10.0), None, Some(10.0), None),
        BackgroundColor(PANEL),
        Text::new(format!(
            "{} [{}]   observed under trace\n{} mapped in {} regions, {} mappings\nstop {:.2} ms over {} thread{}   layout {:.0} ms",
            traced.0.name(),
            meta.pid,
            human_bytes(frame.weight(frame.root())),
            thousands(frame.child_count(frame.root()) as u64),
            thousands(frame.mappings().len() as u64),
            meta.stop.as_secs_f64() * 1000.0,
            meta.threads,
            if meta.threads == 1 { "" } else { "s" },
            space.layout_ms,
        )),
        font(13.0),
        TextColor(INK_SECONDARY),
        DespawnOnExit(Mode::Process),
    ));

    commands.spawn((
        panel(Some(10.0), None, None, Some(10.0)),
        BackgroundColor(PANEL),
        Text::new(""),
        font(13.0),
        TextColor(INK_SECONDARY),
        SpaceStats,
        DespawnOnExit(Mode::Process),
    ));

    commands.spawn((
        Node {
            position_type: PositionType::Absolute,
            left: Val::Percent(50.0),
            top: Val::Percent(50.0),
            margin: UiRect {
                left: Val::Px(16.0),
                top: Val::Px(10.0),
                ..default()
            },
            padding: UiRect::all(Val::Px(6.0)),
            max_width: Val::Px(720.0),
            ..default()
        },
        BackgroundColor(PANEL),
        Text::new(""),
        font(13.0),
        TextColor(Color::WHITE),
        SpaceHover,
        DespawnOnExit(Mode::Process),
    ));

    // Hue carries permissions, so the legend says which is which and the readout says the rest.
    let rows = [
        (Category::Code, "executable   r-x, .text, PLT"),
        (Category::Media, "writable   heap, stack, .data, anon"),
        (Category::Binary, "read-only   .rodata, file-backed"),
        (Category::Other, "reserved   PROT_NONE, guards"),
        (Category::Directory, "regions"),
    ];
    let row_h = 19.0;
    commands.spawn((
        Node {
            position_type: PositionType::Absolute,
            left: Val::Px(6.0),
            bottom: Val::Px(60.0),
            width: Val::Px(380.0),
            height: Val::Px(rows.len() as f32 * row_h + 8.0),
            ..default()
        },
        BackgroundColor(PANEL),
        DespawnOnExit(Mode::Process),
    ));
    for (i, (slot, label)) in rows.iter().enumerate() {
        let y = 66.0 + (rows.len() - 1 - i) as f32 * row_h;
        commands.spawn((
            Node {
                position_type: PositionType::Absolute,
                left: Val::Px(12.0),
                bottom: Val::Px(y + 3.0),
                width: Val::Px(12.0),
                height: Val::Px(12.0),
                ..default()
            },
            BackgroundColor(slot.color()),
            DespawnOnExit(Mode::Process),
        ));
        commands.spawn((
            Node {
                position_type: PositionType::Absolute,
                left: Val::Px(32.0),
                bottom: Val::Px(y),
                ..default()
            },
            Text::new(*label),
            font(12.0),
            TextColor(INK_SECONDARY),
            DespawnOnExit(Mode::Process),
        ));
    }

    commands.spawn((
        panel(None, Some(10.0), Some(10.0), None),
        BackgroundColor(PANEL),
        Text::new(
            "WASD move   space/ctrl up-down   shift run   scroll speed   right-click or M grab mouse\n\
             L level of detail   [ ] detail threshold   H height channel   R reset camera\n\
             left click pins   fly close to a mapping to see its pages",
        ),
        font(12.0),
        TextColor(INK_MUTED),
        DespawnOnExit(Mode::Process),
    ));
}

pub fn update_hover(
    space: Res<Space>,
    hover: Res<Hover>,
    pinned: Res<Pinned>,
    mut readout: Query<(&mut Text, &mut Visibility), With<SpaceHover>>,
) {
    let Ok((mut text, mut vis)) = readout.single_mut() else {
        return;
    };
    let shown = pinned.target.or(hover.target);
    *vis = if shown.is_some() {
        Visibility::Inherited
    } else {
        Visibility::Hidden
    };
    let Some(target) = shown else {
        return;
    };

    let frame = &space.frame;
    let id = target.node();
    let mut out = match target {
        Target::Page { page, .. } => page_readout(&space, id, page),
        Target::Node(_) => node_readout(&space, id),
    };
    if let Some((path, offset)) = frame.backing(id) {
        out.push_str(&format!("\n{path} +{offset:#x}"));
    }
    if pinned.target == Some(target) {
        out.push_str("\npinned   click again or esc to release");
        if frame.child_count(id) > 0 && matches!(target, Target::Node(_)) {
            out.push_str(&format!(
                "\nholds {} mappings in {} regions{}",
                thousands(pinned.leaves),
                thousands(pinned.groups),
                if pinned.painted_all {
                    String::new()
                } else {
                    format!(", {} lit", thousands(MAX_PAINTED as u64))
                }
            ));
        }
    }
    text.0 = out;
}

fn node_readout(space: &Space, id: NodeId) -> String {
    let frame = &space.frame;
    let kind = match frame.kind(id) {
        NodeKind::Process => "process",
        NodeKind::Region => "region",
        NodeKind::Mapping => "mapping",
        NodeKind::Section => "section",
        NodeKind::Gap => "gap",
    };
    let residency = match space.capture.resident_fraction(id) {
        Some(f) => format!(
            "{} resident ({:.0}%)",
            human_bytes(space.capture.resident_bytes(id)),
            f * 100.0
        ),
        // Said plainly, because "not read" is not "not resident".
        None => "residency not read".to_string(),
    };
    format!(
        "{:016x}-{:016x}\n{}   {}   {} {}\n{residency}",
        frame.addr(id),
        frame.end(id),
        human_bytes(frame.weight(id)),
        space.capture.prot(id),
        kind,
        frame.label(id),
    )
}

fn page_readout(space: &Space, mapping: NodeId, page: u32) -> String {
    let frame = &space.frame;
    let addr = frame.addr(mapping) + page as u64 * PAGE;
    let m = space.capture.mapping_index(mapping).unwrap_or(0);
    let pages = space.capture.pages();
    let state = if pages.is_present(m, page) {
        "resident"
    } else if pages.is_swapped(m, page) {
        "swapped"
    } else {
        "not present"
    };
    format!(
        "{:016x}-{:016x}\n4 KiB   {}   page {} of {}\n{state}{}   in {}",
        addr,
        addr + PAGE,
        space.capture.prot(mapping),
        thousands(page as u64),
        thousands(pages.pages(m) as u64),
        if pages.is_dirty(m, page) {
            ", written since"
        } else {
            ""
        },
        frame.label(mapping),
    )
}

/// Thickness is a toggle, as `HeightMode` is in the city, so the honest reading — every mapping
/// the same height, area as the only size channel — is one key away.
pub fn height_input(
    keys: Res<ButtonInput<KeyCode>>,
    mut space: ResMut<Space>,
    mut lod: ResMut<Lod>,
    mut blocks: Query<(&FileNode, &mut Transform)>,
) {
    if !keys.just_pressed(KeyCode::KeyH) {
        return;
    }
    space.height = match space.height {
        SpaceHeight::Resident => SpaceHeight::Uniform,
        SpaceHeight::Uniform => SpaceHeight::Resident,
    };
    space.apply_height();
    for (node, mut transform) in blocks.iter_mut() {
        let i = node.0.index();
        if space.placements.visible[i] {
            transform.translation = space.placements.center[i];
            transform.scale = space.placements.size[i];
        }
    }
    lod.mark_all_expanded();
}

pub fn update_stats(
    space: Res<Space>,
    lod: Res<Lod>,
    pages: Res<PageView>,
    time: Res<Time<Real>>,
    mut smoothed: Local<f32>,
    mut stats: Query<&mut Text, With<SpaceStats>>,
) {
    let dt = time.delta_secs();
    if dt > 0.0 {
        let fps = 1.0 / dt;
        *smoothed = if *smoothed <= 0.0 {
            fps
        } else {
            *smoothed * 0.93 + fps * 0.07
        };
    }
    if let Ok(mut text) = stats.single_mut() {
        text.0 = format!(
            "{:.0} fps\n{} drawn of {}\n{} page blocks\ndetail {} at {:.0} px\nheight {}",
            *smoothed,
            thousands(lod.drawn as u64),
            thousands(space.placements.placed as u64),
            thousands(pages.drawn() as u64),
            if lod.enabled { "on" } else { "off" },
            lod.px_threshold,
            match space.height {
                SpaceHeight::Resident => "resident",
                SpaceHeight::Uniform => "uniform",
            },
        );
    }
}

/// The pin overlay, put back to the permission colour when it moves.
pub fn paint_pin(
    space: Res<Space>,
    pinned: Res<Pinned>,
    materials: Res<BlockMaterials>,
    mut painted: Local<Vec<NodeId>>,
    mut applied: Local<u32>,
    mut blocks: Query<&mut MeshMaterial3d<StandardMaterial>, With<FileNode>>,
) {
    if pinned.generation == *applied {
        return;
    }
    *applied = pinned.generation;

    for id in std::mem::take(&mut *painted) {
        if let Ok(mut mat) = blocks.get_mut(space.entities[id.index()]) {
            *mat = MeshMaterial3d(materials.by_slot[space.slot(id) as usize].clone());
        }
    }
    for id in &pinned.subtree {
        if let Ok(mut mat) = blocks.get_mut(space.entities[id.index()]) {
            *mat = MeshMaterial3d(materials.pinned.clone());
            painted.push(*id);
        }
    }
}

pub fn close(mut commands: Commands) {
    commands.remove_resource::<Space>();
    commands.remove_resource::<PageView>();
    commands.remove_resource::<Traced>();
    commands.remove_resource::<Lod>();
    commands.remove_resource::<Hover>();
    commands.remove_resource::<Pinned>();
    commands.remove_resource::<BlockMaterials>();
}

/// Pool size, and therefore the hard cap on page blocks on screen at once. Spawned once and
/// hidden, repositioned as you fly, exactly as `satellite.rs` does with cluster blocks: a
/// gigabyte of working set is 262k pages and spawning entities for them would cost the 1.5 s per
/// 227k that PLAN.md §5 measured.
pub const PAGE_POOL: usize = 8192;

/// A page block smaller than this on screen is noise, and expanding into it would spend the pool
/// on something nobody can see.
const MIN_PAGE_PX: f32 = 3.5;

#[derive(Component)]
pub struct PageBlock;

#[derive(Clone, Copy)]
struct Slot {
    target: Target,
    center: Vec3,
    size: Vec3,
    /// Decided while the bitsets are already in hand, so the draw loop is a transform write and
    /// one array index rather than a second lookup per page.
    paint: Category,
}

#[derive(Resource, Default)]
pub struct PageView {
    pool: Vec<Entity>,
    live: Vec<Slot>,
    shown: usize,
}

impl PageView {
    pub fn spawn_pool(
        commands: &mut Commands,
        mesh: Handle<Mesh>,
        material: Handle<StandardMaterial>,
    ) -> Self {
        let pool = (0..PAGE_POOL)
            .map(|_| {
                commands
                    .spawn((
                        Mesh3d(mesh.clone()),
                        MeshMaterial3d(material.clone()),
                        Transform::from_scale(Vec3::ZERO),
                        Visibility::Hidden,
                        PageBlock,
                        DespawnOnExit(Mode::Process),
                    ))
                    .id()
            })
            .collect();
        Self {
            pool,
            live: Vec::with_capacity(PAGE_POOL),
            shown: 0,
        }
    }

    pub fn drawn(&self) -> usize {
        self.live.len()
    }
}

/// The one branch §9 adds to the detail pass. A mapping has no child nodes, so when it is close
/// enough to read its pages are synthesised from its bitsets into pool slots and ray-tested there.
/// The walk itself is unchanged: this runs over the set it already reached.
#[allow(clippy::too_many_arguments)]
pub fn update_pages(
    space: Res<Space>,
    lod: Res<Lod>,
    mut view: ResMut<PageView>,
    mut hover: ResMut<Hover>,
    materials: Res<BlockMaterials>,
    camera: Query<&GlobalTransform, With<Camera3d>>,
    windows: Query<&Window, With<PrimaryWindow>>,
    mut blocks: Query<
        (
            &mut Transform,
            &mut Visibility,
            &mut MeshMaterial3d<StandardMaterial>,
        ),
        With<PageBlock>,
    >,
) {
    let (Ok(cam), Ok(window)) = (camera.single(), windows.single()) else {
        return;
    };
    let eye = cam.translation();
    let forward = *cam.forward();
    let focal = Lod::focal(window.resolution.physical_height().max(1) as f32);

    view.live.clear();
    let pages = space.capture.pages();
    let step = space.params.level_step;

    for &raw in lod.reached() {
        let id = NodeId(raw);
        if space.frame.kind(id) != NodeKind::Mapping {
            continue;
        }
        let Some(m) = space.capture.mapping_index(id) else {
            continue;
        };
        let count = pages.pages(m);
        if count == 0 || pages.is_elided(m) {
            continue;
        }

        let center = space.placements.center[id.index()];
        let size = space.placements.size[id.index()];
        // Row-major, so reading the grid is reading addresses, exactly as the strips are.
        let (cols, rows) = page_grid(count, size.x, size.z);
        let cell = Vec3::new(size.x / cols as f32, step * 0.5, size.z / rows as f32);

        let distance = (center - eye).length().max(0.001);
        if cell.x.min(cell.z) * focal / distance < MIN_PAGE_PX {
            continue;
        }
        if view.live.len() + count as usize > PAGE_POOL {
            continue;
        }

        let resident = space.slot(id);
        let origin = Vec3::new(
            center.x - size.x * 0.5,
            center.y + size.y * 0.5,
            center.z - size.z * 0.5,
        );
        for page in 0..count {
            let (col, row) = (page % cols, page / cols);
            // Written-since rides the thickness channel, never a hue: §10 keeps the three
            // validated colours for permissions and gives change its own channel.
            let tall = if pages.is_dirty(m, page) { 2.2 } else { 1.0 };
            let size = Vec3::new(cell.x, cell.y * tall, cell.z);
            // A resident page wears its mapping's permission colour; anything else is a neutral,
            // so a mapping that is mostly holes reads as mostly holes.
            let paint = if pages.is_present(m, page) {
                resident
            } else if pages.is_swapped(m, page) {
                Category::Other
            } else {
                Category::Directory
            };
            view.live.push(Slot {
                target: Target::Page { mapping: id, page },
                center: Vec3::new(
                    origin.x + (col as f32 + 0.5) * cell.x,
                    origin.y + size.y * 0.5,
                    origin.z + (row as f32 + 0.5) * cell.z,
                ),
                size,
                paint,
            });
        }
    }

    let mut best = hover.nearer();
    let mut hit = None;
    for (slot, entry) in view.live.iter().enumerate() {
        if let Ok((mut transform, mut vis, mut mat)) = blocks.get_mut(view.pool[slot]) {
            transform.translation = entry.center;
            transform.scale = entry.size;
            *vis = Visibility::Visible;
            let want = &materials.by_slot[entry.paint as usize];
            if mat.0 != *want {
                *mat = MeshMaterial3d(want.clone());
            }
        }
        if let Some(t) = ray_box(eye, forward, entry.center, entry.size)
            && t < best
        {
            best = t;
            hit = Some(entry.target);
        }
    }
    for slot in view.live.len()..view.shown {
        if let Ok((_, mut vis, _)) = blocks.get_mut(view.pool[slot]) {
            *vis = Visibility::Hidden;
        }
    }
    view.shown = view.live.len();

    if let Some(target) = hit {
        hover.target = Some(target);
        hover.distance = best;
    }
}

/// Rows and columns for a mapping's pages, chosen so the cells come out square-ish in whatever
/// rect the mapping got. Row-major, so reading the grid is reading addresses.
fn page_grid(count: u32, width: f32, depth: f32) -> (u32, u32) {
    let cols = ((count as f32 * (width / depth.max(1e-6))).sqrt().ceil() as u32).clamp(1, count);
    (cols, count.div_ceil(cols))
}

#[cfg(test)]
mod tests {
    use super::page_grid;

    #[test]
    fn a_grid_always_has_room_for_every_page() {
        for count in [1u32, 2, 7, 64, 512, 4095, 262_144] {
            for (w, d) in [(1.0, 1.0), (10.0, 1.0), (1.0, 10.0), (3.0, 7.0)] {
                let (cols, rows) = page_grid(count, w, d);
                assert!(cols >= 1 && rows >= 1);
                assert!(
                    cols * rows >= count,
                    "{count} pages do not fit {cols}x{rows} at {w}x{d}"
                );
                // No wasted row: dropping one column would not still fit.
                assert!(cols == 1 || (cols - 1) * rows < count || rows * cols < count + cols);
            }
        }
    }

    /// Cells in a square footprint should be square-ish, or the page grid is a bank of hairlines.
    #[test]
    fn cells_come_out_square_ish() {
        for count in [4u32, 9, 100, 1024, 4096] {
            let (cols, rows) = page_grid(count, 100.0, 100.0);
            let (cw, ch) = (100.0 / cols as f32, 100.0 / rows as f32);
            let aspect = (cw / ch).max(ch / cw);
            assert!(aspect < 1.6, "{count} pages give {aspect:.2}:1 cells");
        }
    }

    #[test]
    fn a_wide_footprint_gets_more_columns_than_rows() {
        let (cols, rows) = page_grid(256, 40.0, 10.0);
        assert!(cols > rows, "{cols}x{rows} is not wider than it is tall");
        let (cols, rows) = page_grid(256, 10.0, 40.0);
        assert!(rows > cols, "{cols}x{rows} is not taller than it is wide");
    }
}
