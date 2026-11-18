//! "What is this?" for a selected function: X asks REA about it in the background and the answer
//! appears in a panel. Everything shown here is inferred from the compiled binary.

use std::collections::HashMap;
use std::time::Instant;

use bevy::prelude::*;
use traveler_trace::describe::clean_name;
use traveler_trace::rea::{Explanation, Job, Request};

use crate::inspect::{self, Inspect, Target, set_text};
use crate::scene::SceneRef;

type Key = (u16, u64);

const PAGE: usize = 20;
const LINE_WIDTH: usize = 74;
const LIST: usize = 8;

enum State {
    Running,
    Ready(Explanation),
    Failed(String),
}

enum Shown {
    Function(Key, String),
    Note(String),
}

#[derive(Resource, Default)]
pub struct Explain {
    states: HashMap<Key, State>,
    job: Option<(Key, Job)>,
    queued: Option<Key>,
    started: Option<Instant>,
    shown: Option<Shown>,
    scroll: Option<usize>,
    /// Ask about the first pinned function without a key press, for screenshots.
    pub auto: bool,
    created: Option<Instant>,
}

impl Explain {
    pub fn new(auto: bool) -> Self {
        Self { auto, created: Some(Instant::now()), ..default() }
    }

    /// An analysis is running or still about to start, so a screenshot should wait for it.
    pub fn busy(&self) -> bool {
        self.auto || self.job.is_some() || self.queued.is_some()
    }
}

#[derive(Component)]
pub struct CodePanel;

fn request(scene: &SceneRef, (image, rva): Key) -> Request {
    let m = &scene.trace.images[image as usize];
    Request { path: m.path.clone(), code_id: m.code_id.clone(), rva }
}

fn start(e: &mut Explain, scene: &SceneRef, key: Key) {
    e.states.insert(key, State::Running);
    e.started = Some(Instant::now());
    e.job = Some((key, Job::start(request(scene, key))));
}

pub fn update(keys: Res<ButtonInput<KeyCode>>, scene: Res<SceneRef>, inspect: Res<Inspect>, mut explain: ResMut<Explain>) {
    let e = &mut *explain;
    if let Some((key, job)) = &e.job
        && let Some(result) = job.poll()
    {
        e.states.insert(*key, result.map_or_else(State::Failed, State::Ready));
        e.job = None;
    }
    if e.job.is_none()
        && let Some(key) = e.queued.take()
    {
        start(e, &scene, key);
    }
    if keys.just_pressed(KeyCode::Escape) {
        e.shown = None;
    }
    if e.auto && e.created.is_some_and(|t| t.elapsed().as_secs() > 12) {
        e.auto = false;
        e.shown = Some(Shown::Note("The test click did not land on a function line.".into()));
    }
    let asked = keys.just_pressed(KeyCode::KeyX) || (e.auto && e.shown.is_none());
    if asked {
        match inspect.pinned {
            Some(Target::Span(p)) => {
                e.auto = false;
                let node = inspect::span(&scene, p).node;
                let tr = &scene.trace;
                let func = &tr.funcs[tr.func[node as usize] as usize];
                e.scroll = None;
                match func.origin {
                    Some(o) => {
                        let key = (o.image, o.rva);
                        e.shown = Some(Shown::Function(key, func.name.clone()));
                        if !e.states.contains_key(&key) {
                            if e.job.is_none() {
                                start(e, &scene, key);
                            } else {
                                e.queued = Some(key);
                            }
                        }
                    }
                    None => e.shown = Some(Shown::Note("This recording does not say where this function starts in a binary.".into())),
                }
            }
            _ if keys.just_pressed(KeyCode::KeyX) => e.shown = Some(Shown::Note("Click a function line first, then press X.".into())),
            _ => {}
        }
    }
    let step = keys.just_pressed(KeyCode::BracketRight) as isize - keys.just_pressed(KeyCode::BracketLeft) as isize;
    if step != 0
        && let Some(Shown::Function(key, _)) = &e.shown
        && let Some(State::Ready(x)) = e.states.get(key)
    {
        let lines: Vec<&str> = x.pseudocode.lines().collect();
        let at = e.scroll.unwrap_or_else(|| body_start(&lines)) as isize + step * PAGE as isize;
        e.scroll = Some(at.clamp(0, lines.len().saturating_sub(1) as isize) as usize);
    }
}

/// The first line after the opening run of variable declarations.
fn body_start(lines: &[&str]) -> usize {
    lines.iter().enumerate().skip(2).find(|(_, l)| !l.contains("; //")).map_or(0, |(i, _)| i)
}

fn clip(line: &str) -> String {
    if line.chars().count() <= LINE_WIDTH {
        return line.to_string();
    }
    format!("{}...", line.chars().take(LINE_WIDTH - 3).collect::<String>())
}

fn list(names: &[String]) -> String {
    if names.is_empty() {
        return "none found".into();
    }
    let mut shown: Vec<String> = Vec::new();
    for n in names.iter().map(|n| clean_name(n)) {
        if !shown.contains(&n) {
            shown.push(n);
        }
    }
    let extra = shown.len().saturating_sub(LIST);
    shown.truncate(LIST);
    let joined = shown.join(", ");
    if extra > 0 { format!("{joined}, and {extra} more") } else { joined }
}

fn ready_text(x: &Explanation, scroll: Option<usize>) -> String {
    let lines: Vec<&str> = x.pseudocode.lines().collect();
    let at = scroll.unwrap_or_else(|| body_start(&lines)).min(lines.len().saturating_sub(1));
    let end = (at + PAGE).min(lines.len());
    let mut s = format!(
        "WHAT IS THIS?  {}\nInferred from the compiled program. It shows what this code can do, not which path ran.\n\n\
         called by  {}\ncalls      {}\n",
        clean_name(&x.name),
        list(&x.callers),
        list(&x.callees),
    );
    if !x.missing.is_empty() {
        s.push_str(&format!("not available  {}\n", x.missing.join(", ")));
    }
    s.push_str(&format!("\nRECOVERED CODE, lines {}-{} of {}   [ and ] scroll   esc closes\n", at + 1, end, lines.len()));
    for l in lines.get(at..end).unwrap_or_default() {
        s.push_str(&clip(l));
        s.push('\n');
    }
    s
}

pub fn panel(explain: Res<Explain>, mut q: Query<(&mut Text, &mut Node), With<CodePanel>>) {
    let Ok((mut text, mut node)) = q.single_mut() else { return };
    let wanted = if explain.shown.is_some() { Display::Flex } else { Display::None };
    if node.display != wanted {
        node.display = wanted;
    }
    let body = match &explain.shown {
        None => return,
        Some(Shown::Note(n)) => format!("WHAT IS THIS?\n{n}"),
        Some(Shown::Function(key, name)) => match explain.states.get(key) {
            Some(State::Ready(x)) => ready_text(x, explain.scroll),
            Some(State::Failed(m)) => format!("WHAT IS THIS?  {}\n{m}", clean_name(name)),
            _ => {
                let running = explain.job.as_ref().is_some_and(|(k, _)| k == key);
                let secs = explain.started.filter(|_| running).map_or(0, |t| t.elapsed().as_secs());
                let status = if running { format!("Analyzing the binary... {secs} s") } else { "Waiting for the current analysis...".into() };
                format!("WHAT IS THIS?  {}\n{status}\nThe first request for a program can take a few minutes. The timeline stays usable.", clean_name(name))
            }
        },
    };
    set_text(&mut text, body);
}
