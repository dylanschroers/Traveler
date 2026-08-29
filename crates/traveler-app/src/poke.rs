//! Writing bytes into the target.
//!
//! Snapshot semantics make this nearly free, which is the payoff for choosing them: a poke happens
//! at a stop, so there is no race with the target's threads, no torn write, and nothing to reason
//! about regarding memory ordering against a running program.
//!
//! Untyped, because tier 3 is phase 8. Without types a poke is "write these bytes at this
//! address" with the width said out loud; with them it becomes "set this field to this value",
//! which is a formatting layer over exactly this.

use bevy::input::ButtonState;
use bevy::input::keyboard::KeyboardInput;
use bevy::prelude::*;
use traveler_proc::pages::PAGE;

use crate::pin::Pinned;
use crate::scene::Target;
use crate::space::{Space, Traced};

#[derive(Resource, Default)]
pub struct Poke {
    pub open: bool,
    pub text: String,
    /// What happened to the last write, kept on screen until the next one.
    pub result: Option<String>,
}

/// Parses a typed literal into the bytes to write, native-endian.
///
/// The width is always explicit or defaulted out loud, because "write 1 here" is ambiguous about
/// how many bytes it disturbs and that ambiguity is not one to leave in a tool that writes to
/// another process's memory.
pub fn parse(input: &str) -> Result<(Vec<u8>, &'static str), String> {
    let input = input.trim();
    if input.is_empty() {
        return Err("nothing to write".into());
    }
    let (head, rest) = input.split_once(' ').unwrap_or((input, ""));
    let rest = rest.trim();

    let int = |text: &str, width: usize, signed: bool| -> Result<Vec<u8>, String> {
        let value = parse_int(text, signed)?;
        let bytes = value.to_le_bytes();
        if width < 8 {
            // Reject a value that would not survive the width, rather than truncating it.
            let fits = if signed {
                let shift = 64 - width * 8;
                ((value << shift) as i64 >> shift) as u64 == value
            } else {
                value >> (width * 8) == 0
            };
            if !fits {
                return Err(format!("{text} does not fit in {width} byte(s)"));
            }
        }
        Ok(bytes[..width].to_vec())
    };

    Ok(match head {
        "u8" => (int(rest, 1, false)?, "u8"),
        "u16" => (int(rest, 2, false)?, "u16"),
        "u32" => (int(rest, 4, false)?, "u32"),
        "u64" => (int(rest, 8, false)?, "u64"),
        "i8" => (int(rest, 1, true)?, "i8"),
        "i16" => (int(rest, 2, true)?, "i16"),
        "i32" => (int(rest, 4, true)?, "i32"),
        "i64" => (int(rest, 8, true)?, "i64"),
        "f32" => (
            rest.parse::<f32>()
                .map_err(|_| format!("{rest} is not an f32"))?
                .to_le_bytes()
                .to_vec(),
            "f32",
        ),
        "f64" => (
            rest.parse::<f64>()
                .map_err(|_| format!("{rest} is not an f64"))?
                .to_le_bytes()
                .to_vec(),
            "f64",
        ),
        "utf8" => {
            if rest.is_empty() {
                return Err("utf8 needs some text".into());
            }
            (rest.as_bytes().to_vec(), "utf8")
        }
        "bytes" => (parse_bytes(rest)?, "bytes"),
        // No prefix is a 64-bit word, said in the confirmation so the width is never a surprise.
        _ => (int(input, 8, false)?, "u64"),
    })
}

fn parse_int(text: &str, signed: bool) -> Result<u64, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("no value".into());
    }
    if let Some(hex) = text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        return u64::from_str_radix(hex, 16).map_err(|_| format!("{text} is not a number"));
    }
    if signed || text.starts_with('-') {
        return text
            .parse::<i64>()
            .map(|v| v as u64)
            .map_err(|_| format!("{text} is not a number"));
    }
    text.parse::<u64>()
        .map_err(|_| format!("{text} is not a number"))
}

fn parse_bytes(text: &str) -> Result<Vec<u8>, String> {
    let joined: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    if joined.is_empty() {
        return Err("bytes needs some hex".into());
    }
    if !joined.len().is_multiple_of(2) {
        return Err("hex needs an even number of digits".into());
    }
    (0..joined.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&joined[i..i + 2], 16).map_err(|_| "not hex".to_string()))
        .collect()
}

/// The address a pinned target names, which for a page is the page itself.
pub fn address(space: &Space, target: Target) -> u64 {
    match target {
        Target::Node(id) => space.frame.addr(id),
        Target::Page { mapping, page } => space.frame.addr(mapping) + page as u64 * PAGE,
    }
}

pub fn input(
    mut keys: MessageReader<KeyboardInput>,
    mut poke: ResMut<Poke>,
    pinned: Res<Pinned>,
    space: Res<Space>,
    mut symbols: ResMut<crate::symbols::Symbols>,
    mut traced: ResMut<Traced>,
) {
    for key in keys.read() {
        if key.state != ButtonState::Pressed {
            continue;
        }
        if !poke.open {
            // Only with something held: a poke needs an address, and the crosshair moves.
            if key.key_code == KeyCode::KeyP
                && let Some(target) = pinned.target
            {
                poke.open = true;
                poke.result = None;
                // With tier 3, the prompt opens knowing the width: a typed poke is the untyped
                // one with the type filled in, not a second way of writing bytes.
                poke.text = typed_prefix(&space, &mut symbols, target).unwrap_or_default();
            }
            continue;
        }
        match key.key_code {
            KeyCode::Escape => {
                poke.open = false;
                poke.text.clear();
            }
            KeyCode::Backspace => {
                poke.text.pop();
            }
            KeyCode::Enter | KeyCode::NumpadEnter => {
                let Some(target) = pinned.target else {
                    poke.open = false;
                    continue;
                };
                let at = address(&space, target);
                poke.result = Some(match parse(&poke.text) {
                    Ok((bytes, kind)) => match traced.0.write_mem(at, &bytes) {
                        Ok(n) => format!("wrote {n} bytes as {kind} at {at:#018x}"),
                        Err(e) => format!("{e}"),
                    },
                    Err(why) => why,
                });
                poke.open = false;
                poke.text.clear();
            }
            _ => {
                if let Some(text) = key.text.clone()
                    && !text.chars().any(char::is_control)
                {
                    poke.text.push_str(&text);
                }
            }
        }
    }
}

/// The literal prefix for whatever the pin is on, when debug information says what it is.
fn typed_prefix(
    space: &Space,
    symbols: &mut crate::symbols::Symbols,
    target: Target,
) -> Option<String> {
    let (path, _) = space.frame.backing(target.node())?;
    let path = path.to_string();
    let literal = symbols.literal_at(&path, address(space, target))?;
    Some(format!("{literal} "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(input: &str) -> Vec<u8> {
        parse(input).expect(input).0
    }

    #[test]
    fn a_bare_number_is_a_word_and_says_so() {
        let (b, kind) = parse("1").expect("parse");
        assert_eq!(b, 1u64.to_le_bytes());
        assert_eq!(kind, "u64");
        assert_eq!(bytes("0xff"), 255u64.to_le_bytes());
    }

    #[test]
    fn every_width_writes_exactly_its_own_bytes() {
        assert_eq!(bytes("u8 255"), vec![0xff]);
        assert_eq!(bytes("u16 0x1234"), vec![0x34, 0x12]);
        assert_eq!(bytes("u32 1"), vec![1, 0, 0, 0]);
        assert_eq!(bytes("u64 1").len(), 8);
        assert_eq!(bytes("i8 -1"), vec![0xff]);
        assert_eq!(bytes("i32 -1"), vec![0xff; 4]);
    }

    /// Truncating silently would disturb fewer bytes than the number implies, in a tool that
    /// writes to another process's memory.
    #[test]
    fn a_value_too_wide_for_its_type_is_refused_rather_than_truncated() {
        assert!(parse("u8 256").is_err());
        assert!(parse("u16 0x10000").is_err());
        assert!(parse("i8 128").is_err());
        assert!(parse("u8 255").is_ok());
        assert!(parse("i8 -128").is_ok());
    }

    #[test]
    fn floats_and_text_and_raw_hex_all_work() {
        assert_eq!(bytes("f32 1.5"), 1.5f32.to_le_bytes());
        assert_eq!(bytes("f64 -0.25"), (-0.25f64).to_le_bytes());
        assert_eq!(bytes("utf8 hi"), b"hi");
        assert_eq!(bytes("bytes de ad be ef"), vec![0xde, 0xad, 0xbe, 0xef]);
        assert_eq!(bytes("bytes deadbeef"), vec![0xde, 0xad, 0xbe, 0xef]);
    }

    #[test]
    fn junk_is_refused_with_a_reason() {
        for input in [
            "",
            "   ",
            "u8",
            "u8 nope",
            "f32 nope",
            "bytes zz",
            "bytes abc",
            "utf8",
        ] {
            let error = parse(input).expect_err(input);
            assert!(!error.is_empty(), "{input} refused with no reason");
        }
    }
}
