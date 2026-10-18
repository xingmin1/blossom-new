.text
.option norelax
.option norvc

# /// 叶桥保持 syscall SP；a5/a6 跨 syscall 保存回调/参数，子绝不返回父 C 帧。
.global probe_clone
.type probe_clone, @function
probe_clone:
    li a7, 220
    ecall
    beqz a0, .Lchild
    ret
.Lchild:
    mv t0, a5
    mv a0, a6
    mv a1, sp
    mv a2, tp
    andi sp, sp, -16
    jalr t0
    unimp
.size probe_clone, .-probe_clone
