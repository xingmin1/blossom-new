.text

# 普通静态用户程序；共享字发布前Release，观察后Acquire。
# 条件分支只跨本地宏片段，远目标统一使用无条件跳转。
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
.macro GO number
    li.d $t1, \number
991:
    ld.wu $t0, $s0, 12
    dbar 0
    bltu $t0, $t1, 991b
.endm
.macro PHASE number
    li.d $t0, \number
    dbar 0
    st.w $t0, $s0, 8
    GO \number
.endm
.macro FUTEX address, operation, value, timeout=0, destination=0, mask=0
    li.d $a0, \address
    li.d $a1, \operation
    li.d $a2, \value
    li.d $a3, \timeout
    li.d $a4, \destination
    li.d $a5, \mask
    SC 98
.endm
.macro MAP address, flags, code
    li.d $a0, \address
    li.d $a1, 4096
    li.d $a2, 3
    li.d $a3, \flags
    li.d $a4, -1
    li.d $a5, 0
    SC 222
    CHECK $a0, \address, \code
.endm
# LA raw clone顺序：flags、stack、parent_tid、child_tid、tls。
# slot选择真实ID/result和独立16KiB栈；role只选择child用户路径。
.macro CLONE flags, slot, role, code, tid=0
    li.d $s4, \role
    addi.d $s5, $s0, 24+4*\slot
    li.d $a0, \flags
    li.d $a1, 0
    .if (\flags & 0x100)
        .if \slot
            la.local $a1, .Lstack1_top
        .else
            la.local $a1, .Lstack0_top
        .endif
    .endif
    li.d $a2, 0
    li.d $a3, \tid
    li.d $a4, 0
    SC 220
    li.d $s8, \code
    bge $a0, $zero, 991f
    b .Lexit
991:
    bnez $a0, 992f
    b .Lchild
992:
    dbar 0
    st.d $a0, $s0, 16*\slot
.endm
.macro RESULT expected, code
    dbar 0
    st.w $a0, $s5, 0
    CHECK $a0, \expected, \code
.endm
# 真waitid消费本程序的child，不消费根协调者的其他事件。
.macro REAP slot, expected, code
    ld.d $s3, $s0, 16*\slot
    dbar 0
    li.d $a0, 1
    move $a1, $s3
    la.local $a2, .Linfo
    li.d $a3, 4
    li.d $a4, 0
    SC 95
    CHECK $a0, 0, \code
    la.local $t1, .Linfo
    ld.w $t2, $t1, 0
    CHECK $t2, 17, \code+1
    ld.w $t2, $t1, 4
    CHECK $t2, 0, \code+2
    ld.w $t2, $t1, 8
    CHECK $t2, 1, \code+3
    ld.w $t2, $t1, 16
    EQ $t2, $s3, \code+4
    ld.w $t2, $t1, 24
    beqz $t2, 991f
    move $s8, $t2
    b .Lexit
991:
    ld.w $t2, $s0, 24+4*\slot
    dbar 0
    CHECK $t2, \expected, \code+5
.endm
.macro WORD offset, value, code
    ld.w $t2, $s0, \offset
    dbar 0
    CHECK $t2, \value, \code
.endm
.macro ACTION flags, code
    la.local $t1, .Laction
    la.local $t0, .Lhandler
    st.d $t0, $t1, 0
    li.d $t0, \flags
    st.d $t0, $t1, 8
    st.d $zero, $t1, 16
    li.d $a0, 10
    move $a1, $t1
    li.d $a2, 0
    li.d $a3, 8
    SC 134
    CHECK $a0, 0, \code
.endm
# 绝对期限只由真实monotonic读数构造，不能据此猜测Blocked。
.macro DEADLINE seconds, code
    li.d $a0, 1
    addi.d $a1, $s0, 40
    SC 113
    CHECK $a0, 0, \code
    ld.d $t0, $s0, 40
    addi.d $t0, $t0, \seconds
    st.d $t0, $s0, 40
    dbar 0
.endm

.global _start
.type _start, @function
_start:
    li.d $s8, 1
    andi $t2, $sp, 15
    CHECK $t2, 0, 1
    ld.d $t2, $sp, 0
    CHECK $t2, 2, 2
    ld.d $t1, $sp, 16
    POS $t1, 3
    ld.bu $s7, $t1, 0
    ld.bu $t2, $t1, 1
    CHECK $t2, 0, 4
    MAP 0x44000000, 0x31, 5
    li.d $s0, 0x44000000
    move $t1, $s0
    li.d $t2, 512
1:
    st.d $zero, $t1, 0
    addi.d $t1, $t1, 8
    addi.d $t2, $t2, -1
    bnez $t2, 1b
    li.d $t0, -4096
    st.w $t0, $s0, 24
    st.w $t0, $s0, 28
    li.d $t0, 7
    st.w $t0, $s0, 64
    st.w $t0, $s0, 68
    dbar 0
    MODE 119, .Lw
    MODE 112, .Lp
    MODE 110, .Ln
    MODE 97, .La
    MODE 114, .Lr
    MODE 116, .Lt
    MODE 115, .Ls
    MODE 100, .Ld
    li.d $s8, 6
    b .Lexit

.Lw:
    FUTEX 0x44000040, 0x80, 8
    CHECK $a0, -11, 20
    FUTEX 0x44000041, 0x80, 7
    CHECK $a0, -22, 21
    FUTEX 0x70000000, 0x80, 7
    CHECK $a0, -14, 22
    FUTEX 0x44000040, 0x89, 7, 0, 0, 0
    CHECK $a0, -22, 23
    FUTEX 0x44000040, 0x86, 0
    CHECK $a0, -38, 34
    FUTEX 0x44000040, 0x85, 1
    CHECK $a0, -38, 35
    li.d $a0, 0x44000000
    li.d $a1, 24
    SC 99
    CHECK $a0, -38, 36
    FUTEX 0x44000040, 0x81, -1
    CHECK $a0, 0, 37
    MAP 0x46000000, 0x32, 38
    li.d $t1, 0x46000000
    li.d $t0, 7
    st.w $t0, $t1, 0
    dbar 0
    CLONE 0x111, 0, 0, 24
    PHASE 1
    FUTEX 0x46000000, 1, 1
    CHECK $a0, 0, 39
    FUTEX 0x46000000, 0x81, 1
    CHECK $a0, 1, 25
    REAP 0, 0, 26
    WORD 64, 7, 32
    WORD 32, 0, 33
    b .Lpass

.Lp:
    li.d $s2, 0x44000040
    b .Lisolation
.Ln:
    MAP 0x46000000, 0x32, 40
    li.d $s2, 0x46000000
    li.d $t0, 7
    st.w $t0, $s2, 0
    dbar 0
.Lisolation:
    CLONE 17, 0, 0, 41
    PHASE 1
    move $a0, $s2
    li.d $a1, 1
    li.d $t0, 112
    bne $s7, $t0, 1f
    ori $a1, $a1, 0x80
1:
    li.d $a2, 1
    li.d $a3, 0
    li.d $a4, 0
    li.d $a5, 0
    SC 98
    CHECK $a0, 0, 42
    PHASE 2
    REAP 0, -4, 43
    ld.w $t2, $s2, 0
    dbar 0
    CHECK $t2, 7, 49
    WORD 32, 1, 50
    b .Lpass

.La:
    CLONE 17, 0, 0, 55
    PHASE 1
    PHASE 2
    FUTEX 0x44000040, 1, 1
    CHECK $a0, 1, 56
    REAP 0, 0, 57
    WORD 64, 7, 63
    WORD 32, 0, 64
    b .Lpass

.Lr:
    DEADLINE 30, 70
    CLONE 0x111, 0, 0, 71
    CLONE 0x111, 1, 1, 72
    PHASE 1
    FUTEX 0x44000040, 0x8a, 1, 0, 0, 1
    CHECK $a0, 1, 73
    FUTEX 0x44000040, 0x84, 0, 1, 0x44000044, 8
    CHECK $a0, -11, 74
    FUTEX 0x44000040, 0x84, 0, 1, 0x44000044, 7
    CHECK $a0, 1, 75
    PHASE 2
    FUTEX 0x44000044, 0x83, 0, 1, 0x44000040
    CHECK $a0, 1, 76
    PHASE 3
    FUTEX 0x44000040, 0x8a, 1, 0, 0, 1
    CHECK $a0, 0, 77
    FUTEX 0x44000040, 0x8a, 1, 0, 0, 2
    CHECK $a0, 1, 78
    REAP 0, 0, 79
    REAP 1, 0, 85

    # 已消费的child0/result0槽复用；原等待节点的身份由观察者核验。
    st.d $zero, $s0, 0
    li.d $t0, -4096
    st.w $t0, $s0, 24
    dbar 0
    DEADLINE 30, 91
    CLONE 0x111, 0, 2, 92
    PHASE 4
    FUTEX 0x44000040, 0x84, 0, 1, 0x44000044, 7
    CHECK $a0, 1, 93
    PHASE 5
    REAP 0, -4, 94
    FUTEX 0x44000044, 0x81, 1
    CHECK $a0, 0, 100
    WORD 32, 1, 101

    st.d $zero, $s0, 0
    li.d $t0, -4096
    st.w $t0, $s0, 24
    dbar 0
    DEADLINE 1, 102
    CLONE 0x111, 0, 3, 103
    PHASE 6
    FUTEX 0x44000040, 0x84, 0, 1, 0x44000044, 7
    CHECK $a0, 1, 104
    PHASE 7
    REAP 0, -110, 105
    FUTEX 0x44000044, 0x81, 1
    CHECK $a0, 0, 111
    WORD 64, 7, 112
    WORD 68, 7, 113
    WORD 32, 1, 114
    b .Lpass

.Lt:
    CLONE 0x1200111, 0, 0, 120, 0x44000040
1:
    ld.wu $t0, $s0, 8
    dbar 0
    beqz $t0, 1b
    ld.w $s2, $s0, 64
    ld.d $t2, $s0, 0
    dbar 0
    EQ $s2, $t2, 121
    li.d $a0, 0x44000040
    li.d $a1, 0
    move $a2, $s2
    li.d $a3, 0
    li.d $a4, 0
    li.d $a5, 0
    SC 98
    dbar 0
    st.w $a0, $s0, 24
    CHECK $a0, 0, 122
    WORD 64, 0, 123
    REAP 0, 0, 124
    CLONE 0x111, 1, 1, 130
    REAP 1, 0, 131
    WORD 64, 0, 137
    WORD 68, 7, 138
    WORD 32, 0, 139
    b .Lpass

.Ls:
    CLONE 0x111, 0, 0, 145
    PHASE 1
    WORD 32, 1, 146
    FUTEX 0x44000040, 0x81, 1
    CHECK $a0, 1, 147
    REAP 0, 0, 148
    WORD 64, 7, 154
    WORD 32, 1, 155
    b .Lpass

.Ld:
    MAP 0x47000000, 0x32, 160
    li.d $t1, 0x47000000
    st.d $zero, $t1, 0
    li.d $t0, 200000000
    st.d $t0, $t1, 8
    dbar 0
    CLONE 0x111, 0, 0, 161
    PHASE 1
    li.d $a0, 0x47000000
    li.d $a1, 4096
    SC 215
    CHECK $a0, 0, 162
    PHASE 2
    REAP 0, -110, 163
    WORD 64, 7, 169
    WORD 32, 0, 170
    b .Lpass

.Lchild:
    MODE 119, .Lchild_w
    MODE 112, .Lchild_isolation
    MODE 110, .Lchild_isolation
    MODE 97, .Lchild_alias
    MODE 114, .Lchild_r
    MODE 116, .Lchild_t
    MODE 115, .Lchild_s
    MODE 100, .Lchild_d
    li.d $s8, 200
    b .Lexit
.Lchild_w:
    FUTEX 0x46000000, 0x80, 7
    RESULT 0, 201
    li.d $t1, 0x46000000
    ld.w $t2, $t1, 0
    dbar 0
    CHECK $t2, 7, 236
    WORD 64, 7, 202
    b .Lpass
.Lchild_isolation:
    ACTION 0, 203
    move $a0, $s2
    li.d $a1, 0
    li.d $t0, 112
    bne $s7, $t0, 1f
    li.d $a1, 0x80
1:
    li.d $a2, 7
    li.d $a3, 0
    li.d $a4, 0
    li.d $a5, 0
    SC 98
    RESULT -4, 204
    ld.w $t2, $s2, 0
    dbar 0
    CHECK $t2, 7, 205
    WORD 32, 1, 206
    b .Lpass
.Lchild_alias:
    GO 1
    FUTEX 0x45000040, 0, 7
    RESULT 0, 207
    li.d $t1, 0x45000000
    ld.w $t2, $t1, 64
    dbar 0
    CHECK $t2, 7, 208
    WORD 64, 7, 209
    b .Lpass
.Lchild_r:
    li.d $t0, 2
    bne $s4, $t0, 1f
    ACTION 0, 210
1:
    li.d $a0, 0x44000040
    li.d $a1, 0x89
    li.d $a2, 7
    addi.d $a3, $s0, 40
    li.d $a4, 0
    li.d $a5, 1
    sll.d $a5, $a5, $s4
    SC 98
    dbar 0
    st.w $a0, $s5, 0
    li.d $t0, 2
    bne $s4, $t0, 1f
    CHECK $a0, -4, 211
    b .Lchild_r_words
1:
    li.d $t0, 3
    bne $s4, $t0, 2f
    CHECK $a0, -110, 212
    b .Lchild_r_words
2:
    CHECK $a0, 0, 213
.Lchild_r_words:
    WORD 64, 7, 214
    WORD 68, 7, 215
    b .Lpass
.Lchild_t:
    bnez $s4, 1f
    SC 178
    POS $a0, 216
    ld.w $t2, $s0, 64
    dbar 0
    EQ $a0, $t2, 217
    # 先验证内核CHILD_SETTID，再由真实child发布同一TID。
    dbar 0
    st.w $a0, $s0, 64
    li.d $t0, 1
    dbar 0
    st.w $t0, $s0, 8
    GO 1
    b .Lpass
1:
    SC 178
    POS $a0, 219
    move $s2, $a0
    li.d $a0, 0x44000044
    SC 96
    EQ $a0, $s2, 220
    li.d $a0, 0x70000000
    SC 96
    EQ $a0, $s2, 221
    WORD 68, 7, 222
    li.d $a0, 0
    RESULT 0, 223
    b .Lpass
.Lchild_s:
    ACTION 0x10000000, 224
    FUTEX 0x44000040, 0x80, 7
    RESULT 0, 225
    # 重启及信号帧恢复不能破坏原非返回raw参数。
    CHECK $a1, 0x80, 230
    CHECK $a2, 7, 231
    CHECK $a3, 0, 232
    CHECK $a4, 0, 233
    CHECK $a5, 0, 234
    CHECK $a7, 98, 235
    WORD 32, 1, 226
    WORD 64, 7, 227
    b .Lpass
.Lchild_d:
    FUTEX 0x44000040, 0x80, 7, 0x47000000
    RESULT -110, 228
    WORD 64, 7, 229
    b .Lpass

# 普通信号帧保持原SP；直接执行Linux rt_sigreturn恢复原PC及raw参数。
.Lhandler:
    li.d $t7, 0x44000000
    ld.w $t8, $t7, 32
    dbar 0
    addi.d $t8, $t8, 1
    dbar 0
    st.w $t8, $t7, 32
    SC 139
    .word 0
.Lpass:
    li.d $s8, 0
.Lexit:
    move $a0, $s8
    SC 93
    .word 0
.size _start, .-_start

.section .bss, "aw", @nobits
.balign 16
.Laction: .zero 24
.Linfo: .zero 128
.balign 16
.Lstack0: .zero 16384
.Lstack0_top:
.Lstack1: .zero 16384
.Lstack1_top:
