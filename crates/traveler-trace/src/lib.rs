//! Call-stack samples over time, interned as a call-path tree, and the zoom levels the activity
//! timeline draws from.
//!
//! A span is one call path staying on one thread's stack for a run of consecutive slices. At the
//! finest level a slice is about one sample; each coarser level doubles the slice. Spans are
//! bucketed into fixed time chunks so the renderer can fetch one chunk at one level as a tile.

use std::collections::HashMap;

use rayon::prelude::*;

pub mod describe;
pub mod samply;
pub mod story;
pub mod synth;

pub struct Func {
    pub name: String,
    pub module: u16,
}

pub struct Thread {
    pub name: String,
    pub pid: u32,
    pub start: f64,
    pub end: f64,
    pub times: Vec<f64>,
    pub leaves: Vec<u32>,
    /// Frames at or above this depth are drawn as the strand rather than as tendrils. See
    /// [`TRUNK_KEEP`].
    pub trunk: u16,
}

pub struct Trace {
    pub name: String,
    pub modules: Vec<String>,
    pub funcs: Vec<Func>,
    pub parent: Vec<u32>,
    pub func: Vec<u32>,
    pub depth: Vec<u16>,
    pub threads: Vec<Thread>,
    pub duration: f64,
}

pub const ROOT: u32 = 0;

/// The run of frames on a thread's stack for nearly every sample is its trunk, but the last few of
/// them are usually the thread's actual job (a worker loop, `walk::visit`), not runtime setup.
/// Absorbing all of it hid the work of every real worker thread, so these stay as tendrils.
pub const TRUNK_KEEP: u16 = 3;

impl Trace {
    pub fn paths(&self) -> usize {
        self.parent.len() - 1
    }

    pub fn samples(&self) -> usize {
        self.threads.iter().map(|t| t.times.len()).sum()
    }

    pub fn module_of(&self, node: u32) -> u16 {
        self.funcs[self.func[node as usize] as usize].module
    }

    pub fn name_of(&self, node: u32) -> &str {
        &self.funcs[self.func[node as usize] as usize].name
    }

    /// The stack on `thread` at the sample nearest `t`, outermost frame first, or `None` when
    /// the thread is not alive then.
    pub fn stack_at(&self, thread: usize, t: f64) -> Option<Vec<u32>> {
        let th = &self.threads[thread];
        if th.times.is_empty() || t < th.start || t > th.end {
            return None;
        }
        let i = th.times.partition_point(|&x| x < t).min(th.times.len() - 1);
        let i = if i > 0 && (t - th.times[i - 1]) < (th.times[i] - t) { i - 1 } else { i };
        let mut out = Vec::new();
        let mut n = th.leaves[i];
        while n != ROOT {
            out.push(n);
            n = self.parent[n as usize];
        }
        out.reverse();
        Some(out)
    }
}

#[derive(Default)]
pub struct TraceBuilder {
    modules: Vec<String>,
    module_ids: HashMap<String, u16>,
    funcs: Vec<Func>,
    func_ids: HashMap<(u16, String), u32>,
    parent: Vec<u32>,
    func: Vec<u32>,
    depth: Vec<u16>,
    children: HashMap<(u32, u32), u32>,
    threads: Vec<Thread>,
}

impl TraceBuilder {
    pub fn new() -> Self {
        Self { parent: vec![ROOT], func: vec![u32::MAX], depth: vec![0], ..Default::default() }
    }

    pub fn module(&mut self, name: &str) -> u16 {
        if let Some(&id) = self.module_ids.get(name) {
            return id;
        }
        let id = self.modules.len() as u16;
        self.modules.push(name.to_string());
        self.module_ids.insert(name.to_string(), id);
        id
    }

    pub fn func(&mut self, module: u16, name: &str) -> u32 {
        if let Some(&id) = self.func_ids.get(&(module, name.to_string())) {
            return id;
        }
        let id = self.funcs.len() as u32;
        self.funcs.push(Func { name: name.to_string(), module });
        self.func_ids.insert((module, name.to_string()), id);
        id
    }

    pub fn thread(&mut self, name: &str, pid: u32) -> usize {
        self.threads.push(Thread {
            name: name.to_string(),
            pid,
            start: f64::MAX,
            end: f64::MIN,
            times: Vec::new(),
            leaves: Vec::new(),
            trunk: 0,
        });
        self.threads.len() - 1
    }

    pub fn path(&mut self, frames: &[u32]) -> u32 {
        frames.iter().fold(ROOT, |node, &f| self.child(node, f))
    }

    pub fn child(&mut self, node: u32, f: u32) -> u32 {
        if let Some(&c) = self.children.get(&(node, f)) {
            return c;
        }
        let c = self.parent.len() as u32;
        self.parent.push(node);
        self.func.push(f);
        self.depth.push(self.depth[node as usize] + 1);
        self.children.insert((node, f), c);
        c
    }

    /// Samples must arrive in time order per thread.
    pub fn sample(&mut self, thread: usize, time: f64, leaf: u32) {
        let th = &mut self.threads[thread];
        th.start = th.start.min(time);
        th.end = th.end.max(time);
        th.times.push(time);
        th.leaves.push(leaf);
    }

    pub fn finish(mut self) -> Trace {
        self.threads.retain(|t| !t.times.is_empty());
        let t0 = self.threads.iter().map(|t| t.start).fold(f64::MAX, f64::min);
        let t0 = if t0.is_finite() { t0 } else { 0.0 };
        let mut duration: f64 = 0.0;
        for th in &mut self.threads {
            th.times.iter_mut().for_each(|t| *t -= t0);
            th.start -= t0;
            th.end -= t0;
            duration = duration.max(th.end);
        }
        let mut trace = Trace {
            name: String::new(),
            modules: self.modules,
            funcs: self.funcs,
            parent: self.parent,
            func: self.func,
            depth: self.depth,
            threads: self.threads,
            duration: duration.max(1e-3),
        };
        let trunks: Vec<u16> = (0..trace.threads.len()).map(|i| trunk_of(&trace, i)).collect();
        for (th, t) in trace.threads.iter_mut().zip(trunks) {
            th.trunk = t;
        }
        trace
    }
}

fn trunk_of(trace: &Trace, thread: usize) -> u16 {
    let th = &trace.threads[thread];
    let mut counts: HashMap<u32, u32> = HashMap::new();
    for &leaf in &th.leaves {
        let mut n = leaf;
        while n != ROOT {
            *counts.entry(n).or_default() += 1;
            n = trace.parent[n as usize];
        }
    }
    let need = (th.leaves.len() as f64 * 0.98).ceil() as u32;
    let mut node = ROOT;
    loop {
        let next = counts
            .iter()
            .filter(|&(&n, &c)| trace.parent[n as usize] == node && c >= need)
            .map(|(&n, _)| n)
            .next();
        match next {
            Some(n) => node = n,
            None => return trace.depth[node as usize].saturating_sub(TRUNK_KEEP),
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Span {
    pub thread: u32,
    pub node: u32,
    pub t0: f32,
    pub t1: f32,
    /// Mean fraction of each slice's samples that had this path on the stack, 0..=1.
    pub weight: f32,
}

/// One zoom level. Its tiles are `chunk` long and each covers exactly two tiles of the level
/// below, so the renderer can refine a tile into its halves like a map.
pub struct Level {
    pub slice: f64,
    pub chunk: f64,
    pub spans: Vec<Span>,
    /// Span indices per tile; a span is listed in every tile it overlaps.
    pub buckets: Vec<Vec<u32>>,
}

pub struct Levels {
    pub levels: Vec<Level>,
}

/// Slices per tile. Keeps a long span listed in a handful of tiles at any level.
pub const SLICES_PER_TILE: f64 = 64.0;

impl Levels {
    pub fn build(trace: &Trace, base: f64) -> Levels {
        let mut slices = Vec::new();
        let mut s = base;
        while s < trace.duration / 4.0 || slices.is_empty() {
            slices.push(s);
            s *= 2.0;
        }
        let levels = slices
            .par_iter()
            .map(|&slice| {
                let mut spans: Vec<Span> = (0..trace.threads.len())
                    .into_par_iter()
                    .flat_map_iter(|i| spans_for(trace, i, slice))
                    .filter(|sp| slice == base || sp.weight as f64 * (sp.t1 - sp.t0) as f64 >= 0.5 * slice)
                    .collect();
                spans.sort_unstable_by(|a, b| a.t0.total_cmp(&b.t0));
                let chunk = slice * SLICES_PER_TILE;
                let tiles = ((trace.duration / chunk).ceil() as usize).max(1);
                let mut buckets = vec![Vec::new(); tiles];
                for (i, sp) in spans.iter().enumerate() {
                    let c0 = ((sp.t0 as f64 / chunk) as usize).min(tiles - 1);
                    let c1 = (((sp.t1 as f64 - 1e-9) / chunk) as usize).min(tiles - 1);
                    for b in &mut buckets[c0..=c1.max(c0)] {
                        b.push(i as u32);
                    }
                }
                Level { slice, chunk, spans, buckets }
            })
            .collect();
        Levels { levels }
    }
}

struct Open {
    first: u64,
    last: u64,
    sum: f32,
}

/// A run survives a gap of one empty slice at any level.
const GAP: u64 = 2;

/// Gaps shorter than this are bridged. A sampler that looks every millisecond cannot tell a
/// function that returned and was called again from one it just happened to miss, so drawing
/// those gaps showed noise as if it were behaviour.
pub const BRIDGE_SECONDS: f64 = 0.005;

fn spans_for(trace: &Trace, thread: usize, slice: f64) -> Vec<Span> {
    let th = &trace.threads[thread];
    let mut out = Vec::new();
    let mut open: HashMap<u32, Open> = HashMap::new();
    let mut cur: HashMap<u32, u32> = HashMap::new();
    let mut cur_k = u64::MAX;
    let mut cur_n = 0u32;
    let gap = ((BRIDGE_SECONDS / slice).ceil() as u64).max(GAP);

    let emit = |out: &mut Vec<Span>, node: u32, o: &Open| {
        let n = (o.last - o.first + 1) as f32;
        out.push(Span {
            thread: thread as u32,
            node,
            t0: (o.first as f64 * slice) as f32,
            t1: ((o.last + 1) as f64 * slice) as f32,
            weight: o.sum / n,
        });
    };
    let mut flush = |out: &mut Vec<Span>, k: u64, cur: &mut HashMap<u32, u32>, n: u32| {
        for (&node, &c) in cur.iter() {
            let frac = c as f32 / n as f32;
            match open.get_mut(&node) {
                Some(o) if k - o.last <= gap => {
                    o.last = k;
                    o.sum += frac;
                }
                Some(o) => {
                    emit(out, node, o);
                    *o = Open { first: k, last: k, sum: frac };
                }
                None => {
                    open.insert(node, Open { first: k, last: k, sum: frac });
                }
            }
        }
        cur.clear();
        if k % 1024 == 0 {
            open.retain(|&node, o| {
                if k - o.last > gap {
                    emit(out, node, o);
                    false
                } else {
                    true
                }
            });
        }
    };

    for (&t, &leaf) in th.times.iter().zip(&th.leaves) {
        let k = (t / slice) as u64;
        if k != cur_k && cur_k != u64::MAX {
            flush(&mut out, cur_k, &mut cur, cur_n);
            cur_n = 0;
        }
        cur_k = k;
        cur_n += 1;
        let mut n = leaf;
        while n != ROOT && trace.depth[n as usize] > th.trunk {
            *cur.entry(n).or_default() += 1;
            n = trace.parent[n as usize];
        }
    }
    if cur_k != u64::MAX {
        flush(&mut out, cur_k, &mut cur, cur_n);
    }
    for (node, o) in &open {
        emit(&mut out, *node, o);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn small() -> Trace {
        synth::generate(&synth::Params { threads: 4, seconds: 3.0, ..Default::default() })
    }

    #[test]
    fn span_weight_conserves_activity_at_the_finest_level() {
        let trace = small();
        let levels = Levels::build(&trace, 0.001);
        for level in &levels.levels[..1] {
            for (ti, th) in trace.threads.iter().enumerate() {
                let mut per_slice: HashMap<u64, (u32, HashMap<u32, u32>)> = HashMap::new();
                for (&t, &leaf) in th.times.iter().zip(&th.leaves) {
                    let e = per_slice.entry((t / level.slice) as u64).or_default();
                    e.0 += 1;
                    let mut n = leaf;
                    while n != ROOT && trace.depth[n as usize] > th.trunk {
                        *e.1.entry(n).or_default() += 1;
                        n = trace.parent[n as usize];
                    }
                }
                let mut expect: HashMap<u32, f64> = HashMap::new();
                for (n, counts) in per_slice.values() {
                    for (&node, &c) in counts {
                        *expect.entry(node).or_default() += c as f64 / *n as f64;
                    }
                }
                let mut got: HashMap<u32, f64> = HashMap::new();
                for sp in level.spans.iter().filter(|s| s.thread as usize == ti) {
                    let slices = ((sp.t1 - sp.t0) as f64 / level.slice).round();
                    *got.entry(sp.node).or_default() += sp.weight as f64 * slices;
                }
                for (node, e) in &expect {
                    let g = got.get(node).copied().unwrap_or(0.0);
                    assert!(
                        (g - e).abs() < 1e-3 * e.max(1.0),
                        "slice {} thread {ti} node {node}: {g} vs {e}",
                        level.slice
                    );
                }
            }
        }
    }

    #[test]
    fn coarse_levels_keep_only_spans_worth_half_a_slice() {
        let trace = small();
        let levels = Levels::build(&trace, 0.001);
        for level in &levels.levels[1..] {
            for sp in &level.spans {
                assert!(sp.weight as f64 * (sp.t1 - sp.t0) as f64 >= 0.5 * level.slice - 1e-6);
            }
        }
    }

    #[test]
    fn each_tile_is_exactly_two_tiles_of_the_level_below() {
        let trace = small();
        let levels = Levels::build(&trace, 0.001);
        for w in levels.levels.windows(2) {
            assert!((w[1].chunk - 2.0 * w[0].chunk).abs() < 1e-12);
        }
    }

    #[test]
    fn every_span_is_in_every_tile_it_overlaps() {
        let trace = small();
        let levels = Levels::build(&trace, 0.001);
        for level in &levels.levels {
            for (i, sp) in level.spans.iter().enumerate() {
                for (c, bucket) in level.buckets.iter().enumerate() {
                    let (a, b) = (c as f64 * level.chunk, (c + 1) as f64 * level.chunk);
                    if (sp.t0 as f64) < b && (sp.t1 as f64) > a + 1e-6 {
                        assert!(bucket.contains(&(i as u32)), "span {i} missing from chunk {c}");
                    }
                }
            }
        }
    }

    #[test]
    fn coarser_levels_have_fewer_spans() {
        let trace = small();
        let levels = Levels::build(&trace, 0.001);
        for w in levels.levels.windows(2) {
            assert!(w[1].spans.len() <= w[0].spans.len());
        }
    }

    #[test]
    fn the_trunk_is_the_part_every_sample_shares() {
        let trace = small();
        assert_eq!(trace.threads[0].trunk, synth::MAIN_TRUNK.len() as u16 - TRUNK_KEEP);
        let stack = trace.stack_at(0, 1.0).unwrap();
        assert_eq!(trace.name_of(stack[0]), synth::MAIN_TRUNK[0].1);
    }
}
