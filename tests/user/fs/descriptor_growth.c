#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/resource.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#include "kselftest.h"

#define CHECK(expression) do { if (!(expression)) { \
    fprintf(stderr, "# descriptor-growth: line=%d errno=%d\n", __LINE__, errno); \
    return 1; } } while (0)

/// 精确消费实际子进程终态，不把信号终止解释成普通退出。
static int exited(pid_t child)
{
    int status;
    return waitpid(child, &status, 0) == child && WIFEXITED(status) && WEXITSTATUS(status) == 0;
}

/// 共享同一表的独立线程组采用自己的额度，关闭和复用仍修改共同槽。
static int shared_files(void *argument)
{
    int writer = *(int *)argument;
    struct rlimit limit = {64, 8192};
    CHECK(setrlimit(RLIMIT_NOFILE, &limit) == 0);
    errno = 0;
    CHECK(dup(writer) == -1 && errno == EMFILE);
    CHECK(close(10) == 0 && dup(writer) == 10);
    CHECK(close(4096) == 0);
    return 0;
}

/// 原始 ppoll 真正阻塞后由内核夹具关闭并复用整数，仍须由原管道的事件完成。
static int poll_original(void)
{
    CHECK(write(4, "R", 1) == 1);
    struct pollfd descriptor = { .fd = 3, .events = POLLIN };
    struct timespec timeout = {5, 0};
    CHECK(syscall(SYS_ppoll, &descriptor, 1, &timeout, NULL, 8) == 1);
    CHECK(descriptor.revents == POLLIN);
    return 0;
}

/// 真实执行消费高号、额度、fork、CLONE_FILES、CLOEXEC 及动态 poll/select 输入。
static int descriptor_checks(const char *program)
{
    struct rlimit original, limit = {8192, 8192};
    CHECK(getrlimit(RLIMIT_NOFILE, &original) == 0);
    CHECK(setrlimit(RLIMIT_NOFILE, &limit) == 0);
    int pipefd[2];
    CHECK(pipe(pipefd) == 0 && pipefd[0] == 3 && pipefd[1] == 4);
    struct timespec zero = {0, 0};
    struct pollfd *pollfds = calloc(4097, sizeof(*pollfds));
    CHECK(pollfds != NULL);
    for (int index = 0; index < 4097; ++index)
        pollfds[index].fd = -1;
    pollfds[4096] = (struct pollfd){pipefd[1], POLLOUT, 0};
    CHECK(syscall(SYS_ppoll, pollfds, 4097, &zero, NULL, 8) == 1);
    CHECK(pollfds[4096].revents == POLLOUT);
    pollfds[4096].fd = -1;
    size_t span = ((4097 + 63) / 64) * 8;
    unsigned char *selected = calloc(1, span);
    CHECK(selected != NULL);
    selected[4096 / 8] = 1;
    zero = (struct timespec){0, 0};
    CHECK(syscall(SYS_pselect6, 4097, NULL, selected, NULL, &zero, NULL) == 0);
    CHECK(selected[4096 / 8] == 1);
    for (int fd = 5; fd <= 4096; ++fd)
        CHECK(dup(pipefd[1]) == fd);
    for (int fd = 1023; fd <= 1025; ++fd)
        CHECK(fcntl(fd, F_GETFD) == 0);
    CHECK(fcntl(4096, F_GETFD) == 0);
    CHECK(close(1024) == 0 && dup(pipefd[1]) == 1024);
    CHECK(fcntl(pipefd[1], F_DUPFD, 5000) == 5000);
    CHECK(close(5000) == 0);

    limit.rlim_cur = 1024;
    CHECK(setrlimit(RLIMIT_NOFILE, &limit) == 0);
    CHECK(fcntl(4096, F_GETFD) == 0);
    zero = (struct timespec){0, 0};
    CHECK(syscall(SYS_pselect6, 4097, NULL, selected, NULL, &zero, NULL) == 1);
    CHECK(selected[4096 / 8] == 1);
    errno = 0;
    CHECK(dup(pipefd[1]) == -1 && errno == EMFILE);
    errno = 0;
    CHECK(fcntl(pipefd[1], F_DUPFD, 1024) == -1 && errno == EINVAL);
    errno = 0;
    CHECK(dup3(pipefd[1], 4097, 0) == -1 && errno == EBADF);
    CHECK(close(10) == 0 && dup(pipefd[1]) == 10);
    zero = (struct timespec){0, 0};
    errno = 0;
    CHECK(syscall(SYS_ppoll, NULL, 1025, &zero, NULL, 8) == -1 && errno == EINVAL);
    limit.rlim_cur = 8192;
    CHECK(setrlimit(RLIMIT_NOFILE, &limit) == 0);

    pollfds[0] = (struct pollfd){4096, POLLOUT, 0};
    pollfds[1] = pollfds[0];
    pollfds[2] = (struct pollfd){8191, POLLIN, 0};
    CHECK(syscall(SYS_ppoll, pollfds, 4097, &zero, NULL, 8) == 3);
    CHECK(pollfds[0].revents == POLLOUT && pollfds[1].revents == POLLOUT);
    CHECK(pollfds[2].revents == POLLNVAL && pollfds[4096].revents == 0);
    free(pollfds);

    zero = (struct timespec){0, 0};
    CHECK(syscall(SYS_pselect6, 4097, NULL, selected, NULL, &zero, NULL) == 1);
    CHECK(selected[4096 / 8] == 1);
    memset(selected, 0, span);
    CHECK(close(1025) == 0);
    selected[1025 / 8] = 1u << (1025 % 8);
    zero = (struct timespec){0, 0};
    errno = 0;
    CHECK(syscall(SYS_pselect6, 4097, selected, NULL, NULL, &zero, NULL) == -1 && errno == EBADF);
    CHECK(dup3(pipefd[1], 1025, 0) == 1025);
    free(selected);

    pid_t child = fork();
    CHECK(0 <= child);
    if (child == 0) {
        CHECK(close(1025) == 0);
        _exit(0);
    }
    CHECK(exited(child) && fcntl(1025, F_GETFD) == 0);
    void *stack = malloc(64 * 1024);
    CHECK(stack != NULL);
    child = clone(shared_files, (char *)stack + 64 * 1024, CLONE_FILES | SIGCHLD, &pipefd[1]);
    CHECK(0 < child && exited(child));
    free(stack);
    CHECK(getrlimit(RLIMIT_NOFILE, &limit) == 0 && limit.rlim_cur == 8192);
    errno = 0;
    CHECK(fcntl(4096, F_GETFD) == -1 && errno == EBADF);
    CHECK(dup3(pipefd[1], 4096, O_CLOEXEC) == 4096);
    child = fork();
    CHECK(0 <= child);
    if (child == 0) {
        char *arguments[] = {(char *)program, "--exec-check", NULL};
        execv(program, arguments);
        _exit(1);
    }
    CHECK(exited(child));
    for (int fd = 3; fd <= 4096; ++fd)
        CHECK(close(fd) == 0);
    CHECK(setrlimit(RLIMIT_NOFILE, &original) == 0);
    ksft_print_msg("descriptor-growth: growth/quota/dup/fork/clone-files/exec/poll/select passed\n");
    return 0;
}

/// 内部 exec 和夹具握手保留控制模式，普通检查仅由原始进程汇报一个 TAP 结果。
int main(int argc, char **argv)
{
    if (argc == 2 && strcmp(argv[1], "--poll-original") == 0)
        return poll_original();
    if (argc == 2 && strcmp(argv[1], "--exec-check") == 0) {
        errno = 0;
        CHECK(fcntl(4096, F_GETFD) == -1 && errno == EBADF);
        CHECK(fcntl(1025, F_GETFD) == 0);
        return 0;
    }
    pid_t reporter = getpid();
    ksft_print_header();
    ksft_set_plan(1);
    int result = descriptor_checks(argv[0]);
    if (getpid() == reporter)
        ksft_test_result(result == 0, "descriptor growth/quota/dup/fork/clone-files/exec/poll/select\n");
    return result;
}
