#include "tty_wire.h"

static volatile unsigned *control;
static long tty_fd;
static TtyTermios config = TTY_CONFIG_INITIALIZER;
static unsigned char received[128];
static unsigned char large[65536];
static const TtyTime zero_time = {0, 0};
static const unsigned char raw[] = {0, 0xc3, 0xa9, 0xff, 10, 0x80};

/// 失败仍使用原始检查号退出，不提前关闭父正在观察的串口。
_Noreturn void probe_failure(long check)
{
    probe_exit(check);
}

/// 保存实际有符号返回值，供原内核协调者观察最后一次串口消费。
static void result(long value, long expected, long check)
{
    probe_store_release(control + 3, (unsigned)value);
    PROBE_CHECK(check, value == expected);
}

/// 每轮清门后发表阶段，yield只协调配额，不替代生产阻塞。
static void gate(unsigned phase)
{
    probe_store_release(control + 1, 0);
    probe_store_release(control, phase);
    while (probe_load_acquire(control + 1) != phase)
        probe_sys0(SYS_sched_yield);
}

/// 当前阶段的输入输出全部消费后才允许父检查全文。
static void done(unsigned phase)
{
    probe_store_release(control + 2, phase);
}

/// 用完整36字节配置提交普通TCSETS，保留原控制字符和硬件字段。
static void configure(unsigned iflag, unsigned oflag, unsigned lflag,
                      unsigned minimum, unsigned time)
{
    config.iflag = iflag;
    config.oflag = oflag;
    config.cflag = 0xbf;
    config.lflag = lflag;
    config.cc[5] = time;
    config.cc[6] = minimum;
    PROBE_CHECK(10, probe_sys3(SYS_ioctl, tty_fd, 0x5402, &config) == 0);
}

/// ioctl保留真实负errno和可见的最后结果。
static void ioctl_result(unsigned request, unsigned long argument, long expected, long check)
{
    result(probe_sys3(SYS_ioctl, tty_fd, request, argument), expected, check);
}

/// 普通read使用同一接收缓冲，不改变原短读和EOF边界。
static void read_bytes(unsigned length, long expected, long check)
{
    result(probe_sys3(SYS_read, tty_fd, received, length), expected, check);
}

/// 逐字节比较实际read/getter写回的数据，包含NUL与非UTF8字节。
static void bytes(const void *expected, unsigned length, long check)
{
    const unsigned char *wanted = expected;
    for (unsigned i = 0; i < length; ++i)
        PROBE_CHECK(check, received[i] == wanted[i]);
}

/// 单次write仍断言精确输入计数，不把ONLCR展开计为用户字节。
static void write_bytes(const void *buffer, unsigned length, long check)
{
    result(probe_sys3(SYS_write, tty_fd, buffer, length), length, check);
}

/// 大输出按真实短写前缀续传，后次满环调用必须进入生产等待。
static void write_all(unsigned length, long check)
{
    unsigned offset = 0;
    while (offset < length) {
        long count = probe_sys3(SYS_write, tty_fd, large + offset, length - offset);
        probe_store_release(control + 3, (unsigned)count);
        PROBE_CHECK(check, 0 < count && (unsigned long)count <= length - offset);
        offset += count;
    }
}

/// ppoll独立使用POLLIN谓词，空read返回零不表示fd可读。
static void poll_input(const TtyTime *timeout, long expected, long check)
{
    TtyPoll poll = {(int)tty_fd, 1, 0};
    PROBE_CHECK(check, probe_sys5(SYS_ppoll, &poll, 1, timeout, 0, 0) == expected);
}

/// 原跨页样本保固定地址，后一页在实际uaccess前撤掉权限。
static void fault_pages(long map_check, long protect_check)
{
    PROBE_CHECK(map_check, probe_sys6(SYS_mmap, 0x45000000, 8192, 3, 0x31, -1, 0)
                              == 0x45000000);
    PROBE_CHECK(protect_check, probe_sys3(SYS_mprotect, 0x45001000, 4096, 0) == 0);
}

/// raw模式保二进制和UTF8碎片，坏页只消费成功复制的正前缀。
static void raw_bytes(void)
{
    configure(0, 0, 0, 6, 0);
    gate(1);
    read_bytes(0, 0, 20);
    read_bytes(6, 6, 21);
    bytes(raw, 6, 22);
    write_bytes(raw, 2, 23);
    write_bytes(raw + 2, 4, 24);
    done(1);
    configure(0, 0, 0, 3, 0);
    fault_pages(25, 26);
    gate(2);
    PROBE_CHECK(27, probe_sys3(SYS_read, tty_fd, 0x45000fff, 3) == 1);
    PROBE_CHECK(28, *(volatile unsigned char *)0x45000fff == 'a');
    PROBE_CHECK(29, probe_sys3(SYS_read, tty_fd, 0x45001000, 2) == -14);
    read_bytes(2, 2, 121);
    PROBE_CHECK(122, received[0] == 'b');
    PROBE_CHECK(123, received[1] == 'c');
    done(2);
}

/// canonical保编辑、短读、VEOF和literal-next产生的不同记录。
static void canonical(void)
{
    static const unsigned char literal_eof[] = {4, 10};
    configure(0, 0, 0x8002, 1, 0);
    gate(1);
    read_bytes(0, 0, 30);
    read_bytes(2, 2, 31); bytes("ab", 2, 32);
    read_bytes(16, 2, 33); bytes("c\n", 2, 34);
    read_bytes(16, 2, 35); bytes("z\n", 2, 36);
    read_bytes(16, 3, 37); bytes("ok\n", 3, 38);
    read_bytes(3, 3, 39); bytes("par", 3, 40);
    read_bytes(16, 4, 41); bytes("tial", 4, 42);
    read_bytes(16, 0, 43);
    read_bytes(16, 2, 44); bytes(literal_eof, 2, 45);
    done(1);
}

/// 单调时钟差直接度量当前read实际等待，不以yield次数代替期限。
static void timed_read(long before_check, long read_check, long after_check,
                       long elapsed_check, long expected)
{
    TtyTime before, after;
    PROBE_CHECK(before_check, probe_sys2(SYS_clock_gettime, 1, &before) == 0);
    read_bytes(16, expected, read_check);
    PROBE_CHECK(after_check, probe_sys2(SYS_clock_gettime, 1, &after) == 0);
    long elapsed = (after.seconds - before.seconds) * 1000000000L
                   + after.nanoseconds - before.nanoseconds;
    PROBE_CHECK(elapsed_check, 60000000 <= elapsed);
}

/// 四种MIN/TIME保持总期限和首字节后间隔期限各自的边界。
static void read_deadlines(void)
{
    configure(0, 0, 0, 0, 0);
    gate(1); read_bytes(16, 0, 50); done(1);
    configure(0, 0, 0, 3, 0);
    gate(2); read_bytes(16, 3, 51); bytes("xyz", 3, 52); done(2);
    configure(0, 0, 0, 0, 1);
    gate(3); timed_read(53, 54, 55, 56, 0); done(3);
    configure(0, 0, 0, 0, 1);
    gate(4); read_bytes(16, 1, 57); bytes("!", 1, 58); done(4);
    configure(0, 0, 0, 3, 1);
    gate(5); timed_read(59, 60, 61, 62, 1); bytes("A", 1, 63); done(5);
}

/// 不足MIN的输入由NONBLOCK消费，poll仍保持独立可读谓词。
static void poll_predicate(void)
{
    const TtyTime hundred_ms = {0, 100000000}, second = {1, 0};
    configure(0, 0, 0, 3, 0);
    gate(1);
    poll_input(&hundred_ms, 0, 70);
    PROBE_CHECK(71, probe_sys3(SYS_fcntl, tty_fd, 4, 0x802) == 0);
    read_bytes(16, 1, 72); bytes("z", 1, 73);
    configure(0, 0, 0, 0, 0);
    read_bytes(16, 0, 74);
    poll_input(&zero_time, 0, 75);
    PROBE_CHECK(76, probe_sys3(SYS_fcntl, tty_fd, 4, 2) == 0);
    done(1);
    configure(0, 0, 0, 0, 1);
    gate(2);
    poll_input(&second, 1, 77);
    read_bytes(16, 1, 78); bytes("q", 1, 79);
    done(2);
}

/// 不执行应用read，输入IRQ和原Work必须独立完成完整回显。
static void irq_echo(void)
{
    configure(0x100, 5, 0x823a, 1, 0);
    gate(1);
    done(1);
}

/// 65536字节输入产生完整CRLF输出，并等待真实TCSBRK和TCSETSW排空。
static void output_drain(void)
{
    configure(0, 5, 0, 1, 0);
    for (unsigned i = 0; i < sizeof(large); i += 2) {
        large[i] = 'O';
        large[i + 1] = '\n';
    }
    gate(1);
    write_all(65536, 80);
    ioctl_result(0x5409, 1, 0, 81);
    PROBE_CHECK(82, probe_sys3(SYS_ioctl, tty_fd, 0x5403, &config) == 0);
    done(1);
}

/// getter和setter跨坏页均须失败，后继完整getter证明旧36字节配置未变。
static void termios_atomic(void)
{
    configure(0, 0, 0, 1, 0);
    PROBE_CHECK(90, probe_sys3(SYS_ioctl, tty_fd, 0x5401, received) == 0);
    bytes(&config, 36, 91);
#if defined(__riscv)
    const unsigned long user_end = 0x4000000000UL;
#else
    const unsigned long user_end = 0x8000000000UL;
#endif
    ioctl_result(0x5401, user_end, -14, 92);
    ioctl_result(0x5402, user_end, -14, 93);
    PROBE_CHECK(94, probe_sys6(SYS_mmap, 0x45000000, 8192, 3, 0x31, -1, 0)
                       == 0x45000000);
    volatile uint32_t *tail = (volatile uint32_t *)0x45000ff0;
    tail[0] = 0x100; tail[1] = 5; tail[2] = 0xbf; tail[3] = 0x8002;
    PROBE_CHECK(95, probe_sys3(SYS_mprotect, 0x45001000, 4096, 0) == 0);
    ioctl_result(0x5402, 0x45000ff0, -14, 96);
    ioctl_result(0x5401, 0x45000ff0, -14, 97);
    PROBE_CHECK(98, probe_sys3(SYS_ioctl, tty_fd, 0x5401, received) == 0);
    bytes(&config, 36, 99);
    ioctl_result(0x5405, 0, -25, 100);
    ioctl_result(0xdead, 0, -25, 101);
    gate(1); done(1);
    gate(2);
    PROBE_CHECK(102, probe_sys3(SYS_ioctl, tty_fd, 0x5404, &config) == 0);
    poll_input(&zero_time, 0, 103);
    PROBE_CHECK(104, probe_sys3(SYS_ioctl, tty_fd, 0x5403, &config) == 0);
    ioctl_result(0x540b, 2, 0, 105);
    ioctl_result(0x5409, 1, 0, 106);
    done(2);
}

/// STOP不依赖read；后继START输入必须唤醒真实满环writer。
static void flow_resume(void)
{
    configure(0x400, 0, 0, 1, 0);
    for (unsigned i = 0; i < 12288; ++i)
        large[i] = 'F';
    gate(1); done(1);
    gate(2);
    write_all(12288, 110);
    ioctl_result(0x5409, 1, 0, 111);
    done(2);
}

/// 原始argc和空fd表不经CRT处理，全部UART阶段核完才关闭最后OFD。
_Noreturn void probe_main(const unsigned long *initial_stack)
{
    PROBE_CHECK(1, initial_stack[0] == 2);
    unsigned char mode = *(const unsigned char *)initial_stack[2];
    long mapped = probe_sys6(SYS_mmap, 0x44000000, 4096, 3, 0x31, -1, 0);
    PROBE_CHECK(2, mapped == 0x44000000);
    control = (volatile unsigned *)mapped;
    tty_fd = probe_sys4(SYS_openat, -100, "/dev/ttyS0", 2, 0);
    PROBE_CHECK(3, 0 <= tty_fd);
    switch (mode) {
    case 'r': raw_bytes(); break;
    case 'c': canonical(); break;
    case 'v': read_deadlines(); break;
    case 'p': poll_predicate(); break;
    case 'e': irq_echo(); break;
    case 'o': output_drain(); break;
    case 't': termios_atomic(); break;
    case 'x': flow_resume(); break;
    default: probe_failure(4);
    }
    gate(99);
    PROBE_CHECK(120, probe_sys1(SYS_close, tty_fd) == 0);
    probe_exit(0);
}
