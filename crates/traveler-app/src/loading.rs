//! The work between "what to look at" and a city on screen, done off the main thread.
//!
//! One request in, one session out. The worker owns the slow part and drops its answer into a
//! slot polled each frame, which is the pattern `edges::DupeState` already uses; nothing here
//! blocks a frame.

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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
    Launch {
        program: OsString,
        args: Vec<OsString>,
        no_aslr: bool,
    },
    Attach(u32),
}

/// How long a launched target runs before the first capture, so the view opens on a linked
/// process rather than on the four mappings that exist at the `execve` stop. The `execve` frame is
/// the more interesting one, but only next to a later one, which is 6h's job.
const SETTLE: Duration = Duration::from_millis(250);

#[derive(Resource)]
pub struct Loading {
    job: Job,
    started: Instant,
    label: String,
}

enum Job {
    /// A walk takes seconds, so it goes to a worker and drops its answer in a slot.
    Scan(Arc<Mutex<Option<Result<FileIndex, String>>>>),
    /// A capture does not. ptrace attaches a tracee to exactly one tracer *thread* — requests
    /// from any other thread are `ESRCH` — so acquisition and every later capture happen on the
    /// main thread. At half a millisecond a stop costs a fraction of a frame; if 6e's pagemap
    /// read changes that, the fix is a thread that owns the target, not a worker per capture.
    Acquire { request: Request, ready_at: Instant },
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
        Request::Launch { program, .. } => format!("launching {}", program.display()),
        Request::Attach(pid) => format!("attaching to {pid}"),
    };
    println!("{label} ...");

    let job = match request {
        Request::Disk(path) => {
            let slot = Arc::new(Mutex::new(None));
            let sink: Arc<Mutex<Option<Result<FileIndex, String>>>> = slot.clone();
            std::thread::spawn(move || {
                let outcome = scan(&path, &ScanOptions::default())
                    .map_err(|e| format!("cannot scan {}: {e}", path.display()));
                if let Ok(mut guard) = sink.lock() {
                    *guard = Some(outcome);
                }
            });
            Job::Scan(slot)
        }
        request => Job::Acquire {
            ready_at: Instant::now() + SETTLE,
            request,
        },
    };

    commands.insert_resource(Loading {
        job,
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

    let outcome = match &loading.job {
        Job::Scan(slot) => match slot.lock().ok().and_then(|mut g| g.take()) {
            Some(result) => result.map(|index| Opened::Disk(Box::new(index))),
            None => return,
        },
        Job::Acquire { request, ready_at } => {
            if Instant::now() < *ready_at {
                return;
            }
            acquire(request)
        }
    };
    commands.remove_resource::<Loading>();

    match outcome {
        Ok(Opened::Disk(index)) => {
            open_disk(&mut commands, &session, *index);
            next.set(Mode::Disk);
        }
        Ok(Opened::Process(session_data)) => {
            open_process(&mut commands, &session, *session_data);
            next.set(Mode::Process);
        }
        Err(message) => {
            eprintln!("error: {message}");
            launcher.error = Some(message);
            next.set(Mode::Launcher);
        }
    }
}

/// Boxed because the process variant is much larger than the disk one, and this is moved through
/// a `Result` on every poll.
enum Opened {
    Disk(Box<FileIndex>),
    #[cfg(target_os = "linux")]
    Process(Box<ProcessSession>),
}

#[cfg(target_os = "linux")]
struct ProcessSession {
    target: traveler_proc::AttachedTarget,
    /// Oldest first. A launch brings two: the `execve` stop and the entry point, whose difference
    /// is the dynamic linker's work.
    captures: Vec<traveler_proc::AddressSpace>,
    symbols: crate::symbols::Symbols,
}

/// Runs a launched target from its `execve` stop to its entry point, so capture 1 is taken at a
/// place rather than at whenever the loop got round to it.
///
/// §4 called this "about fifty lines and no state machine" and scheduled it at the end of 6h; it
/// arrived with phase 9's breakpoints instead, which is where it was always going to be cheapest.
#[cfg(target_os = "linux")]
fn run_to_entry(target: &mut traveler_proc::AttachedTarget) -> Result<(), String> {
    use traveler_proc::StopReason;

    let exe =
        std::fs::read_link(format!("/proc/{}/exe", target.pid())).map_err(|e| e.to_string())?;
    let image = traveler_sym::Image::parse(&exe)?;
    // At the `execve` stop the executable is mapped and nothing else has moved, so the bias is
    // whatever the loader put it at minus where it was linked.
    let text = std::fs::read_to_string(format!("/proc/{}/maps", target.pid()))
        .map_err(|e| e.to_string())?;
    let base = traveler_proc::maps::parse(&text)
        .iter()
        .filter(|m| m.path == exe.to_string_lossy())
        .map(|m| m.addr.saturating_sub(m.offset))
        .min()
        .ok_or("the executable is not mapped")?;

    let entry = base.saturating_sub(image.base_vaddr) + image.entry;
    target.set_breakpoint(entry).map_err(|e| e.to_string())?;
    let reason = target.resume_until().map_err(|e| e.to_string())?;
    target.clear_breakpoint(entry).map_err(|e| e.to_string())?;
    match reason {
        StopReason::Breakpoint(at) if at == entry => Ok(()),
        other => Err(format!("stopped at {other:?} instead of the entry point")),
    }
}

#[cfg(target_os = "linux")]
fn acquire(request: &Request) -> Result<Opened, String> {
    use crate::symbols::Symbols;
    use traveler_proc::{AttachedTarget, CaptureOptions, Launch};

    let launched = matches!(request, Request::Launch { .. });
    let mut target = match request {
        Request::Launch {
            program,
            args,
            no_aslr,
        } => {
            let mut launch = Launch::new(program);
            for arg in args {
                launch = launch.arg(arg);
            }
            launch.disable_aslr = *no_aslr;
            AttachedTarget::launch(&launch).map_err(|e| e.to_string())?
        }
        Request::Attach(pid) => AttachedTarget::attach(*pid).map_err(|e| e.to_string())?,
        Request::Disk(_) => unreachable!("a disk request is a scan"),
    };
    // Tier 2 rides along with the capture: the images are parsed after the target is resumed,
    // and if none of them parse the view is tier 1 and still complete.
    let mut symbols = Symbols::default();
    let mut take = |target: &mut AttachedTarget| {
        target
            .capture_sections(&CaptureOptions::default(), &mut |mappings| {
                symbols.build(mappings)
            })
            .map_err(|e| e.to_string())
    };

    let mut captures = Vec::new();
    if launched {
        // §4's frame, and the whole reason launch comes first: the address space before the
        // dynamic linker has run. On its own it is four mappings; against the next one it is the
        // linking, drawn.
        captures.push(take(&mut target)?);
        if let Err(e) = run_to_entry(&mut target) {
            eprintln!(
                "note: could not stop at the entry point ({e}), so capture 1 is wherever \
                       the target had got to"
            );
        }
    }
    captures.push(take(&mut target)?);
    target.resume().map_err(|e| e.to_string())?;

    Ok(Opened::Process(Box::new(ProcessSession {
        target,
        captures,
        symbols,
    })))
}

#[cfg(not(target_os = "linux"))]
fn acquire(_: &Request) -> Result<Opened, String> {
    Err("looking inside a process is Linux-only so far".into())
}

#[cfg(target_os = "linux")]
fn open_process(commands: &mut Commands, session: &Session, opened: ProcessSession) {
    use crate::space::{Captures, Source, Space, Traced, View};

    let ProcessSession {
        target,
        captures,
        symbols,
    } = opened;
    let capture = captures.last().expect("at least one capture").clone();
    let shown = captures.len() - 1;

    let meta = capture.meta();
    println!(
        "  {} in {} regions, {} mappings   stop {:.2} ms over {} threads",
        human_bytes(capture.frame().weight(capture.frame().root())),
        capture.frame().child_count(capture.frame().root()),
        capture.frame().mappings().len(),
        meta.stop.as_secs_f64() * 1000.0,
        meta.threads,
    );

    let space = Space::build(
        Source::Capture(Box::new(capture.clone())),
        View::Capture(shown),
        session.space,
    );
    println!(
        "  laid out {} blocks in {:.0} ms",
        space.placements.placed, space.layout_ms
    );

    let mut lod = Lod::new(space.frame.len());
    lod.enabled = session.lod;
    lod.px_threshold = session.lod_px;

    println!(
        "  tier {:?}: {} sections from {} images",
        symbols.tier(),
        symbols.sections(),
        symbols.images()
    );

    commands.insert_resource(space);
    commands.insert_resource(Captures(captures));
    commands.insert_resource(Traced(target));
    commands.insert_resource(symbols);
    commands.insert_resource(lod);
    commands.insert_resource(Hover::default());
    commands.insert_resource(Pinned::default());
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
/// left to drop. The process session's teardown lives beside its own module, in `space::close`.
pub fn close_disk(mut commands: Commands) {
    commands.remove_resource::<City>();
    commands.remove_resource::<Lod>();
    commands.remove_resource::<Edges>();
    commands.remove_resource::<Hover>();
    commands.remove_resource::<Pinned>();
    commands.remove_resource::<SatelliteView>();
    commands.remove_resource::<BlockMaterials>();
}
