# 用户信号返回只发真实 rt_sigreturn，指令字节作为不可写内核 ABI 片段。
.section .rodata.user_signal_return, "a", @progbits

.balign 4
.global la_user_signal_return_start
.type la_user_signal_return_start, @object
la_user_signal_return_start:
    addi.d $a7, $zero, 139
    syscall 0
.global la_user_signal_return_end
la_user_signal_return_end:
.size la_user_signal_return_start, la_user_signal_return_end-la_user_signal_return_start
