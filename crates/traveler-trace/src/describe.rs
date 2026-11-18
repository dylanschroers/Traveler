//! What a function is doing, in plain words.
//!
//! Recognition goes from most to least certain: exact C and system-call names, then known Rust and
//! library paths, then the library a function lives in. The program's own code is never guessed
//! from words in its name (`read_config` is not file I/O), it is just named. Rust programs carry
//! the standard library inside the executable, which is why paths are checked before modules.

use crate::{ROOT, Trace};

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub enum Kind {
    Waiting,
    Locks,
    Memory,
    Copying,
    Files,
    Io,
    Network,
    Threads,
    Drawing,
    Hashing,
    Lookup,
    Sorting,
    Iterating,
    Text,
    Compression,
    Time,
    Math,
    System,
    Plumbing,
    Startup,
    Program,
    Library,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::Waiting => "Waiting",
            Kind::Locks => "Waiting for a lock",
            Kind::Memory => "Managing memory",
            Kind::Copying => "Copying data",
            Kind::Files => "Working with files",
            Kind::Io => "Reading or writing",
            Kind::Network => "Networking",
            Kind::Threads => "Coordinating threads",
            Kind::Drawing => "Drawing graphics",
            Kind::Hashing => "Hashing",
            Kind::Lookup => "Using a lookup table",
            Kind::Sorting => "Sorting or searching",
            Kind::Iterating => "Going through a collection",
            Kind::Text => "Working with text",
            Kind::Compression => "Compressing data",
            Kind::Time => "Checking the time",
            Kind::Math => "Doing math",
            Kind::System => "Asking the operating system",
            Kind::Plumbing => "Passing control along",
            Kind::Startup => "Starting up",
            Kind::Program => "The program's own code",
            Kind::Library => "Library code",
        }
    }

    pub fn sentence(self) -> &'static str {
        match self {
            Kind::Waiting => "Paused, waiting for another thread, a timer or the system to wake it. It uses no CPU while here.",
            Kind::Locks => "Paused until another thread lets go of something they both need.",
            Kind::Memory => "Asking for memory to hold data, growing a buffer, or giving memory back.",
            Kind::Copying => "Moving or clearing bytes in memory.",
            Kind::Files => "Opening, reading, writing or looking up files and folders on disk.",
            Kind::Io => "Moving data in or out: a file, a pipe or a network connection.",
            Kind::Network => "Talking to another machine over the network.",
            Kind::Threads => "Handing out work to other threads, running it, or collecting the results.",
            Kind::Drawing => "Preparing or sending graphics work to the GPU.",
            Kind::Hashing => "Turning a value into a fingerprint, usually to find it in a table.",
            Kind::Lookup => "Storing or finding entries in a table (a map or set).",
            Kind::Sorting => "Putting data in order, or searching data that is in order.",
            Kind::Iterating => "Stepping through a list or other collection, often building a new one.",
            Kind::Text => "Turning data into text or text into data: formatting, printing or parsing.",
            Kind::Compression => "Making data smaller or unpacking it.",
            Kind::Time => "Reading a clock.",
            Kind::Math => "Arithmetic on numbers, vectors or matrices.",
            Kind::System => "A system call: asking macOS to do something the program cannot do itself.",
            Kind::Plumbing => "Frames that only pass control to the next function. The work happens further in.",
            Kind::Startup => "Loading the program and its libraries, or the outermost frames every program has.",
            Kind::Program => "Code written for this program. Its name is the best clue to what it does.",
            Kind::Library => "Code from a library the program uses.",
        }
    }

    /// Time spent here is the thread doing nothing.
    pub fn is_idle(self) -> bool {
        matches!(self, Kind::Waiting | Kind::Locks)
    }
}

const EXACT: &[(&[&str], Kind)] = &[
    (&["psynch_mutexwait", "ulock_wait", "ulock_wait2", "os_unfair_lock_lock_slow", "pthread_mutex_lock", "pthread_mutex_firstfit_lock_slow"], Kind::Locks),
    (
        &[
            "psynch_cvwait", "pthread_cond_wait", "semwait_signal", "semaphore_wait_trap", "semaphore_timedwait_trap",
            "mach_msg2_trap", "mach_msg_trap", "mach_msg", "kevent", "kevent64", "kevent_id", "kevent_qos", "workq_kernreturn",
            "nanosleep", "usleep", "sleep", "select", "poll", "epoll_wait", "futex", "wait4", "waitpid", "sigsuspend", "psynch_rw_rdlock",
        ],
        Kind::Waiting,
    ),
    (&["malloc", "calloc", "realloc", "free", "posix_memalign", "mmap", "munmap", "madvise", "mach_vm_allocate", "mach_vm_deallocate", "vm_allocate", "brk", "sbrk"], Kind::Memory),
    (&["memcpy", "memmove", "memset", "bzero", "platform_memmove", "platform_memset", "platform_bzero"], Kind::Copying),
    (
        &[
            "open", "openat", "open_nocancel", "close", "close_nocancel", "stat", "stat64", "fstat", "fstat64", "fstatat", "fstatat64", "lstat",
            "lstat64", "getdirentries", "getdirentries64", "readdir", "readdir_r", "opendir", "closedir", "getattrlist", "getattrlistbulk",
            "access", "unlink", "rename", "mkdir", "rmdir", "fsync", "fcntl", "lseek", "realpath", "getcwd", "chdir", "statfs", "fstatfs",
        ],
        Kind::Files,
    ),
    (&["read", "read_nocancel", "pread", "readv", "write", "write_nocancel", "pwrite", "writev"], Kind::Io),
    (&["socket", "connect", "accept", "bind", "listen", "send", "sendto", "sendmsg", "recv", "recvfrom", "recvmsg", "getaddrinfo", "shutdown"], Kind::Network),
    (&["mach_absolute_time", "clock_gettime", "gettimeofday", "clock_gettime_nsec_np", "mach_continuous_time"], Kind::Time),
    (&["pthread_create", "thread_start", "pthread_start", "pthread_join", "dispatch_async", "dispatch_sync", "pthread_cond_signal", "pthread_cond_broadcast", "psynch_cvsignal", "psynch_cvbroad", "psynch_mutexdrop"], Kind::Threads),
    (&["start", "dyld_start", "main", "_main"], Kind::Startup),
    (&["objc_msgSend", "objc_retain", "objc_release", "objc_autoreleasePoolPop", "objc_autoreleasePoolPush"], Kind::Plumbing),
    (&["sqrt", "sin", "cos", "tan", "exp", "log", "pow", "fma", "atan2"], Kind::Math),
];

/// Matched against the cleaned path, in order. Earlier entries win.
const PATHS: &[(&[&str], Kind)] = &[
    (&["::wait", "::park", "Latch::", "latch::", "wait_until", "Condvar::wait", "sleep"], Kind::Waiting),
    (&["Mutex::lock", "RwLock::", "lock_contended", "parking_lot"], Kind::Locks),
    (&["drop_in_place", "raw_vec", "alloc::alloc", "rust_alloc", "rust_dealloc", "rust_realloc", "rdl_alloc", "finish_grow", "reserve", "with_capacity", "dealloc"], Kind::Memory),
    (&["hashbrown", "collections::hash", "HashMap", "HashSet", "BTreeMap", "BTreeSet", "IndexMap", "rustc_entry"], Kind::Lookup),
    (&["core::hash", "std::hash", "BuildHasher", "Hasher", "hash_one", "siphash", "SipHasher", "fxhash", "ahash", "blake3", "sha2", "crc32"], Kind::Hashing),
    (&["slice::sort", "::sort", "quicksort", "heapsort", "insertion_sort", "merge_sort", "partition_point", "binary_search", "select_nth"], Kind::Sorting),
    (&["core::fmt", "alloc::fmt", "Formatter", "Display", "Debug>::fmt", "io::stdio", "print", "core::str", "alloc::string", "to_string", "serde", "json", "parse", "regex", "utf8"], Kind::Text),
    (&["std::fs", "sys::fs", "fs::DirEntry", "fs::File", "ReadDir", "walkdir", "ignore::walk"], Kind::Files),
    (&["std::net", "tokio::net", "hyper", "reqwest", "rustls", "openssl", "h2::"], Kind::Network),
    (&["flate2", "miniz", "zstd", "lz4", "inflate", "deflate", "brotli"], Kind::Compression),
    (&["rayon", "crossbeam", "std::thread", "sys::thread", "std::sync", "tokio", "async_task", "futures", "Registry::", "Job>::execute", "HeapJob", "StackJob", "join_context"], Kind::Threads),
    (&["wgpu", "naga", "bevy_render", "metal::", "vulkan", "opengl", "glow::"], Kind::Drawing),
    (&["glam", "nalgebra", "::math"], Kind::Math),
    (&["core::iter", "Iterator", "spec_from_iter", "spec_extend", "from_iter", "collect", "fold", "for_each", "map::Map"], Kind::Iterating),
    (&["std::rt", "lang_start", "rust_begin_short_backtrace", "ops::function", "call_once", "FnOnce", "FnMut", "std::panic", "catch_unwind", "panicking::try"], Kind::Plumbing),
];

/// By library, for code with no better clue.
const MODULES: &[(&[&str], Kind)] = &[
    (&["libsystem_malloc"], Kind::Memory),
    (&["libsystem_platform"], Kind::Copying),
    (&["libsystem_pthread", "libdispatch"], Kind::Threads),
    (&["dyld"], Kind::Startup),
    (&["Metal", "OpenGL", "QuartzCore", "CoreGraphics", "IOSurface", "AGX", "IOGPU", "GPUCompiler", "libGL", "vulkan", "MoltenVK"], Kind::Drawing),
    (&["libz", "libcompression", "libbz2"], Kind::Compression),
    (&["CFNetwork", "libnetwork", "libsystem_networkextension", "Network"], Kind::Network),
    (&["libobjc"], Kind::Plumbing),
    (&["libsystem_m"], Kind::Math),
    (&["libsystem_kernel"], Kind::System),
];

fn bare(name: &str) -> &str {
    name.trim_start_matches('_')
}

/// What `name` in `module` is doing. `program` is the main executable's module name.
pub fn kind_of(name: &str, module: &str, program: &str) -> Kind {
    // A closure does the work of the function it is written in.
    if name.contains("{{closure}}") {
        let outer = name.replace("::{{closure}}", "").replace("{{closure}}", "");
        if !outer.is_empty() && outer != name {
            return kind_of(&outer, module, program);
        }
    }
    let b = bare(name);
    if let Some((_, k)) = EXACT.iter().find(|(names, _)| names.contains(&b)) {
        return *k;
    }
    if name.contains("::") || name.contains('<') {
        if let Some((_, k)) = PATHS.iter().find(|(pats, _)| pats.iter().any(|p| name.contains(p))) {
            return *k;
        }
        if name.ends_with("::main") && !name.contains('<') {
            return Kind::Startup;
        }
    }
    let base = module.trim_end_matches(".dylib");
    if base == program || module == program {
        return Kind::Program;
    }
    // Inside system libraries a word in the name is a reasonable clue; in the program's own code
    // it is not, which is why this comes after the program check.
    let lower = b.to_ascii_lowercase();
    if lower.contains("malloc") || lower.contains("alloc") || lower.starts_with("free") {
        return Kind::Memory;
    }
    if lower.contains("wait") {
        return Kind::Waiting;
    }
    if let Some((_, k)) = MODULES.iter().find(|(mods, _)| mods.iter().any(|m| base.contains(m))) {
        return *k;
    }
    Kind::Library
}

/// A compiler name made shorter for people: no hash suffix, no generic arguments, no closure
/// markers, and only the last two path segments. The full name stays available elsewhere.
pub fn clean_name(name: &str) -> String {
    let mut s = name.to_string();
    if let Some(i) = s.rfind("::h")
        && s[i + 3..].len() == 16
        && s[i + 3..].chars().all(|c| c.is_ascii_hexdigit())
    {
        s.truncate(i);
    }
    let s = s.replace("{{closure}}", "").replace("::::", "::");
    let mut out = String::with_capacity(s.len());
    let mut depth = 0;
    for c in s.chars() {
        match c {
            '<' => depth += 1,
            '>' if depth > 0 => depth -= 1,
            _ if depth == 0 => out.push(c),
            _ => {}
        }
    }
    let out = out.trim_matches(|c: char| c == ':' || c.is_whitespace()).to_string();
    let parts: Vec<&str> = out.split("::").filter(|p| !p.is_empty() && *p != " as ").collect();
    let tail = if parts.len() > 2 { &parts[parts.len() - 2..] } else { &parts[..] };
    let joined = tail.join("::");
    if joined.is_empty() { name.to_string() } else { joined }
}

/// Names people can say: the main thread keeps its name, unnamed threads become "worker N".
pub fn thread_names(trace: &Trace) -> Vec<String> {
    let mut workers = 0;
    trace
        .threads
        .iter()
        .enumerate()
        .map(|(i, th)| {
            if i == 0 {
                format!("main thread ({})", th.name)
            } else if th.name.starts_with("Thread <") || th.name.is_empty() {
                workers += 1;
                format!("worker {workers}")
            } else {
                th.name.clone()
            }
        })
        .collect()
}

/// Names the program module: the one named like the recording, else the busiest non-system one.
pub fn program_module(trace: &Trace) -> String {
    if let Some(m) = trace.modules.iter().find(|m| **m == trace.name) {
        return m.clone();
    }
    let mut weight = vec![0usize; trace.modules.len()];
    for th in &trace.threads {
        for &leaf in &th.leaves {
            let mut n = leaf;
            while n != ROOT {
                weight[trace.module_of(n) as usize] += 1;
                n = trace.parent[n as usize];
            }
        }
    }
    let system = |m: &str| m.starts_with("lib") || m == "dyld" || MODULES.iter().any(|(ms, _)| ms.iter().any(|x| m.contains(x)));
    (0..trace.modules.len())
        .filter(|&m| !system(&trace.modules[m]))
        .max_by_key(|&m| weight[m])
        .map_or_else(String::new, |m| trace.modules[m].clone())
}

/// Precomputed kind per function, and helpers that need the call tree.
pub struct Describer {
    pub program: String,
    kinds: Vec<Kind>,
}

impl Describer {
    pub fn new(trace: &Trace) -> Self {
        let program = program_module(trace);
        let kinds = trace.funcs.iter().map(|f| kind_of(&f.name, &trace.modules[f.module as usize], &program)).collect();
        Describer { program, kinds }
    }

    pub fn kind(&self, trace: &Trace, node: u32) -> Kind {
        self.kinds[trace.func[node as usize] as usize]
    }

    /// What a sample whose innermost frame is `leaf` was doing. A plumbing frame there is a
    /// wrapper the work was inlined into (`call_mut` around a closure took 47% of one recording's
    /// worker samples), so the answer is the nearest real frame above it.
    ///
    /// When that nearest frame is a thread pool or an iterator adapter, the wrapper is the
    /// closure it was handed, which is the program's own code running under it.
    pub fn doing(&self, trace: &Trace, leaf: u32) -> Kind {
        let mut n = leaf;
        let mut skipped = false;
        while n != ROOT {
            let k = self.kind(trace, n);
            if k != Kind::Plumbing {
                return if skipped && matches!(k, Kind::Threads | Kind::Iterating) { Kind::Program } else { k };
            }
            skipped = true;
            n = trace.parent[n as usize];
        }
        Kind::Plumbing
    }

    /// The program's own functions on the way to `node`, outermost first: what the thread was in
    /// the middle of when it got here. The whole stack counts, trunk included: a worker thread's
    /// one job sits in its trunk for its whole life.
    pub fn tasks(&self, trace: &Trace, node: u32) -> Vec<u32> {
        let mut out = Vec::new();
        let mut n = node;
        while n != ROOT {
            if self.kind(trace, n) == Kind::Program {
                out.push(n);
            }
            n = trace.parent[n as usize];
        }
        out.reverse();
        out
    }

    /// The outermost program function under `leaf`, which names the job a sample belongs to.
    /// The job a sample belongs to, in words: the outermost program function on its stack, or
    /// "thread-pool work" when a pool ran it but the job's own function was inlined away.
    pub fn job(&self, trace: &Trace, leaf: u32) -> String {
        if let Some(f) = self.task_of(trace, leaf) {
            return clean_name(&trace.funcs[f as usize].name);
        }
        let mut n = leaf;
        while n != ROOT {
            if self.kind(trace, n) == Kind::Threads {
                return "thread-pool work".into();
            }
            n = trace.parent[n as usize];
        }
        self.doing(trace, leaf).label().to_lowercase()
    }

    pub fn task_of(&self, trace: &Trace, leaf: u32) -> Option<u32> {
        let mut n = leaf;
        let mut found = None;
        while n != ROOT {
            if self.kind(trace, n) == Kind::Program {
                found = Some(trace.func[n as usize]);
            }
            n = trace.parent[n as usize];
        }
        found
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_calls_and_libraries_are_recognised() {
        assert_eq!(kind_of("__psynch_cvwait", "libsystem_kernel.dylib", "app"), Kind::Waiting);
        assert_eq!(kind_of("fstatat", "libsystem_kernel.dylib", "app"), Kind::Files);
        assert_eq!(kind_of("szone_malloc_should_clear", "libsystem_malloc.dylib", "app"), Kind::Memory);
        assert_eq!(kind_of("_platform_memmove", "libsystem_platform.dylib", "app"), Kind::Copying);
        assert_eq!(kind_of("__getdirentries64", "libsystem_kernel.dylib", "app"), Kind::Files);
        assert_eq!(kind_of("mach_msg2_trap", "libsystem_kernel.dylib", "app"), Kind::Waiting);
        assert_eq!(kind_of("something_else", "libsystem_kernel.dylib", "app"), Kind::System);
    }

    #[test]
    fn standard_library_paths_beat_the_program_module() {
        let p = "stats";
        assert_eq!(kind_of("core::hash::BuildHasher::hash_one", p, p), Kind::Hashing);
        assert_eq!(kind_of("hashbrown::map::HashMap<K,V,S,A>::insert", p, p), Kind::Lookup);
        assert_eq!(kind_of("alloc::raw_vec::RawVecInner<A>::finish_grow", p, p), Kind::Memory);
        assert_eq!(kind_of("rayon_core::registry::WorkerThread::wait_until_cold", p, p), Kind::Waiting);
        assert_eq!(kind_of("<rayon_core::job::HeapJob<BODY> as rayon_core::job::Job>::execute", p, p), Kind::Threads);
        assert_eq!(kind_of("std::sys::backtrace::__rust_begin_short_backtrace", p, p), Kind::Plumbing);
    }

    #[test]
    fn the_programs_own_code_is_named_not_guessed() {
        assert_eq!(kind_of("traveler_trace::synth::generate", "stats", "stats"), Kind::Program);
        assert_eq!(kind_of("myapp::read_config", "myapp", "myapp"), Kind::Program);
        assert_eq!(kind_of("stats::main", "stats", "stats"), Kind::Startup);
        assert_eq!(kind_of("traveler_trace::Levels::build::{{closure}}", "stats", "stats"), Kind::Program);
        assert_eq!(kind_of("std::rt::lang_start::{{closure}}", "stats", "stats"), Kind::Plumbing);
    }

    #[test]
    fn names_are_shortened_for_people() {
        assert_eq!(clean_name("traveler_trace::TraceBuilder::child::h0123456789abcdef"), "TraceBuilder::child");
        assert_eq!(clean_name("traveler_trace::Levels::build::{{closure}}"), "Levels::build");
        assert_eq!(clean_name("<alloc::vec::Vec<T> as alloc::vec::spec_from_iter::SpecFromIter<T,I>>::from_iter"), "from_iter");
        assert_eq!(clean_name("fstatat"), "fstatat");
    }
}
