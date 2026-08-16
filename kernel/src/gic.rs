//! Driver for the GICv2, the interrupt controller on the QEMU `virt` machine.
//!
//! Nothing can deliver an interrupt until this is configured. Unmasking IRQs
//! in PSTATE is necessary but not sufficient: the CPU only ever sees an
//! interrupt the GIC decided to forward.
//!
//! The GIC has two halves, and both must be enabled or nothing happens.
//!
//! The **distributor** is global. It knows about every interrupt source in the
//! system, decides which are enabled, what priority each has, and which cores
//! they may be sent to.
//!
//! The **CPU interface** is per core. It is where a core acknowledges an
//! interrupt, and where it sets the priority threshold below which it does not
//! want to be bothered. A common way to see nothing at all is to configure the
//! distributor perfectly and leave `GICC_PMR` at its reset value of 0, which
//! masks every priority there is.
//!
//! Interrupt IDs are split by range, and the split matters because the
//! distributor treats them differently:
//!
//!   0..15    SGI, software generated, for core-to-core signalling
//!   16..31   PPI, private peripheral, one instance per core (our timer)
//!   32..     SPI, shared peripheral, routed to a core of your choosing

use core::ptr::{read_volatile, write_volatile};

/// Distributor base on the QEMU `virt` machine.
const GICD_BASE: usize = 0x0800_0000;
/// CPU interface base on the QEMU `virt` machine.
const GICC_BASE: usize = 0x0801_0000;

// Distributor registers.
const GICD_CTLR: usize = 0x000; // Control
const GICD_TYPER: usize = 0x004; // Controller type, tells us how many lines
const GICD_IGROUPR: usize = 0x080; // Group, 1 bit per interrupt
const GICD_ISENABLER: usize = 0x100; // Set enable, 1 bit per interrupt
const GICD_ICENABLER: usize = 0x180; // Clear enable, 1 bit per interrupt
const GICD_ICPENDR: usize = 0x280; // Clear pending, 1 bit per interrupt
const GICD_IPRIORITYR: usize = 0x400; // Priority, 1 byte per interrupt
const GICD_ITARGETSR: usize = 0x800; // Target core, 1 byte per interrupt

// CPU interface registers.
const GICC_CTLR: usize = 0x00; // Control
const GICC_PMR: usize = 0x04; // Priority mask
const GICC_BPR: usize = 0x08; // Binary point, preemption grouping
const GICC_IAR: usize = 0x0c; // Acknowledge, reading it claims the interrupt
const GICC_EOIR: usize = 0x10; // End of interrupt

/// Returned by `GICC_IAR` when there was nothing to claim after all. Must not
/// be acknowledged with an EOI.
pub const SPURIOUS_INTID: u32 = 1023;

/// Priority given to everything we enable.
///
/// Mid range, so later work can place interrupts either side of it without
/// having to renumber. Lower values are higher priority, which is backwards
/// from intuition and a reliable source of confusion.
const DEFAULT_PRIORITY: u8 = 0xa0;

unsafe fn gicd_read(offset: usize) -> u32 {
    unsafe { read_volatile((GICD_BASE + offset) as *const u32) }
}

unsafe fn gicd_write(offset: usize, value: u32) {
    unsafe { write_volatile((GICD_BASE + offset) as *mut u32, value) }
}

unsafe fn gicd_write_byte(offset: usize, value: u8) {
    unsafe { write_volatile((GICD_BASE + offset) as *mut u8, value) }
}

unsafe fn gicc_read(offset: usize) -> u32 {
    unsafe { read_volatile((GICC_BASE + offset) as *const u32) }
}

unsafe fn gicc_write(offset: usize, value: u32) {
    unsafe { write_volatile((GICC_BASE + offset) as *mut u32, value) }
}

/// How many interrupt IDs this distributor implements.
///
/// `GICD_TYPER.ITLinesNumber` is in units of 32 lines, minus one, so the
/// arithmetic below is the spec's and not a guess.
pub fn num_interrupts() -> u32 {
    let it_lines = unsafe { gicd_read(GICD_TYPER) } & 0x1f;
    32 * (it_lines + 1)
}

/// Bring up the distributor and this core's CPU interface.
pub fn init() {
    let lines = num_interrupts();

    unsafe {
        // Quiet while we reconfigure.
        gicd_write(GICD_CTLR, 0);

        // Start from a known state rather than trusting reset values or
        // whatever firmware left behind. Registers here are one bit per
        // interrupt, so 32 interrupts per 4 byte register.
        for reg in 0..lines.div_ceil(32) {
            let offset = (reg * 4) as usize;
            gicd_write(GICD_ICENABLER + offset, 0xffff_ffff);
            gicd_write(GICD_ICPENDR + offset, 0xffff_ffff);
            // Everything in group 0, which is what GICC_CTLR bit 0 forwards.
            gicd_write(GICD_IGROUPR + offset, 0);
        }

        // Priority is one byte per interrupt.
        for intid in 0..lines {
            gicd_write_byte(GICD_IPRIORITYR + intid as usize, DEFAULT_PRIORITY);
        }

        // Targets are one byte per interrupt, one bit per core. Only valid for
        // SPIs: the first 32 IDs are banked per core and read-only here.
        for intid in 32..lines {
            gicd_write_byte(GICD_ITARGETSR + intid as usize, 0b1);
        }

        gicd_write(GICD_CTLR, 1);

        // Now the CPU interface. PMR resets to 0, which masks every priority,
        // so leaving it alone means the distributor forwards interrupts that
        // this core then silently refuses.
        gicc_write(GICC_PMR, 0xff);
        // No preemption grouping. Everything compares on full priority.
        gicc_write(GICC_BPR, 0);
        gicc_write(GICC_CTLR, 1);
    }
}

/// Allow `intid` to be delivered.
pub fn enable(intid: u32) {
    let reg = (intid / 32) as usize * 4;
    let bit = 1u32 << (intid % 32);
    unsafe { gicd_write(GICD_ISENABLER + reg, bit) };
}

/// Claim the pending interrupt, returning its ID.
///
/// Reading `GICC_IAR` is not a passive query. It moves the interrupt to active
/// state and is how the core takes ownership of it, so it must be read exactly
/// once per delivery.
pub fn acknowledge() -> u32 {
    unsafe { gicc_read(GICC_IAR) }
}

/// Tell the GIC we are finished with `intid`.
///
/// Forgetting this is a good way to get exactly one interrupt ever: the
/// controller keeps it active and will not deliver anything of equal or lower
/// priority again.
pub fn end_of_interrupt(intid: u32) {
    unsafe { gicc_write(GICC_EOIR, intid) };
}
