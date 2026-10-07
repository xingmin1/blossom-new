.text
.option norelax
.option norvc

# 普通静态 ELF；共享页发布采用 Release，观察采用 Acquire。
# s0=控制页，s1=模式；每个检查的退出码直接写在调用处。
.macro ARGS
    li a0, 0
    li a1, 0
    li a2, 0
    li a3, 0
    li a4, 0
    li a5, 0
.endm
.macro SC number
    li a7, \number
    ecall
.endm
.macro CHECK reg, value, code
    li s8, \code
    li t6, \value
    beq \reg, t6, 991f
    j .Lexit
991:
.endm
.macro EQ left, right, code
    li s8, \code
    beq \left, \right, 991f
    j .Lexit
991:
.endm
.macro NONNEG reg, code
    li s8, \code
    bgez \reg, 991f
    j .Lexit
991:
.endm
.macro POSITIVE reg, code
    li s8, \code
    bgtz \reg, 991f
    j .Lexit
991:
.endm
.macro SELECT char, target
    li t0, \char
    bne s1, t0, 991f
    j \target
991:
.endm
.macro PUT offset, value
    li t0, \value
    fence rw, w
    sw t0, \offset(s0)
.endm
.macro WAIT offset, value
991:
    lw t0, \offset(s0)
    fence r, rw
    li t1, \value
    bgeu t0, t1, 992f
    ARGS
    SC 124
    j 991b
992:
.endm
.macro GO value
    WAIT 12, \value
.endm
.macro PHASE value
    PUT 8, \value
.endm
.macro MAP flags, code
    ARGS
    li a0, 0x44000000
    li a1, 4096
    li a2, 3
    li a3, \flags
    li a4, -1
    SC 222
    CHECK a0, 0x44000000, \code
    li s0, 0x44000000
.endm
# RV raw clone 次序为 flags/stack/parent_tid/tls/child_tid。
# VM 子使用独立 16KiB 用户栈；号码由真实 parent_tid copyout 发布。
.macro SPAWN flags, target, slot, code, stack=.Lchild_stack_top, clear=0
    ARGS
    li a0, \flags
    .if \flags & 0x100
    lla a1, \stack
    .endif
    addi a2, s0, \slot
    .if \clear
    addi a4, s0, \clear
    .endif
    SC 220
    NONNEG a0, \code
    bnez a0, 991f
    j \target
991:
.endm
.macro FORK target, slot, code
    SPAWN 0x100011, \target, \slot, \code
.endm
.macro REAP pid, code, statuscode
    ARGS
    mv a0, \pid
    lla a1, .Lstatus
    SC 260
    EQ a0, \pid, \code
    lla t1, .Lstatus
    lw t2, 0(t1)
    CHECK t2, 0, \statuscode
.endm
.macro MASK how, bits, code
    lla t1, .Lmask
    li t0, \bits
    sd t0, 0(t1)
    ARGS
    li a0, \how
    mv a1, t1
    li a3, 8
    SC 135
    CHECK a0, 0, \code
.endm
.macro CAPTURE code
    ARGS
    li a0, 10
    lla a1, .Lcapture
    li a3, 8
    SC 134
    CHECK a0, 0, \code
.endm
.macro ARM timercode, stackcode
    ARGS
    lla a1, .Ltimer
    SC 103
    CHECK a0, 0, \timercode
    ARGS
    lla a0, .Laltstack
    SC 132
    CHECK a0, 0, \stackcode
.endm
.macro EXEC mode
    lla t1, .Lexec_mode
    li t0, \mode
    sb t0, 0(t1)
    ARGS
    lla a0, .Lself
    lla a1, .Largv
    lla a2, .Lenvp
    SC 221
    # 成功绝不能返回旧映像；准备失败也不是此路径的成功。
    li s8, 16
    j .Lexit
.endm
.macro OLD_HOLE code
    ARGS
    li a0, 0x44000044
    li a1, 128
    SC 98
    CHECK a0, -14, \code
.endm

# /// 从真实初始栈分派生命周期协议；未知模式和坏栈明确失败。
.global _start
.type _start, @function
_start:
    andi t0, sp, 15
    CHECK t0, 0, 1
    ld t0, 0(sp)
    CHECK t0, 2, 2
    ld t0, 16(sp)
    POSITIVE t0, 3
    lbu s1, 0(t0)
    lbu t1, 1(t0)
    CHECK t1, 0, 4
    ld t0, 24(sp)
    CHECK t0, 0, 5
    SELECT 78, .Lnew
    SELECT 88, .Lnew
    SELECT 86, .Lnew
    SELECT 85, .Lnew
    SELECT 115, .Lsegv
    SELECT 98, .Lbus
    SELECT 105, .Lill
    MAP 0x31, 6
    ARGS
    SC 172
    mv s3, a0
    ARGS
    SC 178
    EQ a0, s3, 7
    SELECT 110, .Lnonleader
    SELECT 99, .Lcompete
    SELECT 120, .Lexternal
    SELECT 118, .Lvfork
    SELECT 122, .Lvfork
    SELECT 107, .Lvfork
    SELECT 112, .Lparent
    SELECT 103, .Lgroup
    SELECT 119, .Lwait
    SELECT 100, .Lautoreap
    SELECT 101, .Lautoreap
    SELECT 97, .Lautoreap
    SELECT 117, .Lproduction
    li s8, 8
    j .Lexit

# /// 新映像检查 argc/argv/envp 与真实组主身份，不依赖旧控制映射。
.Lnew:
    ld t0, 8(sp)
    POSITIVE t0, 208
    lla t1, .Lself
2:
    lbu t3, 0(t0)
    lbu t4, 0(t1)
    EQ t3, t4, 209
    beqz t3, 3f
    addi t0, t0, 1
    addi t1, t1, 1
    j 2b
3:
    ld t0, 32(sp)
    POSITIVE t0, 9
    lla t1, .Lexec_env
    li t2, 7
1:
    lbu t3, 0(t0)
    lbu t4, 0(t1)
    EQ t3, t4, 10
    addi t0, t0, 1
    addi t1, t1, 1
    addi t2, t2, -1
    bnez t2, 1b
    ld t0, 40(sp)
    CHECK t0, 0, 11
    ARGS
    SC 172
    mv s3, a0
    ARGS
    SC 178
    EQ a0, s3, 12
    SELECT 86, .Lnew_vfork
    SELECT 88, .Lnew_resources
    SELECT 85, .Lnew_resources
    OLD_HOLE 13
.Lnew_control:
    MAP 0x32, 14
    PHASE 100
    GO 100
    j .Lsuccess

.Lnonleader:
    SPAWN 0x110900, .Lnonleader_child, 0, 17
    WAIT 40, 1
    PHASE 1
    ARGS
    addi a0, s0, 68
    li a1, 128
    SC 98
    li s8, 18
    j .Lexit
.Lnonleader_child:
    PUT 40, 1
    GO 1
    EXEC 78

.Lcompete:
    SPAWN 0x110900, .Lcompete_child, 0, 19
    WAIT 40, 1
    PHASE 1
    GO 1
    EXEC 78
.Lcompete_child:
    PUT 40, 1
    GO 1
    EXEC 78

# /// exec 准备失败保留身份、控制映射和真实文件位置。
.Lprepare:
    ARGS
    li a0, -100
    lla a1, .Ldata_path
    li a2, 0x80000
    SC 56
    CHECK a0, 0, 20
    ARGS
    li a0, -100
    lla a1, .Ldata_path
    SC 56
    CHECK a0, 1, 21
    ARGS
    li a0, 1
    li a1, 2
    SC 62
    CHECK a0, 2, 22
    ARGS
    lla a0, .Ldirectory
    SC 49
    CHECK a0, 0, 23
    CAPTURE 24
    ARGS
    li a0, 12
    lla a1, .Lignore
    li a3, 8
    SC 134
    CHECK a0, 0, 25
    MASK 0, 0x200, 26
    li t0, 117
    bne s1, t0, 1f
    ARM 48, 49
    ARGS
    mv a0, s3
    li a1, 10
    SC 129
    CHECK a0, 0, 50
1:
    PUT 112, 0x13579
    ARGS
    lla a0, .Lmissing
    lla a1, .Largv
    lla a2, .Lenvp
    SC 221
    CHECK a0, -2, 27
    ARGS
    lla a0, .Lself
    li a1, 0x70000000
    lla a2, .Lenvp
    SC 221
    CHECK a0, -14, 28
    ARGS
    lla a0, .Lunsupported
    lla a1, .Largv
    lla a2, .Lenvp
    SC 221
    CHECK a0, -8, 29
    ARGS
    SC 172
    EQ a0, s3, 30
    ARGS
    SC 178
    EQ a0, s3, 31
    lw t0, 112(s0)
    CHECK t0, 0x13579, 32
    ARGS
    li a0, 1
    li a2, 1
    SC 62
    CHECK a0, 2, 33
    ret

.Lexternal:
    call .Lprepare
    SPAWN 0x1300f11, .Lexternal_child, 0, 34, .Lchild_stack_top, 64
    mv s2, a0
    WAIT 40, 1
    PHASE 1
    GO 1
    WAIT 72, 1
    ARGS
    mv a0, s2
    li a1, 10
    SC 129
    CHECK a0, 0, 35
    PUT 76, 1
    REAP s2, 36, 37
    ARGS
    li a2, 1
    SC 62
    CHECK a0, 0, 38
    ARGS
    li a0, 10
    lla a2, .Lold_action
    li a3, 8
    SC 134
    CHECK a0, 0, 39
    lla t1, .Lold_action
    ld t2, 0(t1)
    lla t3, .Lhandler
    EQ t2, t3, 40
    ld t2, 8(t1)
    CHECK t2, 4, 41
    ld t2, 16(t1)
    CHECK t2, 0x800, 42
    ARGS
    li a0, 1
    li a2, 1
    SC 62
    CHECK a0, 3, 43
    lw t0, 64(s0)
    fence r, rw
    CHECK t0, 0, 44
    j .Lsuccess
.Lexternal_child:
    ARGS
    addi a0, s0, 64
    SC 96
    ld t1, 0(s0)
    EQ a0, t1, 45
    PUT 40, 1
    GO 1
    ARM 46, 47
    PUT 72, 1
    WAIT 76, 1
    EXEC 88

.Lproduction:
    call .Lprepare
    EXEC 85

# /// 新映像真实检查 CLOEXEC、OFD、动作、pending、mask、timer 与 cwd。
.Lnew_resources:
    OLD_HOLE 51
    ARGS
    li a2, 1
    SC 62
    CHECK a0, -9, 52
    ARGS
    li a0, 1
    li a2, 1
    SC 62
    CHECK a0, 2, 53
    ARGS
    li a0, 1
    lla a1, .Lbyte
    li a2, 1
    SC 63
    CHECK a0, 1, 54
    ARGS
    li a0, 1
    li a2, 1
    SC 62
    CHECK a0, 3, 55
    ARGS
    li a0, 10
    lla a2, .Lold_action
    li a3, 8
    SC 134
    CHECK a0, 0, 56
    lla t1, .Lold_action
    ld t2, 0(t1)
    CHECK t2, 0, 57
    ld t2, 8(t1)
    CHECK t2, 0, 58
    ld t2, 16(t1)
    CHECK t2, 0, 59
    ARGS
    li a0, 12
    lla a2, .Lold_action
    li a3, 8
    SC 134
    CHECK a0, 0, 60
    lla t1, .Lold_action
    ld t2, 0(t1)
    CHECK t2, 1, 61
    ld t2, 8(t1)
    CHECK t2, 0, 62
    ld t2, 16(t1)
    CHECK t2, 0, 63
    ARGS
    lla a2, .Lmask
    li a3, 8
    SC 135
    CHECK a0, 0, 64
    lla t1, .Lmask
    ld t2, 0(t1)
    CHECK t2, 0x200, 65
    ARGS
    lla a0, .Lmask
    li a1, 8
    SC 136
    CHECK a0, 0, 66
    lla t1, .Lmask
    ld t2, 0(t1)
    CHECK t2, 0x200, 67
    ARGS
    lla a0, .Lmask
    lla a1, .Linfo
    lla a2, .Lzero_time
    li a3, 8
    SC 137
    CHECK a0, 10, 68
    lla t1, .Linfo
    lw t2, 0(t1)
    CHECK t2, 10, 69
    lw t2, 8(t1)
    CHECK t2, 0, 200
    ARGS
    lla a1, .Lold_stack
    SC 132
    CHECK a0, 0, 201
    lla t1, .Lold_stack
    lw t2, 8(t1)
    CHECK t2, 2, 202
    ARGS
    lla a1, .Lold_timer
    SC 102
    CHECK a0, 0, 203
    lla t1, .Lold_timer
    ld t2, 16(t1)
    ld t3, 24(t1)
    or t2, t2, t3
    POSITIVE t2, 204
    ARGS
    lla a1, .Ltimer_off
    SC 103
    CHECK a0, 0, 205
    ARGS
    li a0, -100
    lla a1, .Ldata_name
    SC 56
    CHECK a0, 0, 206
    SELECT 88, .Lnew_control
    MAP 0x32, 207
    li s1, 122
    j .Lvfork

# /// vfork 的父只能在 child 结束旧 Mm 使用后返回；新 V 等父的真实信号。
.Lvfork:
    MASK 0, 0x200, 70
    SPAWN 0x1304111, .Lvfork_child, 0, 71, .Lchild_stack_top, 64
    mv s2, a0
    lw t0, 64(s0)
    fence r, rw
    CHECK t0, 0, 72
    SELECT 122, .Lvfork_reap
    ARGS
    mv a0, s2
    li a1, 10
    SC 129
    CHECK a0, 0, 73
.Lvfork_reap:
    REAP s2, 74, 75
    j .Lsuccess
.Lvfork_child:
    ARGS
    SC 178
    fence rw, w
    sd a0, 0(s0)
    SELECT 122, .Lsuccess
    PHASE 1
    GO 1
    SELECT 107, .Lsuccess
    EXEC 86
.Lnew_vfork:
    CAPTURE 76
    MASK 1, 0x200, 77
1:
    lla t1, .Lhandled
    lw t0, 0(t1)
    fence r, rw
    beqz t0, 2f
    j .Lsuccess
2:
    ARGS
    SC 124
    j 1b

.Lgroup:
    SPAWN 0x110900, .Lgroup_child, 0, 78
    WAIT 40, 1
    PHASE 1
    GO 1
    ARGS
    li a0, 5
    SC 94
    li s8, 79
    j .Lexit
.Lgroup_child:
    PUT 40, 1
    ARGS
    addi a0, s0, 68
    li a1, 128
    SC 98
    li s8, 80
    j .Lexit

# /// leader 先退出仍保留同组父资格；孤儿转交内核根并验证号码角色复用。
.Lparent:
    ARGS
    SC 155
    mv s4, a0
    POSITIVE s4, 90
    ARGS
    SC 156
    mv s5, a0
    POSITIVE s5, 91
    FORK .Lprocess_a, 0, 92
    mv s2, a0
    WAIT 88, 1
    ARGS
    mv a0, s2
    mv a1, s4
    SC 154
    CHECK a0, -1, 93
    PUT 92, 1
    WAIT 80, 1
    ld t0, 48(s0)
    EQ t0, s2, 94
    ARGS
    mv a0, s2
    lla a1, .Lstatus
    li a2, 1
    SC 260
    CHECK a0, 0, 95
    ARGS
    ld a0, 16(s0)
    li a2, 1
    SC 260
    CHECK a0, -10, 96
    PHASE 1
    GO 1
    REAP s2, 97, 98
    PHASE 2
    WAIT 84, 1
    ld t0, 48(s0)
    CHECK t0, 0, 99
    ARGS
    ld a0, 16(s0)
    li a2, 1
    SC 260
    CHECK a0, -10, 100
    PHASE 3
    GO 4
    FORK .Lprocess_c, 16, 101
    EQ a0, s2, 102
    WAIT 108, 1
    PHASE 4
    GO 5
    REAP s2, 103, 104
    j .Lsuccess
.Lprocess_a:
    ARGS
    SC 172
    mv s2, a0
    ARGS
    SC 155
    EQ a0, s4, 105
    ARGS
    SC 156
    EQ a0, s5, 106
    ARGS
    SC 154
    CHECK a0, 0, 107
    ARGS
    SC 155
    EQ a0, s2, 108
    ARGS
    SC 157
    CHECK a0, -1, 109
    ARGS
    mv a1, s4
    SC 154
    CHECK a0, 0, 110
    ARGS
    SC 157
    EQ a0, s2, 111
    ARGS
    SC 155
    EQ a0, s2, 112
    ARGS
    SC 156
    EQ a0, s2, 113
    PUT 88, 1
    WAIT 92, 1
    ARGS
    SC 178
    sw a0, 104(s0)
    ARGS
    addi a0, s0, 104
    SC 96
    EQ a0, s2, 114
    FORK .Lprocess_b, 16, 115
    SPAWN 0x110900, .Lparent_peer, 32, 116, .Lthread_stack_top
    WAIT 72, 1
    WAIT 96, 1
    j .Lsuccess
.Lparent_peer:
    PUT 96, 1
    GO 1
    j .Lsuccess
.Lprocess_b:
    ARGS
    SC 173
    EQ a0, s2, 117
    PUT 72, 1
1:
    lw t0, 104(s0)
    fence r, rw
    beqz t0, 2f
    ARGS
    SC 124
    j 1b
2:
    ARGS
    SC 173
    EQ a0, s2, 118
    fence rw, w
    sd a0, 48(s0)
    PUT 80, 1
    WAIT 8, 2
    ARGS
    SC 173
    CHECK a0, 0, 119
    fence rw, w
    sd a0, 48(s0)
    PUT 84, 1
    GO 3
    j .Lsuccess
.Lprocess_c:
    ARGS
    SC 172
    EQ a0, s2, 120
    ARGS
    SC 155
    EQ a0, s4, 121
    ARGS
    SC 156
    EQ a0, s5, 122
    PUT 108, 1
    GO 5
    j .Lsuccess

# waitid siginfo=128B；rusage=144B。两个线程的输出区域互不重叠。
.macro NOHANG info, usage, status, c1, c2, c3, c4, c5
    addi t1, s0, \info
    li t2, 16
    li t3, -1
981:
    sd t3, 0(t1)
    addi t1, t1, 8
    addi t2, t2, -1
    bnez t2, 981b
    ARGS
    li a0, 1
    ld a1, 0(s0)
    addi a2, s0, \info
    li a3, 5
    SC 95
    CHECK a0, 0, \c1
    addi t1, s0, \info
    li t2, 16
981:
    ld t3, 0(t1)
    CHECK t3, 0, \c2
    addi t1, t1, 8
    addi t2, t2, -1
    bnez t2, 981b
    addi t1, s0, \usage
    li t2, 18
    li t3, 0x556677
981:
    sd t3, 0(t1)
    addi t1, t1, 8
    addi t2, t2, -1
    bnez t2, 981b
    li t3, 0x123456
    sw t3, \status(s0)
    ARGS
    ld a0, 0(s0)
    addi a1, s0, \status
    li a2, 1
    addi a3, s0, \usage
    SC 260
    CHECK a0, 0, \c3
    lw t3, \status(s0)
    CHECK t3, 0x123456, \c4
    addi t1, s0, \usage
    li t2, 18
981:
    ld t3, 0(t1)
    CHECK t3, 0x556677, \c5
    addi t1, t1, 8
    addi t2, t2, -1
    bnez t2, 981b
.endm
.macro OBSERVE info, usage, syscallcode, pidcode, signocode, codecode, statuscode
    ARGS
    li a0, 2
    addi a2, s0, \info
    li a3, 0x1000004
    addi a4, s0, \usage
    SC 95
    CHECK a0, 0, \syscallcode
    lw t2, \info+16(s0)
    ld t3, 0(s0)
    EQ t2, t3, \pidcode
    lw t2, \info(s0)
    CHECK t2, 17, \signocode
    lw t2, \info+8(s0)
    CHECK t2, 1, \codecode
    lw t2, \info+24(s0)
    CHECK t2, 0, \statuscode
.endm
# /// SIGCHLD处置由普通syscall设置；存活WNOHANG与离栈后的wait结果分阶段判定。
# 控制页0=child PID、8=phase、12=go、24=存活wait、28=退出wait、32=status、40=sigtimedwait。
.Lautoreap:
    MASK 0, 0x10000, 210
    lla t1, .Lchld_action
    SELECT 101, .Lautoreap_ignore
    SELECT 97, .Lautoreap_nocldwait
    j .Lautoreap_action
.Lautoreap_ignore:
    li t0, 1
    sd t0, 0(t1)
    j .Lautoreap_action
.Lautoreap_nocldwait:
    lla t0, .Lhandler
    sd t0, 0(t1)
    li t0, 2
    sd t0, 8(t1)
.Lautoreap_action:
    ARGS
    li a0, 17
    mv a1, t1
    li a3, 8
    SC 134
    CHECK a0, 0, 211
    FORK .Lautoreap_child, 0, 212
    mv s2, a0
    lla t1, .Lstatus
    li t0, 0x5a5a
    sw t0, 0(t1)
    ARGS
    mv a0, s2
    lla a1, .Lstatus
    li a2, 1
    SC 260
    fence rw, w
    sw a0, 24(s0)
    CHECK a0, 0, 213
    lla t1, .Lstatus
    lw t2, 0(t1)
    CHECK t2, 0x5a5a, 214
    PHASE 1
    # go=2只在夹具确认原Task已释放执行栈后发布，不拿child_tid清零猜退出。
    GO 2
    ARGS
    mv a0, s2
    lla a1, .Lstatus
    li a2, 1
    SC 260
    mv s4, a0
    lla t1, .Lstatus
    lw s5, 0(t1)
    ARGS
    lla a0, .Lchld_mask
    lla a1, .Linfo
    lla a2, .Lzero_time
    li a3, 8
    SC 137
    mv s6, a0
    fence rw, w
    sw s4, 28(s0)
    sw s5, 32(s0)
    sw s6, 40(s0)
    PHASE 3
    GO 3
    # 错误结果仍走原退出/wait/join，phase=3不表示测试通过。
    SELECT 100, .Lautoreap_default_result
    CHECK s4, -10, 220
    SELECT 101, .Lautoreap_ignore_result
    CHECK s6, 17, 221
    lla t1, .Linfo
    lw t2, 0(t1)
    CHECK t2, 17, 222
    lw t2, 8(t1)
    CHECK t2, 1, 223
    lw t2, 16(t1)
    EQ t2, s2, 224
    lw t2, 24(t1)
    CHECK t2, 23, 225
    ARGS
    lla a0, .Lchld_mask
    lla a2, .Lzero_time
    li a3, 8
    SC 137
    CHECK a0, -11, 227
    j .Lsuccess
.Lautoreap_ignore_result:
    CHECK s6, -11, 226
    j .Lsuccess
.Lautoreap_default_result:
    EQ s4, s2, 217
    CHECK s5, 0x1700, 218
    ARGS
    mv a0, s2
    li a2, 1
    SC 260
    CHECK a0, -10, 219
    j .Lsuccess

# /// 子只等待原共享匿名futex再正常exit(23)，父处置不改变实际执行者的离栈责任。
.Lautoreap_child:
    ARGS
    addi a0, s0, 12
    SC 98
    CHECK a0, 0, 215
    GO 1
    ARGS
    li a0, 23
    SC 93
    li s8, 216
    j .Lexit

# /// 两个真实阻塞等待者都观察同一快照，随后竞争唯一消费。
.Lwait:
    PUT 24, -4096
    PUT 28, -4096
    FORK .Lcpu_child, 0, 140
    SPAWN 0x1310900, .Lwait_worker, 32, 141, .Lthread_stack_top, 96
    NOHANG 128, 256, 848, 142, 143, 144, 145, 146
    WAIT 72, 1
    PUT 76, 1
    PHASE 1
    OBSERVE 128, 256, 147, 148, 149, 150, 151
    WAIT 80, 1
    OBSERVE 128, 544, 152, 153, 154, 155, 156
    # 用户CPU时间必须来自最终真实账本，而不是墙钟或猜测。
    ld t0, 256(s0)
    ld t1, 264(s0)
    or t0, t0, t1
    POSITIVE t0, 157
    addi t1, s0, 256
    addi t2, s0, 400
    addi t3, s0, 544
    li t4, 18
1:
    ld t0, 0(t1)
    ld t5, 0(t2)
    EQ t0, t5, 158
    ld t5, 0(t3)
    EQ t0, t5, 159
    addi t1, t1, 8
    addi t2, t2, 8
    addi t3, t3, 8
    addi t4, t4, -1
    bnez t4, 1b
    PUT 84, 1
    ARGS
    # 同一自然消费竞争覆盖 NULL status/infop，无额外子进程。
    SC 260
    li t0, -10
    beq a0, t0, 2f
    ld t0, 0(s0)
    EQ a0, t0, 160
    li a0, 0
2:
    fence rw, w
    sw a0, 24(s0)
    WAIT 88, 1
    lw t0, 24(s0)
    lw t1, 28(s0)
    li t2, -10
    beqz t0, 3f
    EQ t0, t2, 162
    CHECK t1, 0, 163
    j 4f
3:
    EQ t1, t2, 164
4:
    # 消费者线程结束后才创建 D，不让后续 fork 继承活的竞争成员。
5:
    lw t0, 96(s0)
    fence r, rw
    beqz t0, 6f
    ARGS
    SC 124
    j 5b
6:
    FORK .Lsuccess, 16, 165
    mv s2, a0
    ARGS
    li a0, 1
    mv a1, s2
    li a2, 0x70000000
    li a3, 4
    SC 95
    CHECK a0, -14, 166
    ARGS
    li a0, 1
    mv a1, s2
    li a3, 4
    SC 95
    CHECK a0, -10, 167
    ARGS
    mv a0, s2
    SC 260
    CHECK a0, -10, 168
    j .Lsuccess
.Lwait_worker:
    NOHANG 704, 400, 852, 173, 174, 175, 176, 177
    PUT 72, 1
    WAIT 76, 1
    PUT 40, 1
    OBSERVE 704, 400, 178, 179, 180, 181, 182
    PUT 80, 1
    WAIT 84, 1
    ARGS
    li a0, 1
    ld a1, 0(s0)
    li a3, 4
    SC 95
    mv s2, a0
    li t0, -10
    beq a0, t0, 1f
    CHECK a0, 0, 183
1:
    fence rw, w
    sw s2, 28(s0)
    PUT 88, 1
    j .Lsuccess
.Lcpu_child:
    # 非 PRIVATE WAIT 使用真实共享页，Native 的普通 WAKE 才能放行。
    ARGS
    addi a0, s0, 12
    SC 98
    CHECK a0, 0, 187
    GO 1
    li t0, 10000000
    li t1, 1
1:
    add t1, t1, t0
    xor t1, t1, t0
    addi t0, t0, -1
    bnez t0, 1b
    sd t1, 864(s0)
    j .Lsuccess

# /// 用户硬件错误由正常信号退出和wait判定，不借内核panic或控制syscall。
.Lsegv:
    li t0, 0x70000000
    ld t1, 0(t0)
    li s8, 203
    j .Lexit
.Lbus:
    ARGS
    li a0, -100
    lla a1, .Lempty
    SC 56
    NONNEG a0, 204
    mv s2, a0
    ARGS
    li a0, 0x47000000
    li a1, 4096
    li a2, 1
    li a3, 0x12
    mv a4, s2
    SC 222
    CHECK a0, 0x47000000, 205
    ld t1, 0(a0)
    li s8, 206
    j .Lexit
.Lill:
    .word 0
    li s8, 207
    j .Lexit

# /// 捕获处理器只发表本映像标志，返回内核提供的 rt_sigreturn trampoline。
.Lhandler:
    lla t1, .Lhandled
    li t0, 1
    fence rw, w
    sw t0, 0(t1)
    ret
.Lsuccess:
    li s8, 0
.Lexit:
    ARGS
    mv a0, s8
    # 线程断言失败终止本组，避免剩余成员永久等待一个失败的发布者。
    li a7, 93
    beqz s8, 1f
    li a7, 94
1:
    ecall
    .word 0
.size _start, .-_start

.section .rodata
.Lself: .asciz "/test/bin/user-lifecycle"
.Ldirectory: .asciz "/test"
.Ldata_path: .asciz "/test/io-data.bin"
.Ldata_name: .asciz "io-data.bin"
.Lmissing: .asciz "/test/no-lifecycle-image"
.Lunsupported: .asciz "/test/bin/user-interp"
.Lexec_env: .asciz "EXEC=1"
.Lempty: .asciz "/test/empty"

.data
.balign 16
.Lexec_mode: .asciz "U"
.balign 8
.Largv: .quad .Lself, .Lexec_mode, 0
.Lenvp: .quad .Lexec_env, 0
.Lcapture: .quad .Lhandler, 4, 0x800
.Lignore: .quad 1, 0x10000000, 0x800
.Lmask: .quad 0
.Lchld_mask: .quad 0x10000
.Lchld_action: .quad 0, 0, 0
.Lzero_time: .quad 0, 0
.Ltimer: .quad 0, 0, 3, 0
.Ltimer_off: .quad 0, 0, 0, 0
.Laltstack: .quad .Lalt_stack, 0, 16384
.Lold_action: .zero 24
.Lold_stack: .zero 24
.Lold_timer: .zero 32
.Linfo: .zero 128
.Lstatus: .zero 4
.Lhandled: .zero 4
.Lbyte: .zero 1

.bss
.balign 16
.Lchild_stack: .zero 16384
.Lchild_stack_top:
.Lthread_stack: .zero 16384
.Lthread_stack_top:
.Lalt_stack: .zero 16384
.Lalt_stack_top:
