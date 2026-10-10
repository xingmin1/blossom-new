.text
.option norelax
.option norvc

# /// 从真实字符设备打开实例请求映射，核能力错误与访问模式的优先级。
.macro CHECK path, flags, prot, mapping, error, status
    li s1, \status
    li a0, -100
    lla a1, \path
    li a2, \flags
    li a3, 0
    li a7, 56
    ecall
    bltz a0, .Lfailed
    mv s0, a0
    li a0, 0
    li a1, 4096
    li a2, \prot
    li a3, \mapping
    mv a4, s0
    li a5, 0
    li a7, 222
    ecall
    li t0, \error
    bne a0, t0, .Lfailed
    mv a0, s0
    li a7, 57
    ecall
    bnez a0, .Lfailed
.endm

# /// SEEK_SET负编码由具体设备或普通文件决定，不在syscall提前改写错误。
.macro CHECK_SEEK path, error, status
    li s1, \status
    li a0, -100
    lla a1, \path
    li a2, 0
    li a3, 0
    li a7, 56
    ecall
    bltz a0, .Lfailed
    mv s0, a0
    li a1, -1
    li a2, 0
    li a7, 62
    ecall
    li t0, \error
    bne a0, t0, .Lfailed
    mv a0, s0
    li a7, 57
    ecall
    bnez a0, .Lfailed
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
    li s1, 0
.Lfailed:
    mv a0, s1
    li a7, 93
    ecall
    unimp
.size _start, .-_start

.section .rodata, "a", @progbits
.Lnull: .asciz "/plain/null"
.Lzero: .asciz "/plain/zero"
.Lregular: .asciz "/pattern"

.data
.byte 0x51
.bss
.byte 0
