.section .text,"ax"
.balign 16
.global __test_registers
__test_registers:
    addi.d $sp, $sp, -192
    st.d $ra, $sp, 0
    st.d $s0, $sp, 8
    st.d $s1, $sp, 16
    csrrd $t2, 2
    st.d $t2, $sp, 24
    csrrd $t2, 0
    st.d $t2, $sp, 32
    st.d $a0, $sp, 40
    ld.d $t2, $a0, 0
    st.d $t2, $sp, 48
    st.d $a1, $sp, 56
    st.d $a2, $sp, 64
    st.d $a3, $sp, 72
    csrrd $t2, 0x30
    st.d $t2, $sp, 80
    st.d $sp, $sp, 88
    beqz $a3, .Ltest_no_fp
    li.w $t2, 1
    csrwr $t2, 2
    fst.d $f0, $sp, 96
    fst.d $f24, $sp, 104
    movfcsr2gr $t2, $fcsr0
    st.d $t2, $sp, 112
    movcf2gr $t2, $fcc0
    st.d $t2, $sp, 120
    movcf2gr $t2, $fcc1
    st.d $t2, $sp, 128
    movcf2gr $t2, $fcc2
    st.d $t2, $sp, 136
    movcf2gr $t2, $fcc3
    st.d $t2, $sp, 144
    movcf2gr $t2, $fcc4
    st.d $t2, $sp, 152
    movcf2gr $t2, $fcc5
    st.d $t2, $sp, 160
    movcf2gr $t2, $fcc6
    st.d $t2, $sp, 168
    movcf2gr $t2, $fcc7
    st.d $t2, $sp, 176
    li.d $t2, 0x3ff123456789abcd
    movgr2fr.d $f0, $t2
    li.d $t2, 0x400fedcba9876543
    movgr2fr.d $f24, $t2
    li.w $t2, 0x100
    movgr2fcsr $fcsr0, $t2
    srli.d $t2, $a2, 0
    andi $t2, $t2, 1
    movgr2cf $fcc0, $t2
    srli.d $t2, $a2, 1
    andi $t2, $t2, 1
    movgr2cf $fcc1, $t2
    srli.d $t2, $a2, 2
    andi $t2, $t2, 1
    movgr2cf $fcc2, $t2
    srli.d $t2, $a2, 3
    andi $t2, $t2, 1
    movgr2cf $fcc3, $t2
    srli.d $t2, $a2, 4
    andi $t2, $t2, 1
    movgr2cf $fcc4, $t2
    srli.d $t2, $a2, 5
    andi $t2, $t2, 1
    movgr2cf $fcc5, $t2
    srli.d $t2, $a2, 6
    andi $t2, $t2, 1
    movgr2cf $fcc6, $t2
    srli.d $t2, $a2, 7
    andi $t2, $t2, 1
    movgr2cf $fcc7, $t2
    b .Ltest_arm
.Ltest_no_fp:
    csrwr $zero, 2
.Ltest_arm:
    li.d $t0, 0x123
    li.d $t1, 0x456
    li.d $s0, 0x789
    li.d $s1, 0xabc
    li.d $a2, 0xdef
    ld.d $t2, $sp, 56
    ori $t2, $t2, 1
    csrwr $t2, 0x41
    li.w $t2, 4
    li.w $t3, 4
    csrxchg $t2, $t3, 0
.Ltest_wait:
    ld.d $t2, $sp, 40
    ld.d $t2, $t2, 0
    ld.d $t3, $sp, 48
    beq $t2, $t3, .Ltest_wait
    li.w $t2, 4
    csrxchg $zero, $t2, 0
    li.d $t4, 0x123
    bne $t0, $t4, .Ltest_bad
    li.d $t4, 0x456
    bne $t1, $t4, .Ltest_bad
    li.d $t4, 0x789
    bne $s0, $t4, .Ltest_bad
    li.d $t4, 0xabc
    bne $s1, $t4, .Ltest_bad
    li.d $t4, 0xdef
    bne $a2, $t4, .Ltest_bad
    ld.d $t4, $sp, 88
    bne $sp, $t4, .Ltest_bad
    csrrd $t4, 0x30
    ld.d $t5, $sp, 80
    bne $t4, $t5, .Ltest_bad
    ld.d $t4, $sp, 72
    beqz $t4, .Ltest_check_no_fp
    movfr2gr.d $t4, $f0
    li.d $t5, 0x3ff123456789abcd
    bne $t4, $t5, .Ltest_bad
    movfr2gr.d $t4, $f24
    li.d $t5, 0x400fedcba9876543
    bne $t4, $t5, .Ltest_bad
    movfcsr2gr $t4, $fcsr0
    li.w $t5, 0x100
    bne $t4, $t5, .Ltest_bad
    move $t4, $zero
    movcf2gr $t5, $fcc0
    slli.d $t5, $t5, 0
    or $t4, $t4, $t5
    movcf2gr $t5, $fcc1
    slli.d $t5, $t5, 1
    or $t4, $t4, $t5
    movcf2gr $t5, $fcc2
    slli.d $t5, $t5, 2
    or $t4, $t4, $t5
    movcf2gr $t5, $fcc3
    slli.d $t5, $t5, 3
    or $t4, $t4, $t5
    movcf2gr $t5, $fcc4
    slli.d $t5, $t5, 4
    or $t4, $t4, $t5
    movcf2gr $t5, $fcc5
    slli.d $t5, $t5, 5
    or $t4, $t4, $t5
    movcf2gr $t5, $fcc6
    slli.d $t5, $t5, 6
    or $t4, $t4, $t5
    movcf2gr $t5, $fcc7
    slli.d $t5, $t5, 7
    or $t4, $t4, $t5
    ld.d $t5, $sp, 64
    bne $t4, $t5, .Ltest_bad
    b .Ltest_good
.Ltest_check_no_fp:
    csrrd $t4, 2
    bnez $t4, .Ltest_bad
.Ltest_good:
    li.w $a0, 1
    b .Ltest_restore
.Ltest_bad:
    move $a0, $zero
.Ltest_restore:
    ld.d $t2, $sp, 72
    beqz $t2, .Ltest_restore_control
    li.w $t2, 1
    csrwr $t2, 2
    fld.d $f0, $sp, 96
    fld.d $f24, $sp, 104
    ld.d $t2, $sp, 112
    movgr2fcsr $fcsr0, $t2
    ld.d $t2, $sp, 120
    movgr2cf $fcc0, $t2
    ld.d $t2, $sp, 128
    movgr2cf $fcc1, $t2
    ld.d $t2, $sp, 136
    movgr2cf $fcc2, $t2
    ld.d $t2, $sp, 144
    movgr2cf $fcc3, $t2
    ld.d $t2, $sp, 152
    movgr2cf $fcc4, $t2
    ld.d $t2, $sp, 160
    movgr2cf $fcc5, $t2
    ld.d $t2, $sp, 168
    movgr2cf $fcc6, $t2
    ld.d $t2, $sp, 176
    movgr2cf $fcc7, $t2
.Ltest_restore_control:
    ld.d $t2, $sp, 24
    csrwr $t2, 2
    ld.d $t2, $sp, 80
    csrwr $t2, 0x30
    ld.d $t2, $sp, 32
    li.w $t3, 4
    csrxchg $t2, $t3, 0
    ld.d $ra, $sp, 0
    ld.d $s0, $sp, 8
    ld.d $s1, $sp, 16
    addi.d $sp, $sp, 192
    jirl $zero, $ra, 0
