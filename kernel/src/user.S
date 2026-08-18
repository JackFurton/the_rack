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

// A pair of programs that exchange a message.
//
// Two separate blobs, copied into two separate address spaces, sharing nothing
// but the syscall interface. That is the point: neither can see the other's
// memory, so every byte that crosses between them was carried by the kernel.
//
// Each blob carries its own copy of the literals it compares against. They
// cannot share one, because a blob is copied into a page on its own and an
// `adr` in one would be pointing into memory the other does not have.

.set SYS_SEND,  4
.set SYS_RECV,  5
.set SYS_REPLY, 6

// Exit codes, so a failure says which check failed rather than just "not zero".
.set FAIL_RC,   1
.set FAIL_LEN,  2
.set FAIL_BODY, 3
.set FAIL_DEAD, 4

    .section .text.user, "ax"
    .global user_client_start
    .global user_client_end

user_client_start:
    // The kernel planted the server's task id in the top word of the stack.
    ldr     x19, [sp, #8]

    // Scratch for the reply. The stack page is the only writable memory this
    // task has, and the kernel is about to write into it on our behalf, so it
    // has to be somewhere we genuinely own.
    sub     sp, sp, #64
    mov     x20, sp

    // send(server, 0x51, "ping", 4, reply, 32)
    mov     x8, #SYS_SEND
    mov     x0, x19
    mov     x1, #0x51
    adr     x2, .Lping
    mov     x3, #(.Lping_end - .Lping)
    mov     x4, x20
    mov     x5, #32
    svc     #0

    // x0 is whatever the server passed to reply, x1 is how much of its answer
    // fitted in our buffer.
    cmp     x0, #0
    b.ne    .Lfail_rc
    cmp     x1, #(.Lpong_end - .Lpong)
    b.ne    .Lfail_len

    // Compare the reply byte for byte. A length on its own would pass on a
    // copy that moved the right number of wrong bytes.
    adr     x2, .Lpong
    mov     x3, #(.Lpong_end - .Lpong)
    mov     x4, x20
.Lcompare:
    ldrb    w5, [x2], #1
    ldrb    w6, [x4], #1
    cmp     w5, w6
    b.ne    .Lfail_body
    subs    x3, x3, #1
    b.ne    .Lcompare

    // Give the server time to reply to us and then exit, so the next send has
    // a corpse to talk to.
    mov     x21, #16
.Lwait:
    mov     x8, #SYS_YIELD
    svc     #0
    subs    x21, x21, #1
    b.ne    .Lwait

    // Sending to a task that has gone must come back as an error. The
    // alternative is a wait that nothing will ever end.
    mov     x8, #SYS_SEND
    mov     x0, x19
    mov     x1, #0x51
    adr     x2, .Lping
    mov     x3, #(.Lping_end - .Lping)
    mov     x4, x20
    mov     x5, #32
    svc     #0
    mov     x9, #-3                 // EDEAD
    cmp     x0, x9
    b.ne    .Lfail_dead

    mov     x0, #0
    b       .Lclient_exit

.Lfail_rc:
    mov     x0, #FAIL_RC
    b       .Lclient_exit
.Lfail_len:
    mov     x0, #FAIL_LEN
    b       .Lclient_exit
.Lfail_body:
    mov     x0, #FAIL_BODY
    b       .Lclient_exit
.Lfail_dead:
    mov     x0, #FAIL_DEAD

.Lclient_exit:
    mov     x8, #SYS_EXIT
    svc     #0
.Lclient_stuck:
    b       .Lclient_stuck

.Lping:
    .ascii  "ping"
.Lping_end:
.Lpong:
    .ascii  "pong!"
.Lpong_end:

    .balign 4
user_client_end:

    .section .text.user, "ax"
    .global user_server_start
    .global user_server_end

user_server_start:
    sub     sp, sp, #64
    mov     x20, sp

    // recv(buf, 32) -> x0 sender, x1 operation, x2 length
    mov     x8, #SYS_RECV
    mov     x0, x20
    mov     x1, #32
    svc     #0
    mov     x19, x0
    mov     x21, x1
    mov     x22, x2

    // Put the payload on the console. Without this the exchange is only ever
    // two programs agreeing with each other; printing it means the bytes are
    // visible from outside both of them.
    mov     x8, #SYS_WRITE
    mov     x0, #1
    adr     x1, .Lgot
    mov     x2, #(.Lgot_end - .Lgot)
    svc     #0

    mov     x8, #SYS_WRITE
    mov     x0, #1
    mov     x1, x20
    mov     x2, x22
    svc     #0

    mov     x8, #SYS_WRITE
    mov     x0, #1
    adr     x1, .Lnewline
    mov     x2, #1
    svc     #0

    // x23 is the return code: zero unless something about the message is
    // wrong. Checked here rather than only in the client, because the message
    // has to arrive intact in both directions.
    mov     x23, #0

    cmp     x21, #0x51
    b.eq    .Lop_ok
    mov     x23, #1
    b       .Lanswer
.Lop_ok:
    cmp     x22, #(.Lexpect_end - .Lexpect)
    b.eq    .Lsize_ok
    mov     x23, #1
    b       .Lanswer
.Lsize_ok:
    adr     x2, .Lexpect
    mov     x3, x22
    mov     x4, x20
.Lserver_compare:
    ldrb    w5, [x2], #1
    ldrb    w6, [x4], #1
    cmp     w5, w6
    b.eq    .Lserver_same
    mov     x23, #1
    b       .Lanswer
.Lserver_same:
    subs    x3, x3, #1
    b.ne    .Lserver_compare

.Lanswer:
    // reply(sender, rc, "pong!", 5)
    mov     x8, #SYS_REPLY
    mov     x0, x19
    mov     x1, x23
    adr     x2, .Lanswer_body
    mov     x3, #(.Lanswer_body_end - .Lanswer_body)
    svc     #0

    mov     x8, #SYS_EXIT
    mov     x0, x23
    svc     #0
.Lserver_stuck:
    b       .Lserver_stuck

.Lgot:
    .ascii  "ipc: server received "
.Lgot_end:
.Lnewline:
    .ascii  "\n"
.Lexpect:
    .ascii  "ping"
.Lexpect_end:
.Lanswer_body:
    .ascii  "pong!"
.Lanswer_body_end:

    .balign 4
user_server_end:
