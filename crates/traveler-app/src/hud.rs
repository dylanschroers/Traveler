//! On-screen readout.
//!
//! The legend is not decoration. Hue only carries a category here, so the legend and the
//! crosshair readout are what make identity legible: the readout names the exact extension of
//! whatever you are looking at, and legend labels are neutral ink beside a colored swatch rather
//! than colored text.

use bevy::prelude::*;
use bevy::state::state_scoped::DespawnOnExit;

use crate::edges::{DupeState, Edges};
use crate::lod::{Hover, Lod};
use crate::palette::{CATEGORY_COUNT, Category, top_extensions};
use crate::pin::{MAX_PAINTED, Pinned};
use crate::satellite::SatelliteView;
use crate::scene::SceneView;
use crate::{City, Mode};
use traveler_scan::{Tree, human_bytes, thousands};

pub const INK_PRIMARY: Color = Color::srgb(1.0, 1.0, 1.0);
pub const INK_SECONDARY: Color = Color::srgb(0.765, 0.761, 0.718);
pub const INK_MUTED: Color = Color::srgb(0.52, 0.52, 0.50);
pub const PANEL: Color = Color::srgba(0.043, 0.051, 0.063, 0.82);

#[derive(Component)]
pub struct StatsText;

#[derive(Component)]
pub struct HoverText;

#[derive(Component)]
pub struct EdgesText;

pub fn panel(top: Option<f32>, bottom: Option<f32>, left: Option<f32>, right: Option<f32>) -> Node {
    Node {
        position_type: PositionType::Absolute,
        top: top.map(Val::Px).unwrap_or(Val::Auto),
        bottom: bottom.map(Val::Px).unwrap_or(Val::Auto),
        left: left.map(Val::Px).unwrap_or(Val::Auto),
        right: right.map(Val::Px).unwrap_or(Val::Auto),
        padding: UiRect::all(Val::Px(8.0)),
        ..default()
    }
}

pub fn font(size: f32) -> TextFont {
    TextFont {
        // 0.19 made this a FontSize enum rather than a bare f32.
        font_size: FontSize::Px(size),
        ..default()
    }
}

pub fn setup_hud(mut commands: Commands, city: Res<City>) {
    let s = city.index.stats;

    commands.spawn((
        panel(Some(10.0), None, Some(10.0), None),
        BackgroundColor(PANEL),
        Text::new(format!(
            "{}\n{} files, {} folders, {}\nscan {:.2}s   layout {:.0}ms   {} blocks",
            city.index.path(city.index.root()).display(),
            thousands(s.files),
            thousands(s.dirs),
            human_bytes(s.bytes),
            s.walk_millis as f64 / 1000.0,
            city.layout_ms,
            thousands(city.placements.placed as u64),
        )),
        DespawnOnExit(Mode::Disk),
        font(13.0),
        TextColor(INK_SECONDARY),
    ));

    commands.spawn((
        panel(Some(10.0), None, None, Some(10.0)),
        BackgroundColor(PANEL),
        Text::new(""),
        DespawnOnExit(Mode::Disk),
        font(13.0),
        TextColor(INK_SECONDARY),
        StatsText,
    ));

    commands.spawn((
        panel(Some(86.0), None, None, Some(10.0)),
        BackgroundColor(PANEL),
        Text::new(""),
        DespawnOnExit(Mode::Disk),
        font(12.0),
        TextColor(INK_SECONDARY),
        EdgesText,
    ));

    // Crosshair: two thin bars crossing at the centre of the viewport.
    for (w, h, dx, dy) in [(11.0f32, 1.0f32, -5.0f32, 0.0f32), (1.0, 11.0, 0.0, -5.0)] {
        commands.spawn((
            Node {
                position_type: PositionType::Absolute,
                left: Val::Percent(50.0),
                top: Val::Percent(50.0),
                width: Val::Px(w),
                height: Val::Px(h),
                margin: UiRect {
                    left: Val::Px(dx),
                    top: Val::Px(dy),
                    ..default()
                },
                ..default()
            },
            BackgroundColor(Color::srgba(1.0, 1.0, 1.0, 0.55)),
            DespawnOnExit(Mode::Disk),
        ));
    }

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
        DespawnOnExit(Mode::Disk),
        font(13.0),
        TextColor(INK_PRIMARY),
        HoverText,
    ));

    // Legend. One row per category: swatch, then neutral-ink label and the biggest extensions
    // that fall into it.
    let table = &city.categories;
    let tops = top_extensions(&city.index, table, 40);
    let mut per_cat: Vec<(u64, Vec<String>)> = vec![(0, Vec::new()); CATEGORY_COUNT];
    for (name, bytes, cat) in &tops {
        let slot = &mut per_cat[*cat as usize];
        slot.0 += bytes;
        if slot.1.len() < 4 {
            slot.1.push(name.clone());
        }
    }

    let cats = [
        Category::Code,
        Category::Media,
        Category::Binary,
        Category::Other,
        Category::Directory,
    ];
    let row_h = 19.0;
    // Backing panel first, so the rows draw over it.
    commands.spawn((
        Node {
            position_type: PositionType::Absolute,
            left: Val::Px(6.0),
            bottom: Val::Px(86.0),
            width: Val::Px(560.0),
            height: Val::Px(cats.len() as f32 * row_h + 8.0),
            ..default()
        },
        BackgroundColor(PANEL),
        DespawnOnExit(Mode::Disk),
    ));
    for (i, cat) in cats.iter().enumerate() {
        let y = 92.0 + (cats.len() - 1 - i) as f32 * row_h;
        commands.spawn((
            Node {
                position_type: PositionType::Absolute,
                left: Val::Px(12.0),
                bottom: Val::Px(y + 3.0),
                width: Val::Px(12.0),
                height: Val::Px(12.0),
                ..default()
            },
            BackgroundColor(cat.color()),
            DespawnOnExit(Mode::Disk),
        ));

        let (bytes, exts) = &per_cat[*cat as usize];
        let detail = if *cat == Category::Directory {
            String::new()
        } else if exts.is_empty() {
            "  none".to_string()
        } else {
            format!("  {}   {}", human_bytes(*bytes), exts.join(", "))
        };
        commands.spawn((
            Node {
                position_type: PositionType::Absolute,
                left: Val::Px(32.0),
                bottom: Val::Px(y),
                ..default()
            },
            Text::new(format!("{}{}", cat.label(), detail)),
            DespawnOnExit(Mode::Disk),
            font(12.0),
            TextColor(INK_SECONDARY),
        ));
    }

    commands.spawn((
        panel(None, Some(10.0), Some(10.0), None),
        BackgroundColor(PANEL),
        Text::new(
            "WASD move   space/ctrl up-down   shift run   scroll speed   right-click or M grab mouse\n\
             L level of detail   [ ] detail threshold   H height mode   R reset camera\n\
             tab group kind   up/down select (shift for 10)   esc clear   D find duplicates\n\
             left click pins what the crosshair is on, and lights up a folder's contents",
        ),
        DespawnOnExit(Mode::Disk),
        font(12.0),
        TextColor(INK_MUTED),
    ));
}

pub fn update_stats<S: SceneView>(
    scene: Res<S>,
    lod: Res<Lod>,
    time: Res<Time<Real>>,
    mut smoothed: Local<f32>,
    mut stats: Query<&mut Text, With<StatsText>>,
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
            thousands(scene.placements().placed as u64),
            if lod.enabled { "on" } else { "off" },
            lod.px_threshold,
        );
    }
}

pub fn update_hover(
    city: Res<City>,
    hover: Res<Hover>,
    pinned: Res<Pinned>,
    mut hover_text: Query<(&mut Text, &mut Visibility), With<HoverText>>,
) {
    let Ok((mut text, mut vis)) = hover_text.single_mut() else {
        return;
    };
    // A pin outranks the crosshair: holding onto something is the whole reason to click.
    let shown = pinned.node.or(hover.node);
    *vis = if shown.is_some() {
        Visibility::Inherited
    } else {
        Visibility::Hidden
    };
    let Some(id) = shown else {
        return;
    };

    let index = &city.index;
    let flags = index.flags(id);
    let kind = if flags.is_dir() {
        if flags.is_reparse() {
            "junction".to_string()
        } else {
            format!(
                "folder, {} entries",
                thousands(index.child_count(id) as u64)
            )
        }
    } else {
        let ext = index.ext_str(index.ext(id));
        if ext.is_empty() {
            "file".to_string()
        } else {
            format!(".{ext} file")
        }
    };

    let mut out = format!(
        "{}
{}   {}",
        index.path(id).display(),
        human_bytes(index.size(id)),
        kind
    );

    if pinned.node == Some(id) {
        out.push_str(
            "
pinned   click again or esc to release",
        );
        if flags.is_dir() {
            out.push_str(&format!(
                "
holds {} files in {} folders{}",
                thousands(pinned.leaves),
                thousands(pinned.groups),
                if pinned.painted_all {
                    String::new()
                } else {
                    format!(", {} lit", thousands(MAX_PAINTED as u64))
                }
            ));
            if let Some(big) = pinned.biggest {
                out.push_str(&format!(
                    "
biggest: {}   {}",
                    index.name(big),
                    human_bytes(index.size(big))
                ));
            }
        }
    }
    text.0 = out;
}

pub fn update_edges_panel(
    edges: Res<Edges>,
    satellite: Res<SatelliteView>,
    mut panel: Query<&mut Text, With<EdgesText>>,
) {
    if let Ok(mut text) = panel.single_mut() {
        text.0 = edge_panel(&edges, &satellite);
    }
}

/// The group panel: which kind is active, a window of its groups, and the selected one marked.
fn edge_panel(edges: &Edges, satellite: &SatelliteView) -> String {
    let Some(set) = edges.active_set() else {
        return String::new();
    };
    let mut out = format!(
        "[tab] {}   {} groups",
        set.kind.label(),
        thousands(set.groups.len() as u64)
    );
    if set.total_redundant() > 0 {
        out.push_str(&format!(
            "   {} redundant",
            human_bytes(set.total_redundant())
        ));
    }
    out.push('\n');

    // Ten rows, scrolled to keep the selection in view.
    const ROWS: usize = 10;
    let first = edges
        .selected
        .map(|s| s.saturating_sub(ROWS / 2))
        .unwrap_or(0)
        .min(set.groups.len().saturating_sub(ROWS));
    for (i, g) in set.groups.iter().enumerate().skip(first).take(ROWS) {
        out.push_str(&format!(
            "{} {:<34} {:>6} {:>10}\n",
            if edges.selected == Some(i) { ">" } else { " " },
            clip(&g.label, 34),
            thousands(g.members.len() as u64),
            human_bytes(g.bytes)
        ));
    }

    // What the cluster overhead is saying, in words.
    if !satellite.data.is_empty() {
        let folders = satellite.data.buckets.len();
        out.push_str(&format!(
            "{} shown across {} folder{}{}
",
            thousands(satellite.data.members.len() as u64),
            thousands(folders as u64),
            if folders == 1 { "" } else { "s" },
            if satellite.data.omitted > 0 {
                format!(", {} not shown", thousands(satellite.data.omitted as u64))
            } else {
                String::new()
            }
        ));
    }

    out.push_str(&match &edges.dupes {
        DupeState::Idle => "[D] find duplicates (reads file contents)".to_string(),
        DupeState::Running { since, .. } => format!(
            "scanning for duplicates... {:.0}s",
            since.elapsed().as_secs_f32()
        ),
        DupeState::Done => "duplicates ready".to_string(),
    });
    out
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    // Plain dots: the bundled font has no ellipsis glyph and draws a replacement box instead.
    let keep: String = s.chars().take(max.saturating_sub(2)).collect();
    format!("{keep}..")
}
