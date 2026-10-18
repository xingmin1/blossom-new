#include "libc_common.h"
#include "fp.h"

/// 以真实 AT_EXECFN 和环境核对脚本每层形成的拥有式 argv。
int check_script_argv(int argc, char **argv)
{
    const char *execfn = (const char *)(uintptr_t)getauxval(AT_EXECFN);
    const char *script_env = getenv("BLOSSOM_SCRIPT");
    if (execfn == NULL || script_env == NULL || strcmp(script_env, "stage7") != 0)
        return fail("script environment or AT_EXECFN");
    if (strcmp(execfn, "/test/scripts/no-option") == 0) {
        if (argc != 3 || strcmp(argv[1], "/test/scripts/no-option") != 0 ||
            strcmp(argv[2], "tail") != 0)
            return fail("no-option argv");
    } else if (strcmp(execfn, "/test/scripts/option") == 0) {
        if (argc != 4 || strcmp(argv[1], "--script-option one\t two") != 0 ||
            strcmp(argv[2], "/test/scripts/option") != 0 || strcmp(argv[3], "tail") != 0)
            return fail("option argv");
    } else if (strcmp(execfn, "/test/scripts/chain1") == 0) {
        const char *expected[] = {
            "/bin/libc-smoke", "/test/scripts/no-option", "/test/scripts/chain4",
            "/test/scripts/chain3", "/test/scripts/chain2", "/test/scripts/chain1", "tail",
        };
        if (argc != 7)
            return fail("chain argc");
        for (int i = 0; i < argc; ++i)
            if (strcmp(argv[i], expected[i]) != 0)
                return fail("chain argv");
    } else if (strcmp(execfn, "/test/scripts/no-exec") == 0) {
        if (argc != 3 || strcmp(argv[1], "/test/scripts/no-exec") != 0 ||
            strcmp(argv[2], "tail") != 0)
            return fail("0644 script argv");
        return EXEC_CHILD_STATUS;
    } else {
        return fail("script AT_EXECFN");
    }
    return EXIT_SUCCESS;
}

/// 在一次真实 execve 失败后核对 errno 与旧映像 TLS/执行路径仍然存在。
static int failed_exec(const char *path, int expected, const char *label,
                       const char *execfn_before)
{
    char *const args[] = { (char *)path, (char *)"tail", NULL };
    errno = 0;
    if (execve(path, args, environ) != -1 || errno != expected)
        return fail(label);
    if (tls_image != 0x2468ace0U || tls_zero != 0x10203040U ||
        strcmp((const char *)(uintptr_t)getauxval(AT_EXECFN), execfn_before) != 0 ||
        read_fp_value() != fp_pattern)
        return fail("failed exec preserved image");
    for (int fd = 0; fd < 3; ++fd)
        if (fcntl(fd, F_GETFD) != 0 || (fcntl(fd, F_GETFL) & O_ACCMODE) != O_RDWR)
            return fail("failed exec standard descriptors");
    return EXIT_SUCCESS;
}

/// 子进程真正执行0644脚本，父进程消费退出值后继续检查其余失败与旧映像状态。
static int non_executable_script(void)
{
    const char *path = "/test/scripts/no-exec";
    struct stat metadata;
    if (stat(path, &metadata) != 0 || (metadata.st_mode & 0777) != 0644)
        return fail("0644 script mode");
    pid_t child = fork();
    if (child < 0)
        return fail("0644 script fork");
    if (child == 0) {
        char *const args[] = { (char *)path, (char *)"tail", NULL };
        char *const env[] = { (char *)"BLOSSOM_SCRIPT=stage7", NULL };
        execve(path, args, env);
        _exit(127);
    }
    int status;
    pid_t reaped;
    do {
        reaped = waitpid(child, &status, 0);
    } while (reaped < 0 && errno == EINTR);
    if (reaped != child || !WIFEXITED(status) || WEXITSTATUS(status) != EXEC_CHILD_STATUS)
        return fail("0644 script exit");
    return EXIT_SUCCESS;
}

/// 真实消费0644脚本成功，以及解释器深度、缺失、格式和参数预算错误。
int script_errors(void)
{
    const char *execfn = (const char *)(uintptr_t)getauxval(AT_EXECFN);
    char *saved = strdup(execfn == NULL ? "" : execfn);
    if (saved == NULL)
        return fail("script error execfn copy");
    int result = failed_exec("/test/scripts/too-deep", ELOOP, "too-deep", saved);
    if (result == EXIT_SUCCESS)
        result = failed_exec("/test/scripts/missing-interpreter", ENOENT,
                             "missing-interpreter", saved);
    if (result == EXIT_SUCCESS)
        result = non_executable_script();
    if (result == EXIT_SUCCESS)
        result = failed_exec("/test/scripts/non-script", ENOEXEC, "non-script", saved);
    static char *large_argv[129];
    static char *empty_env[] = { NULL };
    for (size_t index = 0; index < 128; ++index)
        large_argv[index] = (char *)(index == 0 ? "/test/scripts/no-option" : "tail");
    large_argv[128] = NULL;
    errno = 0;
    if (result == EXIT_SUCCESS &&
        (execve("/test/scripts/no-option", large_argv, empty_env) != -1 || errno != E2BIG))
        result = fail("oversized exec argv");
    static char large_arg[33000];
    static char *byte_argv[3];
    memset(large_arg, 'x', sizeof(large_arg) - 1);
    large_arg[sizeof(large_arg) - 1] = '\0';
    byte_argv[0] = (char *)"/bin/libc-smoke";
    byte_argv[1] = large_arg;
    byte_argv[2] = NULL;
    errno = 0;
    if (result == EXIT_SUCCESS &&
        (execve("/bin/libc-smoke", byte_argv, empty_env) != -1 || errno != E2BIG))
        result = fail("oversized exec bytes");
    if (result == EXIT_SUCCESS &&
        (tls_image != 0x2468ace0U || tls_zero != 0x10203040U ||
         strcmp((const char *)(uintptr_t)getauxval(AT_EXECFN), saved) != 0 ||
         read_fp_value() != fp_pattern))
        result = fail("oversized exec preserved image");
    if (result == EXIT_SUCCESS && (fputc('E', stdout) == EOF || fflush(stdout) != 0))
        result = fail("failed exec stdio");
    free(saved);
    return result;
}
