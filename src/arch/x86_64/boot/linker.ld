PAGE_SIZE = 4K;
PAGE_MASK = PAGE_SIZE - 1;
KERNEL_VMA = 0xffffffff80000000;

ENTRY(_start)
SECTIONS
{
    /DISCARD/ : {
        *(.interp .dynamic)
        *(.dynsym .dynstr .hash .gnu.hash)
    }

    . = KERNEL_VMA;

    ASSERT((. & PAGE_MASK) == 0, "Kernel VA not page aligned")

    __image_start = .;
    __text_start = .;
    .text.boot : {
        KEEP(*(.text.boot._start))
        *(.text.boot)
    }
    .text : { *(.text*) }
    __text_end = .;

    .rodata : ALIGN(8) {
        *(.rodata*)

        . = ALIGN(8);
        __driver_inits_start = .;
        KEEP(*(.driver_inits*))
        __driver_inits_end = .;

        . = ALIGN(8);
        __fixups_start = .;
        KEEP(*(.exception_fixups*))
        __fixups_end = .;
    }

    .data : ALIGN(8) {
        *(.data*)
        *(.got*)
    }

    .percpu : ALIGN(8) {
        __percpu_start = .;
        KEEP(*(.percpu))
        __percpu_end = .;
    }

    .bss : {
        . = ALIGN(16);
        __bss_start = .;
        *(.bss*)
        *(COMMON)
        . = ALIGN(16);
        __bss_end = .;
    }

    . = ALIGN(PAGE_SIZE);
    __init_pages_start = .;
    . += PAGE_SIZE * 64;
    __init_pages_end = .;

    . += PAGE_SIZE;
    __boot_stack = .;
    __image_end = .;

    /DISCARD/ : {
        *(.comment*)
        *(.eh_frame*)
        *(.note*)
    }
}
