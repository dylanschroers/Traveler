use std::time::Instant;

use traveler_trace::{Levels, synth};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let t = Instant::now();
    let trace = match args.as_slice() {
        [path] => traveler_trace::samply::load(path.as_ref()).unwrap_or_else(|e| panic!("{e}")),
        _ => {
            let threads = args.first().map_or(16, |s| s.parse().unwrap());
            let seconds = args.get(1).map_or(60.0, |s| s.parse().unwrap());
            synth::generate(&synth::Params { threads, seconds, ..Default::default() })
        }
    };
    println!(
        "{}\nload    {:>6.2} s   {} threads, {} samples, {} distinct call paths, {} functions",
        trace.name,
        t.elapsed().as_secs_f64(),
        trace.threads.len(),
        trace.samples(),
        trace.paths(),
        trace.funcs.len()
    );

    let t = Instant::now();
    let levels = Levels::build(&trace, 0.001);
    println!("levels  {:>6.2} s   {} levels", t.elapsed().as_secs_f64(), levels.levels.len());
    let mut total = 0;
    for l in &levels.levels {
        let refs: usize = l.buckets.iter().map(|b| b.len()).sum();
        let max_bucket = l.buckets.iter().map(|b| b.len()).max().unwrap_or(0);
        total += l.spans.len() * size_of::<traveler_trace::Span>() + refs * 4;
        println!(
            "  slice {:>9.3} ms  {:>9} spans  {:>8} max per chunk",
            l.slice * 1000.0,
            l.spans.len(),
            max_bucket
        );
    }
    println!("memory  {:.0} MiB for all levels", total as f64 / (1 << 20) as f64);
}
