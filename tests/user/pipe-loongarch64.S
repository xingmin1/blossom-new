.text
.equ PIPE_USER_END, 0x8000000000
# /// 下列窄指令宏只表达两目标机器指令，客体共享相同 raw ABI 断言。
.macro LI r, v
    li.d $\r, \v
.endm
.macro MOV d, s
    move $\d, $\s
.endm
.macro LA r, label
    la.local $\r, \label
.endm
.macro ADDI d, s, v
    addi.d $\d, $\s, \v
.endm
.macro LD d, s, v
    ld.d $\d, $\s, \v
.endm
.macro LW d, s, v
    ld.w $\d, $\s, \v
.endm
.macro LB d, s, v
    ld.bu $\d, $\s, \v
.endm
.macro SD d, s, v
    st.d $\d, $\s, \v
.endm
.macro SW d, s, v
    st.w $\d, $\s, \v
.endm
.macro SB d, s, v
    st.b $\d, $\s, \v
.endm
.macro BEQ a, b, label
    beq $\a, $\b, 991f
    b 992f
991:
    b \label
992:
.endm
.macro BNE a, b, label
    bne $\a, $\b, 991f
    b 992f
991:
    b \label
992:
.endm
.macro BLT a, b, label
    blt $\a, $\b, 991f
    b 992f
991:
    b \label
992:
.endm
.macro JMP label
    b \label
.endm
.macro CALL label
    bl \label
.endm
.macro RETURN
    jr $ra
.endm
.macro SC n
    li.d $a7, \n
    syscall 0
.endm

# /// 退出编号保留具体 ABI 断言，子进程状态由父通过正式 wait4 消费。
.macro CHECK reg, value, code
    LI s8, \code
    LI t0, \value
    BEQ \reg, t0, 993f
    JMP .Lexit
993:
.endm
.macro EQ left, right, code
    LI s8, \code
    BEQ \left, \right, 993f
    JMP .Lexit
993:
.endm
.macro OK code
    CHECK a0, 0, \code
.endm
.macro FD code
    LI s8, \code
    BLT a0, zero, .Lexit
.endm
.macro CLOSE reg, code
    MOV a0, \reg
    SC 57
    OK \code
.endm
.macro FORK code
    LI a0, 17
    LI a1, 0
    LI a2, 0
    LI a3, 0
    LI a4, 0
    SC 220
    FD \code
.endm
.macro READ reg, label, count, code
    MOV a0, \reg
    LA a1, \label
    LI a2, \count
    SC 63
    CHECK a0, \count, \code
.endm
.macro WRITE reg, label, count, code
    MOV a0, \reg
    LA a1, \label
    LI a2, \count
    SC 64
    CHECK a0, \count, \code
.endm
.macro ACTION sig, handler, flags, code
    LA t1, .Laction
    LA t0, \handler
    SD t0, t1, 0
    LI t0, \flags
    SD t0, t1, 8
    SD zero, t1, 16
    LI a0, \sig
    MOV a1, t1
    LI a2, 0
    LI a3, 8
    SC 134
    OK \code
.endm
.macro VECTOR fd, label, count, syscall, result, code
    MOV a0, \fd
    LA a1, \label
    LI a2, \count
    SC \syscall
    CHECK a0, \result, \code
.endm

# /// 单字母 argv 选择互不替代的真实字节、拥有和错误模式。
.global _start
.type _start, @function
_start:
    LD t0, sp, 0
    CHECK t0, 2, 1
    LD t0, sp, 16
    LB s7, t0, 0
    SC 172
    MOV s5, a0
    LI a0, 0x44000000
    LI a1, 4096
    LI a2, 3
    LI a3, 0x31
    LI a4, -1
    LI a5, 0
    SC 222
    CHECK a0, 0x44000000, 2
    MOV s6, a0
    LA t1, .Lpage
    LI t2, 4096
    LI t3, 65
.Linitial_page:
    SB t3, t1, 0
    ADDI t1, t1, 1
    ADDI t2, t2, -1
    BNE t2, zero, .Linitial_page
    LI a1, 0x80000
    LI t0, 110
    BEQ s7, t0, .Lnonblock_flags
    LI t0, 100
    BNE s7, t0, .Lmake_pipe
.Lnonblock_flags:
    LI a1, 0x80800
.Lmake_pipe:
    LA a0, .Lfds
    SC 59
    OK 3
    LA t1, .Lfds
    LW s0, t1, 0
    LW s1, t1, 4
    CHECK s0, 0, 4
    CHECK s1, 1, 5
    MOV a0, s0
    LI a1, 1
    LI a2, 0
    SC 25
    CHECK a0, 1, 6
    LI t0, 100
    BEQ s7, t0, .Ldata
    LI t0, 97
    BEQ s7, t0, .Latomic
    LI t0, 110
    BEQ s7, t0, .Lnonblock
    LI t0, 118
    BEQ s7, t0, .Lvector_boundary
    LI t0, 105
    BEQ s7, t0, .Lignored
    LI t0, 115
    BEQ s7, t0, .Ldefault
    LI t0, 98
    BEQ s7, t0, .Lblocked_writer
    LI t0, 101
    BEQ s7, t0, .Linterrupt
    LI t0, 114
    BEQ s7, t0, .Linterrupt
    LI t0, 102
    BEQ s7, t0, .Lreservation
    LI t0, 117
    BEQ s7, t0, .Lowner
    LI t0, 112
    BEQ s7, t0, .Lpositioned
    LI s8, 7
    JMP .Lexit

# /// 非root创建者从正式fstat读取原uid/gid，不把匿名对象归给固定root身份。
.Lowner:
    MOV a0, s0
    LA a1, .Lstat
    SC 80
    OK 152
    LA t1, .Lstat
    LW t2, t1, 24
    CHECK t2, 1000, 153
    LW t2, t1, 28
    CHECK t2, 2000, 154
    CLOSE s0, 155
    CLOSE s1, 156
    JMP .Lpass

# /// 两端的按位置调用先拒流能力；访问模式、用户地址和零count不能改变ESPIPE。
.Lpositioned:
.set .Lposition_code, 157
.irp fd, s1, s0
.irp syscall, 67, 68
.irp invalid, 0, 1
.irp count, 0, 1
    MOV a0, \fd
.if \invalid
    LI a1, -1
.else
    LA a1, .Lout
.endif
    LI a2, \count
    LI a3, 0
    SC \syscall
    SW a0, s6, 12
    CHECK a0, -29, .Lposition_code
.set .Lposition_code, .Lposition_code+1
.endr
.endr
    MOV a0, \fd
    LI a1, -1
    LI a2, 1
    LI a3, -1
    SC \syscall
    SW a0, s6, 12
    CHECK a0, -22, .Lposition_code
.set .Lposition_code, .Lposition_code+1
.endr
.endr
.irp syscall, 67, 68
    LI a0, -1
    LI a1, -1
    LI a2, 1
    LI a3, 0
    SC \syscall
    CHECK a0, -9, .Lposition_code
.set .Lposition_code, .Lposition_code+1
    LI a0, -1
    LI a1, -1
    LI a2, 1
    LI a3, -1
    SC \syscall
    CHECK a0, -22, .Lposition_code
.set .Lposition_code, .Lposition_code+1
.endr
    WRITE s1, .Ldata_bytes, 8, 182
    READ s0, .Lout, 8, 183
    CLOSE s0, 184
    CLOSE s1, 185
    JMP .Lpass

# /// 原fstat核FIFO类型和权限；dup/fork持端期间保EAGAIN，末writer关闭才有EOF。
.Ldata:
    MOV a0, s0
    LA a1, .Lstat
    SC 80
    OK 150
    LA t1, .Lstat
    LW t2, t1, 16
    CHECK t2, 0x1180, 151
    MOV a0, s1
    SC 23
    CHECK a0, 2, 8
    MOV s2, a0
    FORK 9
    BEQ a0, zero, .Ldata_child
    MOV s3, a0
    CLOSE s1, 10
    CLOSE s2, 11
    MOV a0, s0
    LA a1, .Lout
    LI a2, 8
    SC 63
    CHECK a0, -11, 12
    MOV a0, s0
    LI a1, 4
    LI a2, 0
    SC 25
    OK 13
    LI t0, 1
    SW t0, s6, 4
    VECTOR s0, .Lread_vectors, 3, 65, 8, 14
    LA t1, .Lout
    LD t2, t1, 0
    LA t1, .Ldata_bytes
    LD t3, t1, 0
    EQ t2, t3, 15
    MOV a0, s0
    LA a1, .Lout
    LI a2, 1
    SC 63
    OK 16
    MOV a0, s3
    CALL .Lwait_zero
    CLOSE s0, 17
    JMP .Lpass
.Ldata_child:
    CLOSE s0, 18
    CLOSE s1, 19
    CALL .Lgate
    VECTOR s2, .Ldata_vectors, 3, 66, 8, 20
    CLOSE s2, 21
    JMP .Lpass

# /// 两个实际子进程各写十六条4096字节向量；父核每条全部字节及两种记录数。
.Latomic:
    LI s4, 0
    FORK 22
    BEQ a0, zero, .Latomic_child
    MOV s2, a0
    LI s4, 1
    FORK 23
    BEQ a0, zero, .Latomic_child
    MOV s3, a0
    CLOSE s1, 24
    LI s4, 0
    LI s5, 0
.Latomic_read:
    READ s0, .Lout, 4096, 25
    LA t1, .Lout
    LB t3, t1, 0
    LI t0, 65
    BEQ t3, t0, .Latomic_A
    CHECK t3, 66, 26
    ADDI s5, s5, 1
    JMP .Latomic_check
.Latomic_A:
    ADDI s4, s4, 1
.Latomic_check:
    LI t2, 4096
.Latomic_bytes:
    LB t4, t1, 0
    EQ t4, t3, 27
    ADDI t1, t1, 1
    ADDI t2, t2, -1
    BNE t2, zero, .Latomic_bytes
    LI t0, 16
    BNE s4, t0, .Latomic_read
    BNE s5, t0, .Latomic_read
    CHECK s4, 16, 28
    CHECK s5, 16, 29
    MOV a0, s0
    LA a1, .Lout
    LI a2, 1
    SC 63
    OK 30
    MOV a0, s2
    CALL .Lwait_zero
    MOV a0, s3
    CALL .Lwait_zero
    CLOSE s0, 31
    JMP .Lpass
.Latomic_child:
    CLOSE s0, 32
    LA t1, .Lpage
    LI t2, 4096
    ADDI t3, s4, 65
.Latomic_fill:
    SB t3, t1, 0
    ADDI t1, t1, 1
    ADDI t2, t2, -1
    BNE t2, zero, .Latomic_fill
    LI s4, 16
.Latomic_write:
    VECTOR s1, .Lpage_vectors, 2, 66, 4096, 33
    ADDI s4, s4, -1
    BNE s4, zero, .Latomic_write
    CLOSE s1, 34
    JMP .Lpass

# /// 满64KiB返回EAGAIN；释放一页后大向量只接受一页，部分uaccess故障保留已消费/提交前缀。
.Lnonblock:
    LI s4, 16
.Lfill:
    WRITE s1, .Lpage, 4096, 35
    ADDI s4, s4, -1
    BNE s4, zero, .Lfill
    MOV a0, s1
    LA a1, .Lpage
    LI a2, 1
    SC 64
    CHECK a0, -11, 36
    MOV a0, s1
    LI a1, -1
    LI a2, 0
    SC 64
    OK 37
    MOV a0, s0
    LI a1, -1
    LI a2, 0
    SC 63
    OK 38
    READ s0, .Lout, 4096, 39
    VECTOR s1, .Llarge_vectors, 2, 66, 4096, 40
    LI s4, 16
.Ldrain:
    READ s0, .Lout, 4096, 41
    LA t1, .Lout
    LI t2, 4096
.Ldrain_bytes:
    LB t3, t1, 0
    CHECK t3, 65, 42
    ADDI t1, t1, 1
    ADDI t2, t2, -1
    BNE t2, zero, .Ldrain_bytes
    ADDI s4, s4, -1
    BNE s4, zero, .Ldrain
    MOV a0, s0
    LA a1, .Lout
    LI a2, 1
    SC 63
    CHECK a0, -11, 43
    VECTOR s1, .Lfault_write, 2, 66, 3, 44
    VECTOR s0, .Lfault_read, 2, 65, 1, 45
    LA t1, .Lout
    LB t2, t1, 0
    CHECK t2, 97, 46
    READ s0, .Lout, 2, 47
    LA t1, .Lout
    LB t2, t1, 0
    CHECK t2, 98, 48
    LB t2, t1, 1
    CHECK t2, 99, 49
    CLOSE s0, 50
    CLOSE s1, 51
    JMP .Lpass

# /// import错误顺序、空段、1024上界和普通文件的一次写位置提交由用户结果验证。
.Lvector_boundary:
    MOV a0, s1
    LI a1, -1
    LI a2, 0
    SC 66
    OK 52
    MOV a0, s1
    LI a1, -1
    LI a2, -1
    SC 66
    CHECK a0, -22, 53
    VECTOR s1, .Lzeros, 1025, 66, -22, 54
    MOV a0, s1
    LI a1, 0x45000000
    LI a2, 1
    SC 66
    CHECK a0, -14, 55
    VECTOR s0, .Lbad_length, 1, 65, -22, 56
    VECTOR s1, .Lbad_range, 2, 66, -14, 57
    VECTOR s1, .Lzeros, 1024, 66, 0, 58
    MOV a0, s0
    LI a1, 0x45000000
    LI a2, 1025
    SC 66
    CHECK a0, -9, 59
    LI a0, -1
    LI a1, 0x45000000
    LI a2, -1
    SC 65
    CHECK a0, -9, 60
    LI a0, -100
    LA a1, .Lfile_path
    LI a2, 2
    LI a3, 0
    SC 56
    FD 61
    MOV s2, a0
    # EOF使长度裁剪可见而不分配巨大用户字节区：单段先裁剪，多段仍验证原范围。
    MOV a0, s2
    LI a1, 0
    LI a2, 2
    SC 62
    FD 137
    VECTOR s2, .Lcap_single, 1, 65, 0, 138
    VECTOR s2, .Lcap_multi_bad, 2, 65, -14, 139
    VECTOR s2, .Lcap_multi_good, 2, 65, 0, 140
    VECTOR s2, .Lcap_suffix_bad, 2, 65, -14, 141
    MOV a0, s2
    LI a1, 0
    LI a2, 0
    SC 62
    OK 142
    MOV a0, s2
    SC 23
    FD 62
    MOV s3, a0
    VECTOR s2, .Ldata_vectors, 3, 66, 8, 63
    MOV a0, s3
    LI a1, 0
    LI a2, 1
    SC 62
    CHECK a0, 8, 64
    MOV a0, s3
    LI a1, 0
    LI a2, 0
    SC 62
    OK 65
    VECTOR s2, .Lread_vectors, 3, 65, 8, 66
    LA t1, .Lout
    LD t2, t1, 0
    LA t1, .Ldata_bytes
    LD t3, t1, 0
    EQ t2, t3, 67
    VECTOR s2, .Lfault_write, 2, 66, 3, 68
    MOV a0, s3
    LI a1, 0
    LI a2, 1
    SC 62
    CHECK a0, 11, 69
    MOV a0, s3
    LI a1, 0
    LI a2, 0
    SC 62
    OK 70
    VECTOR s2, .Lfault_read, 2, 65, 1, 71
    MOV a0, s3
    LI a1, 0
    LI a2, 1
    SC 62
    CHECK a0, 1, 72
    READ s3, .Lout, 2, 73
    LA t1, .Lout
    LB t2, t1, 0
    CHECK t2, 98, 74
    LB t2, t1, 1
    CHECK t2, 99, 75
    CLOSE s3, 76
    CLOSE s2, 77
    CLOSE s1, 78
    CLOSE s0, 79
    JMP .Lpass

# /// SIG_IGN仍返回EPIPE但不产生pending，零长无读端请求保持成功。
.Lignored:
    LA t1, .Laction
    LI t0, 1
    SD t0, t1, 0
    SD zero, t1, 8
    SD zero, t1, 16
    LI a0, 13
    MOV a1, t1
    LI a2, 0
    LI a3, 8
    SC 134
    OK 80
    CLOSE s0, 81
    MOV a0, s1
    LA a1, .Lpage
    LI a2, 1
    SC 64
    CHECK a0, -32, 82
    MOV a0, s1
    LI a1, -1
    LI a2, 0
    SC 64
    OK 83
    CALL .Ltake_pipe
    CHECK a0, -11, 84
    CLOSE s1, 85
    JMP .Lpass

# /// 默认SIGPIPE杀死真实子进程；父先撤自己的reader后才允许子写。
.Ldefault:
    FORK 86
    BEQ a0, zero, .Ldefault_child
    MOV s3, a0
    CLOSE s0, 87
    CLOSE s1, 88
    LI t0, 1
    SW t0, s6, 4
    MOV a0, s3
    LA a1, .Lstatus
    LI a2, 0
    LI a3, 0
    SC 260
    EQ a0, s3, 89
    LA t1, .Lstatus
    LW t2, t1, 0
    CHECK t2, 13, 90
    JMP .Lpass
.Ldefault_child:
    CLOSE s0, 91
    CALL .Lgate
    MOV a0, s1
    LA a1, .Lpage
    LI a2, 1
    SC 64
    LI s8, 92
    JMP .Lexit

# /// 真正满管道writer阻塞后，原fixture才放child关闭最后reader；private信号保原写者身份。
.Lblocked_writer:
    LI a0, 0
    LA a1, .Lpipe_mask
    LI a2, 0
    LI a3, 8
    SC 135
    OK 93
    LI s4, 16
.Lblocked_fill:
    WRITE s1, .Lpage, 4096, 94
    ADDI s4, s4, -1
    BNE s4, zero, .Lblocked_fill
    FORK 95
    BEQ a0, zero, .Lblocked_child
    MOV s3, a0
    CLOSE s0, 96
    LI t0, 1
    SW t0, s6, 0
    MOV a0, s1
    LA a1, .Lpage
    LI a2, 1
    SC 64
    CHECK a0, -32, 97
    CALL .Ltake_pipe
    CHECK a0, 13, 98
    LA t1, .Linfo
    LW t2, t1, 8
    CHECK t2, 0, 99
    SC 172
    LW t2, t1, 16
    EQ t2, a0, 100
    LW t2, t1, 20
    CHECK t2, 0, 101
    CALL .Ltake_pipe
    CHECK a0, -11, 102
    CLOSE s1, 103
    MOV a0, s3
    CALL .Lwait_zero
    # 已接受64KiB后关闭最后reader，下一页失败必须保正前缀并仍生成SIGPIPE。
    LA a0, .Lfds
    LI a1, 0
    SC 59
    OK 143
    LA t1, .Lfds
    LW s0, t1, 0
    LW s1, t1, 4
    SW zero, s6, 4
    FORK 144
    BEQ a0, zero, .Lblocked_child
    MOV s3, a0
    CLOSE s0, 145
    LI t0, 2
    SW t0, s6, 0
    VECTOR s1, .Loversize_vectors, 17, 66, 65536, 146
    CALL .Ltake_pipe
    CHECK a0, 13, 147
    LA t1, .Linfo
    LW t2, t1, 8
    CHECK t2, 0, 148
    CLOSE s1, 149
    MOV a0, s3
    CALL .Lwait_zero
    JMP .Lpass
.Lblocked_child:
    CLOSE s1, 104
    CALL .Lgate
    CLOSE s0, 105
    JMP .Lpass

# /// fixture观察正式read阻塞后放child发送USR1，handler返回后才写字节，区分EINTR与SA_RESTART。
.Linterrupt:
    LI t0, 114
    BEQ s7, t0, .Lrestart_action
    ACTION 10, .Lhandler, 4, 106
    JMP .Linterrupt_fork
.Lrestart_action:
    ACTION 10, .Lhandler, 0x10000004, 107
.Linterrupt_fork:
    FORK 108
    BEQ a0, zero, .Linterrupt_child
    MOV s3, a0
    CLOSE s1, 109
    LI t0, 1
    SW t0, s6, 0
    MOV a0, s0
    LA a1, .Lout
    LI a2, 1
    SC 63
    LI t0, 114
    BEQ s7, t0, .Lrestart_result
    CHECK a0, -4, 110
    READ s0, .Lout, 1, 111
    JMP .Linterrupt_result
.Lrestart_result:
    CHECK a0, 1, 112
.Linterrupt_result:
    LW t2, s6, 8
    CHECK t2, 1, 113
    LA t1, .Lout
    LB t2, t1, 0
    CHECK t2, 65, 114
    MOV a0, s3
    CALL .Lwait_zero
    CLOSE s0, 115
    JMP .Lpass
.Linterrupt_child:
    CLOSE s0, 116
    CALL .Lgate
    MOV a0, s5
    LI a1, 10
    SC 129
    OK 117
.Lhandler_gate:
    LW t2, s6, 8
    BNE t2, zero, .Linterrupt_write
    SC 124
    JMP .Lhandler_gate
.Linterrupt_write:
    WRITE s1, .Lpage, 1, 118
    CLOSE s1, 119
    JMP .Lpass

# /// EFAULT没有发表任何新fd；剩一槽时第二预留失败会退回首槽，随后原dup可使用它。
.Lreservation:
    LI a0, 0x45000000
    LI a1, 0
    SC 59
    CHECK a0, -14, 120
    LI a0, 2
    LI a1, 1
    LI a2, 0
    SC 25
    CHECK a0, -9, 121
    MOV a0, s0
    SC 23
    CHECK a0, 2, 122
    MOV s2, a0
.Lfill_table:
    MOV a0, s0
    SC 23
    FD 123
    ADDI s2, s2, 1
    EQ a0, s2, 124
    LI t0, 126
    BNE s2, t0, .Lfill_table
    LA a0, .Lfds
    LI a1, 0
    SC 59
    CHECK a0, -24, 125
    LI a0, 127
    LI a1, 1
    LI a2, 0
    SC 25
    CHECK a0, -9, 126
    MOV a0, s0
    SC 23
    CHECK a0, 127, 127
    MOV a0, s0
    SC 23
    CHECK a0, -24, 128
    LI a0, 127
    SC 57
    OK 129
    LI a0, 126
    SC 57
    OK 130
    LA a0, .Lfds
    LI a1, 0
    SC 59
    OK 131
    LA t1, .Lfds
    LW t2, t1, 0
    CHECK t2, 126, 132
    LW t2, t1, 4
    CHECK t2, 127, 133
    LI a0, 0x45000000
    LI a1, 1
    SC 59
    CHECK a0, -22, 134
    JMP .Lpass

# /// 普通共享控制字只决定并发次序；所有被验收操作仍由客体执行。
.Lgate:
    LW t2, s6, 4
    BNE t2, zero, .Lgate_return
    SC 124
    JMP .Lgate
.Lgate_return:
    RETURN

# /// private blocked SIGPIPE由标准rt_sigtimedwait领取，零期限不建立额外等待。
.Ltake_pipe:
    LA a0, .Lpipe_mask
    LA a1, .Linfo
    LA a2, .Lzero_time
    LI a3, 8
    SC 137
    RETURN

# /// wait4必须返回原child且其正常退出码为零，不能只观察退出消息。
.Lwait_zero:
    MOV s4, a0
    LA a1, .Lstatus
    LI a2, 0
    LI a3, 0
    SC 260
    EQ a0, s4, 135
    LA t1, .Lstatus
    LW t2, t1, 0
    CHECK t2, 0, 136
    RETURN

# /// handler只改变共享字，返回地址沿正式信号帧协议执行rt_sigreturn。
.Lhandler:
    LI t0, 0x44000000
    LI t1, 1
    SW t1, t0, 8
    RETURN

# /// 正常和断言失败都通过普通exit交付真实wait状态。
.Lpass:
    LI s8, 0
.Lexit:
    MOV a0, s8
    SC 93
.Lhalt:
    JMP .Lhalt

.section .rodata
.balign 8
.Ldata_bytes: .ascii "abcdefgh"
.Lfile_path: .asciz "/test/write-reserve"
.balign 8
.Ldata_vectors:
    .quad .Ldata_bytes, 3, -1, 0, .Ldata_bytes+3, 5
.Lpage_vectors:
    .quad .Lpage, 2048, .Lpage+2048, 2048
.Llarge_vectors:
    .quad .Lpage, 4096, .Lpage, 4096
.Lread_vectors:
    .quad .Lout, 3, -1, 0, .Lout+3, 5
.Lfault_write:
    .quad .Ldata_bytes, 3, 0x45000000, 5
.Lfault_read:
    .quad .Lout, 1, 0x45000000, 2
.Lbad_length:
    .quad .Lout, 0x8000000000000000
.Lbad_range:
    .quad .Lpage, 1, -1, 1
.Lcap_single:
    .quad PIPE_USER_END-0x7ffff000, 0x80000000
.Lcap_multi_bad:
    .quad PIPE_USER_END-0x7ffff000, 0x80000000, -1, 0
.Lcap_multi_good:
    .quad PIPE_USER_END-0x7ffff000, 0x7ffff000, .Lout, 4096
.Lcap_suffix_bad:
    .quad PIPE_USER_END-0x7ffff000, 0x7ffff000, PIPE_USER_END-4096, 8192
.Lpipe_mask: .quad 0x1000
.Lzero_time: .quad 0, 0
.Loversize_vectors:
.rept 17
    .quad .Lpage, 4096
.endr
.Lzeros:
.rept 1024
    .quad -1, 0
.endr

.section .data
# 保留共同链接脚本要求的真实文件段和未页齐 BSS 边缘。
.byte 0x57

.section .bss
.balign 8
.Lfds: .skip 8
.Lstatus: .skip 8
.Laction: .skip 32
.Linfo: .skip 128
.Lstat: .skip 128
.balign 8
.Lpage: .skip 4096
.Lout: .skip 4096
.byte 0
