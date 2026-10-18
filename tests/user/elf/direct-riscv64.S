.pushsection .rodata.user_elf_direct,"a",@progbits
.balign 8
.global user_elf_direct_start
.global user_elf_direct_end
.option push
.option norelax
.option norvc
user_elf_direct_start:
    lla t0, user_elf_direct_end
    ld t1, 0(t0)
    addi t0, t0, 8
1:
    beqz t1, 2f
    ld t2, 0(t0)
    lbu t3, 8(t0)
    lbu t4, 0(t2)
    bne t3, t4, 3f
    lbu t3, 9(t0)
    sb t3, 0(t2)
    lbu t4, 0(t2)
    bne t3, t4, 3f
    addi t0, t0, 16
    addi t1, t1, -1
    j 1b
2:
    li a0, 37
    j 4f
3:
    li a0, 38
4:
    li a7, 93
    ecall
    unimp
.balign 8
user_elf_direct_end:
.option pop
.popsection
