.text
.option norelax
.option norvc

# /// 普通目标 syscall 约定，返回值和具体 errno 均由 a0 消费。
.macro SC number
    li a7, \number
    ecall
.endm
# /// 每个断言点保留独立退出码，分支只跨一条远跳。
# /// t0 用于比较值；调用者在断言后必须重新取得仍要使用的地址。
.macro EXPECT value, number
    li s11, \number
    li t0, \value
    beq a0, t0, 991f
    j .Lexit
991:
.endm
# /// open 和 clone 的非负返回值允许描述符零。
.macro OK number
    li s11, \number
    bge a0, zero, 991f
    j .Lexit
991:
.endm
# /// openat 直接消费普通字节路径，截断意图由生产准入和 inode 实现决定。
.macro OPEN pathname, flags, number
    li a0, -100
    lla a1, \pathname
    li a2, \flags
    li a3, 0
    SC 56
    OK \number
.endm
# /// lseek SEEK_CUR 观察真实共享位置，不在用户端模拟 cursor。
.macro CURSOR fd, value, number
    mv a0, \fd
    li a1, 0
    li a2, 1
    SC 62
    EXPECT \value, \number
.endm
# /// fstat 的 size 来自生产 inode 字节长度，结构偏移使用 Linux 通用 64 位 ABI。
.macro SIZE fd, value, number
    mv a0, \fd
    lla a1, .Lstat
    SC 80
    EXPECT 0, \number
    lla t0, .Lstat
    ld a0, 48(t0)
    EXPECT \value, \number
.endm
# /// 两目标使用 asm-generic stat；mode 和 nlink 是独立的 32 位字段，不能合并读为 64 位。
.macro NSSTAT fd, number
    mv a0, \fd
    lla a1, .Lstat
    SC 80
    EXPECT 0, \number
.endm


# /// 控制字是普通共享匿名页；忙等期间不发 syscall，保留远核真实 W 翻译。
.macro PHASE value
    li t0, \value
    fence rw, rw
    sw t0, 0(s6)
.endm
# /// 每个阶段只消费协调者发布的单调许可，退出仍由普通 wait/join 核验。
.macro GATE value
991:
    lw t0, 4(s6)
    li t1, \value
    bltu t0, t1, 991b
    fence rw, rw
.endm

# /// 每次 msync 都把完整参数装入真实普通 ABI，地址和 errno 不由控制页回声代替。
.macro MS address, length, flags, result, number
    mv a0, \address
    li a1, \length
    li a2, \flags
    SC 227
    EXPECT \result, \number
.endm
# /// argv 选择确定的普通文件、映射、同步和截断模式，所有返回值均消费真实 syscall。
.global _start
.type _start, @function
_start:
    li s11, 1
    ld t0, 0(sp)
    li t1, 2
    bgeu t0, t1, 991f
    j .Lexit
991:
    ld t0, 16(sp)
    lbu t0, 0(t0)
    li t1, 'w'
    bne t0, t1, 991f
    j .Lwrite
991:
    li t1, 'a'
    bne t0, t1, 991f
    j .Lappend
991:
    li t1, 'e'
    bne t0, t1, 991f
    j .Lexec
991:
    li t1, 'n'
    bne t0, t1, 991f
    j .Lchild_image
991:
    li t1, 'o'
    bne t0, t1, 991f
    j .Lold_released
991:
    li t1, 's'
    bne t0, t1, 991f
    j .Lshared
991:
    li t1, 'r'
    bne t0, t1, 991f
    j .Lremote
991:
    li t1, 'm'
    bne t0, t1, 991f
    j .Lremote
991:
    li t1, 'c'
    bne t0, t1, 991f
    j .Lremote
991:
    li t1, 'f'
    bne t0, t1, 991f
    j .Lfile_futex
991:
    li t1, 'p'
    bne t0, t1, 991f
    j .Lfile_futex
991:
    li t1, 'q'
    bne t0, t1, 991f
    j .Lfile_futex
991:
    li t1, 'u'
    bne t0, t1, 991f
    j .Lfile_futex
991:
    li t1, 'y'
    bne t0, t1, 991f
    j .Lsync_abi
991:
    li t1, 'b'
    bne t0, t1, 991f
    j .Lsync_wait
991:
    li t1, 'd'
    bne t0, t1, 991f
    j .Lsync_write
991:
    li t1, 'z'
    bne t0, t1, 991f
    j .Lsync_write
991:
    li t1, 't'
    bne t0, t1, 991f
    j .Ltruncate
991:
    li t1, 'v'
    bne t0, t1, 991f
    j .Ltruncate_ro
991:
    li t1, 'h'
    bne t0, t1, 991f
    j .Ltruncate_dac
991:
    li t1, 'k'
    bne t0, t1, 991f
    j .Ltruncate_cow
991:
    li t1, 'j'
    bne t0, t1, 991f
    j .Ltruncate_wait
991:
    li t1, 'R'
    bne t0, t1, 991f
    j .Lreclaim
991:
    li t1, 'N'
    bne t0, t1, 991f
    j .Lnamespace_dispatch
991:
    j .Lexit

.Lnamespace_dispatch:
    ld t0, 16(sp)
    lbu t0, 1(t0)
    li t1, '3'
    bne t0, t1, 991f
    j .Lnamespace_create
991:
    li t1, '4'
    bne t0, t1, 991f
    j .Lnamespace_unlink
991:
    li t1, '5'
    bne t0, t1, 991f
    j .Lnamespace_rename
991:
    li t1, '6'
    bne t0, t1, 991f
    j .Lnamespace_directories
991:
    j .Lexit

# /// 两个普通 fork Mm 在不同 VA 保留只读文件叶，私有匿名修改在回收和 refault 后仍可读取。
.Lreclaim:
    li s6, 0x49000000
    li s5, 0x4b000000
    li s7, 0x4c000000
    OPEN .Lshared_path, 0, 200
    mv s0, a0
    mv a0, s5
    li a1, 4096
    li a2, 1
    li a3, 0x11
    mv a4, s0
    li a5, 0
    SC 222
    EXPECT 0x4b000000, 201
    lbu a0, 0(s5)
    EXPECT 0x62, 202
    mv a0, s7
    li a1, 4096
    li a2, 3
    li a3, 0x12
    mv a4, s0
    li a5, 0
    SC 222
    EXPECT 0x4c000000, 203
    li t0, 0xa5
    sb t0, 0(s7)
    li s2, 1
    li a0, 17
    li a1, 0
    li a2, 0
    li a3, 0
    li a4, 0
    SC 220
    OK 204
    beq a0, zero, .Lreclaim_child
    mv s1, a0
    sd a0, 16(s6)
    fence rw, rw
.Lreclaim_wait_child:
    lw t0, 8(s6)
    li t1, 1
    bltu t0, t1, .Lreclaim_wait_child
    PHASE 1
    j .Lreclaim_resume
.Lreclaim_child:
    li s2, 0
    mv a0, s5
    li a1, 4096
    SC 215
    EXPECT 0, 205
    li s5, 0x4d000000
    mv a0, s5
    li a1, 4096
    li a2, 1
    li a3, 0x11
    mv a4, s0
    li a5, 0
    SC 222
    EXPECT 0x4d000000, 206
    lbu a0, 0(s5)
    EXPECT 0x62, 207
    li t0, 1
    fence rw, rw
    sw t0, 8(s6)
.Lreclaim_resume:
    GATE 1
    lbu a0, 0(s5)
    EXPECT 0x62, 208
    lbu a0, 0(s7)
    EXPECT 0xa5, 209
    mv a0, s0
    SC 57
    EXPECT 0, 210
    bne s2, zero, .Lreclaim_parent_done
    li t0, 2
    fence rw, rw
    sw t0, 8(s6)
    GATE 2
    j .Lpass
.Lreclaim_parent_done:
    PHASE 2
    GATE 2
    j .Lwait_child

# /// 源跨坏页只接受 17 字节，零前缀故障、负 pwrite 和定位写均不能改共享位置。
.Lwrite:
    OPEN .Luser_path, 2, 2
    mv s0, a0
    li a0, 0
    li a1, 8192
    li a2, 3
    li a3, 0x22
    li a4, -1
    li a5, 0
    SC 222
    OK 3
    mv s4, a0
    li t0, 4079
    add t0, s4, t0
    li t1, 17
    li t2, 0x72
.Lfill_prefix:
    sb t2, 0(t0)
    addi t0, t0, 1
    addi t1, t1, -1
    bnez t1, .Lfill_prefix
    li t0, 4096
    add a0, s4, t0
    li a1, 4096
    SC 215
    EXPECT 0, 4
    mv a0, s0
    li t0, 4079
    add a1, s4, t0
    li a2, 64
    SC 64
    EXPECT 17, 5
    CURSOR s0, 17, 6
    SIZE s0, 17, 7
    mv a0, s0
    li t0, 4096
    add a1, s4, t0
    li a2, 64
    SC 64
    EXPECT -14, 8
    CURSOR s0, 17, 9
    mv a0, s0
    li t0, 4079
    add a1, s4, t0
    li a2, 64
    li a3, 4093
    SC 68
    EXPECT 17, 10
    CURSOR s0, 17, 11
    SIZE s0, 4110, 12
    mv a0, s0
    lla a1, .Lpayload
    li a2, 1
    li a3, -1
    SC 68
    EXPECT -22, 13
    CURSOR s0, 17, 14
    mv a0, s0
    lla a1, .Lbuffer
    li a2, 4096
    li a3, 0
    SC 67
    EXPECT 4096, 15
    lla a0, .Lbuffer
    li a1, 17
    li a2, 0x72
    li s11, 16
    call .Lconstant
    lla a0, .Lbuffer
    addi a0, a0, 17
    li a1, 4076
    li a2, 0
    li s11, 17
    call .Lconstant
    lla a0, .Lbuffer
    li t0, 4093
    add a0, a0, t0
    li a1, 3
    li a2, 0x72
    li s11, 18
    call .Lconstant
    mv a0, s0
    lla a1, .Lbuffer
    li a2, 17
    li a3, 4093
    SC 67
    EXPECT 17, 19
    lla a0, .Lbuffer
    li a1, 17
    li a2, 0x72
    li s11, 20
    call .Lconstant
    # 冷 private 文件页作为同一 inode 的输入，不提前读取或触页。
    OPEN .Lmap_path, 2, 21
    mv s1, a0
    li a0, 0
    li a1, 8192
    li a2, 1
    li a3, 2
    mv a4, s1
    li a5, 0
    SC 222
    OK 22
    mv s5, a0
    mv a0, s1
    li t0, 4093
    add a1, s5, t0
    li a2, 1027
    li a3, 10240
    SC 68
    EXPECT 1027, 23
    CURSOR s1, 0, 24
    SIZE s1, 11267, 25
    mv a0, s1
    lla a1, .Lbuffer
    li a2, 1027
    li a3, 10240
    SC 67
    EXPECT 1027, 26
    lla a0, .Lbuffer
    li a1, 1027
    li a2, 4093
    li s11, 27
    call .Lpattern
    mv a0, s0
    lla a1, .Lbuffer
    li a2, 1027
    SC 64
    EXPECT 1027, 32
    CURSOR s0, 1044, 33
    SIZE s0, 4110, 34
    mv a0, s0
    lla a1, .Lbuffer
    li a2, 1027
    li a3, 17
    SC 67
    EXPECT 1027, 35
    lla a0, .Lbuffer
    li a1, 1027
    li a2, 4093
    li s11, 36
    call .Lpattern
    # 写打开存活时，loader 应先报 ETXTBSY，不能先解析非 ELF 内容报 ENOEXEC。
    lla a0, .Lmap_path
    lla a1, .Lbad_argv
    li a2, 0
    SC 221
    EXPECT -26, 28
    mv a0, s1
    SC 57
    EXPECT 0, 29
    # 文件 VMA 仍保活原写 OFD，解除映射后才结束它的写排斥责任。
    mv a0, s5
    li a1, 8192
    SC 215
    EXPECT 0, 37
    lla a0, .Lmap_path
    lla a1, .Lbad_argv
    li a2, 0
    SC 221
    EXPECT -8, 30
    mv a0, s0
    SC 57
    EXPECT 0, 31
    j .Lpass

# /// dup 共享顺序位置，pwrite O_APPEND 在 EOF 提交但保原 cursor。
.Lappend:
    OPEN .Lappend_path, 0x402, 40
    mv s0, a0
    OPEN .Lappend_path, 0x402, 41
    mv s1, a0
    mv a0, s0
    lla a1, .Lalpha
    li a2, 5
    SC 64
    EXPECT 5, 42
    mv a0, s0
    SC 23
    OK 43
    mv s2, a0
    mv a0, s2
    lla a1, .Lbeta
    li a2, 4
    li a3, 0
    SC 68
    EXPECT 4, 44
    CURSOR s0, 5, 45
    CURSOR s2, 5, 46
    CURSOR s1, 0, 47
    mv a0, s2
    lla a1, .Lgamma
    li a2, 3
    SC 64
    EXPECT 3, 48
    CURSOR s0, 12, 49
    mv a0, s1
    lla a1, .Ldelta
    li a2, 2
    li a3, 123
    SC 68
    EXPECT 2, 50
    CURSOR s1, 0, 51
    SIZE s0, 14, 52
    mv a0, s0
    lla a1, .Lbuffer
    li a2, 14
    li a3, 0
    SC 67
    EXPECT 14, 53
    lla a0, .Lbuffer
    lla a1, .Lappend_expected
    li a2, 14
    li s11, 54
    call .Lequal
    j .Lpass

# /// 活映像禁止 write-open 和 O_TRUNC；fork 子换映像后仍由父旧 Mm 保 deny。
.Lexec:
    li a0, -100
    lla a1, .Lself
    li a2, 1
    li a3, 0
    SC 56
    EXPECT -26, 60
    li a0, -100
    lla a1, .Lself
    li a2, 0x201
    li a3, 0
    SC 56
    EXPECT -26, 61
    li a0, 17
    li a1, 0
    li a2, 0
    li a3, 0
    li a4, 0
    SC 220
    OK 62
    beqz a0, .Lfork_child
    mv s0, a0
    mv a0, s0
    lla a1, .Lstatus
    li a2, 0
    li a3, 0
    SC 260
    li s11, 63
    beq a0, s0, 991f
    j .Lexit
991:
    lla t0, .Lstatus
    lw a0, 0(t0)
    EXPECT 0, 64
    # 换到不同 inode 的真实 ELF 后，原 Mm 最后使用结束才允许写打开旧映像。
    lla a0, .Lnext
    lla a1, .Largv_old
    li a2, 0
    SC 221
    li s11, 65
    j .Lexit
.Lfork_child:
    lla a0, .Lnext
    lla a1, .Largv_child
    li a2, 0
    SC 221
    li s11, 66
    j .Lexit
.Lchild_image:
    li a0, -100
    lla a1, .Lself
    li a2, 1
    li a3, 0
    SC 56
    EXPECT -26, 67
    j .Lpass
.Lold_released:
    OPEN .Lself, 1, 68
    SC 57
    EXPECT 0, 69
    j .Lpass

# /// 只读 OFD 拒绝共享写；同源尾页 store 不增 EOF，增长后新增 gap 必须重新归零。
.Lshared:
    li s6, 0x49000000
    OPEN .Lshared_path, 0, 75
    mv s0, a0
    ld s7, 24(s6)
    li a0, 0x48000000
    li a1, 8192
    li a2, 3
    li a3, 0x11
    mv a4, s0
    mv a5, s7
    SC 222
    EXPECT -13, 76
    mv a0, s0
    SC 57
    EXPECT 0, 77
    OPEN .Lshared_path, 2, 80
    mv s0, a0
    li a0, 0x48000000
    li a1, 8192
    li a2, 3
    li a3, 0x11
    mv a4, s0
    mv a5, s7
    SC 222
    EXPECT 0x48000000, 81
    mv s5, a0
    li t0, 0x61
    sb t0, 0(s5)
    li t0, 0xde
    sb t0, 128(s5)
    PHASE 1
    GATE 1
    lbu a0, 1(s5)
    EXPECT 0x62, 82
    lbu a0, 128(s5)
    EXPECT 0, 83
    mv a0, s0
    lla a1, .Lstat
    SC 80
    EXPECT 0, 84
    lla t0, .Lstat
    ld a0, 48(t0)
    addi t1, s7, 513
    beq a0, t1, 991f
    j .Lexit
991:
    li t0, 0x74
    sb t0, 128(s5)
    PHASE 2
    GATE 2
    j .Lpass

# /// 首次 store 后仅忙等；同步撤 W 后第二次 store 必须经真实 mkwrite。
.Lremote:
    ld t0, 16(sp)
    lbu s7, 0(t0)
    li s6, 0x49000000
    OPEN .Lmap_path, 2, 85
    mv s0, a0
    li a0, 0x48000000
    li a1, 4096
    li a2, 3
    li a3, 0x11
    mv a4, s0
    li a5, 0
    SC 222
    EXPECT 0x48000000, 86
    mv s5, a0
    li t0, 0x81
    sb t0, 0(s5)
    PHASE 1
    GATE 1
    li t0, 'c'
    beq s7, t0, .Lshared_fork
    li t0, 0x82
    sb t0, 1(s5)
    PHASE 2
    GATE 2
    j .Lpass
.Lshared_fork:
    li a0, 17
    li a1, 0
    li a2, 0
    li a3, 0
    li a4, 0
    SC 220
    OK 87
    beqz a0, .Lshared_child
    mv s1, a0
    sd a0, 16(s6)
    PHASE 2
    GATE 4
    j .Lwait_child
.Lshared_child:
    li t0, 3
    fence rw, rw
    sw t0, 8(s6)
    GATE 3
    li t0, 0x83
    sb t0, 2(s5)
    li a0, 0
    SC 93
    j .Lexit

# /// 真正的 fork 子在不同 VA 上 WAIT；父用普通 WAKE 核共享身份或 PRIVATE 隔离。
.Lfile_futex:
    ld t0, 16(sp)
    lbu s7, 0(t0)
    li s6, 0x49000000
    OPEN .Lmap_path, 2, 90
    mv s0, a0
    li a0, 0x48000000
    li a1, 4096
    li a2, 3
    li a3, 0x11
    mv a4, s0
    li a5, 0
    SC 222
    EXPECT 0x48000000, 91
    mv s5, a0
    li a0, 17
    li a1, 0
    li a2, 0
    li a3, 0
    li a4, 0
    SC 220
    OK 92
    beqz a0, .Lfutex_child
    mv s1, a0
    sd a0, 16(s6)
    GATE 1
    addi a0, s5, 64
    li a1, 1
    li t0, 'p'
    bne s7, t0, .Lfutex_wake
    li a1, 129
.Lfutex_wake:
    li a2, 1
    li a3, 0
    li a4, 0
    li a5, 0
    SC 98
    li t0, 'f'
    bne s7, t0, .Lfutex_isolated
    EXPECT 1, 93
    j .Lwait_child
.Lfutex_isolated:
    EXPECT 0, 94
.Lwait_child:
    mv a0, s1
    lla a1, .Lstatus
    li a2, 0
    li a3, 0
    SC 260
    li s11, 95
    beq a0, s1, 991f
    j .Lexit
991:
    lla t0, .Lstatus
    lw a0, 0(t0)
    EXPECT 0, 96
    j .Lpass
.Lfutex_child:
    mv a0, s5
    li a1, 4096
    SC 215
    EXPECT 0, 97
    li t0, 'q'
    bne s7, t0, .Lfutex_child_map
    OPEN .Lshared_path, 2, 98
    mv s0, a0
.Lfutex_child_map:
    li a0, 0x4a000000
    li a1, 4096
    li a2, 3
    li a3, 0x11
    mv a4, s0
    li a5, 0
    SC 222
    EXPECT 0x4a000000, 99
    mv s5, a0
    PHASE 1
    addi a0, s5, 64
    li a1, 0
    li t0, 'p'
    bne s7, t0, .Lfutex_wait
    li a1, 128
.Lfutex_wait:
    li a2, 0
    lla a3, .Lfutex_timeout
    li a4, 0
    li a5, 0
    SC 98
    li t0, 'f'
    bne s7, t0, .Lfutex_cancelled
    EXPECT 0, 100
    j .Lfutex_child_exit
.Lfutex_cancelled:
    EXPECT -110, 101
.Lfutex_child_exit:
    li a0, 0
    SC 93
    j .Lexit

# /// 独立 open 的普通 fsync 在已有 WB 上等待，控制者核真实 Blocked 后才恢复 used 消费。
.Lsync_wait:
    li s6, 0x49000000
    OPEN .Lshared_path, 2, 110
    mv s0, a0
    PHASE 1
    GATE 1
    mv a0, s0
    SC 82
    EXPECT 0, 111
    PHASE 2
    GATE 2
    mv a0, s0
    SC 83
    EXPECT 0, 112
    j .Lpass

# /// 原始 __O_SYNC 由 open 归一化；F_SETFL 不撤同步位，错误 write 保已接受位置，pwrite 不改位置。
.Lsync_write:
    ld t0, 16(sp)
    lbu s7, 0(t0)
    li s6, 0x49000000
    li t1, 'z'
    beq s7, t1, .Lopen_dsync
    OPEN .Lshared_path, 0x100002, 113
    li s4, 0x101002
    j .Lsync_flags
.Lopen_dsync:
    OPEN .Lshared_path, 0x1002, 114
    li s4, 0x1002
.Lsync_flags:
    mv s0, a0
    sd s0, 16(s6)
    mv a0, s0
    li a1, 4
    li a2, 0
    SC 25
    EXPECT 0, 115
    mv a0, s0
    li a1, 3
    SC 25
    li s11, 116
    beq a0, s4, 991f
    j .Lexit
991:
    PHASE 1
    GATE 1
    mv a0, s0
    lla a1, .Lpayload
    li a2, 1
    SC 64
    EXPECT -5, 117
    CURSOR s0, 1, 118
    PHASE 2
    GATE 2
    mv a0, s0
    lla a1, .Lpayload
    li a2, 1
    li a3, 2
    SC 68
    EXPECT 1, 119
    CURSOR s0, 1, 120
    j .Lpass

# /// shared 非零文件偏移、private COW、孔洞、合法零/异步/INVALIDATE 和冲突位均走普通 sys227。
.Lsync_abi:
    li s6, 0x49000000
    OPEN .Lshared_path, 2, 121
    mv s0, a0
    li a0, 0x48000000
    li a1, 4096
    li a2, 3
    li a3, 0x11
    mv a4, s0
    li a5, 4096
    SC 222
    EXPECT 0x48000000, 122
    mv s5, a0
    li t0, 0x79
    sb t0, 29(s5)
    PHASE 1
    GATE 1
    addi s4, s5, 1
    MS s4, 4096, 4, -22, 123
    MS s5, 4096, 8, -22, 124
    MS s5, 4096, 5, -22, 125
    MS s5, 4096, 0, 0, 126
    MS s5, 4096, 1, 0, 127
    MS s5, 4096, 2, 0, 128
    MS s5, 4096, 3, 0, 129
    MS s5, 0, 4, 0, 130
    MS s5, -1, 0, 0, 131
    PHASE 2
    GATE 2
    MS s5, 4096, 4, 0, 132
    MS s5, 8192, 4, -12, 133
    li s4, 0xfffffffffffff000
    MS s4, 8192, 0, -12, 134
    li s4, 0x48001000
    MS s4, 4096, 0, -12, 135
    li a0, 0x4a000000
    li a1, 4096
    li a2, 3
    li a3, 0x12
    mv a4, s0
    li a5, 0
    SC 222
    EXPECT 0x4a000000, 136
    mv s4, a0
    li t0, 0xaf
    sb t0, 29(s4)
    MS s4, 4096, 4, 0, 137
    mv a0, s0
    lla a1, .Lbuffer
    li a2, 1
    li a3, 29
    SC 67
    EXPECT 1, 138
    lla t0, .Lbuffer
    lbu a0, 0(t0)
    EXPECT 0x66, 139
    mv a0, s0
    lla a1, .Lbuffer
    li a2, 1
    li a3, 4125
    SC 67
    EXPECT 1, 140
    lla t0, .Lbuffer
    lbu a0, 0(t0)
    EXPECT 0x79, 141
    mv a0, s0
    SC 82
    EXPECT 0, 142
    mv a0, s0
    SC 83
    EXPECT 0, 143
    li a0, -1
    SC 82
    EXPECT -9, 144
    li a0, -1
    SC 83
    EXPECT -9, 145
    OPEN .Ldirectory, 0, 146
    mv s1, a0
    SC 82
    EXPECT 0, 147
    mv a0, s1
    SC 83
    EXPECT 0, 148
    j .Lpass

# /// 真实 45/46 和 O_TRUNC 共享大小，dup 的位置不变；只读截断打开仍是只读 OFD。
.Ltruncate:
    OPEN .Lshared_path, 2, 150
    mv s0, a0
    li a1, 0
    SC 46
    EXPECT 0, 151
    mv a0, s0
    lla a1, .Ltruncate_input
    li a2, 9
    SC 64
    EXPECT 9, 152
    mv a0, s0
    li a1, 123
    li a2, 0
    SC 62
    EXPECT 123, 153
    mv a0, s0
    SC 23
    OK 154
    mv s2, a0
    mv a0, s0
    li a1, 3
    SC 46
    EXPECT 0, 155
    SIZE s0, 3, 156
    CURSOR s0, 123, 157
    CURSOR s2, 123, 158
    mv a0, s0
    li a1, 9
    SC 46
    EXPECT 0, 159
    SIZE s0, 9, 160
    mv a0, s0
    lla a1, .Lbuffer
    li a2, 9
    li a3, 0
    SC 67
    EXPECT 9, 161
    lla a0, .Lbuffer
    lla a1, .Ltruncate_expected
    li a2, 9
    li s11, 162
    call .Lequal
    lla a0, .Lshared_path
    li a1, 0
    SC 45
    EXPECT 0, 163
    SIZE s0, 0, 164
    CURSOR s2, 123, 165
    lla a0, .Lshared_path
    li a1, 9
    SC 45
    EXPECT 0, 166
    SIZE s0, 9, 167
    mv a0, s0
    lla a1, .Lbuffer
    li a2, 9
    li a3, 0
    SC 67
    EXPECT 9, 168
    lla a0, .Lbuffer
    li a1, 9
    li a2, 0
    li s11, 169
    call .Lconstant
    li a0, -1
    li a1, -1
    SC 46
    EXPECT -22, 170
    li a0, -1
    li a1, 0
    SC 46
    EXPECT -9, 171
    li a0, 0
    li a1, -1
    SC 45
    EXPECT -22, 172
    li a0, 0
    li a1, 0
    SC 45
    EXPECT -14, 173
    mv a0, s0
    li a1, -1
    SC 46
    EXPECT -22, 174
    mv a0, s0
    li a1, 0x7fffffffffffffff
    SC 46
    EXPECT -27, 175
    lla a0, .Lshared_path
    li a1, 0x7fffffffffffffff
    SC 45
    EXPECT -27, 176
    SIZE s0, 9, 177
    CURSOR s0, 123, 178
    OPEN .Lshared_path, 0, 179
    mv s1, a0
    li a1, 0
    SC 46
    EXPECT -22, 180
    OPEN .Ldirectory, 0, 181
    mv s3, a0
    li a1, 0
    SC 46
    EXPECT -22, 182
    lla a0, .Ldirectory
    li a1, 0
    SC 45
    EXPECT -21, 183
    lla a0, .Lself
    li a1, 0
    SC 45
    EXPECT -26, 184
    li a0, -100
    lla a1, .Lself
    li a2, 0x200
    li a3, 0
    SC 56
    EXPECT -26, 185
    OPEN .Lshared_path, 0x200, 186
    mv s4, a0
    li a1, 3
    li a2, 0
    SC 25
    EXPECT 0, 187
    SIZE s4, 0, 188
    SIZE s0, 0, 189
    CURSOR s2, 123, 190
    mv a0, s4
    lla a1, .Lpayload
    li a2, 1
    SC 64
    EXPECT -9, 191
    mv a0, s4
    li a1, 0
    SC 46
    EXPECT -22, 192
    mv a0, s0
    lla a1, .Lpayload
    li a2, 1
    li a3, 0
    SC 68
    EXPECT 1, 193
    OPEN .Lshared_path, 0x202, 194
    mv s5, a0
    SIZE s5, 0, 195
    CURSOR s0, 123, 196
    j .Lpass

# /// 只读挂载保 EROFS，负长度先于 fd 查找；只读 OFD 的 ftruncate 仍按类型返回 EINVAL。
.Ltruncate_ro:
    OPEN .Lmap_path, 0, 197
    mv s0, a0
    CURSOR s0, 0, 222
    mv a0, s0
    li a1, 0
    SC 46
    EXPECT -22, 198
    lla a0, .Lmap_path
    li a1, 0
    SC 45
    EXPECT -30, 199
    li a0, -100
    lla a1, .Lmap_path
    li a2, 0x200
    li a3, 0
    SC 56
    EXPECT -30, 200
    li a0, -100
    lla a1, .Lmap_path
    li a2, 0x202
    li a3, 0
    SC 56
    EXPECT -30, 201
    CURSOR s0, 0, 223
    j .Lpass

# /// 当前非 root 凭据须有写权限才能按路径截断，失败不能改变真实大小或文件字节。
.Ltruncate_dac:
    OPEN .Lshared_path, 0, 202
    mv s0, a0
    SIZE s0, 9, 203
    lla a0, .Lshared_path
    li a1, 0
    SC 45
    EXPECT -13, 204
    li a0, -100
    lla a1, .Lshared_path
    li a2, 0x200
    li a3, 0
    SC 56
    EXPECT -13, 205
    li a0, -100
    lla a1, .Lshared_path
    li a2, 0x202
    li a3, 0
    SC 56
    EXPECT -13, 206
    SIZE s0, 9, 207
    mv a0, s0
    lla a1, .Lbuffer
    li a2, 9
    li a3, 0
    SC 67
    EXPECT 9, 208
    lla a0, .Lbuffer
    lla a1, .Ltruncate_input
    li a2, 9
    li s11, 209
    call .Lequal
    j .Lpass

# /// 新 private 写 fault 先准备真实匿名候选，首失效后完成 store 和普通 fork，子与父等最终失效后才退出。
.Ltruncate_cow:
    li s6, 0x49000000
    OPEN .Lshared_path, 0, 210
    mv s0, a0
    li a0, 0x48000000
    li a1, 4096
    li a2, 3
    li a3, 0x12
    mv a4, s0
    ld a5, 24(s6)
    SC 222
    EXPECT 0x48000000, 211
    mv s5, a0
    PHASE 1
    GATE 1
    li t0, 0xb6
    sb t0, 0(s5)
    lbu a0, 0(s5)
    EXPECT 0xb6, 212
    PHASE 2
    GATE 2
    li a0, 17
    li a1, 0
    li a2, 0
    li a3, 0
    li a4, 0
    SC 220
    OK 213
    beqz a0, .Ltruncate_cow_child
    mv s1, a0
    sd s1, 16(s6)
    PHASE 3
    GATE 3
    SIZE s0, 4096, 214
    mv a0, s1
    lla a1, .Lstatus
    li a2, 0
    li a3, 0
    SC 260
    li s11, 215
    bne a0, s1, .Lexit
    lla t0, .Lstatus
    lw a0, 0(t0)
    EXPECT 0, 216
    j .Lpass
.Ltruncate_cow_child:
    li t0, 3
    fence rw, rw
    sw t0, 8(s6)
    GATE 3
    j .Lpass

# /// 普通 ftruncate 在原数据或元数据完成暂停时真实等待，返回后大小为零且 OFD 位置保持。
.Ltruncate_wait:
    li s6, 0x49000000
    OPEN .Lshared_path, 2, 217
    mv s0, a0
    li a1, 7
    li a2, 0
    SC 62
    EXPECT 7, 218
    PHASE 1
    GATE 1
    mv a0, s0
    li a1, 0
    SC 46
    EXPECT 0, 219
    SIZE s0, 0, 220
    CURSOR s0, 7, 221
    PHASE 2
    GATE 2
    j .Lpass

# /// 真实用户创建对象后停在匿名控制页，控制者只接收inode并同步盘面。
.Lnamespace_create:
    li s6, 0x49000000
    li a0, 0x1017
    SC 166
    EXPECT 0x12, 300
    mv s2, a0

    li a0, -100
    lla a1, .Lns_mode
    li a2, 0xc2
    li a3, 0xfff
    SC 56
    OK 301
    mv s0, a0
    mv a0, s0
    SC 82
    EXPECT 0, 303
    mv a0, s0
    SC 57
    EXPECT 0, 304
    li a0, -100
    lla a1, .Lns_data
    li a2, 0xc2
    li a3, 0x1b6
    SC 56
    OK 645
    mv s0, a0
    mv a0, s0
    lla a1, .Lns_payload_mode
    li a2, 4
    SC 64
    EXPECT 4, 646
    mv a0, s0
    SC 82
    EXPECT 0, 647
    mv a0, s0
    SC 57
    EXPECT 0, 648

    li s3, 12
    li a0, 0x311
    lla a1, .Lns_stack_a
    li t0, 16384
    add a1, a1, t0
    li a2, 0
    li a3, 0
    li a4, 0
    SC 220
    OK 305
    beqz a0, .Lns_race_child
    mv s4, a0
    li s3, 16
    li a0, 0x311
    lla a1, .Lns_stack_b
    li t0, 16384
    add a1, a1, t0
    li a2, 0
    li a3, 0
    li a4, 0
    SC 220
    OK 306
    beqz a0, .Lns_race_child
    mv s5, a0
    li t0, 1
    fence rw, w
    sw t0, 8(s6)
    mv a0, s4
    lla a1, .Lstatus
    li a2, 0
    li a3, 0
    SC 260
    li s11, 307
    beq a0, s4, 991f
    j .Lexit
991:
    lla t0, .Lstatus
    lw a0, 0(t0)
    EXPECT 0, 308
    mv a0, s5
    lla a1, .Lstatus
    li a2, 0
    li a3, 0
    SC 260
    li s11, 309
    beq a0, s5, 991f
    j .Lexit
991:
    lla t0, .Lstatus
    lw a0, 0(t0)
    EXPECT 0, 310
    lw t0, 12(s6)
    lw t1, 16(s6)
    add a0, t0, t1
    EXPECT 1, 311
    OPEN .Lns_race, 0, 638
    mv s0, a0
    NSSTAT s0, 639
    lla t0, .Lstat
    ld t1, 8(t0)
    mv s4, t1
    OPEN .Lns_race, 0, 640
    mv s1, a0
    NSSTAT s1, 641
    lla t0, .Lstat
    ld t0, 8(t0)
    li s11, 642
    beq t0, s4, 991f
    j .Lexit
991:
    mv a0, s1
    SC 57
    EXPECT 0, 643
    mv a0, s0
    SC 57
    EXPECT 0, 644
    OPEN .Lns_race, 2, 312
    mv s0, a0
    mv a0, s0
    lla a1, .Lns_payload_race
    li a2, 4
    SC 64
    EXPECT 4, 313
    mv a0, s0
    SC 82
    EXPECT 0, 314
    mv a0, s0
    SC 57
    EXPECT 0, 315

    li a0, 0x311
    lla a1, .Lns_stack_a
    li t0, 16384
    add a1, a1, t0
    li a2, 0
    li a3, 0
    li a4, 0
    SC 220
    OK 316
    beqz a0, .Lns_shared_umask_child
    mv s4, a0
    mv a0, s4
    lla a1, .Lstatus
    li a2, 0
    li a3, 0
    SC 260
    li s11, 317
    beq a0, s4, 991f
    j .Lexit
991:
    lla t0, .Lstatus
    lw a0, 0(t0)
    EXPECT 0, 318
    li a0, 0
    SC 166
    EXPECT 7, 319
    li a0, 0x17
    SC 166
    EXPECT 0, 320

    li a0, 0x111
    lla a1, .Lns_stack_a
    li t0, 16384
    add a1, a1, t0
    li a2, 0
    li a3, 0
    li a4, 0
    SC 220
    OK 321
    beqz a0, .Lns_copy_umask_child
    mv s4, a0
    mv a0, s4
    lla a1, .Lstatus
    li a2, 0
    li a3, 0
    SC 260
    li s11, 322
    beq a0, s4, 991f
    j .Lexit
991:
    lla t0, .Lstatus
    lw a0, 0(t0)
    EXPECT 0, 323
    li a0, 0
    SC 166
    EXPECT 0x17, 324
    li a0, 0x17
    SC 166
    EXPECT 0, 325

    li a0, -100
    lla a1, .Lns_ro
    li a2, 0x40
    li a3, 0x1b6
    SC 56
    OK 326
    mv s0, a0
    NSSTAT s0, 327
    lla t0, .Lstat
    lwu a0, 16(t0)
    slli a0, a0, 52
    srli a0, a0, 52
    EXPECT 0x1a0, 328
    mv a0, s0
    lla a1, .Lns_payload_mode
    li a2, 1
    SC 64
    EXPECT -9, 329
    mv a0, s0
    SC 82
    EXPECT 0, 330
    mv a0, s0
    SC 57
    EXPECT 0, 331

    li a0, -100
    lla a1, .Lns_mode
    li a2, 0x40
    li a3, 0x180
    SC 56
    OK 332
    mv s0, a0
    NSSTAT s0, 333
    lla t0, .Lstat
    lwu a0, 16(t0)
    slli a0, a0, 52
    srli a0, a0, 52
    EXPECT 0xfe8, 334
    mv a0, s0
    lla a1, .Lns_payload_race
    li a2, 1
    SC 64
    EXPECT -9, 335
    mv a0, s0
    SC 57
    EXPECT 0, 336
    li a0, -100
    lla a1, .Lns_mode
    li a2, 0xc2
    li a3, 0x1b6
    SC 56
    EXPECT -17, 337

    li a0, -100
    lla a1, .Lns_dir
    li a2, 0x3ff
    SC 34
    EXPECT 0, 338
    OPEN .Lns_dir, 0x10000, 339
    mv s0, a0
    NSSTAT s0, 340
    lla t0, .Lstat
    lwu a0, 20(t0)
    EXPECT 2, 341
    lla t0, .Lstat
    lwu a0, 16(t0)
    slli a0, a0, 52
    srli a0, a0, 52
    EXPECT 0x7e8, 342
    mv a0, s0
    lla a1, .Lbuffer
    li a2, 128
    SC 61
    li t0, 48
    bgeu a0, t0, 991f
    j .Lexit
991:
    # s9 在这段 getdents 核验中保持当前记录地址；EXPECT 会覆盖 t0。
    lla s9, .Lbuffer
    lbu a0, 19(s9)
    EXPECT '.', 645
    lbu a0, 20(s9)
    EXPECT 0, 646
    lhu a0, 16(s9)
    EXPECT 24, 647
    lbu a0, 18(s9)
    EXPECT 4, 648
    lhu t1, 16(s9)
    add s9, s9, t1
    lbu a0, 19(s9)
    EXPECT '.', 649
    lbu a0, 20(s9)
    EXPECT '.', 650
    lbu a0, 21(s9)
    EXPECT 0, 651

    mv a0, s0
    SC 82
    EXPECT 0, 343
    mv a0, s0
    SC 57
    EXPECT 0, 344

    lla a0, .Lns_data
    li a1, -100
    lla a2, .Lns_short
    SC 36
    EXPECT 0, 345
    lla a0, .Lns_long_target
    li a1, -100
    lla a2, .Lns_long
    SC 36
    EXPECT 0, 346
    lla a0, .Lns_missing
    li a1, -100
    lla a2, .Lns_dangling
    SC 36
    EXPECT 0, 347

    li a0, -100
    lla a1, .Lns_raw
    li a2, 0xc2
    li a3, 0x180
    SC 56
    OK 348
    mv s0, a0
    mv a0, s0
    lla a1, .Lns_payload_raw
    li a2, 4
    SC 64
    EXPECT 4, 349
    mv a0, s0
    SC 82
    EXPECT 0, 350
    mv a0, s0
    SC 57
    EXPECT 0, 351

    li a0, -100
    lla a1, .Lns_missing_parent
    li a2, 0x42
    li a3, 0x180
    SC 56
    EXPECT -2, 352
    li a0, -100
    lla a1, .Lns_trailing
    li a2, 0x20000
    li a3, 0
    SC 56
    EXPECT -20, 353
    li a0, -100
    lla a1, .Lns_rooted
    li a2, 0
    li a3, 0
    SC 56
    OK 354
    mv s0, a0
    mv a0, s0
    lla a1, .Lbuffer
    li a2, 4
    SC 63
    EXPECT 4, 355
    lla a0, .Lbuffer
    lla a1, .Lns_payload_mode
    li a2, 4
    li s11, 356
    call .Lequal
    mv a0, s0
    SC 57
    EXPECT 0, 357

    li a0, -100
    lla a1, .Lns_permission_denied
    li a2, 0xc2
    li a3, 0x180
    SC 56
    EXPECT -13, 358

    li s0, 0
.Lns_fill_fds:
    li a0, -100
    lla a1, .Lns_mode
    li a2, 0
    li a3, 0
    SC 56
    li t0, -24
    beq a0, t0, .Lns_fds_full
    bgez a0, 991f
    j .Lexit
991:
    li t1, 512
    bltu s0, t1, 991f
    j .Lexit
991:
    slli t1, s0, 3
    lla t2, .Lns_fd_table
    add t1, t1, t2
    sd a0, 0(t1)
    addi s0, s0, 1
    j .Lns_fill_fds
.Lns_fds_full:
    beqz s0, .Lexit
    li a0, -100
    lla a1, .Lns_missing
    li a2, 0xc2
    li a3, 0x180
    SC 56
    EXPECT -24, 359
    li s1, 0
.Lns_close_fds:
    bgeu s1, s0, .Lns_fds_closed
    slli t1, s1, 3
    lla t2, .Lns_fd_table
    add t1, t1, t2
    ld a0, 0(t1)
    SC 57
    EXPECT 0, 360
    addi s1, s1, 1
    j .Lns_close_fds
.Lns_fds_closed:
    li a0, -100
    lla a1, .Lns_missing
    li a2, 0
    li a3, 0
    SC 56
    EXPECT -2, 361

    OPEN .Lns_parent, 0x10000, 362
    mv s0, a0
    mv a0, s0
    SC 82
    EXPECT 0, 363
    mv a0, s0
    SC 57
    EXPECT 0, 364
    PHASE 1
    GATE 1

    li a0, -100
    lla a1, .Lns_dangling
    li a2, 0xc2
    li a3, 0x180
    SC 56
    EXPECT -17, 365
    li a0, -100
    lla a1, .Lns_dangling
    li a2, 0x20000
    li a3, 0
    SC 56
    EXPECT -40, 366
    li a0, -100
    lla a1, .Lns_dangling
    li a2, 0x42
    li a3, 0x180
    SC 56
    OK 367
    mv s0, a0
    mv a0, s0
    lla a1, .Lns_payload_dangling
    li a2, 4
    SC 64
    EXPECT 4, 368
    mv a0, s0
    SC 82
    EXPECT 0, 369
    mv a0, s0
    SC 57
    EXPECT 0, 370

    OPEN .Lns_short, 0, 371
    mv s0, a0
    mv a0, s0
    lla a1, .Lbuffer
    li a2, 4
    SC 63
    EXPECT 4, 372
    lla a0, .Lbuffer
    lla a1, .Lns_payload_mode
    li a2, 4
    li s11, 373
    call .Lequal
    mv a0, s0
    SC 57
    EXPECT 0, 374
    OPEN .Lns_long, 0, 375
    mv s0, a0
    mv a0, s0
    lla a1, .Lbuffer
    li a2, 4
    SC 63
    EXPECT 4, 376
    lla a0, .Lbuffer
    lla a1, .Lns_payload_mode
    li a2, 4
    li s11, 377
    call .Lequal
    mv a0, s0
    SC 57
    EXPECT 0, 378
    OPEN .Lns_dangling, 0, 379
    mv s0, a0
    mv a0, s0
    lla a1, .Lbuffer
    li a2, 4
    SC 63
    EXPECT 4, 380
    lla a0, .Lbuffer
    lla a1, .Lns_payload_dangling
    li a2, 4
    li s11, 381
    call .Lequal
    mv a0, s0
    SC 57
    EXPECT 0, 382

    li a0, -100
    lla a1, .Lns_sgid_child
    li a2, 0xc2
    li a3, 0xfff
    SC 56
    OK 383
    mv s0, a0
    NSSTAT s0, 384
    lla t0, .Lstat
    lwu a0, 28(t0)
    EXPECT 1001, 385
    lla t0, .Lstat
    lwu a0, 16(t0)
    slli a0, a0, 52
    srli a0, a0, 52
    EXPECT 0xfe8, 386
    mv a0, s0
    lla a1, .Lns_payload_alias
    li a2, 4
    SC 64
    EXPECT 4, 387
    mv a0, s0
    SC 82
    EXPECT 0, 388
    mv a0, s0
    SC 57
    EXPECT 0, 389

    OPEN .Lns_parent, 0x10000, 390
    mv s0, a0
    mv a0, s0
    SC 82
    EXPECT 0, 391
    mv a0, s0
    SC 57
    EXPECT 0, 392
    PHASE 2
    GATE 2
    li a0, 0x12
    SC 166
    EXPECT 0x17, 393

    li a0, -100
    lla a1, .Lns_race
    li a2, 0
    SC 35
    EXPECT 0, 394
    li a0, -100
    lla a1, .Lns_mode
    li a2, 0
    SC 35
    EXPECT 0, 395
    li a0, -100
    lla a1, .Lns_data
    li a2, 0
    SC 35
    EXPECT 0, 649
    li a0, -100
    lla a1, .Lns_ro
    li a2, 0
    SC 35
    EXPECT 0, 396
    li a0, -100
    lla a1, .Lns_shared
    li a2, 0
    SC 35
    EXPECT 0, 397
    li a0, -100
    lla a1, .Lns_copy
    li a2, 0
    SC 35
    EXPECT 0, 398
    li a0, -100
    lla a1, .Lns_raw
    li a2, 0
    SC 35
    EXPECT 0, 399
    li a0, -100
    lla a1, .Lns_missing
    li a2, 0
    SC 35
    EXPECT 0, 400
    li a0, -100
    lla a1, .Lns_sgid_child
    li a2, 0
    SC 35
    EXPECT 0, 401
    li a0, -100
    lla a1, .Lns_short
    li a2, 0
    SC 35
    EXPECT 0, 402
    li a0, -100
    lla a1, .Lns_long
    li a2, 0
    SC 35
    EXPECT 0, 403
    li a0, -100
    lla a1, .Lns_dangling
    li a2, 0
    SC 35
    EXPECT 0, 404
    li a0, -100
    lla a1, .Lns_dir
    li a2, 0x200
    SC 35
    EXPECT 0, 405
    OPEN .Lns_parent, 0x10000, 406
    mv s0, a0
    mv a0, s0
    SC 82
    EXPECT 0, 407
    mv a0, s0
    SC 57
    EXPECT 0, 408
    j .Lpass
.Lns_race_child:
    li t0, 0
.Lns_race_gate:
    lw t0, 8(s6)
    beqz t0, .Lns_race_gate
    fence rw, rw
    li a0, -100
    lla a1, .Lns_race
    li a2, 0xc2
    li a3, 0x180
    SC 56
    bgez a0, .Lns_race_won
    li t1, -17
    beq a0, t1, 991f
    j .Lexit
991:
    li t0, 0
    j .Lns_race_record
.Lns_race_won:
    mv s0, a0
    li t0, 1
.Lns_race_record:
    add t1, s6, s3
    sw t0, 0(t1)
    beqz t0, .Lns_race_exit
    mv a0, s0
    SC 57
    EXPECT 0, 409
.Lns_race_exit:
    li a0, 0
    SC 93
    j .Lexit
.Lns_shared_umask_child:
    li a0, 7
    SC 166
    EXPECT 0x17, 410
    li a0, -100
    lla a1, .Lns_shared
    li a2, 0xc2
    li a3, 0x1b6
    SC 56
    OK 411
    mv s0, a0
    NSSTAT s0, 412
    lla t0, .Lstat
    lwu a0, 16(t0)
    slli a0, a0, 52
    srli a0, a0, 52
    EXPECT 0x1b0, 413
    mv a0, s0
    lla a1, .Lns_payload_shared
    li a2, 4
    SC 64
    EXPECT 4, 414
    mv a0, s0
    SC 82
    EXPECT 0, 415
    mv a0, s0
    SC 57
    EXPECT 0, 416
    li a0, 0
    SC 93
    j .Lexit
.Lns_copy_umask_child:
    li a0, 0x3f
    SC 166
    EXPECT 0x17, 417
    li a0, -100
    lla a1, .Lns_copy
    li a2, 0xc2
    li a3, 0x1b6
    SC 56
    OK 418
    mv s0, a0
    NSSTAT s0, 419
    lla t0, .Lstat
    lwu a0, 16(t0)
    slli a0, a0, 52
    srli a0, a0, 52
    EXPECT 0x180, 420
    mv a0, s0
    lla a1, .Lns_payload_copy
    li a2, 4
    SC 64
    EXPECT 4, 421
    mv a0, s0
    SC 82
    EXPECT 0, 422
    mv a0, s0
    SC 57
    EXPECT 0, 423
    li a0, 0
    SC 93
    j .Lexit

# /// 同一 inode 的旧 fd、共享页与私有页跨 unlink 和同名新建仍保持原内容。
.Lnamespace_unlink:
    li s6, 0x49000000
    li a0, -100
    lla a1, .Lns_old
    li a2, 0xc2
    li a3, 0x180
    SC 56
    OK 430
    mv s0, a0
    mv a0, s0
    lla a1, .Lns_payload_old
    li a2, 4
    SC 64
    EXPECT 4, 431
    mv a0, s0
    SC 82
    EXPECT 0, 432
    li a0, -100
    lla a1, .Lns_old
    li a2, -100
    lla a3, .Lns_alias
    li a4, 0
    SC 37
    EXPECT 0, 433
    NSSTAT s0, 434
    lla t0, .Lstat
    ld t1, 8(t0)
    mv s4, t1
    lwu a0, 20(t0)
    EXPECT 2, 435
    OPEN .Lns_alias, 0, 436
    mv s1, a0
    NSSTAT s1, 437
    lla t0, .Lstat
    ld t1, 8(t0)
    li s11, 438
    beq t1, s4, 991f
    j .Lexit
991:
    mv a0, s1
    SC 57
    EXPECT 0, 439

    li a0, 0x4b000000
    li a1, 4096
    li a2, 1
    li a3, 1
    mv a4, s0
    li a5, 0
    SC 222
    EXPECT 0x4b000000, 440
    mv s2, a0
    li a0, 0x4c000000
    li a1, 4096
    li a2, 1
    li a3, 2
    mv a4, s0
    li a5, 0
    SC 222
    EXPECT 0x4c000000, 441
    mv s3, a0
    mv a0, s2
    lla a1, .Lns_payload_old
    li a2, 4
    li s11, 442
    call .Lequal
    mv a0, s3
    lla a1, .Lns_payload_old
    li a2, 4
    li s11, 443
    call .Lequal
    OPEN .Lns_parent, 0x10000, 645
    mv s1, a0
    mv a0, s1
    SC 82
    EXPECT 0, 646
    mv a0, s1
    SC 57
    EXPECT 0, 647
    PHASE 1
    GATE 1

    li a0, -100
    lla a1, .Lns_old
    li a2, 0
    SC 35
    EXPECT 0, 444
    li a0, -100
    lla a1, .Lns_alias
    li a2, 0
    SC 35
    EXPECT 0, 445
    NSSTAT s0, 446
    lla t0, .Lstat
    lwu a0, 20(t0)
    EXPECT 0, 447
    lla t0, .Lstat
    ld a0, 48(t0)
    EXPECT 4, 448
    li a0, -100
    lla a1, .Lns_old
    li a2, 0xc2
    li a3, 0x180
    SC 56
    OK 449
    mv s5, a0
    mv a0, s5
    lla a1, .Lns_payload_new
    li a2, 4
    SC 64
    EXPECT 4, 450
    mv a0, s5
    SC 82
    EXPECT 0, 451
    NSSTAT s5, 452
    lla t0, .Lstat
    ld t1, 8(t0)
    li s11, 453
    bne t1, s4, 991f
    j .Lexit
991:
    lwu a0, 20(t0)
    EXPECT 1, 454
    PHASE 2
    GATE 2

    mv a0, s0
    li a1, 0
    li a2, 0
    SC 62
    EXPECT 0, 455
    mv a0, s0
    lla a1, .Lbuffer
    li a2, 4
    SC 63
    EXPECT 4, 456
    lla a0, .Lbuffer
    lla a1, .Lns_payload_old
    li a2, 4
    li s11, 457
    call .Lequal
    mv a0, s2
    lla a1, .Lns_payload_old
    li a2, 4
    li s11, 458
    call .Lequal
    mv a0, s3
    lla a1, .Lns_payload_old
    li a2, 4
    li s11, 459
    call .Lequal
    NSSTAT s0, 460
    lla t0, .Lstat
    lwu a0, 20(t0)
    EXPECT 0, 461
    mv a0, s0
    SC 57
    EXPECT 0, 462
    mv a0, s2
    li a1, 4096
    SC 215
    EXPECT 0, 463
    mv a0, s3
    li a1, 4096
    SC 215
    EXPECT 0, 464
    PHASE 3
    GATE 3

    mv a0, s5
    SC 82
    EXPECT 0, 465
    li a0, -100
    lla a1, .Lns_old
    li a2, 0
    SC 35
    EXPECT 0, 466
    OPEN .Lns_parent, 0x10000, 467
    mv s0, a0
    mv a0, s0
    SC 82
    EXPECT 0, 468
    mv a0, s0
    SC 57
    EXPECT 0, 469
    mv a0, s5
    SC 57
    EXPECT 0, 470
    j .Lpass

# /// renameat2 覆盖真实目标项后，旧 descriptor 与映射仍指向原 inode。
.Lnamespace_rename:
    li s6, 0x49000000
    li a0, -100
    lla a1, .Lns_source
    li a2, 0xc2
    li a3, 0x180
    SC 56
    OK 480
    mv s1, a0
    mv a0, s1
    lla a1, .Lns_payload_source
    li a2, 4
    SC 64
    EXPECT 4, 481
    mv a0, s1
    SC 82
    EXPECT 0, 482
    mv a0, s1
    SC 57
    EXPECT 0, 483
    li a0, -100
    lla a1, .Lns_target
    li a2, 0xc2
    li a3, 0x180
    SC 56
    OK 484
    mv s0, a0
    mv a0, s0
    lla a1, .Lns_payload_target
    li a2, 4
    SC 64
    EXPECT 4, 485
    mv a0, s0
    SC 82
    EXPECT 0, 486
    NSSTAT s0, 487
    lla t0, .Lstat
    lwu a0, 20(t0)
    EXPECT 1, 488

    li a0, -100
    lla a1, .Lns_plain_source
    li a2, 0xc2
    li a3, 0x180
    SC 56
    OK 489
    mv s1, a0
    mv a0, s1
    lla a1, .Lns_payload_plain
    li a2, 5
    SC 64
    EXPECT 5, 490
    mv a0, s1
    SC 82
    EXPECT 0, 491
    mv a0, s1
    SC 57
    EXPECT 0, 492
    li a0, -100
    lla a1, .Lns_noreplace_source
    li a2, 0xc2
    li a3, 0x180
    SC 56
    OK 493
    mv s1, a0
    mv a0, s1
    lla a1, .Lns_payload_copy
    li a2, 4
    SC 64
    EXPECT 4, 494
    mv a0, s1
    SC 82
    EXPECT 0, 495
    mv a0, s1
    SC 57
    EXPECT 0, 496

    li a0, 0x4b000000
    li a1, 4096
    li a2, 1
    li a3, 1
    mv a4, s0
    li a5, 0
    SC 222
    EXPECT 0x4b000000, 497
    mv s2, a0
    li a0, 0x4c000000
    li a1, 4096
    li a2, 1
    li a3, 2
    mv a4, s0
    li a5, 0
    SC 222
    EXPECT 0x4c000000, 498
    mv s3, a0
    OPEN .Lns_parent, 0x10000, 545
    mv s1, a0
    mv a0, s1
    SC 82
    EXPECT 0, 546
    mv a0, s1
    SC 57
    EXPECT 0, 547
    PHASE 1
    GATE 1

    li a0, -100
    lla a1, .Lns_source
    li a2, -100
    lla a3, .Lns_target
    li a4, 0
    SC 276
    EXPECT 0, 499
    OPEN .Lns_target, 0, 500
    mv s4, a0
    mv a0, s4
    lla a1, .Lbuffer
    li a2, 4
    SC 63
    EXPECT 4, 501
    lla a0, .Lbuffer
    lla a1, .Lns_payload_source
    li a2, 4
    li s11, 502
    call .Lequal
    NSSTAT s0, 503
    lla t0, .Lstat
    lwu a0, 20(t0)
    EXPECT 0, 504
    NSSTAT s4, 505
    lla t0, .Lstat
    lwu a0, 20(t0)
    EXPECT 1, 506
    OPEN .Lns_plain_source, 0, 507
    mv s5, a0
    li a0, -100
    lla a1, .Lns_plain_source
    li a2, -100
    lla a3, .Lns_plain_target
    li a4, 0
    SC 276
    EXPECT 0, 508
    li a0, -100
    lla a1, .Lns_noreplace_source
    li a2, -100
    lla a3, .Lns_plain_target
    li a4, 1
    SC 276
    EXPECT -17, 509
    OPEN .Lns_plain_target, 0, 510
    mv s1, a0
    mv a0, s1
    lla a1, .Lbuffer
    li a2, 5
    SC 63
    EXPECT 5, 511
    lla a0, .Lbuffer
    lla a1, .Lns_payload_plain
    li a2, 5
    li s11, 512
    call .Lequal
    mv a0, s1
    SC 57
    EXPECT 0, 542
    OPEN .Lns_noreplace_source, 0, 513
    mv s1, a0
    mv a0, s1
    lla a1, .Lbuffer
    li a2, 4
    SC 63
    EXPECT 4, 514
    lla a0, .Lbuffer
    lla a1, .Lns_payload_copy
    li a2, 4
    li s11, 515
    call .Lequal
    mv a0, s1
    SC 57
    EXPECT 0, 516
    li a0, -100
    lla a1, .Lns_noreplace_source
    li a2, -100
    lla a3, .Lns_noreplace_target
    li a4, 1
    SC 276
    EXPECT 0, 517
    OPEN .Lns_noreplace_target, 0, 518
    mv s1, a0
    mv a0, s1
    lla a1, .Lbuffer
    li a2, 4
    SC 63
    EXPECT 4, 519
    lla a0, .Lbuffer
    lla a1, .Lns_payload_copy
    li a2, 4
    li s11, 520
    call .Lequal
    mv a0, s1
    SC 57
    EXPECT 0, 521
    OPEN .Lns_parent, 0x10000, 522
    mv s1, a0
    mv a0, s1
    SC 82
    EXPECT 0, 523
    mv a0, s1
    SC 57
    EXPECT 0, 524
    PHASE 2
    GATE 2

    mv a0, s0
    li a1, 0
    li a2, 0
    SC 62
    EXPECT 0, 525
    mv a0, s0
    lla a1, .Lbuffer
    li a2, 4
    SC 63
    EXPECT 4, 526
    lla a0, .Lbuffer
    lla a1, .Lns_payload_target
    li a2, 4
    li s11, 527
    call .Lequal
    mv a0, s2
    lla a1, .Lns_payload_target
    li a2, 4
    li s11, 528
    call .Lequal
    mv a0, s3
    lla a1, .Lns_payload_target
    li a2, 4
    li s11, 529
    call .Lequal
    mv a0, s0
    SC 57
    EXPECT 0, 530
    mv a0, s2
    li a1, 4096
    SC 215
    EXPECT 0, 531
    mv a0, s3
    li a1, 4096
    SC 215
    EXPECT 0, 532
    mv a0, s4
    SC 57
    EXPECT 0, 534
    mv a0, s5
    SC 57
    EXPECT 0, 535
    li a0, -100
    lla a1, .Lns_target
    li a2, 0
    SC 35
    EXPECT 0, 536
    li a0, -100
    lla a1, .Lns_plain_target
    li a2, 0
    SC 35
    EXPECT 0, 537
    li a0, -100
    lla a1, .Lns_noreplace_target
    li a2, 0
    SC 35
    EXPECT 0, 538
    OPEN .Lns_parent, 0x10000, 539
    mv s1, a0
    mv a0, s1
    SC 82
    EXPECT 0, 540
    mv a0, s1
    SC 57
    EXPECT 0, 541
    j .Lpass

# /// 同 inode 别名、目录拓扑与反向父目录改名由真实普通用户 syscall 消费。
.Lnamespace_directories:
    li s6, 0x49000000
    li a0, -100
    lla a1, .Lns_alias_source
    li a2, 0xc2
    li a3, 0x180
    SC 56
    OK 550
    mv s0, a0
    mv a0, s0
    lla a1, .Lns_payload_alias
    li a2, 4
    SC 64
    EXPECT 4, 551
    mv a0, s0
    SC 82
    EXPECT 0, 552
    mv a0, s0
    SC 57
    EXPECT 0, 553
    li a0, -100
    lla a1, .Lns_alias_source
    li a2, -100
    lla a3, .Lns_alias_target
    li a4, 0
    SC 37
    EXPECT 0, 554
    li a0, -100
    lla a1, .Lns_cross_a
    li a2, 0xc2
    li a3, 0x180
    SC 56
    OK 660
    mv s0, a0
    mv a0, s0
    lla a1, .Lns_payload_cross_a
    li a2, 4
    SC 64
    EXPECT 4, 661
    mv a0, s0
    SC 82
    EXPECT 0, 662
    lla t0, .Lns_cross_fds
    sd s0, 0(t0)
    li a0, -100
    lla a1, .Lns_cross_b
    li a2, 0xc2
    li a3, 0x180
    SC 56
    OK 663
    mv s1, a0
    mv a0, s1
    lla a1, .Lns_payload_cross_b
    li a2, 4
    SC 64
    EXPECT 4, 664
    mv a0, s1
    SC 82
    EXPECT 0, 665
    lla t0, .Lns_cross_fds
    sd s1, 8(t0)
    li a0, -100
    lla a1, .Lns_d1
    li a2, 0x1ff
    SC 34
    EXPECT 0, 666
    li a0, -100
    lla a1, .Lns_d2
    li a2, 0x1ff
    SC 34
    EXPECT 0, 667
    li a0, -100
    lla a1, .Lns_cross_a
    li a2, -100
    lla a3, .Lns_d1_a
    li a4, 0
    SC 37
    EXPECT 0, 668
    li a0, -100
    lla a1, .Lns_cross_a
    li a2, -100
    lla a3, .Lns_d2_a
    li a4, 0
    SC 37
    EXPECT 0, 669
    li a0, -100
    lla a1, .Lns_cross_b
    li a2, -100
    lla a3, .Lns_d1_b
    li a4, 0
    SC 37
    EXPECT 0, 670
    li a0, -100
    lla a1, .Lns_cross_b
    li a2, -100
    lla a3, .Lns_d2_b
    li a4, 0
    SC 37
    EXPECT 0, 671
    NSSTAT s0, 672
    lla t0, .Lstat
    lwu a0, 20(t0)
    EXPECT 3, 673
    NSSTAT s1, 674
    lla t0, .Lstat
    lwu a0, 20(t0)
    EXPECT 3, 675
    OPEN .Lns_d1, 0x10000, 676
    mv s0, a0
    mv a0, s0
    SC 82
    EXPECT 0, 677
    mv a0, s0
    SC 57
    EXPECT 0, 678
    OPEN .Lns_d2, 0x10000, 679
    mv s0, a0
    mv a0, s0
    SC 82
    EXPECT 0, 680
    mv a0, s0
    SC 57
    EXPECT 0, 681

    li a0, -100
    lla a1, .Lns_a
    li a2, 0x1ff
    SC 34
    EXPECT 0, 555
    li a0, -100
    lla a1, .Lns_b
    li a2, 0x1ff
    SC 34
    EXPECT 0, 556
    li a0, -100
    lla a1, .Lns_ax
    li a2, 0x1ff
    SC 34
    EXPECT 0, 557
    li a0, -100
    lla a1, .Lns_a2
    li a2, 0x1ff
    SC 34
    EXPECT 0, 558
    li a0, -100
    lla a1, .Lns_by
    li a2, 0x1ff
    SC 34
    EXPECT 0, 559
    li a0, -100
    lla a1, .Lns_cwd
    li a2, 0x1ff
    SC 34
    EXPECT 0, 560
    OPEN .Lns_a, 0x10000, 561
    mv s0, a0
    NSSTAT s0, 562
    lla t0, .Lstat
    lwu a0, 20(t0)
    EXPECT 4, 563
    mv a0, s0
    SC 82
    EXPECT 0, 564
    mv a0, s0
    SC 57
    EXPECT 0, 565
    OPEN .Lns_b, 0x10000, 566
    mv s0, a0
    NSSTAT s0, 567
    lla t0, .Lstat
    lwu a0, 20(t0)
    EXPECT 3, 568
    mv a0, s0
    SC 82
    EXPECT 0, 569
    mv a0, s0
    SC 57
    EXPECT 0, 570
    OPEN .Lns_parent, 0x10000, 571
    mv s0, a0
    mv a0, s0
    SC 82
    EXPECT 0, 572
    mv a0, s0
    SC 57
    EXPECT 0, 573
    PHASE 1
    GATE 1
    li a0, -100
    lla a1, .Lns_cross_a
    li a2, 0
    SC 35
    EXPECT 0, 680
    li a0, -100
    lla a1, .Lns_cross_b
    li a2, 0
    SC 35
    EXPECT 0, 681
    OPEN .Lns_parent, 0x10000, 682
    mv s0, a0
    mv a0, s0
    SC 82
    EXPECT 0, 683
    mv a0, s0
    SC 57
    EXPECT 0, 684


    li a0, -100
    lla a1, .Lns_alias_source
    li a2, -100
    lla a3, .Lns_alias_target
    li a4, 0
    SC 276
    EXPECT 0, 574
    li a0, -100
    lla a1, .Lns_missing
    li a2, -100
    lla a3, .Lns_missing
    li a4, 0
    SC 276
    EXPECT -2, 575
    li a0, -100
    lla a1, .Lns_a
    li a2, -100
    lla a3, .Lns_ax
    li a4, 0
    SC 276
    EXPECT -22, 716
    li a0, -100
    lla a1, .Lns_a
    li a2, -100
    lla a3, .Lns_descendant
    li a4, 0
    SC 276
    EXPECT -22, 576
    li a0, -100
    lla a1, .Lns_ax
    li a2, -100
    lla a3, .Lns_bx
    li a4, 0
    SC 276
    EXPECT 0, 577
    OPEN .Lns_a, 0x10000, 578
    mv s0, a0
    NSSTAT s0, 579
    lla t0, .Lstat
    lwu a0, 20(t0)
    EXPECT 3, 580
    mv a0, s0
    SC 57
    EXPECT 0, 581
    OPEN .Lns_b, 0x10000, 582
    mv s0, a0
    NSSTAT s0, 583
    lla t0, .Lstat
    lwu a0, 20(t0)
    EXPECT 4, 584
    lla t0, .Lstat
    ld a0, 8(t0)
    mv s3, a0
    mv a0, s0
    SC 57
    EXPECT 0, 585
    OPEN .Lns_bx_parent, 0x10000, 586
    mv s1, a0
    NSSTAT s1, 587
    lla t0, .Lstat
    ld a0, 8(t0)
    li s11, 588
    beq a0, s3, 991f
    j .Lexit
991:
    mv a0, s1
    SC 57
    EXPECT 0, 635
991:
    lla a0, .Lns_cwd
    SC 49
    EXPECT 0, 589
    li a0, -100
    lla a1, .Lns_cwd
    li a2, 0x200
    SC 35
    EXPECT 0, 590
    li a0, -100
    lla a1, .Lns_child
    li a2, 0x1ff
    SC 34
    EXPECT -2, 591
    lla a0, .Lns_parent
    SC 49
    EXPECT 0, 592
    li a0, -100
    lla a1, .Lns_alias_source
    li a2, -100
    lla a3, .Lns_alias_target
    li a4, 2
    SC 276
    EXPECT -22, 593
    li a0, -100
    lla a1, .Lns_alias_source
    li a2, -100
    lla a3, .Lns_alias_target
    li a4, 4
    SC 276
    EXPECT -22, 594
    li a0, -100
    lla a1, .Lns_a
    li a2, 0x201
    SC 35
    EXPECT -22, 595

    li s3, 12
    li a0, 0x311
    lla a1, .Lns_stack_a
    li t0, 16384
    add a1, a1, t0
    li a2, 0
    li a3, 0
    li a4, 0
    SC 220
    OK 596
    beqz a0, .Lns_move_a2_child
    mv s4, a0
    li s3, 16
    li a0, 0x311
    lla a1, .Lns_stack_b
    li t0, 16384
    add a1, a1, t0
    li a2, 0
    li a3, 0
    li a4, 0
    SC 220
    OK 597
    beqz a0, .Lns_move_y_child
    mv s5, a0
    li t0, 1
    fence rw, w
    sw t0, 8(s6)
    mv a0, s4
    lla a1, .Lstatus
    li a2, 0
    li a3, 0
    SC 260
    li s11, 598
    beq a0, s4, 991f
    j .Lexit
991:
    lla t0, .Lstatus
    lw a0, 0(t0)
    EXPECT 0, 599
    mv a0, s5
    lla a1, .Lstatus
    li a2, 0
    li a3, 0
    SC 260
    li s11, 600
    beq a0, s5, 991f
    j .Lexit
991:
    lla t0, .Lstatus
    lw a0, 0(t0)
    EXPECT 0, 601
    ld a0, 12(s6)
    EXPECT 0, 602
    ld a0, 16(s6)
    EXPECT 0, 603
    lla t0, .Lns_cross_fds
    ld s0, 0(t0)
    ld s1, 8(t0)
    NSSTAT s0, 640
    lla t0, .Lstat
    lwu a0, 20(t0)
    EXPECT 1, 641
    lla t0, .Lstat
    ld s4, 8(t0)
    OPEN .Lns_d1_b, 0, 642
    mv s5, a0
    NSSTAT s5, 643
    lla t0, .Lstat
    ld a0, 8(t0)
    li s11, 644
    beq a0, s4, 991f
    j .Lexit
991:
    mv a0, s5
    lla a1, .Lbuffer
    li a2, 4
    li a3, 0
    SC 67
    EXPECT 4, 645
    lla a0, .Lbuffer
    lla a1, .Lns_payload_cross_a
    li a2, 4
    li s11, 646
    call .Lequal
    mv a0, s5
    SC 57
    EXPECT 0, 647
    NSSTAT s1, 648
    lla t0, .Lstat
    lwu a0, 20(t0)
    EXPECT 1, 649
    lla t0, .Lstat
    ld s4, 8(t0)
    OPEN .Lns_d2_a, 0, 650
    mv s5, a0
    NSSTAT s5, 651
    lla t0, .Lstat
    ld a0, 8(t0)
    li s11, 652
    beq a0, s4, 991f
    j .Lexit
991:
    mv a0, s5
    lla a1, .Lbuffer
    li a2, 4
    li a3, 0
    SC 67
    EXPECT 4, 653
    lla a0, .Lbuffer
    lla a1, .Lns_payload_cross_b
    li a2, 4
    li s11, 654
    call .Lequal
    mv a0, s5
    SC 57
    EXPECT 0, 655
    mv a0, s0
    li a1, 0
    li a2, 0
    SC 62
    EXPECT 0, 656
    mv a0, s0
    lla a1, .Lbuffer
    li a2, 4
    li a3, 0
    SC 67
    EXPECT 4, 657
    lla a0, .Lbuffer
    lla a1, .Lns_payload_cross_a
    li a2, 4
    li s11, 658
    call .Lequal
    mv a0, s1
    li a1, 0
    li a2, 0
    SC 62
    EXPECT 0, 659
    mv a0, s1
    lla a1, .Lbuffer
    li a2, 4
    li a3, 0
    SC 67
    EXPECT 4, 660
    lla a0, .Lbuffer
    lla a1, .Lns_payload_cross_b
    li a2, 4
    li s11, 661
    call .Lequal

    OPEN .Lns_a, 0x10000, 604
    mv s0, a0
    NSSTAT s0, 605
    lla t0, .Lstat
    lwu a0, 20(t0)
    EXPECT 3, 606
    lla t0, .Lstat
    ld a0, 8(t0)
    mv s2, a0
    mv a0, s0
    SC 82
    EXPECT 0, 607
    mv a0, s0
    SC 57
    EXPECT 0, 608
    OPEN .Lns_b, 0x10000, 609
    mv s0, a0
    NSSTAT s0, 610
    lla t0, .Lstat
    lwu a0, 20(t0)
    EXPECT 4, 611
    lla t0, .Lstat
    ld a0, 8(t0)
    mv s3, a0
    mv a0, s0
    SC 82
    EXPECT 0, 612
    mv a0, s0
    SC 57
    EXPECT 0, 613
    OPEN .Lns_bx_parent, 0x10000, 614
    mv s1, a0
    NSSTAT s1, 615
    lla t0, .Lstat
    ld a0, 8(t0)
    li s11, 616
    beq a0, s3, 991f
    j .Lexit
991:
    mv a0, s1
    SC 57
    EXPECT 0, 636
    OPEN .Lns_ay_parent, 0x10000, 617
    mv s1, a0
    NSSTAT s1, 618
    lla t0, .Lstat
    ld a0, 8(t0)
    li s11, 619
    beq a0, s2, 991f
    j .Lexit
991:
    mv a0, s1
    SC 57
    EXPECT 0, 637
    OPEN .Lns_parent, 0x10000, 620
    mv s0, a0
    mv a0, s0
    SC 82
    EXPECT 0, 621
    mv a0, s0
    SC 57
    EXPECT 0, 622
    PHASE 2
    GATE 2
    lla t0, .Lns_cross_fds
    ld a0, 0(t0)
    SC 57
    EXPECT 0, 662
    lla t0, .Lns_cross_fds
    ld a0, 8(t0)
    SC 57
    EXPECT 0, 663

    li a0, -100
    lla a1, .Lns_alias_source
    li a2, 0
    SC 35
    EXPECT 0, 623
    li a0, -100
    lla a1, .Lns_alias_target
    li a2, 0
    SC 35
    EXPECT 0, 624
    li a0, -100
    lla a1, .Lns_d1_b
    li a2, 0
    SC 35
    EXPECT 0, 638
    li a0, -100
    lla a1, .Lns_d2_a
    li a2, 0
    SC 35
    EXPECT 0, 639
    li a0, -100
    lla a1, .Lns_bx
    li a2, 0x200
    SC 35
    EXPECT 0, 625
    li a0, -100
    lla a1, .Lns_ba2
    li a2, 0x200
    SC 35
    EXPECT 0, 626
    li a0, -100
    lla a1, .Lns_ay
    li a2, 0x200
    SC 35
    EXPECT 0, 627
    li a0, -100
    lla a1, .Lns_a
    li a2, 0x200
    SC 35
    EXPECT 0, 628
    li a0, -100
    lla a1, .Lns_b
    li a2, 0x200
    SC 35
    EXPECT 0, 629
    li a0, -100
    lla a1, .Lns_d1
    li a2, 0x200
    SC 35
    EXPECT 0, 640
    li a0, -100
    lla a1, .Lns_d2
    li a2, 0x200
    SC 35
    EXPECT 0, 641
    OPEN .Lns_parent, 0x10000, 630
    mv s0, a0
    mv a0, s0
    SC 82
    EXPECT 0, 631
    mv a0, s0
    SC 57
    EXPECT 0, 632
    j .Lpass
.Lns_move_a2_child:
    lw t0, 8(s6)
    beqz t0, .Lns_move_a2_child
    fence rw, rw
    li a0, -100
    lla a1, .Lns_d1_a
    li a2, -100
    lla a3, .Lns_d1_b
    li a4, 0
    SC 276
    EXPECT 0, 664
    fence rw, rw
    li a0, -100
    lla a1, .Lns_a2
    li a2, -100
    lla a3, .Lns_ba2
    li a4, 0
    SC 276
    EXPECT 0, 633
    li t0, 0
    add t1, s6, s3
    sw t0, 0(t1)
    li a0, 0
    SC 93
    j .Lexit
.Lns_move_y_child:
    lw t0, 8(s6)
    beqz t0, .Lns_move_y_child
    fence rw, rw
    li a0, -100
    lla a1, .Lns_d2_b
    li a2, -100
    lla a3, .Lns_d2_a
    li a4, 0
    SC 276
    EXPECT 0, 665
    li a0, -100
    lla a1, .Lns_by
    li a2, -100
    lla a3, .Lns_ay
    li a4, 0
    SC 276
    EXPECT 0, 634
    li t0, 0
    add t1, s6, s3
    sw t0, 0(t1)
    li a0, 0
    SC 93
    j .Lexit

# /// 核对整段常量字节，错误保留调用方的具名断言编号。
.type .Lconstant, @function
.Lconstant:
    beqz a1, .Lconstant_done
.Lconstant_next:
    lbu t0, 0(a0)
    bne t0, a2, .Lexit
    addi a0, a0, 1
    addi a1, a1, -1
    bnez a1, .Lconstant_next
.Lconstant_done:
    ret
# /// 同文件源图样按实际原始逻辑偏移核完整字节。
.type .Lpattern, @function
.Lpattern:
    lbu t0, 0(a0)
    andi t1, a2, 255
    bne t0, t1, .Lexit
    addi a0, a0, 1
    addi a1, a1, -1
    addi a2, a2, 1
    bnez a1, .Lpattern
    ret
# /// 比较独立常量输入，append 的顺序和任何重叠都会导致普通非零退出。
.type .Lequal, @function
.Lequal:
    lbu t0, 0(a0)
    lbu t1, 0(a1)
    bne t0, t1, .Lexit
    addi a0, a0, 1
    addi a1, a1, 1
    addi a2, a2, -1
    bnez a2, .Lequal
    ret
# /// 正常结束沿 exit_group，不以共享字或内核回声替代结果。
.Lpass:
    li s11, 0
.Lexit:
    mv a0, s11
    SC 94
    j .Lexit
.size _start, .-_start

.section .rodata
.Luser_path: .asciz "/test/write-user"
.Lshared_path: .asciz "/test/write-reserve"
.Ldirectory: .asciz "/test"
.Lmap_path: .asciz "/test/write-map"
.Lappend_path: .asciz "/test/write-append"
.Lself: .asciz "/test/bin/user-write"
.Lnext: .asciz "/test/bin/user-write-next"
.Lmode_child: .asciz "n"
.Lmode_old: .asciz "o"
.Lalpha: .ascii "alpha"
.Lbeta: .ascii "BETA"
.Lgamma: .ascii "xyz"
.Ldelta: .ascii "!?"
.Lappend_expected: .ascii "alphaBETAxyz!?"
.Lpayload: .byte 0x39
.Ltruncate_input: .ascii "123456789"
.Ltruncate_expected: .byte 0x31, 0x32, 0x33, 0, 0, 0, 0, 0, 0
.Lns_parent: .asciz "/test/ns-work"
.Lns_permission: .asciz "/test/ns-work/ns-permission"
.Lns_permission_denied: .asciz "/test/ns-work/ns-permission/denied"
.Lns_race: .asciz "/test/ns-work/ns-race"
.Lns_mode: .asciz "/test/ns-work/ns-mode"
.Lns_data: .asciz "/test/ns-work/ns-data"
.Lns_ro: .asciz "/test/ns-work/ns-ro"
.Lns_shared: .asciz "/test/ns-work/ns-shared"
.Lns_copy: .asciz "/test/ns-work/ns-copy"
.Lns_dir: .asciz "/test/ns-work/ns-dir"
.Lns_raw:
    .ascii "/test/ns-work/ns-raw-"
    .byte 0xff, 0
.Lns_short: .asciz "/test/ns-work/ns-short"
.Lns_long: .asciz "/test/ns-work/ns-long"
.Lns_long_target:
    .ascii "/test/ns-work"
    .rept 30
    .ascii "/."
    .endr
    .asciz "/ns-data"
.Lns_dangling: .asciz "/test/ns-work/ns-dangling"
.Lns_missing: .asciz "/test/ns-work/ns-missing"
.Lns_sgid_child: .asciz "/test/ns-work/ns-sgid-child"
.Lns_trailing: .asciz "/test/ns-work/ns-mode/"
.Lns_rooted: .asciz "/../../test/ns-work/ns-data"
.Lns_missing_parent: .asciz "/test/ns-work/ns-absent/../ns-lex"
.Lns_old: .asciz "/test/ns-work/ns-old"
.Lns_alias: .asciz "/test/ns-work/ns-alias"
.Lns_source: .asciz "/test/ns-work/ns-source"
.Lns_target: .asciz "/test/ns-work/ns-target"
.Lns_noreplace_source: .asciz "/test/ns-work/ns-noreplace-source"
.Lns_noreplace_target: .asciz "/test/ns-work/ns-noreplace-target"
.Lns_plain_source: .asciz "/test/ns-work/ns-plain-source"
.Lns_plain_target: .asciz "/test/ns-work/ns-plain-target"
.Lns_alias_source: .asciz "/test/ns-work/ns-alias-source"
.Lns_alias_target: .asciz "/test/ns-work/ns-alias-target"
.Lns_cross_a: .asciz "/test/ns-work/ns-cross-a"
.Lns_cross_b: .asciz "/test/ns-work/ns-cross-b"
.Lns_d1: .asciz "/test/ns-work/ns-d1"
.Lns_d2: .asciz "/test/ns-work/ns-d2"
.Lns_d1_a: .asciz "/test/ns-work/ns-d1/a"
.Lns_d1_b: .asciz "/test/ns-work/ns-d1/b"
.Lns_d2_a: .asciz "/test/ns-work/ns-d2/a"
.Lns_d2_b: .asciz "/test/ns-work/ns-d2/b"
.Lns_a: .asciz "/test/ns-work/ns-a"
.Lns_b: .asciz "/test/ns-work/ns-b"
.Lns_ax: .asciz "/test/ns-work/ns-a/x"
.Lns_a2: .asciz "/test/ns-work/ns-a/a2"
.Lns_by: .asciz "/test/ns-work/ns-b/y"
.Lns_bx: .asciz "/test/ns-work/ns-b/x"
.Lns_ba2: .asciz "/test/ns-work/ns-b/a2"
.Lns_ay: .asciz "/test/ns-work/ns-a/y"
.Lns_bx_parent: .asciz "/test/ns-work/ns-b/x/.."
.Lns_ay_parent: .asciz "/test/ns-work/ns-a/y/.."
.Lns_descendant: .asciz "/test/ns-work/ns-a/x/moved"
.Lns_cwd: .asciz "/test/ns-work/ns-cwd"
.Lns_child: .asciz "child"
.Lns_payload_mode: .ascii "MODE"
.Lns_payload_cross_a: .ascii "AAAA"
.Lns_payload_cross_b: .ascii "BBBB"
.Lns_payload_race: .ascii "RACE"
.Lns_payload_shared: .ascii "SHRD"
.Lns_payload_copy: .ascii "COPY"
.Lns_payload_raw: .ascii "RAW!"
.Lns_payload_dangling: .ascii "DANG"
.Lns_payload_old: .ascii "OLD!"
.Lns_payload_new: .ascii "NEW!"
.Lns_payload_source: .ascii "SRC!"
.Lns_payload_target: .ascii "TGT!"
.Lns_payload_plain: .ascii "PLAIN"
.Lns_payload_alias: .ascii "LINK"
.balign 8
.Lfutex_timeout: .quad 5, 0

.section .data
.balign 8
.Largv_child: .quad .Lnext, .Lmode_child, 0
.Largv_old: .quad .Lnext, .Lmode_old, 0
.Lbad_argv: .quad .Lmap_path, 0
.section .bss
.balign 16
.Lbuffer: .space 8192
.Lstat: .space 128
.Lstatus: .space 4
.byte 0
.balign 16
.Lns_stack_a: .space 16384
.Lns_stack_b: .space 16384
.Lns_fd_table: .space 4096
.Lns_cross_fds: .space 16
