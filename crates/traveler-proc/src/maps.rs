//! Hand parser for `/proc/<pid>/maps`.
//!
//! The format is fixed, so a hand parser beats a regex by an order of magnitude, and on a big
//! process this runs over ten thousand lines. It borrows from the slurped text and allocates only
//! the output vector, because §7 spends its budget on the stop and nothing else.

use crate::space::{MappingFacts, Prot};

pub fn parse(text: &str) -> Vec<MappingFacts<'_>> {
    let mut out = Vec::with_capacity(text.len() / 80);
    for line in text.lines() {
        if let Some(m) = parse_line(line) {
            out.push(m);
        }
    }
    out
}

/// Just the address ranges, which is all the pagemap read needs to know.
///
/// §7 says nothing is parsed while the target is stopped, and this is the one exception the rule
/// cannot avoid: pagemap is indexed by address, so the ranges have to be known before the read
/// that the stop exists for. It is a scan of the first field of each line and nothing else, and it
/// accepts exactly the lines [`parse`] accepts, so the two are index-aligned by construction.
pub fn extents(text: &str) -> Vec<(u64, u64)> {
    let mut out = Vec::with_capacity(text.len() / 80);
    for line in text.lines() {
        if let Some(range) = parse_range(line) {
            out.push(range);
        }
    }
    out
}

fn parse_range(line: &str) -> Option<(u64, u64)> {
    let (range, _) = field(line, 0)?;
    let (start, end) = range.split_once('-')?;
    let start = u64::from_str_radix(start, 16).ok()?;
    let end = u64::from_str_radix(end, 16).ok()?;
    (end >= start).then_some((start, end))
}

/// A line is accepted on its range alone, so that [`extents`] and [`parse`] can never disagree
/// about how many mappings there are. Real `maps` output never has a malformed tail; junk input
/// gets defaults rather than shifting every index after it.
fn parse_line(line: &str) -> Option<MappingFacts<'_>> {
    let (start, end) = parse_range(line)?;

    let mut at = field(line, 0).map_or(0, |(_, at)| at);
    let mut next = || {
        let (text, past) = field(line, at)?;
        at = past;
        Some(text)
    };
    let perms = next().unwrap_or("----");
    let offset = next().and_then(|v| u64::from_str_radix(v, 16).ok());
    let dev = next().and_then(|v| {
        let (major, minor) = v.split_once(':')?;
        Some((u64::from_str_radix(major, 16).ok()? << 32) | u64::from_str_radix(minor, 16).ok()?)
    });
    let inode = next().and_then(|v| v.parse().ok());

    Some(MappingFacts {
        addr: start,
        len: end - start,
        prot: Prot::parse(perms),
        path: line[at..].trim_start(),
        offset: offset.unwrap_or(0),
        dev: dev.unwrap_or(0),
        inode: inode.unwrap_or(0),
        group: 0,
    })
}

/// The next space-delimited field and the index just past it. The kernel pads to a fixed column
/// before the path, so the separator is a run of spaces rather than one.
fn field(line: &str, from: usize) -> Option<(&str, usize)> {
    let bytes = line.as_bytes();
    let mut i = from;
    while i < bytes.len() && bytes[i] == b' ' {
        i += 1;
    }
    let start = i;
    while i < bytes.len() && bytes[i] != b' ' {
        i += 1;
    }
    (start != i).then(|| (&line[start..i], i))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
55d1c0a00000-55d1c0a02000 r--p 00000000 08:02 1179657                    /usr/bin/cat
55d1c0a02000-55d1c0a07000 r-xp 00002000 08:02 1179657                    /usr/bin/cat
7f2a1c000000-7f2a1c021000 rw-p 00000000 00:00 0 
7ffd4b7e0000-7ffd4b801000 rw-p 00000000 00:00 0                          [stack]
ffffffffff600000-ffffffffff601000 --xp 00000000 00:00 0                  [vsyscall]
";

    #[test]
    fn reads_every_field_of_a_line() {
        let m = parse(SAMPLE);
        assert_eq!(m.len(), 5);
        assert_eq!(m[0].addr, 0x55d1c0a00000);
        assert_eq!(m[0].len, 0x2000);
        assert_eq!(m[0].path, "/usr/bin/cat");
        assert_eq!(m[0].inode, 1179657);
        assert_eq!(m[0].dev, (8 << 32) | 2);
        assert_eq!(m[0].offset, 0);
        assert!(m[0].prot.is_read() && !m[0].prot.is_write() && !m[0].prot.is_exec());
        assert!(!m[0].prot.is_shared());
    }

    #[test]
    fn an_offset_and_an_executable_mapping_survive() {
        let m = parse(SAMPLE);
        assert_eq!(m[1].offset, 0x2000);
        assert!(m[1].prot.is_exec());
        assert_eq!(m[1].prot.to_string(), "r-xp");
    }

    /// An anonymous line ends in a trailing space with no path, which is where a naive split
    /// picks up a field that is not there.
    #[test]
    fn anonymous_memory_has_no_path() {
        let m = parse(SAMPLE);
        assert_eq!(m[2].path, "");
        assert_eq!(m[2].inode, 0);
    }

    #[test]
    fn pseudo_paths_are_kept_whole() {
        let m = parse(SAMPLE);
        assert_eq!(m[3].path, "[stack]");
        assert_eq!(m[4].path, "[vsyscall]");
        assert_eq!(m[4].prot.to_string(), "--xp");
    }

    #[test]
    fn a_path_with_spaces_is_not_cut_short() {
        let line = "10000-11000 r--s 00000000 08:02 99                    /tmp/two words.so";
        let m = parse(line);
        assert_eq!(m[0].path, "/tmp/two words.so");
        assert!(m[0].prot.is_shared());
    }

    #[test]
    fn junk_lines_are_dropped_rather_than_guessed_at() {
        assert!(parse("not a maps line\n\n").is_empty());
        assert!(extents("not a maps line\n\n").is_empty());
    }

    /// The pagemap read indexes mappings by position, so a line the range scan keeps and the full
    /// parse drops would silently shift every mapping after it.
    #[test]
    fn the_range_scan_and_the_full_parse_always_agree() {
        let messy = format!("{SAMPLE}10000-11000 rubbish\nnonsense\n20000-21000 r--p\n");
        let full = parse(&messy);
        let ranges = extents(&messy);
        assert_eq!(full.len(), ranges.len());
        for (m, (start, end)) in full.iter().zip(ranges) {
            assert_eq!((m.addr, m.addr + m.len), (start, end));
        }
    }
}
