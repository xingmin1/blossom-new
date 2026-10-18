.text
.option norelax
.option norvc

# /// 在真实 syscall 返回点保存结果并核原始参数，未经过任何 C 调用。
.global futex_restart_wait
.type futex_restart_wait, @function
futex_restart_wait:
    li a0, 0x44000040
    li a1, 128
    li a2, 7
    li a3, 0
    li a4, 0
    li a5, 0
    li a7, 98
    ecall
    li t0, 0x44000000
    fence rw, w
    sw a0, 24(t0)
    li t1, 151
    bnez a0, .Lbad
    li t1, 155
    li t6, 128
    bne a1, t6, .Lbad
    li t1, 156
    li t6, 7
    bne a2, t6, .Lbad
    li t1, 157
    bnez a3, .Lbad
    li t1, 158
    bnez a4, .Lbad
    li t1, 159
    bnez a5, .Lbad
    li t1, 173
    li t6, 98
    bne a7, t6, .Lbad
    ret
.Lbad:
    mv a0, t1
    tail probe_failure
.size futex_restart_wait, .-futex_restart_wait

# /// 信号帧保存现场；普通 ret 使用内核交付的 RA trampoline 恢复原 WAIT。
.global futex_handler
.type futex_handler, @function
futex_handler:
    li t0, 0x44000000
    lw t1, 32(t0)
    fence r, rw
    addi t1, t1, 1
    fence rw, w
    sw t1, 32(t0)
    ret
.size futex_handler, .-futex_handler
.section .note.GNU-stack,"",@progbits
