.pushsection .rodata.user_elf_direct,"a",@progbits
.balign 8
.global user_elf_direct_start
.global user_elf_direct_end
user_elf_direct_start:
    # 首两条指令后保存表相对当前PC的位移，复制到任意页内位置后仍可直接定位。
    pcaddi $t0, 0
    b .Ldirect_body
    .dword user_elf_direct_end - user_elf_direct_start
.Ldirect_body:
    ld.d $t1, $t0, 8
    add.d $t0, $t0, $t1
    ld.d $t1, $t0, 0
    addi.d $t0, $t0, 8
1:
    beqz $t1, 2f
    ld.d $t2, $t0, 0
    ld.bu $t3, $t0, 8
    ld.bu $t4, $t2, 0
    bne $t3, $t4, 3f
    ld.bu $t3, $t0, 9
    st.b $t3, $t2, 0
    ld.bu $t4, $t2, 0
    bne $t3, $t4, 3f
    addi.d $t0, $t0, 16
    addi.d $t1, $t1, -1
    b 1b
2:
    li.d $a0, 37
    b 4f
3:
    li.d $a0, 38
4:
    li.d $a7, 93
    syscall 0
    .word 0
.balign 8
user_elf_direct_end:
.popsection
