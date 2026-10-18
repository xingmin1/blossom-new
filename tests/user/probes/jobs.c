#include "tty_wire.h"

#if defined(__riscv)
#define J(rv, la) (rv)
#define JOBS_LA 0
#else
#define J(rv, la) (la)
#define JOBS_LA 1
#endif

static volatile unsigned *control;
static long last_result;
static long tty_fd, root_pid, root_pgid;
static int qpipe[2], apipe[2], wpipe[2];
static TtyTermios config = TTY_CONFIG_INITIALIZER;
static TtyTermios got_config;
static unsigned char byte;
static int pid_value;
static const TtyTime zero_time = {0, 0};
static const unsigned long winch_mask = 0x8000000;
/* siginfo只解释原检查使用的头部，剩余字节仍保内核128字节copyout容量。 */
static struct {
    int signo, error, code;
    unsigned char payload[116];
} info __attribute__((aligned(8)));
_Static_assert(sizeof(info) == 128, "siginfo wire size");

/* 内核raw action由handler、flags和mask组成，RA由真实内核trampoline安装。 */
typedef struct {
    unsigned long handler, flags, mask;
} JobsAction;
_Static_assert(sizeof(JobsAction) == 24, "raw sigaction wire size");

enum Child {
    READER, SUSPENDED, WRITER, MASKS, SETTER, MIDDLE, ORPHAN,
    LEADER, DETACHED, REUSED, WINDOW
};

/// 失败保完整检查号；exec后的q不访问已被退休的共享映射。
_Noreturn void probe_failure(long check)
{
    if (control) {
        if (!JOBS_LA)
            probe_store_release(control + 3, (unsigned)last_result);
        probe_store_release(control + 10, (unsigned)check);
    }
    probe_exit(check);
}

/// 每次检查先保存真实syscall结果，保留负errno诊断。
static void expect(long value, long wanted, long check)
{
    last_result = value;
    PROBE_CHECK(check, value == wanted);
}

/// 非负结果作为实际fd或PID使用，不假设内核预开标准流。
static long nonnegative(long value, long check)
{
    last_result = value;
    PROBE_CHECK(check, 0 <= value);
    return value;
}

/// 共享页以原字节偏移发布，跨Mm观察使用真实32位release。
static void put(unsigned offset, unsigned value)
{
    probe_store_release(control + offset / 4, value);
}

/// yield只等待门控；read、wait和sigtimedwait仍走生产等待路径。
static void wait_word(unsigned offset, unsigned value)
{
    while (probe_load_acquire(control + offset / 4) != value)
        probe_sys0(SYS_sched_yield);
}

/// 清理旧gate后再发表新phase，避免父消费前轮许可。
static void gate(unsigned phase)
{
    put(4, 0);
    put(0, phase);
    wait_word(4, phase);
}

/// 阶段done在真实字节和子事件消费后发表。
static void done(unsigned phase)
{
    put(8, phase);
}

/// 打开实际devfs名字，不创建替代会话或高层shell。
static long open_tty(const char *path, unsigned flags, long check)
{
    return nonnegative(probe_sys4(SYS_openat, -100, path, flags, 0), check);
}

/// close只在原检查指定的OFD边界执行。
static void close_fd(long fd, long check)
{
    expect(probe_sys1(SYS_close, fd), 0, check);
}

/// 普通ioctl保原result槽，参数指向本族明确wire布局。
static void ioctl_check(unsigned request, void *argument, long wanted, long check)
{
    long value = probe_sys3(SYS_ioctl, tty_fd, request, argument);
    put(12, (unsigned)value);
    expect(value, wanted, check);
}

/// 完整36字节termios保真实INT、QUIT和TSTP控制字符。
static void configure(unsigned iflag, unsigned lflag, long check)
{
    config.iflag = iflag;
    config.oflag = 0;
    config.cflag = 0xbf;
    config.lflag = lflag;
    ioctl_check(0x5402, &config, 0, check);
}

/// action直接使用24字节raw ABI，忽略与默认仍由真实信号实现解释。
static void action(unsigned signal, unsigned long handler, long check)
{
    JobsAction value = {handler, 0, 0};
    expect(probe_sys4(SYS_rt_sigaction, signal, &value, 0, 8), 0, check);
}

/// 信号屏蔽保8字节内核sigset，不使用libc扩展布局。
static void mask(unsigned how, unsigned long bits, long check)
{
    expect(probe_sys4(SYS_rt_sigprocmask, how, &bits, 0, 8), 0, check);
}

/// LA原子进程显式恢复处置和mask，RV只恢复原本修改的TTOU。
static void child_defaults(void)
{
    if (JOBS_LA) {
        action(2, 0, 12);
        action(20, 0, 12);
        action(21, 0, 12);
        action(22, 0, 12);
        mask(2, 0, 12);
    }
}

/// 子进程建立真实独立PGID，不能以数字写共享页代替关系更新。
static void own_group(long check)
{
    expect(probe_sys2(SYS_setpgid, 0, 0), 0, check);
}

/// 前台setter必须由生产tty背景检查决定是否允许。
static void foreground(long pid, long check)
{
    pid_value = pid;
    ioctl_check(0x5410, &pid_value, 0, check);
}

/// kill保真实组停止与继续，不模拟scheduler状态。
static void send_signal(long pid, unsigned signal, long check)
{
    expect(probe_sys2(SYS_kill, pid, signal), 0, check);
}

/// wait4同时核真实返回PID和完整status，不缩成shell八位结果。
static void reap(long pid, unsigned options, int wanted, long check)
{
    int status;
    long value = probe_sys4(SYS_wait4, pid, &status, options, 0);
    if (JOBS_LA)
        put(12, (unsigned)value);
    expect(value, pid, check);
    PROBE_CHECK(check, status == wanted);
}

/// 只有用户wait消费实际STOP事件后才递增父白盒读取的停止计数。
static void stopped(long pid, unsigned signal, long check)
{
    reap(pid, 2, (signal << 8) | 127, check);
    put(32, probe_load_acquire(control + 8) + 1);
}

/// 单字节read保实际返回值，背景访问仍由内核递送TTIN或EIO。
static void read_byte(long fd, long wanted, long check)
{
    long value = probe_sys3(SYS_read, fd, &byte, 1);
    put(12, (unsigned)value);
    expect(value, wanted, check);
}

/// 单字节write保TOSTOP、TTOU及真实UART输出。
static void write_byte(long fd, unsigned char value, long check)
{
    byte = value;
    long count = probe_sys3(SYS_write, fd, &byte, 1);
    put(12, (unsigned)count);
    expect(count, 1, check);
}

/// pipe2返回的两真实fd必须不同，后续fork和exec继承同一OFD。
static void make_pipe(int fds[2], long check)
{
    expect(probe_sys2(SYS_pipe2, fds, 0), 0, check);
    PROBE_CHECK(check, 0 <= fds[0] && 0 <= fds[1] && fds[0] != fds[1]);
}

/// 子回调由共享clone桥直接进入，并负责全部正常与信号退出。
static void child_entry(void *argument, unsigned long entry_sp, unsigned long entry_tp);

/// 唯一clone桥执行stack0的真实fork，子回调不返回旧父C帧。
static long fork_child(enum Child child, long check, int publish)
{
    long pid = nonnegative(probe_clone(17, 0, 0, 0, 0, child_entry,
                                      (void *)(unsigned long)child), check);
    if (publish)
        put(16, pid);
    return pid;
}

/// 后台read先TTIN，前台CONT后读取a，再由原read接收默认INT终止。
static void reader(void)
{
    child_defaults();
    own_group(J(42, 49));
    if (JOBS_LA)
        put(28, 1);
    else
        action(22, 0, 43);
    read_byte(tty_fd, 1, J(44, 50));
    PROBE_CHECK(J(45, 51), byte == 'a');
    put(J(28, 68), J(2, 1));
    wait_word(64, 2);
    if (JOBS_LA)
        put(28, 2);
    read_byte(tty_fd, 1, J(46, 52));
    probe_failure(J(47, 53));
}

/// TSTP后同一read在后台再遇TTIN，恢复前台后只能读取b。
static void suspended(void)
{
    child_defaults();
    own_group(J(48, 54));
    if (!JOBS_LA)
        action(22, 0, 49);
    put(J(28, 72), J(3, 1));
    wait_word(64, 3);
    if (JOBS_LA)
        put(28, 3);
    read_byte(tty_fd, 1, J(50, 55));
    PROBE_CHECK(J(51, 56), byte == 'b');
}

/// 背景write必须在配额开放前因TOSTOP停止，CONT后只输出D。
static void writer(void)
{
    child_defaults();
    own_group(J(52, 57));
    if (JOBS_LA)
        put(28, 5);
    else
        action(22, 0, 53);
    write_byte(tty_fd, 'D', J(54, 58));
}

/// TTIN屏蔽和忽略都返回EIO；TTOU的两种处置分别允许M和I。
static void masked_access(void)
{
    child_defaults();
    own_group(J(55, 59));
    if (!JOBS_LA) {
        action(22, 0, 56);
        put(28, 6);
    }
    wait_word(64, 6);
    if (JOBS_LA)
        put(28, 6);
    mask(0, 0x100000, J(57, 60));
    read_byte(tty_fd, -5, J(58, 61));
    mask(1, 0x100000, J(59, 62));
    action(21, 1, J(60, 63));
    read_byte(tty_fd, -5, J(61, 64));
    mask(0, 0x200000, J(62, 65));
    write_byte(tty_fd, 'M', J(63, 66));
    mask(1, 0x200000, J(64, 67));
    action(22, 1, J(65, 68));
    write_byte(tty_fd, 'I', J(66, 69));
}

/// 默认停止后raw ioctl必须以原参数重启并完整提交配置。
static void background_setter(void)
{
    child_defaults();
    own_group(J(67, 70));
    if (JOBS_LA)
        put(28, 7);
    else
        action(22, 0, 68);
    configure(0x20, 0x101, J(69, 71));
    ioctl_check(0x5401, &got_config, 0, J(70, 72));
    PROBE_CHECK(J(71, 73), got_config.iflag == 0x20);
    if (!JOBS_LA)
        PROBE_CHECK(72, got_config.lflag == 0x101);
}

/// 中间父先消费SIGSTOP，退出后消除孙组最后一个同会话父连接。
static void middle(void)
{
    if (!JOBS_LA) {
        own_group(73);
        action(22, 0, 74);
    }
    long pid = fork_child(ORPHAN, J(75, 74), 0);
    if (JOBS_LA)
        put(20, pid);
    stopped(pid, 19, J(76, 75));
    if (!JOBS_LA)
        put(20, pid);
    put(28, 8);
    wait_word(64, 8);
}

/// 停止孙组只能由孤儿HUP/CONT恢复后默认终止，普通返回是失败。
static void orphan(void)
{
    child_defaults();
    if (JOBS_LA)
        action(1, 0, 76);
    own_group(77);
    long pid = probe_sys0(SYS_getpid);
    send_signal(pid, 19, 78);
    probe_failure(79);
}

/// 八阶段保原用户STOP/CONT/EXIT消费顺序及父的UART和孤儿回收观察。
static void foreground_jobs(void)
{
    if (JOBS_LA)
        tty_fd = open_tty("/dev/ttyS0", 2, 10);
    configure(0, 0x101, J(5, 11));
    action(22, 1, J(6, 12));
    put(64, 0);
    long child = fork_child(READER, J(7, 13), 1);
    stopped(child, 21, J(8, 14));
    foreground(child, J(9, 15));
    action(22, 0, 201);
    ioctl_check(0x5410, &pid_value, -25, 202);
    ioctl_check(0x5402, &config, -5, 203);
    read_byte(tty_fd, -5, 204);
    action(22, 1, 205);
    send_signal(child, 18, J(10, 16));
    reap(child, 8, 65535, J(11, 17));
    put(36, 1);
    gate(1);
    wait_word(J(28, 68), J(2, 1));
    done(1);
    gate(2);
    put(64, 2);
    reap(child, 0, 2, J(12, 18));
    foreground(root_pgid, J(13, 19));
    done(2);

    put(64, 0);
    if (JOBS_LA)
        put(72, 0);
    child = fork_child(SUSPENDED, J(14, 20), 1);
    wait_word(J(28, 72), J(3, 1));
    foreground(child, J(15, 21));
    gate(3);
    put(64, 3);
    stopped(child, 20, J(16, 22));
    foreground(root_pgid, J(17, 23));
    send_signal(child, 18, J(18, 24));
    stopped(child, 21, J(19, 25));
    foreground(child, J(20, 26));
    send_signal(child, 18, J(21, 27));
    done(3);
    gate(4);
    reap(child, 0, 0, J(22, 28));
    foreground(root_pgid, J(23, 29));
    done(4);

    child = fork_child(WRITER, J(24, 30), 1);
    stopped(child, 22, J(25, 31));
    foreground(child, J(26, 32));
    gate(5);
    send_signal(child, 18, J(27, 33));
    reap(child, 0, 0, J(28, 34));
    foreground(root_pgid, J(29, 35));
    done(5);

    put(64, 0);
    child = fork_child(MASKS, J(30, 36), 1);
    if (!JOBS_LA)
        wait_word(28, 6);
    gate(6);
    put(64, 6);
    reap(child, 0, 0, J(31, 37));
    done(6);

    child = fork_child(SETTER, J(32, 38), 1);
    stopped(child, 22, J(33, 39));
    gate(7);
    foreground(child, J(34, 40));
    send_signal(child, 18, J(35, 41));
    reap(child, 0, 0, J(36, 42));
    if (JOBS_LA) {
        ioctl_check(0x5401, &got_config, 0, 43);
        PROBE_CHECK(44, got_config.iflag == 0x20);
    }
    foreground(root_pgid, J(37, 45));
    if (!JOBS_LA)
        configure(0, 0x101, 38);
    done(7);

    put(64, 0);
    child = fork_child(MIDDLE, J(39, 46), 1);
    wait_word(28, 8);
    gate(8);
    put(64, 8);
    reap(child, 0, 0, J(40, 47));
    int status;
    long orphan_pid = probe_load_acquire(control + 5);
    expect(probe_sys4(SYS_wait4, orphan_pid, &status, 0, 0), -10, J(41, 48));
    done(8);
}

/// 真handler只增加共享计数，正常C返回沿内核RA上的rt_sigreturn。
static void usr1_handler(void)
{
    volatile unsigned *count = (volatile unsigned *)0x44000018;
    unsigned value = probe_load_acquire(count);
    probe_store_release(count, value + 1);
}

/// 旧OFD永久挂断，console和后续新open不能复活它。
static void hungup_fd(long fd)
{
    long saved = tty_fd;
    tty_fd = fd;
    read_byte(fd, 0, 134);
    byte = 'N';
    expect(probe_sys3(SYS_write, fd, &byte, 1), -5, 135);
    TtyPoll poll = {(int)fd, 0x145, 0};
    expect(probe_sys5(SYS_ppoll, &poll, 1, &zero_time, 0, J(8, 0)), 1, 136);
    if (JOBS_LA)
        PROBE_CHECK(137, poll.revents == 0x15d);
    else
        PROBE_CHECK(137, (poll.revents & 0x15d) == 0x15d);
    ioctl_check(0x5401, &got_config, -5, 138);
    ioctl_check(0x5410, &pid_value, -25, 139);
    tty_fd = saved;
}

/// setsid清自身ctty而保继承OFD，离栈许可由原父白盒发出。
static void detached(void)
{
    long inherited = open_tty("/dev/tty", 2, J(114, 116));
    long pid = probe_sys0(SYS_getpid);
    expect(probe_sys0(SYS_setsid), pid, J(115, 117));
    expect(probe_sys4(SYS_openat, -100, "/dev/tty", 2, 0), -6, J(116, 118));
    close_fd(inherited, J(117, 119));
    put(44, pid);
    put(48, 1);
    wait_word(64, 1);
}

/// 会话首fork保ctty，真实exec仅编码继承fd，不携带匿名共享页。
static void leader(void)
{
    long pid = probe_sys0(SYS_getpid);
    expect(probe_sys0(SYS_setsid), pid, J(104, 106));
    tty_fd = open_tty("/dev/ttyS0", 2, J(105, 107));
    long alias = open_tty("/dev/tty", 2, J(106, 108));
    ioctl_check(0x5429, &pid_value, 0, J(107, 109));
    PROBE_CHECK(J(108, 110), pid_value == pid);
    ioctl_check(0x540f, &pid_value, 0, J(109, 111));
    PROBE_CHECK(J(110, 112), pid_value == pid);
    long child = fork_child(DETACHED, J(111, 113), 0);
    reap(child, 0, 0, J(112, 114));
    char encoded_write[2] = {(char)(qpipe[1] + 33), 0};
    char encoded_read[2] = {(char)(apipe[0] + 33), 0};
    char encoded_tty[2] = {(char)(alias + 33), 0};
    const char *argv[] = {"/test/bin/user-jobs", "q", encoded_write, encoded_read, encoded_tty, 0};
    const char *envp[] = {0};
    last_result = probe_sys3(SYS_execve, argv[0], argv, envp);
    probe_failure(J(113, 115));
}

/// 同号新PGID安装真实handler，并在pipe生产等待中接受父的旧Arc观察。
static void reused(void)
{
    own_group(J(118, 120));
    action(10, (unsigned long)usr1_handler, J(119, 121));
    put(24, 0);
    put(28, 2);
    read_byte(apipe[0], 1, J(120, 122));
    PROBE_CHECK(J(121, 123), byte == 'A');
    PROBE_CHECK(J(122, 124), probe_load_acquire(control + 6) == 0);
}

/// 三打开角色、dup同OFD、exec退出挂断及新旧PGID身份各保真实生命周期。
static void hangup_identity(void)
{
    long console = open_tty("/dev/console", 2, 80);
    expect(probe_sys4(SYS_openat, -100, "/dev/tty", 2, 0), -6, 81);
    tty_fd = open_tty("/dev/ttyS0", 0x102, 82);
    long duplicate = nonnegative(probe_sys1(SYS_dup, tty_fd), 83);
    close_fd(tty_fd, 84);
    tty_fd = duplicate;
    ioctl_check(0x5401, &got_config, 0, 85);
    expect(probe_sys4(SYS_openat, -100, "/dev/tty", 2, 0), -6, 86);
    make_pipe(qpipe, 87);
    make_pipe(apipe, 88);
    long child = fork_child(LEADER, 89, 1);
    read_byte(qpipe[0], 1, 90);
    PROBE_CHECK(91, byte == 'Q');
    gate(1);
    write_byte(apipe[1], 'A', 92);
    reap(child, 0, 0, 93);
    hungup_fd(tty_fd);
    expect(probe_sys3(SYS_ioctl, console, 0x5401, &got_config), 0, 94);
    done(1);

    long replacement = fork_child(REUSED, 95, 1);
    PROBE_CHECK(96, replacement == child);
    wait_word(28, 2);
    gate(2);
    write_byte(apipe[1], 'A', 97);
    reap(replacement, 0, 0, 98);
    done(2);

    long old = tty_fd;
    tty_fd = open_tty("/dev/ttyS0", 0x102, 99);
    configure(0, 0, 100);
    hungup_fd(old);
    gate(3);
    read_byte(tty_fd, 1, 101);
    PROBE_CHECK(102, byte == 'H');
    write_byte(tty_fd, 'N', 103);
    hungup_fd(old);
    done(3);
    if (JOBS_LA) {
        close_fd(old, 104);
        close_fd(console, 105);
        close_fd(qpipe[0], 105);
        close_fd(qpipe[1], 105);
        close_fd(apipe[0], 105);
        close_fd(apipe[1], 105);
    }
}

/// 原fd+33编码必须是单个非NUL字节，RV另保有限表上界。
static long decode_fd(const unsigned char *encoded)
{
    PROBE_CHECK(J(131, 2), encoded[1] == 0);
    long fd = (long)encoded[0] - 33;
    PROBE_CHECK(J(132, 2), 0 <= fd);
    if (!JOBS_LA)
        PROBE_CHECK(133, fd < 128);
    return fd;
}

/// q只消费exec继承fd，SID/PGID和ctty必须保持原会话首身份。
static _Noreturn void exec_handshake(const unsigned long *stack)
{
    PROBE_CHECK(J(123, 1), stack[0] == 5);
    if (JOBS_LA) {
        const unsigned char *mode = (const unsigned char *)stack[2];
        PROBE_CHECK(2, mode[0] == 'q' && mode[1] == 0);
    }
    long output = decode_fd((const unsigned char *)stack[3]);
    long input = decode_fd((const unsigned char *)stack[4]);
    long fd = decode_fd((const unsigned char *)stack[5]);
    long pid = probe_sys0(SYS_getpid);
    if (JOBS_LA) {
        expect(probe_sys1(SYS_getsid, 0), pid, 125);
        expect(probe_sys1(SYS_getpgid, 0), pid, 126);
    }
    expect(probe_sys3(SYS_ioctl, fd, 0x5429, &pid_value), 0, J(124, 127));
    PROBE_CHECK(J(125, 128), pid_value == pid);
    expect(probe_sys3(SYS_ioctl, fd, 0x540f, &pid_value), 0, J(126, 129));
    PROBE_CHECK(J(127, 130), pid_value == pid);
    byte = 'Q';
    expect(probe_sys3(SYS_write, output, &byte, 1), 1, J(128, 131));
    expect(probe_sys3(SYS_read, input, &byte, 1), 1, J(129, 132));
    PROBE_CHECK(J(130, 133), byte == 'A');
    probe_exit(0);
}

/// sigtimedwait必须取得真实WINCH，空队列保EAGAIN。
static void take_winch(long wanted, long check)
{
    expect(probe_sys4(SYS_rt_sigtimedwait, &winch_mask, &info, &zero_time, 8), wanted, check);
}

/// 完整siginfo的signo和内核si_code均保原断言。
static void winch(long check)
{
    take_winch(28, check);
    PROBE_CHECK(check, info.signo == 28);
    PROBE_CHECK(check, info.code == 128);
}

/// winsize用四个u16的原8字节wire，不依赖libc扩展。
static void get_size(uint64_t wanted, long check)
{
    uint64_t size;
    ioctl_check(0x5413, &size, 0, check);
    PROBE_CHECK(check, size == wanted);
}

/// 只有完整copyin成功的真实变化才应提交新窗口并发WINCH。
static void set_size(uint64_t size, long check)
{
    ioctl_check(0x5414, &size, 0, check);
}

static const uint64_t initial_size = 0x0000000000500018ULL;
static const uint64_t changed_size = J(0x0004000300660021ULL, 0x00c801400061001fULL);
static const uint64_t pixel_size = J(0x0004000500660021ULL, 0x00c901410061001fULL);
static const uint64_t foreground_size = J(0x0009000700780028ULL, 0x01e0028000620020ULL);

/// 当前前台child阻塞在生产sigtimedwait上，只该组领取窗口信号。
static void window_child(void)
{
    own_group(J(169, 178));
    put(J(28, 72), 1);
    wait_word(64, 1);
    if (JOBS_LA)
        put(28, 1);
    expect(probe_sys4(SYS_rt_sigtimedwait, &winch_mask, &info, 0, 8), 28, J(170, 179));
    if (JOBS_LA)
        PROBE_CHECK(180, info.signo == 28);
    PROBE_CHECK(J(171, 181), info.code == 128);
    get_size(foreground_size, J(172, 182));
    if (JOBS_LA)
        take_winch(-11, 183);
    write_byte(wpipe[1], 'W', J(173, 184));
}

/// 窗口相同值、pixel变化、坏页原子性和前台递送保两目标原始数值与检查号。
static void window_changes(void)
{
    if (JOBS_LA)
        tty_fd = open_tty("/dev/ttyS0", 2, 140);
    else
        action(22, 1, 140);
    mask(0, 0x8000000, 141);
    get_size(initial_size, 142);
    set_size(initial_size, 143);
    take_winch(-11, 144);
    set_size(changed_size, 145);
    winch(146);
    if (JOBS_LA)
        take_winch(-11, 147);
    get_size(changed_size, J(147, 148));
    if (!JOBS_LA)
        take_winch(-11, 148);
    set_size(changed_size, 149);
    take_winch(-11, 150);
    set_size(pixel_size, 151);
    winch(152);
    if (JOBS_LA)
        take_winch(-11, 153);
    get_size(pixel_size, J(153, 154));
    expect(probe_sys6(SYS_mmap, 0x45000000, 8192, 3, 0x32, -1, 0), 0x45000000, J(154, 155));
    *(volatile uint32_t *)0x45000ffc = J(0x0063002f, 0x00200010);
    expect(probe_sys3(SYS_mprotect, 0x45001000, 4096, 0), 0, J(155, 156));
    ioctl_check(0x5414, (void *)0x45000ffc, -14, J(156, 157));
    get_size(pixel_size, J(157, 158));
    take_winch(-11, J(158, 159));
    expect(probe_sys2(SYS_munmap, 0x45000000, 8192), 0, J(159, 160));
    make_pipe(wpipe, J(160, 161));
    put(64, 0);
    if (JOBS_LA)
        put(72, 0);
    long child = fork_child(WINDOW, J(161, 162), 1);
    wait_word(J(28, 72), 1);
    foreground(child, J(162, 163));
    put(64, 1);
    if (JOBS_LA)
        wait_word(28, 1);
    gate(1);
    set_size(foreground_size, J(163, 164));
    read_byte(wpipe[0], 1, J(164, 165));
    PROBE_CHECK(J(165, 166), byte == 'W');
    if (JOBS_LA)
        take_winch(-11, 167);
    reap(child, 0, 0, J(166, 168));
    if (!JOBS_LA)
        take_winch(-11, 167);
    if (JOBS_LA)
        mask(0, 0x200000, 169);
    foreground(root_pgid, J(168, 170));
    if (JOBS_LA) {
        mask(1, 0x200000, 171);
        take_winch(-11, 172);
    }
    set_size(initial_size, J(174, 173));
    winch(J(175, 174));
    take_winch(-11, J(176, 175));
    if (JOBS_LA) {
        get_size(initial_size, 176);
        close_fd(wpipe[0], 177);
        close_fd(wpipe[1], 177);
    }
    done(1);
}

/// clone子路径按真实业务分派，所有分支以sys93退出而不返回共享桥。
static void child_entry(void *argument, unsigned long entry_sp, unsigned long entry_tp)
{
    (void)entry_sp;
    (void)entry_tp;
    switch ((enum Child)(unsigned long)argument) {
    case READER: reader(); break;
    case SUSPENDED: suspended(); break;
    case WRITER: writer(); break;
    case MASKS: masked_access(); break;
    case SETTER: background_setter(); break;
    case MIDDLE: middle(); break;
    case ORPHAN: orphan(); break;
    case LEADER: leader(); break;
    case DETACHED: detached(); break;
    case REUSED: reused(); break;
    case WINDOW: window_child(); break;
    }
    probe_exit(0);
}

/// 原初始栈分派j/h/w和内部q，正常根在phase99后才结束TTY拥有。
_Noreturn void probe_main(const unsigned long *initial_stack)
{
    control = 0;
    last_result = 0;
    if (JOBS_LA)
        PROBE_CHECK(1, initial_stack[0] == 2 || initial_stack[0] == 5);
    else
        PROBE_CHECK(1, 2 <= initial_stack[0]);
    const unsigned char *mode = (const unsigned char *)initial_stack[2];
    if (JOBS_LA && initial_stack[0] == 5)
        exec_handshake(initial_stack);
    if (!JOBS_LA && mode[0] == 'q')
        exec_handshake(initial_stack);
    if (JOBS_LA)
        PROBE_CHECK(3, mode[1] == 0);
    expect(probe_sys6(SYS_mmap, 0x44000000, 4096, 3, 0x31, -1, 0), 0x44000000, J(2, 4));
    control = (volatile unsigned *)0x44000000;
    root_pid = probe_sys0(SYS_getpid);
    root_pgid = root_pid;
    if (JOBS_LA) {
        nonnegative(root_pid, 5);
        root_pgid = probe_sys1(SYS_getpgid, 0);
        expect(root_pgid, root_pid, 6);
    } else if (mode[0] != 'h') {
        tty_fd = open_tty("/dev/ttyS0", 2, 3);
    }
    switch (mode[0]) {
    case 'j': foreground_jobs(); break;
    case 'h': hangup_identity(); break;
    case 'w': window_changes(); break;
    default: probe_failure(J(4, 7));
    }
    gate(99);
    if (JOBS_LA)
        close_fd(tty_fd, 190);
    probe_exit(0);
}
