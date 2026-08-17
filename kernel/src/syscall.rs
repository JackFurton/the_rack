//! The syscall interface: the only legitimate way for EL0 to ask for
//! anything.
//!
//! # Every argument is a claim, not a fact
//!
//! This is the first code in the project that takes input from something less
//! privileged than itself. A user pointer is a number a task chose. The kernel
//! is running with enough privilege to honour any lie told with one, so every
//! address that crosses this boundary is checked against the caller's own page
//! tables before it is touched.
//!
//! Checking that a pointer "looks like" a user address is not enough. The
//! kernel can read plenty of memory the caller cannot, so the question is not
//! whether the address is low, it is whether *this task* is allowed to touch
//! it. That means walking the task's tables and reading the permissions the
//! hardware would have enforced had the access come from EL0.

use crate::exceptions::TrapFrame;
use crate::paging;
use crate::tasks;
use crate::uart;

pub const SYS_EXIT: u64 = 0;
pub const SYS_WRITE: u64 = 1;
pub const SYS_YIELD: u64 = 2;
pub const SYS_GETPID: u64 = 3;

/// Returned for anything we do not recognise or will not do.
///
/// An error, not a panic. A task is allowed to be wrong; the kernel is not
/// allowed to fall over because of it.
const EINVAL: u64 = -1i64 as u64;
const EFAULT: u64 = -2i64 as u64;

/// The largest write we will accept in one call.
const MAX_WRITE: u64 = 4096;

/// Handle an `SVC` from EL0.
///
/// `ELR_EL1` already points past the `svc` instruction; unlike `BRK`, the
/// hardware advances it. Stepping it again here would silently skip whatever
/// instruction follows every syscall.
pub fn dispatch(frame: &mut TrapFrame) {
    let number = frame.x[8];
    let args = [frame.x[0], frame.x[1], frame.x[2]];

    let result = match number {
        SYS_EXIT => sys_exit(args[0]),
        SYS_WRITE => sys_write(args[0], args[1], args[2]),
        SYS_YIELD => sys_yield(),
        SYS_GETPID => sys_getpid(),
        _ => EINVAL,
    };

    frame.x[0] = result;
}

fn sys_exit(code: u64) -> u64 {
    tasks::exit_current(code)
}

fn sys_yield() -> u64 {
    tasks::yield_now();
    0
}

fn sys_getpid() -> u64 {
    tasks::current_id().0 as u64
}

/// Write `len` bytes from `buf` to the console.
///
/// `fd` is accepted and ignored beyond checking it is 1. There is no file
/// table yet, and pretending otherwise would be worse than being honest about
/// it.
fn sys_write(fd: u64, buf: u64, len: u64) -> u64 {
    if fd != 1 {
        return EINVAL;
    }

    if len > MAX_WRITE {
        return EINVAL;
    }

    let Some(root) = tasks::current_space_root() else {
        // A task with no address space has no user memory to read from, so any
        // pointer it passes refers to something it does not own.
        return EFAULT;
    };

    // The check that matters. Walks the caller's own tables and asks whether
    // EL0 could have read this, rather than whether EL1 can.
    if !paging::user_readable(root, buf, len) {
        return EFAULT;
    }

    let bytes = unsafe { core::slice::from_raw_parts(buf as *const u8, len as usize) };

    match core::str::from_utf8(bytes) {
        Ok(text) => {
            uart::write_str(text);
            len
        }
        Err(_) => EINVAL,
    }
}
