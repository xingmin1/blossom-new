.text
.option norelax
.option norvc

# 普通静态用户程序；控制页的发表与观察分别使用 Release/Acquire。
.macro SC number
    li a7, \number
    ecall
.endm
.macro CHECK reg, value, code
    li s8, \code
    li t6, \value
    beq \reg, t6, 991f
    j .Lexit
991:
.endm
.macro EQ left, right, code
    li s8, \code
    beq \left, \right, 991f
    j .Lexit
991:
.endm
.macro GO number
991:
    lw t0, 12(s0)
    fence r, rw
    li t1, \number
    bltu t0, t1, 991b
.endm
.macro PHASE number
    li t0, \number
    fence rw, w
    sw t0, 8(s0)
    GO \number
.endm
# timeout 参数同时承载 REQUEUE 的整数 move_count，严格按 raw 六参数传递。
.macro FUTEX address, operation, value, timeout=0, destination=0, mask=0
    li a0, \address
    li a1, \operation
    li a2, \value
    li a3, \timeout
    li a4, \destination
    li a5, \mask
    SC 98
.endm
.macro MAP address, flags, code
    li a0, \address
    li a1, 4096
    li a2, 3
    li a3, \flags
    li a4, -1
    li a5, 0
    SC 222
    CHECK a0, \address, \code
.endm
# RV clone 次序为 flags, stack, parent_tid, tls, child_tid；VM 子使用真实独立栈。
.macro SPAWN flags, stack, target, slot, code, tid_address=0
    li t0, -4096
    fence rw, w
    .if \slot == 0
    sw t0, 24(s0)
    .else
    sw t0, 28(s0)
    .endif
    li a0, \flags
    li a1, 0
    .if \flags & 0x100
    lla a1, \stack
    .endif
    li a2, 0
    li a3, 0
    li a4, \tid_address
    SC 220
    li s8, \code
    bgez a0, 991f
    j .Lexit
991:
    bnez a0, 992f
    j \target
992:
    fence rw, w
    sd a0, \slot(s0)
.endm
.macro ACTION flags, code
    lla a1, .Laction
    lla t0, .Lhandler
    sd t0, 0(a1)
    li t0, \flags
    sd t0, 8(a1)
    sd zero, 16(a1)
    li a0, 10
    li a2, 0
    li a3, 8
    SC 134
    CHECK a0, 0, \code
.endm
# waitid 真正消费自己的子进程，区分正常退出、信号死亡以及用户断言状态。
.macro REAP slot, code
    li a0, 1
    ld a1, \slot(s0)
    fence r, rw
    lla a2, .Linfo
    li a3, 4
    li a4, 0
    SC 95
    CHECK a0, 0, \code
    lla t1, .Linfo
    lw t2, 8(t1)
    CHECK t2, 1, \code+1
    lw t2, 16(t1)
    ld t3, \slot(s0)
    EQ t2, t3, \code+2
    lw t2, 24(t1)
    beqz t2, 991f
    mv s8, t2
    j .Lexit
991:
.endm
.macro RESULT offset, value, code
    lw t2, \offset(s0)
    fence r, rw
    CHECK t2, \value, \code
.endm
.macro WORDS code
    lw t2, 64(s0)
    fence r, rw
    CHECK t2, 7, \code
    lw t2, 68(s0)
    fence r, rw
    CHECK t2, 7, \code+1
.endm
# 绝对 monotonic 期限由真实时钟产生；不将时间当作已登记的证据。
.macro DEADLINE seconds, code
    li a0, 1
    addi a1, s0, 40
    SC 113
    CHECK a0, 0, \code
    ld t0, 40(s0)
    addi t0, t0, \seconds
    sd t0, 40(s0)
    fence rw, w
.endm

.global _start
.type _start, @function
_start:
    li s8, 1
    andi t0, sp, 15
    beqz t0, 1f
    j .Lexit
1:
    ld t0, 0(sp)
    CHECK t0, 2, 1
    ld t0, 16(sp)
    bnez t0, 1f
    j .Lexit
1:
    lbu t1, 1(t0)
    CHECK t1, 0, 1
    lbu s1, 0(t0)
    MAP 0x44000000, 0x31, 2
    li s0, 0x44000000
    mv t0, s0
    li t1, 512
1:
    sd zero, 0(t0)
    addi t0, t0, 8
    addi t1, t1, -1
    bnez t1, 1b
    li t0, 7
    sw t0, 64(s0)
    sw t0, 68(s0)
    fence rw, w
    li t0, 119
    bne s1, t0, 1f
    j .Lw
1:
    li t0, 112
    bne s1, t0, 1f
    j .Lp
1:
    li t0, 110
    bne s1, t0, 1f
    j .Ln
1:
    li t0, 97
    bne s1, t0, 1f
    j .La
1:
    li t0, 114
    bne s1, t0, 1f
    j .Lr
1:
    li t0, 116
    bne s1, t0, 1f
    j .Lt
1:
    li t0, 115
    bne s1, t0, 1f
    j .Ls
1:
    CHECK s1, 100, 3
    j .Ld

# 首次比较失败、地址错误和 bitset 零掩码均由真实 syscall 拒绝。
.Lw:
    FUTEX 0x44000040, 128, 8
    CHECK a0, -11, 10
    FUTEX 0x44000041, 128, 7
    CHECK a0, -22, 11
    FUTEX 0x70000000, 128, 7
    CHECK a0, -14, 12
    FUTEX 0x44000040, 137, 7
    CHECK a0, -22, 13
    FUTEX 0x44000040, 134, 0
    CHECK a0, -38, 26
    FUTEX 0x44000040, 133, 1
    CHECK a0, -38, 27
    li a0, 0x44000000
    li a1, 24
    SC 99
    CHECK a0, -38, 28
    FUTEX 0x44000040, 129, -1
    CHECK a0, 0, 29
    MAP 0x46000000, 0x32, 30
    li t1, 0x46000000
    li t0, 7
    sw t0, 0(t1)
    fence rw, w
    SPAWN 0x111, .Lstack0_top, .Lchild_w, 0, 14
    PHASE 1
    FUTEX 0x46000000, 1, 1
    CHECK a0, 0, 31
    FUTEX 0x46000000, 129, 1
    CHECK a0, 1, 15
    REAP 0, 16
    RESULT 24, 0, 19
    WORDS 20
    RESULT 32, 0, 25
    j .Lpass
.Lchild_w:
    FUTEX 0x46000000, 128, 7
    fence rw, w
    sw a0, 24(s0)
    CHECK a0, 0, 22
    li t1, 0x46000000
    lw t2, 0(t1)
    fence r, rw
    CHECK t2, 7, 45
    WORDS 23
    j .Lpass

# PRIVATE 的共享映射以及普通匿名私有映射都必须按真实 Mm 隔离。
.Lp:
    li s2, 0x44000040
    li s3, 128
    j .Lisolate
.Ln:
    MAP 0x46000000, 0x32, 30
    li s2, 0x46000000
    li t0, 7
    sw t0, 0(s2)
    fence rw, w
    li s3, 0
.Lisolate:
    ACTION 0, 31
    SPAWN 17, .Lstack0_top, .Lchild_isolate, 0, 32
    PHASE 1
    mv a0, s2
    ori a1, s3, 1
    li a2, 1
    li a3, 0
    li a4, 0
    li a5, 0
    SC 98
    CHECK a0, 0, 33
    PHASE 2
    REAP 0, 34
    RESULT 24, -4, 37
    RESULT 32, 1, 38
    lw t2, 0(s2)
    fence r, rw
    CHECK t2, 7, 39
    WORDS 40
    j .Lpass
.Lchild_isolate:
    mv a0, s2
    mv a1, s3
    li a2, 7
    li a3, 0
    li a4, 0
    li a5, 0
    SC 98
    fence rw, w
    sw a0, 24(s0)
    CHECK a0, -4, 42
    RESULT 32, 1, 43
    lw t2, 0(s2)
    fence r, rw
    CHECK t2, 7, 44
    j .Lpass

# 子 Mm 的额外别名由观察者安装，原共享映射留存用于协调和父方 WAKE。
.La:
    SPAWN 17, .Lstack0_top, .Lchild_alias, 0, 50
    PHASE 1
    PHASE 2
    FUTEX 0x44000040, 1, 1
    CHECK a0, 1, 51
    REAP 0, 52
    RESULT 24, 0, 55
    WORDS 56
    RESULT 32, 0, 62
    j .Lpass
.Lchild_alias:
    GO 1
    FUTEX 0x45000040, 0, 7
    fence rw, w
    sw a0, 24(s0)
    CHECK a0, 0, 58
    li t1, 0x45000040
    lw t2, 0(t1)
    fence r, rw
    CHECK t2, 7, 59
    WORDS 60
    j .Lpass

# 两掩码同桶选择、比较失败不移动、往返重排保持原节点身份与期限。
.Lr:
    ACTION 0, 70
    DEADLINE 30, 71
    li s4, 1
    li s5, 24
    li s6, 0
    li s7, 0
    li s9, 180
    SPAWN 0x111, .Lstack0_top, .Lchild_bitset, 0, 72
    li s4, 2
    li s5, 28
    li s9, 184
    SPAWN 0x111, .Lstack1_top, .Lchild_bitset, 16, 73
    PHASE 1
    FUTEX 0x44000040, 138, 1, 0, 0, 1
    CHECK a0, 1, 74
    FUTEX 0x44000040, 132, 0, 1, 0x44000044, 8
    CHECK a0, -11, 75
    FUTEX 0x44000040, 132, 0, 1, 0x44000044, 7
    CHECK a0, 1, 76
    PHASE 2
    FUTEX 0x44000044, 131, 0, 1, 0x44000040
    CHECK a0, 1, 77
    PHASE 3
    FUTEX 0x44000040, 138, 1, 0, 0, 1
    CHECK a0, 0, 78
    FUTEX 0x44000040, 138, 1, 0, 0, 2
    CHECK a0, 1, 79
    REAP 0, 80
    REAP 16, 83
    RESULT 24, 0, 86
    RESULT 28, 0, 87
    WORDS 88

    # 先 signal 取消重排后当前位置，再单独核对绝对期限取消。
    sd zero, 0(s0)
    sw zero, 24(s0)
    fence rw, w
    DEADLINE 30, 90
    li s4, 4
    li s5, 24
    li s6, -4
    li s7, 1
    li s9, 188
    SPAWN 0x111, .Lstack0_top, .Lchild_bitset, 0, 91
    PHASE 4
    FUTEX 0x44000040, 132, 0, 1, 0x44000044, 7
    CHECK a0, 1, 92
    PHASE 5
    REAP 0, 93
    RESULT 24, -4, 96
    RESULT 32, 1, 97
    FUTEX 0x44000044, 129, 1
    CHECK a0, 0, 98

    sd zero, 0(s0)
    sw zero, 24(s0)
    fence rw, w
    DEADLINE 1, 99
    li s4, 8
    li s5, 24
    li s6, -110
    li s7, 1
    li s9, 192
    SPAWN 0x111, .Lstack0_top, .Lchild_bitset, 0, 100
    PHASE 6
    FUTEX 0x44000040, 132, 0, 1, 0x44000044, 7
    CHECK a0, 1, 101
    PHASE 7
    REAP 0, 102
    RESULT 24, -110, 105
    RESULT 32, 1, 106
    FUTEX 0x44000044, 129, 1
    CHECK a0, 0, 107
    WORDS 108
    j .Lpass
.Lchild_bitset:
    li a0, 0x44000040
    li a1, 137
    li a2, 7
    addi a3, s0, 40
    li a4, 0
    mv a5, s4
    SC 98
    add t1, s0, s5
    fence rw, w
    sw a0, 0(t1)
    mv s8, s9
    beq a0, s6, 1f
    j .Lexit
1:
    # 后两轮共享 handler_count 已为一，首轮必须仍为零。
    addi s8, s9, 1
    lw t2, 32(s0)
    fence r, rw
    beq t2, s7, 1f
    j .Lexit
1:
    addi s8, s9, 2
    lw t2, 64(s0)
    fence r, rw
    li t6, 7
    beq t2, t6, 1f
    j .Lexit
1:
    addi s8, s9, 3
    lw t2, 68(s0)
    fence r, rw
    beq t2, t6, 1f
    j .Lexit
1:
    j .Lpass

# CHILD_SETTID 后的实际 TID 作为 join 比较值，clear_child_tid 零写与 WAKE 必须真实发生。
.Lt:
    SPAWN 0x1200111, .Lstack0_top, .Lchild_tid, 0, 120, 0x44000040
1:
    lw t0, 8(s0)
    fence r, rw
    beqz t0, 1b
    CHECK t0, 1, 121
    lw s2, 64(s0)
    ld t2, 0(s0)
    fence r, rw
    EQ s2, t2, 122
    li a0, 0x44000040
    li a1, 0
    mv a2, s2
    li a3, 0
    li a4, 0
    li a5, 0
    SC 98
    fence rw, w
    sw a0, 24(s0)
    CHECK a0, 0, 123
    RESULT 64, 0, 124
    REAP 0, 125
    RESULT 24, 0, 128
    SPAWN 0x111, .Lstack1_top, .Lchild_bad_tid, 16, 129
    REAP 16, 130
    RESULT 28, 0, 133
    RESULT 64, 0, 134
    RESULT 32, 0, 138
    RESULT 68, 7, 139
    j .Lpass
.Lchild_tid:
    SC 178
    mv s2, a0
    lw t2, 64(s0)
    fence r, rw
    EQ t2, s2, 135
    # 再由用户入口写一次真实 TID，随后发表 phase，不能由观察者代写成功逻辑。
    fence rw, w
    sw s2, 64(s0)
    PHASE 1
    j .Lpass
.Lchild_bad_tid:
    SC 178
    mv s2, a0
    addi a0, s0, 68
    SC 96
    EQ a0, s2, 136
    li a0, 0x70000000
    SC 96
    EQ a0, s2, 137
    fence rw, w
    sw zero, 28(s0)
    j .Lpass

# 普通 handler 返回至真实 rt_sigreturn trampoline；SA_RESTART 恢复原 WAIT PC 和参数。
.Ls:
    ACTION 0x10000000, 140
    SPAWN 0x111, .Lstack0_top, .Lchild_restart, 0, 141
    PHASE 1
    RESULT 32, 1, 142
    FUTEX 0x44000040, 129, 1
    CHECK a0, 1, 143
    REAP 0, 144
    RESULT 24, 0, 147
    RESULT 32, 1, 148
    WORDS 149
    j .Lpass
.Lchild_restart:
    FUTEX 0x44000040, 128, 7
    fence rw, w
    sw a0, 24(s0)
    CHECK a0, 0, 151
    # 成功返回还必须保留被重启 syscall 的原始非返回参数。
    CHECK a1, 128, 155
    CHECK a2, 7, 156
    CHECK a3, 0, 157
    CHECK a4, 0, 158
    CHECK a5, 0, 159
    CHECK a7, 98, 173
    RESULT 32, 1, 152
    WORDS 153
    j .Lpass

# STOP 后撤去原 timeout 页；CONT 必须只使用已经保存的相对期限。
.Ld:
    MAP 0x47000000, 0x32, 160
    li t1, 0x47000000
    sd zero, 0(t1)
    li t0, 200000000
    sd t0, 8(t1)
    fence rw, w
    SPAWN 0x111, .Lstack0_top, .Lchild_deadline, 0, 161
    PHASE 1
    li a0, 0x47000000
    li a1, 4096
    SC 215
    CHECK a0, 0, 162
    PHASE 2
    REAP 0, 163
    RESULT 24, -110, 166
    RESULT 32, 0, 167
    WORDS 168
    j .Lpass
.Lchild_deadline:
    FUTEX 0x44000040, 128, 7, 0x47000000
    fence rw, w
    sw a0, 24(s0)
    CHECK a0, -110, 170
    RESULT 32, 0, 171
    WORDS 172
    j .Lpass

# 信号帧负责保存寄存器；handler 只递增共享计数并以普通 ret 返回 trampoline。
.Lhandler:
    lw t0, 32(s0)
    fence r, rw
    addi t0, t0, 1
    fence rw, w
    sw t0, 32(s0)
    ret
.Lpass:
    li s8, 0
.Lexit:
    mv a0, s8
    SC 93
1:
    j 1b
.size _start, .-_start

.data
.balign 8
.Laction: .zero 24
.Linfo: .zero 128

.bss
.balign 16
.Lstack0: .zero 16384
.Lstack0_top:
.Lstack1: .zero 16384
.Lstack1_top:
