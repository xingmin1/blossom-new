#include "signal.h"

static const struct SignalTime short_sleep = { 0, 100000000 };
static const struct SignalTime long_sleep = { 0, 500000000 };
static const struct SignalTime zero_time;
static const struct SignalTimer real_timer = { 0, 0, 0, 100000 };
static const struct SignalTimer periodic_timer = { 0, 20000, 0, 20000 };
static const struct SignalTimer cpu_timer = { 0, 0, 0, 10000 };
static const struct SignalTimer timer_off;

/* /// REAL、VIRTUAL和PROF分别更新自己的真实handler观察槽。 */
static void alarm_handler(void) { ++signal_state[5]; }
/* /// VIRTUAL只按用户执行账到期，不能由墙钟模拟。 */
static void virtual_handler(void) { signal_state[6] = 1; }
/* /// PROF计入实际用户与内核CPU账。 */
static void prof_handler(void) { signal_state[7] = 1; }

/* /// 读取指定真实时钟并保留本检查点的errno判断。 */
static struct SignalTime clock_read(long clock, long check)
{
    struct SignalTime result;
    PROBE_CHECK(check, probe_sys2(SYS_clock_gettime, clock, &result) == 0);
    return result;
}

/* /// 两个LP64 timespec转成纳秒差，跨秒仍保持原期限。 */
static unsigned long difference(struct SignalTime before, struct SignalTime after)
{
    return (after.seconds - before.seconds) * 1000000000UL +
           after.nanoseconds - before.nanoseconds;
}

/* /// 绝对请求基于已有monotonic读数，不能在中断后重建期限。 */
static struct SignalTime half_second_after(struct SignalTime time)
{
    time.nanoseconds += 500000000;
    if (1000000000 <= time.nanoseconds) {
        time.nanoseconds -= 1000000000;
        ++time.seconds;
    }
    return time;
}

/* /// 安装指定interval timer，旧值仍由真实syscall写出。 */
static void set_timer(long kind, const struct SignalTimer *timer,
                      struct SignalTimer *old, long check)
{
    PROBE_CHECK(check, probe_sys3(SYS_setitimer, kind, timer, old) == 0);
}

/* /// child真实睡眠500ms后退出，父waitid必须经内核等待。 */
static void sleeping_child(void *argument, unsigned long sp, unsigned long tp)
{
    (void)argument; (void)sp; (void)tp;
    PROBE_CHECK(170, probe_sys2(SYS_nanosleep, &long_sleep, 0) == 0);
    probe_exit(0);
}

/* /// 相对/绝对睡眠、waitid restart与周期合并保持互不替代的原检查组。 */
static void sleep_protocol(void)
{
    struct SignalTime remaining;
    signal_action(14, alarm_handler, 4, 0, 75);
    set_timer(0, &real_timer, 0, 76);
    PROBE_CHECK(77, probe_sys2(SYS_nanosleep, &long_sleep, &remaining) == -EINTR);
    PROBE_CHECK(78, signal_state[5] == 1);
    PROBE_CHECK(79, remaining.seconds == 0);
    PROBE_CHECK(80, 0 < remaining.nanoseconds && remaining.nanoseconds < 500000000);
    struct SignalTime absolute = half_second_after(clock_read(1, 81));
    set_timer(0, &real_timer, 0, 82);
    remaining = (struct SignalTime){ 0x777, 0x777 };
    PROBE_CHECK(83, probe_sys4(SYS_clock_nanosleep, 1, 1, &absolute, &remaining) == -EINTR);
    PROBE_CHECK(84, remaining.seconds == 0x777);
    PROBE_CHECK(85, remaining.nanoseconds == 0x777);
    signal_action(14, alarm_handler, 0x10000004, 0, 163);
    for (unsigned interrupted = 0; interrupted < 2; ++interrupted) {
        long child = signal_spawn(17, sleeping_child, 0);
        PROBE_CHECK(164, 0 <= child);
        set_timer(0, &real_timer, 0, 165);
        if (interrupted) {
            struct SignalInfo info;
            PROBE_CHECK(168, probe_sys5(SYS_waitid, 1, child, &info, 4, 0) == -EINTR);
        }
        signal_wait_child(child);
        if (!interrupted) {
            PROBE_CHECK(166, signal_state[5] == 3);
            signal_action(14, alarm_handler, 4, 0, 167);
        } else {
            PROBE_CHECK(169, signal_state[5] == 4);
        }
    }
    signal_mask(0, 0x2000, 210);
    set_timer(0, &periodic_timer, 0, 211);
    PROBE_CHECK(212, probe_sys2(SYS_nanosleep, &short_sleep, 0) == 0);
    struct SignalTimer old;
    set_timer(0, &timer_off, &old, 213);
    PROBE_CHECK(214, old.interval_seconds == 0);
    PROBE_CHECK(215, old.interval_us == 20000);
    unsigned long mask = 0x2000;
    struct SignalInfo info;
    PROBE_CHECK(216, probe_sys4(SYS_rt_sigtimedwait, &mask, &info, &zero_time, 8) == 14);
    PROBE_CHECK(217, info.code == 128);
    PROBE_CHECK(218, probe_sys4(SYS_rt_sigtimedwait, &mask, &info, &zero_time, 8) == -EAGAIN);
    signal_mask(1, 0x2000, 219);
    PROBE_CHECK(220, signal_state[5] == 4);
}

/* /// 独立进程先STOP父再CONT，子不能与被停父属于同一线程组。 */
static void deadline_child(void *argument, unsigned long sp, unsigned long tp)
{
    (void)sp; (void)tp;
    long parent = (long)argument;
    PROBE_CHECK(91, probe_sys2(SYS_nanosleep, &short_sleep, 0) == 0);
    signal_send(parent, 19, 92);
    PROBE_CHECK(93, probe_sys2(SYS_nanosleep, &short_sleep, 0) == 0);
    signal_send(parent, 18, 94);
    probe_exit(0);
}

/* /// STOP/CONT后的两种sleep总时间仍在原500ms窗口内。 */
static void deadline_protocol(void)
{
    for (unsigned absolute = 0; absolute < 2; ++absolute) {
        struct SignalTime start = clock_read(1, 86);
        long child = signal_spawn(0x111, deadline_child, (void *)signal_pid);
        PROBE_CHECK(87, 0 <= child);
        if (absolute) {
            struct SignalTime target = half_second_after(start);
            PROBE_CHECK(201, probe_sys4(SYS_clock_nanosleep, 1, 1, &target, 0) == 0);
        } else {
            PROBE_CHECK(88, probe_sys2(SYS_nanosleep, &long_sleep, 0) == 0);
        }
        signal_wait_child(child);
        unsigned long elapsed = difference(start, clock_read(1, 89));
        PROBE_CHECK(90, 450000000 <= elapsed && elapsed < 650000000);
    }
}

/* /// 同组成员纯用户计算，放行后只退出当前线程。 */
static void group_member(void *argument, unsigned long sp, unsigned long tp)
{
    (void)argument; (void)sp; (void)tp;
    while (probe_load_acquire(signal_state + 8) == 0) { }
    probe_exit(0);
}

/* /// VM子独立组增加真实THREAD成员，父仍属于另一个组。 */
static void group_child(void *argument, unsigned long sp, unsigned long tp)
{
    (void)argument; (void)sp; (void)tp;
    long member = probe_clone(0x10900, signal_thread_stack + 16384,
                               0, 0, 0, group_member, 0);
    PROBE_CHECK(105, 0 <= member);
    probe_store_release(signal_state + 12, 1);
    group_member(0, 0, 0);
}

/* /// waitid独立核全组stop完成及continued事件，不能以phase代替内核状态。 */
static void group_protocol(void)
{
    long child = signal_spawn(0x111, group_child, 0);
    PROBE_CHECK(95, 0 <= child);
    while (probe_load_acquire(signal_state + 12) == 0) { }
    signal_send(child, 19, 96);
    struct SignalInfo info;
    PROBE_CHECK(97, probe_sys5(SYS_waitid, 1, child, &info, 2, 0) == 0);
    PROBE_CHECK(98, info.code == 5);
    PROBE_CHECK(99, info.payload.sender.pid == child);
    int status;
    memcpy(&status, (unsigned char *)&info + 24, 4);
    PROBE_CHECK(100, status == 19);
    signal_send(child, 18, 101);
    PROBE_CHECK(102, probe_sys5(SYS_waitid, 1, child, &info, 8, 0) == 0);
    PROBE_CHECK(103, info.code == 6);
    memcpy(&status, (unsigned char *)&info + 24, 4);
    PROBE_CHECK(104, status == 18);
    probe_store_release(signal_state + 8, 1);
    signal_wait_child(child);
}

/* /// times与CPU clocks按真实USER_HZ截断比较，sleep不计入offCPU执行账。 */
static void cpu_protocol(void)
{
    signal_action(26, virtual_handler, 4, 0, 124);
    signal_action(27, prof_handler, 4, 0, 125);
    struct SignalTime cpu_start = clock_read(2, 126);
    unsigned long tms_start[4], tms_end[4];
    PROBE_CHECK(127, 0 <= probe_sys1(SYS_times, tms_start));
    set_timer(1, &cpu_timer, 0, 128);
    set_timer(2, &cpu_timer, 0, 129);
    while ((signal_state[6] & signal_state[7]) == 0) { }
    unsigned attempts = 0;
    for (; attempts < 1000000; ++attempts) {
        struct SignalTime time;
        PROBE_CHECK(172, probe_sys2(SYS_gettimeofday, &time, 0) == 0);
        probe_sys1(SYS_times, tms_end);
        if (tms_start[1] < tms_end[1])
            break;
    }
    PROBE_CHECK(173, attempts < 1000000);
    struct SignalTime cpu_end = clock_read(2, 130);
    PROBE_CHECK(131, 0 <= probe_sys1(SYS_times, tms_end));
    PROBE_CHECK(174, tms_start[0] < tms_end[0]);
    unsigned long elapsed = difference(cpu_start, cpu_end);
    PROBE_CHECK(132, 10000000 <= elapsed);
    unsigned long ticks = tms_end[0] + tms_end[1] - tms_start[0] - tms_start[1];
    PROBE_CHECK(133, elapsed / 10000000 - ticks + 2 < 5);
    PROBE_CHECK(134, probe_sys2(SYS_nanosleep, &short_sleep, 0) == 0);
    cpu_start = clock_read(2, 135);
    PROBE_CHECK(136, difference(cpu_end, cpu_start) < 50000000);
    cpu_start = clock_read(2, 202);
    struct SignalTime thread_start = clock_read(3, 203);
    long child = probe_clone(0x10900, signal_thread_stack + 16384,
                              0, 0, 0, group_member, 0);
    PROBE_CHECK(204, 0 <= child);
    PROBE_CHECK(205, probe_sys2(SYS_nanosleep, &short_sleep, 0) == 0);
    cpu_end = clock_read(2, 206);
    struct SignalTime thread_end = clock_read(3, 207);
    PROBE_CHECK(208, 50000000 <= difference(cpu_start, cpu_end));
    PROBE_CHECK(209, difference(thread_start, thread_end) < 50000000);
    probe_store_release(signal_state + 8, 1);
}

/* /// 时间模式各自拥有完整流程，成功只终止当前线程而不偷换exit_group。 */
_Noreturn void signal_time_mode(void)
{
    switch (signal_mode) {
    case 's': sleep_protocol(); break;
    case 'd': deadline_protocol(); break;
    case 'g': group_protocol(); break;
    case 'c': cpu_protocol(); break;
    default: probe_failure(1);
    }
    probe_exit(0);
}
