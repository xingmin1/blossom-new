#include "libc_common.h"

_Thread_local volatile unsigned tls_image = 0x13579bdfU;
_Thread_local volatile unsigned tls_zero;

/// 输出实际失败原因；成功消息只由已消费子进程状态的父进程产生。
int fail(const char *operation)
{
    fprintf(stderr, "libc-smoke: %s failed (errno=%d)\n", operation, errno);
    return EXIT_FAILURE;
}

/// 经可观察的内存访问检查小块分配，释放后不保留指针。
int check_malloc(void)
{
    volatile unsigned char *memory = malloc(257);
    if (memory == NULL)
        return fail("malloc");
    for (size_t i = 0; i < 257; ++i)
        memory[i] = (unsigned char)(i ^ 0xa5);
    for (size_t i = 0; i < 257; ++i) {
        if (memory[i] != (unsigned char)(i ^ 0xa5)) {
            free((void *)memory);
            return fail("malloc contents");
        }
    }
    free((void *)memory);
    return EXIT_SUCCESS;
}

/// 修改模板与零初始化 TLS，在实际调度系统调用返回后逐次核对。
int check_tls(void)
{
    tls_image = 0x2468ace0U;
    tls_zero = 0x10203040U;
    errno = EDOM;
    for (unsigned i = 0; i < 64; ++i) {
        if (sched_yield() != 0)
            return fail("sched_yield");
        if (tls_image != 0x2468ace0U || tls_zero != 0x10203040U || errno != EDOM)
            return fail("TLS after syscall");
    }
    return EXIT_SUCCESS;
}

/// 在迁核 pipe 窗口只核 TLS/errno，不以重复 yield 代替真实阻塞。
int check_tls_state(void)
{
    if (tls_image != 0x2468ace0U || tls_zero != 0x10203040U || errno != EDOM)
        return fail("TLS state");
    return EXIT_SUCCESS;
}

/// 触碰大栈的探测保持独立栈帧，未调用的受限启动模式不预留这128KiB。
__attribute__((noinline)) int check_stack(void)
{
    volatile unsigned char stack[128 * 1024];
    for (size_t offset = 0; offset < sizeof(stack); offset += 4096)
        stack[offset] = (unsigned char)(offset >> 12);
    if (stack[0] != 0 || stack[sizeof(stack) - 4096] != 31)
        return fail("volatile stack");
    return EXIT_SUCCESS;
}

/// 按 Linux dev_t 编码取得原 fstat 的 major，独立对照 statx 的显式设备字段。
uint32_t device_major(uint64_t device)
{
    return (uint32_t)(((device >> 8) & 0xfff) | ((device >> 32) & 0xfffff000));
}

/// 按 Linux dev_t 编码取得原 fstat 的 minor。
uint32_t device_minor(uint64_t device)
{
    return (uint32_t)((device & 0xff) | ((device >> 12) & 0xffffff00));
}

/// 阻塞 pipe 的有限消息保留短读写和 EINTR 语义，EOF 不能当作成功握手。
int transfer_pipe(int fd, void *bytes, size_t length, int writing)
{
    size_t offset = 0;
    while (offset < length) {
        ssize_t count = writing ? write(fd, (unsigned char *)bytes + offset, length - offset) :
                                  read(fd, (unsigned char *)bytes + offset, length - offset);
        if (count < 0 && errno == EINTR)
            continue;
        if (count <= 0)
            return fail("orphan pipe transfer");
        offset += (size_t)count;
    }
    return EXIT_SUCCESS;
}

/// 精确消费子 PID 与完整 wait 编码，不用截出的 exit byte 掩盖信号或停止事件。
int wait_status(pid_t child, int expected)
{
    int status;
    pid_t result;
    do {
        result = waitpid(child, &status, 0);
    } while (result < 0 && errno == EINTR);
    if (result != child || status != expected)
        return fail("root role full wait status");
    return EXIT_SUCCESS;
}

/// 比较标准 timespec 的有符号秒与纳秒，负秒不按无符号时间解释。
int same_file_time(struct timespec left, struct timespec right)
{
    return left.tv_sec == right.tv_sec && left.tv_nsec == right.tv_nsec;
}

/// 时钟前后夹住 NOW 的实际值，不能仅凭时间非零判断提交成功。
int file_time_before(struct timespec left, struct timespec right)
{
    return left.tv_sec < right.tv_sec ||
           (left.tv_sec == right.tv_sec && left.tv_nsec <= right.tv_nsec);
}

/// 时间修改保留文件身份、权限、内容长度和块计数，不比较 stat 的填充字节。
int same_file_attributes(const struct stat *left, const struct stat *right)
{
    return left->st_dev == right->st_dev && left->st_ino == right->st_ino &&
           left->st_mode == right->st_mode && left->st_uid == right->st_uid &&
           left->st_gid == right->st_gid && left->st_nlink == right->st_nlink &&
           left->st_size == right->st_size && left->st_blocks == right->st_blocks &&
           left->st_rdev == right->st_rdev;
}

/// 把实际时钟样本转换为纳秒，拒绝无效字段和不能表示的时间。
int runtime_nanoseconds(clockid_t clock, uint64_t *value)
{
    struct timespec sample;
    if (clock_gettime(clock, &sample) != 0)
        return fail("runtime clock sample");
    if (sample.tv_sec < 0 || sample.tv_nsec < 0 || 1000000000L <= sample.tv_nsec ||
        (UINT64_MAX - (uint64_t)sample.tv_nsec) / 1000000000ULL <
        (uint64_t)sample.tv_sec)
        return fail("runtime clock fields");
    *value = (uint64_t)sample.tv_sec * 1000000000ULL + (uint64_t)sample.tv_nsec;
    return EXIT_SUCCESS;
}

/// 期限检查容许调度迟到，只排除提前完成和数量级错误的等待。
int runtime_elapsed(uint64_t start, uint64_t minimum, const char *operation)
{
    uint64_t end;
    if (runtime_nanoseconds(CLOCK_MONOTONIC, &end) != EXIT_SUCCESS)
        return EXIT_FAILURE;
    if (end < start || end - start < minimum || 10000000000ULL < end - start) {
        fprintf(stderr, "libc-smoke: %s elapsed_ns=%llu minimum_ns=%llu\n",
                operation, (unsigned long long)(end - start),
                (unsigned long long)minimum);
        return fail(operation);
    }
    return EXIT_SUCCESS;
}
