.text

# /// 使用目标 raw syscall 约定，只有 a0 承载返回值。
.macro SC number
    li.d $a7, \number
    syscall 0
.endm

# /// 每个断言点使用独立退出编号，保留原始返回值供后续字段检查。
.macro EXPECT value, number
    li.d $s8, \number
    li.d $t0, \value
    bne $a0, $t0, .Lexit
.endm
.macro CHECK actual, value, number
    li.d $s8, \number
    li.d $t0, \value
    bne \actual, $t0, .Lexit
.endm
.macro CHECK_REG actual, expected, number
    li.d $s8, \number
    bne \actual, \expected, .Lexit
.endm
.macro POS number
    li.d $s8, \number
    bge $zero, $a0, .Lexit
.endm
# /// 文件描述符允许为0；仅负返回值保留对应错误号并退出。
.macro FD number
    li.d $s8, \number
    blt $a0, $zero, .Lexit
.endm
.macro PATTERN number
    li.d $s8, \number
    bl .Lpattern
.endm
.macro ZERO number
    li.d $s8, \number
    bl .Lzero
.endm
.macro EQUAL number
    li.d $s8, \number
    bl .Lequal
.endm
.macro STAT number
    li.d $s8, \number
    bl .Lstat
.endm
.macro DIRENTS number
    li.d $s8, \number
    bl .Ldirents
.endm

# /// 目录读DAC或组件search失败必须保留原始EACCES，不使用内核权限探针。
.macro DENIED dirfd, pathname, number
    move $a0, \dirfd
    la.local $a1, \pathname
    li.d $a2, 0
    li.d $a3, 0
    SC 56
    EXPECT -13, \number
.endm

# /// argv[1]=access只选择非root访问断言，其余参数保持普通文件与内存ABI流程。
.global _start
.type _start, @function
    _start:
    ld.d $t0, $sp, 0
    li.d $t1, 2
    bltu $t0, $t1, .Lraw
    ld.d $a0, $sp, 16
    la.local $a1, .Laccess_mode
    li.d $a2, 7
.Lselect_mode:
    ld.bu $t0, $a0, 0
    ld.bu $t1, $a1, 0
    bne $t0, $t1, .Lraw
    addi.d $a0, $a0, 1
    addi.d $a1, $a1, 1
    addi.d $a2, $a2, -1
    bnez $a2, .Lselect_mode
    b .Laccess
.Lraw:
    # 文件图样为 byte[i] = i & 255，长度 12293。
    li.d $a0, -100
    la.local $a1, .Ldata_path
    li.d $a2, 0x80000
    li.d $a3, 0
    SC 56
    FD 1
    move $s0, $a0

    # 两页映射只保留第一页，跨坏页 read 必须只提交 17 字节。
    li.d $a0, 0
    li.d $a1, 8192
    li.d $a2, 3
    li.d $a3, 0x22
    li.d $a4, -1
    li.d $a5, 0
    SC 222
    POS 2
    move $s4, $a0
    li.d $t0, 4096
    add.d $a0, $s4, $t0
    li.d $a1, 4096
    SC 215
    EXPECT 0, 3
    move $a0, $s0
    li.d $t0, 4079
    add.d $a1, $s4, $t0
    li.d $a2, 64
    SC 63
    EXPECT 17, 4
    li.d $t0, 4079
    add.d $a0, $s4, $t0
    li.d $a1, 17
    li.d $a2, 0
    PATTERN 5
    move $a0, $s0
    li.d $a1, 0
    li.d $a2, 1
    SC 62
    EXPECT 17, 6
    move $a0, $s0
    la.local $a1, .Lbuffer
    li.d $a2, 300
    SC 63
    EXPECT 300, 7
    la.local $a0, .Lbuffer
    li.d $a1, 300
    li.d $a2, 17
    PATTERN 8
    move $a0, $s0
    la.local $a1, .Lbuffer
    li.d $a2, 513
    li.d $a3, 4093
    SC 67
    EXPECT 513, 9
    la.local $a0, .Lbuffer
    li.d $a1, 513
    li.d $a2, 4093
    PATTERN 10
    move $a0, $s0
    li.d $a1, 0
    li.d $a2, 1
    SC 62
    EXPECT 317, 11
    move $a0, $s0
    la.local $a1, .Lbuffer
    li.d $a2, 16
    li.d $a3, 12290
    SC 67
    EXPECT 3, 178
    la.local $a0, .Lbuffer
    li.d $a1, 3
    li.d $a2, 12290
    PATTERN 179
    move $a0, $s0
    la.local $a1, .Lbuffer
    li.d $a2, 1
    li.d $a3, 12293
    SC 67
    EXPECT 0, 180
    move $a0, $s0
    li.d $a1, 0
    li.d $a2, 1
    SC 62
    EXPECT 317, 181

    # dup 共享 cursor，但不继承 fd CLOEXEC；另一次 open 的 cursor 独立。
    move $a0, $s0
    SC 23
    FD 12
    move $s1, $a0
    move $a0, $s1
    la.local $a1, .Lbuffer
    li.d $a2, 37
    SC 63
    EXPECT 37, 13
    la.local $a0, .Lbuffer
    li.d $a1, 37
    li.d $a2, 317
    PATTERN 14
    move $a0, $s0
    li.d $a1, 0
    li.d $a2, 1
    SC 62
    EXPECT 354, 15
    li.d $a0, -100
    la.local $a1, .Ldata_path
    li.d $a2, 0
    li.d $a3, 0
    SC 56
    FD 16
    move $s2, $a0
    move $a0, $s2
    la.local $a1, .Lbuffer
    li.d $a2, 29
    SC 63
    EXPECT 29, 17
    la.local $a0, .Lbuffer
    li.d $a1, 29
    li.d $a2, 0
    PATTERN 18
    move $a0, $s0
    li.d $a1, 1
    SC 25
    EXPECT 1, 19
    move $a0, $s1
    li.d $a1, 1
    SC 25
    EXPECT 0, 20
    move $a0, $s1
    li.d $a1, 2
    li.d $a2, 1
    SC 25
    EXPECT 0, 21
    move $a0, $s0
    li.d $a1, 1
    SC 25
    EXPECT 1, 22
    move $a0, $s1
    li.d $a1, 2
    li.d $a2, 0
    SC 25
    EXPECT 0, 23
    move $a0, $s1
    li.d $a1, 1
    SC 25
    EXPECT 0, 24
    move $a0, $s0
    li.d $a1, 4
    li.d $a2, 0xc00
    SC 25
    EXPECT 0, 25
    move $a0, $s1
    li.d $a1, 3
    SC 25
    li.d $t0, 0xc03
    and $a0, $a0, $t0
    EXPECT 0xc00, 26
    move $a0, $s2
    li.d $a1, 3
    SC 25
    li.d $t0, 0xc03
    and $a0, $a0, $t0
    EXPECT 0, 27
    move $a0, $s0
    li.d $a1, 3
    SC 25
    li.d $t0, 0x80000
    and $a0, $a0, $t0
    EXPECT 0, 28
    move $a0, $s0
    li.d $a1, 0
    li.d $a2, 40
    SC 25
    EXPECT 40, 29
    li.d $a0, 40
    li.d $a1, 1
    SC 25
    EXPECT 0, 30
    li.d $a0, 40
    li.d $a1, 0
    li.d $a2, 1
    SC 62
    EXPECT 354, 31
    li.d $a0, 40
    SC 57
    EXPECT 0, 32
    move $a0, $s0
    li.d $a1, 1030
    li.d $a2, 40
    SC 25
    EXPECT 40, 33
    li.d $a0, 40
    li.d $a1, 1
    SC 25
    EXPECT 1, 34
    li.d $a0, 40
    SC 57
    EXPECT 0, 35
    move $a0, $s0
    li.d $a1, 41
    li.d $a2, 0x80000
    SC 24
    EXPECT 41, 36
    li.d $a0, 41
    li.d $a1, 1
    SC 25
    EXPECT 1, 37
    li.d $a0, 41
    SC 57
    EXPECT 0, 38
    move $a0, $s0
    move $a1, $s0
    li.d $a2, 0
    SC 24
    EXPECT -22, 39

    # 只读 fd 的写与写意图 open 有不同 errno；坏 fd 不访问用户指针。
    move $a0, $s0
    la.local $a1, .Lbuffer
    li.d $a2, 1
    SC 64
    EXPECT -9, 40
    li.d $a0, -1
    li.d $a1, 0
    li.d $a2, 1
    SC 63
    EXPECT -9, 41
    li.d $a0, -100
    la.local $a1, .Ldata_path
    li.d $a2, 1
    li.d $a3, 0
    SC 56
    EXPECT -30, 42
    li.d $a0, -100
    la.local $a1, .Ldata_path
    li.d $a2, 2
    li.d $a3, 0
    SC 56
    EXPECT -30, 43
    li.d $a0, -100
    la.local $a1, .Ldata_path
    li.d $a2, 0x240
    li.d $a3, 0
    SC 56
    EXPECT -30, 44
    move $a0, $s0
    li.d $a1, 0
    li.d $a2, 0
    SC 63
    EXPECT 0, 45
    move $a0, $s0
    li.d $a1, 0
    li.d $a2, 1
    SC 63
    EXPECT -14, 46
    move $a0, $s0
    li.d $a1, 0
    li.d $a2, 1
    SC 62
    EXPECT 354, 47

    # stat128 按字段取值，独立 statat 与 fstat 的整份快照必须一致。
    move $a0, $s0
    la.local $a1, .Lstat_data
    SC 80
    EXPECT 0, 48
    la.local $a0, .Lstat_data
    li.d $a1, 0x8000
    li.d $a2, 12293
    STAT 49
    li.d $a0, -999
    la.local $a1, .Ldata_path
    la.local $a2, .Lstat_temp
    li.d $a3, 0
    SC 79
    EXPECT 0, 50
    la.local $a0, .Lstat_data
    la.local $a1, .Lstat_temp
    li.d $a2, 128
    EQUAL 51
    move $a0, $s0
    li.d $t0, 4032
    add.d $a1, $s4, $t0
    SC 80
    EXPECT -14, 52

    # dirfd 搜索普通目录；绝对路径忽略坏 dirfd，普通文件不能穿越。
    li.d $a0, -100
    la.local $a1, .Ldir_path
    li.d $a2, 0x10000
    li.d $a3, 0
    SC 56
    FD 53
    move $s3, $a0
    move $a0, $s3
    la.local $a1, .Lstat_dir
    SC 80
    EXPECT 0, 54
    la.local $a0, .Lstat_dir
    li.d $a1, 0x4000
    li.d $a2, -1
    STAT 55
    move $a0, $s3
    la.local $a1, .Lfirst_name
    la.local $a2, .Lstat_first
    li.d $a3, 0
    SC 79
    EXPECT 0, 56
    la.local $a0, .Lstat_first
    li.d $a1, 0x8000
    li.d $a2, 11
    STAT 57
    move $a0, $s3
    la.local $a1, .Lsecond_name
    la.local $a2, .Lstat_second
    li.d $a3, 0
    SC 79
    EXPECT 0, 58
    la.local $a0, .Lstat_second
    li.d $a1, 0x8000
    li.d $a2, 12
    STAT 59
    move $a0, $s3
    la.local $a1, .Llink_name
    la.local $a2, .Lstat_link
    li.d $a3, 0x100
    SC 79
    EXPECT 0, 60
    la.local $a0, .Lstat_link
    li.d $a1, 0xa000
    li.d $a2, 5
    STAT 61
    move $a0, $s3
    la.local $a1, .Llink_name
    la.local $a2, .Lstat_temp
    li.d $a3, 0
    SC 79
    EXPECT 0, 62
    la.local $a0, .Lstat_first
    la.local $a1, .Lstat_temp
    li.d $a2, 128
    EQUAL 63
    move $a0, $s3
    la.local $a1, .Lfirst_name
    li.d $a2, 0
    li.d $a3, 0
    SC 56
    FD 64
    move $s6, $a0
    move $a0, $s6
    la.local $a1, .Lbuffer
    li.d $a2, 32
    SC 63
    EXPECT 11, 65
    la.local $a0, .Lbuffer
    la.local $a1, .Lfirst_contents
    li.d $a2, 11
    EQUAL 66
    move $a0, $s6
    SC 57
    EXPECT 0, 67
    li.d $a0, -999
    la.local $a1, .Ldata_path
    li.d $a2, 0
    li.d $a3, 0
    SC 56
    FD 68
    move $s6, $a0
    move $a0, $s6
    SC 57
    EXPECT 0, 69
    li.d $a0, -999
    la.local $a1, .Lfirst_name
    li.d $a2, 0
    li.d $a3, 0
    SC 56
    EXPECT -9, 70
    move $a0, $s0
    la.local $a1, .Lfirst_name
    li.d $a2, 0
    li.d $a3, 0
    SC 56
    EXPECT -20, 71
    move $a0, $s3
    la.local $a1, .Lnot_directory
    li.d $a2, 0
    li.d $a3, 0
    SC 56
    EXPECT -20, 72
    li.d $a0, -100
    la.local $a1, .Ldata_path
    li.d $a2, 0x10000
    li.d $a3, 0
    SC 56
    EXPECT -20, 73
    move $a0, $s3
    la.local $a1, .Llink_name
    li.d $a2, 0x20000
    li.d $a3, 0
    SC 56
    EXPECT -40, 74
    la.local $a0, .Ldata_path
    SC 49
    EXPECT -20, 75
    la.local $a0, .Ldir_path
    SC 49
    EXPECT 0, 76
    li.d $a0, -100
    la.local $a1, .Ldot_first
    li.d $a2, 0
    li.d $a3, 0
    SC 56
    FD 77
    move $s6, $a0
    move $a0, $s6
    la.local $a1, .Lstat_temp
    SC 80
    EXPECT 0, 78
    la.local $a0, .Lstat_first
    la.local $a1, .Lstat_temp
    li.d $a2, 128
    EQUAL 79
    move $a0, $s6
    SC 57
    EXPECT 0, 80
    la.local $a0, .Lparent_name
    SC 49
    EXPECT 0, 81
    li.d $a0, -100
    la.local $a1, .Lrelative_data
    li.d $a2, 0
    li.d $a3, 0
    SC 56
    FD 82
    move $s6, $a0
    move $a0, $s6
    la.local $a1, .Lstat_temp
    SC 80
    EXPECT 0, 83
    la.local $a0, .Lstat_data
    la.local $a1, .Lstat_temp
    li.d $a2, 128
    EQUAL 84
    move $a0, $s6
    SC 57
    EXPECT 0, 85
    la.local $a0, .Lroot_path
    SC 49
    EXPECT 0, 86

    # readlink 不写 NUL：先填哨兵，短读和全读后分别核对尾字节。
    la.local $t0, .Lbuffer
    li.d $t1, 0x5a
    st.b $t1, $t0, 3
    st.b $t1, $t0, 5
    move $a0, $s3
    la.local $a1, .Llink_name
    la.local $a2, .Lbuffer
    li.d $a3, 3
    SC 78
    EXPECT 3, 87
    la.local $a0, .Lbuffer
    la.local $a1, .Lfirst_name
    li.d $a2, 3
    EQUAL 88
    la.local $t0, .Lbuffer
    ld.bu $t1, $t0, 3
    CHECK $t1, 0x5a, 89
    move $a0, $s3
    la.local $a1, .Llink_name
    la.local $a2, .Lbuffer
    li.d $a3, 16
    SC 78
    EXPECT 5, 90
    la.local $a0, .Lbuffer
    la.local $a1, .Lfirst_name
    li.d $a2, 5
    EQUAL 91
    la.local $t0, .Lbuffer
    ld.bu $t1, $t0, 5
    CHECK $t1, 0x5a, 92
    move $a0, $s3
    la.local $a1, .Lfirst_name
    la.local $a2, .Lbuffer
    li.d $a3, 16
    SC 78
    EXPECT -22, 93

    # 首轮完整解析目录，记录真实首项长度/cookie，后续不假定盘面顺序。
    li.d $s5, 0
    move $a0, $s3
    la.local $a1, .Lbuffer
    li.d $a2, 4096
    SC 61
    POS 94
    la.local $t0, .Lbuffer
    ld.hu $s7, $t0, 16
    ld.d $t1, $t0, 8
    la.local $t2, .Lfirst_cookie
    st.d $t1, $t2, 0
    move $a1, $a0
    la.local $a0, .Lbuffer
    DIRENTS 95
    CHECK $s5, 31, 96
    move $a0, $s3
    la.local $a1, .Lbuffer
    li.d $a2, 4096
    SC 61
    EXPECT 0, 97
    move $a0, $s3
    li.d $a1, 0
    li.d $a2, 0
    SC 62
    EXPECT 0, 98
    move $a0, $s3
    la.local $a1, .Lbuffer
    addi.d $a2, $s7, -1
    SC 61
    EXPECT -22, 99
    move $a0, $s3
    li.d $a1, 0
    li.d $a2, 1
    SC 62
    EXPECT 0, 100
    # 起点使真实首项刚好结束在有效页末；下一项 copyout 无法完整交付。
    li.d $t0, 4096
    add.d $s6, $s4, $t0
    sub.d $s6, $s6, $s7
    move $a0, $s3
    move $a1, $s6
    li.d $a2, 4096
    SC 61
    CHECK_REG $a0, $s7, 101
    li.d $s5, 0
    move $a0, $s6
    move $a1, $s7
    DIRENTS 102
    move $a0, $s3
    li.d $a1, 0
    li.d $a2, 1
    SC 62
    la.local $t0, .Lfirst_cookie
    ld.d $t1, $t0, 0
    CHECK_REG $a0, $t1, 103
    la.local $t0, .Llast_cookie
    ld.d $t1, $t0, 0
    CHECK_REG $a0, $t1, 104
    move $a0, $s3
    li.d $t0, 4096
    add.d $a1, $s4, $t0
    li.d $a2, 4096
    SC 61
    EXPECT -14, 105
    move $a0, $s3
    li.d $a1, 0
    li.d $a2, 1
    SC 62
    la.local $t0, .Lfirst_cookie
    ld.d $t1, $t0, 0
    CHECK_REG $a0, $t1, 106
    move $a0, $s3
    la.local $a1, .Lbuffer
    li.d $a2, 4096
    SC 61
    POS 107
    move $a1, $a0
    la.local $a0, .Lbuffer
    DIRENTS 108
    CHECK $s5, 31, 109
    move $a0, $s3
    li.d $a1, 0
    li.d $a2, 1
    SC 62
    la.local $t0, .Llast_cookie
    ld.d $t1, $t0, 0
    CHECK_REG $a0, $t1, 110
    move $a0, $s3
    la.local $a1, .Lbuffer
    li.d $a2, 4096
    SC 61
    EXPECT 0, 111
    move $a0, $s3
    SC 57
    EXPECT 0, 112

    # anonymous 首次全零；固定覆盖、hint 和 NOREPLACE 均核对原内容。
    move $a0, $s4
    li.d $a1, 4096
    SC 215
    EXPECT 0, 113
    move $a0, $s4
    addi.d $a0, $a0, 17
    li.d $a1, 8192
    li.d $a2, 3
    li.d $a3, 0x22
    li.d $a4, -1
    li.d $a5, 0
    SC 222
    CHECK_REG $a0, $s4, 114
    move $a0, $s4
    li.d $a1, 8192
    ZERO 115
    li.d $t0, 0x5b
    st.b $t0, $s4, 0
    move $a0, $s4
    li.d $a1, 8192
    li.d $a2, 3
    li.d $a3, 0x100022
    li.d $a4, -1
    li.d $a5, 0
    SC 222
    EXPECT -17, 116
    ld.bu $t1, $s4, 0
    CHECK $t1, 0x5b, 117
    move $a0, $s4
    li.d $a1, 4096
    li.d $a2, 3
    li.d $a3, 0x32
    li.d $a4, -1
    li.d $a5, 0
    SC 222
    CHECK_REG $a0, $s4, 118
    move $a0, $s4
    li.d $a1, 4096
    ZERO 119
    li.d $t0, 0x71
    st.b $t0, $s4, 0
    move $a0, $s4
    addi.d $a0, $a0, 1
    li.d $a1, 0
    li.d $a2, 1
    SC 226
    EXPECT -22, 120
    move $a0, $s4
    li.d $a1, 0
    li.d $a2, 1
    SC 226
    EXPECT 0, 121
    move $a0, $s4
    li.d $a1, 1
    li.d $a2, 1
    SC 226
    EXPECT 0, 122
    move $a0, $s0
    move $a1, $s4
    li.d $a2, 1
    SC 63
    EXPECT -14, 123
    move $a0, $s0
    li.d $a1, 0
    li.d $a2, 1
    SC 62
    EXPECT 354, 124
    ld.bu $t1, $s4, 0
    CHECK $t1, 0x71, 125
    move $a0, $s4
    li.d $a1, 1
    li.d $a2, 3
    SC 226
    EXPECT 0, 126
    li.d $t0, 0x72
    st.b $t0, $s4, 0
    ld.bu $t1, $s4, 0
    CHECK $t1, 0x72, 127
    move $a0, $s4
    addi.d $a0, $a0, 1
    li.d $a1, 1
    SC 215
    EXPECT -22, 128
    move $a0, $s4
    li.d $a1, 0
    SC 215
    EXPECT -22, 129
    move $a0, $s4
    li.d $a1, 1
    SC 215
    EXPECT 0, 130
    move $a0, $s0
    move $a1, $s4
    li.d $a2, 1
    SC 63
    EXPECT -14, 131
    move $a0, $s4
    li.d $a1, 8192
    li.d $a2, 1
    SC 226
    EXPECT -12, 132
    li.d $t0, 4096
    add.d $t0, $s4, $t0
    ld.bu $t1, $t0, 0
    CHECK $t1, 0, 133
    move $a0, $s4
    li.d $a1, 4097
    SC 215
    EXPECT 0, 134
    move $a0, $s0
    li.d $t0, 4096
    add.d $a1, $s4, $t0
    li.d $a2, 1
    SC 63
    EXPECT -14, 135
    move $a0, $s4
    li.d $a1, 8192
    SC 215
    EXPECT 0, 136
    move $a0, $s4
    li.d $a1, 4096
    li.d $a2, 3
    li.d $a3, 0x100022
    li.d $a4, -1
    li.d $a5, 0
    SC 222
    CHECK_REG $a0, $s4, 137
    move $a0, $s4
    li.d $a1, 4096
    ZERO 138
    move $a0, $s4
    li.d $a1, 4096
    SC 215
    EXPECT 0, 139

    # private 文件写在匿名页，完整 pread 仍必须读到原缓存图样。
    li.d $a0, 0
    li.d $a1, 16384
    li.d $a2, 3
    li.d $a3, 2
    move $a4, $s0
    li.d $a5, 0
    SC 222
    POS 140
    move $s4, $a0
    move $a0, $s4
    li.d $a1, 12293
    li.d $a2, 0
    PATTERN 141
    li.d $t0, 12293
    add.d $a0, $s4, $t0
    li.d $a1, 4091
    ZERO 142
    li.d $t0, 0x9a
    st.b $t0, $s4, 0
    li.d $t1, 4096
    add.d $t1, $s4, $t1
    st.b $t0, $t1, 0
    li.d $t1, 12292
    add.d $t1, $s4, $t1
    st.b $t0, $t1, 0
    ld.bu $t1, $s4, 0
    CHECK $t1, 0x9a, 143
    move $a0, $s0
    la.local $a1, .Lbuffer
    li.d $a2, 12293
    li.d $a3, 0
    SC 67
    EXPECT 12293, 144
    la.local $a0, .Lbuffer
    li.d $a1, 12293
    li.d $a2, 0
    PATTERN 145
    move $a0, $s0
    li.d $a1, 0
    li.d $a2, 1
    SC 62
    EXPECT 354, 146
    move $a0, $s4
    li.d $a1, 16384
    SC 215
    EXPECT 0, 147
    li.d $a0, 0
    li.d $a1, 4096
    li.d $a2, 3
    li.d $a3, 1
    move $a4, $s0
    li.d $a5, 0
    SC 222
    EXPECT -13, 148
    li.d $a0, 0
    li.d $a1, 4096
    li.d $a2, 1
    li.d $a3, 1
    move $a4, $s0
    li.d $a5, 0
    SC 222
    POS 149
    move $s4, $a0
    move $a0, $s4
    li.d $a1, 4096
    li.d $a2, 0
    PATTERN 150
    move $a0, $s4
    li.d $a1, 4096
    li.d $a2, 3
    SC 226
    EXPECT -13, 151
    move $a0, $s4
    li.d $a1, 4096
    SC 215
    EXPECT 0, 152
    li.d $a0, 0
    li.d $a1, 4096
    li.d $a2, 1
    li.d $a3, 2
    move $a4, $s2
    li.d $a5, 4096
    SC 222
    POS 153
    move $s4, $a0
    move $a0, $s4
    li.d $a1, 4096
    li.d $a2, 4096
    PATTERN 154
    move $a0, $s2
    SC 57
    EXPECT 0, 155
    move $a0, $s4
    li.d $a1, 4096
    li.d $a2, 3
    SC 226
    EXPECT 0, 156
    li.d $t0, 0x44
    st.b $t0, $s4, 3
    ld.bu $t1, $s4, 3
    CHECK $t1, 0x44, 157
    move $a0, $s0
    la.local $a1, .Lbuffer
    li.d $a2, 1
    li.d $a3, 4099
    SC 67
    EXPECT 1, 158
    la.local $t0, .Lbuffer
    ld.bu $t1, $t0, 0
    CHECK $t1, 3, 159
    move $a0, $s4
    li.d $a1, 4096
    SC 215
    EXPECT 0, 160
    li.d $a0, 0
    li.d $a1, 4096
    li.d $a2, 1
    li.d $a3, 2
    move $a4, $s0
    li.d $a5, 1
    SC 222
    EXPECT -22, 161
    move $a0, $s1
    SC 57
    EXPECT 0, 162
    move $a0, $s0
    SC 57
    EXPECT 0, 163

    # raw brk 返回 byte break；缩页后再增长清零，保留页中的字节不丢失。
    li.d $a0, 0
    SC 214
    POS 164
    move $s6, $a0
    li.d $t0, 4095
    add.d $s7, $s6, $t0
    li.d $t0, -4096
    and $s7, $s7, $t0
    li.d $t0, 8193
    add.d $a0, $s7, $t0
    move $s4, $a0
    SC 214
    CHECK_REG $a0, $s4, 165
    move $a0, $s6
    sub.d $a1, $s4, $s6
    ZERO 166
    li.d $t0, 0x63
    st.b $t0, $s7, 0
    li.d $t1, 4096
    add.d $t1, $s7, $t1
    st.b $t0, $t1, 0
    li.d $t1, 8192
    add.d $t1, $s7, $t1
    st.b $t0, $t1, 0
    addi.d $a0, $s7, 17
    move $s4, $a0
    SC 214
    CHECK_REG $a0, $s4, 167
    ld.bu $t1, $s7, 0
    CHECK $t1, 0x63, 168
    addi.d $a0, $s7, 33
    move $s4, $a0
    SC 214
    CHECK_REG $a0, $s4, 169
    ld.bu $t1, $s7, 0
    CHECK $t1, 0x63, 170
    li.d $t0, 8193
    add.d $a0, $s7, $t0
    move $s4, $a0
    SC 214
    CHECK_REG $a0, $s4, 171
    ld.bu $t1, $s7, 0
    CHECK $t1, 0x63, 172
    li.d $t0, 4096
    add.d $a0, $s7, $t0
    li.d $a1, 4097
    ZERO 173
    li.d $a0, -4096
    SC 214
    CHECK_REG $a0, $s4, 174
    addi.d $a0, $s6, -1
    SC 214
    CHECK_REG $a0, $s4, 175
    li.d $a0, 0
    SC 214
    CHECK_REG $a0, $s4, 176
    move $a0, $s6
    SC 214
    CHECK_REG $a0, $s6, 177
    li.d $s8, 0
.Lexit:
    move $a0, $s8
    SC 93
    .word 0
.size _start, .-_start

# /// 不可变非owner凭据验证真实openat拒绝；read沿已打开OFD，不重新检查DAC。
.type .Laccess, @function
.Laccess:
    li.d $a0, -100
    la.local $a1, .Ldata_path
    li.d $a2, 0
    li.d $a3, 0
    SC 56
    EXPECT 0, 201
    move $s0, $a0
    move $a0, $s0
    la.local $a1, .Lbuffer
    li.d $a2, 17
    SC 63
    EXPECT 17, 202
    la.local $a0, .Lbuffer
    li.d $a1, 17
    li.d $a2, 0
    PATTERN 203

    # 可读但无search的目录本身能打开，已有dirfd不能绕过后续组件权限。
    li.d $a0, -100
    la.local $a1, .Lno_search_path
    li.d $a2, 0x10000
    li.d $a3, 0
    SC 56
    EXPECT 1, 204
    move $s1, $a0
    li.d $s2, -100
    DENIED $s2, .Ldenied_path, 205
    DENIED $s2, .Ldenied_item, 206
    DENIED $s2, .Ldenied_dot_item, 207
    DENIED $s2, .Ldenied_parent_data, 208
    DENIED $s2, .Lno_search_item, 209
    DENIED $s2, .Lno_search_dot_item, 210
    DENIED $s2, .Lno_search_parent_data, 211
    DENIED $s1, .Litem_name, 212
    DENIED $s1, .Ldot_item_name, 213
    DENIED $s1, .Lparent_data_name, 214
    DENIED $s1, .Ldot_name, 215
    DENIED $s1, .Lparent_name, 216
    la.local $a0, .Lno_search_path
    SC 49
    EXPECT -13, 217

    # 拒绝的open不发表fd2；失败read不写用户字节，也不移动已打开文件位置。
    la.local $s4, .Lbuffer
    li.d $s5, 0x5a5a5a5a5a5a5a5a
    st.d $s5, $s4, 0
    move $a0, $s1
    move $a1, $s4
    li.d $a2, 8
    SC 63
    EXPECT -21, 218
    ld.d $t1, $s4, 0
    CHECK_REG $t1, $s5, 219
    li.d $a0, 2
    move $a1, $s4
    li.d $a2, 8
    SC 63
    EXPECT -9, 220
    ld.d $t1, $s4, 0
    CHECK_REG $t1, $s5, 221
    move $a0, $s0
    li.d $a1, 0
    li.d $a2, 1
    SC 62
    EXPECT 17, 222
    move $a0, $s0
    move $a1, $s4
    li.d $a2, 1
    SC 64
    EXPECT -9, 223
    li.d $a0, -100
    la.local $a1, .Ldata_path
    li.d $a2, 1
    li.d $a3, 0
    SC 56
    EXPECT -30, 224

    # 所有失败后的首个新open必须仍取最低fd2，独立OFD仍从文件起点读。
    li.d $a0, -100
    la.local $a1, .Ldata_path
    li.d $a2, 0
    li.d $a3, 0
    SC 56
    EXPECT 2, 225
    move $s3, $a0
    move $a0, $s3
    move $a1, $s4
    li.d $a2, 1
    SC 63
    EXPECT 1, 226
    move $a0, $s4
    li.d $a1, 1
    li.d $a2, 0
    PATTERN 227
    move $a0, $s0
    move $a1, $s4
    li.d $a2, 1
    SC 63
    EXPECT 1, 228
    move $a0, $s4
    li.d $a1, 1
    li.d $a2, 17
    PATTERN 229
    move $a0, $s3
    SC 57
    EXPECT 0, 230
    move $a0, $s1
    SC 57
    EXPECT 0, 231
    move $a0, $s0
    SC 57
    EXPECT 0, 232
    li.d $a0, -100
    la.local $a1, .Ldata_path
    li.d $a2, 0
    li.d $a3, 0
    SC 56
    EXPECT 0, 233
    SC 57
    EXPECT 0, 234
    li.d $s8, 0
    b .Lexit
.size .Laccess, .-.Laccess

# /// 核对实际交付的连续字节图样，a2 为真实文件偏移；保留所有 s 寄存器。
.type .Lpattern, @function
.Lpattern:
    beqz $a1, .Lpattern_done
.Lpattern_next:
    ld.bu $t0, $a0, 0
    andi $t1, $a2, 255
    bne $t0, $t1, .Lexit
    addi.d $a0, $a0, 1
    addi.d $a2, $a2, 1
    addi.d $a1, $a1, -1
    bnez $a1, .Lpattern_next
.Lpattern_done:
    jr $ra
.size .Lpattern, .-.Lpattern

# /// 核对指定有效范围的每个字节均为零，避免只检查首字节。
.type .Lzero, @function
.Lzero:
    beqz $a1, .Lzero_done
.Lzero_next:
    ld.bu $t0, $a0, 0
    bnez $t0, .Lexit
    addi.d $a0, $a0, 1
    addi.d $a1, $a1, -1
    bnez $a1, .Lzero_next
.Lzero_done:
    jr $ra
.size .Lzero, .-.Lzero

# /// 比较 a2 字节，不越过短 read 或无 NUL 的 readlink 返回范围。
.type .Lequal, @function
.Lequal:
    beqz $a2, .Lequal_done
.Lequal_next:
    ld.bu $t0, $a0, 0
    ld.bu $t1, $a1, 0
    bne $t0, $t1, .Lexit
    addi.d $a0, $a0, 1
    addi.d $a1, $a1, 1
    addi.d $a2, $a2, -1
    bnez $a2, .Lequal_next
.Lequal_done:
    jr $ra
.size .Lequal, .-.Lequal

# /// 检查 stat128 的真实类型、大小、链接和时间字段；a2=-1 不约束目录大小。
.type .Lstat, @function
.Lstat:
    ld.d $t0, $a0, 0
    beqz $t0, .Lexit
    ld.d $t0, $a0, 8
    beqz $t0, .Lexit
    ld.wu $t0, $a0, 16
    li.d $t1, 0xf000
    and $t0, $t0, $t1
    bne $t0, $a1, .Lexit
    ld.wu $t0, $a0, 20
    li.d $t1, 1
    li.d $t2, 0x4000
    beq $a1, $t2, .Lstat_dir_links
    bne $t0, $t1, .Lexit
    b .Lstat_links_done
.Lstat_dir_links:
    li.d $t1, 2
    bltu $t0, $t1, .Lexit
.Lstat_links_done:
    ld.d $t0, $a0, 32
    bnez $t0, .Lexit
    ld.d $t0, $a0, 40
    bnez $t0, .Lexit
    li.d $t0, -1
    beq $a2, $t0, .Lstat_size_done
    ld.d $t0, $a0, 48
    bne $t0, $a2, .Lexit
.Lstat_size_done:
    ld.wu $t0, $a0, 56
    li.d $t1, 1024
    bltu $t0, $t1, .Lexit
    li.d $t1, 4096
    bltu $t1, $t0, .Lexit
    addi.d $t1, $t0, -1
    and $t1, $t0, $t1
    bnez $t1, .Lexit
    ld.wu $t0, $a0, 60
    bnez $t0, .Lexit
    ld.d $t0, $a0, 64
    blt $t0, $zero, .Lexit
    # 普通图样文件不是稀疏文件，512B 单位至少覆盖全部有效数据。
    li.d $t1, 12293
    bne $a2, $t1, .Lstat_times
    li.d $t1, 25
    bltu $t0, $t1, .Lexit
.Lstat_times:
    li.d $t1, 1000000000
    ld.d $t0, $a0, 80
    bgeu $t0, $t1, .Lexit
    ld.d $t0, $a0, 96
    bgeu $t0, $t1, .Lexit
    ld.d $t0, $a0, 112
    bgeu $t0, $t1, .Lexit
    ld.d $t0, $a0, 120
    bnez $t0, .Lexit
    jr $ra
.size .Lstat, .-.Lstat

# /// 逐项解析完整 dirent64；唯一名称、类型、inode 和 d_off 均来自真实目录。
# a0/a1 为返回范围，s5 累计五个已见名称位，禁止重复或未知名称。
.type .Ldirents, @function
.Ldirents:
    addi.d $sp, $sp, -64
    st.d $ra, $sp, 0
    st.d $s6, $sp, 8
    st.d $s7, $sp, 16
    move $s6, $a0
    add.d $s7, $a0, $a1
.Ldirent_next:
    beq $s6, $s7, .Ldirent_done
    sub.d $t0, $s7, $s6
    li.d $t1, 24
    bltu $t0, $t1, .Lexit
    ld.hu $t1, $s6, 16
    bltu $t1, $t0, .Ldirent_length_fits
    bne $t1, $t0, .Lexit
.Ldirent_length_fits:
    li.d $t0, 24
    bltu $t1, $t0, .Lexit
    andi $t0, $t1, 7
    bnez $t0, .Lexit
    st.d $t1, $sp, 24
    ld.d $t0, $s6, 0
    beqz $t0, .Lexit
    ld.d $t0, $s6, 8
    bge $zero, $t0, .Lexit
    la.local $t2, .Llast_cookie
    st.d $t0, $t2, 0
    addi.d $t0, $s6, 19
    add.d $t2, $s6, $t1
    li.d $t3, 0
.Ldirent_name_length:
    beq $t0, $t2, .Lexit
    ld.bu $t4, $t0, 0
    beqz $t4, .Ldirent_name_end
    addi.d $t3, $t3, 1
    addi.d $t0, $t0, 1
    b .Ldirent_name_length
.Ldirent_name_end:
    beqz $t3, .Lexit
    addi.d $t0, $t3, 27
    li.d $t2, -8
    and $t0, $t0, $t2
    bne $t0, $t1, .Lexit
    st.d $t3, $sp, 32
    la.local $t2, .Ldirent_expected
    li.d $t4, 5
.Ldirent_find:
    ld.d $t0, $t2, 8
    bne $t0, $t3, .Ldirent_find_next
    st.d $t2, $sp, 40
    st.d $t4, $sp, 48
    addi.d $a0, $s6, 19
    ld.d $a1, $t2, 0
    move $a2, $t3
    # 比较候选名称；不匹配仍检查同长度的其余真实名称。
.Ldirent_compare:
    ld.bu $t0, $a0, 0
    ld.bu $t1, $a1, 0
    bne $t0, $t1, .Ldirent_compare_bad
    addi.d $a0, $a0, 1
    addi.d $a1, $a1, 1
    addi.d $a2, $a2, -1
    bnez $a2, .Ldirent_compare
    ld.d $t2, $sp, 40
    ld.d $t0, $t2, 16
    ld.bu $t1, $s6, 18
    bne $t0, $t1, .Lexit
    ld.d $t0, $t2, 24
    and $t1, $s5, $t0
    bnez $t1, .Lexit
    or $s5, $s5, $t0
    ld.d $t0, $t2, 32
    beqz $t0, .Ldirent_advance
    ld.d $t0, $t0, 8
    ld.d $t1, $s6, 0
    bne $t0, $t1, .Lexit
    b .Ldirent_advance
.Ldirent_compare_bad:
    ld.d $t2, $sp, 40
    ld.d $t4, $sp, 48
    ld.d $t3, $sp, 32
.Ldirent_find_next:
    addi.d $t2, $t2, 40
    addi.d $t4, $t4, -1
    bnez $t4, .Ldirent_find
    b .Lexit
.Ldirent_advance:
    ld.d $t0, $sp, 24
    add.d $s6, $s6, $t0
    b .Ldirent_next
.Ldirent_done:
    ld.d $ra, $sp, 0
    ld.d $s6, $sp, 8
    ld.d $s7, $sp, 16
    addi.d $sp, $sp, 64
    jr $ra
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
