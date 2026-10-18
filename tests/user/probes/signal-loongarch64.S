.text

# /// 初始SP下32字节留给内核checkpoint，C不会借用该区域保存RA。
.global probe_main
.type probe_main, @function
probe_main:
    addi.d $sp, $sp, -32
    b signal_main
.size probe_main, .-probe_main

# /// 读取实际CPUCfg2，标量、LSX、LASX仅执行当前CPU支持的最宽指令。
.macro FP_WIDTH
    li.d $t0, 2
    cpucfg $t0, $t0
    li.d $t6, 1
    andi $t7, $t0, 0x40
    beqz $t7, .Lwidth_done\@
    li.d $t6, 2
    andi $t7, $t0, 0x80
    beqz $t7, .Lwidth_done\@
    li.d $t6, 4
.Lwidth_done\@:
.endm

# /// 全部32个重叠寄存器按实际宽度装载，不把高lanes降成标量检查。
.macro FP_LOAD
    FP_WIDTH
    li.d $t0, 4
    beq $t6, $t0, .Lload_lasx\@
    li.d $t0, 2
    beq $t6, $t0, .Lload_lsx\@
    .irp n,0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31
        fld.d $f\n, $t1, \n*8
    .endr
    b .Lload_done\@
.Lload_lsx\@:
    .irp n,0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31
        vld $vr\n, $t1, \n*16
    .endr
    b .Lload_done\@
.Lload_lasx\@:
    .irp n,0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31
        xvld $xr\n, $t1, \n*32
    .endr
.Lload_done\@:
.endm

# /// 进入任何C ABI之前保存全部实际lanes，包含caller-saved向量寄存器。
.macro FP_STORE
    FP_WIDTH
    li.d $t0, 4
    beq $t6, $t0, .Lstore_lasx\@
    li.d $t0, 2
    beq $t6, $t0, .Lstore_lsx\@
    .irp n,0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31
        fst.d $f\n, $t1, \n*8
    .endr
    b .Lstore_done\@
.Lstore_lsx\@:
    .irp n,0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31
        vst $vr\n, $t1, \n*16
    .endr
    b .Lstore_done\@
.Lstore_lasx\@:
    .irp n,0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16,17,18,19,20,21,22,23,24,25,26,27,28,29,30,31
        xvst $xr\n, $t1, \n*32
    .endr
.Lstore_done\@:
.endm

# /// 桥只检查硬件边界的整数/控制寄存器，业务帧字节和lanes比较属于共同C。
.macro CHECK reg, value, code
    li.d $t0, \value
    beq \reg, $t0, .Lcheck_ok\@
    li.d $a0, \code
    b probe_failure
.Lcheck_ok\@:
.endm
.macro FCC_INIT
    .irp n,0,1,2,3,4,5,6,7
        li.d $t0, (\n & 1)
        movgr2cf $fcc\n, $t0
    .endr
.endm
.macro FCC_CHECK
    .irp n,0,1,2,3,4,5,6,7
        movcf2gr $t2, $fcc\n
        CHECK $t2, (\n & 1), 60
    .endr
.endm

# /// 保留原handler入口SP及内核RA，栈上的ucontext仍由共同C消费。
.global signal_handler
.type signal_handler, @function
signal_handler:
    move $a3, $sp
    addi.d $sp, $sp, -16
    st.d $ra, $sp, 0
    bl signal_handle
    ld.d $ra, $sp, 0
    addi.d $sp, $sp, 16
    jr $ra
.size signal_handler, .-signal_handler

# /// 先由C检查完整帧，C epilogue之后再独立清零所有FP/向量、FCC、FCSR和TP。
.global signal_edit_handler
.type signal_edit_handler, @function
signal_edit_handler:
    move $a3, $sp
    addi.d $sp, $sp, -16
    st.d $ra, $sp, 0
    bl signal_handle
    la.local $t1, signal_fp_clobber
    FP_LOAD
    movgr2fcsr $fcsr0, $zero
    .irp n,0,1,2,3,4,5,6,7
        movgr2cf $fcc\n, $zero
    .endr
    move $tp, $zero
    ld.d $ra, $sp, 0
    addi.d $sp, $sp, 16
    jr $ra
.size signal_edit_handler, .-signal_edit_handler

# /// 共同C按真实CPU能力核对最宽记录布局。
.global signal_fp_width
.type signal_fp_width, @function
signal_fp_width:
    FP_WIDTH
    move $a0, $t6
    jr $ra
.size signal_fp_width, .-signal_fp_width

# /// 捕获后交C比较，再从已检查的观测值恢复，避免C ABI自然破坏caller-saved现场。
.type signal_capture_check, @function
signal_capture_check:
    addi.d $sp, $sp, -32
    st.d $ra, $sp, 0
    la.local $t1, signal_fp_output
    FP_STORE
    .irp n,0,1,2,3,4,5,6,7
        movcf2gr $t0, $fcc\n
        st.b $t0, $sp, 16+\n
    .endr
    movfcsr2gr $t0, $fcsr0
    st.w $t0, $sp, 24
    bl signal_fp_compare
    la.local $t1, signal_fp_output
    FP_LOAD
    .irp n,0,1,2,3,4,5,6,7
        ld.bu $t0, $sp, 16+\n
        movgr2cf $fcc\n, $t0
    .endr
    ld.w $t0, $sp, 24
    movgr2fcsr $fcsr0, $t0
    ld.d $ra, $sp, 0
    addi.d $sp, $sp, 32
    jr $ra
.size signal_capture_check, .-signal_capture_check

# /// FP现场跨真实计算timer、clock syscall和yield；每个边界先capture再进入C。
.global signal_frame_enter
.type signal_frame_enter, @function
signal_frame_enter:
    move $s0, $a0
    la.local $t1, signal_fp_values
    FP_LOAD
    li.d $t0, 0x100
    movgr2fcsr $fcsr0, $t0
    FCC_INIT
    li.d $tp, 0x12345000
    li.d $a0, 1
    la.local $a1, .Lclock
    li.d $a7, 113
    syscall 0
    CHECK $a0, 0, 59
    la.local $t1, .Lclock
    ld.d $s6, $t1, 0
    li.d $t0, 1000000000
    mul.d $s6, $s6, $t0
    ld.d $t2, $t1, 8
    add.d $s6, $s6, $t2
.Lcompute:
    li.d $t0, 65536
1:
    addi.d $t0, $t0, -1
    bnez $t0, 1b
    li.d $a0, 59
    bl signal_capture_check
    li.d $a0, 1
    la.local $a1, .Lclock
    li.d $a7, 113
    syscall 0
    CHECK $a0, 0, 59
    la.local $t1, .Lclock
    ld.d $t2, $t1, 0
    li.d $t0, 1000000000
    mul.d $t2, $t2, $t0
    ld.d $t3, $t1, 8
    add.d $t2, $t2, $t3
    sub.d $t2, $t2, $s6
    li.d $t0, 50000000
    bltu $t2, $t0, .Lcompute
    li.d $a7, 124
    syscall 0
    li.d $a0, 59
    bl signal_capture_check
    move $a0, $s0
    li.d $a1, 10
    li.d $a7, 129
    syscall 0
    CHECK $a0, 0, 55
    li.d $a0, 56
    b probe_failure
.size signal_frame_enter, .-signal_frame_enter

# /// 合法恢复PC/SP后先核SP/TP/r21，再保存全部现场，任何C prologue都不能先运行。
.global signal_edited_pc
.type signal_edited_pc, @function
signal_edited_pc:
    la.local $t1, signal_new_stack
    li.d $t0, 16384
    add.d $t1, $t1, $t0
    beq $sp, $t1, 1f
    li.d $a0, 57
    b probe_failure
1:
    CHECK $tp, 0x12345000, 58
    CHECK $r21, 0x76543000, 222
    li.d $a7, 172
    syscall 0
    beq $a0, $s0, 2f
    li.d $a0, 221
    b probe_failure
2:
    li.d $a0, 59
    bl signal_capture_check
    movfcsr2gr $t2, $fcsr0
    CHECK $t2, 0x100, 60
    FCC_CHECK
    # 同一clone桥的子回调首先捕获FP，不能在C回调prologue之后观测caller-saved寄存器。
    li.d $a0, 17
    move $a1, $zero
    move $a2, $zero
    move $a3, $zero
    move $a4, $zero
    la.local $a5, .Lfp_child
    move $a6, $zero
    bl probe_clone
    bge $a0, $zero, 3f
    li.d $a0, 224
    b probe_failure
3:
    move $s5, $a0
    la.local $a1, .Lstatus
    move $a2, $zero
    move $a3, $zero
    li.d $a7, 260
    syscall 0
    beq $a0, $s5, 4f
    li.d $a0, 225
    b probe_failure
4:
    la.local $t1, .Lstatus
    ld.w $t2, $t1, 0
    CHECK $t2, 0, 226
    li.d $a0, 229
    bl signal_capture_check
    bl signal_pending_setup
    move $a0, $s0
    li.d $a1, 10
    li.d $a7, 129
    syscall 0
    CHECK $a0, 0, 185
    la.local $t1, signal_state
    li.d $t0, 1
    st.w $t0, $t1, 52
    ld.w $t2, $t1, 48
    CHECK $t2, 1, 186
    movfcsr2gr $t2, $fcsr0
    CHECK $t2, 0x101, 187
    movgr2fcsr $fcsr0, $zero
    b signal_frame_done
.Lfp_child:
    li.d $a0, 227
    bl signal_capture_check
    FCC_CHECK
    movfcsr2gr $t2, $fcsr0
    CHECK $t2, 0x100, 228
    move $a0, $zero
    li.d $a7, 93
    syscall 0
    .word 0
.size signal_edited_pc, .-signal_edited_pc

# /// 启用cause只修改本次用户帧，RA仍返回内核trampoline。
.global signal_inject_handler
.type signal_inject_handler, @function
signal_inject_handler:
    addi.d $sp, $sp, -16
    st.d $ra, $sp, 0
    move $a0, $a2
    bl signal_inject_context
    ld.d $ra, $sp, 0
    addi.d $sp, $sp, 16
    jr $ra
.size signal_inject_handler, .-signal_inject_handler

# /// SIGFPE业务在C核原帧cause回写和新帧内容，返回仍由内核sigreturn提交。
.global signal_fpe_handler
.type signal_fpe_handler, @function
signal_fpe_handler:
    addi.d $sp, $sp, -16
    st.d $ra, $sp, 0
    bl signal_fpe_context
    ld.d $ra, $sp, 0
    addi.d $sp, $sp, 16
    jr $ra
.size signal_fpe_handler, .-signal_fpe_handler

# /// 无法写入信号帧的坏SP必须真实导致SIGSEGV，不能先切回有效C栈。
.global signal_bad_stack
.type signal_bad_stack, @function
signal_bad_stack:
    li.d $sp, 16
    li.d $a1, 10
    li.d $a7, 129
    syscall 0
    bnez $a0, 1f
    li.d $a0, 74
    b 2f
1:
    li.d $a0, 73
2:
    li.d $a7, 93
    syscall 0
    .word 0
.size signal_bad_stack, .-signal_bad_stack

# /// 真实读取指令的发生PC与恢复PC分别发布给共同帧检查。
.global signal_fault_load
.type signal_fault_load, @function
signal_fault_load:
    la.local $t0, signal_fault_pc
    la.local $t1, .Lload_pc
    st.d $t1, $t0, 0
    la.local $t0, signal_resume_pc
    la.local $t1, .Lload_resume
    st.d $t1, $t0, 0
.Lload_pc:
    ld.bu $t2, $a0, 0
    move $a0, $a1
    b probe_failure
.Lload_resume:
    jr $ra
.size signal_fault_load, .-signal_fault_load

# /// 对真实RO映射执行写故障，内核sc_flags必须保留写方向。
.global signal_fault_store
.type signal_fault_store, @function
signal_fault_store:
    la.local $t0, signal_fault_pc
    la.local $t1, .Lstore_pc
    st.d $t1, $t0, 0
    la.local $t0, signal_resume_pc
    la.local $t1, .Lstore_resume
    st.d $t1, $t0, 0
.Lstore_pc:
    st.d $zero, $a0, 0
    move $a0, $a1
    b probe_failure
.Lstore_resume:
    jr $ra
.size signal_fault_store, .-signal_fault_store

# /// 非法指令si_addr和mcontext PC均指向同一真实指令。
.global signal_fault_illegal
.type signal_fault_illegal, @function
signal_fault_illegal:
    la.local $t0, signal_fault_pc
    la.local $t1, .Lillegal_pc
    st.d $t1, $t0, 0
    la.local $t0, signal_fault_address
    st.d $t1, $t0, 0
    la.local $t0, signal_resume_pc
    la.local $t1, .Lillegal_resume
    st.d $t1, $t0, 0
.Lillegal_pc:
    .word 0
    b probe_failure
.Lillegal_resume:
    jr $ra
.size signal_fault_illegal, .-signal_fault_illegal

# /// 原始SP-8发表后保持纯用户无限计算，要求真实timer抢占与远核KILL。
.global signal_infinite
.type signal_infinite, @function
signal_infinite:
    li.d $t0, 0x51a1
    st.d $t0, $a0, -8
    dbar 0
1:
    addi.d $t1, $t1, 1
    xor $t2, $t1, $t2
    b 1b
.size signal_infinite, .-signal_infinite

.bss
.balign 16
.Lclock: .zero 16
.Lstatus: .zero 8
