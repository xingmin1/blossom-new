#define _GNU_SOURCE
#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/resource.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>
#include "kselftest.h"

#define CHECK(expression) do { if (!(expression)) { \
    fprintf(stderr, "# resource-limits: line=%d errno=%d\n", __LINE__, errno); \
    return 1; } } while (0)

/// 精确消费子身份的正常终态。
static int reaped(pid_t pid)
{
    int status;
    return waitpid(pid, &status, 0) == pid && WIFEXITED(status) && WEXITSTATUS(status) == 0;
}

/// 超过 512 个同时存在的真实身份，关闭同一管道后逐一等待全部执行尾部。
static int identity_growth(void)
{
    enum { CHILDREN = 520 };
    pid_t children[CHILDREN];
    int gate[2], count = 0, failed = 0;
    CHECK(pipe(gate) == 0);
    for (; count < CHILDREN; ++count) {
        pid_t child = fork();
        if (child < 0) { failed = 1; break; }
        if (child == 0) {
            char byte;
            close(gate[1]);
            _exit(read(gate[0], &byte, 1) == 0 ? 0 : 1);
        }
        children[count] = child;
    }
    close(gate[0]);
    close(gate[1]);
    pid_t largest = 0;
    for (int index = 0; index < count; ++index) {
        if (largest < children[index]) largest = children[index];
        if (!reaped(children[index])) failed = 1;
    }
    CHECK(!failed && count == CHILDREN && 512 < largest);
    pid_t reused = fork();
    CHECK(0 <= reused);
    if (reused == 0) _exit(0);
    CHECK(reused < largest && reaped(reused));
    return 0;
}

struct thread_gate {
    pthread_mutex_t mutex;
    pthread_cond_t changed;
    int ready, released, failed;
};

#define THREAD_GATE_INITIALIZER { \
    .mutex = PTHREAD_MUTEX_INITIALIZER, .changed = PTHREAD_COND_INITIALIZER }

struct thread_record {
    pthread_t thread;
    struct thread_gate *gate;
    long tid;
};

/// 在线程实际进入后发表 TID；记录和 gate 由 main 保留到全部 join 完成。
static void *held_thread(void *argument)
{
    struct thread_record *record = argument;
    struct thread_gate *gate = record->gate;
    pthread_mutex_lock(&gate->mutex);
    record->tid = syscall(SYS_gettid);
    ++gate->ready;
    pthread_cond_broadcast(&gate->changed);
    int error = 0;
    while (!gate->released && error == 0)
        error = pthread_cond_wait(&gate->changed, &gate->mutex);
    if (error != 0) {
        gate->failed = 1;
        pthread_cond_broadcast(&gate->changed);
    }
    pthread_mutex_unlock(&gate->mutex);
    return error == 0 ? record : NULL;
}

/// 只记录创建成功的线程，失败时由调用者释放全部 gate 并消费已创建者。
static int start_thread_batch(struct thread_record *records, int count,
                              struct thread_gate *gate, int *created)
{
    for (int index = 0; index < count; ++index) {
        records[index].gate = gate;
        int error = pthread_create(&records[index].thread, NULL, held_thread, &records[index]);
        if (error != 0) {
            fprintf(stderr, "# resource-limits: pthread_create error=%d created=%d\n",
                    error, *created);
            return 1;
        }
        ++*created;
    }
    return 0;
}

/// 在同一 mutex 下等待实际进入数，成功返回后未释放的线程仍由 gate 持有。
static int wait_thread_gate(struct thread_gate *gate, int count)
{
    pthread_mutex_lock(&gate->mutex);
    while (gate->ready < count && !gate->failed) {
        if (pthread_cond_wait(&gate->changed, &gate->mutex) != 0)
            gate->failed = 1;
    }
    int failed = gate->failed || gate->released || gate->ready != count;
    pthread_mutex_unlock(&gate->mutex);
    return failed;
}

/// 广播释放已进入和稍后进入的线程，失败清理也使用同一状态转换。
static void release_thread_gate(struct thread_gate *gate)
{
    pthread_mutex_lock(&gate->mutex);
    gate->released = 1;
    pthread_cond_broadcast(&gate->changed);
    pthread_mutex_unlock(&gate->mutex);
}

/// 全部 gate 释放后逐一检查真实 join 与线程返回值，不因首个失败跳过后继。
static int join_thread_batch(struct thread_record *records, int count)
{
    int failed = 0;
    for (int index = 0; index < count; ++index) {
        void *result = NULL;
        int error = pthread_join(records[index].thread, &result);
        if (error != 0 || result != &records[index]) {
            fprintf(stderr, "# resource-limits: pthread_join error=%d index=%d\n", error, index);
            failed = 1;
        }
    }
    return failed;
}

/// 只消费已由 gate 同步发表或已 join 的记录；非正或重复 TID 返回失败值。
static long largest_thread_id(const struct thread_record *records, int count)
{
    long largest = 0;
    for (int index = 0; index < count; ++index) {
        if (records[index].tid <= 0) return 0;
        for (int other = 0; other < index; ++other)
            if (records[index].tid == records[other].tid) return 0;
        if (largest < records[index].tid) largest = records[index].tid;
    }
    return largest;
}

/// 两批真实存活线程跨容量增长；释放首批一部分后，由 main 分配新线程并完整回收。
static int thread_growth(void)
{
    enum { RETIRING = 128, HELD = 392, FIRST = RETIRING + HELD, GROWTH = 260, REUSE = 4 };
    struct thread_gate retiring = THREAD_GATE_INITIALIZER;
    struct thread_gate held = THREAD_GATE_INITIALIZER;
    struct thread_record records[FIRST + GROWTH], reused[REUSE];
    int created = 0, failed = 1;
    if (start_thread_batch(records, RETIRING, &retiring, &created) != 0 ||
        start_thread_batch(records + RETIRING, HELD, &held, &created) != 0 ||
        wait_thread_gate(&retiring, RETIRING) != 0 ||
        wait_thread_gate(&held, HELD) != 0 ||
        !(512 < largest_thread_id(records, FIRST)))
        goto release;

    // 退出与新建走正常并发调度；先保留 392 个线程，不等待退休批 join 再申请新身份。
    release_thread_gate(&retiring);
    if (start_thread_batch(records + FIRST, GROWTH, &held, &created) != 0 ||
        wait_thread_gate(&held, HELD + GROWTH) != 0 ||
        !(512 < largest_thread_id(records + RETIRING, HELD + GROWTH)))
        goto release;
    failed = 0;

release:
    release_thread_gate(&retiring);
    release_thread_gate(&held);
    failed |= join_thread_batch(records, created);
    if (!failed) {
        // 旧记录在全部 join 后保持不变，用新线程的实际 TID 核对号码复用。
        held.ready = held.released = held.failed = 0;
        created = 0;
        failed = start_thread_batch(reused, REUSE, &held, &created);
        if (!failed) failed = wait_thread_gate(&held, REUSE);
        if (!failed && largest_thread_id(reused, REUSE) == 0) failed = 1;
        if (!failed) {
            for (int index = 0; index < REUSE; ++index) {
                int found = 0;
                for (int old = 0; old < FIRST + GROWTH; ++old)
                    if (reused[index].tid == records[old].tid) found = 1;
                if (!found) failed = 1;
            }
        }
        release_thread_gate(&held);
        failed |= join_thread_batch(reused, created);
    }
    failed |= pthread_cond_destroy(&retiring.changed) != 0;
    failed |= pthread_mutex_destroy(&retiring.mutex) != 0;
    failed |= pthread_cond_destroy(&held.changed) != 0;
    failed |= pthread_mutex_destroy(&held.mutex) != 0;
    CHECK(!failed);
    return 0;
}

/// 线程创建失败必须由真实 NPROC 计费决定。
static void *unused_thread(void *unused)
{
    return unused;
}

/// 非 root 的待 reap leader 继续占额，回收后释放；root 初始用户只豁免 NPROC。
static int nproc_limit(void)
{
    struct rlimit original, limit;
    CHECK(getrlimit(RLIMIT_NPROC, &original) == 0);
    limit = original;
    limit.rlim_cur = getuid() == 0 ? 0 : 2;
    CHECK(setrlimit(RLIMIT_NPROC, &limit) == 0);
    pid_t child = fork();
    CHECK(0 <= child);
    if (child == 0) {
        struct rlimit inherited;
        _exit(getrlimit(RLIMIT_NPROC, &inherited) == 0 &&
              inherited.rlim_cur == limit.rlim_cur && inherited.rlim_max == limit.rlim_max ? 0 : 1);
    }
    siginfo_t info;
    CHECK(waitid(P_PID, child, &info, WEXITED | WNOWAIT) == 0 && info.si_pid == child);
    if (getuid() != 0) {
        errno = 0;
        CHECK(fork() == -1 && errno == EAGAIN);
        pthread_t thread;
        CHECK(pthread_create(&thread, NULL, unused_thread, NULL) == EAGAIN);
    }
    CHECK(reaped(child));
    child = fork();
    CHECK(0 <= child);
    if (child == 0) _exit(0);
    CHECK(reaped(child));
    CHECK(setrlimit(RLIMIT_NPROC, &original) == 0);
    return 0;
}

/// 对两个真实 RT 号码分别核对每号 FIFO，并保内核 reserve 的同号插入次序。
static int realtime_fifo(void)
{
    const int signal = SIGRTMIN;
    sigset_t set, oldmask;
    sigemptyset(&set);
    sigaddset(&set, signal);
    sigaddset(&set, signal + 1);
    CHECK(pthread_sigmask(SIG_BLOCK, &set, &oldmask) == 0);
    struct rlimit original, limit;
    CHECK(getrlimit(RLIMIT_SIGPENDING, &original) == 0);
    limit = original;
    limit.rlim_cur = 256;
    CHECK(setrlimit(RLIMIT_SIGPENDING, &limit) == 0);
    for (int index = 0; index < 128; ++index)
        CHECK(sigqueue(getpid(), signal, (union sigval){.sival_int = index}) == 0);
    CHECK(kill(getpid(), signal) == 0);
    for (int index = 128; index < 256; ++index)
        CHECK(sigqueue(getpid(), signal, (union sigval){.sival_int = index}) == 0);
    errno = 0;
    CHECK(sigqueue(getpid(), signal, (union sigval){0}) == -1 && errno == EAGAIN);
    struct timespec zero = {0, 0};
    for (int index = 0; index < 257; ++index) {
        siginfo_t info;
        CHECK(sigtimedwait(&set, &info, &zero) == signal);
        if (index == 128) CHECK(info.si_code == SI_USER);
        else CHECK(info.si_code == SI_QUEUE && info.si_value.sival_int == (index < 128 ? index : index - 1));
    }
    for (int index = 0; index < 65; ++index) {
        CHECK(sigqueue(getpid(), signal + 1, (union sigval){.sival_int = index}) == 0);
        CHECK(sigqueue(getpid(), signal, (union sigval){.sival_int = index}) == 0);
    }
    for (int number = signal; number <= signal + 1; ++number) {
        for (int index = 0; index < 65; ++index) {
            siginfo_t info;
            CHECK(sigtimedwait(&set, &info, &zero) == number && info.si_value.sival_int == index);
        }
    }
    CHECK(setrlimit(RLIMIT_SIGPENDING, &original) == 0);
    CHECK(pthread_sigmask(SIG_SETMASK, &oldmask, NULL) == 0);
    return 0;
}

/// shared 和 private 队列都按目标 uid 聚合，真实退出必须在僵尸 reap 前退费。
static int queued_exit(int private_queue)
{
    const int signal = SIGRTMIN;
    sigset_t set, oldmask;
    sigemptyset(&set);
    sigaddset(&set, signal);
    CHECK(pthread_sigmask(SIG_BLOCK, &set, &oldmask) == 0);
    struct rlimit original, limit;
    CHECK(getrlimit(RLIMIT_SIGPENDING, &original) == 0);
    limit = original;
    limit.rlim_cur = 65;
    CHECK(setrlimit(RLIMIT_SIGPENDING, &limit) == 0);
    int gate[2];
    CHECK(pipe(gate) == 0);
    pid_t child = fork();
    CHECK(0 <= child);
    if (child == 0) {
        char byte;
        close(gate[1]);
        _exit(read(gate[0], &byte, 1) == 0 ? 0 : 1);
    }
    close(gate[0]);
    for (int index = 0; index < 65; ++index) {
        siginfo_t info = {.si_signo = signal, .si_code = SI_QUEUE};
        info.si_pid = getpid();
        info.si_uid = getuid();
        info.si_value.sival_int = index;
        if (private_queue) CHECK(syscall(SYS_rt_tgsigqueueinfo, child, child, signal, &info) == 0);
        else CHECK(sigqueue(child, signal, info.si_value) == 0);
    }
    errno = 0;
    CHECK(sigqueue(getpid(), signal, (union sigval){0}) == -1 && errno == EAGAIN);
    close(gate[1]);
    siginfo_t info;
    CHECK(waitid(P_PID, child, &info, WEXITED | WNOWAIT) == 0 && info.si_pid == child);
    CHECK(sigqueue(getpid(), signal, (union sigval){.sival_int = 99}) == 0);
    struct timespec zero = {0, 0};
    CHECK(sigtimedwait(&set, &info, &zero) == signal && info.si_value.sival_int == 99);
    CHECK(reaped(child));
    CHECK(setrlimit(RLIMIT_SIGPENDING, &original) == 0);
    CHECK(pthread_sigmask(SIG_SETMASK, &oldmask, NULL) == 0);
    return 0;
}

/// 按参数选择完整检查序列，线程增长仅显式执行，默认保留原进程增长和限额序列。
static int resource_checks(int argc, char **argv)
{
    if (argc == 2 && strcmp(argv[1], "--growth") == 0) return identity_growth();
    if (argc == 2 && strcmp(argv[1], "--threads") == 0) return thread_growth();
    struct rlimit nofile, stack;
    CHECK(getrlimit(RLIMIT_NOFILE, &nofile) == 0 && getrlimit(RLIMIT_STACK, &stack) == 0);
    CHECK(nofile.rlim_cur == 1024 && nofile.rlim_max == 4096);
    CHECK(stack.rlim_cur == 8 * 1024 * 1024 && stack.rlim_max == RLIM_INFINITY);
    struct rlimit invalid = {1048577, 1048577};
    errno = 0;
    CHECK(setrlimit(RLIMIT_NOFILE, &invalid) == -1 && errno == EPERM);
    struct rlimit next = nofile;
    next.rlim_cur = 63;
    errno = 0;
    CHECK(syscall(SYS_prlimit64, 0, RLIMIT_NOFILE, &next, (void *)1) == -1 && errno == EFAULT);
    struct rlimit actual;
    CHECK(getrlimit(RLIMIT_NOFILE, &actual) == 0 && actual.rlim_cur == 63);
    CHECK(setrlimit(RLIMIT_NOFILE, &nofile) == 0);
    CHECK(nproc_limit() == 0 && realtime_fifo() == 0 && queued_exit(0) == 0 && queued_exit(1) == 0);
    if (argc == 1) CHECK(identity_growth() == 0);
    puts("# resource-limits: identity/NPROC/RT quota and release passed");
    return 0;
}

/// 只有普通入口发布一次 TAP 汇总，子进程和线程仍沿原检查序列退出。
int main(int argc, char **argv)
{
    ksft_print_header();
    ksft_set_plan(1);
    int status = resource_checks(argc, argv);
    ksft_test_result(status == 0, "resource limits selected check sequence\n");
    return status;
}
