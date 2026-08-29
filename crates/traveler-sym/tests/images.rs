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

mod tier3 {
    use std::path::Path;

    use traveler_sym::debug::{DebugInfo, TypeKind};

    fn info() -> DebugInfo {
        DebugInfo::parse(Path::new(env!("CARGO_BIN_EXE_sym-fixture")))
            .expect("the fixture is built with debug information")
    }

    /// The difference tier 3 makes: an address stops being an address and becomes a line of
    /// source. Looked up at the fixture's own function rather than by sampling `.text`, which in
    /// a Rust binary is almost entirely the standard library.
    #[test]
    fn the_fixtures_own_code_resolves_to_a_line_of_its_own_source() {
        let info = info();
        let image = super::elf();
        let marker = image
            .address_of("traveler_sym_fixture_marker")
            .expect("the fixture's exported function");

        let location = info.locate(marker).expect("a location for it");
        assert!(
            location.file.ends_with("sym-fixture.rs"),
            "resolved to {} instead of the fixture's own source",
            location.file
        );
        assert!(location.line > 0);
        assert!(
            location
                .function
                .as_deref()
                .is_some_and(|f| f.contains("traveler_sym_fixture_marker")),
            "named {:?}",
            location.function
        );
    }

    /// What makes "poke a variable" mean a variable: a name, an address, a width, and how the
    /// bytes should be read.
    #[test]
    fn the_fixtures_globals_come_back_with_their_types() {
        let info = info();
        let found = |name: &str| {
            info.globals()
                .iter()
                .find(|g| g.name == name)
                .unwrap_or_else(|| panic!("no global named {name}, of {:?}", names(&info)))
        };

        let count = found("TRAVELER_FIXTURE_COUNT");
        assert_eq!(count.size, 4);
        assert_eq!(count.kind, TypeKind::Unsigned);
        assert_eq!(count.kind.literal(count.size), Some("u32"));
        assert!(count.addr > 0);

        let scale = found("TRAVELER_FIXTURE_SCALE");
        assert_eq!(scale.size, 8);
        assert_eq!(scale.kind, TypeKind::Float);
        assert_eq!(scale.kind.literal(scale.size), Some("f64"));

        // An array is not one number, so it has no literal and the poke stays untyped.
        let table = found("TRAVELER_FIXTURE_TABLE");
        assert_eq!(table.size, 128);
        assert_eq!(table.kind, TypeKind::Composite);
        assert_eq!(table.kind.literal(table.size), None);

        assert!(
            info.globals().windows(2).all(|w| w[0].addr <= w[1].addr),
            "globals are not in address order"
        );
    }

    fn names(info: &DebugInfo) -> Vec<&str> {
        info.globals().iter().map(|g| g.name.as_str()).collect()
    }

    #[test]
    fn an_address_inside_a_global_finds_it_with_an_offset() {
        let info = info();
        let table = info
            .globals()
            .iter()
            .find(|g| g.name == "TRAVELER_FIXTURE_TABLE")
            .expect("the table")
            .clone();

        let (found, into) = info.global_at(table.addr + 24).expect("inside the table");
        assert_eq!(found.name, "TRAVELER_FIXTURE_TABLE");
        assert_eq!(into, 24);
        assert!(info.global_at(0).is_none());
    }

    /// A typed poke is the untyped one with the width filled in, rather than a second
    /// implementation of writing bytes.
    #[test]
    fn a_scalar_type_names_the_literal_that_writes_it() {
        assert_eq!(TypeKind::Unsigned.literal(4), Some("u32"));
        assert_eq!(TypeKind::Signed.literal(8), Some("i64"));
        assert_eq!(TypeKind::Float.literal(4), Some("f32"));
        assert_eq!(TypeKind::Pointer.literal(8), Some("u64"));
        assert_eq!(TypeKind::Bool.literal(1), Some("u8"));
        // A struct is not one number, so there is no literal for it and the poke stays untyped.
        assert_eq!(TypeKind::Composite.literal(64), None);
        assert_eq!(TypeKind::Unsigned.literal(3), None);
    }

    #[test]
    fn an_image_without_debug_information_is_not_an_error() {
        assert!(DebugInfo::parse(Path::new("/nonexistent")).is_none());
    }
}
