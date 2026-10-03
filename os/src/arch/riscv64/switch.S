.text

.balign 4
.global rv_switch_context
.type rv_switch_context, @function
rv_switch_context:
    // 普通调用只拥有整数现场；tp 始终指向正在执行此代码的 CPU。
    sd ra, {context_ra}(a0)
    sd sp, {context_sp}(a0)
    sd s0, ({context_saved} + 0 * {word})(a0)
    sd s1, ({context_saved} + 1 * {word})(a0)
    sd s2, ({context_saved} + 2 * {word})(a0)
    sd s3, ({context_saved} + 3 * {word})(a0)
    sd s4, ({context_saved} + 4 * {word})(a0)
    sd s5, ({context_saved} + 5 * {word})(a0)
    sd s6, ({context_saved} + 6 * {word})(a0)
    sd s7, ({context_saved} + 7 * {word})(a0)
    sd s8, ({context_saved} + 8 * {word})(a0)
    sd s9, ({context_saved} + 9 * {word})(a0)
    sd s10, ({context_saved} + 10 * {word})(a0)
    sd s11, ({context_saved} + 11 * {word})(a0)
    ld ra, {context_ra}(a1)
    ld sp, {context_sp}(a1)
    ld s0, ({context_saved} + 0 * {word})(a1)
    ld s1, ({context_saved} + 1 * {word})(a1)
    ld s2, ({context_saved} + 2 * {word})(a1)
    ld s3, ({context_saved} + 3 * {word})(a1)
    ld s4, ({context_saved} + 4 * {word})(a1)
    ld s5, ({context_saved} + 5 * {word})(a1)
    ld s6, ({context_saved} + 6 * {word})(a1)
    ld s7, ({context_saved} + 7 * {word})(a1)
    ld s8, ({context_saved} + 8 * {word})(a1)
    ld s9, ({context_saved} + 9 * {word})(a1)
    ld s10, ({context_saved} + 10 * {word})(a1)
    ld s11, ({context_saved} + 11 * {word})(a1)
    ret
.size rv_switch_context, .-rv_switch_context

.if {execution_tests}
.balign 4
.global rv_task_context_roundtrip
.type rv_task_context_roundtrip, @function
rv_task_context_roundtrip:
    // 复用上下文布局保留调用者；帧长为 16 字节的整数倍，不保存旧 CPU 的 tp。
    addi sp, sp, -{context_size}
    sd ra, {context_ra}(sp)
    sd sp, {context_sp}(sp)
    sd s0, ({context_saved} + 0 * {word})(sp)
    sd s1, ({context_saved} + 1 * {word})(sp)
    sd s2, ({context_saved} + 2 * {word})(sp)
    sd s3, ({context_saved} + 3 * {word})(sp)
    sd s4, ({context_saved} + 4 * {word})(sp)
    sd s5, ({context_saved} + 5 * {word})(sp)
    sd s6, ({context_saved} + 6 * {word})(sp)
    sd s7, ({context_saved} + 7 * {word})(sp)
    sd s8, ({context_saved} + 8 * {word})(sp)
    sd s9, ({context_saved} + 9 * {word})(sp)
    sd s10, ({context_saved} + 10 * {word})(sp)
    sd s11, ({context_saved} + 11 * {word})(sp)
    li s0, 0x1122334455667700
    li s1, 0x1122334455667701
    li s2, 0x1122334455667702
    li s3, 0x1122334455667703
    li s4, 0x1122334455667704
    li s5, 0x1122334455667705
    li s6, 0x1122334455667706
    li s7, 0x1122334455667707
    li s8, 0x1122334455667708
    li s9, 0x1122334455667709
    li s10, 0x112233445566770a
    li s11, 0x112233445566770b
    call {yield_now}
.Lrv_task_after_yield:
    // 到达调用后的标签验证返回路径；ra 是 caller-saved，不要求 Rust 返回时保留其数值。
    li a0, 0
    ld t0, {context_sp}(sp)
    bne sp, t0, .Lrv_task_restore
    li t0, 0x1122334455667700
    bne s0, t0, .Lrv_task_restore
    li t0, 0x1122334455667701
    bne s1, t0, .Lrv_task_restore
    li t0, 0x1122334455667702
    bne s2, t0, .Lrv_task_restore
    li t0, 0x1122334455667703
    bne s3, t0, .Lrv_task_restore
    li t0, 0x1122334455667704
    bne s4, t0, .Lrv_task_restore
    li t0, 0x1122334455667705
    bne s5, t0, .Lrv_task_restore
    li t0, 0x1122334455667706
    bne s6, t0, .Lrv_task_restore
    li t0, 0x1122334455667707
    bne s7, t0, .Lrv_task_restore
    li t0, 0x1122334455667708
    bne s8, t0, .Lrv_task_restore
    li t0, 0x1122334455667709
    bne s9, t0, .Lrv_task_restore
    li t0, 0x112233445566770a
    bne s10, t0, .Lrv_task_restore
    li t0, 0x112233445566770b
    bne s11, t0, .Lrv_task_restore
    li a0, 1
.Lrv_task_restore:
    ld ra, {context_ra}(sp)
    ld s0, ({context_saved} + 0 * {word})(sp)
    ld s1, ({context_saved} + 1 * {word})(sp)
    ld s2, ({context_saved} + 2 * {word})(sp)
    ld s3, ({context_saved} + 3 * {word})(sp)
    ld s4, ({context_saved} + 4 * {word})(sp)
    ld s5, ({context_saved} + 5 * {word})(sp)
    ld s6, ({context_saved} + 6 * {word})(sp)
    ld s7, ({context_saved} + 7 * {word})(sp)
    ld s8, ({context_saved} + 8 * {word})(sp)
    ld s9, ({context_saved} + 9 * {word})(sp)
    ld s10, ({context_saved} + 10 * {word})(sp)
    ld s11, ({context_saved} + 11 * {word})(sp)
    addi sp, sp, {context_size}
    ret
.size rv_task_context_roundtrip, .-rv_task_context_roundtrip
.endif
