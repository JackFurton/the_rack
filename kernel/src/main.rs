//! the_rack: a kernel built from the reset vector up.
//!
//! Tier 0: get off the ground and say something.

#![no_std]
#![no_main]

pub mod semihosting;
pub mod uart;

use core::arch::{asm, global_asm};
use core::panic::PanicInfo;

global_asm!(include_str!("boot.S"));

unsafe extern "C" {
    static __kernel_end: u8;
}

/// Called by `boot.S` once core 0 has a stack and a zeroed BSS.
#[unsafe(no_mangle)]
pub extern "C" fn kernel_main() -> ! {
    uart::init();

    println!();
    println!("the_rack {}", env!("CARGO_PKG_VERSION"));
    println!("aarch64 / qemu-virt");
    println!();
    println!("  exception level : EL{}", current_el());
    println!("  kernel loaded   : {:#018x}", 0x4008_0000usize);
    println!(
        "  kernel end      : {:#018x}",
        &raw const __kernel_end as usize
    );
    println!();
    println!("tier 0 complete. we are alive on bare metal.");

    halt()
}

/// Which privilege level did the firmware drop us into?
///
/// CurrentEL holds the level in bits [3:2]. EL1 is normal kernel territory;
/// EL2 means we were handed the hypervisor level and will need to drop down
/// ourselves before tier 1's exception vectors make sense.
fn current_el() -> u64 {
    let el: u64;
    unsafe { asm!("mrs {}, CurrentEL", out(reg) el, options(nomem, nostack)) };
    (el >> 2) & 0b11
}

/// Park the core forever. Cheaper than a spin: `wfe` stops the core until
/// something wakes it, and nothing will.
pub fn halt() -> ! {
    loop {
        unsafe { asm!("wfe", options(nomem, nostack)) };
    }
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    println!();
    println!("!!! kernel panic !!!");
    if let Some(location) = info.location() {
        println!("  at {}:{}", location.file(), location.line());
    }
    println!("  {}", info.message());

    // Bring the machine down rather than hang. Under QEMU this exits with a
    // failure status, so the boot test reports a panic straight away instead
    // of waiting out its timeout wondering why the banner never arrived.
    semihosting::exit(semihosting::EXIT_FAILURE)
}
