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

use crate::exceptions::Fault;
use crate::frames::{self, FRAME_SIZE, Frame};
use crate::paging::{self, AddressSpace};
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

/// Task table size.
///
/// This is a limit on tasks alive at once, not on tasks ever created. The self
/// tests run eleven tasks through these eight slots, which is itself a check
/// that slots really do come back.
pub const MAX_TASKS: usize = 8;

/// Offsets within the 96 byte frame `switch.S` pushes, in u64 units.
///
/// These have to agree with the `stp` offsets in `switch.S`. They are how a
/// task that has never run is given its starting register values.
const SLOT_X19: usize = 0;
const SLOT_X20: usize = 1;
const SLOT_X29: usize = 10;
const SLOT_X30: usize = 11;
const SWITCH_FRAME_WORDS: usize = 12;

/// How important a task is. Lower number wins.
///
/// Backwards from intuition and deliberately so, because it is the convention
/// everything else in this space uses: Hubris, ARM's own interrupt priorities,
/// and Unix nice values all agree that smaller means more urgent. Flipping it
/// to be friendlier would make every comparison against the GIC's priority
/// registers read backwards, which is a worse trade than one surprising
/// `<`.
///
/// Scheduling is strictly priority ordered, not time sliced across
/// priorities: a runnable task at priority 0 runs, and a task at priority 1
/// does not, however long it has been waiting. That is what makes latency
/// something you can reason about instead of measure and hope. It also means
/// starvation is a real outcome rather than a bug, and the fix is to not give
/// a busy task a high priority.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub struct Priority(pub u8);

impl Priority {
    pub const HIGH: Self = Self(0);
    pub const NORMAL: Self = Self(1);
    pub const LOW: Self = Self(2);

    /// Reserved for the idle task. Nothing else should sit here, because a
    /// second task at this level would take turns with idle and the machine
    /// would look busy while doing nothing.
    pub const IDLE: Self = Self(u8::MAX);

    /// Does `self` get the CPU ahead of `other`?
    ///
    /// A named method rather than a bare `<`, because `a < b` meaning "a is
    /// more important than b" is exactly the sort of line that reads fine and
    /// is understood wrongly.
    pub fn outranks(self, other: Self) -> bool {
        self.0 < other.0
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum State {
    Runnable,
    /// Waiting for something that has not happened yet.
    ///
    /// Never a candidate for the CPU, no matter how high its priority. A
    /// blocked task is not slow, it is absent: the scheduler passes over it
    /// entirely and picks the best of what is left. Distinct from the finished
    /// states because a blocked task is expected back, and still owns every
    /// resource it had.
    Blocked,
    /// Finished, but still holding its kernel stack and address space.
    ///
    /// A task cannot free its own kernel stack: it is standing on it. So
    /// exiting is two steps, and this is the gap between them. Nothing will
    /// ever schedule a task in this state, which is what makes its stack safe
    /// for somebody else to release.
    Zombie,
    /// Resources returned to the allocator. Only the table slot and the exit
    /// code remain, so the exit code can still be collected.
    Dead,
    /// Stopped because it did something it was not allowed to.
    ///
    /// Distinct from a zombie on purpose. A faulted task keeps its stack and
    /// address space, because the point of catching the fault rather than
    /// panicking is that somebody may want to look at the wreckage or put the
    /// task back together. Reaping deliberately leaves these alone.
    Faulted,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct TaskId(pub usize);

struct Task {
    name: &'static str,
    /// The task's low half address space, if it has one.
    ///
    /// Task 0 does not. It is the kernel, it lives entirely in the high half,
    /// and giving it a low half would only create somewhere for a stray
    /// pointer to land quietly.
    space: Option<AddressSpace>,
    /// Where this task's saved switch frame lives. Only meaningful while the
    /// task is not running.
    sp: u64,
    /// Base of the kernel stack, kept so it can be handed back later.
    stack: Frame,
    /// Whether `stack` came from the frame allocator and must go back to it.
    ///
    /// Task 0's stack came from the linker. Handing that to the allocator
    /// would mark memory free that the kernel is still using, and the
    /// allocator would believe it.
    owns_stack: bool,
    priority: Priority,
    state: State,
    exit_code: Option<u64>,
    fault: Option<Fault>,
    /// Everything needed to build this task again from nothing.
    ///
    /// Only user tasks have one. A kernel task is a function pointer into an
    /// image that is already running, and there is nothing to rebuild; a user
    /// task is a blob plus an argument, which is exactly a recipe.
    image: Option<Image>,
}

/// How to reconstruct a user task.
#[derive(Clone, Copy)]
struct Image {
    start: u64,
    end: u64,
    arg: u64,
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

    /// The best runnable task, or `None` if nothing at all can run.
    ///
    /// Best means highest priority, and among equals the one that has waited
    /// longest. Both fall out of one scan: start just past `current` and keep
    /// the first task of the best priority seen. Starting past `current`
    /// rather than at slot 0 is what makes equal priorities round robin, and
    /// visiting `current` last is what makes it lose ties to its peers instead
    /// of hogging the CPU.
    ///
    /// `current` is only a candidate if it is still runnable. A task that just
    /// blocked itself is sitting in this table with its own slot number in
    /// `self.current`, and returning it would resume something that explicitly
    /// asked not to run.
    fn next_runnable(&self) -> Option<usize> {
        let mut best: Option<(usize, Priority)> = None;

        for step in 1..=MAX_TASKS {
            let candidate = (self.current + step) % MAX_TASKS;
            let Some(task) = &self.tasks[candidate] else {
                continue;
            };
            if task.state != State::Runnable {
                continue;
            }

            // Strictly outranks, so the earlier task in the scan keeps a tie.
            match best {
                Some((_, best_priority)) if !task.priority.outranks(best_priority) => {}
                _ => best = Some((candidate, task.priority)),
            }
        }

        best.map(|(slot, _)| slot)
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
        space: None,
        sp: 0,
        // Task 0's stack came from the linker, not the allocator. Recorded so
        // the field is not a lie, and never freed.
        stack: Frame::from_addr(0x4000_0000),
        owns_stack: false,
        // The kernel thread is the idle task. It is always runnable and always
        // the least important thing in the table, so it gets the CPU exactly
        // when nothing else can use it, which is the definition of idle. No
        // separate idle task needed, and no special case in the picker.
        priority: Priority::IDLE,
        state: State::Runnable,
        exit_code: None,
        fault: None,
        image: None,
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
    spawn_in(name, entry, arg, None, Priority::NORMAL)
}

/// Create a task at a chosen priority.
pub fn spawn_at(
    name: &'static str,
    entry: extern "C" fn(u64),
    arg: u64,
    priority: Priority,
) -> TaskId {
    spawn_in(name, entry, arg, None, priority)
}

/// Create a task that runs in its own address space.
pub fn spawn_in(
    name: &'static str,
    entry: extern "C" fn(u64),
    arg: u64,
    space: Option<AddressSpace>,
    priority: Priority,
) -> TaskId {
    let stack = frames::alloc_contiguous(STACK_FRAMES).expect("no frames for a kernel stack");
    let sp = plant_switch_frame(stack, entry, arg);

    let mut scheduler = SCHEDULER.lock();
    let slot = scheduler.free_slot().expect("task table is full");
    scheduler.tasks[slot] = Some(Task {
        name,
        space,
        sp,
        stack,
        owns_stack: true,
        priority,
        state: State::Runnable,
        exit_code: None,
        fault: None,
        image: None,
    });

    TaskId(slot)
}

/// Lay a fresh, never-run switch frame on a kernel stack and return the `sp`
/// that resumes it.
///
/// Separate from spawning because restarting a task needs exactly this and
/// nothing else around it: the stack it already owns, wound back to look like
/// a task that has never run. Nobody is standing on it, since a faulted task is
/// never scheduled, so overwriting it is safe and reusing the frames it already
/// has avoids handing memory back only to ask for it again.
fn plant_switch_frame(stack: Frame, entry: extern "C" fn(u64), arg: u64) -> u64 {
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

    sp
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

/// Take the current task out of the running until somebody puts it back.
///
/// Returns once `unblock` has been called on it. The difference from
/// `yield_now` is the whole point: a yielding task is asking to go last, a
/// blocking task is asking not to be considered at all. Priority does not
/// rescue it. The highest priority task in the system, blocked, is passed over
/// in favour of the lowest priority runnable one.
pub fn block_current() {
    let state = sync::disable_interrupts();

    mark_current_blocked();
    park();

    // By the time we are back, whoever woke us has already put the state back
    // to Runnable. Doing it here instead would leave a window where a resumed
    // task is still marked blocked.
    sync::restore_interrupts(state);
}

/// Take the current task out of the running, but keep the CPU for now.
///
/// The first half of `block_current`, split out because a task that is about
/// to wake somebody else has to be out of the run queue *before* it does the
/// waking. Otherwise the woken task can answer immediately, and its answer
/// arrives as a wakeup for a task that still looks runnable, which does
/// nothing. The would-be sleeper then blocks with its answer already delivered
/// and never wakes up again.
///
/// Marking is not stopping. This task keeps running until it calls `park`.
pub fn mark_current_blocked() {
    let mut scheduler = SCHEDULER.lock();
    let current = scheduler.current;
    if let Some(task) = scheduler.tasks[current].as_mut() {
        task.state = State::Blocked;
    }
}

/// Actually stop, if there is still anything to wait for.
///
/// The second half. Conditional on purpose: between marking and parking, the
/// thing being waited for may already have happened and put this task back to
/// `Runnable`. Parking anyway would be a sleep with nobody left holding a
/// reason to wake it, which is the lost wakeup this pair exists to prevent.
///
/// Interrupts must already be masked, and the caller must already have
/// arranged for something to wake it.
pub fn park() {
    let blocked = {
        let scheduler = SCHEDULER.lock();
        scheduler.tasks[scheduler.current]
            .as_ref()
            .is_some_and(|task| task.state == State::Blocked)
    };

    if blocked {
        switch_to_next(Reason::Voluntary);
    }
}

/// Put a blocked task back in the running.
///
/// If the woken task outranks the caller, the caller loses the CPU on this
/// line rather than at the next timer tick. That immediacy is the property the
/// whole priority scheme exists for: "a high priority task runs as soon as it
/// is runnable" is not true if it has to wait out the rest of somebody else's
/// time slice. It does mean `unblock` is a scheduling point, so callers must
/// not hold anything across it that the woken task might want.
///
/// Waking something that is not blocked does nothing, which keeps the caller
/// from having to know whether it won a race to wake it.
pub fn unblock(id: TaskId) {
    let state = sync::disable_interrupts();

    if unblock_deferred(id) {
        switch_to_next(Reason::Voluntary);
    }

    sync::restore_interrupts(state);
}

/// Put a blocked task back in the running without giving up the CPU.
///
/// Returns whether the woken task outranks the caller, so a caller that wants
/// the handoff can make it and one that does not can carry on.
///
/// Not an optimisation. `unblock` may never return, because the switch it makes
/// can be to a task that runs for a long time, and a caller with more than one
/// thing left to do would silently leave the rest undone. `fault_current` is
/// the extreme case: it is already marked as faulted when it wakes anybody, so
/// every switch it makes is permanent and everything it still has to do would
/// simply never happen.
pub fn unblock_deferred(id: TaskId) -> bool {
    let mut scheduler = SCHEDULER.lock();
    let current = scheduler.current;
    let running = scheduler.tasks[current].as_ref().map(|task| task.priority);

    match scheduler.tasks.get_mut(id.0).and_then(Option::as_mut) {
        Some(task) if task.state == State::Blocked => {
            task.state = State::Runnable;
            let woken = task.priority;
            running.is_some_and(|running| woken.outranks(running))
        }
        _ => false,
    }
}

/// Can this task still be sent to?
///
/// False for every terminal state, so a task that has exited, been reaped, or
/// faulted is not something anybody can rendezvous with. Callers use this to
/// turn "wait for a reply that will never come" into an error return.
pub fn is_alive(id: TaskId) -> bool {
    matches!(
        SCHEDULER.lock().tasks.get(id.0).and_then(Option::as_ref),
        Some(task) if matches!(task.state, State::Runnable | State::Blocked)
    )
}

/// This task's scheduling priority, if the slot is occupied.
pub fn priority_of(id: TaskId) -> Option<Priority> {
    Some(SCHEDULER.lock().tasks.get(id.0)?.as_ref()?.priority)
}

/// Root of any task's address space, not just the running one.
///
/// The kernel needs this to touch a blocked task's memory on its behalf, which
/// is the whole basis of message passing: the sender is not running when its
/// buffer is read.
pub fn space_root_of(id: TaskId) -> Option<Frame> {
    SCHEDULER
        .lock()
        .tasks
        .get(id.0)?
        .as_ref()?
        .space
        .as_ref()
        .map(AddressSpace::root)
}

/// Is this task waiting on something?
pub fn is_blocked(id: TaskId) -> bool {
    matches!(
        SCHEDULER.lock().tasks[id.0].as_ref(),
        Some(task) if task.state == State::Blocked
    )
}

/// Pick the next runnable task and switch to it, if there is one.
///
/// Must be called with interrupts already masked. Shared by the voluntary and
/// the preemptive paths, which differ only in how they got here.
fn switch_to_next(reason: Reason) {
    // Before choosing, hand back anything the dead are still holding. This is
    // the natural place: we are running on some other task's stack, so every
    // zombie's stack is memory nobody is standing on.
    reap_zombies();

    let (save_to, resume, root) = {
        let mut scheduler = SCHEDULER.lock();

        // Nothing to switch between before `init` has run.
        if scheduler.tasks[scheduler.current].is_none() {
            return;
        }

        // Nothing runnable anywhere, including us. Only reachable if the idle
        // task itself blocked, since it is otherwise always runnable. There is
        // no correct task to pick and resuming a blocked one is worse than
        // stopping, so say what happened rather than quietly running something
        // that asked not to be run.
        let next = scheduler
            .next_runnable()
            .expect("every task is blocked; nothing can run");
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
        let incoming = scheduler.tasks[next].as_ref().unwrap();
        let resume = incoming.sp;
        let root = incoming.space.as_ref().map(AddressSpace::root);

        (save_to, resume, root)
    };

    // Swap the low half before swapping stacks. Safe in either order, because
    // kernel stacks live in the high half and TTBR1 is untouched by this;
    // doing it first just keeps the window where TTBR0 and the stack disagree
    // out of the switch itself.
    unsafe { paging::activate_root(root.unwrap_or_else(empty_space_root)) };

    unsafe { switch(save_to, resume) };
}

/// Root of the address space used by tasks that have none of their own.
///
/// Empty, so every low address faults. A task without an address space should
/// find nothing down there rather than inheriting whatever the previous task
/// had mapped, which is the difference between "no address space" and "someone
/// else's address space".
fn empty_space_root() -> Frame {
    EMPTY_SPACE
        .lock()
        .as_ref()
        .expect("address spaces not initialised")
        .root()
}

static EMPTY_SPACE: Lock<Option<AddressSpace>> = Lock::new(None);

/// Give the scheduler an empty low half to fall back on, and let the walker
/// consult TTBR0 again.
pub fn init_address_spaces() {
    *EMPTY_SPACE.lock() = Some(AddressSpace::new());
    unsafe { paging::activate_root(empty_space_root()) };
    unsafe { paging::enable_user_translation() };
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
    let id = {
        let mut scheduler = SCHEDULER.lock();
        let current = scheduler.current;
        if let Some(task) = scheduler.tasks[current].as_mut() {
            task.state = State::Zombie;
        }
        current
    };

    crate::ipc::abandon(TaskId(id));

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
        .filter_map(Option::as_ref)
        // Only stacks this task still owns. Task 0's came from the linker and
        // never had a canary, and a reaped task's stack has been handed back
        // to the allocator, so whatever is at its base now belongs to whoever
        // got it next.
        .filter(|task| task.owns_stack && task.state != State::Dead)
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
            "  {}{} {:<8} prio {:<3} stack {:#012x}  {:?}",
            slot,
            if slot == scheduler.current { "*" } else { " " },
            task.name,
            task.priority.0,
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
    release_dead();

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
    release_dead();

    print!("preemption self test: passed, ");
    println!("{total} switches, {preemptions} of them preemptive, {distinct} tasks scheduled");
}

/// Virtual address every isolation worker maps, each to a different frame.
///
/// The same address deliberately. Two tasks reading different values from one
/// address is the whole property; if they had different addresses the test
/// would prove nothing that a single address space could not also do.
const ISOLATION_VA: u64 = 0x1000_0000;

const ISOLATION_WORKERS: u64 = 3;

static ISOLATION_SEEN: Lock<[u64; MAX_TASKS]> = Lock::new([0; MAX_TASKS]);
static ISOLATION_DONE: Lock<u64> = Lock::new(0);

extern "C" fn isolation_worker(tag: u64) {
    // Write our own tag into what should be our own private page.
    unsafe { core::ptr::write_volatile(ISOLATION_VA as *mut u64, tag) };

    // Let every other worker write theirs. If TTBR0 is not being switched,
    // this is the window in which they overwrite each other.
    for _ in 0..ISOLATION_WORKERS * 2 {
        yield_now();
    }

    let seen = unsafe { core::ptr::read_volatile(ISOLATION_VA as *const u64) };

    ISOLATION_SEEN.lock()[tag as usize] = seen;
    *ISOLATION_DONE.lock() += 1;
}

/// Prove that address spaces are private.
pub fn isolation_self_test() {
    *ISOLATION_DONE.lock() = 0;

    for tag in 1..=ISOLATION_WORKERS {
        let space = AddressSpace::new();
        let frame = frames::alloc().expect("no frame for a user page");
        space.map(
            ISOLATION_VA,
            frame.addr(),
            FRAME_SIZE,
            paging::Attributes::user_data(),
        );
        spawn_in("iso", isolation_worker, tag, Some(space), Priority::NORMAL);
    }

    while *ISOLATION_DONE.lock() < ISOLATION_WORKERS {
        yield_now();
    }

    let seen = ISOLATION_SEEN.lock();
    for tag in 1..=ISOLATION_WORKERS {
        let got = seen[tag as usize];
        assert_eq!(
            got, tag,
            "task {tag} read {got} back from {ISOLATION_VA:#x}; \
             the address space was not private"
        );
    }
    drop(seen);

    assert!(canaries_intact(), "a kernel stack overflowed");
    release_dead();

    print!("isolation self test: passed, ");
    println!("{ISOLATION_WORKERS} tasks each read their own value back from {ISOLATION_VA:#x}");
}

// --- IPC ---

/// Spawn the message server at EL0.
pub fn spawn_ipc_server(name: &'static str, priority: Priority) -> TaskId {
    spawn_user_program(
        name,
        (&raw const user_server_start) as u64,
        (&raw const user_server_end) as u64,
        0,
        priority,
    )
}

/// Spawn the message client, told which task to talk to.
pub fn spawn_ipc_client(name: &'static str, server: TaskId, priority: Priority) -> TaskId {
    spawn_user_program(
        name,
        (&raw const user_client_start) as u64,
        (&raw const user_client_end) as u64,
        server.0 as u64,
        priority,
    )
}

/// What the client exits with when a check fails, for a message that names the
/// check rather than the number.
fn client_failure(code: u64) -> &'static str {
    match code {
        1 => "the server reported the message arrived wrong",
        2 => "the reply was the wrong length",
        3 => "the reply bytes did not survive the copy",
        4 => "sending to a dead task did not return EDEAD",
        _ => "unknown failure",
    }
}

/// Run one client and server exchange and check both ends agree it worked.
///
/// `server_priority` decides which of the two rendezvous paths gets used. A
/// server that outranks its client reaches `recv` first and is already waiting
/// when the message arrives; a client that outranks its server sends into an
/// empty room and waits to be collected. Both have to work, and they are
/// different code, so the test runs it twice rather than picking one.
fn ipc_round(server_priority: Priority, client_priority: Priority) {
    let (server, client) = sync::without_interrupts(|| {
        let server = spawn_ipc_server("server", server_priority);
        let client = spawn_ipc_client("client", server, client_priority);
        (server, client)
    });

    // Bounded. A rendezvous that goes wrong goes wrong by waiting forever, and
    // an unbounded wait here would surface as the boot test timing out with
    // nothing to say. One second is several orders of magnitude more than the
    // exchange needs and still fails long before the test harness gives up.
    let deadline = crate::timer::ticks() + 100;
    while !finished(client) || !finished(server) {
        assert!(
            crate::timer::ticks() < deadline,
            "the exchange never finished; somebody is waiting for something that is not coming"
        );
        yield_now();
    }

    let server_code = exit_code(server).expect("server finished without an exit code");
    let client_code = exit_code(client).expect("client finished without an exit code");

    // Collected first, then cleaned up. The wait loop above can exit with the
    // last task still a zombie: it becomes one and yields, and the next switch
    // is the one that reaps it, which is a switch this loop no longer makes.
    reap_zombies();
    release_dead();

    assert_eq!(
        server_code, 0,
        "the server did not receive what the client sent"
    );
    assert_eq!(
        client_code,
        0,
        "client check failed: {}",
        client_failure(client_code)
    );
}

/// Two tasks that cannot see each other's memory exchange a message.
pub fn ipc_self_test() {
    reap_zombies();
    release_dead();
    let before_frames = frames::free_frames();

    // Receiver already waiting when the message arrives.
    ipc_round(Priority::HIGH, Priority::NORMAL);
    // Message queued until somebody comes to collect it.
    ipc_round(Priority::NORMAL, Priority::HIGH);

    assert!(canaries_intact(), "a kernel stack overflowed");
    assert_eq!(
        frames::free_frames(),
        before_frames,
        "an exchange leaked memory"
    );

    print!("ipc self test: passed, ");
    println!(
        "message and reply survived both directions across two address spaces, \
         both rendezvous orders, dead target refused"
    );
}

/// What the borrower reports, one bit per check that was allowed and should
/// not have been.
fn lease_failures(code: u64) -> &'static str {
    match code {
        1 => "reading a lease it was given failed",
        2 => "reading a lease produced the wrong bytes",
        4 => "writing a lease it was given failed",
        8 => "a borrow past the end of a lease was allowed",
        16 => "writing a read only lease was allowed",
        32 => "a lease index nobody lent it was accepted",
        64 => "a borrow after the reply still worked",
        _ => "several checks failed at once",
    }
}

/// Lend memory across an address space boundary and check every way of
/// abusing it is refused.
pub fn lease_self_test() {
    reap_zombies();
    release_dead();
    let before_frames = frames::free_frames();

    let (borrower, lender) = sync::without_interrupts(|| {
        let borrower = spawn_user_program(
            "borrower",
            (&raw const user_borrower_start) as u64,
            (&raw const user_borrower_end) as u64,
            0,
            Priority::HIGH,
        );
        let lender = spawn_user_program(
            "lender",
            (&raw const user_lender_start) as u64,
            (&raw const user_lender_end) as u64,
            borrower.0 as u64,
            Priority::NORMAL,
        );
        (borrower, lender)
    });

    let deadline = crate::timer::ticks() + 100;
    while !finished(lender) || !finished(borrower) {
        assert!(
            crate::timer::ticks() < deadline,
            "the lease exchange never finished"
        );
        yield_now();
    }

    let borrower_code = exit_code(borrower).expect("borrower finished without an exit code");
    let lender_code = exit_code(lender).expect("lender finished without an exit code");

    reap_zombies();
    release_dead();

    assert_eq!(
        borrower_code,
        0,
        "borrow check failed: {}",
        lease_failures(borrower_code)
    );

    // The lender only gets here by finding, in its own memory, bytes that a
    // task in another address space put there. That is the property.
    assert_eq!(
        lender_code,
        0,
        "{}",
        match lender_code {
            1 => "the borrower reported a refused borrow it should have been allowed",
            3 => "a lease over memory the sender does not own was accepted",
            _ => "the lent buffer did not come back with the borrower's bytes in it",
        }
    );

    assert!(canaries_intact(), "a kernel stack overflowed");
    assert_eq!(
        frames::free_frames(),
        before_frames,
        "a lease exchange leaked memory"
    );

    print!("lease self test: passed, ");
    println!(
        "buffer lent and written across address spaces, overrun and wrong direction \
         and unknown index and stale lease all refused"
    );
}

fn supervisor_failures(code: u64) -> &'static str {
    match code {
        1 => "the fault reported was not the task being watched",
        2 => "the first fault was not at the address the victim aimed at",
        4 => "restart was refused",
        8 => "the restarted task came back with a different identity",
        16 => "the restarted task found its old memory still there",
        _ => "several checks failed at once",
    }
}

/// A task dies, a task notices, and the task comes back.
pub fn supervisor_self_test() {
    reap_zombies();
    release_dead();
    let before_frames = frames::free_frames();

    let (victim, supervisor, petitioner) = sync::without_interrupts(|| {
        let victim = spawn_user_program(
            "victim",
            (&raw const user_victim_start) as u64,
            (&raw const user_victim_end) as u64,
            0,
            Priority::NORMAL,
        );
        // Above the victim, so it is already blocked mid conversation when the
        // victim dies. A petitioner that had not sent yet would be told the
        // target is dead by the ordinary liveness check and would prove
        // nothing about releasing somebody who is already waiting.
        let petitioner = spawn_user_program(
            "petitioner",
            (&raw const user_petitioner_start) as u64,
            (&raw const user_petitioner_end) as u64,
            victim.0 as u64,
            Priority::HIGH,
        );
        // Also above the victim, so it is already parked in `fault_wait` when
        // the fault happens. It would work either way, since a faulted task
        // stays faulted and asking late finds it just the same, but only this
        // order tests the wake: a supervisor that asks afterwards never needed
        // waking and would pass with the notification broken.
        let supervisor = spawn_user_program(
            "supervisor",
            (&raw const user_supervisor_start) as u64,
            (&raw const user_supervisor_end) as u64,
            victim.0 as u64,
            Priority::HIGH,
        );
        set_supervisor(supervisor);
        (victim, supervisor, petitioner)
    });

    let deadline = crate::timer::ticks() + 200;
    while !finished(supervisor) || !finished(petitioner) {
        assert!(
            crate::timer::ticks() < deadline,
            "supervision never finished; the supervisor is waiting for a fault that never came, \
             or somebody is stuck on a task that died"
        );
        yield_now();
    }

    let supervisor_code = exit_code(supervisor).expect("supervisor finished without an exit code");
    let petitioner_code = exit_code(petitioner).expect("petitioner finished without an exit code");

    assert_eq!(
        supervisor_code,
        0,
        "supervision failed: {}",
        supervisor_failures(supervisor_code)
    );
    assert_eq!(
        petitioner_code,
        0,
        "{}",
        match petitioner_code {
            1 => "a task blocked sending to the victim was not released when it died",
            _ => "a task that is not the supervisor was allowed to restart another task",
        }
    );

    // The victim is still sitting there faulted for the second time, which is
    // the supervisor deciding it has seen enough. Giving up on it is the other
    // half of the policy, and it is the kernel's job only because there is
    // nobody left to ask.
    assert!(
        is_faulted(victim),
        "the victim should still be stopped after its second fault"
    );
    kill(victim);
    while !is_dead(victim) {
        yield_now();
    }
    release_dead();

    assert!(canaries_intact(), "a kernel stack overflowed");
    assert_eq!(
        frames::free_frames(),
        before_frames,
        "a restart leaked memory; the address space it replaced was not given back"
    );

    print!("supervisor self test: passed, ");
    println!(
        "task faulted, supervisor restarted it with clean memory and the same id, \
         blocked sender released, restart refused to everybody else"
    );
}

/// Start the heartbeat driver and give it the timer interrupt.
///
/// Never finishes, unlike everything else spawned here. It is a permanent
/// system task, so it goes up last, once the tests that count free frames have
/// had their turn.
pub fn spawn_heartbeat() -> TaskId {
    let id = spawn_user_program(
        "heartbeat",
        (&raw const user_heartbeat_start) as u64,
        (&raw const user_heartbeat_end) as u64,
        0,
        Priority::NORMAL,
    );
    crate::notify::route(crate::timer::TIMER_INTID, id, 1);
    id
}

/// Post notifications on a schedule the kernel controls, and check the rules.
///
/// The heartbeat proves the path works end to end and nothing else: the timer
/// only ever sets one bit, and always the one being waited for. Everything
/// awkward about notifications needs a poster that can be told when to post.
pub fn notification_self_test() {
    reap_zombies();
    release_dead();

    let watcher = spawn_user_program(
        "notified",
        (&raw const user_notified_start) as u64,
        (&raw const user_notified_end) as u64,
        0,
        Priority::HIGH,
    );

    // Let it get as far as its first receive and park there.
    let deadline = crate::timer::ticks() + 100;
    while !is_blocked(watcher) {
        assert!(
            crate::timer::ticks() < deadline,
            "the watcher never parked waiting for a notification"
        );
        yield_now();
    }

    // A bit it is not waiting for. Posting this must leave it exactly where it
    // is: a task waiting on one event is not woken by a different one, or
    // every driver becomes a poll loop with extra steps.
    crate::notify::post(watcher, 0b0010);
    assert!(
        is_blocked(watcher),
        "a notification nobody was waiting for woke a task anyway"
    );

    // The bit it is waiting for.
    crate::notify::post(watcher, 0b0001);

    let deadline = crate::timer::ticks() + 100;
    while !finished(watcher) {
        assert!(
            crate::timer::ticks() < deadline,
            "the watcher was never woken, or is still waiting for the bit it was already sent"
        );
        yield_now();
    }

    let code = exit_code(watcher).expect("watcher finished without an exit code");
    reap_zombies();
    release_dead();

    assert_eq!(
        code,
        0,
        "{}",
        match code {
            1 => "a notification arrived claiming to be from a task",
            2 => "receiving one notification collected bits nobody had asked for",
            _ => "a bit posted while the task was not looking was lost rather than kept",
        }
    );

    assert!(canaries_intact(), "a kernel stack overflowed");

    print!("notification self test: passed, ");
    println!(
        "unwanted bit did not wake the task and was kept, wanted bit did, \
         and only the requested bits were collected"
    );
}

/// Only the task a message was sent to may answer it.
///
/// The exchange in `ipc_self_test` cannot check this, because two tasks leave
/// nobody spare to forge anything. What it needs is a moment when a reply is
/// outstanding and the task entitled to give it is not running, and that moment
/// does not exist unless the server is made to wait for something.
pub fn forged_reply_self_test() {
    reap_zombies();
    release_dead();
    let before_frames = frames::free_frames();

    let (server, client) = sync::without_interrupts(|| {
        let server = spawn_user_program(
            "slowsrv",
            (&raw const user_slow_server_start) as u64,
            (&raw const user_slow_server_end) as u64,
            0,
            Priority::HIGH,
        );
        let client = spawn_ipc_client("client", server, Priority::NORMAL);
        (server, client)
    });

    // Wait for the window to open: the client owed a reply, the server parked
    // on a notification. Task 0 runs here precisely because nothing else can.
    let deadline = crate::timer::ticks() + 100;
    while !(crate::ipc::is_awaiting_reply(client) && is_blocked(server)) {
        assert!(
            crate::timer::ticks() < deadline,
            "the exchange never reached the point where a reply is outstanding"
        );
        yield_now();
    }

    // The forgery. Task 0 never received this message and must not be able to
    // answer it: doing so would release somebody else's sender and hand it a
    // reply it has no way to tell from a real one.
    //
    // Zero length on purpose. It gets no further than the guard even if the
    // caller had memory to copy from, which task 0 does not, so a pass here is
    // the guard and not an accident of having nothing to send.
    const FORGED: u64 = 0xbad;
    let refused = crate::ipc::reply(client.0 as u64, FORGED, 0, 0);
    assert_eq!(
        refused,
        crate::ipc::EINVAL,
        "a task that never received the message was allowed to answer it"
    );
    assert!(
        crate::ipc::is_awaiting_reply(client),
        "the forged reply was reported as refused but released the sender anyway"
    );

    // Let the real receiver get on with it.
    crate::notify::post(server, 1);

    let deadline = crate::timer::ticks() + 100;
    while !finished(client) || !finished(server) {
        assert!(
            crate::timer::ticks() < deadline,
            "the exchange never finished after the server was released"
        );
        yield_now();
    }

    let client_code = exit_code(client).expect("client finished without an exit code");
    reap_zombies();
    release_dead();

    // The client checks the reply it got byte for byte, so this is also the
    // assertion that the forged answer did not reach it: 0xbad carries no body
    // at all, and the client would have failed on the length.
    assert_eq!(
        client_code,
        0,
        "client check failed: {}",
        client_failure(client_code)
    );

    assert!(canaries_intact(), "a kernel stack overflowed");
    assert_eq!(
        frames::free_frames(),
        before_frames,
        "the exchange leaked memory"
    );

    print!("forged reply self test: passed, ");
    println!(
        "a task that did not receive the message could not answer it, and the real reply still arrived"
    );
}

// --- priorities and blocking ---

/// Who ran, in the order they ran. The assertion for this tier is about
/// sequence, not about tasks merely having had a turn: a round robin scheduler
/// also gives everybody a turn, which is exactly the thing being ruled out.
const ORDER_LEN: usize = 16;

struct Order {
    tags: [u8; ORDER_LEN],
    len: usize,
}

static ORDER: Lock<Order> = Lock::new(Order {
    tags: [0; ORDER_LEN],
    len: 0,
});

fn note_run(tag: u8) {
    let mut order = ORDER.lock();
    if order.len < ORDER_LEN {
        let index = order.len;
        order.tags[index] = tag;
        order.len += 1;
    }
}

fn reset_order() {
    ORDER.lock().len = 0;
}

/// Number of turns each ladder worker takes.
const LADDER_ROUNDS: usize = 3;

/// Records its tag and yields, over and over.
///
/// It yields rather than spins so that the scheduler is asked to choose
/// repeatedly. A yield is the friendliest thing a task can do, and a round
/// robin scheduler would hand the CPU straight down to a low priority task on
/// each one. A priority ordered scheduler gives it back to the same tier until
/// that tier is empty.
extern "C" fn ladder_worker(tag: u64) {
    for _ in 0..LADDER_ROUNDS {
        note_run(tag as u8);
        yield_now();
    }
}

/// Blocks itself immediately, at the highest priority in the test.
///
/// The priority is the trap. If blocked tasks were still candidates, this one
/// would outrank everything and be picked every single time, and the waker
/// below would never record anything at all.
extern "C" fn sleeper(_arg: u64) {
    note_run(1);
    block_current();
    note_run(3);
}

extern "C" fn waker(_arg: u64) {
    note_run(2);

    let id = SLEEPER.lock().expect("sleeper was never registered");
    assert!(
        is_blocked(id),
        "the sleeper is not blocked, so this test is measuring nothing"
    );

    unblock(id);

    // Only reached after the sleeper has recorded a 3. Waking something that
    // outranks us takes the CPU away inside `unblock`, so the ordering of
    // these last two entries is the assertion about immediacy.
    note_run(4);
}

static SLEEPER: Lock<Option<TaskId>> = Lock::new(None);

/// A task that goes to sleep and stays there until somebody else wakes it.
extern "C" fn napper(_arg: u64) {
    block_current();
}

/// Prove the scheduler picks by priority, round robins only within a
/// priority, and never picks a blocked task.
pub fn priority_self_test() {
    // Part one: strict priority order.
    //
    // The low priority pair is spawned first on purpose. Under the round robin
    // scheduler this replaces, they occupy the earlier slots and would be
    // picked first, so a stale scheduler fails on the very first entry rather
    // than somewhere subtle.
    //
    // Created with interrupts masked, because the group has to become
    // schedulable all at once. Spawning is a scheduling event now: the moment
    // "low-a" exists it outranks the idle task, so a tick landing between two
    // of these calls would run a low priority worker before its high priority
    // rival had been created, and the recorded order would be wrong for a
    // reason that has nothing to do with the scheduler.
    reset_order();
    sync::without_interrupts(|| {
        spawn_at("low-a", ladder_worker, 10, Priority::LOW);
        spawn_at("low-b", ladder_worker, 11, Priority::LOW);
        spawn_at("high-a", ladder_worker, 20, Priority::HIGH);
        spawn_at("high-b", ladder_worker, 21, Priority::HIGH);
    });

    // Task 0 sits at the idle priority, so this loop does not get the CPU back
    // until all four have finished. The waiting is free.
    while runnable_others() > 0 {
        yield_now();
    }

    let order = ORDER.lock();
    let expected = [20u8, 21, 20, 21, 20, 21, 10, 11, 10, 11, 10, 11];
    assert_eq!(order.len, expected.len(), "wrong number of turns taken");
    assert_eq!(
        order.tags[..expected.len()],
        expected,
        "tasks did not run in priority order with round robin inside each level"
    );
    drop(order);

    // Part two: blocked is not a candidate, and waking is immediate.
    reset_order();
    sync::without_interrupts(|| {
        let sleeping = spawn_at("sleeper", sleeper, 0, Priority::HIGH);
        *SLEEPER.lock() = Some(sleeping);
        spawn_at("waker", waker, 0, Priority::NORMAL);
    });

    while runnable_others() > 0 {
        yield_now();
    }

    let order = ORDER.lock();
    // 1: the sleeper ran and blocked.
    // 2: a lower priority task ran while the highest priority task in the
    //    table was sitting there blocked.
    // 3: the sleeper resumed the instant it was woken, not at the next tick.
    // 4: only then did the waker get the rest of its turn.
    assert_eq!(
        order.tags[..order.len],
        [1u8, 2, 3, 4],
        "a blocked task was scheduled, or waking one did not preempt the waker"
    );
    drop(order);

    // Part three: the machine still runs when every real task is asleep.
    //
    // A scheduler that picks blocked tasks, or that has no answer when nothing
    // is runnable, dies here rather than in six months inside the IPC code.
    let napping = spawn_at("napper", napper, 0, Priority::HIGH);
    while !is_blocked(napping) {
        yield_now();
    }

    let ticks_before = crate::timer::ticks();
    let mut spins = 0u32;
    while crate::timer::ticks() == ticks_before && spins < 10_000_000 {
        spins += 1;
        core::hint::black_box(spins);
    }
    let idle_ticks = crate::timer::ticks() - ticks_before;
    assert!(
        idle_ticks > 0,
        "the timer stopped while the only runnable task was idle"
    );

    unblock(napping);
    while !finished(napping) {
        yield_now();
    }

    assert!(canaries_intact(), "a kernel stack overflowed");
    release_dead();

    print!("priority self test: passed, ");
    println!(
        "high ran to completion before low, blocked task skipped while highest priority, \
         idle survived {idle_ticks} ticks with everything asleep"
    );
}

// --- EL0 ---

/// Where user text is mapped in every user address space.
const USER_TEXT_VA: u64 = 0x0040_0000;
/// Top of the user stack. Grows down into the page below.
const USER_STACK_TOP: u64 = 0x0080_0000;

/// The startup argument sits in the top word of the stack page, and `SP_EL0`
/// starts below it, so a program reads it with `ldr x0, [sp, #8]`.
///
/// On the stack rather than in a register because there is nowhere else to put
/// it: `user_task_entry` is an ordinary task entry and gets one `u64`, which
/// the entry point already uses. Handing a new program its arguments on the
/// stack is also what every real loader does, so this is the conventional
/// shape rather than an expedient one. Sixteen bytes, not eight, to keep
/// `SP_EL0` 16 byte aligned as the ABI requires.
const USER_ARG_OFFSET: u64 = 8;
const USER_SP_START: u64 = USER_STACK_TOP - 16;

unsafe extern "C" {
    static user_program_start: u8;
    static user_program_end: u8;
    static user_fault_start: u8;
    static user_fault_end: u8;
    static user_client_start: u8;
    static user_client_end: u8;
    static user_server_start: u8;
    static user_server_end: u8;
    static user_lender_start: u8;
    static user_lender_end: u8;
    static user_borrower_start: u8;
    static user_borrower_end: u8;
    static user_victim_start: u8;
    static user_victim_end: u8;
    static user_supervisor_start: u8;
    static user_supervisor_end: u8;
    static user_petitioner_start: u8;
    static user_petitioner_end: u8;
    static user_heartbeat_start: u8;
    static user_heartbeat_end: u8;
    static user_notified_start: u8;
    static user_notified_end: u8;
    static user_slow_server_start: u8;
    static user_slow_server_end: u8;
}

/// Drop to EL0 and start running `entry`.
///
/// # Safety
///
/// `entry` and `stack` must be mapped executable and writable respectively in
/// the address space currently in `TTBR0_EL1`.
unsafe fn enter_el0(entry: u64, stack: u64) -> ! {
    unsafe {
        core::arch::asm!(
            "msr sp_el0, {stack}",
            "msr elr_el1, {entry}",
            "msr spsr_el1, {spsr}",
            "eret",
            stack = in(reg) stack,
            entry = in(reg) entry,
            // All zero: M[3:0] = 0b0000 selects EL0t, and clear DAIF leaves
            // interrupts unmasked. A user task with interrupts masked could
            // never be preempted, which is the same bug the trampoline had.
            spsr = in(reg) 0u64,
            options(noreturn),
        );
    }
}

/// Kernel side of a user task: set up EL0 and never come back.
///
/// A user task is still a kernel task underneath. It keeps its kernel stack,
/// which is what `SP_EL1` points at when a syscall or an interrupt brings it
/// back up to EL1.
extern "C" fn user_task_entry(entry: u64) {
    unsafe { enter_el0(entry, USER_SP_START) }
}

/// Create a task that runs the well behaved user program at EL0.
pub fn spawn_user(name: &'static str) -> TaskId {
    spawn_user_program(
        name,
        (&raw const user_program_start) as u64,
        (&raw const user_program_end) as u64,
        0,
        Priority::NORMAL,
    )
}

/// Create a task running the program that deliberately faults.
pub fn spawn_faulting_user(name: &'static str) -> TaskId {
    spawn_user_program(
        name,
        (&raw const user_fault_start) as u64,
        (&raw const user_fault_end) as u64,
        0,
        Priority::NORMAL,
    )
}

/// Root of the current task's address space, if it has one.
pub fn current_space_root() -> Option<Frame> {
    let scheduler = SCHEDULER.lock();
    scheduler.tasks[scheduler.current]
        .as_ref()?
        .space
        .as_ref()
        .map(AddressSpace::root)
}

/// End the current task.
///
/// Never returns. The kernel stack under our feet belongs to this task, so it
/// cannot be reclaimed from here; that is #20's problem.
pub fn exit_current(code: u64) -> ! {
    let id = {
        let mut scheduler = SCHEDULER.lock();
        let current = scheduler.current;
        if let Some(task) = scheduler.tasks[current].as_mut() {
            task.state = State::Zombie;
            task.exit_code = Some(code);
        }
        current
    };

    // Anybody waiting on a message from us is waiting on something that will
    // never arrive. Checking liveness when the message was sent is not enough
    // on its own, because a target is free to exit afterwards.
    crate::ipc::abandon(TaskId(id));

    loop {
        yield_now();
    }
}

/// What a finished task exited with.
pub fn exit_code(id: TaskId) -> Option<u64> {
    SCHEDULER.lock().tasks[id.0].as_ref()?.exit_code
}

/// Has this task stopped for good, whether or not it has been cleaned up yet?
///
/// Spelled out rather than written as "not runnable". Blocked is also not
/// runnable, and a blocked task is the opposite of finished: it is waiting to
/// carry on. Reading this as `!= Runnable` would report a task as done the
/// moment it went to sleep.
fn finished(id: TaskId) -> bool {
    matches!(
        SCHEDULER.lock().tasks[id.0].as_ref(),
        Some(task) if matches!(task.state, State::Zombie | State::Dead | State::Faulted)
    )
}

/// Run the embedded program at EL0 and check the privilege boundary held.
pub fn user_self_test() {
    let traps_before = crate::exceptions::privileged_traps();

    let id = spawn_user("user");
    while !finished(id) {
        yield_now();
    }

    let code = exit_code(id).expect("user task finished without an exit code");
    let traps = crate::exceptions::privileged_traps() - traps_before;

    // -2 is EFAULT. The task asked the kernel to print from an address in the
    // kernel's own half. The kernel can read there; the task cannot. Serving
    // that request would be the kernel lending out its privilege.
    const EFAULT: u64 = -2i64 as u64;
    assert_eq!(
        code, EFAULT,
        "syscall accepted a kernel pointer from EL0 and returned {code}"
    );

    assert_eq!(
        traps, 1,
        "expected exactly one privileged instruction to be refused, saw {traps}"
    );

    // Exit code collected, so the slot can go.
    release_dead();

    print!("user self test: passed, ");
    println!("EL0 ran, {traps} privileged instruction refused, kernel pointer rejected");
}

/// Return the resources of every finished task except the running one.
///
/// The exclusion is not a detail. A task that has exited is still executing on
/// its kernel stack until it switches away, and freeing that stack out from
/// under it hands live memory to the allocator, which will cheerfully give it
/// to somebody else.
///
/// The table slot and the exit code stay behind, so a task that exited can
/// still be asked what it exited with.
pub fn reap_zombies() {
    loop {
        // Take one victim's resources out under the lock, then release the
        // lock before touching the allocator. One at a time keeps the borrow
        // simple and the lock held briefly.
        let salvage = {
            let mut scheduler = SCHEDULER.lock();
            let current = scheduler.current;

            let victim = scheduler
                .tasks
                .iter()
                .position(|task| matches!(task, Some(task) if task.state == State::Zombie));

            match victim {
                Some(slot) if slot != current => {
                    let task = scheduler.tasks[slot].as_mut().unwrap();
                    task.state = State::Dead;
                    let stack = task.owns_stack.then_some(task.stack);
                    let space = task.space.take();
                    Some((stack, space))
                }
                _ => None,
            }
        };

        let Some((stack, space)) = salvage else {
            return;
        };

        if let Some(stack) = stack {
            frames::free_contiguous(stack, STACK_FRAMES);
        }
        if let Some(space) = space {
            space.destroy();
        }
    }
}

/// Free the table slots of tasks that have been reaped.
///
/// Separate from reaping because the exit code lives in the slot. Collecting
/// it is the caller's business, and doing this automatically would mean a task
/// could exit and vanish before anybody asked how it went.
pub fn release_dead() -> usize {
    let mut scheduler = SCHEDULER.lock();
    let current = scheduler.current;
    let mut released = 0;

    for slot in 0..MAX_TASKS {
        if slot == current {
            continue;
        }
        if matches!(&scheduler.tasks[slot], Some(task) if task.state == State::Dead) {
            scheduler.tasks[slot] = None;
            released += 1;
        }
    }

    released
}

/// Run a task through its whole life and check the machine is the same size
/// afterwards.
///
/// The frame count is the entire point. It turns a leak from something noticed
/// months later into something a boot fails on.
pub fn lifecycle_self_test() {
    // Settle first. Earlier tests leave finished tasks behind, and reaping
    // those part way through would make the machine *gain* free frames
    // relative to the baseline, which reads as a negative leak and is just as
    // wrong as a positive one.
    reap_zombies();
    release_dead();

    let before_frames = frames::free_frames();
    let before_slots = free_slots();

    let id = spawn_user("shortlived");

    let during = frames::free_frames();
    assert!(
        during < before_frames,
        "spawning a user task consumed no frames, so this test proves nothing"
    );
    let consumed = before_frames - during;

    while !is_dead(id) {
        yield_now();
    }

    // Collectable after reaping, which is the whole reason reaping and
    // releasing are separate steps.
    let code = exit_code(id).expect("reaped task lost its exit code");
    const EFAULT: u64 = -2i64 as u64;
    assert_eq!(
        code, EFAULT,
        "unexpected exit code {code} from the user task"
    );

    let released = release_dead();
    assert!(released > 0, "no slot was released");

    let after_frames = frames::free_frames();
    let after_slots = free_slots();

    // Both directions are wrong. Ending with fewer free frames is a leak;
    // ending with more means something freed memory it did not own, which is
    // the more dangerous of the two and would read as "leaked 0" under a
    // saturating subtraction.
    assert_eq!(
        after_frames, before_frames,
        "task took {consumed} frames; free count went {before_frames} -> {after_frames}"
    );
    assert_eq!(after_slots, before_slots, "task leaked a table slot");

    print!("lifecycle self test: passed, ");
    println!("task took {consumed} frames and gave back all {consumed}");
}

fn is_dead(id: TaskId) -> bool {
    matches!(
        SCHEDULER.lock().tasks[id.0].as_ref(),
        Some(task) if task.state == State::Dead
    )
}

fn free_slots() -> usize {
    SCHEDULER
        .lock()
        .tasks
        .iter()
        .filter(|t| t.is_none())
        .count()
}

/// Stop the current task because it faulted. Never returns.
///
/// Called from the exception handler while still on the task's kernel stack,
/// so this behaves like `exit_current`: mark, switch away, and never come
/// back. The task is never scheduled again, which is what makes returning to
/// the faulting instruction impossible and therefore what stops the fault
/// repeating forever.
pub fn fault_current(fault: Fault) -> ! {
    let (id, name) = {
        let mut scheduler = SCHEDULER.lock();
        let current = scheduler.current;
        let task = scheduler.tasks[current]
            .as_mut()
            .expect("a fault arrived with no current task");
        task.state = State::Faulted;
        task.fault = Some(fault);
        (current, task.name)
    };

    let syndrome = fault.syndrome();
    println!();
    println!("--- task fault ---");
    println!("  task   : {id} ({name})");
    println!("  class  : {}", syndrome.class_name());
    if syndrome.is_abort() {
        print!("  fault  : {}", syndrome.fault_status());
        if let Some(level) = syndrome.fault_level() {
            print!(" at level {level}");
        }
        println!(
            ", on a {}",
            if syndrome.is_write() { "write" } else { "read" }
        );
        println!("  address: {:#018x}", fault.far);
    }
    println!("  pc     : {:#018x}", fault.elr);
    println!("  the kernel is fine. this task is not.");
    println!("------------------");

    // Both of these wake without giving up the CPU, and that is the whole
    // reason they can both be here. This task is already marked as faulted, so
    // the first switch it makes is the last one it ever makes: an immediate
    // handoff to the first task woken would leave everything after it undone,
    // which in this order means either a stranded sender or a supervisor that
    // never hears what happened, depending on which line went first.
    crate::ipc::abandon(TaskId(id));
    wake_fault_waiters();

    loop {
        yield_now();
    }
}

/// What killed a task, if something did.
pub fn fault_of(id: TaskId) -> Option<Fault> {
    SCHEDULER.lock().tasks[id.0].as_ref()?.fault
}

fn is_faulted(id: TaskId) -> bool {
    matches!(
        SCHEDULER.lock().tasks[id.0].as_ref(),
        Some(task) if task.state == State::Faulted
    )
}

// --- supervision ---

/// The one task allowed to hear about faults and act on them.
///
/// Designated by the kernel rather than claimed by a task, because "who is in
/// charge" is not a question a task should be able to answer about itself.
static SUPERVISOR: Lock<Option<TaskId>> = Lock::new(None);

/// Which tasks are parked in `wait_for_fault`.
///
/// A separate flag rather than another `State` variant, for the same reason
/// IPC keeps its own table: the scheduler only needs to know a task is blocked.
/// Why it is blocked belongs to whoever will wake it.
static FAULT_WAITERS: Lock<[bool; MAX_TASKS]> = Lock::new([false; MAX_TASKS]);

pub fn set_supervisor(id: TaskId) {
    *SUPERVISOR.lock() = Some(id);
}

pub fn is_supervisor(id: TaskId) -> bool {
    *SUPERVISOR.lock() == Some(id)
}

/// The lowest numbered task currently stopped by a fault.
fn first_faulted() -> Option<TaskId> {
    let scheduler = SCHEDULER.lock();
    scheduler
        .tasks
        .iter()
        .position(|task| matches!(task, Some(task) if task.state == State::Faulted))
        .map(TaskId)
}

/// Block until some task is faulted, then name it.
///
/// There is no queue behind this and there does not need to be one. A faulted
/// task stays faulted until somebody deals with it, so the state of the task
/// table *is* the backlog: the scan finds whatever is still outstanding, and
/// nothing can be missed by not being collected in time. Two faults while the
/// supervisor is busy are two tasks sitting in the table, not one event that
/// overwrote another.
///
/// The caller is expected to do something about what it is told. A supervisor
/// that asks and then ignores the answer gets the same answer forever.
pub fn wait_for_fault() -> u64 {
    let state = sync::disable_interrupts();

    let id = loop {
        if let Some(id) = first_faulted() {
            break id;
        }

        let me = SCHEDULER.lock().current;
        FAULT_WAITERS.lock()[me] = true;
        mark_current_blocked();
        park();
        FAULT_WAITERS.lock()[me] = false;
    };

    sync::restore_interrupts(state);
    id.0 as u64
}

/// Wake everybody waiting to hear about a fault, without giving up the CPU.
fn wake_fault_waiters() {
    for slot in 0..MAX_TASKS {
        if FAULT_WAITERS.lock()[slot] {
            unblock_deferred(TaskId(slot));
        }
    }
}

/// Put a faulted task back at its entry point with clean memory.
///
/// Keeps the slot, and therefore the identity: anything holding this task's id
/// still refers to this task. That is the whole difference between a restart
/// and a replacement, and it is why the id has to survive even though nothing
/// else does.
///
/// The kernel stack is reused rather than returned and re-fetched. Nobody is
/// standing on it, because a faulted task is never scheduled, so winding it
/// back to a never-run switch frame is enough. The address space is not reused:
/// it is destroyed and rebuilt, so the restarted task cannot find anything the
/// old instance left behind.
pub fn restart(id: TaskId) -> bool {
    // Anybody waiting on the instance that is going away has to be told, not
    // left holding a conversation with a task that is about to forget it ever
    // happened. Already done when the fault was taken, and done again here so
    // that restarting is correct on its own terms rather than only as a
    // follow-up to something else that happened to clean up first.
    crate::ipc::abandon(id);

    let (image, stack, old_space) = {
        let mut scheduler = SCHEDULER.lock();
        let Some(task) = scheduler.tasks.get_mut(id.0).and_then(Option::as_mut) else {
            return false;
        };
        if task.state != State::Faulted {
            return false;
        }
        let Some(image) = task.image else {
            return false;
        };
        (image, task.stack, task.space.take())
    };

    // Outside the lock: both of these touch the frame allocator, and the
    // scheduler lock has no business being held while that happens.
    if let Some(old_space) = old_space {
        old_space.destroy();
    }
    let space = build_user_space(image.start, image.end, image.arg);
    let sp = plant_switch_frame(stack, user_task_entry, USER_TEXT_VA);

    let mut scheduler = SCHEDULER.lock();
    let Some(task) = scheduler.tasks.get_mut(id.0).and_then(Option::as_mut) else {
        return false;
    };
    task.space = Some(space);
    task.sp = sp;
    task.fault = None;
    task.exit_code = None;
    task.state = State::Runnable;
    true
}

/// Give up on a faulted task and let the reaper have its memory.
///
/// Separate from faulting so that stopping a task and discarding it are
/// different decisions. A supervisor will want to inspect, and eventually
/// restart, rather than always destroy.
pub fn kill(id: TaskId) {
    let mut scheduler = SCHEDULER.lock();
    if let Some(task) = scheduler.tasks[id.0].as_mut()
        && task.state == State::Faulted
    {
        task.state = State::Zombie;
    }
}

/// Build a fresh address space holding a user program and its stack.
///
/// Every frame in it comes from the allocator zeroed, so a task built this way
/// starts with memory that carries nothing from whatever used those frames
/// before. That is what makes a restart a genuine restart rather than the same
/// task waking up in its own wreckage.
fn build_user_space(start: u64, end: u64, arg: u64) -> AddressSpace {
    let space = AddressSpace::new();

    let len = (end - start) as usize;
    assert!(
        len as u64 <= FRAME_SIZE,
        "user program does not fit in a page"
    );

    let text = frames::alloc().expect("no frame for user text");
    unsafe {
        core::ptr::copy_nonoverlapping(
            start as *const u8,
            paging::phys_to_virt(text.addr()) as *mut u8,
            len,
        );
    }
    space.map(
        USER_TEXT_VA,
        text.addr(),
        FRAME_SIZE,
        paging::Attributes::user_text(),
    );

    let stack = frames::alloc().expect("no frame for a user stack");
    space.map(
        USER_STACK_TOP - FRAME_SIZE,
        stack.addr(),
        FRAME_SIZE,
        paging::Attributes::user_data(),
    );

    // Planted through the kernel's own view of the frame, because the task's
    // address space is not the one currently in TTBR0 and will not be until it
    // is scheduled.
    let stack_top_alias = paging::phys_to_virt(stack.addr()) + FRAME_SIZE;
    unsafe {
        core::ptr::write_volatile((stack_top_alias - USER_ARG_OFFSET) as *mut u64, arg);
    }

    space
}

/// Copy a user program blob into a fresh address space and spawn it at EL0.
fn spawn_user_program(
    name: &'static str,
    start: u64,
    end: u64,
    arg: u64,
    priority: Priority,
) -> TaskId {
    let space = build_user_space(start, end, arg);
    let id = spawn_in(name, user_task_entry, USER_TEXT_VA, Some(space), priority);

    // Recorded now rather than reconstructed later. A task that faults has
    // nothing left to ask about how it was made.
    if let Some(task) = SCHEDULER.lock().tasks[id.0].as_mut() {
        task.image = Some(Image { start, end, arg });
    }

    id
}

/// Check that an unprivileged task can kill itself without killing anything
/// else.
pub fn fault_self_test() {
    reap_zombies();
    release_dead();

    let before_frames = frames::free_frames();
    let ticks_before = crate::timer::ticks();

    let id = spawn_faulting_user("faulter");
    while !is_faulted(id) {
        yield_now();
    }

    let fault = fault_of(id).expect("faulted task recorded no fault");
    let syndrome = fault.syndrome();

    assert!(
        syndrome.is_abort(),
        "expected a memory abort, got {}",
        syndrome.class_name()
    );
    assert_eq!(
        fault.far, 0,
        "expected the faulting address to be the null that was dereferenced"
    );

    // Reaching this line at all is most of the point: the kernel is still
    // executing after an unprivileged task did something fatal. The tick count
    // says the rest of the system kept running too, rather than the machine
    // limping on with the timer wedged.
    //
    // Waited for rather than sampled, because the fault and its cleanup take
    // well under one 10 ms tick, so an immediate comparison measures nothing.
    // Bounded so a genuinely dead timer fails here instead of hanging the boot
    // and being diagnosed as a timeout.
    let mut spins = 0u32;
    while crate::timer::ticks() == ticks_before && spins < 10_000_000 {
        spins += 1;
        core::hint::black_box(spins);
    }

    let ticks_after = crate::timer::ticks();
    assert!(
        ticks_after > ticks_before,
        "the timer never ticked again after a task faulted"
    );

    kill(id);
    while !is_dead(id) {
        yield_now();
    }
    release_dead();

    assert_eq!(
        frames::free_frames(),
        before_frames,
        "a faulted task did not give its memory back once killed"
    );

    print!("fault self test: passed, ");
    println!(
        "task died on {}, kernel survived, {} ticks elapsed meanwhile",
        syndrome.fault_status(),
        ticks_after - ticks_before
    );
}
