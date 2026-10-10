.text
.option norelax
.option norvc

# /// 普通 Linux RV64 raw ABI；所有错误出口保留真实 syscall 结果。
.macro SC number
    li a7, \number
    ecall
.endm
.macro ARGS
    li a0, 0
    li a1, 0
    li a2, 0
    li a3, 0
    li a4, 0
    li a5, 0
.endm
.macro CHECK reg, value, code
    li s8, \code
    li t0, \value
    beq \reg, t0, 991f
    j .Lfail
991:
.endm
.macro EQ left, right, code
    li s8, \code
    beq \left, \right, 991f
    j .Lfail
991:
.endm
.macro NONNEG reg, code
    li s8, \code
    bge \reg, zero, 991f
    j .Lfail
991:
.endm
.macro SELECT char, label
    li t0, \char
    bne s7, t0, 991f
    j \label
991:
.endm
.macro PUT offset, value
    li t0, \value
    fence rw, w
    sw t0, \offset(s6)
.endm
.macro WAIT offset, value
991:
    lw t0, \offset(s6)
    fence r, rw
    li t1, \value
    beq t0, t1, 992f
    SC 124
    j 991b
992:
.endm
# /// gate 仅协调 UART 配额，实际 read、wait 和停止由各自生产 syscall 完成。
.macro GATE phase
    PUT 4, 0
    PUT 0, \phase
    WAIT 4, \phase
.endm
.macro DONE phase
    PUT 8, \phase
.endm
.macro OPEN path, flags, code
    li a0, -100
    lla a1, \path
    li a2, \flags
    li a3, 0
    SC 56
    NONNEG a0, \code
.endm
.macro CLOSE fd, code
    mv a0, \fd
    SC 57
    CHECK a0, 0, \code
.endm
.macro IOCTL request, label, result, code
    mv a0, s0
    li a1, \request
    lla a2, \label
    SC 29
    sw a0, 12(s6)
    CHECK a0, \result, \code
.endm
.macro ACTION sig, handler, flags, code
    lla t1, .Laction
    li t0, \handler
    sd t0, 0(t1)
    li t0, \flags
    sd t0, 8(t1)
    sd zero, 16(t1)
    li a0, \sig
    mv a1, t1
    li a2, 0
    li a3, 8
    SC 134
    CHECK a0, 0, \code
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
    CHECK a0, 0, \code
.endm
.macro FORK target, code
    ARGS
    li a0, 17
    SC 220
    NONNEG a0, \code
    bnez a0, 991f
    j \target
991:
    mv s2, a0
    fence rw, w
    sw s2, 16(s6)
.endm
.macro PGRP code
    ARGS
    SC 154
    CHECK a0, 0, \code
.endm
.macro FG pid, code
    lla a2, .Lpid
    sw \pid, 0(a2)
    mv a0, s0
    li a1, 0x5410
    SC 29
    CHECK a0, 0, \code
.endm
.macro SEND pid, sig, code
    mv a0, \pid
    li a1, \sig
    SC 129
    CHECK a0, 0, \code
.endm
.macro REAP pid, options, status, code
    mv a0, \pid
    lla a1, .Lstatus
    li a2, \options
    li a3, 0
    SC 260
    EQ a0, \pid, \code
    lla t1, .Lstatus
    lw t2, 0(t1)
    CHECK t2, \status, \code
.endm
.macro STOP pid, sig, code
    REAP \pid, 2, ((\sig << 8) | 127), \code
    lw t1, 32(s6)
    addi t1, t1, 1
    fence rw, w
    sw t1, 32(s6)
.endm
.macro READ fd, result, code
    mv a0, \fd
    lla a1, .Lbyte
    li a2, 1
    SC 63
    sw a0, 12(s6)
    CHECK a0, \result, \code
.endm
.macro BYTE value, code
    lla t1, .Lbyte
    lbu t2, 0(t1)
    CHECK t2, \value, \code
.endm
.macro WRITE fd, value, code
    lla a1, .Lbyte
    li t0, \value
    sb t0, 0(a1)
    mv a0, \fd
    li a2, 1
    SC 64
    sw a0, 12(s6)
    CHECK a0, 1, \code
.endm
# /// termios 固定 36 字节，控制字符保留真实 INT/QUIT/TSTP 且无 echo。
.macro CONFIG iflag, lflag, code
    lla t1, .Lconfig
    li t0, \iflag
    sw t0, 0(t1)
    sw zero, 4(t1)
    li t0, 0xbf
    sw t0, 8(t1)
    li t0, \lflag
    sw t0, 12(t1)
    mv a0, s0
    li a1, 0x5402
    mv a2, t1
    SC 29
    sw a0, 12(s6)
    CHECK a0, 0, \code
.endm
.macro PIPE readfd, writefd, code
    lla a0, .Lfds
    li a1, 0
    SC 59
    CHECK a0, 0, \code
    lla t1, .Lfds
    lw \readfd, 0(t1)
    lw \writefd, 4(t1)
    NONNEG \readfd, \code
    NONNEG \writefd, \code
    li s8, \code
    bne \readfd, \writefd, 991f
    j .Lfail
991:
.endm
.macro TAKE result, code
    lla a0, .Lwinch_mask
    lla a1, .Linfo
    lla a2, .Lzero_time
    li a3, 8
    SC 137
    CHECK a0, \result, \code
.endm
.macro WINCH code
    TAKE 28, \code
    lla t1, .Linfo
    lw t2, 0(t1)
    CHECK t2, 28, \code
    lw t2, 8(t1)
    CHECK t2, 128, \code
.endm
.macro SIZE value, code
    lla t1, .Lwinsize
    li t0, \value
    sd t0, 0(t1)
    IOCTL 0x5414, .Lwinsize, 0, \code
.endm
.macro GETSIZE value, code
    IOCTL 0x5413, .Lgetsize, 0, \code
    lla t1, .Lgetsize
    ld t2, 0(t1)
    CHECK t2, \value, \code
.endm

# /// q 使用普通 exec 参数中的实际 fd，不假设匿名共享 VMA 跨 exec 保留。
.global _start
.type _start, @function
_start:
    li s6, 0
    ld t0, 0(sp)
    li t1, 2
    li s8, 1
    bge t0, t1, 1f
    j .Lfail
1:
    ld t0, 16(sp)
    lbu s7, 0(t0)
    SELECT 113, .Lexec_q
    ARGS
    li a0, 0x44000000
    li a1, 4096
    li a2, 3
    li a3, 0x31
    li a4, -1
    SC 222
    CHECK a0, 0x44000000, 2
    mv s6, a0
    SC 172
    mv s1, a0
    SELECT 104, .Lhangup
    OPEN .Lserial, 2, 3
    mv s0, a0
    SELECT 106, .Ljobs
    SELECT 119, .Lwindow
    li s8, 4
    j .Lfail

# /// 后台 read 的 TTIN 停止、前台 CONT 与串口输入属于同一真实子执行者。
.Ljobs:
    CONFIG 0, 0x101, 5
    ACTION 22, 1, 0, 6
    PUT 64, 0
    FORK .Lj_reader, 7
    STOP s2, 21, 8
    FG s2, 9
    # 根组无用户父连接；后台孤儿的前台 setter 与普通访问保留各自错误。
    ACTION 22, 0, 0, 201
    IOCTL 0x5410, .Lpid, -25, 202
    IOCTL 0x5402, .Lconfig, -5, 203
    READ s0, -5, 204
    ACTION 22, 1, 0, 205
    SEND s2, 18, 10
    REAP s2, 8, 65535, 11
    PUT 36, 1
    GATE 1
    WAIT 28, 2
    DONE 1
    GATE 2
    PUT 64, 2
    REAP s2, 0, 2, 12
    FG s1, 13
    DONE 2

    PUT 64, 0
    FORK .Lj_suspended, 14
    WAIT 28, 3
    FG s2, 15
    GATE 3
    PUT 64, 3
    STOP s2, 20, 16
    FG s1, 17
    SEND s2, 18, 18
    STOP s2, 21, 19
    FG s2, 20
    SEND s2, 18, 21
    DONE 3
    GATE 4
    REAP s2, 0, 0, 22
    FG s1, 23
    DONE 4

    FORK .Lj_writer, 24
    STOP s2, 22, 25
    FG s2, 26
    GATE 5
    SEND s2, 18, 27
    REAP s2, 0, 0, 28
    FG s1, 29
    DONE 5

    PUT 64, 0
    FORK .Lj_masks, 30
    WAIT 28, 6
    GATE 6
    PUT 64, 6
    REAP s2, 0, 0, 31
    DONE 6

    FORK .Lj_setter, 32
    STOP s2, 22, 33
    GATE 7
    FG s2, 34
    SEND s2, 18, 35
    REAP s2, 0, 0, 36
    FG s1, 37
    CONFIG 0, 0x101, 38
    DONE 7

    PUT 64, 0
    FORK .Lj_middle, 39
    WAIT 28, 8
    GATE 8
    PUT 64, 8
    REAP s2, 0, 0, 40
    lw a0, 20(s6)
    lla a1, .Lstatus
    li a2, 0
    li a3, 0
    SC 260
    CHECK a0, -10, 41
    DONE 8
    j .Lfinish

# /// 默认 TTOU 从父的 ignore 独立恢复，背景 read 不自行模拟停止。
.Lj_reader:
    PGRP 42
    ACTION 22, 0, 0, 43
    READ s0, 1, 44
    BYTE 97, 45
    PUT 28, 2
    WAIT 64, 2
    READ s0, 1, 46
    li s8, 47
    j .Lfail

# /// SIGTSTP/TTIN 都重启同一次 read，最终仅 b 才能使该调用完成。
.Lj_suspended:
    PGRP 48
    ACTION 22, 0, 0, 49
    PUT 28, 3
    WAIT 64, 3
    READ s0, 1, 50
    BYTE 98, 51
    j .Lsuccess

# /// TOSTOP 必须在接收 UART 配额前停止写者，CONT 后只发送 D。
.Lj_writer:
    PGRP 52
    ACTION 22, 0, 0, 53
    WRITE s0, 68, 54
    j .Lsuccess

# /// TTIN 的 mask/ignore 返回 EIO；同样的 TTOU 处置允许普通输出。
.Lj_masks:
    PGRP 55
    ACTION 22, 0, 0, 56
    PUT 28, 6
    WAIT 64, 6
    MASK 0, 0x100000, 57
    READ s0, -5, 58
    MASK 1, 0x100000, 59
    ACTION 21, 1, 0, 60
    READ s0, -5, 61
    MASK 0, 0x200000, 62
    WRITE s0, 77, 63
    MASK 1, 0x200000, 64
    ACTION 22, 1, 0, 65
    WRITE s0, 73, 66
    j .Lsuccess

# /// raw29 被默认停止信号打断后从原 PC 重启，getter 验证实际完整提交。
.Lj_setter:
    PGRP 67
    ACTION 22, 0, 0, 68
    CONFIG 0x20, 0x101, 69
    IOCTL 0x5401, .Lgetconfig, 0, 70
    lla t1, .Lgetconfig
    lw t2, 0(t1)
    CHECK t2, 0x20, 71
    lw t2, 12(t1)
    CHECK t2, 0x101, 72
    j .Lsuccess

# /// 中间父消费真实 SIGSTOP，再退出消除孙组的最后同会话父连接。
.Lj_middle:
    PGRP 73
    ACTION 22, 0, 0, 74
    ARGS
    li a0, 17
    SC 220
    NONNEG a0, 75
    bnez a0, 1f
    j .Lj_orphan
1:
    mv s2, a0
    STOP s2, 19, 76
    sw s2, 20(s6)
    PUT 28, 8
    WAIT 64, 8
    j .Lsuccess

# /// HUP 应在 CONT 恢复后按默认处置终止，走到正常出口即为失败。
.Lj_orphan:
    PGRP 77
    SC 172
    mv s2, a0
    SEND s2, 19, 78
    li s8, 79
    j .Lfail

# /// 三种打开角色与 dup 共 OFD 先以无 ctty 的根进程观察。
.Lhangup:
    OPEN .Lconsole, 2, 80
    mv s9, a0
    li a0, -100
    lla a1, .Ltty
    li a2, 2
    li a3, 0
    SC 56
    CHECK a0, -6, 81
    OPEN .Lserial, 0x102, 82
    mv s0, a0
    mv a0, s0
    SC 23
    NONNEG a0, 83
    mv s10, a0
    CLOSE s0, 84
    mv s0, s10
    IOCTL 0x5401, .Lgetconfig, 0, 85
    li a0, -100
    lla a1, .Ltty
    li a2, 2
    li a3, 0
    SC 56
    CHECK a0, -6, 86
    PIPE s3, s4, 87
    PIPE s5, s11, 88
    FORK .Lh_leader, 89
    READ s3, 1, 90
    BYTE 81, 91
    GATE 1
    WRITE s11, 65, 92
    REAP s2, 0, 0, 93
    mv a0, s0
    call .Lhungup_fd
    mv a0, s9
    li a1, 0x5401
    lla a2, .Lgetconfig
    SC 29
    CHECK a0, 0, 94
    DONE 1

    # 数字角色已由真实 wait 退休；诊断 Arc 不得阻止首空槽复用。
    mv s7, s2
    FORK .Lh_reused, 95
    EQ s2, s7, 96
    WAIT 28, 2
    GATE 2
    WRITE s11, 65, 97
    REAP s2, 0, 0, 98
    DONE 2

    mv s10, s0
    OPEN .Lserial, 0x102, 99
    mv s0, a0
    CONFIG 0, 0, 100
    mv a0, s10
    call .Lhungup_fd
    GATE 3
    READ s0, 1, 101
    BYTE 72, 102
    WRITE s0, 78, 103
    mv a0, s10
    call .Lhungup_fd
    DONE 3
    j .Lfinish

# /// 子会话自动取得 serial，后代 fork 保 ctty，setsid 只清后代自己的关联。
.Lh_leader:
    SC 172
    mv s2, a0
    SC 157
    EQ a0, s2, 104
    OPEN .Lserial, 2, 105
    mv s0, a0
    OPEN .Ltty, 2, 106
    mv s10, a0
    IOCTL 0x5429, .Lpid, 0, 107
    lla t1, .Lpid
    lw t2, 0(t1)
    EQ t2, s2, 108
    IOCTL 0x540f, .Lpid, 0, 109
    lla t1, .Lpid
    lw t2, 0(t1)
    EQ t2, s2, 110
    # 嵌套 fork 不覆盖根进程已发表的 leader 数字。
    mv s7, s2
    ARGS
    li a0, 17
    SC 220
    NONNEG a0, 111
    bnez a0, 1f
    j .Lh_detached
1:
    mv s2, a0
    REAP s2, 0, 0, 112
    mv s2, s7
    lla t1, .Lq_write
    addi t0, s4, 33
    sb t0, 0(t1)
    lla t1, .Lq_read
    addi t0, s5, 33
    sb t0, 0(t1)
    lla t1, .Lq_tty
    addi t0, s10, 33
    sb t0, 0(t1)
    lla a0, .Lself
    lla a1, .Lq_argv
    lla a2, .Lenvp
    SC 221
    li s8, 113
    j .Lfail

# /// 继承的 tty OFD 可继续存在，但成功 setsid 后新的 /dev/tty 必须为 ENXIO。
.Lh_detached:
    OPEN .Ltty, 2, 114
    mv s10, a0
    SC 172
    mv s2, a0
    SC 157
    EQ a0, s2, 115
    li a0, -100
    lla a1, .Ltty
    li a2, 2
    li a3, 0
    SC 56
    CHECK a0, -6, 116
    CLOSE s10, 117
    fence rw, w
    sw s2, 44(s6)
    PUT 48, 1
    WAIT 64, 1
    j .Lsuccess

# /// 同号新 PGID 的 handler 必须是真实用户处置；普通 pipe 将其保持在可观察等待中。
.Lh_reused:
    PGRP 118
    lla t1, .Laction
    lla t0, .Lusr1
    sd t0, 0(t1)
    sd zero, 8(t1)
    sd zero, 16(t1)
    li a0, 10
    mv a1, t1
    li a2, 0
    li a3, 8
    SC 134
    CHECK a0, 0, 119
    PUT 24, 0
    PUT 28, 2
    READ s5, 1, 120
    BYTE 65, 121
    lw t2, 24(s6)
    CHECK t2, 0, 122
    j .Lsuccess

# /// handler 使用内核在 RA 安装的 RX rt_sigreturn trampoline，保原 signal frame。
.Lusr1:
    li t0, 0x44000000
    lw t1, 24(t0)
    addi t1, t1, 1
    sw t1, 24(t0)
    ret

# /// exec 新映像仅消费继承 fd，SID/PGID 必须仍等于原会话首 PID。
.Lexec_q:
    ld t2, 0(sp)
    CHECK t2, 5, 123
    ld a0, 24(sp)
    call .Ldecode_fd
    mv s4, a0
    ld a0, 32(sp)
    call .Ldecode_fd
    mv s5, a0
    ld a0, 40(sp)
    call .Ldecode_fd
    mv s0, a0
    SC 172
    mv s2, a0
    mv a0, s0
    li a1, 0x5429
    lla a2, .Lpid
    SC 29
    CHECK a0, 0, 124
    lla t1, .Lpid
    lw t2, 0(t1)
    EQ t2, s2, 125
    mv a0, s0
    li a1, 0x540f
    lla a2, .Lpid
    SC 29
    CHECK a0, 0, 126
    lla t1, .Lpid
    lw t2, 0(t1)
    EQ t2, s2, 127
    # q 没有共享页，避免对已由 exec 退休的旧地址进行写入。
    mv a0, s4
    lla a1, .LQ
    li a2, 1
    SC 64
    CHECK a0, 1, 128
    mv a0, s5
    lla a1, .Lbyte
    li a2, 1
    SC 63
    CHECK a0, 1, 129
    BYTE 65, 130
    j .Lsuccess

# /// fd 参数用单个非 NUL 字节编码真实返回值，有限表范围在两端保持。
.Ldecode_fd:
    lbu t1, 0(a0)
    lbu t2, 1(a0)
    CHECK t2, 0, 131
    addi a0, t1, -33
    NONNEG a0, 132
    li t1, 128
    li s8, 133
    blt a0, t1, 1f
    j .Lfail
1:
    ret

# /// 旧 OFD 永久挂断状态在新 open 前后相同；console 另由调用者独立检查。
.Lhungup_fd:
    addi sp, sp, -32
    sd ra, 0(sp)
    sd s0, 8(sp)
    mv s0, a0
    READ s0, 0, 134
    mv a0, s0
    lla a1, .LN
    li a2, 1
    SC 64
    CHECK a0, -5, 135
    lla t1, .Lpollfd
    sw s0, 0(t1)
    li t0, 0x145
    sh t0, 4(t1)
    sh zero, 6(t1)
    mv a0, t1
    li a1, 1
    lla a2, .Lzero_time
    li a3, 0
    li a4, 8
    SC 73
    CHECK a0, 1, 136
    lla t1, .Lpollfd
    lhu t2, 6(t1)
    li t0, 0x15d
    and t2, t2, t0
    CHECK t2, 0x15d, 137
    IOCTL 0x5401, .Lgetconfig, -5, 138
    IOCTL 0x5410, .Lpid, -25, 139
    ld s0, 8(sp)
    ld ra, 0(sp)
    addi sp, sp, 32
    ret

# /// 完整四字段相等不发 WINCH，pixel 变化与跨页 EFAULT 分别走真实 ioctl。
.Lwindow:
    ACTION 22, 1, 0, 140
    MASK 0, 0x8000000, 141
    GETSIZE 0x0000000000500018, 142
    SIZE 0x0000000000500018, 143
    TAKE -11, 144
    SIZE 0x0004000300660021, 145
    WINCH 146
    GETSIZE 0x0004000300660021, 147
    TAKE -11, 148
    SIZE 0x0004000300660021, 149
    TAKE -11, 150
    SIZE 0x0004000500660021, 151
    WINCH 152
    GETSIZE 0x0004000500660021, 153
    ARGS
    li a0, 0x45000000
    li a1, 8192
    li a2, 3
    li a3, 0x32
    li a4, -1
    SC 222
    CHECK a0, 0x45000000, 154
    li t1, 0x45000ffc
    li t0, 0x0063002f
    sw t0, 0(t1)
    li a0, 0x45001000
    li a1, 4096
    li a2, 0
    SC 226
    CHECK a0, 0, 155
    mv a0, s0
    li a1, 0x5414
    li a2, 0x45000ffc
    SC 29
    sw a0, 12(s6)
    CHECK a0, -14, 156
    GETSIZE 0x0004000500660021, 157
    TAKE -11, 158
    li a0, 0x45000000
    li a1, 8192
    SC 215
    CHECK a0, 0, 159
    PIPE s3, s4, 160
    PUT 64, 0
    FORK .Lw_child, 161
    WAIT 28, 1
    FG s2, 162
    PUT 64, 1
    GATE 1
    SIZE 0x0009000700780028, 163
    READ s3, 1, 164
    BYTE 87, 165
    REAP s2, 0, 0, 166
    TAKE -11, 167
    FG s1, 168
    SIZE 0x0000000000500018, 174
    WINCH 175
    TAKE -11, 176
    DONE 1
    j .Lfinish

# /// 只当前 foreground child 在 sigtimedwait 的生产 Waiter 上等待并领取 WINCH。
.Lw_child:
    PGRP 169
    PUT 28, 1
    WAIT 64, 1
    lla a0, .Lwinch_mask
    lla a1, .Linfo
    li a2, 0
    li a3, 8
    SC 137
    CHECK a0, 28, 170
    lla t1, .Linfo
    lw t2, 8(t1)
    CHECK t2, 128, 171
    GETSIZE 0x0009000700780028, 172
    WRITE s4, 87, 173
    j .Lsuccess

# /// UART 和所有子事件结束后才允许最后关闭与根进程正常退出。
.Lfinish:
    GATE 99
.Lsuccess:
    li a0, 0
    SC 93
1:
    j 1b

# /// 失败记录具体用户检查编号，不写伪造成功结果；q 的旧共享页已不存在。
.Lfail:
    beqz s6, 1f
    fence rw, w
    sw a0, 12(s6)
    sw s8, 40(s6)
1:
    mv a0, s8
    SC 93
2:
    j 2b

.data
.balign 8
.Laction: .zero 24
.Lmask: .zero 8
.Lwinch_mask: .dword 0x8000000
.Lzero_time: .zero 16
.Linfo: .zero 128
.Lpid: .word 0
.Lstatus: .word 0
.Lfds: .zero 8
.Lpollfd: .zero 8
.Lwinsize: .zero 8
.Lgetsize: .zero 8
.Lgetconfig: .zero 36
.Lconfig:
    .word 0, 0, 0xbf, 0x101
    .byte 0
    .byte 3, 28, 127, 21, 4, 0, 1, 0, 17, 19, 26, 0, 18, 15, 23, 22, 0, 0, 0
.Lbyte: .byte 0
.LQ: .byte 81
.LN: .byte 78
.Lserial: .asciz "/dev/ttyS0"
.Lconsole: .asciz "/dev/console"
.Ltty: .asciz "/dev/tty"
.Lself: .asciz "/test/bin/user-jobs"
.Lq_mode: .asciz "q"
.Lq_write: .zero 2
.Lq_read: .zero 2
.Lq_tty: .zero 2
.balign 8
.Lq_argv: .dword .Lself, .Lq_mode, .Lq_write, .Lq_read, .Lq_tty, 0
.Lenvp: .dword 0
