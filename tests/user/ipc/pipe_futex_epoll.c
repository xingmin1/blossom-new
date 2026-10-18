#define _GNU_SOURCE
#include <fcntl.h>
#include <linux/futex.h>
#include <pthread.h>
#include <sched.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/syscall.h>
#include <time.h>
#include "kselftest.h"
#include "selftest_io.h"

/// fork writer 的跨页内容按真实短读消费；关闭最后写端后才能取得 EOF。
static int pipe_stream(void)
{
    unsigned char input[8193], output[113], extra;
    int result = -1, channel[2] = { -1, -1 };
    pid_t child = -1;
    for (size_t i = 0; i < sizeof(input); ++i)
        input[i] = (unsigned char)(i * 37 + i / 251 + 19);
    ST_CHECK(pipe(channel) == 0);
    child = fork();
    ST_CHECK(0 <= child);
    if (child == 0) {
        int ok = close(channel[0]) == 0 &&
                 st_write_all(channel[1], input, sizeof(input)) == 0 &&
                 close(channel[1]) == 0;
        _exit(ok ? 0 : 1);
    }
    ST_CHECK(st_close(&channel[1]) == 0);
    size_t done = 0;
    while (done < sizeof(input)) {
        size_t length = sizeof(input) - done;
        if (sizeof(output) < length)
            length = sizeof(output);
        ssize_t count = read(channel[0], output, length);
        if (count == -1 && errno == EINTR)
            continue;
        ST_CHECK(0 < count && (size_t)count <= length);
        ST_CHECK(memcmp(output, input + done, (size_t)count) == 0);
        done += (size_t)count;
    }
    ST_CHECK(read(channel[0], &extra, 1) == 0);
    ST_CHECK(st_reap(&child, 0) == 0);
    errno = 0;
    ST_CHECK(write(channel[0], &extra, 1) == -1 && errno == EBADF);
    result = 0;
out:
    if (st_stop_child(&child) != 0)
        result = -1;
    if (st_close(&channel[0]) != 0)
        result = -1;
    if (st_close(&channel[1]) != 0)
        result = -1;
    return result;
}

/// 无数据的非阻塞 pipe 返回 EAGAIN，关闭读端后的 EPIPE 使用真实 libc errno。
static int pipe_errors(void)
{
    int result = -1, channel[2] = { -1, -1 }, installed = 0;
    struct sigaction previous, ignore = { .sa_handler = SIG_IGN };
    unsigned char byte;
    ST_CHECK(sigemptyset(&ignore.sa_mask) == 0);
    ST_CHECK(sigaction(SIGPIPE, &ignore, &previous) == 0);
    installed = 1;
    ST_CHECK(pipe2(channel, O_NONBLOCK | O_CLOEXEC) == 0);
    ST_CHECK(fcntl(channel[0], F_GETFD) == FD_CLOEXEC);
    ST_CHECK(read(channel[0], &byte, 0) == 0);
    errno = 0;
    ST_CHECK(read(channel[0], &byte, 1) == -1 && errno == EAGAIN);
    ST_CHECK(st_close(&channel[0]) == 0);
    errno = 0;
    ST_CHECK(write(channel[1], "x", 1) == -1 && errno == EPIPE);
    result = 0;
out:
    if (st_close(&channel[0]) != 0)
        result = -1;
    if (st_close(&channel[1]) != 0)
        result = -1;
    if (installed && sigaction(SIGPIPE, &previous, NULL) != 0)
        result = -1;
    return result;
}

/// 等待字始终保持零；只有真正从 FUTEX_WAIT 被唤醒才向 join 返回成功。
static void *futex_waiter(void *word)
{
    struct timespec timeout = { .tv_sec = 5 };
    long result = syscall(SYS_futex, word, FUTEX_WAIT_PRIVATE, 0, &timeout, NULL, 0);
    if (result != 0)
        fprintf(stderr, "futex waiter result=%ld errno=%d\n", result, errno);
    return (void *)(intptr_t)(result != 0);
}

/// WAKE 返回1证明有实际登记的 pthread waiter，不能以延迟或 WAIT 超时推断唤醒。
static int futex_wake(void)
{
    int result = -1, word = 0, started = 0;
    pthread_t thread;
    struct timespec start, now;
    void *thread_result = (void *)1;
    ST_CHECK(clock_gettime(CLOCK_MONOTONIC, &start) == 0);
    int error = pthread_create(&thread, NULL, futex_waiter, &word);
    errno = error;
    ST_CHECK(error == 0);
    started = 1;
    for (;;) {
        long woken = syscall(SYS_futex, &word, FUTEX_WAKE_PRIVATE, 1, NULL, NULL, 0);
        ST_CHECK(0 <= woken && woken <= 1);
        if (woken == 1)
            break;
        ST_CHECK(clock_gettime(CLOCK_MONOTONIC, &now) == 0);
        ST_CHECK(now.tv_sec < start.tv_sec + 5);
        ST_CHECK(sched_yield() == 0);
    }
    error = pthread_join(thread, &thread_result);
    errno = error;
    ST_CHECK(error == 0);
    started = 0;
    ST_CHECK(thread_result == NULL && word == 0);
    result = 0;
out:
    if (started) {
        /* WAIT 有自身有限期限，清理必须 join 后才能释放栈上的 futex 字。 */
        if (pthread_join(thread, &thread_result) != 0)
            result = -1;
    }
    return result;
}

/// 比较失败与有限超时分别核 EAGAIN、ETIMEDOUT，二者不能充当唤醒成功。
static int futex_errors(void)
{
    int result = -1, word = 1;
    struct timespec timeout = { .tv_nsec = 1000000 };
    errno = 0;
    ST_CHECK(syscall(SYS_futex, &word, FUTEX_WAIT_PRIVATE, 0, &timeout, NULL, 0) == -1 &&
             errno == EAGAIN);
    errno = 0;
    ST_CHECK(syscall(SYS_futex, &word, FUTEX_WAIT_PRIVATE, 1, &timeout, NULL, 0) == -1 &&
             errno == ETIMEDOUT);
    ST_CHECK(syscall(SYS_futex, &word, FUTEX_WAKE_PRIVATE, 1, NULL, NULL, 0) == 0);
    result = 0;
out:
    return result;
}

/// epoll 的就绪数据来自实际 pipe，消费后撤销 IN，最后写端关闭后交付 HUP 与 EOF。
static int epoll_consume(void)
{
    int result = -1, channel[2] = { -1, -1 }, epoll = -1;
    struct epoll_event interest = { .events = EPOLLIN, .data.u64 = 0x1234abcdefULL }, ready;
    unsigned char byte;
    ST_CHECK(pipe2(channel, O_NONBLOCK) == 0);
    epoll = epoll_create1(EPOLL_CLOEXEC);
    ST_CHECK(0 <= epoll);
    ST_CHECK(epoll_ctl(epoll, EPOLL_CTL_ADD, channel[0], &interest) == 0);
    errno = 0;
    ST_CHECK(epoll_ctl(epoll, EPOLL_CTL_ADD, channel[0], &interest) == -1 && errno == EEXIST);
    ST_CHECK(epoll_wait(epoll, &ready, 1, 0) == 0);
    ST_CHECK(write(channel[1], "E", 1) == 1);
    ST_CHECK(epoll_wait(epoll, &ready, 1, 5000) == 1);
    ST_CHECK((ready.events & EPOLLIN) != 0 && ready.data.u64 == interest.data.u64);
    ST_CHECK(read(channel[0], &byte, 1) == 1 && byte == 'E');
    errno = 0;
    ST_CHECK(read(channel[0], &byte, 1) == -1 && errno == EAGAIN);
    ST_CHECK(epoll_wait(epoll, &ready, 1, 0) == 0);
    ST_CHECK(st_close(&channel[1]) == 0);
    ST_CHECK(epoll_wait(epoll, &ready, 1, 5000) == 1);
    ST_CHECK((ready.events & EPOLLHUP) != 0 && ready.data.u64 == interest.data.u64);
    ST_CHECK(read(channel[0], &byte, 1) == 0);
    ST_CHECK(epoll_ctl(epoll, EPOLL_CTL_DEL, channel[0], NULL) == 0);
    ST_CHECK(epoll_wait(epoll, &ready, 1, 0) == 0);
    errno = 0;
    ST_CHECK(epoll_wait(epoll, &ready, 0, 0) == -1 && errno == EINVAL);
    errno = 0;
    ST_CHECK(epoll_ctl(epoll, EPOLL_CTL_ADD, -1, &interest) == -1 && errno == EBADF);
    result = 0;
out:
    if (st_close(&epoll) != 0)
        result = -1;
    if (st_close(&channel[0]) != 0)
        result = -1;
    if (st_close(&channel[1]) != 0)
        result = -1;
    return result;
}

/// 裸 ELF 自建 pipe、pthread 和 epoll，分别报告真实业务与错误分支的 TAP。
int main(int argc, char **argv)
{
    ksft_print_header();
    if (argc != 1)
        ksft_exit_fail_msg("usage: %s\n", argv[0]);
    ksft_set_plan(5);
    ksft_test_result(pipe_stream() == 0, "pipe cross-page short reads EOF and child cleanup\n");
    ksft_test_result(pipe_errors() == 0, "pipe nonblock zero-length CLOEXEC EBADF EPIPE\n");
    ksft_test_result(futex_wake() == 0, "pthread futex registered wait wake and join\n");
    ksft_test_result(futex_errors() == 0, "futex mismatch timeout and empty wake\n");
    ksft_test_result(epoll_consume() == 0, "epoll readiness consumption HUP EOF and errors\n");
    ksft_finished();
}
