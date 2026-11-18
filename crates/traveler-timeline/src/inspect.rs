//! Saying what things are: a legend, thread labels on the strands, a tooltip for whatever is under
//! the mouse, and a details panel for a clicked one. Three things can be picked: a function line,
//! a thread's strand, and the ring. A picked call path or thread lights up wherever it is on
//! screen. The ring can be dragged along the time axis.

use bevy::asset::RenderAssetUsages;
use bevy::mesh::PrimitiveTopology;
use bevy::prelude::*;
use bevy::window::PrimaryWindow;
use traveler_trace::describe::{Kind, clean_name};
use traveler_trace::story::{Phase, thread_summary};
use traveler_trace::{ROOT, Span};

use crate::Cursor;
use crate::scene::SceneRef;
use crate::tiles::{Pick, Tiles};

const PICK_RADIUS: f32 = 7.0;
const MAX_LABELS: usize = 24;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Target {
    Span(Pick),
    Thread(usize),
    Ring,
}

#[derive(Resource, Default)]
pub struct Inspect {
    pub hover: Option<Target>,
    pub pinned: Option<Target>,
    /// A cursor position to use instead of the mouse, for screenshots of the inspector.
    pub fake: Option<Vec2>,
    cursor: Option<Vec2>,
    tiles_version: u64,
    pressed_at: Option<Vec2>,
    dragging: bool,
    lit: Option<(Target, u64)>,
    share: Option<(Pick, f32)>,
    thread_info: Option<(usize, String)>,
}

#[derive(Component)]
pub struct Tooltip;
#[derive(Component)]
pub struct Details;
#[derive(Component)]
pub struct Highlight;
#[derive(Component)]
pub struct ThreadLabel(usize);

const HINT: &str = "Hover a line, a strand or the ring to see what it is.\nClick to pin the details here.";

const INK: Color = Color::srgb(0.86, 0.9, 1.0);
const MUTED: Color = Color::srgb(0.6, 0.66, 0.8);
const PANEL: Color = Color::srgba(0.02, 0.03, 0.08, 0.82);

impl Inspect {
    pub fn new(fake: Option<Vec2>) -> Self {
        Self { fake, ..default() }
    }
}

fn font(size: f32) -> TextFont {
    TextFont { font_size: FontSize::Px(size), ..default() }
}

pub fn span(scene: &SceneRef, p: Pick) -> Span {
    scene.levels.levels[p.level as usize].spans[p.span as usize]
}

/// Assigning `Text` marks it changed and Bevy lays the text out again, every frame if done
/// every frame. Only write when the words differ.
pub fn set_text(text: &mut Mut<Text>, s: String) {
    if text.0 != s {
        text.0 = s;
    }
}

pub fn setup(mut commands: Commands, scene: Res<SceneRef>) {
    let panel = || Node { width: Val::Percent(100.0), padding: UiRect::all(Val::Px(10.0)), ..default() };
    let column = commands
        .spawn(Node {
            position_type: PositionType::Absolute,
            right: Val::Px(12.0),
            top: Val::Px(12.0),
            width: Val::Px(430.0),
            flex_direction: FlexDirection::Column,
            row_gap: Val::Px(8.0),
            ..default()
        })
        .id();

    let legend = commands
        .spawn((panel(), BackgroundColor(PANEL), Text::new(""), font(12.0), TextColor(INK)))
        .with_children(|p| {
            let line = |p: &mut ChildSpawnerCommands, text: String, color: Color| {
                p.spawn((TextSpan(text), font(12.0), TextColor(color)));
            };
            line(p, "HOW TO READ THIS\n".into(), MUTED);
            for row in [
                "Time runs along the long axis from the white dot.",
                "Thick strand: a thread, branching off when it began.",
                "Thin line: a function on that thread's call stack,",
                "  from when it was called until it returned.",
                "  Further out = deeper in the call stack.",
                "  Colour and direction = the library it is in.",
                "  Brighter = on the stack more of that time.",
                "Ring: a moment in time. Drag it to scrub; its",
                "  stacks are listed at the bottom left.",
            ] {
                line(p, format!("{row}\n"), INK);
            }
            line(p, "\nLIBRARIES, BY SHARE OF TIME\n".into(), MUTED);
            let shown = scene.libraries.iter().take(7);
            for (name, c, share) in shown {
                let col = Color::srgb(c.x.min(1.0), c.y.min(1.0), c.z.min(1.0));
                line(p, "|| ".into(), col);
                line(p, format!("{name}  {:.0}%\n", share * 100.0), INK);
            }
            if scene.libraries.len() > 7 {
                let rest: f32 = scene.libraries[7..].iter().map(|l| l.2).sum();
                line(p, "|| ".into(), Color::srgb(0.42, 0.46, 0.56));
                line(p, format!("{} other libraries  {:.0}%\n", scene.libraries.len() - 7, rest * 100.0), INK);
            }
        })
        .id();

    let details = commands
        .spawn((
            panel(),
            BackgroundColor(PANEL),
            Text::new(HINT),
            font(12.0),
            TextColor(INK),
            Details,
        ))
        .id();
    commands.entity(column).add_children(&[legend, details]);

    let left = commands
        .spawn(Node {
            position_type: PositionType::Absolute,
            left: Val::Px(12.0),
            top: Val::Px(96.0),
            width: Val::Px(560.0),
            flex_direction: FlexDirection::Column,
            row_gap: Val::Px(8.0),
            ..default()
        })
        .id();
    let story = commands.spawn((panel(), BackgroundColor(PANEL), Text::new(story_text(&scene)), font(12.0), TextColor(INK))).id();
    let code = commands
        .spawn((
            Node { display: Display::None, ..panel() },
            BackgroundColor(PANEL),
            Text::new(""),
            font(12.0),
            TextColor(INK),
            crate::explain::CodePanel,
        ))
        .id();
    commands.entity(left).add_children(&[story, code]);

    commands.spawn((
        Node { position_type: PositionType::Absolute, padding: UiRect::all(Val::Px(6.0)), ..default() },
        BackgroundColor(PANEL),
        Text::new(""),
        font(12.0),
        TextColor(INK),
        Visibility::Hidden,
        Tooltip,
    ));

    for i in 0..scene.trace.threads.len().min(MAX_LABELS) {
        let name = scene.names[i].clone();
        commands.spawn((
            Node { position_type: PositionType::Absolute, ..default() },
            Text::new(name),
            font(11.0),
            TextColor(MUTED),
            Visibility::Hidden,
            ThreadLabel(i),
        ));
    }
}

fn polyline_distance(view_proj: Mat4, viewport: Vec2, pts: &[Vec3], cursor: Vec2) -> Option<f32> {
    let to_screen = |p: Vec3| {
        let c = view_proj * p.extend(1.0);
        (c.w > 1e-4).then(|| Vec2::new((c.x / c.w + 1.0) * 0.5 * viewport.x, (1.0 - c.y / c.w) * 0.5 * viewport.y))
    };
    let mut best: Option<f32> = None;
    for w in pts.windows(2) {
        let (Some(a), Some(b)) = (to_screen(w[0]), to_screen(w[1])) else { continue };
        let ab = b - a;
        let t = ((cursor - a).dot(ab) / ab.length_squared().max(1e-6)).clamp(0.0, 1.0);
        let d = (a + ab * t).distance(cursor);
        best = Some(best.map_or(d, |x: f32| x.min(d)));
    }
    best
}

fn ring_points(scene: &SceneRef, t: f32) -> Vec<Vec3> {
    let (x, r) = (scene.x_of(t), scene.ring_radius());
    (0..=64)
        .map(|i| {
            let a = std::f32::consts::TAU * i as f32 / 64.0;
            Vec3::new(x, a.cos() * r, a.sin() * r)
        })
        .collect()
}

/// The moment on the time axis nearest the mouse ray.
fn time_under(scene: &SceneRef, ray: Ray3d) -> Option<f32> {
    let (o, d) = (ray.origin, ray.direction.as_vec3());
    let denom = 1.0 - d.x * d.x;
    if denom < 1e-4 {
        return None;
    }
    let x = (d.x * d.dot(-o) + o.x) / denom;
    Some(scene.time_at_x(x).clamp(0.0, scene.trace.duration as f32))
}

#[allow(clippy::too_many_arguments)]
pub fn pick(
    windows: Query<&Window, With<PrimaryWindow>>,
    camera: Query<(&Camera, &GlobalTransform), With<Camera3d>>,
    mouse: Res<ButtonInput<MouseButton>>,
    keys: Res<ButtonInput<KeyCode>>,
    scene: Res<SceneRef>,
    tiles: Res<Tiles>,
    mut moment: ResMut<Cursor>,
    mut inspect: ResMut<Inspect>,
) {
    let Ok((cam, tf)) = camera.single() else { return };
    let cursor = inspect.fake.or_else(|| windows.single().ok().and_then(|w| w.cursor_position()));
    if keys.just_pressed(KeyCode::Escape) {
        inspect.pinned = None;
    }
    if inspect.dragging {
        if let Some(t) = cursor.and_then(|c| cam.viewport_to_world(tf, c).ok()).and_then(|r| time_under(&scene, r)) {
            moment.0 = t;
        }
        if !mouse.pressed(MouseButton::Left) {
            inspect.dragging = false;
        }
        inspect.cursor = cursor;
        return;
    }
    if mouse.pressed(MouseButton::Right) || cursor.is_none() {
        inspect.hover = None;
        inspect.cursor = cursor;
        return;
    }
    if cursor != inspect.cursor || tiles.version != inspect.tiles_version || moment.is_changed() {
        inspect.cursor = cursor;
        inspect.tiles_version = tiles.version;
        if let (Some(c), Some(size)) = (cursor, cam.logical_viewport_size()) {
            let view_proj = cam.clip_from_view() * tf.to_matrix().inverse();
            let mut best: Option<(f32, Target)> = tiles.pick(view_proj, size, c, PICK_RADIUS).map(|(p, d)| (d, Target::Span(p)));
            let mut consider = |d: Option<f32>, t: Target, slack: f32| {
                if let Some(d) = d.map(|d| d - slack)
                    && d <= PICK_RADIUS
                    && best.is_none_or(|(b, _)| d < b)
                {
                    best = Some((d, t));
                }
            };
            for i in 0..scene.trace.threads.len() {
                // A strand is drawn thicker than a line, so it wins a near tie.
                consider(polyline_distance(view_proj, size, &scene.strand_points(i), c), Target::Thread(i), 2.0);
            }
            consider(polyline_distance(view_proj, size, &ring_points(&scene, moment.0), c), Target::Ring, 0.0);
            inspect.hover = best.map(|(_, t)| t);
        }
    }
    if mouse.just_pressed(MouseButton::Left) {
        inspect.pressed_at = cursor;
        if inspect.hover == Some(Target::Ring) {
            inspect.dragging = true;
        }
    }
    if mouse.just_released(MouseButton::Left)
        && let (Some(a), Some(b)) = (inspect.pressed_at.take(), cursor)
        && a.distance(b) < 4.0
    {
        inspect.pinned = inspect.hover;
    }
    // Function lines load after the strands and the ring, so a pretend click prefers them.
    let line_now = matches!(inspect.hover, Some(Target::Span(_))) && !matches!(inspect.pinned, Some(Target::Span(_)));
    if inspect.fake.is_some() && (inspect.pinned.is_none() || line_now) {
        inspect.pinned = inspect.hover;
    }
}

pub fn highlight(
    mut commands: Commands,
    scene: Res<SceneRef>,
    tiles: Res<Tiles>,
    mut inspect: ResMut<Inspect>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    lit: Query<(Entity, &Mesh3d), With<Highlight>>,
) {
    let target = inspect.pinned.or(inspect.hover).filter(|t| *t != Target::Ring).map(|t| (t, tiles.version));
    if target == inspect.lit {
        return;
    }
    inspect.lit = target;
    for (e, m) in &lit {
        meshes.remove(&m.0);
        commands.entity(e).despawn();
    }
    let Some((target, _)) = target else { return };
    let levels = &scene.levels.levels;
    let (pos, glow) = match target {
        Target::Span(p) => {
            let want = span(&scene, p);
            let pos = tiles.visible_lines(|l, s| {
                let sp = &levels[l as usize].spans[s as usize];
                sp.thread == want.thread && sp.node == want.node
            });
            (pos, 5.0)
        }
        Target::Thread(i) => {
            let mut pos = tiles.visible_lines(|l, s| levels[l as usize].spans[s as usize].thread as usize == i);
            for w in scene.strand_points(i).windows(2) {
                pos.extend_from_slice(&[w[0].to_array(), w[1].to_array()]);
            }
            (pos, 1.6)
        }
        Target::Ring => return,
    };
    if pos.is_empty() {
        return;
    }
    let mesh = Mesh::new(PrimitiveTopology::LineList, RenderAssetUsages::default()).with_inserted_attribute(Mesh::ATTRIBUTE_POSITION, pos);
    commands.spawn((
        Mesh3d(meshes.add(mesh)),
        MeshMaterial3d(materials.add(StandardMaterial {
            base_color: Color::linear_rgb(glow, glow, glow * 0.84),
            unlit: true,
            alpha_mode: AlphaMode::Add,
            ..default()
        })),
        Highlight,
    ));
}

fn dur(s: f64) -> String {
    if s == 0.0 {
        "0 s".into()
    } else if s >= 1.0 {
        format!("{s:.2} s")
    } else if s >= 0.001 {
        format!("{:.1} ms", s * 1000.0)
    } else {
        format!("{:.0} us", s * 1e6)
    }
}

fn thread_share(scene: &SceneRef, sp: Span) -> f32 {
    let th = &scene.trace.threads[sp.thread as usize];
    let d = scene.trace.depth[sp.node as usize];
    let hits = th
        .leaves
        .iter()
        .filter(|&&leaf| {
            let mut n = leaf;
            while n != ROOT && scene.trace.depth[n as usize] > d {
                n = scene.trace.parent[n as usize];
            }
            n == sp.node
        })
        .count();
    hits as f32 / th.leaves.len().max(1) as f32
}

fn pct(x: f32) -> String {
    if x < 0.01 { "<1%".into() } else { format!("{:.0}%", x * 100.0) }
}

fn lib_name(m: &str) -> &str {
    m.trim_end_matches(".dylib")
}

/// "hashing 48%, managing memory 26%" from a ranked list, skipping slivers.
fn kind_list(kinds: &[(Kind, f32)], n: usize) -> String {
    kinds.iter().filter(|(_, v)| *v >= 0.02).take(n).map(|(k, v)| format!("{} {}", k.label().to_lowercase(), pct(*v))).collect::<Vec<_>>().join(", ")
}

fn thread_details(scene: &SceneRef, i: usize) -> String {
    let tr = &scene.trace;
    let th = &tr.threads[i];
    let sum = thread_summary(tr, &scene.describe, i);
    let ends = if th.end >= tr.duration - 0.05 { "ran to the end".to_string() } else { format!("finished at {}", dur(th.end)) };
    let starts = if th.start <= 0.05 { "Was there from the start".to_string() } else { format!("Started at {}", dur(th.start)) };
    let mut s = format!(
        "{}\n{starts} and {ends}.\nIt was busy {} of the time and waiting the rest.\n",
        scene.names[i].to_uppercase(),
        pct(sum.busy),
    );
    if let Some((job, share)) = sum.tasks.first() {
        s.push_str(&format!("Its main job: {job} ({} of its busy time).\n", pct(*share)));
    }
    s.push_str("\nWHAT IT SPENT ITS TIME ON\n");
    for (k, v) in sum.kinds.iter().filter(|(_, v)| *v >= 0.01).take(7) {
        s.push_str(&format!("{:>5}  {}\n", pct(*v), k.label()));
    }
    if sum.tasks.len() > 1 {
        s.push_str("\nJOBS WHILE BUSY\n");
        for (job, v) in sum.tasks.iter().filter(|(_, v)| *v >= 0.01).take(5) {
            s.push_str(&format!("{:>5}  {job}\n", pct(*v)));
        }
    }
    s.push_str("\n(esc or click empty space to clear)");
    s
}

fn short(name: &str, max: usize) -> String {
    if name.chars().count() <= max {
        return name.to_string();
    }
    let tail: String = name.chars().rev().take(max - 3).collect::<Vec<_>>().into_iter().rev().collect();
    format!("...{tail}")
}

fn moment_details(scene: &SceneRef, t: f32) -> String {
    let mut s = format!("THE MOMENT {} INTO THE RUN\nDrag the ring, or use , and . to move it.\n", dur(t as f64));
    if let Some(ph) = scene.phases.iter().find(|p| (t as f64) >= p.start && (t as f64) < p.end) {
        s.push_str(&format!("This is during: {}.\n", phase_line(ph)));
    }
    s.push_str("\nWHAT EACH THREAD WAS DOING\n");
    s.push_str(&moment_lines(scene, t, usize::MAX));
    s.push_str("\n(esc or click empty space to clear)");
    s
}

/// One line per thread alive at `t`: what it was doing and as part of which job.
pub fn moment_lines(scene: &SceneRef, t: f32, max: usize) -> String {
    let tr = &scene.trace;
    let d = &scene.describe;
    let mut s = String::new();
    let mut shown = 0;
    for (i, _) in tr.threads.iter().enumerate() {
        let Some(stack) = tr.stack_at(i, t as f64) else { continue };
        if shown == max {
            s.push_str("...\n");
            break;
        }
        shown += 1;
        let Some(&leaf) = stack.last() else { continue };
        let doing = d.doing(tr, leaf);
        let job = short(&d.job(tr, leaf), 32);
        let line = if doing.is_idle() {
            format!("{}: waiting", scene.names[i])
        } else if job.to_lowercase() == doing.label().to_lowercase() {
            format!("{}: {}", scene.names[i], doing.label().to_lowercase())
        } else {
            format!("{}: {}, during {job}", scene.names[i], doing.label().to_lowercase())
        };
        s.push_str(&format!("{line}\n"));
    }
    s
}

fn phase_line(ph: &Phase) -> String {
    let threads = if ph.busy < 0.5 {
        "nothing busy".to_string()
    } else if ph.busy < 1.5 {
        "one thread busy".to_string()
    } else {
        format!("{:.0} threads busy at once", ph.busy)
    };
    match ph.tasks.first() {
        Some((job, _)) => format!("{threads}, mostly {job}"),
        None => threads,
    }
}

fn story_text(scene: &SceneRef) -> String {
    let mut s = format!("WHAT HAPPENED IN THIS RUN  ({}, {})\n", scene.trace.name, dur(scene.trace.duration));
    for ph in &scene.phases {
        s.push_str(&format!("\n{} to {}: {}\n", dur(ph.start), dur(ph.end), phase_line(ph)));
        let doing = kind_list(&ph.kinds, 3);
        if !doing.is_empty() {
            s.push_str(&format!("  time went to: {doing}\n"));
        }
    }
    s
}

pub fn panels(
    scene: Res<SceneRef>,
    moment: Res<Cursor>,
    mut inspect: ResMut<Inspect>,
    mut tooltip: Query<(&mut Text, &mut Node, &mut Visibility), (With<Tooltip>, Without<Details>)>,
    mut details: Query<&mut Text, (With<Details>, Without<Tooltip>)>,
) {
    let tr = &scene.trace;
    let d = &scene.describe;
    if let Ok((mut text, mut node, mut vis)) = tooltip.single_mut() {
        let label = |t: Target| match t {
            Target::Span(p) => {
                let sp = span(&scene, p);
                let kind = d.kind(tr, sp.node);
                format!("{}: {}\n{}  |  click for details", kind.label(), clean_name(tr.name_of(sp.node)), scene.names[sp.thread as usize])
            }
            Target::Thread(i) => {
                let th = &tr.threads[i];
                format!("{}\nfrom {} to {}  |  click for what it did", scene.names[i], dur(th.start), dur(th.end))
            }
            Target::Ring => format!("the moment {}\ndrag to move through time  |  click for what every thread was doing", dur(moment.0 as f64)),
        };
        match (if inspect.dragging { Some(Target::Ring) } else { inspect.hover }, inspect.cursor) {
            (Some(t), Some(c)) => {
                set_text(&mut text, label(t));
                let (l, t) = (Val::Px(c.x + 16.0), Val::Px(c.y + 16.0));
                if node.left != l || node.top != t {
                    node.left = l;
                    node.top = t;
                }
                if *vis != Visibility::Inherited {
                    *vis = Visibility::Inherited;
                }
            }
            _ if *vis != Visibility::Hidden => *vis = Visibility::Hidden,
            _ => {}
        }
    }

    let Ok(mut text) = details.single_mut() else { return };
    let p = match inspect.pinned {
        None => return set_text(&mut text, HINT.into()),
        Some(Target::Ring) => return set_text(&mut text, moment_details(&scene, moment.0)),
        Some(Target::Thread(i)) => {
            let body = match &inspect.thread_info {
                Some((j, s)) if *j == i => s.clone(),
                _ => {
                    let s = thread_details(&scene, i);
                    inspect.thread_info = Some((i, s.clone()));
                    s
                }
            };
            return set_text(&mut text, body);
        }
        Some(Target::Span(p)) => p,
    };
    let sp = span(&scene, p);
    let share = match inspect.share {
        Some((q, v)) if q == p => v,
        _ => {
            let v = thread_share(&scene, sp);
            inspect.share = Some((p, v));
            v
        }
    };
    let kind = d.kind(tr, sp.node);
    let shown_kind = if kind == Kind::Plumbing { d.doing(tr, sp.node) } else { kind };
    let lib = &tr.modules[tr.module_of(sp.node) as usize];
    let steps: Vec<String> = d.tasks(tr, sp.node).iter().filter(|&&n| n != sp.node).map(|&n| clean_name(tr.name_of(n))).collect();
    let while_line = if steps.is_empty() {
        "(no named step of the program here)".to_string()
    } else {
        let tail: Vec<String> = steps[steps.len().saturating_sub(3)..].iter().map(|s| short(s, 30)).collect();
        tail.join(" > ")
    };
    let mut s = format!(
        "{}\n{}\n\n\
         function   {}\n\
         \x20          in {}{}\n\
         thread     {}\n\
         while      {}\n\
         when       {} to {} ({}),\n\
         \x20          on the stack {} of that stretch\n\
         how much   {} of this thread's whole run\n",
        shown_kind.label().to_uppercase(),
        shown_kind.sentence(),
        short(&clean_name(tr.name_of(sp.node)), 46),
        lib_name(lib),
        if lib_name(lib) == scene.describe.program { " (the program itself)" } else { "" },
        scene.names[sp.thread as usize],
        while_line,
        dur(sp.t0 as f64),
        dur(sp.t1 as f64),
        dur((sp.t1 - sp.t0) as f64),
        pct(sp.weight),
        pct(share),
    );
    let mut path = Vec::new();
    let mut n = sp.node;
    while n != ROOT {
        path.push(n);
        n = tr.parent[n as usize];
    }
    path.reverse();
    s.push_str(&format!(
        "\nTECHNICAL\nfull name  {}\ndepth      {} calls below the strand, zoom slice {}\n\
         gaps       under {} are bridged: the sampler cannot\n\
         \x20          tell them from a missed sample\n\
         last calls on the way here:\n",
        short(tr.name_of(sp.node), 52),
        tr.depth[sp.node as usize].saturating_sub(tr.threads[sp.thread as usize].trunk),
        dur(scene.levels.levels[p.level as usize].slice),
        dur(traveler_trace::BRIDGE_SECONDS),
    ));
    for &f in &path[path.len().saturating_sub(5)..] {
        s.push_str(&format!("  {}{}\n", if f == sp.node { "> " } else { "" }, short(&clean_name(tr.name_of(f)), 48)));
    }
    s.push_str("\nPress X to see what this code is.  (esc clears)");
    set_text(&mut text, s);
}

pub fn thread_labels(
    scene: Res<SceneRef>,
    camera: Query<(&Camera, &GlobalTransform), With<Camera3d>>,
    mut labels: Query<(&ThreadLabel, &mut Node, &mut Visibility)>,
) {
    let Ok((cam, tf)) = camera.single() else { return };
    let Some(size) = cam.logical_viewport_size() else { return };
    for (label, mut node, mut vis) in &mut labels {
        match cam.world_to_viewport(tf, scene.label_anchor(label.0)) {
            Ok(p) if p.x > 0.0 && p.y > 0.0 && p.x < size.x && p.y < size.y => {
                let (l, t) = (Val::Px((p.x + 6.0).round()), Val::Px((p.y - 16.0).round()));
                if node.left != l || node.top != t {
                    node.left = l;
                    node.top = t;
                }
                if *vis != Visibility::Inherited {
                    *vis = Visibility::Inherited;
                }
            }
            _ if *vis != Visibility::Hidden => *vis = Visibility::Hidden,
            _ => {}
        }
    }
}

/// Offscreen, UI has to be told which camera it draws through.
pub fn target_ui(
    mut commands: Commands,
    offscreen: Res<crate::Offscreen>,
    camera: Query<Entity, With<Camera3d>>,
    roots: Query<Entity, (With<Node>, Without<ChildOf>)>,
) {
    if offscreen.0.is_none() {
        return;
    }
    let Ok(cam) = camera.single() else { return };
    for e in &roots {
        commands.entity(e).insert(UiTargetCamera(cam));
    }
}
