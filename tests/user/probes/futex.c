#include "probe.h"

#if defined(__riscv)
#define ISA_CHECK(rv, la) (rv)
#define RV 1
#else
#define ISA_CHECK(rv, la) (la)
#define RV 0
#endif
#define CHECK(rv, la, expression) PROBE_CHECK(ISA_CHECK(rv, la), expression)
#define CONTROL 0x44000000UL
#define WORD0 (CONTROL + 64)
#define WORD1 (CONTROL + 68)

struct RawAction {
    unsigned long handler;
    unsigned long flags;
    unsigned long mask;
};
struct RawTime {
    long seconds;
    long nanoseconds;
};
_Static_assert(sizeof(struct RawAction) == 24, "raw action size");
_Static_assert(sizeof(struct RawTime) == 16, "raw timespec size");

static unsigned char stacks[2][16384] __attribute__((aligned(16)));
static unsigned char info[128] __attribute__((aligned(8)));
static struct RawAction action;
static char mode;

/* /// 保持原 ISA 的信号返回规则，不经 C prologue 改变信号帧 SP。 */
extern void futex_handler(void);
/* /// 在真实 WAIT 返回点检查 a1–a5/a7，失败直接携原检查号终止。 */
extern long futex_restart_wait(void);

/* /// 失败和成功均只退出当前执行者，让父 waitid 保留完整状态。 */
_Noreturn void probe_failure(long check)
{
    probe_exit(check);
}

/* /// 协调页只用自然对齐的 32 位 Acquire 观察，忙等不切换用户活动根。 */
static unsigned load_word(unsigned long address)
{
    return probe_load_acquire((const volatile unsigned *)address);
}

/* /// 共享数据先于进度和结果的 Release 存储发布。 */
static void store_word(unsigned long address, unsigned value)
{
    probe_store_release((volatile unsigned *)address, value);
}

/* /// TID 槽保持原 64 位偏移，使用自然对齐原子访问，不拼出混合代次。 */
static unsigned long child_tid(unsigned slot)
{
    return __atomic_load_n((unsigned long *)(CONTROL + 16 * slot), __ATOMIC_ACQUIRE);
}

/* /// 发布真实 clone 返回的 TID，供内核夹具查找同一用户线程。 */
static void publish_tid(unsigned slot, unsigned long tid)
{
    __atomic_store_n((unsigned long *)(CONTROL + 16 * slot), tid, __ATOMIC_RELEASE);
}

/* /// 等待夹具确认真实登记后放行，进度字本身不代表已经 Blocked。 */
static void go(unsigned phase)
{
    while (load_word(CONTROL + 12) < phase)
        ;
}

/* /// 发布原协议检查点，然后等待对应的内核白盒观察完成。 */
static void checkpoint(unsigned phase)
{
    store_word(CONTROL + 8, phase);
    go(phase);
}

/* /// 普通业务通过共享 raw 层执行六参数 futex，第四槽也可承载重排数量。 */
static long futex(unsigned long address, long operation, long value,
                  unsigned long timeout, unsigned long destination, unsigned mask)
{
    return probe_sys6(SYS_futex, address, operation, value, timeout, destination, mask);
}

/* /// 保留固定地址和原共享或私有匿名映射的实际返回断言。 */
static void map_page(unsigned long address, unsigned flags, long check)
{
    PROBE_CHECK(check, probe_sys6(SYS_mmap, address, 4096, 3, flags, -1, 0) == (long)address);
}

/* /// 构造内核要求的 24 字节 action，信号帧和 RA 仍由原内核 ABI 提供。 */
static void install_action(unsigned long flags, long check)
{
    action.handler = (unsigned long)futex_handler;
    action.flags = flags;
    action.mask = 0;
    PROBE_CHECK(check, probe_sys4(SYS_rt_sigaction, 10, &action, 0, 8) == 0);
}

/* /// 从真实 monotonic 读数建立绝对期限，未以时钟代替等待登记。 */
static void deadline(long seconds, long check)
{
    struct RawTime *time = (struct RawTime *)(CONTROL + 40);
    PROBE_CHECK(check, probe_sys2(SYS_clock_gettime, 1, time) == 0);
    time->seconds += seconds;
    __atomic_thread_fence(__ATOMIC_RELEASE);
}

/* /// 检查共享结果的原有有符号 32 位值，包括负 errno。 */
static void result(unsigned offset, long expected, long check)
{
    PROBE_CHECK(check, (int)load_word(CONTROL + offset) == expected);
}

/* /// RV 原每组同时核两个源字，保留连续的检查编号。 */
static void words(long check)
{
    result(64, 7, check);
    result(68, 7, check + 1);
}

/* /// waitid 消费本程序自己的子进程，并传播子检查号；LA 另核 signo/errno。 */
static void reap(unsigned slot, long expected, long check)
{
    unsigned long tid = child_tid(slot);
    PROBE_CHECK(check, probe_sys5(SYS_waitid, 1, tid, info, 4, 0) == 0);
    int value;
    if (!RV) {
        memcpy(&value, info, sizeof(value));
        PROBE_CHECK(check + 1, value == 17);
        memcpy(&value, info + 4, sizeof(value));
        PROBE_CHECK(check + 2, value == 0);
    }
    memcpy(&value, info + 8, sizeof(value));
    PROBE_CHECK(check + (RV ? 1 : 3), value == 1);
    memcpy(&value, info + 16, sizeof(value));
    PROBE_CHECK(check + (RV ? 2 : 4), value == (long)tid);
    memcpy(&value, info + 24, sizeof(value));
    if (value != 0)
        probe_failure(value);
    if (!RV)
        result(24 + 4 * slot, expected, check + 5);
}

/* /// 每个子回调在共享 clone 桥捕获的新栈上运行，父方只在真实 reap 后复用栈。 */
static _Noreturn void child_entry(void *argument, unsigned long entry_sp,
                                  unsigned long entry_tp)
{
    unsigned role = (unsigned long)argument;
    unsigned slot = role == 1 ? 1 : 0;
    long value;
    (void)entry_sp;
    (void)entry_tp;
    switch (mode) {
    case 'w':
        value = futex(0x46000000, 128, 7, 0, 0, 0);
        store_word(CONTROL + 24, value);
        CHECK(22, 201, value == 0);
        CHECK(45, 236, load_word(0x46000000) == 7);
        if (RV)
            words(23);
        else
            result(64, 7, 202);
        break;
    case 'p':
    case 'n': {
        unsigned long address = mode == 'p' ? WORD0 : 0x46000000;
        unsigned operation = mode == 'p' ? 128 : 0;
        if (!RV)
            install_action(0, 203);
        value = futex(address, operation, 7, 0, 0, 0);
        store_word(CONTROL + 24, value);
        CHECK(42, 204, value == -4);
        CHECK(44, 205, load_word(address) == 7);
        result(32, 1, ISA_CHECK(43, 206));
        break;
    }
    case 'a':
        go(1);
        value = futex(0x45000040, 0, 7, 0, 0, 0);
        store_word(CONTROL + 24, value);
        CHECK(58, 207, value == 0);
        CHECK(59, 208, load_word(0x45000040) == 7);
        if (RV)
            words(60);
        else
            result(64, 7, 209);
        break;
    case 'r': {
        long expected = role == 2 ? -4 : role == 3 ? -110 : 0;
        long check = 180 + 4 * role;
        if (!RV && role == 2)
            install_action(0, 210);
        value = futex(WORD0, 137, 7, CONTROL + 40, 0, 1U << role);
        store_word(CONTROL + 24 + 4 * slot, value);
        if (RV) {
            PROBE_CHECK(check, value == expected);
            result(32, role < 2 ? 0 : 1, check + 1);
            words(check + 2);
        } else {
            PROBE_CHECK(role == 2 ? 211 : role == 3 ? 212 : 213, value == expected);
            words(214);
        }
        break;
    }
    case 't':
        value = probe_sys0(SYS_gettid);
        if (role == 0) {
            if (!RV)
                PROBE_CHECK(216, 0 < value);
            CHECK(135, 217, (long)(int)load_word(WORD0) == value);
            store_word(WORD0, value);
            checkpoint(1);
        } else {
            if (!RV)
                PROBE_CHECK(219, 0 < value);
            CHECK(136, 220, probe_sys1(SYS_set_tid_address, WORD1) == value);
            CHECK(137, 221, probe_sys1(SYS_set_tid_address, 0x70000000) == value);
            if (!RV)
                result(68, 7, 222);
            store_word(CONTROL + 28, 0);
            if (!RV)
                result(28, 0, 223);
        }
        break;
    case 's':
        if (!RV)
            install_action(0x10000000, 224);
        /* 返回点的原参数由薄桥检查，C 不能推测 caller-saved 寄存器。 */
        futex_restart_wait();
        result(32, 1, ISA_CHECK(152, 226));
        if (RV)
            words(153);
        else
            result(64, 7, 227);
        break;
    case 'd':
        value = futex(WORD0, 128, 7, 0x47000000, 0, 0);
        store_word(CONTROL + 24, value);
        CHECK(170, 228, value == -110);
        if (RV) {
            result(32, 0, 171);
            words(172);
        } else {
            result(64, 7, 229);
        }
        break;
    default:
        probe_failure(ISA_CHECK(3, 200));
    }
    probe_exit(0);
}

/* /// 所有 fork/共享 Mm 子进程走唯一 clone 桥，保持旧 flags、TID 地址和独立栈。 */
static void spawn(unsigned long flags, unsigned slot, unsigned role,
                  unsigned long tid_address, long check)
{
    if (RV)
        store_word(CONTROL + 24 + 4 * slot, -4096);
    void *stack = flags & 0x100 ? stacks[slot] + sizeof(stacks[slot]) : 0;
    long tid = probe_clone(flags, stack, 0, 0, (int *)tid_address,
                           child_entry, (void *)(unsigned long)role);
    PROBE_CHECK(check, 0 <= tid);
    publish_tid(slot, tid);
}

/* /// 比较失败、原始错误优先级及私有 WAKE 都实际经过 futex 系统调用。 */
static void wait_wake(void)
{
    CHECK(10, 20, futex(WORD0, 128, 8, 0, 0, 0) == -11);
    CHECK(11, 21, futex(WORD0 + 1, 128, 7, 0, 0, 0) == -22);
    CHECK(12, 22, futex(0x70000000, 128, 7, 0, 0, 0) == -14);
    CHECK(13, 23, futex(WORD0, 137, 7, 0, 0, 0) == -22);
    CHECK(26, 34, futex(WORD0, 134, 0, 0, 0, 0) == -38);
    CHECK(27, 35, futex(WORD0, 133, 1, 0, 0, 0) == -38);
    CHECK(28, 36, probe_sys2(SYS_set_robust_list, CONTROL, 24) == -38);
    CHECK(29, 37, futex(WORD0, 129, -1, 0, 0, 0) == 0);
    map_page(0x46000000, 0x32, ISA_CHECK(30, 38));
    store_word(0x46000000, 7);
    spawn(0x111, 0, 0, 0, ISA_CHECK(14, 24));
    checkpoint(1);
    CHECK(31, 39, futex(0x46000000, 1, 1, 0, 0, 0) == 0);
    CHECK(15, 25, futex(0x46000000, 129, 1, 0, 0, 0) == 1);
    reap(0, 0, ISA_CHECK(16, 26));
    if (RV) {
        result(24, 0, 19);
        words(20);
    } else {
        result(64, 7, 32);
    }
    result(32, 0, ISA_CHECK(25, 33));
}

/* /// fork 后父 Mm 的同址 WAKE 不得命中子 Mm 的私有命名空间。 */
static void isolation(void)
{
    unsigned long address = WORD0;
    unsigned operation = 128;
    if (mode == 'n') {
        map_page(0x46000000, 0x32, ISA_CHECK(30, 40));
        address = 0x46000000;
        operation = 0;
        store_word(address, 7);
    }
    if (RV)
        install_action(0, 31);
    spawn(17, 0, 0, 0, ISA_CHECK(32, 41));
    checkpoint(1);
    CHECK(33, 42, futex(address, operation | 1, 1, 0, 0, 0) == 0);
    checkpoint(2);
    reap(0, -4, ISA_CHECK(34, 43));
    if (RV) {
        result(24, -4, 37);
        result(32, 1, 38);
        PROBE_CHECK(39, load_word(address) == 7);
        words(40);
    } else {
        PROBE_CHECK(49, load_word(address) == 7);
        result(32, 1, 50);
    }
}

/* /// 子 Mm 额外别名由夹具安装，普通 WAKE 仍使用原共享地址。 */
static void shared_alias(void)
{
    spawn(17, 0, 0, 0, ISA_CHECK(50, 55));
    checkpoint(1);
    checkpoint(2);
    CHECK(51, 56, futex(WORD0, 1, 1, 0, 0, 0) == 1);
    reap(0, 0, ISA_CHECK(52, 57));
    if (RV) {
        result(24, 0, 55);
        words(56);
    } else {
        result(64, 7, 63);
    }
    result(32, 0, ISA_CHECK(62, 64));
}

/* /// 往返重排、signal 撤销和期限撤销均保留原七个夹具检查点。 */
static void requeue(void)
{
    if (RV)
        install_action(0, 70);
    deadline(30, ISA_CHECK(71, 70));
    spawn(0x111, 0, 0, 0, ISA_CHECK(72, 71));
    spawn(0x111, 1, 1, 0, ISA_CHECK(73, 72));
    checkpoint(1);
    CHECK(74, 73, futex(WORD0, 138, 1, 0, 0, 1) == 1);
    CHECK(75, 74, futex(WORD0, 132, 0, 1, WORD1, 8) == -11);
    CHECK(76, 75, futex(WORD0, 132, 0, 1, WORD1, 7) == 1);
    checkpoint(2);
    CHECK(77, 76, futex(WORD1, 131, 0, 1, WORD0, 0) == 1);
    checkpoint(3);
    CHECK(78, 77, futex(WORD0, 138, 1, 0, 0, 1) == 0);
    CHECK(79, 78, futex(WORD0, 138, 1, 0, 0, 2) == 1);
    reap(0, 0, ISA_CHECK(80, 79));
    reap(1, 0, ISA_CHECK(83, 85));
    if (RV) {
        result(24, 0, 86);
        result(28, 0, 87);
        words(88);
    }

    publish_tid(0, 0);
    store_word(CONTROL + 24, RV ? 0 : (unsigned)-4096);
    deadline(30, ISA_CHECK(90, 91));
    spawn(0x111, 0, 2, 0, ISA_CHECK(91, 92));
    checkpoint(4);
    CHECK(92, 93, futex(WORD0, 132, 0, 1, WORD1, 7) == 1);
    checkpoint(5);
    reap(0, -4, ISA_CHECK(93, 94));
    if (RV)
        result(24, -4, 96);
    result(32, 1, ISA_CHECK(97, 101));
    CHECK(98, 100, futex(WORD1, 129, 1, 0, 0, 0) == 0);

    publish_tid(0, 0);
    store_word(CONTROL + 24, RV ? 0 : (unsigned)-4096);
    deadline(1, ISA_CHECK(99, 102));
    spawn(0x111, 0, 3, 0, ISA_CHECK(100, 103));
    checkpoint(6);
    CHECK(101, 104, futex(WORD0, 132, 0, 1, WORD1, 7) == 1);
    checkpoint(7);
    reap(0, -110, ISA_CHECK(102, 105));
    if (RV)
        result(24, -110, 105);
    result(32, 1, ISA_CHECK(106, 114));
    CHECK(107, 111, futex(WORD1, 129, 1, 0, 0, 0) == 0);
    words(ISA_CHECK(108, 112));
}

/* /// 真实 CHILD_SETTID、clear_child_tid 清零/唤醒和坏退出地址都由子执行。 */
static void clear_tid(void)
{
    spawn(0x1200111, 0, 0, WORD0, 120);
    unsigned phase;
    do {
        phase = load_word(CONTROL + 8);
    } while (phase == 0);
    if (RV)
        PROBE_CHECK(121, phase == 1);
    unsigned tid = load_word(WORD0);
    CHECK(122, 121, (long)(int)tid == (long)child_tid(0));
    long value = futex(WORD0, 0, tid, 0, 0, 0);
    store_word(CONTROL + 24, value);
    CHECK(123, 122, value == 0);
    result(64, 0, ISA_CHECK(124, 123));
    reap(0, 0, ISA_CHECK(125, 124));
    if (RV)
        result(24, 0, 128);
    spawn(0x111, 1, 1, 0, ISA_CHECK(129, 130));
    reap(1, 0, ISA_CHECK(130, 131));
    if (RV)
        result(28, 0, 133);
    result(64, 0, ISA_CHECK(134, 137));
    result(32, 0, ISA_CHECK(138, 139));
    result(68, 7, ISA_CHECK(139, 138));
}

/* /// SA_RESTART 后夹具必须观察到新 WaitId，再由父真实 WAKE 结束等待。 */
static void restart(void)
{
    if (RV)
        install_action(0x10000000, 140);
    spawn(0x111, 0, 0, 0, ISA_CHECK(141, 145));
    checkpoint(1);
    result(32, 1, ISA_CHECK(142, 146));
    CHECK(143, 147, futex(WORD0, 129, 1, 0, 0, 0) == 1);
    reap(0, 0, ISA_CHECK(144, 148));
    if (RV) {
        result(24, 0, 147);
        result(32, 1, 148);
        words(149);
    } else {
        result(64, 7, 154);
        result(32, 1, 155);
    }
}

/* /// STOP 确认后撤销 timeout 页，CONT 必须消费保存的原期限而不重读该页。 */
static void stopped_deadline(void)
{
    map_page(0x47000000, 0x32, 160);
    struct RawTime *time = (struct RawTime *)0x47000000;
    time->seconds = 0;
    time->nanoseconds = 200000000;
    __atomic_thread_fence(__ATOMIC_RELEASE);
    spawn(0x111, 0, 0, 0, 161);
    checkpoint(1);
    PROBE_CHECK(162, probe_sys2(SYS_munmap, 0x47000000, 4096) == 0);
    checkpoint(2);
    reap(0, -110, 163);
    if (RV) {
        result(24, -110, 166);
        result(32, 0, 167);
        words(168);
    } else {
        result(64, 7, 169);
        result(32, 0, 170);
    }
}

/* /// 原始初始栈只读解析；无 CRT、fd/TLS 或 BSS 初始化改变被测前提。 */
_Noreturn void probe_main(const unsigned long *initial_stack)
{
    PROBE_CHECK(1, ((unsigned long)initial_stack & 15) == 0);
    CHECK(1, 2, initial_stack[0] == 2);
    const unsigned char *argument = (const unsigned char *)initial_stack[2];
    CHECK(1, 3, RV ? argument != 0 : 0 < (long)argument);
    CHECK(1, 4, argument[1] == 0);
    mode = argument[0];
    map_page(CONTROL, 0x31, ISA_CHECK(2, 5));
    for (unsigned offset = 0; offset < 4096; offset += 4)
        store_word(CONTROL + offset, 0);
    if (!RV) {
        store_word(CONTROL + 24, -4096);
        store_word(CONTROL + 28, -4096);
    }
    store_word(WORD0, 7);
    store_word(WORD1, 7);
    switch (mode) {
    case 'w': wait_wake(); break;
    case 'p':
    case 'n': isolation(); break;
    case 'a': shared_alias(); break;
    case 'r': requeue(); break;
    case 't': clear_tid(); break;
    case 's': restart(); break;
    case 'd': stopped_deadline(); break;
    default: probe_failure(ISA_CHECK(3, 6));
    }
    probe_exit(0);
}
