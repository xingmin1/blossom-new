.text
# /// 只观察唯一clone桥两侧的SP/TP；syscall和子路径仍归共享probe_clone。
.global clone_state
.type clone_state, @function
clone_state:
    addi.d $sp, $sp, -16
    st.d $ra, $sp, 0
    st.d $s0, $sp, 8
    move $s0, $a6
    st.d $sp, $s0, 0
    li.d $tp, 0x12345000
    bl probe_clone
    st.d $sp, $s0, 8
    st.d $tp, $s0, 16
    ld.d $ra, $sp, 0
    ld.d $s0, $sp, 8
    addi.d $sp, $sp, 16
    jr $ra
.size clone_state, .-clone_state
.section .note.GNU-stack,"",@progbits
