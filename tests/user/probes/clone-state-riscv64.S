.text
.option norelax
.option norvc
# /// 只观察唯一clone桥两侧的SP/TP；syscall和子路径仍归共享probe_clone。
.global clone_state
.type clone_state, @function
clone_state:
    addi sp, sp, -16
    sd ra, 0(sp)
    sd s0, 8(sp)
    mv s0, a6
    sd sp, 0(s0)
    li tp, 0x12345000
    call probe_clone
    sd sp, 8(s0)
    sd tp, 16(s0)
    ld ra, 0(sp)
    ld s0, 8(sp)
    addi sp, sp, 16
    ret
.size clone_state, .-clone_state
.section .note.GNU-stack,"",@progbits
