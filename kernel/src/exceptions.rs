//! Exception handling: catching faults and saying something useful about them.
//!
//! Before this existed, any fault vectored into an uninitialised `VBAR_EL1`,
//! hit whatever bytes happened to live there, took another fault trying to
//! execute them, and looped forever in total silence. That is how tier 0's
//! trapped FP instruction presented: a kernel that printed nothing and did
//! nothing, with no way to tell a hang from a crash.
//!
//! The table itself is in `vectors.S`. This module owns the layout of the
//! saved register state, the decoding of the syndrome registers, and the
//! policy for what to do about each kind of trap.

use core::arch::asm;
use core::fmt;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::{print, println};

/// Registers saved on entry to an exception, and restored on the way out.
///
/// `#[repr(C)]` is load bearing. `vectors.S` writes these fields at hardcoded
/// offsets, so the field order and types here are the assembly's ABI. Adding a
/// field in the middle silently corrupts every trap.
#[repr(C)]
#[derive(Debug)]
pub struct TrapFrame {
    /// x0 through x30. x30 is the link register.
    pub x: [u64; 31],
    /// Exception Link Register: the address execution resumes at on `eret`.
    /// Writable, which is how we step past a breakpoint.
    pub elr: u64,
    /// Saved Program Status Register: the PSTATE to restore.
    pub spsr: u64,
    /// Exception Syndrome Register: what happened, and why.
    pub esr: u64,
    /// Fault Address Register: the address that faulted, for aborts.
    pub far: u64,
    _pad: u64,
}

/// Which of the 16 vector table slots we arrived through.
///
/// Worth reporting, because the slot alone tells you a lot before you decode
/// anything: an unexpected trap through a lower-EL entry means userspace, and
/// an SError means something went wrong asynchronously and the reported
/// address may have nothing to do with the cause.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct VectorIndex(u64);

impl VectorIndex {
    const SOURCES: [&'static str; 4] = [
        "current EL, SP_EL0",
        "current EL, SP_ELx",
        "lower EL, AArch64",
        "lower EL, AArch32",
    ];
    const KINDS: [&'static str; 4] = ["synchronous", "IRQ", "FIQ", "SError"];

    pub fn source(&self) -> &'static str {
        Self::SOURCES
            .get((self.0 >> 2) as usize)
            .copied()
            .unwrap_or("unknown source")
    }

    pub fn kind(&self) -> &'static str {
        Self::KINDS
            .get((self.0 & 0b11) as usize)
            .copied()
            .unwrap_or("unknown kind")
    }

    /// IRQ is the second entry in every group.
    pub fn is_irq(&self) -> bool {
        self.0 & 0b11 == 1
    }
}

impl fmt::Display for VectorIndex {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.kind(), self.source())
    }
}

/// A decoded `ESR_EL1`.
///
/// Layout: EC in bits [31:26] says what class of thing happened, IL in bit 25
/// says whether the faulting instruction was 32 or 16 bits, and ISS in bits
/// [24:0] carries class-specific detail.
#[derive(Clone, Copy)]
pub struct Syndrome(u64);

impl Syndrome {
    pub fn exception_class(&self) -> u64 {
        (self.0 >> 26) & 0x3f
    }

    pub fn iss(&self) -> u64 {
        self.0 & 0x1ff_ffff
    }

    /// Human-readable name for the exception class.
    ///
    /// Not exhaustive. These are the ones that actually come up while bringing
    /// a kernel to life; the rest print as their raw EC and can be looked up
    /// in the ARM ARM section on ESR_EL1.
    pub fn class_name(&self) -> &'static str {
        match self.exception_class() {
            0x00 => "unknown reason",
            0x01 => "trapped WFI or WFE",
            0x07 => "trapped SVE/SIMD/FP access",
            0x0e => "illegal execution state",
            0x15 => "SVC from AArch64",
            0x18 => "trapped MSR/MRS/system instruction",
            0x20 => "instruction abort from lower EL",
            0x21 => "instruction abort from current EL",
            0x22 => "PC alignment fault",
            0x24 => "data abort from lower EL",
            0x25 => "data abort from current EL",
            0x26 => "SP alignment fault",
            0x2c => "trapped floating point exception",
            0x2f => "SError interrupt",
            0x30 | 0x31 => "breakpoint",
            0x32 | 0x33 => "software step",
            0x34 | 0x35 => "watchpoint",
            0x3c => "BRK instruction",
            _ => "unrecognised class",
        }
    }

    /// True for the aborts whose ISS carries a fault status code and where
    /// `FAR_EL1` is meaningful.
    pub fn is_abort(&self) -> bool {
        matches!(self.exception_class(), 0x20 | 0x21 | 0x24 | 0x25)
    }

    /// True for a `BRK`, the software breakpoint we deliberately execute to
    /// prove this whole path works.
    pub fn is_brk(&self) -> bool {
        self.exception_class() == 0x3c
    }

    /// The immediate operand of a `BRK`, held in the low 16 bits of the ISS.
    pub fn brk_comment(&self) -> u64 {
        self.iss() & 0xffff
    }

    /// Decode the Data Fault Status Code in ISS[5:0].
    ///
    /// This is the field you actually read when a memory access goes wrong,
    /// and the difference between "nothing is mapped there" and "something is
    /// mapped but you may not touch it" lives here.
    pub fn fault_status(&self) -> &'static str {
        match self.iss() & 0x3f {
            0x00..=0x03 => "address size fault",
            0x04..=0x07 => "translation fault, nothing mapped",
            0x09..=0x0b => "access flag fault",
            0x0d..=0x0f => "permission fault",
            0x10 => "synchronous external abort",
            0x21 => "alignment fault",
            0x30 => "TLB conflict abort",
            _ => "other",
        }
    }

    /// Translation table level the fault was taken at, for the faults that
    /// report one. Narrows down which table is wrong.
    pub fn fault_level(&self) -> Option<u64> {
        match self.iss() & 0x3f {
            code @ (0x00..=0x03 | 0x04..=0x07 | 0x09..=0x0b | 0x0d..=0x0f) => Some(code & 0b11),
            _ => None,
        }
    }

    /// Was the faulting access a write? ISS bit 6, valid for data aborts.
    pub fn is_write(&self) -> bool {
        self.iss() & (1 << 6) != 0
    }
}

/// Point `VBAR_EL1` at our vector table.
///
/// Call this as early as possible. Every fault before this line is invisible.
pub fn init() {
    unsafe extern "C" {
        static __vectors: u8;
    }

    let base = &raw const __vectors as u64;

    unsafe {
        asm!(
            "msr vbar_el1, {base}",
            // The write has to land before any exception can be taken against
            // it, and a system register write is not otherwise ordered against
            // instruction fetch.
            "isb",
            base = in(reg) base,
            options(nostack),
        );
    }
}

/// Address of the installed vector table, for the boot banner.
pub fn vector_base() -> u64 {
    let base: u64;
    unsafe { asm!("mrs {}, vbar_el1", out(reg) base, options(nomem, nostack)) };
    base
}

/// Set while a probe is deliberately touching memory it may not be allowed to.
static EXPECTING_FAULT: AtomicBool = AtomicBool::new(false);
/// `ESR_EL1` from the probe's fault, or 0 if it did not fault.
static PROBE_ESR: AtomicU64 = AtomicU64::new(0);

/// Result of deliberately touching memory that may be protected.
pub struct Probe {
    esr: u64,
}

impl Probe {
    /// Did the access fault?
    pub fn faulted(&self) -> bool {
        self.esr != 0
    }

    /// What the fault was, for reporting.
    pub fn describe(&self) -> &'static str {
        if self.esr == 0 {
            return "no fault";
        }
        Syndrome(self.esr).fault_status()
    }
}

/// Run `probe`, catching a memory fault instead of panicking on it.
///
/// This is how the paging self test can assert that writing to `.text` is
/// refused: without it, proving the permission works would mean crashing the
/// kernel to demonstrate it.
///
/// The recovery is to skip the faulting instruction, which is sound for a
/// single volatile load or store and nothing more. `probe` should contain one
/// access and no state that a half-executed sequence would corrupt.
///
/// Not reentrant, and not safe once more than one thing can run at a time.
/// Both are fine for a boot-time self test on one core.
pub fn probe(probe: impl FnOnce()) -> Probe {
    PROBE_ESR.store(0, Ordering::Relaxed);
    EXPECTING_FAULT.store(true, Ordering::Relaxed);

    probe();

    EXPECTING_FAULT.store(false, Ordering::Relaxed);
    Probe {
        esr: PROBE_ESR.load(Ordering::Relaxed),
    }
}

/// Every exception in the system arrives here, called from `.Lcommon_trap`.
#[unsafe(no_mangle)]
pub extern "C" fn handle_exception(frame: &mut TrapFrame, index: u64) {
    let index = VectorIndex(index);

    // Interrupts are routine and carry no syndrome: ESR is not meaningful for
    // an IRQ, and printing a report for each one at 100 Hz would bury
    // everything else.
    if index.is_irq() {
        dispatch_irq();
        return;
    }

    let syndrome = Syndrome(frame.esr);

    // A probe deliberately touched something it was not allowed to. Record it,
    // step over the offending instruction, and carry on. This is how the
    // permission checks in the paging self test are able to pass rather than
    // panic.
    if syndrome.is_abort() && EXPECTING_FAULT.load(Ordering::Relaxed) {
        PROBE_ESR.store(frame.esr, Ordering::Relaxed);
        frame.elr += 4;
        return;
    }

    report(frame, index, syndrome);

    // A BRK is a deliberate breakpoint, not a failure. Step over it and let
    // the interrupted code carry on. ELR points at the BRK itself, and every
    // A64 instruction is four bytes wide.
    //
    // This is also the only path today that exercises a return from an
    // exception, which is why the boot self test uses it.
    if syndrome.is_brk() {
        frame.elr += 4;
        return;
    }

    // Everything else is unexpected. Nothing here knows how to recover, and
    // returning would just re-run the faulting instruction and trap forever.
    panic!("unhandled exception: {}", syndrome.class_name());
}

/// Claim an interrupt from the GIC, route it, and release it.
///
/// The acknowledge and the end-of-interrupt have to bracket the handler
/// exactly once each. Skipping the EOI gets you precisely one interrupt: the
/// controller keeps it active and refuses to deliver anything of equal or
/// lower priority afterwards, which looks like the timer having stopped.
fn dispatch_irq() {
    let intid = crate::gic::acknowledge();

    // The GIC can withdraw an interrupt between raising it and our claiming
    // it, in which case it hands back the spurious ID and expects no EOI.
    if intid == crate::gic::SPURIOUS_INTID {
        return;
    }

    match intid {
        crate::timer::TIMER_INTID => crate::timer::on_tick(),
        other => println!("unhandled IRQ {other}"),
    }

    crate::gic::end_of_interrupt(intid);

    // Only now, with the interrupt released, is it safe to run something else.
    crate::tasks::preempt_if_needed();
}

fn report(frame: &TrapFrame, index: VectorIndex, syndrome: Syndrome) {
    println!();
    println!("--- exception ---");
    println!("  vector : {index}");
    println!(
        "  class  : {} (EC {:#04x})",
        syndrome.class_name(),
        syndrome.exception_class()
    );
    println!("  esr    : {:#018x}", frame.esr);
    println!("  elr    : {:#018x}", frame.elr);
    println!("  spsr   : {:#018x}", frame.spsr);

    if syndrome.is_brk() {
        println!("  comment: {:#x}", syndrome.brk_comment());
    }

    if syndrome.is_abort() {
        println!("  far    : {:#018x}", frame.far);
        print!("  fault  : {}", syndrome.fault_status());
        if let Some(level) = syndrome.fault_level() {
            print!(" at level {level}");
        }
        println!(
            ", on a {}",
            if syndrome.is_write() { "write" } else { "read" }
        );
    }

    println!("  x0..x3 : {:#018x} {:#018x}", frame.x[0], frame.x[1]);
    println!("           {:#018x} {:#018x}", frame.x[2], frame.x[3]);
    println!("  lr     : {:#018x}", frame.x[30]);
    println!("-----------------");
}

/// Prove the exception path works, end to end, on every boot.
///
/// Executes a real `BRK`, which traps into the vector table, builds a frame,
/// decodes the syndrome, steps the saved ELR past the breakpoint, and returns
/// here through `eret`. If any part of the save, decode, or restore is wrong,
/// this hangs or faults instead of printing.
///
/// Cheap, and it means a boot log is evidence that the machinery is intact
/// rather than just evidence that nothing has faulted yet.
pub fn self_test() {
    println!("trap self test: executing brk #0x42");

    unsafe { asm!("brk #0x42", options(nomem, nostack)) };

    println!("trap self test: resumed, registers intact");
}
