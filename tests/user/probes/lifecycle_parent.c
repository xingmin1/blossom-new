#include "lifecycle.h"

/// 同组peer延长父资格，必须等原父夹具放行才退出。
static void parent_peer(void *argument, unsigned long entry_sp, unsigned long entry_tp)
{
    (void)argument; (void)entry_sp; (void)entry_tp;
    put_word(96, 1);
    wait_word(12, 1);
#ifndef __riscv
    put_word(100, 1);
#endif
    lifecycle_success();
}

/// B在leader离栈后仍有同组父；最后成员退出才转亲内核根。
static void process_b(void *argument, unsigned long entry_sp, unsigned long entry_tp)
{
    (void)argument; (void)entry_sp; (void)entry_tp;
    CHECK_PAIR(117, 116, probe_sys0(SYS_getppid) == lifecycle.process_a);
    put_word(72, 1);
    wait_zero(104);
    long parent = probe_sys0(SYS_getppid);
    CHECK_PAIR(118, 117, parent == lifecycle.process_a);
    put_slot(48, (unsigned long)parent);
    put_word(80, 1);
    wait_word(8, 2);
    parent = probe_sys0(SYS_getppid);
    CHECK_PAIR(119, 118, parent == 0);
    put_slot(48, (unsigned long)parent);
    put_word(84, 1);
    wait_word(12, 3);
    lifecycle_success();
}

/// A先检查PGID/SID协议，再创建子进程与真实同组线程并退出leader。
static void process_a(void *argument, unsigned long entry_sp, unsigned long entry_tp)
{
    (void)argument; (void)entry_sp; (void)entry_tp;
    lifecycle.process_a = probe_sys0(SYS_getpid);
#ifndef __riscv
    PROBE_CHECK(103, probe_sys0(SYS_getppid) == lifecycle.pid);
#endif
    CHECK_PAIR(105, 104, probe_sys1(SYS_getpgid, 0) == lifecycle.pgid);
    CHECK_PAIR(106, 105, probe_sys1(SYS_getsid, 0) == lifecycle.sid);
    CHECK_PAIR(107, 106, probe_sys2(SYS_setpgid, 0, 0) == 0);
    CHECK_PAIR(108, 107, probe_sys1(SYS_getpgid, 0) == lifecycle.process_a);
    CHECK_PAIR(109, 108, probe_sys0(SYS_setsid) == -EPERM);
    CHECK_PAIR(110, 109, probe_sys2(SYS_setpgid, 0, lifecycle.pgid) == 0);
    CHECK_PAIR(111, 110, probe_sys0(SYS_setsid) == lifecycle.process_a);
    CHECK_PAIR(112, 111, probe_sys1(SYS_getpgid, 0) == lifecycle.process_a);
    CHECK_PAIR(113, 112, probe_sys1(SYS_getsid, 0) == lifecycle.process_a);
    put_word(88, 1);
    wait_word(92, 1);
#ifdef __riscv
    put_word(104, (unsigned)probe_sys0(SYS_gettid));
#else
    put_word(104, (unsigned)lifecycle.process_a);
#endif
    CHECK_PAIR(114, 113, probe_sys1(SYS_set_tid_address, control_at(104)) == lifecycle.process_a);
    lifecycle_fork(process_b, 16, LC(115, 114));
#ifndef __riscv
    wait_word(72, 1);
#endif
    lifecycle_spawn(0x110900, parent_peer, 32, -1, lifecycle_thread_stack + 16384, LC(116, 115));
#ifdef __riscv
    wait_word(72, 1);
#endif
    wait_word(96, 1);
    lifecycle_success();
}

/// C实际复用A的号码，但继承的是当前父的PGID/SID，不能继承旧号码对象。
static void process_c(void *argument, unsigned long entry_sp, unsigned long entry_tp)
{
    (void)argument; (void)entry_sp; (void)entry_tp;
    CHECK_PAIR(120, 119, probe_sys0(SYS_getpid) == lifecycle.process_a);
#ifndef __riscv
    PROBE_CHECK(120, probe_sys0(SYS_getppid) == lifecycle.pid);
#endif
    PROBE_CHECK(121, probe_sys1(SYS_getpgid, 0) == lifecycle.pgid);
    PROBE_CHECK(122, probe_sys1(SYS_getsid, 0) == lifecycle.sid);
    put_word(108, 1);
    wait_word(12, 5);
    lifecycle_success();
}

/// 父逐阶段观察leader接任、孤儿转亲与号码角色复用，保留真实wait关系。
_Noreturn void lifecycle_parent(void)
{
    lifecycle.pgid = probe_sys1(SYS_getpgid, 0);
    PROBE_CHECK(90, 0 < lifecycle.pgid);
    lifecycle.sid = probe_sys1(SYS_getsid, 0);
    PROBE_CHECK(91, 0 < lifecycle.sid);
    long child = lifecycle_fork(process_a, 0, 92);
    lifecycle.process_a = child;
    wait_word(88, 1);
    PROBE_CHECK(93, probe_sys2(SYS_setpgid, child, lifecycle.pgid) == -EPERM);
    put_word(92, 1);
    wait_word(80, 1);
#ifdef __riscv
    PROBE_CHECK(94, get_slot(48) == (unsigned long)child);
#endif
    int status;
    CHECK_PAIR(95, 94, probe_sys4(SYS_wait4, child, &status, 1, 0) == 0);
    CHECK_PAIR(96, 95, probe_sys4(SYS_wait4, get_slot(16), 0, 1, 0) == -ECHILD);
    put_word(8, 1);
    wait_word(12, 1);
    lifecycle_reap(child, LC(97, 96), LC(98, 97));
    put_word(8, 2);
    wait_word(84, 1);
#ifdef __riscv
    PROBE_CHECK(99, get_slot(48) == 0);
#endif
    CHECK_PAIR(100, 98, probe_sys4(SYS_wait4, get_slot(16), 0, LC(1, 0), 0) == -ECHILD);
    put_word(8, 3);
    wait_word(12, 4);
    long replacement = lifecycle_fork(process_c, 16, LC(101, 99));
    CHECK_PAIR(102, 100, replacement == child);
    wait_word(108, 1);
    put_word(8, 4);
    wait_word(12, 5);
    lifecycle_reap(child, LC(103, 101), LC(104, 102));
    lifecycle_success();
}
