.text

# 普通静态用户程序；所有共享状态先Release，读取后Acquire。
# 条件跳转只跨宏内短片段，远目标使用无条件跳转。
.macro Z
    move $a0, $zero
    move $a1, $zero
    move $a2, $zero
    move $a3, $zero
    move $a4, $zero
    move $a5, $zero
.endm
.macro SC number
    li.d $a7, \number
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
.macro POS reg, code
    li.d $s8, \code
    blt $zero, \reg, 991f
    b .Lexit
991:
.endm
.macro MODE value, target
    li.d $t0, \value
    bne $s7, $t0, 991f
    b \target
991:
.endm
.macro STORE offset, value
    li.d $t0, \value
    dbar 0
    st.w $t0, $s0, \offset
.endm
.macro WAIT offset, value
    li.d $t1, \value
991:
    ld.wu $t0, $s0, \offset
    dbar 0
    bltu $t0, $t1, 991b
.endm
.macro WORD offset, value, code
    ld.w $t2, $s0, \offset
    dbar 0
    CHECK $t2, \value, \code
.endm
.macro MAP flags, code
    Z
    li.d $a0, 0x44000000
    li.d $a1, 4096
    li.d $a2, 3
    li.d $a3, \flags
    li.d $a4, -1
    SC 222
    CHECK $a0, 0x44000000, \code
    move $s0, $a0
.endm
# LA raw ABI固定为 flags、stack、parent_tid、child_tid、tls。
.macro CLONE flags, stack, ptid, ctid, target, code
    Z
    li.d $a0, \flags
    .if (\flags & 0x100)
        la.local $a1, \stack
    .endif
    .if \ptid >= 0
        addi.d $a2, $s0, \ptid
    .endif
    .if \ctid >= 0
        addi.d $a3, $s0, \ctid
    .endif
    SC 220
    li.d $s8, \code
    bge $a0, $zero, 991f
    b .Lexit
991:
    bnez $a0, 992f
    b \target
992:
    move $s3, $a0
.endm
.macro EXEC mode, code
    Z
    la.local $a0, .Lself
    la.local $a1, .Largv_\mode
    la.local $a2, .Lenv
    SC 221
    li.d $s8, \code
    b .Lexit
.endm
.macro MASK how, code
    Z
    li.d $a0, \how
    la.local $a1, .Lusr1_mask
    li.d $a3, 8
    SC 135
    CHECK $a0, 0, \code
.endm
.macro QUERY_ACTION sig, code
    Z
    li.d $a0, \sig
    la.local $a2, .Lold_action
    li.d $a3, 8
    SC 134
    CHECK $a0, 0, \code
.endm
.macro STR reg, label, code
    move $t1, \reg
    la.local $t2, \label
990:
    ld.bu $t3, $t1, 0
    ld.bu $t4, $t2, 0
    EQ $t3, $t4, \code
    beqz $t3, 992f
    addi.d $t1, $t1, 1
    addi.d $t2, $t2, 1
    b 990b
992:
.endm
.macro REAP code
    Z
    move $a0, $s3
    la.local $a1, .Lstatus
    SC 260
    EQ $a0, $s3, \code
    la.local $t1, .Lstatus
    ld.w $t2, $t1, 0
    CHECK $t2, 0, \code+1
.endm

# /// 直接读取真实ELF初始栈，以单字符选择生命周期和生产exec路径。
.global _start
.type _start, @function
_start:
    andi $t2, $sp, 15
    CHECK $t2, 0, 1
    ld.d $t2, $sp, 0
    CHECK $t2, 2, 2
    ld.d $t1, $sp, 16
    POS $t1, 3
    ld.bu $s7, $t1, 0
    ld.bu $t2, $t1, 1
    CHECK $t2, 0, 4
    ld.d $t2, $sp, 24
    CHECK $t2, 0, 5
    Z
    SC 172
    POS $a0, 6
    move $s1, $a0
    Z
    SC 178
    POS $a0, 7
    move $s2, $a0
    MODE 78, .Lnew
    MODE 88, .Lnew
    MODE 86, .Lnew
    MODE 85, .Lnew
    MODE 115, .Lsegv
    MODE 98, .Lbus
    MODE 105, .Lill
    MAP 0x31, 8
    STORE 24, -4096
    STORE 28, -4096
    MODE 110, .Lnonleader
    MODE 99, .Lcompete
    MODE 120, .Lexternal
    MODE 118, .Lvfork
    MODE 122, .Lvfork
    MODE 107, .Lvfork
    MODE 103, .Lgroup
    MODE 112, .Lparent
    MODE 119, .Lwait
    MODE 117, .Lproduction
    li.d $s8, 9
    b .Lexit

# /// 新映像验证argv/envp、单一leader身份及旧控制映射已撤销。
.Lnew:
    EQ $s1, $s2, 10
    ld.d $t5, $sp, 8
    STR $t5, .Lself, 11
    ld.d $t5, $sp, 32
    POS $t5, 12
    STR $t5, .Lexec_env, 13
    ld.d $t2, $sp, 40
    CHECK $t2, 0, 14
    MODE 86, .Lnew_vfork
    Z
    li.d $a0, 0x44000044
    li.d $a1, 128
    SC 98
    CHECK $a0, -14, 15
    MODE 78, .Lnew_gate
    b .Lnew_resources
.Lnew_gate:
    MAP 0x32, 16
    STORE 8, 100
    WAIT 12, 100
    b .Lpass

# /// 非leader的真实exec驱逐阻塞leader；竞争双方都调用execve。
.Lnonleader:
    CLONE 0x110900, .Lstack0_top, 0, -1, .Lnonleader_child, 17
    WAIT 40, 1
    STORE 8, 1
    Z
    addi.d $a0, $s0, 68
    li.d $a1, 128
    SC 98
    li.d $s8, 18
    b .Lexit
.Lnonleader_child:
    STORE 40, 1
    WAIT 12, 1
    EXEC N, 19
.Lcompete:
    CLONE 0x110900, .Lstack0_top, 0, -1, .Lcompete_child, 20
    WAIT 40, 1
    STORE 8, 1
    WAIT 12, 1
    EXEC N, 21
.Lcompete_child:
    STORE 40, 1
    WAIT 12, 1
    EXEC N, 22

# /// 先建立真实文件、动作和mask；失败exec必须保留原身份、映射和OFD。
.Lexternal:
.Lproduction:
    Z
    li.d $a0, -100
    la.local $a1, .Ldata_path
    li.d $a2, 0x80000
    SC 56
    CHECK $a0, 0, 23
    Z
    li.d $a0, -100
    la.local $a1, .Ldata_path
    SC 56
    CHECK $a0, 1, 24
    Z
    li.d $a0, 1
    li.d $a1, 2
    SC 62
    CHECK $a0, 2, 25
    Z
    la.local $a0, .Ltest_dir
    SC 49
    CHECK $a0, 0, 26
    Z
    li.d $a0, 10
    la.local $a1, .Lcatch_action
    li.d $a3, 8
    SC 134
    CHECK $a0, 0, 27
    Z
    li.d $a0, 12
    la.local $a1, .Lignore_action
    li.d $a3, 8
    SC 134
    CHECK $a0, 0, 28
    MASK 0, 29
    MODE 117, .Lproduction_preflight
.Lbad_exec:
    STORE 128, 0x1234
    Z
    la.local $a0, .Lmissing
    la.local $a1, .Largv_X
    la.local $a2, .Lenv
    SC 221
    CHECK $a0, -2, 30
    Z
    la.local $a0, .Lself
    li.d $a1, 0x70000000
    la.local $a2, .Lenv
    SC 221
    CHECK $a0, -14, 31
    Z
    la.local $a0, .Lunsupported
    la.local $a1, .Largv_X
    la.local $a2, .Lenv
    SC 221
    CHECK $a0, -8, 32
    WORD 128, 0x1234, 33
    Z
    SC 172
    EQ $a0, $s1, 34
    Z
    SC 178
    EQ $a0, $s2, 35
    Z
    li.d $a0, 1
    li.d $a2, 1
    SC 62
    CHECK $a0, 2, 36
    MODE 117, .Lproduction_exec
    CLONE 0x1300f11, .Lstack0_top, 0, 64, .Lexternal_child, 37
    WAIT 40, 1
    STORE 8, 1
    WAIT 12, 1
    WAIT 72, 1
    Z
    move $a0, $s3
    li.d $a1, 10
    SC 129
    CHECK $a0, 0, 38
    STORE 76, 1
    REAP 39
    WORD 64, 0, 41
    Z
    li.d $a2, 1
    SC 62
    CHECK $a0, 0, 42
    QUERY_ACTION 10, 43
    la.local $t1, .Lold_action
    ld.d $t2, $t1, 0
    la.local $t3, .Lhandler
    EQ $t2, $t3, 44
    ld.d $t2, $t1, 8
    CHECK $t2, 4, 45
    ld.d $t2, $t1, 16
    CHECK $t2, 0x800, 46
    Z
    li.d $a0, 1
    li.d $a2, 1
    SC 62
    CHECK $a0, 3, 47
    b .Lpass
.Lexternal_child:
    Z
    addi.d $a0, $s0, 64
    SC 96
    POS $a0, 48
    STORE 40, 1
    WAIT 12, 1
    bl .Lprepare_timer_stack
    STORE 72, 1
    WAIT 76, 1
    EXEC X, 49
.Lproduction_preflight:
    bl .Lprepare_timer_stack
    Z
    move $a0, $s1
    li.d $a1, 10
    SC 129
    CHECK $a0, 0, 50
    b .Lbad_exec
.Lproduction_exec:
    EXEC U, 51

# /// 备用栈和REAL定时器必须从真实用户调用建立，exec后分别禁用和保留。
.Lprepare_timer_stack:
    move $s6, $ra
    Z
    la.local $a0, .Lalt_desc
    SC 132
    CHECK $a0, 0, 52
    Z
    la.local $a1, .Ltimer_on
    SC 103
    CHECK $a0, 0, 53
    jr $s6
.Lnew_resources:
    Z
    li.d $a2, 1
    SC 62
    CHECK $a0, -9, 54
    Z
    li.d $a0, 1
    li.d $a2, 1
    SC 62
    CHECK $a0, 2, 55
    Z
    li.d $a0, 1
    la.local $a1, .Lbyte
    li.d $a2, 1
    SC 63
    CHECK $a0, 1, 56
    Z
    li.d $a0, 1
    li.d $a2, 1
    SC 62
    CHECK $a0, 3, 57
    QUERY_ACTION 10, 58
    la.local $t1, .Lold_action
    ld.d $t2, $t1, 0
    CHECK $t2, 0, 59
    ld.d $t2, $t1, 8
    CHECK $t2, 0, 60
    ld.d $t2, $t1, 16
    CHECK $t2, 0, 61
    QUERY_ACTION 12, 62
    la.local $t1, .Lold_action
    ld.d $t2, $t1, 0
    CHECK $t2, 1, 63
    ld.d $t2, $t1, 8
    CHECK $t2, 0, 201
    ld.d $t2, $t1, 16
    CHECK $t2, 0, 202
    Z
    la.local $a2, .Lold_mask
    li.d $a3, 8
    SC 135
    CHECK $a0, 0, 64
    la.local $t1, .Lold_mask
    ld.d $t2, $t1, 0
    CHECK $t2, 0x200, 65
    Z
    la.local $a0, .Lold_mask
    li.d $a1, 8
    SC 136
    CHECK $a0, 0, 66
    la.local $t1, .Lold_mask
    ld.d $t2, $t1, 0
    CHECK $t2, 0x200, 67
    Z
    la.local $a0, .Lusr1_mask
    la.local $a1, .Linfo
    la.local $a2, .Lzero_time
    li.d $a3, 8
    SC 137
    CHECK $a0, 10, 68
    Z
    la.local $a1, .Lold_stack
    SC 132
    CHECK $a0, 0, 69
    la.local $t1, .Lold_stack
    ld.w $t2, $t1, 8
    CHECK $t2, 2, 70
    Z
    la.local $a1, .Ltimer_off
    la.local $a2, .Ltimer_old
    SC 103
    CHECK $a0, 0, 71
    la.local $t1, .Ltimer_old
    ld.d $t2, $t1, 16
    ld.d $t3, $t1, 24
    or $t2, $t2, $t3
    POS $t2, 72
    Z
    li.d $a0, -100
    la.local $a1, .Lrelative_data
    SC 56
    CHECK $a0, 0, 73
    MODE 85, .Lnew_production
    b .Lnew_gate
.Lnew_production:
    MAP 0x32, 74
    li.d $s7, 122
    b .Lvfork

# /// vfork父只能在旧MM使用结束后返回；新V在收到父USR1之前不能退出。
.Lvfork:
    MASK 0, 75
    CLONE 0x1204111, .Lstack0_top, -1, 64, .Lvfork_child, 76
    WORD 64, 0, 77
    MODE 122, .Lvfork_reap
    Z
    move $a0, $s3
    li.d $a1, 10
    SC 129
    CHECK $a0, 0, 78
.Lvfork_reap:
    REAP 79
    b .Lpass
.Lvfork_child:
    Z
    SC 178
    POS $a0, 81
    dbar 0
    st.d $a0, $s0, 0
    MODE 122, .Lpass
    STORE 8, 1
    WAIT 12, 1
    MODE 107, .Lpass
    EXEC V, 82
.Lnew_vfork:
    Z
    li.d $a0, 10
    la.local $a1, .Lcatch_action
    li.d $a3, 8
    SC 134
    CHECK $a0, 0, 83
    MASK 1, 84
    la.local $t1, .Lhandled
1:
    ld.w $t2, $t1, 0
    dbar 0
    beqz $t2, 1b
    CHECK $t2, 1, 85
    b .Lpass

# /// exit_group驱逐真实阻塞成员，不允许成员从futex正常返回。
.Lgroup:
    CLONE 0x110900, .Lstack0_top, 0, -1, .Lgroup_child, 86
    WAIT 40, 1
    STORE 8, 1
    WAIT 12, 1
    Z
    li.d $a0, 5
    SC 94
    li.d $s8, 87
    b .Lexit
.Lgroup_child:
    STORE 40, 1
    Z
    addi.d $a0, $s0, 68
    li.d $a1, 128
    SC 98
    li.d $s8, 88
    b .Lexit

# /// leader先退出不夺走同组父资格；最后成员退出后孤儿转亲内核根。
.Lparent:
    Z
    SC 155
    POS $a0, 90
    move $s4, $a0
    Z
    SC 156
    POS $a0, 91
    move $s5, $a0
    CLONE 0x100011, .Lstack0_top, 0, -1, .Lparent_A, 92
    WAIT 88, 1
    Z
    move $a0, $s3
    move $a1, $s4
    SC 154
    CHECK $a0, -1, 93
    STORE 92, 1
    WAIT 80, 1
    Z
    move $a0, $s3
    la.local $a1, .Lstatus
    li.d $a2, 1
    SC 260
    CHECK $a0, 0, 94
    Z
    ld.d $a0, $s0, 16
    li.d $a2, 1
    SC 260
    CHECK $a0, -10, 95
    STORE 8, 1
    WAIT 12, 1
    REAP 96
    STORE 8, 2
    WAIT 84, 1
    Z
    ld.d $a0, $s0, 16
    SC 260
    CHECK $a0, -10, 98
    STORE 8, 3
    WAIT 12, 4
    move $s6, $s3
    CLONE 0x100011, .Lstack0_top, 16, -1, .Lparent_C, 99
    EQ $s3, $s6, 100
    WAIT 108, 1
    STORE 8, 4
    WAIT 12, 5
    REAP 101
    b .Lpass
.Lparent_A:
    Z
    SC 172
    move $s3, $a0
    Z
    SC 173
    EQ $a0, $s1, 103
    Z
    SC 155
    EQ $a0, $s4, 104
    Z
    SC 156
    EQ $a0, $s5, 105
    Z
    SC 154
    CHECK $a0, 0, 106
    Z
    SC 155
    EQ $a0, $s3, 107
    Z
    SC 157
    CHECK $a0, -1, 108
    Z
    move $a1, $s4
    SC 154
    CHECK $a0, 0, 109
    Z
    SC 157
    EQ $a0, $s3, 110
    Z
    SC 155
    EQ $a0, $s3, 111
    Z
    SC 156
    EQ $a0, $s3, 112
    STORE 88, 1
    WAIT 92, 1
    dbar 0
    st.w $s3, $s0, 104
    Z
    addi.d $a0, $s0, 104
    SC 96
    EQ $a0, $s3, 113
    CLONE 0x100011, .Lstack0_top, 16, -1, .Lparent_B, 114
    WAIT 72, 1
    CLONE 0x110900, .Lstack1_top, 32, -1, .Lparent_peer, 115
    WAIT 96, 1
    b .Lpass
.Lparent_peer:
    STORE 96, 1
    WAIT 12, 1
    STORE 100, 1
    b .Lpass
.Lparent_B:
    Z
    SC 173
    EQ $a0, $s3, 116
    STORE 72, 1
1:
    ld.w $t2, $s0, 104
    dbar 0
    bnez $t2, 1b
    Z
    SC 173
    EQ $a0, $s3, 117
    dbar 0
    st.d $a0, $s0, 48
    STORE 80, 1
    WAIT 8, 2
    Z
    SC 173
    CHECK $a0, 0, 118
    dbar 0
    st.d $a0, $s0, 48
    STORE 84, 1
    WAIT 12, 3
    b .Lpass
.Lparent_C:
    Z
    SC 172
    EQ $a0, $s6, 119
    Z
    SC 173
    EQ $a0, $s1, 120
    Z
    SC 155
    EQ $a0, $s4, 121
    Z
    SC 156
    EQ $a0, $s5, 122
    STORE 108, 1
    WAIT 12, 5
    b .Lpass

# /// WNOHANG不制造事件或改写wait4输出；两个WNOWAIT观察相同最终快照。
.Lwait:
    CLONE 0x100011, .Lstack0_top, 0, -1, .Lwait_cpu, 140
    CLONE 0x1310900, .Lstack0_top, 32, 96, .Lwait_worker, 141
    ld.d $s3, $s0, 0
    dbar 0
    addi.d $s4, $s0, 128
    addi.d $s5, $s0, 256
    addi.d $s6, $s0, 112
    bl .Lnohang
    WAIT 72, 1
    STORE 76, 1
    STORE 8, 1
    bl .Lobserve
    WAIT 80, 1
    addi.d $s5, $s0, 544
    bl .Lobserve
    addi.d $t1, $s0, 256
    addi.d $t2, $s0, 400
    addi.d $t3, $s0, 544
    li.d $t4, 18
1:
    ld.d $t5, $t1, 0
    ld.d $t6, $t2, 0
    EQ $t5, $t6, 142
    ld.d $t6, $t3, 0
    EQ $t5, $t6, 143
    addi.d $t1, $t1, 8
    addi.d $t2, $t2, 8
    addi.d $t3, $t3, 8
    addi.d $t4, $t4, -1
    bnez $t4, 1b
    ld.d $t2, $s0, 256
    ld.d $t3, $s0, 264
    or $t2, $t2, $t3
    POS $t2, 144
    addi.d $t1, $s0, 288
    li.d $t4, 14
1:
    ld.d $t2, $t1, 0
    CHECK $t2, 0, 169
    addi.d $t1, $t1, 8
    addi.d $t4, $t4, -1
    bnez $t4, 1b
    li.d $t0, 1000000
    ld.d $t2, $s0, 264
    li.d $s8, 170
    bltu $t2, $t0, 1f
    b .Lexit
1:
    ld.d $t2, $s0, 280
    li.d $s8, 171
    bltu $t2, $t0, 1f
    b .Lexit
1:
    STORE 84, 1
    Z
    SC 260
    li.d $t0, -10
    beq $a0, $t0, 1f
    EQ $a0, $s3, 145
    move $a0, $zero
1:
    dbar 0
    st.w $a0, $s0, 24
    WAIT 88, 1
    ld.w $t2, $s0, 24
    ld.w $t3, $s0, 28
    dbar 0
    beqz $t2, 2f
    CHECK $t2, -10, 147
    CHECK $t3, 0, 148
    b 3f
2:
    CHECK $t3, -10, 149
3:
    WORD 40, 2, 150
1:
    ld.w $t2, $s0, 96
    dbar 0
    bnez $t2, 1b
    CLONE 0x100011, .Lstack0_top, 16, -1, .Lpass, 151
    Z
    li.d $a0, 1
    move $a1, $s3
    li.d $a2, 0x70000000
    li.d $a3, 4
    SC 95
    CHECK $a0, -14, 152
    Z
    li.d $a0, 1
    move $a1, $s3
    li.d $a3, 4
    SC 95
    CHECK $a0, -10, 153
    Z
    move $a0, $s3
    SC 260
    CHECK $a0, -10, 154
    b .Lpass
.Lwait_cpu:
    Z
    addi.d $a0, $s0, 12
    SC 98
    CHECK $a0, 0, 155
    li.d $t1, 20000000
    li.d $t2, 1
1:
    add.d $t2, $t2, $t1
    xor $t2, $t2, $t1
    addi.d $t1, $t1, -1
    bnez $t1, 1b
    dbar 0
    st.d $t2, $s0, 120
    b .Lpass
.Lwait_worker:
    ld.d $s3, $s0, 0
    dbar 0
    addi.d $s4, $s0, 704
    addi.d $s5, $s0, 400
    addi.d $s6, $s0, 116
    bl .Lnohang
    STORE 72, 1
    WAIT 76, 1
    STORE 40, 1
    bl .Lobserve
    STORE 80, 1
    WAIT 84, 1
    Z
    li.d $a0, 1
    move $a1, $s3
    li.d $a3, 4
    SC 95
    li.d $t0, -10
    beq $a0, $t0, 1f
    CHECK $a0, 0, 156
1:
    dbar 0
    st.w $a0, $s0, 28
    STORE 40, 2
    STORE 88, 1
    b .Lpass

# /// 无事件时逐字验证全部siginfo清零，status/rusage保持原哨兵。
.Lnohang:
    move $s2, $ra
    move $t1, $s4
    li.d $t2, 16
    li.d $t3, 0x12345678
1:
    st.d $t3, $t1, 0
    addi.d $t1, $t1, 8
    addi.d $t2, $t2, -1
    bnez $t2, 1b
    Z
    li.d $a0, 1
    move $a1, $s3
    move $a2, $s4
    li.d $a3, 5
    SC 95
    CHECK $a0, 0, 157
    move $t1, $s4
    li.d $t4, 16
1:
    ld.d $t2, $t1, 0
    CHECK $t2, 0, 158
    addi.d $t1, $t1, 8
    addi.d $t4, $t4, -1
    bnez $t4, 1b
    li.d $t3, 0x12345678
    st.w $t3, $s6, 0
    move $t1, $s5
    li.d $t2, 18
1:
    st.d $t3, $t1, 0
    addi.d $t1, $t1, 8
    addi.d $t2, $t2, -1
    bnez $t2, 1b
    Z
    move $a0, $s3
    move $a1, $s6
    li.d $a2, 1
    move $a3, $s5
    SC 260
    CHECK $a0, 0, 159
    ld.w $t2, $s6, 0
    CHECK $t2, 0x12345678, 160
    move $t1, $s5
    li.d $t4, 18
1:
    ld.d $t2, $t1, 0
    CHECK $t2, 0x12345678, 161
    addi.d $t1, $t1, 8
    addi.d $t4, $t4, -1
    bnez $t4, 1b
    jr $s2
.Lobserve:
    move $s2, $ra
    Z
    li.d $a0, 2
    move $a2, $s4
    li.d $a3, 0x1000004
    move $a4, $s5
    SC 95
    CHECK $a0, 0, 162
    bl .Lcheck_info
    jr $s2
.Lcheck_info:
    ld.w $t2, $s4, 0
    CHECK $t2, 17, 163
    ld.w $t2, $s4, 4
    CHECK $t2, 0, 164
    ld.w $t2, $s4, 8
    CHECK $t2, 1, 165
    ld.w $t2, $s4, 16
    EQ $t2, $s3, 166
    ld.w $t2, $s4, 20
    CHECK $t2, 0, 167
    ld.w $t2, $s4, 24
    CHECK $t2, 0, 168
    jr $ra

# /// 真正用户异常只能由信号终止并正常wait，不把kernelpanic当成功。
.Lsegv:
    li.d $t0, 0x70000000
    ld.d $t1, $t0, 0
    li.d $s8, 203
    b .Lexit
.Lbus:
    Z
    li.d $a0, -100
    la.local $a1, .Lempty
    SC 56
    li.d $s8, 204
    bge $a0, $zero, 1f
    b .Lexit
1:
    move $s3, $a0
    Z
    li.d $a0, 0x47000000
    li.d $a1, 4096
    li.d $a2, 1
    li.d $a3, 0x12
    move $a4, $s3
    SC 222
    CHECK $a0, 0x47000000, 205
    ld.d $t1, $a0, 0
    li.d $s8, 206
    b .Lexit
.Lill:
    .word 0
    li.d $s8, 207
    b .Lexit

# /// 捕获USR1只发表本映像标志，由真实信号帧返回。
.Lhandler:
    la.local $t1, .Lhandled
    li.d $t0, 1
    dbar 0
    st.w $t0, $t1, 0
    jr $ra
.Lpass:
    li.d $s8, 0
.Lexit:
    Z
    move $a0, $s8
    beqz $s8, 1f
    SC 94
1:
    SC 93
    .word 0
.size _start, .-_start

.section .rodata
.Lself: .asciz "/test/bin/user-lifecycle"
.Lmissing: .asciz "/test/no-such-lifecycle"
.Lunsupported: .asciz "/test/bin/user-interp"
.Ldata_path: .asciz "/test/io-data.bin"
.Lrelative_data: .asciz "io-data.bin"
.Ltest_dir: .asciz "/test"
.Lexec_env: .asciz "EXEC=1"
.Lmode_N: .asciz "N"
.Lmode_X: .asciz "X"
.Lmode_V: .asciz "V"
.Lmode_U: .asciz "U"
.Lempty: .asciz "/test/empty"

.data
.balign 16
.Largv_N: .quad .Lself, .Lmode_N, 0
.Largv_X: .quad .Lself, .Lmode_X, 0
.Largv_V: .quad .Lself, .Lmode_V, 0
.Largv_U: .quad .Lself, .Lmode_U, 0
.Lenv: .quad .Lexec_env, 0
.Lusr1_mask: .quad 0x200
.Lcatch_action: .quad .Lhandler, 4, 0x800
.Lignore_action: .quad 1, 0x10000000, 0x800
.Ltimer_on: .quad 0, 0, 3, 0
.Ltimer_off: .quad 0, 0, 0, 0
.Lzero_time: .quad 0, 0
.Lalt_desc: .quad .Lalt_stack, 0, 16384
.Lold_action: .zero 24
.Lold_mask: .zero 8
.Lold_stack: .zero 24
.Ltimer_old: .zero 32
.Linfo: .zero 128
.Lstatus: .zero 4
.Lbyte: .zero 1
.bss
.balign 16
.Lhandled: .zero 4
.balign 16
.Lstack0: .zero 16384
.Lstack0_top:
.Lstack1: .zero 16384
.Lstack1_top:
.Lalt_stack: .zero 16384
.zero 1
