//! Golden tests against a fixture tree with hand-computed totals.

use std::{fs, path::PathBuf};

use traveler_scan::{ScanOptions, Tree, scan};

struct Fixture(PathBuf);

impl Fixture {
    /// ```text
    /// root/            a.txt 100, b.TXT 50
    ///   sub/           c.bin 7
    ///     deep/        d.txt 3
    ///   empty/
    /// ```
    /// 4 files, 4 directories including the root, 160 bytes.
    fn new(tag: &str) -> Self {
        let dir =
            std::env::temp_dir().join(format!("traveler-scan-test-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("sub/deep")).unwrap();
        fs::create_dir_all(dir.join("empty")).unwrap();
        fs::write(dir.join("a.txt"), vec![0u8; 100]).unwrap();
        fs::write(dir.join("b.TXT"), vec![0u8; 50]).unwrap();
        fs::write(dir.join("sub/c.bin"), vec![0u8; 7]).unwrap();
        fs::write(dir.join("sub/deep/d.txt"), vec![0u8; 3]).unwrap();
        Fixture(dir)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[test]
fn totals_match_the_fixture() {
    let fx = Fixture::new("totals");
    let index = scan(&fx.0, &ScanOptions::default()).unwrap();

    assert_eq!(index.stats.files, 4, "file count");
    assert_eq!(index.stats.dirs, 4, "dir count, root included");
    assert_eq!(index.stats.bytes, 160, "total bytes");
    assert_eq!(index.len(), 8, "nodes in index");
    assert!(index.errors.is_empty(), "errors: {:?}", index.errors);
    assert_eq!(
        index.size(index.root()),
        160,
        "root carries the subtree total"
    );
}

#[test]
fn sizes_roll_up_per_directory() {
    let fx = Fixture::new("rollup");
    let index = scan(&fx.0, &ScanOptions::default()).unwrap();

    let find = |name: &str| {
        index
            .nodes()
            .find(|id| index.name(*id) == name)
            .unwrap_or_else(|| panic!("no node named {name}"))
    };

    assert_eq!(index.size(find("sub")), 10, "c.bin + d.txt");
    assert_eq!(index.size(find("deep")), 3);
    assert_eq!(index.size(find("empty")), 0);
    assert_eq!(index.size(find("a.txt")), 100);
    assert_eq!(index.depth(find("d.txt")), 3);
}

#[test]
fn extensions_are_interned_case_insensitively() {
    let fx = Fixture::new("ext");
    let index = scan(&fx.0, &ScanOptions::default()).unwrap();

    let mut txt_bytes = 0;
    let mut txt_files = 0;
    for id in index.nodes() {
        if !index.flags(id).is_dir() && index.ext_str(index.ext(id)) == "txt" {
            txt_bytes += index.size(id);
            txt_files += 1;
        }
    }
    assert_eq!(txt_files, 3, "a.txt, b.TXT and d.txt share one extension");
    assert_eq!(txt_bytes, 153);

    // Directories never carry an extension, even when their name looks like it has one.
    for id in index.nodes() {
        if index.flags(id).is_dir() {
            assert_eq!(index.ext_str(index.ext(id)), "");
        }
    }
}

#[test]
fn structure_invariants_hold() {
    let fx = Fixture::new("structure");
    let index = scan(&fx.0, &ScanOptions::default()).unwrap();

    let mut seen = vec![false; index.len()];
    for id in index.nodes() {
        // Parent before child is what makes the reverse-scan rollup correct.
        if id != index.root() {
            assert!(
                index.parent(id).index() < id.index(),
                "node {id:?} precedes its parent"
            );
        }

        // Children are a contiguous run, and every one of them points back.
        let kids: Vec<_> = index.child_ids(id).collect();
        assert_eq!(kids.len(), index.child_count(id) as usize);
        for (i, kid) in kids.iter().enumerate() {
            assert_eq!(index.parent(*kid), id);
            assert!(!seen[kid.index()], "node {kid:?} claimed by two parents");
            seen[kid.index()] = true;
            if i > 0 {
                assert_eq!(kid.0, kids[i - 1].0 + 1, "children are not contiguous");
            }
        }
    }

    // Every node except the root is some directory's child.
    for id in index.nodes() {
        if id != index.root() {
            assert!(seen[id.index()], "node {id:?} is orphaned");
        }
    }
}

#[test]
fn paths_rebuild_from_names_and_parents() {
    let fx = Fixture::new("paths");
    let index = scan(&fx.0, &ScanOptions::default()).unwrap();

    let d = index
        .nodes()
        .find(|id| index.name(*id) == "d.txt")
        .expect("d.txt");
    assert_eq!(index.path(d), fx.0.join("sub").join("deep").join("d.txt"));
    assert_eq!(index.path(index.root()), fx.0);
}

#[test]
fn scanning_a_single_file_gives_a_one_node_index() {
    let fx = Fixture::new("single");
    let index = scan(&fx.0.join("a.txt"), &ScanOptions::default()).unwrap();

    assert_eq!(index.len(), 1);
    assert_eq!(index.stats.files, 1);
    assert_eq!(index.stats.bytes, 100);
}

#[test]
fn missing_root_is_an_error_not_a_panic() {
    let missing = std::env::temp_dir().join("traveler-scan-does-not-exist-xyzzy");
    assert!(scan(&missing, &ScanOptions::default()).is_err());
}

/// A directory junction must be recorded as a directory, not descended into, and not counted
/// twice. pnpm builds node_modules out of these, so getting it wrong inflates the file count by
/// thousands on a real repo tree.
#[cfg(windows)]
#[test]
fn directory_junctions_are_directories_and_are_not_followed() {
    let fx = Fixture::new("junction");
    let link = fx.0.join("link_to_sub");
    let status = std::process::Command::new("cmd")
        .args(["/C", "mklink", "/J"])
        .arg(&link)
        .arg(fx.0.join("sub"))
        .status()
        .expect("run mklink");
    assert!(status.success(), "mklink failed");

    let index = scan(&fx.0, &ScanOptions::default()).unwrap();
    let id = index
        .nodes()
        .find(|id| index.name(*id) == "link_to_sub")
        .expect("junction node");

    assert!(index.flags(id).is_dir(), "junction is a directory");
    assert!(index.flags(id).is_reparse(), "junction is flagged reparse");
    assert_eq!(
        index.child_count(id),
        0,
        "junction must not be descended into"
    );
    assert_eq!(index.stats.files, 4, "junction is not a file");
    assert_eq!(
        index.stats.bytes, 160,
        "junction must not double-count its target"
    );
    assert_eq!(index.stats.reparse_points, 1);
}

/// Paths past Windows' 260 character MAX_PATH must still scan. std applies the verbatim prefix
/// internally for absolute paths, so this works, but a deep node_modules is exactly where it
/// would break if that ever changed.
#[test]
fn paths_longer_than_max_path_are_scanned() {
    let root = std::env::temp_dir().join(format!("traveler-scan-long-{}", std::process::id()));
    let _ = fs::remove_dir_all(&root);

    let mut deep = root.clone();
    for i in 0..12 {
        deep = deep.join(format!("segment_of_reasonable_length_abcdefghij{i}"));
    }
    fs::create_dir_all(&deep).unwrap();
    let file = deep.join("deep_file.txt");
    fs::write(&file, vec![0u8; 4321]).unwrap();
    assert!(
        file.as_os_str().len() > 260,
        "fixture is not actually a long path: {}",
        file.as_os_str().len()
    );

    let index = scan(&root, &ScanOptions::default()).unwrap();
    assert!(index.errors.is_empty(), "errors: {:?}", index.errors);
    assert_eq!(index.stats.files, 1);
    assert_eq!(index.stats.bytes, 4321);
    assert_eq!(index.stats.dirs, 13);

    let _ = fs::remove_dir_all(&root);
}
