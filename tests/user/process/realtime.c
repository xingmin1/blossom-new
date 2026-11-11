#define _GNU_SOURCE
#include <errno.h>
#include <limits.h>
#include <pthread.h>
#include <sched.h>
#include <stdatomic.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <time.h>
#include "kselftest.h"
#include "selftest_io.h"

/// 运行顺序由同一个共享页传递，所有孩子均在实际允许的同一 CPU 上执行。
struct order {
    /// 已记录事件数量。
    atomic_int count;
    /// 真实用户执行顺序，容量覆盖本文件的每个有限场景。
    atomic_int events[8];
};

/// 向原始 sched_setscheduler ABI 交付单个四字节优先级。
static int set_policy(pid_t tid, int policy, int priority)
{
    struct sched_param param = { .sched_priority = priority };
    return syscall(SYS_sched_setscheduler, tid, policy, &param);
}

/// 保持当前策略和 reset 标志，只修改目标线程的实时优先级。
static int set_priority(pid_t tid, int priority)
{
    struct sched_param param = { .sched_priority = priority };
    return syscall(SYS_sched_setparam, tid, &param);
}

/// 同时核对策略与参数，不把字段查询当作调度顺序的证明。
static int matches(pid_t tid, int policy, int priority)
{
    struct sched_param param = { .sched_priority = -1 };
    return syscall(SYS_sched_getscheduler, tid) == policy &&
           syscall(SYS_sched_getparam, tid, &param) == 0 &&
           param.sched_priority == priority;
}

/// 在单调时钟上读取纳秒值，供有界的非协作用户计算使用。
static int monotonic_ns(uint64_t *value)
{
    struct timespec now;
    if (clock_gettime(CLOCK_MONOTONIC, &now) != 0)
        return -1;
    *value = (uint64_t)now.tv_sec * 1000000000 + (uint64_t)now.tv_nsec;
    return 0;
}

/// 记录孩子实际执行的事件，数组越界直接令场景失败。
static int record(struct order *order, int event)
{
    int slot = atomic_fetch_add(&order->count, 1);
    if (slot < 0 || 8 <= slot)
        return -1;
    atomic_store(&order->events[slot], event);
    return 0;
}

/// 检查支持的策略范围、四字节布局和错误优先级，坏用户指针保留 EFAULT。
static int abi_errors(void)
{
    int result = -1;
    int param[2] = { -1, 0x12345678 };
    ST_CHECK(matches(0, SCHED_OTHER, 0));
    ST_CHECK(syscall(SYS_sched_get_priority_min, SCHED_OTHER) == 0);
    ST_CHECK(syscall(SYS_sched_get_priority_max, SCHED_OTHER) == 0);
    ST_CHECK(syscall(SYS_sched_get_priority_min, SCHED_FIFO) == 1);
    ST_CHECK(syscall(SYS_sched_get_priority_max, SCHED_FIFO) == 99);
    ST_CHECK(syscall(SYS_sched_getparam, 0, &param) == 0);
    ST_CHECK(param[0] == 0 && param[1] == 0x12345678);
    ST_CHECK(set_priority(0, 0) == 0);
    errno = 0;
    ST_CHECK(set_policy(0, SCHED_FIFO, 0) == -1 && errno == EINVAL);
    errno = 0;
    ST_CHECK(set_policy(0, SCHED_FIFO, 100) == -1 && errno == EINVAL);
    errno = 0;
    ST_CHECK(set_policy(0, SCHED_FIFO, -1) == -1 && errno == EINVAL);
    errno = 0;
    ST_CHECK(set_priority(0, 1) == -1 && errno == EINVAL);
    errno = 0;
    ST_CHECK(set_policy(0, INT_MAX, 1) == -1 && errno == EINVAL);
    errno = 0;
    ST_CHECK(syscall(SYS_sched_get_priority_max, INT_MAX) == -1 && errno == EINVAL);
    errno = 0;
    ST_CHECK(syscall(SYS_sched_get_priority_min, SCHED_FIFO | SCHED_RESET_ON_FORK) == -1 && errno == EINVAL);
    errno = 0;
    ST_CHECK(syscall(SYS_sched_getscheduler, -1) == -1 && errno == EINVAL);
    errno = 0;
    ST_CHECK(syscall(SYS_sched_getscheduler, INT_MAX) == -1 && errno == ESRCH);
    errno = 0;
    ST_CHECK(syscall(SYS_sched_setparam, 0, NULL) == -1 && errno == EINVAL);
    errno = 0;
    ST_CHECK(syscall(SYS_sched_getparam, 0, NULL) == -1 && errno == EINVAL);
    errno = 0;
    ST_CHECK(syscall(SYS_sched_setscheduler, 0, -1, NULL) == -1 && errno == EINVAL);
    errno = 0;
    ST_CHECK(syscall(SYS_sched_setparam, -1, (void *)(uintptr_t)1) == -1 && errno == EINVAL);
    errno = 0;
    ST_CHECK(syscall(SYS_sched_setparam, 0, (void *)(uintptr_t)1) == -1 && errno == EFAULT);
    errno = 0;
    ST_CHECK(syscall(SYS_sched_getparam, 0, (void *)(uintptr_t)1) == -1 && errno == EFAULT);
    ST_CHECK(matches(0, SCHED_OTHER, 0));
    result = 0;
out:
    return result;
}

/// 探测场景实际需要的 FIFO 优先级；仅合法调用的 EPERM 返回跳过，其余失败均保留为错误。
static int fifo_admission(int priority)
{
    int result = -1;
    ST_CHECK(matches(0, SCHED_OTHER, 0));
    errno = 0;
    int admitted = set_policy(0, SCHED_FIFO, priority);
    if (admitted != 0) {
        ST_CHECK(admitted == -1 && errno == EPERM);
        ST_CHECK(matches(0, SCHED_OTHER, 0));
        result = 1;
    } else {
        ST_CHECK(matches(0, SCHED_FIFO, priority));
        result = 0;
    }
out:
    if (set_policy(0, SCHED_OTHER, 0) != 0 || !matches(0, SCHED_OTHER, 0))
        result = -1;
    return result;
}

/// 在隔离子进程验证 reset 标志；允许清除时核对结果，拒绝时只接受 EPERM 且标志保持不变。
static int reset_permissions(void)
{
    int result = -1;
    ST_CHECK(set_policy(0, SCHED_OTHER | SCHED_RESET_ON_FORK, 0) == 0);
    ST_CHECK(set_priority(0, 0) == 0);
    ST_CHECK(matches(0, SCHED_OTHER | SCHED_RESET_ON_FORK, 0));
    errno = 0;
    int cleared = set_policy(0, SCHED_OTHER, 0);
    if (cleared == -1) {
        ST_CHECK(errno == EPERM);
        ST_CHECK(matches(0, SCHED_OTHER | SCHED_RESET_ON_FORK, 0));
    } else {
        ST_CHECK(cleared == 0 && matches(0, SCHED_OTHER, 0));
    }
    result = 0;
out:
    if (set_policy(0, SCHED_OTHER | SCHED_RESET_ON_FORK, 0) != 0 ||
        !matches(0, SCHED_OTHER | SCHED_RESET_ON_FORK, 0))
        result = -1;
    return result;
}

/// 在已确认可用优先级 99 后检查实时参数升降及普通策略恢复。
static int permissions(void)
{
    int result = -1;
    ST_CHECK(set_policy(0, SCHED_FIFO, 1) == 0 && matches(0, SCHED_FIFO, 1));
    ST_CHECK(set_priority(0, 99) == 0 && matches(0, SCHED_FIFO, 99));
    ST_CHECK(set_priority(0, 1) == 0 && matches(0, SCHED_FIFO, 1));
    ST_CHECK(set_policy(0, SCHED_OTHER, 0) == 0 && matches(0, SCHED_OTHER, 0));
    result = 0;
out:
    if (set_policy(0, SCHED_OTHER, 0) != 0 || !matches(0, SCHED_OTHER, 0))
        result = -1;
    return result;
}

/// 独立运行 reset 权限检查并真实回收子进程，不依赖任何 FIFO 优先级准入。
static int run_reset_permissions(void)
{
    int result = -1;
    pid_t child = -1;
    ST_CHECK(matches(0, SCHED_OTHER, 0));
    child = fork();
    ST_CHECK(0 <= child);
    if (child == 0)
        _exit(reset_permissions() == 0 ? 0 : 1);
    ST_CHECK(st_reap(&child, 0) == 0);
    ST_CHECK(matches(0, SCHED_OTHER, 0));
    result = 0;
out:
    if (st_stop_child(&child) != 0)
        result = -1;
    return result;
}

/// pthread 的默认继承属性必须与原始 clone 得到的实际调度状态一致。
static void *inherited_thread(void *reset)
{
    int cleared = *(int *)reset;
    return matches(0, cleared ? SCHED_OTHER : SCHED_FIFO, cleared ? 0 : 24)
        ? NULL : (void *)1;
}

/// 在隔离子进程核对 fork、pthread、reset 和 exec；恢复普通策略时保留 reset，避免额外要求清除标志的权限。
static int inheritance(const char *program)
{
    int result = -1, started = 0, reset = 0;
    pid_t child = -1;
    pthread_t thread;
    void *status;
    ST_CHECK(set_policy(0, SCHED_FIFO, 24) == 0);
    ST_CHECK(pthread_create(&thread, NULL, inherited_thread, &reset) == 0);
    started = 1;
    ST_CHECK(pthread_join(thread, &status) == 0);
    started = 0;
    ST_CHECK(status == NULL);
    child = fork();
    ST_CHECK(0 <= child);
    if (child == 0)
        _exit(matches(0, SCHED_FIFO, 24) ? 0 : 1);
    ST_CHECK(st_reap(&child, 0) == 0);
    ST_CHECK(set_policy(0, SCHED_FIFO | SCHED_RESET_ON_FORK, 31) == 0);
    ST_CHECK(set_priority(0, 32) == 0);
    ST_CHECK(matches(0, SCHED_FIFO | SCHED_RESET_ON_FORK, 32));
    child = fork();
    ST_CHECK(0 <= child);
    if (child == 0)
        _exit(matches(0, SCHED_OTHER, 0) ? 0 : 1);
    ST_CHECK(st_reap(&child, 0) == 0);
    ST_CHECK(matches(0, SCHED_FIFO | SCHED_RESET_ON_FORK, 32));
    reset = 1;
    ST_CHECK(pthread_create(&thread, NULL, inherited_thread, &reset) == 0);
    started = 1;
    ST_CHECK(pthread_join(thread, &status) == 0);
    started = 0;
    ST_CHECK(status == NULL);
    child = fork();
    ST_CHECK(0 <= child);
    if (child == 0) {
        if (set_policy(0, SCHED_FIFO | SCHED_RESET_ON_FORK, 24) != 0)
            _exit(1);
        execl(program, program, "--verify-exec", (char *)NULL);
        _exit(1);
    }
    ST_CHECK(st_reap(&child, 0) == 0);
    result = 0;
out:
    if (set_policy(0, SCHED_OTHER | SCHED_RESET_ON_FORK, 0) != 0 ||
        !matches(0, SCHED_OTHER | SCHED_RESET_ON_FORK, 0))
        result = -1;
    if (started && pthread_join(thread, &status) != 0)
        result = -1;
    if (st_stop_child(&child) != 0)
        result = -1;
    return result;
}

/// 真实等待并回收继承场景的整个子进程，父进程始终保持不带 reset 的普通策略。
static int run_inheritance(const char *program)
{
    int result = -1;
    pid_t child = -1;
    ST_CHECK(matches(0, SCHED_OTHER, 0));
    child = fork();
    ST_CHECK(0 <= child);
    if (child == 0)
        _exit(inheritance(program) == 0 ? 0 : 1);
    ST_CHECK(st_reap(&child, 0) == 0);
    ST_CHECK(matches(0, SCHED_OTHER, 0));
    result = 0;
out:
    if (st_stop_child(&child) != 0)
        result = -1;
    return result;
}

/// 真正执行五个消费者：降优先级到同级队首、升优先级到同级队尾、不变参数保持位置。
static int priority_order(void)
{
    int result = -1;
    pid_t children[5] = { -1, -1, -1, -1, -1 };
    const int priorities[] = { 20, 20, 10, 30, 10 };
    const int expected[] = { 4, 1, 2, 3, 5 };
    struct order *order = mmap(NULL, 4096, PROT_READ | PROT_WRITE,
                               MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    ST_CHECK(order != MAP_FAILED);
    memset(order, 0, sizeof(*order));
    atomic_init(&order->count, 0);
    for (int index = 0; index < 8; ++index)
        atomic_init(&order->events[index], 0);
    ST_CHECK(record(order, 0) == 0);
    atomic_store(&order->count, 0);
    ST_CHECK(set_policy(0, SCHED_FIFO, 90) == 0);
    for (int index = 0; index < 5; ++index) {
        children[index] = fork();
        ST_CHECK(0 <= children[index]);
        if (children[index] == 0)
            _exit(record(order, index + 1) == 0 ? 0 : 1);
        ST_CHECK(set_policy(children[index], SCHED_OTHER, 0) == 0);
        ST_CHECK(set_policy(children[index], SCHED_FIFO, priorities[index]) == 0);
    }
    ST_CHECK(atomic_load(&order->count) == 0);
    ST_CHECK(set_priority(children[2], 20) == 0);
    ST_CHECK(set_priority(children[3], 20) == 0);
    ST_CHECK(set_priority(children[0], 20) == 0);
    ST_CHECK(matches(children[2], SCHED_FIFO, 20));
    ST_CHECK(set_policy(0, SCHED_OTHER, 0) == 0);
    for (int index = 0; index < 5; ++index)
        ST_CHECK(st_reap(&children[index], 0) == 0);
    ST_CHECK(atomic_load(&order->count) == 5);
    for (int index = 0; index < 5; ++index)
        ST_CHECK(atomic_load(&order->events[index]) == expected[index]);
    result = 0;
out:
    if (set_policy(0, SCHED_OTHER, 0) != 0)
        result = -1;
    for (int index = 0; index < 5; ++index)
        if (st_stop_child(&children[index]) != 0)
            result = -1;
    if (order != MAP_FAILED && munmap(order, 4096) != 0)
        result = -1;
    return result;
}

/// 高优先级消费者先等待管道，再等待真实定时器；两次恢复都必须抢占仍可运行的低优先级任务。
static int high_consumer(struct order *order, int reader)
{
    unsigned char byte;
    struct timespec delay = { .tv_sec = 0, .tv_nsec = 10000000 };
    if (st_read_all(reader, &byte, 1) != 0 || byte != 'G' || record(order, 2) != 0)
        return 1;
    while (nanosleep(&delay, &delay) != 0)
        if (errno != EINTR)
            return 1;
    return record(order, 3) == 0 ? 0 : 1;
}

/// 用户计算中不调用 yield 或睡眠；高优先级任务完成后，同级 peer 必须等到显式 yield。
static int low_consumer(struct order *order, int writer, int fifo)
{
    uint64_t now, end;
    if (record(order, 1) != 0 || st_write_all(writer, "G", 1) != 0 ||
        monotonic_ns(&now) != 0)
        return 1;
    end = now + 50000000;
    do {
        if (monotonic_ns(&now) != 0)
            return 1;
    } while (now < end);
    if (record(order, 4) != 0)
        return 1;
    if (fifo && (sched_yield() != 0 || record(order, 6) != 0))
        return 1;
    return 0;
}

/// 分别对 OTHER 和 FIFO 的运行者验证真实抢占；FIFO 还必须保留被抢占前的同级顺序。
static int preemption(int fifo)
{
    int result = -1, channel[2] = { -1, -1 };
    pid_t children[3] = { -1, -1, -1 };
    const int expected[] = { 1, 2, 3, 4, 5, 6, 7 };
    struct order *order = mmap(NULL, 4096, PROT_READ | PROT_WRITE,
                               MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    ST_CHECK(order != MAP_FAILED);
    memset(order, 0, sizeof(*order));
    atomic_init(&order->count, 0);
    for (int index = 0; index < 8; ++index)
        atomic_init(&order->events[index], 0);
    ST_CHECK(pipe(channel) == 0);
    ST_CHECK(set_policy(0, SCHED_FIFO, 90) == 0);
    children[0] = fork();
    ST_CHECK(0 <= children[0]);
    if (children[0] == 0) {
        if (close(channel[1]) != 0)
            _exit(1);
        _exit(high_consumer(order, channel[0]));
    }
    ST_CHECK(set_policy(children[0], SCHED_FIFO, 60) == 0);
    children[1] = fork();
    ST_CHECK(0 <= children[1]);
    if (children[1] == 0) {
        if (close(channel[0]) != 0)
            _exit(1);
        _exit(low_consumer(order, channel[1], fifo));
    }
    ST_CHECK(set_policy(children[1], fifo ? SCHED_FIFO : SCHED_OTHER, fifo ? 40 : 0) == 0);
    if (fifo) {
        children[2] = fork();
        ST_CHECK(0 <= children[2]);
        if (children[2] == 0) {
            if (close(channel[0]) != 0 || close(channel[1]) != 0 ||
                record(order, 5) != 0 || sched_yield() != 0 || record(order, 7) != 0)
                _exit(1);
            _exit(0);
        }
        ST_CHECK(set_policy(children[2], SCHED_OTHER, 0) == 0);
        ST_CHECK(set_policy(children[2], SCHED_FIFO, 40) == 0);
    }
    ST_CHECK(st_close(&channel[0]) == 0 && st_close(&channel[1]) == 0);
    ST_CHECK(set_policy(0, SCHED_OTHER, 0) == 0);
    ST_CHECK(st_reap(&children[0], 0) == 0 && st_reap(&children[1], 0) == 0);
    if (fifo)
        ST_CHECK(st_reap(&children[2], 0) == 0);
    int count = fifo ? 7 : 4;
    ST_CHECK(atomic_load(&order->count) == count);
    for (int index = 0; index < count; ++index)
        ST_CHECK(atomic_load(&order->events[index]) == expected[index]);
    result = 0;
out:
    if (set_policy(0, SCHED_OTHER, 0) != 0)
        result = -1;
    if (st_close(&channel[0]) != 0)
        result = -1;
    if (st_close(&channel[1]) != 0)
        result = -1;
    for (int index = 0; index < 3; ++index)
        if (st_stop_child(&children[index]) != 0)
            result = -1;
    if (order != MAP_FAILED && munmap(order, 4096) != 0)
        result = -1;
    return result;
}

/// 两核抢占场景的共享状态，阶段标志只协调用户代码，不触发内核唤醒。
struct takeover {
    /// 0 表示准备期，1 表示高优先级执行者正在占用原 CPU，2 表示测试结束。
    atomic_int phase;
    /// 被抢占任务恢复后发布：1 表示已迁至另一 CPU，-1 表示恢复位置错误。
    atomic_int migrated;
};

/// 低优先级 FIFO 任务持续占用第二个 CPU，直到父进程完成观察或到达真实时限。
static int hold_second_cpu(struct takeover *state, int writer, unsigned second)
{
    unsigned observed;
    uint64_t now, end;
    if (!matches(0, SCHED_FIFO, 10) ||
        syscall(SYS_getcpu, &observed, NULL, NULL) != 0 || observed != second ||
        st_write_all(writer, "R", 1) != 0 || monotonic_ns(&now) != 0)
        return 1;
    end = now + 5000000000;
    while (atomic_load(&state->phase) == 0 && now < end)
        if (monotonic_ns(&now) != 0)
            return 1;
    if (atomic_load(&state->phase) == 0)
        return 1;
    end = now + 1000000000;
    while (atomic_load(&state->phase) == 1 && now < end)
        if (monotonic_ns(&now) != 0)
            return 1;
    return atomic_load(&state->phase) == 2 ? 0 : 1;
}

/// 在原 CPU 运行后扩展许可，再唤醒更高优先级的父任务；写返回时必须已抢占另一核的低优先级任务。
static int displaced_consumer(
    struct takeover *state, int writer, const unsigned char *both, size_t length,
    unsigned first, unsigned second)
{
    unsigned observed;
    if (!matches(0, SCHED_FIFO, 80) ||
        syscall(SYS_getcpu, &observed, NULL, NULL) != 0 || observed != first ||
        syscall(SYS_sched_setaffinity, 0, length, both) != 0 ||
        st_write_all(writer, "R", 1) != 0)
        return 1;
    int migrated = syscall(SYS_getcpu, &observed, NULL, NULL) == 0 && observed == second;
    atomic_store(&state->migrated, migrated ? 1 : -1);
    return migrated ? 0 : 1;
}

/// 父任务 B90 占用原 CPU 时，A80 必须抢占另一允许 CPU 上的 C10，不能等待 B90 主动阻塞。
static int displaced_fifo(void)
{
    int result = -1, ready[2] = { -1, -1 }, running[2] = { -1, -1 };
    pid_t children[2] = { -1, -1 };
    unsigned char original[1024], available[1024], both[1024] = { 0 };
    struct takeover *state = MAP_FAILED;
    long length = syscall(SYS_sched_getaffinity, 0, sizeof(original), original);
    ST_CHECK(0 < length && (size_t)length <= sizeof(original));
    memset(available, 0xff, (size_t)length);
    ST_CHECK(syscall(SYS_sched_setaffinity, 0, length, available) == 0);
    ST_CHECK(syscall(SYS_sched_getaffinity, 0, sizeof(available), available) == length);
    unsigned first, second;
    for (first = 0; first < (size_t)length * 8; ++first)
        if ((original[first / 8] & (1u << (first % 8))) != 0)
            break;
    for (second = 0; second < (size_t)length * 8; ++second)
        if (second != first && (available[second / 8] & (1u << (second % 8))) != 0)
            break;
    ST_CHECK(first < (size_t)length * 8);
    if ((size_t)length * 8 <= second) {
        result = 1;
        goto out;
    }
    ST_CHECK(syscall(SYS_sched_setaffinity, 0, length, original) == 0);
    both[first / 8] |= (unsigned char)(1u << (first % 8));
    both[second / 8] |= (unsigned char)(1u << (second % 8));
    memset(available, 0, (size_t)length);
    available[second / 8] = (unsigned char)(1u << (second % 8));
    state = mmap(NULL, 4096, PROT_READ | PROT_WRITE,
                 MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    ST_CHECK(state != MAP_FAILED);
    atomic_init(&state->phase, 0);
    atomic_init(&state->migrated, 0);
    ST_CHECK(set_policy(0, SCHED_FIFO, 90) == 0);
    ST_CHECK(pipe(ready) == 0);
    children[0] = fork();
    ST_CHECK(0 <= children[0]);
    if (children[0] == 0) {
        if (close(ready[0]) != 0)
            _exit(1);
        _exit(hold_second_cpu(state, ready[1], second));
    }
    ST_CHECK(set_policy(children[0], SCHED_FIFO, 10) == 0);
    ST_CHECK(syscall(SYS_sched_setaffinity, children[0], length, available) == 0);
    ST_CHECK(st_close(&ready[1]) == 0);
    unsigned char byte;
    ST_CHECK(st_read_all(ready[0], &byte, 1) == 0 && byte == 'R');
    ST_CHECK(st_close(&ready[0]) == 0);
    ST_CHECK(pipe(running) == 0);
    children[1] = fork();
    ST_CHECK(0 <= children[1]);
    if (children[1] == 0) {
        if (close(running[0]) != 0)
            _exit(1);
        _exit(displaced_consumer(state, running[1], both, (size_t)length, first, second));
    }
    ST_CHECK(set_policy(children[1], SCHED_FIFO, 80) == 0);
    ST_CHECK(st_close(&running[1]) == 0);
    ST_CHECK(st_read_all(running[0], &byte, 1) == 0 && byte == 'R');
    atomic_store(&state->phase, 1);
    uint64_t now, end;
    ST_CHECK(monotonic_ns(&now) == 0);
    end = now + 200000000;
    while (atomic_load(&state->migrated) == 0 && now < end)
        ST_CHECK(monotonic_ns(&now) == 0);
    ST_CHECK(atomic_load(&state->migrated) == 1);
    atomic_store(&state->phase, 2);
    ST_CHECK(set_policy(0, SCHED_OTHER, 0) == 0);
    ST_CHECK(st_reap(&children[0], 0) == 0 && st_reap(&children[1], 0) == 0);
    result = 0;
out:
    if (state != MAP_FAILED)
        atomic_store(&state->phase, 2);
    if (set_policy(0, SCHED_OTHER, 0) != 0)
        result = -1;
    for (int index = 0; index < 2; ++index) {
        if (st_close(&ready[index]) != 0)
            result = -1;
        if (st_close(&running[index]) != 0)
            result = -1;
        if (st_stop_child(&children[index]) != 0)
            result = -1;
    }
    if (state != MAP_FAILED && munmap(state, 4096) != 0)
        result = -1;
    if (0 < length && (size_t)length <= sizeof(original) &&
        syscall(SYS_sched_setaffinity, 0, length, original) != 0)
        result = -1;
    return result;
}

/// 同源消费者按真实 FIFO 准入分别选择各优先级场景，不从 UID 推断权限。
int main(int argc, char **argv)
{
    if (argc == 2 && strcmp(argv[1], "--verify-exec") == 0)
        return matches(0, SCHED_FIFO | SCHED_RESET_ON_FORK, 24) ? 0 : 1;
    ksft_print_header();
    ksft_set_plan(8);
    unsigned char mask[1024] = { 0 };
    long bytes = syscall(SYS_sched_getaffinity, 0, sizeof(mask), mask);
    if (bytes <= 0 || sizeof(mask) < (size_t)bytes)
        ksft_exit_fail_msg("initial affinity errno=%d\n", errno);
    unsigned cpu;
    for (cpu = 0; cpu < (size_t)bytes * 8; ++cpu)
        if ((mask[cpu / 8] & (1u << (cpu % 8))) != 0)
            break;
    if ((size_t)bytes * 8 <= cpu)
        ksft_exit_fail_msg("no allowed CPU\n");
    memset(mask, 0, (size_t)bytes);
    mask[cpu / 8] = (unsigned char)(1u << (cpu % 8));
    if (syscall(SYS_sched_setaffinity, 0, bytes, mask) != 0)
        ksft_exit_fail_msg("pin CPU errno=%d\n", errno);
    char *program = realpath(argv[0], NULL);
    if (program == NULL)
        ksft_exit_fail_msg("realpath executable errno=%d\n", errno);
    ksft_test_result(abi_errors() == 0, "scheduler ABI, priority bounds and real errors\n");
    int admission = fifo_admission(99);
    if (admission == 1)
        ksft_test_result_skip("FIFO priority 99 admission denied with EPERM\n");
    else
        ksft_test_result(admission == 0 && permissions() == 0, "scheduler realtime priority changes\n");
    ksft_test_result(run_reset_permissions() == 0, "reset-on-fork flag and clear permission\n");
    admission = fifo_admission(32);
    if (admission == 1)
        ksft_test_result_skip("FIFO priority 32 admission denied with EPERM\n");
    else
        ksft_test_result(admission == 0 && run_inheritance(program) == 0, "fork, pthread clone, reset and exec inheritance\n");
    admission = fifo_admission(90);
    if (admission == 1) {
        for (int index = 0; index < 4; ++index)
            ksft_test_result_skip("FIFO priority 90 admission denied with EPERM\n");
    } else {
        ksft_test_result(admission == 0 && priority_order() == 0, "real priority order and parameter requeue positions\n");
        ksft_test_result(admission == 0 && preemption(0) == 0, "FIFO pipe and timer wakeups preempt an OTHER consumer\n");
        ksft_test_result(admission == 0 && preemption(1) == 0, "FIFO preemption preserves order and equal-priority yield rotates\n");
        int migration = admission == 0 ? displaced_fifo() : -1;
        if (migration == 1)
            ksft_test_result_skip("displaced FIFO migration requires two allowed CPUs\n");
        else
            ksft_test_result(migration == 0, "preempted FIFO task displaces a lower-priority remote consumer\n");
    }
    free(program);
    ksft_finished();
}
