//! Page state, held per mapping as bitsets rather than per page as nodes.
//!
//! A gigabyte of working set is 262k pages and four gigabytes mapped is a million, so pages are
//! never materialised as nodes. What a capture keeps is three bits per page, and the popcount that
//! turns them into a resident byte count.

pub const PAGE: u64 = 4096;

/// Bit 63 present, 62 swapped, 61 file-backed or shared-anonymous, 56 exclusive, 55 soft-dirty.
/// The PFN in bits 0-54 is zeroed without `CAP_SYS_ADMIN` and is not needed.
const PRESENT: u64 = 1 << 63;
const SWAPPED: u64 = 1 << 62;
const SOFT_DIRTY: u64 = 1 << 55;

/// Both caps are counted in pages, because pages are what the read costs. The kernel walks page
/// tables to answer, so the price is per entry and not per byte of address space: measured at
/// about 130 ns per page, or 7.7M pages per second.
#[derive(Clone, Copy, Debug)]
pub struct PageLimits {
    /// A mapping wider than this does not get its pages read at all. An ASan shadow is a 20 TiB
    /// reservation, five billion pages, for one line of `maps`.
    pub detail_pages: u64,
    /// Pages one capture may read, smallest mapping first so one giant reservation cannot starve
    /// the rest.
    pub budget_pages: u64,
}

impl Default for PageLimits {
    fn default() -> Self {
        Self {
            // 4 GiB of address space. The budget is the operative cap; this only excludes
            // reservations no budget could afford anyway.
            detail_pages: 1 << 20,
            // 2 GiB of address space, about 68 ms of stop at the measured rate. An earlier
            // version capped at a gigabyte and elided exactly the mapping worth looking at, which
            // is the failure this cap has to avoid while still bounding the stop.
            budget_pages: 1 << 19,
        }
    }
}

/// Raw pagemap words, read while the target is stopped and decoded after it is running again.
#[derive(Default)]
pub struct RawPages {
    words: Vec<u64>,
    start: Vec<u32>,
    len: Vec<u32>,
    elided: Vec<bool>,
}

impl RawPages {
    pub fn with_capacity(mappings: usize) -> Self {
        Self {
            words: Vec::new(),
            start: vec![0; mappings],
            len: vec![0; mappings],
            elided: vec![true; mappings],
        }
    }

    /// Which mappings to spend the budget on: everything under `detail_pages`, smallest first
    /// until `budget_pages` is gone.
    pub fn plan(extents: &[(u64, u64)], limits: &PageLimits) -> Vec<usize> {
        let mut affordable: Vec<(u64, usize)> = extents
            .iter()
            .enumerate()
            .map(|(i, (start, end))| (end.saturating_sub(*start) / PAGE, i))
            .filter(|(pages, _)| *pages <= limits.detail_pages)
            .collect();
        affordable.sort_unstable();

        let mut spent = 0u64;
        let mut chosen: Vec<usize> = Vec::with_capacity(affordable.len());
        for (pages, i) in affordable {
            if spent + pages > limits.budget_pages {
                break;
            }
            spent += pages;
            chosen.push(i);
        }
        chosen.sort_unstable();
        chosen
    }

    pub fn set(&mut self, mapping: usize, words: &[u64]) {
        self.start[mapping] = self.words.len() as u32;
        self.len[mapping] = words.len() as u32;
        self.elided[mapping] = false;
        self.words.extend_from_slice(words);
    }

    pub fn read_count(&self) -> usize {
        self.elided.iter().filter(|e| !**e).count()
    }
}

/// Decoded page state, parallel to the frame's mappings.
#[derive(Clone, Default)]
pub struct PageBits {
    bit_start: Vec<u64>,
    pages: Vec<u32>,
    resident: Vec<u32>,
    elided: Vec<bool>,
    present: Vec<u64>,
    swapped: Vec<u64>,
    dirty: Vec<u64>,
}

impl PageBits {
    /// Decoding happens after the target is running again, per §7.
    pub fn decode(raw: &RawPages) -> PageBits {
        let mappings = raw.len.len();
        let total_bits: u64 = raw.len.iter().map(|n| *n as u64).sum();
        let words = (total_bits.div_ceil(64)) as usize;

        let mut bits = PageBits {
            bit_start: Vec::with_capacity(mappings),
            pages: Vec::with_capacity(mappings),
            resident: Vec::with_capacity(mappings),
            elided: raw.elided.clone(),
            present: vec![0; words],
            swapped: vec![0; words],
            dirty: vec![0; words],
        };

        let mut cursor = 0u64;
        for m in 0..mappings {
            let from = raw.start[m] as usize;
            let count = raw.len[m] as usize;
            bits.bit_start.push(cursor);
            bits.pages.push(count as u32);

            let mut resident = 0u32;
            for (i, word) in raw.words[from..from + count].iter().enumerate() {
                let bit = cursor + i as u64;
                if word & PRESENT != 0 {
                    set(&mut bits.present, bit);
                    resident += 1;
                }
                if word & SWAPPED != 0 {
                    set(&mut bits.swapped, bit);
                }
                if word & SOFT_DIRTY != 0 {
                    set(&mut bits.dirty, bit);
                }
            }
            bits.resident.push(resident);
            cursor += count as u64;
        }
        bits
    }

    #[inline]
    pub fn pages(&self, mapping: usize) -> u32 {
        self.pages.get(mapping).copied().unwrap_or(0)
    }

    /// True when the mapping was too wide or too late in the budget to read. Deliberately not the
    /// same thing as a mapping whose pages are all absent.
    #[inline]
    pub fn is_elided(&self, mapping: usize) -> bool {
        self.elided.get(mapping).copied().unwrap_or(true)
    }

    /// Pages present in RAM, derived from `pagemap` rather than from `smaps`, which makes the
    /// kernel walk page tables per mapping.
    #[inline]
    pub fn resident(&self, mapping: usize) -> u32 {
        self.resident.get(mapping).copied().unwrap_or(0)
    }

    pub fn resident_fraction(&self, mapping: usize) -> Option<f32> {
        if self.is_elided(mapping) {
            return None;
        }
        let pages = self.pages(mapping);
        (pages > 0).then(|| self.resident(mapping) as f32 / pages as f32)
    }

    #[inline]
    pub fn is_present(&self, mapping: usize, page: u32) -> bool {
        self.test(&self.present, mapping, page)
    }
    #[inline]
    pub fn is_swapped(&self, mapping: usize, page: u32) -> bool {
        self.test(&self.swapped, mapping, page)
    }
    #[inline]
    pub fn is_dirty(&self, mapping: usize, page: u32) -> bool {
        self.test(&self.dirty, mapping, page)
    }

    #[inline]
    fn test(&self, field: &[u64], mapping: usize, page: u32) -> bool {
        if page >= self.pages(mapping) {
            return false;
        }
        let bit = self.bit_start[mapping] + page as u64;
        field[(bit >> 6) as usize] & (1 << (bit & 63)) != 0
    }
}

#[inline]
fn set(field: &mut [u64], bit: u64) {
    field[(bit >> 6) as usize] |= 1 << (bit & 63);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn raw(mappings: &[&[u64]]) -> RawPages {
        let mut r = RawPages::with_capacity(mappings.len());
        for (i, words) in mappings.iter().enumerate() {
            r.set(i, words);
        }
        r
    }

    #[test]
    fn decoding_separates_present_swapped_and_dirty() {
        let bits = PageBits::decode(&raw(&[&[PRESENT, 0, SWAPPED, PRESENT | SOFT_DIRTY]]));
        assert_eq!(bits.pages(0), 4);
        assert_eq!(bits.resident(0), 2);
        assert!(bits.is_present(0, 0) && !bits.is_present(0, 1));
        assert!(bits.is_swapped(0, 2) && !bits.is_swapped(0, 0));
        assert!(bits.is_dirty(0, 3) && !bits.is_dirty(0, 0));
        assert_eq!(bits.resident_fraction(0), Some(0.5));
    }

    /// Bit positions are per mapping, so a second mapping must not read the first one's bits.
    #[test]
    fn mappings_do_not_share_bits() {
        let bits = PageBits::decode(&raw(&[&[0; 70], &[PRESENT, 0, PRESENT]]));
        assert_eq!(bits.resident(0), 0);
        assert_eq!(bits.resident(1), 2);
        assert!(bits.is_present(1, 0) && !bits.is_present(1, 1) && bits.is_present(1, 2));
        assert!(!bits.is_present(0, 69));
    }

    #[test]
    fn an_elided_mapping_reports_nothing_rather_than_zero() {
        let mut r = RawPages::with_capacity(2);
        r.set(1, &[PRESENT]);
        let bits = PageBits::decode(&r);
        assert!(bits.is_elided(0));
        assert_eq!(bits.resident_fraction(0), None);
        assert!(!bits.is_elided(1));
        assert_eq!(bits.resident_fraction(1), Some(1.0));
    }

    #[test]
    fn a_page_past_the_end_is_absent_rather_than_a_panic() {
        let bits = PageBits::decode(&raw(&[&[PRESENT]]));
        assert!(!bits.is_present(0, 1));
        assert!(!bits.is_present(9, 0));
    }

    #[test]
    fn the_budget_takes_the_smallest_mappings_first() {
        let extents = [
            (0, 400 * PAGE),
            (1 << 40, (1 << 40) + 4 * PAGE),
            (1 << 41, (1 << 41) + 40 * PAGE),
        ];
        let limits = PageLimits {
            detail_pages: u64::MAX,
            // Room for 44 pages, which is the two small mappings and not the big one.
            budget_pages: 44,
        };
        assert_eq!(RawPages::plan(&extents, &limits), vec![1, 2]);
    }

    #[test]
    fn a_mapping_wider_than_the_detail_cap_is_never_read() {
        let extents = [(0, 1 << 40), (1 << 41, (1 << 41) + PAGE)];
        let chosen = RawPages::plan(&extents, &PageLimits::default());
        assert_eq!(chosen, vec![1]);
    }
}
