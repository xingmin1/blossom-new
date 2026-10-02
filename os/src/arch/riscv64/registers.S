.option arch, +d
.section .text,"ax"
.balign 4
.global rv_register_roundtrip
rv_register_roundtrip:
    addi sp, sp, -112
    sd ra, 0(sp)
    sd s0, 8(sp)
    sd s1, 16(sp)
    sd s2, 24(sp)
    sd s3, 32(sp)
    sd s4, 40(sp)
    sd s5, 48(sp)
    csrr t0, sstatus
    sd t0, 56(sp)
    mv s0, sp
    mv s1, tp
    mv s3, a0
    mv s4, a1
    mv s5, a2
    beqz s5, 1f
    li t0, 0x6000
    csrs sstatus, t0
    frcsr t0
    sd t0, 64(sp)
    li t0, 0x61
    fscsr t0
    li t0, 0x3ff123456789abcd
    fmv.d.x f0, t0
    li t0, 0x400fedcba9876543
    fmv.d.x f31, t0
    j 2f
1:  li t0, 0x6000
    csrc sstatus, t0
2:  li t0, 0x123456789abcdef
    li t1, 0x76543210fedcba98
    li a3, 0x13579bdf2468ace0
    li s2, 0x1122334455667788
    csrsi sstatus, 2
3:  ld a0, 0(s3)
    fence r, r
    bltu a0, s4, 3b
    csrci sstatus, 2
    li a0, 0
    bne sp, s0, 9f
    bne tp, s1, 9f
    li a1, 0x123456789abcdef
    bne t0, a1, 9f
    li a1, 0x76543210fedcba98
    bne t1, a1, 9f
    li a1, 0x13579bdf2468ace0
    bne a3, a1, 9f
    li a1, 0x1122334455667788
    bne s2, a1, 9f
    beqz s5, 8f
    fmv.x.d a1, f0
    li a2, 0x3ff123456789abcd
    bne a1, a2, 9f
    fmv.x.d a1, f31
    li a2, 0x400fedcba9876543
    bne a1, a2, 9f
    frcsr a1
    li a2, 0x61
    bne a1, a2, 9f
8:  li a0, 1
9:  beqz s5, 10f
    ld t0, 64(sp)
    fscsr t0
10: ld t0, 56(sp)
    csrw sstatus, t0
    ld ra, 0(sp)
    ld s0, 8(sp)
    ld s1, 16(sp)
    ld s2, 24(sp)
    ld s3, 32(sp)
    ld s4, 40(sp)
    ld s5, 48(sp)
    addi sp, sp, 112
    ret
