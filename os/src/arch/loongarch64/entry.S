.section .text.entry,"ax"
.balign 64
.global __primary_entry
__primary_entry:
    # QEMU 的 a0/a1/a2 原样保留；不是 hart/DTB 协议。
    li.w $t0, 4
    csrxchg $zero, $t0, 0
    la.abs $t0, __direct_base
    ori $t0, $t0, 0x11
    csrwr $t0, 0x181
    la.abs $t0, __mmio_base
    ori $t0, $t0, 1
    csrwr $t0, 0x180
    csrwr $zero, 0x182
    csrwr $zero, 0x183
    li.w $t0, 0x10
    li.w $t1, 0x18
    csrxchg $t0, $t1, 0
    la.abs $sp, BOOT_STACKS
    li.d $t0, {stack_size}
    add.d $sp, $sp, $t0
    la.abs $t0, __bss_start
    la.abs $t1, __bss_end
1:
    bgeu $t0, $t1, 2f
    st.d $zero, $t0, 0
    addi.d $t0, $t0, 8
    b 1b
2:
    b primary_main

.balign 64
.global __secondary_entry
__secondary_entry:
    li.w $t0, 4
    csrxchg $zero, $t0, 0
    # 邮箱入口为物理 PC，先跳到高别名，再打开 PG。
    la.abs $t0, .Lsecondary_high
    jirl $zero, $t0, 0
.Lsecondary_high:
    la.abs $t0, __direct_base
    ori $t0, $t0, 0x11
    csrwr $t0, 0x181
    la.abs $t0, __mmio_base
    ori $t0, $t0, 1
    csrwr $t0, 0x180
    csrwr $zero, 0x182
    csrwr $zero, 0x183
    li.w $t0, 0x10
    li.w $t1, 0x18
    csrxchg $t0, $t1, 0
    li.w $t0, 0x1028
    iocsrrd.d $t1, $t0
    la.abs $t0, __direct_base
    or $t1, $t1, $t0
    ld.d $sp, $t1, {boot_stack}
    ld.d $a0, $t1, {boot_local}
    b secondary_main
