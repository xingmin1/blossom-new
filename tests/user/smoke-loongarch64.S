.text

# 直接消费内核交付的初始栈；全部失败经真实 exit 返回不同状态。
.global _start
.type _start, @function
_start:
    li.w $s8, 1
    andi $t0, $sp, 15
    bnez $t0, .Lexit
    li.w $s8, 2
    ld.d $t0, $sp, 0
    li.w $t1, 2
    bne $t0, $t1, .Lexit
    li.w $s8, 3
    ld.d $a0, $sp, 8
    la.local $a1, .Largv0
    bl .Lstring_equal
    beqz $a0, .Lexit
    li.w $s8, 4
    ld.d $a0, $sp, 16
    la.local $a1, .Largv1
    bl .Lstring_equal
    beqz $a0, .Lexit
    li.w $s8, 5
    ld.d $t0, $sp, 24
    bnez $t0, .Lexit
    ld.d $t0, $sp, 40
    bnez $t0, .Lexit
    li.w $s8, 6
    ld.d $a0, $sp, 32
    la.local $a1, .Lenv
    bl .Lstring_equal
    beqz $a0, .Lexit

    # 有界读取 auxv，拒绝重复必需字段和虚构的 vDSO 入口。
    addi.d $s0, $sp, 48
    li.w $s1, 32
    li.w $s2, 0
.Laux_next:
    li.w $s8, 7
    beqz $s1, .Lexit
    addi.d $s1, $s1, -1
    ld.d $t0, $s0, 0
    ld.d $t1, $s0, 8
    addi.d $s0, $s0, 16
    beqz $t0, .Laux_done
    li.w $s8, 9
    li.w $t2, 33
    beq $t0, $t2, .Lexit
    li.w $t2, 16
    beq $t0, $t2, .Lhwcap
    la.local $t2, .Laux_expected
    li.w $t3, 1
    li.w $t4, 13
.Laux_find:
    ld.d $t5, $t2, 0
    beq $t0, $t5, .Laux_match
    addi.d $t2, $t2, 16
    slli.d $t3, $t3, 1
    addi.d $t4, $t4, -1
    bnez $t4, .Laux_find
    b .Laux_next
.Laux_match:
    li.w $s8, 7
    and $t4, $s2, $t3
    bnez $t4, .Lexit
    or $s2, $s2, $t3
    li.w $s8, 8
    ld.d $t4, $t2, 8
    bne $t1, $t4, .Lexit
    b .Laux_next
.Lhwcap:
    li.w $s8, 7
    li.w $t3, 8192
    and $t4, $s2, $t3
    bnez $t4, .Lexit
    or $s2, $s2, $t3
    li.w $s8, 10
    andi $t2, $t1, 8
    beqz $t2, .Lexit
    andi $t2, $t1, 48
    bnez $t2, .Lexit
    b .Laux_next
.Laux_done:
    li.w $s8, 7
    bnez $t1, .Lexit
    li.w $s8, 8
    li.w $t0, 16383
    bne $s2, $t0, .Lexit
    # 从已映射 ELF 头反算程序头地址与数量，核对 auxv 的真实来源。
    la.local $t0, __image_start
    ld.d $t1, $t0, 32
    add.d $t1, $t0, $t1
    la.local $t2, __phdr_start
    bne $t1, $t2, .Lexit
    ld.hu $t1, $t0, 54
    li.w $t3, 56
    bne $t1, $t3, .Lexit
    ld.hu $t1, $t0, 56
    li.w $t3, 3
    bne $t1, $t3, .Lexit
    ld.d $t1, $t0, 24
    la.local $t3, _start
    bne $t1, $t3, .Lexit
    ld.d $t1, $t2, 16
    bne $t1, $t2, .Lexit
    ld.d $t1, $t2, 32
    li.w $t3, 168
    bne $t1, $t3, .Lexit
    ld.w $t1, $t2, 56
    li.w $t3, 1
    bne $t1, $t3, .Lexit
    ld.w $t1, $t2, 60
    li.w $t3, 5
    bne $t1, $t3, .Lexit
    ld.w $t1, $t2, 112
    li.w $t3, 1
    bne $t1, $t3, .Lexit
    ld.w $t1, $t2, 116
    li.w $t3, 6
    bne $t1, $t3, .Lexit

    # 包括两端链接器哨兵和跨页的全部文件字节，防止边缘页漏填。
    li.w $s8, 11
    la.local $t0, __data_start
    ld.bu $t1, $t0, 0
    li.w $t2, 0x3c
    bne $t1, $t2, .Lexit
    la.local $t0, .Ldata_pattern
    li.w $t2, 8192
    li.w $t3, 0x96
.Ldata_check:
    ld.bu $t1, $t0, 0
    bne $t1, $t3, .Lexit
    addi.d $t0, $t0, 1
    addi.d $t2, $t2, -1
    bnez $t2, .Ldata_check
    ld.bu $t1, $t0, 0
    li.w $t2, 0x69
    bne $t1, $t2, .Lexit
    la.local $t0, __data_end
    ld.bu $t1, $t0, -1
    li.w $t2, 0xc3
    bne $t1, $t2, .Lexit

    # 先核整个 BSS 的零值，再逐字节写入地址图样并完整读回。
    li.w $s8, 12
    la.local $s0, __bss_start
    la.local $s1, __bss_end
    move $t0, $s0
.Lbss_zero:
    ld.bu $t1, $t0, 0
    bnez $t1, .Lexit
    addi.d $t0, $t0, 1
    bltu $t0, $s1, .Lbss_zero
    move $t0, $s0
.Lbss_write:
    xori $t1, $t0, 0x5a
    st.b $t1, $t0, 0
    addi.d $t0, $t0, 1
    bltu $t0, $s1, .Lbss_write
    li.w $s8, 13
    move $t0, $s0
.Lbss_read:
    xori $t2, $t0, 0x5a
    andi $t2, $t2, 255
    ld.bu $t1, $t0, 0
    bne $t1, $t2, .Lexit
    addi.d $t0, $t0, 1
    bltu $t0, $s1, .Lbss_read

    li.w $s8, 14
    li.w $a7, 172
    syscall 0
    bge $zero, $a0, .Lexit
    move $s3, $a0
    li.w $s8, 15
    li.w $a7, 178
    syscall 0
    bge $zero, $a0, .Lexit
    bne $a0, $s3, .Lexit
    li.w $s8, 16
    li.w $a7, 124
    syscall 0
    bnez $a0, .Lexit
    li.w $s8, 17
    li.w $a7, 0x7fffffff
    syscall 0
    li.w $t0, -38
    bne $a0, $t0, .Lexit

    # 有限纯用户计算留出真实 timer 中断窗口，不依赖内核测试开关。
    li.w $s4, 0x40000000
.Lbusy:
    addi.d $s4, $s4, -1
    bnez $s4, .Lbusy
    li.w $s8, 14
    li.w $a7, 172
    syscall 0
    bne $a0, $s3, .Lexit
    li.w $s8, 0
.Lexit:
    move $a0, $s8
    li.w $a7, 93
    syscall 0
    # exit 不得返回；返回即非法指令，不隐藏为忙等成功。
    .word 0
.size _start, .-_start

# 比较含结尾 NUL 的完整字符串；不修改初始 SP 或任何保存寄存器。
.type .Lstring_equal, @function
.Lstring_equal:
    beqz $a0, .Lstring_bad
.Lstring_next:
    ld.bu $t0, $a0, 0
    ld.bu $t1, $a1, 0
    bne $t0, $t1, .Lstring_bad
    beqz $t1, .Lstring_ok
    addi.d $a0, $a0, 1
    addi.d $a1, $a1, 1
    b .Lstring_next
.Lstring_ok:
    li.w $a0, 1
    jr $ra
.Lstring_bad:
    li.w $a0, 0
    jr $ra
.size .Lstring_equal, .-.Lstring_equal

.section .rodata, "a", @progbits

.Largv0: .asciz "/test/bin/user-smoke"
.Largv1: .asciz "raw"
.Lenv: .asciz "USER_SMOKE=1"
.balign 8
# 与静态 ELF 的真实头表、入口及 root 身份逐项核对。
.Laux_expected:
    .quad 3, __phdr_start
    .quad 4, 56
    .quad 5, 3
    .quad 6, 4096
    .quad 7, 0
    .quad 8, 0
    .quad 9, _start
    .quad 11, 0
    .quad 12, 0
    .quad 13, 0
    .quad 14, 0
    .quad 17, 100
    .quad 23, 0

.data

.Ldata_pattern:
    .fill 8192, 1, 0x96
    .byte 0x69

.section .bss, "aw", @nobits

    .skip 12293
