//! A synthetic profile with the shape of a real one: a layered call graph across a few
//! libraries, a fixed runtime prefix under every thread, phases of different work, and idle
//! waits. Used to stress the timeline at sizes no hand-made recording reaches.

use crate::{Trace, TraceBuilder};

pub struct Params {
    pub threads: usize,
    pub seconds: f64,
    pub hz: f64,
    pub funcs: usize,
    pub seed: u64,
}

impl Default for Params {
    fn default() -> Self {
        Self { threads: 8, seconds: 30.0, hz: 1000.0, funcs: 1500, seed: 7 }
    }
}

pub const MAIN_TRUNK: [(&str, &str); 6] = [
    ("dyld", "start"),
    ("app", "main"),
    ("app", "std::rt::lang_start_internal"),
    ("app", "std::rt::lang_start::{{closure}}"),
    ("app", "std::sys::backtrace::__rust_begin_short_backtrace"),
    ("app", "app::main"),
];

const WORKER_TRUNK: [(&str, &str); 4] = [
    ("libsystem_pthread", "thread_start"),
    ("libsystem_pthread", "_pthread_start"),
    ("app", "std::sys::thread::unix::Thread::new::thread_start"),
    ("app", "core::ops::function::FnOnce::call_once"),
];

const MODULES: [(&str, usize, usize); 7] = [
    ("app", 0, 9),
    ("libsystem_malloc", 6, 12),
    ("libsystem_kernel", 9, 13),
    ("CoreFoundation", 4, 11),
    ("libdispatch", 3, 10),
    ("Metal", 5, 12),
    ("libz", 6, 11),
];

const WORDS: [&str; 24] = [
    "parse", "load", "render", "update", "flush", "decode", "encode", "hash", "insert", "lookup",
    "resolve", "compile", "layout", "draw", "fetch", "write", "read", "sort", "merge", "scan",
    "alloc", "free", "notify", "wait",
];

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
    fn f(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

struct Fn_ {
    id: u32,
    layer: usize,
    callees: Vec<usize>,
}

pub fn generate(p: &Params) -> Trace {
    let mut rng = Rng(p.seed);
    let mut b = TraceBuilder::new();

    let mut funcs: Vec<Fn_> = Vec::with_capacity(p.funcs);
    for i in 0..p.funcs {
        let (m, lo, hi) = MODULES[if i < p.funcs / 3 { 0 } else { 1 + rng.below(MODULES.len() - 1) }];
        let module = b.module(m);
        let layer = lo + rng.below(hi - lo);
        let name = format!("{m}::{}_{}", WORDS[rng.below(WORDS.len())], i);
        funcs.push(Fn_ { id: b.func(module, &name), layer, callees: Vec::new() });
    }
    let by_layer = |l: usize, funcs: &[Fn_]| -> Vec<usize> {
        funcs.iter().enumerate().filter(|(_, f)| f.layer == l).map(|(i, _)| i).collect()
    };
    let layers: Vec<Vec<usize>> = (0..14).map(|l| by_layer(l, &funcs)).collect();
    for i in 0..funcs.len() {
        let l = funcs[i].layer;
        let mut callees = Vec::new();
        for _ in 0..2 + rng.below(4) {
            let target = l + 1 + rng.below(3);
            if let Some(pool) = layers.get(target).filter(|p| !p.is_empty()) {
                callees.push(pool[rng.below(pool.len())]);
            }
        }
        funcs[i].callees = callees;
    }
    let entries = &layers[0];
    let wait = {
        let m = b.module("libsystem_kernel");
        b.func(m, "__psynch_cvwait")
    };

    let trunk_ids = |b: &mut TraceBuilder, t: &[(&str, &str)]| -> Vec<u32> {
        t.iter()
            .map(|&(m, n)| {
                let m = b.module(m);
                b.func(m, n)
            })
            .collect()
    };
    let main_trunk = trunk_ids(&mut b, &MAIN_TRUNK);
    let worker_trunk = trunk_ids(&mut b, &WORKER_TRUNK);

    let dt = 1.0 / p.hz;
    for ti in 0..p.threads {
        let name = if ti == 0 { "main".to_string() } else { format!("worker-{ti}") };
        let th = b.thread(&name, 1000);
        let trunk = if ti == 0 { main_trunk.clone() } else { worker_trunk.clone() };
        let (start, end) = if ti == 0 {
            (0.0, p.seconds)
        } else {
            let s = rng.f() * p.seconds * 0.5;
            let e = if rng.f() < 0.3 { s + rng.f() * (p.seconds - s) } else { p.seconds };
            (s, e)
        };
        let mut t = start;
        let mut stack: Vec<usize> = Vec::new();
        let mut idle = false;
        let mut phase_end = t;
        while t < end {
            if t >= phase_end {
                idle = ti != 0 && rng.f() < 0.35;
                phase_end = t + 0.02 + rng.f().powi(2) * 2.0;
                stack.clear();
                stack.push(entries[rng.below(entries.len())]);
            }
            let mut frames = trunk.clone();
            if idle {
                frames.push(wait);
            } else {
                let r = rng.f();
                let top = *stack.last().unwrap();
                if r < 0.32 && !funcs[top].callees.is_empty() {
                    let c = &funcs[top].callees;
                    stack.push(c[rng.below(c.len())]);
                } else if r < 0.62 && stack.len() > 1 {
                    stack.pop();
                }
                frames.extend(stack.iter().map(|&f| funcs[f].id));
            }
            let leaf = b.path(&frames);
            b.sample(th, t, leaf);
            t += dt * (0.95 + rng.f() * 0.25);
        }
    }
    let mut trace = b.finish();
    trace.name = format!("synthetic run ({} threads)", p.threads);
    trace
}
