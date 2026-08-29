//! What the file on disk says about an image: its sections, and the names in them.
//!
//! Deliberately separate from `traveler-proc`, on a clean line. `proc` is what the *operating
//! system* says about a live process; `sym` is what the *file* says about an image. Different
//! questions, different sources, and this one is optional and heavy, so the process view builds
//! and runs with it absent or failing.
//!
//! Tier 1 is no symbol work at all and is always available. This is tier 2: sections and exported
//! symbols, which need only the binary itself. Tier 3 — DWARF and PDB — is phase 8.

pub mod debug;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use object::{Object, ObjectSection, ObjectSegment, ObjectSymbol, SymbolKind};

/// What a section is for. Three of these fall onto the permission palette without inventing a
/// fourth hue, and the rest is a neutral.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SectionKind {
    /// `.text`, `.plt`, `.init`.
    Code,
    /// `.rodata`, `.eh_frame`, constants.
    ReadOnly,
    /// `.data`, `.got`, `.tdata`.
    Data,
    /// `.bss` and `.tbss`: not in the file at all, which is exactly where the size goes missing
    /// if it is taken from the file range rather than from the section header.
    Zeroed,
    Other,
}

#[derive(Clone, Copy, Debug)]
pub struct Section {
    /// Link-view address. Add the load bias to get where it actually is.
    pub addr: u64,
    pub size: u64,
    pub kind: SectionKind,
    name: StrRef,
}

#[derive(Clone, Copy, Debug, Default)]
struct StrRef {
    off: u32,
    len: u32,
}

#[derive(Clone, Copy, Debug)]
struct Symbol {
    addr: u64,
    size: u64,
    name: StrRef,
}

pub struct Image {
    sections: Vec<Section>,
    /// Sorted by address, so naming an address is a binary search.
    symbols: Vec<Symbol>,
    text: String,
    /// Lowest loadable address in the link view. A load bias is where the image actually starts
    /// minus this, which is 0 for a non-relocatable image and the mapping base for a PIE.
    pub base_vaddr: u64,
}

impl Image {
    pub fn parse(path: &Path) -> Result<Image, String> {
        let data = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        Image::from_bytes(&data).map_err(|e| format!("{}: {e}", path.display()))
    }

    pub fn from_bytes(data: &[u8]) -> Result<Image, String> {
        let file = object::File::parse(data).map_err(|e| e.to_string())?;

        let base_vaddr = file
            .segments()
            .map(|s| s.address())
            .filter(|a| *a > 0)
            .min()
            .unwrap_or(0);

        let mut text = String::new();
        let mut sections: Vec<Section> = Vec::new();
        for section in file.sections() {
            if section.address() == 0 || section.size() == 0 {
                continue;
            }
            let name = section.name().unwrap_or("").to_string();
            sections.push(Section {
                addr: section.address(),
                // The section header's size, not the file range: `.bss` has no file range and
                // would come back zero, which is exactly where it matters.
                size: section.size(),
                kind: classify(&name, section.kind()),
                name: intern(&mut text, &name),
            });
        }
        sections.sort_unstable_by_key(|s| s.addr);

        // `.symtab` when it survived, else `.dynsym`, which survives stripping in anything
        // dynamically linked and is where the names come from on a distro binary.
        let mut found = collect(file.symbols());
        if found.is_empty() {
            found = collect(file.dynamic_symbols());
        }
        found.sort_unstable_by(|a, b| a.0.cmp(&b.0).then(a.2.cmp(&b.2)));
        found.dedup_by_key(|s| s.0);

        let symbols = found
            .into_iter()
            .map(|(addr, size, name)| Symbol {
                addr,
                size,
                name: intern(&mut text, &name),
            })
            .collect();

        Ok(Image {
            sections,
            symbols,
            text,
            base_vaddr,
        })
    }

    pub fn sections(&self) -> &[Section] {
        &self.sections
    }

    pub fn symbol_count(&self) -> usize {
        self.symbols.len()
    }

    /// The address of a named symbol, for asking a later tier about a place that is definitely
    /// this image's own.
    pub fn address_of(&self, name: &str) -> Option<u64> {
        self.symbols
            .iter()
            .find(|s| self.str(s.name) == name)
            .map(|s| s.addr)
    }

    #[inline]
    pub fn section_name(&self, section: &Section) -> &str {
        self.str(section.name)
    }

    /// The symbol containing `addr` in the link view, and how far into it the address is.
    ///
    /// A symbol with a size covers exactly its own range. One without — common in `.dynsym` — is
    /// taken to run to the next symbol, which is the usual convention and is right far more often
    /// than reporting nothing would be.
    pub fn symbol_at(&self, addr: u64) -> Option<(&str, u64)> {
        let i = match self.symbols.binary_search_by_key(&addr, |s| s.addr) {
            Ok(i) => i,
            Err(0) => return None,
            Err(i) => i - 1,
        };
        let symbol = &self.symbols[i];
        let end = if symbol.size > 0 {
            symbol.addr + symbol.size
        } else {
            self.symbols.get(i + 1).map_or(u64::MAX, |next| next.addr)
        };
        (addr < end).then(|| (self.str(symbol.name), addr - symbol.addr))
    }

    #[inline]
    fn str(&self, r: StrRef) -> &str {
        &self.text[r.off as usize..(r.off + r.len) as usize]
    }
}

fn intern(text: &mut String, s: &str) -> StrRef {
    let r = StrRef {
        off: text.len() as u32,
        len: s.len() as u32,
    };
    text.push_str(s);
    r
}

fn collect<'a>(symbols: impl Iterator<Item = impl ObjectSymbol<'a>>) -> Vec<(u64, u64, String)> {
    symbols
        .filter(|s| matches!(s.kind(), SymbolKind::Text | SymbolKind::Data))
        .filter(|s| s.address() > 0)
        .filter_map(|s| {
            let name = s.name().ok()?;
            (!name.is_empty()).then(|| (s.address(), s.size(), name.to_string()))
        })
        .collect()
}

fn classify(name: &str, kind: object::SectionKind) -> SectionKind {
    match kind {
        object::SectionKind::Text => SectionKind::Code,
        object::SectionKind::ReadOnlyData
        | object::SectionKind::ReadOnlyString
        | object::SectionKind::ReadOnlyDataWithRel => SectionKind::ReadOnly,
        object::SectionKind::Data | object::SectionKind::Tls => SectionKind::Data,
        object::SectionKind::UninitializedData | object::SectionKind::UninitializedTls => {
            SectionKind::Zeroed
        }
        _ => {
            // PE marks everything `Unknown` more often than ELF does, so fall back to the name.
            if name.starts_with(".text") {
                SectionKind::Code
            } else if name.starts_with(".rdata") || name.starts_with(".rodata") {
                SectionKind::ReadOnly
            } else if name.starts_with(".data") {
                SectionKind::Data
            } else if name.starts_with(".bss") {
                SectionKind::Zeroed
            } else {
                SectionKind::Other
            }
        }
    }
}

/// Parsed images, keyed by path and modification time. A process maps thirty to a hundred files
/// and each is parsed once; a failure is cached too, so a stripped or unreadable image is not
/// re-read on every capture.
#[derive(Default)]
pub struct Images {
    parsed: HashMap<(PathBuf, Option<std::time::SystemTime>), Option<Arc<Image>>>,
}

impl Images {
    pub fn get(&mut self, path: &Path) -> Option<Arc<Image>> {
        let stamp = std::fs::metadata(path).and_then(|m| m.modified()).ok();
        let key = (path.to_path_buf(), stamp);
        self.parsed
            .entry(key)
            .or_insert_with(|| Image::parse(path).ok().map(Arc::new))
            .clone()
    }

    pub fn len(&self) -> usize {
        self.parsed.len()
    }

    pub fn is_empty(&self) -> bool {
        self.parsed.is_empty()
    }
}
