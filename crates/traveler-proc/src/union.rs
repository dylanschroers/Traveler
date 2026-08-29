//! One frame over several captures.
//!
//! Ids belong to the frame, not to the capture, so a diff is not a comparison of two numberings —
//! it is two captures projected onto one. The frame is built over the union of every capture's
//! extent **boundaries**, not over the union of their extents, and that distinction does the work:
//! a mapping that grew from `[a,b)` to `[a,c)` is not one extent that changed size, it is two,
//! `[a,b)` present in both and `[b,c)` present only in the later one. Growth and shrinkage stop
//! needing an encoding of their own and become an unchanged block with a sliver welded to its
//! edge, at the address the memory actually appeared at.
//!
//! It is also what keeps area additive across a diff: every frame extent is wholly present or
//! wholly absent in each capture, so there is no partial node to account for.

use std::sync::Arc;

use traveler_core::{NodeId, Tree};

use crate::space::{Frame, MappingFacts, OwnedMapping, Prot, SubExtent, fill_sections};

/// One frame, and every capture projected onto it.
pub struct Union {
    pub frame: Arc<Frame>,
    /// Per capture, parallel to the frame's ids. `false` where the frame holds an extent that
    /// capture did not: added and removed are exactly this bit differing.
    pub present: Vec<Vec<bool>>,
    pub prot: Vec<Vec<Prot>>,
}

impl Union {
    /// `captures` are in order, oldest first. Later captures win where two disagree about a
    /// label, since the most recent answer is the one a reader is looking at.
    pub fn build(
        root_label: &str,
        captures: &[Vec<OwnedMapping>],
        sections: &dyn Fn(&[MappingFacts<'_>]) -> Vec<Vec<SubExtent>>,
    ) -> Union {
        let cuts = boundaries(captures);
        let pieces = pieces(captures, &cuts);

        let facts: Vec<MappingFacts<'_>> = pieces
            .iter()
            .map(|p| {
                let source = &captures[p.source][p.index];
                MappingFacts {
                    addr: p.addr,
                    len: p.len,
                    prot: source.prot,
                    path: &source.path,
                    // Kept meaningful for the piece rather than for the mapping it came from, so
                    // a split extent still names the right place in its file.
                    offset: source.offset + (p.addr - source.addr),
                    dev: source.dev,
                    inode: source.inode,
                    group: source.group,
                }
            })
            .collect();

        let filled: Vec<Vec<SubExtent>> = {
            let supplied = sections(&facts);
            facts
                .iter()
                .enumerate()
                .map(|(i, m)| fill_sections(m, supplied.get(i).map(Vec::as_slice).unwrap_or(&[])))
                .collect()
        };
        let frame = Frame::build(root_label, &facts, &filled);
        let n = frame.len();
        let first = frame.mappings().start as usize;

        let mut present = vec![vec![false; n]; captures.len()];
        let mut prot = vec![vec![Prot::default(); n]; captures.len()];
        for (c, capture) in captures.iter().enumerate() {
            for (i, piece) in pieces.iter().enumerate() {
                let Some(source) = covering(capture, piece.addr) else {
                    continue;
                };
                let id = NodeId((first + i) as u32);
                present[c][id.index()] = true;
                prot[c][id.index()] = capture[source].prot;
                for (child, sub) in frame.child_ids(id).zip(&filled[i]) {
                    present[c][child.index()] = true;
                    prot[c][child.index()] = sub.prot;
                }
            }
            // A container is present in a capture when anything under it is, and carries the
            // union of what it holds, exactly as a single capture does.
            for id in (1..n).rev() {
                if present[c][id] {
                    let parent = frame.parent(NodeId(id as u32)).index();
                    present[c][parent] = true;
                    prot[c][parent].0 |= prot[c][id].0;
                }
            }
        }

        Union {
            frame: Arc::new(frame),
            present,
            prot,
        }
    }

    /// How a node changed between two captures. Grown and shrunk are absent on purpose: boundary
    /// splitting has already turned them into an added or removed sliver beside an unchanged
    /// block, which is what they actually are.
    pub fn change(&self, before: usize, after: usize, id: NodeId) -> Change {
        match (
            self.present[before][id.index()],
            self.present[after][id.index()],
        ) {
            (false, true) => Change::Added,
            (true, false) => Change::Removed,
            (true, true) => Change::Unchanged,
            (false, false) => Change::Absent,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Change {
    Added,
    Unchanged,
    Removed,
    /// In the frame because some *other* capture had it, and in neither of these two.
    Absent,
}

/// One extent of the union: an interval no capture's boundary falls inside.
struct Piece {
    addr: u64,
    len: u64,
    /// The newest capture that covers it, for its label and backing.
    source: usize,
    index: usize,
}

fn boundaries(captures: &[Vec<OwnedMapping>]) -> Vec<u64> {
    let mut cuts: Vec<u64> = captures
        .iter()
        .flat_map(|c| c.iter().flat_map(|m| [m.addr, m.addr + m.len]))
        .collect();
    cuts.sort_unstable();
    cuts.dedup();
    cuts
}

fn pieces(captures: &[Vec<OwnedMapping>], cuts: &[u64]) -> Vec<Piece> {
    let mut out = Vec::with_capacity(cuts.len());
    for pair in cuts.windows(2) {
        let (addr, end) = (pair[0], pair[1]);
        // The newest capture that covers it, so labels come from the most recent answer.
        let found = captures
            .iter()
            .enumerate()
            .rev()
            .find_map(|(c, capture)| covering(capture, addr).map(|i| (c, i)));
        if let Some((source, index)) = found {
            out.push(Piece {
                addr,
                len: end - addr,
                source,
                index,
            });
        }
    }
    out
}

/// Index of the mapping containing `addr`, by binary search over an address-ordered capture.
fn covering(capture: &[OwnedMapping], addr: u64) -> Option<usize> {
    let i = match capture.binary_search_by_key(&addr, |m| m.addr) {
        Ok(i) => i,
        Err(0) => return None,
        Err(i) => i - 1,
    };
    (addr < capture[i].addr + capture[i].len).then_some(i)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(addr: u64, len: u64, path: &str) -> OwnedMapping {
        OwnedMapping {
            addr,
            len,
            prot: Prot(Prot::READ | Prot::WRITE),
            path: path.to_string(),
            offset: 0,
            dev: 0,
            inode: 0,
            group: 0,
        }
    }

    fn union(captures: Vec<Vec<OwnedMapping>>) -> Union {
        Union::build("t", &captures, &|_| Vec::new())
    }

    fn mapping_at(u: &Union, addr: u64) -> NodeId {
        u.frame
            .mapping_at(addr)
            .unwrap_or_else(|| panic!("no extent covers {addr:#x}"))
    }

    /// A mapping that grew is two extents, not one that changed length. That is the whole reason
    /// the frame is built over boundaries.
    #[test]
    fn growth_becomes_an_unchanged_block_and_an_added_sliver() {
        let u = union(vec![
            vec![m(0x1000, 0x2000, "")],
            vec![m(0x1000, 0x5000, "")],
        ]);

        let old = mapping_at(&u, 0x1000);
        let grown = mapping_at(&u, 0x3000);
        assert_ne!(old, grown);
        assert_eq!(u.frame.weight(old), 0x2000);
        assert_eq!(u.frame.weight(grown), 0x3000);
        assert_eq!(u.change(0, 1, old), Change::Unchanged);
        assert_eq!(u.change(0, 1, grown), Change::Added);
        // Adjacent, so the sliver is drawn welded to the block it grew out of.
        assert_eq!(u.frame.end(old), u.frame.addr(grown));
    }

    #[test]
    fn shrinkage_is_the_same_thing_the_other_way_round() {
        let u = union(vec![
            vec![m(0x1000, 0x5000, "")],
            vec![m(0x1000, 0x2000, "")],
        ]);
        assert_eq!(u.change(0, 1, mapping_at(&u, 0x1000)), Change::Unchanged);
        assert_eq!(u.change(0, 1, mapping_at(&u, 0x3000)), Change::Removed);
    }

    #[test]
    fn a_whole_mapping_arriving_and_leaving_is_added_and_removed() {
        let u = union(vec![
            vec![m(0x1000, 0x1000, "a")],
            vec![m(0x1000, 0x1000, "a"), m(0x9000, 0x1000, "b")],
            vec![m(0x9000, 0x1000, "b")],
        ]);
        let a = mapping_at(&u, 0x1000);
        let b = mapping_at(&u, 0x9000);
        assert_eq!(u.change(0, 1, b), Change::Added);
        assert_eq!(u.change(1, 2, a), Change::Removed);
        assert_eq!(u.change(0, 2, a), Change::Removed);
        assert_eq!(u.change(0, 2, b), Change::Added);
    }

    /// §15's completeness property: the union carries every boundary of every capture, and each
    /// capture projects onto it with none of its bytes lost and no extent straddling a boundary
    /// another capture has.
    #[test]
    fn the_union_is_complete_and_the_projection_is_lossless() {
        let captures = vec![
            vec![m(0x1000, 0x3000, "a"), m(0x8000, 0x2000, "b")],
            vec![m(0x2000, 0x1000, "a"), m(0x8000, 0x9000, "b")],
            vec![m(0x1000, 0x9000, "c")],
        ];
        let u = union(captures.clone());

        for (c, capture) in captures.iter().enumerate() {
            let covered: u64 = u
                .frame
                .mappings()
                .map(NodeId)
                .filter(|id| u.present[c][id.index()])
                .map(|id| u.frame.weight(id))
                .sum();
            let want: u64 = capture.iter().map(|m| m.len).sum();
            assert_eq!(covered, want, "capture {c} lost bytes in the projection");

            // Every frame extent a capture has is wholly inside one of that capture's mappings.
            for id in u.frame.mappings().map(NodeId) {
                if !u.present[c][id.index()] {
                    continue;
                }
                let (start, end) = (u.frame.addr(id), u.frame.end(id));
                assert!(
                    capture
                        .iter()
                        .any(|m| m.addr <= start && end <= m.addr + m.len),
                    "extent {start:#x}..{end:#x} straddles a boundary in capture {c}"
                );
            }
        }
    }

    /// Area has to stay additive across a diff, or a region reports a size its children do not
    /// add to and the picture lies.
    #[test]
    fn weights_still_sum_over_the_union() {
        let u = union(vec![
            vec![m(0x1000, 0x3000, "a")],
            vec![m(0x2000, 0x6000, "a")],
            vec![m(0x1000, 0x1000, "a"), m(0x7000, 0x1000, "b")],
        ]);
        for id in u.frame.nodes() {
            if u.frame.child_count(id) == 0 {
                continue;
            }
            let sum: u64 = u.frame.child_ids(id).map(|c| u.frame.weight(c)).sum();
            assert_eq!(sum, u.frame.weight(id), "children of {} do not sum", id.0);
        }
    }

    #[test]
    fn a_node_absent_from_both_captures_is_neither_added_nor_removed() {
        let u = union(vec![
            vec![m(0x1000, 0x1000, "a")],
            vec![m(0x4000, 0x1000, "c")],
            vec![m(0x1000, 0x1000, "a")],
        ]);
        let only_in_the_middle = mapping_at(&u, 0x4000);
        assert_eq!(u.change(0, 2, only_in_the_middle), Change::Absent);
    }

    /// A frame over one capture must be the same frame that capture builds on its own, or a
    /// single-capture view and the first frame of a sequence would disagree.
    #[test]
    fn one_capture_frames_exactly_as_it_would_alone() {
        let one = vec![m(0x1000, 0x2000, "a"), m(0x8000, 0x1000, "b")];
        let u = union(vec![one.clone()]);
        assert_eq!(u.frame.mappings().len(), 2);
        assert!(u.present[0].iter().all(|p| *p));
        assert_eq!(u.frame.weight(u.frame.root()), 0x3000);
    }
}
