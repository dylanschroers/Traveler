//! An image with a known shape: sections the tests can name and a symbol they can find.

static ROD: [u8; 64] = [7; 64];
static mut ZEROED: [u8; 8192] = [0; 8192];
static mut WRITTEN: [u8; 64] = [1; 64];

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
        total += (*zeroed)[0] as u64 + (*written)[0] as u64;
    }
    println!("{total}");
}
