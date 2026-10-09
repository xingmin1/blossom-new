.text
.option norelax
.option norvc

# /// 两架构使用 asm-generic syscall，所有返回值由调用点核验。
.macro SC number
    li a7, \number
    ecall
.endm
# /// 失败保留具体退出码；比较会覆盖 t0。
.macro EXPECT value, number
    li s11, \number
    li t0, \value
    beq a0, t0, 991f
    j .Lexit
991:
.endm
# /// 非负返回允许描述符零和由内核选择的映射地址。
.macro OK number
    li s11, \number
    bge a0, zero, 991f
    j .Lexit
991:
.endm
# /// 相对路径使用实际目录描述符，创建权限只用于新对象。
.macro OPEN dirfd, pathname, flags, mode, number
    mv a0, \dirfd
    lla a1, \pathname
    li a2, \flags
    li a3, \mode
    SC 56
    OK \number
.endm
# /// 文件和目录同步后显式关闭，close 不代替同步。
.macro FD_SC fd, syscall, number
    mv a0, \fd
    SC \syscall
    EXPECT 0, \number
.endm
# /// 三个结果路径都只读打开并逐字节检查，随后实际释放描述符。
.macro VERIFY pathname
    OPEN s1, \pathname, 0, 0, 32
    mv s0, a0
    call .Lverify_file
    FD_SC s0, 57, 42
.endm

# /// 初始栈必须恰有程序名和单字节 w/r/a；a 的 EEXIST 分支只读取旧结果。
.global _start
.type _start, @function
_start:
    li s11, 1
    ld t0, 0(sp)
    li t1, 2
    beq t0, t1, 991f
    j .Lexit
991:
    ld t0, 16(sp)
    beq t0, zero, .Lexit
    lbu s5, 0(t0)
    lbu t1, 1(t0)
    bne t1, zero, .Lexit
    li t1, 'r'
    beq s5, t1, .Lread
    li t1, 'w'
    beq s5, t1, .Lmkdir
    li t1, 'a'
    beq s5, t1, .Lmkdir
    j .Lexit
.Lmkdir:
    li a0, -100
    lla a1, .Ldirectory
    li a2, 0x1ed
    SC 34
    beq a0, zero, .Lwrite
    li s11, 2
    li t0, 'a'
    bne s5, t0, .Lexit
    li t0, -17
    bne a0, t0, .Lexit
    j .Lread

# /// 首次写只在 mkdir 成功后创建 pending；既存目录或错误内容不得重建。
.Lwrite:
    li t1, -100
    OPEN t1, .Ldirectory, 0x10000, 0, 3
    mv s1, a0
    OPEN s1, .Lpending, 0xc2, 0x1a4, 4
    mv s0, a0
    lla t0, .Lbuffer
    li t1, 8229
    li t2, 71
.Lfill:
    sb t2, 0(t0)
    addi t0, t0, 1
    addi t2, t2, 13
    addi t1, t1, -1
    bne t1, zero, .Lfill
    mv a0, s0
    lla a1, .Lbuffer
    li a2, 8229
    SC 64
    EXPECT 8229, 5
    FD_SC s0, 83, 6

    # 共享映射覆盖前两页，真实 store 在两页分别留下持久修改。
    li a0, 0
    li a1, 8192
    li a2, 3
    li a3, 1
    mv a4, s0
    li a5, 0
    SC 222
    OK 7
    mv s4, a0
    li t0, 0x79
    sb t0, 17(s4)
    li t0, 4127
    add t0, s4, t0
    li t1, 0x53
    sb t1, 0(t0)
    mv a0, s4
    li a1, 8192
    li a2, 4
    SC 227
    EXPECT 0, 8
    lbu a0, 17(s4)
    EXPECT 0x79, 9
    li t0, 4127
    add t0, s4, t0
    lbu a0, 0(t0)
    EXPECT 0x53, 10
    mv a0, s4
    li a1, 8192
    SC 215
    EXPECT 0, 11

    # 私有写经 COW 保留 0xfb，MS_SYNC 不得把它提交给原文件。
    li a0, 0
    li a1, 4096
    li a2, 3
    li a3, 2
    mv a4, s0
    li a5, 0
    SC 222
    OK 12
    mv s4, a0
    lbu a0, 17(s4)
    EXPECT 0x79, 13
    li t0, 0xfb
    sb t0, 17(s4)
    mv a0, s4
    li a1, 4096
    li a2, 4
    SC 227
    EXPECT 0, 14
    lbu a0, 17(s4)
    EXPECT 0xfb, 15
    mv a0, s4
    li a1, 4096
    SC 215
    EXPECT 0, 16

    mv a0, s1
    lla a1, .Lpending
    mv a2, s1
    lla a3, .Lresult
    li a4, 0
    SC 276
    EXPECT 0, 17
    mv a0, s1
    lla a1, .Lresult
    mv a2, s1
    lla a3, .Lhard
    li a4, 0
    SC 37
    EXPECT 0, 18
    lla a0, .Lresult
    mv a1, s1
    lla a2, .Llink
    SC 36
    EXPECT 0, 19
    mv a0, s1
    lla a1, .Ltemporary
    li a2, 0x1ed
    SC 34
    EXPECT 0, 20
    mv a0, s1
    lla a1, .Ltemporary
    li a2, 0x200
    SC 35
    EXPECT 0, 21
    FD_SC s0, 82, 22
    FD_SC s0, 57, 23
    FD_SC s1, 82, 24
    li t1, -100
    OPEN t1, .Lparent, 0x10000, 0, 25
    mv s0, a0
    FD_SC s0, 82, 26
    FD_SC s0, 57, 27
    FD_SC s1, 57, 28

# /// 只读验证文件别名、软链接本身与唯一目录内容，不修改已有对象。
.Lread:
    li t1, -100
    OPEN t1, .Ldirectory, 0x10000, 0, 30
    mv s1, a0
    mv a0, s1
    lla a1, .Lstat
    SC 80
    EXPECT 0, 31
    lla t0, .Lstat
    ld s4, 8(t0)
    lwu a0, 20(t0)
    EXPECT 2, 43
    li s2, 0
    VERIFY .Lresult
    VERIFY .Lhard
    VERIFY .Llink
    mv a0, s1
    lla a1, .Llink
    lla a2, .Lstat
    li a3, 0x100
    SC 79
    EXPECT 0, 44
    lla t0, .Lstat
    ld s5, 8(t0)
    lwu a0, 16(t0)
    li t1, 0xf000
    and a0, a0, t1
    EXPECT 0xa000, 45
    lla t0, .Lstat
    lwu a0, 20(t0)
    EXPECT 1, 46
    lla t0, .Lstat
    ld a0, 48(t0)
    EXPECT 6, 47
    mv a0, s1
    lla a1, .Llink
    lla a2, .Lbuffer
    li a3, 16
    SC 78
    EXPECT 6, 48
    lla t0, .Lbuffer
    lla t1, .Lresult
    li t2, 6
    li s11, 49
.Llink_bytes:
    lbu t3, 0(t0)
    lbu t4, 0(t1)
    bne t3, t4, .Lexit
    addi t0, t0, 1
    addi t1, t1, 1
    addi t2, t2, -1
    bne t2, zero, .Llink_bytes
    li s6, 0
    li s7, 0
.Lgetdents:
    mv a0, s1
    lla a1, .Lbuffer
    li a2, 128
    SC 61
    OK 50
    beq a0, zero, .Ldirectory_done
    li s11, 51
    li t0, 128
    bltu t0, a0, .Lexit
    lla t5, .Lbuffer
    add t6, t5, a0

    # linux_dirent64 的名字从19开始；只在完整且8字节对齐的记录内比较。
.Ldirent:
    sub t0, t6, t5
    li t1, 24
    bltu t0, t1, .Lexit
    lhu t2, 16(t5)
    bltu t2, t1, .Lexit
    bltu t0, t2, .Lexit
    andi t0, t2, 7
    bne t0, zero, .Lexit
    ld t0, 0(t5)
    li s11, 52
    beq t0, zero, .Lexit
    ld t0, 8(t5)
    li s11, 53
    beq t0, s7, .Lexit
    blt t0, zero, .Lexit
    mv s7, t0
    lla t1, .Lnames
    li t3, 1
.Lname_candidate:
    ld t0, 0(t1)
    addi a1, t5, 19
    add a2, t5, t2
.Lname_bytes:
    beq a1, a2, .Lnext_candidate
    lbu a3, 0(a1)
    lbu a4, 0(t0)
    bne a3, a4, .Lnext_candidate
    beq a3, zero, .Lname_found
    addi a1, a1, 1
    addi t0, t0, 1
    j .Lname_bytes
.Lnext_candidate:
    addi t1, t1, 16
    slli t3, t3, 1
    li t0, 32
    bltu t3, t0, .Lname_candidate
    li s11, 54
    j .Lexit
.Lname_found:
    li s11, 55
    and t0, s6, t3
    bne t0, zero, .Lexit
    or s6, s6, t3
    ld t0, 8(t1)
    lbu a0, 18(t5)
    li s11, 56
    bne a0, t0, .Lexit
    # d_reclen 必须等于该名字连同 NUL 的8字节对齐长度。
    sub t0, a1, t5
    addi t0, t0, 8
    li t4, -8
    and t0, t0, t4
    li s11, 57
    bne t0, t2, .Lexit
    ld t0, 0(t5)
    li t1, 1
    beq t3, t1, .Ldot_inode
    li t1, 4
    beq t3, t1, .Lfile_inode
    li t1, 8
    beq t3, t1, .Lfile_inode
    li t1, 16
    beq t3, t1, .Llink_inode
    j .Lnext_dirent
.Ldot_inode:
    li s11, 58
    bne t0, s4, .Lexit
    j .Lnext_dirent
.Lfile_inode:
    li s11, 59
    bne t0, s2, .Lexit
    j .Lnext_dirent
.Llink_inode:
    li s11, 60
    bne t0, s5, .Lexit
.Lnext_dirent:
    add t5, t5, t2
    bltu t5, t6, .Ldirent
    mv a0, s1
    li a1, 0
    li a2, 1
    SC 62
    li s11, 61
    bne a0, s7, .Lexit
    j .Lgetdents
.Ldirectory_done:
    mv a0, s6
    EXPECT 31, 62
    FD_SC s1, 57, 63
    li s11, 0
.Lexit:
    mv a0, s11
    SC 94
    j .Lexit
.size _start, .-_start

# /// 同一 inode 的三个只读路径必须具有完整8229字节、两处共享修改和链接数2。
# /// s2/s3保存首个结果的 inode/dev；stat中 mode/nlink 为独立32位字段。
.type .Lverify_file, @function
.Lverify_file:
    mv a0, s0
    lla a1, .Lstat
    SC 80
    EXPECT 0, 33
    lla t0, .Lstat
    ld t1, 8(t0)
    ld t2, 0(t0)
    li s11, 34
    beq t1, zero, .Lexit
    beq t2, zero, .Lexit
    bne s2, zero, .Lcompare_identity
    mv s2, t1
    mv s3, t2
.Lcompare_identity:
    bne t1, s2, .Lexit
    bne t2, s3, .Lexit
    lwu a0, 16(t0)
    li t1, 0xf000
    and a0, a0, t1
    EXPECT 0x8000, 35
    lla t0, .Lstat
    lwu a0, 20(t0)
    EXPECT 2, 36
    lla t0, .Lstat
    ld a0, 48(t0)
    EXPECT 8229, 37
    mv a0, s0
    lla a1, .Lbuffer
    li a2, 8229
    li a3, 0
    SC 67
    EXPECT 8229, 38
    lla t0, .Lbuffer
    li t1, 0
    li t2, 71
    li t6, 8229
    li s11, 39
.Lcheck_bytes:
    andi t3, t2, 255
    li t4, 17
    bne t1, t4, .Lcheck_second
    li t3, 0x79
.Lcheck_second:
    li t4, 4127
    bne t1, t4, .Lcheck_byte
    li t3, 0x53
.Lcheck_byte:
    lbu t4, 0(t0)
    bne t4, t3, .Lexit
    addi t0, t0, 1
    addi t1, t1, 1
    addi t2, t2, 13
    bltu t1, t6, .Lcheck_bytes
    mv a0, s0
    lla a1, .Lbuffer
    li a2, 1
    li a3, 8229
    SC 67
    EXPECT 0, 40
    mv a0, s0
    li a1, 0
    li a2, 1
    SC 62
    EXPECT 0, 41
    ret
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
