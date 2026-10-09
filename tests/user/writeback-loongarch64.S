.text

# /// 两架构使用 asm-generic syscall，所有返回值由调用点核验。
.macro SC number
    li.d $a7, \number
    syscall 0
.endm
# /// 失败保留具体退出码；比较会覆盖 t0。
.macro EXPECT value, number
    li.d $s8, \number
    li.d $t0, \value
    beq $a0, $t0, 991f
    b .Lexit
991:
.endm
# /// 非负返回允许描述符零和由内核选择的映射地址。
.macro OK number
    li.d $s8, \number
    bge $a0, $zero, 991f
    b .Lexit
991:
.endm
# /// 相对路径使用实际目录描述符，创建权限只用于新对象。
.macro OPEN dirfd, pathname, flags, mode, number
    move $a0, \dirfd
    la.local $a1, \pathname
    li.d $a2, \flags
    li.d $a3, \mode
    SC 56
    OK \number
.endm
# /// 文件和目录同步后显式关闭，close 不代替同步。
.macro FD_SC fd, syscall, number
    move $a0, \fd
    SC \syscall
    EXPECT 0, \number
.endm
# /// 三个结果路径都只读打开并逐字节检查，随后实际释放描述符。
.macro VERIFY pathname
    OPEN $s1, \pathname, 0, 0, 32
    move $s0, $a0
    bl .Lverify_file
    FD_SC $s0, 57, 42
.endm

# /// 初始栈必须恰有程序名和单字节 w/r/a；a 的 EEXIST 分支只读取旧结果。
.global _start
.type _start, @function
_start:
    li.d $s8, 1
    ld.d $t0, $sp, 0
    li.d $t1, 2
    beq $t0, $t1, 991f
    b .Lexit
991:
    ld.d $t0, $sp, 16
    beq $t0, $zero, .Lexit
    ld.bu $s5, $t0, 0
    ld.bu $t1, $t0, 1
    bne $t1, $zero, .Lexit
    li.d $t1, 'r'
    beq $s5, $t1, .Lread
    li.d $t1, 'w'
    beq $s5, $t1, .Lmkdir
    li.d $t1, 'a'
    beq $s5, $t1, .Lmkdir
    b .Lexit
.Lmkdir:
    li.d $a0, -100
    la.local $a1, .Ldirectory
    li.d $a2, 0x1ed
    SC 34
    beq $a0, $zero, .Lwrite
    li.d $s8, 2
    li.d $t0, 'a'
    bne $s5, $t0, .Lexit
    li.d $t0, -17
    bne $a0, $t0, .Lexit
    b .Lread

# /// 首次写只在 mkdir 成功后创建 pending；既存目录或错误内容不得重建。
.Lwrite:
    li.d $t1, -100
    OPEN $t1, .Ldirectory, 0x10000, 0, 3
    move $s1, $a0
    OPEN $s1, .Lpending, 0xc2, 0x1a4, 4
    move $s0, $a0
    la.local $t0, .Lbuffer
    li.d $t1, 8229
    li.d $t2, 71
.Lfill:
    st.b $t2, $t0, 0
    addi.d $t0, $t0, 1
    addi.d $t2, $t2, 13
    addi.d $t1, $t1, -1
    bne $t1, $zero, .Lfill
    move $a0, $s0
    la.local $a1, .Lbuffer
    li.d $a2, 8229
    SC 64
    EXPECT 8229, 5
    FD_SC $s0, 83, 6

    # 共享映射覆盖前两页，真实 store 在两页分别留下持久修改。
    li.d $a0, 0
    li.d $a1, 8192
    li.d $a2, 3
    li.d $a3, 1
    move $a4, $s0
    li.d $a5, 0
    SC 222
    OK 7
    move $s4, $a0
    li.d $t0, 0x79
    st.b $t0, $s4, 17
    li.d $t0, 4127
    add.d $t0, $s4, $t0
    li.d $t1, 0x53
    st.b $t1, $t0, 0
    move $a0, $s4
    li.d $a1, 8192
    li.d $a2, 4
    SC 227
    EXPECT 0, 8
    ld.bu $a0, $s4, 17
    EXPECT 0x79, 9
    li.d $t0, 4127
    add.d $t0, $s4, $t0
    ld.bu $a0, $t0, 0
    EXPECT 0x53, 10
    move $a0, $s4
    li.d $a1, 8192
    SC 215
    EXPECT 0, 11

    # 私有写经 COW 保留 0xfb，MS_SYNC 不得把它提交给原文件。
    li.d $a0, 0
    li.d $a1, 4096
    li.d $a2, 3
    li.d $a3, 2
    move $a4, $s0
    li.d $a5, 0
    SC 222
    OK 12
    move $s4, $a0
    ld.bu $a0, $s4, 17
    EXPECT 0x79, 13
    li.d $t0, 0xfb
    st.b $t0, $s4, 17
    move $a0, $s4
    li.d $a1, 4096
    li.d $a2, 4
    SC 227
    EXPECT 0, 14
    ld.bu $a0, $s4, 17
    EXPECT 0xfb, 15
    move $a0, $s4
    li.d $a1, 4096
    SC 215
    EXPECT 0, 16

    move $a0, $s1
    la.local $a1, .Lpending
    move $a2, $s1
    la.local $a3, .Lresult
    li.d $a4, 0
    SC 276
    EXPECT 0, 17
    move $a0, $s1
    la.local $a1, .Lresult
    move $a2, $s1
    la.local $a3, .Lhard
    li.d $a4, 0
    SC 37
    EXPECT 0, 18
    la.local $a0, .Lresult
    move $a1, $s1
    la.local $a2, .Llink
    SC 36
    EXPECT 0, 19
    move $a0, $s1
    la.local $a1, .Ltemporary
    li.d $a2, 0x1ed
    SC 34
    EXPECT 0, 20
    move $a0, $s1
    la.local $a1, .Ltemporary
    li.d $a2, 0x200
    SC 35
    EXPECT 0, 21
    FD_SC $s0, 82, 22
    FD_SC $s0, 57, 23
    FD_SC $s1, 82, 24
    li.d $t1, -100
    OPEN $t1, .Lparent, 0x10000, 0, 25
    move $s0, $a0
    FD_SC $s0, 82, 26
    FD_SC $s0, 57, 27
    FD_SC $s1, 57, 28

# /// 只读验证文件别名、软链接本身与唯一目录内容，不修改已有对象。
.Lread:
    li.d $t1, -100
    OPEN $t1, .Ldirectory, 0x10000, 0, 30
    move $s1, $a0
    move $a0, $s1
    la.local $a1, .Lstat
    SC 80
    EXPECT 0, 31
    la.local $t0, .Lstat
    ld.d $s4, $t0, 8
    ld.wu $a0, $t0, 20
    EXPECT 2, 43
    li.d $s2, 0
    VERIFY .Lresult
    VERIFY .Lhard
    VERIFY .Llink
    move $a0, $s1
    la.local $a1, .Llink
    la.local $a2, .Lstat
    li.d $a3, 0x100
    SC 79
    EXPECT 0, 44
    la.local $t0, .Lstat
    ld.d $s5, $t0, 8
    ld.wu $a0, $t0, 16
    li.d $t1, 0xf000
    and $a0, $a0, $t1
    EXPECT 0xa000, 45
    la.local $t0, .Lstat
    ld.wu $a0, $t0, 20
    EXPECT 1, 46
    la.local $t0, .Lstat
    ld.d $a0, $t0, 48
    EXPECT 6, 47
    move $a0, $s1
    la.local $a1, .Llink
    la.local $a2, .Lbuffer
    li.d $a3, 16
    SC 78
    EXPECT 6, 48
    la.local $t0, .Lbuffer
    la.local $t1, .Lresult
    li.d $t2, 6
    li.d $s8, 49
.Llink_bytes:
    ld.bu $t3, $t0, 0
    ld.bu $t4, $t1, 0
    bne $t3, $t4, .Lexit
    addi.d $t0, $t0, 1
    addi.d $t1, $t1, 1
    addi.d $t2, $t2, -1
    bne $t2, $zero, .Llink_bytes
    li.d $s6, 0
    li.d $s7, 0
.Lgetdents:
    move $a0, $s1
    la.local $a1, .Lbuffer
    li.d $a2, 128
    SC 61
    OK 50
    beq $a0, $zero, .Ldirectory_done
    li.d $s8, 51
    li.d $t0, 128
    bltu $t0, $a0, .Lexit
    la.local $t5, .Lbuffer
    add.d $t6, $t5, $a0

    # linux_dirent64 的名字从19开始；只在完整且8字节对齐的记录内比较。
.Ldirent:
    sub.d $t0, $t6, $t5
    li.d $t1, 24
    bltu $t0, $t1, .Lexit
    ld.hu $t2, $t5, 16
    bltu $t2, $t1, .Lexit
    bltu $t0, $t2, .Lexit
    andi $t0, $t2, 7
    bne $t0, $zero, .Lexit
    ld.d $t0, $t5, 0
    li.d $s8, 52
    beq $t0, $zero, .Lexit
    ld.d $t0, $t5, 8
    li.d $s8, 53
    beq $t0, $s7, .Lexit
    blt $t0, $zero, .Lexit
    move $s7, $t0
    la.local $t1, .Lnames
    li.d $t3, 1
.Lname_candidate:
    ld.d $t0, $t1, 0
    addi.d $a1, $t5, 19
    add.d $a2, $t5, $t2
.Lname_bytes:
    beq $a1, $a2, .Lnext_candidate
    ld.bu $a3, $a1, 0
    ld.bu $a4, $t0, 0
    bne $a3, $a4, .Lnext_candidate
    beq $a3, $zero, .Lname_found
    addi.d $a1, $a1, 1
    addi.d $t0, $t0, 1
    b .Lname_bytes
.Lnext_candidate:
    addi.d $t1, $t1, 16
    slli.d $t3, $t3, 1
    li.d $t0, 32
    bltu $t3, $t0, .Lname_candidate
    li.d $s8, 54
    b .Lexit
.Lname_found:
    li.d $s8, 55
    and $t0, $s6, $t3
    bne $t0, $zero, .Lexit
    or $s6, $s6, $t3
    ld.d $t0, $t1, 8
    ld.bu $a0, $t5, 18
    li.d $s8, 56
    bne $a0, $t0, .Lexit
    # d_reclen 必须等于该名字连同 NUL 的8字节对齐长度。
    sub.d $t0, $a1, $t5
    addi.d $t0, $t0, 8
    li.d $t4, -8
    and $t0, $t0, $t4
    li.d $s8, 57
    bne $t0, $t2, .Lexit
    ld.d $t0, $t5, 0
    li.d $t1, 1
    beq $t3, $t1, .Ldot_inode
    li.d $t1, 4
    beq $t3, $t1, .Lfile_inode
    li.d $t1, 8
    beq $t3, $t1, .Lfile_inode
    li.d $t1, 16
    beq $t3, $t1, .Llink_inode
    b .Lnext_dirent
.Ldot_inode:
    li.d $s8, 58
    bne $t0, $s4, .Lexit
    b .Lnext_dirent
.Lfile_inode:
    li.d $s8, 59
    bne $t0, $s2, .Lexit
    b .Lnext_dirent
.Llink_inode:
    li.d $s8, 60
    bne $t0, $s5, .Lexit
.Lnext_dirent:
    add.d $t5, $t5, $t2
    bltu $t5, $t6, .Ldirent
    move $a0, $s1
    li.d $a1, 0
    li.d $a2, 1
    SC 62
    li.d $s8, 61
    bne $a0, $s7, .Lexit
    b .Lgetdents
.Ldirectory_done:
    move $a0, $s6
    EXPECT 31, 62
    FD_SC $s1, 57, 63
    li.d $s8, 0
.Lexit:
    move $a0, $s8
    SC 94
    b .Lexit
.size _start, .-_start

# /// 同一 inode 的三个只读路径必须具有完整8229字节、两处共享修改和链接数2。
# /// s2/s3保存首个结果的 inode/dev；stat中 mode/nlink 为独立32位字段。
.type .Lverify_file, @function
.Lverify_file:
    move $a0, $s0
    la.local $a1, .Lstat
    SC 80
    EXPECT 0, 33
    la.local $t0, .Lstat
    ld.d $t1, $t0, 8
    ld.d $t2, $t0, 0
    li.d $s8, 34
    beq $t1, $zero, .Lexit
    beq $t2, $zero, .Lexit
    bne $s2, $zero, .Lcompare_identity
    move $s2, $t1
    move $s3, $t2
.Lcompare_identity:
    bne $t1, $s2, .Lexit
    bne $t2, $s3, .Lexit
    ld.wu $a0, $t0, 16
    li.d $t1, 0xf000
    and $a0, $a0, $t1
    EXPECT 0x8000, 35
    la.local $t0, .Lstat
    ld.wu $a0, $t0, 20
    EXPECT 2, 36
    la.local $t0, .Lstat
    ld.d $a0, $t0, 48
    EXPECT 8229, 37
    move $a0, $s0
    la.local $a1, .Lbuffer
    li.d $a2, 8229
    li.d $a3, 0
    SC 67
    EXPECT 8229, 38
    la.local $t0, .Lbuffer
    li.d $t1, 0
    li.d $t2, 71
    li.d $t6, 8229
    li.d $s8, 39
.Lcheck_bytes:
    andi $t3, $t2, 255
    li.d $t4, 17
    bne $t1, $t4, .Lcheck_second
    li.d $t3, 0x79
.Lcheck_second:
    li.d $t4, 4127
    bne $t1, $t4, .Lcheck_byte
    li.d $t3, 0x53
.Lcheck_byte:
    ld.bu $t4, $t0, 0
    bne $t4, $t3, .Lexit
    addi.d $t0, $t0, 1
    addi.d $t1, $t1, 1
    addi.d $t2, $t2, 13
    bltu $t1, $t6, .Lcheck_bytes
    move $a0, $s0
    la.local $a1, .Lbuffer
    li.d $a2, 1
    li.d $a3, 8229
    SC 67
    EXPECT 0, 40
    move $a0, $s0
    li.d $a1, 0
    li.d $a2, 1
    SC 62
    EXPECT 0, 41
    jirl $zero, $ra, 0
.size .Lverify_file, .-.Lverify_file

.section .rodata
.Ldirectory: .asciz "/test/writeback-dir"
.Lparent: .asciz "/test"
.Lpending: .asciz "pending"
.Lresult: .asciz "result"
.Lhard: .asciz "hard"
.Llink: .asciz "link"
.Ltemporary: .asciz "temporary"
.Ldot: .asciz "."
.Ldotdot: .asciz ".."
.balign 8
.Lnames:
    .quad .Ldot, 4
    .quad .Ldotdot, 4
    .quad .Lresult, 8
    .quad .Lhard, 8
    .quad .Llink, 10

.section .bss
.balign 8
.Lstat: .skip 128
.Lbuffer: .skip 8229
