//! Execution control in the app: a breakpoint on what the crosshair is holding, and what the
//! target stopped on.
//!
//! §12 predicted this would sit entirely on `AttachedTarget` and change nothing about the capture,
//! layout or render path. It does: nothing here touches a frame.

use bevy::prelude::*;
use traveler_proc::StopReason;

use crate::pin::Pinned;
use crate::poke::address;
use crate::space::{Space, Traced};
use crate::symbols::Symbols;

#[derive(Resource, Default)]
pub struct Control {
    /// What the target last stopped on, and where the stack said it came from.
    pub reason: Option<StopReason>,
    pub frames: Vec<String>,
}

impl Control {
    pub fn summary(&self, target: &Traced) -> String {
        let breaks = target.0.breakpoints().len();
        match self.reason {
            None if breaks == 0 => "B sets a breakpoint on the pin".to_string(),
            None => format!("{breaks} breakpoint(s), N runs to the next"),
            Some(reason) => format!("stopped: {reason:?}   ({breaks} set, N continues)"),
        }
    }
}

pub fn input(
    keys: Res<ButtonInput<KeyCode>>,
    pinned: Res<Pinned>,
    space: Res<Space>,
    mut symbols: ResMut<Symbols>,
    mut traced: ResMut<Traced>,
    mut control: ResMut<Control>,
) {
    if keys.just_pressed(KeyCode::KeyB)
        && let Some(target) = pinned.target
    {
        let at = address(&space, target);
        // A second press on the same address lifts it, which is what a toggle has to mean when
        // the only thing naming the address is where the crosshair is pointing.
        let held = traced.0.breakpoints().at(at).is_some();
        let done = if held {
            traced.0.clear_breakpoint(at)
        } else {
            traced.0.set_breakpoint(at)
        };
        if let Err(e) = done {
            eprintln!("error: {e}");
        }
    }

    if !keys.just_pressed(KeyCode::KeyN) {
        return;
    }
    match traced.0.resume_until() {
        Ok(reason) => {
            control.reason = Some(reason);
            control.frames = describe(&space, &mut symbols, &traced);
        }
        Err(e) => eprintln!("error: {e}"),
    }
}

/// The stack, named through whatever tier is available. A frame with no name is still a frame,
/// and saying its address is more honest than dropping it.
fn describe(space: &Space, symbols: &mut Symbols, traced: &Traced) -> Vec<String> {
    let Ok(frames) = traced.0.backtrace(16) else {
        return Vec::new();
    };
    frames
        .iter()
        .map(|frame| {
            let named = space
                .frame
                .mapping_at(frame.pc)
                .and_then(|id| space.frame.backing(id))
                .and_then(|(path, _)| {
                    let path = path.to_string();
                    symbols.name_at(&path, frame.pc)
                });
            match named {
                Some((name, 0)) => format!("{:016x}  {name}", frame.pc),
                Some((name, into)) => format!("{:016x}  {name} +{into:#x}", frame.pc),
                None => format!("{:016x}", frame.pc),
            }
        })
        .collect()
}
