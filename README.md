# the_rack

A kernel built from the reset vector up, in Rust, on aarch64.

Long-horizon project. The goal is not a toy OS and then a rewrite. The goal is
one machine stack, owned end to end, that keeps growing: firmware, kernel,
drivers, storage, network, control plane. Bottom-up, one tier at a time.

## Running it

```sh
cargo run
```

Boots the kernel on the QEMU `virt` machine with its UART on your terminal.
`Ctrl-A` then `X` quits.

```
the_rack: booting, mmu off
the_rack: frame allocator up, building page tables
the_rack: tables built, enabling mmu
the_rack: mmu on, jumping to the high half

the_rack 0.1.0
aarch64 / qemu-virt

  exception level : EL1
  kernel loaded   : 0x0000000040080000 physical
  kernel end      : 0xffff0000400a3000 virtual
  vector table    : 0xffff000040080800

mmu: enabled, 4 KiB granule, 48 bit addresses
  sctlr  : 0x0000000000c5183d  mmu=1 dcache=1 icache=1
  tcr    : 0x00000004b5103590  ttbr0 walks disabled
  ttbr0  : 0x0000000000000000
  ttbr1  : 0x00000000400a4000
  kernel : 0xffff000000000000 + physical

memory: 256 MiB at 0x40000000, 65536 frames of 4 KiB
  reserved : 0x0040000000..0x00400c1000   772 KiB  kernel image
  free     : 0x00400c1000..0x0050000000   255 MiB  65333 frames

device tree:
  address       : 0x0000000048000000 physical
  total size    : 1048576 bytes (0x100000)
  version       : 17 (readable back to 16)
  struct block  : 0x40 + 0x1b88
  strings block : 0x1bc8 + 0x1ce
  boot cpu      : 0
  reserved      : 256 frames

trap self test: resumed, registers intact
lock self test: passed, guarded counter reached 1
frame self test: passed, reuse of 0x400cb000 came back clean, 65077 frames free
fdt self test: passed, a good header parses and ten broken ones are refused
device tree self test: passed, this machine is a linux,dummy-virt with 256 MiB at 0x40000000
paging self test: passed, running at 0xffff000040084a94
  write to .text   : permission fault
  write to .rodata : permission fault
  read low half    : translation fault, nothing mapped
task self test: passed, 2 tasks alternated 3 turns each, locals intact, canaries intact

tasks:
  0* kernel   stack 0xffff000040000000  Runnable
  1  ping     stack 0xffff0000400b1000  Finished
  2  pong     stack 0xffff0000400b5000  Finished

gic: 288 interrupt lines
timer: 62500000 Hz counter, tick every 10 ms

tier 0 complete. we are alive on bare metal.
tier 1: exception vectors online.
tier 1: paging online, kernel in the high half.
tier 1: heartbeat started, interrupts live.

preemption self test: passed, 82 switches, 58 of them preemptive, 4 tasks scheduled
isolation self test: passed, 3 tasks each read their own value back from 0x10000000
hello from EL0, running unprivileged
user self test: passed, EL0 ran, 1 privileged instruction refused, kernel pointer rejected
lifecycle self test: passed, task took 11 frames and gave back all 11

--- task fault ---
  task   : 1 (faulter)
  class  : data abort from lower EL
  fault  : translation fault, nothing mapped at level 2, on a write
  address: 0x0000000000000000
  pc     : 0x0000000000400004
  the kernel is fine. this task is not.
------------------
fault self test: passed, task died on translation fault, kernel survived
priority self test: passed, high ran to completion before low, blocked task skipped
ipc: server received ping
ipc self test: passed, message and reply survived both directions across two address spaces
lease self test: passed, buffer lent and written across address spaces
supervisor self test: passed, task faulted, supervisor restarted it with clean memory
notification self test: passed, unwanted bit did not wake the task and was kept
forged reply self test: passed, a task that did not receive the message could not answer it
tier 2: preemptive scheduling online.
tier 2: EL0 and syscalls online.
tier 3: task faults are contained.
tier 3: priority scheduling and blocking online.
tier 3: synchronous IPC online.
tier 3: leases online.
tier 3: supervised restart online.
tier 3: notifications online, heartbeat now runs at EL0.
tier 4: the machine describes itself, device tree in hand.

uptime 1s (100 ticks)
uptime 2s (200 ticks)
uptime 3s (300 ticks)
```

From `heartbeat started` onwards the machine is running on its own. It sits in
`wfi` and wakes 100 times a second on a timer interrupt, at around 1% host CPU.

The trap self test is not decoration. It executes a real `brk`, which traps
into the vector table, builds a register frame, decodes the syndrome, steps the
saved `ELR` past the breakpoint, and returns through `eret`. A boot log is
therefore evidence that the exception machinery works, rather than evidence
that nothing has faulted yet.

Requires `qemu-system-aarch64` and the `aarch64-unknown-none-softfloat` target
(`rustup target add aarch64-unknown-none-softfloat`).

## The ladder

| Tier | What lands |
| --- | --- |
| 0 | Boot bare metal, PL011 UART, panic handler |
| 1 | Exception vectors, generic timer interrupt, GIC, physical page allocator, MMU |
| 2 | Context switch, scheduler, syscalls, drop to EL0 |
| 3 | Task isolation and IPC, Hubris-shaped microkernel |
| 4 | Device tree parsing, virtio-blk, virtio-net, PCIe |
| 5 | Block layer, filesystem, TCP/IP stack |
| 6 | SMP boot, locks, per-CPU state |
| 7 | ELF loader, userspace, shell |
| 8 | Real hardware bring-up, own debugger |
| 9 | Service processor, host boot orchestration, multi-node control plane |

Tracked as GitHub milestones. Each tier is a milestone, each piece of work is
an issue, each change is a PR.

## How this project is meant to feel

Habits, not rules. The point is that this is still fun to open in ten years.

- **`main` always boots.** The one thing worth being strict about. When you come
  back after six months having forgotten everything, `cargo run` should still
  print the banner. That is the thread you pull to get back in. CI enforces this
  and nothing else; the lint job is advisory and will never block a merge.
- **Stop wherever you want.** Work in progress lives on a branch. Half a driver
  sitting on a branch for a year is fine, that is what branches are for.
- **Prefer a visible win.** One new line of output beats a large refactor with
  nothing to look at. Not a rule, just the thing that makes it feel good.
- **Few dependencies.** `no_std` with almost no crates barely rots. Code written
  today should still build in ten years, which is the whole bet.
- **Explain the hardware in comments.** Register offsets and magic constants get
  a sentence saying where they came from. Future you is the reader, and future
  you will not remember.

## Design notes

**Why aarch64.** QEMU `virt` drops us at the ELF entry point with the MMU off
and no legacy boot sequence to unwind. No real mode, no A20 gate, no multiboot.
Time goes into OS concepts instead of PC archaeology, and the same code path
leads to real Raspberry Pi hardware at tier 8.

**Why softfloat.** `CPACR_EL1.FPEN` is 0 at reset, so any FP or SIMD
instruction at EL1 traps. We could enable FP for the kernel, but then every
context switch in tier 2 has to save and restore 512 bytes of vector state.
Instead the kernel is compiled so it never emits an FP instruction. FP stays
trapped and belongs to userspace.

**Why the kernel loads at 0x4008_0000.** RAM on `virt` starts at 0x4000_0000,
and 0x8_0000 above the base of RAM is where the Linux arm64 boot protocol says
a flat kernel image goes. That is where QEMU loads one, so agreeing with it is
what makes the flat image bootable at all.

This was originally done for a different and wrong reason: the belief that QEMU
drops the device tree at the base of RAM and that loading 512 KiB up leaves it
alone. It does not. On `virt` the blob lands 128 MiB into RAM, well above the
kernel image and squarely inside the frame allocator's pool, which is why the
blob now has to be reserved explicitly once its size is known.

**Why a task fault is judged by exception level, not address.** A fault from
EL0 is the task's mistake and stops only that task; a fault at EL1 is a kernel
bug and stays fatal. The tempting shortcut is to look at the faulting address
and treat low ones as user problems, which is wrong in both directions: the
kernel faults on user addresses all the time while servicing a syscall, and
those are kernel bugs.

**Why exiting is two steps.** A task cannot free its own kernel stack: it is
standing on it. So exiting marks the task a zombie and switches away, and the
stack and address space are released later by whichever task happens to run
next, for which that memory is just memory. The table slot and the exit code
outlive the reaping, so a task that has exited can still be asked how it went,
which is the same reason Unix has zombies at all.

**Why a user pointer is checked against the caller's page tables.** A syscall
argument is a number the task chose, and the kernel is running with enough
privilege to honour any lie told with one. Checking that a pointer "looks like"
a user address is not enough, because the kernel can read plenty the caller
cannot. The question is not whether the address is low, it is whether *this
task* is allowed to touch it, which means walking the task's own tables and
reading the permissions the hardware would have enforced had the access come
from EL0.

**Why an EL0 privilege violation reports EC 0x00 and not EC 0x18.** EC 0x18 is
a *configurable* trap: a register EL0 could otherwise reach, which a higher
level asked to be told about. A register EL0 simply may not access, like
`SCTLR_EL1`, is not trapped at all. It is undefined at that level, and reports
the architecture's "unknown reason". Checking only for 0x18 catches none of the
ordinary privilege violations.

**Why the kernel has no low half of its own.** Task 0 is the kernel, it lives
entirely above `KERNEL_BASE`, and it runs with an empty `TTBR0`. Tasks without
an address space get that same empty root rather than inheriting whatever the
previous task had mapped, which is the difference between "no address space"
and "somebody else's address space". Switching tasks swaps `TTBR0` and leaves
`TTBR1` alone, which is the entire reason the kernel was moved high in the
first place: changing address spaces cannot disturb the kernel's own mappings.
TLB invalidation is currently `tlbi vmalle1`, which throws away kernel entries
too. ASIDs are the right answer and are not implemented yet.

**Why a new task must start with interrupts enabled.** `yield_now` always
switches with interrupts masked, and restores the mask from a local on its own
stack once it is running again. A task that has never run has no saved mask to
restore, because it has never been through the second half of `switch`, so it
inherits whatever the task that created it happened to be holding. The result
is a task that can never be preempted. This is hard to notice because it looks
like a working scheduler: tasks still run, still finish, still hand off in
order. Only counting the switches gives it away. `task_trampoline` clears the
mask before calling the entry point.

**Why a cooperative context switch saves so little.** Under AAPCS64 the
compiler already treats a function call as destroying `x0` through `x18`, so at
the moment `switch` is called, every value the outgoing task still cares about
is either in a callee-saved register or already on its stack. Saving twelve
registers and swapping stack pointers is the whole job. That only holds for a
*voluntary* switch: preemption from an interrupt gets no such promise, because
the interrupted code never agreed to a call boundary, which is why the trap
frame in `vectors.S` saves everything.

**Why the kernel is linked high but loaded low.** PC relative references
(`adrp`) fix themselves up for free: they encode a link-time *difference*, so
they resolve to physical addresses with the MMU off and to high ones afterwards
with no relocation step. Absolute pointers stored in static data do not. The
function pointers `format_args!` builds into `.rodata` hold whatever address
the linker chose. Linking low and jumping to the high half therefore works
right up until the first `println!`, which branches into an address that no
longer translates. Linking at the high address and loading at the physical one
makes those pointers correct from the start. The price is that nothing before
the MMU comes on may use formatting at all, which is what `emergency_print`
is for.

**Why the frame allocator is a bitmap.** The compact alternative threads a
free list through the free frames themselves and costs no separate storage. It
also puts the allocator's metadata inside memory it has given away the rights
to, so one wild write into a freed frame corrupts the allocator rather than the
caller. A bitmap costs one bit per frame, 8 KiB of BSS for 256 MiB, and buys
O(1) queries, a printable memory map, and double-free detection. Allocation is
always the lowest free frame with no search hint, which makes reuse
deterministic and therefore testable.

**Why the timer re-arms with CVAL, not TVAL.** `CNTP_TVAL_EL0` is a countdown
from the moment it is written, so re-arming with it makes every period
`interval + however long it took to reach the handler`. That latency compounds
on every tick: measured against wall clock, a TVAL re-arm at 100 Hz ran 25%
slow in a debug build under TCG. `CNTP_CVAL_EL0` is an absolute deadline, so
anchoring the next one to the previous deadline means handler latency has to
exceed a whole interval before it costs anything. Measured gap is now a
constant 1 second of startup at both 12 and 24 seconds of runtime, with no
accumulation.

**Why the console lock is not a spinlock.** Rust's atomics compile to
`LDXR`/`STXR`, which depend on the exclusive monitor, and the architecture only
guarantees that works for Normal cacheable memory. With the MMU off every
access is Device memory, where `STXR` is permitted to fail forever. QEMU's TCG
emulates exclusives faithfully anyway, so an `AtomicBool` spinlock passes every
test we can run today and then hangs on real silicon. On one core with no
preemption, masking interrupts is not an approximation of mutual exclusion, it
is exactly mutual exclusion, and it works with the MMU off. This grows a real
spinlock underneath the same API when tier 6 boots a second core.

**Why lower priority numbers win.** `Priority(0)` outranks `Priority(1)`, which
reads backwards until you notice that everything else in the neighbourhood
agrees: Hubris, the GIC's priority registers, and Unix nice values all treat a
smaller number as more urgent. Flipping it to be friendlier would mean every
comparison against a hardware priority register read the opposite way round
from the one next to it. A named `outranks` method carries the meaning instead,
because `a < b` meaning "a is more important" is exactly the line that gets
read wrong.

**Why scheduling is strictly priority ordered, not fair.** A runnable task at
priority 0 runs and a task at priority 1 does not, however long it has waited.
Round robin applies only within one priority. This is the Hubris model, and the
reason is latency: "the high priority task runs as soon as it is runnable" is a
sentence you can build a real-time guarantee on, and no amount of time slicing
between priorities produces it. The cost is that starvation is a designed
outcome rather than a bug, and the answer to a starved task is to not give a
busy task a high priority.

**Why the kernel thread is the idle task.** Task 0 sits at `Priority::IDLE` and
is always runnable, so it gets the CPU exactly when nothing else can use it.
That is what an idle task is, so there is no separate one and no special case
in the picker. It also means the "nothing is runnable" branch is unreachable in
practice, which is why hitting it panics with a message rather than quietly
resuming something that asked not to run.

**Why blocked is not just "very low priority".** A blocked task is skipped
entirely, not deprioritised. The highest priority task in the system, blocked,
loses to the lowest priority runnable one. Modelling waiting as low priority
instead would mean a blocked task runs the moment the machine is otherwise
quiet, which is precisely when a waiting task must not run.

**Why waking somebody more important costs you the CPU immediately.** `unblock`
switches away on the spot if the woken task outranks the caller, rather than
setting a flag for the next tick. Without that, "runs as soon as it is
runnable" quietly means "runs within 10 milliseconds", and the priority scheme
stops being worth having. It does make `unblock` a scheduling point, so nothing
may be held across it that the woken task might want. Removing that switch
still leaves a test that looks healthy: the woken task runs, just late, which
is why the self test asserts on the order of four recorded turns rather than on
whether each task had one.

**Why message passing is a rendezvous with no queue.** `send` blocks until the
target replies. There is no buffer in the middle and no allocation on the send
path: every message in flight is described by two buffers belonging to the two
tasks involved, and the kernel copies between them exactly once. One decision
that pays three times. There is no queue to size, so there is no wrong size to
pick. The kernel allocates nothing, so a task cannot exhaust kernel memory by
sending faster than anybody listens. And back pressure is free, because a
sender that outruns its receiver blocks, which is information about the system
rather than a slow leak of it. The cost is real: a send to a task that never
receives waits forever, so who may send to whom becomes a design decision
rather than an accident. Hubris answers that with an ordering on senders.
Nothing enforces one here yet.

**Why the kernel does the copying, twice validated.** Neither task can see the
other's memory, which was the entire point of tier 2's address spaces, so the
kernel reads the sender's buffer and writes the receiver's. Each side is
checked against *its own owner's* page tables, not against whoever happens to
be running. The receiver is the task on the CPU when a queued message is
collected, and validating the sender's pointer against the receiver's tables
would let a receiver name any address it liked and have the kernel treat it as
the sender's. The copy goes through the high half physical map rather than
swapping `TTBR0_EL1` twice per chunk, because the high half already maps all of
physical memory and both sides are reachable at once.

**Why blocking is two calls and not one.** `mark_current_blocked` takes a task
out of the run queue; `park` actually stops it. A task about to wake somebody
else has to be out of the queue *before* it does the waking, because the woken
task can answer immediately and an answer that arrives while the sender still
looks runnable is a wakeup delivered to nobody. `park` is then conditional on
still being blocked, because by the time it is reached the thing being waited
for may already have happened. Writing this as one call deadlocked on the first
message the kernel ever sent.

**Why an answered sender is cleared by whoever answered it.** `pending` means
"what this task is waiting for right now", and the instant an outcome is
written it is waiting for nothing. Leaving the flag for the sender to clear
when it next runs opened a window where the reply had been delivered but the
sender still looked like it was waiting on its replier, and the replier exiting
a few instructions later overwrote a perfectly good reply with `EDEAD`.

**Why a lease can be validated once and never again.** A sender attaches
regions of its own memory to a send, marked readable, writable, or both, and
the kernel checks each one against the sender's page tables at send time only.
In almost any other design that would be a stale check by the time anybody used
it. Here it is exact, because a lease lives only while its owner is blocked on
the send that carried it: the sender cannot be running to remap or free the
memory underneath a borrow, since being blocked until the reply is what having
sent *means*. The reply ends the send and the lease with it, and nothing has to
be revoked, because nothing was ever handed out. The receiver only ever had an
index, and an index into the lease table of a task that is no longer waiting
means nothing.

**Why the receiver gets an index and not a pointer.** `borrow_read` and
`borrow_write` name a lease by number and ask the kernel to move the bytes. A
receiver never holds an address in the sender's space and could do nothing with
one if it did, which is the only reason it is safe to give it access to memory
it must not be able to forge access to. Every borrow is bounded by the lease's
own length, checked without wrapping, and refused outright if the direction was
not lent.

**Why "it was refused" is too weak a test.** A borrow with an out of range
index hits an empty lease slot, which the *direction* check refuses on its own
because an empty slot permits nothing. A test that only looks for a nonzero
return therefore passes with the index bound deleted. The self test compares
against the specific error each refusal should produce, which turns "something
said no" into "the check that should have said no is the one that did".

**Why a restart keeps the task's id and nothing else.** Everything about a
restarted task is thrown away except the one thing anybody else might be
holding. The address space is destroyed and rebuilt from the program image, so
the task cannot come back to find its own wreckage; every frame comes from the
allocator zeroed. The kernel stack is reused rather than returned and re-asked
for, because a faulted task is never scheduled and so nobody is standing on it,
and winding it back to a never-run switch frame is the whole of what "fresh
stack" means here. The slot stays, which is what makes this a restart rather
than a replacement: anything holding the task's id still refers to that task.

**Why a dying task wakes people without switching to them.** `unblock` hands
the CPU straight over when the woken task outranks the caller, which is exactly
right for a reply and exactly wrong for a task on its way out. `fault_current`
is already marked as faulted by the time it wakes anybody, so the first switch
it makes is the last one it will ever make, and everything it had left to do
would simply never happen. It has two things left: releasing whoever was
blocked sending to it, and telling the supervisor. With an immediate wakeup,
whichever came second was lost, and which one that was depended on priorities.
`unblock_deferred` marks the task runnable and returns.

**Why the supervisor needs no queue of faults.** `fault_wait` blocks until some
task is faulted, then scans the task table and names one. A faulted task stays
faulted until somebody deals with it, so the table *is* the backlog: two faults
while the supervisor is busy are two tasks sitting in it, not one event that
overwrote another. Nothing can be missed by not being collected in time, which
is the failure mode a queue would have introduced along with a size to pick.

**Why the privilege check comes before the argument check.** `fault_info` and
`restart` refuse a caller that is not the supervisor before they look at what
was asked. The other order leaks the answer: a task told "no such faulted task"
has learned that there is no such faulted task, which is the thing it was not
supposed to be able to find out.

**Why a notification is a bitmask and not a queue.** Two of the same event
before the task gets a turn collapse into one bit. That is the contract, not a
limitation waiting to be lifted: a notification says *something happened*,
never *how many times*, and a driver that needs the count reads it from the
device. A queue would need a length, and a length needs an answer to what
happens when it fills. Drop the oldest, drop the newest, and block the
interrupt handler are all worse than a bit that is already set, and all three
make the kernel allocate on a path that runs with interrupts masked. A bit that
is already set requires no decision at all.

**Why the heartbeat reads the tick count instead of counting wakeups.** It is
the demonstration of the paragraph above. The heartbeat task is woken by a bit
and then asks the kernel how many ticks there have been. If it does not get a
turn for five ticks it is woken once, so counting wakeups would drift and
reading the count cannot. A notification it never saw costs it nothing.

**Why the kernel still owns the timer.** Every other line could be handed to a
task entirely. The timer cannot, because preemption is the scheduler's business
and the scheduler is not a task. So the interrupt does two things: the kernel
re-arms the deadline and asks for a reschedule, and the driver that got the
notification decides what the tick *means*. The console heartbeat used to be
four lines inside the interrupt handler and is now an unprivileged task that
formats its own decimal.

**Why routing an interrupt is not a syscall.** Which task owns which line is set
by the kernel at startup. A task that could claim a line could take another
task's device or silence it, and there is no way to tell a legitimate claim from
a theft without something that already knows the intended shape of the system.
The supervisor is where that will go.

**Why only the receiver may answer a message.** `reply` checks both that the
sender is waiting for a reply and that it is waiting on *the caller*. Without
the second half any task could release somebody else's sender and hand it a
fabricated answer, which the sender has no way to tell from a real one.

The check went in with the IPC and stayed untested for two tiers, because
proving it needs a moment when a reply is outstanding and the task entitled to
give it is not running, and a two task exchange never has one: the receiver
answers immediately. Notifications made the window: a server that parks waiting
for its device leaves a message in its hands and the CPU to somebody else, which
is both the realistic case and the one worth defending.

**Why a lock guard must not live in a `match` scrutinee.** `Lock` is interrupt
masking, so its guard restores the interrupt state it captured when it drops. A
guard created in a scrutinee lives until the end of the whole `match`, which
means an arm that restores the interrupt state itself and returns has the guard
drop *after* it and restore the state again, to whatever was current when the
lock was taken. That is "masked", and the mask never comes off. The machine
keeps running, the timer never fires again, and nothing anywhere says why.

This was sitting in `reply`'s refusal path from the day it was written. Nothing
had ever taken that path, because refusing a reply needs a task that did not
receive the message to try to answer it, which is exactly the case that had no
test. Writing the test found the check worked and the code around it did not.

**Why the kernel is booted as a flat image and not an ELF.** Handed an ELF,
QEMU assumes a bare metal program and does the minimum: sets the program
counter to the entry point, and nothing else. No device tree is built, and x0
is zero. Handed a flat binary it follows the Linux arm64 boot protocol
instead, which means it assembles a device tree describing the machine it just
created and passes that blob's address in x0. The tree is not an optional extra
QEMU offers on request; it exists only on the boot path that expects it. The
same is true of real firmware at tier 8, which loads images and not ELFs.

**Why the device tree pointer is an argument rather than a static.** It arrives
in x0 at the reset vector and is destroyed one instruction later, because the
first thing the boot code does is read `mpidr_el1` into the same register.
Stashing it in a static is the obvious fix and is wrong twice: the BSS has not
been zeroed yet, so the store is undone a few lines further down, and once the
BSS *is* zeroed the MMU is still off, so the static is being written through an
address that stops meaning that in a moment. Carrying it in a callee saved
register and passing it as an argument, through the jump to the high half and
all, avoids both. It is one `u64` and it costs nothing to hand along.

**Why the device tree header is checked before it is believed.** The blob is
the only input this kernel takes from outside itself, and it is a structure
made almost entirely of offsets and lengths, at an address we were merely told
about. Every field is a chance to read somewhere we should not: a `totalsize`
of `u32::MAX`, a struct block that starts past the end, an offset near the top
of the range whose bounds check passes only because the addition wrapped. On
QEMU the blob is always well formed, which is exactly why the checking had to
be written now. The first machine to hand us a bad one will not be the machine
we are testing on, and the self test is ten headers built by hand to be wrong
in ten specific ways.

**Why a reservation checks the whole range before setting a bit.** `reserve_range`
walks the range twice: once to find a frame that is already taken, and only
then again to claim them. Setting bits as it goes would leave a caller that
sees a failure, assumes nothing happened, and carries on, while part of the
range has quietly left the pool for good. The leak would be invisible, because
a reserved frame looks exactly like a frame in use.

**Why `reg` is decoded with the parent's cell counts.** `#address-cells` and
`#size-cells` describe a node's *children*, never the node itself, so a `reg`
is read with numbers declared one level up. On `virt` both are 2, which means
reading every `reg` as a pair of `u64` works perfectly and would keep working
until the first machine that uses 1 and 1, which is most 32 bit ones. The
walker therefore carries a cell count per open level, because by the time a
`reg` is decoded its parent's declaration is several tokens and possibly
several levels behind.

The defaults are their own trap. When a node does not say, the spec's answer is
2 address cells and 1 size cell, and it is a default rather than an
inheritance: a child of a node that declared 2 and 2 does not get 2 and 2, it
gets 2 and 1.

**Why `compatible` is a list and not a string.** A node names the specific
device first and the generic thing it behaves like afterwards, NUL separated,
most specific first. Comparing the property against one string finds the first
entry and misses every other one, which means a driver matching on the generic
name never matches any device that also names its model. The whole point of the
property is the fallback.

**Why every malformed tree ends the walk rather than reporting how.** The blob
is offsets and lengths all the way down, and the failures are not
distinguishable in any useful way: a token that is not a token, a length that
runs off the end, a name with no terminator, and a tree deeper than the walker
can hold all mean the same thing, which is that we are no longer reading
structure. Returning a reason would tempt a caller into carrying on with half a
tree. Stopping is the only answer that is safe for all of them.

The test builds those trees by hand, because QEMU will never hand us one. A
property that claims to be 64 KiB long, a node name with no NUL, eighteen
levels of nesting against a walker that holds sixteen: each is a real read that
would have gone somewhere it should not.

## Layout

```
kernel/
  linker.ld       memory layout, load address, stack, BSS bounds
  build.rs        hands linker.ld to the linker
  src/
    boot.S        reset vector: keep the DTB pointer, park secondary cores, set sp, zero BSS
    vectors.S     the 16 entry exception vector table, save and restore
    main.rs       kernel_main, panic handler
    uart.rs       PL011 driver and the print!/println! macros
    exceptions.rs trap frame layout, ESR decoding, handler policy
    sync.rs       interrupt masking and the console lock
    frames.rs     physical frame allocator
    fdt.rs        the device tree the firmware left us: header, walker, lookups
    gic.rs        GICv2 distributor and CPU interface
    paging.rs     page tables, permissions, the move to the high half
    switch.S      the callee-saved swap that changes which task is running
    tasks.rs      kernel tasks, scheduling, address spaces, EL0 entry
    syscall.rs    the SVC interface and its argument checking
    user.S        the program that runs at EL0
    timer.rs      generic timer, the 100 Hz heartbeat
    semihosting.rs asking QEMU to shut the machine down
scripts/
  boot-test.sh    boots the kernel and fails if the banner never appears
  image.sh        turns the linked ELF into the flat image QEMU boots
  qemu-run.sh     what `cargo run` actually runs
```
