//! The work between "what to look at" and a city on screen, done off the main thread.
//!
//! One request in, one session out. The worker owns the slow part and drops its answer into a
//! slot polled each frame, which is the pattern `edges::DupeState` already uses; nothing here
//! blocks a frame.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use bevy::prelude::*;
use bevy::state::state_scoped::DespawnOnExit;
use traveler_edges::{EdgeProvider, ExtensionGroups, NameFamilies};
use traveler_layout::Placements;
use traveler_scan::{FileIndex, ScanOptions, Tree, human_bytes, scan};

use crate::edges::Edges;
use crate::hud::{INK_SECONDARY, PANEL, font};
use crate::launcher::Launcher;
use crate::lod::{Hover, Lod};
use crate::palette::CategoryTable;
use crate::pin::Pinned;
use crate::satellite::SatelliteView;
use crate::{BlockMaterials, City, Mode, Session};

/// What the launcher, or a command-line flag, asked for. Inserted before entering `Loading`.
#[derive(Resource, Clone, Debug)]
pub enum Request {
    Disk(PathBuf),
}

enum Loaded {
    Disk(FileIndex),
}

type Outcome = Result<Loaded, String>;

#[derive(Resource)]
pub struct Loading {
    slot: Arc<Mutex<Option<Outcome>>>,
    started: Instant,
    label: String,
}

#[derive(Component)]
pub struct ProgressText;

pub fn begin(mut commands: Commands, request: Option<Res<Request>>) {
    let Some(request) = request else {
        return;
    };
    let request = request.clone();
    let label = match &request {
        Request::Disk(path) => format!("scanning {}", path.display()),
    };
    println!("{label} ...");

    let slot: Arc<Mutex<Option<Outcome>>> = Arc::new(Mutex::new(None));
    let sink = slot.clone();
    std::thread::spawn(move || {
        let outcome = match request {
            Request::Disk(path) => scan(&path, &ScanOptions::default())
                .map(Loaded::Disk)
                .map_err(|e| format!("cannot scan {}: {e}", path.display())),
        };
        if let Ok(mut guard) = sink.lock() {
            *guard = Some(outcome);
        }
    });

    commands.insert_resource(Loading {
        slot,
        started: Instant::now(),
        label,
    });
    commands.remove_resource::<Request>();

    commands.spawn((
        Node {
            position_type: PositionType::Absolute,
            left: Val::Percent(50.0),
            top: Val::Percent(45.0),
            margin: UiRect {
                left: Val::Px(-380.0),
                ..default()
            },
            width: Val::Px(760.0),
            padding: UiRect::all(Val::Px(20.0)),
            ..default()
        },
        BackgroundColor(PANEL),
        Text::new(""),
        font(15.0),
        TextColor(INK_SECONDARY),
        ProgressText,
        DespawnOnExit(Mode::Loading),
    ));
}

pub fn update(loading: Option<Res<Loading>>, mut progress: Query<&mut Text, With<ProgressText>>) {
    let Some(loading) = loading else {
        return;
    };
    if let Ok(mut text) = progress.single_mut() {
        text.0 = format!(
            "{} ... {:.1}s",
            loading.label,
            loading.started.elapsed().as_secs_f64()
        );
    }
}

pub fn poll(
    mut commands: Commands,
    loading: Option<Res<Loading>>,
    session: Res<Session>,
    mut launcher: ResMut<Launcher>,
    mut next: ResMut<NextState<Mode>>,
) {
    let Some(loading) = loading else {
        next.set(Mode::Launcher);
        return;
    };
    let Some(outcome) = loading.slot.lock().ok().and_then(|mut g| g.take()) else {
        return;
    };
    commands.remove_resource::<Loading>();

    match outcome {
        Ok(Loaded::Disk(index)) => {
            open_disk(&mut commands, &session, index);
            next.set(Mode::Disk);
        }
        Err(message) => {
            eprintln!("error: {message}");
            launcher.error = Some(message);
            next.set(Mode::Launcher);
        }
    }
}

fn open_disk(commands: &mut Commands, session: &Session, index: FileIndex) {
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
    let started = Instant::now();
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

    let mut city = City {
        placements: Placements::with_capacity(n),
        subtree_top: Vec::new(),
        entities: vec![Entity::PLACEHOLDER; n],
        categories,
        params: session.params,
        layout_ms: 0.0,
        index: Arc::new(index),
    };
    city.relayout();
    println!(
        "  laid out {} blocks in {:.0} ms",
        city.placements.placed, city.layout_ms
    );

    let mut lod = Lod::new(n);
    lod.enabled = session.lod;
    lod.px_threshold = session.lod_px;

    let mut edges = Edges::new(sets);
    edges.selected = session.select;
    if session.dupes {
        edges.start_dupe_scan(city.index.clone());
    }

    commands.insert_resource(city);
    commands.insert_resource(lod);
    commands.insert_resource(edges);
    commands.insert_resource(Hover::default());
    commands.insert_resource(Pinned::default());
}

/// Everything a disk session owns. Its entities carry `DespawnOnExit`, so only the resources are
/// left to drop.
pub fn close_disk(mut commands: Commands) {
    commands.remove_resource::<City>();
    commands.remove_resource::<Lod>();
    commands.remove_resource::<Edges>();
    commands.remove_resource::<Hover>();
    commands.remove_resource::<Pinned>();
    commands.remove_resource::<SatelliteView>();
    commands.remove_resource::<BlockMaterials>();
}
