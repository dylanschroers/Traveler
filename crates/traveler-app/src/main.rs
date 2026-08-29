//! Traveler: fly through a folder tree.
//!
//! Phase 3. Scan, lay out, and render the city, with a free camera, a level-of-detail pass, and
//! a crosshair readout. The scan runs before the window opens, so a cold drive prints progress to
//! the console rather than showing a frozen frame.

mod edges;
mod hud;
mod lod;
mod palette;
mod pin;
mod satellite;

use std::path::PathBuf;
use std::sync::Arc;

use bevy::camera_controller::free_camera::{FreeCamera, FreeCameraPlugin};
use bevy::prelude::*;
use bevy::render::view::screenshot::{Screenshot, save_to_disk};
use bevy::window::{PresentMode, WindowResolution};
use bevy::winit::WinitSettings;

use edges::Edges;
use lod::{Hover, Lod};
pub use lod::ray_box;
use pin::Pinned;
use satellite::SatelliteView;
use palette::{Category, CategoryTable};
use traveler_layout::{CityLayout, CityParams, HeightMode, Layout, LayoutCtx, Placements};
use traveler_edges::{EdgeProvider, ExtensionGroups, NameFamilies};
use traveler_scan::{FileIndex, NodeId, ScanOptions, Tree, human_bytes, scan};

/// The block materials, kept so a highlighted node can be put back the way it was.
#[derive(Resource)]
pub struct BlockMaterials {
    pub by_category: Vec<Handle<StandardMaterial>>,
    pub highlight: Handle<StandardMaterial>,
    pub pinned: Handle<StandardMaterial>,
}

/// Marks an entity as the block for one node of the tree.
#[derive(Component)]
pub struct FileNode(pub NodeId);

#[derive(Resource)]
pub struct City {
    /// Shared, because the duplicate finder reads it from a worker thread.
    pub index: Arc<FileIndex>,
    pub placements: Placements,
    /// Highest point of everything inside a node, for drawing it as one solid block when its
    /// contents are not being shown.
    pub subtree_top: Vec<f32>,
    pub entities: Vec<Entity>,
    pub categories: CategoryTable,
    pub params: CityParams,
    pub layout_ms: f64,
}

impl City {
    fn relayout(&mut self) {
        let started = std::time::Instant::now();
        let ctx = LayoutCtx::new(self.index.as_ref());
        CityLayout::new(self.params).place(&ctx, &mut self.placements);
        self.layout_ms = started.elapsed().as_secs_f64() * 1000.0;
        self.subtree_top = subtree_tops(&self.index, &self.placements);
    }
}

/// Highest y reached by a node's whole subtree. One reverse pass, since a child always has a
/// higher id than its parent.
fn subtree_tops(index: &FileIndex, places: &Placements) -> Vec<f32> {
    let n = index.len();
    let mut top = vec![0.0f32; n];
    for (i, t) in top.iter_mut().enumerate() {
        if places.visible[i] {
            *t = places.center[i].y + places.size[i].y * 0.5;
        }
    }
    for i in (1..n).rev() {
        let p = index.parent(NodeId(i as u32)).index();
        top[p] = top[p].max(top[i]);
    }
    top
}

struct Args {
    path: PathBuf,
    params: CityParams,
    vsync: bool,
    lod: bool,
    lod_px: f32,
    shot: Option<PathBuf>,
    shot_eye: Option<Vec3>,
    /// Preselects a group, so a screenshot can show a selection without a person pressing keys.
    select: Option<usize>,
    /// Starts the duplicate scan immediately instead of waiting for the key.
    dupes: bool,
    shot_after: f32,
    /// Pins whatever the crosshair lands on, so a screenshot can show a held selection.
    pin: bool,
}

/// Renders a fixed number of frames, saves a screenshot, and exits. Used to check the render
/// without a person having to sit and look at it.
#[derive(Resource)]
struct ShotPlan {
    path: PathBuf,
    after: f32,
    elapsed: f32,
    taken: bool,
    frames: u32,
    measured: f32,
}

const USAGE: &str = "\
usage: traveler <path> [options]

  --world N       side length of the city in world units (default 1000)
  --spacing N     scale the streets and walls, 1.0 is the default
  --uniform       uniform file thickness instead of log(size)
  --lod-px N      detail threshold in pixels (default 28)
  --no-lod        draw every block, no level of detail
  --no-vsync      uncap the framerate
  --shot <file>   render a few frames, save a png, and exit
  --shot-eye x,y,z  camera position for the screenshot
  --select N      preselect group N of the first group kind
  --dupes         start the duplicate scan at launch instead of on the D key
  --shot-after N  seconds to wait before the screenshot (default 3)
  --pin           pin whatever the crosshair lands on, for screenshots";

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        path: PathBuf::new(),
        params: CityParams::default(),
        vsync: true,
        lod: true,
        lod_px: 28.0,
        shot: None,
        shot_eye: None,
        select: None,
        dupes: false,
        shot_after: 3.0,
        pin: false,
    };
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let mut got_path = false;
    let mut i = 0;
    while i < raw.len() {
        match raw[i].as_str() {
            "--world" => {
                i += 1;
                args.params.world_size = raw
                    .get(i)
                    .and_then(|v| v.parse().ok())
                    .ok_or("--world needs a number")?;
            }
            "--lod-px" => {
                i += 1;
                args.lod_px = raw
                    .get(i)
                    .and_then(|v| v.parse().ok())
                    .ok_or("--lod-px needs a number")?;
            }
            "--shot" => {
                i += 1;
                args.shot = Some(PathBuf::from(raw.get(i).ok_or("--shot needs a file")?));
            }
            "--shot-eye" => {
                i += 1;
                let v: Vec<f32> = raw
                    .get(i)
                    .ok_or("--shot-eye needs x,y,z")?
                    .split(',')
                    .filter_map(|c| c.parse().ok())
                    .collect();
                if v.len() != 3 {
                    return Err("--shot-eye needs x,y,z".into());
                }
                args.shot_eye = Some(Vec3::new(v[0], v[1], v[2]));
            }
            "--spacing" => {
                i += 1;
                let f: f64 = raw
                    .get(i)
                    .and_then(|v| v.parse().ok())
                    .ok_or("--spacing needs a number")?;
                args.params.margin_ratio *= f;
                args.params.gap_ratio *= f;
            }
            "--select" => {
                i += 1;
                args.select = raw.get(i).and_then(|v| v.parse().ok());
                if args.select.is_none() {
                    return Err("--select needs a group index".into());
                }
            }
            "--shot-after" => {
                i += 1;
                args.shot_after = raw
                    .get(i)
                    .and_then(|v| v.parse().ok())
                    .ok_or("--shot-after needs seconds")?;
            }
            "--dupes" => args.dupes = true,
            "--pin" => args.pin = true,
            "--uniform" => args.params.height = HeightMode::Uniform,
            "--no-lod" => args.lod = false,
            "--no-vsync" => args.vsync = false,
            "-h" | "--help" => return Err(String::new()),
            other if other.starts_with('-') => return Err(format!("unknown flag: {other}")),
            other => {
                args.path = PathBuf::from(other);
                got_path = true;
            }
        }
        i += 1;
    }
    if !got_path {
        return Err(String::new());
    }
    Ok(args)
}

fn main() {
    let args = match parse_args() {
        Ok(a) => a,
        Err(msg) => {
            if !msg.is_empty() {
                eprintln!("error: {msg}\n");
            }
            eprintln!("{USAGE}");
            std::process::exit(2);
        }
    };

    println!("scanning {} ...", args.path.display());
    let index = match scan(&args.path, &ScanOptions::default()) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("error: cannot scan {}: {e}", args.path.display());
            std::process::exit(1);
        }
    };
    let s = index.stats;
    println!(
        "  {} files, {} folders, {} in {:.2}s{}",
        s.files,
        s.dirs,
        human_bytes(s.bytes),
        s.walk_millis as f64 / 1000.0,
        if index.errors.is_empty() {
            String::new()
        } else {
            format!(", {} unreadable", index.errors.len())
        }
    );

    let categories = CategoryTable::build(&index);
    let n = index.len();

    // The providers that need no I/O are cheap enough to just run. The duplicate finder is not,
    // and waits for a keypress.
    let started = std::time::Instant::now();
    let sets = vec![
        ExtensionGroups.compute(&index),
        NameFamilies.compute(&index),
    ];
    println!(
        "  {} extension groups, {} name families in {:.0} ms",
        sets[0].groups.len(),
        sets[1].groups.len(),
        started.elapsed().as_secs_f64() * 1000.0
    );
    let index = Arc::new(index);
    let index_for_dupes = index.clone();
    let mut city = City {
        placements: Placements::with_capacity(n),
        subtree_top: Vec::new(),
        entities: vec![Entity::PLACEHOLDER; n],
        categories,
        params: args.params,
        layout_ms: 0.0,
        index,
    };
    city.relayout();
    println!(
        "  laid out {} blocks in {:.0} ms",
        city.placements.placed, city.layout_ms
    );

    let start_eye = args.shot_eye;
    let mut lod = Lod::new(n);
    lod.enabled = args.lod;
    lod.px_threshold = args.lod_px;

    let mut app = App::new();
    app.add_plugins(DefaultPlugins.set(WindowPlugin {
        primary_window: Some(Window {
            title: format!("traveler — {}", args.path.display()),
            present_mode: if args.vsync {
                PresentMode::AutoVsync
            } else {
                PresentMode::AutoNoVsync
            },
            resolution: WindowResolution::new(1600, 900),
            ..default()
        }),
        ..default()
    }))
    .add_plugins(FreeCameraPlugin)
    .insert_resource(WinitSettings::continuous())
    .insert_resource(ClearColor(Color::srgb(0.043, 0.051, 0.063)))
    .insert_resource(city)
    .insert_resource(lod)
    .insert_resource(Hover::default())
    .insert_resource(Pinned::default())
    .insert_resource({
        let mut e = Edges::new(sets);
        e.selected = args.select;
        if args.dupes {
            e.start_dupe_scan(index_for_dupes);
        }
        e
    })
    .insert_resource(StartEye(start_eye))
    .add_systems(Startup, (spawn_city, hud::setup_hud))
    .add_systems(
        Update,
        (
            (
                lod::update_lod,
                satellite::update_satellite,
                satellite::pick_satellite,
            )
                .chain(),
            hud::update_stats,
            hud::update_hover,
            hud::update_edges_panel,
            controls,
            reset_camera_on_key,
            take_screenshot,
            edges::edge_input,
            edges::poll_dupes,
            (pin::pin_input, pin::update_pin, edges::paint_selection).chain(),
            satellite::draw_threads,
        ),
    );

    if args.pin {
        app.add_systems(Update, pin_from_crosshair);
    }
    if let Some(path) = args.shot {
        app.add_observer(save_to_disk(path.clone()));
        app.insert_resource(ShotPlan {
            path,
            after: args.shot_after,
            elapsed: 0.0,
            taken: false,
            frames: 0,
            measured: 0.0,
        });
    }
    app.run();
}

#[derive(Resource)]
struct StartEye(Option<Vec3>);

/// Waits for the first frames to settle, grabs the window, then quits.
fn take_screenshot(mut commands: Commands, time: Res<Time<Real>>, plan: Option<ResMut<ShotPlan>>) {
    let Some(mut plan) = plan else {
        return;
    };
    plan.elapsed += time.delta_secs();
    // Wall clock, not a frame count. Bevy compiles a mesh pipeline the first time it is used and
    // silently skips the mesh until it is ready, so an early capture shows an empty world.
    if !plan.taken && plan.elapsed > plan.after {
        plan.taken = true;
        // The observer is registered globally in main: 0.19 triggers ScreenshotCaptured as a
        // global event carrying the entity, so an entity-scoped observer never sees it.
        commands.spawn(Screenshot::primary_window());
    }
    // Measure only after the pipelines are warm, so the number is steady state.
    if plan.elapsed > plan.after {
        plan.frames += 1;
        plan.measured += time.delta_secs();
    }
    if plan.elapsed > plan.after + 3.0 {
        println!(
            "shot {}   {:.0} fps over {} frames",
            plan.path.display(),
            plan.frames as f32 / plan.measured.max(1e-6),
            plan.frames
        );
        std::process::exit(0);
    }
}

fn spawn_city(
    mut commands: Commands,
    mut city: ResMut<City>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    start_eye: Res<StartEye>,
) {
    // One shared mesh and one material per category. Phase 0 showed this is what keeps automatic
    // batching engaged; a material per file would kill it.
    let cube = meshes.add(Cuboid::from_size(Vec3::ONE));
    let cats = [
        Category::Code,
        Category::Media,
        Category::Binary,
        Category::Other,
        Category::Directory,
    ];
    let mats: Vec<Handle<StandardMaterial>> = cats
        .iter()
        .map(|c| {
            materials.add(StandardMaterial {
                base_color: c.color(),
                perceptual_roughness: 0.92,
                metallic: 0.0,
                ..default()
            })
        })
        .collect();

    let highlight = materials.add(StandardMaterial {
        base_color: edges::HIGHLIGHT,
        perceptual_roughness: 0.6,
        ..default()
    });
    let pinned = materials.add(StandardMaterial {
        base_color: edges::PINNED,
        perceptual_roughness: 0.6,
        ..default()
    });
    commands.insert_resource(BlockMaterials {
        by_category: mats.clone(),
        highlight: highlight.clone(),
        pinned,
    });

    // Pool for the satellite cluster: spawned once, hidden, repositioned on selection.
    let pool = satellite::spawn_pool(&mut commands, cube.clone(), highlight);
    commands.insert_resource(SatelliteView {
        pool,
        ..Default::default()
    });

    let mut spawned = Vec::with_capacity(city.placements.placed);
    {
        let index = &city.index;
        let places = &city.placements;
        let table = &city.categories;
        for id in index.nodes() {
            if !places.is_visible(id) {
                continue;
            }
            let cat = if index.flags(id).is_dir() {
                Category::Directory
            } else {
                table.of(index.ext(id))
            };
            let entity = commands
                .spawn((
                    Mesh3d(cube.clone()),
                    MeshMaterial3d(mats[cat as usize].clone()),
                    Transform::from_translation(places.center[id.index()])
                        .with_scale(places.size[id.index()]),
                    Visibility::Hidden,
                    FileNode(id),
                ))
                .id();
            spawned.push((id, entity));
        }
    }
    for (id, entity) in spawned {
        city.entities[id.index()] = entity;
    }

    let world = city.params.world_size as f32;

    // Key light plus two fills. The palette was validated as base colors, so the lighting is kept
    // close to flat: a steep key on the top faces, fills to keep the sides from going black.
    commands.spawn((
        DirectionalLight {
            illuminance: 4_000.0,
            shadow_maps_enabled: false,
            ..default()
        },
        Transform::IDENTITY.looking_at(Vec3::new(-0.35, -1.0, -0.5), Vec3::Y),
    ));
    commands.spawn((
        DirectionalLight {
            illuminance: 1_500.0,
            shadow_maps_enabled: false,
            ..default()
        },
        Transform::IDENTITY.looking_at(Vec3::new(0.8, -0.35, 0.6), Vec3::Y),
    ));
    commands.spawn((
        DirectionalLight {
            illuminance: 1_100.0,
            shadow_maps_enabled: false,
            ..default()
        },
        Transform::IDENTITY.looking_at(Vec3::new(0.2, -0.2, -0.9), Vec3::Y),
    ));

    let start = match start_eye.0 {
        Some(eye) => {
            Transform::from_translation(eye).looking_at(Vec3::new(0.0, world * 0.02, 0.0), Vec3::Y)
        }
        None => camera_start(world),
    };
    commands.spawn((
        Camera3d::default(),
        start,
        FreeCamera {
            walk_speed: world * 0.09,
            run_speed: world * 0.4,
            // Space and ctrl rather than the default Q/E. Vertical movement is world-aligned, so
            // space is straight up no matter where the camera points.
            key_up: KeyCode::Space,
            key_down: KeyCode::ControlLeft,
            // Ctrl is the default axis-snap modifier, which would collide with descending.
            key_snap_reverse: KeyCode::AltLeft,
            ..default()
        },
    ));
}

/// Screenshot helper: takes the first thing the crosshair sees and holds it.
fn pin_from_crosshair(hover: Res<Hover>, mut pinned: ResMut<Pinned>, mut done: Local<bool>) {
    if *done {
        return;
    }
    if let Some(node) = hover.node {
        pinned.node = Some(node);
        *done = true;
    }
}

fn camera_start(world: f32) -> Transform {
    Transform::from_xyz(0.0, world * 0.85, world * 1.0)
        .looking_at(Vec3::new(0.0, 0.0, 0.0), Vec3::Y)
}

fn controls(
    keys: Res<ButtonInput<KeyCode>>,
    mut lod: ResMut<Lod>,
    mut city: ResMut<City>,
    mut blocks: Query<(&FileNode, &mut Transform)>,
) {
    if keys.just_pressed(KeyCode::KeyL) {
        lod.enabled = !lod.enabled;
    }
    if keys.just_pressed(KeyCode::BracketLeft) {
        lod.px_threshold = (lod.px_threshold - 4.0).max(2.0);
    }
    if keys.just_pressed(KeyCode::BracketRight) {
        lod.px_threshold = (lod.px_threshold + 4.0).min(400.0);
    }

    if keys.just_pressed(KeyCode::KeyH) {
        city.params.height = match city.params.height {
            HeightMode::LogSize => HeightMode::Uniform,
            HeightMode::Uniform => HeightMode::LogSize,
        };
        city.relayout();
        // Layout is a pure function of the index, so a whole new city is one call plus a pass
        // over the transforms.
        for (node, mut transform) in blocks.iter_mut() {
            let i = node.0.index();
            if city.placements.visible[i] {
                transform.translation = city.placements.center[i];
                transform.scale = city.placements.size[i];
            }
        }
        lod.mark_all_expanded();
    }
}

fn reset_camera_on_key(
    keys: Res<ButtonInput<KeyCode>>,
    city: Res<City>,
    mut camera: Query<&mut Transform, With<Camera3d>>,
) {
    if !keys.just_pressed(KeyCode::KeyR) {
        return;
    }
    if let Ok(mut t) = camera.single_mut() {
        *t = camera_start(city.params.world_size as f32);
    }
}

