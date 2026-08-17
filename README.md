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
  reserved : 0x0040000000..0x00400a3000   652 KiB  kernel image and DTB
  free     : 0x00400a3000..0x0050000000   255 MiB  65363 frames

trap self test: resumed, registers intact
lock self test: passed, guarded counter reached 1
frame self test: passed, reuse of 0x400ad000 came back clean, 65363 frames free
paging self test: passed, running at 0xffff000040084a94
  write to .text   : permission fault
  write to .rodata : permission fault
  read low half    : translation fault, nothing mapped

gic: 288 interrupt lines
timer: 62500000 Hz counter, tick every 10 ms

tier 0 complete. we are alive on bare metal.
tier 1: exception vectors online.
tier 1: paging online, kernel in the high half.
tier 1: heartbeat started, interrupts live.

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

**Why the kernel loads at 0x4008_0000.** RAM on `virt` starts at 0x4000_0000
and QEMU puts the device tree blob at the base of it. Loading 512 KiB up leaves
the DTB intact for tier 4.

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

## Layout

```
kernel/
  linker.ld       memory layout, load address, stack, BSS bounds
  build.rs        hands linker.ld to the linker
  src/
    boot.S        reset vector: park secondary cores, set sp, zero BSS
    vectors.S     the 16 entry exception vector table, save and restore
    main.rs       kernel_main, panic handler
    uart.rs       PL011 driver and the print!/println! macros
    exceptions.rs trap frame layout, ESR decoding, handler policy
    sync.rs       interrupt masking and the console lock
    frames.rs     physical frame allocator
    gic.rs        GICv2 distributor and CPU interface
    paging.rs     page tables, permissions, the move to the high half
    timer.rs      generic timer, the 100 Hz heartbeat
    semihosting.rs asking QEMU to shut the machine down
scripts/
  boot-test.sh    boots the kernel and fails if the banner never appears
```
