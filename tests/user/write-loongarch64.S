.text

# /// 普通目标 syscall 约定，返回值和具体 errno 均由 a0 消费。
.macro SC number
    li.d $a7, \number
    syscall 0
.endm
# /// 每个断言点保留独立退出码，条件分支只跨一条普通跳转。
# /// t0 用于比较值；调用者在断言后必须重新取得仍要使用的地址。
.macro EXPECT value, number
    li.d $s8, \number
    li.d $t0, \value
    beq $a0, $t0, 991f
    b .Lexit
991:
.endm
# /// open 和 clone 的非负返回值允许描述符零。
.macro OK number
    li.d $s8, \number
    bge $a0, $zero, 991f
    b .Lexit
991:
.endm
# /// openat 直接消费普通字节路径，截断意图由生产准入和 inode 实现决定。
.macro OPEN pathname, flags, number
    li.d $a0, -100
    la.local $a1, \pathname
    li.d $a2, \flags
    li.d $a3, 0
    SC 56
    OK \number
.endm
# /// lseek SEEK_CUR 观察真实共享位置，不在用户端模拟 cursor。
.macro CURSOR fd, value, number
    move $a0, \fd
    li.d $a1, 0
    li.d $a2, 1
    SC 62
    EXPECT \value, \number
.endm
# /// fstat 的 size 来自生产 inode 字节长度，结构偏移使用 Linux 通用 64 位 ABI。
.macro SIZE fd, value, number
    move $a0, \fd
    la.local $a1, .Lstat
    SC 80
    EXPECT 0, \number
    la.local $t0, .Lstat
    ld.d $a0, $t0, 48
    EXPECT \value, \number
.endm
# /// 两目标使用 asm-generic stat；mode 和 nlink 是独立的 32 位字段，不能合并读为 64 位。
.macro NSSTAT fd, number
    move $a0, \fd
    la.local $a1, .Lstat
    SC 80
    EXPECT 0, \number
.endm


# /// 控制字是普通共享匿名页；忙等期间不发 syscall，保留远核真实 W 翻译。
.macro PHASE value
    li.d $t0, \value
    dbar 0
    st.w $t0, $s6, 0
.endm
# /// 每个阶段只消费协调者发布的单调许可，退出仍由普通 wait/join 核验。
.macro GATE value
991:
    ld.w $t0, $s6, 4
    li.d $t1, \value
    bltu $t0, $t1, 991b
    dbar 0
.endm

# /// 每次 msync 都把完整参数装入真实普通 ABI，地址和 errno 不由控制页回声代替。
.macro MS address, length, flags, result, number
    move $a0, \address
    li.d $a1, \length
    li.d $a2, \flags
    SC 227
    EXPECT \result, \number
.endm
# /// argv 选择确定的普通文件、映射、同步和截断模式，所有返回值均消费真实 syscall。
.global _start
.type _start, @function
_start:
    li.d $s8, 1
    ld.d $t0, $sp, 0
    li.d $t1, 2
    bgeu $t0, $t1, 991f
    b .Lexit
991:
    ld.d $t0, $sp, 16
    ld.bu $t0, $t0, 0
    li.d $t1, 'w'
    beq $t0, $t1, .Lwrite
    li.d $t1, 'a'
    beq $t0, $t1, .Lappend
    li.d $t1, 'e'
    beq $t0, $t1, .Lexec
    li.d $t1, 'n'
    beq $t0, $t1, .Lchild_image
    li.d $t1, 'o'
    beq $t0, $t1, .Lold_released
    li.d $t1, 's'
    beq $t0, $t1, .Lshared
    li.d $t1, 'r'
    beq $t0, $t1, .Lremote
    li.d $t1, 'm'
    beq $t0, $t1, .Lremote
    li.d $t1, 'c'
    beq $t0, $t1, .Lremote
    li.d $t1, 'f'
    beq $t0, $t1, .Lfile_futex
    li.d $t1, 'p'
    beq $t0, $t1, .Lfile_futex
    li.d $t1, 'q'
    beq $t0, $t1, .Lfile_futex
    li.d $t1, 'u'
    beq $t0, $t1, .Lfile_futex
    li.d $t1, 'y'
    beq $t0, $t1, .Lsync_abi
    li.d $t1, 'b'
    beq $t0, $t1, .Lsync_wait
    li.d $t1, 'd'
    beq $t0, $t1, .Lsync_write
    li.d $t1, 'z'
    beq $t0, $t1, .Lsync_write
    li.d $t1, 't'
    bne $t0, $t1, 991f
    b .Ltruncate
991:
    li.d $t1, 'v'
    bne $t0, $t1, 991f
    b .Ltruncate_ro
991:
    li.d $t1, 'h'
    bne $t0, $t1, 991f
    b .Ltruncate_dac
991:
    li.d $t1, 'k'
    bne $t0, $t1, 991f
    b .Ltruncate_cow
991:
    li.d $t1, 'j'
    bne $t0, $t1, 991f
    b .Ltruncate_wait
991:
    li.d $t1, 'N'
    beq $t0, $t1, .Lnamespace_dispatch
    b .Lexit

.Lnamespace_dispatch:
    ld.d $t0, $sp, 16
    ld.bu $t0, $t0, 1
    li.d $t1, '3'
    beq $t0, $t1, .Lnamespace_create
    li.d $t1, '4'
    beq $t0, $t1, .Lnamespace_unlink
    li.d $t1, '5'
    beq $t0, $t1, .Lnamespace_rename
    li.d $t1, '6'
    beq $t0, $t1, .Lnamespace_directories
    b .Lexit

# /// 源跨坏页只接受 17 字节，零前缀故障、负 pwrite 和定位写均不能改共享位置。
.Lwrite:
    OPEN .Luser_path, 2, 2
    move $s0, $a0
    li.d $a0, 0
    li.d $a1, 8192
    li.d $a2, 3
    li.d $a3, 0x22
    li.d $a4, -1
    li.d $a5, 0
    SC 222
    OK 3
    move $s4, $a0
    li.d $t0, 4079
    add.d $t0, $s4, $t0
    li.d $t1, 17
    li.d $t2, 0x72
.Lfill_prefix:
    st.b $t2, $t0, 0
    addi.d $t0, $t0, 1
    addi.d $t1, $t1, -1
    bnez $t1, .Lfill_prefix
    li.d $t0, 4096
    add.d $a0, $s4, $t0
    li.d $a1, 4096
    SC 215
    EXPECT 0, 4
    move $a0, $s0
    li.d $t0, 4079
    add.d $a1, $s4, $t0
    li.d $a2, 64
    SC 64
    EXPECT 17, 5
    CURSOR $s0, 17, 6
    SIZE $s0, 17, 7
    move $a0, $s0
    li.d $t0, 4096
    add.d $a1, $s4, $t0
    li.d $a2, 64
    SC 64
    EXPECT -14, 8
    CURSOR $s0, 17, 9
    move $a0, $s0
    li.d $t0, 4079
    add.d $a1, $s4, $t0
    li.d $a2, 64
    li.d $a3, 4093
    SC 68
    EXPECT 17, 10
    CURSOR $s0, 17, 11
    SIZE $s0, 4110, 12
    move $a0, $s0
    la.local $a1, .Lpayload
    li.d $a2, 1
    li.d $a3, -1
    SC 68
    EXPECT -22, 13
    CURSOR $s0, 17, 14
    move $a0, $s0
    la.local $a1, .Lbuffer
    li.d $a2, 4096
    li.d $a3, 0
    SC 67
    EXPECT 4096, 15
    la.local $a0, .Lbuffer
    li.d $a1, 17
    li.d $a2, 0x72
    li.d $s8, 16
    bl .Lconstant
    la.local $a0, .Lbuffer
    addi.d $a0, $a0, 17
    li.d $a1, 4076
    li.d $a2, 0
    li.d $s8, 17
    bl .Lconstant
    la.local $a0, .Lbuffer
    li.d $t0, 4093
    add.d $a0, $a0, $t0
    li.d $a1, 3
    li.d $a2, 0x72
    li.d $s8, 18
    bl .Lconstant
    move $a0, $s0
    la.local $a1, .Lbuffer
    li.d $a2, 17
    li.d $a3, 4093
    SC 67
    EXPECT 17, 19
    la.local $a0, .Lbuffer
    li.d $a1, 17
    li.d $a2, 0x72
    li.d $s8, 20
    bl .Lconstant
    # 冷 private 文件页作为同一 inode 的输入，不提前读取或触页。
    OPEN .Lmap_path, 2, 21
    move $s1, $a0
    li.d $a0, 0
    li.d $a1, 8192
    li.d $a2, 1
    li.d $a3, 2
    move $a4, $s1
    li.d $a5, 0
    SC 222
    OK 22
    move $s5, $a0
    move $a0, $s1
    li.d $t0, 4093
    add.d $a1, $s5, $t0
    li.d $a2, 1027
    li.d $a3, 10240
    SC 68
    EXPECT 1027, 23
    CURSOR $s1, 0, 24
    SIZE $s1, 11267, 25
    move $a0, $s1
    la.local $a1, .Lbuffer
    li.d $a2, 1027
    li.d $a3, 10240
    SC 67
    EXPECT 1027, 26
    la.local $a0, .Lbuffer
    li.d $a1, 1027
    li.d $a2, 4093
    li.d $s8, 27
    bl .Lpattern
    move $a0, $s0
    la.local $a1, .Lbuffer
    li.d $a2, 1027
    SC 64
    EXPECT 1027, 32
    CURSOR $s0, 1044, 33
    SIZE $s0, 4110, 34
    move $a0, $s0
    la.local $a1, .Lbuffer
    li.d $a2, 1027
    li.d $a3, 17
    SC 67
    EXPECT 1027, 35
    la.local $a0, .Lbuffer
    li.d $a1, 1027
    li.d $a2, 4093
    li.d $s8, 36
    bl .Lpattern
    # 写打开存活时，loader 应先报 ETXTBSY，不能先解析非 ELF 内容报 ENOEXEC。
    la.local $a0, .Lmap_path
    la.local $a1, .Lbad_argv
    li.d $a2, 0
    SC 221
    EXPECT -26, 28
    move $a0, $s1
    SC 57
    EXPECT 0, 29
    # 文件 VMA 仍保活原写 OFD，解除映射后才结束它的写排斥责任。
    move $a0, $s5
    li.d $a1, 8192
    SC 215
    EXPECT 0, 37
    la.local $a0, .Lmap_path
    la.local $a1, .Lbad_argv
    li.d $a2, 0
    SC 221
    EXPECT -8, 30
    move $a0, $s0
    SC 57
    EXPECT 0, 31
    b .Lpass

# /// dup 共享顺序位置，pwrite O_APPEND 在 EOF 提交但保原 cursor。
.Lappend:
    OPEN .Lappend_path, 0x402, 40
    move $s0, $a0
    OPEN .Lappend_path, 0x402, 41
    move $s1, $a0
    move $a0, $s0
    la.local $a1, .Lalpha
    li.d $a2, 5
    SC 64
    EXPECT 5, 42
    move $a0, $s0
    SC 23
    OK 43
    move $s2, $a0
    move $a0, $s2
    la.local $a1, .Lbeta
    li.d $a2, 4
    li.d $a3, 0
    SC 68
    EXPECT 4, 44
    CURSOR $s0, 5, 45
    CURSOR $s2, 5, 46
    CURSOR $s1, 0, 47
    move $a0, $s2
    la.local $a1, .Lgamma
    li.d $a2, 3
    SC 64
    EXPECT 3, 48
    CURSOR $s0, 12, 49
    move $a0, $s1
    la.local $a1, .Ldelta
    li.d $a2, 2
    li.d $a3, 123
    SC 68
    EXPECT 2, 50
    CURSOR $s1, 0, 51
    SIZE $s0, 14, 52
    move $a0, $s0
    la.local $a1, .Lbuffer
    li.d $a2, 14
    li.d $a3, 0
    SC 67
    EXPECT 14, 53
    la.local $a0, .Lbuffer
    la.local $a1, .Lappend_expected
    li.d $a2, 14
    li.d $s8, 54
    bl .Lequal
    b .Lpass

# /// 活映像禁止 write-open 和 O_TRUNC；fork 子换映像后仍由父旧 Mm 保 deny。
.Lexec:
    li.d $a0, -100
    la.local $a1, .Lself
    li.d $a2, 1
    li.d $a3, 0
    SC 56
    EXPECT -26, 60
    li.d $a0, -100
    la.local $a1, .Lself
    li.d $a2, 0x201
    li.d $a3, 0
    SC 56
    EXPECT -26, 61
    li.d $a0, 17
    li.d $a1, 0
    li.d $a2, 0
    li.d $a3, 0
    li.d $a4, 0
    SC 220
    OK 62
    beqz $a0, .Lfork_child
    move $s0, $a0
    move $a0, $s0
    la.local $a1, .Lstatus
    li.d $a2, 0
    li.d $a3, 0
    SC 260
    li.d $s8, 63
    bne $a0, $s0, .Lexit
    la.local $t0, .Lstatus
    ld.w $a0, $t0, 0
    EXPECT 0, 64
    # 换到不同 inode 的真实 ELF 后，原 Mm 最后使用结束才允许写打开旧映像。
    la.local $a0, .Lnext
    la.local $a1, .Largv_old
    li.d $a2, 0
    SC 221
    li.d $s8, 65
    b .Lexit
.Lfork_child:
    la.local $a0, .Lnext
    la.local $a1, .Largv_child
    li.d $a2, 0
    SC 221
    li.d $s8, 66
    b .Lexit
.Lchild_image:
    li.d $a0, -100
    la.local $a1, .Lself
    li.d $a2, 1
    li.d $a3, 0
    SC 56
    EXPECT -26, 67
    b .Lpass
.Lold_released:
    OPEN .Lself, 1, 68
    SC 57
    EXPECT 0, 69
    b .Lpass

# /// 只读 OFD 拒绝共享写；同源尾页 store 不增 EOF，增长后新增 gap 必须重新归零。
.Lshared:
    li.d $s6, 0x49000000
    OPEN .Lshared_path, 0, 75
    move $s0, $a0
    ld.d $s4, $s6, 24
    li.d $a0, 0x48000000
    li.d $a1, 8192
    li.d $a2, 3
    li.d $a3, 0x11
    move $a4, $s0
    move $a5, $s4
    SC 222
    EXPECT -13, 76
    move $a0, $s0
    SC 57
    EXPECT 0, 77
    OPEN .Lshared_path, 2, 80
    move $s0, $a0
    li.d $a0, 0x48000000
    li.d $a1, 8192
    li.d $a2, 3
    li.d $a3, 0x11
    move $a4, $s0
    move $a5, $s4
    SC 222
    EXPECT 0x48000000, 81
    move $s5, $a0
    li.d $t0, 0x61
    st.b $t0, $s5, 0
    li.d $t0, 0xde
    st.b $t0, $s5, 128
    PHASE 1
    GATE 1
    ld.bu $a0, $s5, 1
    EXPECT 0x62, 82
    ld.bu $a0, $s5, 128
    EXPECT 0, 83
    move $a0, $s0
    la.local $a1, .Lstat
    SC 80
    EXPECT 0, 84
    la.local $t0, .Lstat
    ld.d $a0, $t0, 48
    addi.d $t1, $s4, 513
    bne $a0, $t1, .Lexit
    li.d $t0, 0x74
    st.b $t0, $s5, 128
    PHASE 2
    GATE 2
    b .Lpass

# /// 首次 store 后仅忙等；同步撤 W 后第二次 store 必须经真实 mkwrite。
.Lremote:
    ld.d $t0, $sp, 16
    ld.bu $s7, $t0, 0
    li.d $s6, 0x49000000
    OPEN .Lmap_path, 2, 85
    move $s0, $a0
    li.d $a0, 0x48000000
    li.d $a1, 4096
    li.d $a2, 3
    li.d $a3, 0x11
    move $a4, $s0
    li.d $a5, 0
    SC 222
    EXPECT 0x48000000, 86
    move $s5, $a0
    li.d $t0, 0x81
    st.b $t0, $s5, 0
    PHASE 1
    GATE 1
    li.d $t0, 'c'
    beq $s7, $t0, .Lshared_fork
    li.d $t0, 0x82
    st.b $t0, $s5, 1
    PHASE 2
    GATE 2
    b .Lpass
.Lshared_fork:
    li.d $a0, 17
    li.d $a1, 0
    li.d $a2, 0
    li.d $a3, 0
    li.d $a4, 0
    SC 220
    OK 87
    beqz $a0, .Lshared_child
    move $s1, $a0
    st.d $a0, $s6, 16
    PHASE 2
    GATE 4
    b .Lwait_child
.Lshared_child:
    li.d $t0, 3
    dbar 0
    st.w $t0, $s6, 8
    GATE 3
    li.d $t0, 0x83
    st.b $t0, $s5, 2
    li.d $a0, 0
    SC 93
    b .Lexit

# /// 真正的 fork 子在不同 VA 上 WAIT；父用普通 WAKE 核共享身份或 PRIVATE 隔离。
.Lfile_futex:
    ld.d $t0, $sp, 16
    ld.bu $s7, $t0, 0
    li.d $s6, 0x49000000
    OPEN .Lmap_path, 2, 90
    move $s0, $a0
    li.d $a0, 0x48000000
    li.d $a1, 4096
    li.d $a2, 3
    li.d $a3, 0x11
    move $a4, $s0
    li.d $a5, 0
    SC 222
    EXPECT 0x48000000, 91
    move $s5, $a0
    li.d $a0, 17
    li.d $a1, 0
    li.d $a2, 0
    li.d $a3, 0
    li.d $a4, 0
    SC 220
    OK 92
    beqz $a0, .Lfutex_child
    move $s1, $a0
    st.d $a0, $s6, 16
    GATE 1
    addi.d $a0, $s5, 64
    li.d $a1, 1
    li.d $t0, 'p'
    bne $s7, $t0, .Lfutex_wake
    li.d $a1, 129
.Lfutex_wake:
    li.d $a2, 1
    li.d $a3, 0
    li.d $a4, 0
    li.d $a5, 0
    SC 98
    li.d $t0, 'f'
    bne $s7, $t0, .Lfutex_isolated
    EXPECT 1, 93
    b .Lwait_child
.Lfutex_isolated:
    EXPECT 0, 94
.Lwait_child:
    move $a0, $s1
    la.local $a1, .Lstatus
    li.d $a2, 0
    li.d $a3, 0
    SC 260
    li.d $s8, 95
    bne $a0, $s1, .Lexit
    la.local $t0, .Lstatus
    ld.w $a0, $t0, 0
    EXPECT 0, 96
    b .Lpass
.Lfutex_child:
    move $a0, $s5
    li.d $a1, 4096
    SC 215
    EXPECT 0, 97
    li.d $t0, 'q'
    bne $s7, $t0, .Lfutex_child_map
    OPEN .Lshared_path, 2, 98
    move $s0, $a0
.Lfutex_child_map:
    li.d $a0, 0x4a000000
    li.d $a1, 4096
    li.d $a2, 3
    li.d $a3, 0x11
    move $a4, $s0
    li.d $a5, 0
    SC 222
    EXPECT 0x4a000000, 99
    move $s5, $a0
    PHASE 1
    addi.d $a0, $s5, 64
    li.d $a1, 0
    li.d $t0, 'p'
    bne $s7, $t0, .Lfutex_wait
    li.d $a1, 128
.Lfutex_wait:
    li.d $a2, 0
    la.local $a3, .Lfutex_timeout
    li.d $a4, 0
    li.d $a5, 0
    SC 98
    li.d $t0, 'f'
    bne $s7, $t0, .Lfutex_cancelled
    EXPECT 0, 100
    b .Lfutex_child_exit
.Lfutex_cancelled:
    EXPECT -110, 101
.Lfutex_child_exit:
    li.d $a0, 0
    SC 93
    b .Lexit

# /// 独立 open 的普通 fsync 在已有 WB 上等待，控制者核真实 Blocked 后才恢复 used 消费。
.Lsync_wait:
    li.d $s6, 0x49000000
    OPEN .Lshared_path, 2, 110
    move $s0, $a0
    PHASE 1
    GATE 1
    move $a0, $s0
    SC 82
    EXPECT 0, 111
    PHASE 2
    GATE 2
    move $a0, $s0
    SC 83
    EXPECT 0, 112
    b .Lpass

# /// 原始 __O_SYNC 由 open 归一化；F_SETFL 不撤同步位，错误 write 保已接受位置，pwrite 不改位置。
.Lsync_write:
    ld.d $t0, $sp, 16
    ld.bu $s7, $t0, 0
    li.d $s6, 0x49000000
    li.d $t1, 'z'
    beq $s7, $t1, .Lopen_dsync
    OPEN .Lshared_path, 0x100002, 113
    li.d $s4, 0x101002
    b .Lsync_flags
.Lopen_dsync:
    OPEN .Lshared_path, 0x1002, 114
    li.d $s4, 0x1002
.Lsync_flags:
    move $s0, $a0
    st.d $s0, $s6, 16
    move $a0, $s0
    li.d $a1, 4
    li.d $a2, 0
    SC 25
    EXPECT 0, 115
    move $a0, $s0
    li.d $a1, 3
    SC 25
    li.d $s8, 116
    bne $a0, $s4, .Lexit
    PHASE 1
    GATE 1
    move $a0, $s0
    la.local $a1, .Lpayload
    li.d $a2, 1
    SC 64
    EXPECT -5, 117
    CURSOR $s0, 1, 118
    PHASE 2
    GATE 2
    move $a0, $s0
    la.local $a1, .Lpayload
    li.d $a2, 1
    li.d $a3, 2
    SC 68
    EXPECT 1, 119
    CURSOR $s0, 1, 120
    b .Lpass

# /// shared 非零文件偏移、private COW、孔洞、合法零/异步/INVALIDATE 和冲突位均走普通 sys227。
.Lsync_abi:
    li.d $s6, 0x49000000
    OPEN .Lshared_path, 2, 121
    move $s0, $a0
    li.d $a0, 0x48000000
    li.d $a1, 4096
    li.d $a2, 3
    li.d $a3, 0x11
    move $a4, $s0
    li.d $a5, 4096
    SC 222
    EXPECT 0x48000000, 122
    move $s5, $a0
    li.d $t0, 0x79
    st.b $t0, $s5, 29
    PHASE 1
    GATE 1
    addi.d $s4, $s5, 1
    MS $s4, 4096, 4, -22, 123
    MS $s5, 4096, 8, -22, 124
    MS $s5, 4096, 5, -22, 125
    MS $s5, 4096, 0, 0, 126
    MS $s5, 4096, 1, 0, 127
    MS $s5, 4096, 2, 0, 128
    MS $s5, 4096, 3, 0, 129
    MS $s5, 0, 4, 0, 130
    MS $s5, -1, 0, 0, 131
    PHASE 2
    GATE 2
    MS $s5, 4096, 4, 0, 132
    MS $s5, 8192, 4, -12, 133
    li.d $s4, 0xfffffffffffff000
    MS $s4, 8192, 0, -12, 134
    li.d $s4, 0x48001000
    MS $s4, 4096, 0, -12, 135
    li.d $a0, 0x4a000000
    li.d $a1, 4096
    li.d $a2, 3
    li.d $a3, 0x12
    move $a4, $s0
    li.d $a5, 0
    SC 222
    EXPECT 0x4a000000, 136
    move $s4, $a0
    li.d $t0, 0xaf
    st.b $t0, $s4, 29
    MS $s4, 4096, 4, 0, 137
    move $a0, $s0
    la.local $a1, .Lbuffer
    li.d $a2, 1
    li.d $a3, 29
    SC 67
    EXPECT 1, 138
    la.local $t0, .Lbuffer
    ld.bu $a0, $t0, 0
    EXPECT 0x66, 139
    move $a0, $s0
    la.local $a1, .Lbuffer
    li.d $a2, 1
    li.d $a3, 4125
    SC 67
    EXPECT 1, 140
    la.local $t0, .Lbuffer
    ld.bu $a0, $t0, 0
    EXPECT 0x79, 141
    move $a0, $s0
    SC 82
    EXPECT 0, 142
    move $a0, $s0
    SC 83
    EXPECT 0, 143
    li.d $a0, -1
    SC 82
    EXPECT -9, 144
    li.d $a0, -1
    SC 83
    EXPECT -9, 145
    OPEN .Ldirectory, 0, 146
    move $s1, $a0
    SC 82
    EXPECT 0, 147
    move $a0, $s1
    SC 83
    EXPECT 0, 148
    b .Lpass

# /// 真实 45/46 和 O_TRUNC 共享大小，dup 的位置不变；只读截断打开仍是只读 OFD。
.Ltruncate:
    OPEN .Lshared_path, 2, 150
    move $s0, $a0
    li.d $a1, 0
    SC 46
    EXPECT 0, 151
    move $a0, $s0
    la.local $a1, .Ltruncate_input
    li.d $a2, 9
    SC 64
    EXPECT 9, 152
    move $a0, $s0
    li.d $a1, 123
    li.d $a2, 0
    SC 62
    EXPECT 123, 153
    move $a0, $s0
    SC 23
    OK 154
    move $s2, $a0
    move $a0, $s0
    li.d $a1, 3
    SC 46
    EXPECT 0, 155
    SIZE $s0, 3, 156
    CURSOR $s0, 123, 157
    CURSOR $s2, 123, 158
    move $a0, $s0
    li.d $a1, 9
    SC 46
    EXPECT 0, 159
    SIZE $s0, 9, 160
    move $a0, $s0
    la.local $a1, .Lbuffer
    li.d $a2, 9
    li.d $a3, 0
    SC 67
    EXPECT 9, 161
    la.local $a0, .Lbuffer
    la.local $a1, .Ltruncate_expected
    li.d $a2, 9
    li.d $s8, 162
    bl .Lequal
    la.local $a0, .Lshared_path
    li.d $a1, 0
    SC 45
    EXPECT 0, 163
    SIZE $s0, 0, 164
    CURSOR $s2, 123, 165
    la.local $a0, .Lshared_path
    li.d $a1, 9
    SC 45
    EXPECT 0, 166
    SIZE $s0, 9, 167
    move $a0, $s0
    la.local $a1, .Lbuffer
    li.d $a2, 9
    li.d $a3, 0
    SC 67
    EXPECT 9, 168
    la.local $a0, .Lbuffer
    li.d $a1, 9
    li.d $a2, 0
    li.d $s8, 169
    bl .Lconstant
    li.d $a0, -1
    li.d $a1, -1
    SC 46
    EXPECT -22, 170
    li.d $a0, -1
    li.d $a1, 0
    SC 46
    EXPECT -9, 171
    li.d $a0, 0
    li.d $a1, -1
    SC 45
    EXPECT -22, 172
    li.d $a0, 0
    li.d $a1, 0
    SC 45
    EXPECT -14, 173
    move $a0, $s0
    li.d $a1, -1
    SC 46
    EXPECT -22, 174
    move $a0, $s0
    li.d $a1, 0x7fffffffffffffff
    SC 46
    EXPECT -27, 175
    la.local $a0, .Lshared_path
    li.d $a1, 0x7fffffffffffffff
    SC 45
    EXPECT -27, 176
    SIZE $s0, 9, 177
    CURSOR $s0, 123, 178
    OPEN .Lshared_path, 0, 179
    move $s1, $a0
    li.d $a1, 0
    SC 46
    EXPECT -22, 180
    OPEN .Ldirectory, 0, 181
    move $s3, $a0
    li.d $a1, 0
    SC 46
    EXPECT -22, 182
    la.local $a0, .Ldirectory
    li.d $a1, 0
    SC 45
    EXPECT -21, 183
    la.local $a0, .Lself
    li.d $a1, 0
    SC 45
    EXPECT -26, 184
    li.d $a0, -100
    la.local $a1, .Lself
    li.d $a2, 0x200
    li.d $a3, 0
    SC 56
    EXPECT -26, 185
    OPEN .Lshared_path, 0x200, 186
    move $s4, $a0
    li.d $a1, 3
    li.d $a2, 0
    SC 25
    EXPECT 0, 187
    SIZE $s4, 0, 188
    SIZE $s0, 0, 189
    CURSOR $s2, 123, 190
    move $a0, $s4
    la.local $a1, .Lpayload
    li.d $a2, 1
    SC 64
    EXPECT -9, 191
    move $a0, $s4
    li.d $a1, 0
    SC 46
    EXPECT -22, 192
    move $a0, $s0
    la.local $a1, .Lpayload
    li.d $a2, 1
    li.d $a3, 0
    SC 68
    EXPECT 1, 193
    OPEN .Lshared_path, 0x202, 194
    move $s5, $a0
    SIZE $s5, 0, 195
    CURSOR $s0, 123, 196
    b .Lpass

# /// 只读挂载保 EROFS，负长度先于 fd 查找；只读 OFD 的 ftruncate 仍按类型返回 EINVAL。
.Ltruncate_ro:
    OPEN .Lmap_path, 0, 197
    move $s0, $a0
    CURSOR $s0, 0, 222
    move $a0, $s0
    li.d $a1, 0
    SC 46
    EXPECT -22, 198
    la.local $a0, .Lmap_path
    li.d $a1, 0
    SC 45
    EXPECT -30, 199
    li.d $a0, -100
    la.local $a1, .Lmap_path
    li.d $a2, 0x200
    li.d $a3, 0
    SC 56
    EXPECT -30, 200
    li.d $a0, -100
    la.local $a1, .Lmap_path
    li.d $a2, 0x202
    li.d $a3, 0
    SC 56
    EXPECT -30, 201
    CURSOR $s0, 0, 223
    b .Lpass

# /// 当前非 root 凭据须有写权限才能按路径截断，失败不能改变真实大小或文件字节。
.Ltruncate_dac:
    OPEN .Lshared_path, 0, 202
    move $s0, $a0
    SIZE $s0, 9, 203
    la.local $a0, .Lshared_path
    li.d $a1, 0
    SC 45
    EXPECT -13, 204
    li.d $a0, -100
    la.local $a1, .Lshared_path
    li.d $a2, 0x200
    li.d $a3, 0
    SC 56
    EXPECT -13, 205
    li.d $a0, -100
    la.local $a1, .Lshared_path
    li.d $a2, 0x202
    li.d $a3, 0
    SC 56
    EXPECT -13, 206
    SIZE $s0, 9, 207
    move $a0, $s0
    la.local $a1, .Lbuffer
    li.d $a2, 9
    li.d $a3, 0
    SC 67
    EXPECT 9, 208
    la.local $a0, .Lbuffer
    la.local $a1, .Ltruncate_input
    li.d $a2, 9
    li.d $s8, 209
    bl .Lequal
    b .Lpass

# /// 新 private 写 fault 先准备真实匿名候选，首失效后完成 store 和普通 fork，子与父等最终失效后才退出。
.Ltruncate_cow:
    li.d $s6, 0x49000000
    OPEN .Lshared_path, 0, 210
    move $s0, $a0
    li.d $a0, 0x48000000
    li.d $a1, 4096
    li.d $a2, 3
    li.d $a3, 0x12
    move $a4, $s0
    ld.d $a5, $s6, 24
    SC 222
    EXPECT 0x48000000, 211
    move $s5, $a0
    PHASE 1
    GATE 1
    li.d $t0, 0xb6
    st.b $t0, $s5, 0
    ld.bu $a0, $s5, 0
    EXPECT 0xb6, 212
    PHASE 2
    GATE 2
    li.d $a0, 17
    li.d $a1, 0
    li.d $a2, 0
    li.d $a3, 0
    li.d $a4, 0
    SC 220
    OK 213
    beqz $a0, .Ltruncate_cow_child
    move $s1, $a0
    st.d $s1, $s6, 16
    PHASE 3
    GATE 3
    SIZE $s0, 4096, 214
    move $a0, $s1
    la.local $a1, .Lstatus
    li.d $a2, 0
    li.d $a3, 0
    SC 260
    li.d $s8, 215
    bne $a0, $s1, .Lexit
    la.local $t0, .Lstatus
    ld.w $a0, $t0, 0
    EXPECT 0, 216
    b .Lpass
.Ltruncate_cow_child:
    li.d $t0, 3
    dbar 0
    st.w $t0, $s6, 8
    GATE 3
    b .Lpass

# /// 普通 ftruncate 在原数据或元数据完成暂停时真实等待，返回后大小为零且 OFD 位置保持。
.Ltruncate_wait:
    li.d $s6, 0x49000000
    OPEN .Lshared_path, 2, 217
    move $s0, $a0
    li.d $a1, 7
    li.d $a2, 0
    SC 62
    EXPECT 7, 218
    PHASE 1
    GATE 1
    move $a0, $s0
    li.d $a1, 0
    SC 46
    EXPECT 0, 219
    SIZE $s0, 0, 220
    CURSOR $s0, 7, 221
    PHASE 2
    GATE 2
    b .Lpass

# /// 真实用户创建对象后停在匿名控制页，控制者只接收inode并同步盘面。
.Lnamespace_create:
    li.d $s6, 0x49000000
    li.d $a0, 0x1017
    SC 166
    EXPECT 0x12, 300
    move $s2, $a0

    li.d $a0, -100
    la.local $a1, .Lns_mode
    li.d $a2, 0xc2
    li.d $a3, 0xfff
    SC 56
    OK 301
    move $s0, $a0
    move $a0, $s0
    SC 82
    EXPECT 0, 303
    move $a0, $s0
    SC 57
    EXPECT 0, 304
    li.d $a0, -100
    la.local $a1, .Lns_data
    li.d $a2, 0xc2
    li.d $a3, 0x1b6
    SC 56
    OK 645
    move $s0, $a0
    move $a0, $s0
    la.local $a1, .Lns_payload_mode
    li.d $a2, 4
    SC 64
    EXPECT 4, 646
    move $a0, $s0
    SC 82
    EXPECT 0, 647
    move $a0, $s0
    SC 57
    EXPECT 0, 648

    li.d $s3, 12
    li.d $a0, 0x311
    la.local $a1, .Lns_stack_a
    li.d $t0, 16384
    add.d $a1, $a1, $t0
    li.d $a2, 0
    li.d $a3, 0
    li.d $a4, 0
    SC 220
    OK 305
    beqz $a0, .Lns_race_child
    move $s4, $a0
    li.d $s3, 16
    li.d $a0, 0x311
    la.local $a1, .Lns_stack_b
    li.d $t0, 16384
    add.d $a1, $a1, $t0
    li.d $a2, 0
    li.d $a3, 0
    li.d $a4, 0
    SC 220
    OK 306
    beqz $a0, .Lns_race_child
    move $s5, $a0
    li.d $t0, 1
    dbar 0
    st.w $t0, $s6, 8
    move $a0, $s4
    la.local $a1, .Lstatus
    li.d $a2, 0
    li.d $a3, 0
    SC 260
    li.d $s8, 307
    bne $a0, $s4, .Lexit
    la.local $t0, .Lstatus
    ld.w $a0, $t0, 0
    EXPECT 0, 308
    move $a0, $s5
    la.local $a1, .Lstatus
    li.d $a2, 0
    li.d $a3, 0
    SC 260
    li.d $s8, 309
    bne $a0, $s5, .Lexit
    la.local $t0, .Lstatus
    ld.w $a0, $t0, 0
    EXPECT 0, 310
    ld.w $t0, $s6, 12
    ld.w $t1, $s6, 16
    add.d $a0, $t0, $t1
    EXPECT 1, 311
    OPEN .Lns_race, 0, 638
    move $s0, $a0
    NSSTAT $s0, 639
    la.local $t0, .Lstat
    ld.d $t1, $t0, 8
    move $s4, $t1
    OPEN .Lns_race, 0, 640
    move $s1, $a0
    NSSTAT $s1, 641
    la.local $t0, .Lstat
    ld.d $t0, $t0, 8
    li.d $s8, 642
    beq $t0, $s4, 991f
    b .Lexit
991:
    move $a0, $s1
    SC 57
    EXPECT 0, 643
    move $a0, $s0
    SC 57
    EXPECT 0, 644
    OPEN .Lns_race, 2, 312
    move $s0, $a0
    move $a0, $s0
    la.local $a1, .Lns_payload_race
    li.d $a2, 4
    SC 64
    EXPECT 4, 313
    move $a0, $s0
    SC 82
    EXPECT 0, 314
    move $a0, $s0
    SC 57
    EXPECT 0, 315

    li.d $a0, 0x311
    la.local $a1, .Lns_stack_a
    li.d $t0, 16384
    add.d $a1, $a1, $t0
    li.d $a2, 0
    li.d $a3, 0
    li.d $a4, 0
    SC 220
    OK 316
    beqz $a0, .Lns_shared_umask_child
    move $s4, $a0
    move $a0, $s4
    la.local $a1, .Lstatus
    li.d $a2, 0
    li.d $a3, 0
    SC 260
    li.d $s8, 317
    bne $a0, $s4, .Lexit
    la.local $t0, .Lstatus
    ld.w $a0, $t0, 0
    EXPECT 0, 318
    li.d $a0, 0
    SC 166
    EXPECT 7, 319
    li.d $a0, 0x17
    SC 166
    EXPECT 0, 320

    li.d $a0, 0x111
    la.local $a1, .Lns_stack_a
    li.d $t0, 16384
    add.d $a1, $a1, $t0
    li.d $a2, 0
    li.d $a3, 0
    li.d $a4, 0
    SC 220
    OK 321
    beqz $a0, .Lns_copy_umask_child
    move $s4, $a0
    move $a0, $s4
    la.local $a1, .Lstatus
    li.d $a2, 0
    li.d $a3, 0
    SC 260
    li.d $s8, 322
    bne $a0, $s4, .Lexit
    la.local $t0, .Lstatus
    ld.w $a0, $t0, 0
    EXPECT 0, 323
    li.d $a0, 0
    SC 166
    EXPECT 0x17, 324
    li.d $a0, 0x17
    SC 166
    EXPECT 0, 325

    li.d $a0, -100
    la.local $a1, .Lns_ro
    li.d $a2, 0x40
    li.d $a3, 0x1b6
    SC 56
    OK 326
    move $s0, $a0
    NSSTAT $s0, 327
    la.local $t0, .Lstat
    ld.wu $a0, $t0, 16
    andi $a0, $a0, 0xfff
    EXPECT 0x1a0, 328
    move $a0, $s0
    la.local $a1, .Lns_payload_mode
    li.d $a2, 1
    SC 64
    EXPECT -9, 329
    move $a0, $s0
    SC 82
    EXPECT 0, 330
    move $a0, $s0
    SC 57
    EXPECT 0, 331

    li.d $a0, -100
    la.local $a1, .Lns_mode
    li.d $a2, 0x40
    li.d $a3, 0x180
    SC 56
    OK 332
    move $s0, $a0
    NSSTAT $s0, 333
    la.local $t0, .Lstat
    ld.wu $a0, $t0, 16
    andi $a0, $a0, 0xfff
    EXPECT 0xfe8, 334
    move $a0, $s0
    la.local $a1, .Lns_payload_race
    li.d $a2, 1
    SC 64
    EXPECT -9, 335
    move $a0, $s0
    SC 57
    EXPECT 0, 336
    li.d $a0, -100
    la.local $a1, .Lns_mode
    li.d $a2, 0xc2
    li.d $a3, 0x1b6
    SC 56
    EXPECT -17, 337

    li.d $a0, -100
    la.local $a1, .Lns_dir
    li.d $a2, 0x3ff
    SC 34
    EXPECT 0, 338
    OPEN .Lns_dir, 0x10000, 339
    move $s0, $a0
    NSSTAT $s0, 340
    la.local $t0, .Lstat
    ld.wu $a0, $t0, 20
    EXPECT 2, 341
    la.local $t0, .Lstat
    ld.wu $a0, $t0, 16
    andi $a0, $a0, 0xfff
    EXPECT 0x7e8, 342
    move $a0, $s0
    la.local $a1, .Lbuffer
    li.d $a2, 128
    SC 61
    li.d $t0, 48
    bgeu $a0, $t0, 991f
    b .Lexit
991:
    # 前面的子进程均已等待并回收，s3 原存放的控制偏移不再存活；用它保留目录项地址。
    la.local $s3, .Lbuffer
    ld.bu $a0, $s3, 19
    EXPECT '.', 645
    ld.bu $a0, $s3, 20
    EXPECT 0, 646
    ld.hu $a0, $s3, 16
    EXPECT 24, 647
    ld.bu $a0, $s3, 18
    EXPECT 4, 648
    ld.hu $t1, $s3, 16
    add.d $s3, $s3, $t1
    ld.bu $a0, $s3, 19
    EXPECT '.', 649
    ld.bu $a0, $s3, 20
    EXPECT '.', 650
    ld.bu $a0, $s3, 21
    EXPECT 0, 651

    move $a0, $s0
    SC 82
    EXPECT 0, 343
    move $a0, $s0
    SC 57
    EXPECT 0, 344

    la.local $a0, .Lns_data
    li.d $a1, -100
    la.local $a2, .Lns_short
    SC 36
    EXPECT 0, 345
    la.local $a0, .Lns_long_target
    li.d $a1, -100
    la.local $a2, .Lns_long
    SC 36
    EXPECT 0, 346
    la.local $a0, .Lns_missing
    li.d $a1, -100
    la.local $a2, .Lns_dangling
    SC 36
    EXPECT 0, 347

    li.d $a0, -100
    la.local $a1, .Lns_raw
    li.d $a2, 0xc2
    li.d $a3, 0x180
    SC 56
    OK 348
    move $s0, $a0
    move $a0, $s0
    la.local $a1, .Lns_payload_raw
    li.d $a2, 4
    SC 64
    EXPECT 4, 349
    move $a0, $s0
    SC 82
    EXPECT 0, 350
    move $a0, $s0
    SC 57
    EXPECT 0, 351

    li.d $a0, -100
    la.local $a1, .Lns_missing_parent
    li.d $a2, 0x42
    li.d $a3, 0x180
    SC 56
    EXPECT -2, 352
    li.d $a0, -100
    la.local $a1, .Lns_trailing
    li.d $a2, 0x20000
    li.d $a3, 0
    SC 56
    EXPECT -20, 353
    li.d $a0, -100
    la.local $a1, .Lns_rooted
    li.d $a2, 0
    li.d $a3, 0
    SC 56
    OK 354
    move $s0, $a0
    move $a0, $s0
    la.local $a1, .Lbuffer
    li.d $a2, 4
    SC 63
    EXPECT 4, 355
    la.local $a0, .Lbuffer
    la.local $a1, .Lns_payload_mode
    li.d $a2, 4
    li.d $s8, 356
    bl .Lequal
    move $a0, $s0
    SC 57
    EXPECT 0, 357

    li.d $a0, -100
    la.local $a1, .Lns_permission_denied
    li.d $a2, 0xc2
    li.d $a3, 0x180
    SC 56
    EXPECT -13, 358

    li.d $s0, 0
.Lns_fill_fds:
    li.d $a0, -100
    la.local $a1, .Lns_mode
    li.d $a2, 0
    li.d $a3, 0
    SC 56
    li.d $t0, -24
    beq $a0, $t0, .Lns_fds_full
    blt $a0, $zero, .Lexit
    li.d $t1, 512
    bgeu $s0, $t1, .Lexit
    slli.d $t1, $s0, 3
    la.local $t2, .Lns_fd_table
    add.d $t1, $t1, $t2
    st.d $a0, $t1, 0
    addi.d $s0, $s0, 1
    b .Lns_fill_fds
.Lns_fds_full:
    beqz $s0, .Lexit
    li.d $a0, -100
    la.local $a1, .Lns_missing
    li.d $a2, 0xc2
    li.d $a3, 0x180
    SC 56
    EXPECT -24, 359
    li.d $s1, 0
.Lns_close_fds:
    bgeu $s1, $s0, .Lns_fds_closed
    slli.d $t1, $s1, 3
    la.local $t2, .Lns_fd_table
    add.d $t1, $t1, $t2
    ld.d $a0, $t1, 0
    SC 57
    EXPECT 0, 360
    addi.d $s1, $s1, 1
    b .Lns_close_fds
.Lns_fds_closed:
    li.d $a0, -100
    la.local $a1, .Lns_missing
    li.d $a2, 0
    li.d $a3, 0
    SC 56
    EXPECT -2, 361

    OPEN .Lns_parent, 0x10000, 362
    move $s0, $a0
    move $a0, $s0
    SC 82
    EXPECT 0, 363
    move $a0, $s0
    SC 57
    EXPECT 0, 364
    PHASE 1
    GATE 1

    li.d $a0, -100
    la.local $a1, .Lns_dangling
    li.d $a2, 0xc2
    li.d $a3, 0x180
    SC 56
    EXPECT -17, 365
    li.d $a0, -100
    la.local $a1, .Lns_dangling
    li.d $a2, 0x20000
    li.d $a3, 0
    SC 56
    EXPECT -40, 366
    li.d $a0, -100
    la.local $a1, .Lns_dangling
    li.d $a2, 0x42
    li.d $a3, 0x180
    SC 56
    OK 367
    move $s0, $a0
    move $a0, $s0
    la.local $a1, .Lns_payload_dangling
    li.d $a2, 4
    SC 64
    EXPECT 4, 368
    move $a0, $s0
    SC 82
    EXPECT 0, 369
    move $a0, $s0
    SC 57
    EXPECT 0, 370

    OPEN .Lns_short, 0, 371
    move $s0, $a0
    move $a0, $s0
    la.local $a1, .Lbuffer
    li.d $a2, 4
    SC 63
    EXPECT 4, 372
    la.local $a0, .Lbuffer
    la.local $a1, .Lns_payload_mode
    li.d $a2, 4
    li.d $s8, 373
    bl .Lequal
    move $a0, $s0
    SC 57
    EXPECT 0, 374
    OPEN .Lns_long, 0, 375
    move $s0, $a0
    move $a0, $s0
    la.local $a1, .Lbuffer
    li.d $a2, 4
    SC 63
    EXPECT 4, 376
    la.local $a0, .Lbuffer
    la.local $a1, .Lns_payload_mode
    li.d $a2, 4
    li.d $s8, 377
    bl .Lequal
    move $a0, $s0
    SC 57
    EXPECT 0, 378
    OPEN .Lns_dangling, 0, 379
    move $s0, $a0
    move $a0, $s0
    la.local $a1, .Lbuffer
    li.d $a2, 4
    SC 63
    EXPECT 4, 380
    la.local $a0, .Lbuffer
    la.local $a1, .Lns_payload_dangling
    li.d $a2, 4
    li.d $s8, 381
    bl .Lequal
    move $a0, $s0
    SC 57
    EXPECT 0, 382

    li.d $a0, -100
    la.local $a1, .Lns_sgid_child
    li.d $a2, 0xc2
    li.d $a3, 0xfff
    SC 56
    OK 383
    move $s0, $a0
    NSSTAT $s0, 384
    la.local $t0, .Lstat
    ld.wu $a0, $t0, 28
    EXPECT 1001, 385
    la.local $t0, .Lstat
    ld.wu $a0, $t0, 16
    andi $a0, $a0, 0xfff
    EXPECT 0xfe8, 386
    move $a0, $s0
    la.local $a1, .Lns_payload_alias
    li.d $a2, 4
    SC 64
    EXPECT 4, 387
    move $a0, $s0
    SC 82
    EXPECT 0, 388
    move $a0, $s0
    SC 57
    EXPECT 0, 389

    OPEN .Lns_parent, 0x10000, 390
    move $s0, $a0
    move $a0, $s0
    SC 82
    EXPECT 0, 391
    move $a0, $s0
    SC 57
    EXPECT 0, 392
    PHASE 2
    GATE 2
    li.d $a0, 0x12
    SC 166
    EXPECT 0x17, 393

    li.d $a0, -100
    la.local $a1, .Lns_race
    li.d $a2, 0
    SC 35
    EXPECT 0, 394
    li.d $a0, -100
    la.local $a1, .Lns_mode
    li.d $a2, 0
    SC 35
    EXPECT 0, 395
    li.d $a0, -100
    la.local $a1, .Lns_data
    li.d $a2, 0
    SC 35
    EXPECT 0, 649
    li.d $a0, -100
    la.local $a1, .Lns_ro
    li.d $a2, 0
    SC 35
    EXPECT 0, 396
    li.d $a0, -100
    la.local $a1, .Lns_shared
    li.d $a2, 0
    SC 35
    EXPECT 0, 397
    li.d $a0, -100
    la.local $a1, .Lns_copy
    li.d $a2, 0
    SC 35
    EXPECT 0, 398
    li.d $a0, -100
    la.local $a1, .Lns_raw
    li.d $a2, 0
    SC 35
    EXPECT 0, 399
    li.d $a0, -100
    la.local $a1, .Lns_missing
    li.d $a2, 0
    SC 35
    EXPECT 0, 400
    li.d $a0, -100
    la.local $a1, .Lns_sgid_child
    li.d $a2, 0
    SC 35
    EXPECT 0, 401
    li.d $a0, -100
    la.local $a1, .Lns_short
    li.d $a2, 0
    SC 35
    EXPECT 0, 402
    li.d $a0, -100
    la.local $a1, .Lns_long
    li.d $a2, 0
    SC 35
    EXPECT 0, 403
    li.d $a0, -100
    la.local $a1, .Lns_dangling
    li.d $a2, 0
    SC 35
    EXPECT 0, 404
    li.d $a0, -100
    la.local $a1, .Lns_dir
    li.d $a2, 0x200
    SC 35
    EXPECT 0, 405
    OPEN .Lns_parent, 0x10000, 406
    move $s0, $a0
    move $a0, $s0
    SC 82
    EXPECT 0, 407
    move $a0, $s0
    SC 57
    EXPECT 0, 408
    b .Lpass
.Lns_race_child:
    li.d $t0, 0
.Lns_race_gate:
    ld.w $t0, $s6, 8
    beqz $t0, .Lns_race_gate
    dbar 0
    li.d $a0, -100
    la.local $a1, .Lns_race
    li.d $a2, 0xc2
    li.d $a3, 0x180
    SC 56
    bge $a0, $zero, .Lns_race_won
    li.d $t1, -17
    bne $a0, $t1, .Lexit
    li.d $t0, 0
    b .Lns_race_record
.Lns_race_won:
    move $s0, $a0
    li.d $t0, 1
.Lns_race_record:
    add.d $t1, $s6, $s3
    st.w $t0, $t1, 0
    beqz $t0, .Lns_race_exit
    move $a0, $s0
    SC 57
    EXPECT 0, 409
.Lns_race_exit:
    li.d $a0, 0
    SC 93
    b .Lexit
.Lns_shared_umask_child:
    li.d $a0, 7
    SC 166
    EXPECT 0x17, 410
    li.d $a0, -100
    la.local $a1, .Lns_shared
    li.d $a2, 0xc2
    li.d $a3, 0x1b6
    SC 56
    OK 411
    move $s0, $a0
    NSSTAT $s0, 412
    la.local $t0, .Lstat
    ld.wu $a0, $t0, 16
    andi $a0, $a0, 0xfff
    EXPECT 0x1b0, 413
    move $a0, $s0
    la.local $a1, .Lns_payload_shared
    li.d $a2, 4
    SC 64
    EXPECT 4, 414
    move $a0, $s0
    SC 82
    EXPECT 0, 415
    move $a0, $s0
    SC 57
    EXPECT 0, 416
    li.d $a0, 0
    SC 93
    b .Lexit
.Lns_copy_umask_child:
    li.d $a0, 0x3f
    SC 166
    EXPECT 0x17, 417
    li.d $a0, -100
    la.local $a1, .Lns_copy
    li.d $a2, 0xc2
    li.d $a3, 0x1b6
    SC 56
    OK 418
    move $s0, $a0
    NSSTAT $s0, 419
    la.local $t0, .Lstat
    ld.wu $a0, $t0, 16
    andi $a0, $a0, 0xfff
    EXPECT 0x180, 420
    move $a0, $s0
    la.local $a1, .Lns_payload_copy
    li.d $a2, 4
    SC 64
    EXPECT 4, 421
    move $a0, $s0
    SC 82
    EXPECT 0, 422
    move $a0, $s0
    SC 57
    EXPECT 0, 423
    li.d $a0, 0
    SC 93
    b .Lexit

# /// 同一 inode 的旧 fd、共享页与私有页跨 unlink 和同名新建仍保持原内容。
.Lnamespace_unlink:
    li.d $s6, 0x49000000
    li.d $a0, -100
    la.local $a1, .Lns_old
    li.d $a2, 0xc2
    li.d $a3, 0x180
    SC 56
    OK 430
    move $s0, $a0
    move $a0, $s0
    la.local $a1, .Lns_payload_old
    li.d $a2, 4
    SC 64
    EXPECT 4, 431
    move $a0, $s0
    SC 82
    EXPECT 0, 432
    li.d $a0, -100
    la.local $a1, .Lns_old
    li.d $a2, -100
    la.local $a3, .Lns_alias
    li.d $a4, 0
    SC 37
    EXPECT 0, 433
    NSSTAT $s0, 434
    la.local $t0, .Lstat
    ld.d $t1, $t0, 8
    move $s4, $t1
    ld.wu $a0, $t0, 20
    EXPECT 2, 435
    OPEN .Lns_alias, 0, 436
    move $s1, $a0
    NSSTAT $s1, 437
    la.local $t0, .Lstat
    ld.d $t1, $t0, 8
    li.d $s8, 438
    beq $t1, $s4, 991f
    b .Lexit
991:
    move $a0, $s1
    SC 57
    EXPECT 0, 439

    li.d $a0, 0x4b000000
    li.d $a1, 4096
    li.d $a2, 1
    li.d $a3, 1
    move $a4, $s0
    li.d $a5, 0
    SC 222
    EXPECT 0x4b000000, 440
    move $s2, $a0
    li.d $a0, 0x4c000000
    li.d $a1, 4096
    li.d $a2, 1
    li.d $a3, 2
    move $a4, $s0
    li.d $a5, 0
    SC 222
    EXPECT 0x4c000000, 441
    move $s3, $a0
    move $a0, $s2
    la.local $a1, .Lns_payload_old
    li.d $a2, 4
    li.d $s8, 442
    bl .Lequal
    move $a0, $s3
    la.local $a1, .Lns_payload_old
    li.d $a2, 4
    li.d $s8, 443
    bl .Lequal
    OPEN .Lns_parent, 0x10000, 645
    move $s1, $a0
    move $a0, $s1
    SC 82
    EXPECT 0, 646
    move $a0, $s1
    SC 57
    EXPECT 0, 647
    PHASE 1
    GATE 1

    li.d $a0, -100
    la.local $a1, .Lns_old
    li.d $a2, 0
    SC 35
    EXPECT 0, 444
    li.d $a0, -100
    la.local $a1, .Lns_alias
    li.d $a2, 0
    SC 35
    EXPECT 0, 445
    NSSTAT $s0, 446
    la.local $t0, .Lstat
    ld.wu $a0, $t0, 20
    EXPECT 0, 447
    la.local $t0, .Lstat
    ld.d $a0, $t0, 48
    EXPECT 4, 448
    li.d $a0, -100
    la.local $a1, .Lns_old
    li.d $a2, 0xc2
    li.d $a3, 0x180
    SC 56
    OK 449
    move $s5, $a0
    move $a0, $s5
    la.local $a1, .Lns_payload_new
    li.d $a2, 4
    SC 64
    EXPECT 4, 450
    move $a0, $s5
    SC 82
    EXPECT 0, 451
    NSSTAT $s5, 452
    la.local $t0, .Lstat
    ld.d $t1, $t0, 8
    li.d $s8, 453
    bne $t1, $s4, 991f
    b .Lexit
991:
    ld.wu $a0, $t0, 20
    EXPECT 1, 454
    PHASE 2
    GATE 2

    move $a0, $s0
    li.d $a1, 0
    li.d $a2, 0
    SC 62
    EXPECT 0, 455
    move $a0, $s0
    la.local $a1, .Lbuffer
    li.d $a2, 4
    SC 63
    EXPECT 4, 456
    la.local $a0, .Lbuffer
    la.local $a1, .Lns_payload_old
    li.d $a2, 4
    li.d $s8, 457
    bl .Lequal
    move $a0, $s2
    la.local $a1, .Lns_payload_old
    li.d $a2, 4
    li.d $s8, 458
    bl .Lequal
    move $a0, $s3
    la.local $a1, .Lns_payload_old
    li.d $a2, 4
    li.d $s8, 459
    bl .Lequal
    NSSTAT $s0, 460
    la.local $t0, .Lstat
    ld.wu $a0, $t0, 20
    EXPECT 0, 461
    move $a0, $s0
    SC 57
    EXPECT 0, 462
    move $a0, $s2
    li.d $a1, 4096
    SC 215
    EXPECT 0, 463
    move $a0, $s3
    li.d $a1, 4096
    SC 215
    EXPECT 0, 464
    PHASE 3
    GATE 3

    move $a0, $s5
    SC 82
    EXPECT 0, 465
    li.d $a0, -100
    la.local $a1, .Lns_old
    li.d $a2, 0
    SC 35
    EXPECT 0, 466
    OPEN .Lns_parent, 0x10000, 467
    move $s0, $a0
    move $a0, $s0
    SC 82
    EXPECT 0, 468
    move $a0, $s0
    SC 57
    EXPECT 0, 469
    move $a0, $s5
    SC 57
    EXPECT 0, 470
    b .Lpass

# /// renameat2 覆盖真实目标项后，旧 descriptor 与映射仍指向原 inode。
.Lnamespace_rename:
    li.d $s6, 0x49000000
    li.d $a0, -100
    la.local $a1, .Lns_source
    li.d $a2, 0xc2
    li.d $a3, 0x180
    SC 56
    OK 480
    move $s1, $a0
    move $a0, $s1
    la.local $a1, .Lns_payload_source
    li.d $a2, 4
    SC 64
    EXPECT 4, 481
    move $a0, $s1
    SC 82
    EXPECT 0, 482
    move $a0, $s1
    SC 57
    EXPECT 0, 483
    li.d $a0, -100
    la.local $a1, .Lns_target
    li.d $a2, 0xc2
    li.d $a3, 0x180
    SC 56
    OK 484
    move $s0, $a0
    move $a0, $s0
    la.local $a1, .Lns_payload_target
    li.d $a2, 4
    SC 64
    EXPECT 4, 485
    move $a0, $s0
    SC 82
    EXPECT 0, 486
    NSSTAT $s0, 487
    la.local $t0, .Lstat
    ld.wu $a0, $t0, 20
    EXPECT 1, 488

    li.d $a0, -100
    la.local $a1, .Lns_plain_source
    li.d $a2, 0xc2
    li.d $a3, 0x180
    SC 56
    OK 489
    move $s1, $a0
    move $a0, $s1
    la.local $a1, .Lns_payload_plain
    li.d $a2, 5
    SC 64
    EXPECT 5, 490
    move $a0, $s1
    SC 82
    EXPECT 0, 491
    move $a0, $s1
    SC 57
    EXPECT 0, 492
    li.d $a0, -100
    la.local $a1, .Lns_noreplace_source
    li.d $a2, 0xc2
    li.d $a3, 0x180
    SC 56
    OK 493
    move $s1, $a0
    move $a0, $s1
    la.local $a1, .Lns_payload_copy
    li.d $a2, 4
    SC 64
    EXPECT 4, 494
    move $a0, $s1
    SC 82
    EXPECT 0, 495
    move $a0, $s1
    SC 57
    EXPECT 0, 496

    li.d $a0, 0x4b000000
    li.d $a1, 4096
    li.d $a2, 1
    li.d $a3, 1
    move $a4, $s0
    li.d $a5, 0
    SC 222
    EXPECT 0x4b000000, 497
    move $s2, $a0
    li.d $a0, 0x4c000000
    li.d $a1, 4096
    li.d $a2, 1
    li.d $a3, 2
    move $a4, $s0
    li.d $a5, 0
    SC 222
    EXPECT 0x4c000000, 498
    move $s3, $a0
    OPEN .Lns_parent, 0x10000, 545
    move $s1, $a0
    move $a0, $s1
    SC 82
    EXPECT 0, 546
    move $a0, $s1
    SC 57
    EXPECT 0, 547
    PHASE 1
    GATE 1

    li.d $a0, -100
    la.local $a1, .Lns_source
    li.d $a2, -100
    la.local $a3, .Lns_target
    li.d $a4, 0
    SC 276
    EXPECT 0, 499
    OPEN .Lns_target, 0, 500
    move $s4, $a0
    move $a0, $s4
    la.local $a1, .Lbuffer
    li.d $a2, 4
    SC 63
    EXPECT 4, 501
    la.local $a0, .Lbuffer
    la.local $a1, .Lns_payload_source
    li.d $a2, 4
    li.d $s8, 502
    bl .Lequal
    NSSTAT $s0, 503
    la.local $t0, .Lstat
    ld.wu $a0, $t0, 20
    EXPECT 0, 504
    NSSTAT $s4, 505
    la.local $t0, .Lstat
    ld.wu $a0, $t0, 20
    EXPECT 1, 506
    OPEN .Lns_plain_source, 0, 507
    move $s5, $a0
    li.d $a0, -100
    la.local $a1, .Lns_plain_source
    li.d $a2, -100
    la.local $a3, .Lns_plain_target
    li.d $a4, 0
    SC 276
    EXPECT 0, 508
    li.d $a0, -100
    la.local $a1, .Lns_noreplace_source
    li.d $a2, -100
    la.local $a3, .Lns_plain_target
    li.d $a4, 1
    SC 276
    EXPECT -17, 509
    OPEN .Lns_plain_target, 0, 510
    move $s1, $a0
    move $a0, $s1
    la.local $a1, .Lbuffer
    li.d $a2, 5
    SC 63
    EXPECT 5, 511
    la.local $a0, .Lbuffer
    la.local $a1, .Lns_payload_plain
    li.d $a2, 5
    li.d $s8, 512
    bl .Lequal
    move $a0, $s1
    SC 57
    EXPECT 0, 542
    OPEN .Lns_noreplace_source, 0, 513
    move $s1, $a0
    move $a0, $s1
    la.local $a1, .Lbuffer
    li.d $a2, 4
    SC 63
    EXPECT 4, 514
    la.local $a0, .Lbuffer
    la.local $a1, .Lns_payload_copy
    li.d $a2, 4
    li.d $s8, 515
    bl .Lequal
    move $a0, $s1
    SC 57
    EXPECT 0, 516
    li.d $a0, -100
    la.local $a1, .Lns_noreplace_source
    li.d $a2, -100
    la.local $a3, .Lns_noreplace_target
    li.d $a4, 1
    SC 276
    EXPECT 0, 517
    OPEN .Lns_noreplace_target, 0, 518
    move $s1, $a0
    move $a0, $s1
    la.local $a1, .Lbuffer
    li.d $a2, 4
    SC 63
    EXPECT 4, 519
    la.local $a0, .Lbuffer
    la.local $a1, .Lns_payload_copy
    li.d $a2, 4
    li.d $s8, 520
    bl .Lequal
    move $a0, $s1
    SC 57
    EXPECT 0, 521
    OPEN .Lns_parent, 0x10000, 522
    move $s1, $a0
    move $a0, $s1
    SC 82
    EXPECT 0, 523
    move $a0, $s1
    SC 57
    EXPECT 0, 524
    PHASE 2
    GATE 2

    move $a0, $s0
    li.d $a1, 0
    li.d $a2, 0
    SC 62
    EXPECT 0, 525
    move $a0, $s0
    la.local $a1, .Lbuffer
    li.d $a2, 4
    SC 63
    EXPECT 4, 526
    la.local $a0, .Lbuffer
    la.local $a1, .Lns_payload_target
    li.d $a2, 4
    li.d $s8, 527
    bl .Lequal
    move $a0, $s2
    la.local $a1, .Lns_payload_target
    li.d $a2, 4
    li.d $s8, 528
    bl .Lequal
    move $a0, $s3
    la.local $a1, .Lns_payload_target
    li.d $a2, 4
    li.d $s8, 529
    bl .Lequal
    move $a0, $s0
    SC 57
    EXPECT 0, 530
    move $a0, $s2
    li.d $a1, 4096
    SC 215
    EXPECT 0, 531
    move $a0, $s3
    li.d $a1, 4096
    SC 215
    EXPECT 0, 532
    move $a0, $s4
    SC 57
    EXPECT 0, 534
    move $a0, $s5
    SC 57
    EXPECT 0, 535
    li.d $a0, -100
    la.local $a1, .Lns_target
    li.d $a2, 0
    SC 35
    EXPECT 0, 536
    li.d $a0, -100
    la.local $a1, .Lns_plain_target
    li.d $a2, 0
    SC 35
    EXPECT 0, 537
    li.d $a0, -100
    la.local $a1, .Lns_noreplace_target
    li.d $a2, 0
    SC 35
    EXPECT 0, 538
    OPEN .Lns_parent, 0x10000, 539
    move $s1, $a0
    move $a0, $s1
    SC 82
    EXPECT 0, 540
    move $a0, $s1
    SC 57
    EXPECT 0, 541
    b .Lpass

# /// 同 inode 别名、目录拓扑与反向父目录改名由真实普通用户 syscall 消费。
.Lnamespace_directories:
    li.d $s6, 0x49000000
    li.d $a0, -100
    la.local $a1, .Lns_alias_source
    li.d $a2, 0xc2
    li.d $a3, 0x180
    SC 56
    OK 550
    move $s0, $a0
    move $a0, $s0
    la.local $a1, .Lns_payload_alias
    li.d $a2, 4
    SC 64
    EXPECT 4, 551
    move $a0, $s0
    SC 82
    EXPECT 0, 552
    move $a0, $s0
    SC 57
    EXPECT 0, 553
    li.d $a0, -100
    la.local $a1, .Lns_alias_source
    li.d $a2, -100
    la.local $a3, .Lns_alias_target
    li.d $a4, 0
    SC 37
    EXPECT 0, 554
    li.d $a0, -100
    la.local $a1, .Lns_cross_a
    li.d $a2, 0xc2
    li.d $a3, 0x180
    SC 56
    OK 660
    move $s0, $a0
    move $a0, $s0
    la.local $a1, .Lns_payload_cross_a
    li.d $a2, 4
    SC 64
    EXPECT 4, 661
    move $a0, $s0
    SC 82
    EXPECT 0, 662
    la.local $t0, .Lns_cross_fds
    st.d $s0, $t0, 0
    li.d $a0, -100
    la.local $a1, .Lns_cross_b
    li.d $a2, 0xc2
    li.d $a3, 0x180
    SC 56
    OK 663
    move $s1, $a0
    move $a0, $s1
    la.local $a1, .Lns_payload_cross_b
    li.d $a2, 4
    SC 64
    EXPECT 4, 664
    move $a0, $s1
    SC 82
    EXPECT 0, 665
    la.local $t0, .Lns_cross_fds
    st.d $s1, $t0, 8
    li.d $a0, -100
    la.local $a1, .Lns_d1
    li.d $a2, 0x1ff
    SC 34
    EXPECT 0, 666
    li.d $a0, -100
    la.local $a1, .Lns_d2
    li.d $a2, 0x1ff
    SC 34
    EXPECT 0, 667
    li.d $a0, -100
    la.local $a1, .Lns_cross_a
    li.d $a2, -100
    la.local $a3, .Lns_d1_a
    li.d $a4, 0
    SC 37
    EXPECT 0, 668
    li.d $a0, -100
    la.local $a1, .Lns_cross_a
    li.d $a2, -100
    la.local $a3, .Lns_d2_a
    li.d $a4, 0
    SC 37
    EXPECT 0, 669
    li.d $a0, -100
    la.local $a1, .Lns_cross_b
    li.d $a2, -100
    la.local $a3, .Lns_d1_b
    li.d $a4, 0
    SC 37
    EXPECT 0, 670
    li.d $a0, -100
    la.local $a1, .Lns_cross_b
    li.d $a2, -100
    la.local $a3, .Lns_d2_b
    li.d $a4, 0
    SC 37
    EXPECT 0, 671
    NSSTAT $s0, 672
    la.local $t0, .Lstat
    ld.wu $a0, $t0, 20
    EXPECT 3, 673
    NSSTAT $s1, 674
    la.local $t0, .Lstat
    ld.wu $a0, $t0, 20
    EXPECT 3, 675
    OPEN .Lns_d1, 0x10000, 676
    move $s0, $a0
    move $a0, $s0
    SC 82
    EXPECT 0, 677
    move $a0, $s0
    SC 57
    EXPECT 0, 678
    OPEN .Lns_d2, 0x10000, 679
    move $s0, $a0
    move $a0, $s0
    SC 82
    EXPECT 0, 680
    move $a0, $s0
    SC 57
    EXPECT 0, 681

    li.d $a0, -100
    la.local $a1, .Lns_a
    li.d $a2, 0x1ff
    SC 34
    EXPECT 0, 555
    li.d $a0, -100
    la.local $a1, .Lns_b
    li.d $a2, 0x1ff
    SC 34
    EXPECT 0, 556
    li.d $a0, -100
    la.local $a1, .Lns_ax
    li.d $a2, 0x1ff
    SC 34
    EXPECT 0, 557
    li.d $a0, -100
    la.local $a1, .Lns_a2
    li.d $a2, 0x1ff
    SC 34
    EXPECT 0, 558
    li.d $a0, -100
    la.local $a1, .Lns_by
    li.d $a2, 0x1ff
    SC 34
    EXPECT 0, 559
    li.d $a0, -100
    la.local $a1, .Lns_cwd
    li.d $a2, 0x1ff
    SC 34
    EXPECT 0, 560
    OPEN .Lns_a, 0x10000, 561
    move $s0, $a0
    NSSTAT $s0, 562
    la.local $t0, .Lstat
    ld.wu $a0, $t0, 20
    EXPECT 4, 563
    move $a0, $s0
    SC 82
    EXPECT 0, 564
    move $a0, $s0
    SC 57
    EXPECT 0, 565
    OPEN .Lns_b, 0x10000, 566
    move $s0, $a0
    NSSTAT $s0, 567
    la.local $t0, .Lstat
    ld.wu $a0, $t0, 20
    EXPECT 3, 568
    move $a0, $s0
    SC 82
    EXPECT 0, 569
    move $a0, $s0
    SC 57
    EXPECT 0, 570
    OPEN .Lns_parent, 0x10000, 571
    move $s0, $a0
    move $a0, $s0
    SC 82
    EXPECT 0, 572
    move $a0, $s0
    SC 57
    EXPECT 0, 573
    PHASE 1
    GATE 1
    li.d $a0, -100
    la.local $a1, .Lns_cross_a
    li.d $a2, 0
    SC 35
    EXPECT 0, 680
    li.d $a0, -100
    la.local $a1, .Lns_cross_b
    li.d $a2, 0
    SC 35
    EXPECT 0, 681
    OPEN .Lns_parent, 0x10000, 682
    move $s0, $a0
    move $a0, $s0
    SC 82
    EXPECT 0, 683
    move $a0, $s0
    SC 57
    EXPECT 0, 684


    li.d $a0, -100
    la.local $a1, .Lns_alias_source
    li.d $a2, -100
    la.local $a3, .Lns_alias_target
    li.d $a4, 0
    SC 276
    EXPECT 0, 574
    li.d $a0, -100
    la.local $a1, .Lns_missing
    li.d $a2, -100
    la.local $a3, .Lns_missing
    li.d $a4, 0
    SC 276
    EXPECT -2, 575
    li.d $a0, -100
    la.local $a1, .Lns_a
    li.d $a2, -100
    la.local $a3, .Lns_ax
    li.d $a4, 0
    SC 276
    EXPECT -22, 716
    li.d $a0, -100
    la.local $a1, .Lns_a
    li.d $a2, -100
    la.local $a3, .Lns_descendant
    li.d $a4, 0
    SC 276
    EXPECT -22, 576
    li.d $a0, -100
    la.local $a1, .Lns_ax
    li.d $a2, -100
    la.local $a3, .Lns_bx
    li.d $a4, 0
    SC 276
    EXPECT 0, 577
    OPEN .Lns_a, 0x10000, 578
    move $s0, $a0
    NSSTAT $s0, 579
    la.local $t0, .Lstat
    ld.wu $a0, $t0, 20
    EXPECT 3, 580
    move $a0, $s0
    SC 57
    EXPECT 0, 581
    OPEN .Lns_b, 0x10000, 582
    move $s0, $a0
    NSSTAT $s0, 583
    la.local $t0, .Lstat
    ld.wu $a0, $t0, 20
    EXPECT 4, 584
    la.local $t0, .Lstat
    ld.d $a0, $t0, 8
    move $s3, $a0
    move $a0, $s0
    SC 57
    EXPECT 0, 585
    OPEN .Lns_bx_parent, 0x10000, 586
    move $s1, $a0
    NSSTAT $s1, 587
    la.local $t0, .Lstat
    ld.d $a0, $t0, 8
    li.d $s8, 588
    beq $a0, $s3, 991f
    b .Lexit
991:
    move $a0, $s1
    SC 57
    EXPECT 0, 635

    la.local $a0, .Lns_cwd
    SC 49
    EXPECT 0, 589
    li.d $a0, -100
    la.local $a1, .Lns_cwd
    li.d $a2, 0x200
    SC 35
    EXPECT 0, 590
    li.d $a0, -100
    la.local $a1, .Lns_child
    li.d $a2, 0x1ff
    SC 34
    EXPECT -2, 591
    la.local $a0, .Lns_parent
    SC 49
    EXPECT 0, 592
    li.d $a0, -100
    la.local $a1, .Lns_alias_source
    li.d $a2, -100
    la.local $a3, .Lns_alias_target
    li.d $a4, 2
    SC 276
    EXPECT -22, 593
    li.d $a0, -100
    la.local $a1, .Lns_alias_source
    li.d $a2, -100
    la.local $a3, .Lns_alias_target
    li.d $a4, 4
    SC 276
    EXPECT -22, 594
    li.d $a0, -100
    la.local $a1, .Lns_a
    li.d $a2, 0x201
    SC 35
    EXPECT -22, 595

    li.d $s3, 12
    li.d $a0, 0x311
    la.local $a1, .Lns_stack_a
    li.d $t0, 16384
    add.d $a1, $a1, $t0
    li.d $a2, 0
    li.d $a3, 0
    li.d $a4, 0
    SC 220
    OK 596
    beqz $a0, .Lns_move_a2_child
    move $s4, $a0
    li.d $s3, 16
    li.d $a0, 0x311
    la.local $a1, .Lns_stack_b
    li.d $t0, 16384
    add.d $a1, $a1, $t0
    li.d $a2, 0
    li.d $a3, 0
    li.d $a4, 0
    SC 220
    OK 597
    beqz $a0, .Lns_move_y_child
    move $s5, $a0
    li.d $t0, 1
    dbar 0
    st.w $t0, $s6, 8
    move $a0, $s4
    la.local $a1, .Lstatus
    li.d $a2, 0
    li.d $a3, 0
    SC 260
    li.d $s8, 598
    bne $a0, $s4, .Lexit
    la.local $t0, .Lstatus
    ld.w $a0, $t0, 0
    EXPECT 0, 599
    move $a0, $s5
    la.local $a1, .Lstatus
    li.d $a2, 0
    li.d $a3, 0
    SC 260
    li.d $s8, 600
    bne $a0, $s5, .Lexit
    la.local $t0, .Lstatus
    ld.w $a0, $t0, 0
    EXPECT 0, 601
    ld.w $a0, $s6, 12
    EXPECT 0, 602
    ld.w $a0, $s6, 16
    EXPECT 0, 603
    la.local $t0, .Lns_cross_fds
    ld.d $s0, $t0, 0
    ld.d $s1, $t0, 8
    NSSTAT $s0, 687
    la.local $t0, .Lstat
    ld.wu $a0, $t0, 20
    EXPECT 1, 688
    la.local $t0, .Lstat
    ld.d $s4, $t0, 8
    OPEN .Lns_d1_b, 0, 689
    move $s5, $a0
    NSSTAT $s5, 690
    la.local $t0, .Lstat
    ld.d $a0, $t0, 8
    li.d $s8, 691
    beq $a0, $s4, 991f
    b .Lexit
991:
    move $a0, $s5
    la.local $a1, .Lbuffer
    li.d $a2, 4
    SC 63
    EXPECT 4, 692
    la.local $a0, .Lbuffer
    la.local $a1, .Lns_payload_cross_a
    li.d $a2, 4
    li.d $s8, 693
    bl .Lequal
    move $a0, $s5
    SC 57
    EXPECT 0, 694
    NSSTAT $s1, 695
    la.local $t0, .Lstat
    ld.wu $a0, $t0, 20
    EXPECT 1, 696
    la.local $t0, .Lstat
    ld.d $s4, $t0, 8
    OPEN .Lns_d2_a, 0, 697
    move $s5, $a0
    NSSTAT $s5, 698
    la.local $t0, .Lstat
    ld.d $a0, $t0, 8
    li.d $s8, 699
    beq $a0, $s4, 991f
    b .Lexit
991:
    move $a0, $s5
    la.local $a1, .Lbuffer
    li.d $a2, 4
    SC 63
    EXPECT 4, 700
    la.local $a0, .Lbuffer
    la.local $a1, .Lns_payload_cross_b
    li.d $a2, 4
    li.d $s8, 701
    bl .Lequal
    move $a0, $s5
    SC 57
    EXPECT 0, 702
    move $a0, $s0
    li.d $a1, 0
    li.d $a2, 0
    SC 62
    EXPECT 0, 703
    move $a0, $s0
    la.local $a1, .Lbuffer
    li.d $a2, 4
    SC 63
    EXPECT 4, 704
    la.local $a0, .Lbuffer
    la.local $a1, .Lns_payload_cross_a
    li.d $a2, 4
    li.d $s8, 705
    bl .Lequal
    move $a0, $s1
    li.d $a1, 0
    li.d $a2, 0
    SC 62
    EXPECT 0, 706
    move $a0, $s1
    la.local $a1, .Lbuffer
    li.d $a2, 4
    SC 63
    EXPECT 4, 707
    la.local $a0, .Lbuffer
    la.local $a1, .Lns_payload_cross_b
    li.d $a2, 4
    li.d $s8, 708
    bl .Lequal


    OPEN .Lns_a, 0x10000, 604
    move $s0, $a0
    NSSTAT $s0, 605
    la.local $t0, .Lstat
    ld.wu $a0, $t0, 20
    EXPECT 3, 606
    la.local $t0, .Lstat
    ld.d $a0, $t0, 8
    move $s2, $a0
    move $a0, $s0
    SC 82
    EXPECT 0, 607
    move $a0, $s0
    SC 57
    EXPECT 0, 608
    OPEN .Lns_b, 0x10000, 609
    move $s0, $a0
    NSSTAT $s0, 610
    la.local $t0, .Lstat
    ld.wu $a0, $t0, 20
    EXPECT 4, 611
    la.local $t0, .Lstat
    ld.d $a0, $t0, 8
    move $s3, $a0
    move $a0, $s0
    SC 82
    EXPECT 0, 612
    move $a0, $s0
    SC 57
    EXPECT 0, 613
    OPEN .Lns_bx_parent, 0x10000, 614
    move $s1, $a0
    NSSTAT $s1, 615
    la.local $t0, .Lstat
    ld.d $a0, $t0, 8
    li.d $s8, 616
    beq $a0, $s3, 991f
    b .Lexit
991:
    move $a0, $s1
    SC 57
    EXPECT 0, 636
    OPEN .Lns_ay_parent, 0x10000, 617
    move $s1, $a0
    NSSTAT $s1, 618
    la.local $t0, .Lstat
    ld.d $a0, $t0, 8
    li.d $s8, 619
    beq $a0, $s2, 991f
    b .Lexit
991:
    move $a0, $s1
    SC 57
    EXPECT 0, 637
    OPEN .Lns_parent, 0x10000, 620
    move $s0, $a0
    move $a0, $s0
    SC 82
    EXPECT 0, 621
    move $a0, $s0
    SC 57
    EXPECT 0, 622
    PHASE 2
    GATE 2
    la.local $t0, .Lns_cross_fds
    ld.d $a0, $t0, 0
    SC 57
    EXPECT 0, 709
    la.local $t0, .Lns_cross_fds
    ld.d $a0, $t0, 8
    SC 57
    EXPECT 0, 710


    li.d $a0, -100
    la.local $a1, .Lns_alias_source
    li.d $a2, 0
    SC 35
    EXPECT 0, 623
    li.d $a0, -100
    la.local $a1, .Lns_alias_target
    li.d $a2, 0
    SC 35
    EXPECT 0, 624
    li.d $a0, -100
    la.local $a1, .Lns_d1_b
    li.d $a2, 0
    SC 35
    EXPECT 0, 638
    li.d $a0, -100
    la.local $a1, .Lns_d2_a
    li.d $a2, 0
    SC 35
    EXPECT 0, 639
    li.d $a0, -100
    la.local $a1, .Lns_bx
    li.d $a2, 0x200
    SC 35
    EXPECT 0, 625
    li.d $a0, -100
    la.local $a1, .Lns_ba2
    li.d $a2, 0x200
    SC 35
    EXPECT 0, 626
    li.d $a0, -100
    la.local $a1, .Lns_ay
    li.d $a2, 0x200
    SC 35
    EXPECT 0, 627
    li.d $a0, -100
    la.local $a1, .Lns_a
    li.d $a2, 0x200
    SC 35
    EXPECT 0, 628
    li.d $a0, -100
    la.local $a1, .Lns_b
    li.d $a2, 0x200
    SC 35
    EXPECT 0, 629
    li.d $a0, -100
    la.local $a1, .Lns_d1
    li.d $a2, 0x200
    SC 35
    EXPECT 0, 640
    li.d $a0, -100
    la.local $a1, .Lns_d2
    li.d $a2, 0x200
    SC 35
    EXPECT 0, 641
    OPEN .Lns_parent, 0x10000, 630
    move $s0, $a0
    move $a0, $s0
    SC 82
    EXPECT 0, 631
    move $a0, $s0
    SC 57
    EXPECT 0, 632
    b .Lpass
.Lns_move_a2_child:
    ld.w $t0, $s6, 8
    beqz $t0, .Lns_move_a2_child
    dbar 0
    li.d $a0, -100
    la.local $a1, .Lns_d1_a
    li.d $a2, -100
    la.local $a3, .Lns_d1_b
    li.d $a4, 0
    SC 276
    EXPECT 0, 664
    li.d $a0, -100
    la.local $a1, .Lns_a2
    li.d $a2, -100
    la.local $a3, .Lns_ba2
    li.d $a4, 0
    SC 276
    EXPECT 0, 633
    li.d $t0, 0
    add.d $t1, $s6, $s3
    st.w $t0, $t1, 0
    li.d $a0, 0
    SC 93
    b .Lexit
.Lns_move_y_child:
    ld.w $t0, $s6, 8
    beqz $t0, .Lns_move_y_child
    dbar 0
    li.d $a0, -100
    la.local $a1, .Lns_d2_b
    li.d $a2, -100
    la.local $a3, .Lns_d2_a
    li.d $a4, 0
    SC 276
    EXPECT 0, 665
    li.d $a0, -100
    la.local $a1, .Lns_by
    li.d $a2, -100
    la.local $a3, .Lns_ay
    li.d $a4, 0
    SC 276
    EXPECT 0, 634
    li.d $t0, 0
    add.d $t1, $s6, $s3
    st.w $t0, $t1, 0
    li.d $a0, 0
    SC 93
    b .Lexit

# /// 核对整段常量字节，错误保留调用方的具名断言编号。
.type .Lconstant, @function
.Lconstant:
    beqz $a1, .Lconstant_done
.Lconstant_next:
    ld.bu $t0, $a0, 0
    bne $t0, $a2, .Lexit
    addi.d $a0, $a0, 1
    addi.d $a1, $a1, -1
    bnez $a1, .Lconstant_next
.Lconstant_done:
    jirl $zero, $ra, 0
# /// 同文件源图样按实际原始逻辑偏移核完整字节。
.type .Lpattern, @function
.Lpattern:
    ld.bu $t0, $a0, 0
    andi $t1, $a2, 255
    bne $t0, $t1, .Lexit
    addi.d $a0, $a0, 1
    addi.d $a1, $a1, -1
    addi.d $a2, $a2, 1
    bnez $a1, .Lpattern
    jirl $zero, $ra, 0
# /// 比较独立常量输入，append 的顺序和任何重叠都会导致普通非零退出。
.type .Lequal, @function
.Lequal:
    ld.bu $t0, $a0, 0
    ld.bu $t1, $a1, 0
    bne $t0, $t1, .Lexit
    addi.d $a0, $a0, 1
    addi.d $a1, $a1, 1
    addi.d $a2, $a2, -1
    bnez $a2, .Lequal
    jirl $zero, $ra, 0
# /// 正常结束沿 exit_group，不以共享字或内核回声替代结果。
.Lpass:
    li.d $s8, 0
.Lexit:
    move $a0, $s8
    SC 94
    b .Lexit
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
