/*
 * Layout for the QEMU `virt` machine.
 *
 * The kernel is *linked* at its high half virtual address and *loaded* at its
 * physical one. Every section therefore has a VMA up at KERNEL_BASE and an LMA
 * down in RAM, which is what the AT() directives below express.
 *
 * That split is not cosmetic. PC relative references (`adrp`) fix themselves
 * up automatically: they encode a link-time difference between two addresses,
 * so they resolve to physical addresses while the MMU is off and to high ones
 * afterwards, with no relocation. Absolute pointers stored in static data do
 * not. A vtable entry, or the function pointers `format_args!` builds into
 * .rodata, holds whatever address the linker decided on.
 *
 * Link the kernel low and those pointers stay low forever, so the first
 * `println!` after moving to the high half branches into an address that no
 * longer translates. Link it high and they are right from the start, at the
 * cost that nothing before the MMU comes on may use them.
 *
 * RAM starts at 0x4000_0000. QEMU drops the device tree blob at the base of
 * RAM, so the kernel loads 512 KiB up to leave it alone.
 *
 * Every section boundary is 4 KiB aligned. Permissions are per page, so a
 * boundary falling mid page would force that page to carry the weaker of two
 * permissions: read-only code sharing a page with writable data means writable
 * code, which defeats the point.
 */

KERNEL_BASE = 0xffff000000000000;
LOAD_BASE   = 0x40080000;

/* QEMU starts us at the ELF entry point with the MMU off, so the entry has to
   be the physical address, not the linked one. */
ENTRY(_start_phys)

SECTIONS
{
    . = KERNEL_BASE + LOAD_BASE;

    __text_start = .;
    .text : AT(ADDR(.text) - KERNEL_BASE) {
        KEEP(*(.text.boot))
        *(.text .text.*)
    }
    . = ALIGN(4096);
    __text_end = .;

    __rodata_start = .;
    .rodata : AT(ADDR(.rodata) - KERNEL_BASE) {
        *(.rodata .rodata.*)
        /* Placed explicitly rather than left as orphans, so nothing lands
           between the markers we map with. */
        *(.eh_frame_hdr)
        *(.eh_frame)
    }
    . = ALIGN(4096);
    __rodata_end = .;

    __data_start = .;
    .data : AT(ADDR(.data) - KERNEL_BASE) {
        *(.data .data.*)
    }

    .bss : AT(ADDR(.bss) - KERNEL_BASE) {
        . = ALIGN(16);
        __bss_start = .;
        *(.bss .bss.*)
        *(COMMON)
        . = ALIGN(16);
        __bss_end = .;
    }

    /* Boot stack for core 0. Grows down from __stack_top. */
    . = ALIGN(16);
    . += 0x10000;
    __stack_top = .;

    /* First free byte after the kernel image. The frame allocator hands out
       memory from here. */
    . = ALIGN(4096);
    __kernel_end = .;

    /DISCARD/ : {
        *(.comment)
        *(.note .note.*)
    }
}

_start_phys = _start - KERNEL_BASE;
