//! `traveler-layout <path>` — scans a tree, lays out the city, and reports whether the geometry
//! is sound. With `--svg` it writes a top-down picture of the footprint.

use std::{path::PathBuf, process::ExitCode};

use traveler_layout::{
    CityLayout, CityParams, HeightMode, Layout, LayoutCtx, Placements,
    svg::{self, SvgParams},
};
use traveler_scan::{ExtId, ScanOptions, Tree, scan};

const USAGE: &str = "\
usage: traveler-layout <path> [options]

  --svg <file>    write a top-down SVG of the footprint
  --px N          SVG size in pixels (default 1400)
  --world N       side length of the root footprint in world units (default 1000)
  --spacing N     scale the streets and walls, 1.0 is the default
  --uniform       uniform file thickness instead of log(size)
  --raw           no margins and no gaps, the packing on its own";

fn main() -> ExitCode {
    let mut path: Option<PathBuf> = None;
    let mut svg_out: Option<PathBuf> = None;
    let mut params = CityParams::default();
    let mut svg_params = SvgParams::default();

    let raw: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < raw.len() {
        match raw[i].as_str() {
            "--svg" => {
                i += 1;
                match raw.get(i) {
                    Some(v) => svg_out = Some(PathBuf::from(v)),
                    None => return usage("--svg needs a file"),
                }
            }
            "--px" => {
                i += 1;
                match raw.get(i).and_then(|v| v.parse().ok()) {
                    Some(v) => svg_params.px = v,
                    None => return usage("--px needs a number"),
                }
            }
            "--world" => {
                i += 1;
                match raw.get(i).and_then(|v| v.parse().ok()) {
                    Some(v) => params.world_size = v,
                    None => return usage("--world needs a number"),
                }
            }
            "--spacing" => {
                i += 1;
                match raw.get(i).and_then(|v| v.parse::<f64>().ok()) {
                    Some(f) => {
                        params.margin_ratio *= f;
                        params.gap_ratio *= f;
                    }
                    None => return usage("--spacing needs a number"),
                }
            }
            "--uniform" => params.height = HeightMode::Uniform,
            "--raw" => {
                params.margin_ratio = 0.0;
                params.gap_ratio = 0.0;
            }
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

    let layout = CityLayout::new(params);
    let ctx = LayoutCtx::new(&index);
    let mut placements = Placements::with_capacity(index.len());

    let started = std::time::Instant::now();
    layout.place(&ctx, &mut placements);
    let elapsed = started.elapsed();

    println!("\n{}", index.path(index.root()).display());
    println!(
        "  {} nodes, {} placed, {} collapsed",
        index.len(),
        placements.placed,
        index.len() - placements.placed
    );
    println!(
        "  layout in {:.1} ms  ({:.0} nodes/sec)",
        elapsed.as_secs_f64() * 1000.0,
        index.len() as f64 / elapsed.as_secs_f64().max(1e-9)
    );
    println!(
        "  bounds  x {:.1}..{:.1}   y {:.1}..{:.1}   z {:.1}..{:.1}",
        placements.min.x,
        placements.max.x,
        placements.min.y,
        placements.max.y,
        placements.min.z,
        placements.max.z
    );

    let a = svg::aspect_stats(&index, &placements);
    println!(
        "  file aspect ratio over {} tiles: median {:.2}  p90 {:.2}  p99 {:.2}  worst {:.1}",
        a.count, a.median, a.p90, a.p99, a.worst
    );
    println!(
        "  hairlines worse than 50:1: {} ({:.3}% of tiles)",
        a.slivers,
        a.slivers as f64 / a.count.max(1) as f64 * 100.0
    );

    // Independent check that the packing conserves area, walked here rather than trusted.
    let mut worst_area_err = 0.0f64;
    let mut worst_overflow = 0.0f32;
    for id in index.nodes() {
        if !placements.is_visible(id) || index.child_count(id) == 0 {
            continue;
        }
        let (px0, pz0, px1, pz1) = placements.footprint(id);
        let mut child_area = 0.0f64;
        let mut any = false;
        for c in index.child_ids(id) {
            if !placements.is_visible(c) {
                continue;
            }
            any = true;
            let s = placements.size[c.index()];
            child_area += (s.x as f64) * (s.z as f64);
            let (cx0, cz0, cx1, cz1) = placements.footprint(c);
            worst_overflow = worst_overflow
                .max(px0 - cx0)
                .max(pz0 - cz0)
                .max(cx1 - px1)
                .max(cz1 - pz1);
        }
        if !any {
            continue;
        }
        let parent_area = ((px1 - px0) as f64) * ((pz1 - pz0) as f64);
        if parent_area > 0.0 {
            worst_area_err = worst_area_err.max(1.0 - child_area / parent_area);
        }
    }
    println!(
        "  children fill at least {:.1}% of every parent, and overflow it by at most {:.4} units",
        (1.0 - worst_area_err) * 100.0,
        worst_overflow.max(0.0)
    );

    // Top-level districts and the streets between them, which is the thing you actually read
    // from above.
    let root = index.root();
    let districts: Vec<_> = index
        .child_ids(root)
        .filter(|c| placements.is_visible(*c))
        .collect();
    println!("
  {} top-level districts:", districts.len());
    for d in districts.iter().take(12) {
        let (x0, z0, x1, z1) = placements.footprint(*d);
        println!(
            "    {:<24} {:>6.0} x {:<6.0} units",
            index.name(*d),
            x1 - x0,
            z1 - z0
        );
    }
    let mut streets: Vec<f32> = Vec::new();
    for (i, a) in districts.iter().enumerate() {
        let (ax0, az0, ax1, az1) = placements.footprint(*a);
        for b in &districts[i + 1..] {
            let (bx0, bz0, bx1, bz1) = placements.footprint(*b);
            // Distance along whichever axis they are actually separated on.
            let dx = (bx0 - ax1).max(ax0 - bx1);
            let dz = (bz0 - az1).max(az0 - bz1);
            let overlap_x = ax1.min(bx1) > ax0.max(bx0);
            let overlap_z = az1.min(bz1) > az0.max(bz0);
            if overlap_z && dx > 0.0 {
                streets.push(dx);
            }
            if overlap_x && dz > 0.0 {
                streets.push(dz);
            }
        }
    }
    if !streets.is_empty() {
        streets.sort_by(|a, b| a.partial_cmp(b).unwrap());
        // The narrowest street is whatever runs alongside the smallest district, which is a
        // speck; the widest is the boulevard between the districts you actually see.
        println!(
            "  streets between districts: {:.1} narrowest, {:.1} median, {:.1} widest (units)",
            streets[0],
            streets[streets.len() / 2],
            streets[streets.len() - 1]
        );
    }

    if let Some(out) = svg_out {
        let doc = svg::render(&index, &placements, &svg_params, |id| ext_color(index.ext(id)));
        if let Err(e) = std::fs::write(&out, doc.as_bytes()) {
            eprintln!("error: cannot write {}: {e}", out.display());
            return ExitCode::FAILURE;
        }
        println!(
            "\n  wrote {} ({:.1} MiB)",
            out.display(),
            doc.len() as f64 / (1024.0 * 1024.0)
        );
    }
    println!();

    ExitCode::SUCCESS
}

/// Stable pseudo-random hue per extension. The app has a validated categorical palette; this
/// dump only needs neighbouring extensions to look different.
fn ext_color(ext: ExtId) -> [u8; 3] {
    if ext.0 == 0 {
        return [0x5b, 0x64, 0x70];
    }
    let mut x = (ext.0 as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    x ^= x >> 29;
    x = x.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x ^= x >> 32;
    let hue = (x % 360) as f64;
    let sat = 0.45 + ((x >> 12) % 30) as f64 / 100.0;
    let light = 0.45 + ((x >> 24) % 20) as f64 / 100.0;
    hsl_to_rgb(hue, sat, light)
}

fn hsl_to_rgb(h: f64, s: f64, l: f64) -> [u8; 3] {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let hp = h / 60.0;
    let x = c * (1.0 - (hp % 2.0 - 1.0).abs());
    let (r, g, b) = match hp as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c * 0.5;
    [
        ((r + m) * 255.0).round().clamp(0.0, 255.0) as u8,
        ((g + m) * 255.0).round().clamp(0.0, 255.0) as u8,
        ((b + m) * 255.0).round().clamp(0.0, 255.0) as u8,
    ]
}

fn usage(msg: &str) -> ExitCode {
    if !msg.is_empty() {
        eprintln!("error: {msg}\n");
    }
    eprintln!("{USAGE}");
    ExitCode::from(2)
}
