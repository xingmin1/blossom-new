OUTPUT_ARCH(loongarch)
ENTRY(__primary_entry)
__direct_base = 0x9000000000000000;
__mmio_base = 0x8000000000000000;
__kernel_phys_start = 0x90000000;
PHDRS {
    text PT_LOAD FLAGS(5);
    rodata PT_LOAD FLAGS(4);
    data PT_LOAD FLAGS(6);
}
SECTIONS {
    . = __direct_base + __kernel_phys_start;
    __kernel_virt_start = .;
    .text : AT(ADDR(.text) - __direct_base) {
        KEEP(*(.text.entry))
        *(.text .text.*)
    } :text
    . = ALIGN(4096);
    .rodata : AT(ADDR(.rodata) - __direct_base) { *(.rodata .rodata.*) } :rodata
    . = ALIGN(4096);
    .data : AT(ADDR(.data) - __direct_base) { *(.data .data.*) *(.sdata .sdata.*) } :data
    . = ALIGN(4096);
    __boot_reserved_phys_start = . - __direct_base;
    .boot (NOLOAD) : AT(ADDR(.boot) - __direct_base) {
        KEEP(*(.boot.stacks))
        KEEP(*(.boot.tables))
    } :data
    . = ALIGN(4096);
    __boot_reserved_phys_end = . - __direct_base;
    .bss (NOLOAD) : AT(ADDR(.bss) - __direct_base) {
        __bss_start = .;
        *(.bss .bss.*) *(.sbss .sbss.*) *(COMMON)
        . = ALIGN(8);
        __bss_end = .;
    } :data
    . = ALIGN(4096);
    __kernel_virt_end = .;
    __kernel_phys_end = . - __direct_base;
    __primary_entry_phys = __primary_entry - __direct_base;
    __secondary_entry_phys = __secondary_entry - __direct_base;
    /DISCARD/ : { *(.eh_frame .eh_frame_hdr) }
}
