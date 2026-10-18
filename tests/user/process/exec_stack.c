#define _GNU_SOURCE
#include <fcntl.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/resource.h>
#include <sys/syscall.h>
#include "kselftest.h"
#include "selftest_io.h"

#define PAGE_BYTES 4096UL
#define MIB (1024UL * 1024UL)
#define ARGUMENT_BYTES (2UL * MIB)
#define STRING_BYTES (32UL * PAGE_BYTES)
#define CHUNK_BYTES (120UL * 1024UL)
#define VERIFY_STATUS 37

enum StackCase {
    STACK_DEFAULT, STACK_SMALL, STACK_RAISED, STACK_INFINITE,
    STACK_SHRUNK, STACK_GUARD, STACK_UACCESS, STACK_FORK,
};

/// 只改变本进程软限额，硬限额及父进程限额保持原值。
static int stack_limit(rlim_t bytes)
{
    struct rlimit limit;
    if (getrlimit(RLIMIT_STACK, &limit) != 0)
        return -1;
    limit.rlim_cur = bytes;
    return setrlimit(RLIMIT_STACK, &limit);
}

/// 完整检查参数中的非 NUL 后缀，首尾字节相等不能代替完整快照。
static int filled(const char *bytes, size_t length, unsigned char value)
{
    for (size_t index = 0; index < length; ++index)
        if ((unsigned char)bytes[index] != value)
            return 0;
    return 1;
}

/// 扣除执行文件名、三个参数和二十个有效指针后，最后一个环境串恰好占满2MiB。
static size_t final_environment_bytes(const char *program)
{
    size_t fixed = 2 * (strlen(program) + 1) + sizeof("--verify-arguments")
        + sizeof("large") + 20 * sizeof(char *) + 16 * CHUNK_BYTES;
    return ARGUMENT_BYTES - fixed;
}

/// 新映像核对全部参数及环境字节，以独立退出值区分成功 exec 与预期错误返回。
static int verify_arguments(int argc, char **argv, char **envp)
{
    int result = 1;
    ST_CHECK(3 <= argc);
    if (strcmp(argv[2], "short") == 0) {
        ST_CHECK(argc == 257);
        for (int index = 3; index < argc; ++index)
            ST_CHECK(strcmp(argv[index], "word") == 0);
        ST_CHECK(envp[0] != NULL && envp[1] == NULL);
        ST_CHECK(strlen(envp[0]) == 65535);
        ST_CHECK(filled(envp[0], 65535, 'q'));
    } else if (strcmp(argv[2], "large") == 0) {
        ST_CHECK(argc == 3);
        for (int index = 0; index < 16; ++index) {
            ST_CHECK(envp[index] != NULL && strlen(envp[index]) == CHUNK_BYTES - 1);
            ST_CHECK(filled(envp[index], CHUNK_BYTES - 1, 'k'));
        }
        size_t final_bytes = final_environment_bytes(argv[0]);
        ST_CHECK(envp[16] != NULL && envp[17] == NULL);
        ST_CHECK(strlen(envp[16]) == final_bytes - 1);
        ST_CHECK(filled(envp[16], final_bytes - 1, 'z'));
    } else {
        ST_CHECK(strcmp(argv[2], "single") == 0 && argc == 4);
        ST_CHECK(envp[0] == NULL && strlen(argv[3]) == STRING_BYTES - 1);
        ST_CHECK(filled(argv[3], STRING_BYTES - 1, 'x'));
    }
    result = VERIFY_STATUS;
out:
    return result;
}

/// 每次 exec 都在独立子进程中运行，失败保旧映像，成功必须经过新 CRT 的字节检查。
static int exec_checked(const char *program, char *const argv[], char *const envp[],
                        int expected_errno, rlim_t soft)
{
    int result = -1;
    pid_t child = fork();
    ST_CHECK(0 <= child);
    if (child == 0) {
        if (stack_limit(soft) != 0)
            _exit(90);
        errno = 0;
        long returned = syscall(SYS_execve, program, argv, envp);
        _exit(expected_errno != 0 && returned == -1 && errno == expected_errno ? 0 : 91);
    }
    ST_CHECK(st_reap(&child, expected_errno == 0 ? VERIFY_STATUS : 0) == 0);
    result = 0;
out:
    if (st_stop_child(&child) != 0)
        result = -1;
    return result;
}

/// 真实 argv 越过旧128项限制，独立长环境串也超过旧32KiB总预算。
static int many_arguments(const char *program)
{
    int result = -1;
    char **arguments = calloc(258, sizeof(*arguments));
    char *environment = malloc(65536);
    ST_CHECK(arguments != NULL && environment != NULL);
    arguments[0] = (char *)program;
    arguments[1] = "--verify-arguments";
    arguments[2] = "short";
    for (int index = 3; index < 257; ++index)
        arguments[index] = "word";
    memset(environment, 'q', 65535);
    environment[65535] = '\0';
    char *envp[] = { environment, NULL };
    ST_CHECK(exec_checked(program, arguments, envp, 0, 8 * MIB) == 0);
    result = 0;
out:
    free(arguments);
    free(environment);
    return result;
}

/// 每个环境串均小于单串边界，只增最后一字节就必须触发共同总预算的 E2BIG。
static int total_arguments(const char *program)
{
    int result = -1;
    size_t final_bytes = final_environment_bytes(program);
    char *chunk = malloc(CHUNK_BYTES);
    char *last = malloc(final_bytes + 1);
    ST_CHECK(2 < final_bytes && final_bytes < STRING_BYTES);
    ST_CHECK(chunk != NULL && last != NULL);
    memset(chunk, 'k', CHUNK_BYTES - 1);
    chunk[CHUNK_BYTES - 1] = '\0';
    memset(last, 'z', final_bytes);
    last[final_bytes - 1] = '\0';
    char *arguments[] = { (char *)program, "--verify-arguments", "large", NULL };
    char *envp[18];
    for (int index = 0; index < 16; ++index)
        envp[index] = chunk;
    envp[16] = last;
    envp[17] = NULL;
    ST_CHECK(exec_checked(program, arguments, envp, 0, 8 * MIB) == 0);
    last[final_bytes - 1] = 'z';
    last[final_bytes] = '\0';
    ST_CHECK(exec_checked(program, arguments, envp, E2BIG, 8 * MIB) == 0);
    result = 0;
out:
    free(chunk);
    free(last);
    return result;
}

/// 单串含 NUL 的32页边界独立于总预算，并且失败 exec 不能丢失原进程。
static int single_argument(const char *program)
{
    int result = -1;
    char *string = malloc(STRING_BYTES + 1);
    ST_CHECK(string != NULL);
    memset(string, 'x', STRING_BYTES);
    string[STRING_BYTES - 1] = '\0';
    char *arguments[] = { (char *)program, "--verify-arguments", "single", string, NULL };
    ST_CHECK(exec_checked(program, arguments, NULL, 0, 8 * MIB) == 0);
    string[STRING_BYTES - 1] = 'x';
    string[STRING_BYTES] = '\0';
    ST_CHECK(exec_checked(program, arguments, NULL, E2BIG, 8 * MIB) == 0);
    result = 0;
out:
    free(string);
    return result;
}

/// NULL 和显式空 argv 都生成空 argv[0]；坏向量和坏字符串继续返回真实 EFAULT。
static int pointer_arguments(const char *program)
{
    int result = -1;
    char *empty[] = { NULL };
    char *bad_string[] = { (char *)(uintptr_t)-1, NULL };
    char **bad_vector = (char **)(uintptr_t)-8;
    ST_CHECK(exec_checked(program, NULL, NULL, 0, 8 * MIB) == 0);
    ST_CHECK(exec_checked(program, empty, empty, 0, 8 * MIB) == 0);
    ST_CHECK(exec_checked(program, bad_vector, NULL, EFAULT, 8 * MIB) == 0);
    ST_CHECK(exec_checked(program, empty, bad_vector, EFAULT, 8 * MIB) == 0);
    ST_CHECK(exec_checked(program, bad_string, NULL, EFAULT, 8 * MIB) == 0);
    result = 0;
out:
    return result;
}

/// 地址使用整数偏移后再访问真实页，不用编译器可能消除的局部数组代替缺页。
static int store_stack(uintptr_t address, unsigned char value)
{
    volatile unsigned char *byte = (volatile unsigned char *)address;
    if (*byte != 0)
        return -1;
    *byte = value;
    return *byte == value ? 0 : -1;
}

/// 管道的用户目标和源分别落在两个新缺口，要求 copyout 和 copyin 都沿同一 fault 增长。
static int stack_uaccess(uintptr_t anchor)
{
    int result = -1, channel[2] = { -1, -1 };
    unsigned char byte = 'p';
    uintptr_t output = anchor - 2 * MIB, input = anchor - 4 * MIB;
    ST_CHECK(pipe(channel) == 0);
    ST_CHECK(write(channel[1], &byte, 1) == 1);
    ST_CHECK(read(channel[0], (void *)output, 1) == 1);
    ST_CHECK(*(volatile unsigned char *)output == 'p');
    ST_CHECK(write(channel[1], (void *)input, 1) == 1);
    ST_CHECK(read(channel[0], &byte, 1) == 1 && byte == 0);
    result = 0;
out:
    if (st_close(&channel[0]) != 0)
        result = -1;
    if (st_close(&channel[1]) != 0)
        result = -1;
    return result;
}

/// 改权切开栈后，fork 两侧仍可各自增长，已有页的 COW 不能改掉父进程字节。
static int stack_fork(uintptr_t anchor)
{
    int result = -1;
    pid_t child = -1;
    uintptr_t initial = anchor - 2 * MIB;
    ST_CHECK(store_stack(initial, 0x31) == 0);
    ST_CHECK(mprotect((void *)(initial + PAGE_BYTES), PAGE_BYTES, PROT_READ) == 0);
    child = fork();
    ST_CHECK(0 <= child);
    if (child == 0) {
        if (store_stack(anchor - 4 * MIB, 0x52) != 0)
            _exit(1);
        *(volatile unsigned char *)initial = 0x63;
        _exit(*(volatile unsigned char *)initial == 0x63 ? 0 : 1);
    }
    ST_CHECK(st_reap(&child, 0) == 0);
    ST_CHECK(*(volatile unsigned char *)initial == 0x31);
    ST_CHECK(store_stack(anchor - 3 * MIB, 0x74) == 0);
    result = 0;
out:
    if (st_stop_child(&child) != 0)
        result = -1;
    return result;
}

/// guard 的合法边缘先成功；普通 hint/brk 不得进入间隙，显式固定映射仍按实际范围处理。
static int stack_guard(uintptr_t anchor, uintptr_t *denied)
{
    int result = -1;
    uintptr_t obstacle = anchor - 4 * MIB;
    uintptr_t lower = obstacle + 256 * PAGE_BYTES + PAGE_BYTES;
    void *mapped = mmap((void *)obstacle, PAGE_BYTES, PROT_READ | PROT_WRITE,
                        MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0);
    ST_CHECK(mapped == (void *)obstacle);
    ST_CHECK(store_stack(lower, 0x45) == 0);
    uintptr_t hint = lower - PAGE_BYTES;
    mapped = mmap((void *)hint, PAGE_BYTES, PROT_READ | PROT_WRITE,
                  MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ST_CHECK(mapped != MAP_FAILED && mapped != (void *)hint);
    ST_CHECK(munmap(mapped, PAGE_BYTES) == 0);
    long old_heap = syscall(SYS_brk, 0);
    ST_CHECK(syscall(SYS_brk, lower - PAGE_BYTES) == old_heap);
    mapped = mmap((void *)hint, PAGE_BYTES, PROT_READ | PROT_WRITE,
                  MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0);
    ST_CHECK(mapped == (void *)hint && store_stack(hint, 0x56) == 0);
    ST_CHECK(munmap(mapped, PAGE_BYTES) == 0);
    *denied = obstacle + 256 * PAGE_BYTES;
    result = 0;
out:
    return result;
}

/// 子进程先报告所有前置动作完成，再进行唯一预期拒绝访问，避免把准备期崩溃误算成功。
static int stack_action(enum StackCase which, int ready)
{
    int result = 1;
    uintptr_t anchor = (uintptr_t)__builtin_frame_address(0) & ~(PAGE_BYTES - 1);
    uintptr_t denied = 0;
    ST_CHECK(stack_limit(8 * MIB) == 0);
    switch (which) {
    case STACK_DEFAULT:
        ST_CHECK(store_stack(anchor - 4 * MIB, 0x11) == 0);
        break;
    case STACK_SMALL:
        ST_CHECK(stack_limit(256 * 1024) == 0);
        denied = anchor - 512 * 1024;
        break;
    case STACK_RAISED:
        ST_CHECK(stack_limit(16 * MIB) == 0);
        ST_CHECK(store_stack(anchor - 12 * MIB, 0x22) == 0);
        break;
    case STACK_INFINITE:
        ST_CHECK(stack_limit(RLIM_INFINITY) == 0);
        ST_CHECK(store_stack(anchor - 64 * MIB, 0x33) == 0);
        ST_CHECK(store_stack(anchor - 32 * MIB, 0x44) == 0);
        break;
    case STACK_SHRUNK:
        ST_CHECK(stack_limit(16 * MIB) == 0);
        ST_CHECK(store_stack(anchor - 12 * MIB, 0x55) == 0);
        ST_CHECK(stack_limit(2 * MIB) == 0);
        denied = anchor - 14 * MIB;
        break;
    case STACK_GUARD:
        ST_CHECK(stack_guard(anchor, &denied) == 0);
        break;
    case STACK_UACCESS:
        ST_CHECK(stack_uaccess(anchor) == 0);
        break;
    case STACK_FORK:
        ST_CHECK(stack_fork(anchor) == 0);
        break;
    }
    ST_CHECK(st_write_all(ready, "r", 1) == 0);
    if (denied != 0)
        *(volatile unsigned char *)denied = 0x7a;
    result = 0;
out:
    return result;
}

/// 父同时消费准备字节和真实 wait 类型，限额及 guard 拒绝必须是 SIGSEGV，不能是 OOM 的 SIGKILL。
static int stack_case(enum StackCase which, int expected_signal)
{
    int result = -1, channel[2] = { -1, -1 }, status;
    pid_t child = -1;
    unsigned char ready = 0;
    ST_CHECK(pipe(channel) == 0);
    child = fork();
    ST_CHECK(0 <= child);
    if (child == 0) {
        close(channel[0]);
        _exit(stack_action(which, channel[1]));
    }
    ST_CHECK(st_close(&channel[1]) == 0);
    ST_CHECK(st_read_all(channel[0], &ready, 1) == 0 && ready == 'r');
    pid_t reaped;
    do {
        reaped = waitpid(child, &status, 0);
    } while (reaped == -1 && errno == EINTR);
    ST_CHECK(reaped == child);
    child = -1;
    if (expected_signal != 0)
        ST_CHECK(WIFSIGNALED(status) && WTERMSIG(status) == expected_signal);
    else
        ST_CHECK(WIFEXITED(status) && WEXITSTATUS(status) == 0);
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

/// 新 CRT 读取继承的同一软限额，有限值和无限值都必须完成实际 exec。
static int verify_stack_limit(const char *mode)
{
    struct rlimit limit;
    if (getrlimit(RLIMIT_STACK, &limit) != 0)
        return 1;
    rlim_t expected = strcmp(mode, "small") == 0 ? 256 * 1024 :
        strcmp(mode, "raised") == 0 ? 16 * MIB : RLIM_INFINITY;
    return limit.rlim_cur == expected ? VERIFY_STATUS : 1;
}

/// 软限额在 exec 前设置，新映像初始映射不能拿无限值当地址长度。
static int exec_stack_limits(const char *program)
{
    int result = -1;
    char *arguments[] = { (char *)program, "--verify-stack", "small", NULL };
    ST_CHECK(exec_checked(program, arguments, NULL, 0, 256 * 1024) == 0);
    arguments[2] = "raised";
    ST_CHECK(exec_checked(program, arguments, NULL, 0, 16 * MIB) == 0);
    arguments[2] = "infinite";
    ST_CHECK(exec_checked(program, arguments, NULL, 0, RLIM_INFINITY) == 0);
    result = 0;
out:
    return result;
}

/// 默认、有限增大和无限软限额分别真实增长，缩小后的新缺口使用当前限额。
static int stack_limits(void)
{
    int result = -1;
    ST_CHECK(stack_case(STACK_DEFAULT, 0) == 0);
    ST_CHECK(stack_case(STACK_SMALL, SIGSEGV) == 0);
    ST_CHECK(stack_case(STACK_RAISED, 0) == 0);
    ST_CHECK(stack_case(STACK_INFINITE, 0) == 0);
    ST_CHECK(stack_case(STACK_SHRUNK, SIGSEGV) == 0);
    result = 0;
out:
    return result;
}

/// 正式 selftest 可分别选择参数或栈，内部 exec 模式只返回独立验证状态，不重复发布 TAP 计划。
int main(int argc, char **argv, char **envp)
{
    if (argc == 1 && argv[0][0] == '\0')
        return envp[0] == NULL ? VERIFY_STATUS : 1;
    if (argc == 3 && strcmp(argv[1], "--verify-stack") == 0)
        return verify_stack_limit(argv[2]);
    if (3 <= argc && strcmp(argv[1], "--verify-arguments") == 0)
        return verify_arguments(argc, argv, envp);
    int arguments = argc == 1 || (argc == 2 && strcmp(argv[1], "--arguments") == 0);
    int stack = argc == 1 || (argc == 2 && strcmp(argv[1], "--stack") == 0);
    ksft_print_header();
    if (!arguments && !stack)
        ksft_exit_fail_msg("usage: %s [--arguments|--stack]\n", argv[0]);
    char *program = realpath(argv[0], NULL);
    if (program == NULL)
        ksft_exit_fail_msg("realpath executable errno=%d\n", errno);
    ksft_set_plan(4 * arguments + 5 * stack);
    if (arguments) {
        ksft_test_result(many_arguments(program) == 0, "257 arguments and a 64KiB environment survive exec\n");
        ksft_test_result(total_arguments(program) == 0, "exact 2MiB combined budget and one-byte E2BIG\n");
        ksft_test_result(single_argument(program) == 0, "32-page string boundary including NUL\n");
        ksft_test_result(pointer_arguments(program) == 0, "empty argv and genuine pointer errors\n");
    }
    if (stack) {
        ksft_test_result(exec_stack_limits(program) == 0, "small raised and infinite stack limits survive exec\n");
        ksft_test_result(stack_limits() == 0, "default small raised infinite and lowered stack limits\n");
        ksft_test_result(stack_case(STACK_GUARD, SIGSEGV) == 0, "guard neighbor nonfixed hint brk and explicit fixed map\n");
        ksft_test_result(stack_case(STACK_UACCESS, 0) == 0, "copyout and copyin expand two real stack gaps\n");
        ksft_test_result(stack_case(STACK_FORK, 0) == 0, "protect split fork growth and private COW\n");
    }
    free(program);
    ksft_finished();
}
