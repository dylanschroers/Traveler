//! The inside of a running process, as an address space.
//!
//! Two ways in, one thing out: `AttachedTarget` is "a stopped process I may read and write", and
//! everything downstream of it is platform-neutral. Capability flags rather than a lowest common
//! denominator, so a backend says what it can answer and callers degrade explicitly.

pub mod maps;
pub mod pages;
mod space;

#[cfg(target_os = "linux")]
mod linux;

pub use pages::{PageBits, PageLimits};
pub use space::{Backing, Frame, MappingFacts, NodeKind, Prot};

#[cfg(target_os = "linux")]
pub use linux::{AttachedTarget, CaptureOptions, Launch};

use std::sync::Arc;
use std::time::{Duration, SystemTime};

use traveler_core::{NodeId, Tree};

/// What a backend could actually answer. A view that needs a bit the backend lacks says so in the
/// panel; it does not silently render zeros, and it does not get compiled out.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Caps(pub u32);

impl Caps {
    pub const PAGE_RESIDENCY: u32 = 1 << 0;
    pub const PAGE_SWAPPED: u32 = 1 << 1;
    pub const SOFT_DIRTY: u32 = 1 << 2;
    pub const PAGE_SHARED: u32 = 1 << 3;
    pub const WRITE_TRACKING: u32 = 1 << 4;
    pub const BACKING_PATH: u32 = 1 << 5;

    #[inline]
    pub fn has(self, cap: u32) -> bool {
        self.0 & cap != 0
    }
}

#[derive(Clone, Debug)]
pub struct CaptureMeta {
    pub pid: u32,
    pub wall: SystemTime,
    /// Since the target was acquired, so a sequence of captures has a timeline.
    pub monotonic: Duration,
    /// How long the target was frozen. The number §7 exists to minimise.
    pub stop: Duration,
    pub threads: usize,
    /// Every thread was stopped for the whole read. False means the snapshot may be torn.
    pub consistent: bool,
}

/// One capture, over a frame. Every column is parallel to that frame's ids.
pub struct AddressSpace {
    frame: Arc<Frame>,
    present: Vec<bool>,
    prot: Vec<Prot>,
    /// Page state per mapping, never per page as nodes. See §9.
    pages: PageBits,
    /// Resident bytes and unreadable bytes, rolled up so every level of the tree can answer.
    /// Both are exactly additive, which is what lets a region report what its mappings hold.
    resident: Vec<u64>,
    unknown: Vec<u64>,
    caps: Caps,
    meta: CaptureMeta,
}

impl AddressSpace {
    /// Builds a frame from one capture and projects that capture onto it. A frame spanning
    /// several captures is phase 6h; the projection is the same either way.
    pub fn single(
        root_label: &str,
        mappings: &[MappingFacts<'_>],
        pages: PageBits,
        caps: Caps,
        meta: CaptureMeta,
    ) -> Self {
        let frame = Frame::build(root_label, mappings);
        let n = frame.len();

        // A container's permissions are the union of what it holds, so a region reads as
        // executable when any of it is, rather than as nothing at all.
        let mut prot = vec![Prot::default(); n];
        for (i, m) in mappings.iter().enumerate() {
            prot[frame.mappings().start as usize + i] = m.prot;
        }
        for id in (1..n).rev() {
            let bits = prot[id].0;
            let parent = frame.parent(NodeId(id as u32)).index();
            prot[parent].0 |= bits;
        }

        let mut resident = vec![0u64; n];
        let mut unknown = vec![0u64; n];
        let first = frame.mappings().start as usize;
        for (i, m) in mappings.iter().enumerate() {
            if pages.is_elided(i) {
                unknown[first + i] = m.len;
            } else {
                resident[first + i] = pages.resident(i) as u64 * pages::PAGE;
            }
        }
        for id in (1..n).rev() {
            let parent = frame.parent(NodeId(id as u32)).index();
            resident[parent] += resident[id];
            unknown[parent] += unknown[id];
        }

        Self {
            frame: Arc::new(frame),
            present: vec![true; n],
            prot,
            pages,
            resident,
            unknown,
            caps,
            meta,
        }
    }

    #[inline]
    pub fn pages(&self) -> &PageBits {
        &self.pages
    }

    /// Page state is parallel to the frame's mappings, not to its nodes, so a node that is not a
    /// mapping has none.
    #[inline]
    pub fn mapping_index(&self, id: NodeId) -> Option<usize> {
        let mappings = self.frame.mappings();
        (mappings.start..mappings.end)
            .contains(&id.0)
            .then(|| (id.0 - mappings.start) as usize)
    }

    /// Resident bytes anywhere under `id`, and the bytes below it whose residency was never
    /// read. Reported apart rather than summed, because "not read" is not "not resident".
    #[inline]
    pub fn resident_bytes(&self, id: NodeId) -> u64 {
        self.resident[id.index()]
    }

    #[inline]
    pub fn unknown_bytes(&self, id: NodeId) -> u64 {
        self.unknown[id.index()]
    }

    /// `None` where nothing under `id` was read, rather than zero: a mapping whose pages were
    /// never looked at is not a mapping with nothing resident.
    pub fn resident_fraction(&self, id: NodeId) -> Option<f32> {
        let known = self
            .frame
            .weight(id)
            .checked_sub(self.unknown[id.index()])?;
        (known > 0).then(|| self.resident[id.index()] as f32 / known as f32)
    }

    #[inline]
    pub fn frame(&self) -> &Arc<Frame> {
        &self.frame
    }
    #[inline]
    pub fn present(&self, id: NodeId) -> bool {
        self.present[id.index()]
    }
    #[inline]
    pub fn prot(&self, id: NodeId) -> Prot {
        self.prot[id.index()]
    }
    #[inline]
    pub fn caps(&self) -> Caps {
        self.caps
    }
    #[inline]
    pub fn meta(&self) -> &CaptureMeta {
        &self.meta
    }
}

#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    /// A ptrace request failed. `call` is the request name so the message says what was refused.
    Ptrace {
        call: &'static str,
        pid: i32,
        source: std::io::Error,
    },
    /// Attaching was refused. Carries the explanation rather than just the errno, because
    /// `ptrace_scope` is the usual cause and the fix is not obvious.
    Denied(String),
    Gone(u32),
    Unsupported(&'static str),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(e) => write!(f, "{e}"),
            Error::Ptrace { call, pid, source } => write!(f, "{call} on {pid} failed: {source}"),
            Error::Denied(why) => write!(f, "{why}"),
            Error::Gone(pid) => write!(f, "process {pid} is gone"),
            Error::Unsupported(what) => write!(f, "{what} is not supported on this platform"),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Error::Io(e) => Some(e),
            Error::Ptrace { source, .. } => Some(source),
            _ => None,
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, Error>;
