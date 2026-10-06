# 用户信号返回只发真实 rt_sigreturn，指令字节作为不可写内核 ABI 片段。
.section .rodata.user_signal_return, "a", @progbits

.option push
.option norvc
.option norelax
.balign 4
.global rv_user_signal_return_start
.type rv_user_signal_return_start, @object
rv_user_signal_return_start:
    li a7, 139
    ecall
.global rv_user_signal_return_end
rv_user_signal_return_end:
.size rv_user_signal_return_start, rv_user_signal_return_end-rv_user_signal_return_start
.option pop
