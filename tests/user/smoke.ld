/* ELF 头和完整程序头表归入可读可执行段，数据段保留真实页内边缘。 */
ENTRY(_start)

PHDRS
{
    headers PT_PHDR PHDRS FLAGS(4);
    text PT_LOAD FILEHDR PHDRS FLAGS(5);
    data PT_LOAD FLAGS(6);
}

SECTIONS
{
    __image_start = 0x10000;
    __phdr_start = __image_start + 64;
    . = __image_start + SIZEOF_HEADERS;
    .text : { *(.text .text.*) } :text
    .rodata : { *(.rodata .rodata.*) } :text

    /* 页内起点与文件偏移同余；首尾文件页不能按整页字节直接复制。 */
    . = ALIGN(4096) + 0x123;
    .data :
    {
        __data_start = .;
        BYTE(0x3c)
        *(.data .data.*)
        BYTE(0xc3)
        __data_end = .;
    } :data

    /* BSS 紧随文件尾；具体程序决定零区长度，不给没有大BSS的程序补占位内存。 */
    .bss (NOLOAD) :
    {
        __bss_start = .;
        *(.bss .bss.*)
        __bss_end = .;
    } :data

    /DISCARD/ : { *(.comment .note* .eh_frame* .riscv.attributes) }
}

ASSERT((__data_start & 4095) == 0x123, "data boundary changed")
ASSERT((__bss_start & 4095) != 0, "bss start must remain unaligned")
ASSERT((__bss_end & 4095) != 0, "bss end must remain unaligned")
