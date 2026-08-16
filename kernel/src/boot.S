// First instructions the machine runs. QEMU jumps straight here with the MMU
// off, caches cold, and every core released at once.
//
// Job: park the cores we are not ready for, give core 0 a stack, zero the BSS
// (Rust assumes statics start at zero), then hand control to Rust and never
// come back.

.section ".text.boot"
.global _start

_start:
    // QEMU starts every core at the same address. Only core 0 continues.
    mrs     x0, mpidr_el1
    and     x0, x0, #0xff
    cbz     x0, .Lprimary

.Lpark:
    wfe
    b       .Lpark

.Lprimary:
    adrp    x0, __stack_top
    add     x0, x0, :lo12:__stack_top
    mov     sp, x0

    adrp    x0, __bss_start
    add     x0, x0, :lo12:__bss_start
    adrp    x1, __bss_end
    add     x1, x1, :lo12:__bss_end
.Lzero_bss:
    cmp     x0, x1
    b.hs    .Lzero_done
    str     xzr, [x0], #8
    b       .Lzero_bss
.Lzero_done:

    bl      kernel_main

    // kernel_main is `-> !`, so reaching here means something went very wrong.
.Lhalt:
    wfe
    b       .Lhalt
