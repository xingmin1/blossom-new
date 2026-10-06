.text

# f/v/s/t 分别选择 fork、VM、VM+SIGHAND、VM+SIGHAND+THREAD。
# 第二字符 0..3 的 bit0/bit1 独立选择 FILES/FS；大写首字符使用坏 parent_tid。
# r/R 仍按fork创建，另外检查child最低空fd0，配合另一真实Task持父预留槽。
# 所有协调字均在共享匿名页中；状态发表前 Release，观察状态后 Acquire。
.macro expect reg, value, status
    li.w $s8, \status
    li.w $t2, \value
    bne \reg, $t2, .Lexit
.endm

# /// 直接从初始 argv 选择资源组合，经真实 raw clone 检查父子状态与资源行为。
.global _start
.type _start, @function
_start:
    li.w $s1, 0
    li.w $s8, 1
    andi $t0, $sp, 15
    bnez $t0, .Lexit
    ld.d $t0, $sp, 0
    expect $t0, 2, 1
    ld.d $t0, $sp, 16
    beqz $t0, .Lexit
    ld.bu $t1, $t0, 2
    bnez $t1, .Lexit
    ld.bu $s7, $t0, 1
    addi.d $s7, $s7, -48
    li.w $t1, 4
    bgeu $s7, $t1, .Lexit
    ld.bu $t1, $t0, 0
    andi $t0, $t1, 32
    bnez $t0, .Lmode_lower
    ori $s7, $s7, 4
.Lmode_lower:
    ori $t1, $t1, 32
    li.w $s0, 17
    li.w $t0, 102
    beq $t1, $t0, .Lmode_ready
    li.w $t0, 114
    bne $t1, $t0, .Lmode_nonreserved
    ori $s7, $s7, 8
    b .Lmode_ready
.Lmode_nonreserved:
    li.w $s0, 0x111
    li.w $t0, 118
    beq $t1, $t0, .Lmode_ready
    li.w $s0, 0x911
    li.w $t0, 115
    beq $t1, $t0, .Lmode_ready
    li.w $s0, 0x10900
    li.w $t0, 116
    bne $t1, $t0, .Lexit
.Lmode_ready:
    andi $t0, $s7, 1
    slli.d $t0, $t0, 10
    or $s0, $s0, $t0
    andi $t0, $s7, 2
    slli.d $t0, $t0, 8
    or $s0, $s0, $t0
    li.w $t0, 0x1180000
    or $s0, $s0, $t0
    move $s4, $sp
    move $s5, $sp

    li.w $s8, 2
    li.w $a0, 0
    li.w $a1, 8192
    li.w $a2, 3
    li.w $a3, 0x21
    li.w $a4, -1
    li.w $a5, 0
    li.w $a7, 222
    syscall 0
    blt $a0, $zero, .Lexit
    move $s1, $a0
    # 映射首字已触页，fork 后协调仍必须共享同一匿名后备。
    st.w $zero, $s1, 0
    st.w $zero, $s1, 4
    st.w $zero, $s1, 8
    st.w $zero, $s1, 12
    li.w $s8, 3
    li.w $a0, 0
    li.w $a1, 12288
    li.w $a2, 3
    li.w $a3, 0x22
    li.w $a4, -1
    li.w $a5, 0
    li.w $a7, 222
    syscall 0
    blt $a0, $zero, .Lexit
    move $s2, $a0
    li.w $t0, 0xaa
    st.b $t0, $s2, 0
    li.w $t1, 4096
    add.d $t1, $s2, $t1
    st.b $t0, $t1, 0
    li.w $t1, 8192
    add.d $t1, $s2, $t1
    st.w $zero, $t1, 0
    andi $t0, $s0, 0x100
    beqz $t0, .Lstack_ready
    li.w $s8, 4
    li.w $a0, 0
    li.w $a1, 8192
    li.w $a2, 3
    li.w $a3, 0x22
    li.w $a4, -1
    li.w $a5, 0
    li.w $a7, 222
    syscall 0
    blt $a0, $zero, .Lexit
    li.w $t0, 8192
    add.d $s5, $a0, $t0
.Lstack_ready:
    la.local $a0, .Ltest_dir
    li.w $a7, 49
    syscall 0
    expect $a0, 0, 5
    li.w $a0, -100
    la.local $a1, .Ldata_name
    li.w $a2, 0
    li.w $a3, 0
    li.w $a7, 56
    syscall 0
    li.w $s8, 6
    blt $a0, $zero, .Lexit
    move $s3, $a0
    li.w $a1, 2
    li.w $a2, 1
    li.w $a7, 25
    syscall 0
    expect $a0, 0, 7
    move $a0, $s3
    li.w $a7, 23
    syscall 0
    li.w $s8, 8
    blt $a0, $zero, .Lexit
    st.w $a0, $s1, 32
    li.w $a0, 10
    la.local $a1, .Linitial_action
    li.w $a2, 0
    li.w $a3, 8
    li.w $a7, 134
    syscall 0
    expect $a0, 0, 9
    li.w $a7, 172
    syscall 0
    li.w $s8, 10
    bge $zero, $a0, .Lexit
    st.w $a0, $s1, 16
    li.w $a7, 178
    syscall 0
    li.w $s8, 11
    bge $zero, $a0, .Lexit
    st.w $a0, $s1, 20
    li.w $tp, 0x12345000

    # LA raw 次序为 flags, stack, parent_tid, child_tid, tls。
    move $a0, $s0
    li.w $a1, 0
    andi $t0, $s0, 0x100
    beqz $t0, .Lclone_stack
    move $a1, $s5
.Lclone_stack:
    addi.d $a2, $s1, 12
    andi $t0, $s7, 4
    beqz $t0, .Lclone_ptid
    li.w $a2, 1
.Lclone_ptid:
    li.w $t0, 8192
    add.d $a3, $s2, $t0
    li.w $a4, 0x23456000
    li.w $a7, 220
    syscall 0
    li.w $s8, 12
    blt $a0, $zero, .Lexit
    beqz $a0, .Lchild
    move $s6, $a0
    li.w $s8, 13
    bne $sp, $s4, .Lexit
    expect $tp, 0x12345000, 14
    andi $t0, $s7, 4
    bnez $t0, .Lparent_bad_ptid
    ld.w $t0, $s1, 12
    li.w $s8, 15
    bne $t0, $s6, .Lexit
    b .Lparent_identity
.Lparent_bad_ptid:
    ld.w $t0, $s1, 12
    expect $t0, 0, 15
.Lparent_identity:
    move $a0, $s1
    li.w $a1, 1
    bl .Lwait_word
    bnez $a0, .Lwait_failed
    # 第二共享页在fork前未触页，必须由child首次硬件写入后在父看到同一内容。
    li.w $t0, 4096
    add.d $t0, $s1, $t0
    ld.bu $t0, $t0, 0
    expect $t0, 0x5e, 34
    ld.w $t0, $s1, 24
    li.w $s8, 16
    bne $t0, $s6, .Lexit
    li.w $t0, 8192
    add.d $t0, $s2, $t0
    ld.w $t0, $t0, 0
    li.w $t1, 0
    andi $t2, $s0, 0x100
    beqz $t2, .Lparent_tid_check
    move $t1, $s6
.Lparent_tid_check:
    li.w $s8, 17
    bne $t0, $t1, .Lexit
    li.w $t0, 1
    dbar 0
    st.w $t0, $s1, 4
    move $a0, $s1
    li.w $a1, 2
    bl .Lwait_word
    bnez $a0, .Lwait_failed

    li.w $t1, 0xaa
    andi $t0, $s0, 0x100
    beqz $t0, .Lparent_store_check
    li.w $t1, 0xcc
.Lparent_store_check:
    ld.bu $t0, $s2, 0
    li.w $s8, 18
    bne $t0, $t1, .Lexit
    li.w $t1, 0xaa
    andi $t0, $s0, 0x100
    beqz $t0, .Lparent_copyout_check
    li.w $t1, 0
.Lparent_copyout_check:
    li.w $t0, 4096
    add.d $t0, $s2, $t0
    ld.bu $t0, $t0, 0
    li.w $s8, 19
    bne $t0, $t1, .Lexit
    # 即使 FILES 复制，继承的 OFD 仍共享读位置；dup 槽始终保持打开。
    ld.w $a0, $s1, 32
    li.w $a1, 0
    li.w $a2, 1
    li.w $a7, 62
    syscall 0
    expect $a0, 1, 20
    move $a0, $s3
    li.w $a1, 1
    li.w $a2, 0
    li.w $a7, 25
    syscall 0
    li.w $t1, 1
    andi $t0, $s0, 0x400
    beqz $t0, .Lparent_fdflags
    li.w $t1, 0
.Lparent_fdflags:
    li.w $s8, 21
    bne $a0, $t1, .Lexit
    li.w $a0, -100
    la.local $a1, .Lfirst_name
    li.w $a2, 0
    li.w $a3, 0
    li.w $a7, 56
    syscall 0
    andi $t0, $s0, 0x200
    bnez $t0, .Lparent_fs_shared
    expect $a0, -2, 22
    li.w $a0, -100
    la.local $a1, .Ldata_name
    li.w $a2, 0
    li.w $a3, 0
    li.w $a7, 56
    syscall 0
    li.w $s8, 23
    blt $a0, $zero, .Lexit
    b .Lparent_fs_close
.Lparent_fs_shared:
    li.w $s8, 22
    blt $a0, $zero, .Lexit
    move $t3, $a0
    addi.d $a1, $s1, 40
    li.w $a2, 1
    li.w $a7, 63
    syscall 0
    expect $a0, 1, 23
    ld.bu $t0, $s1, 40
    expect $t0, 102, 23
    move $a0, $t3
.Lparent_fs_close:
    li.w $a7, 57
    syscall 0
    expect $a0, 0, 24
    li.w $a0, 10
    li.w $a1, 0
    addi.d $a2, $s1, 64
    li.w $a3, 8
    li.w $a7, 134
    syscall 0
    expect $a0, 0, 25
    la.local $t0, .Linitial_action
    li.w $t1, 0x800
    and $t1, $s0, $t1
    beqz $t1, .Lparent_action
    la.local $t0, .Lchild_action
.Lparent_action:
    ld.d $t1, $t0, 0
    ld.d $t2, $s1, 64
    li.w $s8, 26
    bne $t1, $t2, .Lexit
    ld.d $t1, $t0, 8
    ld.d $t2, $s1, 72
    li.w $s8, 27
    bne $t1, $t2, .Lexit
    ld.d $t1, $t0, 16
    ld.d $t2, $s1, 80
    li.w $s8, 28
    bne $t1, $t2, .Lexit

    # 父也分别执行用户 store 与 read copyout，child 随后核对自己的内容。
    li.w $t0, 0xbb
    st.b $t0, $s2, 0
    ld.w $a0, $s1, 32
    li.w $t0, 4096
    add.d $a1, $s2, $t0
    li.w $a2, 1
    li.w $a7, 63
    syscall 0
    expect $a0, 1, 29
    li.w $t0, 4096
    add.d $t0, $s2, $t0
    ld.bu $t0, $t0, 0
    expect $t0, 1, 30
    li.w $t0, 2
    dbar 0
    st.w $t0, $s1, 4
    move $a0, $s1
    li.w $a1, 3
    bl .Lwait_word
    bnez $a0, .Lwait_failed
    move $a0, $s3
    li.w $a1, 1
    li.w $a2, 0
    li.w $a7, 25
    syscall 0
    andi $t0, $s0, 0x400
    beqz $t0, .Lparent_close_copied
    expect $a0, -9, 31
    b .Lparent_finish
.Lparent_close_copied:
    expect $a0, 1, 31
    move $a0, $s3
    li.w $a7, 57
    syscall 0
    expect $a0, 0, 32
.Lparent_finish:
    ld.w $a0, $s1, 32
    li.w $a7, 57
    syscall 0
    expect $a0, 0, 33
    li.w $t0, 3
    dbar 0
    st.w $t0, $s1, 4
    # 完成字仅说明用户检查结束；真正 child 退出及离栈 join 由内核观察者消费。
    li.w $s8, 0
    b .Lexit

.Lchild:
    li.w $s8, 40
    bne $sp, $s5, .Lexit
    expect $tp, 0x23456000, 41
    li.w $a7, 178
    syscall 0
    li.w $s8, 42
    bge $zero, $a0, .Lexit
    ld.w $t0, $s1, 20
    beq $a0, $t0, .Lexit
    move $s6, $a0
    st.w $a0, $s1, 24
    li.w $t0, 8192
    add.d $t0, $s2, $t0
    ld.w $t0, $t0, 0
    li.w $s8, 43
    bne $t0, $s6, .Lexit
    li.w $a7, 172
    syscall 0
    li.w $s8, 44
    bge $zero, $a0, .Lexit
    ld.w $t0, $s1, 16
    li.w $t1, 0x10000
    and $t1, $s0, $t1
    bnez $t1, .Lchild_thread
    bne $a0, $s6, .Lexit
    beq $a0, $t0, .Lexit
    b .Lchild_ready
.Lchild_thread:
    bne $a0, $t0, .Lexit
.Lchild_ready:
    andi $t0, $s7, 8
    beqz $t0, .Lreserved_checked
    li.w $a0, -100
    la.local $a1, .Ldata_name
    li.w $a2, 0
    li.w $a3, 0
    li.w $a7, 56
    syscall 0
    expect $a0, 0, 68
    li.w $a7, 57
    syscall 0
    expect $a0, 0, 69
.Lreserved_checked:
    li.w $t0, 4096
    add.d $t0, $s1, $t0
    li.w $t1, 0x5e
    st.b $t1, $t0, 0
    li.w $t0, 1
    dbar 0
    st.w $t0, $s1, 0
    addi.d $a0, $s1, 4
    li.w $a1, 1
    bl .Lwait_word
    bnez $a0, .Lwait_failed
    li.w $t0, 0xcc
    st.b $t0, $s2, 0
    ld.bu $t0, $s2, 0
    expect $t0, 0xcc, 45
    move $a0, $s3
    li.w $t0, 4096
    add.d $a1, $s2, $t0
    li.w $a2, 1
    li.w $a7, 63
    syscall 0
    expect $a0, 1, 46
    li.w $t0, 4096
    add.d $t0, $s2, $t0
    ld.bu $t0, $t0, 0
    expect $t0, 0, 47
    move $a0, $s3
    li.w $a1, 1
    li.w $a2, 0
    li.w $a7, 25
    syscall 0
    expect $a0, 1, 48
    move $a0, $s3
    li.w $a1, 2
    li.w $a2, 0
    li.w $a7, 25
    syscall 0
    expect $a0, 0, 49
    la.local $a0, .Lchild_dir
    li.w $a7, 49
    syscall 0
    expect $a0, 0, 50
    li.w $a0, 10
    la.local $a1, .Lchild_action
    addi.d $a2, $s1, 96
    li.w $a3, 8
    li.w $a7, 134
    syscall 0
    expect $a0, 0, 51
    ld.d $t0, $s1, 96
    expect $t0, 0, 52
    ld.d $t0, $s1, 104
    expect $t0, 0x10000000, 53
    ld.d $t0, $s1, 112
    expect $t0, 0x800, 54
    # 安装后再读取完整动作，防止仅检查输入回显或漏存 flags/mask。
    li.w $a0, 10
    li.w $a1, 0
    addi.d $a2, $s1, 96
    li.w $a3, 8
    li.w $a7, 134
    syscall 0
    expect $a0, 0, 55
    ld.d $t0, $s1, 96
    expect $t0, 1, 56
    ld.d $t0, $s1, 104
    expect $t0, 0x40000000, 57
    ld.d $t0, $s1, 112
    expect $t0, 0x4000, 58
    li.w $t0, 2
    dbar 0
    st.w $t0, $s1, 0
    addi.d $a0, $s1, 4
    li.w $a1, 2
    bl .Lwait_word
    bnez $a0, .Lwait_failed
    li.w $t1, 0xcc
    andi $t0, $s0, 0x100
    beqz $t0, .Lchild_store_check
    li.w $t1, 0xbb
.Lchild_store_check:
    ld.bu $t0, $s2, 0
    li.w $s8, 59
    bne $t0, $t1, .Lexit
    li.w $t1, 0
    andi $t0, $s0, 0x100
    beqz $t0, .Lchild_copyout_check
    li.w $t1, 1
.Lchild_copyout_check:
    li.w $t0, 4096
    add.d $t0, $s2, $t0
    ld.bu $t0, $t0, 0
    li.w $s8, 60
    bne $t0, $t1, .Lexit
    move $a0, $s3
    li.w $a7, 57
    syscall 0
    expect $a0, 0, 61
    li.w $t0, 3
    dbar 0
    st.w $t0, $s1, 0
    addi.d $a0, $s1, 4
    li.w $a1, 3
    bl .Lwait_word
    bnez $a0, .Lwait_failed
    li.w $s8, 0
    b .Lexit
.Lwait_failed:
    move $s8, $a0
.Lexit:
    beqz $s8, .Lexit_syscall
    beqz $s1, .Lexit_syscall
    st.w $s8, $s1, 8
    li.w $t0, -1
    dbar 0
    st.w $t0, $s1, 0
    st.w $t0, $s1, 4
.Lexit_syscall:
    move $a0, $s8
    li.w $a7, 93
    syscall 0
    .word 0
.size _start, .-_start

# /// Acquire 观察指定共享字，至多一百万次真实 yield，失败返回非零退出状态。
.type .Lwait_word, @function
.Lwait_word:
    move $t4, $a0
    move $t5, $a1
    li.w $t3, 1000000
.Lwait_next:
    ld.w $t0, $t4, 0
    dbar 0
    blt $t0, $zero, .Lwait_peer_failed
    beq $t0, $t5, .Lwait_ok
    addi.d $t3, $t3, -1
    beqz $t3, .Lwait_timeout
    li.w $a7, 124
    syscall 0
    bnez $a0, .Lwait_yield_failed
    b .Lwait_next
.Lwait_peer_failed:
    ld.w $a0, $s1, 8
    bnez $a0, .Lwait_return
    li.w $a0, 82
    jr $ra
.Lwait_timeout:
    li.w $a0, 80
    jr $ra
.Lwait_yield_failed:
    li.w $a0, 81
    jr $ra
.Lwait_ok:
    li.w $a0, 0
.Lwait_return:
    jr $ra
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
