.text
.option norelax
.option norvc
.equ TTY_USER_END, 0x4000000000
# /// 两目标指令宏保持同一 raw ABI 与字节断言。
.macro LI r,v
    li \r, \v
.endm
.macro MOV d,s
    mv \d, \s
.endm
.macro LA r,label
    lla \r, \label
.endm
.macro ADDI d,s,v
    addi \d, \s, \v
.endm
.macro ADD d,a,b
    add \d, \a, \b
.endm
.macro SUB d,a,b
    sub \d, \a, \b
.endm
.macro MUL d,a,b
    mul \d, \a, \b
.endm
.macro LD d,s,v
    ld \d, \v(\s)
.endm
.macro LW d,s,v
    lw \d, \v(\s)
.endm
.macro LB d,s,v
    lbu \d, \v(\s)
.endm
.macro SD d,s,v
    sd \d, \v(\s)
.endm
.macro SW d,s,v
    sw \d, \v(\s)
.endm
.macro SB d,s,v
    sb \d, \v(\s)
.endm
.macro BEQ a,b,label
    beq \a, \b, 991f
    j 992f
991:
    j \label
992:
.endm
.macro BNE a,b,label
    bne \a, \b, 991f
    j 992f
991:
    j \label
992:
.endm
.macro BLT a,b,label
    blt \a, \b, 991f
    j 992f
991:
    j \label
992:
.endm
.macro JMP label
    j \label
.endm
.macro FENCE 
    fence rw, rw
.endm
.macro SC n
    li a7, \n
    ecall
.endm

# /// 失败退出码标识具体消费边界，共享 result 保留最后一次真实 syscall 返回。
.macro CHECK reg,value,code
    LI s8, \code
    LI t0, \value
    BEQ \reg,t0,993f
    JMP .Lexit
993:
.endm
.macro OK code
    CHECK a0,0,\code
.endm
.macro IOCTL request,arg,result,code
    MOV a0,s0
    LI a1,\request
    LI a2,\arg
    SC 29
    SW a0,s6,12
    CHECK a0,\result,\code
.endm
# /// 配置完整 36 字节，用户只用标准 TCSETS 提交，不调用测试入口。
.macro SET iflag,oflag,lflag,min,time
    LA t1,.Lconfig
    LI t2,\iflag
    SW t2,t1,0
    LI t2,\oflag
    SW t2,t1,4
    LI t2,0xbf
    SW t2,t1,8
    LI t2,\lflag
    SW t2,t1,12
    LI t2,\time
    SB t2,t1,22
    LI t2,\min
    SB t2,t1,23
    MOV a0,s0
    LI a1,0x5402
    MOV a2,t1
    SC 29
    OK 10
.endm
# /// 每轮先清 gate 再发布 phase；sched_yield 只作为测试门控，不冒称生产阻塞。
.macro GATE phase
    SW zero,s6,4
    LI t1,\phase
    FENCE
    SW t1,s6,0
994:
    FENCE
    LW t1,s6,4
    LI t2,\phase
    BEQ t1,t2,995f
    SC 124
    JMP 994b
995:
    FENCE
.endm
.macro DONE phase
    LI t1,\phase
    FENCE
    SW t1,s6,8
    FENCE
.endm
.macro READ length,result,code
    MOV a0,s0
    LA a1,.Lreceived
    LI a2,\length
    SC 63
    SW a0,s6,12
    CHECK a0,\result,\code
.endm
.macro BYTES label,length,code
    LA t1,.Lreceived
    LA t2,\label
    LI t3,\length
996:
    LB t4,t1,0
    LB t5,t2,0
    LI s8,\code
    BNE t4,t5,.Lexit
    ADDI t1,t1,1
    ADDI t2,t2,1
    ADDI t3,t3,-1
    BNE t3,zero,996b
.endm
.macro WRITE label,length,code
    MOV a0,s0
    LA a1,\label
    LI a2,\length
    SC 64
    SW a0,s6,12
    CHECK a0,\length,\code
.endm
# /// 短write按实际接受前缀续传，满环后的后次调用才进入真实等待。
.macro WRITE_ALL label,length,code
    LA s2,\label
    LI s3,\length
997:
    MOV a0,s0
    MOV a1,s2
    MOV a2,s3
    SC 64
    SW a0,s6,12
    LI s8,\code
    BLT zero,a0,998f
    JMP .Lexit
998:
    BLT s3,a0,.Lexit
    ADD s2,s2,a0
    SUB s3,s3,a0
    BNE s3,zero,997b
.endm
.macro CLOCK label,code
    LI a0,1
    LA a1,\label
    SC 113
    OK \code
.endm
.macro ELAPSED minimum,code
    LA t1,.Lbefore
    LD t2,t1,0
    LD t3,t1,8
    LA t1,.Lafter
    LD t4,t1,0
    LD t5,t1,8
    SUB t4,t4,t2
    LI t1,1000000000
    MUL t4,t4,t1
    SUB t5,t5,t3
    ADD t4,t4,t5
    LI t1,\minimum
    LI s8,\code
    BLT t4,t1,.Lexit
.endm
.macro POLL time,result,code
    LA t1,.Lpoll
    SW s0,t1,0
    LI t2,1
    SB t2,t1,4
    SB zero,t1,5
    SW zero,t1,8
    LA a0,.Lpoll
    LI a1,1
    LA a2,\time
    LI a3,0
    LI a4,0
    SC 73
    CHECK a0,\result,\code
.endm
# /// 八模式执行普通串口 fd、ioctl、read/write 与 ppoll，结束前等待宿主核完阶段。
.global _start
.type _start,@function
_start:
    LD t1,sp,0
    CHECK t1,2,1
    LD t1,sp,16
    LB s7,t1,0
    LI a0,0x44000000
    LI a1,4096
    LI a2,3
    LI a3,0x31
    LI a4,-1
    LI a5,0
    SC 222
    CHECK a0,0x44000000,2
    MOV s6,a0
    LI a0,-100
    LA a1,.Lpath
    LI a2,2
    LI a3,0
    SC 56
    MOV s0,a0
    LI s8,3
    BLT a0,zero,.Lexit
    LI t1,114
    BEQ s7,t1,.Lraw
    LI t1,99
    BEQ s7,t1,.Lcanonical
    LI t1,118
    BEQ s7,t1,.Ltiming
    LI t1,112
    BEQ s7,t1,.Lpoll_mode
    LI t1,101
    BEQ s7,t1,.Lecho
    LI t1,111
    BEQ s7,t1,.Loutput_mode
    LI t1,116
    BEQ s7,t1,.Ltermios
    LI t1,120
    BEQ s7,t1,.Lflow
    LI s8,4
    JMP .Lexit
# /// raw 输入输出保留 NUL、FF 与 UTF-8 碎片，跨 write 边界逐字节验证。
.Lraw:
    SET 0,0,0,6,0
    GATE 1
    READ 0,0,20
    READ 6,6,21
    BYTES .Lraw_bytes,6,22
    WRITE .Lraw_bytes,2,23
    WRITE .Lraw_tail,4,24
    DONE 1
    SET 0,0,0,3,0
    LI a0,0x45000000
    LI a1,8192
    LI a2,3
    LI a3,0x31
    LI a4,-1
    LI a5,0
    SC 222
    CHECK a0,0x45000000,25
    LI a0,0x45001000
    LI a1,4096
    LI a2,0
    SC 226
    OK 26
    GATE 2
    MOV a0,s0
    LI a1,0x45000fff
    LI a2,3
    SC 63
    CHECK a0,1,27
    LI t1,0x45000fff
    LB t2,t1,0
    CHECK t2,97,28
    MOV a0,s0
    LI a1,0x45001000
    LI a2,2
    SC 63
    CHECK a0,-14,29
    READ 2,2,121
    LA t1,.Lreceived
    LB t2,t1,0
    CHECK t2,98,122
    LB t2,t1,1
    CHECK t2,99,123
    DONE 2
    JMP .Lpass
# /// 短 read 消费当前记录前缀，VEOF 与 literal-next 不混淆。
.Lcanonical:
    SET 0,0,0x8002,1,0
    GATE 1
    READ 0,0,30
    READ 2,2,31
    BYTES .Lab,2,32
    READ 16,2,33
    BYTES .Lc_newline,2,34
    READ 16,2,35
    BYTES .Lz_newline,2,36
    READ 16,3,37
    BYTES .Lok_newline,3,38
    READ 3,3,39
    BYTES .Lpar,3,40
    READ 16,4,41
    BYTES .Ltial,4,42
    READ 16,0,43
    READ 16,2,44
    BYTES .Lliteral_eof,2,45
    DONE 1
    JMP .Lpass
# /// 总期限从调用开始，间隔期限从首字节开始；各阶段由真实 read 结果区分。
.Ltiming:
    SET 0,0,0,0,0
    GATE 1
    READ 16,0,50
    DONE 1
    SET 0,0,0,3,0
    GATE 2
    READ 16,3,51
    BYTES .Lxyz,3,52
    DONE 2
    SET 0,0,0,0,1
    GATE 3
    CLOCK .Lbefore,53
    READ 16,0,54
    CLOCK .Lafter,55
    ELAPSED 60000000,56
    DONE 3
    SET 0,0,0,0,1
    GATE 4
    READ 16,1,57
    BYTES .Lexclaim,1,58
    DONE 4
    SET 0,0,0,3,1
    GATE 5
    CLOCK .Lbefore,59
    READ 16,1,60
    CLOCK .Lafter,61
    ELAPSED 60000000,62
    BYTES .LA_byte,1,63
    DONE 5
    JMP .Lpass
# /// poll 采用独立 MIN 谓词，NONBLOCK read 仍可消费不足 MIN 的实际输入。
.Lpoll_mode:
    SET 0,0,0,3,0
    GATE 1
    POLL .Lhundred_ms,0,70
    MOV a0,s0
    LI a1,4
    LI a2,0x802
    SC 25
    OK 71
    READ 16,1,72
    BYTES .Lz,1,73
    SET 0,0,0,0,0
    READ 16,0,74
    POLL .Lzero,0,75
    MOV a0,s0
    LI a1,4
    LI a2,2
    SC 25
    OK 76
    DONE 1
    SET 0,0,0,0,1
    GATE 2
    POLL .Lsecond,1,77
    READ 16,1,78
    BYTES .Lq,1,79
    DONE 2
    JMP .Lpass
# /// 本模式没有应用 read；输入 Work 必须完成擦除与 CR 映射后的回显。
.Lecho:
    SET 0x100,5,0x823a,1,0
    GATE 1
    DONE 1
    JMP .Lpass
# /// 大于有限 output 环的实际 write 经 ONLCR 转换，非零 TCSBRK 等真正 drain。
.Loutput_mode:
    SET 0,5,0,1,0
    LA t1,.Llarge
    LI t2,32768
    LI t3,79
    LI t4,10
.Lfill_output:
    SB t3,t1,0
    SB t4,t1,1
    ADDI t1,t1,2
    ADDI t2,t2,-1
    BNE t2,zero,.Lfill_output
    GATE 1
    WRITE_ALL .Llarge,65536,80
    IOCTL 0x5409,1,0,81
    MOV a0,s0
    LI a1,0x5403
    LA a2,.Lconfig
    SC 29
    OK 82
    DONE 1
    JMP .Lpass
# /// fault 的完整 setter 不改变原配置，合法 W/F 分别等待发送与退休旧输入。
.Ltermios:
    SET 0,0,0,1,0
    MOV a0,s0
    LI a1,0x5401
    LA a2,.Lreceived
    SC 29
    OK 90
    BYTES .Lconfig,36,91
    IOCTL 0x5401,TTY_USER_END,-14,92
    IOCTL 0x5402,TTY_USER_END,-14,93
    LI a0,0x45000000
    LI a1,8192
    LI a2,3
    LI a3,0x31
    LI a4,-1
    LI a5,0
    SC 222
    CHECK a0,0x45000000,94
    LI t1,0x45000ff0
    LI t2,0x100
    SW t2,t1,0
    LI t2,5
    SW t2,t1,4
    LI t2,0xbf
    SW t2,t1,8
    LI t2,0x8002
    SW t2,t1,12
    LI a0,0x45001000
    LI a1,4096
    LI a2,0
    SC 226
    OK 95
    IOCTL 0x5402,0x45000ff0,-14,96
    IOCTL 0x5401,0x45000ff0,-14,97
    MOV a0,s0
    LI a1,0x5401
    LA a2,.Lreceived
    SC 29
    OK 98
    BYTES .Lconfig,36,99
    IOCTL 0x5405,0,-25,100
    IOCTL 0xdead,0,-25,101
    GATE 1
    DONE 1
    GATE 2
    MOV a0,s0
    LI a1,0x5404
    LA a2,.Lconfig
    SC 29
    OK 102
    POLL .Lzero,0,103
    MOV a0,s0
    LI a1,0x5403
    LA a2,.Lconfig
    SC 29
    OK 104
    IOCTL 0x540b,2,0,105
    IOCTL 0x5409,1,0,106
    DONE 2
    JMP .Lpass
# /// STOP 不依赖应用 read，真实满环 writer 必须由后继 START 的 RX Work 恢复。
.Lflow:
    SET 0x400,0,0,1,0
    LA t1,.Llarge
    LI t2,12288
    LI t3,70
.Lfill_flow:
    SB t3,t1,0
    ADDI t1,t1,1
    ADDI t2,t2,-1
    BNE t2,zero,.Lfill_flow
    GATE 1
    DONE 1
    GATE 2
    WRITE_ALL .Llarge,12288,110
    IOCTL 0x5409,1,0,111
    DONE 2
    JMP .Lpass
# /// 最后一个 OFD 关闭前由父先核实际 UART 阶段，随后走普通退出与离栈回收。
.Lpass:
    GATE 99
    MOV a0,s0
    SC 57
    OK 120
    LI s8,0
.Lexit:
    MOV a0,s8
    SC 93
.Lexit_spin:
    JMP .Lexit_spin
.section .rodata
.balign 8
.Lpath: .asciz "/dev/ttyS0"
.Lraw_bytes: .byte 0,0xc3
.Lraw_tail: .byte 0xa9,0xff,10,0x80
.Lab: .ascii "ab"
.Lc_newline: .ascii "c\n"
.Lz_newline: .ascii "z\n"
.Lok_newline: .ascii "ok\n"
.Lpar: .ascii "par"
.Ltial: .ascii "tial"
.Lliteral_eof: .byte 4,10
.Lxyz: .ascii "xyz"
.Lexclaim: .byte 33
.LA_byte: .byte 65
.Lz: .byte 122
.Lq: .byte 113
.balign 8
.Lzero: .dword 0,0
.Lhundred_ms: .dword 0,100000000
.Lsecond: .dword 1,0
.data
.balign 4
.Lconfig: .word 0,0,0xbf,0
.byte 0
.byte 3,28,127,21,4,0,1,0,17,19,26,0,18,15,23,22,0,0,0
.bss
.balign 16
.Lreceived: .space 128
.Lpoll: .space 16
.Lbefore: .space 16
.Lafter: .space 16
.Llarge: .space 65536
