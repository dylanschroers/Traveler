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
use traveler_proc::{
    AddressSpace, AttachedTarget, Change, Frame, NodeKind, Prot, Union, pages::PAGE,
};

use crate::hud::{INK_MUTED, INK_SECONDARY, PANEL, font, panel};
use crate::lod::{Hover, Lod};
use crate::palette::Category;
use crate::pin::{MAX_PAINTED, Pinned};
use crate::scene::{SceneView, Target, subtree_tops};
use crate::symbols::{Symbols, Tier};
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

/// What the city is showing: one capture, or the change between two.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum View {
    Capture(usize),
    /// Both captures render into one geometry, because the frame is shared. §8: the diff is a
    /// recolour pass over one `Placements`, not a second layout.
    Diff {
        before: usize,
        after: usize,
    },
}

/// Where a node's facts come from. A single capture answers about pages and residency; a diff
/// answers about change, and deliberately answers nothing about pages — §10 replaces the
/// categorical encoding in diff mode rather than layering on it.
pub enum Source {
    Capture(Box<AddressSpace>),
    Diff(Box<Union>),
}

#[derive(Resource)]
pub struct Space {
    pub source: Source,
    pub view: View,
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

/// Every capture taken of this target, oldest first. Kept whole, because a frame is built over
/// all of them and a capture cannot be re-taken.
#[derive(Resource, Default)]
pub struct Captures(pub Vec<AddressSpace>);

impl Space {
    pub fn build(source: Source, view: View, params: SpaceParams) -> Self {
        let frame = match &source {
            Source::Capture(space) => space.frame().clone(),
            Source::Diff(union) => union.frame.clone(),
        };
        let mut placements = Placements::with_capacity(frame.len());
        let started = std::time::Instant::now();
        SpaceLayout::new(params).place(&LayoutCtx::new(frame.as_ref()), &mut placements);
        let layout_ms = started.elapsed().as_secs_f64() * 1000.0;
        let entities = vec![Entity::PLACEHOLDER; frame.len()];
        let mut space = Self {
            source,
            view,
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

    /// The capture whose facts a readout should quote: the later one in a diff.
    pub fn shown(&self) -> usize {
        match self.view {
            View::Capture(i) => i,
            View::Diff { after, .. } => after,
        }
    }

    #[inline]
    pub fn prot(&self, id: NodeId) -> Prot {
        match &self.source {
            Source::Capture(space) => space.prot(id),
            Source::Diff(union) => union.prot[self.shown()][id.index()],
        }
    }

    /// `None` in diff mode: page state belongs to one capture, and a union extent is not one.
    pub fn pages(&self) -> Option<&AddressSpace> {
        match &self.source {
            Source::Capture(space) => Some(space),
            Source::Diff(_) => None,
        }
    }

    pub fn change(&self, id: NodeId) -> Option<Change> {
        match (&self.source, self.view) {
            (Source::Diff(union), View::Diff { before, after }) => {
                Some(union.change(before, after, id))
            }
            _ => None,
        }
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
            let fraction = match (&self.source, self.height) {
                (_, SpaceHeight::Uniform) => None,
                // Unknown reads as flat rather than as full, since a mapping nobody looked at is
                // not a mapping that is entirely resident.
                (Source::Capture(space), SpaceHeight::Resident) => space.resident_fraction(id),
                (Source::Diff(_), SpaceHeight::Resident) => None,
            };
            let t = thin + fraction.unwrap_or(0.0) * (thick - thin);
            let i = id.index();
            self.placements.size[i].y = t;
            self.placements.center[i].y = base(id) + t * 0.5;
        }
        self.subtree_top = subtree_tops(self.frame.as_ref(), &self.placements);
    }

    /// Which of the materials a node wears. In a single capture that is its permissions, using
    /// the three validated hues. In a diff the categorical encoding is switched off entirely and
    /// the diverging ramp takes over, which is what §10 requires rather than six hues.
    fn slot(&self, id: NodeId) -> Category {
        if let Some(change) = self.change(id) {
            return match change {
                Change::Added => Category::Code,
                Change::Removed => Category::Media,
                Change::Unchanged => Category::Other,
                Change::Absent => Category::Directory,
            };
        }
        if self.frame.kind(id) != NodeKind::Mapping && self.frame.kind(id) != NodeKind::Section {
            return Category::Directory;
        }
        let prot = self.prot(id);
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

pub fn source_for(captures: &Captures, view: View) -> Source {
    match view {
        View::Capture(i) => Source::Capture(Box::new(captures.0[i].clone())),
        View::Diff { .. } => {
            let facts: Vec<Vec<traveler_proc::OwnedMapping>> =
                captures.0.iter().map(|c| c.mappings().to_vec()).collect();
            Source::Diff(Box::new(Union::build("diff", &facts, &|_| Vec::new())))
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

/// The one cuboid every block shares, kept so a new frame can respawn without re-adding it.
#[derive(Resource)]
pub struct BlockMesh(pub Handle<Mesh>);

/// Set when the frame changed under the scene, which a new capture or a switch of view does.
#[derive(Resource, Default)]
pub struct Restage(pub bool);

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

    stage(&mut commands, &mut space, &cube, &block);
    commands.insert_resource(BlockMesh(cube));
    commands.insert_resource(block);
    commands.init_resource::<Restage>();

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

/// One block per placed node. Extending a frame renumbers it, so a new capture is a full rebuild
/// rather than an incremental one — which is what §5 says it costs and why it is not done per
/// capture behind the user's back.
fn stage(commands: &mut Commands, space: &mut Space, cube: &Handle<Mesh>, block: &BlockMaterials) {
    space.entities = vec![Entity::PLACEHOLDER; space.frame.len()];
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
}

/// Another capture, or another view of the ones already taken.
#[allow(clippy::too_many_arguments)]
pub fn capture_input(
    keys: Res<ButtonInput<KeyCode>>,
    mut traced: ResMut<Traced>,
    mut captures: ResMut<Captures>,
    mut space: ResMut<Space>,
    mut symbols: ResMut<Symbols>,
    mut restage: ResMut<Restage>,
    mut lod: ResMut<Lod>,
) {
    let count = captures.0.len();
    let want = if keys.just_pressed(KeyCode::KeyC) {
        let taken = traced
            .0
            .capture_sections(&traveler_proc::CaptureOptions::default(), &mut |mappings| {
                symbols.build(mappings)
            });
        match taken {
            Ok(capture) => {
                captures.0.push(capture);
                Some(View::Capture(captures.0.len() - 1))
            }
            Err(e) => {
                eprintln!("error: {e}");
                None
            }
        }
    } else if keys.just_pressed(KeyCode::Comma) {
        Some(View::Capture(space.shown().saturating_sub(1)))
    } else if keys.just_pressed(KeyCode::Period) {
        Some(View::Capture((space.shown() + 1).min(count - 1)))
    } else if keys.just_pressed(KeyCode::KeyX) && count > 1 {
        Some(match space.view {
            View::Diff { .. } => View::Capture(count - 1),
            View::Capture(_) => View::Diff {
                before: count - 2,
                after: count - 1,
            },
        })
    } else {
        None
    };

    let Some(want) = want else {
        return;
    };
    if want == space.view {
        return;
    }
    *space = Space::build(source_for(&captures, want), want, space.params);
    restage.0 = true;
    lod.mark_all_expanded();
}

#[allow(clippy::too_many_arguments)]
pub fn restage(
    mut commands: Commands,
    mut restage: ResMut<Restage>,
    mut space: ResMut<Space>,
    mesh: Res<BlockMesh>,
    block: Res<BlockMaterials>,
    mut lod: ResMut<Lod>,
    mut pinned: ResMut<Pinned>,
    mut hover: ResMut<Hover>,
    existing: Query<Entity, With<FileNode>>,
) {
    if !restage.0 {
        return;
    }
    restage.0 = false;
    for entity in existing.iter() {
        commands.entity(entity).despawn();
    }
    stage(&mut commands, &mut space, &mesh.0, &block);
    // Every id the old frame minted is meaningless against the new one.
    *lod = Lod::new(space.frame.len());
    pinned.target = None;
    hover.target = None;
}

pub fn setup_hud(
    mut commands: Commands,
    space: Res<Space>,
    traced: Res<Traced>,
    symbols: Res<Symbols>,
    captures: Res<Captures>,
) {
    let frame = &space.frame;
    let meta = captures.0[space.shown()].meta().clone();
    commands.spawn((
        panel(Some(10.0), None, Some(10.0), None),
        BackgroundColor(PANEL),
        Text::new(format!(
            "{} [{}]   observed under trace\n{} mapped in {} regions, {} mappings\nstop {:.2} ms over {} thread{}   layout {:.0} ms\n{}",
            traced.0.name(),
            meta.pid,
            human_bytes(frame.weight(frame.root())),
            thousands(frame.child_count(frame.root()) as u64),
            thousands(frame.mappings().len() as u64),
            meta.stop.as_secs_f64() * 1000.0,
            meta.threads,
            if meta.threads == 1 { "" } else { "s" },
            space.layout_ms,
            // A person needs to know whether they are reading a name or a guess.
            match symbols.tier() {
                Tier::Two => format!(
                    "tier 2: {} sections from {} images",
                    thousands(symbols.sections() as u64),
                    thousands(symbols.images() as u64)
                ),
                Tier::One => "tier 1: no image parsed, mappings only".to_string(),
            },
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
    // In diff mode the categorical encoding is switched off entirely and the same three slots
    // carry a diverging ramp instead, which is why the legend is rebuilt with the view.
    let rows = match space.view {
        View::Capture(_) => [
            (Category::Code, "executable   r-x, .text, PLT"),
            (Category::Media, "writable   heap, stack, .data, anon"),
            (Category::Binary, "read-only   .rodata, file-backed"),
            (Category::Other, "reserved   PROT_NONE, guards"),
            (Category::Directory, "regions"),
        ],
        View::Diff { .. } => [
            (Category::Code, "added   mapped since the earlier capture"),
            (Category::Other, "unchanged"),
            (Category::Media, "removed   unmapped since"),
            (Category::Binary, "unused in diff mode"),
            (Category::Directory, "in neither capture"),
        ],
    };
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
             C new capture   , . step captures   X diff the last two\n\
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
    mut symbols: ResMut<Symbols>,
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
        // Tier 2's payoff: a name for an address, out of `.dynsym`, with no debug info anywhere.
        let at = match target {
            Target::Page { page, .. } => frame.addr(id) + page as u64 * PAGE,
            Target::Node(_) => frame.addr(id),
        };
        let path = path.to_string();
        if let Some((name, into)) = symbols.name_at(&path, at) {
            out.push_str(&format!(
                "\n{name}{}",
                if into == 0 {
                    String::new()
                } else {
                    format!(" +{into:#x}")
                }
            ));
        }
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
    let detail = match (space.change(id), space.pages()) {
        (Some(change), _) => format!("{change:?} between the two captures"),
        (None, Some(capture)) => match capture.resident_fraction(id) {
            Some(f) => format!(
                "{} resident ({:.0}%)",
                human_bytes(capture.resident_bytes(id)),
                f * 100.0
            ),
            // Said plainly, because "not read" is not "not resident".
            None => "residency not read".to_string(),
        },
        (None, None) => String::new(),
    };
    format!(
        "{:016x}-{:016x}\n{}   {}   {} {}\n{detail}",
        frame.addr(id),
        frame.end(id),
        human_bytes(frame.weight(id)),
        space.prot(id),
        kind,
        frame.label(id),
    )
}

fn page_readout(space: &Space, mapping: NodeId, page: u32) -> String {
    let frame = &space.frame;
    let addr = frame.addr(mapping) + page as u64 * PAGE;
    let Some(capture) = space.pages() else {
        return node_readout(space, mapping);
    };
    let m = capture.mapping_index(mapping).unwrap_or(0);
    let pages = capture.pages();
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
        space.prot(mapping),
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
    captures: Res<Captures>,
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
            "{:.0} fps\n{} drawn of {}\n{} page blocks\ndetail {} at {:.0} px\nheight {}\n{}",
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
            match space.view {
                View::Capture(i) => format!("capture {} of {}", i + 1, captures.0.len()),
                View::Diff { before, after } => format!("diff {} to {}", before + 1, after + 1),
            },
        );
    }
}

/// The pin overlay, put back to whatever the node was wearing when it moves.
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
    commands.remove_resource::<Symbols>();
    commands.remove_resource::<PageView>();
    commands.remove_resource::<Captures>();
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
    // Page state belongs to one capture. A diff renders a union frame, where an extent is not
    // any one capture's mapping, so there is nothing to draw pages from.
    let Some(capture) = space.pages() else {
        hide(&mut view, &mut blocks);
        return;
    };
    let pages = capture.pages();
    let step = space.params.level_step;

    for &raw in lod.reached() {
        let id = NodeId(raw);
        if space.frame.kind(id) != NodeKind::Mapping {
            continue;
        }
        let Some(m) = capture.mapping_index(id) else {
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
fn hide(
    view: &mut PageView,
    blocks: &mut Query<
        (
            &mut Transform,
            &mut Visibility,
            &mut MeshMaterial3d<StandardMaterial>,
        ),
        With<PageBlock>,
    >,
) {
    for slot in 0..view.shown {
        if let Ok((_, mut vis, _)) = blocks.get_mut(view.pool[slot]) {
            *vis = Visibility::Hidden;
        }
    }
    view.shown = 0;
}

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
