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
pub mod virtio;

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

    // The header of the device tree, and nothing else. Reading the tree
    // properly means comparing strings, and the address of a string literal
    // here is a high half address that nothing translates yet, so that has to
    // wait for the MMU. The header is ten `u32` at fixed offsets and needs no
    // strings at all, which is enough to learn how much room the blob takes.
    let blob = fdt::early_probe(dtb);

    // PC relative addressing means the linker symbols this reads resolve to
    // physical addresses right now, and to high ones after the jump, with no
    // relocation and no special casing.
    //
    // The memory map is a guess until the tree can be read. It only has to be
    // large enough to allocate page tables out of, and it is corrected as soon
    // as the kernel is running somewhere the tree makes sense.
    frames::init(frames::DEFAULT_RAM_BASE, frames::DEFAULT_RAM_SIZE);
    uart::emergency_print("the_rack: frame allocator up, building page tables\n");

    // Build both roots while the MMU is off, enable with the identity map
    // holding the program counter steady, then leave the low half behind.
    let (ttbr0, ttbr1) = paging::build_tables();

    // The blob is not necessarily inside the window the kernel guessed at: on
    // `virt` it is, at 128 MiB up, but that is a QEMU decision and not a rule.
    // Anything outside gets mapped explicitly, so the high half can read the
    // tree whatever the machine did with it.
    //
    // Only outside. The guessed window is already mapped, in 2 MiB blocks, and
    // mapping a page inside one would mean splitting a block descriptor, which
    // this page table code does not do.
    if let Some((pa, size)) = blob {
        let guessed_end = frames::ram_base() + frames::ram_size();
        if pa < frames::ram_base() || pa + size > guessed_end {
            paging::map_physical(ttbr1, pa, size);
        }
    }

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

    // Before the map is printed, because until the tree has been read it is a
    // guess and printing a guess as though it were the machine is how a wrong
    // number survives for two tiers.
    match fdt::init(dtb) {
        Ok(blob) => {
            fdt::print_info(&blob);
            adopt_memory_map(&blob);
            discover_devices(&blob);
            let (slots, occupied) = virtio::discover(&blob);
            println!();
            virtio::print_table(slots, occupied);
        }
        Err(error) => {
            println!(
                "device tree: none ({}), memory map stays a guess",
                error.describe()
            );
        }
    }
    println!();

    frames::print_map();
    println!();

    exceptions::self_test();
    fdt::self_test();
    fdt::tree_self_test();
    virtio::self_test();
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

/// Replace the guessed memory map with the one the machine describes.
///
/// Everything up to here has been running on `DEFAULT_RAM_SIZE`, which is a
/// number this kernel made up. It was enough to allocate page tables from, and
/// that is all it was ever meant to be.
///
/// Order matters. The physical map has to cover the new memory before the
/// allocator will hand any of it out, or the first page table that lands up
/// there faults on a kernel address that does not translate. Reservations come
/// last, since they are only meaningful once the range they live in exists.
fn adopt_memory_map(blob: &fdt::Blob) {
    let map = fdt::memory_map(blob);

    if map.size == 0 {
        println!("  memory        : no /memory node, keeping the guess");
        return;
    }

    if map.banks > 1 {
        println!(
            "  WARNING       : {} memory banks, using the first",
            map.banks
        );
    }

    let old_end = frames::ram_base() + frames::ram_size();
    let new_end = map.base + map.size.min(frames::MAX_RAM);
    paging::extend_physical_map(old_end, new_end);

    let outcome = frames::adopt(map.base, map.size);

    // Printed from the allocator rather than from the tree, deliberately. The
    // tree's number is only interesting once something acted on it, and a line
    // that reads back what was just parsed would say the same thing whether or
    // not the allocator ever heard about it.
    println!(
        "  memory        : {} MiB at {:#x}, from the tree",
        frames::ram_size() / 1024 / 1024,
        frames::ram_base()
    );

    match outcome {
        Ok(0) => println!("  frames        : the guess was right"),
        Ok(change) if change > 0 => println!("  frames        : {change} more than guessed"),
        Ok(change) => println!("  frames        : {} fewer than guessed", -change),
        Err(error) => println!("  WARNING       : cannot use this map ({error:?})"),
    }

    // Memory that is in RAM and is not ours: the blob itself, whatever the
    // firmware put in the header's reservation block, and anything in
    // `/reserved-memory`.
    let mut reserved = 0;
    for (base, size) in map.reserved() {
        match frames::reserve_range(*base, *size) {
            Ok(frames) => reserved += frames,
            Err(error) => println!("  WARNING       : {base:#x} not reserved ({error:?})"),
        }
    }

    if map.dropped > 0 {
        println!(
            "  WARNING       : {} reserved regions did not fit",
            map.dropped
        );
    }

    // The blob is the reservation every machine has, and the one whose absence
    // would be silent: nothing reads the tree again until later in tier 4, by
    // which time whatever overwrote it is long gone.
    assert!(
        frames::is_taken(blob.pa),
        "the device tree was not reserved"
    );
    assert!(
        frames::is_taken(blob.pa + blob.header.totalsize as u64 - 1),
        "the end of the device tree was not reserved"
    );

    println!("  reserved      : {reserved} frames, blob included");
}

/// Find the console and the interrupt controller in the tree.
///
/// Both are already running on constants by the time this happens, which is
/// not a shortcut but the only possible order: the console has to work before
/// anything can report that the console could not be found. So the constants
/// are the bootstrap, the tree is the authority, and a disagreement is
/// something the boot log says out loud rather than something that silently
/// leaves the kernel talking to the wrong address.
fn discover_devices(blob: &fdt::Blob) {
    let mut windows = [fdt::Region::default(); 4];

    match blob.find_compatible("arm,pl011") {
        Some(node) => {
            let found = blob.regions(&node, &mut windows);
            assert!(found >= 1, "the pl011 node has no reg");
            let uart = windows[0];

            paging::map_device(uart.base, uart.size);
            uart::adopt(uart.base as usize);

            // Printed from the driver rather than from the tree, so the line
            // is evidence that the address was taken and not merely read.
            println!(
                "  console       : {:#012x} + {:#x}{}",
                uart::base(),
                uart.size,
                if uart.base as usize == uart::bootstrap_base() {
                    ", where we were already talking"
                } else {
                    ", moved"
                }
            );
        }
        None => println!("  WARNING       : no pl011 in the tree, staying on the constant"),
    }

    match blob.find_compatible("arm,cortex-a15-gic") {
        Some(node) => {
            let found = blob.regions(&node, &mut windows);
            assert!(
                found >= 2,
                "the gic node needs two reg entries: distributor and cpu interface"
            );

            let (distributor, cpu) = (windows[0], windows[1]);
            assert_ne!(
                distributor.base, cpu.base,
                "distributor and cpu interface cannot be the same window"
            );

            paging::map_device(distributor.base, distributor.size);
            paging::map_device(cpu.base, cpu.size);
            gic::adopt(distributor.base as usize, cpu.base as usize);

            let (boot_distributor, boot_cpu) = gic::bootstrap_bases();
            let agreed =
                distributor.base as usize == boot_distributor && cpu.base as usize == boot_cpu;

            let (live_distributor, live_cpu) = gic::bases();
            println!(
                "  interrupts    : dist {:#012x} cpu {:#012x}{}",
                live_distributor,
                live_cpu,
                if agreed { ", as assumed" } else { ", moved" }
            );
        }
        None => println!("  WARNING       : no gic in the tree, staying on the constants"),
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
