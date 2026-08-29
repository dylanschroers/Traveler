//! What to look at, asked before anything is loaded.
//!
//! The scan used to run before the window opened, which a launcher makes untenable: there is
//! nothing to scan until someone says what. So the window comes up first and the target is a
//! request, handed to `loading` and answered on a worker thread.

use std::path::PathBuf;

use bevy::input::ButtonState;
use bevy::input::keyboard::KeyboardInput;
use bevy::prelude::*;
use bevy::state::state_scoped::DespawnOnExit;

use crate::Mode;
use crate::hud::{INK_MUTED, INK_PRIMARY, INK_SECONDARY, PANEL, font};
use crate::loading::Request;

const ERROR_INK: Color = Color::srgb(0.90, 0.45, 0.35);

#[derive(Resource)]
pub struct Launcher {
    pub path: String,
    /// Why the last attempt did not open, kept so the message survives the bounce back here.
    pub error: Option<String>,
}

impl Default for Launcher {
    fn default() -> Self {
        Self {
            path: std::env::current_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            error: None,
        }
    }
}

#[derive(Component)]
pub struct PathField;

#[derive(Component)]
pub struct ErrorLine;

pub fn setup(mut commands: Commands) {
    commands
        .spawn((
            Node {
                position_type: PositionType::Absolute,
                left: Val::Percent(50.0),
                top: Val::Percent(38.0),
                width: Val::Px(760.0),
                margin: UiRect {
                    left: Val::Px(-380.0),
                    ..default()
                },
                padding: UiRect::all(Val::Px(20.0)),
                flex_direction: FlexDirection::Column,
                row_gap: Val::Px(10.0),
                ..default()
            },
            BackgroundColor(PANEL),
            DespawnOnExit(Mode::Launcher),
        ))
        .with_children(|panel| {
            panel.spawn((Text::new("traveler"), font(22.0), TextColor(INK_PRIMARY)));
            panel.spawn((
                Text::new("folder to scan"),
                font(13.0),
                TextColor(INK_MUTED),
            ));
            panel.spawn((
                Text::new(""),
                font(15.0),
                TextColor(INK_SECONDARY),
                PathField,
            ));
            panel.spawn((Text::new(""), font(13.0), TextColor(ERROR_INK), ErrorLine));
            panel.spawn((
                Text::new("enter to open   esc to clear"),
                font(12.0),
                TextColor(INK_MUTED),
            ));
        });
}

pub fn input(
    mut keys: MessageReader<KeyboardInput>,
    mut launcher: ResMut<Launcher>,
    mut commands: Commands,
    mut next: ResMut<NextState<Mode>>,
) {
    for key in keys.read() {
        if key.state != ButtonState::Pressed {
            continue;
        }
        match key.key_code {
            KeyCode::Enter | KeyCode::NumpadEnter => {
                let path = launcher.path.trim();
                if path.is_empty() {
                    launcher.error = Some("type a folder to scan".into());
                    continue;
                }
                commands.insert_resource(Request::Disk(PathBuf::from(path)));
                next.set(Mode::Loading);
                return;
            }
            KeyCode::Backspace => {
                launcher.path.pop();
                launcher.error = None;
            }
            KeyCode::Escape => {
                launcher.path.clear();
                launcher.error = None;
            }
            _ => {
                // `text` is None for keys that produce none, and control characters arrive here
                // for enter and backspace on some platforms.
                if let Some(text) = &key.text
                    && !text.chars().any(char::is_control)
                {
                    launcher.path.push_str(text);
                    launcher.error = None;
                }
            }
        }
    }
}

pub fn update(
    launcher: Res<Launcher>,
    mut field: Query<&mut Text, (With<PathField>, Without<ErrorLine>)>,
    mut error: Query<&mut Text, With<ErrorLine>>,
) {
    if !launcher.is_changed() {
        return;
    }
    if let Ok(mut text) = field.single_mut() {
        text.0 = format!("{}_", launcher.path);
    }
    if let Ok(mut text) = error.single_mut() {
        text.0 = launcher.error.clone().unwrap_or_default();
    }
}
