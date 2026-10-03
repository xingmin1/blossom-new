.text

.balign 16
.global la_switch_context
.type la_switch_context, @function
la_switch_context:
    # 普通调用现场只含整数 callee-saved；KS0 留在目的 CPU，普通 tp 不承担 Task TLS。
    st.d $ra, $a0, {context_ra}
    st.d $sp, $a0, {context_sp}
    st.d $s0, $a0, {context_saved}+0*{word_size}
    st.d $s1, $a0, {context_saved}+1*{word_size}
    st.d $s2, $a0, {context_saved}+2*{word_size}
    st.d $s3, $a0, {context_saved}+3*{word_size}
    st.d $s4, $a0, {context_saved}+4*{word_size}
    st.d $s5, $a0, {context_saved}+5*{word_size}
    st.d $s6, $a0, {context_saved}+6*{word_size}
    st.d $s7, $a0, {context_saved}+7*{word_size}
    st.d $s8, $a0, {context_saved}+8*{word_size}
    st.d $fp, $a0, {context_saved}+9*{word_size}
    ld.d $ra, $a1, {context_ra}
    ld.d $sp, $a1, {context_sp}
    ld.d $s0, $a1, {context_saved}+0*{word_size}
    ld.d $s1, $a1, {context_saved}+1*{word_size}
    ld.d $s2, $a1, {context_saved}+2*{word_size}
    ld.d $s3, $a1, {context_saved}+3*{word_size}
    ld.d $s4, $a1, {context_saved}+4*{word_size}
    ld.d $s5, $a1, {context_saved}+5*{word_size}
    ld.d $s6, $a1, {context_saved}+6*{word_size}
    ld.d $s7, $a1, {context_saved}+7*{word_size}
    ld.d $s8, $a1, {context_saved}+8*{word_size}
    ld.d $fp, $a1, {context_saved}+9*{word_size}
    jirl $zero, $ra, 0
.size la_switch_context, .-la_switch_context

.if {execution_tests}
.balign 16
.global la_test_task_context_preservation
.type la_test_task_context_preservation, @function
la_test_task_context_preservation:
    # 保存调用者现场；96 字节保持普通函数 ABI 的 16 字节栈对齐。
    addi.d $sp, $sp, -96
    st.d $ra, $sp, 0
    st.d $s0, $sp, 8
    st.d $s1, $sp, 16
    st.d $s2, $sp, 24
    st.d $s3, $sp, 32
    st.d $s4, $sp, 40
    st.d $s5, $sp, 48
    st.d $s6, $sp, 56
    st.d $s7, $sp, 64
    st.d $s8, $sp, 72
    st.d $fp, $sp, 80
    st.d $sp, $sp, 88
    li.d $s0, 0x13579bdf2468ace0
    li.d $s1, 0x2468ace013579bdf
    li.d $s2, 0x3141592653589793
    li.d $s3, 0x2718281828459045
    li.d $s4, 0x0123456789abcdef
    li.d $s5, 0xfedcba9876543210
    li.d $s6, 0x55aa55aa33cc33cc
    li.d $s7, 0xaa55aa55cc33cc33
    li.d $s8, 0x0f0ff0f096966969
    li.d $fp, 0xf0f00f0f69699696
    # a0 是 Rust 包装传入的真实 yield 入口，返回路径仍位于同一 Task 栈。
    jirl $ra, $a0, 0
.Lla_task_probe_return:
    ld.d $t0, $sp, 88
    bne $sp, $t0, .Lla_task_probe_bad
    li.d $t0, 0x13579bdf2468ace0
    bne $s0, $t0, .Lla_task_probe_bad
    li.d $t0, 0x2468ace013579bdf
    bne $s1, $t0, .Lla_task_probe_bad
    li.d $t0, 0x3141592653589793
    bne $s2, $t0, .Lla_task_probe_bad
    li.d $t0, 0x2718281828459045
    bne $s3, $t0, .Lla_task_probe_bad
    li.d $t0, 0x0123456789abcdef
    bne $s4, $t0, .Lla_task_probe_bad
    li.d $t0, 0xfedcba9876543210
    bne $s5, $t0, .Lla_task_probe_bad
    li.d $t0, 0x55aa55aa33cc33cc
    bne $s6, $t0, .Lla_task_probe_bad
    li.d $t0, 0xaa55aa55cc33cc33
    bne $s7, $t0, .Lla_task_probe_bad
    li.d $t0, 0x0f0ff0f096966969
    bne $s8, $t0, .Lla_task_probe_bad
    li.d $t0, 0xf0f00f0f69699696
    bne $fp, $t0, .Lla_task_probe_bad
    li.w $a0, 1
    b .Lla_task_probe_restore
.Lla_task_probe_bad:
    move $a0, $zero
.Lla_task_probe_restore:
    ld.d $ra, $sp, 0
    ld.d $s0, $sp, 8
    ld.d $s1, $sp, 16
    ld.d $s2, $sp, 24
    ld.d $s3, $sp, 32
    ld.d $s4, $sp, 40
    ld.d $s5, $sp, 48
    ld.d $s6, $sp, 56
    ld.d $s7, $sp, 64
    ld.d $s8, $sp, 72
    ld.d $fp, $sp, 80
    addi.d $sp, $sp, 96
    jirl $zero, $ra, 0
.size la_test_task_context_preservation, .-la_test_task_context_preservation
.endif
