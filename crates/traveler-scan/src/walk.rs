//! Parallel directory walk.
//!
//! One rayon task per directory. Each task reads its own directory outside the lock, then takes
//! the builder lock once to append all of its children as a contiguous block. That gives node ids
//! allocated parent-before-child and children stored contiguously, without sorting a million
//! paths or keeping a hash map of them.

use std::{
    collections::HashMap,
    fs,
    path::{Path, PathBuf},
    sync::Mutex,
    time::Instant,
};

use crate::{ExtId, FileIndex, NodeFlags, NodeId, ScanError, ScanStats, StrRef};

#[cfg(windows)]
const FILE_ATTRIBUTE_HIDDEN: u32 = 0x0000_0002;
#[cfg(windows)]
const FILE_ATTRIBUTE_DIRECTORY: u32 = 0x0000_0010;
#[cfg(windows)]
const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;

#[derive(Clone, Debug, Default)]
pub struct ScanOptions {
    /// 0 means "use rayon's default", which is one thread per logical core.
    pub threads: usize,
    /// Diagnostic only. Skips the per-entry `metadata()` call, so every size is 0 and directory
    /// junctions land on the wrong side of the file/folder split. Exists solely to measure what
    /// metadata costs on this platform; the resulting index is not usable for anything else.
    pub skip_metadata: bool,
}


struct Entry {
    name: String,
    lossy: bool,
    size: u64,
    flags: NodeFlags,
}

struct Builder {
    parent: Vec<NodeId>,
    name: Vec<StrRef>,
    size: Vec<u64>,
    ext: Vec<ExtId>,
    flags: Vec<NodeFlags>,
    depth: Vec<u16>,
    child_start: Vec<u32>,
    child_len: Vec<u32>,
    names: String,
    ext_text: Vec<String>,
    ext_lookup: HashMap<String, ExtId>,
    errors: Vec<ScanError>,
    stats: ScanStats,
}

impl Builder {
    fn new(root_display: &str, root_flags: NodeFlags, root_size: u64) -> Self {
        let mut b = Builder {
            parent: Vec::new(),
            name: Vec::new(),
            size: Vec::new(),
            ext: Vec::new(),
            flags: Vec::new(),
            depth: Vec::new(),
            child_start: Vec::new(),
            child_len: Vec::new(),
            names: String::new(),
            ext_text: vec![String::new()],
            ext_lookup: HashMap::from([(String::new(), ExtId(0))]),
            errors: Vec::new(),
            stats: ScanStats::default(),
        };
        let name = b.intern(root_display);
        b.parent.push(NodeId(0));
        b.name.push(name);
        b.size.push(root_size);
        b.ext.push(ExtId(0));
        b.flags.push(root_flags);
        b.depth.push(0);
        b.child_start.push(0);
        b.child_len.push(0);
        if root_flags.is_dir() {
            b.stats.dirs = 1;
        } else {
            b.stats.files = 1;
        }
        b
    }

    fn intern(&mut self, s: &str) -> StrRef {
        let off = self.names.len();
        assert!(off <= u32::MAX as usize, "name blob exceeded 4 GiB");
        self.names.push_str(s);
        StrRef {
            off: off as u32,
            len: s.len() as u32,
        }
    }

    fn intern_ext(&mut self, name: &str) -> ExtId {
        let Some(ext) = Path::new(name).extension() else {
            return ExtId(0);
        };
        let ext = ext.to_string_lossy().to_ascii_lowercase();
        if let Some(id) = self.ext_lookup.get(&ext) {
            return *id;
        }
        let id = ExtId(self.ext_text.len() as u32);
        self.ext_text.push(ext.clone());
        self.ext_lookup.insert(ext, id);
        id
    }

    /// Appends `entries` as the children of `parent`, returning the subdirectories to descend into.
    fn push_children(&mut self, parent: NodeId, entries: Vec<Entry>) -> Vec<(String, NodeId)> {
        let start = self.parent.len();
        assert!(
            start + entries.len() <= u32::MAX as usize,
            "more than 4 billion nodes"
        );
        self.child_start[parent.index()] = start as u32;
        self.child_len[parent.index()] = entries.len() as u32;
        let depth = self.depth[parent.index()].saturating_add(1);

        let mut subdirs = Vec::new();
        for e in entries {
            let id = NodeId(self.parent.len() as u32);
            let name = self.intern(&e.name);
            let ext = if e.flags.is_dir() {
                ExtId(0)
            } else {
                self.intern_ext(&e.name)
            };

            self.parent.push(parent);
            self.name.push(name);
            self.size.push(e.size);
            self.ext.push(ext);
            self.flags.push(e.flags);
            self.depth.push(depth);
            self.child_start.push(0);
            self.child_len.push(0);

            if e.flags.is_dir() {
                self.stats.dirs += 1;
            } else {
                self.stats.files += 1;
            }
            if e.flags.is_reparse() {
                self.stats.reparse_points += 1;
            }
            if e.lossy {
                self.stats.lossy_names += 1;
            }

            // Reparse points are recorded but never descended into, or a junction pointing at an
            // ancestor would walk forever.
            if e.flags.is_dir() && !e.flags.is_reparse() {
                subdirs.push((e.name, id));
            }
        }
        subdirs
    }

    fn finish(mut self, walk_millis: u64) -> FileIndex {
        // Roll subtree sizes up into parents. Node ids are allocated parent-before-child, so a
        // single reverse pass is enough: every child is visited before its parent is read.
        for i in (1..self.parent.len()).rev() {
            let p = self.parent[i].index();
            self.size[p] = self.size[p].saturating_add(self.size[i]);
        }
        self.stats.bytes = self.size.first().copied().unwrap_or(0);
        self.stats.walk_millis = walk_millis;

        FileIndex {
            parent: self.parent,
            name: self.name,
            size: self.size,
            ext: self.ext,
            flags: self.flags,
            depth: self.depth,
            child_start: self.child_start,
            child_len: self.child_len,
            names: self.names,
            ext_text: self.ext_text,
            errors: self.errors,
            stats: self.stats,
        }
    }
}

struct Shared {
    builder: Mutex<Builder>,
    skip_metadata: bool,
}

/// Walks `root` and returns the index. Symlinks and junctions are recorded but never followed.
pub fn scan(root: &Path, opts: &ScanOptions) -> std::io::Result<FileIndex> {
    let meta = fs::symlink_metadata(root)?;
    let root_display = root.display().to_string();

    let mut root_flags = NodeFlags::default();
    if meta.is_dir() {
        root_flags.0 |= NodeFlags::DIR;
    }
    let root_size = if meta.is_dir() { 0 } else { meta.len() };

    let started = Instant::now();
    let shared = Shared {
        builder: Mutex::new(Builder::new(&root_display, root_flags, root_size)),
        skip_metadata: opts.skip_metadata,
    };

    if meta.is_dir() {
        let run = || {
            rayon::scope(|s| {
                visit(s, root.to_path_buf(), NodeId(0), &shared);
            })
        };
        if opts.threads > 0 {
            rayon::ThreadPoolBuilder::new()
                .num_threads(opts.threads)
                .build()
                .map_err(std::io::Error::other)?
                .install(run);
        } else {
            run();
        }
    }

    let elapsed = started.elapsed().as_millis() as u64;
    let builder = shared
        .builder
        .into_inner()
        .unwrap_or_else(|e| e.into_inner());
    Ok(builder.finish(elapsed))
}

fn visit<'s>(scope: &rayon::Scope<'s>, dir: PathBuf, id: NodeId, shared: &'s Shared) {
    let (entries, errors, unreadable) = read_dir_entries(&dir, shared.skip_metadata);

    let subdirs = {
        let mut b = shared.builder.lock().unwrap_or_else(|e| e.into_inner());
        b.errors.extend(errors);
        if unreadable {
            b.flags[id.index()].0 |= NodeFlags::UNREADABLE;
        }
        b.push_children(id, entries)
    };

    for (name, child_id) in subdirs {
        let child_path = dir.join(name);
        scope.spawn(move |s| visit(s, child_path, child_id, shared));
    }
}

/// Reads one directory. Errors are collected rather than propagated: a scan of a whole drive will
/// always hit directories it is not allowed to read, and aborting on the first one is useless.
fn read_dir_entries(dir: &Path, skip_metadata: bool) -> (Vec<Entry>, Vec<ScanError>, bool) {
    let mut entries = Vec::new();
    let mut errors = Vec::new();

    let iter = match fs::read_dir(dir) {
        Ok(iter) => iter,
        Err(e) => {
            errors.push(ScanError {
                path: dir.display().to_string(),
                message: e.to_string(),
            });
            return (entries, errors, true);
        }
    };

    for item in iter {
        let entry = match item {
            Ok(entry) => entry,
            Err(e) => {
                errors.push(ScanError {
                    path: dir.display().to_string(),
                    message: e.to_string(),
                });
                continue;
            }
        };

        let raw = entry.file_name();
        let name = raw.to_string_lossy();
        let lossy = matches!(name, std::borrow::Cow::Owned(_));
        let name = name.into_owned();

        let mut flags = NodeFlags::default();
        let mut size = 0u64;

        if skip_metadata {
            match entry.file_type() {
                Ok(ft) => {
                    if ft.is_dir() {
                        flags.0 |= NodeFlags::DIR;
                    }
                    if ft.is_symlink() {
                        flags.0 |= NodeFlags::REPARSE;
                    }
                }
                Err(e) => {
                    errors.push(ScanError {
                        path: dir.join(&name).display().to_string(),
                        message: e.to_string(),
                    });
                    continue;
                }
            }
        } else {
            // `DirEntry::metadata` does not traverse symlinks, which is what we want: a junction
            // should be recorded at its own size, not at the size of whatever it points at.
            match entry.metadata() {
                Ok(md) => {
                    #[cfg(windows)]
                    let is_dir = {
                        use std::os::windows::fs::MetadataExt;
                        let attrs = md.file_attributes();
                        if attrs & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
                            flags.0 |= NodeFlags::REPARSE;
                        }
                        if attrs & FILE_ATTRIBUTE_HIDDEN != 0 {
                            flags.0 |= NodeFlags::HIDDEN;
                        }
                        // Read the attribute rather than trusting `md.is_dir()`. A directory
                        // junction carries both DIRECTORY and REPARSE_POINT, but std classifies
                        // every reparse point as a symlink, so `is_dir()` returns false and the
                        // junction would be filed as a zero-byte *file*. pnpm's node_modules is
                        // full of these, so on a repo tree that is thousands of phantom files.
                        attrs & FILE_ATTRIBUTE_DIRECTORY != 0
                    };
                    #[cfg(not(windows))]
                    let is_dir = {
                        if md.is_symlink() {
                            flags.0 |= NodeFlags::REPARSE;
                        }
                        if name.starts_with('.') {
                            flags.0 |= NodeFlags::HIDDEN;
                        }
                        md.is_dir()
                    };

                    if is_dir {
                        flags.0 |= NodeFlags::DIR;
                    } else {
                        size = md.len();
                    }
                }
                Err(e) => {
                    errors.push(ScanError {
                        path: dir.join(&name).display().to_string(),
                        message: e.to_string(),
                    });
                    continue;
                }
            }
        }

        entries.push(Entry {
            name,
            lossy,
            size,
            flags,
        });
    }

    (entries, errors, false)
}
