//! The run told as a few phases, and a thread told in a sentence or two.
//!
//! A phase is a stretch where the same job dominates and about the same number of threads are
//! busy. The job of a sample is the outermost function of the program's own on its stack, which
//! is what a person would call the task ("generate", "build"), not whatever leaf it was in.

use std::collections::HashMap;

use crate::describe::{Describer, Kind};
use crate::Trace;

const BINS: usize = 120;
const MIN_PHASE_SHARE: f64 = 0.04;

pub struct Phase {
    pub start: f64,
    pub end: f64,
    /// Mean number of threads doing something other than waiting.
    pub busy: f32,
    /// Jobs by share of the phase's busy samples, largest first.
    pub tasks: Vec<(String, f32)>,
    /// What the busy samples were doing, by share, largest first.
    pub kinds: Vec<(Kind, f32)>,
}

pub struct Summary {
    pub busy: f32,
    pub tasks: Vec<(String, f32)>,
    pub kinds: Vec<(Kind, f32)>,
}

fn ranked<K: std::hash::Hash + Eq>(counts: HashMap<K, usize>, total: usize) -> Vec<(K, f32)> {
    let mut v: Vec<(K, f32)> = counts.into_iter().map(|(k, c)| (k, c as f32 / total.max(1) as f32)).collect();
    v.sort_by(|a, b| b.1.total_cmp(&a.1));
    v
}

fn task_name(trace: &Trace, d: &Describer, leaf: u32) -> String {
    d.job(trace, leaf)
}

/// How one thread spent its time.
pub fn thread_summary(trace: &Trace, d: &Describer, thread: usize) -> Summary {
    let th = &trace.threads[thread];
    let mut kinds: HashMap<Kind, usize> = HashMap::new();
    let mut tasks: HashMap<String, usize> = HashMap::new();
    let mut busy = 0;
    for &leaf in &th.leaves {
        let k = d.doing(trace, leaf);
        *kinds.entry(k).or_default() += 1;
        if !k.is_idle() {
            busy += 1;
            *tasks.entry(task_name(trace, d, leaf)).or_default() += 1;
        }
    }
    Summary {
        busy: busy as f32 / th.leaves.len().max(1) as f32,
        tasks: ranked(tasks, busy),
        kinds: ranked(kinds, th.leaves.len()),
    }
}

struct Bin {
    busy_threads: f32,
    top: Option<String>,
    tasks: HashMap<String, usize>,
    kinds: HashMap<Kind, usize>,
    busy_samples: usize,
}

pub fn phases(trace: &Trace, d: &Describer) -> Vec<Phase> {
    let width = (trace.duration / BINS as f64).max(0.005);
    let n = (trace.duration / width).ceil() as usize;
    let mut bins: Vec<Bin> = (0..n.max(1))
        .map(|_| Bin { busy_threads: 0.0, top: None, tasks: HashMap::new(), kinds: HashMap::new(), busy_samples: 0 })
        .collect();
    for th in &trace.threads {
        let mut per_bin = vec![(0usize, 0usize); bins.len()];
        for (&t, &leaf) in th.times.iter().zip(&th.leaves) {
            let b = ((t / width) as usize).min(bins.len() - 1);
            per_bin[b].0 += 1;
            let k = d.doing(trace, leaf);
            if !k.is_idle() {
                per_bin[b].1 += 1;
                let bin = &mut bins[b];
                *bin.kinds.entry(k).or_default() += 1;
                *bin.tasks.entry(task_name(trace, d, leaf)).or_default() += 1;
                bin.busy_samples += 1;
            }
        }
        for (b, &(all, busy)) in per_bin.iter().enumerate() {
            if all > 0 {
                bins[b].busy_threads += busy as f32 / all as f32;
            }
        }
    }
    for bin in &mut bins {
        bin.top = bin.tasks.iter().max_by_key(|(_, c)| **c).map(|(k, _)| k.clone());
    }

    let mut groups: Vec<(usize, usize)> = Vec::new();
    for (i, bin) in bins.iter().enumerate() {
        match groups.last_mut() {
            Some((s, e)) if {
                let prev = &bins[*s];
                let close = (bin.busy_threads - prev.busy_threads).abs() <= (prev.busy_threads * 0.35).max(1.0);
                bin.top == prev.top && close
            } =>
            {
                *e = i
            }
            _ => groups.push((i, i)),
        }
    }
    let min_bins = ((BINS as f64 * MIN_PHASE_SHARE) as usize).max(1);
    let mut merged: Vec<(usize, usize)> = Vec::new();
    for g in groups {
        match merged.last_mut() {
            Some(last) if g.1 - g.0 + 1 < min_bins => last.1 = g.1,
            Some(last) if last.1 - last.0 + 1 < min_bins => last.1 = g.1,
            _ => merged.push(g),
        }
    }

    struct Acc {
        s: usize,
        e: usize,
        tasks: HashMap<String, usize>,
        kinds: HashMap<Kind, usize>,
        busy: f32,
        samples: usize,
    }
    impl Acc {
        fn top(&self) -> Option<&String> {
            self.tasks.iter().max_by_key(|(_, c)| **c).map(|(k, _)| k)
        }
        fn mean_busy(&self) -> f32 {
            self.busy / (self.e - self.s + 1) as f32
        }
    }
    let mut accs: Vec<Acc> = Vec::new();
    for (s, e) in merged {
        let mut acc = Acc { s, e, tasks: HashMap::new(), kinds: HashMap::new(), busy: 0.0, samples: 0 };
        for bin in &bins[s..=e] {
            for (k, c) in &bin.tasks {
                *acc.tasks.entry(k.clone()).or_default() += c;
            }
            for (k, c) in &bin.kinds {
                *acc.kinds.entry(*k).or_default() += c;
            }
            acc.busy += bin.busy_threads;
            acc.samples += bin.busy_samples;
        }
        // Neighbours with the same main job and about the same number of busy threads are one
        // phase told twice; bins alone split them on noise.
        if let Some(prev) = accs.last_mut()
            && prev.top() == acc.top()
            && (prev.mean_busy() - acc.mean_busy()).abs() <= (prev.mean_busy() * 0.35).max(1.0)
        {
            prev.e = acc.e;
            for (k, c) in acc.tasks {
                *prev.tasks.entry(k).or_default() += c;
            }
            for (k, c) in acc.kinds {
                *prev.kinds.entry(k).or_default() += c;
            }
            prev.busy += acc.busy;
            prev.samples += acc.samples;
            continue;
        }
        accs.push(acc);
    }
    accs.into_iter()
        .map(|a| Phase {
            start: a.s as f64 * width,
            end: ((a.e + 1) as f64 * width).min(trace.duration),
            busy: a.busy / (a.e - a.s + 1) as f32,
            tasks: ranked(a.tasks, a.samples),
            kinds: ranked(a.kinds, a.samples),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TraceBuilder;

    /// One thread works alone on "load", then four threads share "crunch".
    fn two_phase() -> Trace {
        let mut b = TraceBuilder::new();
        let app = b.module("app");
        let kernel = b.module("libsystem_kernel.dylib");
        let main = b.func(app, "app::main");
        let load = b.func(app, "app::load");
        let crunch = b.func(app, "app::crunch");
        let wait = b.func(kernel, "__psynch_cvwait");
        let start = b.func(app, "std::rt::lang_start");
        let threads: Vec<usize> = (0..4).map(|i| b.thread(&format!("t{i}"), 1)).collect();
        for k in 0..2000 {
            let t = k as f64 * 0.001;
            for (i, &th) in threads.iter().enumerate() {
                let frames: Vec<u32> = if t < 1.0 {
                    if i == 0 { vec![start, main, load] } else { vec![start, wait] }
                } else if i == 0 {
                    vec![start, main, wait]
                } else {
                    vec![start, crunch]
                };
                let leaf = b.path(&frames);
                b.sample(th, t, leaf);
            }
        }
        let mut tr = b.finish();
        tr.name = "app".into();
        tr
    }

    #[test]
    fn a_change_of_job_and_thread_count_is_a_new_phase() {
        let tr = two_phase();
        let d = Describer::new(&tr);
        let ph = phases(&tr, &d);
        assert_eq!(ph.len(), 2, "phases: {:?}", ph.iter().map(|p| (p.start, p.end, &p.tasks)).collect::<Vec<_>>());
        assert_eq!(ph[0].tasks[0].0, "app::load");
        assert!((ph[0].busy - 1.0).abs() < 0.2);
        assert_eq!(ph[1].tasks[0].0, "app::crunch");
        assert!((ph[1].busy - 3.0).abs() < 0.3);
        assert!((ph[1].start - 1.0).abs() < 0.05);
    }

    #[test]
    fn a_thread_summary_separates_waiting_from_work() {
        let tr = two_phase();
        let d = Describer::new(&tr);
        let s = thread_summary(&tr, &d, 0);
        assert!((s.busy - 0.5).abs() < 0.02);
        assert_eq!(s.tasks[0].0, "app::load");
        let waiting = s.kinds.iter().find(|(k, _)| *k == Kind::Waiting).map_or(0.0, |k| k.1);
        assert!((waiting - 0.5).abs() < 0.02);
    }
}
