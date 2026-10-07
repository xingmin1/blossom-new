.text
.option norelax
.option norvc

# /// 普通目标 syscall 约定，返回值和具体 errno 均由 a0 消费。
.macro SC number
    li a7, \number
    ecall
.endm
# /// 每个断言点保留独立退出码，分支只跨一条远跳。
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
# /// openat 只消费本程序实际字节路径，不创建或截断文件。
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

# /// argv 仅选择具体写、append 或真实 exec 交接，不使用测试 syscall。
.global _start
.type _start, @function
_start:
    li s11, 1
    ld t0, 0(sp)
    li t1, 2
    bltu t0, t1, .Lexit
    ld t0, 16(sp)
    lbu t0, 0(t0)
    li t1, 'w'
    beq t0, t1, .Lwrite
    li t1, 'a'
    beq t0, t1, .Lappend
    li t1, 'e'
    beq t0, t1, .Lexec
    li t1, 'n'
    beq t0, t1, .Lchild_image
    li t1, 'o'
    beq t0, t1, .Lold_released
    j .Lexit

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
    bne a0, s0, .Lexit
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
