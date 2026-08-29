//! `traveler-edges <path>` — runs every provider and prints what it found.

use std::{path::PathBuf, process::ExitCode};

use traveler_edges::{
    EdgeKind, EdgeProvider, EdgeSet, ExactDuplicates, ExtensionGroups, NameFamilies,
};
use traveler_scan::{ScanOptions, Tree, human_bytes, scan, thousands};

const USAGE: &str = "\
usage: traveler-edges <path> [options]

  --top N       groups to list per provider (default 8)
  --no-dupes    skip the duplicate finder, which is the only one that reads file contents
  --prefix N    screening prefix in KiB for the duplicate finder (default 64)
  --threads N   reading threads for the duplicate finder (default 4)
  --min-size N  ignore duplicates smaller than N bytes (default 4096)";

fn main() -> ExitCode {
    let mut path: Option<PathBuf> = None;
    let mut top = 8usize;
    let mut dupes = true;
    let mut prefix_kib = 64u64;
    let mut threads = 4usize;
    let mut min_size = 4096u64;

    let raw: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < raw.len() {
        match raw[i].as_str() {
            "--top" => {
                i += 1;
                match raw.get(i).and_then(|v| v.parse().ok()) {
                    Some(v) => top = v,
                    None => return usage("--top needs a number"),
                }
            }
            "--prefix" => {
                i += 1;
                match raw.get(i).and_then(|v| v.parse().ok()) {
                    Some(v) => prefix_kib = v,
                    None => return usage("--prefix needs a number"),
                }
            }
            "--threads" => {
                i += 1;
                match raw.get(i).and_then(|v| v.parse().ok()) {
                    Some(v) => threads = v,
                    None => return usage("--threads needs a number"),
                }
            }
            "--min-size" => {
                i += 1;
                match raw.get(i).and_then(|v| v.parse().ok()) {
                    Some(v) => min_size = v,
                    None => return usage("--min-size needs a number"),
                }
            }
            "--no-dupes" => dupes = false,
            "-h" | "--help" => return usage(""),
            other if other.starts_with('-') => return usage(&format!("unknown flag: {other}")),
            other => path = Some(PathBuf::from(other)),
        }
        i += 1;
    }
    let Some(path) = path else {
        return usage("");
    };

    let index = match scan(&path, &ScanOptions::default()) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("error: cannot scan {}: {e}", path.display());
            return ExitCode::FAILURE;
        }
    };
    println!(
        "\n{}\n  {} files, {}",
        index.path(index.root()).display(),
        thousands(index.stats.files),
        human_bytes(index.stats.bytes)
    );

    let mut providers: Vec<Box<dyn EdgeProvider>> =
        vec![Box::new(ExtensionGroups), Box::new(NameFamilies)];
    if dupes {
        providers.push(Box::new(ExactDuplicates {
            prefix_bytes: prefix_kib * 1024,
            min_size,
            threads,
        }));
    }

    for provider in providers {
        let set = provider.compute(&index);
        report(&set, top);
    }
    println!();
    ExitCode::SUCCESS
}

fn report(set: &EdgeSet, top: usize) {
    println!(
        "\n  {}: {} groups over {} files, {}{}  [{} ms]",
        set.kind.label(),
        thousands(set.groups.len() as u64),
        thousands(set.members() as u64),
        human_bytes(set.total_bytes()),
        if set.kind == EdgeKind::Duplicate {
            format!(", {} redundant", human_bytes(set.total_redundant()))
        } else {
            String::new()
        },
        set.stats.millis
    );

    for g in set.groups.iter().take(top) {
        println!(
            "    {:<52} {:>4} files  {:>10}",
            truncate(&g.label, 52),
            thousands(g.members.len() as u64),
            human_bytes(g.bytes)
        );
    }
    if set.groups.len() > top {
        println!("    ... and {} more", thousands((set.groups.len() - top) as u64));
    }
    if set.stats.files_hashed > 0 {
        println!(
            "    read {} files, {}{}",
            thousands(set.stats.files_hashed),
            human_bytes(set.stats.bytes_hashed),
            if set.stats.read_errors > 0 {
                format!(", {} unreadable", set.stats.read_errors)
            } else {
                String::new()
            }
        );
        if set.stats.changed_while_reading > 0 {
            println!(
                "    {} files changed length while being read and were dropped",
                thousands(set.stats.changed_while_reading)
            );
        }
        println!(
            "    skipped {} candidates under the size floor",
            thousands(set.stats.skipped_small)
        );
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let keep: String = s.chars().take(max.saturating_sub(2)).collect();
    format!("{keep}..")
}

fn usage(msg: &str) -> ExitCode {
    if !msg.is_empty() {
        eprintln!("error: {msg}\n");
    }
    eprintln!("{USAGE}");
    ExitCode::from(2)
}
