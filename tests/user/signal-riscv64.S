.text
.option norelax
.option norvc

# /// 跨协议分派和错误出口使用长跳转，条件分支只覆盖一条JAL。
.macro FAR inverse, left, right, target
    \inverse \left, \right, 991f
    j \target
991:
.endm

.macro FP_LOAD
    fld f0, 0(t1)
    fld f1, 8(t1)
    fld f2, 16(t1)
    fld f3, 24(t1)
    fld f4, 32(t1)
    fld f5, 40(t1)
    fld f6, 48(t1)
    fld f7, 56(t1)
    fld f8, 64(t1)
    fld f9, 72(t1)
    fld f10, 80(t1)
    fld f11, 88(t1)
    fld f12, 96(t1)
    fld f13, 104(t1)
    fld f14, 112(t1)
    fld f15, 120(t1)
    fld f16, 128(t1)
    fld f17, 136(t1)
    fld f18, 144(t1)
    fld f19, 152(t1)
    fld f20, 160(t1)
    fld f21, 168(t1)
    fld f22, 176(t1)
    fld f23, 184(t1)
    fld f24, 192(t1)
    fld f25, 200(t1)
    fld f26, 208(t1)
    fld f27, 216(t1)
    fld f28, 224(t1)
    fld f29, 232(t1)
    fld f30, 240(t1)
    fld f31, 248(t1)
.endm
.macro FP_STORE
    fsd f0, 0(t1)
    fsd f1, 8(t1)
    fsd f2, 16(t1)
    fsd f3, 24(t1)
    fsd f4, 32(t1)
    fsd f5, 40(t1)
    fsd f6, 48(t1)
    fsd f7, 56(t1)
    fsd f8, 64(t1)
    fsd f9, 72(t1)
    fsd f10, 80(t1)
    fsd f11, 88(t1)
    fsd f12, 96(t1)
    fsd f13, 104(t1)
    fsd f14, 112(t1)
    fsd f15, 120(t1)
    fsd f16, 128(t1)
    fsd f17, 136(t1)
    fsd f18, 144(t1)
    fsd f19, 152(t1)
    fsd f20, 160(t1)
    fsd f21, 168(t1)
    fsd f22, 176(t1)
    fsd f23, 184(t1)
    fsd f24, 192(t1)
    fsd f25, 200(t1)
    fsd f26, 208(t1)
    fsd f27, 216(t1)
    fsd f28, 224(t1)
    fsd f29, 232(t1)
    fsd f30, 240(t1)
    fsd f31, 248(t1)
.endm

# 普通静态磁盘程序；模式分别定位pending、动作、帧、栈、等待和时间协议。
# 所有编号均为用户exit状态，信号死亡由真实wait的低七位区分。
.macro SC n
    li a7, \n
    ecall
.endm
.macro CHECK reg, value, code
    li s8, \code
    li t0, \value
    beq \reg, t0, 991f
    j .Lexit
991:
.endm
.macro EQ left, right, code
    li s8, \code
    beq \left, \right, 991f
    j .Lexit
991:
.endm
.macro OK code
    CHECK a0, 0, \code
.endm
.macro ACTION sig, handler, flags, mask, code
    lla t1, .Laction
    lla t0, \handler
    sd t0, 0(t1)
    li t0, \flags
    sd t0, 8(t1)
    li t0, \mask
    sd t0, 16(t1)
    li a0, \sig
    mv a1, t1
    li a2, 0
    li a3, 8
    SC 134
    OK \code
.endm
.macro SEND sig, code
    mv a0, s0
    li a1, \sig
    SC 129
    OK \code
.endm
.macro MASK how, bits, code
    lla t1, .Lmask
    li t0, \bits
    sd t0, 0(t1)
    li a0, \how
    mv a1, t1
    li a2, 0
    li a3, 8
    SC 135
    OK \code
.endm
.macro TAKE sig, value, code
    lla a0, .Lmask
    li t0, 1
    li t1, \sig-1
    sll t0, t0, t1
    sd t0, 0(a0)
    lla a1, .Linfo
    lla a2, .Lzero_time
    li a3, 8
    SC 137
    CHECK a0, \sig, \code
    lla t1, .Linfo
    lw t2, 8(t1)
    CHECK t2, -1, \code+1
    lw t2, 16(t1)
    EQ t2, s0, \code+2
    lw t2, 20(t1)
    CHECK t2, 0, \code+3
    ld t2, 24(t1)
    CHECK t2, \value, \code+4
.endm
.macro QUEUE sig, value, code
    lla a2, .Lqueue_info
    li t0, \sig
    sw t0, 0(a2)
    li t0, -1
    sw t0, 8(a2)
    sw s0, 16(a2)
    li t0, \value
    sd t0, 24(a2)
    mv a0, s0
    li a1, \sig
    SC 138
    OK \code
.endm

# /// 从真实argv选择单一协议，保存初始栈供无限计算的内核观察者读取。
.global _start
.type _start, @function
_start:
    li s8, 1
    ld t2, 0(sp)
    CHECK t2, 2, 1
    ld t0, 16(sp)
    lbu s7, 0(t0)
    li t0, 105
    FAR bne, s7, t0, .Linfinite
    SC 172
    mv s0, a0
    SC 178
    mv s1, a0
    lla s2, .Lstate
    # envp为空；逐对读取真实auxv的AT_CLKTCK。
    addi t1, sp, 40
1:
    ld t2, 0(t1)
    li t0, 17
    beq t2, t0, 2f
    li s8, 153
    FAR bne, t2, zero, .Lexit
    addi t1, t1, 16
    j 1b
2:
    ld t2, 8(t1)
    CHECK t2, 100, 154
    sd t2, 56(s2)
    li t0, 112
    FAR bne, s7, t0, .Lpending
    li t0, 97
    FAR bne, s7, t0, .Lactions
    li t0, 102
    FAR bne, s7, t0, .Lframe
    li t0, 122
    FAR bne, s7, t0, .Lsynchronous_fault
    li t0, 110
    FAR bne, s7, t0, .Lnested
    li t0, 98
    FAR bne, s7, t0, .Lbad_return
    li t0, 107
    FAR bne, s7, t0, .Lbad_return
    li t0, 120
    FAR bne, s7, t0, .Lreset_default
    li t0, 119
    FAR bne, s7, t0, .Lbad_write
    li t0, 115
    FAR bne, s7, t0, .Lsleep
    li t0, 100
    FAR bne, s7, t0, .Ldeadline
    li t0, 103
    FAR bne, s7, t0, .Lgroup
    li t0, 99
    FAR bne, s7, t0, .Lcpu_time
    li t0, 118
    FAR bne, s7, t0, .Lvm_pending
    li t0, 114
    FAR bne, s7, t0, .Lread
    li t0, 101
    FAR bne, s7, t0, .Lexit_signal
    li t0, 117
    FAR bne, s7, t0, .Lsuspend_fault
    j .Lexit
.Linfinite:
    li t0, 0x51a1
    sd t0, -8(sp)
    fence rw, rw
1:
    addi t1, t1, 1
    xor t2, t1, t2
    j 1b

# /// 屏蔽后分别入private与group；普通合并保首次siginfo，RT逐实例FIFO。
.Lpending:
    MASK 0, 0x80000a00, 2
    QUEUE 10, 0x31, 3
    QUEUE 10, 0x32, 4
    TAKE 10, 0x31, 5
    lla a0, .Lmask
    lla a1, .Linfo
    lla a2, .Lzero_time
    li a3, 8
    SC 137
    CHECK a0, -11, 10
    mv a0, s0
    mv a1, s1
    li a2, 10
    SC 131
    OK 11
    SEND 10, 12
    # private与group各保一个；tkill先消费，kill实例随后消费。
    li s4, -6
    call .Ltake_sender
    li s4, 0
    call .Ltake_sender
    QUEUE 32, 0x41, 15
    QUEUE 32, 0x42, 16
    QUEUE 32, 0x43, 17
    TAKE 32, 0x41, 18
    TAKE 32, 0x42, 23
    TAKE 32, 0x43, 28
    # 上限只用防无限运行的外部预算；不要求内核容量为某个固定数。
    li s5, 0
.Lfill_rt:
    lla a2, .Lqueue_info
    li t0, 32
    sw t0, 0(a2)
    sd s5, 24(a2)
    mv a0, s0
    li a1, 32
    SC 138
    li t0, -11
    beq a0, t0, .Lfull_rt
    OK 33
    addi s5, s5, 1
    li t0, 65536
    bltu s5, t0, .Lfill_rt
    li s8, 34
    j .Lexit
.Lfull_rt:
    li s8, 35
    FAR bne, s5, zero, .Lexit
    li s6, 0
.Ldrain_rt:
    lla a0, .Lmask
    li t0, 0x80000000
    sd t0, 0(a0)
    lla a1, .Linfo
    lla a2, .Lzero_time
    li a3, 8
    SC 137
    CHECK a0, 32, 36
    lla t1, .Linfo
    ld t2, 24(t1)
    EQ t2, s6, 37
    addi s6, s6, 1
    bltu s6, s5, .Ldrain_rt
    # copyout失败仍消费一次：随后同信号应EAGAIN。
    QUEUE 32, 0x71, 38
    lla a0, .Lmask
    li a1, -8
    lla a2, .Lzero_time
    li a3, 8
    SC 137
    CHECK a0, -14, 39
    lla a0, .Lmask
    lla a1, .Linfo
    lla a2, .Lzero_time
    li a3, 8
    SC 137
    CHECK a0, -11, 40
    j .Lpass

# /// 从领取的sender载荷核对SI_TKILL/SI_USER与真实pid。
.Ltake_sender:
    li a0, 0
    lla a0, .Lmask
    li t0, 0x200
    sd t0, 0(a0)
    lla a1, .Linfo
    lla a2, .Lzero_time
    li a3, 8
    SC 137
    CHECK a0, 10, 13
    lla t1, .Linfo
    lw t2, 8(t1)
    EQ t2, s4, 14
    lw t2, 16(t1)
    EQ t2, s0, 15
    ret

# /// NODEFER不移除action mask；嵌套self被允许，USR2在handler完成后递送。
.Lactions:
    ACTION 10, .Lnodefer, 0x40000004, 0x800, 41
    ACTION 12, .Lusr2, 4, 0, 42
    SEND 10, 43
    lw t2, 0(s2)
    CHECK t2, 2, 44
    lw t2, 4(s2)
    CHECK t2, 1, 45
    lw t2, 8(s2)
    CHECK t2, 0, 46
    # 默认SIGCHLD/SIGWINCH被忽略，不执行用户handler。
    SEND 17, 47
    SEND 28, 48
    ACTION 10, .Lcount, 0x80000004, 0, 49
    SEND 10, 50
    li a0, 10
    li a1, 0
    lla a2, .Lold_action
    li a3, 8
    SC 134
    OK 51
    lla t1, .Lold_action
    ld t2, 0(t1)
    CHECK t2, 0, 52
    lw t2, 12(s2)
    CHECK t2, 1, 53
    ACTION 10, .Lswap_action, 4, 0, 155
    SEND 10, 156
    lw t2, 12(s2)
    CHECK t2, 2, 157
    SEND 10, 158
    lw t2, 12(s2)
    CHECK t2, 3, 159
    MASK 2, 0x40100, 160
    li a0, 2
    li a1, 0
    lla a2, .Lmask
    li a3, 8
    SC 135
    OK 161
    lla t1, .Lmask
    ld t2, 0(t1)
    CHECK t2, 0, 162
    lla a1, .Laction
    li t0, 1
    sd t0, 0(a1)
    sd zero, 8(a1)
    sd zero, 16(a1)
    li a0, 12
    li a2, 0
    li a3, 8
    SC 134
    OK 194
    SEND 12, 195
    lw t2, 4(s2)
    CHECK t2, 1, 196
    j .Lpass

# /// RESETHAND的第二次实例必须按真正默认动作终止进程。
.Lreset_default:
    ACTION 10, .Lcount, 0x80000004, 0, 197
    SEND 10, 198
    SEND 10, 199
    li s8, 200
    j .Lexit

# /// 处理函数使用自己的栈区，sigreturn恢复全部32个FP及FCSR并采用用户修改的PC/SP。
.Lframe:
    ACTION 10, .Ledit_frame, 4, 0, 54
    lla t1, .Lfp_values
    FP_LOAD
    li t0, 0x20
    csrw fcsr, t0
    li tp, 0x12345000
    SEND 10, 55
    li s8, 56
    j .Lexit
.Ledited_pc:
    lla t1, .Lnew_stack_top
    EQ sp, t1, 57
    CHECK tp, 0x12345000, 58
    SC 172
    EQ a0, s0, 221
    lla t1, .Lfp_output
    FP_STORE
    lla t1, .Lfp_values
    lla t2, .Lfp_output
    li t3, 32
1:
    ld t4, 0(t1)
    ld t5, 0(t2)
    EQ t4, t5, 59
    addi t1, t1, 8
    addi t2, t2, 8
    addi t3, t3, -1
    bnez t3, 1b
    csrr t2, fcsr
    CHECK t2, 0x20, 60
    j .Lpass

# /// 真正同步故障核对发生时siginfo和PC；每次只由用户改帧跳过该条坏指令。
.Lsynchronous_fault:
    ACTION 11, .Lsynchronous_handler, 4, 0, 227
    ACTION 7, .Lsynchronous_handler, 4, 0, 228
    ACTION 4, .Lsynchronous_handler, 4, 0, 229
    # 先成功分配再撤映射，坏地址不依赖固定地址空间猜测。
    li a0, 0
    li a1, 4096
    li a2, 3
    li a3, 0x22
    li a4, -1
    li a5, 0
    SC 222
    li s8, 230
    FAR bge, a0, zero, .Lexit
    mv s4, a0
    li a1, 4096
    SC 215
    OK 231
    li s3, 11
    li s5, 1
    lla s6, .Lfault_unmapped
    lla s9, .Lresume_unmapped
.Lfault_unmapped:
    ld t2, 0(s4)
    li s8, 249
    j .Lexit
.Lresume_unmapped:
    lw t2, 0(s2)
    CHECK t2, 1, 249
    # 已触页的同一匿名页分别禁止读和写，不能以未分配页代替权限故障。
    li a0, 0
    li a1, 4096
    li a2, 3
    li a3, 0x22
    li a4, -1
    li a5, 0
    SC 222
    li s8, 232
    FAR bge, a0, zero, .Lexit
    mv s4, a0
    sd zero, 0(s4)
    li a1, 4096
    li a2, 0
    SC 226
    OK 233
    li s5, 2
    lla s6, .Lfault_no_read
    lla s9, .Lresume_no_read
.Lfault_no_read:
    ld t2, 0(s4)
    li s8, 250
    j .Lexit
.Lresume_no_read:
    lw t2, 0(s2)
    CHECK t2, 2, 250
    mv a0, s4
    li a1, 4096
    li a2, 1
    SC 226
    OK 234
    lla s6, .Lfault_no_write
    lla s9, .Lresume_no_write
.Lfault_no_write:
    sd zero, 0(s4)
    li s8, 251
    j .Lexit
.Lresume_no_write:
    lw t2, 0(s2)
    CHECK t2, 3, 251
    mv a0, s4
    li a1, 4096
    li a2, 3
    SC 226
    OK 235
    sd zero, 0(s4)
    mv a0, s4
    li a1, 4096
    SC 215
    OK 236
    # 原io-data.bin长12293；offset16384开始的整页确在真实文件EOF之外。
    li a0, -100
    lla a1, .Lfault_file_path
    li a2, 0
    li a3, 0
    SC 56
    li s8, 237
    FAR bge, a0, zero, .Lexit
    mv s0, a0
    li a0, 0
    li a1, 4096
    li a2, 1
    li a3, 2
    mv a4, s0
    li a5, 16384
    SC 222
    li s8, 238
    FAR bge, a0, zero, .Lexit
    mv s4, a0
    li s3, 7
    lla s6, .Lfault_eof
    lla s9, .Lresume_eof
.Lfault_eof:
    ld t2, 0(s4)
    li s8, 252
    j .Lexit
.Lresume_eof:
    lw t2, 0(s2)
    CHECK t2, 4, 252
    mv a0, s4
    li a1, 4096
    SC 215
    OK 239
    mv a0, s0
    SC 57
    OK 239
    li s3, 4
    li s5, 1
    lla s6, .Lfault_illegal
    mv s4, s6
    lla s9, .Lresume_illegal
.Lfault_illegal:
    .word 0
    li s8, 253
    j .Lexit
.Lresume_illegal:
    lw t2, 0(s2)
    CHECK t2, 5, 253
    j .Lpass

# /// 匿名私有altstack先触页再fork；child内核frame copyout必须COW而父整页保持图样。
.Lnested:
    li a0, 0
    li a1, 16384
    li a2, 3
    li a3, 0x22
    li a4, -1
    li a5, 0
    SC 222
    li s8, 61
    FAR bge, a0, zero, .Lexit
    mv s4, a0
    li t2, 0x5a5a5a5a5a5a5a5a
    li t3, 2048
    mv t1, s4
1:
    sd t2, 0(t1)
    addi t1, t1, 8
    addi t3, t3, -1
    bnez t3, 1b
    lla a0, .Laltstack
    sd s4, 0(a0)
    sd zero, 8(a0)
    li t0, 16384
    sd t0, 16(a0)
    li a1, 0
    SC 132
    OK 62
    ACTION 10, .Lalt_outer, 0x08000004, 0, 63
    ACTION 12, .Lalt_inner, 0x08000004, 0, 64
    li a0, 17
    li a1, 0
    li a2, 0
    li a3, 0
    li a4, 0
    SC 220
    beqz a0, .Lalt_child
    li s8, 65
    FAR bge, a0, zero, .Lexit
    mv s3, a0
    call .Lwait_child
    mv t1, s4
    li t3, 2048
    li t4, 0x5a5a5a5a5a5a5a5a
1:
    ld t2, 0(t1)
    EQ t2, t4, 66
    addi t1, t1, 8
    addi t3, t3, -1
    bnez t3, 1b
    j .Lpass
.Lalt_child:
    SC 172
    mv s0, a0
    SEND 10, 67
    lw t2, 16(s2)
    CHECK t2, 2, 68
    j .Lpass

# /// 坏sigreturn结构和无法写入frame均经有限SIGSEGV终止，绝不返回成功。
.Lbad_return:
    ACTION 10, .Lcorrupt, 4, 0, 69
    SEND 10, 70
    li s8, 71
    j .Lexit
.Lbad_write:
    ACTION 10, .Lcount, 4, 0, 72
    li sp, 16
    SEND 10, 73
    li s8, 74
    j .Lexit

# /// REAL timer在真正睡眠中递送；相对rem为剩余而绝对请求不碰rem。
.Lsleep:
    ACTION 14, .Lalarm, 4, 0, 75
    li a0, 0
    lla a1, .Litimer_real
    li a2, 0
    SC 103
    OK 76
    lla a0, .Lsleep_long
    lla a1, .Lremaining
    SC 101
    CHECK a0, -4, 77
    lw t2, 20(s2)
    CHECK t2, 1, 78
    lla t1, .Lremaining
    ld t2, 0(t1)
    CHECK t2, 0, 79
    ld t2, 8(t1)
    li s8, 80
    li t0, 0
    FAR blt, t0, t2, .Lexit
    li t0, 500000000
    FAR blt, t2, t0, .Lexit
    li a0, 1
    lla a1, .Lclock_a
    SC 113
    OK 81
    lla t1, .Lclock_a
    ld t2, 8(t1)
    li t0, 500000000
    add t2, t2, t0
    li t0, 1000000000
    bltu t2, t0, 1f
    sub t2, t2, t0
    ld t3, 0(t1)
    addi t3, t3, 1
    sd t3, 0(t1)
1:
    sd t2, 8(t1)
    li a0, 0
    lla a1, .Litimer_real
    li a2, 0
    SC 103
    OK 82
    lla t1, .Lremaining
    li t0, 0x777
    sd t0, 0(t1)
    sd t0, 8(t1)
    li a0, 1
    li a1, 1
    lla a2, .Lclock_a
    lla a3, .Lremaining
    SC 115
    CHECK a0, -4, 83
    lla t1, .Lremaining
    ld t2, 0(t1)
    CHECK t2, 0x777, 84
    ld t2, 8(t1)
    CHECK t2, 0x777, 85
    j .Lrestart_wait

# /// SA_RESTART只重启可重启waitid；同一signal无该标志时返回EINTR并保真实child。
.Lrestart_wait:
    ACTION 14, .Lalarm, 0x10000004, 0, 163
    li s5, 0
.Lwait_fork:
    li a0, 17
    li a1, 0
    li a2, 0
    li a3, 0
    li a4, 0
    SC 220
    beqz a0, .Lwait_sleep_child
    li s8, 164
    FAR bge, a0, zero, .Lexit
    mv s3, a0
    li a0, 0
    lla a1, .Litimer_real
    li a2, 0
    SC 103
    OK 165
    bnez s5, .Lwait_eintr
    call .Lwait_child
    lw t2, 20(s2)
    CHECK t2, 3, 166
    ACTION 14, .Lalarm, 4, 0, 167
    li s5, 1
    j .Lwait_fork
.Lwait_eintr:
    li a0, 1
    mv a1, s3
    lla a2, .Linfo
    li a3, 4
    li a4, 0
    SC 95
    CHECK a0, -4, 168
    call .Lwait_child
    lw t2, 20(s2)
    CHECK t2, 4, 169
    j .Lperiodic_real
.Lwait_sleep_child:
    lla a0, .Lsleep_long
    li a1, 0
    SC 101
    OK 170
    j .Lpass

# /// 多个实际REAL周期在blocked mask内只留一实例，取消后领取不补排旧相位。
.Lperiodic_real:
    MASK 0, 0x2000, 210
    li a0, 0
    lla a1, .Litimer_periodic
    li a2, 0
    SC 103
    OK 211
    lla a0, .Lsleep_short
    li a1, 0
    SC 101
    OK 212
    li a0, 0
    lla a1, .Litimer_off
    lla a2, .Litimer_old
    SC 103
    OK 213
    lla t1, .Litimer_old
    ld t2, 0(t1)
    CHECK t2, 0, 214
    ld t2, 8(t1)
    CHECK t2, 20000, 215
    lla a0, .Lmask
    lla a1, .Linfo
    lla a2, .Lzero_time
    li a3, 8
    SC 137
    CHECK a0, 14, 216
    lla t1, .Linfo
    lw t2, 8(t1)
    CHECK t2, 128, 217
    lla a0, .Lmask
    lla a1, .Linfo
    lla a2, .Lzero_time
    li a3, 8
    SC 137
    CHECK a0, -11, 218
    MASK 1, 0x2000, 219
    lw t2, 20(s2)
    CHECK t2, 4, 220
    j .Lpass

# /// child真实stop/continue打断无handler sleep；结束时原500ms期限没有重新加一遍。
.Ldeadline:
    li s5, 0
.Ldeadline_again:
    li a0, 1
    lla a1, .Lclock_a
    SC 113
    OK 86
    li a0, 0x111
    lla a1, .Lchild_stack_top
    li a2, 0
    li a3, 0
    li a4, 0
    SC 220
    beqz a0, .Ldeadline_child
    li s8, 87
    FAR bge, a0, zero, .Lexit
    mv s3, a0
    bnez s5, .Ldeadline_absolute
    lla a0, .Lsleep_long
    li a1, 0
    SC 101
    OK 88
    j .Ldeadline_slept
.Ldeadline_absolute:
    lla t1, .Lclock_a
    lla t2, .Lclock_b
    ld t3, 0(t1)
    ld t4, 8(t1)
    li t0, 500000000
    add t4, t4, t0
    li t0, 1000000000
    bltu t4, t0, 1f
    sub t4, t4, t0
    addi t3, t3, 1
1:
    sd t3, 0(t2)
    sd t4, 8(t2)
    li a0, 1
    li a1, 1
    mv a2, t2
    li a3, 0
    SC 115
    OK 201
.Ldeadline_slept:
    call .Lwait_child
    li a0, 1
    lla a1, .Lclock_b
    SC 113
    OK 89
    call .Lelapsed
    li t0, 450000000
    li s8, 90
    FAR bgeu, a0, t0, .Lexit
    li t0, 650000000
    FAR bltu, a0, t0, .Lexit
    FAR beq, s5, zero, .Lpass
    li s5, 1
    j .Ldeadline_again
.Ldeadline_child:
    lla a0, .Lsleep_short
    li a1, 0
    SC 101
    OK 91
    SEND 19, 92
    lla a0, .Lsleep_short
    li a1, 0
    SC 101
    OK 93
    SEND 18, 94
    j .Lpass

# /// 用户父通过waitid观察真实全成员stop完成，再continue并正常wait退出。
.Lgroup:
    li a0, 0x111
    lla a1, .Lchild_stack_top
    li a2, 0
    li a3, 0
    li a4, 0
    SC 220
    beqz a0, .Lgroup_child
    li s8, 95
    FAR bge, a0, zero, .Lexit
    mv s3, a0
1:
    lw t0, 48(s2)
    fence r, rw
    beqz t0, 1b
    mv a0, s3
    li a1, 19
    SC 129
    OK 96
    li a0, 1
    mv a1, s3
    lla a2, .Linfo
    li a3, 2
    li a4, 0
    SC 95
    OK 97
    lla t1, .Linfo
    lw t2, 8(t1)
    CHECK t2, 5, 98
    lw t2, 16(t1)
    EQ t2, s3, 99
    lw t2, 24(t1)
    CHECK t2, 19, 100
    mv a0, s3
    li a1, 18
    SC 129
    OK 101
    li a0, 1
    mv a1, s3
    lla a2, .Linfo
    li a3, 8
    li a4, 0
    SC 95
    OK 102
    lla t1, .Linfo
    lw t2, 8(t1)
    CHECK t2, 6, 103
    lw t2, 24(t1)
    CHECK t2, 18, 104
    li t0, 1
    sw t0, 32(s2)
    call .Lwait_child
    j .Lpass
.Lgroup_child:
    # child建立真实THREAD成员；VM非THREAD的父不加入停止组。
    SC 172
    mv s0, a0
    li a0, 0x10900
    lla a1, .Lthread_stack_top
    li a2, 0
    li a3, 0
    li a4, 0
    SC 220
    beqz a0, .Lgroup_thread
    li s8, 105
    FAR bge, a0, zero, .Lexit
    li t0, 1
    fence rw, w
    sw t0, 48(s2)
.Lgroup_thread:
1:
    lw t0, 32(s2)
    fence r, rw
    beqz t0, 1b
    j .Lpass

# /// VM与SIGHAND共享仍有独立组pending，child既不继承旧pending也不能领取父实例。
.Lvm_pending:
    MASK 0, 0x200, 106
    QUEUE 10, 0x91, 107
    li a0, 0x911
    lla a1, .Lchild_stack_top
    li a2, 0
    li a3, 0
    li a4, 0
    SC 220
    beqz a0, .Lvm_child
    li s8, 108
    FAR bge, a0, zero, .Lexit
    mv s3, a0
1:
    lw t0, 44(s2)
    fence r, rw
    beqz t0, 1b
    TAKE 10, 0x91, 109
    li t0, 1
    sw t0, 36(s2)
    call .Lwait_child
    j .Lpass
.Lvm_child:
    lla a0, .Lmask
    lla a1, .Linfo
    lla a2, .Lzero_time
    li a3, 8
    SC 137
    CHECK a0, -11, 114
    SC 172
    mv s0, a0
    SEND 10, 115
    li s4, 0
    call .Ltake_sender
    li t0, 1
    fence rw, w
    sw t0, 44(s2)
1:
    lw t0, 36(s2)
    fence r, rw
    beqz t0, 1b
    lla a0, .Lmask
    lla a1, .Linfo
    lla a2, .Lzero_time
    li a3, 8
    SC 137
    CHECK a0, -11, 116
    j .Lpass

# /// 父只在真实wait消费并确认child离栈后检查退出信号。
.Lexit_signal:
    addi s9, sp, -16
    sd zero, 0(s9)
    sd zero, 8(s9)
    MASK 0, 0x10000, 2
    li s4, 1
    li s5, 0
    li s6, 0x100
.Lexit_signal_clone:
    mv a0, s6
    lla a1, .Lchild_stack_top
    li a2, 0
    li a3, 0
    li a4, 0
    SC 220
    FAR bne, a0, zero, .Lpass
    li s8, 3
    FAR bge, a0, zero, .Lexit
    sd a0, 0(s9)
    fence rw, w
    sd s4, 8(s9)
    addi t3, s4, 1
1:
    ld t2, 8(s9)
    fence r, rw
    bne t2, t3, 1b
    lla a0, .Lmask
    li a1, 8
    SC 136
    OK 4
    lla t1, .Lmask
    ld t2, 0(t1)
    li t0, 0x10000
    and t2, t2, t0
    addi s8, s4, 5
    FAR beq, t2, s5, .Lexit
    FAR beq, s5, zero, .Lpass
    li s4, 3
    li s5, 0x10000
    li s6, 0x111
    j .Lexit_signal_clone

# /// STOP完成后父撤销参数页；CONT后的真实重放必须恢复并消费旧mask。
.Lsuspend_fault:
    addi s9, sp, -16
    sd zero, 0(s9)
    sd zero, 8(s9)
    li s4, 0x43001000
    mv a0, s4
    li a1, 4096
    li a2, 3
    li a3, 0x32
    li a4, -1
    li a5, 0
    SC 222
    CHECK a0, 0x43001000, 20
    li t0, 0x800
    sd t0, 0(s4)
    li a0, 0x111
    lla a1, .Lchild_stack_top
    li a2, 0
    li a3, 0
    li a4, 0
    SC 220
    FAR bne, a0, zero, .Lsuspend_fault_child
    li s8, 21
    FAR bge, a0, zero, .Lexit
    mv s3, a0
    sd s3, 0(s9)
    li t0, 1
    fence rw, w
    sd t0, 8(s9)
1:
    ld t2, 8(s9)
    fence r, rw
    li t0, 2
    bne t2, t0, 1b
    mv a0, s4
    li a1, 4096
    SC 215
    OK 22
    li t0, 3
    fence rw, w
    sd t0, 8(s9)
    call .Lwait_child
    j .Lpass
.Lsuspend_fault_child:
    SC 172
    mv s0, a0
    MASK 2, 0x10200, 23
    mv a0, s4
    li a1, 8
    SC 133
    CHECK a0, -14, 24
    li a0, 0
    li a1, 0
    lla a2, .Lmask
    li a3, 8
    SC 135
    OK 25
    lla t1, .Lmask
    ld t2, 0(t1)
    CHECK t2, 0x10200, 26
    MASK 2, 0x200, 27
    ACTION 12, .Lsuspend_noop, 4, 0, 28
    SEND 12, 29
    li a0, 0
    li a1, 0
    lla a2, .Lmask
    li a3, 8
    SC 135
    OK 30
    lla t1, .Lmask
    ld t2, 0(t1)
    CHECK t2, 0x200, 31
    j .Lpass
.Lsuspend_noop:
    ret

# /// 同一OFD读责任完成后才处理pending；每字节核对盘内容和真实游标。
.Lread:
    ACTION 14, .Lalarm, 4, 0, 117
    li a0, -100
    lla a1, .Lfile_path
    li a2, 0
    li a3, 0
    SC 56
    CHECK a0, 0, 118
    mv s3, a0
    mv a0, s3
    lla a1, .Lread_buffer
    li a2, 65536
    li t0, 0x51a2
    sd t0, -8(sp)
    fence rw, rw
    SC 63
    CHECK a0, 65536, 120
    lw t2, 20(s2)
    CHECK t2, 1, 171
    lla t1, .Lread_buffer
    li t3, 0
1:
    lbu t2, 0(t1)
    srli t4, t3, 9
    li t0, 17
    mul t4, t4, t0
    andi t5, t3, 511
    add t4, t4, t5
    addi t4, t4, 37
    andi t4, t4, 255
    EQ t2, t4, 121
    addi t1, t1, 1
    addi t3, t3, 1
    li t0, 65536
    bltu t3, t0, 1b
    mv a0, s3
    li a1, 0
    li a2, 1
    SC 62
    CHECK a0, 65536, 122
    mv a0, s3
    SC 57
    OK 123
    j .Lpass

# /// CPU clock/times以真实执行账核对USER_HZ，offCPU睡眠不能算成进程CPU时间。
.Lcpu_time:
    ACTION 26, .Lvirtual, 4, 0, 124
    ACTION 27, .Lprof, 4, 0, 125
    li a0, 2
    lla a1, .Lcpu_a
    SC 113
    OK 126
    lla a0, .Ltms_a
    SC 153
    li s8, 127
    FAR bge, a0, zero, .Lexit
    li a0, 1
    lla a1, .Litimer_cpu
    li a2, 0
    SC 103
    OK 128
    li a0, 2
    lla a1, .Litimer_cpu
    li a2, 0
    SC 103
    OK 129
1:
    lw t1, 24(s2)
    lw t2, 28(s2)
    and t1, t1, t2
    beqz t1, 1b
    # 原time/tms的stime只有kernel执行累计至少一tick后才前进。
    li s6, 1000000
.Lkernel_account:
    lla a0, .Lremaining
    li a1, 0
    SC 169
    OK 172
    lla a0, .Ltms_b
    SC 153
    lla t1, .Ltms_a
    ld t2, 8(t1)
    lla t1, .Ltms_b
    ld t3, 8(t1)
    bltu t2, t3, .Lkernel_accounted
    addi s6, s6, -1
    bnez s6, .Lkernel_account
    li s8, 173
    j .Lexit
.Lkernel_accounted:
    li a0, 2
    lla a1, .Lcpu_b
    SC 113
    OK 130
    lla a0, .Ltms_b
    SC 153
    li s8, 131
    FAR bge, a0, zero, .Lexit
    lla t1, .Ltms_a
    ld t2, 0(t1)
    lla t1, .Ltms_b
    ld t3, 0(t1)
    li s8, 174
    FAR bltu, t2, t3, .Lexit
    # 两个分账各自截成USER_HZ tick，两次syscall的取样边界允许正负两个tick误差。
    lla t1, .Lcpu_a
    lla t2, .Lcpu_b
    call .Ldiff
    mv s4, a0
    li s8, 132
    li t0, 10000000
    FAR bgeu, s4, t0, .Lexit
    lla t1, .Ltms_a
    ld t2, 0(t1)
    ld t3, 8(t1)
    add t2, t2, t3
    lla t1, .Ltms_b
    ld t3, 0(t1)
    ld t4, 8(t1)
    add t3, t3, t4
    sub t3, t3, t2
    ld t0, 56(s2)
    li t1, 1000000000
    divu t0, t1, t0
    divu t4, s4, t0
    sub t4, t4, t3
    addi t4, t4, 2
    li s8, 133
    li t0, 5
    FAR bltu, t4, t0, .Lexit
    lla a0, .Lsleep_short
    li a1, 0
    SC 101
    OK 134
    li a0, 2
    lla a1, .Lcpu_a
    SC 113
    OK 135
    lla t1, .Lcpu_b
    lla t2, .Lcpu_a
    call .Ldiff
    li t0, 50000000
    li s8, 136
    FAR bltu, a0, t0, .Lexit
    # 同组另一实际成员继续用户计算，process clock推进而睡眠线程clock保持offCPU。
    li a0, 2
    lla a1, .Lcpu_a
    SC 113
    OK 202
    li a0, 3
    lla a1, .Lclock_a
    SC 113
    OK 203
    li a0, 0x10900
    lla a1, .Lthread_stack_top
    li a2, 0
    li a3, 0
    li a4, 0
    SC 220
    beqz a0, .Lcpu_member
    li s8, 204
    FAR bge, a0, zero, .Lexit
    lla a0, .Lsleep_short
    li a1, 0
    SC 101
    OK 205
    li a0, 2
    lla a1, .Lcpu_b
    SC 113
    OK 206
    li a0, 3
    lla a1, .Lclock_b
    SC 113
    OK 207
    lla t1, .Lcpu_a
    lla t2, .Lcpu_b
    call .Ldiff
    li t0, 50000000
    li s8, 208
    FAR bgeu, a0, t0, .Lexit
    call .Lelapsed
    li t0, 50000000
    li s8, 209
    FAR bltu, a0, t0, .Lexit
    li t0, 1
    fence rw, w
    sw t0, 32(s2)
    j .Lpass
.Lcpu_member:
1:
    lw t0, 32(s2)
    fence r, rw
    beqz t0, 1b
    j .Lpass

# /// 真实父waitid消费退出并核child status；失败编号由child原状态传到父。
.Lwait_child:
    li a0, 1
    mv a1, s3
    lla a2, .Linfo
    li a3, 4
    li a4, 0
    SC 95
    OK 137
    lla t1, .Linfo
    lw t2, 8(t1)
    CHECK t2, 1, 138
    lw t2, 16(t1)
    EQ t2, s3, 139
    lw t2, 24(t1)
    mv s8, t2
    FAR beq, t2, zero, .Lexit
    ret

# /// 两次monotonic读数化成经过纳秒，时钟只用于边界而非猜调度状态。
.Lelapsed:
    lla t1, .Lclock_a
    lla t2, .Lclock_b
.Ldiff:
    ld t3, 0(t1)
    ld t4, 0(t2)
    sub t4, t4, t3
    li t0, 1000000000
    mul t4, t4, t0
    ld t3, 8(t1)
    ld t5, 8(t2)
    sub t5, t5, t3
    add a0, t4, t5
    ret

# /// 嵌套处理仅保存当前frame SP，修改动作不会重选当前实例。
.Lnodefer:
    addi sp, sp, -32
    sd ra, 0(sp)
    sd a2, 8(sp)
    lw t1, 0(s2)
    addi t1, t1, 1
    sw t1, 0(s2)
    li t0, 1
    bne t1, t0, 1f
    li t0, 1
    sw t0, 8(s2)
    SEND 12, 141
    lw t2, 4(s2)
    CHECK t2, 0, 142
    SEND 10, 143
    li t0, 0
    sw t0, 8(s2)
1:
    ld ra, 0(sp)
    addi sp, sp, 32
    ret
.Lusr2:
    lw t2, 8(s2)
    CHECK t2, 0, 144
    lw t1, 4(s2)
    addi t1, t1, 1
    sw t1, 4(s2)
    ret
.Lcount:
    lw t1, 12(s2)
    addi t1, t1, 1
    sw t1, 12(s2)
    ret

# /// 已经选取的handler只完成本次实例；更新动作仅影响下一次发送。
.Lswap_action:
    addi sp, sp, -16
    sd ra, 0(sp)
    ACTION 10, .Lcount, 4, 0, 175
    lw t1, 12(s2)
    addi t1, t1, 1
    sw t1, 12(s2)
    ld ra, 0(sp)
    addi sp, sp, 16
    ret
.Ledit_frame:
    CHECK a0, 10, 145
    lw t2, 0(a1)
    CHECK t2, 10, 146
    lw t2, 8(a1)
    CHECK t2, 0, 176
    lw t2, 16(a1)
    EQ t2, s0, 177
    lw t2, 688(a2)
    CHECK t2, 0x20, 178
    lw t2, 948(a2)
    CHECK t2, 0, 179
    lw t2, 952(a2)
    CHECK t2, 0, 180
    lw t2, 956(a2)
    CHECK t2, 0, 181
    addi t1, a2, 432
    lla t2, .Lfp_values
    li t3, 32
1:
    ld t4, 0(t1)
    ld t5, 0(t2)
    EQ t4, t5, 182
    addi t1, t1, 8
    addi t2, t2, 8
    addi t3, t3, -1
    bnez t3, 1b
    # ucontext固定mcontext偏移176，sp位于第三个机器字。
    lla t1, .Ledited_pc
    sd t1, 176(a2)
    lla t1, .Lnew_stack_top
    sd t1, 192(a2)
    lla t1, .Lfp_output
    FP_LOAD
    li t0, 0
    csrw fcsr, t0
    li tp, 0
    ret
.Lalt_outer:
    li s8, 147
    FAR bltu, s4, sp, .Lexit
    li t0, 16384
    add t0, s4, t0
    FAR bgeu, t0, sp, .Lexit
    sd sp, 40(s2)
    addi sp, sp, -32
    sd ra, 0(sp)
    li a0, 0
    lla a1, .Lold_stack
    SC 132
    OK 148
    lla t1, .Lold_stack
    lw t2, 8(t1)
    CHECK t2, 1, 149
    lla a0, .Laltstack
    li a1, 0
    SC 132
    CHECK a0, -1, 150
    SEND 12, 151
    ld ra, 0(sp)
    addi sp, sp, 32
    ret
.Lalt_inner:
    li s8, 152
    FAR bltu, s4, sp, .Lexit
    ld t1, 40(s2)
    FAR bltu, sp, t1, .Lexit
    ld t2, 16(a2)
    EQ t2, s4, 224
    lw t2, 24(a2)
    CHECK t2, 0, 225
    ld t2, 32(a2)
    CHECK t2, 16384, 226
    li t0, 2
    sw t0, 16(s2)
    ret
# /// 只读取Linux固定UAPI字节；恢复地址保留在s9，CHECK修改t0/s8不能污染它。
.Lsynchronous_handler:
    EQ a0, s3, 240
    EQ a1, sp, 241
    addi t1, sp, 128
    EQ a2, t1, 242
    lw t2, 0(a1)
    EQ t2, s3, 243
    lw t2, 4(a1)
    CHECK t2, 0, 244
    lw t2, 8(a1)
    EQ t2, s5, 245
    ld t2, 16(a1)
    EQ t2, s4, 246
    # uc+176为mcontext第0个机器字，保存的必须是坏指令本身的PC。
    ld t2, 176(a2)
    EQ t2, s6, 247
    lw t2, 0(s2)
    addi t2, t2, 1
    sw t2, 0(s2)
    sd s9, 176(a2)
    ret
.Lcorrupt:
    li t0, 107
    bne s7, t0, 1f
    li t0, -4096
    sd t0, 176(a2)
    ret
1:
    li t0, 1
    sw t0, 948(a2)
    ret
.Lalarm:
    lw t1, 20(s2)
    addi t1, t1, 1
    sw t1, 20(s2)
    ret
.Lvirtual:
    li t0, 1
    sw t0, 24(s2)
    ret
.Lprof:
    li t0, 1
    sw t0, 28(s2)
    ret
.Lpass:
    li s8, 0
.Lexit:
    mv a0, s8
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
