.text
.option norelax
.option norvc

# 直接消费内核交付的初始栈；全部失败经真实 exit 返回不同状态。
.global _start
.type _start, @function
_start:
    li s11, 1
    andi t0, sp, 15
    bnez t0, .Lexit
    li s11, 2
    ld t0, 0(sp)
    li t1, 2
    bne t0, t1, .Lexit
    li s11, 3
    ld a0, 8(sp)
    lla a1, .Largv0
    call .Lstring_equal
    beqz a0, .Lexit
    li s11, 4
    ld a0, 16(sp)
    lla a1, .Largv1
    call .Lstring_equal
    beqz a0, .Lexit
    li s11, 5
    ld t0, 24(sp)
    bnez t0, .Lexit
    ld t0, 40(sp)
    bnez t0, .Lexit
    li s11, 6
    ld a0, 32(sp)
    lla a1, .Lenv
    call .Lstring_equal
    beqz a0, .Lexit

    # 有界读取 auxv，拒绝重复必需字段和虚构的 vDSO 入口。
    addi s0, sp, 48
    li s1, 32
    li s2, 0
.Laux_next:
    li s11, 7
    beqz s1, .Lexit
    addi s1, s1, -1
    ld t0, 0(s0)
    ld t1, 8(s0)
    addi s0, s0, 16
    beqz t0, .Laux_done
    li s11, 9
    li t2, 33
    beq t0, t2, .Lexit
    li t2, 16
    beq t0, t2, .Lhwcap
    lla t2, .Laux_expected
    li t3, 1
    li t4, 13
.Laux_find:
    ld t5, 0(t2)
    beq t0, t5, .Laux_match
    addi t2, t2, 16
    slli t3, t3, 1
    addi t4, t4, -1
    bnez t4, .Laux_find
    j .Laux_next
.Laux_match:
    li s11, 7
    and t4, s2, t3
    bnez t4, .Lexit
    or s2, s2, t3
    li s11, 8
    ld t4, 8(t2)
    bne t1, t4, .Lexit
    j .Laux_next
.Lhwcap:
    li s11, 7
    li t3, 8192
    and t4, s2, t3
    bnez t4, .Lexit
    or s2, s2, t3
    li s11, 10
    andi t2, t1, 0x28
    li t3, 0x28
    bne t2, t3, .Lexit
    li t2, 0x200000
    and t2, t1, t2
    bnez t2, .Lexit
    j .Laux_next
.Laux_done:
    li s11, 7
    bnez t1, .Lexit
    li s11, 8
    li t0, 16383
    bne s2, t0, .Lexit
    # 从已映射 ELF 头反算程序头地址与数量，核对 auxv 的真实来源。
    lla t0, __image_start
    ld t1, 32(t0)
    add t1, t0, t1
    lla t2, __phdr_start
    bne t1, t2, .Lexit
    lhu t1, 54(t0)
    li t3, 56
    bne t1, t3, .Lexit
    lhu t1, 56(t0)
    li t3, 3
    bne t1, t3, .Lexit
    ld t1, 24(t0)
    lla t3, _start
    bne t1, t3, .Lexit
    ld t1, 16(t2)
    bne t1, t2, .Lexit
    ld t1, 32(t2)
    li t3, 168
    bne t1, t3, .Lexit
    lw t1, 56(t2)
    li t3, 1
    bne t1, t3, .Lexit
    lw t1, 60(t2)
    li t3, 5
    bne t1, t3, .Lexit
    lw t1, 112(t2)
    li t3, 1
    bne t1, t3, .Lexit
    lw t1, 116(t2)
    li t3, 6
    bne t1, t3, .Lexit

    # 包括两端链接器哨兵和跨页的全部文件字节，防止边缘页漏填。
    li s11, 11
    lla t0, __data_start
    lbu t1, 0(t0)
    li t2, 0x3c
    bne t1, t2, .Lexit
    lla t0, .Ldata_pattern
    li t2, 8192
    li t3, 0x96
.Ldata_check:
    lbu t1, 0(t0)
    bne t1, t3, .Lexit
    addi t0, t0, 1
    addi t2, t2, -1
    bnez t2, .Ldata_check
    lbu t1, 0(t0)
    li t2, 0x69
    bne t1, t2, .Lexit
    lla t0, __data_end
    lbu t1, -1(t0)
    li t2, 0xc3
    bne t1, t2, .Lexit

    # 先核整个 BSS 的零值，再逐字节写入地址图样并完整读回。
    li s11, 12
    lla s0, __bss_start
    lla s1, __bss_end
    mv t0, s0
.Lbss_zero:
    lbu t1, 0(t0)
    bnez t1, .Lexit
    addi t0, t0, 1
    bltu t0, s1, .Lbss_zero
    mv t0, s0
.Lbss_write:
    xori t1, t0, 0x5a
    sb t1, 0(t0)
    addi t0, t0, 1
    bltu t0, s1, .Lbss_write
    li s11, 13
    mv t0, s0
.Lbss_read:
    xori t2, t0, 0x5a
    andi t2, t2, 255
    lbu t1, 0(t0)
    bne t1, t2, .Lexit
    addi t0, t0, 1
    bltu t0, s1, .Lbss_read

    li s11, 14
    li a7, 172
    ecall
    blez a0, .Lexit
    mv s3, a0
    li s11, 15
    li a7, 178
    ecall
    blez a0, .Lexit
    bne a0, s3, .Lexit
    li s11, 16
    li a7, 124
    ecall
    bnez a0, .Lexit
    li s11, 17
    li a7, 0x7fffffff
    ecall
    li t0, -38
    bne a0, t0, .Lexit

    # 有限纯用户计算留出真实 timer 中断窗口，不依赖内核测试开关。
    li s4, 0x40000000
.Lbusy:
    addi s4, s4, -1
    bnez s4, .Lbusy
    li s11, 14
    li a7, 172
    ecall
    bne a0, s3, .Lexit
    li s11, 0
.Lexit:
    mv a0, s11
    li a7, 93
    ecall
    # exit 不得返回；返回即非法指令，不隐藏为忙等成功。
    .word 0
.size _start, .-_start

# 比较含结尾 NUL 的完整字符串；不修改初始 SP 或任何保存寄存器。
.type .Lstring_equal, @function
.Lstring_equal:
    beqz a0, .Lstring_bad
.Lstring_next:
    lbu t0, 0(a0)
    lbu t1, 0(a1)
    bne t0, t1, .Lstring_bad
    beqz t1, .Lstring_ok
    addi a0, a0, 1
    addi a1, a1, 1
    j .Lstring_next
.Lstring_ok:
    li a0, 1
    ret
.Lstring_bad:
    li a0, 0
    ret
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
