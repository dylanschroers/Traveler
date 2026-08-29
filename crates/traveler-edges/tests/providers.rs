//! Fixture with hand-placed relationships, so every group below is one somebody wrote on purpose.

use std::{collections::HashMap, fs, path::PathBuf};

use traveler_edges::{EdgeProvider, ExactDuplicates, ExtensionGroups, Group, NameFamilies};
use traveler_scan::{FileIndex, ScanOptions, Tree, scan};

struct Fixture(PathBuf);

impl Fixture {
    fn new(tag: &str) -> Self {
        let root = std::env::temp_dir().join(format!("traveler-edges-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let src = root.join("src");
        let shots = root.join("shots");
        let other = root.join("other");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&shots).unwrap();
        fs::create_dir_all(&other).unwrap();

        // A name family split across extensions, plus an unrelated neighbour.
        fs::write(src.join("parser.c"), b"int parse(void);").unwrap();
        fs::write(src.join("parser.h"), b"/* header */").unwrap();
        fs::write(src.join("main.c"), b"int main(void){}").unwrap();

        // A numbered sequence.
        for i in 1..=4 {
            fs::write(shots.join(format!("IMG_{i:04}.png")), vec![7u8; 64 + i]).unwrap();
        }

        // Three byte-identical files in two different folders, and a decoy of the same length.
        let payload = vec![42u8; 4096];
        fs::write(root.join("copy_a.bin"), &payload).unwrap();
        fs::write(other.join("copy_b.bin"), &payload).unwrap();
        fs::write(other.join("copy_c.bin"), &payload).unwrap();
        let mut decoy = payload.clone();
        decoy[0] = 1;
        fs::write(other.join("not_a_copy.bin"), &decoy).unwrap();

        // Two files that agree for far longer than the screening prefix and then diverge. This is
        // the case a prefix-only duplicate finder gets wrong.
        let mut long_a = vec![9u8; 200 * 1024];
        let mut long_b = long_a.clone();
        long_a[199 * 1024] = 1;
        long_b[199 * 1024] = 2;
        fs::write(root.join("long_a.dat"), &long_a).unwrap();
        fs::write(root.join("long_b.dat"), &long_b).unwrap();

        // Empty files must never be reported as duplicates of each other.
        fs::write(root.join("empty_one.txt"), b"").unwrap();
        fs::write(root.join("empty_two.txt"), b"").unwrap();

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

fn names(index: &FileIndex, g: &Group) -> Vec<String> {
    let mut v: Vec<String> = g.members.iter().map(|m| index.name(*m).to_string()).collect();
    v.sort();
    v
}

#[test]
fn extensions_group_every_file_of_a_type() {
    let fx = Fixture::new("ext");
    let index = fx.scan();
    let set = ExtensionGroups.compute(&index);

    let bin = set
        .groups
        .iter()
        .find(|g| g.label == ".bin")
        .expect(".bin group");
    assert_eq!(bin.members.len(), 4, "three copies and the decoy");

    let png = set.groups.iter().find(|g| g.label == ".png").expect(".png");
    assert_eq!(png.members.len(), 4);

    // A type with a single file is not a group.
    assert!(set.groups.iter().all(|g| g.members.len() >= 2));
    // Groups arrive largest first.
    for w in set.groups.windows(2) {
        assert!(w[0].bytes >= w[1].bytes);
    }
}

#[test]
fn name_families_find_shared_stems_within_one_folder() {
    let fx = Fixture::new("family");
    let index = fx.scan();
    let set = NameFamilies.compute(&index);

    let parser = set
        .groups
        .iter()
        .find(|g| g.label.starts_with("parser*"))
        .expect("parser family");
    assert_eq!(names(&index, parser), vec!["parser.c", "parser.h"]);

    let shots = set
        .groups
        .iter()
        .find(|g| g.label.starts_with("IMG*"))
        .expect("IMG family");
    assert_eq!(shots.members.len(), 4);

    // main.c shares a folder with the parser pair but not a stem, so it is in no family.
    let main_id = index
        .nodes()
        .find(|id| index.name(*id) == "main.c")
        .expect("main.c");
    assert!(
        !set.groups.iter().any(|g| g.members.contains(&main_id)),
        "main.c must not be pulled into a family"
    );

    // The identical copies live in different folders, so they are not one family.
    assert!(
        !set.groups.iter().any(|g| g.label.starts_with("copy*")
            && g.members.len() > 2),
        "families never span directories"
    );
}

#[test]
fn duplicates_are_exact_and_ignore_empty_files() {
    let fx = Fixture::new("dupes");
    let index = fx.scan();
    let set = ExactDuplicates::default().compute(&index);

    let copies = set
        .groups
        .iter()
        .find(|g| g.members.len() == 3)
        .expect("the three identical copies");
    assert_eq!(
        names(&index, copies),
        vec!["copy_a.bin", "copy_b.bin", "copy_c.bin"]
    );
    assert_eq!(copies.bytes, 3 * 4096);
    assert_eq!(copies.redundant_bytes, 2 * 4096, "keeping one frees the rest");

    // Same length, one byte different: not a duplicate.
    for g in &set.groups {
        let n = names(&index, g);
        assert!(!n.contains(&"not_a_copy.bin".to_string()), "decoy was grouped");
        assert!(!n.contains(&"empty_one.txt".to_string()), "empty file was grouped");
    }
}

#[test]
fn files_that_diverge_after_the_prefix_are_not_duplicates() {
    let fx = Fixture::new("prefix");
    let index = fx.scan();
    // Screen on far less than the point where the two files differ, so the prefix pass must
    // report them as identical and only the full hash can tell them apart.
    let set = ExactDuplicates { prefix_bytes: 1024, ..Default::default() }.compute(&index);

    for g in &set.groups {
        let n = names(&index, g);
        assert!(
            !n.contains(&"long_a.dat".to_string()),
            "prefix collision reported as a duplicate: {}",
            g.label
        );
    }
    assert!(set.stats.files_hashed > 0);
    assert_eq!(set.stats.read_errors, 0);
}

#[test]
fn a_file_smaller_than_the_prefix_needs_only_one_pass() {
    let fx = Fixture::new("small");
    let index = fx.scan();
    let set = ExactDuplicates {
        prefix_bytes: 1024 * 1024,
        // No floor, so this measures the one-pass property and nothing else.
        min_size: 0,
        ..Default::default()
    }
    .compute(&index);

    let copies = set
        .groups
        .iter()
        .find(|g| g.members.len() == 3)
        .expect("copies");
    assert_eq!(copies.members.len(), 3);
    // With the prefix larger than every file, one pass should read exactly the candidate bytes
    // and nothing twice. Derived from the index rather than hand-counted, because "which files
    // happen to share a length" is not obvious by eye: two of the sources here are both 16 bytes.
    let mut counts: HashMap<u64, u64> = HashMap::new();
    for id in index.nodes() {
        if !index.flags(id).is_dir() && index.size(id) > 0 {
            *counts.entry(index.size(id)).or_default() += 1;
        }
    }
    let expected: u64 = counts
        .iter()
        .filter(|(_, n)| **n >= 2)
        .map(|(size, n)| size * n)
        .sum();
    assert_eq!(
        set.stats.bytes_hashed, expected,
        "should read every candidate exactly once"
    );
}

#[test]
fn the_size_floor_skips_small_candidates_and_says_so() {
    let fx = Fixture::new("floor");
    let index = fx.scan();
    let set = ExactDuplicates {
        // Above the 4 KiB copies, so nothing at all should be read.
        min_size: 1024 * 1024,
        ..Default::default()
    }
    .compute(&index);

    assert!(set.groups.is_empty(), "everything is under the floor");
    assert_eq!(set.stats.bytes_hashed, 0, "the floor should prevent all I/O");
    assert!(
        set.stats.skipped_small >= 5,
        "the skipped count should report what the floor cost: got {}",
        set.stats.skipped_small
    );
}

/// A live disk changes under you. If a file is no longer the length the scan recorded, hashing it
/// would compare a different number of bytes than its sibling, so it must be dropped rather than
/// grouped on a stale size.
#[test]
fn files_that_change_after_the_scan_are_dropped() {
    let fx = Fixture::new("changed");
    let index = fx.scan();

    // Shrink one copy and grow another, behind the index's back.
    fs::write(fx.0.join("copy_a.bin"), vec![42u8; 2048]).unwrap();
    fs::write(fx.0.join("other").join("copy_b.bin"), vec![42u8; 8192]).unwrap();

    let set = ExactDuplicates::default().compute(&index);

    for g in &set.groups {
        let n = names(&index, g);
        assert!(
            !n.contains(&"copy_a.bin".to_string()),
            "a file that shrank was still grouped"
        );
        assert!(
            !n.contains(&"copy_b.bin".to_string()),
            "a file that grew was still grouped"
        );
    }
    assert_eq!(
        set.stats.changed_while_reading, 2,
        "both changed files should be reported"
    );
}
