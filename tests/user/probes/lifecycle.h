#ifndef PROBE_LIFECYCLE_H
#define PROBE_LIFECYCLE_H

#include "probe.h"

#ifdef __riscv
#define LC(rv, la) (rv)
#else
#define LC(rv, la) (la)
#endif
#define CHECK_PAIR(rv, la, expression) PROBE_CHECK(LC(rv, la), (expression))

struct Lifecycle {
    unsigned char mode;
    long pid;
    long tid;
    long pgid;
    long sid;
    long process_a;
};
struct LifecycleAction { unsigned long handler, flags, mask; };
_Static_assert(sizeof(struct LifecycleAction) == 24, "raw action size");
extern struct Lifecycle lifecycle;
extern unsigned char lifecycle_child_stack[16384];
extern unsigned char lifecycle_thread_stack[16384];
extern const char lifecycle_self[];

/// 固定页只承载普通用户协议，父夹具仍观察真实内核对象。
static inline void *control_at(unsigned offset)
{
    return (void *)(0x44000000UL + offset);
}

/// 使用原32位Release发表共享状态。
static inline void put_word(unsigned offset, unsigned value)
{
    probe_store_release(control_at(offset), value);
}

/// 使用原32位Acquire观察共享状态。
static inline unsigned get_word(unsigned offset)
{
    return probe_load_acquire(control_at(offset));
}

/// 64位PID槽均自然对齐，单次原子访问保留原共享身份快照。
static inline unsigned long get_slot(unsigned offset)
{
    return __atomic_load_n((unsigned long *)control_at(offset), __ATOMIC_ACQUIRE);
}

/// PID和计算结果在原字节偏移发表，不借用32位phase槽。
static inline void put_slot(unsigned offset, unsigned long value)
{
    __atomic_store_n((unsigned long *)control_at(offset), value, __ATOMIC_RELEASE);
}

/// 等待父或同伴的真实检查点；RV保留原yield，LA保留忙等。
static inline void wait_word(unsigned offset, unsigned value)
{
    while (get_word(offset) < value) {
#ifdef __riscv
        probe_sys0(SYS_sched_yield);
#endif
    }
}

/// 等到clear_tid真正清零，不把非零值当作退出事件。
static inline void wait_zero(unsigned offset)
{
    while (get_word(offset) != 0) {
#ifdef __riscv
        probe_sys0(SYS_sched_yield);
#endif
    }
}

/// 成功只退出当前执行者，失败由probe_failure终止整组。
static inline _Noreturn void lifecycle_success(void)
{
    probe_exit(0);
}

/// 只通过唯一共享桥创建子执行者；新栈与私有状态活到真实退出。
static inline long lifecycle_spawn(unsigned long flags, ProbeChildEntry entry,
                                   int slot, int clear, void *stack, long check)
{
    long child = probe_clone(flags, (flags & 0x100) ? stack : 0,
                             slot < 0 ? 0 : control_at((unsigned)slot), 0,
                             clear < 0 ? 0 : control_at((unsigned)clear), entry, 0);
    PROBE_CHECK(check, 0 <= child);
    return child;
}

/// fork保持父栈及私有Mm，不把普通syscall包装成新栈返回。
static inline long lifecycle_fork(ProbeChildEntry entry, unsigned slot, long check)
{
    return lifecycle_spawn(0x100011, entry, (int)slot, -1, 0, check);
}

/// 原wait4逐个消费指定PID，status必须为正常零退出。
static inline void lifecycle_reap(long pid, long check, long status_check)
{
    int status = -1;
    PROBE_CHECK(check, probe_sys4(SYS_wait4, pid, &status, 0, 0) == pid);
    PROBE_CHECK(status_check, status == 0);
}

void lifecycle_handler(void);
void lifecycle_map(unsigned flags, long check);
_Noreturn void lifecycle_exec(char mode, long check);
_Noreturn void lifecycle_vfork(void);
_Noreturn void lifecycle_parent(void);
_Noreturn void lifecycle_wait(void);
_Noreturn void lifecycle_autoreap(void);
_Noreturn void lifecycle_illegal(void);

#endif
