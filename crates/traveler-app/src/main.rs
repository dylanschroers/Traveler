//! Traveler: fly through a folder tree.
//!
//! Phase 3, restructured in 6b. A launcher asks what to look at, the work happens on a worker
//! thread, and a `Mode` state machine gates every system so nothing touches a session that does
//! not exist yet.

mod edges;
mod hud;
mod launcher;
mod loading;
mod lod;
mod palette;
mod pin;
mod satellite;
mod scene;
mod space;
mod symbols;

use std::path::PathBuf;
use std::sync::Arc;

use bevy::camera_controller::free_camera::{FreeCamera, FreeCameraPlugin};
use bevy::prelude::*;
use bevy::render::view::screenshot::{Screenshot, save_to_disk};
use bevy::state::state_scoped::DespawnOnExit;
use bevy::window::{PresentMode, WindowResolution};
use bevy::winit::WinitSettings;

use launcher::Launcher;
use loading::Request;
pub use lod::ray_box;
use lod::{Hover, Lod};
use palette::{Category, CategoryTable};
use pin::Pinned;
use satellite::SatelliteView;
use scene::SceneView;
use space::Space;
use traveler_layout::{
    CityLayout, CityParams, HeightMode, Layout, LayoutCtx, Placements, SpaceParams,
};
use traveler_scan::{FileIndex, NodeId, Tree};

/// What the app is currently doing. Every system is gated on one of these, so a resource a mode
/// does not own is never reachable from it.
#[derive(States, Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Mode {
    #[default]
    Launcher,
    Loading,
    Disk,
    Process,
}

/// Command-line settings that outlive any one session, so `loading` can build the next one
/// without going back to `main`.
#[derive(Resource, Clone, Copy)]
pub struct Session {
    pub params: CityParams,
    pub space: SpaceParams,
    pub no_aslr: bool,
    pub lod: bool,
    pub lod_px: f32,
    pub select: Option<usize>,
    pub dupes: bool,
}

/// The block materials, kept so a highlighted node can be put back the way it was. A slot is a
/// file category in disk mode and a permission in process mode; the renderer does not care which.
#[derive(Resource)]
pub struct BlockMaterials {
    pub by_slot: Vec<Handle<StandardMaterial>>,
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
        self.subtree_top = scene::subtree_tops(self.index.as_ref(), &self.placements);
    }
}

impl SceneView for City {
    type Tree = FileIndex;

    fn tree(&self) -> &FileIndex {
        &self.index
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

struct Args {
    /// `None` opens the launcher. A flag short-circuits it, which is what keeps `--shot` and the
    /// rest of the screenshot automation working unchanged.
    request: Option<Request>,
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
    no_aslr: bool,
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
usage: traveler [<path>] [options]

  no path         open the launcher and ask
  --scan <path>   scan a folder, skipping the launcher
  --run <cmd>     launch a program and look inside it. Takes the rest of
                  the line, so other options come before it.
  --attach <pid>  look inside a process that is already running
  --no-aslr       launch with ADDR_NO_RANDOMIZE, so two runs are comparable
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
    parse(&std::env::args().skip(1).collect::<Vec<_>>())
}

fn parse(raw: &[String]) -> Result<Args, String> {
    let mut args = Args {
        request: None,
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
        no_aslr: false,
    };
    let mut i = 0;
    while i < raw.len() {
        match raw[i].as_str() {
            "--scan" => {
                i += 1;
                let path = raw.get(i).ok_or("--scan needs a path")?;
                args.request = Some(Request::Disk(PathBuf::from(path)));
            }
            "--run" => {
                let program = raw.get(i + 1).ok_or("--run needs a command")?;
                args.request = Some(Request::Launch {
                    program: program.into(),
                    args: raw[i + 2..].iter().map(Into::into).collect(),
                    no_aslr: args.no_aslr,
                });
                i = raw.len();
            }
            "--attach" => {
                i += 1;
                let pid = raw
                    .get(i)
                    .and_then(|v| v.parse().ok())
                    .ok_or("--attach needs a pid")?;
                args.request = Some(Request::Attach(pid));
            }
            "--no-aslr" => args.no_aslr = true,
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
            other => args.request = Some(Request::Disk(PathBuf::from(other))),
        }
        i += 1;
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

    let mut app = App::new();
    app.add_plugins(DefaultPlugins.set(WindowPlugin {
        primary_window: Some(Window {
            title: "traveler".to_string(),
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
    .insert_resource(Session {
        params: args.params,
        space: SpaceParams::default(),
        no_aslr: args.no_aslr,
        lod: args.lod,
        lod_px: args.lod_px,
        select: args.select,
        dupes: args.dupes,
    })
    .insert_resource(StartEye(args.shot_eye))
    .init_resource::<Launcher>()
    .insert_state(if args.request.is_some() {
        Mode::Loading
    } else {
        Mode::Launcher
    })
    .add_systems(Startup, setup_camera)
    .add_systems(OnEnter(Mode::Launcher), launcher::setup)
    .add_systems(
        Update,
        (launcher::input, launcher::update)
            .chain()
            .run_if(in_state(Mode::Launcher)),
    )
    .add_systems(OnEnter(Mode::Loading), loading::begin)
    .add_systems(
        Update,
        (loading::update, loading::poll)
            .chain()
            .run_if(in_state(Mode::Loading)),
    )
    .add_systems(
        OnEnter(Mode::Disk),
        (spawn_city, hud::setup_hud, enter_disk),
    )
    .add_systems(OnExit(Mode::Disk), (loading::close_disk, exit_disk))
    .add_systems(
        Update,
        (
            (
                lod::update_lod::<City>,
                satellite::update_satellite,
                satellite::pick_satellite,
            )
                .chain(),
            hud::update_stats::<City>,
            hud::update_hover,
            hud::update_edges_panel,
            height_mode_input,
            reset_camera_on_key::<City>,
            edges::edge_input,
            edges::poll_dupes,
            (
                pin::pin_input,
                pin::update_pin::<City>,
                edges::paint_selection,
            )
                .chain(),
            satellite::draw_threads,
        )
            .run_if(in_state(Mode::Disk)),
    )
    .add_systems(OnEnter(Mode::Process), (space::spawn, space::setup_hud))
    .add_systems(OnExit(Mode::Process), (space::close, exit_disk))
    .add_systems(
        Update,
        (
            (lod::update_lod::<Space>, space::update_pages).chain(),
            space::update_stats,
            space::update_hover,
            space::height_input,
            reset_camera_on_key::<Space>,
            (pin::pin_input, pin::update_pin::<Space>, space::paint_pin).chain(),
        )
            .run_if(in_state(Mode::Process)),
    )
    .add_systems(
        Update,
        (lod_input, take_screenshot).run_if(in_state(Mode::Disk).or_else(in_state(Mode::Process))),
    );

    if let Some(request) = args.request {
        app.insert_resource(request);
    }
    if args.pin {
        app.add_systems(Update, pin_from_crosshair.run_if(in_state(Mode::Disk)));
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
) {
    // One shared mesh and one material per category. Phase 0 showed this is what keeps automatic
    // batching engaged; a material per file would kill it.
    let cube = meshes.add(Cuboid::from_size(Vec3::ONE));
    let block = block_materials(&mut materials);
    let mats = block.by_slot.clone();
    let highlight = block.highlight.clone();
    commands.insert_resource(block);

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
                    DespawnOnExit(Mode::Disk),
                ))
                .id();
            spawned.push((id, entity));
        }
    }
    for (id, entity) in spawned {
        city.entities[id.index()] = entity;
    }

    // Key light plus two fills. The palette was validated as base colors, so the lighting is kept
    // close to flat: a steep key on the top faces, fills to keep the sides from going black.
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
            DespawnOnExit(Mode::Disk),
        ));
    }
}

/// One shared mesh and one material per slot. Phase 0 showed this is what keeps automatic
/// batching engaged; a material per node would kill it.
pub fn block_materials(materials: &mut Assets<StandardMaterial>) -> BlockMaterials {
    let by_slot = [
        Category::Code,
        Category::Media,
        Category::Binary,
        Category::Other,
        Category::Directory,
    ]
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
    BlockMaterials {
        by_slot,
        highlight: materials.add(StandardMaterial {
            base_color: edges::HIGHLIGHT,
            perceptual_roughness: 0.6,
            ..default()
        }),
        pinned: materials.add(StandardMaterial {
            base_color: edges::PINNED,
            perceptual_roughness: 0.6,
            ..default()
        }),
    }
}

/// Space and ctrl rather than the default Q/E. Vertical movement is world-aligned, so space is
/// straight up no matter where the camera points, and ctrl would otherwise collide with the
/// default axis-snap modifier.
pub fn free_camera(world: f32) -> FreeCamera {
    FreeCamera {
        walk_speed: world * 0.09,
        run_speed: world * 0.4,
        key_up: KeyCode::Space,
        key_down: KeyCode::ControlLeft,
        key_snap_reverse: KeyCode::AltLeft,
        ..default()
    }
}

/// The camera outlives every session, so the launcher has something to draw its UI through.
fn setup_camera(mut commands: Commands) {
    commands.spawn((Camera3d::default(), Transform::IDENTITY));
}

/// The free-look controller is a disk-mode thing: in the launcher, W and S are letters.
fn enter_disk(
    mut commands: Commands,
    city: Res<City>,
    start_eye: Res<StartEye>,
    camera: Query<Entity, With<Camera3d>>,
) {
    let world = city.params.world_size as f32;
    let start = match start_eye.0 {
        Some(eye) => {
            Transform::from_translation(eye).looking_at(Vec3::new(0.0, world * 0.02, 0.0), Vec3::Y)
        }
        None => camera_start(world),
    };
    for entity in camera.iter() {
        commands.entity(entity).insert((start, free_camera(world)));
    }
}

fn exit_disk(mut commands: Commands, camera: Query<Entity, With<FreeCamera>>) {
    for entity in camera.iter() {
        commands.entity(entity).remove::<FreeCamera>();
    }
}

/// Screenshot helper: takes the first thing the crosshair sees and holds it.
fn pin_from_crosshair(hover: Res<Hover>, mut pinned: ResMut<Pinned>, mut done: Local<bool>) {
    if *done {
        return;
    }
    if let Some(target) = hover.target {
        pinned.target = Some(target);
        *done = true;
    }
}

pub fn camera_start(world: f32) -> Transform {
    Transform::from_xyz(0.0, world * 0.85, world * 1.0)
        .looking_at(Vec3::new(0.0, 0.0, 0.0), Vec3::Y)
}

fn lod_input(keys: Res<ButtonInput<KeyCode>>, mut lod: ResMut<Lod>) {
    if keys.just_pressed(KeyCode::KeyL) {
        lod.enabled = !lod.enabled;
    }
    if keys.just_pressed(KeyCode::BracketLeft) {
        lod.px_threshold = (lod.px_threshold - 4.0).max(2.0);
    }
    if keys.just_pressed(KeyCode::BracketRight) {
        lod.px_threshold = (lod.px_threshold + 4.0).min(400.0);
    }
}

/// Disk only: the address space's thickness channel is not a log of anything, it is a fixed slab
/// until 6e gives it resident fraction to carry.
fn height_mode_input(
    keys: Res<ButtonInput<KeyCode>>,
    mut lod: ResMut<Lod>,
    mut city: ResMut<City>,
    mut blocks: Query<(&FileNode, &mut Transform)>,
) {
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

fn reset_camera_on_key<S: SceneView>(
    keys: Res<ButtonInput<KeyCode>>,
    scene: Res<S>,
    mut camera: Query<&mut Transform, With<Camera3d>>,
) {
    if !keys.just_pressed(KeyCode::KeyR) {
        return;
    }
    if let Ok(mut t) = camera.single_mut() {
        *t = camera_start(scene.world_size());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy::state::app::StatesPlugin;

    fn argv(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    fn disk_path(args: &Args) -> Option<&std::path::Path> {
        match args.request.as_ref()? {
            Request::Disk(path) => Some(path.as_path()),
            _ => None,
        }
    }

    #[test]
    fn no_arguments_opens_the_launcher() {
        let args = parse(&argv(&[])).expect("parse");
        assert!(args.request.is_none());
    }

    #[test]
    fn a_bare_path_still_goes_straight_to_the_disk_city() {
        let args = parse(&argv(&["/tmp/x"])).expect("parse");
        assert_eq!(disk_path(&args), Some(std::path::Path::new("/tmp/x")));
    }

    #[test]
    fn scan_flag_goes_straight_to_the_disk_city() {
        let args = parse(&argv(&["--scan", "/tmp/x"])).expect("parse");
        assert_eq!(disk_path(&args), Some(std::path::Path::new("/tmp/x")));
    }

    /// The screenshot workflow is a hard requirement of the restructure: every flag it uses has
    /// to survive alongside a target, and the target still has to short-circuit the launcher.
    #[test]
    fn the_screenshot_flags_survive_the_launcher() {
        let args = parse(&argv(&[
            "/tmp/x",
            "--shot",
            "out.png",
            "--shot-eye",
            "1,2,3",
            "--shot-after",
            "5",
            "--select",
            "7",
            "--dupes",
            "--pin",
            "--no-vsync",
            "--no-lod",
            "--uniform",
            "--lod-px",
            "12",
            "--world",
            "500",
        ]))
        .expect("parse");

        assert_eq!(disk_path(&args), Some(std::path::Path::new("/tmp/x")));
        assert_eq!(args.shot.as_deref(), Some(std::path::Path::new("out.png")));
        assert_eq!(args.shot_eye, Some(Vec3::new(1.0, 2.0, 3.0)));
        assert_eq!(args.shot_after, 5.0);
        assert_eq!(args.select, Some(7));
        assert!(args.dupes && args.pin && !args.vsync && !args.lod);
        assert_eq!(args.params.height, HeightMode::Uniform);
        assert_eq!(args.lod_px, 12.0);
        assert_eq!(args.params.world_size, 500.0);
    }

    #[test]
    fn run_takes_the_rest_of_the_line_as_the_targets_own_arguments() {
        let args = parse(&argv(&[
            "--no-aslr",
            "--run",
            "/bin/ls",
            "-l",
            "--scan",
            "/tmp",
        ]))
        .expect("parse");
        match args.request.expect("a request") {
            Request::Launch {
                program,
                args,
                no_aslr,
            } => {
                assert_eq!(program, "/bin/ls");
                // --scan after --run belongs to the target, not to Traveler.
                assert_eq!(args, ["-l", "--scan", "/tmp"]);
                assert!(no_aslr);
            }
            other => panic!("wrong request: {other:?}"),
        }
    }

    #[test]
    fn attach_takes_a_pid() {
        let args = parse(&argv(&["--attach", "1234"])).expect("parse");
        assert!(matches!(args.request, Some(Request::Attach(1234))));
        assert!(parse(&argv(&["--attach", "nope"])).is_err());
    }

    #[test]
    fn an_unknown_flag_is_an_error() {
        assert!(parse(&argv(&["--nope"])).is_err());
    }

    /// Set by an `OnEnter(Disk)` system, which is where `spawn_city` and `setup_hud` run and
    /// where a missing `City` would be a panic rather than a wrong picture.
    #[derive(Resource, Default)]
    struct CityOnEnter(bool);

    fn headless(request: Request) -> App {
        let mut app = App::new();
        app.add_plugins(MinimalPlugins)
            .add_plugins(StatesPlugin)
            .add_plugins(AssetPlugin::default())
            .init_asset::<Mesh>()
            .init_asset::<StandardMaterial>()
            .insert_resource(Session {
                params: CityParams::default(),
                space: SpaceParams::default(),
                no_aslr: false,
                lod: true,
                lod_px: 28.0,
                select: None,
                dupes: false,
            })
            .init_resource::<Launcher>()
            .insert_resource(request)
            .insert_state(Mode::Loading)
            .add_systems(OnEnter(Mode::Loading), loading::begin)
            .add_systems(
                Update,
                (loading::update, loading::poll)
                    .chain()
                    .run_if(in_state(Mode::Loading)),
            )
            .init_resource::<CityOnEnter>()
            .add_systems(
                OnEnter(Mode::Disk),
                |city: Option<Res<City>>, mut seen: ResMut<CityOnEnter>| {
                    seen.0 = city.is_some();
                },
            )
            // The real entry systems, so a panic in them is a failing test rather than a blank
            // window. They need no renderer, only the two asset stores.
            .add_systems(OnEnter(Mode::Process), (space::spawn, space::setup_hud));
        app
    }

    fn run_until(app: &mut App, want: Mode) -> bool {
        for _ in 0..600 {
            app.update();
            if *app.world().resource::<State<Mode>>().get() == want {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        false
    }

    /// The scan is on a worker thread now, so the transition into a session is the thing that can
    /// break: the city has to exist by the time anything gated on `Disk` can run.
    #[test]
    fn a_scan_lands_in_disk_mode_with_a_city() {
        let dir = std::env::temp_dir().join(format!("traveler-mode-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("sub")).expect("fixture");
        std::fs::write(dir.join("a.rs"), b"fn main() {}").expect("fixture");
        std::fs::write(dir.join("sub/b.txt"), b"hello").expect("fixture");

        let mut app = headless(Request::Disk(dir.clone()));
        assert!(run_until(&mut app, Mode::Disk), "never reached disk mode");

        let city = app.world().resource::<City>();
        assert_eq!(city.index.len(), 4);
        assert!(city.placements.placed > 0);
        assert!(app.world().get_resource::<Lod>().is_some());
        assert!(app.world().get_resource::<Hover>().is_some());
        assert!(app.world().get_resource::<loading::Loading>().is_none());
        assert!(
            app.world().resource::<CityOnEnter>().0,
            "OnEnter(Disk) ran before the city was inserted"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    /// The other half of the mode machine: a launched target has to arrive as a laid-out address
    /// space, and the target has to be reachable afterwards so later captures and pokes can find
    /// it. Dropping the app kills the child, since `AttachedTarget` owns what it launched.
    #[test]
    #[cfg(target_os = "linux")]
    fn a_launch_lands_in_process_mode_with_an_address_space() {
        use space::{Space, Traced};

        let mut app = headless(Request::Launch {
            program: "/bin/sleep".into(),
            args: vec!["30".into()],
            no_aslr: true,
        });
        assert!(
            run_until(&mut app, Mode::Process),
            "never reached process mode"
        );

        let space = app.world().resource::<Space>();
        assert!(space.frame.mappings().len() > 3, "too few mappings");
        assert_eq!(space.frame.weight(space.frame.root()) % 4096, 0);
        assert!(space.placements.placed > 0);
        assert!(app.world().resource::<Traced>().0.is_alive());
        // OnEnter(Process) ran for real, so every visible node has a block.
        let space = app.world().resource::<Space>();
        let missing = space
            .frame
            .nodes()
            .filter(|id| space.placements.is_visible(*id))
            .filter(|id| space.entities[id.index()] == Entity::PLACEHOLDER)
            .count();
        assert_eq!(missing, 0, "{missing} placed nodes never got a block");
        assert!(app.world().get_resource::<Lod>().is_some());
        assert!(app.world().get_resource::<City>().is_none());
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn a_launch_that_cannot_start_goes_back_to_the_launcher() {
        let mut app = headless(Request::Launch {
            program: "/nonexistent/program".into(),
            args: vec![],
            no_aslr: false,
        });
        assert!(run_until(&mut app, Mode::Launcher));
        assert!(app.world().resource::<Launcher>().error.is_some());
    }

    #[test]
    fn a_scan_that_fails_goes_back_to_the_launcher_with_the_reason() {
        let missing = std::env::temp_dir().join("traveler-does-not-exist-9c1f");
        let mut app = headless(Request::Disk(missing));
        assert!(
            run_until(&mut app, Mode::Launcher),
            "never returned to the launcher"
        );
        assert!(app.world().resource::<Launcher>().error.is_some());
        assert!(app.world().get_resource::<City>().is_none());
    }
}
