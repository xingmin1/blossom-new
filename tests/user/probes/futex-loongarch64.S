.text

# /// syscall 返回后直接核原 a1–a5/a7，C caller-saved 规则不能替代现场断言。
.global futex_restart_wait
.type futex_restart_wait, @function
futex_restart_wait:
    li.d $a0, 0x44000040
    li.d $a1, 128
    li.d $a2, 7
    li.d $a3, 0
    li.d $a4, 0
    li.d $a5, 0
    li.d $a7, 98
    syscall 0
    li.d $t0, 0x44000000
    dbar 0
    st.w $a0, $t0, 24
    li.d $t1, 225
    bnez $a0, .Lbad
    li.d $t1, 230
    li.d $t0, 128
    bne $a1, $t0, .Lbad
    li.d $t1, 231
    li.d $t0, 7
    bne $a2, $t0, .Lbad
    li.d $t1, 232
    bnez $a3, .Lbad
    li.d $t1, 233
    bnez $a4, .Lbad
    li.d $t1, 234
    bnez $a5, .Lbad
    li.d $t1, 235
    li.d $t0, 98
    bne $a7, $t0, .Lbad
    jr $ra
.Lbad:
    move $a0, $t1
    b probe_failure
.size futex_restart_wait, .-futex_restart_wait

# /// 原 LA action 直接 sigreturn；不改 SP，也不套 libc restorer 或 C 栈帧。
.global futex_handler
.type futex_handler, @function
futex_handler:
    li.d $t7, 0x44000000
    ld.w $t8, $t7, 32
    dbar 0
    addi.d $t8, $t8, 1
    dbar 0
    st.w $t8, $t7, 32
    li.d $a7, 139
    syscall 0
    .word 0
.size futex_handler, .-futex_handler
.section .note.GNU-stack,"",@progbits
