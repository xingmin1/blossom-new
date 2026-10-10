.text
# /// 条件跳转仅跨局部短片段，远目标使用 LoongArch 的 26 位无条件分支。
.macro SC number
    li.d $a7, \number
    syscall 0
.endm
.macro Z
    move $a0, $zero
    move $a1, $zero
    move $a2, $zero
    move $a3, $zero
    move $a4, $zero
    move $a5, $zero
.endm
.macro CHECK reg, value, code
    li.d $s8, \code
    li.d $t0, \value
    beq \reg, $t0, 991f
    b .Lfail
991:
.endm
.macro EQ lhs, rhs, code
    li.d $s8, \code
    beq \lhs, \rhs, 991f
    b .Lfail
991:
.endm
.macro NONNEG reg, code
    li.d $s8, \code
    blt \reg, $zero, 991f
    b 992f
991:
    b .Lfail
992:
.endm
# /// 同页门控采用 dbar 发表；yield 不代替 read、wait 或 sigtimedwait 的实际阻塞。
.macro PUT offset, value
    li.d $t1, \value
    dbar 0
    st.w $t1, $s6, \offset
.endm
.macro WAIT offset, value
991:
    ld.w $t1, $s6, \offset
    dbar 0
    li.d $t2, \value
    beq $t1, $t2, 992f
    SC 124
    b 991b
992:
.endm
.macro GATE phase
    PUT 4, 0
    PUT 0, \phase
    WAIT 4, \phase
.endm
.macro DONE phase
    PUT 8, \phase
.endm
.macro OPEN path, flags, code
    li.d $a0, -100
    la.local $a1, \path
    li.d $a2, \flags
    li.d $a3, 0
    SC 56
    NONNEG $a0, \code
.endm
.macro CLOSE fd, code
    move $a0, \fd
    SC 57
    CHECK $a0, 0, \code
.endm
.macro IOCTL request, buffer, result, code
    move $a0, $s0
    li.d $a1, \request
    la.local $a2, \buffer
    SC 29
    st.w $a0, $s6, 12
    CHECK $a0, \result, \code
.endm
# /// clone 的目标 ABI 为 flags、stack、parent_tid、child_tid、tls；普通 fork 继承用户栈。
.macro FORK target, code
    Z
    li.d $a0, 17
    SC 220
    NONNEG $a0, \code
    bnez $a0, 991f
    b \target
991:
    move $s3, $a0
    dbar 0
    st.w $s3, $s6, 16
.endm
.macro PGRP code
    Z
    SC 154
    CHECK $a0, 0, \code
.endm
.macro FG pid, code
    la.local $t1, .Lpid
    st.w \pid, $t1, 0
    IOCTL 0x5410, .Lpid, 0, \code
.endm
.macro SEND pid, sig, code
    move $a0, \pid
    li.d $a1, \sig
    SC 129
    CHECK $a0, 0, \code
.endm
.macro REAP pid, options, status, code
    move $a0, \pid
    la.local $a1, .Lstatus
    li.d $a2, \options
    li.d $a3, 0
    SC 260
    st.w $a0, $s6, 12
    EQ $a0, \pid, \code
    la.local $t1, .Lstatus
    ld.w $t2, $t1, 0
    CHECK $t2, \status, \code
.endm
.macro STOP pid, sig, code
    REAP \pid, 2, ((\sig << 8) | 127), \code
    ld.w $t1, $s6, 32
    addi.d $t1, $t1, 1
    dbar 0
    st.w $t1, $s6, 32
.endm
# /// LoongArch sigaction 是 handler、flags、mask 三个 64 位字段；restorer 来自内核 RX trampoline。
.macro ACTION sig, handler, code
    la.local $t1, .Laction
    li.d $t0, \handler
    st.d $t0, $t1, 0
    st.d $zero, $t1, 8
    st.d $zero, $t1, 16
    li.d $a0, \sig
    move $a1, $t1
    li.d $a2, 0
    li.d $a3, 8
    SC 134
    CHECK $a0, 0, \code
.endm
.macro MASK how, bits, code
    la.local $a1, .Lmask
    li.d $t0, \bits
    st.d $t0, $a1, 0
    li.d $a0, \how
    li.d $a2, 0
    li.d $a3, 8
    SC 135
    CHECK $a0, 0, \code
.endm
.macro DEFAULTS
    ACTION 2, 0, 12
    ACTION 20, 0, 12
    ACTION 21, 0, 12
    ACTION 22, 0, 12
    MASK 2, 0, 12
.endm
.macro READ fd, result, code
    move $a0, \fd
    la.local $a1, .Lbyte
    li.d $a2, 1
    SC 63
    st.w $a0, $s6, 12
    CHECK $a0, \result, \code
.endm
.macro BYTE value, code
    la.local $t1, .Lbyte
    ld.bu $t2, $t1, 0
    CHECK $t2, \value, \code
.endm
.macro WRITE fd, value, code
    move $a0, \fd
    la.local $a1, .Lbyte
    li.d $t0, \value
    st.b $t0, $a1, 0
    li.d $a2, 1
    SC 64
    st.w $a0, $s6, 12
    CHECK $a0, 1, \code
.endm
# /// 正常 pipe2 返回两个真实 fd，保存于映像数据段，fork 继承同一 OFD。
.macro PIPE label, code
    la.local $a0, \label
    li.d $a1, 0
    SC 59
    CHECK $a0, 0, \code
    la.local $t1, \label
    ld.w $t2, $t1, 0
    ld.w $t3, $t1, 4
    NONNEG $t2, \code
    NONNEG $t3, \code
    li.d $s8, \code
    bne $t2, $t3, 991f
    b .Lfail
991:
.endm
.macro FD reg, label, offset
    la.local $t1, \label
    ld.w \reg, $t1, \offset
.endm
# /// 36 字节 termios 保 INT、QUIT、TSTP，raw 的 VMIN=1、VTIME=0。
.macro CONFIG iflag, lflag, code
    la.local $t1, .Lconfig
    li.d $t0, \iflag
    st.w $t0, $t1, 0
    st.w $zero, $t1, 4
    li.d $t0, 0xbf
    st.w $t0, $t1, 8
    li.d $t0, \lflag
    st.w $t0, $t1, 12
    IOCTL 0x5402, .Lconfig, 0, \code
.endm
.macro TAKE result, code
    la.local $a0, .Lwinch_mask
    la.local $a1, .Linfo
    la.local $a2, .Lzero_time
    li.d $a3, 8
    SC 137
    CHECK $a0, \result, \code
.endm
.macro WINCH code
    TAKE 28, \code
    la.local $t1, .Linfo
    ld.w $t2, $t1, 0
    CHECK $t2, 28, \code
    ld.w $t2, $t1, 8
    CHECK $t2, 128, \code
.endm
.macro SIZE label, code
    IOCTL 0x5413, .Lgot_size, 0, \code
    la.local $t1, .Lgot_size
    la.local $t2, \label
    ld.d $t3, $t1, 0
    ld.d $t4, $t2, 0
    EQ $t3, $t4, \code
.endm

# /// 普通 ELF 入口选择 j/h/w；q 的继承 pipe fd 从 exec argv 读取，不保留匿名共享 VMA。
.global _start
.type _start, @function
_start:
    li.d $s6, 0
    ld.d $t1, $sp, 0
    li.d $s8, 1
    li.d $t2, 2
    beq $t1, $t2, .Lnormal_args
    li.d $t2, 5
    beq $t1, $t2, .Lq_args
    b .Lfail
.Lq_args:
    ld.d $t1, $sp, 16
    ld.bu $t2, $t1, 0
    CHECK $t2, 113, 2
    ld.bu $t2, $t1, 1
    CHECK $t2, 0, 2
    ld.d $t1, $sp, 24
    ld.bu $s4, $t1, 0
    addi.d $s4, $s4, -33
    ld.bu $t2, $t1, 1
    CHECK $t2, 0, 2
    ld.d $t1, $sp, 32
    ld.bu $s5, $t1, 0
    addi.d $s5, $s5, -33
    ld.bu $t2, $t1, 1
    CHECK $t2, 0, 2
    ld.d $t1, $sp, 40
    ld.bu $s0, $t1, 0
    addi.d $s0, $s0, -33
    ld.bu $t2, $t1, 1
    CHECK $t2, 0, 2
    NONNEG $s4, 2
    NONNEG $s5, 2
    NONNEG $s0, 2
    b .Lq
.Lnormal_args:
    ld.d $t1, $sp, 16
    ld.bu $s7, $t1, 0
    ld.bu $t2, $t1, 1
    CHECK $t2, 0, 3
    Z
    li.d $a0, 0x44000000
    li.d $a1, 4096
    li.d $a2, 3
    li.d $a3, 0x31
    li.d $a4, -1
    SC 222
    CHECK $a0, 0x44000000, 4
    move $s6, $a0
    SC 172
    NONNEG $a0, 5
    move $s1, $a0
    li.d $a0, 0
    SC 155
    EQ $a0, $s1, 6
    move $s2, $a0
    li.d $t0, 106
    bne $s7, $t0, 1f
    b .Lj
1:
    li.d $t0, 104
    bne $s7, $t0, 2f
    b .Lh
2:
    CHECK $s7, 119, 7
    b .Lw

# /// 八个真实作业控制阶段分别消费用户 wait 的 STOP/CONT/EXIT。
.Lj:
    OPEN .Lserial, 2, 10
    move $s0, $a0
    CONFIG 0, 0x101, 11
    ACTION 22, 1, 12
    PUT 64, 0
    FORK .Lj_first, 13
    STOP $s3, 21, 14
    FG $s3, 15
    # 根组无用户父连接；后台孤儿的前台 setter 与普通访问保留各自错误。
    ACTION 22, 0, 201
    IOCTL 0x5410, .Lpid, -25, 202
    IOCTL 0x5402, .Lconfig, -5, 203
    READ $s0, -5, 204
    ACTION 22, 1, 205
    SEND $s3, 18, 16
    REAP $s3, 8, 65535, 17
    PUT 36, 1
    GATE 1
    WAIT 68, 1
    DONE 1
    GATE 2
    PUT 64, 2
    REAP $s3, 0, 2, 18
    FG $s2, 19
    DONE 2

    PUT 64, 0
    PUT 72, 0
    FORK .Lj_second, 20
    WAIT 72, 1
    FG $s3, 21
    GATE 3
    PUT 64, 3
    STOP $s3, 20, 22
    FG $s2, 23
    SEND $s3, 18, 24
    STOP $s3, 21, 25
    FG $s3, 26
    SEND $s3, 18, 27
    DONE 3
    GATE 4
    REAP $s3, 0, 0, 28
    FG $s2, 29
    DONE 4

    FORK .Lj_write, 30
    STOP $s3, 22, 31
    FG $s3, 32
    GATE 5
    SEND $s3, 18, 33
    REAP $s3, 0, 0, 34
    FG $s2, 35
    DONE 5

    PUT 64, 0
    FORK .Lj_mask, 36
    GATE 6
    PUT 64, 6
    REAP $s3, 0, 0, 37
    DONE 6

    FORK .Lj_setter, 38
    STOP $s3, 22, 39
    GATE 7
    FG $s3, 40
    SEND $s3, 18, 41
    REAP $s3, 0, 0, 42
    IOCTL 0x5401, .Lgetconfig, 0, 43
    la.local $t1, .Lgetconfig
    ld.w $t2, $t1, 0
    CHECK $t2, 0x20, 44
    FG $s2, 45
    DONE 7

    PUT 64, 0
    FORK .Lj_middle, 46
    WAIT 28, 8
    GATE 8
    PUT 64, 8
    REAP $s3, 0, 0, 47
    ld.w $a0, $s6, 20
    la.local $a1, .Lstatus
    li.d $a2, 0
    li.d $a3, 0
    SC 260
    CHECK $a0, -10, 48
    DONE 8
    b .Lfinish

# /// 第一后台 read 必须 TTIN；fg/CONT 后重放同一 read，再由原前台收到 INT。
.Lj_first:
    DEFAULTS
    PGRP 49
    PUT 28, 1
    READ $s0, 1, 50
    BYTE 97, 51
    PUT 68, 1
    WAIT 64, 2
    PUT 28, 2
    READ $s0, 1, 52
    li.d $s8, 53
    b .Lfail

# /// TSTP 的 read 重启在后台先遇 TTIN，再由 fg/CONT 继续读取阶段四的 b。
.Lj_second:
    DEFAULTS
    PGRP 54
    PUT 72, 1
    WAIT 64, 3
    PUT 28, 3
    READ $s0, 1, 55
    BYTE 98, 56
    b .Lsuccess

# /// TOSTOP 的后台 write 在 gate 前停于 TTOU，CONT 后只发送一个 D。
.Lj_write:
    DEFAULTS
    PGRP 57
    PUT 28, 5
    WRITE $s0, 68, 58
    b .Lsuccess

# /// 屏蔽及忽略 TTIN 都返回 EIO；屏蔽及忽略 TTOU 分别允许 M、I。
.Lj_mask:
    DEFAULTS
    PGRP 59
    WAIT 64, 6
    PUT 28, 6
    MASK 0, 0x100000, 60
    READ $s0, -5, 61
    MASK 1, 0x100000, 62
    ACTION 21, 1, 63
    READ $s0, -5, 64
    MASK 0, 0x200000, 65
    WRITE $s0, 77, 66
    MASK 1, 0x200000, 67
    ACTION 22, 1, 68
    WRITE $s0, 73, 69
    b .Lsuccess

# /// 后台 raw29 停止后必须按原参数重启，不能返回 EINTR 或留下半配置。
.Lj_setter:
    DEFAULTS
    PGRP 70
    PUT 28, 7
    CONFIG 0x20, 0x101, 71
    IOCTL 0x5401, .Lgetconfig, 0, 72
    la.local $t1, .Lgetconfig
    ld.w $t2, $t1, 0
    CHECK $t2, 0x20, 73
    b .Lsuccess

# /// middle 留在根进程组，孙子使用不同 PGID；真实父连接消失使停止组孤儿化。
.Lj_middle:
    Z
    li.d $a0, 17
    SC 220
    NONNEG $a0, 74
    bnez $a0, 1f
    b .Lj_grandchild
1:
    move $s3, $a0
    dbar 0
    st.w $s3, $s6, 20
    STOP $s3, 19, 75
    PUT 28, 8
    WAIT 64, 8
    b .Lsuccess
.Lj_grandchild:
    DEFAULTS
    ACTION 1, 0, 76
    PGRP 77
    SC 172
    move $s3, $a0
    SEND $s3, 19, 78
    li.d $s8, 79
    b .Lfail

# /// 三个名字保独立打开身份，session leader 的真实 exec/退出完成挂断。
.Lh:
    OPEN .Lconsole, 2, 80
    la.local $t1, .Lconsole_fd
    st.w $a0, $t1, 0
    li.d $a0, -100
    la.local $a1, .Ltty
    li.d $a2, 2
    li.d $a3, 0
    SC 56
    CHECK $a0, -6, 81
    OPEN .Lserial, 0x102, 82
    move $s0, $a0
    SC 23
    NONNEG $a0, 83
    move $s5, $a0
    CLOSE $s0, 84
    move $s0, $s5
    IOCTL 0x5401, .Lgetconfig, 0, 85
    li.d $a0, -100
    la.local $a1, .Ltty
    li.d $a2, 2
    li.d $a3, 0
    SC 56
    CHECK $a0, -6, 86
    PIPE .Lqpipe, 87
    PIPE .Lapipe, 88
    FORK .Lh_leader, 89
    move $s4, $s3
    FD $s5, .Lqpipe, 0
    READ $s5, 1, 90
    BYTE 81, 91
    GATE 1
    FD $s5, .Lapipe, 4
    WRITE $s5, 65, 92
    REAP $s3, 0, 0, 93
    move $a0, $s0
    bl .Lhungup
    FD $a0, .Lconsole_fd, 0
    li.d $a1, 0x5401
    la.local $a2, .Lgetconfig
    SC 29
    CHECK $a0, 0, 94
    DONE 1

    FORK .Lh_reused, 95
    EQ $s3, $s4, 96
    WAIT 28, 2
    GATE 2
    FD $s5, .Lapipe, 4
    WRITE $s5, 65, 97
    REAP $s3, 0, 0, 98
    DONE 2

    move $s5, $s0
    OPEN .Lserial, 0x102, 99
    move $s0, $a0
    CONFIG 0, 0, 100
    move $a0, $s5
    bl .Lhungup
    GATE 3
    READ $s0, 1, 101
    BYTE 72, 102
    WRITE $s0, 78, 103
    move $a0, $s5
    bl .Lhungup
    DONE 3
    CLOSE $s5, 104
    FD $s5, .Lconsole_fd, 0
    CLOSE $s5, 105
    FD $s5, .Lqpipe, 0
    CLOSE $s5, 105
    FD $s5, .Lqpipe, 4
    CLOSE $s5, 105
    FD $s5, .Lapipe, 0
    CLOSE $s5, 105
    FD $s5, .Lapipe, 4
    CLOSE $s5, 105
    b .Lfinish

# /// leader 自动取得 ctty，子 fork 继承而 setsid 清自身；exec 参数仅使用真实继承 fd。
.Lh_leader:
    SC 172
    move $s4, $a0
    SC 157
    EQ $a0, $s4, 106
    OPEN .Lserial, 2, 107
    move $s0, $a0
    OPEN .Ltty, 2, 108
    move $s5, $a0
    IOCTL 0x5429, .Lpid, 0, 109
    la.local $t1, .Lpid
    ld.w $t2, $t1, 0
    EQ $t2, $s4, 110
    IOCTL 0x540f, .Lpid, 0, 111
    la.local $t1, .Lpid
    ld.w $t2, $t1, 0
    EQ $t2, $s4, 112
    Z
    li.d $a0, 17
    SC 220
    NONNEG $a0, 113
    bnez $a0, 1f
    b .Lh_detached
1:
    move $s3, $a0
    REAP $s3, 0, 0, 114
    FD $t2, .Lqpipe, 4
    addi.d $t2, $t2, 33
    la.local $t1, .Lq_write
    st.b $t2, $t1, 0
    FD $t2, .Lapipe, 0
    addi.d $t2, $t2, 33
    la.local $t1, .Lq_read
    st.b $t2, $t1, 0
    addi.d $t2, $s5, 33
    la.local $t1, .Lq_tty
    st.b $t2, $t1, 0
    la.local $a0, .Lself
    la.local $a1, .Lq_argv
    la.local $a2, .Lenvp
    SC 221
    li.d $s8, 115
    b .Lfail

# /// 继承 OFD 的 TCGETS 不取代新 open 对实际 ctty 关系的检查。
.Lh_detached:
    OPEN .Ltty, 2, 116
    move $s5, $a0
    SC 172
    move $s4, $a0
    SC 157
    EQ $a0, $s4, 117
    li.d $a0, -100
    la.local $a1, .Ltty
    li.d $a2, 2
    li.d $a3, 0
    SC 56
    CHECK $a0, -6, 118
    CLOSE $s5, 119
    dbar 0
    st.w $s4, $s6, 44
    PUT 48, 1
    WAIT 64, 1
    b .Lsuccess

# /// handler 真实存在，普通 pipe 保存新组等待；旧 Pid Arc 的 USR1 不应命中该组。
.Lh_reused:
    PGRP 120
    la.local $t1, .Laction
    la.local $t0, .Lusr1
    st.d $t0, $t1, 0
    st.d $zero, $t1, 8
    st.d $zero, $t1, 16
    li.d $a0, 10
    move $a1, $t1
    li.d $a2, 0
    li.d $a3, 8
    SC 134
    CHECK $a0, 0, 121
    PUT 24, 0
    PUT 28, 2
    FD $s5, .Lapipe, 0
    READ $s5, 1, 122
    BYTE 65, 123
    ld.w $t2, $s6, 24
    CHECK $t2, 0, 124
    b .Lsuccess

# /// 内核在 ra 安装 sigreturn trampoline；handler 不自行拆解或改动 LA 信号帧。
.Lusr1:
    li.d $t0, 0x44000000
    ld.w $t1, $t0, 24
    addi.d $t1, $t1, 1
    dbar 0
    st.w $t1, $t0, 24
    jr $ra

# /// exec 后仍为原 session leader/foreground，q 只使用继承 pipe 完成 Q/A。
.Lq:
    SC 172
    move $s1, $a0
    li.d $a0, 0
    SC 156
    EQ $a0, $s1, 125
    li.d $a0, 0
    SC 155
    EQ $a0, $s1, 126
    move $a0, $s0
    li.d $a1, 0x5429
    la.local $a2, .Lpid
    SC 29
    CHECK $a0, 0, 127
    la.local $t1, .Lpid
    ld.w $t2, $t1, 0
    EQ $t2, $s1, 128
    move $a0, $s0
    li.d $a1, 0x540f
    la.local $a2, .Lpid
    SC 29
    CHECK $a0, 0, 129
    la.local $t1, .Lpid
    ld.w $t2, $t1, 0
    EQ $t2, $s1, 130
    la.local $a1, .Lbyte
    li.d $t0, 81
    st.b $t0, $a1, 0
    move $a0, $s4
    li.d $a2, 1
    SC 64
    CHECK $a0, 1, 131
    move $a0, $s5
    la.local $a1, .Lbyte
    li.d $a2, 1
    SC 63
    CHECK $a0, 1, 132
    BYTE 65, 133
    b .Lsuccess

# /// 旧打开的 read/write/poll/ioctl 永久挂断，console 与新 open 不改变旧 OFD。
.Lhungup:
    addi.d $sp, $sp, -32
    st.d $ra, $sp, 0
    st.d $a0, $sp, 8
    la.local $a1, .Lbyte
    li.d $a2, 1
    SC 63
    CHECK $a0, 0, 134
    ld.d $a0, $sp, 8
    la.local $a1, .Lbyte
    li.d $a2, 1
    SC 64
    CHECK $a0, -5, 135
    ld.d $t2, $sp, 8
    la.local $a0, .Lpoll
    st.w $t2, $a0, 0
    li.d $t0, 0x145
    st.h $t0, $a0, 4
    st.h $zero, $a0, 6
    li.d $a1, 1
    la.local $a2, .Lzero_time
    li.d $a3, 0
    li.d $a4, 0
    SC 73
    CHECK $a0, 1, 136
    la.local $t1, .Lpoll
    ld.hu $t2, $t1, 6
    CHECK $t2, 0x15d, 137
    ld.d $a0, $sp, 8
    li.d $a1, 0x5401
    la.local $a2, .Lgetconfig
    SC 29
    CHECK $a0, -5, 138
    ld.d $a0, $sp, 8
    li.d $a1, 0x5410
    la.local $a2, .Lpid
    SC 29
    CHECK $a0, -25, 139
    ld.d $ra, $sp, 0
    addi.d $sp, $sp, 32
    jr $ra

# /// 完整四 u16 getter、相同值、pixel 变化及跨页失败都由实际 WINCH 消费者检查。
.Lw:
    OPEN .Lserial, 2, 140
    move $s0, $a0
    MASK 0, 0x8000000, 141
    SIZE .Linitial_size, 142
    IOCTL 0x5414, .Linitial_size, 0, 143
    TAKE -11, 144
    IOCTL 0x5414, .Lchanged_size, 0, 145
    WINCH 146
    TAKE -11, 147
    SIZE .Lchanged_size, 148
    IOCTL 0x5414, .Lchanged_size, 0, 149
    TAKE -11, 150
    IOCTL 0x5414, .Lpixel_size, 0, 151
    WINCH 152
    TAKE -11, 153
    SIZE .Lpixel_size, 154

    Z
    li.d $a0, 0x45000000
    li.d $a1, 8192
    li.d $a2, 3
    li.d $a3, 0x32
    li.d $a4, -1
    SC 222
    CHECK $a0, 0x45000000, 155
    li.d $t1, 0x45000ffc
    li.d $t2, 0x00200010
    st.w $t2, $t1, 0
    li.d $a0, 0x45001000
    li.d $a1, 4096
    li.d $a2, 0
    SC 226
    CHECK $a0, 0, 156
    move $a0, $s0
    li.d $a1, 0x5414
    li.d $a2, 0x45000ffc
    SC 29
    CHECK $a0, -14, 157
    SIZE .Lpixel_size, 158
    TAKE -11, 159
    li.d $a0, 0x45000000
    li.d $a1, 8192
    SC 215
    CHECK $a0, 0, 160

    PIPE .Lwpipe, 161
    PUT 64, 0
    PUT 72, 0
    FORK .Lw_child, 162
    WAIT 72, 1
    FG $s3, 163
    PUT 64, 1
    WAIT 28, 1
    GATE 1
    IOCTL 0x5414, .Lforeground_size, 0, 164
    FD $s5, .Lwpipe, 0
    READ $s5, 1, 165
    BYTE 87, 166
    TAKE -11, 167
    REAP $s3, 0, 0, 168
    # fg setter 自身有背景 TTOU 检查，父仅在恢复时屏蔽该信号。
    MASK 0, 0x200000, 169
    FG $s2, 170
    MASK 1, 0x200000, 171
    TAKE -11, 172
    IOCTL 0x5414, .Linitial_size, 0, 173
    WINCH 174
    TAKE -11, 175
    SIZE .Linitial_size, 176
    FD $s5, .Lwpipe, 0
    CLOSE $s5, 177
    FD $s5, .Lwpipe, 4
    CLOSE $s5, 177
    DONE 1
    b .Lfinish

# /// 子进程不同 PGID，前台切换后实际阻塞 sigtimedwait，只有该组收到 WINCH。
.Lw_child:
    PGRP 178
    PUT 72, 1
    WAIT 64, 1
    PUT 28, 1
    la.local $a0, .Lwinch_mask
    la.local $a1, .Linfo
    li.d $a2, 0
    li.d $a3, 8
    SC 137
    CHECK $a0, 28, 179
    la.local $t1, .Linfo
    ld.w $t2, $t1, 0
    CHECK $t2, 28, 180
    ld.w $t2, $t1, 8
    CHECK $t2, 128, 181
    SIZE .Lforeground_size, 182
    TAKE -11, 183
    FD $s5, .Lwpipe, 4
    WRITE $s5, 87, 184
    b .Lsuccess

# /// 99 由原协调者在所有字节和子事件消费后释放，正常退出仍走原 wait/reap/join。
.Lfinish:
    GATE 99
    CLOSE $s0, 190
.Lsuccess:
    li.d $s8, 0
    b .Lexit
.Lfail:
    beqz $s6, .Lexit
    dbar 0
    st.w $s8, $s6, 40
.Lexit:
    move $a0, $s8
    SC 93
1:
    b 1b

.section .rodata
.balign 8
.Lserial: .asciz "/dev/ttyS0"
.Lconsole: .asciz "/dev/console"
.Ltty: .asciz "/dev/tty"
.Lself: .asciz "/test/bin/user-jobs"
.Lq_mode: .asciz "q"
.balign 8
.Lzero_time: .dword 0, 0
.Lwinch_mask: .dword 0x8000000
.Linitial_size: .half 24, 80, 0, 0
.Lchanged_size: .half 31, 97, 320, 200
.Lpixel_size: .half 31, 97, 321, 201
.Lforeground_size: .half 32, 98, 640, 480
.data
.balign 8
.Lq_write: .zero 2
.Lq_read: .zero 2
.Lq_tty: .zero 2
.balign 8
.Lq_argv: .dword .Lself, .Lq_mode, .Lq_write, .Lq_read, .Lq_tty, 0
.Lenvp: .dword 0
.balign 4
.Lconfig: .word 0, 0, 0xbf, 0x101
.byte 0
.byte 3, 28, 127, 21, 4, 0, 1, 0, 17, 19, 26, 0, 18, 15, 23, 22, 0, 0, 0
.bss
.balign 8
.Laction: .space 24
.Lmask: .space 8
.Linfo: .space 128
.Lstatus: .space 4
.Lpid: .space 4
.Lbyte: .space 8
.Lgetconfig: .space 40
.Lpoll: .space 8
.Lgot_size: .space 8
.Lqpipe: .space 8
.Lapipe: .space 8
.Lwpipe: .space 8
.Lconsole_fd: .space 4
