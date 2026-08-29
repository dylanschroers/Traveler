//! The snapshot: a frame of address intervals, and one capture's state over it.
//!
//! Ids belong to the frame, not to the capture. Numbering each capture on its own would move
//! every id above a newly mapped extent, and the `Tree` contract forbids appending, so a capture
//! is matched onto a frame by address and an id means nothing outside the frame that minted it.

use std::ops::Range;

use traveler_core::{NodeId, Tree};

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Prot(pub u8);

impl Prot {
    pub const READ: u8 = 1 << 0;
    pub const WRITE: u8 = 1 << 1;
    pub const EXEC: u8 = 1 << 2;
    pub const SHARED: u8 = 1 << 3;

    #[inline]
    pub fn is_read(self) -> bool {
        self.0 & Self::READ != 0
    }
    #[inline]
    pub fn is_write(self) -> bool {
        self.0 & Self::WRITE != 0
    }
    #[inline]
    pub fn is_exec(self) -> bool {
        self.0 & Self::EXEC != 0
    }
    #[inline]
    pub fn is_shared(self) -> bool {
        self.0 & Self::SHARED != 0
    }

    /// Parses the four-character field from a `maps` line, e.g. `r-xp`.
    pub fn parse(field: &str) -> Prot {
        let mut bits = 0;
        for (i, c) in field.bytes().take(4).enumerate() {
            bits |= match (i, c) {
                (0, b'r') => Self::READ,
                (1, b'w') => Self::WRITE,
                (2, b'x') => Self::EXEC,
                (3, b's') => Self::SHARED,
                _ => 0,
            };
        }
        Prot(bits)
    }
}

impl std::fmt::Display for Prot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let c = |b: u8, on: char| if self.0 & b != 0 { on } else { '-' };
        write!(
            f,
            "{}{}{}{}",
            c(Prot::READ, 'r'),
            c(Prot::WRITE, 'w'),
            c(Prot::EXEC, 'x'),
            if self.is_shared() { 's' } else { 'p' }
        )
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NodeKind {
    Process,
    Region,
    Mapping,
    Section,
    Gap,
}

/// Slice of the interned text blob. A zero length means "none".
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct StrRef {
    off: u32,
    len: u32,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Backing {
    path: StrRef,
    pub offset: u64,
}

/// One line of `maps`, or one `VirtualQueryEx` region, before anything is grouped.
#[derive(Clone, Copy, Debug)]
pub struct MappingFacts<'a> {
    pub addr: u64,
    pub len: u64,
    pub prot: Prot,
    /// Empty for anonymous memory. Pseudo-paths such as `[heap]` count as names of their own.
    pub path: &'a str,
    pub offset: u64,
    pub dev: u64,
    pub inode: u64,
    /// The allocation this mapping came from, when the operating system records one, and 0 when
    /// it does not. Windows has `AllocationBase`; Linux has nothing, and falls back to the rule
    /// in §2 — adjacency plus a shared backing.
    pub group: u64,
}

impl MappingFacts<'_> {
    #[inline]
    fn end(&self) -> u64 {
        self.addr + self.len
    }

    /// Two mappings belong to one region when they are address-adjacent and name the same
    /// backing. This is the Linux stand-in for `AllocationBase`: a library's segments fuse
    /// because they share a file, a thread stack fuses with its guard because both are unnamed.
    #[inline]
    fn same_backing(&self, other: &Self) -> bool {
        if self.group != 0 || other.group != 0 {
            return self.group == other.group;
        }
        self.dev == other.dev && self.inode == other.inode && self.path == other.path
    }
}

/// The ordered set of extents ids are assigned over, and what a layout is a pure function of.
pub struct Frame {
    parent: Vec<NodeId>,
    child_start: Vec<u32>,
    child_len: Vec<u32>,
    depth: Vec<u16>,

    addr: Vec<u64>,
    len: Vec<u64>,

    kind: Vec<NodeKind>,
    label: Vec<StrRef>,
    backing: Vec<Backing>,

    text: String,
    mappings: Range<u32>,
}

impl Tree for Frame {
    #[inline]
    fn len(&self) -> usize {
        self.parent.len()
    }
    #[inline]
    fn root(&self) -> NodeId {
        NodeId(0)
    }
    #[inline]
    fn parent(&self, id: NodeId) -> NodeId {
        self.parent[id.index()]
    }
    #[inline]
    fn children(&self, id: NodeId) -> Range<u32> {
        let start = self.child_start[id.index()];
        start..start + self.child_len[id.index()]
    }
    #[inline]
    fn depth(&self, id: NodeId) -> u16 {
        self.depth[id.index()]
    }
    #[inline]
    fn weight(&self, id: NodeId) -> u64 {
        self.len[id.index()]
    }
}

impl Frame {
    #[inline]
    pub fn addr(&self, id: NodeId) -> u64 {
        self.addr[id.index()]
    }
    #[inline]
    pub fn end(&self, id: NodeId) -> u64 {
        self.addr[id.index()] + self.len[id.index()]
    }
    #[inline]
    pub fn kind(&self, id: NodeId) -> NodeKind {
        self.kind[id.index()]
    }
    #[inline]
    pub fn label(&self, id: NodeId) -> &str {
        self.text(self.label[id.index()])
    }
    #[inline]
    pub fn backing(&self, id: NodeId) -> Option<(&str, u64)> {
        let b = self.backing[id.index()];
        (b.path.len > 0).then(|| (self.text(b.path), b.offset))
    }

    /// Mapping ids are one contiguous run in address order, which is what makes the lookup below
    /// a binary search with no index to maintain.
    #[inline]
    pub fn mappings(&self) -> Range<u32> {
        self.mappings.clone()
    }

    pub fn mapping_at(&self, addr: u64) -> Option<NodeId> {
        let ids = self.mappings.clone();
        let lo = ids.start as usize;
        let slice = &self.addr[lo..ids.end as usize];
        let i = match slice.binary_search(&addr) {
            Ok(i) => i,
            Err(0) => return None,
            Err(i) => i - 1,
        };
        let id = NodeId((lo + i) as u32);
        (addr < self.end(id)).then_some(id)
    }

    #[inline]
    fn text(&self, r: StrRef) -> &str {
        &self.text[r.off as usize..(r.off + r.len) as usize]
    }

    /// `mappings` must be sorted by address and must not overlap.
    pub fn build(root_label: &str, mappings: &[MappingFacts<'_>]) -> Frame {
        let regions = region_bounds(mappings);
        let n = 1 + regions.len() + mappings.len();
        let first_mapping = 1 + regions.len() as u32;

        let mut f = Frame {
            parent: Vec::with_capacity(n),
            child_start: Vec::with_capacity(n),
            child_len: Vec::with_capacity(n),
            depth: Vec::with_capacity(n),
            addr: Vec::with_capacity(n),
            len: Vec::with_capacity(n),
            kind: Vec::with_capacity(n),
            label: Vec::with_capacity(n),
            backing: Vec::with_capacity(n),
            text: String::new(),
            mappings: first_mapping..n as u32,
        };
        let mut intern = Interner::default();

        let root = intern.get(root_label);
        f.push(
            NodeId(0),
            0,
            mappings.first().map_or(0, |m| m.addr),
            mappings.iter().map(|m| m.len).sum(),
            NodeKind::Process,
            root,
            Backing::default(),
        );
        f.child_start[0] = 1;
        f.child_len[0] = regions.len() as u32;

        for run in &regions {
            let members = &mappings[run.clone()];
            let head = &members[0];
            let label = intern.get(display_name(head.path));
            let backing = intern.backing(head);
            let id = NodeId(f.parent.len() as u32);
            f.push(
                NodeId(0),
                1,
                head.addr,
                members.iter().map(|m| m.len).sum(),
                NodeKind::Region,
                label,
                backing,
            );
            f.child_start[id.index()] = first_mapping + run.start as u32;
            f.child_len[id.index()] = run.len() as u32;
        }

        for (i, run) in regions.iter().enumerate() {
            let region = NodeId(1 + i as u32);
            for m in &mappings[run.clone()] {
                let label = intern.get(display_name(m.path));
                let backing = intern.backing(m);
                f.push(region, 2, m.addr, m.len, NodeKind::Mapping, label, backing);
            }
        }

        f.text = intern.blob;
        f
    }

    #[allow(clippy::too_many_arguments)]
    fn push(
        &mut self,
        parent: NodeId,
        depth: u16,
        addr: u64,
        len: u64,
        kind: NodeKind,
        label: StrRef,
        backing: Backing,
    ) {
        self.parent.push(parent);
        self.child_start.push(0);
        self.child_len.push(0);
        self.depth.push(depth);
        self.addr.push(addr);
        self.len.push(len);
        self.kind.push(kind);
        self.label.push(label);
        self.backing.push(backing);
    }
}

/// Index ranges of `mappings` that each form one region.
fn region_bounds(mappings: &[MappingFacts<'_>]) -> Vec<Range<usize>> {
    let mut runs: Vec<Range<usize>> = Vec::new();
    for (i, m) in mappings.iter().enumerate() {
        match runs.last_mut() {
            Some(run)
                if mappings[run.end - 1].end() == m.addr
                    && mappings[run.end - 1].same_backing(m) =>
            {
                run.end = i + 1;
            }
            _ => runs.push(i..i + 1),
        }
    }
    runs
}

fn display_name(path: &str) -> &str {
    if path.is_empty() {
        return "anonymous";
    }
    match path.rsplit_once('/') {
        Some((_, name)) if !name.is_empty() => name,
        _ => path,
    }
}

/// A process maps a hundred files across thousands of mappings, so each path is interned once.
#[derive(Default)]
struct Interner {
    blob: String,
    seen: std::collections::HashMap<String, StrRef>,
}

impl Interner {
    fn get(&mut self, s: &str) -> StrRef {
        if s.is_empty() {
            return StrRef::default();
        }
        if let Some(r) = self.seen.get(s) {
            return *r;
        }
        let r = StrRef {
            off: self.blob.len() as u32,
            len: s.len() as u32,
        };
        self.blob.push_str(s);
        self.seen.insert(s.to_string(), r);
        r
    }

    /// A pseudo-path such as `[heap]` is a label, not a file, so it is not a backing.
    fn backing(&mut self, m: &MappingFacts<'_>) -> Backing {
        if m.path.is_empty() || m.path.starts_with('[') {
            return Backing::default();
        }
        Backing {
            path: self.get(m.path),
            offset: m.offset,
        }
    }
}
