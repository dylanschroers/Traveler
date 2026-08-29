//! `traveler-scan <path>` — walks a tree and prints enough to prove the index is telling the truth.
//!
//! The total it prints must match, to the byte:
//!   (Get-ChildItem -Path <path> -Recurse -File -Force | Measure-Object -Sum Length).Sum

use std::{cmp::Reverse, path::PathBuf, process::ExitCode};

use traveler_scan::{ExtId, NodeId, ScanOptions, Tree, human_bytes, scan, thousands};

struct Args {
    path: PathBuf,
    top: usize,
    errors: usize,
    opts: ScanOptions,
}

fn parse_args() -> Result<Args, String> {
    let mut args = Args {
        path: PathBuf::new(),
        top: 15,
        errors: 5,
        opts: ScanOptions::default(),
    };
    let raw: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    let mut got_path = false;
    while i < raw.len() {
        match raw[i].as_str() {
            "--threads" => {
                i += 1;
                args.opts.threads = raw
                    .get(i)
                    .and_then(|v| v.parse().ok())
                    .ok_or("--threads needs a number")?;
            }
            "--top" => {
                i += 1;
                args.top = raw
                    .get(i)
                    .and_then(|v| v.parse().ok())
                    .ok_or("--top needs a number")?;
            }
            "--errors" => {
                i += 1;
                args.errors = raw
                    .get(i)
                    .and_then(|v| v.parse().ok())
                    .ok_or("--errors needs a number")?;
            }
            "--no-metadata" => args.opts.skip_metadata = true,
            "-h" | "--help" => return Err("help".into()),
            other if other.starts_with('-') => return Err(format!("unknown flag: {other}")),
            other => {
                args.path = PathBuf::from(other);
                got_path = true;
            }
        }
        i += 1;
    }
    if !got_path {
        return Err("help".into());
    }
    Ok(args)
}

const USAGE: &str = "\
usage: traveler-scan <path> [options]

  --threads N     worker threads (default: one per core)
  --top N         how many extensions and large files to list (default 15)
  --errors N      how many scan errors to show (default 5)
  --no-metadata   diagnostic: skip per-entry metadata(), which zeroes every size.
                  Only useful for measuring what metadata costs on this platform.";

fn main() -> ExitCode {
    let args = match parse_args() {
        Ok(a) => a,
        Err(e) => {
            if e != "help" {
                eprintln!("error: {e}\n");
            }
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };

    let index = match scan(&args.path, &args.opts) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("error: cannot scan {}: {e}", args.path.display());
            return ExitCode::FAILURE;
        }
    };

    let s = index.stats;
    let secs = s.walk_millis as f64 / 1000.0;
    let entries = s.files + s.dirs;

    println!("\n{}", index.path(index.root()).display());
    println!(
        "  {:>14}  files\n  {:>14}  directories\n  {:>14}  nodes in index",
        thousands(s.files),
        thousands(s.dirs),
        thousands(index.len() as u64)
    );
    println!("  {:>14}  bytes ({})", thousands(s.bytes), human_bytes(s.bytes));
    println!(
        "  {:>14}  seconds  ({} entries/sec)",
        format!("{secs:.3}"),
        thousands(if secs > 0.0 {
            (entries as f64 / secs) as u64
        } else {
            0
        })
    );
    if args.opts.skip_metadata {
        println!("  NOTE: --no-metadata was set, every size above is meaningless.");
    }
    if s.reparse_points > 0 {
        println!(
            "  {:>14}  reparse points recorded, not descended into",
            thousands(s.reparse_points)
        );
    }
    if s.lossy_names > 0 {
        println!(
            "  {:>14}  names required lossy UTF-8 conversion",
            thousands(s.lossy_names)
        );
    }

    if !index.errors.is_empty() {
        println!("\nerrors: {}", thousands(index.errors.len() as u64));
        for e in index.errors.iter().take(args.errors) {
            println!("  {}: {}", e.path, e.message);
        }
        if index.errors.len() > args.errors {
            println!("  ... and {} more", index.errors.len() - args.errors);
        }
    }

    // Bytes and counts per extension.
    let mut by_ext: Vec<(u64, u64)> = vec![(0, 0); index.ext_count()];
    let mut largest: Vec<(u64, NodeId)> = Vec::new();
    let mut max_depth = 0u16;
    for id in index.nodes() {
        max_depth = max_depth.max(index.depth(id));
        if index.flags(id).is_dir() {
            continue;
        }
        let e = &mut by_ext[index.ext(id).0 as usize];
        e.0 += index.size(id);
        e.1 += 1;
        largest.push((index.size(id), id));
    }

    println!("\ntop {} extensions by bytes:", args.top);
    let mut ranked: Vec<(usize, (u64, u64))> = by_ext.into_iter().enumerate().collect();
    ranked.sort_unstable_by_key(|r| Reverse(r.1.0));
    for (ext, (bytes, count)) in ranked.iter().take(args.top) {
        let name = index.ext_str(ExtId(*ext as u32));
        let name = if name.is_empty() { "(none)" } else { name };
        println!(
            "  {:<12} {:>10}  {:>12} files  {:>5.1}%",
            name,
            human_bytes(*bytes),
            thousands(*count),
            if s.bytes > 0 {
                *bytes as f64 / s.bytes as f64 * 100.0
            } else {
                0.0
            }
        );
    }
    println!(
        "  ({} distinct extensions)",
        thousands(index.ext_count() as u64)
    );

    println!("\ntop {} largest files:", args.top);
    largest.sort_unstable_by_key(|l| Reverse(l.0));
    for (bytes, id) in largest.iter().take(args.top) {
        println!("  {:>10}  {}", human_bytes(*bytes), index.path(*id).display());
    }

    println!("\nmax depth: {max_depth}");
    println!(
        "verify with:\n  (Get-ChildItem -Path '{}' -Recurse -File -Force -ErrorAction SilentlyContinue | Measure-Object -Sum Length).Sum",
        args.path.display()
    );
    println!("  expected: {}\n", s.bytes);

    ExitCode::SUCCESS
}


