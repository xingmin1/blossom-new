#include "lifecycle.h"

static const unsigned long zero_time[2] = { 0, 0 };

/// 无事件时siginfo全清零，wait4的status和rusage全部保持输入哨兵。
static void nohang(unsigned info, unsigned usage, unsigned status,
                   long c1, long c2, long c3, long c4, long c5)
{
    const unsigned long sentinel = LC(0x556677, 0x12345678);
    for (unsigned i = 0; i < 16; ++i)
        put_slot(info + i * 8, LC(~0UL, 0x12345678));
    PROBE_CHECK(c1, probe_sys5(SYS_waitid, 1, get_slot(0), control_at(info), 5, 0) == 0);
    for (unsigned i = 0; i < 16; ++i)
        PROBE_CHECK(c2, get_slot(info + i * 8) == 0);
    for (unsigned i = 0; i < 18; ++i)
        put_slot(usage + i * 8, sentinel);
    put_word(status, LC(0x123456, 0x12345678));
    PROBE_CHECK(c3, probe_sys4(SYS_wait4, get_slot(0), control_at(status), 1, control_at(usage)) == 0);
    PROBE_CHECK(c4, get_word(status) == LC(0x123456, 0x12345678));
    for (unsigned i = 0; i < 18; ++i)
        PROBE_CHECK(c5, get_slot(usage + i * 8) == sentinel);
}

/// WNOWAIT保留最终快照供两个真实等待者反复观察，不消费子进程。
static void observe(unsigned info, unsigned usage,
                    long syscall_check, long pid_check, long signo_check,
                    long code_check, long status_check)
{
    PROBE_CHECK(syscall_check, probe_sys5(SYS_waitid, 2, 0, control_at(info),
                                         0x1000004, control_at(usage)) == 0);
    PROBE_CHECK(pid_check, get_word(info + 16) == get_slot(0));
    PROBE_CHECK(signo_check, get_word(info) == 17);
    PROBE_CHECK(code_check, get_word(info + 8) == 1);
    PROBE_CHECK(status_check, get_word(info + 24) == 0);
#ifndef __riscv
    PROBE_CHECK(164, get_word(info + 4) == 0);
    PROBE_CHECK(167, get_word(info + 20) == 0);
#endif
}

/// 原阻塞CPU子由父的真实非PRIVATE futex WAKE唤醒，再产生用户CPU时间。
static void cpu_child(void *argument, unsigned long entry_sp, unsigned long entry_tp)
{
    (void)argument; (void)entry_sp; (void)entry_tp;
    CHECK_PAIR(187, 155, probe_sys6(SYS_futex, control_at(12), 0, 0, 0, 0, 0) == 0);
#ifdef __riscv
    wait_word(12, 1);
#endif
    unsigned long value = 1;
    /* volatile阻止把真实用户计算替换为常量，账本仍来自普通timer抢占。 */
    for (volatile unsigned long count = LC(10000000, 20000000); 0 < count; --count) {
        value += count;
        value ^= count;
    }
    put_slot(LC(864, 120), value);
    lifecycle_success();
}

/// 第二真实等待者先观察同一快照，随后与父竞争唯一消费。
static void wait_worker(void *argument, unsigned long entry_sp, unsigned long entry_tp)
{
    (void)argument; (void)entry_sp; (void)entry_tp;
    nohang(704, 400, LC(852, 116), LC(173, 157), LC(174, 158),
           LC(175, 159), LC(176, 160), LC(177, 161));
    put_word(72, 1);
    wait_word(76, 1);
    put_word(40, 1);
    observe(704, 400, LC(178, 162), LC(179, 166), LC(180, 163), LC(181, 165), LC(182, 168));
    put_word(80, 1);
    wait_word(84, 1);
    long result = probe_sys5(SYS_waitid, 1, get_slot(0), 0, 4, 0);
    if (result != -ECHILD)
        CHECK_PAIR(183, 156, result == 0);
    put_word(28, (unsigned)result);
#ifndef __riscv
    put_word(40, 2);
#endif
    put_word(88, 1);
    lifecycle_success();
}

/// 无业务操作的fork子正常exit，供坏copyout后消费状态检查。
static void empty_child(void *argument, unsigned long entry_sp, unsigned long entry_tp)
{
    (void)argument; (void)entry_sp; (void)entry_tp;
    lifecycle_success();
}

/// 保留双waiter、全rusage、唯一消费与EFAULT后已消费的原wait协议。
_Noreturn void lifecycle_wait(void)
{
    put_word(24, (unsigned)-4096);
    put_word(28, (unsigned)-4096);
    lifecycle_fork(cpu_child, 0, 140);
    lifecycle_spawn(0x1310900, wait_worker, 32, 96, lifecycle_thread_stack + 16384, 141);
    nohang(128, 256, LC(848, 112), LC(142, 157), LC(143, 158),
           LC(144, 159), LC(145, 160), LC(146, 161));
    wait_word(72, 1);
    put_word(76, 1);
    put_word(8, 1);
    observe(128, 256, LC(147, 162), LC(148, 166), LC(149, 163), LC(150, 165), LC(151, 168));
    wait_word(80, 1);
    observe(128, 544, LC(152, 162), LC(153, 166), LC(154, 163), LC(155, 165), LC(156, 168));
    CHECK_PAIR(157, 144, 0 < (get_slot(256) | get_slot(264)));
    for (unsigned i = 0; i < 18; ++i) {
        CHECK_PAIR(158, 142, get_slot(256 + i * 8) == get_slot(400 + i * 8));
        CHECK_PAIR(159, 143, get_slot(256 + i * 8) == get_slot(544 + i * 8));
    }
#ifndef __riscv
    for (unsigned i = 0; i < 14; ++i)
        PROBE_CHECK(169, get_slot(288 + i * 8) == 0);
    PROBE_CHECK(170, get_slot(264) < 1000000);
    PROBE_CHECK(171, get_slot(280) < 1000000);
#endif
    put_word(84, 1);
    long result = probe_sys4(SYS_wait4, 0, 0, 0, 0);
    if (result != -ECHILD) {
        CHECK_PAIR(160, 145, (unsigned long)result == get_slot(0));
        result = 0;
    }
    put_word(24, (unsigned)result);
    wait_word(88, 1);
    int first = (int)get_word(24), second = (int)get_word(28);
    if (first != 0) {
        CHECK_PAIR(162, 147, first == -ECHILD);
        CHECK_PAIR(163, 148, second == 0);
    } else {
        CHECK_PAIR(164, 149, second == -ECHILD);
    }
#ifndef __riscv
    PROBE_CHECK(150, get_word(40) == 2);
#endif
    wait_zero(96);
    long child = lifecycle_fork(empty_child, 16, LC(165, 151));
    CHECK_PAIR(166, 152, probe_sys5(SYS_waitid, 1, child, 0x70000000, 4, 0) == -EFAULT);
    CHECK_PAIR(167, 153, probe_sys5(SYS_waitid, 1, child, 0, 4, 0) == -ECHILD);
    CHECK_PAIR(168, 154, probe_sys4(SYS_wait4, child, 0, 0, 0) == -ECHILD);
    lifecycle_success();
}

/// 自动回收子必须先在原共享futex真正阻塞，再以exit(23)释放真实Task。
static void autoreap_child(void *argument, unsigned long entry_sp, unsigned long entry_tp)
{
    (void)argument; (void)entry_sp; (void)entry_tp;
    PROBE_CHECK(215, probe_sys6(SYS_futex, control_at(12), 0, 0, 0, 0, 0) == 0);
    wait_word(12, 1);
    probe_sys1(SYS_exit, 23);
    probe_failure(216);
}

/// 保留默认、忽略和NOCLDWAIT三种回收语义及完整phase3失败报告。
_Noreturn void lifecycle_autoreap(void)
{
    const unsigned long mask = 0x10000;
    struct LifecycleAction action = { 0, 0, 0 };
    unsigned char info[128] __attribute__((aligned(8)));
    PROBE_CHECK(210, probe_sys4(SYS_rt_sigprocmask, 0, &mask, 0, 8) == 0);
    if (lifecycle.mode == 'e')
        action.handler = 1;
    if (lifecycle.mode == 'a') {
        action.handler = (unsigned long)lifecycle_handler;
        action.flags = 2;
    }
    PROBE_CHECK(211, probe_sys4(SYS_rt_sigaction, 17, &action, 0, 8) == 0);
    long child = lifecycle_fork(autoreap_child, 0, 212);
    int status = 0x5a5a;
    long result = probe_sys4(SYS_wait4, child, &status, 1, 0);
    put_word(24, (unsigned)result);
    PROBE_CHECK(213, result == 0);
    PROBE_CHECK(214, status == 0x5a5a);
    put_word(8, 1);
    /* go2由父确认原Task真正离栈后发表，clear_tid不能替代该事实。 */
    wait_word(12, 2);
    result = probe_sys4(SYS_wait4, child, &status, 1, 0);
    long signal = probe_sys4(SYS_rt_sigtimedwait, &mask, info, zero_time, 8);
    put_word(28, (unsigned)result);
    put_word(32, (unsigned)status);
    put_word(40, (unsigned)signal);
    put_word(8, 3);
    wait_word(12, 3);
    if (lifecycle.mode == 'd') {
        PROBE_CHECK(217, result == child);
        PROBE_CHECK(218, status == 0x1700);
        PROBE_CHECK(219, probe_sys4(SYS_wait4, child, 0, 1, 0) == -ECHILD);
        lifecycle_success();
    }
    PROBE_CHECK(220, result == -ECHILD);
    if (lifecycle.mode == 'e') {
        PROBE_CHECK(226, signal == -EAGAIN);
        lifecycle_success();
    }
    PROBE_CHECK(221, signal == 17);
    PROBE_CHECK(222, *(int *)info == 17);
    PROBE_CHECK(223, *(int *)(info + 8) == 1);
    PROBE_CHECK(224, *(int *)(info + 16) == child);
    PROBE_CHECK(225, *(int *)(info + 24) == 23);
    PROBE_CHECK(227, probe_sys4(SYS_rt_sigtimedwait, &mask, 0, zero_time, 8) == -EAGAIN);
    lifecycle_success();
}
