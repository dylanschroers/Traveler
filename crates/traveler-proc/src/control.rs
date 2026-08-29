//! Execution control: breakpoints, stepping, and where the stack came from.
//!
//! §12 said the seam for this is `AttachedTarget`, which already owns stop, resume, read, write
//! and the thread list, and that breakpoints add an instruction-patch table, a stop-reason enum,
//! single-step and unwinding — and that **none of it changes the capture, layout, or render path**,
//! which is the test of whether the seam is in the right place. It held: nothing in this module is
//! reachable from a capture.

/// The one-byte trap on x86-64. Patching the first byte of an instruction is what a breakpoint is.
pub const TRAP: u8 = 0xcc;

/// What a resume ended in.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum StopReason {
    /// A breakpoint at this address. The program counter has already been rewound onto it, so
    /// resuming continues from the instruction rather than from the middle of it.
    Breakpoint(u64),
    /// One instruction completed.
    Step,
    /// A signal was delivered. Re-delivered on the next resume unless it was ours.
    Signal(i32),
    Exited(i32),
    Killed(i32),
}

/// One patched instruction, and the byte that was there before it.
#[derive(Clone, Copy, Debug)]
pub struct Breakpoint {
    pub addr: u64,
    pub original: u8,
    pub enabled: bool,
}

/// The patch table. Small and linear on purpose: a person sets a handful of breakpoints, and a
/// map would cost more to reason about than the scan costs to run.
#[derive(Default, Debug)]
pub struct Breakpoints {
    set: Vec<Breakpoint>,
}

impl Breakpoints {
    pub fn iter(&self) -> impl Iterator<Item = &Breakpoint> {
        self.set.iter()
    }

    pub fn len(&self) -> usize {
        self.set.len()
    }

    pub fn is_empty(&self) -> bool {
        self.set.is_empty()
    }

    pub fn at(&self, addr: u64) -> Option<&Breakpoint> {
        self.set.iter().find(|b| b.addr == addr && b.enabled)
    }

    pub(crate) fn insert(&mut self, addr: u64, original: u8) {
        match self.set.iter_mut().find(|b| b.addr == addr) {
            Some(existing) => {
                existing.enabled = true;
            }
            None => self.set.push(Breakpoint {
                addr,
                original,
                enabled: true,
            }),
        }
    }

    pub(crate) fn take(&mut self, addr: u64) -> Option<Breakpoint> {
        let i = self.set.iter().position(|b| b.addr == addr && b.enabled)?;
        let removed = self.set[i];
        self.set.remove(i);
        Some(removed)
    }
}

/// One level of the call stack.
#[derive(Clone, Copy, Debug)]
pub struct Frame {
    pub pc: u64,
    /// The frame's base pointer, and where the next one was found.
    pub frame: u64,
}
