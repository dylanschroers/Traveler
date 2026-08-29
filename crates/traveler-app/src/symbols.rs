//! Tier 2 in the app: sections underneath their mapping, and a name for an address.
//!
//! A library stops being one grey slab and becomes a readable strip of code, constants and data,
//! and the crosshair can name a function inside `.text` with no debug information anywhere on the
//! machine. This is the single biggest legibility win in the plan and it costs one file parse per
//! image.
//!
//! Everything here is optional. If nothing parses, the view is exactly tier 1 and still complete.

use std::collections::HashMap;

use bevy::prelude::*;
use traveler_proc::{MappingFacts, Prot, SubExtent};
use traveler_sym::{Images, SectionKind};

/// Matches the page the rest of the view is drawn in.
const PAGE: u64 = 4096;

/// Which tier the view is actually running at, so the HUD can say whether a name is a name or a
/// guess.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tier {
    /// Mappings, permissions, sizes, page state, backing paths. Never empty, never gated.
    One,
    /// Plus sections and exported symbols, from the image on disk.
    Two,
}

#[derive(Resource, Default)]
pub struct Symbols {
    images: Images,
    /// Where each image's link view actually landed, so a link-view address can be turned into a
    /// real one and back.
    bias: HashMap<String, u64>,
    sections: usize,
    named: usize,
}

impl Symbols {
    pub fn tier(&self) -> Tier {
        if self.sections > 0 {
            Tier::Two
        } else {
            Tier::One
        }
    }

    pub fn sections(&self) -> usize {
        self.sections
    }

    pub fn images(&self) -> usize {
        self.named
    }

    /// One list per mapping, ready for the frame. Runs after the target is resumed.
    pub fn build(&mut self, mappings: &[MappingFacts<'_>]) -> Vec<Vec<SubExtent>> {
        self.bias.clear();
        self.sections = 0;
        self.named = 0;

        // Where file offset zero of each image landed. For a position-independent image that is
        // the mapping address; for a fixed one it cancels out to nothing.
        let mut start: HashMap<&str, u64> = HashMap::new();
        for m in mappings.iter().filter(|m| is_image(m.path)) {
            let at = m.addr.saturating_sub(m.offset);
            start
                .entry(m.path)
                .and_modify(|e| *e = (*e).min(at))
                .or_insert(at);
        }

        for (path, at) in &start {
            let Some(image) = self.images.get(std::path::Path::new(path)) else {
                continue;
            };
            self.bias
                .insert((*path).to_string(), at.saturating_sub(image.base_vaddr));
            self.named += 1;
        }

        let out = mappings
            .iter()
            .map(|m| self.for_mapping(m))
            .collect::<Vec<_>>();
        self.sections = out.iter().map(Vec::len).sum();
        out
    }

    fn for_mapping(&mut self, m: &MappingFacts<'_>) -> Vec<SubExtent> {
        if !is_image(m.path) {
            return Vec::new();
        }
        let Some(bias) = self.bias.get(m.path).copied() else {
            return Vec::new();
        };
        let Some(image) = self.images.get(std::path::Path::new(m.path)) else {
            return Vec::new();
        };

        let end = m.addr + m.len;
        image
            .sections()
            .iter()
            // A page is the granularity of the whole view, so a section smaller than one cannot
            // have a tile worth looking at and falls into the gap around it instead. Measured on
            // `gdb`: median tile aspect 4.50 with every section, 2.22 with this floor, while
            // still keeping two and a half times as many nodes as the mapping level alone.
            .filter(|section| section.size >= PAGE)
            .filter_map(|section| {
                let addr = bias + section.addr;
                (addr < end && addr + section.size > m.addr).then(|| SubExtent {
                    addr,
                    len: section.size,
                    label: image.section_name(section).to_string(),
                    prot: section_prot(section.kind),
                })
            })
            .collect()
    }

    /// The name of whatever `addr` is inside, and how far into it, given the image `path` was
    /// mapped from. `None` when nothing was parsed or nothing covers it.
    pub fn name_at(&mut self, path: &str, addr: u64) -> Option<(String, u64)> {
        let bias = self.bias.get(path).copied()?;
        let image = self.images.get(std::path::Path::new(path))?;
        let (name, offset) = image.symbol_at(addr.checked_sub(bias)?)?;
        Some((name.to_string(), offset))
    }
}

/// A pseudo-path such as `[heap]` names no file, and an anonymous mapping names nothing at all.
fn is_image(path: &str) -> bool {
    !path.is_empty() && !path.starts_with('[') && !path.ends_with(" (deleted)")
}

/// What a section holds, in the same vocabulary the mapping level uses, so one palette serves
/// both and no fourth hue is needed.
fn section_prot(kind: SectionKind) -> Prot {
    Prot(match kind {
        SectionKind::Code => Prot::READ | Prot::EXEC,
        SectionKind::ReadOnly => Prot::READ,
        SectionKind::Data | SectionKind::Zeroed => Prot::READ | Prot::WRITE,
        SectionKind::Other => Prot::READ,
    })
}
