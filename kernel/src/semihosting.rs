//! ARM semihosting: asking the thing running us to do something for us.
//!
//! A semihosting call is a trap the debugger or emulator recognises and
//! services on the guest's behalf. We use exactly one of them, `SYS_EXIT`, so
//! that a kernel panic terminates QEMU with a failure status instead of
//! spinning forever and making CI wait out its timeout.
//!
//! This only works because the QEMU runner passes
//! `-semihosting-config enable=on`. On real hardware with nothing attached the
//! `hlt` traps as an undefined instruction, which is why `exit` falls through
//! into a halt loop rather than assuming it never returns.

use core::arch::asm;

/// Terminate execution and report a status to the host.
const SYS_EXIT: u64 = 0x18;

/// The one reason code QEMU treats as a clean, intentional shutdown. Any other
/// reason makes it exit non-zero regardless of the status we pass.
const ADP_STOPPED_APPLICATION_EXIT: u64 = 0x2_0026;

/// Status codes the boot test reads back as QEMU's exit code.
pub const EXIT_SUCCESS: u64 = 0;
pub const EXIT_FAILURE: u64 = 1;

/// Ask the host to tear down the machine and exit with `status`.
///
/// Deliberately not called on a successful boot. An operating system that
/// finishes and exits is not an operating system, so the normal path runs
/// forever and only failures come through here.
pub fn exit(status: u64) -> ! {
    // On aarch64, SYS_EXIT takes x1 as a pointer to a two-word block rather
    // than an immediate: field 0 is the reason, field 1 is the exit status.
    let block = [ADP_STOPPED_APPLICATION_EXIT, status];

    unsafe {
        asm!(
            "hlt #0xf000",
            in("x0") SYS_EXIT,
            in("x1") block.as_ptr(),
            options(nostack),
        );
    }

    // Reached only when nobody is listening for semihosting calls.
    crate::halt()
}
