//! What to look at, asked before anything is loaded.
//!
//! The scan used to run before the window opened, which a launcher makes untenable: there is
//! nothing to scan until someone says what. So the window comes up first and the target is a
//! request, handed to `loading` and answered on a worker thread.

use std::ffi::OsString;
use std::path::PathBuf;

use bevy::input::ButtonState;
use bevy::input::keyboard::KeyboardInput;
use bevy::prelude::*;
use bevy::state::state_scoped::DespawnOnExit;

use crate::hud::{INK_MUTED, INK_PRIMARY, INK_SECONDARY, PANEL, font};
use crate::loading::Request;
use crate::{Mode, Session};

const ERROR_INK: Color = Color::srgb(0.90, 0.45, 0.35);

/// What the launcher is currently asking for. The three the plan settled on, and no more: a
/// folder, a program to start, or a process already running.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Choice {
    Disk,
    Launch,
    Attach,
}

impl Choice {
    const ALL: [Choice; 3] = [Choice::Disk, Choice::Launch, Choice::Attach];

    fn label(self) -> &'static str {
        match self {
            Choice::Disk => "folder",
            Choice::Launch => "run",
            Choice::Attach => "attach",
        }
    }

    fn prompt(self) -> &'static str {
        match self {
            Choice::Disk => "folder to scan",
            Choice::Launch => "program to launch, then its arguments",
            Choice::Attach => "pid to attach to",
        }
    }
}

#[derive(Resource)]
pub struct Launcher {
    pub choice: Choice,
    pub path: String,
    pub command: String,
    pub pid: String,
    /// Why the last attempt did not open, kept so the message survives the bounce back here.
    pub error: Option<String>,
}

impl Default for Launcher {
    fn default() -> Self {
        Self {
            choice: Choice::Disk,
            path: std::env::current_dir()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            command: String::new(),
            pid: String::new(),
            error: None,
        }
    }
}

impl Launcher {
    fn field(&mut self) -> &mut String {
        match self.choice {
            Choice::Disk => &mut self.path,
            Choice::Launch => &mut self.command,
            Choice::Attach => &mut self.pid,
        }
    }

    fn text(&self) -> &str {
        match self.choice {
            Choice::Disk => &self.path,
            Choice::Launch => &self.command,
            Choice::Attach => &self.pid,
        }
    }

    /// Arguments are split on whitespace and nothing else. Quoting would be a shell, and a target
    /// that needs one can be launched with `--run` from a real one.
    pub(crate) fn request(&self, no_aslr: bool) -> Result<Request, &'static str> {
        let text = self.text().trim();
        if text.is_empty() {
            return Err("nothing to open");
        }
        Ok(match self.choice {
            Choice::Disk => Request::Disk(PathBuf::from(text)),
            Choice::Launch => {
                let mut words = text.split_whitespace().map(OsString::from);
                Request::Launch {
                    program: words.next().ok_or("nothing to open")?,
                    args: words.collect(),
                    no_aslr,
                }
            }
            Choice::Attach => Request::Attach(text.parse().map_err(|_| "a pid is a number")?),
        })
    }
}

#[derive(Component)]
pub struct PathField;

#[derive(Component)]
pub struct ErrorLine;

#[derive(Component)]
pub struct ChoiceLine;

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
            panel.spawn((Text::new(""), font(13.0), TextColor(INK_MUTED), ChoiceLine));
            panel.spawn((
                Text::new(""),
                font(15.0),
                TextColor(INK_SECONDARY),
                PathField,
            ));
            panel.spawn((Text::new(""), font(13.0), TextColor(ERROR_INK), ErrorLine));
            panel.spawn((
                Text::new("tab switches   enter opens   esc clears"),
                font(12.0),
                TextColor(INK_MUTED),
            ));
        });
}

pub fn input(
    mut keys: MessageReader<KeyboardInput>,
    mut launcher: ResMut<Launcher>,
    session: Res<Session>,
    mut commands: Commands,
    mut next: ResMut<NextState<Mode>>,
) {
    for key in keys.read() {
        if key.state != ButtonState::Pressed {
            continue;
        }
        match key.key_code {
            KeyCode::Tab => {
                let at = Choice::ALL.iter().position(|c| *c == launcher.choice);
                launcher.choice = Choice::ALL[(at.unwrap_or(0) + 1) % Choice::ALL.len()];
                launcher.error = None;
            }
            KeyCode::Enter | KeyCode::NumpadEnter => match launcher.request(session.no_aslr) {
                Ok(request) => {
                    commands.insert_resource(request);
                    next.set(Mode::Loading);
                    return;
                }
                Err(why) => launcher.error = Some(why.to_string()),
            },
            KeyCode::Backspace => {
                launcher.field().pop();
                launcher.error = None;
            }
            KeyCode::Escape => {
                launcher.field().clear();
                launcher.error = None;
            }
            _ => {
                // `text` is None for keys that produce none, and control characters arrive here
                // for enter and backspace on some platforms.
                if let Some(text) = key.text.clone()
                    && !text.chars().any(char::is_control)
                {
                    launcher.field().push_str(&text);
                    launcher.error = None;
                }
            }
        }
    }
}

type Line<'w, 's, Marker, A, B> =
    Query<'w, 's, &'static mut Text, (With<Marker>, Without<A>, Without<B>)>;

pub fn update(
    launcher: Res<Launcher>,
    mut field: Line<PathField, ErrorLine, ChoiceLine>,
    mut error: Line<ErrorLine, PathField, ChoiceLine>,
    mut choice: Line<ChoiceLine, PathField, ErrorLine>,
) {
    if !launcher.is_changed() {
        return;
    }
    if let Ok(mut text) = choice.single_mut() {
        let tabs: Vec<String> = Choice::ALL
            .iter()
            .map(|c| {
                if *c == launcher.choice {
                    format!("[{}]", c.label())
                } else {
                    format!(" {} ", c.label())
                }
            })
            .collect();
        text.0 = format!("{}    {}", tabs.join(" "), launcher.choice.prompt());
    }
    if let Ok(mut text) = field.single_mut() {
        text.0 = format!("{}_", launcher.text());
    }
    if let Ok(mut text) = error.single_mut() {
        text.0 = launcher.error.clone().unwrap_or_default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn with(choice: Choice, text: &str) -> Launcher {
        let mut l = Launcher {
            choice,
            path: String::new(),
            command: String::new(),
            pid: String::new(),
            error: None,
        };
        l.field().push_str(text);
        l
    }

    #[test]
    fn a_folder_becomes_a_scan() {
        let l = with(Choice::Disk, "  /tmp/x  ");
        assert!(matches!(l.request(false), Ok(Request::Disk(p)) if p == Path::new("/tmp/x")));
    }

    #[test]
    fn a_command_line_splits_into_a_program_and_its_arguments() {
        let l = with(Choice::Launch, "/bin/ls -l /tmp");
        match l.request(true).expect("a request") {
            Request::Launch {
                program,
                args,
                no_aslr,
            } => {
                assert_eq!(program, "/bin/ls");
                assert_eq!(args, ["-l", "/tmp"]);
                assert!(no_aslr);
            }
            other => panic!("wrong request: {other:?}"),
        }
    }

    #[test]
    fn a_pid_has_to_be_a_number() {
        assert!(matches!(
            with(Choice::Attach, "1234").request(false),
            Ok(Request::Attach(1234))
        ));
        assert!(with(Choice::Attach, "init").request(false).is_err());
    }

    #[test]
    fn an_empty_field_opens_nothing() {
        for choice in Choice::ALL {
            assert!(with(choice, "   ").request(false).is_err());
        }
    }

    /// Each choice keeps its own text, so switching tabs does not eat what was typed.
    #[test]
    fn the_three_fields_are_independent() {
        let mut l = with(Choice::Disk, "/tmp");
        l.choice = Choice::Launch;
        l.field().push_str("/bin/ls");
        l.choice = Choice::Disk;
        assert_eq!(l.text(), "/tmp");
        l.choice = Choice::Launch;
        assert_eq!(l.text(), "/bin/ls");
    }
}
