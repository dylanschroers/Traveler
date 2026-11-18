//! The activity timeline: a recorded run as a branching tree along a time axis.
//!
//! Threads are strands that branch off the main thread. Call paths are tendrils off their strand:
//! distance is stack depth below the thread's trunk, direction is the library, brightness is the
//! share of time the path was on the stack. Drawing is tiled by zoom level, so cost follows what
//! is on screen rather than how long the recording is.

mod explain;
mod inspect;
mod scene;
mod tiles;

use std::path::{Path, PathBuf};
use std::time::Instant;

use bevy::camera_controller::free_camera::{FreeCamera, FreeCameraPlugin};
use bevy::camera::RenderTarget;
use bevy::post_process::bloom::Bloom;
use bevy::render::render_resource::{TextureFormat, TextureUsages};
use bevy::prelude::*;
use bevy::render::view::Msaa;
use bevy::render::view::screenshot::{Screenshot, save_to_disk};
use bevy::window::{PresentMode, WindowResolution};
use bevy::winit::WinitSettings;
use traveler_trace::{Levels, samply, synth};

use scene::{Scene, SceneRef};
use tiles::Tiles;

const USAGE: &str = "usage: traveler-timeline [options] --record PROGRAM [ARGS...]
       traveler-timeline [options] --profile RECORDING.json.gz
       traveler-timeline [options] --synth THREADS SECONDS

  --record runs PROGRAM under samply, saves the recording, and opens it. It takes the rest of the
  line, so options come before it.

options: [--budget MAX_SPANS] [--px N] [--bench] [--shot FILE] [--vsync] [--no-bloom] [--offscreen] [--near] [--pick X Y] [--explain]";

enum Source {
    Synth { threads: usize, seconds: f64 },
    Profile(PathBuf),
    Record(Vec<String>),
}

struct Args {
    source: Source,
    budget: usize,
    px: f32,
    bench: bool,
    shot: Option<PathBuf>,
    vsync: bool,
    bloom: bool,
    offscreen: bool,
    near: bool,
    pick: Option<Vec2>,
    inspect: bool,
    msaa: u32,
    scale1: bool,
    explain: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args { source: Source::Synth { threads: 16, seconds: 60.0 }, budget: 150_000, px: 1.5, bench: false, shot: None, vsync: false, bloom: true, offscreen: false, near: false, pick: None, inspect: true, msaa: 4, scale1: false, explain: false };
    let mut it = std::env::args().skip(1);
    let num = |v: Option<String>, what: &str| -> Result<f64, String> {
        v.ok_or(format!("{what} needs a value"))?.replace('_', "").parse().map_err(|_| format!("{what} needs a number"))
    };
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--synth" => {
                let threads = num(it.next(), "--synth")? as usize;
                a.source = Source::Synth { threads, seconds: num(it.next(), "--synth")? };
            }
            "--profile" => a.source = Source::Profile(it.next().ok_or("--profile needs a file")?.into()),
            "--record" => {
                let cmd: Vec<String> = it.by_ref().collect();
                if cmd.is_empty() {
                    return Err("--record needs a program to run".into());
                }
                a.source = Source::Record(cmd);
            }
            "--budget" => a.budget = num(it.next(), "--budget")? as usize,
            "--px" => a.px = num(it.next(), "--px")? as f32,
            "--bench" => a.bench = true,
            "--shot" => a.shot = Some(it.next().ok_or("--shot needs a file")?.into()),
            "--vsync" => a.vsync = true,
            "--no-bloom" => a.bloom = false,
            "--offscreen" => a.offscreen = true,
            "--near" => a.near = true,
            "--no-inspect" => a.inspect = false,
            "--msaa" => a.msaa = num(it.next(), "--msaa")? as u32,
            "--scale1" => a.scale1 = true,
            "--explain" => a.explain = true,
            "--pick" => {
                let x = num(it.next(), "--pick")? as f32;
                a.pick = Some(Vec2::new(x, num(it.next(), "--pick")? as f32));
            }
            "-h" | "--help" => return Err(String::new()),
            other => return Err(format!("unknown argument {other}")),
        }
    }
    Ok(a)
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

    let t = Instant::now();
    let loaded = match &args.source {
        Source::Synth { threads, seconds } => {
            Ok(synth::generate(&synth::Params { threads: *threads, seconds: *seconds, ..Default::default() }))
        }
        Source::Profile(path) => samply::load(path),
        Source::Record(cmd) => record(cmd).and_then(|path| samply::load(&path)),
    };
    let trace = match loaded {
        Ok(t) => t,
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    };
    let gen_s = t.elapsed().as_secs_f64();
    let t = Instant::now();
    let levels = Levels::build(&trace, 0.001);
    println!(
        "{}: {} threads, {:.1} s, {} samples, {} distinct call paths   (loaded {:.2} s, zoom levels {:.2} s)",
        trace.name,
        trace.threads.len(),
        trace.duration,
        trace.samples(),
        trace.paths(),
        gen_s,
        t.elapsed().as_secs_f64()
    );
    let scene = Scene::new(trace, levels);

    let mut app = App::new();
    app.add_plugins(DefaultPlugins.set(WindowPlugin {
        primary_window: Some(Window {
            title: "traveler timeline".into(),
            present_mode: if args.vsync { PresentMode::AutoVsync } else { PresentMode::AutoNoVsync },
            resolution: if args.scale1 {
                WindowResolution::new(1600, 900).with_scale_factor_override(1.0)
            } else {
                WindowResolution::new(1600, 900)
            },
            ..default()
        }),
        ..default()
    }))
    .insert_resource(WinitSettings::continuous())
    .insert_resource(ClearColor(Color::srgb(0.016, 0.024, 0.06)))
    .insert_resource(Tiles::new(args.budget, args.px))
    .insert_resource(Cursor(scene.trace.duration as f32 * 0.42))
    .insert_resource(SceneRef(std::sync::Arc::new(scene)))
    .insert_resource(Fps::default())
    .insert_resource(UseBloom(args.bloom))
    .insert_resource(MsaaSetting(args.msaa))
    .insert_resource(Near(args.near))
    .insert_resource(inspect::Inspect::new(args.pick))
    .insert_resource(explain::Explain::new(args.explain))
    .insert_resource(Offscreen(args.offscreen.then(Handle::default)))
    .add_systems(Startup, (setup, setup_hud).chain())
    .add_systems(PostStartup, inspect::target_ui)
    .add_systems(Update, (
            tiles::adapt_budget,
            tiles::update_tiles,
            cursor_input,
            update_cursor,
            update_hud,
            track_fps,
        )
            .chain());

    if args.inspect {
        app.add_systems(Startup, inspect::setup.after(setup_hud)).add_systems(
            Update,
            (inspect::pick, inspect::highlight, inspect::panels, inspect::thread_labels, explain::update, explain::panel).chain().after(tiles::update_tiles).before(cursor_input),
        );
    }
    if args.bench {
        app.insert_resource(Bench::new()).add_systems(Update, drive_bench);
    } else {
        app.add_plugins(FreeCameraPlugin);
    }
    if let Some(path) = args.shot {
        app.add_observer(save_to_disk(path.clone()));
        app.insert_resource(Shot { path, elapsed: 0.0, taken: false });
        app.add_systems(Update, take_shot);
    }
    app.run();
}

/// Runs `cmd` under samply and returns where the recording went. The recording is kept, so it can
/// be reopened with `--profile` without running the program again.
fn record(cmd: &[String]) -> Result<PathBuf, String> {
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    let program = Path::new(&cmd[0]).file_name().map_or("program".into(), |n| n.to_string_lossy().into_owned());
    let out = std::env::temp_dir().join(format!("traveler-{program}-{stamp}.json.gz"));
    println!("recording {} with samply ...", cmd.join(" "));
    let status = std::process::Command::new("samply")
        .args(["record", "--save-only", "--unstable-presymbolicate", "-o"])
        .arg(&out)
        .arg("--")
        .args(cmd)
        .status()
        .map_err(|e| format!("could not start samply ({e}); install it with: cargo install --locked samply"))?;
    if !out.exists() {
        return Err(format!("samply exited with {status} and wrote no recording"));
    }
    println!("recording saved to {}", out.display());
    Ok(out)
}

fn setup(
    mut commands: Commands,
    scene: Res<SceneRef>,
    bench: Option<Res<Bench>>,
    use_bloom: Res<UseBloom>,
    msaa: Res<MsaaSetting>,
    near: Res<Near>,
    cursor: Res<Cursor>,
    mut offscreen: ResMut<Offscreen>,
    mut images: ResMut<Assets<Image>>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    let len = scene.length();
    let eye = if near.0 {
        let x = scene.x_of(cursor.0);
        Transform::from_xyz(x - 40.0, 22.0, 34.0).looking_at(Vec3::new(x + 25.0, 0.0, 0.0), Vec3::Y)
    } else {
        Transform::from_xyz(-0.12 * len, 0.22 * len, 0.36 * len).looking_at(Vec3::new(0.35 * len, 0.0, 0.0), Vec3::Y)
    };
    let cam = (Camera3d::default(), eye);
    let id = if bench.is_some() {
        commands.spawn(cam).id()
    } else {
        commands.spawn((
            cam,
            FreeCamera {
                walk_speed: len * 0.03,
                run_speed: len * 0.15,
                key_up: KeyCode::Space,
                key_down: KeyCode::ControlLeft,
                key_snap_reverse: KeyCode::AltLeft,
                ..default()
            },
        )).id()
    };
    if offscreen.0.is_some() {
        let mut image = Image::new_target_texture(1600, 900, TextureFormat::Bgra8UnormSrgb, None);
        image.texture_descriptor.usage |= TextureUsages::COPY_SRC;
        let handle = images.add(image);
        commands.entity(id).insert(RenderTarget::Image(handle.clone().into()));
        offscreen.0 = Some(handle);
    }
    commands.entity(id).insert(match msaa.0 {
        1 => Msaa::Off,
        2 => Msaa::Sample2,
        8 => Msaa::Sample8,
        _ => Msaa::Sample4,
    });
    if use_bloom.0 {
        commands.entity(id).insert(Bloom { intensity: 0.28, ..Bloom::NATURAL });
    }

    let strand_mat = materials.add(StandardMaterial {
        base_color: Color::linear_rgb(0.7, 0.95, 1.4),
        unlit: true,
        alpha_mode: AlphaMode::Add,
        ..default()
    });
    for thread in 0..scene.trace.threads.len() {
        commands.spawn((Mesh3d(meshes.add(scene.strand_mesh(thread))), MeshMaterial3d(strand_mat.clone())));
    }
    commands.spawn((
        Mesh3d(meshes.add(Sphere::new(scene.strand_radius() * 3.0))),
        MeshMaterial3d(materials.add(StandardMaterial { base_color: Color::linear_rgb(6.0, 7.0, 8.0), unlit: true, ..default() })),
        Transform::from_translation(scene.lane_at(0, 0.0)),
    ));
    commands.spawn((
        Mesh3d(meshes.add(Torus { minor_radius: scene.strand_radius() * 0.6, major_radius: scene.ring_radius() })),
        MeshMaterial3d(materials.add(StandardMaterial {
            base_color: Color::linear_rgb(0.5, 1.6, 2.2),
            unlit: true,
            alpha_mode: AlphaMode::Add,
            ..default()
        })),
        Transform::from_rotation(Quat::from_rotation_z(std::f32::consts::FRAC_PI_2)),
        CursorRing,
    ));
}

#[derive(Resource)]
struct UseBloom(bool);

#[derive(Resource)]
struct MsaaSetting(u32);

/// Start close to the main strand at the cursor instead of with the whole run in view.
#[derive(Resource)]
struct Near(bool);

/// Render into an image instead of the window, so screenshots and timings do not depend on
/// the display being awake.
#[derive(Resource)]
pub(crate) struct Offscreen(pub(crate) Option<Handle<Image>>);

/// The moment the ring marks, in seconds from the start of the run.
#[derive(Resource)]
pub(crate) struct Cursor(pub(crate) f32);

#[derive(Component)]
struct CursorRing;

fn cursor_input(
    keys: Res<ButtonInput<KeyCode>>,
    time: Res<Time<Real>>,
    scene: Res<SceneRef>,
    camera: Query<&GlobalTransform, With<Camera3d>>,
    mut cursor: ResMut<Cursor>,
) {
    let dur = scene.trace.duration as f32;
    let speed = if keys.pressed(KeyCode::ShiftLeft) { 0.2 } else { 0.03 } * dur;
    if keys.pressed(KeyCode::Period) {
        cursor.0 += speed * time.delta_secs();
    }
    if keys.pressed(KeyCode::Comma) {
        cursor.0 -= speed * time.delta_secs();
    }
    if keys.just_pressed(KeyCode::KeyC)
        && let Ok(cam) = camera.single()
    {
        cursor.0 = scene.time_at_x(cam.translation().x + cam.forward().x * 30.0);
    }
    cursor.0 = cursor.0.clamp(0.0, dur);
}

fn update_cursor(cursor: Res<Cursor>, scene: Res<SceneRef>, mut ring: Query<&mut Transform, With<CursorRing>>) {
    if let Ok(mut tf) = ring.single_mut() {
        tf.translation = Vec3::new(scene.x_of(cursor.0), 0.0, 0.0);
    }
}

#[derive(Component)]
struct InfoText;
#[derive(Component)]
struct StackText;

fn font(size: f32) -> TextFont {
    TextFont { font_size: FontSize::Px(size), ..default() }
}

fn setup_hud(mut commands: Commands) {
    let panel = |top: Option<f32>, bottom: Option<f32>| Node {
        position_type: PositionType::Absolute,
        left: Val::Px(12.0),
        top: top.map_or(Val::Auto, Val::Px),
        bottom: bottom.map_or(Val::Auto, Val::Px),
        padding: UiRect::all(Val::Px(8.0)),
        ..default()
    };
    let bg = BackgroundColor(Color::srgba(0.02, 0.03, 0.08, 0.78));
    commands.spawn((panel(Some(12.0), None), bg, Text::new(""), font(13.0), TextColor(Color::srgb(0.85, 0.9, 1.0)), InfoText));
    commands.spawn((panel(None, Some(12.0)), bg, Text::new(""), font(12.0), TextColor(Color::srgb(0.75, 0.82, 0.98)), StackText));
}

#[derive(Resource, Default)]
struct Fps {
    smooth: f32,
}

fn track_fps(time: Res<Time<Real>>, mut fps: ResMut<Fps>) {
    let dt = time.delta_secs().max(1e-4);
    fps.smooth = if fps.smooth == 0.0 { 1.0 / dt } else { fps.smooth * 0.95 + 0.05 / dt };
}

fn update_hud(
    scene: Res<SceneRef>,
    tiles: Res<Tiles>,
    cursor: Res<Cursor>,
    fps: Res<Fps>,
    mut info: Query<&mut Text, (With<InfoText>, Without<StackText>)>,
    mut stacks: Query<&mut Text, (With<StackText>, Without<InfoText>)>,
) {
    let tr = &scene.trace;
    if let Ok(mut text) = info.single_mut() {
        inspect::set_text(&mut text, format!(
            "{}   {} threads   {:.1} s   {} samples   {} call paths\n\
             {:.0} fps   {} spans drawn, budget {} of max {} (follows frame time)   {} tiles shown, {} cached, {} building   detail {:.1} px\n\
             WASD fly   space/ctrl up-down   shift run   right-drag look   hover or click a line, strand or the ring to inspect   drag the ring to scrub\n\
             , . move the moment   C moment here   X explain selected function   esc clear selection",
            tr.name,
            tr.threads.len(),
            tr.duration,
            tr.samples(),
            tr.paths(),
            fps.smooth,
            tiles.drawn,
            tiles.budget,
            tiles.max_budget,
            tiles.shown,
            tiles.cached(),
            tiles.in_flight,
            tiles.used_px,
        ));
    }
    if let Ok(mut text) = stacks.single_mut() {
        let t = cursor.0;
        inspect::set_text(&mut text, format!("AT THE RING, {t:.2} s IN\n{}", inspect::moment_lines(&scene, t, 6)));
    }
}

#[derive(Resource)]
struct Shot {
    path: PathBuf,
    elapsed: f32,
    taken: bool,
}

fn take_shot(
    mut commands: Commands,
    time: Res<Time<Real>>,
    mut shot: ResMut<Shot>,
    fps: Res<Fps>,
    offscreen: Res<Offscreen>,
    explain: Res<explain::Explain>,
) {
    if explain.busy() {
        return;
    }
    shot.elapsed += time.delta_secs();
    if !shot.taken && shot.elapsed > 4.0 {
        shot.taken = true;
        match &offscreen.0 {
            Some(h) => commands.spawn(Screenshot::image(h.clone())),
            None => commands.spawn(Screenshot::primary_window()),
        };
    }
    if shot.elapsed > 6.0 {
        println!("shot {}   {:.0} fps", shot.path.display(), fps.smooth);
        std::process::exit(0);
    }
}

#[derive(PartialEq, Clone, Copy)]
enum Phase {
    Warmup,
    Overview,
    Settle,
    Close,
    Done,
}

#[derive(Resource)]
struct Bench {
    t: f32,
    phase: Phase,
    overview: Vec<f32>,
    close: Vec<f32>,
    drawn: Vec<usize>,
    shown: Vec<usize>,
}

impl Bench {
    fn new() -> Self {
        Self { t: 0.0, phase: Phase::Warmup, overview: Vec::new(), close: Vec::new(), drawn: Vec::new(), shown: Vec::new() }
    }
}

/// Overview: the whole run in view, orbiting. Close: low along the main strand, travelling
/// forward through time, which is what flying around looks like and what churns tiles.
fn drive_bench(
    time: Res<Time<Real>>,
    scene: Res<SceneRef>,
    tiles: Res<Tiles>,
    mut bench: ResMut<Bench>,
    mut cam: Query<&mut Transform, With<Camera3d>>,
) {
    let dt = time.delta_secs();
    bench.t += dt;
    let t = bench.t;
    bench.phase = match t {
        t if t < 3.0 => Phase::Warmup,
        t if t < 11.0 => Phase::Overview,
        t if t < 12.5 => Phase::Settle,
        t if t < 20.5 => Phase::Close,
        _ => Phase::Done,
    };
    let len = scene.length();
    let Ok(mut tf) = cam.single_mut() else { return };
    match bench.phase {
        Phase::Warmup | Phase::Overview => {
            let a = t * 0.1;
            let c = Vec3::new(0.5 * len, 0.0, 0.0);
            *tf = Transform::from_translation(c + Vec3::new(a.sin() * 0.6 * len, 0.22 * len, a.cos() * 0.6 * len)).looking_at(c, Vec3::Y);
        }
        Phase::Settle | Phase::Close => {
            let x = 0.2 * len + (t - 11.0).max(0.0) * 0.03 * len;
            *tf = Transform::from_xyz(x - 25.0, 18.0, 30.0).looking_at(Vec3::new(x + 40.0, 0.0, 0.0), Vec3::Y);
        }
        Phase::Done => {}
    }
    match bench.phase {
        Phase::Overview => {
            bench.overview.push(dt);
            bench.drawn.push(tiles.drawn);
        }
        Phase::Close => {
            bench.close.push(dt);
            bench.drawn.push(tiles.drawn);
            bench.shown.push(tiles.shown);
        }
        Phase::Done => {
            let mut o = bench.overview.clone();
            let mut c = bench.close.clone();
            let avg_drawn = bench.drawn.iter().sum::<usize>() / bench.drawn.len().max(1);
            println!("=== RESULT: budget max {} spans (ended at {}), {} tiles built off the main thread ===", tiles.max_budget, tiles.budget, tiles.built);
            stats("overview", &mut o);
            stats("close", &mut c);
            let avg_shown = bench.shown.iter().sum::<usize>() / bench.shown.len().max(1);
            println!("  spans drawn {avg_drawn} avg, tiles on screen while close {avg_shown} avg\n");
            std::process::exit(0);
        }
        _ => {}
    }
}

fn stats(label: &str, frames: &mut [f32]) {
    if frames.is_empty() {
        println!("  {label:<9} no frames");
        return;
    }
    frames.sort_by(f32::total_cmp);
    let n = frames.len();
    let mean = frames.iter().sum::<f32>() / n as f32;
    let p = |q: f32| frames[((n as f32 * q) as usize).min(n - 1)] * 1000.0;
    println!(
        "  {label:<9} {:>6.1} fps avg   frame ms p50 {:>6.2}  p95 {:>6.2}  p99 {:>6.2}   ({n} frames)",
        1.0 / mean,
        p(0.5),
        p(0.95),
        p(0.99)
    );
}
