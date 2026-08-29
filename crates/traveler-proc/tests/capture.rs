//! Capture, measured against a target whose address space is known in advance.
//!
//! The analogue of phase 1's "byte-exact against PowerShell": a plausible-looking city that is
//! quietly wrong is the failure that matters, so these assert the exact extents the fixture made
//! rather than that a capture produced something.

#![cfg(target_os = "linux")]

use std::collections::HashMap;
use std::io::{BufRead, BufReader};

use traveler_core::{NodeId, Tree};
use traveler_proc::{AddressSpace, AttachedTarget, Caps, CaptureOptions, Frame, Launch, NodeKind};

struct Fixture {
    target: AttachedTarget,
    marks: HashMap<String, (u64, u64)>,
}

impl Fixture {
    fn start(threads: usize) -> Fixture {
        let extra: Vec<String> = (threads > 0)
            .then(|| format!("--threads={threads}"))
            .into_iter()
            .collect();
        Fixture::start_with(&extra)
    }

    /// Launches the fixture, lets it build its mappings, and leaves it running and parked.
    fn start_with(extra: &[String]) -> Fixture {
        let mut launch = Launch::new(env!("CARGO_BIN_EXE_traveler-fixture")).piped();
        for arg in extra {
            launch = launch.arg(arg);
        }
        let mut target = AttachedTarget::launch(&launch).expect("launch the fixture");
        let stdout = target.take_stdout().expect("piped stdout");
        target.resume().expect("resume");

        let mut marks = HashMap::new();
        for line in BufReader::new(stdout).lines() {
            let line = line.expect("fixture output");
            let mut parts = line.split(' ');
            let key = parts.next().unwrap_or_default().to_string();
            if key == "ready" {
                return Fixture { target, marks };
            }
            let a = parts.next().and_then(|v| u64::from_str_radix(v, 16).ok());
            let b = parts.next().and_then(|v| u64::from_str_radix(v, 16).ok());
            if let (Some(a), Some(b)) = (a, b) {
                marks.insert(key, (a, b));
            } else if let Some(a) = a {
                marks.insert(key, (a, 0));
            }
        }
        panic!("fixture never became ready");
    }

    fn mark(&self, name: &str) -> (u64, u64) {
        *self
            .marks
            .get(name)
            .unwrap_or_else(|| panic!("no mark {name}"))
    }
}

fn mapping_covering(space: &AddressSpace, addr: u64) -> NodeId {
    space
        .frame()
        .mapping_at(addr)
        .unwrap_or_else(|| panic!("no mapping covers {addr:#x}"))
}

fn assert_tree_is_well_formed(frame: &Frame) {
    assert_eq!(frame.depth(frame.root()), 0);
    for id in frame.nodes() {
        let kids = frame.children(id);
        if kids.is_empty() {
            continue;
        }
        // Parents before children, children contiguous, and the whole subtree accounted for.
        assert!(
            kids.start > id.0,
            "child {} precedes parent {}",
            kids.start,
            id.0
        );
        let mut sum = 0u64;
        let mut cursor = frame.addr(id);
        for c in frame.child_ids(id) {
            assert_eq!(frame.parent(c), id);
            assert_eq!(frame.depth(c), frame.depth(id) + 1);
            assert!(
                frame.addr(c) >= cursor,
                "children of {} are not in address order",
                id.0
            );
            cursor = frame.end(c);
            sum += frame.weight(c);
        }
        assert_eq!(
            sum,
            frame.weight(id),
            "children of {} do not sum to it",
            id.0
        );
    }
}

#[test]
fn a_capture_sees_the_exact_mappings_the_fixture_made() {
    let mut fx = Fixture::start(0);
    let space = fx.target.capture().expect("capture");
    let frame = space.frame();

    let (window, window_len) = fx.mark("window");
    let id = mapping_covering(&space, window);
    assert_eq!(frame.addr(id), window);
    assert_eq!(frame.weight(id), window_len);
    assert_eq!(frame.kind(id), NodeKind::Mapping);
    let prot = space.prot(id);
    assert!(prot.is_read() && prot.is_write() && !prot.is_exec());

    let (code, code_len) = fx.mark("code");
    let id = mapping_covering(&space, code);
    assert_eq!(frame.addr(id), code);
    assert_eq!(frame.weight(id), code_len);
    assert!(space.prot(id).is_exec() && !space.prot(id).is_write());
}

/// One reservation with a window mprotected inside it is three mappings and one region. This is
/// the Linux stand-in for `AllocationBase`, which has no kernel field to read it off.
///
/// The region may be wider than the arena, and that is not a bug: two adjacent anonymous mappings
/// from unrelated `mmap` calls are indistinguishable in `maps`, so they fuse. Windows keeps them
/// apart because it records the allocation; Linux records nothing.
#[test]
fn an_mprotected_window_splits_a_mapping_without_splitting_its_region() {
    let mut fx = Fixture::start(0);
    let space = fx.target.capture().expect("capture");
    let frame = space.frame();

    let (arena, arena_len) = fx.mark("arena");
    let (window, window_len) = fx.mark("window");
    let id = mapping_covering(&space, window);
    let region = frame.parent(id);

    assert_eq!(frame.kind(region), NodeKind::Region);
    assert!(frame.addr(region) <= arena && frame.end(region) >= arena + arena_len);

    let kids: Vec<NodeId> = frame.child_ids(region).collect();
    let at = kids
        .iter()
        .position(|k| *k == id)
        .expect("window is a child");
    let (head, tail) = (kids[at - 1], kids[at + 1]);

    // The mprotect cut one mapping into three, and the three still cover the arena exactly.
    assert_eq!(frame.addr(head), arena);
    assert_eq!(frame.end(head), window);
    assert_eq!(frame.addr(id), window);
    assert_eq!(frame.weight(id), window_len);
    assert_eq!(frame.addr(tail), window + window_len);
    assert_eq!(frame.end(tail), arena + arena_len);

    assert_eq!(space.prot(head).to_string(), "---p");
    assert_eq!(space.prot(id).to_string(), "rw-p");
    assert_eq!(space.prot(tail).to_string(), "---p");
    // A region reads as writable because part of it is, rather than as nothing at all.
    assert!(space.prot(region).is_write());
}

#[test]
fn weights_are_exactly_additive_and_ids_follow_the_address() {
    let mut fx = Fixture::start(0);
    let space = fx.target.capture().expect("capture");
    let frame = space.frame();

    assert_tree_is_well_formed(frame);

    let mappings = frame.mappings();
    assert!(mappings.len() > 4);
    let total: u64 = frame
        .child_ids(frame.root())
        .map(|r| frame.weight(r))
        .sum::<u64>();
    assert_eq!(total, frame.weight(frame.root()));

    let addrs: Vec<u64> = (mappings.start..mappings.end)
        .map(|i| frame.addr(NodeId(i)))
        .collect();
    assert!(
        addrs.windows(2).all(|w| w[0] < w[1]),
        "mapping ids are not in address order"
    );
}

/// The stop policy: a capture observes, it does not decide. Whether the target runs afterwards is
/// exactly whether it ran before.
#[test]
fn a_capture_does_not_change_whether_the_target_is_running() {
    let mut fx = Fixture::start(0);
    assert!(!fx.target.is_stopped());
    fx.target.capture().expect("capture");
    assert!(!fx.target.is_stopped());
    assert!(fx.target.is_alive());

    fx.target.stop().expect("stop");
    fx.target.capture().expect("capture");
    assert!(fx.target.is_stopped());
}

/// The reason launch comes first: capture 0 is the address space before the dynamic linker has
/// run. That frame is unobtainable if you arrive late.
#[test]
fn the_first_capture_is_taken_before_the_dynamic_linker_has_run() {
    let launch = Launch::new(env!("CARGO_BIN_EXE_traveler-fixture")).piped();
    let mut target = AttachedTarget::launch(&launch).expect("launch");
    assert!(target.is_stopped());

    let at_entry = target.capture().expect("capture at entry");
    assert!(target.is_stopped(), "capture 0 must not release the target");
    let names = |space: &AddressSpace| -> Vec<String> {
        let frame = space.frame();
        frame
            .nodes()
            .map(|id| frame.label(id).to_string())
            .collect()
    };
    let entry = names(&at_entry);
    assert!(
        !entry.iter().any(|n| n.starts_with("libc.so")),
        "libc is already mapped at the execve stop: {entry:?}"
    );

    target.resume().expect("resume");
    let stdout = BufReader::new(target.take_stdout().expect("stdout"));
    assert!(
        stdout
            .lines()
            .map_while(std::result::Result::ok)
            .any(|l| l.starts_with("ready")),
        "fixture never became ready"
    );

    let linked = target.capture().expect("capture after linking");
    assert!(
        names(&linked).iter().any(|n| n.starts_with("libc.so")),
        "libc never arrived"
    );
    assert!(linked.frame().len() > at_entry.frame().len());
}

/// Ptrace is per-thread, so seizing the leader alone leaves the others writing underneath the
/// read. This is the test that catches that, against a fixture that never stops writing.
#[test]
fn every_thread_is_stopped_for_the_read() {
    let mut fx = Fixture::start(4);
    let space = fx.target.capture().expect("capture");
    let meta = space.meta();

    assert_eq!(meta.threads, 5, "not every thread was stopped");
    assert!(meta.consistent, "the task list never settled");
    assert!(meta.stop.as_millis() < 500, "stop took {:?}", meta.stop);
}

#[test]
fn memory_reads_come_back_with_what_the_fixture_wrote() {
    let fx = Fixture::start(0);
    let (window, _) = fx.mark("window");
    let (marker, _) = fx.mark("marker");

    let mut buf = [0u8; 8];
    let n = fx.target.read_mem(window, &mut buf).expect("read");
    assert_eq!(n, 8);
    assert_eq!(u64::from_ne_bytes(buf), marker);
}

#[test]
fn attaching_to_a_process_we_did_not_launch_is_refused_with_a_reason() {
    // pid 1 is never traceable from a normal user, and the message has to say why rather than
    // leave a bare "operation not permitted".
    let text = match AttachedTarget::attach(1) {
        Ok(_) => panic!("attaching to init should not have been allowed"),
        Err(e) => e.to_string(),
    };
    assert!(text.contains('1'), "unhelpful message: {text}");
}

/// The fixture touches page 0 and page 2 of its window and nothing else, so residency is a fact
/// with a known answer rather than a plausible-looking number.
#[test]
fn residency_matches_the_pages_the_fixture_actually_touched() {
    let mut fx = Fixture::start(0);
    let space = fx.target.capture().expect("capture");
    assert!(space.caps().has(Caps::PAGE_RESIDENCY));

    let (window, window_len) = fx.mark("window");
    let id = mapping_covering(&space, window);
    let m = space.mapping_index(id).expect("a mapping");
    let pages = space.pages();

    assert!(!pages.is_elided(m));
    assert_eq!(pages.pages(m) as u64, window_len / 4096);
    assert!(
        pages.is_present(m, 0),
        "page 0 was written and is not resident"
    );
    assert!(
        pages.is_present(m, 2),
        "page 2 was written and is not resident"
    );
    assert!(!pages.is_present(m, 1), "page 1 was never touched");
    assert_eq!(pages.resident(m), 2);
    assert_eq!(space.resident_bytes(id), 2 * 4096);

    // The reservation around it was never touched at all.
    let head = space.frame().parent(id);
    let guard = space.frame().child_ids(head).next().expect("a guard");
    assert_eq!(space.resident_bytes(guard), 0);
}

#[test]
fn a_mapping_over_the_detail_cap_reports_nothing_rather_than_zero() {
    let mut fx = Fixture::start(0);
    let options = CaptureOptions {
        pages: traveler_proc::PageLimits {
            // Smaller than the fixture's own window, so it has to be elided.
            detail_pages: 1,
            ..Default::default()
        },
        ..Default::default()
    };
    let space = fx.target.capture_with(&options).expect("capture");
    let id = mapping_covering(&space, fx.mark("window").0);
    assert_eq!(space.resident_fraction(id), None);
    assert_eq!(space.unknown_bytes(id), space.frame().weight(id));
    assert!(space.pages().is_elided(space.mapping_index(id).unwrap()));

    // The region above it inherits the gap rather than reporting a confident zero.
    let region = space.frame().parent(id);
    assert!(space.unknown_bytes(region) >= space.frame().weight(id));
}

#[test]
fn skipping_pages_clears_the_capability_rather_than_reporting_zeros() {
    let mut fx = Fixture::start(0);
    let options = CaptureOptions {
        without_pages: true,
        ..Default::default()
    };
    let space = fx.target.capture_with(&options).expect("capture");
    assert!(!space.caps().has(Caps::PAGE_RESIDENCY));
    let id = mapping_covering(&space, fx.mark("window").0);
    assert_eq!(space.resident_fraction(id), None);
    assert_eq!(space.resident_bytes(space.frame().root()), 0);
}

/// Soft-dirty is relative: clear at one capture, read at the next. The fixture's threads write to
/// one page and nothing else, so exactly that page should come back dirty.
#[test]
fn soft_dirty_reports_what_was_written_between_two_captures() {
    let mut fx = Fixture::start(1);
    let cleared = fx
        .target
        .capture_with(&CaptureOptions {
            clear_soft_dirty: true,
            ..Default::default()
        })
        .expect("capture");
    if !cleared.caps().has(Caps::SOFT_DIRTY) {
        return; // CONFIG_MEM_SOFT_DIRTY is off, and the capability says so.
    }

    std::thread::sleep(std::time::Duration::from_millis(50));
    let space = fx.target.capture().expect("capture");
    let id = mapping_covering(&space, fx.mark("window").0);
    let m = space.mapping_index(id).expect("a mapping");
    let pages = space.pages();

    // The spinning thread writes page 3, and only page 3.
    assert!(pages.is_dirty(m, 3), "the written page is not marked dirty");
    assert!(!pages.is_dirty(m, 1), "an untouched page came back dirty");
}

/// Residency has to roll up exactly, or a region reports a number its mappings do not add to.
#[test]
fn residency_sums_up_the_tree() {
    let mut fx = Fixture::start(0);
    let space = fx.target.capture().expect("capture");
    let frame = space.frame();
    for id in frame.nodes() {
        if frame.child_count(id) == 0 {
            continue;
        }
        let resident: u64 = frame.child_ids(id).map(|c| space.resident_bytes(c)).sum();
        let unknown: u64 = frame.child_ids(id).map(|c| space.unknown_bytes(c)).sum();
        assert_eq!(resident, space.resident_bytes(id));
        assert_eq!(unknown, space.unknown_bytes(id));
        assert!(space.resident_bytes(id) + space.unknown_bytes(id) <= frame.weight(id));
    }
}

/// The cap §7 exists for. A reservation far larger than the working set must not be read, and the
/// stop must stay bounded rather than scaling with the address space.
#[test]
fn a_huge_reservation_is_elided_and_the_stop_stays_bounded() {
    let plain = Fixture::start(0);
    let plain_stop = {
        let mut fx = plain;
        fx.target.capture().expect("capture").meta().stop
    };

    let mut fx = Fixture::start_with(&["--reserve=64".to_string()]);
    let space = fx.target.capture().expect("capture");
    let (reserve, reserve_len) = fx.mark("reserve");

    let id = mapping_covering(&space, reserve);
    assert_eq!(space.frame().weight(id), reserve_len);
    assert_eq!(
        space.resident_fraction(id),
        None,
        "a 64 GiB reservation was read rather than elided"
    );
    assert_eq!(space.unknown_bytes(id), reserve_len);

    // Everything else still gets read, and the reservation costs nothing.
    let window = mapping_covering(&space, fx.mark("window").0);
    assert_eq!(space.resident_bytes(window), 2 * 4096);
    assert!(
        space.meta().stop < plain_stop * 4 + std::time::Duration::from_millis(20),
        "stop grew with the address space: {:?} against {plain_stop:?}",
        space.meta().stop
    );
}

/// §12: a poke happens at a stop, so there is no race with the target's threads and no torn
/// write. The fixture writes a known marker, so a read-back proves the write rather than assuming
/// the call succeeded.
#[test]
fn a_poke_lands_where_it_was_aimed() {
    let mut fx = Fixture::start(0);
    let (window, _) = fx.mark("window");
    let (marker, _) = fx.mark("marker");

    let mut before = [0u8; 8];
    fx.target.read_mem(window, &mut before).expect("read");
    assert_eq!(u64::from_ne_bytes(before), marker);

    let poked: u64 = 0x0123_4567_89ab_cdef;
    let n = fx
        .target
        .write_mem(window, &poked.to_ne_bytes())
        .expect("write");
    assert_eq!(n, 8);

    let mut after = [0u8; 8];
    fx.target.read_mem(window, &mut after).expect("read");
    assert_eq!(u64::from_ne_bytes(after), poked);
    // The target is still running, because a poke observes the same policy a capture does.
    assert!(!fx.target.is_stopped());
    assert!(fx.target.is_alive());
}

/// The reason it is `/proc/pid/mem` and not `process_vm_writev`: a read-only page. The fixture's
/// code mapping is `r-x`, and writing it is how a debugger patches `.text`.
#[test]
fn a_poke_can_write_a_page_that_is_not_writable() {
    let mut fx = Fixture::start(0);
    let (code, _) = fx.mark("code");

    let poked: u64 = 0xdead_beef_feed_face;
    fx.target
        .write_mem(code, &poked.to_ne_bytes())
        .expect("write to a read-only mapping");

    let mut back = [0u8; 8];
    fx.target.read_mem(code, &mut back).expect("read");
    assert_eq!(u64::from_ne_bytes(back), poked);
}

#[test]
fn a_poke_into_nothing_fails_rather_than_pretending() {
    let mut fx = Fixture::start(0);
    assert!(fx.target.write_mem(0x10, &[1, 2, 3, 4]).is_err());
    assert_eq!(fx.target.write_mem(fx.mark("window").0, &[]).ok(), Some(0));
}

/// §13: for a binary with no symbols, the pointer topology is the clearest available statement of
/// what the program's data looks like. The fixture writes pointers with known destinations, so the
/// scan has an exact answer rather than a plausible one.
#[test]
fn the_pointer_scan_finds_the_edges_the_fixture_wrote() {
    use traveler_proc::{MemoryReader, ScanLimits, pointers};

    let mut fx = Fixture::start(0);
    let space = fx.target.capture().expect("capture");
    let reader = MemoryReader::new(fx.target.pid());
    let found = pointers::scan(&reader, &space, &ScanLimits::default());

    let window = mapping_covering(&space, fx.mark("window").0);
    let code = mapping_covering(&space, fx.mark("code").0);

    let to_code = found
        .out_of(window)
        .find(|e| e.to == code)
        .expect("no edge from the window to the code mapping");
    assert_eq!(to_code.count, 3, "three pointers were written into code");

    let to_self = found
        .out_of(window)
        .find(|e| e.to == window)
        .expect("no edge from the window back to itself");
    assert!(to_self.count >= 1);

    // Heaviest first, and the scan reports its own precision rather than assuming it.
    assert!(found.edges.windows(2).all(|w| w[0].count >= w[1].count));
    assert!(found.words > 0);
    assert!(!found.truncated);
}

/// Reading an absent page would fault it in and change the working set of the process being
/// measured, so only resident pages are read.
#[test]
fn the_pointer_scan_leaves_the_working_set_alone() {
    use traveler_proc::{MemoryReader, ScanLimits, pointers};

    let mut fx = Fixture::start(0);
    let before = fx.target.capture().expect("capture");
    let resident = before.resident_bytes(before.frame().root());

    let reader = MemoryReader::new(fx.target.pid());
    pointers::scan(&reader, &before, &ScanLimits::default());

    let after = fx.target.capture().expect("capture");
    let grown = after
        .resident_bytes(after.frame().root())
        .saturating_sub(resident);
    assert!(
        grown < 256 * 4096,
        "the scan faulted in {grown} bytes of the target"
    );
}

/// The budget has to bound the scan, or a target with a large resident set is unbounded work.
#[test]
fn the_pointer_scan_stops_when_its_budget_runs_out() {
    use traveler_proc::{MemoryReader, ScanLimits, pointers};

    let mut fx = Fixture::start(0);
    let space = fx.target.capture().expect("capture");
    let reader = MemoryReader::new(fx.target.pid());
    let found = pointers::scan(
        &reader,
        &space,
        &ScanLimits {
            budget_bytes: 4096,
            chunk_bytes: 4096,
        },
    );
    assert!(
        found.truncated,
        "a 4 KiB budget should not cover the target"
    );
    assert!(found.bytes_read <= 8192);
}
