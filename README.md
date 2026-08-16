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
the_rack 0.1.0
aarch64 / qemu-virt

  exception level : EL1
  kernel loaded   : 0x0000000040080000
  kernel end      : 0x0000000040095d80
  vector table    : 0x0000000040080800

trap self test: executing brk #0x42

--- exception ---
  vector : synchronous (current EL, SP_ELx)
  class  : BRK instruction (EC 0x3c)
  esr    : 0x00000000f2000042
  elr    : 0x0000000040081d80
  spsr   : 0x00000000600003c5
  comment: 0x42
-----------------
trap self test: resumed, registers intact

tier 0 complete. we are alive on bare metal.
tier 1: exception vectors online.
```

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
    semihosting.rs asking QEMU to shut the machine down
scripts/
  boot-test.sh    boots the kernel and fails if the banner never appears
```
