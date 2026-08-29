//! The address space on screen.
//!
//! Same renderer, same detail pass, same picking as the disk city. What differs is the tree it
//! walks and what a colour means: hue carries permissions here, which is the most informative
//! three-way split for an unfamiliar program, and it reuses the three validated hues rather than
//! picking new ones.

use std::sync::Arc;

use bevy::prelude::*;
use bevy::state::state_scoped::DespawnOnExit;
use traveler_core::{NodeId, Tree, human_bytes, thousands};
use traveler_layout::{Layout, LayoutCtx, Placements, SpaceLayout, SpaceParams};
use traveler_proc::{AddressSpace, AttachedTarget, Frame, NodeKind};

use crate::hud::{INK_MUTED, INK_SECONDARY, PANEL, font, panel};
use crate::lod::{Hover, Lod};
use crate::palette::Category;
use crate::pin::{MAX_PAINTED, Pinned};
use crate::scene::{SceneView, subtree_tops};
use crate::{BlockMaterials, FileNode, Mode, block_materials, camera_start};

#[derive(Resource)]
pub struct Space {
    pub capture: AddressSpace,
    pub frame: Arc<Frame>,
    pub placements: Placements,
    pub subtree_top: Vec<f32>,
    pub entities: Vec<Entity>,
    pub params: SpaceParams,
    pub layout_ms: f64,
}

/// Held apart from the snapshot so a capture can borrow the space immutably while the target,
/// which every later capture and poke needs mutably, stays reachable.
#[derive(Resource)]
pub struct Target(pub AttachedTarget);

impl Space {
    pub fn new(capture: AddressSpace, params: SpaceParams) -> Self {
        let frame = capture.frame().clone();
        let mut placements = Placements::with_capacity(frame.len());
        let started = std::time::Instant::now();
        SpaceLayout::new(params).place(&LayoutCtx::new(frame.as_ref()), &mut placements);
        let layout_ms = started.elapsed().as_secs_f64() * 1000.0;
        let subtree_top = subtree_tops(frame.as_ref(), &placements);
        let entities = vec![Entity::PLACEHOLDER; frame.len()];
        Self {
            capture,
            frame,
            placements,
            subtree_top,
            entities,
            params,
            layout_ms,
        }
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

pub fn setup_hud(mut commands: Commands, space: Res<Space>, target: Res<Target>) {
    let frame = &space.frame;
    let meta = space.capture.meta();
    commands.spawn((
        panel(Some(10.0), None, Some(10.0), None),
        BackgroundColor(PANEL),
        Text::new(format!(
            "{} [{}]   observed under trace\n{} mapped in {} regions, {} mappings\nstop {:.2} ms over {} thread{}   layout {:.0} ms",
            target.0.name(),
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
             L level of detail   [ ] detail threshold   R reset camera   left click pins",
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
    let shown = pinned.node.or(hover.node);
    *vis = if shown.is_some() {
        Visibility::Inherited
    } else {
        Visibility::Hidden
    };
    let Some(id) = shown else {
        return;
    };

    let frame = &space.frame;
    let kind = match frame.kind(id) {
        NodeKind::Process => "process",
        NodeKind::Region => "region",
        NodeKind::Mapping => "mapping",
        NodeKind::Section => "section",
        NodeKind::Gap => "gap",
    };
    let mut out = format!(
        "{:016x}-{:016x}\n{}   {}   {} {}",
        frame.addr(id),
        frame.end(id),
        human_bytes(frame.weight(id)),
        space.capture.prot(id),
        kind,
        frame.label(id),
    );
    if let Some((path, offset)) = frame.backing(id) {
        out.push_str(&format!("\n{path} +{offset:#x}"));
    }
    if pinned.node == Some(id) {
        out.push_str("\npinned   click again or esc to release");
        if frame.child_count(id) > 0 {
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

pub fn update_stats(
    space: Res<Space>,
    lod: Res<Lod>,
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
            "{:.0} fps\n{} drawn of {}\ndetail {} at {:.0} px",
            *smoothed,
            thousands(lod.drawn as u64),
            thousands(space.placements.placed as u64),
            if lod.enabled { "on" } else { "off" },
            lod.px_threshold,
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
    commands.remove_resource::<Target>();
    commands.remove_resource::<Lod>();
    commands.remove_resource::<Hover>();
    commands.remove_resource::<Pinned>();
    commands.remove_resource::<BlockMaterials>();
}
