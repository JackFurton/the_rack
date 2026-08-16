// The AArch64 exception vector table.
//
// VBAR_EL1 points at a 2 KiB aligned table of 16 entries, each exactly 128
// bytes. The hardware computes the target as VBAR + (index * 128) and jumps
// there directly, so an entry is not a pointer to a handler, it is the first
// instructions of the handler.
//
// The 16 entries are four groups of four. Which group fires depends on where
// the exception came from, and within a group the four entries are always
// synchronous, IRQ, FIQ, SError in that order:
//
//   0x000  current EL, using SP_EL0
//   0x200  current EL, using SP_ELx      <- where we land today
//   0x400  lower EL, caller was AArch64  <- userspace, once tier 2 exists
//   0x600  lower EL, caller was AArch32
//
// SPSel is 1 out of reset, so the kernel runs on SP_EL1 and every fault we can
// currently take arrives in the 0x200 group. That is exactly where tier 0's
// trapped FP instruction went, into a table that did not exist.

// Trap frame layout. Must stay in sync with `struct TrapFrame` in
// exceptions.rs, which is #[repr(C)] so its offsets are these offsets.
.set FRAME_SIZE, 0x120  // 288 bytes: 31 GPRs + ELR + SPSR + ESR + FAR + pad
.set FRAME_X30,  0x0f0
.set FRAME_SPSR, 0x100
.set FRAME_FAR,  0x110

// One vector entry. Kept to four instructions so it comfortably fits the 128
// byte slot: make room, stash x0/x1 so we have scratch registers, record which
// entry we came through, and jump to the shared body.
.macro VECTOR index
    .balign 128
    sub     sp, sp, #FRAME_SIZE
    stp     x0, x1, [sp, #0x000]
    mov     x0, #\index
    b       .Lcommon_trap
.endm

    .section .text.vectors
    .balign 2048
    .global __vectors
__vectors:
    // Current EL with SP_EL0
    VECTOR 0    // synchronous
    VECTOR 1    // IRQ
    VECTOR 2    // FIQ
    VECTOR 3    // SError

    // Current EL with SP_ELx
    VECTOR 4    // synchronous
    VECTOR 5    // IRQ
    VECTOR 6    // FIQ
    VECTOR 7    // SError

    // Lower EL, AArch64
    VECTOR 8    // synchronous
    VECTOR 9    // IRQ
    VECTOR 10   // FIQ
    VECTOR 11   // SError

    // Lower EL, AArch32
    VECTOR 12   // synchronous
    VECTOR 13   // IRQ
    VECTOR 14   // FIQ
    VECTOR 15   // SError

// Shared body for every entry.
//
// On arrival: sp already points at the frame, x0 and x1 are saved, and x0
// holds the vector index. Everything else is still live and must be preserved,
// because a handler may return and the interrupted code has no idea any of
// this happened.
    .section .text
.Lcommon_trap:
    stp     x2,  x3,  [sp, #0x010]
    stp     x4,  x5,  [sp, #0x020]
    stp     x6,  x7,  [sp, #0x030]
    stp     x8,  x9,  [sp, #0x040]
    stp     x10, x11, [sp, #0x050]
    stp     x12, x13, [sp, #0x060]
    stp     x14, x15, [sp, #0x070]
    stp     x16, x17, [sp, #0x080]
    stp     x18, x19, [sp, #0x090]
    stp     x20, x21, [sp, #0x0a0]
    stp     x22, x23, [sp, #0x0b0]
    stp     x24, x25, [sp, #0x0c0]
    stp     x26, x27, [sp, #0x0d0]
    stp     x28, x29, [sp, #0x0e0]

    // The syndrome registers, captured before anything can overwrite them.
    // ELR is the address to resume at, ESR says what happened, FAR holds the
    // faulting address for aborts.
    mrs     x1, elr_el1
    stp     x30, x1,  [sp, #FRAME_X30]
    mrs     x1, spsr_el1
    mrs     x2, esr_el1
    stp     x1,  x2,  [sp, #FRAME_SPSR]
    mrs     x1, far_el1
    str     x1,       [sp, #FRAME_FAR]

    // handle_exception(frame: &mut TrapFrame, index: u64)
    mov     x1, x0      // index, saved from the vector entry
    mov     x0, sp      // pointer to the frame we just built
    bl      handle_exception

    // The handler may have edited the frame, so reload everything from it
    // rather than assuming nothing changed. Skipping past a BRK works by
    // bumping the saved ELR here.
    ldp     x1,  x2,  [sp, #FRAME_SPSR]
    msr     spsr_el1, x1
    ldp     x30, x1,  [sp, #FRAME_X30]
    msr     elr_el1, x1

    ldp     x2,  x3,  [sp, #0x010]
    ldp     x4,  x5,  [sp, #0x020]
    ldp     x6,  x7,  [sp, #0x030]
    ldp     x8,  x9,  [sp, #0x040]
    ldp     x10, x11, [sp, #0x050]
    ldp     x12, x13, [sp, #0x060]
    ldp     x14, x15, [sp, #0x070]
    ldp     x16, x17, [sp, #0x080]
    ldp     x18, x19, [sp, #0x090]
    ldp     x20, x21, [sp, #0x0a0]
    ldp     x22, x23, [sp, #0x0b0]
    ldp     x24, x25, [sp, #0x0c0]
    ldp     x26, x27, [sp, #0x0d0]
    ldp     x28, x29, [sp, #0x0e0]
    ldp     x0,  x1,  [sp, #0x000]

    add     sp, sp, #FRAME_SIZE
    eret
