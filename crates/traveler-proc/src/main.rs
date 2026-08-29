//! `traveler-proc --run <cmd>` — capture a process's address space and report what is in it.
//!
//! The numeric read on the capture, the way `traveler-layout` is the numeric read on the treemap.
//! Stop time is the number §7 of PLAN-DYNAMIC exists to minimise, so it is what this prints first.

use std::process::ExitCode;

use traveler_core::Tree;
use traveler_proc::{AddressSpace, AttachedTarget, Launch, NodeKind};

const USAGE: &str = "\
usage: traveler-proc [options] --run <cmd> [args...]
       traveler-proc [options] --attach <pid>

  --run takes the rest of the line, so options come before it.

  --captures N    take N captures rather than one
  --wait MS       let the target run this long before the first capture (default 300)
  --no-aslr       launch with ADDR_NO_RANDOMIZE, so two runs are comparable
  --top N         list the N largest regions (default 12)";

fn main() -> ExitCode {
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let mut launch: Option<Launch> = None;
    let mut attach: Option<u32> = None;
    let mut captures = 1usize;
    let mut wait_ms = 300u64;
    let mut no_aslr = false;
    let mut top = 12usize;

    let mut i = 0;
    while i < raw.len() {
        match raw[i].as_str() {
            "--run" => {
                let mut l = Launch::new(raw.get(i + 1).map(String::as_str).unwrap_or_default());
                if l.program.is_empty() {
                    return usage("--run needs a command");
                }
                for extra in &raw[i + 2..] {
                    l = l.arg(extra);
                }
                launch = Some(l);
                i = raw.len();
            }
            "--attach" => {
                i += 1;
                match raw.get(i).and_then(|v| v.parse().ok()) {
                    Some(pid) => attach = Some(pid),
                    None => return usage("--attach needs a pid"),
                }
            }
            "--captures" => {
                i += 1;
                match raw.get(i).and_then(|v| v.parse().ok()) {
                    Some(n) => captures = n,
                    None => return usage("--captures needs a number"),
                }
            }
            "--wait" => {
                i += 1;
                match raw.get(i).and_then(|v| v.parse().ok()) {
                    Some(n) => wait_ms = n,
                    None => return usage("--wait needs milliseconds"),
                }
            }
            "--top" => {
                i += 1;
                match raw.get(i).and_then(|v| v.parse().ok()) {
                    Some(n) => top = n,
                    None => return usage("--top needs a number"),
                }
            }
            "--no-aslr" => no_aslr = true,
            "-h" | "--help" => return usage(""),
            other => return usage(&format!("unknown argument: {other}")),
        }
        i += 1;
    }

    let mut target = match (launch, attach) {
        (Some(mut l), _) => {
            l.disable_aslr = no_aslr;
            match AttachedTarget::launch(&l) {
                Ok(t) => t,
                Err(e) => return fail(&format!("cannot launch: {e}")),
            }
        }
        (None, Some(pid)) => match AttachedTarget::attach(pid) {
            Ok(t) => t,
            Err(e) => return fail(&e.to_string()),
        },
        (None, None) => return usage(""),
    };

    println!("\n{} [{}]", target.name(), target.pid());
    if let Err(e) = target.resume() {
        return fail(&format!("cannot resume: {e}"));
    }

    for n in 0..captures {
        std::thread::sleep(std::time::Duration::from_millis(wait_ms));
        match target.capture() {
            Ok(space) => report(&space, n, top),
            Err(e) => return fail(&e.to_string()),
        }
    }
    println!();
    ExitCode::SUCCESS
}

fn report(space: &AddressSpace, n: usize, top: usize) {
    let frame = space.frame();
    let meta = space.meta();
    let regions = frame.child_count(frame.root()) as usize;
    let mappings = frame.mappings().len();

    println!(
        "\ncapture {n} at {:.3}s   stop {:.2} ms over {} thread{}{}",
        meta.monotonic.as_secs_f64(),
        meta.stop.as_secs_f64() * 1000.0,
        meta.threads,
        if meta.threads == 1 { "" } else { "s" },
        if meta.consistent {
            ""
        } else {
            "   (TORN: the task list never settled)"
        }
    );
    let ids = || frame.mappings().map(traveler_core::NodeId);
    let root = frame.root();
    let unread = space.unknown_bytes(root);
    println!(
        "  {} nodes: {regions} regions, {mappings} mappings, {} mapped, {} resident{}",
        frame.len(),
        bytes(frame.weight(root)),
        bytes(space.resident_bytes(root)),
        if unread == 0 {
            String::new()
        } else {
            format!("   ({} unread)", bytes(unread))
        }
    );

    let mut by_size: Vec<_> = frame
        .child_ids(frame.root())
        .map(|id| (frame.weight(id), id))
        .collect();
    by_size.sort_unstable_by_key(|(size, _)| std::cmp::Reverse(*size));

    for (size, id) in by_size.iter().take(top) {
        println!(
            "  {:>10} {:>5}  {:>4} {:016x}  {:<28} {}",
            bytes(*size),
            match space.resident_fraction(*id) {
                Some(f) => format!("{:.0}%", f * 100.0),
                None => "-".into(),
            },
            space.prot(*id).to_string(),
            frame.addr(*id),
            clip(frame.label(*id), 28),
            match frame.backing(*id) {
                Some((path, _)) => path,
                None => "",
            }
        );
    }

    let exec: u64 = ids()
        .filter(|id| space.prot(*id).is_exec())
        .map(|id| frame.weight(id))
        .sum();
    let writable: u64 = ids()
        .filter(|id| space.prot(*id).is_write())
        .map(|id| frame.weight(id))
        .sum();
    let anonymous: u64 = ids()
        .filter(|id| frame.backing(*id).is_none())
        .map(|id| frame.weight(id))
        .sum();
    println!(
        "  {} executable, {} writable, {} anonymous",
        bytes(exec),
        bytes(writable),
        bytes(anonymous)
    );
    debug_assert_eq!(frame.kind(frame.root()), NodeKind::Process);
}

fn bytes(n: u64) -> String {
    traveler_core::human_bytes(n)
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max - 1).collect::<String>() + "…"
}

fn fail(message: &str) -> ExitCode {
    eprintln!("error: {message}");
    ExitCode::FAILURE
}

fn usage(message: &str) -> ExitCode {
    if !message.is_empty() {
        eprintln!("error: {message}\n");
    }
    eprintln!("{USAGE}");
    ExitCode::from(2)
}
