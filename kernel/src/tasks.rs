//! Kernel tasks and cooperative switching.
//!
//! This is the piece everything else in tier 2 stands on. A scheduler is just
//! a policy for choosing who runs next; being able to stop one thing and start
//! another is the mechanism, and it is worth having working and debuggable
//! before an interrupt can arrive in the middle of it.
//!
//! Switching here is voluntary only. Preemption is a separate problem with a
//! different set of registers to save, and mixing the two before either works
//! is a good way to spend a week.
//!
//! # What a task is
//!
//! A kernel stack and a saved stack pointer. That is genuinely all, at this
//! stage. There is no address space yet, no privilege boundary, no register
//! file kept anywhere except on the task's own stack. Everything a suspended
//! task remembers is sitting in a 96 byte frame that `switch` pushed onto it.

use core::arch::global_asm;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::frames::{self, FRAME_SIZE, Frame};
use crate::paging;
use crate::sync::{self, Lock};
use crate::{print, println};

global_asm!(include_str!("switch.S"));

unsafe extern "C" {
    /// Save the callee-saved set, swap stacks, restore the other one's.
    fn switch(save_sp_to: *mut u64, resume_sp: u64);
    /// Entry stub for a task that has never run.
    fn task_trampoline();
}

/// Kernel stack size per task. Four frames.
///
/// Generous, because a debug build's stack frames are large and because a
/// kernel stack overflow does not announce itself: it quietly writes into
/// whatever is below, and the damage surfaces somewhere unrelated.
const STACK_FRAMES: usize = 4;
const STACK_SIZE: u64 = STACK_FRAMES as u64 * FRAME_SIZE;

/// Written at the lowest address of every kernel stack and checked afterwards.
/// Cheap, and turns silent overflow into something we can actually see.
const STACK_CANARY: u64 = 0x5441_434b_5f43_414e; // "TACK_CAN"

const MAX_TASKS: usize = 8;

/// Offsets within the 96 byte frame `switch.S` pushes, in u64 units.
///
/// These have to agree with the `stp` offsets in `switch.S`. They are how a
/// task that has never run is given its starting register values.
const SLOT_X19: usize = 0;
const SLOT_X20: usize = 1;
const SLOT_X29: usize = 10;
const SLOT_X30: usize = 11;
const SWITCH_FRAME_WORDS: usize = 12;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
    Runnable,
    Finished,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TaskId(pub usize);

struct Task {
    name: &'static str,
    /// Where this task's saved switch frame lives. Only meaningful while the
    /// task is not running.
    sp: u64,
    /// Base of the kernel stack, kept so it can be handed back later.
    stack: Frame,
    state: State,
}

struct Scheduler {
    tasks: [Option<Task>; MAX_TASKS],
    current: usize,
}

impl Scheduler {
    const fn new() -> Self {
        Self {
            tasks: [const { None }; MAX_TASKS],
            current: 0,
        }
    }

    fn free_slot(&self) -> Option<usize> {
        self.tasks.iter().position(Option::is_none)
    }

    /// Next runnable task after `current`, wrapping. Returns `current` itself
    /// if nothing else can run.
    fn next_runnable(&self) -> usize {
        for step in 1..=MAX_TASKS {
            let candidate = (self.current + step) % MAX_TASKS;
            if let Some(task) = &self.tasks[candidate]
                && task.state == State::Runnable
            {
                return candidate;
            }
        }
        self.current
    }
}

static SCHEDULER: Lock<Scheduler> = Lock::new(Scheduler::new());

/// Record of who handed off to whom, for the preemption self test.
///
/// Written from inside the scheduler lock, so it is a faithful record rather
/// than a sampled one.
const SWITCH_LOG_LEN: usize = 32;

/// Why a switch happened. Recorded rather than inferred, because inferring it
/// from the task ids alone quietly counts a finishing task's handoff as a
/// preemption and reports a working scheduler that never preempted anything.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    Voluntary,
    Preemptive,
}

struct SwitchLog {
    from: [u8; SWITCH_LOG_LEN],
    to: [u8; SWITCH_LOG_LEN],
    preemptive: [bool; SWITCH_LOG_LEN],
    len: usize,
    /// Switches that happened after the log filled up. Kept so the test can
    /// tell "preemption stopped" from "the log ran out of room".
    overflow: usize,
    /// Counted for every switch, logged or not.
    preemptions: usize,
}

static SWITCH_LOG: Lock<SwitchLog> = Lock::new(SwitchLog {
    from: [0; SWITCH_LOG_LEN],
    to: [0; SWITCH_LOG_LEN],
    preemptive: [false; SWITCH_LOG_LEN],
    len: 0,
    overflow: 0,
    preemptions: 0,
});

fn note_switch(from: usize, to: usize, reason: Reason) {
    let mut log = SWITCH_LOG.lock();
    if log.len < SWITCH_LOG_LEN {
        let index = log.len;
        log.from[index] = from as u8;
        log.to[index] = to as u8;
        log.preemptive[index] = reason == Reason::Preemptive;
        log.len += 1;
    } else {
        log.overflow += 1;
    }
    if reason == Reason::Preemptive {
        log.preemptions += 1;
    }
}

fn reset_switch_log() {
    let mut log = SWITCH_LOG.lock();
    log.len = 0;
    log.overflow = 0;
    log.preemptions = 0;
}

/// Adopt the currently executing context as task 0.
///
/// The kernel is already running on a stack with a call history; it does not
/// need to be created, only named. Its `sp` is filled in the first time it
/// switches away.
pub fn init() {
    let mut scheduler = SCHEDULER.lock();
    scheduler.tasks[0] = Some(Task {
        name: "kernel",
        sp: 0,
        // Task 0's stack came from the linker, not the allocator. Recorded so
        // the field is not a lie, and never freed.
        stack: Frame::from_addr(0x4000_0000),
        state: State::Runnable,
    });
    scheduler.current = 0;
}

/// Create a task that will begin at `entry(arg)` the next time it is chosen.
///
/// The trick is that a new task has to look exactly like a suspended one, so
/// `switch` needs no special case for the first run. We fake the frame
/// `switch` would have pushed, with `x30` pointing at the trampoline and the
/// entry point and argument sitting in the saved `x19` and `x20` slots.
pub fn spawn(name: &'static str, entry: extern "C" fn(u64), arg: u64) -> TaskId {
    let stack = frames::alloc_contiguous(STACK_FRAMES).expect("no frames for a kernel stack");

    let stack_base = paging::phys_to_virt(stack.addr());
    let stack_top = stack_base + STACK_SIZE;

    unsafe { core::ptr::write_volatile(stack_base as *mut u64, STACK_CANARY) };

    // The frame sits at the very top of the stack, 16 byte aligned as the ABI
    // requires.
    let sp = stack_top - SWITCH_FRAME_WORDS as u64 * 8;
    let frame = sp as *mut u64;

    unsafe {
        core::ptr::write_volatile(frame.add(SLOT_X19), entry as *const () as u64);
        core::ptr::write_volatile(frame.add(SLOT_X20), arg);
        // No caller below us, so the frame pointer chain ends here.
        core::ptr::write_volatile(frame.add(SLOT_X29), 0);
        core::ptr::write_volatile(frame.add(SLOT_X30), task_trampoline as *const () as u64);
    }

    let mut scheduler = SCHEDULER.lock();
    let slot = scheduler.free_slot().expect("task table is full");
    scheduler.tasks[slot] = Some(Task {
        name,
        sp,
        stack,
        state: State::Runnable,
    });

    TaskId(slot)
}

/// Give up the CPU to the next runnable task.
///
/// Returns once something switches back, which may be a long time and a lot of
/// other work later.
pub fn yield_now() {
    // Interrupts stay masked from here until this task is running again.
    //
    // `state` is a local, which means it lives on *this task's* stack. So when
    // this task is eventually resumed it restores the mask it was holding, not
    // whatever the task that happened to switch to it was holding. Getting
    // this wrong is subtle: the interrupt mask is a property of the CPU, but
    // the value that should be restored is a property of the task.
    let state = sync::disable_interrupts();

    switch_to_next(Reason::Voluntary);

    sync::restore_interrupts(state);
}

/// Pick the next runnable task and switch to it, if there is one.
///
/// Must be called with interrupts already masked. Shared by the voluntary and
/// the preemptive paths, which differ only in how they got here.
fn switch_to_next(reason: Reason) {
    let (save_to, resume) = {
        let mut scheduler = SCHEDULER.lock();

        // Nothing to switch between before `init` has run.
        if scheduler.tasks[scheduler.current].is_none() {
            return;
        }

        let next = scheduler.next_runnable();
        if next == scheduler.current {
            return;
        }

        let current = scheduler.current;
        scheduler.current = next;
        note_switch(current, next, reason);

        // Raw pointers deliberately outlive the guard, because the switch must
        // not happen while the lock is held. Sound because SCHEDULER is a
        // static that never moves, and because interrupts are masked for the
        // whole window, so nothing else can reach the table in between.
        let save_to = &mut scheduler.tasks[current].as_mut().unwrap().sp as *mut u64;
        let resume = scheduler.tasks[next].as_ref().unwrap().sp;

        (save_to, resume)
    };

    unsafe { switch(save_to, resume) };
}

/// Set by the timer handler to ask for a reschedule.
static NEED_RESCHED: AtomicBool = AtomicBool::new(false);

/// Ask for the current task to be preempted at the next safe moment.
pub fn request_reschedule() {
    NEED_RESCHED.store(true, Ordering::Relaxed);
}

/// Switch tasks if the timer asked for it.
///
/// Called from the IRQ path *after* the interrupt has been acknowledged to the
/// GIC. Order matters: switching first would leave the interrupt active while
/// another task runs, and the controller will not deliver anything of equal or
/// lower priority until it is released, so the timer would appear to stop.
///
/// The switch nests inside the trap frame rather than replacing it. The frame
/// `vectors.S` pushed stays on this task's stack, `switch` pushes its own
/// frame above it, and both are still there when this task is resumed and
/// unwinds back out through `eret`.
pub fn preempt_if_needed() {
    if !NEED_RESCHED.swap(false, Ordering::Relaxed) {
        return;
    }

    // Already masked: the hardware sets PSTATE.I on taking an IRQ, and `eret`
    // restores the caller's mask from SPSR on the way out.
    switch_to_next(Reason::Preemptive);
}

/// The task currently on the CPU.
pub fn current_id() -> TaskId {
    TaskId(SCHEDULER.lock().current)
}

/// Where a task lands when its entry function returns.
///
/// Called from `task_trampoline`, never from Rust.
#[unsafe(no_mangle)]
pub extern "C" fn task_finished() -> ! {
    {
        let mut scheduler = SCHEDULER.lock();
        let current = scheduler.current;
        if let Some(task) = scheduler.tasks[current].as_mut() {
            task.state = State::Finished;
        }
    }

    // The stack under our feet belongs to this task, so it cannot be reclaimed
    // from here. Freeing it is #20's problem, and doing it now would mean
    // running on memory we had already given away.
    loop {
        yield_now();
    }
}

/// How many tasks other than the current one can still run.
pub fn runnable_others() -> usize {
    let scheduler = SCHEDULER.lock();
    scheduler
        .tasks
        .iter()
        .enumerate()
        .filter(|(slot, task)| {
            *slot != scheduler.current
                && matches!(task, Some(task) if task.state == State::Runnable)
        })
        .count()
}

/// Check every task's stack canary.
pub fn canaries_intact() -> bool {
    let scheduler = SCHEDULER.lock();
    scheduler
        .tasks
        .iter()
        .enumerate()
        .filter(|(slot, _)| *slot != 0) // task 0's stack is the linker's
        .filter_map(|(_, task)| task.as_ref())
        .all(|task| {
            let base = paging::phys_to_virt(task.stack.addr());
            unsafe { core::ptr::read_volatile(base as *const u64) == STACK_CANARY }
        })
}

pub fn print_table() {
    let scheduler = SCHEDULER.lock();
    println!("tasks:");
    for (slot, task) in scheduler.tasks.iter().enumerate() {
        let Some(task) = task else { continue };
        println!(
            "  {}{} {:<8} stack {:#012x}  {:?}",
            slot,
            if slot == scheduler.current { "*" } else { " " },
            task.name,
            paging::phys_to_virt(task.stack.addr()),
            task.state
        );
    }
}

// The self test's shared record of who ran when.
const TRACE_LEN: usize = 8;

struct Trace {
    tags: [u64; TRACE_LEN],
    values: [u64; TRACE_LEN],
    len: usize,
}

static TRACE: Lock<Trace> = Lock::new(Trace {
    tags: [0; TRACE_LEN],
    values: [0; TRACE_LEN],
    len: 0,
});

fn record(tag: u64, value: u64) {
    let mut trace = TRACE.lock();
    if trace.len < TRACE_LEN {
        let index = trace.len;
        trace.tags[index] = tag;
        trace.values[index] = value;
        trace.len += 1;
    }
}

/// A task that counts in a local, yielding between increments.
///
/// `local` has to survive across `yield_now`, which means it lives in a
/// callee-saved register or on this task's own stack. Either way it is exactly
/// the state a broken `switch` would corrupt, which is why the self test
/// checks the values and not just the order.
extern "C" fn counting_worker(tag: u64) {
    let mut local = tag * 1000;

    for _ in 0..3 {
        local += 1;
        record(tag, local);
        yield_now();
    }
}

pub fn self_test() {
    init();

    spawn("ping", counting_worker, 1);
    spawn("pong", counting_worker, 2);

    // Task 0 stays in the rotation and does nothing but hand control on.
    while runnable_others() > 0 {
        yield_now();
    }

    let trace = TRACE.lock();

    // Strict round robin over three tasks, with task 0 contributing nothing,
    // gives strictly alternating workers.
    let expected_tags = [1, 2, 1, 2, 1, 2];
    let expected_values = [1001, 2001, 1002, 2002, 1003, 2003];

    assert_eq!(trace.len, 6, "wrong number of turns taken");
    assert_eq!(
        trace.tags[..6],
        expected_tags,
        "tasks did not alternate as expected"
    );
    // The real assertion. Correct order with wrong values would mean the
    // tasks ran but did not keep their own state across the switch.
    assert_eq!(
        trace.values[..6],
        expected_values,
        "a task's local state did not survive the switch"
    );
    drop(trace);

    assert!(canaries_intact(), "a kernel stack overflowed");

    print!("task self test: passed, ");
    println!("2 tasks alternated 3 turns each, locals intact, canaries intact");
}

/// Work each spinner does. Tuned so a spinner outlives several 10 ms ticks,
/// which is the only way preemption can be observed at all.
const SPIN_ROUNDS: u64 = 4_000_000;

/// Number of spinners in the preemption self test.
const SPINNERS: u64 = 3;

/// How many spinners have finished. Not behind the scheduler lock, because the
/// waiter reads it from another task.
static SPINNERS_DONE: Lock<u64> = Lock::new(0);

/// A task that never gives up the CPU voluntarily.
///
/// This is the whole point. It contains no call to `yield_now`, so if it ever
/// stops running while still having work left, the only thing that can have
/// moved it aside is the timer.
extern "C" fn spinner(_tag: u64) {
    for round in 0..SPIN_ROUNDS {
        // Opaque to the optimiser, so the loop survives into the binary.
        core::hint::black_box(round);
    }

    *SPINNERS_DONE.lock() += 1;
}

/// Prove that preemption actually preempts.
///
/// Requires interrupts to be live, so this runs after the GIC and timer are
/// up, unlike the cooperative self test.
pub fn preemption_self_test() {
    reset_switch_log();
    *SPINNERS_DONE.lock() = 0;

    for _ in 0..SPINNERS {
        spawn("spin", spinner, 0);
    }

    // The waiter yields, but the spinners never do, so any handoff *between*
    // spinners had to come from the timer.
    while *SPINNERS_DONE.lock() < SPINNERS {
        yield_now();
    }

    let log = SWITCH_LOG.lock();
    let recorded = log.len;
    let total = recorded + log.overflow;
    let preemptions = log.preemptions;

    let mut seen = [false; MAX_TASKS];
    for index in 0..recorded {
        seen[log.to[index] as usize] = true;
    }
    let distinct = seen.iter().filter(|running| **running).count();
    drop(log);

    // The spinners contain no yield at all, so every one of these is the timer
    // taking the CPU away from something that had not finished with it.
    //
    // The threshold is not 1. A single preemption is what you get from a
    // scheduler that preempts once and then stops, and one is also what the
    // first version of this test scored while preempting exactly zero times.
    assert!(
        preemptions >= SPINNERS as usize,
        "only {preemptions} preemptions across {SPINNERS} spinners; the timer is not taking the CPU away"
    );
    assert!(
        distinct >= SPINNERS as usize,
        "not every spinner got scheduled"
    );
    assert!(canaries_intact(), "a kernel stack overflowed");

    print!("preemption self test: passed, ");
    println!("{total} switches, {preemptions} of them preemptive, {distinct} tasks scheduled");
}
