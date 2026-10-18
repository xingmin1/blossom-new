#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include "kselftest.h"
#include "selftest_io.h"

static unsigned char original[1024];
static size_t mask_bytes;
static unsigned first_cpu, last_cpu;

/// 直接消费 syscall 返回的实际字节数，不把 libc 的零返回误当位图长度。
static long get_mask(pid_t tid, size_t length, void *mask)
{
    return syscall(SYS_sched_getaffinity, tid, length, mask);
}

/// 用字节表示用户 ABI，避免测试自身在 CPU64 或第二字发生越界移位。
static void single_mask(unsigned char *mask, unsigned cpu)
{
    memset(mask, 0, mask_bytes);
    mask[cpu / 8] = (unsigned char)(1u << (cpu % 8));
}

/// 设置一个实际允许 CPU，调用返回必须已完成当前执行者的真实迁移。
static int pin_cpu(pid_t tid, unsigned cpu)
{
    unsigned char mask[sizeof(original)];
    single_mask(mask, cpu);
    return syscall(SYS_sched_setaffinity, tid, mask_bytes, mask);
}

/// getcpu 同时核对逻辑编号与单节点 ABI。
static int current_cpu(unsigned expected)
{
    unsigned cpu = UINT32_MAX, node = UINT32_MAX;
    return syscall(SYS_getcpu, &cpu, &node, NULL) == 0 && cpu == expected && node == 0;
}

/// 核对精确的继承或远程许可，不以当前执行 CPU 代替调度集合。
static int is_single(pid_t tid, unsigned cpu)
{
    unsigned char actual[sizeof(original)], expected[sizeof(original)];
    single_mask(expected, cpu);
    return get_mask(tid, sizeof(actual), actual) == (long)mask_bytes &&
           memcmp(actual, expected, mask_bytes) == 0;
}

/// 长 mask 只访问内部前缀，短 mask 补零；get 不修改更长缓冲尾部。
static int abi_boundaries(void)
{
    int result = -1;
    unsigned char mask[sizeof(original)], actual[sizeof(original)];
    long page = sysconf(_SC_PAGESIZE);
    void *mapping = MAP_FAILED;
    ST_CHECK(0 < page && mask_bytes <= (size_t)page);
    memset(actual, 0xa5, sizeof(actual));
    ST_CHECK(get_mask(0, sizeof(actual), actual) == (long)mask_bytes);
    ST_CHECK(memcmp(actual, original, mask_bytes) == 0);
    for (size_t i = mask_bytes; i < sizeof(actual); ++i)
        ST_CHECK(actual[i] == 0xa5);
    errno = 0;
    ST_CHECK(get_mask(0, mask_bytes - 1, actual) == -1 && errno == EINVAL);
    memset(mask, 0, sizeof(mask));
    errno = 0;
    ST_CHECK(syscall(SYS_sched_setaffinity, 0, mask_bytes, mask) == -1 && errno == EINVAL);
    errno = 0;
    ST_CHECK(syscall(SYS_sched_setaffinity, 0, 0, mask) == -1 && errno == EINVAL);
    errno = 0;
    ST_CHECK(syscall(SYS_sched_setaffinity, 0, mask_bytes, (void *)1) == -1 && errno == EFAULT);
    errno = 0;
    ST_CHECK(get_mask(0, sizeof(actual), (void *)1) == -1 && errno == EFAULT);
    errno = 0;
    ST_CHECK(get_mask(-1, sizeof(actual), actual) == -1 && errno == EINVAL);
    ST_CHECK(syscall(SYS_getcpu, NULL, NULL, (void *)1) == 0);
    errno = 0;
    ST_CHECK(syscall(SYS_getcpu, (void *)1, NULL, NULL) == -1 && errno == EFAULT);
    errno = 0;
    ST_CHECK(syscall(SYS_getcpu, NULL, (void *)1, NULL) == -1 && errno == EFAULT);
    single_mask(mask, first_cpu);
    ST_CHECK(syscall(SYS_sched_setaffinity, 0, first_cpu / 8 + 1, mask) == 0);
    ST_CHECK(is_single(0, first_cpu) && current_cpu(first_cpu));
    mapping = mmap(NULL, (size_t)page * 2, PROT_READ | PROT_WRITE,
                   MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ST_CHECK(mapping != MAP_FAILED);
    ST_CHECK(mprotect((unsigned char *)mapping + page, (size_t)page, PROT_NONE) == 0);
    unsigned char *edge = (unsigned char *)mapping + page - mask_bytes;
    memcpy(edge, mask, mask_bytes);
    ST_CHECK(syscall(SYS_sched_setaffinity, 0, mask_bytes + page, edge) == 0);
    ST_CHECK(get_mask(0, mask_bytes + page, edge) == (long)mask_bytes);
    ST_CHECK(memcmp(edge, mask, mask_bytes) == 0);
    result = 0;
out:
    if (mapping != MAP_FAILED && munmap(mapping, (size_t)page * 2) != 0)
        result = -1;
    if (syscall(SYS_sched_setaffinity, 0, mask_bytes, original) != 0)
        result = -1;
    return result;
}

/// 对本次允许集合的每个实际 CPU 执行用户代码，包含字宽边界及第二字。
static int migrate_every_cpu(void)
{
    int result = -1;
    for (unsigned cpu = 0; cpu < mask_bytes * 8; ++cpu) {
        if ((original[cpu / 8] & (1u << (cpu % 8))) == 0)
            continue;
        ST_CHECK(pin_cpu(0, cpu) == 0);
        ST_CHECK(current_cpu(cpu) && is_single(0, cpu));
    }
    result = 0;
out:
    if (syscall(SYS_sched_setaffinity, 0, mask_bytes, original) != 0)
        result = -1;
    return result;
}

/// fork 首次调度就遵守父许可，父修改不改变子的集合。
static int fork_inheritance(void)
{
    int result = -1;
    pid_t child = -1;
    ST_CHECK(pin_cpu(0, first_cpu) == 0);
    child = fork();
    ST_CHECK(0 <= child);
    if (child == 0)
        _exit(is_single(0, first_cpu) && current_cpu(first_cpu) ? 0 : 1);
    ST_CHECK(st_reap(&child, 0) == 0);
    result = 0;
out:
    if (st_stop_child(&child) != 0)
        result = -1;
    if (syscall(SYS_sched_setaffinity, 0, mask_bytes, original) != 0)
        result = -1;
    return result;
}

/// pipe 等待者在许可修改后恢复时必须使用新 CPU，旧等待仍由原事件释放。
static int wake_with_changed_mask(void)
{
    int result = -1, ready[2] = { -1, -1 }, go[2] = { -1, -1 };
    pid_t child = -1;
    unsigned char byte;
    ST_CHECK(pin_cpu(0, first_cpu) == 0);
    ST_CHECK(pipe(ready) == 0 && pipe(go) == 0);
    child = fork();
    ST_CHECK(0 <= child);
    if (child == 0) {
        if (close(ready[0]) != 0 || close(go[1]) != 0 ||
            st_write_all(ready[1], "R", 1) != 0 ||
            st_read_all(go[0], &byte, 1) != 0 || byte != 'G')
            _exit(1);
        _exit(current_cpu(last_cpu) && is_single(0, last_cpu) ? 0 : 1);
    }
    ST_CHECK(st_close(&ready[1]) == 0 && st_close(&go[0]) == 0);
    ST_CHECK(st_read_all(ready[0], &byte, 1) == 0 && byte == 'R');
    ST_CHECK(pin_cpu(child, last_cpu) == 0);
    ST_CHECK(st_write_all(go[1], "G", 1) == 0);
    ST_CHECK(st_reap(&child, 0) == 0);
    result = 0;
out:
    if (st_stop_child(&child) != 0)
        result = -1;
    if (st_close(&ready[0]) != 0)
        result = -1;
    if (st_close(&ready[1]) != 0)
        result = -1;
    if (st_close(&go[0]) != 0)
        result = -1;
    if (st_close(&go[1]) != 0)
        result = -1;
    if (syscall(SYS_sched_setaffinity, 0, mask_bytes, original) != 0)
        result = -1;
    return result;
}

struct remote {
    int writer;
    atomic_int phase;
};

struct ready {
    pid_t tid;
    int inherited;
};

/// 子线程发出身份后持续执行用户循环，父 set 返回后的首个快照必须在新 CPU。
static void *remote_thread(void *argument)
{
    struct remote *remote = argument;
    struct ready ready = { (pid_t)syscall(SYS_gettid), is_single(0, first_cpu) && current_cpu(first_cpu) };
    if (st_write_all(remote->writer, &ready, sizeof(ready)) != 0)
        return (void *)1;
    int phase;
    do {
        phase = atomic_load_explicit(&remote->phase, memory_order_acquire);
    } while (phase == 0);
    if (phase < 0)
        return (void *)1;
    unsigned cpu = UINT32_MAX;
    if (syscall(SYS_getcpu, &cpu, NULL, NULL) != 0 ||
        st_write_all(remote->writer, &cpu, sizeof(cpu)) != 0)
        return (void *)1;
    return NULL;
}

/// 从另一 CPU 修改正在执行的原线程，真实等待迁出后检查新执行位置并 join。
static int remote_migration(void)
{
    int result = -1, channel[2] = { -1, -1 }, started = 0;
    pthread_t thread;
    struct remote remote = { -1, ATOMIC_VAR_INIT(0) };
    struct ready ready;
    unsigned observed;
    void *status;
    ST_CHECK(pin_cpu(0, first_cpu) == 0);
    ST_CHECK(pipe(channel) == 0);
    remote.writer = channel[1];
    ST_CHECK(pthread_create(&thread, NULL, remote_thread, &remote) == 0);
    started = 1;
    ST_CHECK(pin_cpu(0, last_cpu) == 0);
    ST_CHECK(st_read_all(channel[0], &ready, sizeof(ready)) == 0 && ready.inherited);
    ST_CHECK(pin_cpu(ready.tid, last_cpu) == 0);
    ST_CHECK(is_single(ready.tid, last_cpu));
    atomic_store_explicit(&remote.phase, 1, memory_order_release);
    ST_CHECK(st_read_all(channel[0], &observed, sizeof(observed)) == 0 && observed == last_cpu);
    ST_CHECK(pthread_join(thread, &status) == 0);
    started = 0;
    ST_CHECK(status == NULL);
    result = 0;
out:
    atomic_store_explicit(&remote.phase, -1, memory_order_release);
    if (started && pthread_join(thread, &status) != 0)
        result = -1;
    if (st_close(&channel[0]) != 0)
        result = -1;
    if (st_close(&channel[1]) != 0)
        result = -1;
    if (syscall(SYS_sched_setaffinity, 0, mask_bytes, original) != 0)
        result = -1;
    return result;
}

/// 同一普通 C 输入消费精确 ABI、每核迁移、fork 与远程 clone 继承。
int main(void)
{
    ksft_print_header();
    ksft_set_plan(5);
    long length = get_mask(0, sizeof(original), original);
    if (length <= 0 || sizeof(original) < (size_t)length)
        ksft_exit_fail_msg("initial affinity snapshot failed: %ld errno=%d\n", length, errno);
    mask_bytes = (size_t)length;
    first_cpu = UINT32_MAX;
    for (unsigned cpu = 0; cpu < mask_bytes * 8; ++cpu) {
        if ((original[cpu / 8] & (1u << (cpu % 8))) == 0)
            continue;
        if (first_cpu == UINT32_MAX)
            first_cpu = cpu;
        last_cpu = cpu;
    }
    if (first_cpu == UINT32_MAX)
        ksft_exit_fail_msg("initial affinity has no CPU\n");
    ksft_test_result(abi_boundaries() == 0, "affinity short/long masks, untouched tail and getcpu pointers\n");
    ksft_test_result(migrate_every_cpu() == 0, "same user thread executes on every allowed CPU\n");
    ksft_test_result(fork_inheritance() == 0, "fork inherits actual scheduler permission\n");
    ksft_test_result(wake_with_changed_mask() == 0, "pipe waiter resumes under changed scheduler permission\n");
    if (first_cpu == last_cpu)
        ksft_test_result_skip("remote running migration requires two allowed CPUs\n");
    else
        ksft_test_result(remote_migration() == 0, "clone inheritance and synchronous remote running migration\n");
    ksft_finished();
}
