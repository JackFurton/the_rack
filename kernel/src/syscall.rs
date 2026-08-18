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
use crate::ipc;
use crate::paging;
use crate::tasks;
use crate::uart;

pub const SYS_EXIT: u64 = 0;
pub const SYS_WRITE: u64 = 1;
pub const SYS_YIELD: u64 = 2;
pub const SYS_GETPID: u64 = 3;
pub const SYS_SEND: u64 = 4;
pub const SYS_RECV: u64 = 5;
pub const SYS_REPLY: u64 = 6;
pub const SYS_BORROW_READ: u64 = 7;
pub const SYS_BORROW_WRITE: u64 = 8;

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
    // Eight argument registers rather than the six Linux uses. `send` genuinely
    // needs eight: a target, an operation, a message, a reply buffer, and a
    // lease table are five things with a length each. Packing two of them into
    // one register to hit a number borrowed from another kernel would buy
    // nothing and cost a shift at both ends of every call.
    let a = [
        frame.x[0], frame.x[1], frame.x[2], frame.x[3], frame.x[4], frame.x[5], frame.x[6],
        frame.x[7],
    ];

    // The message calls return two or three values, so results come back as a
    // triple and the unused words are zeros. Writing every one of x0..x2 on
    // every syscall keeps a caller from reading a stale register and believing
    // it, which is a bug that only shows up in the calls that return less.
    let (r0, r1, r2) = match number {
        SYS_EXIT => (sys_exit(a[0]), 0, 0),
        SYS_WRITE => (sys_write(a[0], a[1], a[2]), 0, 0),
        SYS_YIELD => (sys_yield(), 0, 0),
        SYS_GETPID => (sys_getpid(), 0, 0),
        SYS_SEND => {
            let (rc, len) = ipc::send(a[0], a[1], a[2], a[3], a[4], a[5], a[6], a[7]);
            (rc, len, 0)
        }
        SYS_RECV => ipc::recv(a[0], a[1]),
        SYS_REPLY => (ipc::reply(a[0], a[1], a[2], a[3]), 0, 0),
        SYS_BORROW_READ => (ipc::borrow_read(a[0], a[1], a[2], a[3], a[4]), 0, 0),
        SYS_BORROW_WRITE => (ipc::borrow_write(a[0], a[1], a[2], a[3], a[4]), 0, 0),
        _ => (EINVAL, 0, 0),
    };

    frame.x[0] = r0;
    frame.x[1] = r1;
    frame.x[2] = r2;
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
