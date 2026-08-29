//! The vocabulary two index types share: node ids, the tree they form, and the number formatting
//! that every front end prints.
//!
//! It exists so `traveler-proc` can talk about a tree without depending on `traveler-scan`, which
//! would drag a directory walker and rayon along to borrow a `u32` newtype.

use std::ops::Range;

/// Index of a node in a tree. The root is always `NodeId(0)`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub struct NodeId(pub u32);

impl NodeId {
    #[inline]
    pub fn index(self) -> usize {
        self.0 as usize
    }
}

/// Iterator over a contiguous id range. Concrete rather than `impl Iterator` so the trait below
/// stays simple to implement and to name.
#[derive(Clone, Debug)]
pub struct NodeIter(Range<u32>);

impl NodeIter {
    #[inline]
    pub fn new(range: Range<u32>) -> Self {
        Self(range)
    }
}

impl Iterator for NodeIter {
    type Item = NodeId;

    #[inline]
    fn next(&mut self) -> Option<NodeId> {
        self.0.next().map(NodeId)
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        self.0.size_hint()
    }
}

impl DoubleEndedIterator for NodeIter {
    #[inline]
    fn next_back(&mut self) -> Option<NodeId> {
        self.0.next_back().map(NodeId)
    }
}

impl ExactSizeIterator for NodeIter {}

/// The minimum a layout needs from an index.
///
/// Every method is one array index, and layouts take `T: Tree` generically rather than
/// `&dyn Tree`: these are called once per node per layout and a vtable hop in that loop is
/// measurable at a million nodes.
pub trait Tree {
    fn len(&self) -> usize;
    fn root(&self) -> NodeId;
    fn parent(&self, id: NodeId) -> NodeId;
    /// Children are always a contiguous id range, enforced by construction in every implementor.
    /// Returning the range rather than an iterator keeps that invariant in the type, so a new
    /// index cannot quietly drop the property the whole layout depends on.
    fn children(&self, id: NodeId) -> Range<u32>;
    fn depth(&self, id: NodeId) -> u16;
    /// Layout weight before any floor: bytes for a file, length for an address interval. A
    /// group's weight is the total of its subtree.
    fn weight(&self, id: NodeId) -> u64;

    /// A container, drawn as a slab with its children resting on it rather than as content.
    /// Defaults to "has children", which is exactly right for an address space; `FileIndex`
    /// overrides it so an empty directory still reads as a folder.
    #[inline]
    fn is_group(&self, id: NodeId) -> bool {
        !self.children(id).is_empty()
    }

    #[inline]
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[inline]
    fn child_count(&self, id: NodeId) -> u32 {
        let r = self.children(id);
        r.end - r.start
    }

    #[inline]
    fn child_ids(&self, id: NodeId) -> NodeIter {
        NodeIter(self.children(id))
    }

    /// Every node in id order. Parents always precede their children.
    #[inline]
    fn nodes(&self) -> NodeIter {
        NodeIter(0..self.len() as u32)
    }
}

impl<T: Tree + ?Sized> Tree for &T {
    #[inline]
    fn len(&self) -> usize {
        (**self).len()
    }
    #[inline]
    fn root(&self) -> NodeId {
        (**self).root()
    }
    #[inline]
    fn parent(&self, id: NodeId) -> NodeId {
        (**self).parent(id)
    }
    #[inline]
    fn children(&self, id: NodeId) -> Range<u32> {
        (**self).children(id)
    }
    #[inline]
    fn depth(&self, id: NodeId) -> u16 {
        (**self).depth(id)
    }
    #[inline]
    fn weight(&self, id: NodeId) -> u64 {
        (**self).weight(id)
    }
    #[inline]
    fn is_group(&self, id: NodeId) -> bool {
        (**self).is_group(id)
    }
}

/// Byte and count formatting. Lives here because every CLI and the app print the same numbers and
/// they should read identically.
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
