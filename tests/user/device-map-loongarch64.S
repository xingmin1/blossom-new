.text

# /// 从真实字符设备打开实例请求映射，核能力错误与访问模式的优先级。
.macro CHECK path, flags, prot, mapping, error, status
    li.d $s1, \status
    li.d $a0, -100
    la.local $a1, \path
    li.d $a2, \flags
    li.d $a3, 0
    li.d $a7, 56
    syscall 0
    bltz $a0, .Lfailed
    move $s0, $a0
    li.d $a0, 0
    li.d $a1, 4096
    li.d $a2, \prot
    li.d $a3, \mapping
    move $a4, $s0
    li.d $a5, 0
    li.d $a7, 222
    syscall 0
    li.d $t0, \error
    bne $a0, $t0, .Lfailed
    move $a0, $s0
    li.d $a7, 57
    syscall 0
    bnez $a0, .Lfailed
.endm

# /// SEEK_SET负编码由具体设备或普通文件决定，不在syscall提前改写错误。
.macro CHECK_SEEK path, error, status
    li.d $s1, \status
    li.d $a0, -100
    la.local $a1, \path
    li.d $a2, 0
    li.d $a3, 0
    li.d $a7, 56
    syscall 0
    bltz $a0, .Lfailed
    move $s0, $a0
    li.d $a1, -1
    li.d $a2, 0
    li.d $a7, 62
    syscall 0
    li.d $t0, \error
    bne $a0, $t0, .Lfailed
    move $a0, $s0
    li.d $a7, 57
    syscall 0
    bnez $a0, .Lfailed
.endm

# /// 普通ELF只以退出码交付真实syscall断言结果，不调用测试专用接口。
.global _start
.type _start, @function
_start:
    CHECK .Lnull, 2, 3, 1, -19, 1
    CHECK .Lzero, 2, 3, 1, -19, 2
    CHECK .Lnull, 0, 3, 1, -13, 3
    CHECK .Lzero, 1, 1, 2, -13, 4
    CHECK_SEEK .Lnull, 0, 5
    CHECK_SEEK .Lzero, 0, 6
    CHECK_SEEK .Lregular, -22, 7
    li.d $s1, 0
.Lfailed:
    move $a0, $s1
    li.d $a7, 93
    syscall 0
    break 0
.size _start, .-_start

.section .rodata, "a", @progbits
.Lnull: .asciz "/plain/null"
.Lzero: .asciz "/plain/zero"
.Lregular: .asciz "/pattern"

.data
.byte 0x51
.bss
.byte 0
