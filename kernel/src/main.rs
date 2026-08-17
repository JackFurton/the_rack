//! the_rack: a kernel built from the reset vector up.
//!
//! Tier 0: get off the ground and say something.

#![no_std]
#![no_main]

pub mod exceptions;
pub mod frames;
pub mod gic;
pub mod semihosting;
pub mod sync;
pub mod timer;
pub mod uart;

use core::arch::{asm, global_asm};
use core::panic::PanicInfo;

global_asm!(include_str!("boot.S"));
global_asm!(include_str!("vectors.S"));

unsafe extern "C" {
    static __kernel_end: u8;
}

/// Called by `boot.S` once core 0 has a stack and a zeroed BSS.
#[unsafe(no_mangle)]
pub extern "C" fn kernel_main() -> ! {
    uart::init();

    // Before anything else that could fault. Until VBAR_EL1 is set, a fault
    // produces silence rather than a report.
    exceptions::init();

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
    println!("  vector table    : {:#018x}", exceptions::vector_base());
    println!();

    frames::init();
    println!();
    frames::print_map();
    println!();

    exceptions::self_test();
    sync::self_test();
    frames::self_test();

    // Interrupt controller first: unmasking IRQs in PSTATE achieves nothing
    // until something is willing to forward one.
    gic::init();
    println!();
    println!("gic: {} interrupt lines", gic::num_interrupts());

    timer::init();
    println!(
        "timer: {} Hz counter, tick every {} ms",
        timer::frequency(),
        1000 / timer::TICK_HZ
    );

    println!();
    println!("tier 0 complete. we are alive on bare metal.");
    println!("tier 1: exception vectors online.");

    // Everything is armed. From here the machine runs on its own.
    sync::enable_interrupts();
    println!("tier 1: heartbeat started, interrupts live.");
    println!();

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

/// Park the core forever.
///
/// `wfi`, not `wfe`. The two look interchangeable and are not. `wfe` sleeps
/// only until the event register is set, and both real cores and QEMU's TCG
/// treat it as close to a no-op when nothing is going to signal an event, so a
/// `wfe` loop spins at full tilt. Under QEMU that is a host core pinned at
/// 100% by a kernel that is doing nothing at all.
///
/// `wfi` genuinely stops the core until an interrupt arrives, and QEMU halts
/// the vCPU thread for it. It is also the right primitive for the eventual
/// idle task, so the scheduler in tier 2 wants this shape anyway.
pub fn halt() -> ! {
    loop {
        unsafe { asm!("wfi", options(nomem, nostack)) };
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
