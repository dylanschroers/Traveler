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

fn parse_line(line: &str) -> Option<MappingFacts<'_>> {
    let (range, at) = field(line, 0)?;
    let (perms, at) = field(line, at)?;
    let (offset, at) = field(line, at)?;
    let (dev, at) = field(line, at)?;
    let (inode, at) = field(line, at)?;

    let (start, end) = range.split_once('-')?;
    let start = u64::from_str_radix(start, 16).ok()?;
    let end = u64::from_str_radix(end, 16).ok()?;
    if end < start {
        return None;
    }

    let (major, minor) = dev.split_once(':')?;
    let dev = (u64::from_str_radix(major, 16).ok()? << 32) | u64::from_str_radix(minor, 16).ok()?;

    Some(MappingFacts {
        addr: start,
        len: end - start,
        prot: Prot::parse(perms),
        path: line[at..].trim_start(),
        offset: u64::from_str_radix(offset, 16).ok()?,
        dev,
        inode: inode.parse().ok()?,
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
    }
}
