.text
.option norelax
.option norvc
.equ EVENT_USER_END, 0x4000000000
# /// 窄指令宏保留两目标相同的 raw ABI 断言与寄存器责任。
.macro LI r, v
    li \r, \v
.endm
.macro MOV d, s
    mv \d, \s
.endm
.macro LA r, label
    lla \r, \label
.endm
.macro ADDI d, s, v
    addi \d, \s, \v
.endm
.macro ADD d, a, b
    add \d, \a, \b
.endm
.macro SUB d, a, b
    sub \d, \a, \b
.endm
.macro MUL d, a, b
    mul \d, \a, \b
.endm
.macro LD d, s, v
    ld \d, \v(\s)
.endm
.macro LW d, s, v
    lw \d, \v(\s)
.endm
.macro LH d, s, v
    lhu \d, \v(\s)
.endm
.macro LB d, s, v
    lbu \d, \v(\s)
.endm
.macro SD d, s, v
    sd \d, \v(\s)
.endm
.macro SW d, s, v
    sw \d, \v(\s)
.endm
.macro SH d, s, v
    sh \d, \v(\s)
.endm
.macro BEQ a, b, label
    beq \a, \b, 991f
    j 992f
991:
    j \label
992:
.endm
.macro BNE a, b, label
    bne \a, \b, 991f
    j 992f
991:
    j \label
992:
.endm
.macro BLT a, b, label
    blt \a, \b, 991f
    j 992f
991:
    j \label
992:
.endm
.macro JMP label
    j \label
.endm
.macro RETURN
    ret
.endm
.macro SC n
    li a7, \n
    ecall
.endm

# /// 共享控制字的发布与领取跨目标核有序，旧 gate 不与下一 phase 混合。
.macro FENCE
    fence rw, rw
.endm

# /// 退出值对应具体 raw ABI 断言，正常子进程由本客体的 wait4 消费。
.macro CHECK reg, value, code
    LI s8, \code
    LI t0, \value
    BEQ \reg, t0, 993f
    JMP .Lexit
993:
.endm
.macro EQ left, right, code
    LI s8, \code
    BEQ \left, \right, 993f
    JMP .Lexit
993:
.endm
.macro OK code
    CHECK a0, 0, \code
.endm
.macro FD code
    LI s8, \code
    BLT a0, zero, .Lexit
.endm
.macro CLOSE reg, code
    MOV a0, \reg
    SC 57
    OK \code
.endm
.macro WRITE reg, code
    MOV a0, \reg
    LA a1, .Lbyte
    LI a2, 1
    SC 64
    CHECK a0, 1, \code
.endm
.macro READ reg, code
    MOV a0, \reg
    LA a1, .Lreceived
    LI a2, 1
    SC 63
    CHECK a0, 1, \code
.endm
.macro EVENT mask, data
    LA t1, .Levent
    LI t2, \mask
    SW t2, t1, 0
    SW zero, t1, 4
    LI t2, \data
    SD t2, t1, 8
.endm
.macro CTL op, fd, result, code
    MOV a0, s2
    LI a1, \op
    MOV a2, \fd
    LA a3, .Levent
    SC 21
    CHECK a0, \result, \code
.endm
.macro WAIT count, result, code
    MOV a0, s2
    LA a1, .Loutput
    LI a2, \count
    LI a3, 0
    LI a4, 0
    LI a5, 777
    SC 22
    CHECK a0, \result, \code
.endm
.macro OUTPUT mask, data, code
    LA t1, .Loutput
    LW t2, t1, 0
    CHECK t2, \mask, \code
    LW t2, t1, 4
    CHECK t2, 0, \code
    LD t2, t1, 8
    CHECK t2, \data, \code
.endm
.macro POLL count, time, result, code
    LA a0, .Lpollfds
    LI a1, \count
    LA a2, \time
    LI a3, 0
    LI a4, 777
    SC 73
    CHECK a0, \result, \code
.endm
.macro SELECT n, result, code
    LI a0, \n
    LA a1, .Lsets
    ADDI a2, a1, 16
    ADDI a3, a1, 32
    LA a4, .Lzero_time
    LI a5, 0
    SC 72
    CHECK a0, \result, \code
.endm
.macro MASK value, code
    LA t1, .Lmask_input
    LI t2, \value
    SD t2, t1, 0
    LI a0, 2
    MOV a1, t1
    LI a2, 0
    LI a3, 8
    SC 135
    OK \code
.endm
.macro QUERY_MASK value, code
    LI a0, 999
    LI a1, 0
    LA a2, .Lmask_output
    LI a3, 8
    SC 135
    OK \code
    LA t1, .Lmask_output
    LD t2, t1, 0
    CHECK t2, \value, \code
.endm
.macro CLOCK label, code
    LI a0, 1
    LA a1, \label
    SC 113
    OK \code
.endm
.macro ELAPSED minimum, code
    LA t1, .Lclock_before
    LD t2, t1, 0
    LD t3, t1, 8
    LA t1, .Lclock_after
    LD t4, t1, 0
    LD t5, t1, 8
    SUB t4, t4, t2
    LI t1, 1000000000
    MUL t4, t4, t1
    SUB t5, t5, t3
    ADD t4, t4, t5
    LI t1, \minimum
    LI s8, \code
    BLT t4, t1, .Lexit
.endm
# /// gate 和 childdone 复位完成后才发表下一轮等待身份。
.macro PHASE n
    SW zero, s6, 4
    SW zero, s6, 16
    LI t1, \n
    FENCE
    SW t1, s6, 0
    FENCE
.endm
.macro DONE n, code
995:
    LW t1, s6, 16
    LI t2, \n
    BEQ t1, t2, 996f
    SC 124
    JMP 995b
996:
    FENCE
    CHECK t1, \n, \code
.endm

# /// 七个单字母模式沿正式文件、信号、VM 与事件 syscall，不设置测试 syscall。
.global _start
.type _start, @function
_start:
    LD t1, sp, 0
    CHECK t1, 2, 1
    LD t1, sp, 16
    LB s7, t1, 0
    SC 172
    MOV s5, a0
    LI a0, 0x44000000
    LI a1, 4096
    LI a2, 3
    LI a3, 0x31
    LI a4, -1
    LI a5, 0
    SC 222
    CHECK a0, 0x44000000, 2
    MOV s6, a0
    LA a0, .Lfds
    LI a1, 0x80000
    SC 59
    OK 3
    LA t1, .Lfds
    LW s0, t1, 0
    LW s1, t1, 4
    CHECK s0, 0, 4
    CHECK s1, 1, 5
    LI a0, 0x80000
    SC 20
    CHECK a0, 2, 6
    MOV s2, a0
    LI t0, 97
    BEQ s7, t0, .Labi
    LI t0, 116
    BEQ s7, t0, .Ltime
    LI t0, 98
    BEQ s7, t0, .Lblocked
    LI t0, 115
    BEQ s7, t0, .Lsignals
    LI t0, 101
    BEQ s7, t0, .Lready
    LI t0, 114
    BEQ s7, t0, .Lreuse
    LI t0, 102
    BEQ s7, t0, .Lfault
    LI s8, 7
    JMP .Lexit

# /// pollfd 的 signed fd、16 位输入/输出和重复项计数均由用户字节断言。
.Labi:
    LI a0, -100
    LA a1, .Lpath
    LI a2, 0
    LI a3, 0
    SC 56
    CHECK a0, 3, 10
    MOV s3, a0
    LA t1, .Lpollfds
    LI t2, -1
    SW t2, t1, 0
    SH t2, t1, 4
    SH t2, t1, 6
    LI t2, 400
    SW t2, t1, 8
    SH zero, t1, 12
    LI t2, -1
    SH t2, t1, 14
    SW s1, t1, 16
    SW s1, t1, 24
    LI t2, 4
    SH t2, t1, 20
    SH t2, t1, 28
    LI t2, -1
    SH t2, t1, 22
    SH t2, t1, 30
    POLL 4, .Lzero_time, 3, 11
    LA t1, .Lpollfds
    LH t2, t1, 6
    CHECK t2, 0, 12
    LH t2, t1, 14
    CHECK t2, 32, 13
    LH t2, t1, 22
    CHECK t2, 4, 14
    LH t2, t1, 30
    CHECK t2, 4, 15
    SW s3, t1, 0
    LI t2, 0x145
    SH t2, t1, 4
    POLL 1, .Lzero_time, 1, 16
    LA t1, .Lpollfds
    LH t2, t1, 6
    CHECK t2, 0x145, 17
    LI a0, EVENT_USER_END
    LI a1, 0x100000000
    LA a2, .Lzero_time
    LI a3, 0
    LI a4, 999
    SC 73
    OK 18

# /// 大数组的末项仍参与就绪查询，负 fd 项的 revents 必须清零。
    LA t1, .Lpollfds
    LI t2, 129
    LI t3, -1
    LI t4, 4
.Lmany_pollfd_init:
    SW t3, t1, 0
    SH t4, t1, 4
    SH t3, t1, 6
    ADDI t1, t1, 8
    ADDI t2, t2, -1
    BNE t2, zero, .Lmany_pollfd_init
    ADDI t1, t1, -8
    SW s1, t1, 0
    POLL 129, .Lzero_time, 1, 201
    LA t1, .Lpollfds
    LH t2, t1, 6
    CHECK t2, 0, 202
    ADDI t1, t1, 1024
    LH t2, t1, 6
    CHECK t2, 4, 203

    POLL -1, .Lzero_time, -22, 20
    LI a0, EVENT_USER_END
    LI a1, 1
    LA a2, .Lzero_time
    LI a3, 0
    LI a4, 8
    SC 73
    CHECK a0, -14, 21

# /// 同一普通 OFD 同时出现在读/写集合，另一个写端使三个独立集合结果计数为三。
    LA t1, .Lsets
    LI t2, 8
    SD t2, t1, 0
    LI t2, 10
    SD t2, t1, 16
    LI t2, 8
    SD t2, t1, 32
    SELECT 4, 3, 22
    LA t1, .Lsets
    LD t2, t1, 0
    CHECK t2, 8, 23
    LD t2, t1, 16
    CHECK t2, 10, 24
    LD t2, t1, 32
    CHECK t2, 0, 25
    SELECT -1, -22, 26
    LA t1, .Lsets
    LI t2, 1 << 7
    SD t2, t1, 0
    SD zero, t1, 16
    SELECT 8, -9, 27
    LI a0, 0
    LI a1, EVENT_USER_END
    LI a2, EVENT_USER_END
    LI a3, EVENT_USER_END
    LA a4, .Lzero_time
    LA a5, .Lnull_pack
    SC 72
    OK 28
    LI a0, -1
    LI a1, 0
    LI a2, 0
    LI a3, 0
    LI a4, 0
    LI a5, EVENT_USER_END
    SC 72
    CHECK a0, -14, 29

# /// 65 个 fd 的真实跨度为 16 字节，4 个 fd 仅为 8 字节；越过该跨度的页不访问。
    MOV a0, s3
    LI a1, 0
    LI a2, 64
    SC 25
    CHECK a0, 64, 30
    MOV s4, a0
    LI a0, 0x45000000
    LI a1, 8192
    LI a2, 3
    LI a3, 0x32
    LI a4, -1
    LI a5, 0
    SC 222
    CHECK a0, 0x45000000, 31
    LI t1, 0x45000ff8
    LI t2, 8
    SD t2, t1, 0
    LI t2, 1
    SD t2, t1, 8
    LI a0, 65
    MOV a1, t1
    LI a2, 0
    LI a3, 0
    LA a4, .Lzero_time
    LI a5, 0
    SC 72
    CHECK a0, 2, 32
    LI t1, 0x45000ff8
    LD t2, t1, 0
    CHECK t2, 8, 33
    LD t2, t1, 8
    CHECK t2, 1, 34
    LI a0, 0x45001000
    LI a1, 4096
    SC 215
    OK 35
    LI a0, 4
    LI a1, 0x45000ff8
    LI a2, 0
    LI a3, 0
    LA a4, .Lzero_time
    LI a5, 0
    SC 72
    CHECK a0, 1, 36
    LI a0, 65
    LI a1, 0x45000ff8
    LI a2, 0
    LI a3, 0
    LA a4, .Lzero_time
    LI a5, 0
    SC 72
    CHECK a0, -14, 37
    CLOSE s4, 39

# /// 显式选择高编号空槽，fdset 的末个机器字必须被读取并交付结果。
    MOV a0, s3
    LI a1, 0
    LI a2, 192
    SC 25
    CHECK a0, 192, 204
    MOV s5, a0
    LA t1, .Lsets
    SD zero, t1, 0
    SD zero, t1, 8
    SD zero, t1, 16
    LI t2, 1
    SD t2, t1, 24
    LI a0, 193
    MOV a1, t1
    LI a2, 0
    LI a3, 0
    LA a4, .Lzero_time
    LI a5, 0
    SC 72
    CHECK a0, 1, 205
    LA t1, .Lsets
    LD t2, t1, 24
    CHECK t2, 1, 206
    CLOSE s5, 207

    LI a0, 0x45000000
    LI a1, 4096
    SC 215
    OK 40

# /// 未支持 flags、错误尺寸/范围、fd、能力和 ctl 顺序保留精确 errno。
    LI a0, 1
    SC 20
    CHECK a0, -22, 41
    MOV a0, s2
    LI a1, 1
    MOV a2, s0
    LI a3, EVENT_USER_END
    SC 21
    CHECK a0, -14, 42
    LI a0, -1
    LI a1, 3
    LI a2, -1
    LI a3, EVENT_USER_END
    SC 21
    CHECK a0, -14, 43
    EVENT 1, 0xfedcba9876543210
    MOV a0, s1
    LI a1, 1
    MOV a2, s3
    LA a3, .Levent
    SC 21
    CHECK a0, -1, 44
    MOV a0, s1
    LI a1, 1
    MOV a2, s0
    LA a3, .Levent
    SC 21
    CHECK a0, -22, 45
    CTL 1, s2, -22, 46
    LI a0, 0
    SC 20
    CHECK a0, 4, 47
    MOV s4, a0
    CTL 1, s4, -22, 48
    CLOSE s4, 49
    MOV a0, s2
    LI a1, 999
    MOV a2, s0
    LI a3, EVENT_USER_END
    SC 21
    CHECK a0, -22, 50
    EVENT 0x10000001, 0xfedcba9876543210
    CTL 1, s0, -22, 51
    EVENT 1, 0xfedcba9876543210
    CTL 3, s0, -2, 52
    CTL 1, s0, 0, 53
    CTL 1, s0, -17, 54
    MOV a0, s2
    LI a1, 2
    MOV a2, s0
    LI a3, EVENT_USER_END
    SC 21
    OK 55
    CTL 2, s0, -2, 56
    WAIT 0, -22, 57
    WAIT -1, -22, 58
    WAIT 134217728, -22, 59
    LI a0, -1
    LI a1, EVENT_USER_END-16
    LI a2, 2
    LI a3, 0
    LI a4, 0
    LI a5, 777
    SC 22
    CHECK a0, -14, 60
    LI a0, -1
    LA a1, .Loutput
    LI a2, 1
    LI a3, 0
    LI a4, 0
    SC 22
    CHECK a0, -9, 61
    MOV a0, s3
    SC 22
    CHECK a0, -22, 62
    MOV a0, s2
    LA a1, .Loutput
    LI a2, 1
    LI a3, 0
    LA a4, .Ltemporary_mask
    LI a5, 16
    SC 22
    CHECK a0, -22, 63
    MOV a0, s2
    LA a1, .Lreceived
    LI a2, 0
    SC 63
    CHECK a0, -22, 64
    MOV a0, s2
    LI a1, EVENT_USER_END
    LI a2, 0
    LI a3, 0
    SC 67
    CHECK a0, -29, 65
    MOV a0, s2
    LI a1, 7
    LI a2, 0
    SC 62
    OK 66
    MOV a0, s2
    LI a1, 3
    LI a2, 0
    SC 25
    CHECK a0, 2, 67
    MOV a0, s2
    LI a1, 1
    SC 25
    CHECK a0, 1, 68
    MOV a0, s2
    LA a1, .Lstat
    SC 80
    OK 69
    LA t1, .Lstat
    LW t2, t1, 16
    CHECK t2, 0x180, 70
    CLOSE s3, 71
    JMP .Lclose_pass

# /// 零 fd 等待使用真实精确 timer；普通剩余时间写回 fault 不改变就绪/timeout 返回值。
.Ltime:
    CLOCK .Lclock_before, 72
    LI a0, EVENT_USER_END
    LI a1, 0
    LA a2, .Ltwenty_ms
    LI a3, 0
    LI a4, 999
    SC 73
    OK 73
    CLOCK .Lclock_after, 74
    ELAPSED 20000000, 75
    LA t1, .Ltwenty_ms
    LD t2, t1, 0
    CHECK t2, 0, 76
    LD t2, t1, 8
    CHECK t2, 0, 77
    LA t1, .Ltwenty_ms
    LI t2, 20000000
    SD t2, t1, 8
    CLOCK .Lclock_before, 78
    LI a0, 0
    LI a1, EVENT_USER_END
    LI a2, EVENT_USER_END
    LI a3, EVENT_USER_END
    LA a4, .Ltwenty_ms
    LA a5, .Lnull_pack
    SC 72
    OK 79
    CLOCK .Lclock_after, 80
    ELAPSED 20000000, 81
    MASK 0xa00, 82
    LA t1, .Lpollfds
    SW s1, t1, 0
    LI t2, 4
    SH t2, t1, 4
    LA a0, .Lpollfds
    LI a1, 1
    LA a2, .Lreadonly_short
    LA a3, .Ltemporary_mask
    LI a4, 8
    SC 73
    CHECK a0, 1, 83
    QUERY_MASK 0xa00, 84
    LI a0, 0
    LI a1, 0
    LA a2, .Lreadonly_short
    LA a3, .Ltemporary_mask
    LI a4, 8
    SC 73
    OK 85
    QUERY_MASK 0xa00, 86
    LA t1, .Lsets
    SD zero, t1, 0
    LI t2, 2
    SD t2, t1, 16
    SD zero, t1, 32
    LI a0, 2
    LI a1, 0
    ADDI a2, t1, 16
    LI a3, 0
    LA a4, .Lreadonly_short
    LA a5, .Lmask_pack
    SC 72
    CHECK a0, 1, 87
    QUERY_MASK 0xa00, 88
    MOV a0, s2
    LA a1, .Loutput
    LI a2, 1
    LI a3, 1
    LA a4, .Ltemporary_mask
    LI a5, 8
    SC 22
    OK 89
    QUERY_MASK 0xa00, 90
    LI a0, 0
    LI a1, 0
    LA a2, .Lbad_time
    LI a3, 0
    LI a4, 8
    SC 73
    CHECK a0, -22, 91
    POLL 0, .Lnegative_seconds, -22, 212
    POLL 0, .Lnegative_nanos, -22, 213
    LI a0, 0
    LI a1, 0
    LA a2, .Lzero_time
    LI a3, EVENT_USER_END
    LI a4, 8
    SC 73
    CHECK a0, -14, 214
    QUERY_MASK 0xa00, 215
    LI a0, 0
    LI a1, 0
    LA a2, .Lzero_time
    LA a3, .Ltemporary_mask
    LI a4, 16
    SC 73
    CHECK a0, -22, 92
    QUERY_MASK 0xa00, 93
    LI a0, -1
    LI a1, 0
    LI a2, 0
    LI a3, 0
    LA a4, .Lzero_time
    LA a5, .Lmask_pack
    SC 72
    CHECK a0, -22, 94
    QUERY_MASK 0xa00, 95
    JMP .Lclose_pass

# /// 子进程只在 native 观察原 Task 为 Blocked 后写真实 pipe；三个入口各经历独立等待。
.Lblocked:
    EVENT 1, 0xfedcba9876543210
    CTL 1, s0, 0, 96
    JMP .Lfork_worker

# /// 原 mask 阻断 USR1，临时 mask 解除它；SA_NODEFER 使 handler 可断言选择时仍为临时 mask。
.Lsignals:
    # 先触页全部 syscall 输入，磁盘 ELF 缺页等待不得被观察成临时 mask 的真实 park。
    LA t1, .Lreadonly_long
    LD t2, t1, 0
    LA t1, .Ltemporary_mask
    LD t2, t1, 0
    LA t1, .Lmask_pack
    LD t2, t1, 0
    LA t1, .Llong_time
    LD t2, t1, 0
    MASK 0xa00, 97
    LA t1, .Laction
    LA t2, .Lhandler
    SD t2, t1, 0
    LI t2, 0x50000000
    SD t2, t1, 8
    SD zero, t1, 16
    LI a0, 10
    MOV a1, t1
    LI a2, 0
    LI a3, 8
    SC 134
    OK 98
.Lfork_worker:
    LI a0, 17
    LI a1, 0
    LI a2, 0
    LI a3, 0
    LI a4, 0
    SC 220
    FD 99
    BEQ a0, zero, .Lworker
    MOV s3, a0
    PHASE 1
    LI t0, 115
    BEQ s7, t0, .Lsignal_poll
    LA t1, .Lpollfds
    SW s0, t1, 0
    LI t2, 1
    SH t2, t1, 4
    LA a0, .Lpollfds
    LI a1, 1
    LI a2, 0
    LI a3, 0
    LI a4, 999
    SC 73
    SW a0, s6, 12
    CHECK a0, 1, 100
    LA t1, .Lpollfds
    LH t2, t1, 6
    CHECK t2, 1, 101
    READ s0, 102
    JMP .Lphase_two
.Lsignal_poll:
    LI a0, EVENT_USER_END
    LI a1, 0
    LA a2, .Lreadonly_long
    LA a3, .Ltemporary_mask
    LI a4, 8
    SC 73
    SW a0, s6, 12
    CHECK a0, -4, 103
    QUERY_MASK 0xa00, 104
    LW t1, s6, 8
    CHECK t1, 1, 105
.Lphase_two:
    DONE 1, 106
    PHASE 2
    LI t0, 115
    BEQ s7, t0, .Lsignal_select
    LA t1, .Lsets
    LI t2, 1
    SD t2, t1, 0
    LI a0, 1
    MOV a1, t1
    LI a2, 0
    LI a3, 0
    LI a4, 0
    LI a5, 0
    SC 72
    SW a0, s6, 12
    CHECK a0, 1, 107
    LA t1, .Lsets
    LD t2, t1, 0
    CHECK t2, 1, 108
    READ s0, 109
    JMP .Lphase_three
.Lsignal_select:
    LI a0, 0
    LI a1, EVENT_USER_END
    LI a2, EVENT_USER_END
    LI a3, EVENT_USER_END
    LA a4, .Llong_time
    LA a5, .Lmask_pack
    SC 72
    SW a0, s6, 12
    CHECK a0, -4, 110
    QUERY_MASK 0xa00, 111
    LW t1, s6, 8
    CHECK t1, 2, 112
.Lphase_three:
    DONE 2, 113
    PHASE 3
    MOV a0, s2
    LA a1, .Loutput
    LI a2, 1
    LI a3, -1
    LI a4, 0
    LI a5, 999
    LI t0, 115
    BNE s7, t0, .Lthird_wait
    LA a4, .Ltemporary_mask
    LI a5, 8
.Lthird_wait:
    SC 22
    SW a0, s6, 12
    LI t0, 115
    BEQ s7, t0, .Lthird_signal
    CHECK a0, 1, 114
    OUTPUT 1, 0xfedcba9876543210, 115
    READ s0, 116
    JMP .Ljoin_worker
.Lthird_signal:
    CHECK a0, -4, 117
    QUERY_MASK 0xa00, 118
    LW t1, s6, 8
    CHECK t1, 3, 119
.Ljoin_worker:
    DONE 3, 120
    MOV a0, s3
    LA a1, .Lwait_status
    LI a2, 0
    LI a3, 0
    SC 260
    EQ a0, s3, 121
    LA t1, .Lwait_status
    LW t2, t1, 0
    CHECK t2, 0, 122
    JMP .Lclose_pass

# /// 共享 gate 只决定写入或发送时刻，真实 child 身份、信号与 pipe 通知保持生产路径。
.Lworker:
    LI s4, 1
.Lworker_gate:
    LW t1, s6, 0
    BNE t1, s4, .Lworker_yield
    LW t1, s6, 4
    LI t2, 1
    BNE t1, t2, .Lworker_yield
    FENCE
    LI t0, 115
    BEQ s7, t0, .Lworker_signal
    WRITE s1, 123
    JMP .Lworker_done
.Lworker_signal:
    MOV a0, s5
    LI a1, 10
    SC 129
    OK 124
.Lworker_done:
    FENCE
    SW s4, s6, 16
    FENCE
    ADDI s4, s4, 1
    LI t1, 4
    BEQ s4, t1, .Lpass
    JMP .Lworker_gate
.Lworker_yield:
    SC 124
    JMP .Lworker_gate

# /// 普通 handler 查询真实 blocked mask；sigreturn 必须恢复等待之前的原 mask。
.Lhandler:
    QUERY_MASK 0x800, 125
    LW t1, s6, 8
    ADDI t1, t1, 1
    SW t1, s6, 8
    RETURN

# /// 初始就绪、MOD 重新观察、LT 重复交付、ET 新通知和 ONESHOT 完整交付后禁用。
.Lready:
    EVENT 1, 0xfedcba9876543210
    WRITE s1, 128
    CTL 1, s0, 0, 126
    WAIT 1, 1, 129
    OUTPUT 1, 0xfedcba9876543210, 130
    WAIT 1, 1, 131
    READ s0, 132
    WAIT 1, 0, 133
    EVENT 1, 0x8877665544332211
    CTL 1, s1, 0, 134
    WAIT 2, 0, 135
    EVENT 4, 0x8877665544332211
    CTL 3, s1, 0, 136
    WAIT 1, 1, 137
    OUTPUT 4, 0x8877665544332211, 138
    CTL 2, s1, 0, 139
    EVENT 0x40000001, 0xfedcba9876543210
    CTL 3, s0, 0, 140
    WRITE s1, 141
    WAIT 1, 1, 142
    OUTPUT 1, 0xfedcba9876543210, 143
    WAIT 1, 0, 144
    CTL 3, s0, 0, 145
    WAIT 1, 1, 146
    WAIT 1, 0, 147
    READ s0, 148
    WRITE s1, 149
    WAIT 1, 0, 150
    CTL 3, s0, 0, 151
    WAIT 1, 1, 152
    EVENT 0x80000001, 0xfedcba9876543210
    CTL 3, s0, 0, 153
    WAIT 1, 1, 154
    WAIT 1, 0, 155
    READ s0, 156
    WRITE s1, 157
    WAIT 1, 1, 158
    WAIT 1, 0, 159
    EVENT 1, 0xfedcba9876543210
    CTL 3, s0, 0, 160
    WAIT 1, 1, 161
    WAIT 1, 1, 162
    READ s0, 163
    WAIT 1, 0, 164
    JMP .Lclose_pass

# /// 三份不同 opaque data 的成员和计数共同校验输出，不要求 ready 链的交付次序。
.macro OUTPUT_READERS count, sum, code
    LA t1, .Loutput
    LI t4, 0
    LI t5, \count
994:
    LW t2, t1, 0
    CHECK t2, 1, \code
    LW t2, t1, 4
    CHECK t2, 0, \code
    LD t2, t1, 8
    LI t3, 0xfedcba9876543210
    BEQ t2, t3, 995f
    LI t3, 0x8877665544332211
    BEQ t2, t3, 996f
    CHECK t2, 0x1122334455667788, \code
    LI t3, 4
    JMP 997f
995:
    LI t3, 1
    JMP 997f
996:
    LI t3, 2
997:
    ADD t4, t4, t3
    ADDI t1, t1, 16
    ADDI t5, t5, -1
    BNE t5, zero, 994b
    CHECK t4, \sum, \code
.endm

# /// dup 分别 ADD 保存两个键；整数 fd 复用新建第三个键，末 OFD close 同步删除原两个 watch。
.Lreuse:
    EVENT 1, 0xfedcba9876543210
    CTL 1, s0, 0, 165
    MOV a0, s0
    SC 23
    CHECK a0, 3, 166
    MOV s3, a0
    EVENT 1, 0x1122334455667788
    CTL 1, s3, 0, 211
    CLOSE s0, 167
    LA a0, .Lfds
    LI a1, 0
    SC 59
    OK 168
    LA t1, .Lfds
    LW s0, t1, 0
    LW s4, t1, 4
    CHECK s0, 0, 169
    CHECK s4, 4, 170
    EVENT 1, 0x8877665544332211
    CTL 1, s0, 0, 171
    WRITE s1, 172
    WAIT 3, 2, 173
    OUTPUT_READERS 2, 5, 174
    WRITE s4, 175
    WAIT 3, 3, 176
    OUTPUT_READERS 3, 7, 177
    CLOSE s3, 180
    WAIT 2, 1, 181
    OUTPUT 1, 0x8877665544332211, 182
    CLOSE s0, 183
    WAIT 2, 0, 184
    CLOSE s4, 185
    CLOSE s1, 186
    CLOSE s2, 187
    JMP .Lpass

# /// 首项 fault 不消费 ONESHOT；合法总范围的中途未映射页只保完整成功前缀。
.Lfault:
    EVENT 0xc0000001, 0xfedcba9876543210
    CTL 1, s0, 0, 188
    EVENT 0xc0000004, 0x8877665544332211
    CTL 1, s1, 0, 189
    WRITE s1, 190
    MOV a0, s2
    LI a1, 0x48000000
    LI a2, 2
    LI a3, 0
    LI a4, 0
    LI a5, 8
    SC 22
    CHECK a0, -14, 191
    LI a0, 0x46000000
    LI a1, 4096
    LI a2, 3
    LI a3, 0x32
    LI a4, -1
    LI a5, 0
    SC 222
    CHECK a0, 0x46000000, 192
    MOV a0, s2
    LI a1, 0x46000ff0
    LI a2, 2
    LI a3, 0
    LI a4, 0
    LI a5, 8
    SC 22
    CHECK a0, 1, 193
    LI t1, 0x46000ff0
    LW t2, t1, 4
    CHECK t2, 0, 194
    LD s3, t1, 8
    WAIT 2, 1, 195
    LA t1, .Loutput
    LW t2, t1, 4
    CHECK t2, 0, 196
    LD t3, t1, 8
    LI s8, 197
    BEQ t3, s3, .Lexit
    LI t4, 0xfedcba9876543210
    BEQ s3, t4, .Lfault_reader_first
    CHECK s3, 0x8877665544332211, 198
    CHECK t3, 0xfedcba9876543210, 199
    JMP .Lfault_rearm
.Lfault_reader_first:
    CHECK t3, 0x8877665544332211, 200
.Lfault_rearm:
    WAIT 2, 0, 201
    EVENT 0x40000001, 0xfedcba9876543210
    CTL 3, s0, 0, 202
    MOV a0, s2
    LI a1, 0x48000000
    LI a2, 1
    LI a3, 0
    LI a4, 0
    LI a5, 8
    SC 22
    CHECK a0, -14, 203
    WAIT 1, 1, 204
    OUTPUT 1, 0xfedcba9876543210, 205
    WAIT 1, 0, 206
    LI a0, 0x46000000
    LI a1, 4096
    SC 215
    OK 207
    JMP .Lclose_pass

# /// 全部显式 fd 关闭后再正常退出，Mm/最后运行拥有仍由生产退出边界释放。
.Lclose_pass:
    CLOSE s0, 208
    CLOSE s1, 209
    CLOSE s2, 210
.Lpass:
    LI s8, 0
.Lexit:
    MOV a0, s8
    SC 93
.Lexit_spin:
    JMP .Lexit_spin

.section .rodata
.balign 8
.Lpath: .asciz "/test/data.bin"
.Lbyte: .byte 81
.balign 8
.Lzero_time: .dword 0, 0
.Lreadonly_short: .dword 0, 1000000
.Lreadonly_long: .dword 10, 0
.Lbad_time: .dword 0, 1000000000
.Lnegative_seconds: .dword -1, 0
.Lnegative_nanos: .dword 0, -1
.Ltemporary_mask: .dword 0x800
.Lnull_pack: .dword 0, 999
.Lmask_pack: .dword .Ltemporary_mask, 8

.data
.balign 8
.Ltwenty_ms: .dword 0, 20000000
.Llong_time: .dword 10, 0

.bss
.balign 16
.Lfds: .space 8
.Lpollfds: .space 129 * 8
.Lsets: .space 48
.Levent: .space 16
.Loutput: .space 48
.Lreceived: .space 8
.Lmask_input: .space 8
.Lmask_output: .space 8
.Laction: .space 24
.Lclock_before: .space 16
.Lclock_after: .space 16
.Lwait_status: .space 8
.Lstat: .space 128
