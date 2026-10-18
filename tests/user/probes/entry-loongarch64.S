.text

# /// 保存内核初始 SP 后只对齐 C 调用栈，不初始化 TP、BSS 或 fd。
.global _start
.type _start, @function
_start:
    move $a0, $sp
    bstrins.d $sp, $zero, 3, 0
    bl probe_main
    .word 0
.size _start, .-_start
