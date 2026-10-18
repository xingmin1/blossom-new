#include "probe.h"

#if defined(__riscv)
#define EVENT_USER_END 0x4000000000UL
#elif defined(__loongarch64)
#define EVENT_USER_END 0x8000000000UL
#else
#error events requires RV64 or LA64
#endif

#define EXPECT(code, actual, expected) \
    PROBE_CHECK((code), (actual) == (expected))
#define DATA_READER 0xfedcba9876543210UL
#define DATA_WRITER 0x8877665544332211UL
#define DATA_ALIAS 0x1122334455667788UL

struct Event {
    unsigned events;
    unsigned padding;
    unsigned long data;
};

struct PollFd {
    int fd;
    unsigned short events;
    unsigned short revents;
};

struct Time {
    long seconds;
    long nanos;
};

struct MaskPack {
    const unsigned long *mask;
    unsigned long size;
};

struct RawAction {
    void (*handler)(int);
    unsigned long flags;
    unsigned long mask;
};

struct Control {
    unsigned phase;
    unsigned gate;
    unsigned handlers;
    unsigned result;
    unsigned child_done;
};

_Static_assert(sizeof(struct Event) == 16, "event wire size");
_Static_assert(offsetof(struct Event, data) == 8, "event data offset");
_Static_assert(sizeof(struct PollFd) == 8, "pollfd wire size");
_Static_assert(offsetof(struct PollFd, revents) == 6, "pollfd output offset");
_Static_assert(sizeof(struct Time) == 16, "timespec wire size");
_Static_assert(sizeof(struct MaskPack) == 16, "pselect mask package size");
_Static_assert(sizeof(struct RawAction) == 24, "raw action size");
_Static_assert(offsetof(struct Control, child_done) == 16, "control page offsets");

static const struct Time zero_time = {0, 0};
static const struct Time readonly_short = {0, 1000000};
static const struct Time readonly_long = {10, 0};
static const struct Time bad_time = {0, 1000000000};
static const struct Time negative_seconds = {-1, 0};
static const struct Time negative_nanos = {0, -1};
static const unsigned long temporary_mask = 0x800;
static const struct MaskPack null_pack = {0, 999};
static const struct MaskPack mask_pack = {&temporary_mask, 8};
static struct Time twenty_ms = {0, 20000000};
static struct Time long_time = {10, 0};
static struct PollFd pollfds[129];
static unsigned long sets[6];
static struct Event event;
static struct Event output[3];
static unsigned char received;
static int fds[2];
static long reader;
static long writer;
static long epoll;
static long root_pid;
static unsigned char mode;
static volatile struct Control *const control = (void *)0x44000000UL;

/// 每个失败号保持原 raw 客体的 exit93 结果，不改变整组退出语义。
_Noreturn void probe_failure(long check)
{
    probe_exit(check);
}

/// 显式关闭本客体拥有的槽，并保留各调用点的精确检查号。
static void close_fd(long fd, long check)
{
    EXPECT(check, probe_sys1(SYS_close, fd), 0);
}

/// 用真实单字节写产生管道通知，不向事件对象注入 ready 状态。
static void write_byte(long fd, long check)
{
    static const unsigned char byte = 81;
    EXPECT(check, probe_sys3(SYS_write, fd, &byte, 1), 1);
}

/// 消费真实管道字节，使后续查询经过源对象的状态变化。
static void read_byte(long fd, long check)
{
    EXPECT(check, probe_sys3(SYS_read, fd, &received, 1), 1);
}

/// 构造自然对齐的16字节事件；四字节padding始终显式归零。
static void set_event(unsigned mask, unsigned long data)
{
    event.events = mask;
    event.padding = 0;
    event.data = data;
}

/// 同一事件OFD上的控制调用保留真实目标fd及原errno断言。
static void ctl(long operation, long fd, long expected, long check)
{
    EXPECT(check, probe_sys4(SYS_epoll_ctl, epoll, operation, fd, &event), expected);
}

/// 零期限扫描仍传原无mask时被忽略的尺寸777。
static void wait_events(long count, long expected, long check)
{
    EXPECT(check, probe_sys6(SYS_epoll_pwait, epoll, output, count, 0, 0, 777), expected);
}

/// 检查完整事件的位、零padding与不被解引用的64位data。
static void check_output(unsigned mask, unsigned long data, long check)
{
    EXPECT(check, output[0].events, mask);
    EXPECT(check, output[0].padding, 0U);
    EXPECT(check, output[0].data, data);
}

/// ppoll使用本族固定wire数组，所有输入仍通过共享raw调用提交。
static void poll_events(long count, const struct Time *time, long expected, long check)
{
    EXPECT(check, probe_sys5(SYS_ppoll, pollfds, count, time, 0, 777), expected);
}

/// 三个集合分别位于相距16字节的原用户缓冲，独立结果参与计数。
static void select_events(long count, long expected, long check)
{
    EXPECT(check, probe_sys6(SYS_pselect6, count, sets, sets + 2, sets + 4,
                            &zero_time, 0), expected);
}

/// 替换真实线程mask，8字节raw集合不经过libc sigset转换。
static void set_mask(unsigned long mask, long check)
{
    EXPECT(check, probe_sys4(SYS_rt_sigprocmask, 2, &mask, 0, 8), 0);
}

/// NULL新mask时how被忽略；查询值必须反映真实恢复后的blocked集合。
static void query_mask(unsigned long expected, long check)
{
    unsigned long mask;
    EXPECT(check, probe_sys4(SYS_rt_sigprocmask, 999, 0, &mask, 8), 0);
    EXPECT(check, mask, expected);
}

/// 单调时间差保留纳秒下界，不以调度次数代替真正期限。
static long elapsed(const struct Time *before, const struct Time *after)
{
    return (after->seconds - before->seconds) * 1000000000L +
           after->nanos - before->nanos;
}

/// 129项、负fd、重复fd和u32 nfds截断均从真实用户数组核对。
static long poll_abi(void)
{
    long regular = probe_sys4(SYS_openat, -100, "/test/data.bin", 0, 0);
    EXPECT(10, regular, 3);
    pollfds[0] = (struct PollFd){-1, 0xffff, 0xffff};
    pollfds[1] = (struct PollFd){400, 0, 0xffff};
    pollfds[2] = (struct PollFd){writer, 4, 0xffff};
    pollfds[3] = (struct PollFd){writer, 4, 0xffff};
    poll_events(4, &zero_time, 3, 11);
    EXPECT(12, pollfds[0].revents, 0);
    EXPECT(13, pollfds[1].revents, 32);
    EXPECT(14, pollfds[2].revents, 4);
    EXPECT(15, pollfds[3].revents, 4);
    pollfds[0].fd = regular;
    pollfds[0].events = 0x145;
    poll_events(1, &zero_time, 1, 16);
    EXPECT(17, pollfds[0].revents, 0x145);
    EXPECT(18, probe_sys5(SYS_ppoll, EVENT_USER_END, 0x100000000UL,
                         &zero_time, 0, 999), 0);
    for (unsigned i = 0; i < 129; ++i)
        pollfds[i] = (struct PollFd){-1, 4, 0xffff};
    pollfds[128].fd = writer;
    poll_events(129, &zero_time, 1, 201);
    EXPECT(202, pollfds[0].revents, 0);
    EXPECT(203, pollfds[128].revents, 4);
    poll_events(-1, &zero_time, -EINVAL, 20);
    EXPECT(21, probe_sys5(SYS_ppoll, EVENT_USER_END, 1, &zero_time, 0, 8), -EFAULT);
    return regular;
}

/// 高fd与页尾缓冲验证fdset的机器字跨度，unmap后只访问真正选中的字节。
static void select_abi(long regular)
{
    sets[0] = 8;
    sets[2] = 10;
    sets[4] = 8;
    select_events(4, 3, 22);
    EXPECT(23, sets[0], 8UL);
    EXPECT(24, sets[2], 10UL);
    EXPECT(25, sets[4], 0UL);
    select_events(-1, -EINVAL, 26);
    sets[0] = 1UL << 7;
    sets[2] = 0;
    select_events(8, -EBADF, 27);
    EXPECT(28, probe_sys6(SYS_pselect6, 0, EVENT_USER_END, EVENT_USER_END,
                         EVENT_USER_END, &zero_time, &null_pack), 0);
    EXPECT(29, probe_sys6(SYS_pselect6, -1, 0, 0, 0, 0, EVENT_USER_END), -EFAULT);
    long alias = probe_sys3(SYS_fcntl, regular, 0, 64);
    EXPECT(30, alias, 64);
    EXPECT(31, probe_sys6(SYS_mmap, 0x45000000UL, 8192, PROT_READ | PROT_WRITE,
                         MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0), 0x45000000L);
    volatile unsigned long *boundary = (void *)0x45000ff8UL;
    boundary[0] = 8;
    boundary[1] = 1;
    EXPECT(32, probe_sys6(SYS_pselect6, 65, boundary, 0, 0, &zero_time, 0), 2);
    EXPECT(33, boundary[0], 8UL);
    EXPECT(34, boundary[1], 1UL);
    EXPECT(35, probe_sys2(SYS_munmap, 0x45001000UL, 4096), 0);
    EXPECT(36, probe_sys6(SYS_pselect6, 4, boundary, 0, 0, &zero_time, 0), 1);
    EXPECT(37, probe_sys6(SYS_pselect6, 65, boundary, 0, 0, &zero_time, 0), -EFAULT);
    close_fd(alias, 39);
    alias = probe_sys3(SYS_fcntl, regular, 0, 192);
    EXPECT(204, alias, 192);
    sets[0] = 0;
    sets[1] = 0;
    sets[2] = 0;
    sets[3] = 1;
    EXPECT(205, probe_sys6(SYS_pselect6, 193, sets, 0, 0, &zero_time, 0), 1);
    EXPECT(206, sets[3], 1UL);
    close_fd(alias, 207);
    EXPECT(40, probe_sys2(SYS_munmap, 0x45000000UL, 4096), 0);
}

/// 控制输入先后顺序、未支持能力和事件匿名OFD属性保留精确负errno。
static void epoll_abi(long regular)
{
    EXPECT(41, probe_sys1(SYS_epoll_create1, 1), -EINVAL);
    EXPECT(42, probe_sys4(SYS_epoll_ctl, epoll, 1, reader, EVENT_USER_END), -EFAULT);
    EXPECT(43, probe_sys4(SYS_epoll_ctl, -1, 3, -1, EVENT_USER_END), -EFAULT);
    set_event(1, DATA_READER);
    EXPECT(44, probe_sys4(SYS_epoll_ctl, writer, 1, regular, &event), -EPERM);
    EXPECT(45, probe_sys4(SYS_epoll_ctl, writer, 1, reader, &event), -EINVAL);
    ctl(1, epoll, -EINVAL, 46);
    long nested = probe_sys1(SYS_epoll_create1, 0);
    EXPECT(47, nested, 4);
    ctl(1, nested, -EINVAL, 48);
    close_fd(nested, 49);
    EXPECT(50, probe_sys4(SYS_epoll_ctl, epoll, 999, reader, EVENT_USER_END), -EINVAL);
    set_event(0x10000001, DATA_READER);
    ctl(1, reader, -EINVAL, 51);
    set_event(1, DATA_READER);
    ctl(3, reader, -ENOENT, 52);
    ctl(1, reader, 0, 53);
    ctl(1, reader, -EEXIST, 54);
    EXPECT(55, probe_sys4(SYS_epoll_ctl, epoll, 2, reader, EVENT_USER_END), 0);
    ctl(2, reader, -ENOENT, 56);
    wait_events(0, -EINVAL, 57);
    wait_events(-1, -EINVAL, 58);
    wait_events(134217728, -EINVAL, 59);
    EXPECT(60, probe_sys6(SYS_epoll_pwait, -1, EVENT_USER_END - 16, 2, 0, 0, 777), -EFAULT);
    EXPECT(61, probe_sys6(SYS_epoll_pwait, -1, output, 1, 0, 0, 777), -EBADF);
    EXPECT(62, probe_sys6(SYS_epoll_pwait, regular, output, 1, 0, 0, 777), -EINVAL);
    EXPECT(63, probe_sys6(SYS_epoll_pwait, epoll, output, 1, 0, &temporary_mask, 16), -EINVAL);
    EXPECT(64, probe_sys3(SYS_read, epoll, &received, 0), -EINVAL);
    EXPECT(65, probe_sys4(SYS_pread64, epoll, EVENT_USER_END, 0, 0), -ESPIPE);
    EXPECT(66, probe_sys3(SYS_lseek, epoll, 7, 0), 0);
    EXPECT(67, probe_sys3(SYS_fcntl, epoll, 3, 0), 2);
    EXPECT(68, probe_sys2(SYS_fcntl, epoll, 1), 1);
    union {
        unsigned long alignment;
        unsigned char bytes[128];
    } stat;
    EXPECT(69, probe_sys2(PROBE_NR_FSTAT, epoll, &stat), 0);
    unsigned stat_mode;
    memcpy(&stat_mode, stat.bytes + 16, sizeof(stat_mode));
    EXPECT(70, stat_mode, 0x180U);
    close_fd(regular, 71);
}

/// 两次20ms真实期限及只读剩余时间写回的结果优先级独立验证。
static void timeout_abi(void)
{
    struct Time before, after;
    EXPECT(72, probe_sys2(SYS_clock_gettime, 1, &before), 0);
    EXPECT(73, probe_sys5(SYS_ppoll, EVENT_USER_END, 0, &twenty_ms, 0, 999), 0);
    EXPECT(74, probe_sys2(SYS_clock_gettime, 1, &after), 0);
    PROBE_CHECK(75, 20000000L <= elapsed(&before, &after));
    EXPECT(76, twenty_ms.seconds, 0);
    EXPECT(77, twenty_ms.nanos, 0);
    twenty_ms.nanos = 20000000;
    EXPECT(78, probe_sys2(SYS_clock_gettime, 1, &before), 0);
    EXPECT(79, probe_sys6(SYS_pselect6, 0, EVENT_USER_END, EVENT_USER_END,
                         EVENT_USER_END, &twenty_ms, &null_pack), 0);
    EXPECT(80, probe_sys2(SYS_clock_gettime, 1, &after), 0);
    PROBE_CHECK(81, 20000000L <= elapsed(&before, &after));
    set_mask(0xa00, 82);
    pollfds[0].fd = writer;
    pollfds[0].events = 4;
    EXPECT(83, probe_sys5(SYS_ppoll, pollfds, 1, &readonly_short, &temporary_mask, 8), 1);
    query_mask(0xa00, 84);
    EXPECT(85, probe_sys5(SYS_ppoll, 0, 0, &readonly_short, &temporary_mask, 8), 0);
    query_mask(0xa00, 86);
    sets[0] = 0;
    sets[2] = 2;
    sets[4] = 0;
    EXPECT(87, probe_sys6(SYS_pselect6, 2, 0, sets + 2, 0, &readonly_short, &mask_pack), 1);
    query_mask(0xa00, 88);
    EXPECT(89, probe_sys6(SYS_epoll_pwait, epoll, output, 1, 1, &temporary_mask, 8), 0);
    query_mask(0xa00, 90);
    EXPECT(91, probe_sys5(SYS_ppoll, 0, 0, &bad_time, 0, 8), -EINVAL);
    poll_events(0, &negative_seconds, -EINVAL, 212);
    poll_events(0, &negative_nanos, -EINVAL, 213);
    EXPECT(214, probe_sys5(SYS_ppoll, 0, 0, &zero_time, EVENT_USER_END, 8), -EFAULT);
    query_mask(0xa00, 215);
    EXPECT(92, probe_sys5(SYS_ppoll, 0, 0, &zero_time, &temporary_mask, 16), -EINVAL);
    query_mask(0xa00, 93);
    EXPECT(94, probe_sys6(SYS_pselect6, -1, 0, 0, 0, &zero_time, &mask_pack), -EINVAL);
    query_mask(0xa00, 95);
}

/// 上一轮gate与完成字归零后，发布新phase供真实内核Blocked观察消费。
static void publish_phase(unsigned phase)
{
    probe_store_release(&control->gate, 0);
    probe_store_release(&control->child_done, 0);
    probe_store_release(&control->phase, phase);
}

/// 完成字只由真实子进程发布，yield不改变父等待的观察条件。
static void child_done(unsigned phase, long check)
{
    unsigned done;
    do {
        done = probe_load_acquire(&control->child_done);
        if (done != phase)
            probe_sys0(SYS_sched_yield);
    } while (done != phase);
    EXPECT(check, done, phase);
}

/// handler查询选择时的临时mask；内核RA trampoline负责恢复原signal frame。
static void signal_handler(int signal)
{
    (void)signal;
    query_mask(0x800, 125);
    unsigned count = probe_load_acquire(&control->handlers);
    probe_store_release(&control->handlers, count + 1);
}

/// fork子进程仅在父观察者放行当前gate后发真实信号或pipe字节。
static _Noreturn void worker(void *argument, unsigned long entry_sp, unsigned long entry_tp)
{
    (void)argument;
    (void)entry_sp;
    (void)entry_tp;
    for (unsigned phase = 1; phase < 4; ++phase) {
        while (probe_load_acquire(&control->phase) != phase ||
               probe_load_acquire(&control->gate) != 1)
            probe_sys0(SYS_sched_yield);
        if (mode == 's')
            EXPECT(124, probe_sys2(SYS_kill, root_pid, 10), 0);
        else
            write_byte(writer, 123);
        probe_store_release(&control->child_done, phase);
    }
    probe_exit(0);
}

/// 触碰真实输入页，避免文件缺页睡眠被父观察为安装临时mask后的park。
static void touch_input(const void *address)
{
    (void)*(const volatile unsigned char *)address;
}

/// 三轮分别沿ppoll、pselect6和epoll_pwait进入真正等待，保留原页协议。
static void wait_modes(void)
{
    if (mode == 'b') {
        set_event(1, DATA_READER);
        ctl(1, reader, 0, 96);
    } else {
        touch_input(&readonly_long);
        touch_input(&temporary_mask);
        touch_input(&mask_pack);
        touch_input(&long_time);
        set_mask(0xa00, 97);
        struct RawAction action = {signal_handler, 0x50000000UL, 0};
        EXPECT(98, probe_sys4(SYS_rt_sigaction, 10, &action, 0, 8), 0);
    }
    long child = probe_clone(17, 0, 0, 0, 0, worker, 0);
    PROBE_CHECK(99, 0 <= child);
    publish_phase(1);
    long result;
    if (mode == 's') {
        result = probe_sys5(SYS_ppoll, EVENT_USER_END, 0, &readonly_long, &temporary_mask, 8);
        probe_store_release(&control->result, (unsigned)result);
        EXPECT(103, result, -EINTR);
        query_mask(0xa00, 104);
        EXPECT(105, probe_load_acquire(&control->handlers), 1U);
    } else {
        pollfds[0].fd = reader;
        pollfds[0].events = 1;
        result = probe_sys5(SYS_ppoll, pollfds, 1, 0, 0, 999);
        probe_store_release(&control->result, (unsigned)result);
        EXPECT(100, result, 1);
        EXPECT(101, pollfds[0].revents, 1);
        read_byte(reader, 102);
    }
    child_done(1, 106);
    publish_phase(2);
    if (mode == 's') {
        result = probe_sys6(SYS_pselect6, 0, EVENT_USER_END, EVENT_USER_END,
                            EVENT_USER_END, &long_time, &mask_pack);
        probe_store_release(&control->result, (unsigned)result);
        EXPECT(110, result, -EINTR);
        query_mask(0xa00, 111);
        EXPECT(112, probe_load_acquire(&control->handlers), 2U);
    } else {
        sets[0] = 1;
        result = probe_sys6(SYS_pselect6, 1, sets, 0, 0, 0, 0);
        probe_store_release(&control->result, (unsigned)result);
        EXPECT(107, result, 1);
        EXPECT(108, sets[0], 1UL);
        read_byte(reader, 109);
    }
    child_done(2, 113);
    publish_phase(3);
    result = probe_sys6(SYS_epoll_pwait, epoll, output, 1, -1,
                        mode == 's' ? &temporary_mask : 0, mode == 's' ? 8 : 999);
    probe_store_release(&control->result, (unsigned)result);
    if (mode == 's') {
        EXPECT(117, result, -EINTR);
        query_mask(0xa00, 118);
        EXPECT(119, probe_load_acquire(&control->handlers), 3U);
    } else {
        EXPECT(114, result, 1);
        check_output(1, DATA_READER, 115);
        read_byte(reader, 116);
    }
    child_done(3, 120);
    int status;
    EXPECT(121, probe_sys4(SYS_wait4, child, &status, 0, 0), child);
    EXPECT(122, status, 0);
}

/// 原就绪、MOD、LT、ONESHOT与ET按真实读写和通知逐步消费。
static void ready_modes(void)
{
    set_event(1, DATA_READER);
    write_byte(writer, 128);
    ctl(1, reader, 0, 126);
    wait_events(1, 1, 129);
    check_output(1, DATA_READER, 130);
    wait_events(1, 1, 131);
    read_byte(reader, 132);
    wait_events(1, 0, 133);
    set_event(1, DATA_WRITER);
    ctl(1, writer, 0, 134);
    wait_events(2, 0, 135);
    set_event(4, DATA_WRITER);
    ctl(3, writer, 0, 136);
    wait_events(1, 1, 137);
    check_output(4, DATA_WRITER, 138);
    ctl(2, writer, 0, 139);
    set_event(0x40000001, DATA_READER);
    ctl(3, reader, 0, 140);
    write_byte(writer, 141);
    wait_events(1, 1, 142);
    check_output(1, DATA_READER, 143);
    wait_events(1, 0, 144);
    ctl(3, reader, 0, 145);
    wait_events(1, 1, 146);
    wait_events(1, 0, 147);
    read_byte(reader, 148);
    write_byte(writer, 149);
    wait_events(1, 0, 150);
    ctl(3, reader, 0, 151);
    wait_events(1, 1, 152);
    set_event(0x80000001, DATA_READER);
    ctl(3, reader, 0, 153);
    wait_events(1, 1, 154);
    wait_events(1, 0, 155);
    read_byte(reader, 156);
    write_byte(writer, 157);
    wait_events(1, 1, 158);
    wait_events(1, 0, 159);
    set_event(1, DATA_READER);
    ctl(3, reader, 0, 160);
    wait_events(1, 1, 161);
    wait_events(1, 1, 162);
    read_byte(reader, 163);
    wait_events(1, 0, 164);
}

/// 三个opaque身份的成员和计数保留原断言，不依赖ready链输出次序。
static void check_readers(unsigned count, unsigned expected_sum, long check)
{
    unsigned sum = 0;
    for (unsigned i = 0; i < count; ++i) {
        EXPECT(check, output[i].events, 1U);
        EXPECT(check, output[i].padding, 0U);
        unsigned long data = output[i].data;
        if (data == DATA_READER)
            sum += 1;
        else if (data == DATA_WRITER)
            sum += 2;
        else {
            EXPECT(check, data, DATA_ALIAS);
            sum += 4;
        }
    }
    EXPECT(check, sum, expected_sum);
}

/// dup别名与同号新OFD分别登记；原OFD最后close同步移除两个旧键。
static _Noreturn void reuse_modes(void)
{
    set_event(1, DATA_READER);
    ctl(1, reader, 0, 165);
    long alias = probe_sys1(SYS_dup, reader);
    EXPECT(166, alias, 3);
    set_event(1, DATA_ALIAS);
    ctl(1, alias, 0, 211);
    close_fd(reader, 167);
    EXPECT(168, probe_sys2(SYS_pipe2, fds, 0), 0);
    reader = fds[0];
    long new_writer = fds[1];
    EXPECT(169, reader, 0);
    EXPECT(170, new_writer, 4);
    set_event(1, DATA_WRITER);
    ctl(1, reader, 0, 171);
    write_byte(writer, 172);
    wait_events(3, 2, 173);
    check_readers(2, 5, 174);
    write_byte(new_writer, 175);
    wait_events(3, 3, 176);
    check_readers(3, 7, 177);
    close_fd(alias, 180);
    wait_events(2, 1, 181);
    check_output(1, DATA_WRITER, 182);
    close_fd(reader, 183);
    wait_events(2, 0, 184);
    close_fd(new_writer, 185);
    close_fd(writer, 186);
    close_fd(epoll, 187);
    probe_exit(0);
}

/// 跨坏页只确认完整事件，首轮ONESHOT|ET与重arm的ONESHOT均在fault后保项。
static void fault_modes(void)
{
    set_event(0xc0000001, DATA_READER);
    ctl(1, reader, 0, 188);
    set_event(0xc0000004, DATA_WRITER);
    ctl(1, writer, 0, 189);
    write_byte(writer, 190);
    EXPECT(191, probe_sys6(SYS_epoll_pwait, epoll, 0x48000000UL, 2, 0, 0, 8), -EFAULT);
    EXPECT(192, probe_sys6(SYS_mmap, 0x46000000UL, 4096, PROT_READ | PROT_WRITE,
                          MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0), 0x46000000L);
    volatile struct Event *boundary = (void *)0x46000ff0UL;
    EXPECT(193, probe_sys6(SYS_epoll_pwait, epoll, boundary, 2, 0, 0, 8), 1);
    EXPECT(194, boundary->padding, 0U);
    unsigned long first = boundary->data;
    wait_events(2, 1, 195);
    EXPECT(196, output[0].padding, 0U);
    unsigned long second = output[0].data;
    PROBE_CHECK(197, second != first);
    if (first == DATA_READER) {
        EXPECT(200, second, DATA_WRITER);
    } else {
        EXPECT(198, first, DATA_WRITER);
        EXPECT(199, second, DATA_READER);
    }
    wait_events(2, 0, 201);
    set_event(0x40000001, DATA_READER);
    ctl(3, reader, 0, 202);
    EXPECT(203, probe_sys6(SYS_epoll_pwait, epoll, 0x48000000UL, 1, 0, 0, 8), -EFAULT);
    wait_events(1, 1, 204);
    check_output(1, DATA_READER, 205);
    wait_events(1, 0, 206);
    EXPECT(207, probe_sys2(SYS_munmap, 0x46000000UL, 4096), 0);
}

/// 正常根进程显式关闭三个初始槽，退出后的Mm回收仍交生产拥有者。
static _Noreturn void close_pass(void)
{
    close_fd(reader, 208);
    close_fd(writer, 209);
    close_fd(epoll, 210);
    probe_exit(0);
}

/// 保留空fd表、原始argc和共享控制页，七模式只沿正式raw系统调用执行。
_Noreturn void probe_main(const unsigned long *initial_stack)
{
    EXPECT(1, initial_stack[0], 2UL);
    mode = *(const unsigned char *)initial_stack[2];
    root_pid = probe_sys0(SYS_getpid);
    EXPECT(2, probe_sys6(SYS_mmap, 0x44000000UL, 4096, PROT_READ | PROT_WRITE,
                        MAP_SHARED | MAP_ANONYMOUS | MAP_FIXED, -1, 0), 0x44000000L);
    EXPECT(3, probe_sys2(SYS_pipe2, fds, O_CLOEXEC), 0);
    reader = fds[0];
    writer = fds[1];
    EXPECT(4, reader, 0);
    EXPECT(5, writer, 1);
    epoll = probe_sys1(SYS_epoll_create1, O_CLOEXEC);
    EXPECT(6, epoll, 2);
    switch (mode) {
    case 'a': {
        long regular = poll_abi();
        select_abi(regular);
        epoll_abi(regular);
        break;
    }
    case 't':
        timeout_abi();
        break;
    case 'b':
    case 's':
        wait_modes();
        break;
    case 'e':
        ready_modes();
        break;
    case 'r':
        reuse_modes();
    case 'f':
        fault_modes();
        break;
    default:
        probe_failure(7);
    }
    close_pass();
}
