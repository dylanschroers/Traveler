//! A target with an address space known ahead of time.
//!
//! The capture tests assert the snapshot sees *exactly* these mappings. A city that looks
//! plausible and is quietly wrong is the failure mode that matters, and only a fixture whose
//! answer is known in advance catches it.

#[cfg(not(target_os = "linux"))]
fn main() {}

#[cfg(target_os = "linux")]
fn main() {
    use std::io::Write;

    const PAGE: usize = 4096;
    const ARENA: usize = 64 * PAGE;
    const WINDOW_OFF: usize = 16 * PAGE;
    const WINDOW: usize = 16 * PAGE;
    const CODE: usize = 8 * PAGE;
    const MARKER: u64 = 0x5445_4c45_5641_524d;

    let threads: usize = std::env::args()
        .nth(1)
        .and_then(|a| a.strip_prefix("--threads=").map(str::to_owned))
        .and_then(|n| n.parse().ok())
        .unwrap_or(0);

    // One reservation, then a window mprotected inside it. The three mappings that result are
    // adjacent and anonymous, so they must come back as one region with three children.
    let arena = map(ARENA, libc::PROT_NONE);
    unsafe {
        assert_eq!(
            libc::mprotect(
                arena.add(WINDOW_OFF).cast(),
                WINDOW,
                libc::PROT_READ | libc::PROT_WRITE
            ),
            0
        );
    }
    let code = map(CODE, libc::PROT_READ | libc::PROT_EXEC);

    let window = unsafe { arena.add(WINDOW_OFF) };
    unsafe {
        // Two touched pages with a resident gap between them, for the residency tests.
        std::ptr::write_volatile(window.cast::<u64>(), MARKER);
        std::ptr::write_volatile(window.add(2 * PAGE).cast::<u64>(), MARKER.wrapping_add(1));
    }

    let mut out = std::io::stdout();
    writeln!(out, "arena {:x} {:x}", arena as usize, ARENA).unwrap();
    writeln!(out, "window {:x} {:x}", window as usize, WINDOW).unwrap();
    writeln!(out, "code {:x} {:x}", code as usize, CODE).unwrap();
    writeln!(out, "marker {MARKER:x}").unwrap();

    for _ in 0..threads {
        let target = window as usize;
        std::thread::spawn(move || {
            let mut n: u64 = 0;
            loop {
                // Keep writing, so a capture that does not stop every thread sees memory move
                // underneath it.
                unsafe { std::ptr::write_volatile((target + 3 * PAGE) as *mut u64, n) };
                n = n.wrapping_add(1);
            }
        });
    }

    writeln!(out, "ready {threads}").unwrap();
    out.flush().unwrap();

    loop {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

#[cfg(target_os = "linux")]
fn map(len: usize, prot: libc::c_int) -> *mut u8 {
    let p = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            len,
            prot,
            libc::MAP_PRIVATE | libc::MAP_ANONYMOUS,
            -1,
            0,
        )
    };
    assert_ne!(p, libc::MAP_FAILED, "mmap failed");
    p.cast()
}
