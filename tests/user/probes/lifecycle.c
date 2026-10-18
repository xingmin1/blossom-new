#include "lifecycle.h"

struct Lifecycle lifecycle;
unsigned char lifecycle_child_stack[16384] __attribute__((aligned(16)));
unsigned char lifecycle_thread_stack[16384] __attribute__((aligned(16)));
static unsigned char alt_stack[16384] __attribute__((aligned(16)));
static volatile unsigned handled;
const char lifecycle_self[] = "/test/bin/user-lifecycle";
static const char exec_environment[] = "EXEC=1";
static const unsigned long zero_time[2] = { 0, 0 };
static const unsigned long timer_on[4] = { 0, 0, 3, 0 };
static const unsigned long timer_off[4] = { 0, 0, 0, 0 };

/// 任一线程断言失败终止整组，避免同伴永久等候失败的发布者。
_Noreturn void probe_failure(long check)
{
    probe_exit_group(check);
}

/// 捕获USR1只发表本映像标志，按内核提供的返回地址恢复信号帧。
void lifecycle_handler(void)
{
    probe_store_release(&handled, 1);
}

/// 固定地址映射保留原fixture使用的虚拟地址及MAP_SHARED/PRIVATE区别。
void lifecycle_map(unsigned flags, long check)
{
    PROBE_CHECK(check, probe_sys6(SYS_mmap, 0x44000000, 4096, 3, flags, -1, 0)
                          == 0x44000000);
}

/// 旧Mm已撤销时，futex必须在原地址返回EFAULT。
static void old_hole(long check)
{
    PROBE_CHECK(check, probe_sys6(SYS_futex, 0x44000044, 128, 0, 0, 0, 0) == -EFAULT);
}

/// 改变真实信号mask，调用期间本地64位输入保持有效。
static void set_mask(int how, unsigned long mask, long check)
{
    PROBE_CHECK(check, probe_sys4(SYS_rt_sigprocmask, how, &mask, 0, 8) == 0);
}

/// 安装原24字节raw动作，不使用带restorer的libc布局。
static void capture(long check)
{
    const struct LifecycleAction action = { (unsigned long)lifecycle_handler, 4, 0x800 };
    PROBE_CHECK(check, probe_sys4(SYS_rt_sigaction, 10, &action, 0, 8) == 0);
}

/// 真实备用栈和REAL计时器随旧Mm建立，exec分别禁用栈并保留计时器。
static void arm_timer_stack(long timer_check, long stack_check)
{
    const unsigned long stack[3] = { (unsigned long)alt_stack, 0, sizeof(alt_stack) };
#ifdef __riscv
    PROBE_CHECK(timer_check, probe_sys3(SYS_setitimer, 0, timer_on, 0) == 0);
    PROBE_CHECK(stack_check, probe_sys2(SYS_sigaltstack, stack, 0) == 0);
#else
    PROBE_CHECK(stack_check, probe_sys2(SYS_sigaltstack, stack, 0) == 0);
    PROBE_CHECK(timer_check, probe_sys3(SYS_setitimer, 0, timer_on, 0) == 0);
#endif
}

/// exec参数在提交前保持有效，成功不能回到旧映像。
_Noreturn void lifecycle_exec(char mode, long check)
{
    char mode_string[2] = { mode, 0 };
    const char *argv[3] = { lifecycle_self, mode_string, 0 };
    const char *envp[2] = { exec_environment, 0 };
    probe_sys3(SYS_execve, lifecycle_self, argv, envp);
    probe_failure(check);
}

/// 新映像到达phase100后仍由原父白盒检查对象接任和资源拥有。
static _Noreturn void new_control(void)
{
    lifecycle_map(0x32, LC(14, 16));
    put_word(8, 100);
    wait_word(12, 100);
    lifecycle_success();
}

/// fork/vfork子只在真实旧Mm使用结束后释放父；V映像等父的实际USR1。
static void vfork_child(void *argument, unsigned long entry_sp, unsigned long entry_tp)
{
    (void)argument; (void)entry_sp; (void)entry_tp;
    long tid = probe_sys0(SYS_gettid);
#ifndef __riscv
    PROBE_CHECK(81, 0 < tid);
#endif
    put_slot(0, (unsigned long)tid);
    if (lifecycle.mode == 'z')
        lifecycle_success();
    put_word(8, 1);
    wait_word(12, 1);
    if (lifecycle.mode == 'k')
        lifecycle_success();
    lifecycle_exec('V', LC(16, 82));
}

/// vfork的clear_tid、父返回及wait全部来自真实clone/exec/退出协议。
_Noreturn void lifecycle_vfork(void)
{
    set_mask(0, 0x200, LC(70, 75));
    long child = lifecycle_spawn(LC(0x1304111, 0x1204111), vfork_child,
                                 LC(0, -1), 64, lifecycle_child_stack + 16384,
                                 LC(71, 76));
    CHECK_PAIR(72, 77, get_word(64) == 0);
    if (lifecycle.mode != 'z')
        CHECK_PAIR(73, 78, probe_sys2(SYS_kill, child, 10) == 0);
    lifecycle_reap(child, LC(74, 79), LC(75, 80));
    lifecycle_success();
}

/// 新V恢复原USR1捕获并解除mask，不能以轮询到时限代替实际信号。
static _Noreturn void new_vfork(void)
{
    capture(LC(76, 83));
    set_mask(1, 0x200, LC(77, 84));
    while (probe_load_acquire(&handled) == 0) {
#ifdef __riscv
        probe_sys0(SYS_sched_yield);
#endif
    }
#ifndef __riscv
    PROBE_CHECK(85, probe_load_acquire(&handled) == 1);
#endif
    lifecycle_success();
}

/// 逐项验证新映像CLOEXEC、OFD、动作、pending、mask、timer与cwd。
static _Noreturn void new_resources(void)
{
    struct LifecycleAction action;
    unsigned long mask = 0;
    unsigned char info[128] __attribute__((aligned(8)));
    unsigned long stack[3], timer[4];
    unsigned char byte;
#ifdef __riscv
    old_hole(51);
#endif
    CHECK_PAIR(52, 54, probe_sys3(SYS_lseek, 0, 0, 1) == -EBADF);
    CHECK_PAIR(53, 55, probe_sys3(SYS_lseek, 1, 0, 1) == 2);
    CHECK_PAIR(54, 56, probe_sys3(SYS_read, 1, &byte, 1) == 1);
    CHECK_PAIR(55, 57, probe_sys3(SYS_lseek, 1, 0, 1) == 3);
    CHECK_PAIR(56, 58, probe_sys4(SYS_rt_sigaction, 10, 0, &action, 8) == 0);
    CHECK_PAIR(57, 59, action.handler == 0);
    CHECK_PAIR(58, 60, action.flags == 0);
    CHECK_PAIR(59, 61, action.mask == 0);
    CHECK_PAIR(60, 62, probe_sys4(SYS_rt_sigaction, 12, 0, &action, 8) == 0);
    CHECK_PAIR(61, 63, action.handler == 1);
    CHECK_PAIR(62, 201, action.flags == 0);
    CHECK_PAIR(63, 202, action.mask == 0);
    PROBE_CHECK(64, probe_sys4(SYS_rt_sigprocmask, 0, 0, &mask, 8) == 0);
    PROBE_CHECK(65, mask == 0x200);
    PROBE_CHECK(66, probe_sys2(SYS_rt_sigpending, &mask, 8) == 0);
    PROBE_CHECK(67, mask == 0x200);
    PROBE_CHECK(68, probe_sys4(SYS_rt_sigtimedwait, &mask, info, zero_time, 8) == 10);
#ifdef __riscv
    PROBE_CHECK(69, *(int *)info == 10);
    PROBE_CHECK(200, *(int *)(info + 8) == 0);
#endif
    CHECK_PAIR(201, 69, probe_sys2(SYS_sigaltstack, 0, stack) == 0);
    CHECK_PAIR(202, 70, *(int *)((char *)stack + 8) == 2);
#ifdef __riscv
    PROBE_CHECK(203, probe_sys2(SYS_getitimer, 0, timer) == 0);
    PROBE_CHECK(204, 0 < (timer[2] | timer[3]));
    PROBE_CHECK(205, probe_sys3(SYS_setitimer, 0, timer_off, 0) == 0);
#else
    PROBE_CHECK(71, probe_sys3(SYS_setitimer, 0, timer_off, timer) == 0);
    PROBE_CHECK(72, 0 < (timer[2] | timer[3]));
#endif
    CHECK_PAIR(206, 73, probe_sys4(SYS_openat, -100, "io-data.bin", 0, 0) == 0);
    if (lifecycle.mode == 'X')
        new_control();
    lifecycle_map(0x32, LC(207, 74));
    lifecycle.mode = 'z';
    lifecycle_vfork();
}

/// 按原初始栈逐字节核验exec路径、唯一环境和字符串终止。
static _Noreturn void new_image(const unsigned long *initial_stack)
{
    const unsigned char *path = (void *)initial_stack[1];
    const unsigned char *expected = (const unsigned char *)lifecycle_self;
    CHECK_PAIR(208, 11, path != 0);
    do {
        CHECK_PAIR(209, 11, *path == *expected);
    } while (*path++ != 0 && *expected++ != 0);
    const unsigned char *environment = (void *)initial_stack[4];
    CHECK_PAIR(9, 12, environment != 0);
    for (unsigned i = 0; i < sizeof(exec_environment); ++i)
        CHECK_PAIR(10, 13, environment[i] == (unsigned char)exec_environment[i]);
    CHECK_PAIR(11, 14, initial_stack[5] == 0);
    CHECK_PAIR(12, 10, lifecycle.pid == lifecycle.tid);
    if (lifecycle.mode == 'V')
        new_vfork();
    old_hole(LC(13, 15));
    if (lifecycle.mode == 'N')
        new_control();
    new_resources();
}

/// 准备失败exec保留旧身份、映射、OFD、信号和cwd，成功路径继续使用原资源。
static void prepare_exec(void)
{
    const struct LifecycleAction ignore = { 1, 0x10000000, 0x800 };
    const char *argv[3] = { lifecycle_self, "X", 0 };
    const char *envp[2] = { exec_environment, 0 };
    CHECK_PAIR(20, 23, probe_sys4(SYS_openat, -100, "/test/io-data.bin", 0x80000, 0) == 0);
    CHECK_PAIR(21, 24, probe_sys4(SYS_openat, -100, "/test/io-data.bin", 0, 0) == 1);
    CHECK_PAIR(22, 25, probe_sys3(SYS_lseek, 1, 2, 0) == 2);
    CHECK_PAIR(23, 26, probe_sys1(SYS_chdir, "/test") == 0);
    capture(LC(24, 27));
    CHECK_PAIR(25, 28, probe_sys4(SYS_rt_sigaction, 12, &ignore, 0, 8) == 0);
    set_mask(0, 0x200, LC(26, 29));
    if (lifecycle.mode == 'u') {
        arm_timer_stack(LC(48, 53), LC(49, 52));
        PROBE_CHECK(50, probe_sys2(SYS_kill, lifecycle.pid, 10) == 0);
    }
    put_word(LC(112, 128), LC(0x13579, 0x1234));
    CHECK_PAIR(27, 30, probe_sys3(SYS_execve,
                    LC("/test/no-lifecycle-image", "/test/no-such-lifecycle"), argv, envp) == -ENOENT);
    CHECK_PAIR(28, 31, probe_sys3(SYS_execve, lifecycle_self, 0x70000000, envp) == -EFAULT);
    CHECK_PAIR(29, 32, probe_sys3(SYS_execve, "/test/bin/user-interp", argv, envp) == -ENOEXEC);
    CHECK_PAIR(30, 34, probe_sys0(SYS_getpid) == lifecycle.pid);
    CHECK_PAIR(31, 35, probe_sys0(SYS_gettid) == lifecycle.tid);
    CHECK_PAIR(32, 33, get_word(LC(112, 128)) == LC(0x13579, 0x1234));
    CHECK_PAIR(33, 36, probe_sys3(SYS_lseek, 1, 0, 1) == 2);
}

/// 外部共享者在旧Mm保留原表和动作，child执行exec才分离资源。
static void external_child(void *argument, unsigned long entry_sp, unsigned long entry_tp)
{
    (void)argument; (void)entry_sp; (void)entry_tp;
    long tid = probe_sys1(SYS_set_tid_address, control_at(64));
#ifdef __riscv
    PROBE_CHECK(45, (unsigned long)tid == get_slot(0));
#else
    PROBE_CHECK(48, 0 < tid);
#endif
    put_word(40, 1);
    wait_word(12, 1);
    arm_timer_stack(LC(46, 53), LC(47, 52));
    put_word(72, 1);
    wait_word(76, 1);
    lifecycle_exec('X', LC(16, 49));
}

/// 父保留旧OFD共享位置与动作；clear_tid只作Mm移交的观察之一。
static _Noreturn void external_exec(void)
{
    struct LifecycleAction action;
    prepare_exec();
    long child = lifecycle_spawn(0x1300f11, external_child, 0, 64,
                                 lifecycle_child_stack + 16384, LC(34, 37));
    wait_word(40, 1);
    put_word(8, 1);
    wait_word(12, 1);
    wait_word(72, 1);
    CHECK_PAIR(35, 38, probe_sys2(SYS_kill, child, 10) == 0);
    put_word(76, 1);
    lifecycle_reap(child, LC(36, 39), LC(37, 40));
    CHECK_PAIR(38, 42, probe_sys3(SYS_lseek, 0, 0, 1) == 0);
    CHECK_PAIR(39, 43, probe_sys4(SYS_rt_sigaction, 10, 0, &action, 8) == 0);
    CHECK_PAIR(40, 44, action.handler == (unsigned long)lifecycle_handler);
    CHECK_PAIR(41, 45, action.flags == 4);
    CHECK_PAIR(42, 46, action.mask == 0x800);
    CHECK_PAIR(43, 47, probe_sys3(SYS_lseek, 1, 0, 1) == 3);
    CHECK_PAIR(44, 41, get_word(64) == 0);
    lifecycle_success();
}

/// 非leader或竞争线程在同一真实Task内提交新映像。
static void exec_peer(void *argument, unsigned long entry_sp, unsigned long entry_tp)
{
    (void)argument; (void)entry_sp; (void)entry_tp;
    put_word(40, 1);
    wait_word(12, 1);
    lifecycle_exec('N', LC(16, lifecycle.mode == 'n' ? 19 : 22));
}

/// leader或双竞争者分别保留真实阻塞/exec状态供父观察。
static _Noreturn void exec_threads(void)
{
    lifecycle_spawn(0x110900, exec_peer, 0, -1, lifecycle_child_stack + 16384,
                     LC(lifecycle.mode == 'n' ? 17 : 19, lifecycle.mode == 'n' ? 17 : 20));
    wait_word(40, 1);
    put_word(8, 1);
    if (lifecycle.mode == 'n') {
        probe_sys6(SYS_futex, control_at(68), 128, 0, 0, 0, 0);
        probe_failure(18);
    }
    wait_word(12, 1);
    lifecycle_exec('N', LC(16, 21));
}

/// 阻塞成员只能由真正exit_group驱逐，futex正常返回属于失败。
static void group_peer(void *argument, unsigned long entry_sp, unsigned long entry_tp)
{
    (void)argument; (void)entry_sp; (void)entry_tp;
    put_word(40, 1);
    probe_sys6(SYS_futex, control_at(68), 128, 0, 0, 0, 0);
    probe_failure(LC(80, 88));
}

/// exit_group(5)必须驱逐真实阻塞成员，不统一替换成exit。
static _Noreturn void exit_group_case(void)
{
    lifecycle_spawn(0x110900, group_peer, 0, -1, lifecycle_child_stack + 16384, LC(78, 86));
    wait_word(40, 1);
    put_word(8, 1);
    wait_word(12, 1);
    probe_sys1(SYS_exit_group, 5);
    probe_failure(LC(79, 87));
}

/// 对未映射页、文件EOF页和非法指令执行真实硬件访问。
static _Noreturn void fault_case(void)
{
    if (lifecycle.mode == 's') {
        (void)*(volatile unsigned long *)0x70000000UL;
        probe_failure(203);
    }
    if (lifecycle.mode == 'i')
        lifecycle_illegal();
    long fd = probe_sys4(SYS_openat, -100, "/test/empty", 0, 0);
    PROBE_CHECK(204, 0 <= fd);
    PROBE_CHECK(205, probe_sys6(SYS_mmap, 0x47000000, 4096, 1, 0x12, fd, 0) == 0x47000000);
    (void)*(volatile unsigned long *)0x47000000UL;
    probe_failure(206);
}

/// 入口保留原16字节栈、单字符模式及argv终止约束，分派全部旧模式。
_Noreturn void probe_main(const unsigned long *initial_stack)
{
    PROBE_CHECK(1, ((unsigned long)initial_stack & 15) == 0);
    PROBE_CHECK(2, initial_stack[0] == 2);
    const unsigned char *mode = (void *)initial_stack[2];
    PROBE_CHECK(3, mode != 0);
    PROBE_CHECK(4, mode[1] == 0);
    PROBE_CHECK(5, initial_stack[3] == 0);
    lifecycle.mode = mode[0];
#ifdef __riscv
    if (lifecycle.mode != 'N' && lifecycle.mode != 'X' && lifecycle.mode != 'V' &&
        lifecycle.mode != 'U' && lifecycle.mode != 's' && lifecycle.mode != 'b' &&
        lifecycle.mode != 'i')
        lifecycle_map(0x31, 6);
#endif
    lifecycle.pid = probe_sys0(SYS_getpid);
    lifecycle.tid = probe_sys0(SYS_gettid);
#ifndef __riscv
    PROBE_CHECK(6, 0 < lifecycle.pid);
    PROBE_CHECK(7, 0 < lifecycle.tid);
#endif
    if (lifecycle.mode == 'N' || lifecycle.mode == 'X' || lifecycle.mode == 'V' || lifecycle.mode == 'U')
        new_image(initial_stack);
    if (lifecycle.mode == 's' || lifecycle.mode == 'b' || lifecycle.mode == 'i')
        fault_case();
#ifdef __riscv
    PROBE_CHECK(7, lifecycle.pid == lifecycle.tid);
#else
    lifecycle_map(0x31, 8);
    put_word(24, (unsigned)-4096);
    put_word(28, (unsigned)-4096);
#endif
    switch (lifecycle.mode) {
    case 'n': case 'c': exec_threads();
    case 'x': external_exec();
    case 'u': prepare_exec(); lifecycle_exec('U', LC(16, 51));
    case 'z': case 'v': case 'k': lifecycle_vfork();
    case 'g': exit_group_case();
    case 'p': lifecycle_parent();
    case 'w': lifecycle_wait();
    case 'd': case 'e': case 'a': lifecycle_autoreap();
    default: probe_failure(LC(8, 9));
    }
}
