//! Byte-identical files.
//!
//! Three stages, each one cutting the work for the next:
//!
//! 1. **Bucket by size.** Identical files must have identical length, and this costs no I/O at
//!    all. Most files are the only one of their exact size and never get read.
//! 2. **Hash a prefix** of whatever survives. A file no larger than the prefix is fully hashed
//!    here and needs no second pass.
//! 3. **Hash in full** only the files that still collide, which is a small fraction of a
//!    fraction. The full hash is what makes the answer exact rather than probable, and it matters
//!    because the obvious next feature is deleting things.
//!
//! Zero-length files are excluded outright. They are all byte-identical to each other, there are
//! thousands of them on a real disk, and reporting one enormous group of them is noise.

use std::{
    fs::File,
    io::Read,
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
    time::Instant,
};

use rayon::prelude::*;
use traveler_scan::{FileIndex, NodeId, Tree};

use crate::{EdgeKind, EdgeProvider, EdgeSet, EdgeStats, Group};

pub struct ExactDuplicates {
    /// How much of a file to read in the screening pass.
    pub prefix_bytes: u64,
    /// Files below this are never read. On a spinning disk the cost of this pass is one seek per
    /// file, about 6 ms, no matter how little you read; a 500 byte duplicate cannot pay that back.
    /// Set to 0 to consider everything.
    pub min_size: u64,
    /// Threads used for reading. Deliberately not one per core: this stage is disk-bound, and on
    /// a spinning disk ten threads reading at once just makes the head seek between them. Four is
    /// a compromise that keeps an SSD busy without thrashing a platter.
    pub threads: usize,
}

impl Default for ExactDuplicates {
    fn default() -> Self {
        Self {
            prefix_bytes: 64 * 1024,
            min_size: 4096,
            threads: 4,
        }
    }
}

#[derive(Default)]
struct Counters {
    files: AtomicU64,
    bytes: AtomicU64,
    errors: AtomicU64,
    changed: AtomicU64,
}

impl EdgeProvider for ExactDuplicates {
    fn kind(&self) -> EdgeKind {
        EdgeKind::Duplicate
    }

    fn compute(&self, index: &FileIndex) -> EdgeSet {
        let started = Instant::now();
        let counters = Counters::default();

        // Stage 1: same length, or it cannot be a duplicate.
        let mut by_size: Vec<(u64, NodeId)> = index
            .nodes()
            .filter(|id| !index.flags(*id).is_dir() && index.size(*id) >= self.min_size.max(1))
            .map(|id| (index.size(id), id))
            .collect();
        by_size.sort_unstable();

        // What the floor cost us, so the tradeoff is visible rather than silent.
        let mut small: Vec<(u64, NodeId)> = index
            .nodes()
            .filter(|id| {
                !index.flags(*id).is_dir()
                    && index.size(*id) > 0
                    && index.size(*id) < self.min_size
            })
            .map(|id| (index.size(id), id))
            .collect();
        small.sort_unstable();
        let skipped_small = runs_of_two_or_more(&small, |a, b| a.0 == b.0).len() as u64;
        let mut candidates = runs_of_two_or_more(&by_size, |a, b| a.0 == b.0);
        // Read in walk order, not size order. Consecutive ids are neighbours in the directory
        // tree and usually neighbours on the disk too, so this turns a scatter of seeks into
        // something closer to a sweep.
        candidates.sort_unstable_by_key(|(_, id)| *id);

        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(self.threads.max(1))
            .build();

        let work = || {
        // Stage 2: screen on a prefix.
        let screened: Vec<(u64, [u8; 32], NodeId)> = candidates
            .par_iter()
            .filter_map(|(size, id)| {
                let limit = (*size).min(self.prefix_bytes);
                // When the prefix covers the file, this read is the whole thing.
                let whole = *size <= self.prefix_bytes;
                hash_file(&index.path(*id), limit, whole, &counters).map(|h| (*size, h, *id))
            })
            .collect();

        let mut keyed = screened;
        keyed.sort_unstable();
        let colliding = runs_of_two_or_more(&keyed, |a, b| a.0 == b.0 && a.1 == b.1);

        // Anything that fits inside the prefix is already fully hashed.
        let (settled, needs_full): (Vec<_>, Vec<_>) = colliding
            .into_iter()
            .partition(|(size, _, _)| *size <= self.prefix_bytes);

        // Stage 3: full hashes for the rest, again in walk order.
        let mut needs_full = needs_full;
        needs_full.sort_unstable_by_key(|(_, _, id)| *id);
        let mut full: Vec<(u64, [u8; 32], NodeId)> = needs_full
            .par_iter()
            .filter_map(|(size, _, id)| {
                hash_file(&index.path(*id), *size, true, &counters).map(|h| (*size, h, *id))
            })
            .collect();
        full.sort_unstable();

        let mut groups = Vec::new();
        collect_groups(index, &settled, &mut groups);
        collect_groups(
            index,
            &runs_of_two_or_more(&full, |a, b| a.0 == b.0 && a.1 == b.1),
            &mut groups,
        );
        groups
        };
        let groups = match pool {
            Ok(p) => p.install(work),
            Err(_) => work(),
        };

        EdgeSet::finish(
            EdgeKind::Duplicate,
            groups,
            EdgeStats {
                millis: started.elapsed().as_millis() as u64,
                files_hashed: counters.files.load(Ordering::Relaxed),
                bytes_hashed: counters.bytes.load(Ordering::Relaxed),
                read_errors: counters.errors.load(Ordering::Relaxed),
                changed_while_reading: counters.changed.load(Ordering::Relaxed),
                skipped_small,
            },
        )
    }
}

/// Keeps only the elements that belong to a run of two or more equal neighbours. Input must be
/// sorted so equal elements are adjacent.
fn runs_of_two_or_more<T: Clone>(sorted: &[T], eq: impl Fn(&T, &T) -> bool) -> Vec<T> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < sorted.len() {
        let mut j = i + 1;
        while j < sorted.len() && eq(&sorted[i], &sorted[j]) {
            j += 1;
        }
        if j - i >= 2 {
            out.extend_from_slice(&sorted[i..j]);
        }
        i = j;
    }
    out
}

/// Turns runs of equal (size, hash) into groups.
fn collect_groups(index: &FileIndex, sorted: &[(u64, [u8; 32], NodeId)], out: &mut Vec<Group>) {
    let mut i = 0;
    while i < sorted.len() {
        let (size, hash, _) = sorted[i];
        let mut j = i + 1;
        while j < sorted.len() && sorted[j].0 == size && sorted[j].1 == hash {
            j += 1;
        }
        let n = (j - i) as u64;
        if n >= 2 {
            let mut members: Vec<NodeId> = sorted[i..j].iter().map(|(_, _, id)| *id).collect();
            members.sort_unstable();
            out.push(Group {
                label: index.name(members[0]).to_string(),
                bytes: size * n,
                // Keep one, drop the rest.
                redundant_bytes: size * (n - 1),
                members,
            });
        }
        i = j;
    }
}

/// Hashes exactly `limit` bytes. Returns None when the file cannot be read, which happens often
/// enough on a live disk that it must not be fatal, and also when the file is no longer the
/// length the scan recorded.
///
/// `whole` says that `limit` is meant to be the entire file, in which case reaching the limit
/// without hitting end-of-file means the file grew. Either way the size this file was grouped
/// under is now wrong, and hashing a different number of bytes than a sibling would make the
/// comparison meaningless, so it is dropped rather than guessed at.
fn hash_file(path: &Path, limit: u64, whole: bool, counters: &Counters) -> Option<[u8; 32]> {
    let mut file = match File::open(path) {
        Ok(f) => f,
        Err(_) => {
            counters.errors.fetch_add(1, Ordering::Relaxed);
            return None;
        }
    };
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; 64 * 1024];
    let mut read = 0u64;
    while read < limit {
        let want = ((limit - read) as usize).min(buf.len());
        match file.read(&mut buf[..want]) {
            // Short: the file shrank since the scan.
            Ok(0) => break,
            Ok(n) => {
                hasher.update(&buf[..n]);
                read += n as u64;
            }
            Err(_) => {
                counters.errors.fetch_add(1, Ordering::Relaxed);
                return None;
            }
        }
    }
    if read != limit {
        counters.changed.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    if whole {
        // Long: anything left after the recorded length means the file grew.
        match file.read(&mut buf[..1]) {
            Ok(0) => {}
            Ok(_) => {
                counters.changed.fetch_add(1, Ordering::Relaxed);
                return None;
            }
            Err(_) => {
                counters.errors.fetch_add(1, Ordering::Relaxed);
                return None;
            }
        }
    }
    counters.files.fetch_add(1, Ordering::Relaxed);
    counters.bytes.fetch_add(read, Ordering::Relaxed);
    Some(*hasher.finalize().as_bytes())
}
