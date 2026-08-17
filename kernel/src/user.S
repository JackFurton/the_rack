// A program that runs at EL0.
//
// Written in assembly and copied into a user page rather than linked into the
// kernel and jumped to, because user code has to live in the task's own
// address space at an address the kernel never executes from. The blob is
// copied out of the kernel image byte for byte, so everything it touches must
// be PC relative: it runs at a completely different address from the one it
// was linked at.
//
// Syscall convention follows Linux on aarch64 rather than something invented:
// number in x8, arguments in x0 through x5, result back in x0.

.set SYS_EXIT,   0
.set SYS_WRITE,  1
.set SYS_YIELD,  2
.set SYS_GETPID, 3

    .section .text.user, "ax"
    .global user_program_start
    .global user_program_end

user_program_start:
    // write(1, message, len). Should print.
    mov     x8, #SYS_WRITE
    mov     x0, #1
    adr     x1, .Lmessage
    mov     x2, #(.Lmessage_end - .Lmessage)
    svc     #0

    // Prove we really are unprivileged. Reading SCTLR_EL1 is ordinary at EL1
    // and must trap from EL0. If this simply executes, the eret set up the
    // wrong exception level and everything else here proves nothing. The
    // kernel counts the trap and steps over it.
    mrs     x0, sctlr_el1

    // Exercise the syscall path for something that switches tasks rather than
    // just returning a value.
    mov     x8, #SYS_YIELD
    svc     #0

    mov     x8, #SYS_GETPID
    svc     #0

    // write(1, KERNEL_BASE, 16). A pointer into the kernel's half of the
    // address space, which this task cannot read. The kernel can, which is
    // exactly why it has to check the caller's tables rather than its own
    // ability. Must come back as EFAULT.
    mov     x8, #SYS_WRITE
    mov     x0, #1
    movz    x1, #0xffff, lsl #48
    mov     x2, #16
    svc     #0

    // Exit carrying whatever the refused write returned, so the kernel can
    // check the task was told no rather than quietly served.
    mov     x8, #SYS_EXIT
    svc     #0

    // exit never returns. If it does, do not fall into the message.
.Lstuck:
    b       .Lstuck

.Lmessage:
    .ascii  "hello from EL0, running unprivileged\n"
.Lmessage_end:

    .balign 4
user_program_end:

// A program that gets itself killed.
//
// Deliberately separate from the working one, so the fault path can be tested
// without the well behaved program having to misbehave.
    .section .text.user, "ax"
    .global user_fault_start
    .global user_fault_end

user_fault_start:
    // Dereference null. Nothing is mapped at zero in any address space, which
    // is the whole reason the identity map was retired when paging came up.
    mov     x0, xzr
    str     x0, [x0]

    // The kernel must never let us reach here.
.Lnever:
    b       .Lnever

    .balign 4
user_fault_end:
