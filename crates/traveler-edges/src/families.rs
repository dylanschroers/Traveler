//! Files in the same folder whose names come from one stem: `foo.h` beside `foo.c`,
//! `IMG_0001.jpg` through `IMG_0099.jpg`.
//!
//! Deliberately scoped to a single directory. Matching stems across the whole tree finds thousands
//! of unrelated `index`, `main` and `mod` files and tells you nothing.
//!
//! The normalised stem is always a *prefix* of the file name, so grouping needs no allocation at
//! all: the keys are borrowed slices of the index's name blob.

use std::time::Instant;

use traveler_scan::{FileIndex, NodeId};

use crate::{EdgeKind, EdgeProvider, EdgeSet, EdgeStats, Group};

pub struct NameFamilies;

/// Drops the extension, then any trailing counter. `IMG_0042.jpg` becomes `IMG`, `foo.c` stays
/// `foo`. A dotfile like `.gitignore` keeps its whole name, since the leading dot is not an
/// extension separator.
fn normalized_stem(name: &str) -> &str {
    let stem = match name.rfind('.') {
        Some(i) if i > 0 => &name[..i],
        _ => name,
    };
    stem.trim_end_matches(|c: char| c.is_ascii_digit() || matches!(c, '-' | '_' | '.' | ' '))
}

impl EdgeProvider for NameFamilies {
    fn kind(&self) -> EdgeKind {
        EdgeKind::NameFamily
    }

    fn compute(&self, index: &FileIndex) -> EdgeSet {
        let started = Instant::now();
        let mut groups = Vec::new();
        // Reused per directory so a tree of 100k folders does not allocate 100k vectors.
        let mut scratch: Vec<(&str, NodeId)> = Vec::new();

        for dir in index.iter() {
            if index.child_count(dir) < 2 {
                continue;
            }
            // Two components of context, because a leaf name like `deps` occurs once per build
            // profile and a bare label cannot be told apart in a list.
            let parent = index.parent(dir);
            let where_ = if parent == dir {
                index.name(dir).to_string()
            } else {
                format!("{}/{}", index.name(parent), index.name(dir))
            };
            scratch.clear();
            for child in index.children(dir) {
                if index.flags(child).is_dir() {
                    continue;
                }
                scratch.push((normalized_stem(index.name(child)), child));
            }
            if scratch.len() < 2 {
                continue;
            }
            scratch.sort_unstable_by(|a, b| a.0.cmp(b.0).then(a.1.cmp(&b.1)));

            let mut i = 0;
            while i < scratch.len() {
                let stem = scratch[i].0;
                let mut j = i + 1;
                while j < scratch.len() && scratch[j].0 == stem {
                    j += 1;
                }
                if j - i >= 2 {
                    let members: Vec<NodeId> = scratch[i..j].iter().map(|(_, id)| *id).collect();
                    let bytes = members.iter().map(|m| index.size(*m)).sum();
                    // An empty stem means the names were nothing but digits and separators, which
                    // is a numbered sequence with no prefix.
                    let shown = if stem.is_empty() { "(numbered)" } else { stem };
                    groups.push(Group {
                        label: format!("{shown}* in {where_}"),
                        members,
                        bytes,
                        redundant_bytes: 0,
                    });
                }
                i = j;
            }
        }

        EdgeSet::finish(
            EdgeKind::NameFamily,
            groups,
            EdgeStats {
                millis: started.elapsed().as_millis() as u64,
                ..Default::default()
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::normalized_stem;

    #[test]
    fn strips_extension_and_counter() {
        assert_eq!(normalized_stem("foo.c"), "foo");
        assert_eq!(normalized_stem("foo.h"), "foo");
        assert_eq!(normalized_stem("IMG_0042.jpg"), "IMG");
        assert_eq!(normalized_stem("frame 12.png"), "frame");
        assert_eq!(normalized_stem("report-2024.pdf"), "report");
    }

    #[test]
    fn leaves_names_that_have_no_counter_alone() {
        assert_eq!(normalized_stem("main.rs"), "main");
        assert_eq!(normalized_stem("package-lock.json"), "package-lock");
        assert_eq!(normalized_stem("README"), "README");
    }

    #[test]
    fn a_dotfile_is_not_an_extension() {
        assert_eq!(normalized_stem(".gitignore"), ".gitignore");
    }

    #[test]
    fn an_all_digit_name_normalises_to_nothing() {
        assert_eq!(normalized_stem("0001.png"), "");
        assert_eq!(normalized_stem("12"), "");
    }
}
