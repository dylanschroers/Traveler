//! Structure, from raw bytes, with no debug information.
//!
//! Scan a mapping for aligned words whose value lands inside another mapped extent. Each hit is a
//! candidate pointer, and for a binary with no symbols the pointer topology is the clearest
//! available statement of what the program's data actually looks like: stacks into heap, heap into
//! heap, GOT entries into libraries, vtables into `.rodata`.
//!
//! Aggregated to mapping-to-mapping edges with weights, which is a grouping and therefore stays
//! inside PLAN.md §6's rule against drawing n² of anything.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use traveler_core::{NodeId, Tree};

use crate::pages::PAGE;
use crate::{AddressSpace, Error, Result};

/// A pointer scan reads memory rather than page tables, so it is priced in bytes and capped the
/// way §7 caps everything that scales with the target rather than with the view.
#[derive(Clone, Copy, Debug)]
pub struct ScanLimits {
    pub budget_bytes: u64,
    /// One read, reused. Large enough that the syscall cost disappears against the scan.
    pub chunk_bytes: usize,
}

impl Default for ScanLimits {
    fn default() -> Self {
        Self {
            budget_bytes: 512 << 20,
            chunk_bytes: 1 << 20,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct PointerEdge {
    pub from: NodeId,
    pub to: NodeId,
    pub count: u64,
}

#[derive(Clone, Debug, Default)]
pub struct Pointers {
    /// Sorted by count, descending.
    pub edges: Vec<PointerEdge>,
    pub words: u64,
    pub found: u64,
    pub bytes_read: u64,
    /// Total mapped bytes of the capture, for the chance baseline below.
    pub mapped: u64,
    /// True when the budget ran out before the resident set did.
    pub truncated: bool,
    pub elapsed: Duration,
}

impl Pointers {
    /// Hits per million words scanned.
    pub fn hit_rate(&self) -> f64 {
        if self.words == 0 {
            return 0.0;
        }
        self.found as f64 / self.words as f64 * 1e6
    }

    /// What the hit rate would be if every word were a number picked uniformly from the
    /// user-space address range: `mapped / 2^47`, per million words.
    ///
    /// §13 says report the precision rather than assume it, and this is what makes the reported
    /// rate mean something. The measured rate divided by this one is how far above chance the
    /// scan is, which is the only honest statement available without ground truth.
    pub fn chance_rate(&self) -> f64 {
        self.mapped as f64 / (1u64 << 47) as f64 * 1e6
    }

    pub fn out_of(&self, from: NodeId) -> impl Iterator<Item = &PointerEdge> {
        self.edges.iter().filter(move |e| e.from == from)
    }
}

/// Reads another process's memory without owning its ptrace attachment.
///
/// `process_vm_readv` is checked against the calling *process*, not the tracer thread, so a
/// worker can hold one of these while the main thread keeps the target. That is what lets a scan
/// costing a full read of the resident set run the way the duplicate finder does: on a key, on a
/// worker, with nothing blocking.
#[derive(Clone, Copy, Debug)]
pub struct MemoryReader {
    pid: i32,
}

impl MemoryReader {
    pub fn new(pid: u32) -> Self {
        Self { pid: pid as i32 }
    }

    #[cfg(target_os = "linux")]
    pub fn read(&self, addr: u64, buf: &mut [u8]) -> Result<usize> {
        let local = libc::iovec {
            iov_base: buf.as_mut_ptr().cast(),
            iov_len: buf.len(),
        };
        let remote = libc::iovec {
            iov_base: addr as *mut libc::c_void,
            iov_len: buf.len(),
        };
        let n = unsafe { libc::process_vm_readv(self.pid, &local, 1, &remote, 1, 0) };
        if n < 0 {
            return Err(Error::Io(std::io::Error::last_os_error()));
        }
        Ok(n as usize)
    }

    #[cfg(not(target_os = "linux"))]
    pub fn read(&self, _addr: u64, _buf: &mut [u8]) -> Result<usize> {
        Err(Error::Unsupported("reading memory off the main thread"))
    }
}

/// One extent of the target, for turning a word into the node it points at.
struct Extents {
    start: Vec<u64>,
    end: Vec<u64>,
    id: Vec<NodeId>,
}

impl Extents {
    fn of(space: &AddressSpace) -> Extents {
        let frame = space.frame();
        let ids: Vec<NodeId> = frame.mappings().map(NodeId).collect();
        Extents {
            start: ids.iter().map(|id| frame.addr(*id)).collect(),
            end: ids.iter().map(|id| frame.end(*id)).collect(),
            id: ids,
        }
    }

    #[inline]
    fn at(&self, addr: u64) -> Option<NodeId> {
        let i = match self.start.binary_search(&addr) {
            Ok(i) => i,
            Err(0) => return None,
            Err(i) => i - 1,
        };
        (addr < self.end[i]).then(|| self.id[i])
    }
}

/// Scans every readable, resident page of `space` for pointers into it.
///
/// Only resident pages are read, and that is not an optimisation: reading an absent page would
/// fault it in and change the working set of the process being measured.
pub fn scan(reader: &MemoryReader, space: &AddressSpace, limits: &ScanLimits) -> Pointers {
    let started = Instant::now();
    let frame = space.frame();
    let extents = Extents::of(space);
    let pages = space.pages();

    let mut counts: HashMap<(u32, u32), u64> = HashMap::new();
    let mut out = Pointers {
        mapped: frame.weight(frame.root()),
        ..Default::default()
    };
    let mut buffer = vec![0u8; limits.chunk_bytes];

    'mappings: for id in frame.mappings().map(NodeId) {
        if !space.prot(id).is_read() {
            continue;
        }
        let Some(m) = space.mapping_index(id) else {
            continue;
        };
        if pages.is_elided(m) {
            continue;
        }

        let base = frame.addr(id);
        for run in resident_runs(pages, m) {
            let (from, to) = (base + run.0 as u64 * PAGE, base + run.1 as u64 * PAGE);
            let mut at = from;
            while at < to {
                if out.bytes_read >= limits.budget_bytes {
                    out.truncated = true;
                    break 'mappings;
                }
                let want = ((to - at) as usize).min(buffer.len());
                let Ok(got) = reader.read(at, &mut buffer[..want]) else {
                    break;
                };
                if got < 8 {
                    break;
                }
                out.bytes_read += got as u64;
                out.words += (got / 8) as u64;

                for word in buffer[..got].chunks_exact(8) {
                    let value = u64::from_ne_bytes(word.try_into().expect("eight bytes"));
                    if let Some(target) = extents.at(value) {
                        out.found += 1;
                        *counts.entry((id.0, target.0)).or_default() += 1;
                    }
                }
                at += got as u64;
            }
        }
    }

    out.edges = counts
        .into_iter()
        .map(|((from, to), count)| PointerEdge {
            from: NodeId(from),
            to: NodeId(to),
            count,
        })
        .collect();
    // Heaviest first, since the interesting edges are the ones with weight behind them.
    out.edges
        .sort_unstable_by(|a, b| b.count.cmp(&a.count).then(a.from.cmp(&b.from)));
    out.elapsed = started.elapsed();
    out
}

/// Runs of consecutive resident pages, so the scan reads long stretches rather than a syscall per
/// page across a sparse mapping.
fn resident_runs(pages: &crate::PageBits, m: usize) -> Vec<(u32, u32)> {
    let count = pages.pages(m);
    let mut runs = Vec::new();
    let mut start = None;
    for page in 0..count {
        match (pages.is_present(m, page), start) {
            (true, None) => start = Some(page),
            (false, Some(from)) => {
                runs.push((from, page));
                start = None;
            }
            _ => {}
        }
    }
    if let Some(from) = start {
        runs.push((from, count));
    }
    runs
}
