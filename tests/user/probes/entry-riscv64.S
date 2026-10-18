.text
.option norelax
.option norvc

# /// 保存内核初始 SP 后只对齐 C 调用栈，不初始化 gp/TP、BSS 或 fd。
.global _start
.type _start, @function
_start:
    mv a0, sp
    andi sp, sp, -16
    call probe_main
    unimp
.size _start, .-_start
