#include "probe.h"

/* 内核 action 只含 handler、flags、mask；返回地址由内核信号帧提供。 */
struct PipeAction { unsigned long handler, flags, mask; };
struct PipeVector { void *base; unsigned long length; };
_Static_assert(sizeof(struct PipeAction) == 24, "raw signal action size");
_Static_assert(sizeof(struct PipeVector) == 16, "raw iovec size");

#if defined(__riscv)
#define PIPE_USER_END 0x4000000000UL
#elif defined(__loongarch64)
#define PIPE_USER_END 0x8000000000UL
#else
#error Unsupported pipe probe architecture
#endif

#define PTR(value) ((long)(value))
#define EXPECT(code, call, expected) PROBE_CHECK(code, (call) == (expected))
static volatile unsigned *const control = (volatile unsigned *)0x44000000UL;
static int fds[2];
static long root_pid;
static unsigned char page[4096], out[4096];
static unsigned char info[128] __attribute__((aligned(8)));
static unsigned char stat_bytes[128] __attribute__((aligned(8)));
static const unsigned char data[] = "abcdefgh";
static const unsigned long pipe_mask = 0x1000;
static const long zero_time[2] = { 0, 0 };
static const int nonblock_on = -1, nonblock_off = 0;
static const struct PipeVector data_vectors[] = {
    { (void *)data, 3 }, { (void *)-1L, 0 }, { (void *)(data + 3), 5 }
};
static const struct PipeVector read_vectors[] = {
    { out, 3 }, { (void *)-1L, 0 }, { out + 3, 5 }
};
static const struct PipeVector page_vectors[] = {
    { page, 2048 }, { page + 2048, 2048 }
};
static const struct PipeVector large_vectors[] = {
    { page, 4096 }, { page, 4096 }
};
static const struct PipeVector fault_write[] = {
    { (void *)data, 3 }, { (void *)0x45000000UL, 5 }
};
static const struct PipeVector fault_read[] = {
    { out, 1 }, { (void *)0x45000000UL, 2 }
};
static const struct PipeVector bad_length[] = { { out, 0x8000000000000000UL } };
static const struct PipeVector bad_range[] = { { page, 1 }, { (void *)-1L, 1 } };
static const struct PipeVector cap_single[] = {
    { (void *)(PIPE_USER_END - 0x7ffff000), 0x80000000UL }
};
static const struct PipeVector cap_multi_bad[] = {
    { (void *)(PIPE_USER_END - 0x7ffff000), 0x80000000UL }, { (void *)-1L, 0 }
};
static const struct PipeVector cap_multi_good[] = {
    { (void *)(PIPE_USER_END - 0x7ffff000), 0x7ffff000 }, { out, 4096 }
};
static const struct PipeVector cap_suffix_bad[] = {
    { (void *)(PIPE_USER_END - 0x7ffff000), 0x7ffff000 },
    { (void *)(PIPE_USER_END - 4096), 8192 }
};
static const struct PipeVector zeros[1024] = { [0 ... 1023] = { (void *)-1L, 0 } };
static const struct PipeVector oversize_vectors[17] = { [0 ... 16] = { page, 4096 } };
/* 保留真实文件 data 段，入口不清 BSS。 */
static volatile unsigned char file_data __attribute__((used)) = 0x57;

/// 断言失败仍用普通 exit 交付原始检查号，不改成组退出或成功结果。
_Noreturn void probe_failure(long check)
{
    probe_exit(check);
}

/// 按字节复制 wire 的 32 位字段，避免以不同有效类型访问内核输出缓冲区。
static unsigned wire_u32(const unsigned char *bytes)
{
    unsigned value;
    memcpy(&value, bytes, sizeof(value));
    return value;
}

/// 关闭真实 fd，并在失败时保留该操作原有的检查号。
static void close_fd(long fd, long check)
{
    EXPECT(check, probe_sys1(SYS_close, fd), 0);
}

/// 一次普通读必须交付指定长度，短读也作为原检查失败。
static void read_bytes(long fd, void *buffer, long length, long check)
{
    EXPECT(check, probe_sys3(SYS_read, fd, PTR(buffer), length), length);
}

/// 一次普通写必须接受指定长度，不能以重试掩盖短写。
static void write_bytes(long fd, const void *buffer, long length, long check)
{
    EXPECT(check, probe_sys3(SYS_write, fd, PTR(buffer), length), length);
}

/// 等待原共享 gate；yield 只保留旧客体的调度协议，不代替内核 Blocked 观察。
static void wait_gate(const volatile unsigned *word)
{
    while (probe_load_acquire(word) == 0)
        probe_sys0(SYS_sched_yield);
}

/// stack0 fork 由唯一 clone 桥分流，子路径不能返回父 C 调用帧。
static long fork_child(ProbeChildEntry entry, void *argument, long check)
{
    long pid = probe_clone(17, 0, 0, 0, 0, entry, argument);
    PROBE_CHECK(check, 0 <= pid);
    return pid;
}

/// wait4 消费指定子进程的真实正常退出状态。
static void wait_zero(long pid)
{
    int status;
    EXPECT(135, probe_sys4(SYS_wait4, pid, PTR(&status), 0, 0), pid);
    EXPECT(136, status, 0);
}

/// 非阻塞领取私有 SIGPIPE，零期限不增加一次真实睡眠。
static long take_pipe(void)
{
    return probe_sys4(SYS_rt_sigtimedwait, PTR(&pipe_mask), PTR(info), PTR(zero_time), 8);
}

/// 信号 handler 只发表一次完成字，普通返回沿内核提供的 RA trampoline 恢复现场。
static void pipe_handler(long signal, void *siginfo, void *context)
{
    (void)signal;
    (void)siginfo;
    (void)context;
    probe_store_release(control + 2, 1);
}

/// 提交精确 24 字节 action，不带 libc restorer 字段。
static void install_action(long signal, unsigned long handler, unsigned long flags, long check)
{
    const struct PipeAction action = { handler, flags, 0 };
    EXPECT(check, probe_sys4(SYS_rt_sigaction, signal, PTR(&action), 0, 8), 0);
}

/// 子进程持有最后 writer，直到父完成 EAGAIN 和共享 flags 检查才写入向量。
static void data_child(void *argument, unsigned long sp, unsigned long tp)
{
    (void)sp;
    (void)tp;
    long duplicate = (long)argument;
    close_fd(fds[0], 18);
    close_fd(fds[1], 19);
    wait_gate(control + 1);
    EXPECT(20, probe_sys3(SYS_writev, duplicate, PTR(data_vectors), 3), 8);
    close_fd(duplicate, 21);
    probe_exit(0);
}

/// 验证 FIFO 属性、dup/fork 端点拥有、带空段向量以及末 writer 关闭后的 EOF。
static void data_mode(void)
{
    EXPECT(150, probe_sys2(PROBE_NR_FSTAT, fds[0], PTR(stat_bytes)), 0);
    EXPECT(151, wire_u32(stat_bytes + 16), 0x1180);
    long duplicate = probe_sys1(SYS_dup, fds[1]);
    EXPECT(8, duplicate, 2);
    long child = fork_child(data_child, (void *)duplicate, 9);
    close_fd(fds[1], 10);
    close_fd(duplicate, 11);
    EXPECT(12, probe_sys3(SYS_read, fds[0], PTR(out), 8), -EAGAIN);
    EXPECT(13, probe_sys3(SYS_fcntl, fds[0], F_SETFL, 0), 0);
    probe_store_release(control + 1, 1);
    EXPECT(14, probe_sys3(SYS_readv, fds[0], PTR(read_vectors), 3), 8);
    for (unsigned i = 0; i < 8; ++i)
        EXPECT(15, out[i], data[i]);
    EXPECT(16, probe_sys3(SYS_read, fds[0], PTR(out), 1), 0);
    wait_zero(child);
    close_fd(fds[0], 17);
}

/// 每个独立 COW 子进程以两段向量原子写十六条完整页记录。
static void atomic_child(void *argument, unsigned long sp, unsigned long tp)
{
    (void)sp;
    (void)tp;
    close_fd(fds[0], 32);
    for (unsigned i = 0; i < sizeof(page); ++i)
        page[i] = 65 + (unsigned long)argument;
    for (unsigned i = 0; i < 16; ++i)
        EXPECT(33, probe_sys3(SYS_writev, fds[1], PTR(page_vectors), 2), 4096);
    close_fd(fds[1], 34);
    probe_exit(0);
}

/// 两个真实写者竞争管道，逐条逐字节核验 4096 字节原子性和各自记录数。
static void atomic_mode(void)
{
    long first = fork_child(atomic_child, 0, 22);
    long second = fork_child(atomic_child, (void *)1, 23);
    close_fd(fds[1], 24);
    unsigned a = 0, b = 0;
    do {
        read_bytes(fds[0], out, 4096, 25);
        unsigned byte = out[0];
        if (byte == 65)
            ++a;
        else {
            EXPECT(26, byte, 66);
            ++b;
        }
        for (unsigned i = 0; i < sizeof(out); ++i)
            EXPECT(27, out[i], byte);
    } while (a != 16 || b != 16);
    EXPECT(28, a, 16);
    EXPECT(29, b, 16);
    EXPECT(30, probe_sys3(SYS_read, fds[0], PTR(out), 1), 0);
    wait_zero(first);
    wait_zero(second);
    close_fd(fds[0], 31);
}

/// FIONBIO 先完整读取 int 才更改共享 OFD，不能破坏其它 flags 或 fd 的 CLOEXEC。
static void nonblock_flags(void)
{
    long duplicate = probe_sys1(SYS_dup, fds[0]);
    EXPECT(186, duplicate, 2);
    EXPECT(187, probe_sys2(SYS_fcntl, duplicate, F_GETFD), 0);
    EXPECT(188, probe_sys3(SYS_fcntl, fds[0], F_SETFL, 0x400), 0);
    EXPECT(189, probe_sys2(SYS_fcntl, duplicate, F_GETFL), 0x400);
    EXPECT(190, probe_sys3(SYS_ioctl, duplicate, 0x5421, PTR(&nonblock_on)), 0);
    EXPECT(191, probe_sys2(SYS_fcntl, fds[0], F_GETFL), 0xc00);
    EXPECT(192, probe_sys3(SYS_read, fds[0], PTR(out), 1), -EAGAIN);
    EXPECT(193, probe_sys3(SYS_ioctl, fds[0], 0x5421, 0x45000000), -EFAULT);
    volatile unsigned char *edge = (volatile unsigned char *)control + 4094;
    edge[0] = 0;
    edge[1] = 0;
    EXPECT(194, probe_sys3(SYS_ioctl, duplicate, 0x5421, PTR(edge)), -EFAULT);
    EXPECT(195, probe_sys2(SYS_fcntl, duplicate, F_GETFL), 0xc00);
    EXPECT(196, probe_sys3(SYS_ioctl, -1, 0x5421, 0x45000000), -EBADF);
    EXPECT(197, probe_sys3(SYS_ioctl, fds[0], 0, 0), -ENOTTY);
    EXPECT(198, probe_sys3(SYS_ioctl, fds[0], 0x5421, PTR(&nonblock_off)), 0);
    EXPECT(199, probe_sys2(SYS_fcntl, duplicate, F_GETFL), 0x400);
    EXPECT(200, probe_sys2(SYS_fcntl, fds[1], F_GETFL), 0x801);
    EXPECT(201, probe_sys2(SYS_fcntl, fds[0], F_GETFD), 1);
    EXPECT(202, probe_sys3(SYS_ioctl, duplicate, 0x5421, PTR(&nonblock_on)), 0);
    EXPECT(203, probe_sys2(SYS_fcntl, fds[0], F_GETFL), 0xc00);
    EXPECT(204, probe_sys3(SYS_fcntl, duplicate, F_SETFL, 0x800), 0);
    EXPECT(205, probe_sys2(SYS_fcntl, fds[0], F_GETFL), 0x800);
    close_fd(duplicate, 206);
}

/// 满十六物理页时保持 EAGAIN、零长成功、大向量短写和故障后的合法正前缀。
static void nonblock_mode(void)
{
    nonblock_flags();
    for (unsigned i = 0; i < 16; ++i)
        write_bytes(fds[1], page, 4096, 35);
    EXPECT(36, probe_sys3(SYS_write, fds[1], PTR(page), 1), -EAGAIN);
    EXPECT(37, probe_sys3(SYS_write, fds[1], -1, 0), 0);
    EXPECT(38, probe_sys3(SYS_read, fds[0], -1, 0), 0);
    read_bytes(fds[0], out, 4096, 39);
    EXPECT(40, probe_sys3(SYS_writev, fds[1], PTR(large_vectors), 2), 4096);
    for (unsigned i = 0; i < 16; ++i) {
        read_bytes(fds[0], out, 4096, 41);
        for (unsigned j = 0; j < sizeof(out); ++j)
            EXPECT(42, out[j], 65);
    }
    EXPECT(43, probe_sys3(SYS_read, fds[0], PTR(out), 1), -EAGAIN);
    EXPECT(44, probe_sys3(SYS_writev, fds[1], PTR(fault_write), 2), 3);
    EXPECT(45, probe_sys3(SYS_readv, fds[0], PTR(fault_read), 2), 1);
    EXPECT(46, out[0], 97);
    read_bytes(fds[0], out, 2, 47);
    EXPECT(48, out[0], 98);
    EXPECT(49, out[1], 99);
    close_fd(fds[0], 50);
    close_fd(fds[1], 51);
}

/// 向量导入保原错误顺序、1024 上界、MAX_RW_COUNT 裁剪及 dup 共享文件位置。
static void vector_mode(void)
{
    EXPECT(52, probe_sys3(SYS_writev, fds[1], -1, 0), 0);
    EXPECT(53, probe_sys3(SYS_writev, fds[1], -1, -1), -EINVAL);
    EXPECT(54, probe_sys3(SYS_writev, fds[1], PTR(zeros), 1025), -EINVAL);
    EXPECT(55, probe_sys3(SYS_writev, fds[1], 0x45000000, 1), -EFAULT);
    EXPECT(56, probe_sys3(SYS_readv, fds[0], PTR(bad_length), 1), -EINVAL);
    EXPECT(57, probe_sys3(SYS_writev, fds[1], PTR(bad_range), 2), -EFAULT);
    EXPECT(58, probe_sys3(SYS_writev, fds[1], PTR(zeros), 1024), 0);
    EXPECT(59, probe_sys3(SYS_writev, fds[0], 0x45000000, 1025), -EBADF);
    EXPECT(60, probe_sys3(SYS_readv, -1, 0x45000000, -1), -EBADF);
    long file = probe_sys4(SYS_openat, -100, PTR("/test/write-reserve"), 2, 0);
    PROBE_CHECK(61, 0 <= file);
    PROBE_CHECK(137, 0 <= probe_sys3(SYS_lseek, file, 0, 2));
    EXPECT(138, probe_sys3(SYS_readv, file, PTR(cap_single), 1), 0);
    EXPECT(139, probe_sys3(SYS_readv, file, PTR(cap_multi_bad), 2), -EFAULT);
    EXPECT(140, probe_sys3(SYS_readv, file, PTR(cap_multi_good), 2), 0);
    EXPECT(141, probe_sys3(SYS_readv, file, PTR(cap_suffix_bad), 2), -EFAULT);
    EXPECT(142, probe_sys3(SYS_lseek, file, 0, 0), 0);
    long duplicate = probe_sys1(SYS_dup, file);
    PROBE_CHECK(62, 0 <= duplicate);
    EXPECT(63, probe_sys3(SYS_writev, file, PTR(data_vectors), 3), 8);
    EXPECT(64, probe_sys3(SYS_lseek, duplicate, 0, 1), 8);
    EXPECT(65, probe_sys3(SYS_lseek, duplicate, 0, 0), 0);
    EXPECT(66, probe_sys3(SYS_readv, file, PTR(read_vectors), 3), 8);
    for (unsigned i = 0; i < 8; ++i)
        EXPECT(67, out[i], data[i]);
    EXPECT(68, probe_sys3(SYS_writev, file, PTR(fault_write), 2), 3);
    EXPECT(69, probe_sys3(SYS_lseek, duplicate, 0, 1), 11);
    EXPECT(70, probe_sys3(SYS_lseek, duplicate, 0, 0), 0);
    EXPECT(71, probe_sys3(SYS_readv, file, PTR(fault_read), 2), 1);
    EXPECT(72, probe_sys3(SYS_lseek, duplicate, 0, 1), 1);
    read_bytes(duplicate, out, 2, 73);
    EXPECT(74, out[0], 98);
    EXPECT(75, out[1], 99);
    close_fd(duplicate, 76);
    close_fd(file, 77);
    close_fd(fds[1], 78);
    close_fd(fds[0], 79);
}

/// SIG_IGN 下 EPIPE 无 pending，零长写仍成功。
static void ignored_mode(void)
{
    install_action(13, 1, 0, 80);
    close_fd(fds[0], 81);
    EXPECT(82, probe_sys3(SYS_write, fds[1], PTR(page), 1), -EPIPE);
    EXPECT(83, probe_sys3(SYS_write, fds[1], -1, 0), 0);
    EXPECT(84, take_pipe(), -EAGAIN);
    close_fd(fds[1], 85);
}

/// 最后 reader 关闭后执行真实写，默认 SIGPIPE 必须阻止普通失败退出。
static void default_child(void *argument, unsigned long sp, unsigned long tp)
{
    (void)argument;
    (void)sp;
    (void)tp;
    close_fd(fds[0], 91);
    wait_gate(control + 1);
    probe_sys3(SYS_write, fds[1], PTR(page), 1);
    probe_failure(92);
}

/// 父先撤全部端点再放子写，精确 wait4 验证信号 13 死亡。
static void default_mode(void)
{
    long child = fork_child(default_child, 0, 86);
    close_fd(fds[0], 87);
    close_fd(fds[1], 88);
    probe_store_release(control + 1, 1);
    int status;
    EXPECT(89, probe_sys4(SYS_wait4, child, PTR(&status), 0, 0), child);
    EXPECT(90, status, 13);
}

/// 子持最后 reader，只有原内核 fixture 放 gate 后才撤端点唤醒 writer。
static void blocked_child(void *argument, unsigned long sp, unsigned long tp)
{
    (void)argument;
    (void)sp;
    (void)tp;
    close_fd(fds[1], 104);
    wait_gate(control + 1);
    close_fd(fds[0], 105);
    probe_exit(0);
}

/// 两次真实 Blocked 分别验证无进度 EPIPE 与 64KiB 正前缀，并消费私有 SIGPIPE。
static void blocked_mode(void)
{
    EXPECT(93, probe_sys4(SYS_rt_sigprocmask, 0, PTR(&pipe_mask), 0, 8), 0);
    for (unsigned i = 0; i < 16; ++i)
        write_bytes(fds[1], page, 4096, 94);
    long child = fork_child(blocked_child, 0, 95);
    close_fd(fds[0], 96);
    probe_store_release(control, 1);
    EXPECT(97, probe_sys3(SYS_write, fds[1], PTR(page), 1), -EPIPE);
    EXPECT(98, take_pipe(), 13);
    EXPECT(99, wire_u32(info + 8), 0);
    EXPECT(100, wire_u32(info + 16), (unsigned)probe_sys0(SYS_getpid));
    EXPECT(101, wire_u32(info + 20), 0);
    EXPECT(102, take_pipe(), -EAGAIN);
    close_fd(fds[1], 103);
    wait_zero(child);
    EXPECT(143, probe_sys2(SYS_pipe2, PTR(fds), 0), 0);
    probe_store_release(control + 1, 0);
    child = fork_child(blocked_child, 0, 144);
    close_fd(fds[0], 145);
    probe_store_release(control, 2);
    EXPECT(146, probe_sys3(SYS_writev, fds[1], PTR(oversize_vectors), 17), 65536);
    EXPECT(147, take_pipe(), 13);
    EXPECT(148, wire_u32(info + 8), 0);
    close_fd(fds[1], 149);
    wait_zero(child);
}

/// 子在父真实 read 阻塞后发 USR1，等 handler 发表完成才写入 A。
static void interrupt_child(void *argument, unsigned long sp, unsigned long tp)
{
    (void)argument;
    (void)sp;
    (void)tp;
    close_fd(fds[0], 116);
    wait_gate(control + 1);
    EXPECT(117, probe_sys2(SYS_kill, root_pid, 10), 0);
    wait_gate(control + 2);
    write_bytes(fds[1], page, 1, 118);
    close_fd(fds[1], 119);
    probe_exit(0);
}

/// 同一 handler 返回路径分别观察 EINTR 和 SA_RESTART，不用用户延迟猜测等待登记。
static void interrupt_mode(int restart)
{
    install_action(10, (unsigned long)pipe_handler, restart ? 0x10000004 : 4,
                   restart ? 107 : 106);
    long child = fork_child(interrupt_child, 0, 108);
    close_fd(fds[1], 109);
    probe_store_release(control, 1);
    long result = probe_sys3(SYS_read, fds[0], PTR(out), 1);
    if (restart)
        EXPECT(112, result, 1);
    else {
        EXPECT(110, result, -EINTR);
        read_bytes(fds[0], out, 1, 111);
    }
    EXPECT(113, probe_load_acquire(control + 2), 1);
    EXPECT(114, out[0], 65);
    wait_zero(child);
    close_fd(fds[0], 115);
}

/// 实际填满 fd 表后验证 pipe2 第二预留失败撤回首槽，并按最低可用 fd 重用。
static void reservation_mode(void)
{
    EXPECT(120, probe_sys2(SYS_pipe2, 0x45000000, 0), -EFAULT);
    EXPECT(121, probe_sys3(SYS_fcntl, 2, F_GETFD, 0), -EBADF);
    long last = probe_sys1(SYS_dup, fds[0]);
    EXPECT(122, last, 2);
    for (;;) {
        long fd = probe_sys1(SYS_dup, fds[0]);
        if (fd == -EMFILE)
            break;
        PROBE_CHECK(123, 0 <= fd);
        ++last;
        EXPECT(124, fd, last);
    }
    close_fd(last, 123);
    EXPECT(125, probe_sys2(SYS_pipe2, PTR(fds), 0), -EMFILE);
    EXPECT(126, probe_sys3(SYS_fcntl, last, F_GETFD, 0), -EBADF);
    EXPECT(127, probe_sys1(SYS_dup, fds[0]), last);
    EXPECT(128, probe_sys1(SYS_dup, fds[0]), -EMFILE);
    close_fd(last, 129);
    close_fd(last - 1, 130);
    EXPECT(131, probe_sys2(SYS_pipe2, PTR(fds), 0), 0);
    EXPECT(132, fds[0], last - 1);
    EXPECT(133, fds[1], last);
    EXPECT(134, probe_sys2(SYS_pipe2, 0x45000000, 1), -EINVAL);
}

/// fstat 必须返回真实非 root 创建者身份。
static void owner_mode(void)
{
    EXPECT(152, probe_sys2(PROBE_NR_FSTAT, fds[0], PTR(stat_bytes)), 0);
    EXPECT(153, wire_u32(stat_bytes + 24), 1000);
    EXPECT(154, wire_u32(stat_bytes + 28), 2000);
    close_fd(fds[0], 155);
    close_fd(fds[1], 156);
}

/// 两端 pread/pwrite 在地址和 count 前拒绝流能力，负 offset 仍优先 EINVAL。
static void positioned_mode(void)
{
    long check = 157;
    for (unsigned endpoint = 0; endpoint < 2; ++endpoint) {
        long fd = fds[1 - endpoint];
        for (long syscall = SYS_pread64; syscall <= SYS_pwrite64; ++syscall) {
            for (unsigned invalid = 0; invalid < 2; ++invalid) {
                for (long count = 0; count < 2; ++count) {
                    long result = probe_sys4(syscall, fd, invalid ? -1 : PTR(out), count, 0);
                    probe_store_release(control + 3, (unsigned)result);
                    EXPECT(check, result, -ESPIPE);
                    ++check;
                }
            }
            long result = probe_sys4(syscall, fd, -1, 1, -1);
            probe_store_release(control + 3, (unsigned)result);
            EXPECT(check, result, -EINVAL);
            ++check;
        }
    }
    for (long syscall = SYS_pread64; syscall <= SYS_pwrite64; ++syscall) {
        EXPECT(check, probe_sys4(syscall, -1, -1, 1, 0), -EBADF);
        ++check;
        EXPECT(check, probe_sys4(syscall, -1, -1, 1, -1), -EINVAL);
        ++check;
    }
    write_bytes(fds[1], data, 8, 182);
    read_bytes(fds[0], out, 8, 183);
    close_fd(fds[0], 184);
    close_fd(fds[1], 185);
}

/// 保留空 fd 表、原始 argc 和全部十二种业务分派，成功与失败均用普通 exit。
_Noreturn void probe_main(const unsigned long *initial_stack)
{
    EXPECT(1, initial_stack[0], 2);
    (void)file_data;
    unsigned char mode = *(const unsigned char *)initial_stack[2];
    root_pid = probe_sys0(SYS_getpid);
    EXPECT(2, probe_sys6(SYS_mmap, PTR(control), 4096, 3, 0x31, -1, 0), PTR(control));
    for (unsigned i = 0; i < sizeof(page); ++i)
        page[i] = 65;
    long flags = 0x80000;
    if (mode == 'n' || mode == 'd')
        flags = 0x80800;
    EXPECT(3, probe_sys2(SYS_pipe2, PTR(fds), flags), 0);
    EXPECT(4, fds[0], 0);
    EXPECT(5, fds[1], 1);
    EXPECT(6, probe_sys3(SYS_fcntl, fds[0], F_GETFD, 0), 1);
    switch (mode) {
    case 'd': data_mode(); break;
    case 'a': atomic_mode(); break;
    case 'n': nonblock_mode(); break;
    case 'v': vector_mode(); break;
    case 'i': ignored_mode(); break;
    case 's': default_mode(); break;
    case 'b': blocked_mode(); break;
    case 'e': interrupt_mode(0); break;
    case 'r': interrupt_mode(1); break;
    case 'f': reservation_mode(); break;
    case 'u': owner_mode(); break;
    case 'p': positioned_mode(); break;
    default: probe_failure(7);
    }
    probe_exit(0);
}
