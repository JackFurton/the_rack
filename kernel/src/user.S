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
    mov     x6, xzr                 // no leases
    mov     x7, xzr
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
    mov     x6, xzr                 // no leases
    mov     x7, xzr
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

// A pair of programs that lend memory instead of copying it.
//
// The lender hands over two regions of its own stack: one the borrower may
// read and write, one it may only read. The borrower never sees an address in
// the lender's space. It names a lease by index and asks the kernel to move
// bytes, and every way of asking for something it was not lent has to be
// refused.

.set EINVAL, -1
.set EFAULT, -2
.set EPERM,  -4

.set SYS_BORROW_READ,  7
.set SYS_BORROW_WRITE, 8

.set LEASE_READ,  1
.set LEASE_WRITE, 2

// Bits the borrower sets in its return code, one per check that went wrong.
.set BAD_READ_RC,    1      // reading a lease we were given failed
.set BAD_READ_BODY,  2      // it succeeded and produced the wrong bytes
.set BAD_WRITE_RC,   4      // writing a lease we were given failed
.set BAD_OVERRUN,    8      // a borrow past the end of a lease was allowed
.set BAD_DIRECTION, 16      // writing a read only lease was allowed
.set BAD_INDEX,     32      // a lease index nobody lent us was accepted
.set BAD_STALE,     64      // a borrow after the reply still worked

    .section .text.user, "ax"
    .global user_lender_start
    .global user_lender_end

user_lender_start:
    ldr     x19, [sp, #8]
    sub     sp, sp, #256
    mov     x20, sp

    // Lease 0 lives at x20 and is 32 bytes. Seed it with something the
    // borrower can read back, so the read direction is proved by content
    // rather than by a call returning zero.
    adr     x2, .Lseed
    mov     x3, #(.Lseed_end - .Lseed)
    mov     x4, x20
.Lseed_copy:
    ldrb    w5, [x2], #1
    strb    w5, [x4], #1
    subs    x3, x3, #1
    b.ne    .Lseed_copy

    // The descriptor table: two entries of base, length, attributes.
    add     x21, x20, #64

    str     x20, [x21]              // lease 0 base
    mov     x4, #32
    str     x4, [x21, #8]           // lease 0 length
    mov     x4, #(LEASE_READ | LEASE_WRITE)
    str     x4, [x21, #16]

    add     x5, x20, #32
    str     x5, [x21, #24]          // lease 1 base
    mov     x4, #16
    str     x4, [x21, #32]          // lease 1 length
    mov     x4, #LEASE_READ         // read only, on purpose
    str     x4, [x21, #40]

    add     x22, x20, #128          // reply buffer

    // Before the real send: try to lend memory we do not own. A lease over the
    // kernel's half of the address space has to be refused at send time, while
    // this task is still running and can be told no, rather than becoming
    // something the receiver can ask the kernel to read on its behalf.
    add     x23, x20, #192
    movz    x4, #0xffff, lsl #48
    str     x4, [x23]
    mov     x4, #16
    str     x4, [x23, #8]
    mov     x4, #LEASE_READ
    str     x4, [x23, #16]

    mov     x8, #SYS_SEND
    mov     x0, x19
    mov     x1, #0x60
    adr     x2, .Llend_msg
    mov     x3, #(.Llend_msg_end - .Llend_msg)
    mov     x4, x22
    mov     x5, #32
    mov     x6, x23
    mov     x7, #1
    svc     #0
    mov     x9, #EFAULT
    cmp     x0, x9
    b.ne    .Llend_fail_lease

    mov     x8, #SYS_SEND
    mov     x0, x19
    mov     x1, #0x60
    adr     x2, .Llend_msg
    mov     x3, #(.Llend_msg_end - .Llend_msg)
    mov     x4, x22
    mov     x5, #32
    mov     x6, x21
    mov     x7, #2
    svc     #0

    // The borrower reports every refused check it should have been refused.
    cmp     x0, #0
    b.ne    .Llend_fail_rc

    // The bytes the borrower wrote through the lease have to be here now. This
    // is the whole point: memory changed hands without either task ever seeing
    // the other's address space.
    adr     x2, .Lpatched
    mov     x3, #(.Lpatched_end - .Lpatched)
    mov     x4, x20
.Llend_compare:
    ldrb    w5, [x2], #1
    ldrb    w6, [x4], #1
    cmp     w5, w6
    b.ne    .Llend_fail_body
    subs    x3, x3, #1
    b.ne    .Llend_compare

    mov     x0, #0
    b       .Llend_exit
.Llend_fail_rc:
    mov     x0, #1
    b       .Llend_exit
.Llend_fail_body:
    mov     x0, #2
    b       .Llend_exit
.Llend_fail_lease:
    mov     x0, #3
.Llend_exit:
    mov     x8, #SYS_EXIT
    svc     #0
.Llend_stuck:
    b       .Llend_stuck

.Lseed:
    .ascii  "seed"
.Lseed_end:
.Lpatched:
    .ascii  "written through a lease"
.Lpatched_end:
.Llend_msg:
    .ascii  "lend"
.Llend_msg_end:

    .balign 4
user_lender_end:

    .section .text.user, "ax"
    .global user_borrower_start
    .global user_borrower_end

user_borrower_start:
    sub     sp, sp, #128
    mov     x20, sp
    add     x22, x20, #64           // scratch, for bytes read out of a lease

    mov     x8, #SYS_RECV
    mov     x0, x20
    mov     x1, #32
    svc     #0
    mov     x19, x0                 // the lender
    mov     x21, #0                 // accumulated failures

    // 1. Read what we were lent, and check we got the right bytes.
    mov     x8, #SYS_BORROW_READ
    mov     x0, x19
    mov     x1, #0
    mov     x2, #0
    mov     x3, x22
    mov     x4, #(.Lexpect_seed_end - .Lexpect_seed)
    svc     #0
    cmp     x0, #0
    b.eq    .Lcheck_body
    orr     x21, x21, #BAD_READ_RC
    b       .Lwrite_step

.Lcheck_body:
    adr     x2, .Lexpect_seed
    mov     x3, #(.Lexpect_seed_end - .Lexpect_seed)
    mov     x4, x22
.Lseed_compare:
    ldrb    w5, [x2], #1
    ldrb    w6, [x4], #1
    cmp     w5, w6
    b.eq    .Lseed_same
    orr     x21, x21, #BAD_READ_BODY
    b       .Lwrite_step
.Lseed_same:
    subs    x3, x3, #1
    b.ne    .Lseed_compare

.Lwrite_step:
    // 2. Write into it. The lender checks this landed.
    mov     x8, #SYS_BORROW_WRITE
    mov     x0, x19
    mov     x1, #0
    mov     x2, #0
    adr     x3, .Lnew_body
    mov     x4, #(.Lnew_body_end - .Lnew_body)
    svc     #0
    cmp     x0, #0
    b.eq    .Loverrun_step
    orr     x21, x21, #BAD_WRITE_RC

.Loverrun_step:
    // 3. Lease 0 is 32 bytes. Offset 30 plus 8 runs off the end of it and has
    //    to be refused, or a lease is just a pointer with extra steps.
    mov     x8, #SYS_BORROW_WRITE
    mov     x0, x19
    mov     x1, #0
    mov     x2, #30
    adr     x3, .Lnew_body
    mov     x4, #8
    svc     #0
    // Refused *for the right reason*. "Some error came back" is too weak: a
    // zeroed lease slot is refused too, by a different check, so a bound test
    // that only looks for nonzero passes with the bound removed.
    mov     x9, #EINVAL
    cmp     x0, x9
    b.eq    .Ldirection_step
    orr     x21, x21, #BAD_OVERRUN

.Ldirection_step:
    // 4. Lease 1 was lent read only.
    mov     x8, #SYS_BORROW_WRITE
    mov     x0, x19
    mov     x1, #1
    mov     x2, #0
    adr     x3, .Lnew_body
    mov     x4, #4
    svc     #0
    mov     x9, #EPERM
    cmp     x0, x9
    b.eq    .Lindex_step
    orr     x21, x21, #BAD_DIRECTION

.Lindex_step:
    // 5. There is no lease 2.
    mov     x8, #SYS_BORROW_READ
    mov     x0, x19
    mov     x1, #2
    mov     x2, #0
    mov     x3, x22
    mov     x4, #4
    svc     #0
    mov     x9, #EINVAL
    cmp     x0, x9
    b.eq    .Lborrow_reply
    orr     x21, x21, #BAD_INDEX

.Lborrow_reply:
    mov     x8, #SYS_REPLY
    mov     x0, x19
    mov     x1, x21
    adr     x2, .Ldone
    mov     x3, #(.Ldone_end - .Ldone)
    svc     #0

    // 6. The reply ended the send, and the lease died with it. This one is
    //    checked after replying, so it cannot be reported in the return code
    //    and shows up in the exit status instead.
    mov     x8, #SYS_BORROW_READ
    mov     x0, x19
    mov     x1, #0
    mov     x2, #0
    mov     x3, x22
    mov     x4, #4
    svc     #0
    cmp     x0, #0
    b.ne    .Lborrow_exit
    orr     x21, x21, #BAD_STALE

.Lborrow_exit:
    mov     x8, #SYS_EXIT
    mov     x0, x21
    svc     #0
.Lborrow_stuck:
    b       .Lborrow_stuck

.Lexpect_seed:
    .ascii  "seed"
.Lexpect_seed_end:
.Lnew_body:
    .ascii  "written through a lease"
.Lnew_body_end:
.Ldone:
    .ascii  "ok"
.Ldone_end:

    .balign 4
user_borrower_end:

// Supervision: a task that faults, a task watching it, and a bystander.
//
// The kernel's part in a fault ends at "stop this task and say so". What to do
// about it is policy, and policy belongs in a task. The supervisor is the one
// task allowed to ask what happened and to put a broken task back together.

.set SYS_FAULT_WAIT, 9
.set SYS_FAULT_INFO, 10
.set SYS_RESTART,    11

.set EDEAD, -3

// The victim's scratch word, in its own stack page. A restart must not leave
// anything of the old instance here.
.set MARKER_OFFSET, 32

    .section .text.user, "ax"
    .global user_victim_start
    .global user_victim_end

user_victim_start:
    sub     sp, sp, #64
    add     x19, sp, #MARKER_OFFSET

    // Whether this instance is starting on clean memory is decided here, by
    // reading before writing. A restart that handed back the same page with
    // the same contents would be caught by the fault address changing, which
    // the supervisor checks.
    ldr     x0, [x19]
    cbnz    x0, .Lvictim_dirty

    mov     x0, #1
    str     x0, [x19]

    // Fault at address zero.
    mov     x1, xzr
    str     x1, [x1]

.Lvictim_dirty:
    // Fault at address eight instead, so the supervisor can tell "restarted
    // with clean memory" from "restarted into its own leftovers" without
    // either instance being able to report anything itself.
    mov     x1, #8
    str     x1, [x1]

.Lvictim_stuck:
    b       .Lvictim_stuck

    .balign 4
user_victim_end:

    .section .text.user, "ax"
    .global user_supervisor_start
    .global user_supervisor_end

user_supervisor_start:
    ldr     x19, [sp, #8]           // the task we are expected to look after
    mov     x21, #0                 // accumulated failures

    // Wait for the first fault and check it is the one we expected.
    mov     x8, #SYS_FAULT_WAIT
    svc     #0
    mov     x20, x0
    cmp     x20, x19
    b.eq    .Lsup_info_one
    orr     x21, x21, #1

.Lsup_info_one:
    mov     x8, #SYS_FAULT_INFO
    mov     x0, x20
    svc     #0
    // x1 is the faulting address. The first fault is at zero.
    cbz     x1, .Lsup_restart
    orr     x21, x21, #2

.Lsup_restart:
    mov     x8, #SYS_RESTART
    mov     x0, x20
    svc     #0
    cbz     x0, .Lsup_wait_two
    orr     x21, x21, #4

.Lsup_wait_two:
    // The restarted task runs again from the top, so it faults again. Getting
    // here at all is the proof that it ran.
    mov     x8, #SYS_FAULT_WAIT
    svc     #0
    cmp     x0, x20
    b.eq    .Lsup_info_two
    // A different task, which means the restart produced a new identity rather
    // than the same task over again.
    orr     x21, x21, #8

.Lsup_info_two:
    mov     x8, #SYS_FAULT_INFO
    mov     x0, x20
    svc     #0
    // Zero again means the restarted task read zero out of its scratch word,
    // so the memory it came back with was clean.
    cbz     x1, .Lsup_exit
    orr     x21, x21, #16

.Lsup_exit:
    mov     x8, #SYS_EXIT
    mov     x0, x21
    svc     #0
.Lsup_stuck:
    b       .Lsup_stuck

    .balign 4
user_supervisor_end:

    .section .text.user, "ax"
    .global user_petitioner_start
    .global user_petitioner_end

// A task that was in the middle of talking to the victim when it died.
//
// It sends to a task that never receives, so it is still blocked when the
// victim faults. Being told the other end is gone is the only acceptable
// outcome; waiting forever for a reply from a task that no longer exists is
// the failure this exists to rule out.
user_petitioner_start:
    ldr     x19, [sp, #8]
    sub     sp, sp, #64
    mov     x20, sp
    mov     x21, #0

    mov     x8, #SYS_SEND
    mov     x0, x19
    mov     x1, #0x70
    adr     x2, .Lpetition
    mov     x3, #(.Lpetition_end - .Lpetition)
    mov     x4, x20
    mov     x5, #16
    mov     x6, xzr
    mov     x7, xzr
    svc     #0
    mov     x9, #EDEAD
    cmp     x0, x9
    b.eq    .Lpet_privilege
    orr     x21, x21, #1

.Lpet_privilege:
    // Restarting somebody else's task is the supervisor's business, and this
    // task is not the supervisor.
    mov     x8, #SYS_RESTART
    mov     x0, x19
    svc     #0
    mov     x9, #EPERM
    cmp     x0, x9
    b.eq    .Lpet_exit
    orr     x21, x21, #2

.Lpet_exit:
    mov     x8, #SYS_EXIT
    mov     x0, x21
    svc     #0
.Lpet_stuck:
    b       .Lpet_stuck

.Lpetition:
    .ascii  "are you there"
.Lpetition_end:

    .balign 4
user_petitioner_end:
