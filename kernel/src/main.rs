//! the_rack: a kernel built from the reset vector up.
//!
//! Tier 0: get off the ground and say something.

#![no_std]
#![no_main]

pub mod exceptions;
pub mod fdt;
pub mod frames;
pub mod gic;
pub mod ipc;
pub mod notify;
pub mod paging;
pub mod semihosting;
pub mod sync;
pub mod syscall;
pub mod tasks;
pub mod timer;
pub mod uart;

use core::arch::{asm, global_asm};
use core::panic::PanicInfo;

global_asm!(include_str!("boot.S"));
global_asm!(include_str!("vectors.S"));
global_asm!(include_str!("user.S"));

unsafe extern "C" {
    static __kernel_end: u8;
}

/// Called by `boot.S` once core 0 has a stack and a zeroed BSS.
///
/// `dtb` is the physical address of the device tree blob, straight from the x0
/// the firmware entered with. Nothing is done with it here: reading it means
/// reading memory, and the only sane way to say where memory is happens after
/// the MMU is on. It is carried across the jump and looked at there.
///
/// Runs at physical addresses with the MMU off, and is deliberately tiny.
///
/// Nothing here may use `println!`. The kernel is linked at its high half
/// address, so the function pointers `format_args!` places in `.rodata` are
/// high addresses, and nothing translates yet. Only `uart::emergency_print`
/// works, and only with string literals.
///
/// The job is to get the MMU on and get out.
#[unsafe(no_mangle)]
pub extern "C" fn kernel_main(dtb: u64) -> ! {
    uart::emergency_print("\nthe_rack: booting, mmu off\n");

    // PC relative addressing means the linker symbols this reads resolve to
    // physical addresses right now, and to high ones after the jump, with no
    // relocation and no special casing.
    frames::init();
    uart::emergency_print("the_rack: frame allocator up, building page tables\n");

    // Build both roots while the MMU is off, enable with the identity map
    // holding the program counter steady, then leave the low half behind.
    let (ttbr0, ttbr1) = paging::build_tables();
    uart::emergency_print("the_rack: tables built, enabling mmu\n");

    unsafe { paging::enable(ttbr0, ttbr1) };
    uart::emergency_print("the_rack: mmu on, jumping to the high half\n");

    unsafe { paging::jump_to_high_half(kernel_main_high, dtb) }
}

/// Everything from here runs at a high half virtual address.
///
/// Separate function because the jump is a branch to a computed address, not a
/// call: there is no return path to the low half, and the low half is about to
/// stop existing.
extern "C" fn kernel_main_high(dtb: u64) -> ! {
    // Nothing may touch a device register or a frame before this: the physical
    // addresses they hold are no longer the addresses that work.
    unsafe { paging::finish_high_half() };

    uart::init();

    // The vector table's address is only meaningful now. Everything before
    // this point faults silently, which is the price of linking high.
    exceptions::init();

    println!();
    println!("the_rack {}", env!("CARGO_PKG_VERSION"));
    println!("aarch64 / qemu-virt");
    println!();
    println!("  exception level : EL{}", current_el());
    println!("  kernel loaded   : {:#018x} physical", 0x4008_0000usize);
    println!(
        "  kernel end      : {:#018x} virtual",
        &raw const __kernel_end as usize
    );
    println!("  vector table    : {:#018x}", exceptions::vector_base());
    println!();

    paging::print_config();
    println!();
    frames::print_map();
    println!();

    // The first thing this kernel has been told rather than told itself.
    // Nothing depends on it yet: every address it would supply is still a
    // constant, and will be until #42 and #43. Getting it validated and
    // printed first means the rest of tier 4 starts from something known good.
    match fdt::init(dtb) {
        Ok(blob) => {
            fdt::print_info(&blob);
            reserve_blob(&blob);
        }
        Err(error) => {
            println!(
                "device tree: none ({}), carrying on with constants",
                error.describe()
            );
        }
    }
    println!();

    exceptions::self_test();
    fdt::self_test();
    fdt::tree_self_test();
    sync::self_test();
    frames::self_test();
    paging::self_test();

    // Needed before any task switches, since switching now swaps TTBR0.
    tasks::init_address_spaces();
    tasks::self_test();
    println!();
    tasks::print_table();

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
    println!("tier 1: paging online, kernel in the high half.");

    // Everything is armed. From here the machine runs on its own.
    sync::enable_interrupts();
    println!("tier 1: heartbeat started, interrupts live.");
    println!();

    // Needs live interrupts, so it cannot run with the other self tests.
    tasks::preemption_self_test();
    tasks::isolation_self_test();
    tasks::user_self_test();
    tasks::lifecycle_self_test();
    tasks::fault_self_test();
    tasks::priority_self_test();
    tasks::ipc_self_test();
    tasks::lease_self_test();
    tasks::supervisor_self_test();
    tasks::notification_self_test();
    tasks::forged_reply_self_test();
    println!("tier 2: preemptive scheduling online.");
    println!("tier 2: EL0 and syscalls online.");
    println!("tier 3: task faults are contained.");
    println!("tier 3: priority scheduling and blocking online.");
    println!("tier 3: synchronous IPC online.");
    println!("tier 3: leases online.");
    println!("tier 3: supervised restart online.");
    println!();

    // Last, and never finishes. From here the heartbeat on the console is
    // printed by an unprivileged task that the timer interrupt wakes, and the
    // kernel's part in a tick is re-arming the deadline and setting one bit.
    tasks::spawn_heartbeat();
    println!("tier 3: notifications online, heartbeat now runs at EL0.");
    println!("tier 4: the machine describes itself, device tree in hand.");
    println!();

    halt()
}

/// Keep the allocator's hands off the device tree.
///
/// QEMU does not put the blob at the base of RAM, which is what this project
/// assumed until it could actually see one: on `virt` it lands 128 MiB up,
/// well past the kernel image and squarely inside the pool. Nothing has read
/// the tree yet, so the corruption would have been invisible until #41 tried
/// to walk it and found somebody's page table in the middle.
///
/// A failure here is a warning rather than a panic. The tree is not load
/// bearing yet, and a kernel that boots and complains is more useful than one
/// that dies over memory it is not using.
fn reserve_blob(blob: &fdt::Blob) {
    let size = blob.header.totalsize as u64;

    match frames::reserve_range(blob.pa, size) {
        Ok(count) => {
            assert!(frames::is_taken(blob.pa), "the first frame is not held");
            assert!(
                frames::is_taken(blob.pa + size - 1),
                "the last frame is not held"
            );
            println!("  reserved      : {} frames", count);
        }
        Err(error) => {
            println!("  WARNING       : could not reserve the blob ({:?})", error);
        }
    }
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
