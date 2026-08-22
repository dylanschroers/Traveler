//! Phase 1: walk a directory tree into a `FileIndex`.
//!
//! The index is struct-of-arrays, indexed by `NodeId`. At a million files an array-of-structs
//! holding `PathBuf`s is both slow to build and hundreds of megabytes, so nothing here stores a
//! full path: a node knows its name and its parent, and paths are rebuilt on demand.
//!
//! Node ids are allocated parent-before-child, and a directory's children are always a contiguous
//! run. Both facts are load-bearing:
//!
//! - parent-before-child makes the subtree size rollup a single reverse scan, with no recursion
//!   and therefore no stack overflow on a pathologically deep tree.
//! - contiguous children means `children()` is a range, not a lookup, which is what the treemap
//!   in phase 2 wants.

mod walk;

pub use walk::{ScanOptions, scan};

/// Index of a node in a [`FileIndex`]. The root is always `NodeId(0)`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub struct NodeId(pub u32);

impl NodeId {
    #[inline]
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

/// Interned file extension. `ExtId(0)` is always the empty extension.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub struct ExtId(pub u32);

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct NodeFlags(pub u8);

impl NodeFlags {
    pub const DIR: u8 = 1 << 0;
    /// A junction, symlink, or other reparse point. Never descended into.
    pub const REPARSE: u8 = 1 << 1;
    pub const HIDDEN: u8 = 1 << 2;
    /// The directory could not be read, so its size is understated.
    pub const UNREADABLE: u8 = 1 << 3;

    #[inline]
    pub fn is_dir(self) -> bool {
        self.0 & Self::DIR != 0
    }
    #[inline]
    pub fn is_reparse(self) -> bool {
        self.0 & Self::REPARSE != 0
    }
    #[inline]
    pub fn is_hidden(self) -> bool {
        self.0 & Self::HIDDEN != 0
    }
    #[inline]
    pub fn is_unreadable(self) -> bool {
        self.0 & Self::UNREADABLE != 0
    }
}

/// Slice of the interned name blob.
#[derive(Clone, Copy, Debug)]
struct StrRef {
    off: u32,
    len: u32,
}

#[derive(Clone, Debug)]
pub struct ScanError {
    pub path: String,
    pub message: String,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct ScanStats {
    pub files: u64,
    pub dirs: u64,
    /// Logical bytes, i.e. file length, not size on disk. Allocated size is a later option.
    pub bytes: u64,
    /// Reparse points recorded but not descended into.
    pub reparse_points: u64,
    /// Names that were not valid UTF-16 to UTF-8 round trips and got replacement characters.
    pub lossy_names: u64,
    pub walk_millis: u64,
}

pub struct FileIndex {
    parent: Vec<NodeId>,
    name: Vec<StrRef>,
    size: Vec<u64>,
    ext: Vec<ExtId>,
    flags: Vec<NodeFlags>,
    depth: Vec<u16>,
    child_start: Vec<u32>,
    child_len: Vec<u32>,

    names: String,
    ext_text: Vec<String>,

    pub errors: Vec<ScanError>,
    pub stats: ScanStats,
}

impl FileIndex {
    #[inline]
    pub fn len(&self) -> usize {
        self.parent.len()
    }
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.parent.is_empty()
    }
    #[inline]
    pub fn root(&self) -> NodeId {
        NodeId(0)
    }
    #[inline]
    pub fn parent(&self, id: NodeId) -> NodeId {
        self.parent[id.index()]
    }
    #[inline]
    pub fn size(&self, id: NodeId) -> u64 {
        self.size[id.index()]
    }
    #[inline]
    pub fn flags(&self, id: NodeId) -> NodeFlags {
        self.flags[id.index()]
    }
    #[inline]
    pub fn depth(&self, id: NodeId) -> u16 {
        self.depth[id.index()]
    }
    #[inline]
    pub fn ext(&self, id: NodeId) -> ExtId {
        self.ext[id.index()]
    }

    #[inline]
    pub fn name(&self, id: NodeId) -> &str {
        let r = self.name[id.index()];
        &self.names[r.off as usize..(r.off + r.len) as usize]
    }

    #[inline]
    pub fn ext_str(&self, ext: ExtId) -> &str {
        &self.ext_text[ext.index_usize()]
    }

    #[inline]
    pub fn ext_count(&self) -> usize {
        self.ext_text.len()
    }

    /// Children of `id`, always a contiguous id range.
    #[inline]
    pub fn children(&self, id: NodeId) -> impl Iterator<Item = NodeId> + use<> {
        let start = self.child_start[id.index()];
        let end = start + self.child_len[id.index()];
        (start..end).map(NodeId)
    }

    #[inline]
    pub fn child_count(&self, id: NodeId) -> u32 {
        self.child_len[id.index()]
    }

    /// Every node in id order. Parents always precede their children.
    #[inline]
    pub fn iter(&self) -> impl Iterator<Item = NodeId> + use<> {
        (0..self.parent.len() as u32).map(NodeId)
    }

    /// Rebuilds the full path by walking up to the root.
    pub fn path(&self, id: NodeId) -> std::path::PathBuf {
        let mut parts = Vec::new();
        let mut cur = id;
        loop {
            parts.push(self.name(cur));
            let p = self.parent(cur);
            if p == cur {
                break;
            }
            cur = p;
        }
        parts.reverse();
        let mut path = std::path::PathBuf::from(parts[0]);
        for part in &parts[1..] {
            path.push(part);
        }
        path
    }
}

impl ExtId {
    #[inline]
    fn index_usize(self) -> usize {
        self.0 as usize
    }
}

/// Byte and count formatting. Lives here because both CLIs and the app print the same numbers and
/// they should read identically; it is the crate everything else already depends on.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    let mut v = bytes as f64;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    if u == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
}

pub fn thousands(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::with_capacity(s.len() + s.len() / 3);
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}
