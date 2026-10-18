.text
.option norelax
.option norvc
# /// 执行原非法指令，若硬件未产生SIGILL则保留原检查207。
.global lifecycle_illegal
.type lifecycle_illegal, @function
lifecycle_illegal:
    .word 0
    li a0, 207
    tail probe_failure
.size lifecycle_illegal, .-lifecycle_illegal
.section .note.GNU-stack,"",@progbits
