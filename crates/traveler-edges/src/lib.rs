//! Phase 4: which files belong together.
//!
//! Everything here produces **groups**, never pairs. A group of n members implies n^2 pairwise
//! lines, and at the sizes involved that is instant spaghetti rather than information. The app
//! draws a group as a shared highlight plus a hub, so n lines, and only for the group you select.
//!
//! Providers are independent and cheap to add. The two that need no I/O run on every scan; the
//! duplicate finder reads bytes off the disk and is the only one that costs anything.

mod dupes;
mod extensions;
mod families;

pub use dupes::ExactDuplicates;
pub use extensions::ExtensionGroups;
pub use families::NameFamilies;

use traveler_scan::{FileIndex, NodeId};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum EdgeKind {
    Extension,
    NameFamily,
    Duplicate,
}

impl EdgeKind {
    pub fn label(self) -> &'static str {
        match self {
            EdgeKind::Extension => "extensions",
            EdgeKind::NameFamily => "name families",
            EdgeKind::Duplicate => "duplicates",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Group {
    pub label: String,
    /// Always at least two, and in ascending id order.
    pub members: Vec<NodeId>,
    /// Total bytes across the members.
    pub bytes: u64,
    /// Bytes that would come back by keeping one member and deleting the rest. Only the duplicate
    /// provider sets this; for the others there is nothing redundant to reclaim.
    pub redundant_bytes: u64,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct EdgeStats {
    pub millis: u64,
    pub files_hashed: u64,
    pub bytes_hashed: u64,
    pub read_errors: u64,
    /// Files whose length no longer matched the scan when they were read, so they were dropped
    /// rather than compared against a sibling over a different number of bytes.
    pub changed_while_reading: u64,
    /// Files that were candidates by size but fell under the duplicate finder's size floor.
    pub skipped_small: u64,
}

#[derive(Clone, Debug)]
pub struct EdgeSet {
    pub kind: EdgeKind,
    /// Sorted by `bytes` descending, so the interesting ones are first.
    pub groups: Vec<Group>,
    pub stats: EdgeStats,
}

impl EdgeSet {
    pub fn total_bytes(&self) -> u64 {
        self.groups.iter().map(|g| g.bytes).sum()
    }

    pub fn total_redundant(&self) -> u64 {
        self.groups.iter().map(|g| g.redundant_bytes).sum()
    }

    pub fn members(&self) -> usize {
        self.groups.iter().map(|g| g.members.len()).sum()
    }

    /// Sorts groups by bytes descending. Every provider ends with this.
    fn finish(kind: EdgeKind, mut groups: Vec<Group>, stats: EdgeStats) -> Self {
        groups.sort_unstable_by(|a, b| b.bytes.cmp(&a.bytes).then(a.label.cmp(&b.label)));
        EdgeSet {
            kind,
            groups,
            stats,
        }
    }
}

pub trait EdgeProvider {
    fn kind(&self) -> EdgeKind;
    fn compute(&self, index: &FileIndex) -> EdgeSet;
}

/// The providers that cost nothing but a pass over the index.
pub fn free_providers() -> Vec<Box<dyn EdgeProvider>> {
    vec![Box::new(ExtensionGroups), Box::new(NameFamilies)]
}
