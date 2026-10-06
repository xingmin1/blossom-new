.text

# 本次普通 Rust 调用的 160 字节续接帧，保持 16 字节 ABI 对齐。
.balign 64
.global la_run_user
la_run_user:
    addi.d $sp, $sp, -160
    st.d $r1, $sp, 0
    st.d $r2, $sp, 8
    st.d $r21, $sp, 16
    st.d $r22, $sp, 24
    st.d $r23, $sp, 32
    st.d $r24, $sp, 40
    st.d $r25, $sp, 48
    st.d $r26, $sp, 56
    st.d $r27, $sp, 64
    st.d $r28, $sp, 72
    st.d $r29, $sp, 80
    st.d $r30, $sp, 88
    st.d $r31, $sp, 96
    csrrd $t0, 51
    st.d $t0, $sp, 104
    csrrd $t0, 12
    st.d $t0, $sp, 112
    csrrd $t0, 1
    st.d $t0, $sp, 120
    csrrd $t0, 6
    st.d $t0, $sp, 128
    csrrd $t0, 49
    st.d $t0, $sp, 136
    csrrd $t0, 50
    st.d $t0, $sp, 144
    csrrd $t0, 0
    st.d $t0, $sp, 152
    move $t0, $sp
    st.d $t0, $a0, {kernel_sp}
    # KS3 保持受信 Task 续接 SP，覆盖入口及退出期间所有 context/用户 SP 窗口。
    move $t0, $sp
    csrwr $t0, 51
    move $t0, $a0
    csrwr $t0, 49
    la.abs $t0, la_user_trap
    csrwr $t0, 12
    move $sp, $a0
    li.w $t0, 1
    csrwr $t0, 2
    fld.d $f0, $sp, {fpr}+0*8
    fld.d $f1, $sp, {fpr}+1*8
    fld.d $f2, $sp, {fpr}+2*8
    fld.d $f3, $sp, {fpr}+3*8
    fld.d $f4, $sp, {fpr}+4*8
    fld.d $f5, $sp, {fpr}+5*8
    fld.d $f6, $sp, {fpr}+6*8
    fld.d $f7, $sp, {fpr}+7*8
    fld.d $f8, $sp, {fpr}+8*8
    fld.d $f9, $sp, {fpr}+9*8
    fld.d $f10, $sp, {fpr}+10*8
    fld.d $f11, $sp, {fpr}+11*8
    fld.d $f12, $sp, {fpr}+12*8
    fld.d $f13, $sp, {fpr}+13*8
    fld.d $f14, $sp, {fpr}+14*8
    fld.d $f15, $sp, {fpr}+15*8
    fld.d $f16, $sp, {fpr}+16*8
    fld.d $f17, $sp, {fpr}+17*8
    fld.d $f18, $sp, {fpr}+18*8
    fld.d $f19, $sp, {fpr}+19*8
    fld.d $f20, $sp, {fpr}+20*8
    fld.d $f21, $sp, {fpr}+21*8
    fld.d $f22, $sp, {fpr}+22*8
    fld.d $f23, $sp, {fpr}+23*8
    fld.d $f24, $sp, {fpr}+24*8
    fld.d $f25, $sp, {fpr}+25*8
    fld.d $f26, $sp, {fpr}+26*8
    fld.d $f27, $sp, {fpr}+27*8
    fld.d $f28, $sp, {fpr}+28*8
    fld.d $f29, $sp, {fpr}+29*8
    fld.d $f30, $sp, {fpr}+30*8
    fld.d $f31, $sp, {fpr}+31*8
    ld.d $t0, $sp, {fcsr}
    movgr2fcsr $fcsr0, $t0
    ld.d $t0, $sp, {fcc}
    bstrpick.d $t1, $t0, 7, 0
    movgr2cf $fcc0, $t1
    bstrpick.d $t1, $t0, 15, 8
    movgr2cf $fcc1, $t1
    bstrpick.d $t1, $t0, 23, 16
    movgr2cf $fcc2, $t1
    bstrpick.d $t1, $t0, 31, 24
    movgr2cf $fcc3, $t1
    bstrpick.d $t1, $t0, 39, 32
    movgr2cf $fcc4, $t1
    bstrpick.d $t1, $t0, 47, 40
    movgr2cf $fcc5, $t1
    bstrpick.d $t1, $t0, 55, 48
    movgr2cf $fcc6, $t1
    bstrpick.d $t1, $t0, 63, 56
    movgr2cf $fcc7, $t1
    ld.d $t0, $sp, {pc}
    csrwr $t0, 6
    # 特权和 IRQ 状态只由内核常量构造，用户寄存器不参与 PRMD。
    li.w $t0, 7
    csrwr $t0, 1
    ld.d $r1, $sp, {gpr}+1*8
    ld.d $r2, $sp, {gpr}+2*8
    ld.d $r4, $sp, {gpr}+4*8
    ld.d $r5, $sp, {gpr}+5*8
    ld.d $r6, $sp, {gpr}+6*8
    ld.d $r7, $sp, {gpr}+7*8
    ld.d $r8, $sp, {gpr}+8*8
    ld.d $r9, $sp, {gpr}+9*8
    ld.d $r10, $sp, {gpr}+10*8
    ld.d $r11, $sp, {gpr}+11*8
    ld.d $r12, $sp, {gpr}+12*8
    ld.d $r13, $sp, {gpr}+13*8
    ld.d $r14, $sp, {gpr}+14*8
    ld.d $r15, $sp, {gpr}+15*8
    ld.d $r16, $sp, {gpr}+16*8
    ld.d $r17, $sp, {gpr}+17*8
    ld.d $r18, $sp, {gpr}+18*8
    ld.d $r19, $sp, {gpr}+19*8
    ld.d $r20, $sp, {gpr}+20*8
    ld.d $r21, $sp, {gpr}+21*8
    ld.d $r22, $sp, {gpr}+22*8
    ld.d $r23, $sp, {gpr}+23*8
    ld.d $r24, $sp, {gpr}+24*8
    ld.d $r25, $sp, {gpr}+25*8
    ld.d $r26, $sp, {gpr}+26*8
    ld.d $r27, $sp, {gpr}+27*8
    ld.d $r28, $sp, {gpr}+28*8
    ld.d $r29, $sp, {gpr}+29*8
    ld.d $r30, $sp, {gpr}+30*8
    ld.d $r31, $sp, {gpr}+31*8
    ld.d $sp, $sp, {gpr}+3*8
    ertn

.balign 64
.global la_user_trap
la_user_trap:
    # KS0 永久保留 CpuLocal；KS2 只在入口暂存用户 t0。
    csrwr $t0, 50
    csrrd $t0, 1
    andi $t0, $t0, 3
    bnez $t0, .Luser_source
    b .Luser_kernel_fault
.Luser_source:
    csrwr $sp, 49
    st.d $r0, $sp, {gpr}+0*8
    st.d $r1, $sp, {gpr}+1*8
    st.d $r2, $sp, {gpr}+2*8
    st.d $r4, $sp, {gpr}+4*8
    st.d $r5, $sp, {gpr}+5*8
    st.d $r6, $sp, {gpr}+6*8
    st.d $r7, $sp, {gpr}+7*8
    st.d $r8, $sp, {gpr}+8*8
    st.d $r9, $sp, {gpr}+9*8
    st.d $r10, $sp, {gpr}+10*8
    st.d $r11, $sp, {gpr}+11*8
    st.d $r13, $sp, {gpr}+13*8
    st.d $r14, $sp, {gpr}+14*8
    st.d $r15, $sp, {gpr}+15*8
    st.d $r16, $sp, {gpr}+16*8
    st.d $r17, $sp, {gpr}+17*8
    st.d $r18, $sp, {gpr}+18*8
    st.d $r19, $sp, {gpr}+19*8
    st.d $r20, $sp, {gpr}+20*8
    st.d $r21, $sp, {gpr}+21*8
    st.d $r22, $sp, {gpr}+22*8
    st.d $r23, $sp, {gpr}+23*8
    st.d $r24, $sp, {gpr}+24*8
    st.d $r25, $sp, {gpr}+25*8
    st.d $r26, $sp, {gpr}+26*8
    st.d $r27, $sp, {gpr}+27*8
    st.d $r28, $sp, {gpr}+28*8
    st.d $r29, $sp, {gpr}+29*8
    st.d $r30, $sp, {gpr}+30*8
    st.d $r31, $sp, {gpr}+31*8
    csrrd $t0, 50
    st.d $t0, $sp, {gpr}+12*8
    csrrd $t0, 49
    st.d $t0, $sp, {gpr}+3*8
    csrrd $t0, 6
    st.d $t0, $sp, {pc}
    csrrd $t0, 5
    st.d $t0, $sp, {estat}
    csrrd $t0, 7
    st.d $t0, $sp, {badv}
    fst.d $f0, $sp, {fpr}+0*8
    fst.d $f1, $sp, {fpr}+1*8
    fst.d $f2, $sp, {fpr}+2*8
    fst.d $f3, $sp, {fpr}+3*8
    fst.d $f4, $sp, {fpr}+4*8
    fst.d $f5, $sp, {fpr}+5*8
    fst.d $f6, $sp, {fpr}+6*8
    fst.d $f7, $sp, {fpr}+7*8
    fst.d $f8, $sp, {fpr}+8*8
    fst.d $f9, $sp, {fpr}+9*8
    fst.d $f10, $sp, {fpr}+10*8
    fst.d $f11, $sp, {fpr}+11*8
    fst.d $f12, $sp, {fpr}+12*8
    fst.d $f13, $sp, {fpr}+13*8
    fst.d $f14, $sp, {fpr}+14*8
    fst.d $f15, $sp, {fpr}+15*8
    fst.d $f16, $sp, {fpr}+16*8
    fst.d $f17, $sp, {fpr}+17*8
    fst.d $f18, $sp, {fpr}+18*8
    fst.d $f19, $sp, {fpr}+19*8
    fst.d $f20, $sp, {fpr}+20*8
    fst.d $f21, $sp, {fpr}+21*8
    fst.d $f22, $sp, {fpr}+22*8
    fst.d $f23, $sp, {fpr}+23*8
    fst.d $f24, $sp, {fpr}+24*8
    fst.d $f25, $sp, {fpr}+25*8
    fst.d $f26, $sp, {fpr}+26*8
    fst.d $f27, $sp, {fpr}+27*8
    fst.d $f28, $sp, {fpr}+28*8
    fst.d $f29, $sp, {fpr}+29*8
    fst.d $f30, $sp, {fpr}+30*8
    fst.d $f31, $sp, {fpr}+31*8
    movfcsr2gr $t0, $fcsr0
    st.d $t0, $sp, {fcsr}
    movcf2gr $t0, $fcc0
    movcf2gr $t1, $fcc1
    bstrins.d $t0, $t1, 15, 8
    movcf2gr $t1, $fcc2
    bstrins.d $t0, $t1, 23, 16
    movcf2gr $t1, $fcc3
    bstrins.d $t0, $t1, 31, 24
    movcf2gr $t1, $fcc4
    bstrins.d $t0, $t1, 39, 32
    movcf2gr $t1, $fcc5
    bstrins.d $t0, $t1, 47, 40
    movcf2gr $t1, $fcc6
    bstrins.d $t0, $t1, 55, 48
    movcf2gr $t1, $fcc7
    bstrins.d $t0, $t1, 63, 56
    st.d $t0, $sp, {fcc}
    csrwr $zero, 2
    ld.d $sp, $sp, {kernel_sp}
    ld.d $tp, $sp, 8
    ld.d $r21, $sp, 16
    ld.d $t0, $sp, 112
    csrwr $t0, 12
    ld.d $t0, $sp, 104
    csrwr $t0, 51
    ld.d $t0, $sp, 120
    csrwr $t0, 1
    ld.d $t0, $sp, 128
    csrwr $t0, 6
    ld.d $t0, $sp, 136
    csrwr $t0, 49
    ld.d $t0, $sp, 144
    csrwr $t0, 50
    ld.d $t0, $sp, 152
    csrwr $t0, 0
    ld.d $r1, $sp, 0
    ld.d $r2, $sp, 8
    ld.d $r21, $sp, 16
    ld.d $r22, $sp, 24
    ld.d $r23, $sp, 32
    ld.d $r24, $sp, 40
    ld.d $r25, $sp, 48
    ld.d $r26, $sp, 56
    ld.d $r27, $sp, 64
    ld.d $r28, $sp, 72
    ld.d $r29, $sp, 80
    ld.d $r30, $sp, 88
    ld.d $r31, $sp, 96
    addi.d $sp, $sp, 160
    jr $ra

.Luser_kernel_fault:
    # PLV0 入口故障不使用当前不可信 SP 压帧，独立快照保留真正故障 CSR 和故障 SP。
    move $a1, $sp
    csrrd $a0, 6
    csrrd $a2, 5
    csrrd $a3, 7
    move $a4, $zero
    b .Luser_kernel_fault_restore

.global la_user_kernel_refill_fault
la_user_kernel_refill_fault:
    # refill 保留专属 CSR 语义；DA 下只跳转高地址别名，再恢复 PLV0 DMW。
    move $a1, $sp
    csrrd $a0, 138
    andi $a2, $a0, 1
    srli.d $a0, $a0, 1
    slli.d $a0, $a0, 1
    csrrd $a3, 137
    li.w $a4, 1
    la.abs $t0, .Luser_kernel_refill_high
    jirl $zero, $t0, 0
.Luser_kernel_refill_high:
    li.w $t0, 0x10
    li.w $t1, 0x18
    csrxchg $t0, $t1, 0
.Luser_kernel_fault_restore:
    csrrd $sp, 51
    ld.d $tp, $sp, 8
    ld.d $r21, $sp, 16
    csrwr $zero, 2
    ld.d $t0, $sp, 112
    csrwr $t0, 12
    ld.d $t0, $sp, 120
    csrwr $t0, 1
    ld.d $t0, $sp, 128
    csrwr $t0, 6
    ld.d $t0, $sp, 136
    csrwr $t0, 49
    ld.d $t0, $sp, 144
    csrwr $t0, 50
    ld.d $t0, $sp, 104
    csrwr $t0, 51
    ld.d $t0, $sp, 152
    csrwr $t0, 0
    bl loongarch_user_kernel_fault

.if {execution_tests}
.section .text.la_user_probe, "ax"
.balign 4096
.global la_user_code_start
.global la_user_code_end
.global la_user_loop_entry
.global la_user_loop_spin
.global la_user_exit_entry
.global la_user_fault_read
.global la_user_fault_write
.global la_user_fault_execute
la_user_code_start:
    # 页内仅用相对分支和寄存器地址，复制后不依赖原内核虚拟地址。
    st.d $sp, $a6, 0
    st.d $tp, $a6, 8
    st.d $r21, $a6, 16
    addi.d $sp, $sp, -16
    st.d $tp, $sp, 0
    ld.d $t0, $sp, 0
    addi.d $sp, $sp, 16
    st.d $t0, $a6, 40
    fadd.d $f2, $f0, $f1
    fst.d $f2, $a6, 24
    movfcsr2gr $t0, $fcsr0
    st.d $t0, $a6, 32
    fcmp.clt.d $fcc3, $f0, $f1
    movcf2gr $t0, $fcc3
    st.d $t0, $a6, 48
    fcmp.clt.d $fcc7, $f1, $f0
    movcf2gr $t0, $fcc7
    st.d $t0, $a6, 56
    li.w $a7, 93
    li.w $a0, 41
    syscall 0
    li.w $a0, 42
    syscall 0
.Lla_probe_completed:
    b .Lla_probe_completed

la_user_loop_entry:
    st.d $sp, $a6, 0
    st.d $tp, $a6, 8
    st.d $r21, $a6, 16
    addi.d $sp, $sp, -16
    st.d $tp, $sp, 0
    ld.d $t0, $sp, 0
    addi.d $sp, $sp, 16
    st.d $t0, $a6, 40
    fadd.d $f2, $f0, $f1
    fst.d $f2, $a6, 24
    movfcsr2gr $t0, $fcsr0
    st.d $t0, $a6, 32
    fcmp.clt.d $fcc3, $f0, $f1
    movcf2gr $t0, $fcc3
    st.d $t0, $a6, 48
    fcmp.clt.d $fcc7, $f1, $f0
    movcf2gr $t0, $fcc7
    st.d $t0, $a6, 56
la_user_loop_spin:
    b la_user_loop_spin

la_user_exit_entry:
    li.w $a7, 93
    li.w $a0, 43
    syscall 0
    b .Lla_probe_completed

la_user_fault_read:
    ld.d $t0, $a5, 0
    b la_user_exit_entry
la_user_fault_write:
    # 先使同一只读页进入 TLB，再执行 store 触发 modify 而非仅观察首次 refill。
    ld.d $t0, $a5, 0
    st.d $zero, $a5, 0
    b la_user_exit_entry
la_user_fault_execute:
    jirl $zero, $a5, 0
la_user_code_end:
.endif
