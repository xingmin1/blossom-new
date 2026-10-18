#include "libc_common.h"
#include "fp.h"

/// 普通 C 入口检查各模式的实际结果，并在返回前核对贯穿整个程序的 callee-saved 浮点图样。
int main(int argc, char **argv)
{
    if (argc < 1 || argv == NULL || argv[0] == NULL || argv[0][0] != '/' ||
        argv[argc] != NULL)
        return fail("main argv");
    const char *execfn = (const char *)(uintptr_t)getauxval(AT_EXECFN);
    int script_mode = execfn != NULL && strncmp(execfn, "/test/scripts/", 14) == 0;
    int exec_child = argc == 3 && strcmp(argv[1], "--exec-child") == 0 &&
                     strcmp(argv[2], "stage7") == 0;
    int migration = argc == 5 && strcmp(argv[1], "stage7") == 0 &&
                    strcmp(argv[2], "--migrate") == 0;
    int abi = argc == 3 && strcmp(argv[1], "--abi") == 0 && strcmp(argv[2], "stage7") == 0;
    int abi_user = argc == 3 && strcmp(argv[1], "--abi-user") == 0 &&
                   strcmp(argv[2], "stage7") == 0;
    int errors = argc == 3 && strcmp(argv[1], "--script-errors") == 0 &&
                 strcmp(argv[2], "stage7") == 0;
    int pid1 = argc == 3 && strcmp(argv[1], "--pid1") == 0 && strcmp(argv[2], "stage7") == 0;
    int fixture_root = argc == 3 && strcmp(argv[1], "--fixture-root") == 0 &&
                       strcmp(argv[2], "stage7") == 0;
    int view = argc == 2 && strcmp(argv[1], "--runtime-view") == 0;
    int limits = argc == 2 && strcmp(argv[1], "--runtime-limits") == 0;
    int stack_child = argc == 2 && strcmp(argv[1], "--runtime-stack-child") == 0;
    int file_times = argc == 2 && strcmp(argv[1], "--runtime-file-times") == 0;
    int socket_timeouts = argc == 2 && strcmp(argv[1], "--runtime-socket-timeouts") == 0;
    int clocks = argc == 2 && strcmp(argv[1], "--runtime-clock") == 0;
    if (!script_mode && !exec_child && !migration && !abi && !abi_user && !errors &&
        !pid1 && !fixture_root && !view && !limits && !stack_child && !file_times &&
        !socket_timeouts && !clocks &&
        !(argc == 2 && strcmp(argv[1], "stage7") == 0))
        return fail("main arguments");
    if (tls_image != 0x13579bdfU || tls_zero != 0)
        return fail("initial TLS template");
    if (check_malloc() != EXIT_SUCCESS || (!stack_child && check_stack() != EXIT_SUCCESS))
        return EXIT_FAILURE;
    set_fp_value();
    int tls_status;
    if (migration) {
        tls_image = 0x2468ace0U;
        tls_zero = 0x10203040U;
        errno = EDOM;
        tls_status = check_tls_state();
    } else {
        tls_status = check_tls();
    }
    if (check_fp() != EXIT_SUCCESS || tls_status != EXIT_SUCCESS)
        return EXIT_FAILURE;
    int result;
    if (script_mode) {
        result = check_script_argv(argc, argv);
    } else if (exec_child) {
        if (printf("libc-smoke: exec child initialized\n") < 0 || fflush(stdout) != 0)
            result = fail("child stdio");
        else
            result = EXEC_CHILD_STATUS;
    } else if (abi) {
        result = abi_check(0, 0);
    } else if (abi_user) {
        result = abi_check(1000, 2000);
    } else if (errors) {
        result = script_errors();
    } else if (pid1 || fixture_root) {
        result = root_role_body(pid1);
    } else if (view) {
        result = runtime_view();
    } else if (limits) {
        result = runtime_limits(argv[0]);
    } else if (stack_child) {
        result = limited_stack();
    } else if (file_times) {
        result = runtime_file_times(argv[0]);
    } else if (socket_timeouts) {
        result = runtime_socket_timeouts();
    } else if (clocks) {
        result = runtime_clock();
    } else if (migration) {
        result = migration_handshake(argv[3], argv[4]);
        if (result == EXIT_SUCCESS)
            result = run_body(argv[0]);
    } else {
        result = run_body(argv[0]);
    }
    if (read_fp_value() != fp_pattern)
        return fail("main completion floating point");
    if (result == EXIT_SUCCESS && pid1) {
        if (syscall(REBOOT_NUMBER, 0xfee1deadU, REBOOT_MAGIC2, 0, 0) != 0)
            return fail("PID1 final CAD_OFF");
        long shutdown_result = syscall(REBOOT_NUMBER, 0xfee1deadU, REBOOT_MAGIC2,
                                       REBOOT_POWER_OFF, 0);
        if (shutdown_result == -1)
            return fail("PID1 POWER_OFF");
        return fail("PID1 POWER_OFF returned");
    }
    if (result == EXIT_SUCCESS && fixture_root) {
        if (kill(getpid(), SIGKILL) != 0)
            return fail("fixture root SIGKILL");
        return fail("fixture root SIGKILL continued");
    }
    return result;
}
