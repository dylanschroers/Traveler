//! Geometric invariants of the city layout, checked against a real scanned tree.
//!
//! These are the properties a treemap has to satisfy to be a treemap at all: children stay inside
//! their parent, siblings never overlap, and the packing conserves area. A layout that violates
//! any of them will look plausible in a screenshot and be wrong.

use std::fs;
use std::path::PathBuf;

use traveler_layout::{CityLayout, CityParams, HeightMode, Layout, LayoutCtx, Placements, svg};
use traveler_scan::{FileIndex, NodeId, ScanOptions, Tree, scan};

/// Nested tree with a heavy-tailed size distribution and a scattering of zero-byte files, which
/// is where naive treemap code divides by zero.
struct Fixture(PathBuf);

impl Fixture {
    fn new(tag: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("traveler-layout-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);

        let exts = ["rs", "txt", "png", "bin", "json"];
        let mut h: u64 = 0x243f_6a88_85a3_08d3;
        let mut next = move || {
            h ^= h << 13;
            h ^= h >> 7;
            h ^= h << 17;
            h
        };

        for a in 0..6 {
            for b in 0..3 {
                let dir = root.join(format!("dir_{a}")).join(format!("sub_{b}"));
                fs::create_dir_all(&dir).unwrap();
                for f in 0..20 {
                    let r = next();
                    // Mostly small, occasionally four orders of magnitude bigger, sometimes empty.
                    let size = match r % 16 {
                        0 => 0,
                        1 => 60_000 + (r % 40_000) as usize,
                        _ => (r % 1500) as usize,
                    };
                    let ext = exts[(r >> 8) as usize % exts.len()];
                    fs::write(dir.join(format!("file_{f}.{ext}")), vec![0u8; size]).unwrap();
                }
            }
            // An empty directory at every level, which must still get a footprint.
            fs::create_dir_all(root.join(format!("dir_{a}")).join("empty")).unwrap();
        }
        Fixture(root)
    }

    fn scan(&self) -> FileIndex {
        scan(&self.0, &ScanOptions::default()).unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn lay_out(index: &FileIndex, params: CityParams) -> Placements {
    let mut p = Placements::with_capacity(index.len());
    CityLayout::new(params).place(&LayoutCtx::new(index), &mut p);
    p
}

/// No margins or gaps, so the packing has to account for every square unit exactly.
fn raw_params() -> CityParams {
    CityParams {
        margin_ratio: 0.0,
        gap_ratio: 0.0,
        min_side: 0.0,
        ..Default::default()
    }
}

#[test]
fn every_child_stays_inside_its_parent() {
    let fx = Fixture::new("contain");
    let index = fx.scan();
    for params in [CityParams::default(), raw_params()] {
        let p = lay_out(&index, params);
        for id in index.nodes() {
            if !p.is_visible(id) {
                continue;
            }
            let (x0, z0, x1, z1) = p.footprint(id);
            for c in index.child_ids(id) {
                if !p.is_visible(c) {
                    continue;
                }
                let (cx0, cz0, cx1, cz1) = p.footprint(c);
                let eps = 1e-3;
                assert!(
                    cx0 >= x0 - eps && cz0 >= z0 - eps && cx1 <= x1 + eps && cz1 <= z1 + eps,
                    "child {c:?} ({cx0},{cz0})-({cx1},{cz1}) escapes parent {id:?} ({x0},{z0})-({x1},{z1})"
                );
            }
        }
    }
}

#[test]
fn siblings_never_overlap() {
    let fx = Fixture::new("overlap");
    let index = fx.scan();
    for params in [CityParams::default(), raw_params()] {
        let p = lay_out(&index, params);
        for id in index.nodes() {
            let kids: Vec<NodeId> = index.child_ids(id).filter(|c| p.is_visible(*c)).collect();
            for i in 0..kids.len() {
                let (ax0, az0, ax1, az1) = p.footprint(kids[i]);
                for j in (i + 1)..kids.len() {
                    let (bx0, bz0, bx1, bz1) = p.footprint(kids[j]);
                    // Touching edges are fine, genuine area overlap is not.
                    let eps = 1e-3;
                    let overlap_x = (ax1.min(bx1) - ax0.max(bx0)) > eps;
                    let overlap_z = (az1.min(bz1) - az0.max(bz0)) > eps;
                    assert!(
                        !(overlap_x && overlap_z),
                        "{:?} and {:?} overlap under parent {id:?}",
                        kids[i],
                        kids[j]
                    );
                }
            }
        }
    }
}

#[test]
fn packing_conserves_area() {
    let fx = Fixture::new("area");
    let index = fx.scan();
    let p = lay_out(&index, raw_params());

    for id in index.nodes() {
        if !p.is_visible(id) || index.child_count(id) == 0 {
            continue;
        }
        let parent = p.size[id.index()];
        let parent_area = (parent.x as f64) * (parent.z as f64);
        if parent_area < 1e-4 {
            continue;
        }
        let child_area: f64 = index
            .child_ids(id)
            .filter(|c| p.is_visible(*c))
            .map(|c| {
                let s = p.size[c.index()];
                (s.x as f64) * (s.z as f64)
            })
            .sum();
        let err = (child_area / parent_area - 1.0).abs();
        assert!(
            err < 2e-3,
            "children of {id:?} cover {child_area} of {parent_area}, off by {:.3}%",
            err * 100.0
        );
    }
}

#[test]
fn areas_are_proportional_to_bytes_when_nothing_is_floored() {
    let fx = Fixture::new("proportional");
    let index = fx.scan();
    // With no minimum leaf weight, weight is exactly bytes, so area must track bytes exactly.
    let p = lay_out(
        &index,
        CityParams {
            min_leaf_weight: 0.0,
            ..raw_params()
        },
    );

    let kids: Vec<NodeId> = index
        .child_ids(index.root())
        .filter(|c| p.is_visible(*c) && index.size(*c) > 100_000)
        .collect();
    assert!(kids.len() >= 2, "fixture needs a few substantial top dirs");

    let area = |id: NodeId| {
        let s = p.size[id.index()];
        (s.x as f64) * (s.z as f64)
    };
    for w in kids.windows(2) {
        let size_ratio = index.size(w[0]) as f64 / index.size(w[1]) as f64;
        let area_ratio = area(w[0]) / area(w[1]);
        assert!(
            (area_ratio / size_ratio - 1.0).abs() < 0.01,
            "area ratio {area_ratio:.4} does not track byte ratio {size_ratio:.4}"
        );
    }
}

#[test]
fn areas_track_the_floored_weight_by_default() {
    let fx = Fixture::new("weighted");
    let index = fx.scan();
    let params = raw_params();
    let p = lay_out(&index, params);
    // The default floors every leaf so empty files stay visible, which means area is proportional
    // to that floored weight rather than to raw bytes. Same invariant, different quantity.
    let weights = traveler_layout::layout_weights(&index, params.min_leaf_weight);

    let kids: Vec<NodeId> = index
        .child_ids(index.root())
        .filter(|c| p.is_visible(*c))
        .collect();
    assert!(kids.len() >= 2);

    let area = |id: NodeId| {
        let s = p.size[id.index()];
        (s.x as f64) * (s.z as f64)
    };
    for w in kids.windows(2) {
        let weight_ratio = weights[w[0].index()] / weights[w[1].index()];
        let area_ratio = area(w[0]) / area(w[1]);
        assert!(
            (area_ratio / weight_ratio - 1.0).abs() < 0.01,
            "area ratio {area_ratio:.4} does not track weight ratio {weight_ratio:.4}"
        );
    }
}

#[test]
fn zero_byte_files_still_get_a_footprint() {
    let fx = Fixture::new("zero");
    let index = fx.scan();
    let p = lay_out(&index, CityParams::default());

    let mut checked = 0;
    for id in index.nodes() {
        if index.flags(id).is_dir() || index.size(id) != 0 {
            continue;
        }
        checked += 1;
        assert!(p.is_visible(id), "zero-byte file {id:?} was dropped");
        let s = p.size[id.index()];
        assert!(s.x > 0.0 && s.z > 0.0, "zero-byte file got an empty rect");
    }
    assert!(checked > 0, "fixture produced no zero-byte files");

    // Empty directories too.
    let empties: Vec<NodeId> = index
        .nodes()
        .filter(|id| index.name(*id) == "empty")
        .collect();
    assert!(!empties.is_empty());
    for e in empties {
        assert!(p.is_visible(e), "empty directory {e:?} was dropped");
    }
}

#[test]
fn children_sit_on_top_of_their_parent() {
    let fx = Fixture::new("terrace");
    let index = fx.scan();
    let params = CityParams::default();
    let p = lay_out(&index, params);

    let base = |id: NodeId| p.center[id.index()].y - p.size[id.index()].y * 0.5;
    for id in index.nodes() {
        if !p.is_visible(id) {
            continue;
        }
        let top = base(id) + params.level_step;
        for c in index.child_ids(id) {
            if !p.is_visible(c) {
                continue;
            }
            assert!(
                (base(c) - top).abs() < 1e-3,
                "child {c:?} starts at {} but parent {id:?} ends at {top}",
                base(c)
            );
        }
    }
}

#[test]
fn tiles_are_squarish_not_slivers() {
    let fx = Fixture::new("aspect");
    let index = fx.scan();
    let p = lay_out(&index, CityParams::default());
    let a = svg::aspect_stats(&index, &p);

    assert!(a.count > 300, "not enough tiles to judge: {}", a.count);
    assert!(
        a.median < 3.0,
        "median aspect ratio {:.2} is too far from square",
        a.median
    );
    assert!(
        a.p90 < 8.0,
        "p90 aspect ratio {:.2} means lots of slivers",
        a.p90
    );
}

#[test]
fn layout_is_deterministic() {
    let fx = Fixture::new("determinism");
    let index = fx.scan();
    let a = lay_out(&index, CityParams::default());
    let b = lay_out(&index, CityParams::default());

    assert_eq!(a.placed, b.placed);
    assert_eq!(a.center, b.center);
    assert_eq!(a.size, b.size);
    assert_eq!(a.visible, b.visible);
}

#[test]
fn uniform_height_flattens_only_the_files() {
    let fx = Fixture::new("uniform");
    let index = fx.scan();
    let params = CityParams {
        height: HeightMode::Uniform,
        ..Default::default()
    };
    let p = lay_out(&index, params);

    for id in index.nodes() {
        if !p.is_visible(id) || index.flags(id).is_dir() {
            continue;
        }
        assert!((p.size[id.index()].y - params.file_thickness).abs() < 1e-4);
    }

    // Footprints must not change when only the height channel changes.
    let log = lay_out(&index, CityParams::default());
    for id in index.nodes() {
        assert_eq!(p.size[id.index()].x, log.size[id.index()].x);
        assert_eq!(p.size[id.index()].z, log.size[id.index()].z);
    }
}

#[test]
fn the_city_fits_the_requested_footprint() {
    let fx = Fixture::new("bounds");
    let index = fx.scan();
    let params = CityParams {
        world_size: 500.0,
        ..Default::default()
    };
    let p = lay_out(&index, params);

    assert!((p.min.x + 250.0).abs() < 1e-3, "min x was {}", p.min.x);
    assert!((p.max.x - 250.0).abs() < 1e-3, "max x was {}", p.max.x);
    assert!((p.min.z + 250.0).abs() < 1e-3);
    assert!((p.max.z - 250.0).abs() < 1e-3);
    assert!(p.min.y >= 0.0, "nothing should sit below the ground");
}

/// The log height scale must be anchored to the largest *file*. Directories carry subtree totals,
/// so including them silently makes the denominator the root's total bytes and nothing ever
/// reaches full height.
#[test]
fn log_height_spans_the_full_range() {
    let fx = Fixture::new("height");
    let index = fx.scan();
    let params = CityParams::default();
    let p = lay_out(&index, params);

    let biggest = index
        .nodes()
        .filter(|id| !index.flags(*id).is_dir())
        .max_by_key(|id| index.size(*id))
        .expect("a file");
    assert!(
        index.size(biggest) < index.size(index.root()),
        "fixture must have a root total larger than its biggest file, or this proves nothing"
    );
    let t = p.size[biggest.index()].y;
    assert!(
        (t - params.max_file_thickness).abs() < 0.01,
        "largest file is {t} tall, expected {}",
        params.max_file_thickness
    );

    let empty = index
        .nodes()
        .find(|id| !index.flags(*id).is_dir() && index.size(*id) == 0)
        .expect("a zero-byte file");
    assert!((p.size[empty.index()].y - params.file_thickness).abs() < 0.01);
}
