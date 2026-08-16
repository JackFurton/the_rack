//! Mutual exclusion, at the stage of the project where the honest
//! implementation is "turn interrupts off".
//!
//! # Why this is not a spinlock
//!
//! The reflex is an `AtomicBool` and a compare-exchange loop. That is wrong
//! here, and it is wrong in a way that works on QEMU and fails on hardware,
//! which is the worst kind of wrong.
//!
//! Rust's atomics compile to `LDXR`/`STXR`, the load/store exclusive pair.
//! Those depend on the exclusive monitor, and the architecture only guarantees
//! the monitor works for Normal cacheable memory. With the MMU off, every
//! access is treated as Device memory, and exclusives against Device memory
//! have constrained unpredictable behaviour: `STXR` is permitted to fail
//! forever. A compare-exchange loop that never succeeds is an infinite loop
//! inside your locking primitive.
//!
//! QEMU's TCG emulates exclusives faithfully regardless of memory type, so the
//! broken version passes every test we can currently run and then hangs the
//! first time it touches real silicon.
//!
//! # What this does instead
//!
//! One core, no preemption between kernel threads, and the only thing that can
//! interrupt us is an interrupt. So masking interrupts is not an approximation
//! of mutual exclusion here, it is exactly mutual exclusion. No atomics, no
//! exclusive monitor, works with the MMU off.
//!
//! This stops being sufficient when tier 6 boots a second core. By then the
//! MMU is on (tier 1), exclusives work, and this grows a real spinlock
//! underneath the same API. Callers should not have to change.
//!
//! Note that synchronous exceptions are not maskable, so a fault taken while a
//! lock is held will still run its handler and can still print. That is
//! deliberate: a fault report is worth more than tidy output.

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};

/// Saved interrupt mask state, to be put back exactly as it was found.
///
/// Restoring the previous value rather than blindly enabling is what makes
/// these safe to nest. Code that unconditionally re-enabled interrupts on
/// unlock would silently turn them on inside an outer critical section.
#[derive(Clone, Copy)]
pub struct InterruptState(u64);

/// Mask IRQ and FIQ, returning the previous state.
pub fn disable_interrupts() -> InterruptState {
    let daif: u64;
    unsafe {
        core::arch::asm!(
            "mrs {daif}, daif",
            // The immediate is a 4 bit field: bit 3 D, bit 2 A, bit 1 I,
            // bit 0 F. 0b0011 masks IRQ and FIQ and leaves debug and SError
            // alone, because an SError is a hardware error report we would
            // rather hear about immediately than defer.
            "msr daifset, #0b0011",
            daif = out(reg) daif,
            // No nomem. The compiler must not sink memory accesses out of the
            // critical section we are opening.
            options(nostack),
        );
    }
    InterruptState(daif)
}

/// Put the interrupt mask back to what `disable_interrupts` found.
pub fn restore_interrupts(state: InterruptState) {
    unsafe {
        core::arch::asm!(
            "msr daif, {daif}",
            daif = in(reg) state.0,
            options(nostack),
        );
    }
}

/// Unmask IRQ and FIQ.
///
/// Safe to call, but only useful once something can actually deliver an
/// interrupt. Until the GIC is up nothing can, so this only ever changes a
/// PSTATE bit.
pub fn enable_interrupts() {
    unsafe {
        core::arch::asm!("msr daifclr, #0b0011", options(nostack));
    }
}

/// True if IRQs are currently masked. Reads PSTATE.I, bit 7 of DAIF.
pub fn interrupts_masked() -> bool {
    let daif: u64;
    unsafe { core::arch::asm!("mrs {}, daif", out(reg) daif, options(nomem, nostack)) };
    daif & (1 << 7) != 0
}

/// Run `f` with interrupts masked, restoring the previous state afterwards.
pub fn without_interrupts<R>(f: impl FnOnce() -> R) -> R {
    let state = disable_interrupts();
    let result = f();
    restore_interrupts(state);
    result
}

/// A value that can only be touched with interrupts masked.
pub struct Lock<T> {
    data: UnsafeCell<T>,
}

// Safe because every path to the data goes through `lock`, which masks
// interrupts, and there is exactly one core. The moment a second core exists
// this claim needs a real spinlock behind it.
unsafe impl<T: Send> Sync for Lock<T> {}

impl<T> Lock<T> {
    pub const fn new(data: T) -> Self {
        Self {
            data: UnsafeCell::new(data),
        }
    }

    /// Mask interrupts and hand out access. Interrupts come back on when the
    /// returned guard is dropped.
    pub fn lock(&self) -> LockGuard<'_, T> {
        LockGuard {
            state: disable_interrupts(),
            data: unsafe { &mut *self.data.get() },
        }
    }
}

pub struct LockGuard<'a, T> {
    state: InterruptState,
    data: &'a mut T,
}

impl<T> Deref for LockGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        self.data
    }
}

impl<T> DerefMut for LockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        self.data
    }
}

impl<T> Drop for LockGuard<'_, T> {
    fn drop(&mut self) {
        restore_interrupts(self.state);
    }
}

/// Prove the lock actually masks interrupts, and that unlocking restores the
/// previous state rather than blindly enabling.
///
/// The nesting case is the one worth testing. A lock that re-enabled
/// interrupts on unlock would look correct in isolation and would quietly
/// open a window in the middle of any outer critical section that happened to
/// take a second lock. That bug is invisible until something interrupts at
/// exactly the wrong moment, months later.
///
/// Safe to run at boot: the vector table is installed and nothing can deliver
/// an interrupt yet, so unmasking here only moves a PSTATE bit.
pub fn self_test() {
    static PROBE: Lock<u32> = Lock::new(0);
    static OTHER: Lock<u32> = Lock::new(0);

    let entry_state = disable_interrupts();

    enable_interrupts();
    let unlocked = interrupts_masked();

    let mut guard = PROBE.lock();
    let locked = interrupts_masked();
    *guard += 1;

    // Take and drop a second lock while the first is still held. Dropping the
    // inner guard must leave interrupts masked, because the outer one still
    // needs them masked.
    let nested_after_inner_drop = {
        {
            let mut inner = OTHER.lock();
            *inner += 1;
        }
        interrupts_masked()
    };

    let counted = *guard;
    drop(guard);
    let released = interrupts_masked();

    restore_interrupts(entry_state);

    crate::println!(
        "lock self test: unlocked={unlocked} locked={locked} nested={nested_after_inner_drop} released={released}"
    );

    assert!(!unlocked, "interrupts should be unmasked outside a lock");
    assert!(locked, "lock must mask interrupts");
    assert!(
        nested_after_inner_drop,
        "dropping an inner guard must not unmask inside an outer critical section"
    );
    assert!(
        !released,
        "dropping the last guard must restore the previous state"
    );

    crate::println!("lock self test: passed, guarded counter reached {counted}");
}
