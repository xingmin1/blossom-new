.text

# /// 普通目标 syscall 约定，返回值和具体 errno 均由 a0 消费。
.macro SC number
    li.d $a7, \number
    syscall 0
.endm
# /// 每个断言点保留独立退出码，条件分支只跨一条普通跳转。
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
# /// openat 只消费本程序实际字节路径，不创建或截断文件。
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
# /// argv 仅选择具体写、append 或真实 exec 交接，不使用测试 syscall。
.global _start
.type _start, @function
_start:
    li.d $s8, 1
    ld.d $t0, $sp, 0
    li.d $t1, 2
    bltu $t0, $t1, .Lexit
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
