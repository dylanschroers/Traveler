//! Geometric invariants of the strip layout, plus the one the city does not have: reading order
//! is address order. A layout that loses that is not wrong-looking, it is just no longer
//! comparable between captures, which is the property the whole diff rests on.

use std::ops::Range;

use traveler_core::{NodeId, Tree};
use traveler_layout::{Layout, LayoutCtx, Placements, SpaceLayout, SpaceParams};

/// A three-level address space: root, regions, mappings. Weights span four orders of magnitude,
/// because a real one spans twelve and the thin end is where a packer misbehaves.
struct Space {
    parent: Vec<NodeId>,
    child_start: Vec<u32>,
    child_len: Vec<u32>,
    depth: Vec<u16>,
    weight: Vec<u64>,
}

impl Tree for Space {
    fn len(&self) -> usize {
        self.parent.len()
    }
    fn root(&self) -> NodeId {
        NodeId(0)
    }
    fn parent(&self, id: NodeId) -> NodeId {
        self.parent[id.index()]
    }
    fn children(&self, id: NodeId) -> Range<u32> {
        let start = self.child_start[id.index()];
        start..start + self.child_len[id.index()]
    }
    fn depth(&self, id: NodeId) -> u16 {
        self.depth[id.index()]
    }
    fn weight(&self, id: NodeId) -> u64 {
        self.weight[id.index()]
    }
}

fn space(regions: usize, per_region: usize) -> Space {
    let mut seed = 0x2545_F491_4F6C_DD1Du64;
    let mut next = move || {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        // One page to a few thousand, in page multiples, as a real mapping is.
        (1 + seed % 4096) * 4096
    };

    let n = 1 + regions + regions * per_region;
    let first_mapping = 1 + regions;
    let mut s = Space {
        parent: vec![NodeId(0); n],
        child_start: vec![0; n],
        child_len: vec![0; n],
        depth: vec![0; n],
        weight: vec![0; n],
    };

    s.child_start[0] = 1;
    s.child_len[0] = regions as u32;
    for r in 0..regions {
        let id = 1 + r;
        s.parent[id] = NodeId(0);
        s.depth[id] = 1;
        s.child_start[id] = (first_mapping + r * per_region) as u32;
        s.child_len[id] = per_region as u32;
        for m in 0..per_region {
            let mid = first_mapping + r * per_region + m;
            s.parent[mid] = NodeId(id as u32);
            s.depth[mid] = 2;
            s.weight[mid] = next();
        }
    }
    // Weight is exactly additive: a parent's length is the sum of its children's.
    for id in (1..n).rev() {
        let w = s.weight[id];
        s.weight[s.parent[id].index()] += w;
    }
    s
}

fn lay_out(tree: &Space, params: SpaceParams) -> Placements {
    let mut p = Placements::with_capacity(tree.len());
    SpaceLayout::new(params).place(&LayoutCtx::new(tree), &mut p);
    p
}

fn raw() -> SpaceParams {
    SpaceParams {
        margin_ratio: 0.0,
        gap_ratio: 0.0,
        min_side: 0.0,
        ..Default::default()
    }
}

#[test]
fn every_child_stays_inside_its_parent() {
    let tree = space(24, 5);
    let p = lay_out(&tree, SpaceParams::default());
    for id in tree.nodes() {
        if !p.is_visible(id) {
            continue;
        }
        let (px0, pz0, px1, pz1) = p.footprint(id);
        for c in tree.child_ids(id) {
            if !p.is_visible(c) {
                continue;
            }
            let (cx0, cz0, cx1, cz1) = p.footprint(c);
            let slack = 1e-3;
            assert!(
                cx0 >= px0 - slack
                    && cz0 >= pz0 - slack
                    && cx1 <= px1 + slack
                    && cz1 <= pz1 + slack,
                "child {} escapes parent {}",
                c.0,
                id.0
            );
        }
    }
}

#[test]
fn siblings_never_overlap() {
    let tree = space(18, 7);
    let p = lay_out(&tree, SpaceParams::default());
    for id in tree.nodes() {
        let kids: Vec<NodeId> = tree.child_ids(id).filter(|c| p.is_visible(*c)).collect();
        for (i, a) in kids.iter().enumerate() {
            let (ax0, az0, ax1, az1) = p.footprint(*a);
            for b in &kids[i + 1..] {
                let (bx0, bz0, bx1, bz1) = p.footprint(*b);
                let overlap =
                    ax1.min(bx1) - ax0.max(bx0) > 1e-3 && az1.min(bz1) - az0.max(bz0) > 1e-3;
                assert!(!overlap, "{} overlaps {}", a.0, b.0);
            }
        }
    }
}

#[test]
fn packing_conserves_area_exactly() {
    let tree = space(16, 6);
    let p = lay_out(&tree, raw());
    for id in tree.nodes() {
        if !p.is_visible(id) || tree.child_count(id) == 0 {
            continue;
        }
        let (px0, pz0, px1, pz1) = p.footprint(id);
        let parent = ((px1 - px0) as f64) * ((pz1 - pz0) as f64);
        let kids: f64 = tree
            .child_ids(id)
            .filter(|c| p.is_visible(*c))
            .map(|c| {
                let s = p.size[c.index()];
                (s.x as f64) * (s.z as f64)
            })
            .sum();
        assert!(
            (1.0 - kids / parent).abs() < 1e-3,
            "children of {} cover {:.4} of it",
            id.0,
            kids / parent
        );
    }
}

/// Area is proportional to bytes with no floor and no fudge, which is what §2 buys by measuring
/// intervals: a parent's length *is* the sum of its children's.
#[test]
fn area_is_proportional_to_bytes() {
    let tree = space(12, 8);
    let p = lay_out(&tree, raw());
    let root = tree.root();
    let (rx0, rz0, rx1, rz1) = p.footprint(root);
    let root_area = ((rx1 - rx0) as f64) * ((rz1 - rz0) as f64);

    for id in tree.child_ids(root) {
        let s = p.size[id.index()];
        let share = ((s.x as f64) * (s.z as f64)) / root_area;
        let want = tree.weight(id) as f64 / tree.weight(root) as f64;
        assert!(
            (share - want).abs() < 1e-4,
            "region {} covers {share:.5} but holds {want:.5} of the bytes",
            id.0
        );
    }
}

/// The property the city cannot have and the diff depends on: the tiles are placed in address
/// order and the picture never doubles back.
///
/// The first version of this asserted strict row-major reading, which held only while strips ran
/// full width. Strips now run along the shorter side of what is left of the rect, which is what
/// took the median aspect from 6.6 to 2.0, so a strip may be a column as easily as a row. Order is
/// still total: consecutive tiles either continue the current strip or begin one wholly beyond it.
#[test]
fn placement_follows_address_order_and_never_doubles_back() {
    let tree = space(20, 9);
    let p = lay_out(&tree, SpaceParams::default());
    for id in tree.nodes() {
        let kids: Vec<NodeId> = tree.child_ids(id).filter(|c| p.is_visible(*c)).collect();
        for pair in kids.windows(2) {
            let (a, b) = (pair[0], pair[1]);
            // An inset shrinks a tile evenly, so tiles in one strip share a centre line exactly.
            let (ac, bc) = (p.center[a.index()], p.center[b.index()]);
            let (ax0, az0, ax1, az1) = p.footprint(a);
            let (bx0, bz0, _, _) = p.footprint(b);
            let eps = 1e-3;

            if (ac.x - bc.x).abs() < eps {
                assert!(bc.z > ac.z, "{} precedes {} down its column", b.0, a.0);
            } else if (ac.z - bc.z).abs() < eps {
                assert!(bc.x > ac.x, "{} precedes {} along its row", b.0, a.0);
            } else {
                assert!(
                    bx0 >= ax1 - eps || bz0 >= az1 - eps,
                    "{} starts inside the strip {} is in",
                    b.0,
                    a.0
                );
                let _ = (ax0, az0);
            }
        }
    }
}

#[test]
fn the_layout_is_deterministic() {
    let tree = space(15, 6);
    let a = lay_out(&tree, SpaceParams::default());
    let b = lay_out(&tree, SpaceParams::default());
    assert_eq!(a.placed, b.placed);
    for id in tree.nodes() {
        assert_eq!(a.center[id.index()], b.center[id.index()]);
        assert_eq!(a.size[id.index()], b.size[id.index()]);
    }
}

#[test]
fn the_space_fits_the_requested_footprint() {
    let params = SpaceParams {
        world_size: 400.0,
        ..Default::default()
    };
    let tree = space(10, 4);
    let p = lay_out(&tree, params);
    let half = (params.world_size * 0.5) as f32;
    assert!(p.min.x >= -half - 1e-3 && p.max.x <= half + 1e-3);
    assert!(p.min.z >= -half - 1e-3 && p.max.z <= half + 1e-3);
}

#[test]
fn children_sit_on_top_of_their_parent() {
    let params = SpaceParams::default();
    let tree = space(8, 5);
    let p = lay_out(&tree, params);
    let base = |id: NodeId| p.center[id.index()].y - p.size[id.index()].y * 0.5;
    for id in tree.nodes() {
        let top = base(id) + params.level_step;
        for c in tree.child_ids(id) {
            assert!((base(c) - top).abs() < 1e-3);
        }
    }
}

/// Two frames over the same extents must place them identically, since the diff renders both
/// captures into one geometry. Laying out a *subset* is a different frame and is expected to
/// differ, which is why §15 asserts completeness rather than cross-frame identity.
#[test]
fn one_frame_laid_out_twice_is_byte_identical() {
    let tree = space(22, 4);
    let a = lay_out(&tree, SpaceParams::default());
    let b = lay_out(&tree, SpaceParams::default());
    for id in tree.nodes() {
        assert_eq!(a.footprint(id), b.footprint(id));
    }
}
