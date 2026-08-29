//! Tier 3: what the debug information says, when there is any.
//!
//! Tiers 1 and 2 answer "where is this and what is it made of" from the operating system and the
//! image. This answers "what *is* it": which line of which file an address belongs to, and what
//! type a global has. That is the difference between poking an address and setting a field.
//!
//! Every part of it is allowed to be missing. A stripped binary has no `.debug_info`, a distro one
//! keeps it in a separate file, and the view is tier 2 either way.

use std::path::{Path, PathBuf};

use std::collections::HashMap;
use std::sync::Mutex;

use gimli::{AttributeValue, Dwarf, EndianSlice, RunTimeEndian};
use object::{Object, ObjectSection, ObjectSymbol};

/// A global variable the debug information named, with enough about its type to write to it.
#[derive(Clone, Debug)]
pub struct Global {
    pub name: String,
    /// Link-view address. Add the load bias, exactly as for a section.
    pub addr: u64,
    pub size: u64,
    /// The type's own name, for the readout.
    pub type_name: String,
    pub kind: TypeKind,
}

/// Enough of a type to make a poke mean something. Not a type system: the question this has to
/// answer is "how many bytes, and how should the number be read".
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TypeKind {
    Signed,
    Unsigned,
    Float,
    Bool,
    Pointer,
    /// A struct, an array, or anything else that is not one scalar.
    Composite,
}

impl TypeKind {
    /// The literal prefix `traveler-app`'s poke parser understands, so a typed poke is the
    /// untyped one with the width filled in rather than a second implementation.
    pub fn literal(self, size: u64) -> Option<&'static str> {
        Some(match (self, size) {
            (TypeKind::Unsigned | TypeKind::Bool, 1) => "u8",
            (TypeKind::Unsigned, 2) => "u16",
            (TypeKind::Unsigned, 4) => "u32",
            (TypeKind::Unsigned | TypeKind::Pointer, 8) => "u64",
            (TypeKind::Signed, 1) => "i8",
            (TypeKind::Signed, 2) => "i16",
            (TypeKind::Signed, 4) => "i32",
            (TypeKind::Signed, 8) => "i64",
            (TypeKind::Float, 4) => "f32",
            (TypeKind::Float, 8) => "f64",
            _ => return None,
        })
    }
}

#[derive(Clone, Debug)]
pub struct Location {
    pub file: String,
    pub line: u32,
    pub function: Option<String>,
}

pub struct DebugInfo {
    /// Behind a lock because `addr2line` fills lazy caches as it answers, which makes the context
    /// `Send` but not `Sync`. Confined here rather than pushed onto every caller: a lookup is
    /// rare, and a shared reference to this is not.
    context: Mutex<addr2line::Context<EndianSlice<'static, RunTimeEndian>>>,
    /// Sorted by address, so a lookup is a binary search like the symbol table's.
    globals: Vec<Global>,
    /// The mapped file, kept alive because everything above borrows out of it.
    _data: &'static [u8],
}

impl DebugInfo {
    /// `None` when the image carries no debug information, which is the common case and not an
    /// error.
    pub fn parse(path: &Path) -> Option<DebugInfo> {
        let data = std::fs::read(path).ok()?;
        // Leaked on purpose: an image's debug info is parsed once and lives as long as the
        // session, and the alternative is a self-referential struct for no gain.
        let data: &'static [u8] = Box::leak(data.into_boxed_slice());
        let file = object::File::parse(data).ok()?;
        // No debug information is the common case, not an error.
        file.section_by_name(".debug_info")?;

        let endian = if file.is_little_endian() {
            RunTimeEndian::Little
        } else {
            RunTimeEndian::Big
        };
        let load = |id: gimli::SectionId| -> Result<EndianSlice<'static, RunTimeEndian>, ()> {
            let bytes = file
                .section_by_name(id.name())
                .and_then(|s| s.data().ok())
                .unwrap_or(&[]);
            Ok(EndianSlice::new(bytes, endian))
        };
        let dwarf = Dwarf::load(load).ok()?;
        // Where a static's address comes from when the debug information does not carry one.
        let mut placed: HashMap<&str, (u64, u64)> = HashMap::new();
        let mut sized: HashMap<u64, u64> = HashMap::new();
        for symbol in file.symbols().filter(|s| s.address() > 0) {
            if let Ok(name) = symbol.name() {
                placed.insert(name, (symbol.address(), symbol.size()));
            }
            sized.insert(symbol.address(), symbol.size());
        }
        let globals = globals(&dwarf, &placed, &sized).unwrap_or_default();
        let context = addr2line::Context::from_dwarf(dwarf).ok()?;

        Some(DebugInfo {
            context: Mutex::new(context),
            globals,
            _data: data,
        })
    }

    /// Which line of which file a link-view address belongs to.
    pub fn locate(&self, addr: u64) -> Option<Location> {
        let context = self.context.lock().ok()?;
        let mut frames = context.find_frames(addr).skip_all_loads().ok()?;
        let frame = frames.next().ok()??;
        let location = frame.location.as_ref();
        Some(Location {
            file: location
                .and_then(|l| l.file)
                .unwrap_or_default()
                .to_string(),
            line: location.and_then(|l| l.line).unwrap_or(0),
            function: frame
                .function
                .as_ref()
                .and_then(|f| f.raw_name().ok())
                .map(|n| n.to_string()),
        })
    }

    pub fn globals(&self) -> &[Global] {
        &self.globals
    }

    /// The global containing a link-view address, and how far into it the address is.
    pub fn global_at(&self, addr: u64) -> Option<(&Global, u64)> {
        let i = match self.globals.binary_search_by_key(&addr, |g| g.addr) {
            Ok(i) => i,
            Err(0) => return None,
            Err(i) => i - 1,
        };
        let global = &self.globals[i];
        (addr < global.addr + global.size.max(1)).then(|| (global, addr - global.addr))
    }
}

/// Every `DW_TAG_variable` this can place, which is what a global is.
///
/// Two ways to place one, and the second was not obvious. C and C++ give the variable a
/// `DW_AT_location` holding `DW_OP_addr`. **Rust does not**: it emits the type and a
/// `DW_AT_linkage_name` and leaves the address to the ELF symbol table, so tier 3 has to join the
/// two. Requiring a location finds one global in a Rust binary — a vtable — and misses every
/// static in the program.
fn globals(
    dwarf: &Dwarf<EndianSlice<'_, RunTimeEndian>>,
    placed: &HashMap<&str, (u64, u64)>,
    sized: &HashMap<u64, u64>,
) -> gimli::Result<Vec<Global>> {
    let mut out: Vec<Global> = Vec::new();
    let mut units = dwarf.units();
    while let Some(header) = units.next()? {
        let unit = dwarf.unit(header)?;
        let mut entries = unit.entries();
        while let Some((_, entry)) = entries.next_dfs()? {
            if entry.tag() != gimli::DW_TAG_variable {
                continue;
            }
            let text = |at: gimli::DwAt| -> Option<String> {
                let attr = entry.attr(at).ok()??;
                let s = dwarf.attr_string(&unit, attr.value()).ok()?;
                s.to_string().ok().map(str::to_owned)
            };
            let Some(name) = text(gimli::DW_AT_name) else {
                continue;
            };
            let linkage = text(gimli::DW_AT_linkage_name);
            let symbol = linkage.as_deref().and_then(|l| placed.get(l)).copied();
            let Some(addr) = static_address(entry)?.or(symbol.map(|(a, _)| a)) else {
                continue;
            };

            let (type_name, kind, size) = match entry.attr(gimli::DW_AT_type)? {
                Some(attr) => describe(dwarf, &unit, attr.value())?,
                None => ("?".to_string(), TypeKind::Composite, 0),
            };
            // The type's size when DWARF states one, and the symbol's when it does not. An
            // array states its extent through `DW_TAG_subrange_type` bounds rather than a byte
            // size, so a fallback is needed for the common case rather than the odd one.
            let size = if size > 0 {
                size
            } else {
                symbol
                    .map(|(_, s)| s)
                    .filter(|s| *s > 0)
                    .or_else(|| sized.get(&addr).copied())
                    .unwrap_or(0)
            };
            out.push(Global {
                name,
                addr,
                size,
                type_name,
                kind,
            });
        }
    }
    out.sort_unstable_by_key(|g| g.addr);
    out.dedup_by_key(|g| g.addr);
    Ok(out)
}

/// A `DW_OP_addr` location, when there is one. Anything else is on a stack or in a register,
/// which is phase 9's problem rather than this one's.
fn static_address(
    entry: &gimli::DebuggingInformationEntry<'_, '_, EndianSlice<'_, RunTimeEndian>>,
) -> gimli::Result<Option<u64>> {
    let Some(attr) = entry.attr(gimli::DW_AT_location)? else {
        return Ok(None);
    };
    let AttributeValue::Exprloc(expression) = attr.value() else {
        return Ok(None);
    };
    let mut operations = expression.operations(gimli::Encoding {
        address_size: 8,
        format: gimli::Format::Dwarf32,
        version: 5,
    });
    match operations.next()? {
        Some(gimli::Operation::Address { address }) if address > 0 => Ok(Some(address)),
        _ => Ok(None),
    }
}

/// A type's name, how to read it, and how wide it is, following typedefs and const wrappers to
/// whatever is underneath.
fn describe<'a>(
    dwarf: &Dwarf<EndianSlice<'a, RunTimeEndian>>,
    unit: &gimli::Unit<EndianSlice<'a, RunTimeEndian>>,
    mut value: AttributeValue<EndianSlice<'a, RunTimeEndian>>,
) -> gimli::Result<(String, TypeKind, u64)> {
    let mut name = String::new();
    // Bounded, because a malformed or circular chain must not spin.
    for _ in 0..8 {
        let AttributeValue::UnitRef(offset) = value else {
            break;
        };
        let entry = unit.entry(offset)?;
        if name.is_empty()
            && let Some(attr) = entry.attr(gimli::DW_AT_name)?
            && let Ok(s) = dwarf.attr_string(unit, attr.value())
            && let Ok(text) = s.to_string()
        {
            name = text.to_string();
        }
        let size = entry
            .attr(gimli::DW_AT_byte_size)?
            .and_then(|a| a.udata_value())
            .unwrap_or(0);

        let kind = match entry.tag() {
            gimli::DW_TAG_pointer_type => Some(TypeKind::Pointer),
            gimli::DW_TAG_base_type => Some(match encoding(&entry)? {
                gimli::DW_ATE_boolean => TypeKind::Bool,
                gimli::DW_ATE_float => TypeKind::Float,
                gimli::DW_ATE_signed | gimli::DW_ATE_signed_char => TypeKind::Signed,
                _ => TypeKind::Unsigned,
            }),
            gimli::DW_TAG_structure_type
            | gimli::DW_TAG_array_type
            | gimli::DW_TAG_union_type
            | gimli::DW_TAG_class_type
            | gimli::DW_TAG_enumeration_type => Some(TypeKind::Composite),
            _ => None,
        };
        if let Some(kind) = kind {
            let size = if kind == TypeKind::Pointer && size == 0 {
                8
            } else {
                size
            };
            return Ok((if name.is_empty() { "?".into() } else { name }, kind, size));
        }
        // A typedef, const, volatile or similar wrapper: keep going to what it wraps.
        match entry.attr(gimli::DW_AT_type)? {
            Some(next) => value = next.value(),
            None => break,
        }
    }
    Ok((
        if name.is_empty() { "?".into() } else { name },
        TypeKind::Composite,
        0,
    ))
}

fn encoding(
    entry: &gimli::DebuggingInformationEntry<'_, '_, EndianSlice<'_, RunTimeEndian>>,
) -> gimli::Result<gimli::DwAte> {
    Ok(entry
        .attr(gimli::DW_AT_encoding)?
        .and_then(|a| match a.value() {
            AttributeValue::Encoding(e) => Some(e),
            _ => None,
        })
        .unwrap_or(gimli::DW_ATE_unsigned))
}

/// Where a distro binary keeps its debug information: `.note.gnu.build-id` points at a file under
/// `/usr/lib/debug/.build-id`. Cheap to check, and it upgrades tier 2 to tier 3 for free.
pub fn separate_debug_file(path: &Path) -> Option<PathBuf> {
    let data = std::fs::read(path).ok()?;
    let file = object::File::parse(&*data).ok()?;
    let id = file.build_id().ok()??;
    let (first, rest) = id.split_first()?;
    let mut out = String::with_capacity(rest.len() * 2 + 3);
    out.push_str(&format!("{first:02x}/"));
    for byte in rest {
        out.push_str(&format!("{byte:02x}"));
    }
    out.push_str(".debug");
    let candidate = Path::new("/usr/lib/debug/.build-id").join(out);
    candidate.exists().then_some(candidate)
}
