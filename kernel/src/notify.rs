//! Notifications: how an interrupt reaches a driver that runs at EL0.
//!
//! The kernel's whole part in a device interrupt is to acknowledge it at the
//! controller, set a bit, and get out. Everything device specific happens in an
//! ordinary unprivileged task. That is the Hubris shape, and it is what makes
//! a driver bug a task fault rather than a kernel panic.
//!
//! # Why a bitmask and not a queue
//!
//! Two of the same event before the task gets a turn collapse into one bit. It
//! is not a limitation to be worked around later; it is the contract. A
//! notification says *something happened*, never *how many times*, and a
//! driver that needs the count reads it from the device rather than inferring
//! it from how often it was woken.
//!
//! The alternative costs more than it looks. A queue needs a length, and a
//! length needs an answer to what happens when it fills: drop the oldest, drop
//! the newest, or block the interrupt handler. All three are worse than a bit
//! that is already set, and all three make the kernel allocate on a path that
//! runs with interrupts masked. A bit that is already set needs no decision at
//! all.
//!
//! The heartbeat in this kernel is the demonstration. It is woken by a bit and
//! then asks the kernel for the tick count, so a notification it never saw
//! costs it nothing: the count it reads is right either way.
//!
//! # Why routing is not a syscall
//!
//! Which task owns which interrupt is set by the kernel at startup. Letting a
//! task claim an interrupt line would mean any task could take another's
//! device, or silence it, and there is no way to tell a legitimate claim from
//! a theft without something that already knows the intended shape of the
//! system. The supervisor is the natural place for that; it does not exist for
//! this purpose yet.

use crate::sync::Lock;
use crate::tasks::{self, MAX_TASKS, TaskId};

/// How many interrupt lines can be routed to tasks at once.
///
/// Scanned linearly on every interrupt, which is why it is a short list rather
/// than an array indexed by interrupt number: the machine reports 288 lines
/// and a handful will ever be routed, so the sparse form is both smaller and
/// the one that stays honest as the number of lines grows.
const MAX_ROUTES: usize = 8;

#[derive(Clone, Copy)]
struct Route {
    intid: u32,
    task: TaskId,
    bits: u32,
}

static ROUTES: Lock<[Option<Route>; MAX_ROUTES]> = Lock::new([None; MAX_ROUTES]);

/// Notifications posted to each task and not yet collected.
static POSTED: Lock<[u32; MAX_TASKS]> = Lock::new([0; MAX_TASKS]);

/// What each task is currently parked waiting for, or zero if it is not parked.
///
/// Kept here rather than read out of the IPC tables so that posting a
/// notification never has to reason about message state. The two wait
/// conditions meet in `recv` and nowhere else.
static WANTED: Lock<[u32; MAX_TASKS]> = Lock::new([0; MAX_TASKS]);

/// Send `intid` to `task` as `bits` from now on.
pub fn route(intid: u32, task: TaskId, bits: u32) {
    let mut routes = ROUTES.lock();
    let slot = routes
        .iter()
        .position(Option::is_none)
        .expect("interrupt routing table is full");
    routes[slot] = Some(Route { intid, task, bits });
}

/// Deliver an interrupt to whoever owns it. Returns whether anybody did.
///
/// Called from the IRQ path with the interrupt still active at the controller,
/// so nothing here may give up the CPU. `post` wakes without switching for
/// exactly that reason, and the switch happens later, after the EOI, through
/// the reschedule the caller asks for.
pub fn on_irq(intid: u32) -> bool {
    let route = ROUTES
        .lock()
        .iter()
        .flatten()
        .find(|route| route.intid == intid)
        .copied();

    match route {
        Some(route) => {
            post(route.task, route.bits);
            true
        }
        None => false,
    }
}

/// Set notification bits on a task, waking it if it was waiting for any of
/// them.
///
/// Never switches. Callers include the interrupt path, where switching before
/// the controller has been told we are finished would stop the very interrupt
/// that got us here.
pub fn post(task: TaskId, bits: u32) {
    if task.0 >= MAX_TASKS {
        return;
    }

    POSTED.lock()[task.0] |= bits;

    // Only a task waiting for one of these bits is woken. A bit nobody asked
    // for stays set and is delivered whenever it is next asked for, which is
    // what makes it safe to post something a task is not currently interested
    // in rather than having to know what it is doing.
    if WANTED.lock()[task.0] & bits != 0 {
        tasks::unblock_deferred(task);
        tasks::request_reschedule();
    }
}

/// Collect whichever of `mask` have been posted, clearing exactly those.
///
/// Bits outside `mask` are left alone. A task that asks about one event does
/// not thereby discard another it has not got around to.
pub fn take(task: TaskId, mask: u32) -> u32 {
    let mut posted = POSTED.lock();
    let fired = posted[task.0] & mask;
    posted[task.0] &= !fired;
    fired
}

/// Declare what a task is about to park waiting for.
pub fn arm(task: TaskId, mask: u32) {
    WANTED.lock()[task.0] = mask;
}

/// This task is running again and is not waiting for anything.
pub fn disarm(task: TaskId) {
    WANTED.lock()[task.0] = 0;
}

/// Forget everything about a task, for a slot that is being reused.
pub fn clear(task: TaskId) {
    POSTED.lock()[task.0] = 0;
    WANTED.lock()[task.0] = 0;
}
