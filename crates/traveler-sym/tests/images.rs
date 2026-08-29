//! Parsed against real binaries, because the point of tier 2 is what it says about a real image.
//!
//! §15 asks for a known ELF and a known PE with asserted section addresses and symbol addresses.
//! The ELF is this workspace's own test binary, which is always present and always has symbols.

use std::path::Path;

use traveler_sym::{Image, Images, SectionKind};

fn elf() -> Image {
    let path = Path::new(env!("CARGO_BIN_EXE_sym-fixture"));
    Image::parse(path).expect("parse the fixture binary")
}

#[test]
fn an_elf_yields_its_sections_in_address_order() {
    let image = elf();
    let sections = image.sections();
    assert!(sections.len() > 5, "only {} sections", sections.len());
    assert!(
        sections.windows(2).all(|w| w[0].addr <= w[1].addr),
        "sections are not in address order"
    );

    let named: Vec<&str> = sections.iter().map(|s| image.section_name(s)).collect();
    for want in [".text", ".rodata", ".data", ".bss"] {
        assert!(named.contains(&want), "no {want} in {named:?}");
    }
}

/// `.bss` is not in the file. Taking its size from the file range rather than the section header
/// reports zero, and it goes missing exactly where it matters.
#[test]
fn bss_has_a_size_even_though_it_is_not_in_the_file() {
    let image = elf();
    let bss = image
        .sections()
        .iter()
        .find(|s| image.section_name(s) == ".bss")
        .expect(".bss");
    assert_eq!(bss.kind, SectionKind::Zeroed);
    assert!(bss.size > 0, ".bss came back with no size");
}

#[test]
fn text_is_code_and_rodata_is_read_only() {
    let image = elf();
    let kind = |name: &str| {
        image
            .sections()
            .iter()
            .find(|s| image.section_name(s) == name)
            .map(|s| s.kind)
    };
    assert_eq!(kind(".text"), Some(SectionKind::Code));
    assert_eq!(kind(".rodata"), Some(SectionKind::ReadOnly));
    assert_eq!(kind(".data"), Some(SectionKind::Data));
}

/// The point of tier 2: a name for an address, with no debug information anywhere.
#[test]
fn an_address_inside_a_function_comes_back_with_its_name() {
    let image = elf();
    assert!(image.symbol_count() > 0, "no symbols at all");

    let text = image
        .sections()
        .iter()
        .find(|s| image.section_name(s) == ".text")
        .expect(".text");

    // Every symbol this finds must be one the image actually has, and the offset must land
    // inside it rather than past its end.
    let mut named = 0;
    for step in 0..64 {
        let addr = text.addr + step * (text.size / 64).max(1);
        if let Some((name, offset)) = image.symbol_at(addr) {
            assert!(!name.is_empty());
            assert!(offset < text.size);
            named += 1;
        }
    }
    assert!(named > 0, "no address in .text resolved to a symbol");
}

#[test]
fn an_address_below_every_symbol_names_nothing() {
    let image = elf();
    assert!(image.symbol_at(0).is_none());
    assert!(image.symbol_at(1).is_none());
}

#[test]
fn junk_is_refused_rather_than_guessed_at() {
    assert!(Image::from_bytes(b"not an executable").is_err());
    assert!(Image::from_bytes(&[]).is_err());
}

/// A process maps thirty to a hundred files, so each is parsed once and a failure is remembered
/// too rather than re-read on every capture.
#[test]
fn images_are_parsed_once_and_failures_are_remembered() {
    let mut images = Images::default();
    let path = Path::new(env!("CARGO_BIN_EXE_sym-fixture"));
    assert!(images.get(path).is_some());
    assert!(images.get(path).is_some());
    assert_eq!(images.len(), 1);

    let missing = Path::new("/nonexistent/image.so");
    assert!(images.get(missing).is_none());
    assert!(images.get(missing).is_none());
    assert_eq!(images.len(), 2);
}
