//! Phase 0 spike.
//!
//! Question: can this machine draw a treemap city of N boxes at an interactive framerate,
//! using stock Bevy mesh rendering (one shared mesh, a handful of materials, automatic batching)?
//!
//! Two camera profiles are measured, because they stress different things:
//!   OVERVIEW - the whole city in frustum. Worst case for draw submission.
//!   STREET   - ground level, most of the city behind the camera. Typical case, tests culling.
//!
//! Usage:
//!   cargo run --release -- [--count N] [--materials M] [--shadows] [--vsync] [--interactive]

use std::time::Instant;

use bevy::{
    camera_controller::free_camera::{FreeCamera, FreeCameraPlugin},
    diagnostic::{FrameTimeDiagnosticsPlugin, LogDiagnosticsPlugin},
    light::NotShadowCaster,
    prelude::*,
    window::{PresentMode, WindowResolution},
    winit::WinitSettings,
};

#[derive(Resource, Clone)]
struct Cfg {
    count: usize,
    materials: usize,
    shadows: bool,
    vsync: bool,
    interactive: bool,
    warmup: f32,
    measure: f32,
}

impl Default for Cfg {
    fn default() -> Self {
        Self {
            count: 1_000_000,
            materials: 12,
            shadows: false,
            vsync: false,
            interactive: false,
            warmup: 3.0,
            measure: 8.0,
        }
    }
}

impl Cfg {
    fn from_args() -> Self {
        let mut cfg = Cfg::default();
        let args: Vec<String> = std::env::args().skip(1).collect();
        let mut i = 0;
        while i < args.len() {
            match args[i].as_str() {
                "--count" => {
                    i += 1;
                    cfg.count = args[i].replace('_', "").parse().expect("--count needs a number");
                }
                "--materials" => {
                    i += 1;
                    cfg.materials = args[i].parse().expect("--materials needs a number");
                }
                "--warmup" => {
                    i += 1;
                    cfg.warmup = args[i].parse().expect("--warmup needs seconds");
                }
                "--measure" => {
                    i += 1;
                    cfg.measure = args[i].parse().expect("--measure needs seconds");
                }
                "--shadows" => cfg.shadows = true,
                "--vsync" => cfg.vsync = true,
                "--interactive" => cfg.interactive = true,
                other => panic!("unknown arg: {other}"),
            }
            i += 1;
        }
        cfg
    }
}

/// City is a `side x side` grid of unit-spaced footprints centred on the origin.
fn side_of(count: usize) -> f32 {
    (count as f32).sqrt().ceil()
}

/// Cheap deterministic hash so the spike needs no rng dependency.
fn hash01(i: u64, salt: u64) -> f32 {
    let mut x = i.wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ salt;
    x ^= x >> 33;
    x = x.wrapping_mul(0xff51_afd7_ed55_8ccd);
    x ^= x >> 33;
    x = x.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    x ^= x >> 33;
    (x >> 40) as f32 / 16_777_216.0
}

fn main() {
    let cfg = Cfg::from_args();
    println!(
        "\n=== traveler phase 0 spike ===\ncount={} materials={} shadows={} vsync={} mode={}",
        cfg.count,
        cfg.materials,
        cfg.shadows,
        cfg.vsync,
        if cfg.interactive { "interactive" } else { "bench" }
    );

    let mut app = App::new();
    app.add_plugins((
        DefaultPlugins.set(WindowPlugin {
            primary_window: Some(Window {
                title: "traveler spike".into(),
                present_mode: if cfg.vsync {
                    PresentMode::AutoVsync
                } else {
                    PresentMode::AutoNoVsync
                },
                resolution: WindowResolution::new(1600, 900).with_scale_factor_override(1.0),
                ..default()
            }),
            ..default()
        }),
        FrameTimeDiagnosticsPlugin::default(),
        LogDiagnosticsPlugin::default(),
    ))
    .insert_resource(WinitSettings::continuous())
    .insert_resource(cfg.clone())
    .insert_resource(Bench::new())
    .add_systems(Startup, setup);

    if cfg.interactive {
        app.add_plugins(FreeCameraPlugin);
    } else {
        app.add_systems(Update, (drive_bench, sample).chain());
    }

    app.run();
}

fn setup(
    mut commands: Commands,
    cfg: Res<Cfg>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    let side = side_of(cfg.count);
    let half = side * 0.5;

    // One shared mesh and a small fixed palette. This is the whole point: batching only
    // engages if every block shares a mesh handle and draws from few material handles.
    let cube = meshes.add(Cuboid::from_size(Vec3::ONE));
    let palette: Vec<Handle<StandardMaterial>> = (0..cfg.materials.max(1))
        .map(|i| {
            let hue = 360.0 * i as f32 / cfg.materials.max(1) as f32;
            materials.add(StandardMaterial {
                base_color: Color::hsl(hue, 0.55, 0.55),
                perceptual_roughness: 0.85,
                ..default()
            })
        })
        .collect();

    let count = cfg.count;
    let n_mat = palette.len();
    let blocks = (0..count).map(move |i| {
        let x = (i % side as usize) as f32 - half;
        let z = (i / side as usize) as f32 - half;

        // Heavy-tailed heights, standing in for the real file size distribution.
        let u = hash01(i as u64, 0xa5a5).min(0.999_9);
        let h = (0.15 * (1.0 - u).powf(-0.45)).clamp(0.15, 80.0);
        let m = (hash01(i as u64, 0x1234) * n_mat as f32) as usize % n_mat;

        (
            Mesh3d(cube.clone()),
            MeshMaterial3d(palette[m].clone()),
            Transform::from_xyz(x, h * 0.5, z).with_scale(Vec3::new(0.85, h, 0.85)),
        )
    });

    if cfg.shadows {
        commands.spawn_batch(blocks);
    } else {
        commands.spawn_batch(blocks.map(|b| (b, NotShadowCaster)));
    }

    // Ground slab, one entity, deliberately not a Plane3d so this uses no extra API surface.
    let ground = materials.add(StandardMaterial {
        base_color: Color::srgb(0.06, 0.07, 0.09),
        perceptual_roughness: 1.0,
        ..default()
    });
    commands.spawn((
        Mesh3d(meshes.add(Cuboid::from_size(Vec3::new(side * 1.4, 0.5, side * 1.4)))),
        MeshMaterial3d(ground),
        Transform::from_xyz(0.0, -0.25, 0.0),
        NotShadowCaster,
    ));

    // Key light, plus an unshadowed fill so the dark sides of blocks stay readable.
    commands.spawn((
        DirectionalLight {
            shadow_maps_enabled: cfg.shadows,
            illuminance: 12_000.0,
            ..default()
        },
        Transform::IDENTITY.looking_at(Vec3::new(-0.4, -1.0, -0.6), Vec3::Y),
    ));
    commands.spawn((
        DirectionalLight {
            shadow_maps_enabled: false,
            illuminance: 4_000.0,
            ..default()
        },
        Transform::IDENTITY.looking_at(Vec3::new(0.7, -0.4, 0.8), Vec3::Y),
    ));

    let cam = (
        Camera3d::default(),
        Transform::from_xyz(half * 0.9, side * 0.30, half * 1.25)
            .looking_at(Vec3::new(0.0, 10.0, 0.0), Vec3::Y),
    );
    if cfg.interactive {
        commands.spawn((
            cam,
            FreeCamera {
                walk_speed: side * 0.08,
                run_speed: side * 0.35,
                // Space/ctrl instead of the default Q/E. Vertical movement is world-aligned by
                // default, so space is straight up no matter where the camera is looking.
                key_up: KeyCode::Space,
                key_down: KeyCode::ControlLeft,
                // Ctrl is the default axis-snap modifier, which would now collide with descending.
                key_snap_reverse: KeyCode::AltLeft,
                ..default()
            },
        ));
    } else {
        commands.spawn(cam);
    }
}

#[derive(PartialEq, Clone, Copy, Debug)]
enum Phase {
    WarmupOverview,
    Overview,
    SettleStreet,
    Street,
    Done,
}

#[derive(Resource)]
struct Bench {
    launched: Instant,
    first_frame_ms: Option<f64>,
    t: f32,
    phase: Phase,
    overview: Vec<f32>,
    street: Vec<f32>,
}

impl Bench {
    fn new() -> Self {
        Self {
            launched: Instant::now(),
            first_frame_ms: None,
            t: 0.0,
            phase: Phase::WarmupOverview,
            overview: Vec::with_capacity(4096),
            street: Vec::with_capacity(4096),
        }
    }
}

/// Advances the scripted camera and the phase state machine.
fn drive_bench(
    time: Res<Time<Real>>,
    cfg: Res<Cfg>,
    mut bench: ResMut<Bench>,
    mut cam: Query<&mut Transform, With<Camera3d>>,
) {
    if bench.phase == Phase::Done {
        return;
    }
    if bench.first_frame_ms.is_none() {
        let ms = bench.launched.elapsed().as_secs_f64() * 1000.0;
        bench.first_frame_ms = Some(ms);
        println!("[spike] first frame after {ms:.0} ms (process start to first present)");
    }

    bench.t += time.delta_secs();
    let (w, m) = (cfg.warmup, cfg.measure);
    bench.phase = if bench.t < w {
        Phase::WarmupOverview
    } else if bench.t < w + m {
        Phase::Overview
    } else if bench.t < w + m + 1.5 {
        Phase::SettleStreet
    } else if bench.t < w + m + 1.5 + m {
        Phase::Street
    } else {
        Phase::Done
    };

    let side = side_of(cfg.count);
    let half = side * 0.5;
    let Ok(mut tf) = cam.single_mut() else {
        return;
    };

    match bench.phase {
        Phase::WarmupOverview | Phase::Overview => {
            // Slow orbit so nothing is a static-scene best case.
            let a = bench.t * 0.12;
            *tf = Transform::from_xyz(a.sin() * half * 1.3, side * 0.30, a.cos() * half * 1.3)
                .looking_at(Vec3::new(0.0, 8.0, 0.0), Vec3::Y);
        }
        Phase::SettleStreet | Phase::Street => {
            // Ground level, flying down an avenue.
            let travel = (bench.t - (cfg.warmup + cfg.measure)) * side * 0.05;
            *tf = Transform::from_xyz(-half + travel, 6.0, 0.0)
                .looking_at(Vec3::new(-half + travel + 40.0, 3.0, 6.0), Vec3::Y);
        }
        Phase::Done => {}
    }
}

fn sample(time: Res<Time<Real>>, cfg: Res<Cfg>, mut bench: ResMut<Bench>) {
    let dt = time.delta_secs();
    match bench.phase {
        Phase::Overview => bench.overview.push(dt),
        Phase::Street => bench.street.push(dt),
        Phase::Done => {
            report(&cfg, &bench);
            // Blunt, but a spike does not need a clean shutdown path and this avoids
            // betting on the app-exit API surface.
            std::process::exit(0);
        }
        _ => {}
    }
}

fn stats(label: &str, frames: &mut [f32]) {
    if frames.is_empty() {
        println!("  {label:<10} no frames recorded");
        return;
    }
    frames.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = frames.len();
    let mean: f32 = frames.iter().sum::<f32>() / n as f32;
    let p = |q: f32| frames[((n as f32 * q) as usize).min(n - 1)] * 1000.0;
    println!(
        "  {label:<10} {:>6.1} fps avg   frame ms p50 {:>6.2}  p95 {:>6.2}  p99 {:>6.2}   ({n} frames)",
        1.0 / mean,
        p(0.50),
        p(0.95),
        p(0.99)
    );
}

fn report(cfg: &Cfg, bench: &Bench) {
    let mut o = bench.overview.clone();
    let mut s = bench.street.clone();
    println!("\n=== RESULT: {} blocks, {} materials, shadows={} ===", cfg.count, cfg.materials, cfg.shadows);
    if let Some(ms) = bench.first_frame_ms {
        println!("  startup    {ms:.0} ms to first frame");
    }
    stats("overview", &mut o);
    stats("street", &mut s);
    println!();
}
