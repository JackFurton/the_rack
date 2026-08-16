//! The ARM generic timer: the first thing that makes this machine act on its
//! own rather than only reacting to us.
//!
//! Every core has one, it counts at a fixed frequency reported by `CNTFRQ_EL0`,
//! and it raises an interrupt when its countdown reaches zero. We use the EL1
//! physical timer, the `CNTP_*` registers.
//!
//! There is no periodic mode. The hardware fires once and stops, so the
//! handler has to arm the next deadline itself. Forgetting that gives you
//! exactly one tick and a machine that then sits there looking healthy.

use core::arch::asm;

use crate::sync::Lock;
use crate::{gic, println};

/// The EL1 non-secure physical timer is PPI 14, which is interrupt ID 30.
///
/// It is a PPI rather than an SPI because every core has its own timer, so the
/// ID is private to each core and needs no routing.
pub const TIMER_INTID: u32 = 30;

/// Ticks per second. 10 ms between interrupts.
///
/// Fast enough that the interrupt path gets real exercise rather than being
/// touched once a second, slow enough to leave the core mostly idle.
pub const TICK_HZ: u64 = 100;

/// Ticks since the timer started.
///
/// Behind a `Lock` rather than an atomic on purpose. An atomic `fetch_add`
/// compiles to `LDXR`/`STXR`, which is not dependable with the MMU off. See
/// the module docs in `sync.rs`.
static TICKS: Lock<u64> = Lock::new(0);

/// Counter frequency, from `CNTFRQ_EL0`. Fixed by the platform, not settable.
pub fn frequency() -> u64 {
    let freq: u64;
    unsafe { asm!("mrs {}, cntfrq_el0", out(reg) freq, options(nomem, nostack)) };
    freq
}

/// Counter ticks between our interrupts.
fn interval() -> u64 {
    frequency() / TICK_HZ
}

/// The always-running physical counter. Monotonic, never written by us.
pub fn count() -> u64 {
    let count: u64;
    unsafe { asm!("mrs {}, cntpct_el0", out(reg) count, options(nomem, nostack)) };
    count
}

/// The absolute counter value at which the timer will next fire.
fn deadline() -> u64 {
    let cval: u64;
    unsafe { asm!("mrs {}, cntp_cval_el0", out(reg) cval, options(nomem, nostack)) };
    cval
}

fn set_deadline(cval: u64) {
    unsafe { asm!("msr cntp_cval_el0, {}", in(reg) cval, options(nomem, nostack)) };
}

/// Set the first deadline, `interval()` ticks from now.
fn arm_first() {
    set_deadline(count() + interval());
}

/// Advance the deadline by exactly one interval from the *previous deadline*.
///
/// Deliberately not `now + interval`. `CNTP_TVAL_EL0` counts down from the
/// moment it is written, so re-arming that way makes every period
/// `interval + however long it took to reach the handler`, and that latency
/// compounds on every tick. Measured against wall clock, a TVAL re-arm at
/// 100 Hz ran 25% slow in a debug build under TCG.
///
/// Anchoring to the previous deadline instead means handler latency has to
/// exceed a whole interval before it costs us anything.
fn arm_next() {
    let interval = interval();
    let mut next = deadline() + interval;
    let now = count();

    // If we fell so far behind that the next deadline is already in the past,
    // walking forward one interval at a time would fire immediately, over and
    // over, and never catch up. Skip the missed ticks instead.
    if next <= now {
        next = now + interval;
    }

    set_deadline(next);
}

/// Start the heartbeat.
///
/// Order matters. The GIC must already be up, the interrupt must be enabled
/// there, and the timer must be armed before its interrupt is unmasked, or the
/// first thing that arrives is an interrupt with no deadline behind it.
pub fn init() {
    gic::enable(TIMER_INTID);

    arm_first();

    // CNTP_CTL_EL0: bit 0 ENABLE, bit 1 IMASK, bit 2 ISTATUS (read only).
    // Enable and explicitly clear the mask, since IMASK set is another way to
    // have a perfectly configured timer that never interrupts anything.
    let ctl: u64 = 1;
    unsafe { asm!("msr cntp_ctl_el0, {}", in(reg) ctl, options(nomem, nostack)) };
}

/// Called from the IRQ dispatcher when the timer fires.
pub fn on_tick() {
    // Re-arm before doing anything else, so the next deadline is set while we
    // still know exactly where the last one was.
    arm_next();

    let mut ticks = TICKS.lock();
    *ticks += 1;
    let count = *ticks;
    // Release before printing: the console takes its own lock, and holding two
    // locks at once is a habit worth not forming.
    drop(ticks);

    if count.is_multiple_of(TICK_HZ) {
        println!("uptime {}s ({} ticks)", count / TICK_HZ, count);
    }
}

/// Ticks recorded so far.
pub fn ticks() -> u64 {
    *TICKS.lock()
}
