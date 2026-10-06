.text
.option norelax
.option norvc

# /// 使用目标 raw syscall 约定，只有 a0 承载返回值。
.macro SC number
    li a7, \number
    ecall
.endm

# /// 每个断言点使用独立退出编号，保留原始返回值供后续字段检查。
# 条件分支只跳过一条 JAL，完整程序增长不受 4KiB 分支距离限制。
.macro EXPECT value, number
    li s11, \number
    li t0, \value
    beq a0, t0, 991f
    j .Lexit
991:
.endm
.macro CHECK actual, value, number
    li s11, \number
    li t0, \value
    beq \actual, t0, 991f
    j .Lexit
991:
.endm
.macro CHECK_REG actual, expected, number
    li s11, \number
    beq \actual, \expected, 991f
    j .Lexit
991:
.endm
.macro POS number
    li s11, \number
    blt zero, a0, 991f
    j .Lexit
991:
.endm
# /// 文件描述符允许 fd0；仅负返回值视为失败。
.macro FD number
    li s11, \number
    bge a0, zero, 991f
    j .Lexit
991:
.endm
.macro PATTERN number
    li s11, \number
    call .Lpattern
.endm
.macro ZERO number
    li s11, \number
    call .Lzero
.endm
.macro EQUAL number
    li s11, \number
    call .Lequal
.endm
.macro STAT number
    li s11, \number
    call .Lstat
.endm
.macro DIRENTS number
    li s11, \number
    call .Ldirents
.endm

# /// 目录读DAC或组件search失败必须保留原始EACCES，不使用内核权限探针。
.macro DENIED dirfd, pathname, number
    mv a0, \dirfd
    lla a1, \pathname
    li a2, 0
    li a3, 0
    SC 56
    EXPECT -13, \number
.endm

# /// argv[1]=access只选择非root访问断言，其余参数保持普通文件与内存ABI流程。
.global _start
.type _start, @function
_start:
    ld t0, 0(sp)
    li t1, 2
    bltu t0, t1, .Lraw
    ld a0, 16(sp)
    lla a1, .Laccess_mode
    li a2, 7
.Lselect_mode:
    lbu t0, 0(a0)
    lbu t1, 0(a1)
    bne t0, t1, .Lraw
    addi a0, a0, 1
    addi a1, a1, 1
    addi a2, a2, -1
    bnez a2, .Lselect_mode
    j .Laccess
.Lraw:
    # 文件图样为 byte[i] = i & 255，长度 12293。
    li a0, -100
    lla a1, .Ldata_path
    li a2, 0x80000
    li a3, 0
    SC 56
    FD 1
    mv s0, a0

    # 两页映射只保留第一页，跨坏页 read 必须只提交 17 字节。
    li a0, 0
    li a1, 8192
    li a2, 3
    li a3, 0x22
    li a4, -1
    li a5, 0
    SC 222
    POS 2
    mv s4, a0
    li t0, 4096
    add a0, s4, t0
    li a1, 4096
    SC 215
    EXPECT 0, 3
    mv a0, s0
    li t0, 4079
    add a1, s4, t0
    li a2, 64
    SC 63
    EXPECT 17, 4
    li t0, 4079
    add a0, s4, t0
    li a1, 17
    li a2, 0
    PATTERN 5
    mv a0, s0
    li a1, 0
    li a2, 1
    SC 62
    EXPECT 17, 6
    mv a0, s0
    lla a1, .Lbuffer
    li a2, 300
    SC 63
    EXPECT 300, 7
    lla a0, .Lbuffer
    li a1, 300
    li a2, 17
    PATTERN 8
    mv a0, s0
    lla a1, .Lbuffer
    li a2, 513
    li a3, 4093
    SC 67
    EXPECT 513, 9
    lla a0, .Lbuffer
    li a1, 513
    li a2, 4093
    PATTERN 10
    mv a0, s0
    li a1, 0
    li a2, 1
    SC 62
    EXPECT 317, 11
    mv a0, s0
    lla a1, .Lbuffer
    li a2, 16
    li a3, 12290
    SC 67
    EXPECT 3, 178
    lla a0, .Lbuffer
    li a1, 3
    li a2, 12290
    PATTERN 179
    mv a0, s0
    lla a1, .Lbuffer
    li a2, 1
    li a3, 12293
    SC 67
    EXPECT 0, 180
    mv a0, s0
    li a1, 0
    li a2, 1
    SC 62
    EXPECT 317, 181

    # dup 共享 cursor，但不继承 fd CLOEXEC；另一次 open 的 cursor 独立。
    mv a0, s0
    SC 23
    FD 12
    mv s1, a0
    mv a0, s1
    lla a1, .Lbuffer
    li a2, 37
    SC 63
    EXPECT 37, 13
    lla a0, .Lbuffer
    li a1, 37
    li a2, 317
    PATTERN 14
    mv a0, s0
    li a1, 0
    li a2, 1
    SC 62
    EXPECT 354, 15
    li a0, -100
    lla a1, .Ldata_path
    li a2, 0
    li a3, 0
    SC 56
    FD 16
    mv s2, a0
    mv a0, s2
    lla a1, .Lbuffer
    li a2, 29
    SC 63
    EXPECT 29, 17
    lla a0, .Lbuffer
    li a1, 29
    li a2, 0
    PATTERN 18
    mv a0, s0
    li a1, 1
    SC 25
    EXPECT 1, 19
    mv a0, s1
    li a1, 1
    SC 25
    EXPECT 0, 20
    mv a0, s1
    li a1, 2
    li a2, 1
    SC 25
    EXPECT 0, 21
    mv a0, s0
    li a1, 1
    SC 25
    EXPECT 1, 22
    mv a0, s1
    li a1, 2
    li a2, 0
    SC 25
    EXPECT 0, 23
    mv a0, s1
    li a1, 1
    SC 25
    EXPECT 0, 24
    mv a0, s0
    li a1, 4
    li a2, 0xc00
    SC 25
    EXPECT 0, 25
    mv a0, s1
    li a1, 3
    SC 25
    li t0, 0xc03
    and a0, a0, t0
    EXPECT 0xc00, 26
    mv a0, s2
    li a1, 3
    SC 25
    li t0, 0xc03
    and a0, a0, t0
    EXPECT 0, 27
    mv a0, s0
    li a1, 3
    SC 25
    li t0, 0x80000
    and a0, a0, t0
    EXPECT 0, 28
    mv a0, s0
    li a1, 0
    li a2, 40
    SC 25
    EXPECT 40, 29
    li a0, 40
    li a1, 1
    SC 25
    EXPECT 0, 30
    li a0, 40
    li a1, 0
    li a2, 1
    SC 62
    EXPECT 354, 31
    li a0, 40
    SC 57
    EXPECT 0, 32
    mv a0, s0
    li a1, 1030
    li a2, 40
    SC 25
    EXPECT 40, 33
    li a0, 40
    li a1, 1
    SC 25
    EXPECT 1, 34
    li a0, 40
    SC 57
    EXPECT 0, 35
    mv a0, s0
    li a1, 41
    li a2, 0x80000
    SC 24
    EXPECT 41, 36
    li a0, 41
    li a1, 1
    SC 25
    EXPECT 1, 37
    li a0, 41
    SC 57
    EXPECT 0, 38
    mv a0, s0
    mv a1, s0
    li a2, 0
    SC 24
    EXPECT -22, 39

    # 只读 fd 的写与写意图 open 有不同 errno；坏 fd 不访问用户指针。
    mv a0, s0
    lla a1, .Lbuffer
    li a2, 1
    SC 64
    EXPECT -9, 40
    li a0, -1
    li a1, 0
    li a2, 1
    SC 63
    EXPECT -9, 41
    li a0, -100
    lla a1, .Ldata_path
    li a2, 1
    li a3, 0
    SC 56
    EXPECT -30, 42
    li a0, -100
    lla a1, .Ldata_path
    li a2, 2
    li a3, 0
    SC 56
    EXPECT -30, 43
    li a0, -100
    lla a1, .Ldata_path
    li a2, 0x240
    li a3, 0
    SC 56
    EXPECT -30, 44
    mv a0, s0
    li a1, 0
    li a2, 0
    SC 63
    EXPECT 0, 45
    mv a0, s0
    li a1, 0
    li a2, 1
    SC 63
    EXPECT -14, 46
    mv a0, s0
    li a1, 0
    li a2, 1
    SC 62
    EXPECT 354, 47

    # stat128 按字段取值，独立 statat 与 fstat 的整份快照必须一致。
    mv a0, s0
    lla a1, .Lstat_data
    SC 80
    EXPECT 0, 48
    lla a0, .Lstat_data
    li a1, 0x8000
    li a2, 12293
    STAT 49
    li a0, -999
    lla a1, .Ldata_path
    lla a2, .Lstat_temp
    li a3, 0
    SC 79
    EXPECT 0, 50
    lla a0, .Lstat_data
    lla a1, .Lstat_temp
    li a2, 128
    EQUAL 51
    mv a0, s0
    li t0, 4032
    add a1, s4, t0
    SC 80
    EXPECT -14, 52

    # dirfd 搜索普通目录；绝对路径忽略坏 dirfd，普通文件不能穿越。
    li a0, -100
    lla a1, .Ldir_path
    li a2, 0x10000
    li a3, 0
    SC 56
    FD 53
    mv s3, a0
    mv a0, s3
    lla a1, .Lstat_dir
    SC 80
    EXPECT 0, 54
    lla a0, .Lstat_dir
    li a1, 0x4000
    li a2, -1
    STAT 55
    mv a0, s3
    lla a1, .Lfirst_name
    lla a2, .Lstat_first
    li a3, 0
    SC 79
    EXPECT 0, 56
    lla a0, .Lstat_first
    li a1, 0x8000
    li a2, 11
    STAT 57
    mv a0, s3
    lla a1, .Lsecond_name
    lla a2, .Lstat_second
    li a3, 0
    SC 79
    EXPECT 0, 58
    lla a0, .Lstat_second
    li a1, 0x8000
    li a2, 12
    STAT 59
    mv a0, s3
    lla a1, .Llink_name
    lla a2, .Lstat_link
    li a3, 0x100
    SC 79
    EXPECT 0, 60
    lla a0, .Lstat_link
    li a1, 0xa000
    li a2, 5
    STAT 61
    mv a0, s3
    lla a1, .Llink_name
    lla a2, .Lstat_temp
    li a3, 0
    SC 79
    EXPECT 0, 62
    lla a0, .Lstat_first
    lla a1, .Lstat_temp
    li a2, 128
    EQUAL 63
    mv a0, s3
    lla a1, .Lfirst_name
    li a2, 0
    li a3, 0
    SC 56
    FD 64
    mv s6, a0
    mv a0, s6
    lla a1, .Lbuffer
    li a2, 32
    SC 63
    EXPECT 11, 65
    lla a0, .Lbuffer
    lla a1, .Lfirst_contents
    li a2, 11
    EQUAL 66
    mv a0, s6
    SC 57
    EXPECT 0, 67
    li a0, -999
    lla a1, .Ldata_path
    li a2, 0
    li a3, 0
    SC 56
    FD 68
    mv s6, a0
    mv a0, s6
    SC 57
    EXPECT 0, 69
    li a0, -999
    lla a1, .Lfirst_name
    li a2, 0
    li a3, 0
    SC 56
    EXPECT -9, 70
    mv a0, s0
    lla a1, .Lfirst_name
    li a2, 0
    li a3, 0
    SC 56
    EXPECT -20, 71
    mv a0, s3
    lla a1, .Lnot_directory
    li a2, 0
    li a3, 0
    SC 56
    EXPECT -20, 72
    li a0, -100
    lla a1, .Ldata_path
    li a2, 0x10000
    li a3, 0
    SC 56
    EXPECT -20, 73
    mv a0, s3
    lla a1, .Llink_name
    li a2, 0x20000
    li a3, 0
    SC 56
    EXPECT -40, 74
    lla a0, .Ldata_path
    SC 49
    EXPECT -20, 75
    lla a0, .Ldir_path
    SC 49
    EXPECT 0, 76
    li a0, -100
    lla a1, .Ldot_first
    li a2, 0
    li a3, 0
    SC 56
    FD 77
    mv s6, a0
    mv a0, s6
    lla a1, .Lstat_temp
    SC 80
    EXPECT 0, 78
    lla a0, .Lstat_first
    lla a1, .Lstat_temp
    li a2, 128
    EQUAL 79
    mv a0, s6
    SC 57
    EXPECT 0, 80
    lla a0, .Lparent_name
    SC 49
    EXPECT 0, 81
    li a0, -100
    lla a1, .Lrelative_data
    li a2, 0
    li a3, 0
    SC 56
    FD 82
    mv s6, a0
    mv a0, s6
    lla a1, .Lstat_temp
    SC 80
    EXPECT 0, 83
    lla a0, .Lstat_data
    lla a1, .Lstat_temp
    li a2, 128
    EQUAL 84
    mv a0, s6
    SC 57
    EXPECT 0, 85
    lla a0, .Lroot_path
    SC 49
    EXPECT 0, 86

    # readlink 不写 NUL：先填哨兵，短读和全读后分别核对尾字节。
    lla t0, .Lbuffer
    li t1, 0x5a
    sb t1, 3(t0)
    sb t1, 5(t0)
    mv a0, s3
    lla a1, .Llink_name
    lla a2, .Lbuffer
    li a3, 3
    SC 78
    EXPECT 3, 87
    lla a0, .Lbuffer
    lla a1, .Lfirst_name
    li a2, 3
    EQUAL 88
    lla t0, .Lbuffer
    lbu t1, 3(t0)
    CHECK t1, 0x5a, 89
    mv a0, s3
    lla a1, .Llink_name
    lla a2, .Lbuffer
    li a3, 16
    SC 78
    EXPECT 5, 90
    lla a0, .Lbuffer
    lla a1, .Lfirst_name
    li a2, 5
    EQUAL 91
    lla t0, .Lbuffer
    lbu t1, 5(t0)
    CHECK t1, 0x5a, 92
    mv a0, s3
    lla a1, .Lfirst_name
    lla a2, .Lbuffer
    li a3, 16
    SC 78
    EXPECT -22, 93

    # 首轮完整解析目录，记录真实首项长度/cookie，后续不假定盘面顺序。
    li s5, 0
    mv a0, s3
    lla a1, .Lbuffer
    li a2, 4096
    SC 61
    POS 94
    lla t0, .Lbuffer
    lhu s7, 16(t0)
    ld t1, 8(t0)
    lla t2, .Lfirst_cookie
    sd t1, 0(t2)
    mv a1, a0
    lla a0, .Lbuffer
    DIRENTS 95
    CHECK s5, 31, 96
    mv a0, s3
    lla a1, .Lbuffer
    li a2, 4096
    SC 61
    EXPECT 0, 97
    mv a0, s3
    li a1, 0
    li a2, 0
    SC 62
    EXPECT 0, 98
    mv a0, s3
    lla a1, .Lbuffer
    addi a2, s7, -1
    SC 61
    EXPECT -22, 99
    mv a0, s3
    li a1, 0
    li a2, 1
    SC 62
    EXPECT 0, 100
    # 起点使真实首项刚好结束在有效页末；下一项 copyout 无法完整交付。
    li t0, 4096
    add s6, s4, t0
    sub s6, s6, s7
    mv a0, s3
    mv a1, s6
    li a2, 4096
    SC 61
    CHECK_REG a0, s7, 101
    li s5, 0
    mv a0, s6
    mv a1, s7
    DIRENTS 102
    mv a0, s3
    li a1, 0
    li a2, 1
    SC 62
    lla t0, .Lfirst_cookie
    ld t1, 0(t0)
    CHECK_REG a0, t1, 103
    lla t0, .Llast_cookie
    ld t1, 0(t0)
    CHECK_REG a0, t1, 104
    mv a0, s3
    li t0, 4096
    add a1, s4, t0
    li a2, 4096
    SC 61
    EXPECT -14, 105
    mv a0, s3
    li a1, 0
    li a2, 1
    SC 62
    lla t0, .Lfirst_cookie
    ld t1, 0(t0)
    CHECK_REG a0, t1, 106
    mv a0, s3
    lla a1, .Lbuffer
    li a2, 4096
    SC 61
    POS 107
    mv a1, a0
    lla a0, .Lbuffer
    DIRENTS 108
    CHECK s5, 31, 109
    mv a0, s3
    li a1, 0
    li a2, 1
    SC 62
    lla t0, .Llast_cookie
    ld t1, 0(t0)
    CHECK_REG a0, t1, 110
    mv a0, s3
    lla a1, .Lbuffer
    li a2, 4096
    SC 61
    EXPECT 0, 111
    mv a0, s3
    SC 57
    EXPECT 0, 112

    # anonymous 首次全零；固定覆盖、hint 和 NOREPLACE 均核对原内容。
    mv a0, s4
    li a1, 4096
    SC 215
    EXPECT 0, 113
    mv a0, s4
    addi a0, a0, 17
    li a1, 8192
    li a2, 3
    li a3, 0x22
    li a4, -1
    li a5, 0
    SC 222
    CHECK_REG a0, s4, 114
    mv a0, s4
    li a1, 8192
    ZERO 115
    li t0, 0x5b
    sb t0, 0(s4)
    mv a0, s4
    li a1, 8192
    li a2, 3
    li a3, 0x100022
    li a4, -1
    li a5, 0
    SC 222
    EXPECT -17, 116
    lbu t1, 0(s4)
    CHECK t1, 0x5b, 117
    mv a0, s4
    li a1, 4096
    li a2, 3
    li a3, 0x32
    li a4, -1
    li a5, 0
    SC 222
    CHECK_REG a0, s4, 118
    mv a0, s4
    li a1, 4096
    ZERO 119
    li t0, 0x71
    sb t0, 0(s4)
    mv a0, s4
    addi a0, a0, 1
    li a1, 0
    li a2, 1
    SC 226
    EXPECT -22, 120
    mv a0, s4
    li a1, 0
    li a2, 1
    SC 226
    EXPECT 0, 121
    mv a0, s4
    li a1, 1
    li a2, 1
    SC 226
    EXPECT 0, 122
    mv a0, s0
    mv a1, s4
    li a2, 1
    SC 63
    EXPECT -14, 123
    mv a0, s0
    li a1, 0
    li a2, 1
    SC 62
    EXPECT 354, 124
    lbu t1, 0(s4)
    CHECK t1, 0x71, 125
    mv a0, s4
    li a1, 1
    li a2, 3
    SC 226
    EXPECT 0, 126
    li t0, 0x72
    sb t0, 0(s4)
    lbu t1, 0(s4)
    CHECK t1, 0x72, 127
    mv a0, s4
    addi a0, a0, 1
    li a1, 1
    SC 215
    EXPECT -22, 128
    mv a0, s4
    li a1, 0
    SC 215
    EXPECT -22, 129
    mv a0, s4
    li a1, 1
    SC 215
    EXPECT 0, 130
    mv a0, s0
    mv a1, s4
    li a2, 1
    SC 63
    EXPECT -14, 131
    mv a0, s4
    li a1, 8192
    li a2, 1
    SC 226
    EXPECT -12, 132
    li t0, 4096
    add t0, s4, t0
    lbu t1, 0(t0)
    CHECK t1, 0, 133
    mv a0, s4
    li a1, 4097
    SC 215
    EXPECT 0, 134
    mv a0, s0
    li t0, 4096
    add a1, s4, t0
    li a2, 1
    SC 63
    EXPECT -14, 135
    mv a0, s4
    li a1, 8192
    SC 215
    EXPECT 0, 136
    mv a0, s4
    li a1, 4096
    li a2, 3
    li a3, 0x100022
    li a4, -1
    li a5, 0
    SC 222
    CHECK_REG a0, s4, 137
    mv a0, s4
    li a1, 4096
    ZERO 138
    mv a0, s4
    li a1, 4096
    SC 215
    EXPECT 0, 139

    # private 文件写在匿名页，完整 pread 仍必须读到原缓存图样。
    li a0, 0
    li a1, 16384
    li a2, 3
    li a3, 2
    mv a4, s0
    li a5, 0
    SC 222
    POS 140
    mv s4, a0
    mv a0, s4
    li a1, 12293
    li a2, 0
    PATTERN 141
    li t0, 12293
    add a0, s4, t0
    li a1, 4091
    ZERO 142
    li t0, 0x9a
    sb t0, 0(s4)
    li t1, 4096
    add t1, s4, t1
    sb t0, 0(t1)
    li t1, 12292
    add t1, s4, t1
    sb t0, 0(t1)
    lbu t1, 0(s4)
    CHECK t1, 0x9a, 143
    mv a0, s0
    lla a1, .Lbuffer
    li a2, 12293
    li a3, 0
    SC 67
    EXPECT 12293, 144
    lla a0, .Lbuffer
    li a1, 12293
    li a2, 0
    PATTERN 145
    mv a0, s0
    li a1, 0
    li a2, 1
    SC 62
    EXPECT 354, 146
    mv a0, s4
    li a1, 16384
    SC 215
    EXPECT 0, 147
    li a0, 0
    li a1, 4096
    li a2, 3
    li a3, 1
    mv a4, s0
    li a5, 0
    SC 222
    EXPECT -13, 148
    li a0, 0
    li a1, 4096
    li a2, 1
    li a3, 1
    mv a4, s0
    li a5, 0
    SC 222
    POS 149
    mv s4, a0
    mv a0, s4
    li a1, 4096
    li a2, 0
    PATTERN 150
    mv a0, s4
    li a1, 4096
    li a2, 3
    SC 226
    EXPECT -13, 151
    mv a0, s4
    li a1, 4096
    SC 215
    EXPECT 0, 152
    li a0, 0
    li a1, 4096
    li a2, 1
    li a3, 2
    mv a4, s2
    li a5, 4096
    SC 222
    POS 153
    mv s4, a0
    mv a0, s4
    li a1, 4096
    li a2, 4096
    PATTERN 154
    mv a0, s2
    SC 57
    EXPECT 0, 155
    mv a0, s4
    li a1, 4096
    li a2, 3
    SC 226
    EXPECT 0, 156
    li t0, 0x44
    sb t0, 3(s4)
    lbu t1, 3(s4)
    CHECK t1, 0x44, 157
    mv a0, s0
    lla a1, .Lbuffer
    li a2, 1
    li a3, 4099
    SC 67
    EXPECT 1, 158
    lla t0, .Lbuffer
    lbu t1, 0(t0)
    CHECK t1, 3, 159
    mv a0, s4
    li a1, 4096
    SC 215
    EXPECT 0, 160
    li a0, 0
    li a1, 4096
    li a2, 1
    li a3, 2
    mv a4, s0
    li a5, 1
    SC 222
    EXPECT -22, 161
    mv a0, s1
    SC 57
    EXPECT 0, 162
    mv a0, s0
    SC 57
    EXPECT 0, 163

    # raw brk 返回 byte break；缩页后再增长清零，保留页中的字节不丢失。
    li a0, 0
    SC 214
    POS 164
    mv s6, a0
    li t0, 4095
    add s7, s6, t0
    li t0, -4096
    and s7, s7, t0
    li t0, 8193
    add a0, s7, t0
    mv s4, a0
    SC 214
    CHECK_REG a0, s4, 165
    mv a0, s6
    sub a1, s4, s6
    ZERO 166
    li t0, 0x63
    sb t0, 0(s7)
    li t1, 4096
    add t1, s7, t1
    sb t0, 0(t1)
    li t1, 8192
    add t1, s7, t1
    sb t0, 0(t1)
    addi a0, s7, 17
    mv s4, a0
    SC 214
    CHECK_REG a0, s4, 167
    lbu t1, 0(s7)
    CHECK t1, 0x63, 168
    addi a0, s7, 33
    mv s4, a0
    SC 214
    CHECK_REG a0, s4, 169
    lbu t1, 0(s7)
    CHECK t1, 0x63, 170
    li t0, 8193
    add a0, s7, t0
    mv s4, a0
    SC 214
    CHECK_REG a0, s4, 171
    lbu t1, 0(s7)
    CHECK t1, 0x63, 172
    li t0, 4096
    add a0, s7, t0
    li a1, 4097
    ZERO 173
    li a0, -4096
    SC 214
    CHECK_REG a0, s4, 174
    addi a0, s6, -1
    SC 214
    CHECK_REG a0, s4, 175
    li a0, 0
    SC 214
    CHECK_REG a0, s4, 176
    mv a0, s6
    SC 214
    CHECK_REG a0, s6, 177
    li s11, 0
.Lexit:
    mv a0, s11
    SC 93
    .word 0
.size _start, .-_start

# /// 不可变非owner凭据验证真实openat拒绝；read沿已打开OFD，不重新检查DAC。
.type .Laccess, @function
.Laccess:
    li a0, -100
    lla a1, .Ldata_path
    li a2, 0
    li a3, 0
    SC 56
    EXPECT 0, 201
    mv s0, a0
    mv a0, s0
    lla a1, .Lbuffer
    li a2, 17
    SC 63
    EXPECT 17, 202
    lla a0, .Lbuffer
    li a1, 17
    li a2, 0
    PATTERN 203

    # 可读但无search的目录本身能打开，已有dirfd不能绕过后续组件权限。
    li a0, -100
    lla a1, .Lno_search_path
    li a2, 0x10000
    li a3, 0
    SC 56
    EXPECT 1, 204
    mv s1, a0
    li s2, -100
    DENIED s2, .Ldenied_path, 205
    DENIED s2, .Ldenied_item, 206
    DENIED s2, .Ldenied_dot_item, 207
    DENIED s2, .Ldenied_parent_data, 208
    DENIED s2, .Lno_search_item, 209
    DENIED s2, .Lno_search_dot_item, 210
    DENIED s2, .Lno_search_parent_data, 211
    DENIED s1, .Litem_name, 212
    DENIED s1, .Ldot_item_name, 213
    DENIED s1, .Lparent_data_name, 214
    DENIED s1, .Ldot_name, 215
    DENIED s1, .Lparent_name, 216
    lla a0, .Lno_search_path
    SC 49
    EXPECT -13, 217

    # 拒绝的open不发表fd2；失败read不写用户字节，也不移动已打开文件位置。
    lla s4, .Lbuffer
    li s5, 0x5a5a5a5a5a5a5a5a
    sd s5, 0(s4)
    mv a0, s1
    mv a1, s4
    li a2, 8
    SC 63
    EXPECT -21, 218
    ld t1, 0(s4)
    CHECK_REG t1, s5, 219
    li a0, 2
    mv a1, s4
    li a2, 8
    SC 63
    EXPECT -9, 220
    ld t1, 0(s4)
    CHECK_REG t1, s5, 221
    mv a0, s0
    li a1, 0
    li a2, 1
    SC 62
    EXPECT 17, 222
    mv a0, s0
    mv a1, s4
    li a2, 1
    SC 64
    EXPECT -9, 223
    li a0, -100
    lla a1, .Ldata_path
    li a2, 1
    li a3, 0
    SC 56
    EXPECT -30, 224

    # 所有失败后的首个新open必须仍取最低fd2，独立OFD仍从文件起点读。
    li a0, -100
    lla a1, .Ldata_path
    li a2, 0
    li a3, 0
    SC 56
    EXPECT 2, 225
    mv s3, a0
    mv a0, s3
    mv a1, s4
    li a2, 1
    SC 63
    EXPECT 1, 226
    mv a0, s4
    li a1, 1
    li a2, 0
    PATTERN 227
    mv a0, s0
    mv a1, s4
    li a2, 1
    SC 63
    EXPECT 1, 228
    mv a0, s4
    li a1, 1
    li a2, 17
    PATTERN 229
    mv a0, s3
    SC 57
    EXPECT 0, 230
    mv a0, s1
    SC 57
    EXPECT 0, 231
    mv a0, s0
    SC 57
    EXPECT 0, 232
    li a0, -100
    lla a1, .Ldata_path
    li a2, 0
    li a3, 0
    SC 56
    EXPECT 0, 233
    SC 57
    EXPECT 0, 234
    li s11, 0
    j .Lexit
.size .Laccess, .-.Laccess

# /// 核对实际交付的连续字节图样，a2 为真实文件偏移；保留所有 s 寄存器。
.type .Lpattern, @function
.Lpattern:
    beqz a1, .Lpattern_done
.Lpattern_next:
    lbu t0, 0(a0)
    andi t1, a2, 255
    bne t0, t1, .Lexit
    addi a0, a0, 1
    addi a2, a2, 1
    addi a1, a1, -1
    bnez a1, .Lpattern_next
.Lpattern_done:
    ret
.size .Lpattern, .-.Lpattern

# /// 核对指定有效范围的每个字节均为零，避免只检查首字节。
.type .Lzero, @function
.Lzero:
    beqz a1, .Lzero_done
.Lzero_next:
    lbu t0, 0(a0)
    bnez t0, .Lexit
    addi a0, a0, 1
    addi a1, a1, -1
    bnez a1, .Lzero_next
.Lzero_done:
    ret
.size .Lzero, .-.Lzero

# /// 比较 a2 字节，不越过短 read 或无 NUL 的 readlink 返回范围。
.type .Lequal, @function
.Lequal:
    beqz a2, .Lequal_done
.Lequal_next:
    lbu t0, 0(a0)
    lbu t1, 0(a1)
    bne t0, t1, .Lexit
    addi a0, a0, 1
    addi a1, a1, 1
    addi a2, a2, -1
    bnez a2, .Lequal_next
.Lequal_done:
    ret
.size .Lequal, .-.Lequal

# /// 检查 stat128 的真实类型、大小、链接和时间字段；a2=-1 不约束目录大小。
.type .Lstat, @function
.Lstat:
    ld t0, 0(a0)
    beqz t0, .Lexit
    ld t0, 8(a0)
    beqz t0, .Lexit
    lwu t0, 16(a0)
    li t1, 0xf000
    and t0, t0, t1
    bne t0, a1, .Lexit
    lwu t0, 20(a0)
    li t1, 1
    li t2, 0x4000
    beq a1, t2, .Lstat_dir_links
    bne t0, t1, .Lexit
    j .Lstat_links_done
.Lstat_dir_links:
    li t1, 2
    bltu t0, t1, .Lexit
.Lstat_links_done:
    ld t0, 32(a0)
    bnez t0, .Lexit
    ld t0, 40(a0)
    bnez t0, .Lexit
    li t0, -1
    beq a2, t0, .Lstat_size_done
    ld t0, 48(a0)
    bne t0, a2, .Lexit
.Lstat_size_done:
    lwu t0, 56(a0)
    li t1, 1024
    bltu t0, t1, .Lexit
    li t1, 4096
    bltu t1, t0, .Lexit
    addi t1, t0, -1
    and t1, t0, t1
    bnez t1, .Lexit
    lwu t0, 60(a0)
    bnez t0, .Lexit
    ld t0, 64(a0)
    bltz t0, .Lexit
    # 普通图样文件不是稀疏文件，512B 单位至少覆盖全部有效数据。
    li t1, 12293
    bne a2, t1, .Lstat_times
    li t1, 25
    bltu t0, t1, .Lexit
.Lstat_times:
    li t1, 1000000000
    ld t0, 80(a0)
    bgeu t0, t1, .Lexit
    ld t0, 96(a0)
    bgeu t0, t1, .Lexit
    ld t0, 112(a0)
    bgeu t0, t1, .Lexit
    ld t0, 120(a0)
    bnez t0, .Lexit
    ret
.size .Lstat, .-.Lstat

# /// 逐项解析完整 dirent64；唯一名称、类型、inode 和 d_off 均来自真实目录。
# a0/a1 为返回范围，s5 累计五个已见名称位，禁止重复或未知名称。
.type .Ldirents, @function
.Ldirents:
    addi sp, sp, -64
    sd ra, 0(sp)
    sd s6, 8(sp)
    sd s7, 16(sp)
    mv s6, a0
    add s7, a0, a1
.Ldirent_next:
    beq s6, s7, .Ldirent_done
    sub t0, s7, s6
    li t1, 24
    bltu t0, t1, .Lexit
    lhu t1, 16(s6)
    bltu t1, t0, .Ldirent_length_fits
    bne t1, t0, .Lexit
.Ldirent_length_fits:
    li t0, 24
    bltu t1, t0, .Lexit
    andi t0, t1, 7
    bnez t0, .Lexit
    sd t1, 24(sp)
    ld t0, 0(s6)
    beqz t0, .Lexit
    ld t0, 8(s6)
    blez t0, .Lexit
    lla t2, .Llast_cookie
    sd t0, 0(t2)
    addi t0, s6, 19
    add t2, s6, t1
    li t3, 0
.Ldirent_name_length:
    beq t0, t2, .Lexit
    lbu t4, 0(t0)
    beqz t4, .Ldirent_name_end
    addi t3, t3, 1
    addi t0, t0, 1
    j .Ldirent_name_length
.Ldirent_name_end:
    beqz t3, .Lexit
    addi t0, t3, 27
    li t2, -8
    and t0, t0, t2
    bne t0, t1, .Lexit
    sd t3, 32(sp)
    lla t2, .Ldirent_expected
    li t4, 5
.Ldirent_find:
    ld t0, 8(t2)
    bne t0, t3, .Ldirent_find_next
    sd t2, 40(sp)
    sd t4, 48(sp)
    addi a0, s6, 19
    ld a1, 0(t2)
    mv a2, t3
    # 比较候选名称；不匹配仍检查同长度的其余真实名称。
.Ldirent_compare:
    lbu t0, 0(a0)
    lbu t1, 0(a1)
    bne t0, t1, .Ldirent_compare_bad
    addi a0, a0, 1
    addi a1, a1, 1
    addi a2, a2, -1
    bnez a2, .Ldirent_compare
    ld t2, 40(sp)
    ld t0, 16(t2)
    lbu t1, 18(s6)
    bne t0, t1, .Lexit
    ld t0, 24(t2)
    and t1, s5, t0
    bnez t1, .Lexit
    or s5, s5, t0
    ld t0, 32(t2)
    beqz t0, .Ldirent_advance
    ld t0, 8(t0)
    ld t1, 0(s6)
    bne t0, t1, .Lexit
    j .Ldirent_advance
.Ldirent_compare_bad:
    ld t2, 40(sp)
    ld t4, 48(sp)
    ld t3, 32(sp)
.Ldirent_find_next:
    addi t2, t2, 40
    addi t4, t4, -1
    bnez t4, .Ldirent_find
    j .Lexit
.Ldirent_advance:
    ld t0, 24(sp)
    add s6, s6, t0
    j .Ldirent_next
.Ldirent_done:
    ld ra, 0(sp)
    ld s6, 8(sp)
    ld s7, 16(sp)
    addi sp, sp, 64
    ret
.size .Ldirents, .-.Ldirents

.section .rodata, "a", @progbits

.Ldata_path: .asciz "/test/io-data.bin"
.Ldir_path: .asciz "/test/io-dir"
.Laccess_mode: .asciz "access"
.Ldenied_path: .asciz "/test/io-denied"
.Ldenied_item: .asciz "/test/io-denied/item"
.Ldenied_dot_item: .asciz "/test/io-denied/./item"
.Ldenied_parent_data: .asciz "/test/io-denied/../io-data.bin"
.Lno_search_path: .asciz "/test/io-no-search"
.Lno_search_item: .asciz "/test/io-no-search/item"
.Lno_search_dot_item: .asciz "/test/io-no-search/./item"
.Lno_search_parent_data: .asciz "/test/io-no-search/../io-data.bin"
.Litem_name: .asciz "item"
.Ldot_item_name: .asciz "./item"
.Lparent_data_name: .asciz "../io-data.bin"
.Lroot_path: .asciz "/"
.Lfirst_name: .asciz "first"
.Lsecond_name: .asciz "second"
.Llink_name: .asciz "link"
.Ldot_name: .asciz "."
.Lparent_name: .asciz ".."
.Ldot_first: .asciz "./first"
.Lrelative_data: .asciz "io-data.bin"
.Lnot_directory: .asciz "first/child"
.Lfirst_contents: .ascii "first-data\n"
.balign 8
.Ldirent_expected:
    .quad .Ldot_name, 1, 4, 1, .Lstat_dir
    .quad .Lparent_name, 2, 4, 2, 0
    .quad .Lfirst_name, 5, 8, 4, .Lstat_first
    .quad .Lsecond_name, 6, 8, 8, .Lstat_second
    .quad .Llink_name, 4, 10, 16, .Lstat_link

.data

# smoke.ld 要求真实文件段后紧随未页齐 BSS。
.byte 0x57

.bss

.balign 8
.Lstat_data: .skip 128
.Lstat_dir: .skip 128
.Lstat_first: .skip 128
.Lstat_second: .skip 128
.Lstat_link: .skip 128
.Lstat_temp: .skip 128
.Lfirst_cookie: .skip 8
.Llast_cookie: .skip 8
.Lbuffer: .skip 16384
.byte 0
