.text

# /// 叶桥保持 syscall SP；LA raw clone 的 child_tid/TLS 与共享接口次序相反。
.global probe_clone
.type probe_clone, @function
probe_clone:
    move $t0, $a3
    move $a3, $a4
    move $a4, $t0
    li.w $a7, 220
    syscall 0
    beqz $a0, .Lchild
    jr $ra
.Lchild:
    move $t0, $a5
    move $a0, $a6
    move $a1, $sp
    move $a2, $tp
    bstrins.d $sp, $zero, 3, 0
    jirl $ra, $t0, 0
    .word 0
.size probe_clone, .-probe_clone
