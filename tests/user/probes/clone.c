#include "probe.h"

struct CloneAction { unsigned long handler, flags, mask; };
_Static_assert(sizeof(struct CloneAction) == 24, "raw action size");
static const struct CloneAction initial_action = { 0, 0x10000000, 0x800 };
static const struct CloneAction child_action = { 1, 0x40000000, 0x4000 };
static volatile unsigned *coordination;

struct CloneState {
    /* 这三个槽由无syscall观察桥在唯一probe_clone的边界写入。 */
    unsigned long expected_sp, returned_sp, returned_tp;
    unsigned long flags;
    unsigned mode_bits;
    volatile unsigned char *private_bytes;
    void *child_stack;
    long fd;
};
_Static_assert(offsetof(struct CloneState, expected_sp) == 0, "clone observer SP");
_Static_assert(offsetof(struct CloneState, returned_sp) == 8, "clone observer return SP");
_Static_assert(offsetof(struct CloneState, returned_tp) == 16, "clone observer TP");
long clone_state(unsigned long flags, void *stack_top, int *parent_tid,
                 unsigned long tls, int *child_tid, ProbeChildEntry entry, void *argument);

/// 原失败号完整写入协调页，并把父子两phase都发表为-1后只退出当前线程。
_Noreturn void probe_failure(long check)
{
    if (coordination != 0) {
        coordination[2] = (unsigned)check;
        probe_store_release(coordination, (unsigned)-1);
        probe_store_release(coordination + 1, (unsigned)-1);
    }
    probe_exit(check);
}

/// 每个phase至多一百万次观察，原yield失败和同伴失败仍有独立检查号。
static void wait_phase(unsigned slot, unsigned phase)
{
    unsigned remaining = 1000000;
    for (;;) {
        int value = (int)probe_load_acquire(coordination + slot);
        if (value < 0) {
            unsigned check = coordination[2];
            probe_failure(check != 0 ? check : 82);
        }
        if ((unsigned)value == phase)
            return;
        if (--remaining == 0)
            probe_failure(80);
        PROBE_CHECK(81, probe_sys0(SYS_sched_yield) == 0);
    }
}

/// 校验完整三字raw动作，避免只检查handler漏掉flags和mask。
static void check_action(const volatile struct CloneAction *actual,
                          const struct CloneAction *expected, long first_check)
{
    PROBE_CHECK(first_check, actual->handler == expected->handler);
    PROBE_CHECK(first_check + 1, actual->flags == expected->flags);
    PROBE_CHECK(first_check + 2, actual->mask == expected->mask);
}

/// 子先检查syscall当刻的SP/TP、TID与组身份，reserved模式另验证最低空fd0。
static void child_identity(struct CloneState *state, unsigned long entry_sp, unsigned long entry_tp)
{
    unsigned long expected_sp = (state->flags & 0x100) ?
                                (unsigned long)state->child_stack : state->expected_sp;
    PROBE_CHECK(40, entry_sp == expected_sp);
    PROBE_CHECK(41, entry_tp == 0x23456000);
    long tid = probe_sys0(SYS_gettid);
    PROBE_CHECK(42, 0 < tid && tid != coordination[5]);
    coordination[6] = (unsigned)tid;
    PROBE_CHECK(43, *(volatile unsigned *)(state->private_bytes + 8192) == (unsigned)tid);
    long pid = probe_sys0(SYS_getpid);
    PROBE_CHECK(44, 0 < pid);
    if (state->flags & 0x10000) {
        PROBE_CHECK(44, pid == coordination[4]);
    } else {
        PROBE_CHECK(44, pid == tid && pid != coordination[4]);
    }
    if (state->mode_bits & 8) {
        PROBE_CHECK(68, probe_sys4(SYS_openat, -100, "io-data.bin", 0, 0) == 0);
        PROBE_CHECK(69, probe_sys1(SYS_close, 0) == 0);
    }
}

/// 子真实store和read copyout分别触发COW，FD槽和FS/SIGHAND按独立轴修改。
static void child_resources(struct CloneState *state)
{
    state->private_bytes[0] = 0xcc;
    PROBE_CHECK(45, state->private_bytes[0] == 0xcc);
    PROBE_CHECK(46, probe_sys3(SYS_read, state->fd, state->private_bytes + 4096, 1) == 1);
    PROBE_CHECK(47, state->private_bytes[4096] == 0);
    PROBE_CHECK(48, probe_sys3(SYS_fcntl, state->fd, 1, 0) == 1);
    PROBE_CHECK(49, probe_sys3(SYS_fcntl, state->fd, 2, 0) == 0);
    PROBE_CHECK(50, probe_sys1(SYS_chdir, "/test/io-dir") == 0);
    PROBE_CHECK(51, probe_sys4(SYS_rt_sigaction, 10, &child_action, coordination + 24, 8) == 0);
    check_action((const volatile struct CloneAction *)(coordination + 24), &initial_action, 52);
    PROBE_CHECK(55, probe_sys4(SYS_rt_sigaction, 10, 0, coordination + 24, 8) == 0);
    check_action((const volatile struct CloneAction *)(coordination + 24), &child_action, 56);
}

/// 唯一共享clone桥直接进入子回调；首次shared fault、三阶段和exit语义保持原样。
static void clone_child(void *argument, unsigned long entry_sp, unsigned long entry_tp)
{
    struct CloneState *state = argument;
    child_identity(state, entry_sp, entry_tp);
    /* 第二共享页在fork前没有触页，必须经子首次真实store后由父读取。 */
    ((volatile unsigned char *)coordination)[4096] = 0x5e;
    probe_store_release(coordination, 1);
    wait_phase(1, 1);
    child_resources(state);
    probe_store_release(coordination, 2);
    wait_phase(1, 2);
    PROBE_CHECK(59, state->private_bytes[0] == ((state->flags & 0x100) ? 0xbb : 0xcc));
    PROBE_CHECK(60, state->private_bytes[4096] == ((state->flags & 0x100) ? 1 : 0));
    PROBE_CHECK(61, probe_sys1(SYS_close, state->fd) == 0);
    probe_store_release(coordination, 3);
    wait_phase(1, 3);
    probe_exit(0);
}

/// 父保留原OFD位置及FILES/FS/SIGHAND轴的独立观察，不扩代表运行集合。
static void parent_resources(struct CloneState *state)
{
    PROBE_CHECK(18, state->private_bytes[0] == ((state->flags & 0x100) ? 0xcc : 0xaa));
    PROBE_CHECK(19, state->private_bytes[4096] == ((state->flags & 0x100) ? 0 : 0xaa));
    PROBE_CHECK(20, probe_sys3(SYS_lseek, coordination[8], 0, 1) == 1);
    PROBE_CHECK(21, probe_sys3(SYS_fcntl, state->fd, 1, 0) == ((state->flags & 0x400) ? 0 : 1));
    long fd = probe_sys4(SYS_openat, -100, "first", 0, 0);
    if (state->flags & 0x200) {
        PROBE_CHECK(22, 0 <= fd);
        PROBE_CHECK(23, probe_sys3(SYS_read, fd, coordination + 10, 1) == 1);
        PROBE_CHECK(23, ((volatile unsigned char *)coordination)[40] == 'f');
    } else {
        PROBE_CHECK(22, fd == -ENOENT);
        fd = probe_sys4(SYS_openat, -100, "io-data.bin", 0, 0);
        PROBE_CHECK(23, 0 <= fd);
    }
    PROBE_CHECK(24, probe_sys1(SYS_close, fd) == 0);
    PROBE_CHECK(25, probe_sys4(SYS_rt_sigaction, 10, 0, coordination + 16, 8) == 0);
    check_action((const volatile struct CloneAction *)(coordination + 16),
                  (state->flags & 0x800) ? &child_action : &initial_action, 26);
    state->private_bytes[0] = 0xbb;
    PROBE_CHECK(29, probe_sys3(SYS_read, coordination[8], state->private_bytes + 4096, 1) == 1);
    PROBE_CHECK(30, state->private_bytes[4096] == 1);
}

/// 父完整核对clone返回、坏parent_tid成功不撤及每个原phase。
static _Noreturn void clone_parent(struct CloneState *state, long child)
{
    PROBE_CHECK(12, 0 <= child);
    PROBE_CHECK(13, state->returned_sp == state->expected_sp);
    PROBE_CHECK(14, state->returned_tp == 0x12345000);
    PROBE_CHECK(15, coordination[3] == ((state->mode_bits & 4) ? 0 : (unsigned)child));
    wait_phase(0, 1);
    PROBE_CHECK(34, ((volatile unsigned char *)coordination)[4096] == 0x5e);
    PROBE_CHECK(16, coordination[6] == (unsigned)child);
    PROBE_CHECK(17, *(volatile unsigned *)(state->private_bytes + 8192) ==
                      ((state->flags & 0x100) ? (unsigned)child : 0));
    probe_store_release(coordination + 1, 1);
    wait_phase(0, 2);
    parent_resources(state);
    probe_store_release(coordination + 1, 2);
    wait_phase(0, 3);
    long flags = probe_sys3(SYS_fcntl, state->fd, 1, 0);
    if (state->flags & 0x400) {
        PROBE_CHECK(31, flags == -EBADF);
    } else {
        PROBE_CHECK(31, flags == 1);
        PROBE_CHECK(32, probe_sys1(SYS_close, state->fd) == 0);
    }
    PROBE_CHECK(33, probe_sys1(SYS_close, coordination[8]) == 0);
    probe_store_release(coordination + 1, 3);
    /* 完成字只表示用户检查结束；真实child退出及离栈由原内核观察者消费。 */
    probe_exit(0);
}

/// 解码全部大小写f/v/s/t/r与0..3资源轴，稀有模式仍是正式可执行输入。
static void parse_mode(struct CloneState *state, const unsigned long *initial_stack)
{
    PROBE_CHECK(1, ((unsigned long)initial_stack & 15) == 0);
    PROBE_CHECK(1, initial_stack[0] == 2);
    const unsigned char *mode = (void *)initial_stack[2];
    PROBE_CHECK(1, mode != 0);
    PROBE_CHECK(1, mode[2] == 0);
    unsigned bits = (unsigned)mode[1] - '0';
    PROBE_CHECK(1, bits < 4);
    if ((mode[0] & 32) == 0)
        bits |= 4;
    switch (mode[0] | 32) {
    case 'f': state->flags = 17; break;
    case 'r': state->flags = 17; bits |= 8; break;
    case 'v': state->flags = 0x111; break;
    case 's': state->flags = 0x911; break;
    case 't': state->flags = 0x10900; break;
    default: probe_failure(1);
    }
    state->mode_bits = bits;
    state->flags |= ((bits & 1) << 10) | ((bits & 2) << 8) | 0x1180000;
}

/// 准备原匿名shared/COW页和空FD表资源，再以唯一clone桥进入真实父子协议。
_Noreturn void probe_main(const unsigned long *initial_stack)
{
    struct CloneState state = { 0 };
    parse_mode(&state, initial_stack);
    long shared = probe_sys6(SYS_mmap, 0, 8192, 3, 0x21, -1, 0);
    PROBE_CHECK(2, 0 <= shared);
    coordination = (volatile unsigned *)shared;
    coordination[0] = 0;
    coordination[1] = 0;
    coordination[2] = 0;
    coordination[3] = 0;
    long private = probe_sys6(SYS_mmap, 0, 12288, 3, 0x22, -1, 0);
    PROBE_CHECK(3, 0 <= private);
    state.private_bytes = (volatile unsigned char *)private;
    state.private_bytes[0] = 0xaa;
    state.private_bytes[4096] = 0xaa;
    *(volatile unsigned *)(state.private_bytes + 8192) = 0;
    if (state.flags & 0x100) {
        long stack = probe_sys6(SYS_mmap, 0, 8192, 3, 0x22, -1, 0);
        PROBE_CHECK(4, 0 <= stack);
        state.child_stack = (void *)(stack + 8192);
    }
    PROBE_CHECK(5, probe_sys1(SYS_chdir, "/test") == 0);
    state.fd = probe_sys4(SYS_openat, -100, "io-data.bin", 0, 0);
    PROBE_CHECK(6, 0 <= state.fd);
    PROBE_CHECK(7, probe_sys3(SYS_fcntl, state.fd, 2, 1) == 0);
    long duplicate = probe_sys1(SYS_dup, state.fd);
    PROBE_CHECK(8, 0 <= duplicate);
    coordination[8] = (unsigned)duplicate;
    PROBE_CHECK(9, probe_sys4(SYS_rt_sigaction, 10, &initial_action, 0, 8) == 0);
    long pid = probe_sys0(SYS_getpid);
    PROBE_CHECK(10, 0 < pid);
    coordination[4] = (unsigned)pid;
    long tid = probe_sys0(SYS_gettid);
    PROBE_CHECK(11, 0 < tid);
    coordination[5] = (unsigned)tid;
    long child = clone_state(state.flags, state.child_stack,
                             (state.mode_bits & 4) ? (int *)1 : (int *)(coordination + 3),
                             0x23456000, (int *)(state.private_bytes + 8192), clone_child, &state);
    clone_parent(&state, child);
}
