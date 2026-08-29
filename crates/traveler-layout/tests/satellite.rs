//! The satellite exists to answer one question: did these files come from one folder or many?
//! These tests check that the arrangement actually carries that answer.

use std::fs;
use std::path::PathBuf;

use traveler_layout::satellite::{SatelliteParams, lay_out};
use traveler_layout::{CityLayout, CityParams, Layout, LayoutCtx, Placements};
use traveler_scan::{FileIndex, NodeId, ScanOptions, Tree, scan};

struct Fixture(PathBuf);

impl Fixture {
    /// `one/` holds twelve files, `spread_a`..`spread_c` hold one each. That gives a dominant
    /// clump plus three lonely ones, which is the shape the view has to distinguish.
    fn new(tag: &str) -> Self {
        let root =
            std::env::temp_dir().join(format!("traveler-satellite-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("one")).unwrap();
        for i in 0..12 {
            fs::write(root.join("one").join(format!("f{i}.bin")), vec![0u8; 100 + i]).unwrap();
        }
        for d in ["spread_a", "spread_b", "spread_c"] {
            fs::create_dir_all(root.join(d)).unwrap();
            fs::write(root.join(d).join("lonely.bin"), vec![0u8; 50]).unwrap();
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

/// The satellite keeps the city's bearings, so every test needs a real city underneath it.
fn city_of(index: &FileIndex) -> Placements {
    let mut p = Placements::with_capacity(index.len());
    CityLayout::new(CityParams::default()).place(&LayoutCtx::new(index), &mut p);
    p
}

fn all_bin_files(index: &FileIndex) -> Vec<NodeId> {
    index
        .nodes()
        .filter(|id| !index.flags(*id).is_dir() && index.ext_str(index.ext(*id)) == "bin")
        .collect()
}

#[test]
fn members_are_bucketed_by_folder_with_the_biggest_first() {
    let fx = Fixture::new("buckets");
    let index = fx.scan();
    let members = all_bin_files(&index);
    let sat = lay_out(&index, &members, &city_of(&index), &SatelliteParams::default());

    assert_eq!(sat.members.len(), 15, "twelve plus three");
    assert_eq!(sat.buckets.len(), 4, "one crowded folder and three lonely ones");
    assert_eq!(sat.buckets[0].count, 12, "the biggest bucket comes first");
    assert!(sat.omitted == 0);
}

#[test]
fn files_from_one_folder_land_closer_together_than_files_from_different_folders() {
    let fx = Fixture::new("cohesion");
    let index = fx.scan();
    let members = all_bin_files(&index);
    let sat = lay_out(&index, &members, &city_of(&index), &SatelliteParams::default());

    let mut same = Vec::new();
    let mut different = Vec::new();
    for i in 0..sat.members.len() {
        for j in (i + 1)..sat.members.len() {
            let d = sat.offsets[i].distance(sat.offsets[j]);
            if index.parent(sat.members[i]) == index.parent(sat.members[j]) {
                same.push(d);
            } else {
                different.push(d);
            }
        }
    }
    assert!(!same.is_empty() && !different.is_empty());

    let mean = |v: &[f32]| v.iter().sum::<f32>() / v.len() as f32;
    // This is the whole point of the view. If it ever stops holding, the arrangement is noise.
    // The margin is a regression guard rather than a target: this fixture measures about 1.9x,
    // and anything under 1.4x would mean the clumping had stopped doing real work.
    assert!(
        mean(&same) < mean(&different) * 0.7,
        "same-folder mean {:.1} vs cross-folder mean {:.1}: clumping is not separating them",
        mean(&same),
        mean(&different)
    );
}

#[test]
fn blocks_never_overlap() {
    let fx = Fixture::new("overlap");
    let index = fx.scan();
    let members = all_bin_files(&index);
    let params = SatelliteParams::default();
    let sat = lay_out(&index, &members, &city_of(&index), &params);

    for i in 0..sat.offsets.len() {
        for j in (i + 1)..sat.offsets.len() {
            let d = sat.offsets[i].distance(sat.offsets[j]);
            assert!(
                d >= params.block,
                "blocks {i} and {j} are {d:.2} apart, closer than one block edge"
            );
        }
    }
}

#[test]
fn everything_stays_inside_the_reported_radius() {
    let fx = Fixture::new("radius");
    let index = fx.scan();
    let members = all_bin_files(&index);
    let sat = lay_out(&index, &members, &city_of(&index), &SatelliteParams::default());

    for (i, o) in sat.offsets.iter().enumerate() {
        assert!(o.is_finite(), "offset {i} is not finite: {o:?}");
        assert!(
            o.length() <= sat.radius,
            "offset {i} at {:.1} escapes the reported radius {:.1}",
            o.length(),
            sat.radius
        );
    }
}

#[test]
fn the_cap_drops_whole_folders_and_reports_the_loss() {
    let fx = Fixture::new("cap");
    let index = fx.scan();
    let members = all_bin_files(&index);
    let sat = lay_out(
        &index,
        &members,
        &city_of(&index),
        &SatelliteParams {
            max_members: 13,
            ..Default::default()
        },
    );

    assert_eq!(sat.members.len() + sat.omitted, members.len());
    assert!(sat.omitted > 0, "the cap should have dropped something");
    // Whole buckets only: no folder is shown half present.
    for b in &sat.buckets {
        let placed = sat
            .members
            .iter()
            .filter(|m| index.parent(**m) == b.dir)
            .count();
        assert_eq!(placed, b.count, "a folder was shown partially");
    }
}

#[test]
fn a_single_member_sits_at_the_centre() {
    let fx = Fixture::new("single");
    let index = fx.scan();
    let one = all_bin_files(&index)[0];
    let sat = lay_out(&index, &[one], &city_of(&index), &SatelliteParams::default());

    assert_eq!(sat.members, vec![one]);
    assert_eq!(sat.offsets[0], glam::Vec3::ZERO);
}

#[test]
fn layout_is_deterministic() {
    let fx = Fixture::new("determinism");
    let index = fx.scan();
    let members = all_bin_files(&index);
    let a = lay_out(&index, &members, &city_of(&index), &SatelliteParams::default());
    // Reversed input must not change the result: the arrangement comes from the tree, not from
    // whatever order the caller happened to hold.
    let reversed: Vec<NodeId> = members.iter().rev().copied().collect();
    let b = lay_out(&index, &reversed, &city_of(&index), &SatelliteParams::default());

    assert_eq!(a.members, b.members);
    assert_eq!(a.offsets, b.offsets);
    assert_eq!(a.radius, b.radius);
}

#[test]
fn an_empty_group_produces_an_empty_cluster() {
    let fx = Fixture::new("empty");
    let index = fx.scan();
    let sat = lay_out(&index, &[], &city_of(&index), &SatelliteParams::default());
    assert!(sat.is_empty());
    assert!(sat.buckets.is_empty());
}

/// The reason the arrangement is worth anything: a folder to the east of the group gets its clump
/// to the east of the cluster. Placement may push a disc outward to stop it overlapping, but it
/// may never rotate one, because that is what makes the threads cross.
#[test]
fn each_clump_keeps_its_folders_bearing_from_the_city() {
    let fx = Fixture::new("bearing");
    let index = fx.scan();
    let city = city_of(&index);
    let members = all_bin_files(&index);
    let sat = lay_out(&index, &members, &city, &SatelliteParams::default());

    // Independently recompute where each folder sits in the city, and where the group sits.
    let folder_pos = |dir: NodeId| {
        let mut sum = glam::Vec2::ZERO;
        let mut n = 0.0f32;
        for m in sat.members.iter().filter(|m| index.parent(**m) == dir) {
            let c = city.center[m.index()];
            sum += glam::Vec2::new(c.x, c.z);
            n += 1.0;
        }
        sum / n
    };
    let anchor: glam::Vec2 = sat.buckets.iter().map(|b| folder_pos(b.dir)).sum::<glam::Vec2>()
        / sat.buckets.len() as f32;

    // Angles wrap: -pi and pi are the same bearing, so compare the shortest way round.
    let angle_delta = |a: f32, b: f32| {
        let two_pi = std::f32::consts::TAU;
        ((a - b + std::f32::consts::PI).rem_euclid(two_pi) - std::f32::consts::PI).abs()
    };

    for b in &sat.buckets {
        let want = folder_pos(b.dir) - anchor;
        if want.length() < 1e-3 {
            continue;
        }
        let expected = want.y.atan2(want.x);
        let delta = angle_delta(b.bearing, expected);
        assert!(
            delta < 1e-3,
            "clump for {:?} was placed on bearing {:.3} but its folder lies at {:.3}",
            b.dir,
            b.bearing,
            expected
        );

        // And the disc really sits on that bearing, so only the radius was ever adjusted.
        if b.center.length() > 1e-3 {
            let placed = b.center.y.atan2(b.center.x);
            assert!(
                angle_delta(placed, b.bearing) < 1e-3,
                "clump for {:?} drifted off its bearing while being placed",
                b.dir
            );
        }
    }
}
