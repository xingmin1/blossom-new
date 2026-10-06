.text
.option norelax
.option norvc

# f/v/s/t 分别选择 fork、VM、VM+SIGHAND、VM+SIGHAND+THREAD。
# 第二字符 0..3 的 bit0/bit1 独立选择 FILES/FS；大写首字符使用坏 parent_tid。
# r/R 仍按fork创建，另外检查child最低空fd0，配合另一真实Task持父预留槽。
# 所有协调字均在共享匿名页中；状态发表前 Release，观察状态后 Acquire。
.macro expect reg, value, status
    li s8, \status
    li t2, \value
    bne \reg, t2, .Lexit
.endm

# /// 直接从初始 argv 选择资源组合，经真实 raw clone 检查父子状态与资源行为。
.global _start
.type _start, @function
_start:
    li s1, 0
    li s8, 1
    andi t0, sp, 15
    bnez t0, .Lexit
    ld t0, 0(sp)
    expect t0, 2, 1
    ld t0, 16(sp)
    beqz t0, .Lexit
    lbu t1, 2(t0)
    bnez t1, .Lexit
    lbu s7, 1(t0)
    addi s7, s7, -48
    li t1, 4
    bgeu s7, t1, .Lexit
    lbu t1, 0(t0)
    andi t0, t1, 32
    bnez t0, .Lmode_lower
    ori s7, s7, 4
.Lmode_lower:
    ori t1, t1, 32
    li s0, 17
    li t0, 102
    beq t1, t0, .Lmode_ready
    li t0, 114
    bne t1, t0, .Lmode_nonreserved
    ori s7, s7, 8
    j .Lmode_ready
.Lmode_nonreserved:
    li s0, 0x111
    li t0, 118
    beq t1, t0, .Lmode_ready
    li s0, 0x911
    li t0, 115
    beq t1, t0, .Lmode_ready
    li s0, 0x10900
    li t0, 116
    bne t1, t0, .Lexit
.Lmode_ready:
    andi t0, s7, 1
    slli t0, t0, 10
    or s0, s0, t0
    andi t0, s7, 2
    slli t0, t0, 8
    or s0, s0, t0
    li t0, 0x1180000
    or s0, s0, t0
    mv s4, sp
    mv s5, sp

    li s8, 2
    li a0, 0
    li a1, 8192
    li a2, 3
    li a3, 0x21
    li a4, -1
    li a5, 0
    li a7, 222
    ecall
    bltz a0, .Lexit
    mv s1, a0
    # 映射首字已触页，fork 后协调仍必须共享同一匿名后备。
    sw zero, 0(s1)
    sw zero, 4(s1)
    sw zero, 8(s1)
    sw zero, 12(s1)
    li s8, 3
    li a0, 0
    li a1, 12288
    li a2, 3
    li a3, 0x22
    li a4, -1
    li a5, 0
    li a7, 222
    ecall
    bltz a0, .Lexit
    mv s2, a0
    li t0, 0xaa
    sb t0, 0(s2)
    li t1, 4096
    add t1, s2, t1
    sb t0, 0(t1)
    li t1, 8192
    add t1, s2, t1
    sw zero, 0(t1)
    andi t0, s0, 0x100
    beqz t0, .Lstack_ready
    li s8, 4
    li a0, 0
    li a1, 8192
    li a2, 3
    li a3, 0x22
    li a4, -1
    li a5, 0
    li a7, 222
    ecall
    bltz a0, .Lexit
    li t0, 8192
    add s5, a0, t0
.Lstack_ready:
    lla a0, .Ltest_dir
    li a7, 49
    ecall
    expect a0, 0, 5
    li a0, -100
    lla a1, .Ldata_name
    li a2, 0
    li a3, 0
    li a7, 56
    ecall
    li s8, 6
    bltz a0, .Lexit
    mv s3, a0
    li a1, 2
    li a2, 1
    li a7, 25
    ecall
    expect a0, 0, 7
    mv a0, s3
    li a7, 23
    ecall
    li s8, 8
    bltz a0, .Lexit
    sw a0, 32(s1)
    li a0, 10
    lla a1, .Linitial_action
    li a2, 0
    li a3, 8
    li a7, 134
    ecall
    expect a0, 0, 9
    li a7, 172
    ecall
    li s8, 10
    blez a0, .Lexit
    sw a0, 16(s1)
    li a7, 178
    ecall
    li s8, 11
    blez a0, .Lexit
    sw a0, 20(s1)
    li tp, 0x12345000

    # RV raw 次序为 flags, stack, parent_tid, tls, child_tid。
    mv a0, s0
    li a1, 0
    andi t0, s0, 0x100
    beqz t0, .Lclone_stack
    mv a1, s5
.Lclone_stack:
    addi a2, s1, 12
    andi t0, s7, 4
    beqz t0, .Lclone_ptid
    li a2, 1
.Lclone_ptid:
    li a3, 0x23456000
    li t0, 8192
    add a4, s2, t0
    li a7, 220
    ecall
    li s8, 12
    bltz a0, .Lexit
    beqz a0, .Lchild
    mv s6, a0
    li s8, 13
    bne sp, s4, .Lexit
    expect tp, 0x12345000, 14
    andi t0, s7, 4
    bnez t0, .Lparent_bad_ptid
    lw t0, 12(s1)
    li s8, 15
    bne t0, s6, .Lexit
    j .Lparent_identity
.Lparent_bad_ptid:
    lw t0, 12(s1)
    expect t0, 0, 15
.Lparent_identity:
    mv a0, s1
    li a1, 1
    call .Lwait_word
    bnez a0, .Lwait_failed
    # 第二共享页在fork前未触页，必须由child首次硬件写入后在父看到同一内容。
    li t0, 4096
    add t0, s1, t0
    lbu t0, 0(t0)
    expect t0, 0x5e, 34
    lw t0, 24(s1)
    li s8, 16
    bne t0, s6, .Lexit
    li t0, 8192
    add t0, s2, t0
    lw t0, 0(t0)
    li t1, 0
    andi t2, s0, 0x100
    beqz t2, .Lparent_tid_check
    mv t1, s6
.Lparent_tid_check:
    li s8, 17
    bne t0, t1, .Lexit
    li t0, 1
    fence rw, w
    sw t0, 4(s1)
    mv a0, s1
    li a1, 2
    call .Lwait_word
    bnez a0, .Lwait_failed

    li t1, 0xaa
    andi t0, s0, 0x100
    beqz t0, .Lparent_store_check
    li t1, 0xcc
.Lparent_store_check:
    lbu t0, 0(s2)
    li s8, 18
    bne t0, t1, .Lexit
    li t1, 0xaa
    andi t0, s0, 0x100
    beqz t0, .Lparent_copyout_check
    li t1, 0
.Lparent_copyout_check:
    li t0, 4096
    add t0, s2, t0
    lbu t0, 0(t0)
    li s8, 19
    bne t0, t1, .Lexit
    # 即使 FILES 复制，继承的 OFD 仍共享读位置；dup 槽始终保持打开。
    lw a0, 32(s1)
    li a1, 0
    li a2, 1
    li a7, 62
    ecall
    expect a0, 1, 20
    mv a0, s3
    li a1, 1
    li a2, 0
    li a7, 25
    ecall
    li t1, 1
    andi t0, s0, 0x400
    beqz t0, .Lparent_fdflags
    li t1, 0
.Lparent_fdflags:
    li s8, 21
    bne a0, t1, .Lexit
    li a0, -100
    lla a1, .Lfirst_name
    li a2, 0
    li a3, 0
    li a7, 56
    ecall
    andi t0, s0, 0x200
    bnez t0, .Lparent_fs_shared
    expect a0, -2, 22
    li a0, -100
    lla a1, .Ldata_name
    li a2, 0
    li a3, 0
    li a7, 56
    ecall
    li s8, 23
    bltz a0, .Lexit
    j .Lparent_fs_close
.Lparent_fs_shared:
    li s8, 22
    bltz a0, .Lexit
    mv t3, a0
    addi a1, s1, 40
    li a2, 1
    li a7, 63
    ecall
    expect a0, 1, 23
    lbu t0, 40(s1)
    expect t0, 102, 23
    mv a0, t3
.Lparent_fs_close:
    li a7, 57
    ecall
    expect a0, 0, 24
    li a0, 10
    li a1, 0
    addi a2, s1, 64
    li a3, 8
    li a7, 134
    ecall
    expect a0, 0, 25
    lla t0, .Linitial_action
    li t1, 0x800
    and t1, s0, t1
    beqz t1, .Lparent_action
    lla t0, .Lchild_action
.Lparent_action:
    ld t1, 0(t0)
    ld t2, 64(s1)
    li s8, 26
    bne t1, t2, .Lexit
    ld t1, 8(t0)
    ld t2, 72(s1)
    li s8, 27
    bne t1, t2, .Lexit
    ld t1, 16(t0)
    ld t2, 80(s1)
    li s8, 28
    bne t1, t2, .Lexit

    # 父也分别执行用户 store 与 read copyout，child 随后核对自己的内容。
    li t0, 0xbb
    sb t0, 0(s2)
    lw a0, 32(s1)
    li t0, 4096
    add a1, s2, t0
    li a2, 1
    li a7, 63
    ecall
    expect a0, 1, 29
    li t0, 4096
    add t0, s2, t0
    lbu t0, 0(t0)
    expect t0, 1, 30
    li t0, 2
    fence rw, w
    sw t0, 4(s1)
    mv a0, s1
    li a1, 3
    call .Lwait_word
    bnez a0, .Lwait_failed
    mv a0, s3
    li a1, 1
    li a2, 0
    li a7, 25
    ecall
    andi t0, s0, 0x400
    beqz t0, .Lparent_close_copied
    expect a0, -9, 31
    j .Lparent_finish
.Lparent_close_copied:
    expect a0, 1, 31
    mv a0, s3
    li a7, 57
    ecall
    expect a0, 0, 32
.Lparent_finish:
    lw a0, 32(s1)
    li a7, 57
    ecall
    expect a0, 0, 33
    li t0, 3
    fence rw, w
    sw t0, 4(s1)
    # 完成字仅说明用户检查结束；真正 child 退出及离栈 join 由内核观察者消费。
    li s8, 0
    j .Lexit

.Lchild:
    li s8, 40
    bne sp, s5, .Lexit
    expect tp, 0x23456000, 41
    li a7, 178
    ecall
    li s8, 42
    blez a0, .Lexit
    lw t0, 20(s1)
    beq a0, t0, .Lexit
    mv s6, a0
    sw a0, 24(s1)
    li t0, 8192
    add t0, s2, t0
    lw t0, 0(t0)
    li s8, 43
    bne t0, s6, .Lexit
    li a7, 172
    ecall
    li s8, 44
    blez a0, .Lexit
    lw t0, 16(s1)
    li t1, 0x10000
    and t1, s0, t1
    bnez t1, .Lchild_thread
    bne a0, s6, .Lexit
    beq a0, t0, .Lexit
    j .Lchild_ready
.Lchild_thread:
    bne a0, t0, .Lexit
.Lchild_ready:
    andi t0, s7, 8
    beqz t0, .Lreserved_checked
    li a0, -100
    lla a1, .Ldata_name
    li a2, 0
    li a3, 0
    li a7, 56
    ecall
    expect a0, 0, 68
    li a7, 57
    ecall
    expect a0, 0, 69
.Lreserved_checked:
    li t0, 4096
    add t0, s1, t0
    li t1, 0x5e
    sb t1, 0(t0)
    li t0, 1
    fence rw, w
    sw t0, 0(s1)
    addi a0, s1, 4
    li a1, 1
    call .Lwait_word
    bnez a0, .Lwait_failed
    li t0, 0xcc
    sb t0, 0(s2)
    lbu t0, 0(s2)
    expect t0, 0xcc, 45
    mv a0, s3
    li t0, 4096
    add a1, s2, t0
    li a2, 1
    li a7, 63
    ecall
    expect a0, 1, 46
    li t0, 4096
    add t0, s2, t0
    lbu t0, 0(t0)
    expect t0, 0, 47
    mv a0, s3
    li a1, 1
    li a2, 0
    li a7, 25
    ecall
    expect a0, 1, 48
    mv a0, s3
    li a1, 2
    li a2, 0
    li a7, 25
    ecall
    expect a0, 0, 49
    lla a0, .Lchild_dir
    li a7, 49
    ecall
    expect a0, 0, 50
    li a0, 10
    lla a1, .Lchild_action
    addi a2, s1, 96
    li a3, 8
    li a7, 134
    ecall
    expect a0, 0, 51
    ld t0, 96(s1)
    expect t0, 0, 52
    ld t0, 104(s1)
    expect t0, 0x10000000, 53
    ld t0, 112(s1)
    expect t0, 0x800, 54
    # 安装后再读取完整动作，防止仅检查输入回显或漏存 flags/mask。
    li a0, 10
    li a1, 0
    addi a2, s1, 96
    li a3, 8
    li a7, 134
    ecall
    expect a0, 0, 55
    ld t0, 96(s1)
    expect t0, 1, 56
    ld t0, 104(s1)
    expect t0, 0x40000000, 57
    ld t0, 112(s1)
    expect t0, 0x4000, 58
    li t0, 2
    fence rw, w
    sw t0, 0(s1)
    addi a0, s1, 4
    li a1, 2
    call .Lwait_word
    bnez a0, .Lwait_failed
    li t1, 0xcc
    andi t0, s0, 0x100
    beqz t0, .Lchild_store_check
    li t1, 0xbb
.Lchild_store_check:
    lbu t0, 0(s2)
    li s8, 59
    bne t0, t1, .Lexit
    li t1, 0
    andi t0, s0, 0x100
    beqz t0, .Lchild_copyout_check
    li t1, 1
.Lchild_copyout_check:
    li t0, 4096
    add t0, s2, t0
    lbu t0, 0(t0)
    li s8, 60
    bne t0, t1, .Lexit
    mv a0, s3
    li a7, 57
    ecall
    expect a0, 0, 61
    li t0, 3
    fence rw, w
    sw t0, 0(s1)
    addi a0, s1, 4
    li a1, 3
    call .Lwait_word
    bnez a0, .Lwait_failed
    li s8, 0
    j .Lexit
.Lwait_failed:
    mv s8, a0
.Lexit:
    beqz s8, .Lexit_syscall
    beqz s1, .Lexit_syscall
    sw s8, 8(s1)
    li t0, -1
    fence rw, w
    sw t0, 0(s1)
    sw t0, 4(s1)
.Lexit_syscall:
    mv a0, s8
    li a7, 93
    ecall
    .word 0
.size _start, .-_start

# /// Acquire 观察指定共享字，至多一百万次真实 yield，失败返回非零退出状态。
.type .Lwait_word, @function
.Lwait_word:
    mv t4, a0
    mv t5, a1
    li t3, 1000000
.Lwait_next:
    lw t0, 0(t4)
    fence r, rw
    bltz t0, .Lwait_peer_failed
    beq t0, t5, .Lwait_ok
    addi t3, t3, -1
    beqz t3, .Lwait_timeout
    li a7, 124
    ecall
    bnez a0, .Lwait_yield_failed
    j .Lwait_next
.Lwait_peer_failed:
    lw a0, 8(s1)
    bnez a0, .Lwait_return
    li a0, 82
    ret
.Lwait_timeout:
    li a0, 80
    ret
.Lwait_yield_failed:
    li a0, 81
    ret
.Lwait_ok:
    li a0, 0
.Lwait_return:
    ret
.size .Lwait_word, .-.Lwait_word

.section .rodata, "a", @progbits

.Ltest_dir: .asciz "/test"
.Lchild_dir: .asciz "/test/io-dir"
.Ldata_name: .asciz "io-data.bin"
.Lfirst_name: .asciz "first"
.balign 8
# RV/LA 均为 handler、flags、mask 三个 u64，不含 restorer。
.Linitial_action: .quad 0, 0x10000000, 0x800
.Lchild_action: .quad 1, 0x40000000, 0x4000
