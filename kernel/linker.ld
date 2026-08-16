/*
 * Layout for the QEMU `virt` machine.
 *
 * RAM starts at 0x4000_0000. QEMU drops the device tree blob at the base of
 * RAM, so we load the kernel 512 KiB up to leave it alone. We will want that
 * DTB later when we start probing for real devices.
 */

ENTRY(_start)

SECTIONS
{
    . = 0x40080000;

    .text : {
        KEEP(*(.text.boot))
        *(.text .text.*)
    }

    .rodata : ALIGN(8) {
        *(.rodata .rodata.*)
    }

    .data : ALIGN(8) {
        *(.data .data.*)
    }

    .bss : ALIGN(16) {
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

    /* First free byte after the kernel image. The page allocator will start
       handing out memory from here once it exists. */
    __kernel_end = .;

    /DISCARD/ : {
        *(.comment)
        *(.note .note.*)
    }
}
