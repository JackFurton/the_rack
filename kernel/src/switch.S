// Context switching between kernel tasks.
//
// The surprising thing about a cooperative switch is how little it has to
// save. Under AAPCS64 the compiler already treats a function call as
// destroying x0 through x18, so at the moment `switch` is called every value
// the outgoing task still cares about is either in a callee-saved register or
// already on its stack. Saving the callee-saved set and swapping stack
// pointers is therefore the whole job.
//
// That is only true of a *voluntary* switch. Preemption from an interrupt gets
// no such promise, because the interrupted code never agreed to a call
// boundary, which is why the trap frame in vectors.S saves everything.

// 10 registers (x19..x28) plus x29 and x30, at 8 bytes each.
.set SWITCH_FRAME_SIZE, 96

    .section .text

// fn switch(save_sp_to: *mut u64, resume_sp: u64)
//
// x0 points at the outgoing task's saved stack pointer field.
// x1 is the incoming task's saved stack pointer.
//
// Returns, from the caller's point of view, once something switches back.
// Between those two instants an arbitrary amount of other work happens on
// other stacks.
    .global switch
switch:
    sub     sp, sp, #SWITCH_FRAME_SIZE
    stp     x19, x20, [sp, #0x00]
    stp     x21, x22, [sp, #0x10]
    stp     x23, x24, [sp, #0x20]
    stp     x25, x26, [sp, #0x30]
    stp     x27, x28, [sp, #0x40]
    // x30 is the address we will resume at. Saving it here is what makes the
    // `ret` at the bottom return into whichever task we are switching *to*.
    stp     x29, x30, [sp, #0x50]

    mov     x2, sp
    str     x2, [x0]

    mov     sp, x1

    ldp     x19, x20, [sp, #0x00]
    ldp     x21, x22, [sp, #0x10]
    ldp     x23, x24, [sp, #0x20]
    ldp     x25, x26, [sp, #0x30]
    ldp     x27, x28, [sp, #0x40]
    ldp     x29, x30, [sp, #0x50]
    add     sp, sp, #SWITCH_FRAME_SIZE

    ret

// Where a task that has never run begins.
//
// `Task::new` fakes a switch frame on the new stack with x30 pointing here,
// so the first `ret` inside `switch` lands on this label with x19 and x20
// holding whatever we planted in their saved slots.
    .global task_trampoline
task_trampoline:
    // Start with interrupts on.
    //
    // A task that has never run has no saved interrupt state to restore,
    // because it has never been through the second half of `switch`. Without
    // this it inherits whatever mask the task that created it happened to be
    // holding, and `yield_now` always switches with interrupts masked. The
    // result is a task that runs forever and can never be preempted, which
    // looks exactly like a scheduler that works until you count the switches.
    msr     daifclr, #0b0011

    mov     x0, x20         // the task's argument
    blr     x19             // the task's entry point

    // A task that returns has finished. This never comes back.
    bl      task_finished
    brk     #0
