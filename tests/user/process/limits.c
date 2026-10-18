#include "libc_common.h"

/// 经普通线程修改同组软限额，join 后由创建者读取实际共享值。
static void *limit_thread(void *unused)
{
    (void)unused;
    struct rlimit limit = { .rlim_cur = 96 * 1024, .rlim_max = 100 * 1024 };
    return (void *)(uintptr_t)(setrlimit(RLIMIT_STACK, &limit) != 0);
}

/// 新 exec 栈的最低页能交付 pipe，guard 页返回 EFAULT，不能用限额 getter 代替范围验证。
int limited_stack(void)
{
    struct rlimit limit;
    if (getrlimit(RLIMIT_STACK, &limit) != 0 ||
        limit.rlim_cur != 100 * 1024 || limit.rlim_max != 100 * 1024)
        return fail("exec inherited stack limit");
#if defined(__riscv)
    const uintptr_t top = ((uintptr_t)1 << 38) - 4096;
#elif defined(__loongarch__)
    const uintptr_t top = ((uintptr_t)1 << 39) - 4096;
#endif
    int descriptors[2];
    if (pipe(descriptors) != 0)
        return fail("stack range pipe");
    unsigned char value;
    if (write(descriptors[1], (void *)(top - limit.rlim_cur), 1) != 1 ||
        read(descriptors[0], &value, 1) != 1 || value != 0)
        return fail("stack lowest mapped page");
    errno = 0;
    if (write(descriptors[1], (void *)(top - limit.rlim_cur - 4096), 1) != -1 ||
        errno != EFAULT || close(descriptors[0]) != 0 || close(descriptors[1]) != 0)
        return fail("stack guard bound");
    return EXIT_SUCCESS;
}

/// fork 子继承父值，提交真实硬限额和共享线程更新，再用同一文件完成 exec。
static int limit_child(const char *program, const struct rlimit *inherited)
{
    struct rlimit limit;
    if (getrlimit(RLIMIT_STACK, &limit) != 0 ||
        memcmp(&limit, inherited, sizeof(limit)) != 0)
        return fail("fork inherited stack limit");
    limit.rlim_cur = 100 * 1024;
    limit.rlim_max = 100 * 1024;
    if (setrlimit(RLIMIT_STACK, &limit) != 0)
        return fail("set stack limit");
    if (getuid() != 0) {
        struct rlimit raised = { .rlim_cur = 100 * 1024, .rlim_max = 104 * 1024 };
        errno = 0;
        if (setrlimit(RLIMIT_STACK, &raised) != -1 || errno != EPERM)
            return fail("stack hard limit privilege");
    }
    struct rlimit invalid = { .rlim_cur = 104 * 1024, .rlim_max = 100 * 1024 };
    errno = 0;
    if (setrlimit(RLIMIT_STACK, &invalid) != -1 || errno != EINVAL)
        return fail("stack soft hard order");
    struct rlimit next = { .rlim_cur = 96 * 1024, .rlim_max = 100 * 1024 };
    errno = 0;
    if (syscall(SYS_prlimit64, 0, RLIMIT_STACK, &next, (void *)1) != -1 ||
        errno != EFAULT || getrlimit(RLIMIT_STACK, &limit) != 0 ||
        memcmp(&limit, &next, sizeof(limit)) != 0)
        return fail("prlimit committed before old copy fault");
    limit.rlim_cur = 100 * 1024;
    if (setrlimit(RLIMIT_STACK, &limit) != 0)
        return fail("restore stack soft limit");
    pthread_t thread;
    if (pthread_create(&thread, NULL, limit_thread, NULL) != 0)
        return fail("stack limit thread");
    void *thread_result;
    if (pthread_join(thread, &thread_result) != 0 || thread_result != NULL ||
        getrlimit(RLIMIT_STACK, &limit) != 0 ||
        limit.rlim_cur != 96 * 1024 || limit.rlim_max != 100 * 1024)
        return fail("thread shared stack limit");
    limit.rlim_cur = 100 * 1024;
    if (setrlimit(RLIMIT_STACK, &limit) != 0)
        return fail("thread stack limit restore");
    char *const arguments[] = { (char *)program, "--runtime-stack-child", NULL };
    char *const environment[] = { NULL };
    execve(program, arguments, environment);
    return fail("limited exec");
}

/// 正反限额请求均由实际用户 ABI 消费，父在完整 wait 后保持原组限额。
int runtime_limits(const char *program)
{
    struct rlimit original, after;
    if (getrlimit(RLIMIT_STACK, &original) != 0)
        return fail("get stack limit");
    errno = 0;
    if (syscall(SYS_prlimit64, 0, RLIMIT_STACK, (void *)NULL, (void *)1) != -1 || errno != EFAULT)
        return fail("get stack limit user fault");
    errno = 0;
    if (syscall(SYS_prlimit64, 0, RLIMIT_STACK, (void *)1, (void *)NULL) != -1 || errno != EFAULT)
        return fail("set stack limit user fault");
    pid_t child = fork();
    if (child < 0)
        return fail("stack limit fork");
    if (child == 0)
        _exit(limit_child(program, &original));
    if (wait_status(child, 0) != EXIT_SUCCESS ||
        getrlimit(RLIMIT_STACK, &after) != 0 ||
        memcmp(&original, &after, sizeof(after)) != 0)
        return fail("independent parent stack limit");
    if (printf("libc-smoke: stack limit fork/thread/exec/guard passed uid=%ld\n",
               (long)getuid()) < 0 || fflush(stdout) != 0)
        return fail("stack limit output");
    return EXIT_SUCCESS;
}
