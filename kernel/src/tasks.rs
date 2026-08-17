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

use crate::frames::{self, FRAME_SIZE, Frame};
use crate::paging;
use crate::sync::Lock;
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
    let (save_to, resume) = {
        let mut scheduler = SCHEDULER.lock();

        let next = scheduler.next_runnable();
        if next == scheduler.current {
            return;
        }

        let current = scheduler.current;
        scheduler.current = next;

        // Raw pointers deliberately outlive the guard. The switch itself must
        // not happen while the lock is held: the lock masks interrupts, and
        // whether they are masked is a property of the CPU rather than of a
        // task, so releasing it on the far side of a switch would leave the
        // wrong task holding the mask.
        //
        // Sound because SCHEDULER is a static that never moves, and because
        // one core with no preemption means nothing can touch the table in
        // between. #16 has to revisit this the moment the timer can switch.
        let save_to = &mut scheduler.tasks[current].as_mut().unwrap().sp as *mut u64;
        let resume = scheduler.tasks[next].as_ref().unwrap().sp;

        (save_to, resume)
    };

    unsafe { switch(save_to, resume) };
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
