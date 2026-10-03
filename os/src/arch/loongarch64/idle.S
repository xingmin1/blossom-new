.text

.balign 32
.global la_idle_once_asm
.type la_idle_once_asm, @function
la_idle_once_asm:
    # 回滚重启必须重新准备掩码，csrxchg 会把旧 CRMD 写回 t0。
    ori $t0, $zero, 4
    ori $t1, $zero, 4
    b .Lidle_enable
    .balign 32
.global la_idle_window_start
la_idle_window_start:
.Lidle_enable:
    # 精确 32 字节窗口覆盖启 IE、提示重查和 IDLE；中断返回重启整个函数。
    csrxchg $t0, $t1, 0
    dbar 0
    ld.d $t0, $a0, 0
    bnez $t0, .Lidle_skip
    ld.bu $t0, $a1, 0
    bnez $t0, .Lidle_skip
    nop
    idle 0
.global la_idle_window_end
la_idle_window_end:
.if {execution_tests}
    # 只有 IDLE 的完成路径到达这里；原子旧值、增量和地址使用三个不同寄存器。
    ori $t0, $zero, 1
    amadd_db.d $t1, $t0, $a2
.endif
.Lidle_skip:
    # 返回调度栈之前只关闭 IE，不覆盖其他 CRMD 位，也不修改 FP 或 KS0。
    ori $t1, $zero, 4
    csrxchg $zero, $t1, 0
    jirl $zero, $ra, 0
.size la_idle_once_asm, .-la_idle_once_asm
