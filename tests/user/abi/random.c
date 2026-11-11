#define _GNU_SOURCE
#include <fcntl.h>
#include <pthread.h>
#include <sched.h>
#include <stdbool.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/sysmacros.h>
#include "kselftest.h"
#include "selftest_io.h"

enum {
    RANDOM_NONBLOCK = 1,
    RANDOM_POOL = 2,
    RANDOM_INSECURE = 4,
    BLOCK_BYTES = 64,
    THREADS = 4,
    THREAD_BLOCKS = 16,
};

/// 每个线程只写自己的样本，主线程在 join 后统一检查所有输出。
struct RandomWorker {
    /// 各线程共同持有的设备描述符，由主线程在全部 join 后关闭。
    int fd;
    /// 受测启动是否允许安全请求，由命令行明确选择。
    unsigned flags;
    /// 主线程创建全部工作线程后统一释放的启动门。
    atomic_bool *start;
    /// 当前线程独占的固定长度样本数组。
    unsigned char (*samples)[BLOCK_BYTES];
    /// 系统调用短读或失败时保存的错误，由 join 同步给主线程。
    int error;
};

/// 只检查独立哨兵区域，不把随机数据是否为某个固定值作为正确性条件。
static int unchanged(const unsigned char *bytes, size_t length, unsigned char value)
{
    for (size_t index = 0; index < length; ++index)
        if (bytes[index] != value)
            return 0;
    return 1;
}

/// 检查完整 512 位样本没有重放；该检查用于发现固定输出及独立打开或线程状态重置。
static int distinct_blocks(const unsigned char *bytes, size_t blocks)
{
    for (size_t left = 0; left < blocks; ++left) {
        for (size_t right = 0; right < left; ++right) {
            if (memcmp(bytes + left * BLOCK_BYTES,
                       bytes + right * BLOCK_BYTES, BLOCK_BYTES) == 0) {
                fprintf(stderr, "random repeated blocks %zu and %zu\n", left, right);
                return -1;
            }
        }
    }
    return 0;
}

/// 设备必须是权限 0666、设备号 1:9 的真实字符节点，打开实例仍指向同一个 inode。
static int node_identity(int fd)
{
    int result = -1;
    int writer = -1;
    struct stat path_stat, opened_stat;
    unsigned char byte;
    ST_CHECK(stat("/dev/urandom", &path_stat) == 0);
    ST_CHECK(fstat(fd, &opened_stat) == 0);
    ST_CHECK(S_ISCHR(path_stat.st_mode) && S_ISCHR(opened_stat.st_mode));
    ST_CHECK((path_stat.st_mode & 07777) == 0666);
    ST_CHECK((opened_stat.st_mode & 07777) == 0666);
    ST_CHECK(major(opened_stat.st_rdev) == 1 && minor(opened_stat.st_rdev) == 9);
    ST_CHECK(path_stat.st_rdev == opened_stat.st_rdev);
    ST_CHECK(path_stat.st_dev == opened_stat.st_dev && path_stat.st_ino == opened_stat.st_ino);
    writer = open("/dev/urandom", O_WRONLY | O_CLOEXEC);
    ST_CHECK(0 <= writer);
    errno = 0;
    ST_CHECK(read(writer, &byte, 1) == -1 && errno == EBADF);
    result = 0;
out:
    if (st_close(&writer) != 0)
        result = -1;
    return result;
}

/// read、重复偏移的 pread、seek 及独立打开均消费新字节，不提供可重放的文件内容。
static int device_stream(int fd)
{
    int result = -1;
    int independent = -1;
    unsigned char samples[4][512];
    unsigned char guarded[2055];
    independent = open("/dev/urandom", O_RDONLY | O_NONBLOCK | O_CLOEXEC);
    ST_CHECK(0 <= independent);
    ST_CHECK(read(fd, samples[0], sizeof samples[0]) == (ssize_t)sizeof samples[0]);
    ST_CHECK(pread(fd, samples[1], sizeof samples[1], 0) == (ssize_t)sizeof samples[1]);
    ST_CHECK(lseek(fd, 4096, SEEK_SET) == 0);
    ST_CHECK(pread(fd, samples[2], sizeof samples[2], 0) == (ssize_t)sizeof samples[2]);
    ST_CHECK(read(independent, samples[3], sizeof samples[3]) == (ssize_t)sizeof samples[3]);
    ST_CHECK(lseek(fd, -1, SEEK_SET) == 0 && lseek(fd, 0, SEEK_CUR) == 0);
    ST_CHECK(distinct_blocks((const unsigned char *)samples,
                            sizeof samples / BLOCK_BYTES) == 0);
    memset(guarded, 0x5a, sizeof guarded);
    ST_CHECK(read(fd, guarded + 1, sizeof guarded - 2) == (ssize_t)(sizeof guarded - 2));
    ST_CHECK(guarded[0] == 0x5a && guarded[sizeof guarded - 1] == 0x5a);
    ST_CHECK(distinct_blocks(guarded + 1, (sizeof guarded - 2) / BLOCK_BYTES) == 0);
    result = 0;
out:
    if (st_close(&independent) != 0)
        result = -1;
    return result;
}

/// 默认启动必须已有可信熵；显式无种子启动保持 EAGAIN，不安全输出不能使安全请求就绪。
static int getrandom_contract(int unseeded)
{
    int result = -1;
    unsigned char samples[4][BLOCK_BYTES];
    const unsigned flags[] = {
        0, RANDOM_NONBLOCK, RANDOM_POOL | RANDOM_NONBLOCK, RANDOM_INSECURE,
    };
    memset(samples, 0xa5, sizeof samples);
    errno = 0;
    ST_CHECK(syscall(SYS_getrandom, samples, BLOCK_BYTES, 8) == -1 && errno == EINVAL);
    errno = 0;
    ST_CHECK(syscall(SYS_getrandom, samples, BLOCK_BYTES,
                     RANDOM_POOL | RANDOM_INSECURE) == -1 && errno == EINVAL);
    ST_CHECK(unchanged((const unsigned char *)samples, sizeof samples, 0xa5));
    ST_CHECK(syscall(SYS_getrandom, NULL, 0, RANDOM_INSECURE) == 0);
    if (unseeded) {
        errno = 0;
        ST_CHECK(syscall(SYS_getrandom, samples, BLOCK_BYTES,
                         RANDOM_NONBLOCK) == -1 && errno == EAGAIN);
        ST_CHECK(unchanged((const unsigned char *)samples, sizeof samples, 0xa5));
    } else {
        ST_CHECK(syscall(SYS_getrandom, NULL, 0, RANDOM_NONBLOCK) == 0);
    }
    for (size_t index = 0; index < 4; ++index) {
        unsigned requested = unseeded ? RANDOM_INSECURE | RANDOM_NONBLOCK : flags[index];
        ST_CHECK(syscall(SYS_getrandom, samples[index], BLOCK_BYTES, requested) == BLOCK_BYTES);
    }
    ST_CHECK(distinct_blocks((const unsigned char *)samples, 4) == 0);
    if (unseeded) {
        unsigned char sentinel[BLOCK_BYTES];
        memset(sentinel, 0xa5, sizeof sentinel);
        errno = 0;
        ST_CHECK(syscall(SYS_getrandom, sentinel, sizeof sentinel,
                         RANDOM_POOL | RANDOM_NONBLOCK) == -1 && errno == EAGAIN);
        ST_CHECK(unchanged(sentinel, sizeof sentinel, 0xa5));
    }
    result = 0;
out:
    return result;
}

/// 设备与系统调用使用相同的真实用户地址，负描述符仅选择系统调用入口。
static ssize_t random_read(int fd, void *bytes, size_t length, unsigned flags)
{
    if (fd < 0)
        return syscall(SYS_getrandom, bytes, length, flags);
    return read(fd, bytes, length);
}

/// 跨保护页请求保留 137 字节实际前缀，首字节故障返回 EFAULT，零长不触碰保护页。
static int fault_prefix(int fd, unsigned flags)
{
    int result = -1;
    unsigned char *mapping = MAP_FAILED;
    long page = sysconf(_SC_PAGESIZE);
    ST_CHECK(137 < page);
    mapping = mmap(NULL, 2 * (size_t)page, PROT_READ | PROT_WRITE,
                   MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ST_CHECK(mapping != MAP_FAILED);
    memset(mapping, 0xa5, 2 * (size_t)page);
    ST_CHECK(mprotect(mapping + page, (size_t)page, PROT_NONE) == 0);
    ST_CHECK(random_read(fd, mapping + page, 0, flags) == 0);
    errno = 0;
    ST_CHECK(random_read(fd, mapping + page, 1, flags) == -1 && errno == EFAULT);
    ST_CHECK(random_read(fd, mapping + page - 137, 512, flags) == 137);
    ST_CHECK(unchanged(mapping, (size_t)page - 137, 0xa5));
    ST_CHECK(distinct_blocks(mapping + page - 137, 2) == 0);
    ST_CHECK(mprotect(mapping + page, (size_t)page, PROT_READ | PROT_WRITE) == 0);
    ST_CHECK(unchanged(mapping + page, (size_t)page, 0xa5));
    result = 0;
out:
    if (mapping != MAP_FAILED && munmap(mapping, 2 * (size_t)page) != 0)
        result = -1;
    return result;
}

/// 统一起跑后交替消费共享设备、定位读和 getrandom，每个线程只保存自己的完整样本。
static void *consume_random(void *argument)
{
    struct RandomWorker *worker = argument;
    while (!atomic_load_explicit(worker->start, memory_order_acquire))
        sched_yield();
    for (size_t index = 0; index < THREAD_BLOCKS; ++index) {
        ssize_t count;
        if (index % 3 == 0)
            count = read(worker->fd, worker->samples[index], BLOCK_BYTES);
        else if (index % 3 == 1)
            count = pread(worker->fd, worker->samples[index], BLOCK_BYTES, 0);
        else
            count = syscall(SYS_getrandom, worker->samples[index], BLOCK_BYTES, worker->flags);
        if (count != BLOCK_BYTES) {
            worker->error = count < 0 ? errno : EIO;
            break;
        }
    }
    return NULL;
}

/// 多线程的每个 512 位输出都必须独立，创建失败也释放起跑门并 join 已创建的线程。
static int threaded_stream(int fd, unsigned flags)
{
    int result = -1;
    size_t created = 0;
    atomic_bool start = ATOMIC_VAR_INIT(false);
    pthread_t threads[THREADS];
    unsigned char samples[THREADS][THREAD_BLOCKS][BLOCK_BYTES];
    struct RandomWorker workers[THREADS];
    for (; created < THREADS; ++created) {
        workers[created] = (struct RandomWorker) {
            .fd = fd, .flags = flags, .start = &start, .samples = samples[created],
        };
        int error = pthread_create(&threads[created], NULL, consume_random, &workers[created]);
        if (error != 0) {
            fprintf(stderr, "random pthread_create error=%d\n", error);
            goto out;
        }
    }
    result = 0;
out:
    atomic_store_explicit(&start, true, memory_order_release);
    for (size_t index = 0; index < created; ++index) {
        int error = pthread_join(threads[index], NULL);
        if (error != 0)
            ksft_exit_fail_msg("random pthread_join error=%d\n", error);
        if (workers[index].error != 0) {
            fprintf(stderr, "random thread read error=%d\n", workers[index].error);
            result = -1;
        }
    }
    if (result == 0)
        result = distinct_blocks((const unsigned char *)samples, THREADS * THREAD_BLOCKS);
    return result;
}

/// 默认模式验证可信启动；无种子模式单独验证早期非阻塞输出与安全请求的拒绝行为。
int main(int argc, char **argv)
{
    int unseeded = argc == 2 && strcmp(argv[1], "--unseeded") == 0;
    ksft_print_header();
    if (argc != 1 && !unseeded)
        ksft_exit_fail_msg("usage: %s [--unseeded]\n", argv[0]);
    int fd = open("/dev/urandom", O_RDONLY | O_NONBLOCK | O_CLOEXEC);
    if (fd < 0)
        ksft_exit_fail_msg("open urandom errno=%d\n", errno);
    unsigned flags = RANDOM_NONBLOCK | (unseeded ? RANDOM_INSECURE : 0);
    ksft_set_plan(6);
    ksft_test_result(node_identity(fd) == 0, "urandom character type mode and device identity\n");
    ksft_test_result(device_stream(fd) == 0, "read pread seek and independent open consume fresh blocks\n");
    ksft_test_result(getrandom_contract(unseeded) == 0, "getrandom flags readiness and explicit insecure output\n");
    ksft_test_result(fault_prefix(fd, flags) == 0, "urandom real EFAULT prefix and protected-page canaries\n");
    ksft_test_result(fault_prefix(-1, flags) == 0, "getrandom real EFAULT prefix and protected-page canaries\n");
    int threaded = threaded_stream(fd, flags);
    if (st_close(&fd) != 0)
        threaded = -1;
    ksft_test_result(threaded == 0, "threads share one advancing device and syscall stream\n");
    ksft_finished();
}
