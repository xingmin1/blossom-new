.text

.macro FP_LOAD
    fld.d $f0, $t1, 0
    fld.d $f1, $t1, 8
    fld.d $f2, $t1, 16
    fld.d $f3, $t1, 24
    fld.d $f4, $t1, 32
    fld.d $f5, $t1, 40
    fld.d $f6, $t1, 48
    fld.d $f7, $t1, 56
    fld.d $f8, $t1, 64
    fld.d $f9, $t1, 72
    fld.d $f10, $t1, 80
    fld.d $f11, $t1, 88
    fld.d $f12, $t1, 96
    fld.d $f13, $t1, 104
    fld.d $f14, $t1, 112
    fld.d $f15, $t1, 120
    fld.d $f16, $t1, 128
    fld.d $f17, $t1, 136
    fld.d $f18, $t1, 144
    fld.d $f19, $t1, 152
    fld.d $f20, $t1, 160
    fld.d $f21, $t1, 168
    fld.d $f22, $t1, 176
    fld.d $f23, $t1, 184
    fld.d $f24, $t1, 192
    fld.d $f25, $t1, 200
    fld.d $f26, $t1, 208
    fld.d $f27, $t1, 216
    fld.d $f28, $t1, 224
    fld.d $f29, $t1, 232
    fld.d $f30, $t1, 240
    fld.d $f31, $t1, 248
.endm
.macro FP_STORE
    fst.d $f0, $t1, 0
    fst.d $f1, $t1, 8
    fst.d $f2, $t1, 16
    fst.d $f3, $t1, 24
    fst.d $f4, $t1, 32
    fst.d $f5, $t1, 40
    fst.d $f6, $t1, 48
    fst.d $f7, $t1, 56
    fst.d $f8, $t1, 64
    fst.d $f9, $t1, 72
    fst.d $f10, $t1, 80
    fst.d $f11, $t1, 88
    fst.d $f12, $t1, 96
    fst.d $f13, $t1, 104
    fst.d $f14, $t1, 112
    fst.d $f15, $t1, 120
    fst.d $f16, $t1, 128
    fst.d $f17, $t1, 136
    fst.d $f18, $t1, 144
    fst.d $f19, $t1, 152
    fst.d $f20, $t1, 160
    fst.d $f21, $t1, 168
    fst.d $f22, $t1, 176
    fst.d $f23, $t1, 184
    fst.d $f24, $t1, 192
    fst.d $f25, $t1, 200
    fst.d $f26, $t1, 208
    fst.d $f27, $t1, 216
    fst.d $f28, $t1, 224
    fst.d $f29, $t1, 232
    fst.d $f30, $t1, 240
    fst.d $f31, $t1, 248
.endm

.macro FCC_INIT
    li.d $t0, 0
    movgr2cf $fcc0, $t0
    li.d $t0, 1
    movgr2cf $fcc1, $t0
    li.d $t0, 0
    movgr2cf $fcc2, $t0
    li.d $t0, 1
    movgr2cf $fcc3, $t0
    li.d $t0, 0
    movgr2cf $fcc4, $t0
    li.d $t0, 1
    movgr2cf $fcc5, $t0
    li.d $t0, 0
    movgr2cf $fcc6, $t0
    li.d $t0, 1
    movgr2cf $fcc7, $t0
.endm
.macro FCC_ZERO
    movgr2cf $fcc0, $zero
    movgr2cf $fcc1, $zero
    movgr2cf $fcc2, $zero
    movgr2cf $fcc3, $zero
    movgr2cf $fcc4, $zero
    movgr2cf $fcc5, $zero
    movgr2cf $fcc6, $zero
    movgr2cf $fcc7, $zero
.endm
.macro FCC_CHECK
    movcf2gr $t2, $fcc0
    CHECK $t2, 0, 60
    movcf2gr $t2, $fcc1
    CHECK $t2, 1, 60
    movcf2gr $t2, $fcc2
    CHECK $t2, 0, 60
    movcf2gr $t2, $fcc3
    CHECK $t2, 1, 60
    movcf2gr $t2, $fcc4
    CHECK $t2, 0, 60
    movcf2gr $t2, $fcc5
    CHECK $t2, 1, 60
    movcf2gr $t2, $fcc6
    CHECK $t2, 0, 60
    movcf2gr $t2, $fcc7
    CHECK $t2, 1, 60
.endm

# 普通静态磁盘程序；模式分别定位pending、动作、帧、栈、等待和时间协议。
# 所有编号均为用户exit状态，信号死亡由真实wait的低七位区分。
.macro SC n
    li.d $a7, \n
    syscall 0
.endm
.macro CHECK reg, value, code
    li.d $s8, \code
    li.d $t0, \value
    beq \reg, $t0, 991f
    b .Lexit
991:
.endm
.macro EQ left, right, code
    li.d $s8, \code
    beq \left, \right, 991f
    b .Lexit
991:
.endm
.macro OK code
    CHECK $a0, 0, \code
.endm
.macro ACTION sig, handler, flags, mask, code
    la.local $t1, .Laction
    la.local $t0, \handler
    st.d $t0, $t1, 0
    li.d $t0, \flags
    st.d $t0, $t1, 8
    li.d $t0, \mask
    st.d $t0, $t1, 16
    li.d $a0, \sig
    move $a1, $t1
    li.d $a2, 0
    li.d $a3, 8
    SC 134
    OK \code
.endm
.macro SEND sig, code
    move $a0, $s0
    li.d $a1, \sig
    SC 129
    OK \code
.endm
.macro MASK how, bits, code
    la.local $t1, .Lmask
    li.d $t0, \bits
    st.d $t0, $t1, 0
    li.d $a0, \how
    move $a1, $t1
    li.d $a2, 0
    li.d $a3, 8
    SC 135
    OK \code
.endm
.macro TAKE sig, value, code
    la.local $a0, .Lmask
    li.d $t0, 1
    li.d $t1, \sig-1
    sll.d $t0, $t0, $t1
    st.d $t0, $a0, 0
    la.local $a1, .Linfo
    la.local $a2, .Lzero_time
    li.d $a3, 8
    SC 137
    CHECK $a0, \sig, \code
    la.local $t1, .Linfo
    ld.w $t2, $t1, 8
    CHECK $t2, -1, \code+1
    ld.w $t2, $t1, 16
    EQ $t2, $s0, \code+2
    ld.w $t2, $t1, 20
    CHECK $t2, 0, \code+3
    ld.d $t2, $t1, 24
    CHECK $t2, \value, \code+4
.endm
.macro QUEUE sig, value, code
    la.local $a2, .Lqueue_info
    li.d $t0, \sig
    st.w $t0, $a2, 0
    li.d $t0, -1
    st.w $t0, $a2, 8
    st.w $s0, $a2, 16
    li.d $t0, \value
    st.d $t0, $a2, 24
    move $a0, $s0
    li.d $a1, \sig
    SC 138
    OK \code
.endm

# /// 从真实argv选择单一协议，保存初始栈供无限计算的内核观察者读取。
.global _start
.type _start, @function
_start:
    li.d $s8, 1
    ld.d $t2, $sp, 0
    CHECK $t2, 2, 1
    ld.d $t0, $sp, 16
    ld.bu $s7, $t0, 0
    li.d $t0, 105
    beq $s7, $t0, .Linfinite
    SC 172
    move $s0, $a0
    SC 178
    move $s1, $a0
    la.local $s2, .Lstate
    # envp为空；逐对读取真实auxv的AT_CLKTCK。
    addi.d $t1, $sp, 40
1:
    ld.d $t2, $t1, 0
    li.d $t0, 17
    beq $t2, $t0, 2f
    li.d $s8, 153
    beqz $t2, .Lexit
    addi.d $t1, $t1, 16
    b 1b
2:
    ld.d $t2, $t1, 8
    CHECK $t2, 100, 154
    st.d $t2, $s2, 56
    li.d $t0, 112
    beq $s7, $t0, .Lpending
    li.d $t0, 97
    beq $s7, $t0, .Lactions
    li.d $t0, 102
    beq $s7, $t0, .Lframe
    li.d $t0, 110
    beq $s7, $t0, .Lnested
    li.d $t0, 98
    beq $s7, $t0, .Lbad_return
    li.d $t0, 107
    beq $s7, $t0, .Lbad_return
    li.d $t0, 120
    beq $s7, $t0, .Lreset_default
    li.d $t0, 119
    beq $s7, $t0, .Lbad_write
    li.d $t0, 115
    beq $s7, $t0, .Lsleep
    li.d $t0, 100
    beq $s7, $t0, .Ldeadline
    li.d $t0, 103
    beq $s7, $t0, .Lgroup
    li.d $t0, 99
    beq $s7, $t0, .Lcpu_time
    li.d $t0, 118
    beq $s7, $t0, .Lvm_pending
    li.d $t0, 114
    beq $s7, $t0, .Lread
    li.d $t0, 122
    beq $s7, $t0, .Lsynchronous_fault
    li.d $t0, 101
    beq $s7, $t0, .Lexit_signal
    li.d $t0, 117
    beq $s7, $t0, .Lsuspend_unmap
    b .Lexit
.Linfinite:
    li.d $t0, 0x51a1
    st.d $t0, $sp, -8
    dbar 0
1:
    addi.d $t1, $t1, 1
    xor $t2, $t1, $t2
    b 1b

# /// 等真实wait消费并确认child离栈，再检查零及非零退出信号。
.Lexit_signal:
    addi.d $s6, $sp, -16
    st.d $zero, $s6, 0
    st.d $zero, $s6, 8
    MASK 0, 0x10000, 2
    li.d $a0, 0x100
    la.local $a1, .Lchild_stack_top
    li.d $a2, 0
    li.d $a3, 0
    li.d $a4, 0
    SC 220
    beqz $a0, .Lexit_signal_child
    li.d $s8, 3
    blt $a0, $zero, .Lexit
    st.d $a0, $s6, 0
    li.d $t0, 1
    dbar 0
    st.d $t0, $s6, 8
1:
    ld.d $t2, $s6, 8
    dbar 0
    li.d $t0, 2
    bne $t2, $t0, 1b
    la.local $a0, .Lmask
    li.d $a1, 8
    SC 136
    OK 4
    la.local $t1, .Lmask
    ld.d $t2, $t1, 0
    li.d $t0, 0x10000
    and $t2, $t2, $t0
    CHECK $t2, 0, 5
    li.d $a0, 0x111
    la.local $a1, .Lchild_stack_top
    li.d $a2, 0
    li.d $a3, 0
    li.d $a4, 0
    SC 220
    beqz $a0, .Lexit_signal_child
    li.d $s8, 6
    blt $a0, $zero, .Lexit
    st.d $a0, $s6, 0
    li.d $t0, 3
    dbar 0
    st.d $t0, $s6, 8
1:
    ld.d $t2, $s6, 8
    dbar 0
    li.d $t0, 4
    bne $t2, $t0, 1b
    la.local $a0, .Lmask
    li.d $a1, 8
    SC 136
    OK 7
    la.local $t1, .Lmask
    ld.d $t2, $t1, 0
    li.d $t0, 0x10000
    and $t2, $t2, $t0
    CHECK $t2, 0x10000, 8
    b .Lpass
.Lexit_signal_child:
    b .Lpass

# /// STOP后父解除共享参数页，CONT重放失败也必须恢复并消费临时mask。
.Lsuspend_unmap:
    addi.d $s6, $sp, -16
    st.d $zero, $s6, 0
    st.d $zero, $s6, 8
    li.d $a0, 0x43001000
    li.d $a1, 4096
    li.d $a2, 3
    li.d $a3, 0x32
    li.d $a4, -1
    li.d $a5, 0
    SC 222
    CHECK $a0, 0x43001000, 9
    li.d $t0, 0x800
    st.d $t0, $a0, 0
    li.d $a0, 0x111
    la.local $a1, .Lchild_stack_top
    li.d $a2, 0
    li.d $a3, 0
    li.d $a4, 0
    SC 220
    beqz $a0, .Lsuspend_unmap_child
    li.d $s8, 10
    blt $a0, $zero, .Lexit
    move $s3, $a0
    st.d $a0, $s6, 0
    li.d $t0, 1
    dbar 0
    st.d $t0, $s6, 8
1:
    ld.d $t2, $s6, 8
    dbar 0
    li.d $t0, 2
    bne $t2, $t0, 1b
    li.d $a0, 0x43001000
    li.d $a1, 4096
    SC 215
    OK 11
    li.d $t0, 3
    dbar 0
    st.d $t0, $s6, 8
    bl .Lwait_child
    b .Lpass
.Lsuspend_unmap_child:
    SC 172
    move $s0, $a0
    MASK 2, 0x10200, 12
    li.d $a0, 0x43001000
    li.d $a1, 8
    SC 133
    CHECK $a0, -14, 13
    li.d $a0, 2
    li.d $a1, 0
    la.local $a2, .Lmask
    li.d $a3, 8
    SC 135
    OK 14
    la.local $t1, .Lmask
    ld.d $t2, $t1, 0
    CHECK $t2, 0x10200, 15
    MASK 2, 0x200, 16
    ACTION 12, .Lcount, 4, 0, 17
    SEND 12, 18
    ld.w $t2, $s2, 12
    CHECK $t2, 1, 19
    li.d $a0, 2
    li.d $a1, 0
    la.local $a2, .Lmask
    li.d $a3, 8
    SC 135
    OK 20
    la.local $t1, .Lmask
    ld.d $t2, $t1, 0
    CHECK $t2, 0x200, 21
    b .Lpass

# /// 五次真实同步异常分别核发生时siginfo/PC，合法修改恢复PC后继续用户执行。
.Lsynchronous_fault:
    ACTION 11, .Lsynchronous_handler, 4, 0, 227
    ACTION 7, .Lsynchronous_handler, 4, 0, 227
    ACTION 4, .Lsynchronous_handler, 4, 0, 227
    li.d $a0, 0
    li.d $a1, 4096
    li.d $a2, 3
    li.d $a3, 0x22
    li.d $a4, -1
    li.d $a5, 0
    SC 222
    li.d $s8, 228
    blt $a0, $zero, .Lexit
    move $s4, $a0
    st.d $zero, $s4, 0
    move $a0, $s4
    li.d $a1, 4096
    SC 215
    OK 229
    # s3/s5保存信号及code，s4地址、s6发生PC、s0继续PC、s1地址方向。
    li.d $s3, 11
    li.d $s5, 1
    li.d $s1, 0x40000000
    la.local $s6, .Lfault_maperr_pc
    la.local $s0, .Lfault_maperr_resume
.Lfault_maperr_pc:
    ld.bu $t2, $s4, 0
    li.d $s8, 230
    b .Lexit
.Lfault_maperr_resume:
    ld.w $t2, $s2, 12
    CHECK $t2, 1, 231
    li.d $a0, 0
    li.d $a1, 4096
    li.d $a2, 3
    li.d $a3, 0x22
    li.d $a4, -1
    li.d $a5, 0
    SC 222
    li.d $s8, 232
    blt $a0, $zero, .Lexit
    move $s4, $a0
    # 先触页，再收回权限，确保检查的是实际访问而非用户给定的错误码。
    st.d $zero, $s4, 0
    move $a0, $s4
    li.d $a1, 4096
    li.d $a2, 0
    SC 226
    OK 233
    li.d $s5, 2
    la.local $s6, .Lfault_read_pc
    la.local $s0, .Lfault_read_resume
.Lfault_read_pc:
    ld.bu $t2, $s4, 0
    li.d $s8, 234
    b .Lexit
.Lfault_read_resume:
    ld.w $t2, $s2, 12
    CHECK $t2, 2, 235
    move $a0, $s4
    li.d $a1, 4096
    li.d $a2, 1
    SC 226
    OK 236
    li.d $s1, 0x80000000
    la.local $s6, .Lfault_write_pc
    la.local $s0, .Lfault_write_resume
.Lfault_write_pc:
    st.d $zero, $s4, 0
    li.d $s8, 237
    b .Lexit
.Lfault_write_resume:
    ld.w $t2, $s2, 12
    CHECK $t2, 3, 238
    move $a0, $s4
    li.d $a1, 4096
    li.d $a2, 3
    SC 226
    OK 239
    li.d $t1, 0x51a3
    st.d $t1, $s4, 0
    ld.d $t2, $s4, 0
    EQ $t2, $t1, 240
    move $a0, $s4
    li.d $a1, 4096
    SC 215
    OK 240
    # 独立短文件为12293字节；16384起的整页在EOF外，保留旧r模式65536字节文件。
    li.d $a0, -100
    la.local $a1, .Lfault_file_path
    li.d $a2, 0
    li.d $a3, 0
    SC 56
    li.d $s8, 241
    blt $a0, $zero, .Lexit
    move $s3, $a0
    li.d $a0, 0
    li.d $a1, 4096
    li.d $a2, 1
    li.d $a3, 2
    move $a4, $s3
    li.d $a5, 16384
    SC 222
    li.d $s8, 242
    blt $a0, $zero, .Lexit
    move $s4, $a0
    move $a0, $s3
    SC 57
    OK 243
    li.d $s3, 7
    li.d $s1, 0x40000000
    la.local $s6, .Lfault_bus_pc
    la.local $s0, .Lfault_bus_resume
.Lfault_bus_pc:
    ld.bu $t2, $s4, 0
    li.d $s8, 244
    b .Lexit
.Lfault_bus_resume:
    ld.w $t2, $s2, 12
    CHECK $t2, 4, 245
    move $a0, $s4
    li.d $a1, 4096
    SC 215
    OK 245
    li.d $s3, 4
    li.d $s5, 1
    li.d $s1, 0
    la.local $s6, .Lfault_ill_pc
    move $s4, $s6
    la.local $s0, .Lfault_ill_resume
.Lfault_ill_pc:
    .word 0
    li.d $s8, 246
    b .Lexit
.Lfault_ill_resume:
    ld.w $t2, $s2, 12
    CHECK $t2, 5, 247
    b .Lpass

# /// 屏蔽后分别入private与group；普通合并保首次siginfo，RT逐实例FIFO。
.Lpending:
    MASK 0, 0x80000a00, 2
    QUEUE 10, 0x31, 3
    QUEUE 10, 0x32, 4
    TAKE 10, 0x31, 5
    la.local $a0, .Lmask
    la.local $a1, .Linfo
    la.local $a2, .Lzero_time
    li.d $a3, 8
    SC 137
    CHECK $a0, -11, 10
    move $a0, $s0
    move $a1, $s1
    li.d $a2, 10
    SC 131
    OK 11
    SEND 10, 12
    # private与group各保一个；tkill先消费，kill实例随后消费。
    li.d $s4, -6
    bl .Ltake_sender
    li.d $s4, 0
    bl .Ltake_sender
    QUEUE 32, 0x41, 15
    QUEUE 32, 0x42, 16
    QUEUE 32, 0x43, 17
    TAKE 32, 0x41, 18
    TAKE 32, 0x42, 23
    TAKE 32, 0x43, 28
    # 上限只用防无限运行的外部预算；不要求内核容量为某个固定数。
    li.d $s5, 0
.Lfill_rt:
    la.local $a2, .Lqueue_info
    li.d $t0, 32
    st.w $t0, $a2, 0
    st.d $s5, $a2, 24
    move $a0, $s0
    li.d $a1, 32
    SC 138
    li.d $t0, -11
    beq $a0, $t0, .Lfull_rt
    OK 33
    addi.d $s5, $s5, 1
    li.d $t0, 65536
    bltu $s5, $t0, .Lfill_rt
    li.d $s8, 34
    b .Lexit
.Lfull_rt:
    li.d $s8, 35
    beqz $s5, .Lexit
    li.d $s6, 0
.Ldrain_rt:
    la.local $a0, .Lmask
    li.d $t0, 0x80000000
    st.d $t0, $a0, 0
    la.local $a1, .Linfo
    la.local $a2, .Lzero_time
    li.d $a3, 8
    SC 137
    CHECK $a0, 32, 36
    la.local $t1, .Linfo
    ld.d $t2, $t1, 24
    EQ $t2, $s6, 37
    addi.d $s6, $s6, 1
    bltu $s6, $s5, .Ldrain_rt
    # copyout失败仍消费一次：随后同信号应EAGAIN。
    QUEUE 32, 0x71, 38
    la.local $a0, .Lmask
    li.d $a1, -8
    la.local $a2, .Lzero_time
    li.d $a3, 8
    SC 137
    CHECK $a0, -14, 39
    la.local $a0, .Lmask
    la.local $a1, .Linfo
    la.local $a2, .Lzero_time
    li.d $a3, 8
    SC 137
    CHECK $a0, -11, 40
    b .Lpass

# /// 从领取的sender载荷核对SI_TKILL/SI_USER与真实pid。
.Ltake_sender:
    li.d $a0, 0
    la.local $a0, .Lmask
    li.d $t0, 0x200
    st.d $t0, $a0, 0
    la.local $a1, .Linfo
    la.local $a2, .Lzero_time
    li.d $a3, 8
    SC 137
    CHECK $a0, 10, 13
    la.local $t1, .Linfo
    ld.w $t2, $t1, 8
    EQ $t2, $s4, 14
    ld.w $t2, $t1, 16
    EQ $t2, $s0, 15
    jr $ra

# /// NODEFER不移除action mask；嵌套self被允许，USR2在handler完成后递送。
.Lactions:
    ACTION 10, .Lnodefer, 0x40000004, 0x800, 41
    ACTION 12, .Lusr2, 4, 0, 42
    SEND 10, 43
    ld.w $t2, $s2, 0
    CHECK $t2, 2, 44
    ld.w $t2, $s2, 4
    CHECK $t2, 1, 45
    ld.w $t2, $s2, 8
    CHECK $t2, 0, 46
    # 默认SIGCHLD/SIGWINCH被忽略，不执行用户handler。
    SEND 17, 47
    SEND 28, 48
    ACTION 10, .Lcount, 0x80000004, 0, 49
    SEND 10, 50
    li.d $a0, 10
    li.d $a1, 0
    la.local $a2, .Lold_action
    li.d $a3, 8
    SC 134
    OK 51
    la.local $t1, .Lold_action
    ld.d $t2, $t1, 0
    CHECK $t2, 0, 52
    ld.w $t2, $s2, 12
    CHECK $t2, 1, 53
    ACTION 10, .Lswap_action, 4, 0, 155
    SEND 10, 156
    ld.w $t2, $s2, 12
    CHECK $t2, 2, 157
    SEND 10, 158
    ld.w $t2, $s2, 12
    CHECK $t2, 3, 159
    MASK 2, 0x40100, 160
    li.d $a0, 2
    li.d $a1, 0
    la.local $a2, .Lmask
    li.d $a3, 8
    SC 135
    OK 161
    la.local $t1, .Lmask
    ld.d $t2, $t1, 0
    CHECK $t2, 0, 162
    la.local $a1, .Laction
    li.d $t0, 1
    st.d $t0, $a1, 0
    st.d $zero, $a1, 8
    st.d $zero, $a1, 16
    li.d $a0, 12
    li.d $a2, 0
    li.d $a3, 8
    SC 134
    OK 194
    SEND 12, 195
    ld.w $t2, $s2, 4
    CHECK $t2, 1, 196
    b .Lpass

# /// RESETHAND的第二次实例必须按真正默认动作终止进程。
.Lreset_default:
    ACTION 10, .Lcount, 0x80000004, 0, 197
    SEND 10, 198
    SEND 10, 199
    li.d $s8, 200
    b .Lexit

# /// 处理函数使用自己的栈区，sigreturn恢复全部32个FP及FCSR并采用用户修改的PC/SP。
.Lframe:
    ACTION 10, .Ledit_frame, 4, 0, 54
    la.local $t1, .Lfp_values
    FP_LOAD
    li.d $t0, 0x100
    movgr2fcsr $fcsr0, $t0
    FCC_INIT
    li.d $tp, 0x12345000
    SEND 10, 55
    li.d $s8, 56
    b .Lexit
.Ledited_pc:
    la.local $t1, .Lnew_stack_top
    EQ $sp, $t1, 57
    CHECK $tp, 0x12345000, 58
    CHECK $r21, 0x76543000, 222
    SC 172
    EQ $a0, $s0, 221
    la.local $t1, .Lfp_output
    FP_STORE
    la.local $t1, .Lfp_values
    la.local $t2, .Lfp_output
    li.d $t3, 32
1:
    ld.d $t4, $t1, 0
    ld.d $t5, $t2, 0
    EQ $t4, $t5, 59
    addi.d $t1, $t1, 8
    addi.d $t2, $t2, 8
    addi.d $t3, $t3, -1
    bnez $t3, 1b
    movfcsr2gr $t2, $fcsr0
    CHECK $t2, 0x100, 60
    FCC_CHECK
    # sigreturn帧启用已有cause后，SIGFPE必须在下一条用户指令前到达。
    ACTION 8, .Lfpe_pending, 4, 0, 183
    ACTION 10, .Linject_fcsr, 4, 0, 184
    SEND 10, 185
    li.d $t0, 1
    st.w $t0, $s2, 52
    ld.w $t2, $s2, 48
    CHECK $t2, 1, 186
    movfcsr2gr $t2, $fcsr0
    CHECK $t2, 0x101, 187
    movgr2fcsr $fcsr0, $zero
    b .Lpass

# /// 匿名私有altstack先触页再fork；child内核frame copyout必须COW而父整页保持图样。
.Lnested:
    li.d $a0, 0
    li.d $a1, 16384
    li.d $a2, 3
    li.d $a3, 0x22
    li.d $a4, -1
    li.d $a5, 0
    SC 222
    li.d $s8, 61
    blt $a0, $zero, .Lexit
    move $s4, $a0
    li.d $t2, 0x5a5a5a5a5a5a5a5a
    li.d $t3, 2048
    move $t1, $s4
1:
    st.d $t2, $t1, 0
    addi.d $t1, $t1, 8
    addi.d $t3, $t3, -1
    bnez $t3, 1b
    la.local $a0, .Laltstack
    st.d $s4, $a0, 0
    st.d $zero, $a0, 8
    li.d $t0, 16384
    st.d $t0, $a0, 16
    li.d $a1, 0
    SC 132
    OK 62
    ACTION 10, .Lalt_outer, 0x08000004, 0, 63
    ACTION 12, .Lalt_inner, 0x08000004, 0, 64
    li.d $a0, 17
    li.d $a1, 0
    li.d $a2, 0
    li.d $a3, 0
    li.d $a4, 0
    SC 220
    beqz $a0, .Lalt_child
    li.d $s8, 65
    blt $a0, $zero, .Lexit
    move $s3, $a0
    bl .Lwait_child
    move $t1, $s4
    li.d $t3, 2048
    li.d $t4, 0x5a5a5a5a5a5a5a5a
1:
    ld.d $t2, $t1, 0
    EQ $t2, $t4, 66
    addi.d $t1, $t1, 8
    addi.d $t3, $t3, -1
    bnez $t3, 1b
    b .Lpass
.Lalt_child:
    SC 172
    move $s0, $a0
    SEND 10, 67
    ld.w $t2, $s2, 16
    CHECK $t2, 2, 68
    b .Lpass

# /// 坏sigreturn结构和无法写入frame均经有限SIGSEGV终止，绝不返回成功。
.Lbad_return:
    ACTION 10, .Lcorrupt, 4, 0, 69
    SEND 10, 70
    li.d $s8, 71
    b .Lexit
.Lbad_write:
    ACTION 10, .Lcount, 4, 0, 72
    li.d $sp, 16
    SEND 10, 73
    li.d $s8, 74
    b .Lexit

# /// REAL timer在真正睡眠中递送；相对rem为剩余而绝对请求不碰rem。
.Lsleep:
    ACTION 14, .Lalarm, 4, 0, 75
    li.d $a0, 0
    la.local $a1, .Litimer_real
    li.d $a2, 0
    SC 103
    OK 76
    la.local $a0, .Lsleep_long
    la.local $a1, .Lremaining
    SC 101
    CHECK $a0, -4, 77
    ld.w $t2, $s2, 20
    CHECK $t2, 1, 78
    la.local $t1, .Lremaining
    ld.d $t2, $t1, 0
    CHECK $t2, 0, 79
    ld.d $t2, $t1, 8
    li.d $s8, 80
    li.d $t0, 0
    bge $t0, $t2, .Lexit
    li.d $t0, 500000000
    bge $t2, $t0, .Lexit
    li.d $a0, 1
    la.local $a1, .Lclock_a
    SC 113
    OK 81
    la.local $t1, .Lclock_a
    ld.d $t2, $t1, 8
    li.d $t0, 500000000
    add.d $t2, $t2, $t0
    li.d $t0, 1000000000
    bltu $t2, $t0, 1f
    sub.d $t2, $t2, $t0
    ld.d $t3, $t1, 0
    addi.d $t3, $t3, 1
    st.d $t3, $t1, 0
1:
    st.d $t2, $t1, 8
    li.d $a0, 0
    la.local $a1, .Litimer_real
    li.d $a2, 0
    SC 103
    OK 82
    la.local $t1, .Lremaining
    li.d $t0, 0x777
    st.d $t0, $t1, 0
    st.d $t0, $t1, 8
    li.d $a0, 1
    li.d $a1, 1
    la.local $a2, .Lclock_a
    la.local $a3, .Lremaining
    SC 115
    CHECK $a0, -4, 83
    la.local $t1, .Lremaining
    ld.d $t2, $t1, 0
    CHECK $t2, 0x777, 84
    ld.d $t2, $t1, 8
    CHECK $t2, 0x777, 85
    b .Lrestart_wait

# /// SA_RESTART只重启可重启waitid；同一signal无该标志时返回EINTR并保真实child。
.Lrestart_wait:
    ACTION 14, .Lalarm, 0x10000004, 0, 163
    li.d $s5, 0
.Lwait_fork:
    li.d $a0, 17
    li.d $a1, 0
    li.d $a2, 0
    li.d $a3, 0
    li.d $a4, 0
    SC 220
    beqz $a0, .Lwait_sleep_child
    li.d $s8, 164
    blt $a0, $zero, .Lexit
    move $s3, $a0
    li.d $a0, 0
    la.local $a1, .Litimer_real
    li.d $a2, 0
    SC 103
    OK 165
    bnez $s5, .Lwait_eintr
    bl .Lwait_child
    ld.w $t2, $s2, 20
    CHECK $t2, 3, 166
    ACTION 14, .Lalarm, 4, 0, 167
    li.d $s5, 1
    b .Lwait_fork
.Lwait_eintr:
    li.d $a0, 1
    move $a1, $s3
    la.local $a2, .Linfo
    li.d $a3, 4
    li.d $a4, 0
    SC 95
    CHECK $a0, -4, 168
    bl .Lwait_child
    ld.w $t2, $s2, 20
    CHECK $t2, 4, 169
    b .Lperiodic_real
.Lwait_sleep_child:
    la.local $a0, .Lsleep_long
    li.d $a1, 0
    SC 101
    OK 170
    b .Lpass

# /// 多个实际REAL周期在blocked mask内只留一实例，取消后领取不补排旧相位。
.Lperiodic_real:
    MASK 0, 0x2000, 210
    li.d $a0, 0
    la.local $a1, .Litimer_periodic
    li.d $a2, 0
    SC 103
    OK 211
    la.local $a0, .Lsleep_short
    li.d $a1, 0
    SC 101
    OK 212
    li.d $a0, 0
    la.local $a1, .Litimer_off
    la.local $a2, .Litimer_old
    SC 103
    OK 213
    la.local $t1, .Litimer_old
    ld.d $t2, $t1, 0
    CHECK $t2, 0, 214
    ld.d $t2, $t1, 8
    CHECK $t2, 20000, 215
    la.local $a0, .Lmask
    la.local $a1, .Linfo
    la.local $a2, .Lzero_time
    li.d $a3, 8
    SC 137
    CHECK $a0, 14, 216
    la.local $t1, .Linfo
    ld.w $t2, $t1, 8
    CHECK $t2, 128, 217
    la.local $a0, .Lmask
    la.local $a1, .Linfo
    la.local $a2, .Lzero_time
    li.d $a3, 8
    SC 137
    CHECK $a0, -11, 218
    MASK 1, 0x2000, 219
    ld.w $t2, $s2, 20
    CHECK $t2, 4, 220
    b .Lpass

# /// child真实stop/continue打断无handler sleep；结束时原500ms期限没有重新加一遍。
.Ldeadline:
    li.d $s5, 0
.Ldeadline_again:
    li.d $a0, 1
    la.local $a1, .Lclock_a
    SC 113
    OK 86
    li.d $a0, 0x111
    la.local $a1, .Lchild_stack_top
    li.d $a2, 0
    li.d $a3, 0
    li.d $a4, 0
    SC 220
    beqz $a0, .Ldeadline_child
    li.d $s8, 87
    blt $a0, $zero, .Lexit
    move $s3, $a0
    bnez $s5, .Ldeadline_absolute
    la.local $a0, .Lsleep_long
    li.d $a1, 0
    SC 101
    OK 88
    b .Ldeadline_slept
.Ldeadline_absolute:
    la.local $t1, .Lclock_a
    la.local $t2, .Lclock_b
    ld.d $t3, $t1, 0
    ld.d $t4, $t1, 8
    li.d $t0, 500000000
    add.d $t4, $t4, $t0
    li.d $t0, 1000000000
    bltu $t4, $t0, 1f
    sub.d $t4, $t4, $t0
    addi.d $t3, $t3, 1
1:
    st.d $t3, $t2, 0
    st.d $t4, $t2, 8
    li.d $a0, 1
    li.d $a1, 1
    move $a2, $t2
    li.d $a3, 0
    SC 115
    OK 201
.Ldeadline_slept:
    bl .Lwait_child
    li.d $a0, 1
    la.local $a1, .Lclock_b
    SC 113
    OK 89
    bl .Lelapsed
    li.d $t0, 450000000
    li.d $s8, 90
    bltu $a0, $t0, .Lexit
    li.d $t0, 650000000
    bgeu $a0, $t0, .Lexit
    bnez $s5, .Lpass
    li.d $s5, 1
    b .Ldeadline_again
.Ldeadline_child:
    la.local $a0, .Lsleep_short
    li.d $a1, 0
    SC 101
    OK 91
    SEND 19, 92
    la.local $a0, .Lsleep_short
    li.d $a1, 0
    SC 101
    OK 93
    SEND 18, 94
    b .Lpass

# /// 用户父通过waitid观察真实全成员stop完成，再continue并正常wait退出。
.Lgroup:
    li.d $a0, 0x111
    la.local $a1, .Lchild_stack_top
    li.d $a2, 0
    li.d $a3, 0
    li.d $a4, 0
    SC 220
    beqz $a0, .Lgroup_child
    li.d $s8, 95
    blt $a0, $zero, .Lexit
    move $s3, $a0
1:
    ld.w $t0, $s2, 48
    dbar 0
    beqz $t0, 1b
    move $a0, $s3
    li.d $a1, 19
    SC 129
    OK 96
    li.d $a0, 1
    move $a1, $s3
    la.local $a2, .Linfo
    li.d $a3, 2
    li.d $a4, 0
    SC 95
    OK 97
    la.local $t1, .Linfo
    ld.w $t2, $t1, 8
    CHECK $t2, 5, 98
    ld.w $t2, $t1, 16
    EQ $t2, $s3, 99
    ld.w $t2, $t1, 24
    CHECK $t2, 19, 100
    move $a0, $s3
    li.d $a1, 18
    SC 129
    OK 101
    li.d $a0, 1
    move $a1, $s3
    la.local $a2, .Linfo
    li.d $a3, 8
    li.d $a4, 0
    SC 95
    OK 102
    la.local $t1, .Linfo
    ld.w $t2, $t1, 8
    CHECK $t2, 6, 103
    ld.w $t2, $t1, 24
    CHECK $t2, 18, 104
    li.d $t0, 1
    st.w $t0, $s2, 32
    bl .Lwait_child
    b .Lpass
.Lgroup_child:
    # child建立真实THREAD成员；VM非THREAD的父不加入停止组。
    SC 172
    move $s0, $a0
    li.d $a0, 0x10900
    la.local $a1, .Lthread_stack_top
    li.d $a2, 0
    li.d $a3, 0
    li.d $a4, 0
    SC 220
    beqz $a0, .Lgroup_thread
    li.d $s8, 105
    blt $a0, $zero, .Lexit
    li.d $t0, 1
    dbar 0
    st.w $t0, $s2, 48
.Lgroup_thread:
1:
    ld.w $t0, $s2, 32
    dbar 0
    beqz $t0, 1b
    b .Lpass

# /// VM与SIGHAND共享仍有独立组pending，child既不继承旧pending也不能领取父实例。
.Lvm_pending:
    MASK 0, 0x200, 106
    QUEUE 10, 0x91, 107
    li.d $a0, 0x911
    la.local $a1, .Lchild_stack_top
    li.d $a2, 0
    li.d $a3, 0
    li.d $a4, 0
    SC 220
    beqz $a0, .Lvm_child
    li.d $s8, 108
    blt $a0, $zero, .Lexit
    move $s3, $a0
1:
    ld.w $t0, $s2, 44
    dbar 0
    beqz $t0, 1b
    TAKE 10, 0x91, 109
    li.d $t0, 1
    st.w $t0, $s2, 36
    bl .Lwait_child
    b .Lpass
.Lvm_child:
    la.local $a0, .Lmask
    la.local $a1, .Linfo
    la.local $a2, .Lzero_time
    li.d $a3, 8
    SC 137
    CHECK $a0, -11, 114
    SC 172
    move $s0, $a0
    SEND 10, 115
    li.d $s4, 0
    bl .Ltake_sender
    li.d $t0, 1
    dbar 0
    st.w $t0, $s2, 44
1:
    ld.w $t0, $s2, 36
    dbar 0
    beqz $t0, 1b
    la.local $a0, .Lmask
    la.local $a1, .Linfo
    la.local $a2, .Lzero_time
    li.d $a3, 8
    SC 137
    CHECK $a0, -11, 116
    b .Lpass

# /// 同一OFD读责任完成后才处理pending；每字节核对盘内容和真实游标。
.Lread:
    ACTION 14, .Lalarm, 4, 0, 117
    li.d $a0, -100
    la.local $a1, .Lfile_path
    li.d $a2, 0
    li.d $a3, 0
    SC 56
    CHECK $a0, 0, 118
    move $s3, $a0
    move $a0, $s3
    la.local $a1, .Lread_buffer
    li.d $a2, 65536
    li.d $t0, 0x51a2
    st.d $t0, $sp, -8
    dbar 0
    SC 63
    CHECK $a0, 65536, 120
    ld.w $t2, $s2, 20
    CHECK $t2, 1, 171
    la.local $t1, .Lread_buffer
    li.d $t3, 0
1:
    ld.bu $t2, $t1, 0
    srli.d $t4, $t3, 9
    li.d $t0, 17
    mul.d $t4, $t4, $t0
    andi $t5, $t3, 511
    add.d $t4, $t4, $t5
    addi.d $t4, $t4, 37
    andi $t4, $t4, 255
    EQ $t2, $t4, 121
    addi.d $t1, $t1, 1
    addi.d $t3, $t3, 1
    li.d $t0, 65536
    bltu $t3, $t0, 1b
    move $a0, $s3
    li.d $a1, 0
    li.d $a2, 1
    SC 62
    CHECK $a0, 65536, 122
    move $a0, $s3
    SC 57
    OK 123
    b .Lpass

# /// CPU clock/times以真实执行账核对USER_HZ，offCPU睡眠不能算成进程CPU时间。
.Lcpu_time:
    ACTION 26, .Lvirtual, 4, 0, 124
    ACTION 27, .Lprof, 4, 0, 125
    li.d $a0, 2
    la.local $a1, .Lcpu_a
    SC 113
    OK 126
    la.local $a0, .Ltms_a
    SC 153
    li.d $s8, 127
    blt $a0, $zero, .Lexit
    li.d $a0, 1
    la.local $a1, .Litimer_cpu
    li.d $a2, 0
    SC 103
    OK 128
    li.d $a0, 2
    la.local $a1, .Litimer_cpu
    li.d $a2, 0
    SC 103
    OK 129
1:
    ld.w $t1, $s2, 24
    ld.w $t2, $s2, 28
    and $t1, $t1, $t2
    beqz $t1, 1b
    # 原time/tms的stime只有kernel执行累计至少一tick后才前进。
    li.d $s6, 1000000
.Lkernel_account:
    la.local $a0, .Lremaining
    li.d $a1, 0
    SC 169
    OK 172
    la.local $a0, .Ltms_b
    SC 153
    la.local $t1, .Ltms_a
    ld.d $t2, $t1, 8
    la.local $t1, .Ltms_b
    ld.d $t3, $t1, 8
    bltu $t2, $t3, .Lkernel_accounted
    addi.d $s6, $s6, -1
    bnez $s6, .Lkernel_account
    li.d $s8, 173
    b .Lexit
.Lkernel_accounted:
    li.d $a0, 2
    la.local $a1, .Lcpu_b
    SC 113
    OK 130
    la.local $a0, .Ltms_b
    SC 153
    li.d $s8, 131
    blt $a0, $zero, .Lexit
    la.local $t1, .Ltms_a
    ld.d $t2, $t1, 0
    la.local $t1, .Ltms_b
    ld.d $t3, $t1, 0
    li.d $s8, 174
    bgeu $t2, $t3, .Lexit
    # 两个分账各自截成USER_HZ tick，两次syscall的取样边界允许正负两个tick误差。
    la.local $t1, .Lcpu_a
    la.local $t2, .Lcpu_b
    bl .Ldiff
    move $s4, $a0
    li.d $s8, 132
    li.d $t0, 10000000
    bltu $s4, $t0, .Lexit
    la.local $t1, .Ltms_a
    ld.d $t2, $t1, 0
    ld.d $t3, $t1, 8
    add.d $t2, $t2, $t3
    la.local $t1, .Ltms_b
    ld.d $t3, $t1, 0
    ld.d $t4, $t1, 8
    add.d $t3, $t3, $t4
    sub.d $t3, $t3, $t2
    ld.d $t0, $s2, 56
    li.d $t1, 1000000000
    div.du $t0, $t1, $t0
    div.du $t4, $s4, $t0
    sub.d $t4, $t4, $t3
    addi.d $t4, $t4, 2
    li.d $s8, 133
    li.d $t0, 5
    bgeu $t4, $t0, .Lexit
    la.local $a0, .Lsleep_short
    li.d $a1, 0
    SC 101
    OK 134
    li.d $a0, 2
    la.local $a1, .Lcpu_a
    SC 113
    OK 135
    la.local $t1, .Lcpu_b
    la.local $t2, .Lcpu_a
    bl .Ldiff
    li.d $t0, 50000000
    li.d $s8, 136
    bgeu $a0, $t0, .Lexit
    # 同组另一实际成员继续用户计算，process clock推进而睡眠线程clock保持offCPU。
    li.d $a0, 2
    la.local $a1, .Lcpu_a
    SC 113
    OK 202
    li.d $a0, 3
    la.local $a1, .Lclock_a
    SC 113
    OK 203
    li.d $a0, 0x10900
    la.local $a1, .Lthread_stack_top
    li.d $a2, 0
    li.d $a3, 0
    li.d $a4, 0
    SC 220
    beqz $a0, .Lcpu_member
    li.d $s8, 204
    blt $a0, $zero, .Lexit
    la.local $a0, .Lsleep_short
    li.d $a1, 0
    SC 101
    OK 205
    li.d $a0, 2
    la.local $a1, .Lcpu_b
    SC 113
    OK 206
    li.d $a0, 3
    la.local $a1, .Lclock_b
    SC 113
    OK 207
    la.local $t1, .Lcpu_a
    la.local $t2, .Lcpu_b
    bl .Ldiff
    li.d $t0, 50000000
    li.d $s8, 208
    bltu $a0, $t0, .Lexit
    bl .Lelapsed
    li.d $t0, 50000000
    li.d $s8, 209
    bgeu $a0, $t0, .Lexit
    li.d $t0, 1
    dbar 0
    st.w $t0, $s2, 32
    b .Lpass
.Lcpu_member:
1:
    ld.w $t0, $s2, 32
    dbar 0
    beqz $t0, 1b
    b .Lpass

# /// 真实父waitid消费退出并核child status；失败编号由child原状态传到父。
.Lwait_child:
    li.d $a0, 1
    move $a1, $s3
    la.local $a2, .Linfo
    li.d $a3, 4
    li.d $a4, 0
    SC 95
    OK 137
    la.local $t1, .Linfo
    ld.w $t2, $t1, 8
    CHECK $t2, 1, 138
    ld.w $t2, $t1, 16
    EQ $t2, $s3, 139
    ld.w $t2, $t1, 24
    move $s8, $t2
    bnez $t2, .Lexit
    jr $ra

# /// 两次monotonic读数化成经过纳秒，时钟只用于边界而非猜调度状态。
.Lelapsed:
    la.local $t1, .Lclock_a
    la.local $t2, .Lclock_b
.Ldiff:
    ld.d $t3, $t1, 0
    ld.d $t4, $t2, 0
    sub.d $t4, $t4, $t3
    li.d $t0, 1000000000
    mul.d $t4, $t4, $t0
    ld.d $t3, $t1, 8
    ld.d $t5, $t2, 8
    sub.d $t5, $t5, $t3
    add.d $a0, $t4, $t5
    jr $ra

# /// 嵌套处理仅保存当前frame SP，修改动作不会重选当前实例。
.Lnodefer:
    addi.d $sp, $sp, -32
    st.d $ra, $sp, 0
    st.d $a2, $sp, 8
    ld.w $t1, $s2, 0
    addi.d $t1, $t1, 1
    st.w $t1, $s2, 0
    li.d $t0, 1
    bne $t1, $t0, 1f
    li.d $t0, 1
    st.w $t0, $s2, 8
    SEND 12, 141
    ld.w $t2, $s2, 4
    CHECK $t2, 0, 142
    SEND 10, 143
    li.d $t0, 0
    st.w $t0, $s2, 8
1:
    ld.d $ra, $sp, 0
    addi.d $sp, $sp, 32
    jr $ra
.Lusr2:
    ld.w $t2, $s2, 8
    CHECK $t2, 0, 144
    ld.w $t1, $s2, 4
    addi.d $t1, $t1, 1
    st.w $t1, $s2, 4
    jr $ra
.Lcount:
    ld.w $t1, $s2, 12
    addi.d $t1, $t1, 1
    st.w $t1, $s2, 12
    jr $ra

# /// 已经选取的handler只完成本次实例；更新动作仅影响下一次发送。
.Lswap_action:
    addi.d $sp, $sp, -16
    st.d $ra, $sp, 0
    ACTION 10, .Lcount, 4, 0, 175
    ld.w $t1, $s2, 12
    addi.d $t1, $t1, 1
    st.w $t1, $s2, 12
    ld.d $ra, $sp, 0
    addi.d $sp, $sp, 16
    jr $ra
# /// 检查内核写入的siginfo和原PC；只改本次帧的恢复PC，不重试同一故障指令。
.Lsynchronous_handler:
    EQ $a0, $s3, 248
    ld.w $t2, $a1, 0
    EQ $t2, $s3, 249
    ld.w $t2, $a1, 4
    CHECK $t2, 0, 250
    ld.w $t2, $a1, 8
    EQ $t2, $s5, 251
    ld.d $t2, $a1, 16
    EQ $t2, $s4, 252
    # Linux LA的uc从frame SP+128开始；mcontext PC为uc+176，flags为uc+440。
    addi.d $t2, $sp, 128
    EQ $a2, $t2, 253
    ld.d $t2, $a2, 176
    EQ $t2, $s6, 254
    ld.wu $t2, $a2, 440
    li.d $t1, 0xc0000000
    and $t2, $t2, $t1
    EQ $t2, $s1, 255
    st.d $s0, $a2, 176
    ld.w $t2, $s2, 12
    addi.d $t2, $t2, 1
    st.w $t2, $s2, 12
    jr $ra

.Ledit_frame:
    CHECK $a0, 10, 145
    ld.w $t2, $a1, 0
    CHECK $t2, 10, 146
    ld.w $t2, $a1, 8
    CHECK $t2, 0, 176
    ld.w $t2, $a1, 16
    EQ $t2, $s0, 177
    ld.w $t2, $a2, 728
    CHECK $t2, 0x100, 178
    ld.d $t2, $a2, 720
    CHECK $t2, 0x0100010001000100, 223
    ld.w $t2, $a2, 448
    CHECK $t2, 0x46505501, 179
    ld.w $t2, $a2, 452
    CHECK $t2, 288, 180
    ld.w $t2, $a2, 736
    CHECK $t2, 0, 181
    ld.w $t2, $a2, 740
    CHECK $t2, 0, 181
    addi.d $t1, $a2, 464
    la.local $t2, .Lfp_values
    li.d $t3, 32
1:
    ld.d $t4, $t1, 0
    ld.d $t5, $t2, 0
    EQ $t4, $t5, 182
    addi.d $t1, $t1, 8
    addi.d $t2, $t2, 8
    addi.d $t3, $t3, -1
    bnez $t3, 1b
    # ucontext固定mcontext偏移176，sc_regs从下一机器字开始，SP为r3。
    la.local $t1, .Ledited_pc
    st.d $t1, $a2, 176
    la.local $t1, .Lnew_stack_top
    st.d $t1, $a2, 208
    # 用户r21与内核CpuLocal/KS0各自保存，修改此字段不能污染下一次kernel入口。
    li.d $t1, 0x76543000
    st.d $t1, $a2, 352
    la.local $t1, .Lfp_output
    FP_LOAD
    li.d $t0, 0
    movgr2fcsr $fcsr0, $t0
    FCC_ZERO
    li.d $tp, 0
    jr $ra
.Lalt_outer:
    li.d $s8, 147
    bgeu $s4, $sp, .Lexit
    li.d $t0, 16384
    add.d $t0, $s4, $t0
    bltu $t0, $sp, .Lexit
    st.d $sp, $s2, 40
    addi.d $sp, $sp, -32
    st.d $ra, $sp, 0
    li.d $a0, 0
    la.local $a1, .Lold_stack
    SC 132
    OK 148
    la.local $t1, .Lold_stack
    ld.w $t2, $t1, 8
    CHECK $t2, 1, 149
    la.local $a0, .Laltstack
    li.d $a1, 0
    SC 132
    CHECK $a0, -1, 150
    SEND 12, 151
    ld.d $ra, $sp, 0
    addi.d $sp, $sp, 32
    jr $ra
.Lalt_inner:
    li.d $s8, 152
    bgeu $s4, $sp, .Lexit
    ld.d $t1, $s2, 40
    bgeu $sp, $t1, .Lexit
    ld.d $t2, $a2, 16
    EQ $t2, $s4, 224
    ld.w $t2, $a2, 24
    CHECK $t2, 0, 225
    ld.d $t2, $a2, 32
    CHECK $t2, 16384, 226
    li.d $t0, 2
    st.w $t0, $s2, 16
    jr $ra
.Lcorrupt:
    li.d $t0, 107
    bne $s7, $t0, 1f
    li.d $t0, -4096
    st.d $t0, $a2, 176
    jr $ra
1:
    li.d $t0, 1
    st.w $t0, $a2, 448
    jr $ra

# /// 原frame与再次递送frame的cause均必须清零，siginfo保存内核生成的SIGFPE事实。
.Linject_fcsr:
    addi.d $t1, $a2, 728
    st.d $t1, $s2, 40
    li.d $t0, 0x01000101
    st.w $t0, $t1, 0
    # 后续SIGFPE使用另一个栈区，原frame的cause回写不能被新frame覆盖冒充。
    ld.d $t2, $a2, 208
    li.d $t0, 4096
    sub.d $t2, $t2, $t0
    st.d $t2, $a2, 208
    jr $ra
.Lfpe_pending:
    CHECK $a0, 8, 188
    ld.w $t2, $a1, 0
    CHECK $t2, 8, 189
    ld.w $t2, $a1, 8
    CHECK $t2, 128, 190
    ld.w $t2, $s2, 52
    CHECK $t2, 0, 191
    ld.d $t1, $s2, 40
    ld.w $t2, $t1, 0
    CHECK $t2, 0x101, 192
    ld.w $t2, $a2, 728
    CHECK $t2, 0x101, 193
    li.d $t0, 1
    st.w $t0, $s2, 48
    jr $ra
.Lalarm:
    ld.w $t1, $s2, 20
    addi.d $t1, $t1, 1
    st.w $t1, $s2, 20
    jr $ra
.Lvirtual:
    li.d $t0, 1
    st.w $t0, $s2, 24
    jr $ra
.Lprof:
    li.d $t0, 1
    st.w $t0, $s2, 28
    jr $ra
.Lpass:
    li.d $s8, 0
.Lexit:
    move $a0, $s8
    SC 93
    .word 0
.size _start, .-_start

.data
.balign 16
.Laction: .zero 24
.Lold_action: .zero 24
.Lmask: .zero 8
.Linfo: .zero 128
.Lqueue_info: .zero 128
.Lstate: .zero 64
.Lzero_time: .quad 0,0
.Lsleep_short: .quad 0,100000000
.Lsleep_long: .quad 0,500000000
.Lremaining: .zero 16
.Lclock_a: .zero 16
.Lclock_b: .zero 16
.Lcpu_a: .zero 16
.Lcpu_b: .zero 16
.Ltms_a: .zero 32
.Ltms_b: .zero 32
.Litimer_real: .quad 0,0,0,100000
.Litimer_periodic: .quad 0,20000,0,20000
.Litimer_off: .zero 32
.Litimer_old: .zero 32
.Litimer_cpu: .quad 0,0,0,10000
.Laltstack: .zero 24
.Lold_stack: .zero 24
.Lfile_path: .asciz "/test/data.bin"
.Lfault_file_path: .asciz "/test/io-data.bin"
.balign 16
.Lfp_values:
    .quad 0x3ff0000000000000
    .quad 0x3ff0000000010001
    .quad 0x3ff0000000020002
    .quad 0x3ff0000000030003
    .quad 0x3ff0000000040004
    .quad 0x3ff0000000050005
    .quad 0x3ff0000000060006
    .quad 0x3ff0000000070007
    .quad 0x3ff0000000080008
    .quad 0x3ff0000000090009
    .quad 0x3ff00000000a000a
    .quad 0x3ff00000000b000b
    .quad 0x3ff00000000c000c
    .quad 0x3ff00000000d000d
    .quad 0x3ff00000000e000e
    .quad 0x3ff00000000f000f
    .quad 0x3ff0000000100010
    .quad 0x3ff0000000110011
    .quad 0x3ff0000000120012
    .quad 0x3ff0000000130013
    .quad 0x3ff0000000140014
    .quad 0x3ff0000000150015
    .quad 0x3ff0000000160016
    .quad 0x3ff0000000170017
    .quad 0x3ff0000000180018
    .quad 0x3ff0000000190019
    .quad 0x3ff00000001a001a
    .quad 0x3ff00000001b001b
    .quad 0x3ff00000001c001c
    .quad 0x3ff00000001d001d
    .quad 0x3ff00000001e001e
    .quad 0x3ff00000001f001f

.bss
.balign 16
.Lfp_output: .zero 256
.Lnew_stack: .zero 16384
.Lnew_stack_top:
.Lchild_stack: .zero 16384
.Lchild_stack_top:
.Lthread_stack: .zero 16384
.Lthread_stack_top:
.Lread_buffer: .zero 65536
