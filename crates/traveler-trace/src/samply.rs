//! Reads a samply recording: the Firefox Profiler's processed format (`.json` or `.json.gz`),
//! plus the `.syms.json` sidecar that `--unstable-presymbolicate` writes beside it.
//!
//! Without the sidecar every function is a library-relative address, which still draws, just
//! without names.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::{Origin, ROOT, Trace, TraceBuilder};

#[derive(Deserialize)]
struct Profile {
    meta: Meta,
    libs: Vec<Lib>,
    threads: Vec<RawThread>,
}

#[derive(Deserialize)]
struct Meta {
    interval: f64,
    product: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Lib {
    path: Option<String>,
    debug_name: String,
    code_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawThread {
    name: String,
    pid: serde_json::Value,
    is_main_thread: bool,
    process_name: Option<String>,
    register_time: f64,
    samples: Samples,
    stack_table: StackTable,
    frame_table: FrameTable,
    func_table: FuncTable,
    resource_table: ResourceTable,
    string_array: Vec<String>,
}

#[derive(Deserialize)]
struct Samples {
    stack: Vec<Option<usize>>,
    time: Vec<f64>,
    weight: Option<Vec<u32>>,
}

#[derive(Deserialize)]
struct StackTable {
    prefix: Vec<Option<usize>>,
    frame: Vec<usize>,
}

#[derive(Deserialize)]
struct FrameTable {
    address: Vec<i64>,
    func: Vec<usize>,
}

#[derive(Deserialize)]
struct FuncTable {
    name: Vec<usize>,
    resource: Vec<Option<i64>>,
}

#[derive(Deserialize)]
struct ResourceTable {
    lib: Vec<Option<usize>>,
    name: Vec<usize>,
}

#[derive(Deserialize)]
struct Syms {
    string_table: Vec<String>,
    data: Vec<SymLib>,
}

#[derive(Deserialize)]
struct SymLib {
    debug_name: String,
    code_id: Option<String>,
    symbol_table: Vec<SymEntry>,
    known_addresses: Vec<(u64, usize)>,
}

#[derive(Deserialize, Clone, Copy)]
struct SymEntry {
    rva: u64,
    size: u64,
    symbol: usize,
}

struct LibSyms {
    sorted: Vec<SymEntry>,
    known: HashMap<u64, SymEntry>,
}

impl LibSyms {
    fn lookup(&self, addr: u64) -> Option<SymEntry> {
        if let Some(e) = self.known.get(&addr) {
            return Some(*e);
        }
        let i = self.sorted.partition_point(|e| e.rva <= addr);
        let e = self.sorted.get(i.checked_sub(1)?)?;
        (addr < e.rva + e.size.max(1)).then_some(*e)
    }
}

/// `scan.json.gz` -> `scan.json.syms.json`, which is where samply puts it.
pub fn sidecar_of(path: &Path) -> PathBuf {
    let s = path.to_string_lossy();
    let base = s.strip_suffix(".gz").unwrap_or(&s);
    PathBuf::from(format!("{base}.syms.json"))
}

fn open(path: &Path) -> Result<Box<dyn Read>, String> {
    let f = File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let r = BufReader::new(f);
    Ok(if path.extension().is_some_and(|e| e == "gz") { Box::new(flate2::read::GzDecoder::new(r)) } else { Box::new(r) })
}

pub fn load(path: &Path) -> Result<Trace, String> {
    let profile: Profile =
        serde_json::from_reader(open(path)?).map_err(|e| format!("{}: not a samply recording: {e}", path.display()))?;
    let sidecar = sidecar_of(path);
    let syms: Option<Syms> = match File::open(&sidecar) {
        Ok(f) => Some(serde_json::from_reader(BufReader::new(f)).map_err(|e| format!("{}: {e}", sidecar.display()))?),
        Err(_) => None,
    };

    let mut lib_syms: Vec<Option<LibSyms>> = profile.libs.iter().map(|_| None).collect();
    if let Some(syms) = &syms {
        for d in &syms.data {
            let matches = |lib: &Lib| match (&lib.code_id, &d.code_id) {
                (Some(a), Some(b)) => a.eq_ignore_ascii_case(b),
                _ => lib.debug_name == d.debug_name,
            };
            if let Some(i) = profile.libs.iter().position(matches) {
                let mut sorted = d.symbol_table.clone();
                sorted.sort_unstable_by_key(|e| e.rva);
                let known = d.known_addresses.iter().filter_map(|&(a, k)| d.symbol_table.get(k).map(|&e| (a, e))).collect();
                lib_syms[i] = Some(LibSyms { sorted, known });
            }
        }
    }
    let symbol = |lib: usize, addr: i64| -> Option<(&str, u64)> {
        let syms = syms.as_ref()?;
        let e = lib_syms.get(lib)?.as_ref()?.lookup(u64::try_from(addr).ok()?)?;
        syms.string_table.get(e.symbol).map(|name| (name.as_str(), e.rva))
    };

    let mut b = TraceBuilder::new();
    let mut order: Vec<usize> = (0..profile.threads.len()).collect();
    order.sort_by(|&x, &y| {
        let (a, c) = (&profile.threads[x], &profile.threads[y]);
        c.is_main_thread.cmp(&a.is_main_thread).then(a.register_time.total_cmp(&c.register_time))
    });
    let interval = profile.meta.interval;
    let processes: std::collections::HashSet<String> = profile.threads.iter().map(|t| t.pid.to_string()).collect();

    for &ti in &order {
        let th = &profile.threads[ti];
        let pid = th.pid.to_string().trim_matches('"').parse::<u32>().unwrap_or(0);
        let mut name = if th.is_main_thread { th.process_name.clone().unwrap_or_else(|| th.name.clone()) } else { th.name.clone() };
        if processes.len() > 1 {
            name = format!("{pid}:{name}");
        }
        let thread = b.thread(&name, pid);

        let mut frame_func: Vec<Option<u32>> = vec![None; th.frame_table.func.len()];
        let mut node_of: Vec<u32> = Vec::with_capacity(th.stack_table.frame.len());
        for (si, (&prefix, &frame)) in th.stack_table.prefix.iter().zip(&th.stack_table.frame).enumerate() {
            let func = match frame_func[frame] {
                Some(f) => f,
                None => {
                    let fi = th.frame_table.func[frame];
                    let res = th.func_table.resource[fi].filter(|&r| r >= 0).map(|r| r as usize);
                    let lib = res.and_then(|r| th.resource_table.lib[r]);
                    let module_name = match res {
                        Some(r) => th.string_array[th.resource_table.name[r]].as_str(),
                        None => "unknown",
                    };
                    let raw = th.string_array[th.func_table.name[fi]].as_str();
                    let found = lib.and_then(|l| symbol(l, th.frame_table.address[frame]).map(|s| (l, s)));
                    let name = found.map_or(raw, |(_, (name, _))| name);
                    let origin = found.and_then(|(l, (_, rva))| {
                        let info = profile.libs.get(l)?;
                        let image = b.image(info.path.as_deref()?, info.code_id.as_deref());
                        Some(Origin { image, rva })
                    });
                    let m = b.module(module_name);
                    let f = b.func_at(m, name, origin);
                    frame_func[frame] = Some(f);
                    f
                }
            };
            let parent = match prefix {
                Some(p) if p < si => node_of[p],
                _ => ROOT,
            };
            node_of.push(b.child(parent, func));
        }

        let mut last = f64::MIN;
        for (i, (&stack, &t)) in th.samples.stack.iter().zip(&th.samples.time).enumerate() {
            let Some(stack) = stack else { continue };
            let leaf = node_of[stack];
            let w = th.samples.weight.as_ref().map_or(1, |w| w[i].max(1));
            for k in 0..w {
                let at = (t - (w - 1 - k) as f64 * interval).max(last + 1e-6);
                b.sample(thread, at / 1000.0, leaf);
                last = at;
            }
        }
    }
    let mut trace = b.finish();
    trace.name = profile.meta.product.unwrap_or_else(|| path.display().to_string());
    if trace.threads.is_empty() {
        return Err(format!("{}: the recording has no samples", path.display()));
    }
    Ok(trace)
}

#[cfg(test)]
mod tests {
    use super::*;

    const PROFILE: &str = r#"{
      "meta": { "interval": 1.0, "product": "demo" },
      "libs": [ { "path": "/bin/demo", "debugName": "demo", "codeId": "ABC" } ],
      "threads": [
        { "name": "worker", "pid": 7, "isMainThread": false, "processName": "demo", "registerTime": 5.0,
          "samples": { "stack": [0], "time": [20.0], "weight": [1] },
          "stackTable": { "prefix": [null], "frame": [0] },
          "frameTable": { "address": [16], "func": [0] },
          "funcTable": { "name": [1], "resource": [0] },
          "resourceTable": { "lib": [0], "name": [0] },
          "stringArray": ["demo", "0x10"] },
        { "name": "demo", "pid": 7, "isMainThread": true, "processName": "demo", "registerTime": 0.0,
          "samples": { "stack": [1, 2, null], "time": [10.0, 15.0, 16.0], "weight": [1, 3, 1] },
          "stackTable": { "prefix": [null, 0, 0], "frame": [0, 1, 2] },
          "frameTable": { "address": [16, 40, 999], "func": [0, 1, 2] },
          "funcTable": { "name": [1, 2, 3], "resource": [0, 0, null] },
          "resourceTable": { "lib": [0], "name": [0] },
          "stringArray": ["demo", "0x10", "0x28", "0x3e7"] }
      ]
    }"#;

    const SYMS: &str = r#"{
      "string_table": ["UNKNOWN", "main", "parse"],
      "data": [ { "debug_name": "demo", "code_id": "abc",
                  "symbol_table": [ { "rva": 32, "size": 16, "symbol": 2 }, { "rva": 0, "size": 32, "symbol": 1 } ],
                  "known_addresses": [] } ]
    }"#;

    fn write(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("traveler-samply-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join("rec.json");
        std::fs::write(&p, PROFILE).unwrap();
        std::fs::write(sidecar_of(&p), SYMS).unwrap();
        p
    }

    #[test]
    fn weighted_samples_expand_backwards_from_their_time() {
        let trace = load(&write("weights")).unwrap();
        let main = &trace.threads[0];
        let times: Vec<f64> = main.times.iter().map(|t| (t * 1000.0).round()).collect();
        assert_eq!(times, vec![0.0, 3.0, 4.0, 5.0]);
    }

    #[test]
    fn the_main_thread_comes_first_and_names_resolve_through_the_sidecar() {
        let trace = load(&write("names")).unwrap();
        assert_eq!(trace.threads[0].name, "demo");
        assert_eq!(trace.threads[1].name, "worker");
        let stack = trace.stack_at(0, 0.0).unwrap();
        let names: Vec<&str> = stack.iter().map(|&n| trace.name_of(n)).collect();
        assert_eq!(names, vec!["main", "parse"]);
        let stack = trace.stack_at(0, 0.004).unwrap();
        assert_eq!(trace.name_of(*stack.last().unwrap()), "0x3e7");
        assert_eq!(trace.modules[trace.module_of(*stack.last().unwrap()) as usize], "unknown");
    }

    #[test]
    fn a_function_remembers_its_binary_and_entry_not_the_sampled_address() {
        let trace = load(&write("origin")).unwrap();
        let f = trace.funcs.iter().find(|f| f.name == "parse").unwrap();
        let origin = f.origin.unwrap();
        assert_eq!(origin.rva, 32);
        assert_eq!(trace.images[origin.image as usize].path, "/bin/demo");
        assert_eq!(trace.images[origin.image as usize].code_id.as_deref(), Some("ABC"));
    }

    #[test]
    fn identical_symbols_share_one_function() {
        let trace = load(&write("shared")).unwrap();
        let mains = trace.funcs.iter().filter(|f| f.name == "main").count();
        assert_eq!(mains, 1);
    }
}
