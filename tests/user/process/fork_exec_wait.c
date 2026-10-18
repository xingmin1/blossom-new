#define _GNU_SOURCE
#include <fcntl.h>
#include <limits.h>
#include <stdlib.h>
#include <string.h>
#include "kselftest.h"
#include "selftest_io.h"

static _Thread_local volatile unsigned tls_image = 0x13579bdf;
static _Thread_local volatile unsigned tls_zero;
static volatile unsigned private_value = 0x2468ace0;
static const unsigned char fork_packet[] = { 'f', 0, 0xff, 'k' };
static const unsigned char exec_packet[] = { 'e', 0xfe, 0, 'c' };

/// 内部 exec 模式只解析本次父进程生成的有限描述符，不假定固定表槽。
static int descriptor(const char *text)
{
    char *end;
    errno = 0;
    long value = strtol(text, &end, 10);
    if (errno != 0 || text[0] == '\0' || *end != '\0' || value < 0 || INT_MAX < value)
        return -1;
    return (int)value;
}

/// 新映像核对 CRT 的 TLS 模板和 CLOEXEC，再向继承的普通 pipe 发送二进制记录。
static int exec_child(const char *kept_text, const char *closed_text)
{
    int fd = descriptor(kept_text), closed = descriptor(closed_text);
    int result = 1;
    ST_CHECK(0 <= fd && 0 <= closed && fd != closed);
    ST_CHECK(tls_image == 0x13579bdf && tls_zero == 0 && private_value == 0x2468ace0);
    errno = 0;
    ST_CHECK(fcntl(closed, F_GETFD) == -1 && errno == EBADF);
    ST_CHECK(st_write_all(fd, exec_packet, sizeof(exec_packet)) == 0);
    ST_CHECK(st_close(&fd) == 0);
    result = 37;
out:
    if (st_close(&fd) != 0)
        result = 1;
    return result;
}

/// fork 子核继承的状态，失败 exec 保旧映像，再由绝对路径执行同源 ELF。
static int fork_child(const char *program, int read_fd, int write_fd, int closed)
{
    int result = 1;
    ST_CHECK(st_close(&read_fd) == 0);
    ST_CHECK(tls_image == 0x10203040 && tls_zero == 0x55667788 && private_value == 0x76543210);
    tls_image = 7;
    tls_zero = 9;
    private_value = 11;
    errno = 0;
    execl("", "", (char *)NULL);
    ST_CHECK(errno == ENOENT && tls_image == 7 && tls_zero == 9 && private_value == 11);
    ST_CHECK(st_write_all(write_fd, fork_packet, sizeof(fork_packet)) == 0);
    ST_CHECK(chdir("/") == 0);
    char kept_text[32], closed_text[32];
    ST_CHECK(0 < snprintf(kept_text, sizeof(kept_text), "%d", write_fd));
    ST_CHECK(0 < snprintf(closed_text, sizeof(closed_text), "%d", closed));
    execl(program, program, "--exec-child", kept_text, closed_text, (char *)NULL);
    ST_CHECK(0);
out:
    if (st_close(&read_fd) != 0)
        result = 1;
    if (st_close(&write_fd) != 0)
        result = 1;
    return result;
}

/// 父消费 fork 与 exec 两笔记录、真实 EOF 和状态37，父内存、TLS、cwd 均不得被子修改。
static int lifecycle(const char *program)
{
    int result = -1, channel[2] = { -1, -1 }, closed = -1;
    pid_t child = -1;
    char before[PATH_MAX], after[PATH_MAX];
    unsigned char packet[8], extra;
    ST_CHECK(getcwd(before, sizeof(before)) != NULL);
    closed = open(".", O_RDONLY | O_DIRECTORY | O_CLOEXEC);
    ST_CHECK(0 <= closed);
    ST_CHECK(pipe(channel) == 0);
    tls_image = 0x10203040;
    tls_zero = 0x55667788;
    private_value = 0x76543210;
    child = fork();
    ST_CHECK(0 <= child);
    if (child == 0)
        _exit(fork_child(program, channel[0], channel[1], closed));
    ST_CHECK(st_close(&channel[1]) == 0);
    ST_CHECK(st_read_all(channel[0], packet, sizeof(packet)) == 0);
    ST_CHECK(memcmp(packet, fork_packet, sizeof(fork_packet)) == 0);
    ST_CHECK(memcmp(packet + sizeof(fork_packet), exec_packet, sizeof(exec_packet)) == 0);
    ST_CHECK(read(channel[0], &extra, 1) == 0);
    ST_CHECK(st_reap(&child, 37) == 0);
    ST_CHECK(tls_image == 0x10203040 && tls_zero == 0x55667788 && private_value == 0x76543210);
    ST_CHECK(getcwd(after, sizeof(after)) != NULL && strcmp(before, after) == 0);
    ST_CHECK(fcntl(closed, F_GETFD) == FD_CLOEXEC);
    result = 0;
out:
    if (st_stop_child(&child) != 0)
        result = -1;
    if (st_close(&channel[0]) != 0)
        result = -1;
    if (st_close(&channel[1]) != 0)
        result = -1;
    if (st_close(&closed) != 0)
        result = -1;
    return result;
}

/// WNOHANG 在孩子仍等待 pipe 时返回零，放行后的 wait 只领取一次真实退出状态。
static int wait_boundaries(void)
{
    int result = -1, channel[2] = { -1, -1 }, status;
    pid_t child = -1;
    unsigned char byte;
    ST_CHECK(pipe(channel) == 0);
    child = fork();
    ST_CHECK(0 <= child);
    if (child == 0) {
        if (close(channel[1]) != 0 || st_read_all(channel[0], &byte, 1) != 0 ||
            byte != 'G' || close(channel[0]) != 0)
            _exit(1);
        _exit(19);
    }
    ST_CHECK(st_close(&channel[0]) == 0);
    ST_CHECK(waitpid(child, &status, WNOHANG) == 0);
    ST_CHECK(st_write_all(channel[1], "G", 1) == 0);
    ST_CHECK(st_close(&channel[1]) == 0);
    pid_t original = child;
    ST_CHECK(st_reap(&child, 19) == 0);
    errno = 0;
    ST_CHECK(waitpid(original, &status, WNOHANG) == -1 && errno == ECHILD);
    errno = 0;
    ST_CHECK(waitpid(getpid(), &status, 0) == -1 && errno == ECHILD);
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

/// 默认程序通过普通 fork/self-exec/wait 自测，内部子模式不产生独立 TAP 清单项。
int main(int argc, char **argv)
{
    if (argc == 4 && strcmp(argv[1], "--exec-child") == 0)
        return exec_child(argv[2], argv[3]);
    ksft_print_header();
    if (argc != 1)
        ksft_exit_fail_msg("usage: %s\n", argv[0]);
    char *program = realpath(argv[0], NULL);
    if (program == NULL)
        ksft_exit_fail_msg("realpath executable errno=%d\n", errno);
    ksft_set_plan(2);
    ksft_test_result(lifecycle(program) == 0, "fork self-exec TLS memory cwd CLOEXEC pipe EOF and exit status\n");
    ksft_test_result(wait_boundaries() == 0, "wait WNOHANG release reap and ECHILD\n");
    free(program);
    ksft_finished();
}
