.text
.option norelax
.option norvc

# /// 预留内核观察的初始SP-16双槽，之后所有C栈帧都在其下方。
.global probe_main
.type probe_main, @function
probe_main:
    addi sp, sp, -32
    tail signal_main
.size probe_main, .-probe_main

# /// 精确捕获handler入口SP，C业务返回后仍使用内核提供的RA trampoline。
.global signal_handler
.type signal_handler, @function
signal_handler:
    mv a3, sp
    addi sp, sp, -16
    sd ra, 0(sp)
    call signal_handle
    ld ra, 0(sp)
    addi sp, sp, 16
    ret
.size signal_handler, .-signal_handler

# /// 逐个访问32个硬件FP寄存器，包含C ABI caller-saved寄存器。
.macro FP_LOAD
    .irp n,0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31
        fld f\n, \n*8(t1)
    .endr
.endm
.macro FP_STORE
    .irp n,0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31
        fsd f\n, \n*8(t1)
    .endr
.endm

# /// 共同C帧业务结束后才破坏硬件现场，C epilogue不能恢复callee-saved FP。
.global signal_edit_handler
.type signal_edit_handler, @function
signal_edit_handler:
    mv a3, sp
    addi sp, sp, -16
    sd ra, 0(sp)
    call signal_handle
    lla t1, signal_fp_clobber
    FP_LOAD
    csrw fcsr, zero
    li tp, 0
    ld ra, 0(sp)
    addi sp, sp, 16
    ret
.size signal_edit_handler, .-signal_edit_handler

# /// 查询固定D扩展现场宽度，不触及被测寄存器。
.global signal_fp_width
.type signal_fp_width, @function
signal_fp_width:
    li a0, 1
    ret
.size signal_fp_width, .-signal_fp_width

# /// 装载后直接发送信号，寄存器在任何C调用之前被内核保存。
.global signal_frame_enter
.type signal_frame_enter, @function
signal_frame_enter:
    lla t1, signal_fp_values
    FP_LOAD
    li t0, 0x20
    csrw fcsr, t0
    li tp, 0x12345000
    li a1, 10
    li a7, 129
    ecall
    beqz a0, 1f
    li a0, 55
    tail probe_failure
1:
    li a0, 56
    tail probe_failure
.size signal_frame_enter, .-signal_frame_enter

# /// 任意PC/SP恢复后立即捕获硬件状态，再交给C比较；不先经过函数prologue。
.global signal_edited_pc
.type signal_edited_pc, @function
signal_edited_pc:
    lla t1, signal_new_stack
    li t0, 16384
    add t1, t1, t0
    bne sp, t1, .Lbad_sp
    li t0, 0x12345000
    bne tp, t0, .Lbad_tp
    li a7, 172
    ecall
    lla t0, signal_pid
    ld t0, 0(t0)
    bne a0, t0, .Lbad_pid
    lla t1, signal_fp_output
    FP_STORE
    csrr t0, fcsr
    li t1, 0x20
    bne t0, t1, .Lbad_fcsr
    li a0, 59
    call signal_fp_compare
    tail signal_frame_done
.Lbad_sp:
    li a0, 57
    tail probe_failure
.Lbad_tp:
    li a0, 58
    tail probe_failure
.Lbad_pid:
    li a0, 221
    tail probe_failure
.Lbad_fcsr:
    li a0, 60
    tail probe_failure
.size signal_edited_pc, .-signal_edited_pc

# /// 改为坏SP后直接syscall，不能创建任何C栈帧。
.global signal_bad_stack
.type signal_bad_stack, @function
signal_bad_stack:
    li sp, 16
    li a1, 10
    li a7, 129
    ecall
    bnez a0, 1f
    li a0, 74
    j 2f
1:
    li a0, 73
2:
    li a7, 93
    ecall
    .word 0
.size signal_bad_stack, .-signal_bad_stack

# /// 指令前写入真实PC和恢复PC，缺页异常不得推进发生PC。
.global signal_fault_load
.type signal_fault_load, @function
signal_fault_load:
    lla t0, signal_fault_pc
    lla t1, .Lload_pc
    sd t1, 0(t0)
    lla t0, signal_resume_pc
    lla t1, .Lload_resume
    sd t1, 0(t0)
.Lload_pc:
    ld t2, 0(a0)
    mv a0, a1
    tail probe_failure
.Lload_resume:
    ret
.size signal_fault_load, .-signal_fault_load

# /// 对同一已触页RO页执行真实store，保留独立故障指令地址。
.global signal_fault_store
.type signal_fault_store, @function
signal_fault_store:
    lla t0, signal_fault_pc
    lla t1, .Lstore_pc
    sd t1, 0(t0)
    lla t0, signal_resume_pc
    lla t1, .Lstore_resume
    sd t1, 0(t0)
.Lstore_pc:
    sd zero, 0(a0)
    mv a0, a1
    tail probe_failure
.Lstore_resume:
    ret
.size signal_fault_store, .-signal_fault_store

# /// 非法指令的si_addr必须是该条指令本身，而非下一条PC。
.global signal_fault_illegal
.type signal_fault_illegal, @function
signal_fault_illegal:
    lla t0, signal_fault_pc
    lla t1, .Lillegal_pc
    sd t1, 0(t0)
    lla t0, signal_fault_address
    sd t1, 0(t0)
    lla t0, signal_resume_pc
    lla t1, .Lillegal_resume
    sd t1, 0(t0)
.Lillegal_pc:
    .word 0
    tail probe_failure
.Lillegal_resume:
    ret
.size signal_fault_illegal, .-signal_fault_illegal

# /// 原始栈标记后不再syscall，同CPU控制者只能通过真实timer抢占取得进展。
.global signal_infinite
.type signal_infinite, @function
signal_infinite:
    li t0, 0x51a1
    sd t0, -8(a0)
    fence rw, rw
1:
    addi t1, t1, 1
    xor t2, t1, t2
    j 1b
.size signal_infinite, .-signal_infinite
