//! An image with a known shape: sections the tests can name and a symbol they can find.

static ROD: [u8; 64] = [7; 64];
static mut ZEROED: [u8; 8192] = [0; 8192];
static mut WRITTEN: [u8; 64] = [1; 64];

// Exported on purpose. A plain `static` used only inside one crate is internalised even at
// opt-level 1, so it ends up with a type in the debug information and no address anywhere — which
// is exactly the case tier 3 cannot place, and not the case the tests are for.
// `#[used]` as well as `#[no_mangle]`: in a binary nothing outside can reference a static, so
// without it the optimiser folds the value into its uses and the symbol never reaches the table.
#[used]
#[unsafe(no_mangle)]
pub static TRAVELER_FIXTURE_COUNT: u32 = 0xabcd_1234;

#[used]
#[unsafe(no_mangle)]
pub static TRAVELER_FIXTURE_SCALE: f64 = 1.5;

#[unsafe(no_mangle)]
pub static mut TRAVELER_FIXTURE_TABLE: [u64; 16] = [0; 16];

// Same reason as the statics: nothing outside a binary can call it, so without a reference the
// optimiser internalises it and the symbol never reaches the table.
#[used]
static KEEP_MARKER: extern "C" fn(u64) -> u64 = traveler_sym_fixture_marker;

#[unsafe(no_mangle)]
pub extern "C" fn traveler_sym_fixture_marker(n: u64) -> u64 {
    n.wrapping_mul(2_654_435_761)
}

fn main() {
    let mut total = traveler_sym_fixture_marker(ROD[0] as u64);
    unsafe {
        let zeroed = &raw mut ZEROED;
        let written = &raw mut WRITTEN;
        (*zeroed)[0] = 1;
        let table = &raw mut TRAVELER_FIXTURE_TABLE;
        (*table)[0] = TRAVELER_FIXTURE_COUNT as u64;
        total += (*zeroed)[0] as u64 + (*written)[0] as u64 + (*table)[0];
    }
    total += TRAVELER_FIXTURE_SCALE as u64;
    println!("{total}");
}
