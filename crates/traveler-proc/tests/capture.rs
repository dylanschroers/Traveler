//! Capture, measured against a target whose address space is known in advance.
//!
//! The analogue of phase 1's "byte-exact against PowerShell": a plausible-looking city that is
//! quietly wrong is the failure that matters, so these assert the exact extents the fixture made
//! rather than that a capture produced something.

#![cfg(target_os = "linux")]

use std::collections::HashMap;
use std::io::{BufRead, BufReader};

use traveler_core::{NodeId, Tree};
use traveler_proc::{AddressSpace, AttachedTarget, Frame, Launch, NodeKind};

struct Fixture {
    target: AttachedTarget,
    marks: HashMap<String, (u64, u64)>,
}

impl Fixture {
    /// Launches the fixture, lets it build its mappings, and leaves it running and parked.
    fn start(threads: usize) -> Fixture {
        let mut launch = Launch::new(env!("CARGO_BIN_EXE_traveler-fixture")).piped();
        if threads > 0 {
            launch = launch.arg(format!("--threads={threads}"));
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
