//! Synchronous message passing: the only way two tasks are allowed to
//! cooperate.
//!
//! # Why a rendezvous and not a queue
//!
//! `send` blocks until the target replies. There is no buffer in the middle,
//! no message sitting in kernel memory waiting to be collected, and no
//! allocation on the send path at all. Every message in flight is described by
//! two buffers that belong to the two tasks involved, and the kernel copies
//! between them exactly once.
//!
//! That falls out of one decision and pays for itself three times. There is no
//! queue to size, so there is no wrong size to pick. There is nothing for the
//! kernel to allocate, so a task cannot make the kernel run out of memory by
//! sending faster than anybody listens. And back pressure is free: a sender
//! that outruns its receiver blocks, which is information about the system
//! rather than a slow leak of it.
//!
//! The cost is real and worth naming. A task that sends to a task that never
//! receives waits forever, so who may send to whom becomes a design decision
//! rather than an accident, and a cycle of sends is a deadlock. Hubris answers
//! that with a strict priority ordering on who is allowed to send to whom.
//! Nothing enforces that here yet.
//!
//! # Why the kernel does the copying
//!
//! Neither task can see the other's memory; that was the entire point of tier
//! 2's address spaces. So the kernel reads the sender's buffer and writes the
//! receiver's, and it validates each one against *its own owner's* page
//! tables. The sender's buffer is checked against the sender's tables even
//! though the receiver is the task running at the time. Checking against
//! whoever happens to be current would let a receiver name any address it
//! liked and have the kernel treat it as the sender's.

use crate::frames::Frame;
use crate::paging;
use crate::sync::{self, Lock};
use crate::tasks::{self, MAX_TASKS, Priority, TaskId};

/// Largest message or reply the kernel will copy in one call.
///
/// A bound rather than a limit anybody asked for. The copy happens with
/// interrupts masked and a task chooses the length, so an unbounded copy is an
/// unbounded amount of time in which nothing else on the machine can run. One
/// page is enough for anything a task should be passing by value, and anything
/// larger is what leases (#29) are for.
pub const MAX_MESSAGE: u64 = 256;

pub const EINVAL: u64 = -1i64 as u64;
pub const EFAULT: u64 = -2i64 as u64;
/// The other end is gone, or was never there.
pub const EDEAD: u64 = -3i64 as u64;

/// One half of a rendezvous, from the sender's side.
#[derive(Clone, Copy)]
struct Message {
    target: usize,
    operation: u64,
    /// Outgoing bytes, in the sender's address space.
    out: u64,
    out_len: u64,
    /// Where the reply goes, in the sender's address space.
    reply_buf: u64,
    reply_cap: u64,
}

/// What a blocked task is blocked on.
///
/// Kept here rather than inside `State` so that the scheduler stays ignorant
/// of IPC. It only needs to know a task is blocked; why it is blocked is this
/// module's business, and keeping the two apart means notifications and
/// timeouts can add their own reasons later without touching the picker.
#[derive(Clone, Copy)]
#[allow(clippy::enum_variant_names)] // All three really are kinds of waiting.
enum Pending {
    /// Sent, nobody has picked it up yet.
    SendWait(Message),
    /// Picked up and being worked on. Waiting for `reply`.
    ReplyWait(Message),
    /// Waiting for somebody to send. Buffer is in the receiver's space.
    RecvWait { buf: u64, cap: u64 },
}

/// What a task found waiting for it when it woke up.
///
/// Written by whoever unblocked it, read by the blocked task itself the
/// instant it resumes inside the syscall. Three words because that is the
/// widest return any of these calls has.
#[derive(Clone, Copy, Default)]
struct Outcome {
    a: u64,
    b: u64,
    c: u64,
}

struct Table {
    pending: [Option<Pending>; MAX_TASKS],
    outcome: [Outcome; MAX_TASKS],
}

static TABLE: Lock<Table> = Lock::new(Table {
    pending: [None; MAX_TASKS],
    outcome: [Outcome { a: 0, b: 0, c: 0 }; MAX_TASKS],
});

/// Where a task's message buffers live.
///
/// A task with no address space has no user memory, so every pointer it could
/// pass names something it does not own. Refusing here rather than further in
/// keeps the "check against the owner's tables" rule from having an exception.
fn root_of(id: TaskId) -> Option<Frame> {
    tasks::space_root_of(id)
}

/// Copy a message body from one task to another, capped at what the
/// destination said it could hold.
///
/// Truncation rather than refusal is deliberate: the receiver decides how much
/// it is willing to look at, and a sender should not be able to fail a
/// receiver's call by being verbose. The returned length is what actually
/// arrived, so the receiver is never told it got more than it did.
fn deliver(
    src: TaskId,
    src_buf: u64,
    src_len: u64,
    dst: TaskId,
    dst_buf: u64,
    dst_cap: u64,
) -> Option<u64> {
    let len = src_len.min(dst_cap);
    if len == 0 {
        return Some(0);
    }

    let (src_root, dst_root) = (root_of(src)?, root_of(dst)?);

    // Each side against its own owner's tables. This is the line that keeps a
    // receiver from naming an address and having it treated as the sender's.
    if !paging::user_readable(src_root, src_buf, len) {
        return None;
    }
    if !paging::user_writable(dst_root, dst_buf, len) {
        return None;
    }

    paging::copy_across(src_root, src_buf, dst_root, dst_buf, len).then_some(len)
}

/// Send a message and block until the target replies.
///
/// Returns `(rc, len)`: whatever the replier chose, and how much of the reply
/// landed in `reply_buf`.
pub fn send(
    target: u64,
    operation: u64,
    out: u64,
    out_len: u64,
    reply_buf: u64,
    reply_cap: u64,
) -> (u64, u64) {
    if out_len > MAX_MESSAGE || reply_cap > MAX_MESSAGE {
        return (EINVAL, 0);
    }
    if target as usize >= MAX_TASKS {
        return (EINVAL, 0);
    }

    let me = tasks::current_id();
    let target_id = TaskId(target as usize);

    if target_id == me {
        // Would block forever waiting for itself, which is a deadlock the
        // kernel can see coming.
        return (EINVAL, 0);
    }
    if !tasks::is_alive(target_id) {
        return (EDEAD, 0);
    }

    let Some(root) = root_of(me) else {
        return (EFAULT, 0);
    };
    // Checked here as well as in `deliver`, so a bad pointer is an immediate
    // error rather than something the sender discovers after blocking.
    if !paging::user_readable(root, out, out_len)
        || !paging::user_writable(root, reply_buf, reply_cap)
    {
        return (EFAULT, 0);
    }

    let message = Message {
        target: target_id.0,
        operation,
        out,
        out_len,
        reply_buf,
        reply_cap,
    };

    // Masked across the whole rendezvous. Deciding the target is receiving and
    // then acting on it has to be one step: a timer tick in between could run
    // the target, which would leave us handing a message to a task that is no
    // longer waiting for one.
    let state = sync::disable_interrupts();

    let handoff = {
        let mut table = TABLE.lock();
        match table.pending[target_id.0] {
            Some(Pending::RecvWait { buf, cap }) => {
                table.pending[target_id.0] = None;
                Some((buf, cap))
            }
            _ => None,
        }
    };

    match handoff {
        // The target is already waiting. Copy now and wake it.
        Some((buf, cap)) => {
            let Some(len) = deliver(me, out, out_len, target_id, buf, cap) else {
                // Put the receiver back where it was. It never learned that
                // anything happened, so from its side this send did not occur.
                TABLE.lock().pending[target_id.0] = Some(Pending::RecvWait { buf, cap });
                sync::restore_interrupts(state);
                return (EFAULT, 0);
            };

            // Out of the run queue first, then wake the target. The target
            // may reply the instant it runs, and a reply that lands while the
            // sender still looks runnable is a wakeup delivered to nobody.
            tasks::mark_current_blocked();

            {
                let mut table = TABLE.lock();
                table.outcome[target_id.0] = Outcome {
                    a: me.0 as u64,
                    b: operation,
                    c: len,
                };
                table.pending[me.0] = Some(Pending::ReplyWait(message));
            }

            // May switch to the target on this line if it outranks us, and may
            // not come back until the whole exchange is over. Both are fine:
            // `park` below checks whether there is still anything to wait for.
            tasks::unblock(target_id);
            tasks::park();
        }
        // Nobody is listening yet. Wait to be collected.
        None => {
            TABLE.lock().pending[me.0] = Some(Pending::SendWait(message));
            tasks::mark_current_blocked();
            tasks::park();
        }
    }

    // Read, not cleared. Whoever wrote the outcome cleared `pending` at the
    // same moment, which is the only place that pairing is allowed to happen.
    let outcome = core::mem::take(&mut TABLE.lock().outcome[me.0]);

    sync::restore_interrupts(state);

    (outcome.a, outcome.b)
}

/// Block until somebody sends, then return `(sender, operation, len)`.
pub fn recv(buf: u64, cap: u64) -> (u64, u64, u64) {
    if cap > MAX_MESSAGE {
        return (EINVAL, 0, 0);
    }

    let me = tasks::current_id();
    let Some(root) = root_of(me) else {
        return (EFAULT, 0, 0);
    };
    if !paging::user_writable(root, buf, cap) {
        return (EFAULT, 0, 0);
    }

    let state = sync::disable_interrupts();

    // Loops rather than recurses, because each turn of it is a sender being
    // rejected for a bad buffer, and there can be one of those per task.
    // Recursion here would put an attacker-chosen number of frames on a kernel
    // stack four pages deep.
    let result = loop {
        // Somebody may already be waiting. Take the best of them rather than
        // the first: senders queue by priority, so the most urgent request is
        // served first even if it arrived last. Scanning is fine at this table
        // size and avoids a second sorted structure to keep honest.
        let Some(sender) = best_sender(me) else {
            TABLE.lock().pending[me.0] = Some(Pending::RecvWait { buf, cap });
            tasks::mark_current_blocked();
            tasks::park();

            let outcome = core::mem::take(&mut TABLE.lock().outcome[me.0]);
            break (outcome.a, outcome.b, outcome.c);
        };

        let Some(Pending::SendWait(message)) = TABLE.lock().pending[sender.0] else {
            unreachable!("best_sender returned a task that is not sending")
        };

        match deliver(sender, message.out, message.out_len, me, buf, cap) {
            Some(len) => {
                TABLE.lock().pending[sender.0] = Some(Pending::ReplyWait(message));
                break (sender.0 as u64, message.operation, len);
            }
            // The sender's buffer is unreadable. That is the sender's fault,
            // so the sender is the one that fails: it gets its error and this
            // receive looks at the next candidate, rather than the receiver
            // inheriting somebody else's bad pointer.
            None => finish_sender(sender, EFAULT, 0),
        }
    };

    sync::restore_interrupts(state);
    result
}

/// Answer a message and let its sender go.
pub fn reply(sender: u64, rc: u64, data: u64, data_len: u64) -> u64 {
    if data_len > MAX_MESSAGE || sender as usize >= MAX_TASKS {
        return EINVAL;
    }

    let me = tasks::current_id();
    let sender_id = TaskId(sender as usize);

    let state = sync::disable_interrupts();

    // Only the task that received the message may answer it, and only while
    // the sender is actually waiting. Without the first check any task could
    // release somebody else's sender and feed it a fabricated reply.
    let message = match TABLE.lock().pending[sender_id.0] {
        Some(Pending::ReplyWait(message)) if message.target == me.0 => message,
        _ => {
            sync::restore_interrupts(state);
            return EINVAL;
        }
    };

    let len = match deliver(
        me,
        data,
        data_len,
        sender_id,
        message.reply_buf,
        message.reply_cap,
    ) {
        Some(len) => len,
        None => {
            sync::restore_interrupts(state);
            return EFAULT;
        }
    };

    finish_sender(sender_id, rc, len);

    sync::restore_interrupts(state);
    0
}

/// Hand a blocked sender its result and put it back in the running.
fn finish_sender(sender: TaskId, rc: u64, len: u64) {
    {
        let mut table = TABLE.lock();
        table.outcome[sender.0] = Outcome {
            a: rc,
            b: len,
            c: 0,
        };
        // Cleared here, by the waker, not later by the sender. `pending` means
        // "what this task is waiting for right now", and the moment an outcome
        // is written it is waiting for nothing. Leaving it set until the sender
        // got around to running left a window in which the answer was already
        // delivered but the sender still looked like it was waiting on the
        // replier, and `abandon` would overwrite a perfectly good reply with
        // EDEAD when that replier exited a few instructions later.
        table.pending[sender.0] = None;
    }
    tasks::unblock(sender);
}

/// The highest priority task currently waiting to send to `target`.
///
/// Ties go to the lower slot number, which is arbitrary but stable. Priority
/// order here is what stops a busy low priority client from delaying an urgent
/// one that arrived while it was queued.
fn best_sender(target: TaskId) -> Option<TaskId> {
    let table = TABLE.lock();
    let mut best: Option<(TaskId, Priority)> = None;

    for slot in 0..MAX_TASKS {
        let Some(Pending::SendWait(message)) = table.pending[slot] else {
            continue;
        };
        if message.target != target.0 {
            continue;
        }
        let Some(priority) = tasks::priority_of(TaskId(slot)) else {
            continue;
        };
        match best {
            Some((_, best_priority)) if !priority.outranks(best_priority) => {}
            _ => best = Some((TaskId(slot), priority)),
        }
    }

    best.map(|(id, _)| id)
}

/// Release anybody stuck on a task that has stopped for good.
///
/// Checking liveness at `send` time is not enough on its own: a target can
/// exit or fault after the message is queued, and a sender waiting on a task
/// that no longer exists waits forever. Called from the exit and fault paths,
/// so a dying task takes its correspondents' hopes with it rather than their
/// progress.
pub fn abandon(gone: TaskId) {
    for slot in 0..MAX_TASKS {
        if slot == gone.0 {
            continue;
        }

        let waiting_on_gone = {
            let table = TABLE.lock();
            match table.pending[slot] {
                Some(Pending::SendWait(message) | Pending::ReplyWait(message)) => {
                    message.target == gone.0
                }
                _ => false,
            }
        };

        if waiting_on_gone {
            finish_sender(TaskId(slot), EDEAD, 0);
        }
    }

    // Whatever the departing task was itself waiting on is no longer anybody's
    // business, and leaving it behind would make the slot look occupied to the
    // next task that lands in it.
    TABLE.lock().pending[gone.0] = None;
}
