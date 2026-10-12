#define _GNU_SOURCE
#define _POSIX_C_SOURCE 200809L

#include <stdint.h>
#include <errno.h>
#include <fcntl.h>
#include <sched.h>
#include <netinet/in.h>
#include <poll.h>
#include <signal.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/auxv.h>
#include <sys/mount.h>
#include <sys/resource.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <termios.h>
#include <time.h>
#include <unistd.h>

extern char **environ;

static _Thread_local volatile unsigned tls_image = 0x13579bdfU;
static _Thread_local volatile unsigned tls_zero;
static const unsigned char packet_head[] = { 'T', 'L', 'S', 0 };
static const unsigned char packet_tail[] = { 0xff, 'f', 'o', 'r', 'k', '\n' };
static const unsigned long long fp_pattern = 0x400921fb54442d18ULL;

enum { EXEC_CHILD_STATUS = 37 };

/// 输出实际失败原因；成功消息只由已消费子进程状态的父进程产生。
static int fail(const char *operation)
{
    fprintf(stderr, "libc-smoke: %s failed (errno=%d)\n", operation, errno);
    return EXIT_FAILURE;
}

/// 经可观察的内存访问检查小块分配，释放后不保留指针。
static int check_malloc(void)
{
    volatile unsigned char *memory = malloc(257);
    if (memory == NULL)
        return fail("malloc");
    for (size_t i = 0; i < 257; ++i)
        memory[i] = (unsigned char)(i ^ 0xa5);
    for (size_t i = 0; i < 257; ++i) {
        if (memory[i] != (unsigned char)(i ^ 0xa5)) {
            free((void *)memory);
            return fail("malloc contents");
        }
    }
    free((void *)memory);
    return EXIT_SUCCESS;
}

/// 修改模板与零初始化 TLS，在实际调度系统调用返回后逐次核对。
static int check_tls(void)
{
    tls_image = 0x2468ace0U;
    tls_zero = 0x10203040U;
    errno = EDOM;
    for (unsigned i = 0; i < 64; ++i) {
        if (sched_yield() != 0)
            return fail("sched_yield");
        if (tls_image != 0x2468ace0U || tls_zero != 0x10203040U || errno != EDOM)
            return fail("TLS after syscall");
    }
    return EXIT_SUCCESS;
}

/// 在迁核 pipe 窗口只核 TLS/errno，不以重复 yield 代替真实阻塞。
static int check_tls_state(void)
{
    if (tls_image != 0x2468ace0U || tls_zero != 0x10203040U || errno != EDOM)
        return fail("TLS state");
    return EXIT_SUCCESS;
}

/// 触碰大栈的探测保持独立栈帧，未调用的受限启动模式不预留这128KiB。
static __attribute__((noinline)) int check_stack(void)
{
    volatile unsigned char stack[128 * 1024];
    for (size_t offset = 0; offset < sizeof(stack); offset += 4096)
        stack[offset] = (unsigned char)(offset >> 12);
    if (stack[0] != 0 || stack[sizeof(stack) - 4096] != 31)
        return fail("volatile stack");
    return EXIT_SUCCESS;
}

/// 在程序初始化边界把图样写入 ABI callee-saved 浮点寄存器。
static inline __attribute__((always_inline)) void set_fp_value(void)
{
#if defined(__riscv)
    __asm__ volatile("fmv.d.x fs0, %0" : : "r"(fp_pattern) : "fs0", "memory");
#elif defined(__loongarch__)
    __asm__ volatile("movgr2fr.d $f24, %0" : : "r"(fp_pattern) : "$f24", "memory");
#endif
}

/// 从 ABI callee-saved 浮点寄存器读取当前真实位图，不重新写入图样。
static inline __attribute__((always_inline)) unsigned long long read_fp_value(void)
{
    unsigned long long result = 0;
#if defined(__riscv)
    __asm__ volatile("fmv.x.d %0, fs0" : "=r"(result) : : "memory");
#elif defined(__loongarch__)
    __asm__ volatile("movfr2gr.d %0, $f24" : "=r"(result) : : "memory");
#endif
    return result;
}

/// 检查用户 ABI 保留的真实浮点寄存器，不依赖内核填充观测值。
static int check_fp(void)
{
    if (read_fp_value() != fp_pattern)
        return fail("callee-saved floating point");
    return EXIT_SUCCESS;
}

/// 处理合法短读，核对跨 iovec 边界的二进制内容，并读取真实 EOF。
static int read_packet(int fd)
{
    unsigned char head[sizeof(packet_head)];
    unsigned char tail[sizeof(packet_tail)];
    struct iovec vectors[] = {
        { .iov_base = head, .iov_len = sizeof(head) },
        { .iov_base = tail, .iov_len = sizeof(tail) },
    };
    size_t index = 0;
    while (index < 2) {
        ssize_t count = readv(fd, vectors + index, (int)(2 - index));
        if (count < 0 && errno == EINTR)
            continue;
        if (count <= 0)
            return fail("readv packet");
        size_t consumed = (size_t)count;
        while (index < 2 && vectors[index].iov_len <= consumed) {
            consumed -= vectors[index].iov_len;
            ++index;
        }
        if (index < 2) {
            vectors[index].iov_base = (unsigned char *)vectors[index].iov_base + consumed;
            vectors[index].iov_len -= consumed;
        }
    }
    if (memcmp(head, packet_head, sizeof(head)) != 0 ||
        memcmp(tail, packet_tail, sizeof(tail)) != 0)
        return fail("readv contents");

    unsigned char extra;
    struct iovec eof = { .iov_base = &extra, .iov_len = 1 };
    ssize_t count;
    do {
        count = readv(fd, &eof, 1);
    } while (count < 0 && errno == EINTR);
    if (count != 0)
        return fail("pipe EOF");
    return EXIT_SUCCESS;
}

/// fork 子进程检查继承 TLS，发送一笔原子小 writev，再执行磁盘上的同一普通程序。
static void fork_child(int read_fd, int write_fd, const char *executable)
{
    if (close(read_fd) != 0)
        _exit(fail("child close read end"));
    if (tls_image != 0x2468ace0U || tls_zero != 0x10203040U)
        _exit(fail("fork TLS inheritance"));
    tls_image = 7;
    tls_zero = 9;

    struct iovec vectors[] = {
        { .iov_base = (void *)packet_head, .iov_len = sizeof(packet_head) },
        { .iov_base = (void *)packet_tail, .iov_len = sizeof(packet_tail) },
    };
    ssize_t count;
    do {
        count = writev(write_fd, vectors, 2);
    } while (count < 0 && errno == EINTR);
    if (count != (ssize_t)(sizeof(packet_head) + sizeof(packet_tail)))
        _exit(fail("writev packet"));
    if (close(write_fd) != 0)
        _exit(fail("child close write end"));
    execl(executable, executable, "--exec-child", "stage7", (char *)NULL);
    _exit(fail("exec"));
}

/// 把十进制用户参数完整解析为真实描述符，不预设表槽编号。
static int parse_fd(const char *text, int *value)
{
    unsigned long parsed = 0;
    if (text == NULL || text[0] == '\0')
        return -1;
    for (const unsigned char *cursor = (const unsigned char *)text; *cursor != '\0'; ++cursor) {
        if (*cursor < '0' || '9' < *cursor || 127UL / 10 < parsed)
            return -1;
        parsed = parsed * 10 + (*cursor - '0');
    }
    if (127UL < parsed)
        return -1;
    *value = (int)parsed;
    return 0;
}

/// 迁核模式通过两个普通 pipe 逐次握手，不用额外 syscall 或共享测试页。
static int migration_handshake(const char *ready_text, const char *go_text)
{
    int ready_fd;
    int go_fd;
    if (parse_fd(ready_text, &ready_fd) != 0 || parse_fd(go_text, &go_fd) != 0 ||
        ready_fd == go_fd || ready_fd < 0 || go_fd < 0)
        return fail("migration fd arguments");
    for (unsigned round = 0; round < 2; ++round) {
        unsigned char ready = 'R';
        unsigned char go;
        ssize_t written;
        do {
            written = write(ready_fd, &ready, 1);
        } while (written < 0 && errno == EINTR);
        if (written != 1)
            return fail("migration ready write");
        ssize_t received;
        do {
            received = read(go_fd, &go, 1);
        } while (received < 0 && errno == EINTR);
        if (received != 1 || go != 'M')
            return fail("migration wake read");
        if (check_tls_state() != EXIT_SUCCESS || check_fp() != EXIT_SUCCESS)
            return EXIT_FAILURE;
    }
    if (close(ready_fd) != 0 || close(go_fd) != 0)
        return fail("migration close");
    return EXIT_SUCCESS;
}

/// 以真实 AT_EXECFN 和环境核对脚本每层形成的拥有式 argv。
static int check_script_argv(int argc, char **argv)
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

/// 真实消费解释器深度、缺失、权限、格式和参数预算错误。
static int script_errors(void)
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
        result = failed_exec("/test/scripts/no-exec", EACCES, "no-exec", saved);
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

/// 从实际 little-endian statx 字节布局取得 mode，不依赖工具链的 struct statx 定义。
static uint16_t statx_u16(const unsigned char *bytes, size_t offset)
{
    uint16_t value;
    memcpy(&value, bytes + offset, sizeof(value));
    return value;
}

/// 从实际 statx 字节布局取得 32 位 ABI 字段。
static uint32_t statx_u32(const unsigned char *bytes, size_t offset)
{
    uint32_t value;
    memcpy(&value, bytes + offset, sizeof(value));
    return value;
}

/// 从实际 statx 字节布局取得 64 位 ABI 字段。
static uint64_t statx_u64(const unsigned char *bytes, size_t offset)
{
    uint64_t value;
    memcpy(&value, bytes + offset, sizeof(value));
    return value;
}

/// 按 Linux dev_t 编码取得原 fstat 的 major，独立对照 statx 的显式设备字段。
static uint32_t device_major(uint64_t device)
{
    return (uint32_t)(((device >> 8) & 0xfff) | ((device >> 32) & 0xfffff000));
}

/// 按 Linux dev_t 编码取得原 fstat 的 minor。
static uint32_t device_minor(uint64_t device)
{
    return (uint32_t)((device & 0xff) | ((device >> 12) & 0xffffff00));
}

/// 直接执行 statx syscall，核对真实普通文件、console、未选能力与错误后的旧运行资源。
static int check_statx(int fd)
{
    unsigned char path_stat[256] = { 0 };
    unsigned char fd_stat[256] = { 0 };
    unsigned char console_stat[256] = { 0 };
    struct stat old_stat;
    struct stat old_console;
    if (fstat(fd, &old_stat) != 0 || fstat(0, &old_console) != 0)
        return fail("statx independent fstat");
    off_t end = lseek(fd, 0, SEEK_END);
    if (end <= 0)
        return fail("statx independent EOF");
    if (syscall(SYS_statx, AT_FDCWD, "/bin/libc-smoke", 0x800U,
                0x1fffU, path_stat) != 0 ||
        syscall(SYS_statx, fd, "", 0x1000U, 0x7ffU, fd_stat) != 0 ||
        syscall(SYS_statx, 0, "", 0x1000U, 0x7ffU, console_stat) != 0)
        return fail("statx actual queries");
    uint16_t mode = statx_u16(path_stat, 28);
    if ((mode & S_IFMT) != S_IFREG || (mode & 0111) == 0 ||
        mode != (uint16_t)old_stat.st_mode || statx_u32(path_stat, 0) != 0x7ffU ||
        statx_u32(fd_stat, 0) != 0x7ffU || statx_u32(console_stat, 0) != 0x7ffU ||
        statx_u64(path_stat, 32) == 0 || statx_u32(path_stat, 16) == 0 ||
        statx_u64(path_stat, 32) != (uint64_t)old_stat.st_ino ||
        statx_u32(path_stat, 16) != (uint32_t)old_stat.st_nlink ||
        statx_u32(path_stat, 20) != (uint32_t)old_stat.st_uid ||
        statx_u32(path_stat, 24) != (uint32_t)old_stat.st_gid ||
        statx_u64(path_stat, 40) != (uint64_t)end ||
        statx_u64(path_stat, 48) != (uint64_t)old_stat.st_blocks ||
        statx_u32(path_stat, 4) != (uint32_t)old_stat.st_blksize)
        return fail("statx regular attributes");
    if (statx_u16(fd_stat, 28) != mode ||
        statx_u64(fd_stat, 32) != statx_u64(path_stat, 32) ||
        statx_u64(fd_stat, 40) != (uint64_t)end ||
        statx_u32(fd_stat, 16) != statx_u32(path_stat, 16) ||
        statx_u32(fd_stat, 20) != statx_u32(path_stat, 20) ||
        statx_u32(fd_stat, 24) != statx_u32(path_stat, 24))
        return fail("statx empty-path fd identity");
    if (statx_u32(path_stat, 136) != device_major((uint64_t)old_stat.st_dev) ||
        statx_u32(path_stat, 140) != device_minor((uint64_t)old_stat.st_dev) ||
        statx_u32(fd_stat, 136) != statx_u32(path_stat, 136) ||
        statx_u32(fd_stat, 140) != statx_u32(path_stat, 140) ||
        (statx_u16(console_stat, 28) & S_IFMT) != S_IFCHR ||
        statx_u32(console_stat, 128) != 5 || statx_u32(console_stat, 132) != 1 ||
        statx_u32(console_stat, 136) != device_major((uint64_t)old_console.st_dev) ||
        statx_u32(console_stat, 140) != device_minor((uint64_t)old_console.st_dev))
        return fail("statx real device encoding");
    const size_t zero_ranges[][2] = { { 8, 16 }, { 56, 64 }, { 80, 96 }, { 144, 256 } };
    for (size_t range = 0; range < sizeof(zero_ranges) / sizeof(zero_ranges[0]); ++range)
        for (size_t offset = zero_ranges[range][0]; offset < zero_ranges[range][1]; ++offset)
            if (path_stat[offset] != 0 || fd_stat[offset] != 0 || console_stat[offset] != 0)
                return fail("statx unsupported fields");
    errno = 0;
    if (syscall(SYS_statx, AT_FDCWD, "/bin/libc-smoke", 0, 0x80000000U, fd_stat) != -1 ||
        errno != EINVAL)
        return fail("statx reserved request mask");
    errno = 0;
    if (syscall(SYS_statx, AT_FDCWD, "/bin/libc-smoke", 0x6000U, 0x7ffU, fd_stat) != -1 ||
        errno != EINVAL)
        return fail("statx conflicting sync flags");
    errno = 0;
    if (syscall(SYS_statx, AT_FDCWD, "/bin/libc-smoke", 0, 0x7ffU, (void *)NULL) != -1 ||
        errno != EFAULT)
        return fail("statx output fault");
    if (tls_image != 0x2468ace0U || tls_zero != 0x10203040U ||
        read_fp_value() != fp_pattern)
        return fail("statx error preserved user state");
    for (int standard = 0; standard < 3; ++standard)
        if (fcntl(standard, F_GETFD) != 0 ||
            (fcntl(standard, F_GETFL) & O_ACCMODE) != O_RDWR)
            return fail("statx error preserved standard descriptors");
    return EXIT_SUCCESS;
}

/// 核对 root 与非 root 两种真实凭据，并用 fchdir 恢复实际 cwd。
static int abi_check(unsigned uid, unsigned gid)
{
    if ((unsigned)getuid() != uid || (unsigned)geteuid() != uid ||
        (unsigned)getgid() != gid || (unsigned)getegid() != gid)
        return fail("identity syscall");
    if ((unsigned)getauxval(AT_UID) != uid || (unsigned)getauxval(AT_EUID) != uid ||
        (unsigned)getauxval(AT_GID) != gid || (unsigned)getauxval(AT_EGID) != gid)
        return fail("identity auxv");
    char before[4096];
    if (getcwd(before, sizeof(before)) == NULL ||
        strcmp(before, uid == 0 ? "/tmp" : "/") != 0)
        return fail("initial getcwd");
    char short_path[1];
    errno = 0;
    if (getcwd(short_path, sizeof(short_path)) != NULL || errno != ERANGE)
        return fail("getcwd short size");
    int directory = open(".", O_RDONLY | O_DIRECTORY);
    if (directory < 0)
        return fail("open cwd");
    errno = 0;
    if (fchdir(-1) != -1 || errno != EBADF)
        return fail("fchdir bad descriptor");
    int plain = open("/bin/libc-smoke", O_RDONLY);
    if (plain < 0)
        return fail("open non-directory");
    if (check_statx(plain) != EXIT_SUCCESS) {
        close(plain);
        close(directory);
        return EXIT_FAILURE;
    }
    errno = 0;
    if (fchdir(plain) != -1 || errno != ENOTDIR || close(plain) != 0)
        return fail("fchdir non-directory");
    char after[4096];
    if (chdir("/test") != 0 || getcwd(after, sizeof(after)) == NULL ||
        strcmp(after, "/test") != 0)
        return fail("chdir distinct directory");
    if (fchdir(directory) != 0 || close(directory) != 0)
        return fail("fchdir");
    if (getcwd(after, sizeof(after)) == NULL || strcmp(before, after) != 0)
        return fail("fchdir restored cwd");
    if (uid == 0) {
        errno = 0;
        if (umount("/tmp") != -1 || errno != EBUSY)
            return fail("umount cwd Busy");
    }
    if (chdir("/") != 0 || getcwd(after, 2) == NULL || strcmp(after, "/") != 0)
        return fail("getcwd exact size");
    return EXIT_SUCCESS;
}

/// fork、readv/writev、磁盘 exec、wait 与父子 TLS 隔离共同走真实生命周期。
static int run_body(const char *executable)
{
    int descriptors[2];
    if (pipe(descriptors) != 0)
        return fail("pipe");
    pid_t child = fork();
    if (child < 0) {
        int saved_errno = errno;
        close(descriptors[0]);
        close(descriptors[1]);
        errno = saved_errno;
        return fail("fork");
    }
    if (child == 0)
        fork_child(descriptors[0], descriptors[1], executable);

    int result = EXIT_SUCCESS;
    if (close(descriptors[1]) != 0)
        result = fail("parent close write end");
    if (read_packet(descriptors[0]) != EXIT_SUCCESS)
        result = EXIT_FAILURE;
    if (close(descriptors[0]) != 0)
        result = fail("parent close read end");

    int status;
    pid_t reaped;
    do {
        reaped = waitpid(child, &status, 0);
    } while (reaped < 0 && errno == EINTR);
    if (reaped != child || !WIFEXITED(status) || WEXITSTATUS(status) != EXEC_CHILD_STATUS)
        result = fail("waitpid exec child status");
    if (tls_image != 0x2468ace0U || tls_zero != 0x10203040U || errno != EDOM ||
        read_fp_value() != fp_pattern)
        result = fail("parent state isolation");
    if (result != EXIT_SUCCESS)
        return result;
    if (printf("libc-smoke: main argv TLS malloc stdio readv writev fork exec wait passed\n") < 0 ||
        fflush(stdout) != 0)
        return fail("parent stdio");
    return EXIT_SUCCESS;
}

enum {
    REBOOT_NUMBER = 142,
    REBOOT_MAGIC2 = 672274793,
    REBOOT_POWER_OFF = 0x4321fedc,
    ORPHAN_LIVE_STATUS = 41,
    ORPHAN_ZOMBIE_STATUS = 42,
    ORPHAN_PARENT_STATUS = 43,
};

struct orphan_pids {
    pid_t live;
    pid_t zombie;
};

/// 标准流必须指向同一真实 console，保非 CLOEXEC 的 RDWR 属性及实际 termios。
static int check_console(void)
{
    struct stat first;
    if (fstat(STDIN_FILENO, &first) != 0 || !S_ISCHR(first.st_mode) ||
        device_major((uint64_t)first.st_rdev) != 5 ||
        device_minor((uint64_t)first.st_rdev) != 1)
        return fail("standard console device");
    for (int fd = 0; fd < 3; ++fd) {
        struct stat current;
        struct termios attributes;
        if (fstat(fd, &current) != 0 || !S_ISCHR(current.st_mode) ||
            current.st_dev != first.st_dev || current.st_ino != first.st_ino ||
            current.st_rdev != first.st_rdev || fcntl(fd, F_GETFD) != 0 ||
            (fcntl(fd, F_GETFL) & O_ACCMODE) != O_RDWR ||
            tcgetattr(fd, &attributes) != 0)
            return fail("standard console attributes");
    }
    if (printf("libc-smoke: console fds=0,1,2 RDWR character=5:1\n") < 0 ||
        fflush(stdout) != 0)
        return fail("root role stdio");
    return EXIT_SUCCESS;
}

/// 权限优先于 magic 与命令检查，每项都消费正式 syscall 的返回和 errno。
static int check_reboot_errors(int system_root)
{
    unsigned uid = (unsigned)getuid();
    unsigned gid = (unsigned)getgid();
    if ((uid != 0 && uid != 1000) || gid != (uid == 0 ? 0U : 2000U) ||
        (unsigned)geteuid() != uid || (unsigned)getegid() != gid ||
        getauxval(AT_UID) != uid || getauxval(AT_EUID) != uid ||
        getauxval(AT_GID) != gid || getauxval(AT_EGID) != gid ||
        (system_root && uid != 0))
        return fail("root role credentials");
    const uint32_t invalid[][3] = {
        { 0, REBOOT_MAGIC2, 0 },
        { 0xfee1deadU, 0, 0 },
        { 0xfee1deadU, REBOOT_MAGIC2, 0x12345678U },
        { 0xfee1deadU, REBOOT_MAGIC2, 0x89abcdefU },
    };
    for (size_t i = 0; i < sizeof(invalid) / sizeof(invalid[0]); ++i) {
        errno = 0;
        if (syscall(REBOOT_NUMBER, invalid[i][0], invalid[i][1], invalid[i][2], 0) != -1 ||
            errno != (uid == 0 ? EINVAL : EPERM))
            return fail("reboot invalid argument errno");
    }
    if (!system_root) {
        errno = 0;
        if (syscall(REBOOT_NUMBER, 0xfee1deadU, REBOOT_MAGIC2, REBOOT_POWER_OFF, 0) != -1 ||
            errno != (uid == 0 ? EINVAL : EPERM))
            return fail("fixture reboot role errno");
    }
    const uint32_t second_magics[] = { REBOOT_MAGIC2, 85072278U, 369367448U, 537993216U };
    for (size_t i = 0; i < sizeof(second_magics) / sizeof(second_magics[0]); ++i) {
        errno = 0;
        long result = syscall(REBOOT_NUMBER, 0xfee1deadU, second_magics[i], 0, 0);
        if ((uid == 0 && result != 0) ||
            (uid != 0 && (result != -1 || errno != EPERM)))
            return fail("CAD_OFF permission or magic");
    }
    return EXIT_SUCCESS;
}

/// 阻塞 pipe 的有限消息保留短读写和 EINTR 语义，EOF 不能当作成功握手。
static int transfer_pipe(int fd, void *bytes, size_t length, int writing)
{
    size_t offset = 0;
    while (offset < length) {
        ssize_t count = writing ? write(fd, (unsigned char *)bytes + offset, length - offset) :
                                  read(fd, (unsigned char *)bytes + offset, length - offset);
        if (count < 0 && errno == EINTR)
            continue;
        if (count <= 0)
            return fail("orphan pipe transfer");
        offset += (size_t)count;
    }
    return EXIT_SUCCESS;
}

/// 精确消费子 PID 与完整 wait 编码，不用截出的 exit byte 掩盖信号或停止事件。
static int wait_status(pid_t child, int expected)
{
    int status;
    pid_t result;
    do {
        result = waitpid(child, &status, 0);
    } while (result < 0 && errno == EINTR);
    if (result != child || status != expected)
        return fail("root role full wait status");
    return EXIT_SUCCESS;
}

/// 普通子组的默认 TERM 必须终止真实执行，系统根的角色不会被 fork 子继承。
static int check_default_signal(void)
{
    pid_t child = fork();
    if (child < 0)
        return fail("default signal fork");
    if (child == 0) {
        if (kill(getpid(), SIGTERM) != 0)
            _exit(fail("default signal kill"));
        _exit(fail("default signal continued"));
    }
    return wait_status(child, SIGTERM);
}

/// 中间父先保一个活子，再用 WNOWAIT 确认另一子已经退出；最后成员退出才交出亲缘。
static void orphan_parent(int report[2], int release[2])
{
    if (close(report[0]) != 0 || close(release[1]) != 0)
        _exit(fail("orphan parent pipe close"));
    struct orphan_pids children;
    children.live = fork();
    if (children.live < 0)
        _exit(fail("live orphan fork"));
    if (children.live == 0) {
        if (close(report[1]) != 0)
            _exit(fail("live orphan report close"));
        unsigned char go;
        if (transfer_pipe(release[0], &go, 1, 0) != EXIT_SUCCESS || go != 'G' ||
            getppid() != 1 || tls_image != 0x2468ace0U || tls_zero != 0x10203040U ||
            check_fp() != EXIT_SUCCESS || close(release[0]) != 0)
            _exit(fail("live orphan adopted state"));
        _exit(ORPHAN_LIVE_STATUS);
    }
    if (close(release[0]) != 0)
        _exit(fail("orphan parent release close"));
    children.zombie = fork();
    if (children.zombie < 0)
        _exit(fail("zombie orphan fork"));
    if (children.zombie == 0)
        _exit(ORPHAN_ZOMBIE_STATUS);
    siginfo_t information;
    int result;
    do {
        result = waitid(P_PID, (id_t)children.zombie, &information, WEXITED | WNOWAIT);
    } while (result < 0 && errno == EINTR);
    if (result != 0 || information.si_pid != children.zombie ||
        information.si_code != CLD_EXITED || information.si_status != ORPHAN_ZOMBIE_STATUS)
        _exit(fail("zombie WNOWAIT observation"));
    if (transfer_pipe(report[1], &children, sizeof(children), 1) != EXIT_SUCCESS ||
        close(report[1]) != 0)
        _exit(fail("orphan parent report"));
    _exit(ORPHAN_PARENT_STATUS);
}

/// PID1 等中间父完整退出后消费已转亲 zombie，再放行仍活的孤儿并消费它的真实状态。
static int check_adopted_orphans(void)
{
    int report[2];
    int release[2];
    if (pipe(report) != 0)
        return fail("orphan report pipe");
    if (pipe(release) != 0) {
        close(report[0]);
        close(report[1]);
        return fail("orphan release pipe");
    }
    pid_t parent = fork();
    if (parent < 0) {
        close(report[0]);
        close(report[1]);
        close(release[0]);
        close(release[1]);
        return fail("orphan parent fork");
    }
    if (parent == 0)
        orphan_parent(report, release);
    struct orphan_pids children;
    if (close(report[1]) != 0 || close(release[0]) != 0 ||
        transfer_pipe(report[0], &children, sizeof(children), 0) != EXIT_SUCCESS ||
        close(report[0]) != 0 || children.live <= 1 || children.zombie <= 1 ||
        children.live == children.zombie || children.live == parent || children.zombie == parent ||
        wait_status(parent, ORPHAN_PARENT_STATUS << 8) != EXIT_SUCCESS ||
        wait_status(children.zombie, ORPHAN_ZOMBIE_STATUS << 8) != EXIT_SUCCESS)
        return fail("zombie orphan adoption");
    unsigned char go = 'G';
    if (transfer_pipe(release[1], &go, 1, 1) != EXIT_SUCCESS || close(release[1]) != 0 ||
        wait_status(children.live, ORPHAN_LIVE_STATUS << 8) != EXIT_SUCCESS)
        return fail("live orphan adoption");
    int status;
    errno = 0;
    if (waitpid(-1, &status, WNOHANG) != -1 || errno != ECHILD)
        return fail("PID1 final child ownership");
    if (printf("libc-smoke: PID1 adopted live=%ld status=%d zombie=%ld status=%d parent=%ld status=%d\n",
               (long)children.live, ORPHAN_LIVE_STATUS << 8,
               (long)children.zombie, ORPHAN_ZOMBIE_STATUS << 8,
               (long)parent, ORPHAN_PARENT_STATUS << 8) < 0 || fflush(stdout) != 0)
        return fail("PID1 adoption stdio");
    return EXIT_SUCCESS;
}

/// 数值身份与角色分别核验；系统根在 KILL、STOP、默认 TERM 后继续普通用户指令。
static int root_role_body(int system_root)
{
    if (getpid() != 1 || getppid() != 0)
        return fail("root pid and ppid");
    if (check_console() != EXIT_SUCCESS || check_reboot_errors(system_root) != EXIT_SUCCESS ||
        check_default_signal() != EXIT_SUCCESS)
        return EXIT_FAILURE;
    if (system_root) {
        volatile unsigned continued = 0;
        if (kill(getpid(), SIGKILL) != 0)
            return fail("PID1 KILL");
        continued += 1;
        if (kill(getpid(), SIGSTOP) != 0)
            return fail("PID1 STOP");
        continued += 2;
        if (kill(getpid(), SIGTERM) != 0)
            return fail("PID1 default TERM");
        continued += 4;
        if (continued != 7 || tls_image != 0x2468ace0U || tls_zero != 0x10203040U)
            return fail("PID1 continued user state");
        return check_adopted_orphans();
    }
    return EXIT_SUCCESS;
}

/// 检查 uname 的完整字节布局和跨页 EFAULT，失败复制仍保留真实已写前缀。
static int check_uname(void)
{
    enum { FIELD_BYTES = 65, IDENTITY_BYTES = 6 * FIELD_BYTES };
    unsigned char identity[IDENTITY_BYTES];
    memset(identity, 0xa5, sizeof(identity));
    if (syscall(SYS_uname, identity) != 0)
        return fail("uname");
    for (size_t field = 0; field < 6; ++field) {
        unsigned char *start = identity + field * FIELD_BYTES;
        unsigned char *zero = memchr(start, 0, FIELD_BYTES);
        if (zero == NULL)
            return fail("uname field terminator");
        for (unsigned char *tail = zero; tail < start + FIELD_BYTES; ++tail)
            if (*tail != 0)
                return fail("uname field zero tail");
    }
#if defined(__riscv)
    const char *machine = "riscv64";
#elif defined(__loongarch__)
    const char *machine = "loongarch64";
#endif
    if (strcmp((char *)identity + 4 * FIELD_BYTES, machine) != 0)
        return fail("uname target machine");
    errno = 0;
    if (syscall(SYS_uname, (void *)NULL) != -1 || errno != EFAULT)
        return fail("uname first byte fault");
    long page_size = sysconf(_SC_PAGESIZE);
    if (page_size < IDENTITY_BYTES)
        return fail("uname page size");
    size_t page = (size_t)page_size;
    unsigned char *memory = mmap(NULL, 2 * page, PROT_READ | PROT_WRITE,
                                MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (memory == MAP_FAILED)
        return fail("uname fault mapping");
    if (munmap(memory + page, page) != 0)
        return fail("uname guard unmap");
    size_t prefix = 3 * FIELD_BYTES;
    unsigned char *boundary = memory + page - prefix;
    memset(boundary, 0xa5, prefix);
    errno = 0;
    if (syscall(SYS_uname, boundary) != -1 || errno != EFAULT ||
        memcmp(boundary, identity, prefix) != 0)
        return fail("uname partial byte fault");
    if (munmap(memory, page) != 0)
        return fail("uname fault mapping release");
    return EXIT_SUCCESS;
}

/// 子进程保旧 cwd 和目录 FD，换根后绝对名字使用新根，再显式进入新根目录。
static int chroot_child(int outside)
{
    if (chroot("/test") != 0)
        return fail("chroot directory");
    if (umask(027) != 027)
        return fail("chroot preserved umask");
    char path[4096];
    errno = 0;
    if (getcwd(path, sizeof(path)) != NULL || errno != ENOENT)
        return fail("chroot preserved outside cwd");
    struct stat metadata;
    if (stat("/scripts", &metadata) != 0 || !S_ISDIR(metadata.st_mode))
        return fail("chroot absolute lookup");
    int executable = openat(outside, "bin/busybox", O_RDONLY);
    unsigned char header[7];
    if (executable < 0 || read(executable, header, sizeof(header)) != sizeof(header) ||
        memcmp(header, "\x7f" "ELF\x02\x01\x01", sizeof(header)) != 0 ||
        close(executable) != 0)
        return fail("chroot old directory descriptor");
    if (fchdir(outside) != 0)
        return fail("chroot outside fchdir");
    errno = 0;
    if (getcwd(path, sizeof(path)) != NULL || errno != ENOENT)
        return fail("chroot outside fchdir cwd");
    if (close(outside) != 0 || chdir("/") != 0 ||
        getcwd(path, sizeof(path)) == NULL || strcmp(path, "/") != 0)
        return fail("chroot explicit cwd");
    return EXIT_SUCCESS;
}

/// 普通 fork 消费独立 FS 视图，拒绝换根不提交，父视图和创建掩码保持原值。
static int runtime_view(void)
{
    if (check_uname() != EXIT_SUCCESS || chdir("/") != 0)
        return EXIT_FAILURE;
    char before[4096], after[4096];
    if (getcwd(before, sizeof(before)) == NULL)
        return fail("runtime parent cwd");
    errno = 0;
    if (chroot("/bin/busybox") != -1 || errno != ENOTDIR)
        return fail("chroot non-directory");
    errno = 0;
    if (chroot("/test/no-such-runtime-root") != -1 || errno != ENOENT)
        return fail("chroot missing directory");
    if (getuid() != 0) {
        errno = 0;
        if (chroot("/test") != -1 || errno != EPERM)
            return fail("chroot permission");
    } else {
        int outside = open("/", O_RDONLY | O_DIRECTORY);
        if (outside < 0)
            return fail("runtime parent directory");
        mode_t saved_mask = umask(027);
        pid_t child = fork();
        if (child < 0)
            return fail("runtime view fork");
        if (child == 0)
            _exit(chroot_child(outside));
        if (close(outside) != 0 || wait_status(child, 0) != EXIT_SUCCESS ||
            umask(saved_mask) != 027)
            return fail("runtime view child completion");
    }
    struct stat metadata;
    if (getcwd(after, sizeof(after)) == NULL || strcmp(before, after) != 0 ||
        stat("/bin/busybox", &metadata) != 0 || !S_ISREG(metadata.st_mode))
        return fail("runtime parent root preserved");
    if (printf("libc-smoke: uname bytes and independent chroot view passed uid=%ld\n",
               (long)getuid()) < 0 || fflush(stdout) != 0)
        return fail("runtime view output");
    return EXIT_SUCCESS;
}

/// 经普通线程修改同组软限额，join 后由创建者读取实际共享值。
static void *limit_thread(void *unused)
{
    (void)unused;
    struct rlimit limit = { .rlim_cur = 96 * 1024, .rlim_max = 100 * 1024 };
    return (void *)(uintptr_t)(setrlimit(RLIMIT_STACK, &limit) != 0);
}

/// 新 exec 栈的最低页能交付 pipe，guard 页返回 EFAULT，不能用限额 getter 代替范围验证。
static int limited_stack(void)
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
static int runtime_limits(const char *program)
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

/// 比较标准 timespec 的有符号秒与纳秒，负秒不按无符号时间解释。
static int same_file_time(struct timespec left, struct timespec right)
{
    return left.tv_sec == right.tv_sec && left.tv_nsec == right.tv_nsec;
}

/// 时钟前后夹住 NOW 的实际值，不能仅凭时间非零判断提交成功。
static int file_time_before(struct timespec left, struct timespec right)
{
    return left.tv_sec < right.tv_sec ||
           (left.tv_sec == right.tv_sec && left.tv_nsec <= right.tv_nsec);
}

/// 时间修改保留文件身份、权限、内容长度和块计数，不比较 stat 的填充字节。
static int same_file_attributes(const struct stat *left, const struct stat *right)
{
    return left->st_dev == right->st_dev && left->st_ino == right->st_ino &&
           left->st_mode == right->st_mode && left->st_uid == right->st_uid &&
           left->st_gid == right->st_gid && left->st_nlink == right->st_nlink &&
           left->st_size == right->st_size && left->st_blocks == right->st_blocks &&
           left->st_rdev == right->st_rdev;
}

/// 错误和双 OMIT 必须保留三个真实时间与原属性，包含不能被失败调用更新的 ctime。
static int file_times_preserved(int fd, const struct stat *before)
{
    struct stat after;
    if (fstat(fd, &after) != 0 || !same_file_attributes(before, &after) ||
        !same_file_time(before->st_atim, after.st_atim) ||
        !same_file_time(before->st_mtim, after.st_mtim) ||
        !same_file_time(before->st_ctim, after.st_ctim))
        return fail("file times preserved");
    return EXIT_SUCCESS;
}

/// NULL 和双 NOW 经真实 futimens 更新，三时间必须取同一提交时刻。
static int file_times_now(int fd, const struct timespec *times)
{
    struct timespec before, after;
    struct stat initial, changed;
    if (fstat(fd, &initial) != 0 || clock_gettime(CLOCK_REALTIME, &before) != 0 ||
        futimens(fd, times) != 0 || clock_gettime(CLOCK_REALTIME, &after) != 0 ||
        fstat(fd, &changed) != 0 || !same_file_attributes(&initial, &changed) ||
        !file_time_before(before, changed.st_atim) ||
        !file_time_before(changed.st_atim, after) ||
        !same_file_time(changed.st_atim, changed.st_mtim) ||
        !same_file_time(changed.st_atim, changed.st_ctim))
        return fail("futimens current times");
    return EXIT_SUCCESS;
}

/// 普通可写与不可写的非所有者文件区分显式 EPERM 和 touch 的真实 DAC。
static int file_times_permissions(void)
{
    const char *paths[] = {
        "/test/runtime-times/foreign-write", "/test/runtime-times/foreign-read",
    };
    struct timespec explicit[2] = { { -123, 456789012 }, { 456, 123456789 } };
    struct timespec now[2] = { { 0, UTIME_NOW }, { 0, UTIME_NOW } };
    for (size_t index = 0; index < 2; ++index) {
        int fd = open(paths[index], O_RDONLY);
        struct stat initial;
        if (fd < 0 || fstat(fd, &initial) != 0 || initial.st_uid == getuid())
            return fail("foreign time fixture");
        errno = 0;
        int result = futimens(fd, explicit);
        if (getuid() == 0) {
            struct stat changed;
            if (result != 0 || fstat(fd, &changed) != 0 ||
                !same_file_attributes(&initial, &changed) ||
                !same_file_time(changed.st_atim, explicit[0]) ||
                !same_file_time(changed.st_mtim, explicit[1]))
                return fail("root explicit foreign times");
        } else {
            if (result != -1 || errno != EPERM ||
                file_times_preserved(fd, &initial) != EXIT_SUCCESS)
                return fail("nonowner explicit times");
            struct timespec mixed[2] = { { 0, UTIME_NOW }, { 0, UTIME_OMIT } };
            errno = 0;
            if (futimens(fd, mixed) != -1 || errno != EPERM ||
                file_times_preserved(fd, &initial) != EXIT_SUCCESS)
                return fail("nonowner NOW OMIT times");
        }
        if (getuid() == 0 || index == 0) {
            if (file_times_now(fd, NULL) != EXIT_SUCCESS ||
                file_times_now(fd, now) != EXIT_SUCCESS)
                return EXIT_FAILURE;
            struct timespec saved[2] = { initial.st_atim, initial.st_mtim };
            // 非所有者只能 touch；恢复显式时间仍须由真实所有者或 root 完成。
            if (getuid() == 0 && futimens(fd, saved) != 0)
                return fail("foreign time restore");
        } else {
            errno = 0;
            if (futimens(fd, NULL) != -1 || errno != EACCES ||
                file_times_preserved(fd, &initial) != EXIT_SUCCESS)
                return fail("nonowner readonly NULL times");
            errno = 0;
            if (futimens(fd, now) != -1 || errno != EACCES ||
                file_times_preserved(fd, &initial) != EXIT_SUCCESS)
                return fail("nonowner readonly NOW times");
        }
        if (close(fd) != 0)
            return fail("foreign time close");
    }
    return EXIT_SUCCESS;
}

/// 纳秒与用户复制错误在提交前结束；跨页输入完整导入失败不能留下半个时间更新。
static int file_times_errors(int fd, const char *path)
{
    struct stat initial;
    struct timespec times[2] = { { 123, 42 }, { 456, 1000000000 } };
    if (fstat(fd, &initial) != 0)
        return fail("file time error baseline");
    errno = 0;
    if (utimensat(AT_FDCWD, path, times, 0) != -1 || errno != EINVAL)
        return fail("file time large nanoseconds");
    times[1].tv_nsec = -1;
    errno = 0;
    if (futimens(fd, times) != -1 || errno != EINVAL)
        return fail("file time negative nanoseconds");
    times[1].tv_nsec = 99;
    errno = 0;
    if (syscall(SYS_utimensat, fd, NULL, times, AT_SYMLINK_NOFOLLOW) != -1 ||
        errno != EINVAL)
        return fail("file time NULL fd flags");
    errno = 0;
    if (syscall(SYS_utimensat, AT_FDCWD, NULL, times, 0) != -1 || errno != EFAULT)
        return fail("file time NULL cwd pathname");
    errno = 0;
    if (syscall(SYS_utimensat, AT_FDCWD, path, times, 2) != -1 || errno != EINVAL)
        return fail("file time unknown flags");
    errno = 0;
    if (syscall(SYS_utimensat, -1, NULL, times, 0) != -1 || errno != EBADF)
        return fail("file time invalid fd");
    if (file_times_preserved(fd, &initial) != EXIT_SUCCESS)
        return EXIT_FAILURE;

    long page_size = sysconf(_SC_PAGESIZE);
    if (page_size < (long)sizeof(times))
        return fail("file time fault page size");
    size_t page = (size_t)page_size;
    unsigned char *memory = mmap(NULL, 2 * page, PROT_READ | PROT_WRITE,
                                MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (memory == MAP_FAILED)
        return fail("file time fault mapping");
    if (munmap(memory + page, page) != 0)
        return fail("file time guard unmap");
    memcpy(memory + page - sizeof(times[0]), &times[0], sizeof(times[0]));
    errno = 0;
    if (syscall(SYS_utimensat, fd, NULL, memory + page - sizeof(times[0]), 0) != -1 ||
        errno != EFAULT || file_times_preserved(fd, &initial) != EXIT_SUCCESS)
        return fail("file time partial input fault");
    errno = 0;
    if (syscall(SYS_utimensat, fd, NULL, memory + page, 0) != -1 || errno != EFAULT)
        return fail("file time first input fault");
    errno = 0;
    if (syscall(SYS_utimensat, AT_FDCWD, memory + page, times, 0) != -1 ||
        errno != EFAULT || file_times_preserved(fd, &initial) != EXIT_SUCCESS)
        return fail("file time pathname fault");
    struct timespec omit[2] = { { INT64_MIN, UTIME_OMIT }, { INT64_MAX, UTIME_OMIT } };
    if (syscall(SYS_utimensat, -1, memory + page, omit, -1) != 0 ||
        file_times_preserved(fd, &initial) != EXIT_SUCCESS)
        return fail("file time double OMIT inaccessible pathname");
    if (utimensat(AT_FDCWD, "/test/runtime-times/absent", omit, 0) != 0 ||
        file_times_preserved(fd, &initial) != EXIT_SUCCESS || munmap(memory, page) != 0)
        return fail("file time double OMIT missing pathname");
    return EXIT_SUCCESS;
}

/// 真实文件的显式时间、OMIT、fd NULL 和末链接各自读回，文件字节与游标保持不变。
static int check_file_times(int fd, int directory, const char *name, const char *path,
                            const char *link_path)
{
    const char payload[] = "file-time-content";
    char contents[sizeof(payload)];
    struct timespec times[2] = { { -123456, 123456789 }, { 2147483648LL, 987654321 } };
    struct stat initial, changed;
    struct timespec before, after;
    if (write(fd, payload, sizeof(payload)) != (ssize_t)sizeof(payload) ||
        fstat(fd, &initial) != 0 || clock_gettime(CLOCK_REALTIME, &before) != 0 ||
        utimensat(directory, name, times, 0) != 0 ||
        clock_gettime(CLOCK_REALTIME, &after) != 0 || fstat(fd, &changed) != 0 ||
        !same_file_attributes(&initial, &changed) ||
        !file_time_before(before, changed.st_ctim) ||
        !file_time_before(changed.st_ctim, after) ||
        !same_file_time(changed.st_atim, times[0]) ||
        !same_file_time(changed.st_mtim, times[1]))
        return fail("relative explicit file times");
    initial = changed;
    times[0].tv_nsec = UTIME_OMIT;
    times[1] = (struct timespec){ -654321, 999999999 };
    if (futimens(fd, times) != 0 || fstat(fd, &changed) != 0 ||
        !same_file_attributes(&initial, &changed) ||
        !same_file_time(initial.st_atim, changed.st_atim) ||
        !same_file_time(times[1], changed.st_mtim))
        return fail("file atime OMIT");
    initial = changed;
    times[0] = (struct timespec){ 7654321, 1234567 };
    times[1].tv_nsec = UTIME_OMIT;
    // ABI 的 EMPTY_PATH 位直接交给 raw syscall，不依赖 libc 包装层对空名字的限制。
    if (syscall(SYS_utimensat, fd, "", times, 0x1000) != 0 ||
        fstat(fd, &changed) != 0 || !same_file_attributes(&initial, &changed) ||
        !same_file_time(times[0], changed.st_atim) ||
        !same_file_time(initial.st_mtim, changed.st_mtim))
        return fail("file mtime OMIT empty path");
    if (file_times_errors(fd, path) != EXIT_SUCCESS)
        return EXIT_FAILURE;
    if (fchmod(fd, 0) != 0 || file_times_now(fd, NULL) != EXIT_SUCCESS)
        return fail("owner no-write current times");
    struct timespec now[2] = { { INT64_MIN, UTIME_NOW }, { INT64_MAX, UTIME_NOW } };
    if (file_times_now(fd, now) != EXIT_SUCCESS ||
        syscall(SYS_utimensat, fd, NULL, times, 0) != 0)
        return fail("owner no-write explicit fd times");
    if (symlink(name, link_path) != 0 || fstat(fd, &initial) != 0)
        return fail("file time symlink create");
    times[0] = (struct timespec){ -777, 111222333 };
    times[1] = (struct timespec){ 888, 444555666 };
    if (utimensat(AT_FDCWD, link_path, times, AT_SYMLINK_NOFOLLOW) != 0 ||
        lstat(link_path, &changed) != 0 || !S_ISLNK(changed.st_mode) ||
        !same_file_time(times[0], changed.st_atim) ||
        !same_file_time(times[1], changed.st_mtim) ||
        file_times_preserved(fd, &initial) != EXIT_SUCCESS)
        return fail("file time nofollow symlink");
    if (utimensat(AT_FDCWD, link_path, times, 0) != 0 || fstat(fd, &changed) != 0 ||
        !same_file_attributes(&initial, &changed) ||
        !same_file_time(times[0], changed.st_atim) ||
        !same_file_time(times[1], changed.st_mtim))
        return fail("file time follow symlink");
    if (lseek(fd, 0, SEEK_CUR) != (off_t)sizeof(payload) ||
        pread(fd, contents, sizeof(contents), 0) != (ssize_t)sizeof(contents) ||
        memcmp(payload, contents, sizeof(payload)) != 0 || fsync(fd) != 0)
        return fail("file time contents position and sync");
    return file_times_permissions();
}

/// 活执行映像允许属性更新，读 fd 不取得被 ExecAccess 排斥的内容写能力。
static int file_times_executable(const char *program)
{
    if (getuid() != 0)
        return EXIT_SUCCESS;
    int fd = open(program, O_RDONLY);
    struct stat initial;
    if (fd < 0 || fstat(fd, &initial) != 0)
        return fail("executable time open");
    struct timespec times[2] = { initial.st_atim, initial.st_mtim };
    int result = futimens(fd, times);
    struct stat after;
    if (result != 0 || fstat(fd, &after) != 0 ||
        !same_file_attributes(&initial, &after) ||
        !same_file_time(initial.st_atim, after.st_atim) ||
        !same_file_time(initial.st_mtim, after.st_mtim))
        result = fail("executable time attribute update");
    if (close(fd) != 0)
        return fail("executable time close");
    return result;
}

/// 独立 RW 目录保存普通测试文件，成功与失败均撤销本次创建的名字和 fd。
static int runtime_file_times(const char *program)
{
    char name[64], path[128], link_path[128];
    int count = snprintf(name, sizeof(name), "owned-%ld", (long)getpid());
    if (count < 0 || sizeof(name) <= (size_t)count)
        return fail("file time name");
    count = snprintf(path, sizeof(path), "/test/runtime-times/%s", name);
    if (count < 0 || sizeof(path) <= (size_t)count)
        return fail("file time path");
    count = snprintf(link_path, sizeof(link_path), "/test/runtime-times/link-%ld",
                     (long)getpid());
    if (count < 0 || sizeof(link_path) <= (size_t)count)
        return fail("file time link path");
    int directory = open("/test/runtime-times", O_RDONLY | O_DIRECTORY);
    if (directory < 0)
        return fail("file time directory");
    int fd = openat(directory, name, O_RDWR | O_CREAT | O_EXCL, 0600);
    if (fd < 0) {
        close(directory);
        return fail("file time create");
    }
    int result = check_file_times(fd, directory, name, path, link_path);
    if (result == EXIT_SUCCESS)
        result = file_times_executable(program);
    if (close(fd) != 0)
        result = fail("file time close");
    if (close(directory) != 0)
        result = fail("file time directory close");
    if (unlink(path) != 0)
        result = fail("file time unlink");
    if (unlink(link_path) != 0 && errno != ENOENT)
        result = fail("file time link cleanup");
    if (result == EXIT_SUCCESS &&
        (printf("libc-smoke: real file times and permissions passed uid=%ld\n",
                (long)getuid()) < 0 || fflush(stdout) != 0))
        return fail("file time output");
    return result;
}

/// 把实际时钟样本转换为纳秒，拒绝无效字段和不能表示的时间。
static int runtime_nanoseconds(clockid_t clock, uint64_t *value)
{
    struct timespec sample;
    if (clock_gettime(clock, &sample) != 0)
        return fail("runtime clock sample");
    if (sample.tv_sec < 0 || sample.tv_nsec < 0 || 1000000000L <= sample.tv_nsec ||
        (UINT64_MAX - (uint64_t)sample.tv_nsec) / 1000000000ULL <
        (uint64_t)sample.tv_sec)
        return fail("runtime clock fields");
    *value = (uint64_t)sample.tv_sec * 1000000000ULL + (uint64_t)sample.tv_nsec;
    return EXIT_SUCCESS;
}

/// 期限检查容许调度迟到，只排除提前完成和数量级错误的等待。
static int runtime_elapsed(uint64_t start, uint64_t minimum, const char *operation)
{
    uint64_t end;
    if (runtime_nanoseconds(CLOCK_MONOTONIC, &end) != EXIT_SUCCESS)
        return EXIT_FAILURE;
    if (end < start || end - start < minimum || 10000000000ULL < end - start) {
        fprintf(stderr, "libc-smoke: %s elapsed_ns=%llu minimum_ns=%llu\n",
                operation, (unsigned long long)(end - start),
                (unsigned long long)minimum);
        return fail(operation);
    }
    return EXIT_SUCCESS;
}

/// 查询同一端点的实际量化 timeval，同时检查完整 LP64 输出长度。
static int socket_timeout_value(int fd, int option, time_t seconds, suseconds_t usec)
{
    struct timeval value = { -1, -1 };
    socklen_t length = sizeof(value);
    if (getsockopt(fd, SOL_SOCKET, option, &value, &length) != 0 ||
        length != sizeof(value) || value.tv_sec != seconds || value.tv_usec != usec)
        return fail(option == SO_RCVTIMEO ? "SO_RCVTIMEO value" : "SO_SNDTIMEO value");
    return EXIT_SUCCESS;
}

/// 修改真实 socket 控制属性，不改变 fd 状态或用用户空间 timer 代替期限。
static int socket_timeout_set(int fd, int option, time_t seconds, suseconds_t usec)
{
    struct timeval value = { seconds, usec };
    if (setsockopt(fd, SOL_SOCKET, option, &value, sizeof(value)) != 0)
        return fail(option == SO_RCVTIMEO ? "SO_RCVTIMEO set" : "SO_SNDTIMEO set");
    return EXIT_SUCCESS;
}

/// 一组控制请求覆盖真实复制、截断和转换错误；失败 setter 必须保留旧属性。
static int socket_timeout_options(int fd)
{
    _Static_assert(sizeof(struct timeval) == 16, "socket timeval LP64 ABI");
    _Static_assert(sizeof(socklen_t) == 4, "socket optlen ABI");
    const int options[] = { SO_RCVTIMEO, SO_SNDTIMEO };
    for (size_t index = 0; index < sizeof(options) / sizeof(options[0]); ++index) {
        int option = options[index];
        struct timeval input = { 0, 50001 };
        struct {
            struct timeval value;
            uint64_t guard;
        } output;
        memset(&output, 0xa5, sizeof(output));
        socklen_t length = sizeof(output);
        if (socket_timeout_value(fd, option, 0, 0) != EXIT_SUCCESS ||
            setsockopt(fd, SOL_SOCKET, option, &input, sizeof(input)) != 0 ||
            getsockopt(fd, SOL_SOCKET, option, &output, &length) != 0 ||
            length != 16 || output.value.tv_sec != 0 || output.value.tv_usec != 60000 ||
            output.guard != UINT64_C(0xa5a5a5a5a5a5a5a5))
            return fail("socket timeout full GET and quantization");
        memset(&output, 0xa5, sizeof(output));
        length = 8;
        if (getsockopt(fd, SOL_SOCKET, option, &output, &length) != 0 ||
            length != 8 || output.value.tv_sec != 0 ||
            (uint64_t)output.value.tv_usec != UINT64_C(0xa5a5a5a5a5a5a5a5))
            return fail("socket timeout short GET");
        length = 0;
        if (getsockopt(fd, SOL_SOCKET, option, &output, &length) != 0 || length != 0)
            return fail("socket timeout zero GET length");
        errno = 0;
        if (setsockopt(fd, SOL_SOCKET, option, &input, 15) != -1 || errno != EINVAL)
            return fail("socket timeout short SET EINVAL");
        errno = 0;
        if (syscall(SYS_setsockopt, fd, SOL_SOCKET, option, 1UL, 16UL) != -1 ||
            errno != EFAULT)
            return fail("socket timeout SET EFAULT");
        length = 16;
        errno = 0;
        if (syscall(SYS_getsockopt, fd, SOL_SOCKET, option, 1UL, &length) != -1 ||
            errno != EFAULT)
            return fail("socket timeout GET EFAULT");
        errno = 0;
        if (syscall(SYS_getsockopt, fd, SOL_SOCKET, option, &output, 1UL) != -1 ||
            errno != EFAULT)
            return fail("socket timeout optlen EFAULT");
        length = (socklen_t)-1;
        errno = 0;
        if (getsockopt(fd, SOL_SOCKET, option, &output, &length) != -1 || errno != EINVAL)
            return fail("socket timeout negative optlen EINVAL");
        const suseconds_t invalid[] = { -1, 1000000 };
        for (size_t bad = 0; bad < sizeof(invalid) / sizeof(invalid[0]); ++bad) {
            input.tv_sec = -1;
            input.tv_usec = invalid[bad];
            errno = 0;
            if (setsockopt(fd, SOL_SOCKET, option, &input, sizeof(input)) != -1 ||
                errno != 33 || errno != EDOM)
                return fail("socket timeout usec EDOM33");
        }
        if (socket_timeout_value(fd, option, 0, 60000) != EXIT_SUCCESS)
            return fail("socket timeout failed SET preserved");
        input.tv_sec = 1;
        input.tv_usec = 1;
        memcpy(&output.value, &input, sizeof(input));
        if (setsockopt(fd, SOL_SOCKET, option, &output, sizeof(output)) != 0 ||
            socket_timeout_value(fd, option, 1, 10000) != EXIT_SUCCESS ||
            socket_timeout_set(fd, option, -1, 1) != EXIT_SUCCESS ||
            socket_timeout_value(fd, option, 0, 0) != EXIT_SUCCESS ||
            socket_timeout_set(fd, option, 0, 0) != EXIT_SUCCESS)
            return fail("socket timeout signed and extended SET");
    }
    return EXIT_SUCCESS;
}

/// dup 和 fork 沿同一 OFD 修改端点，wait 后父进程必须见到孩子提交的两个期限。
static int socket_timeout_shared(int fd)
{
    int alias = dup(fd);
    if (alias < 0)
        return fail("socket timeout dup");
    int result = socket_timeout_set(alias, SO_RCVTIMEO, 0, 80000);
    if (result == EXIT_SUCCESS)
        result = socket_timeout_value(fd, SO_RCVTIMEO, 0, 80000);
    pid_t child = -1;
    if (result == EXIT_SUCCESS) {
        child = fork();
        if (child < 0) {
            result = fail("socket timeout fork");
        } else if (child == 0) {
            int status = socket_timeout_value(alias, SO_RCVTIMEO, 0, 80000);
            if (status == EXIT_SUCCESS)
                status = socket_timeout_set(fd, SO_RCVTIMEO, 0, 70000);
            if (status == EXIT_SUCCESS)
                status = socket_timeout_set(alias, SO_SNDTIMEO, 0, 90000);
            if (close(alias) != 0)
                status = fail("socket timeout child dup close");
            if (close(fd) != 0)
                status = fail("socket timeout child original close");
            _exit(status);
        } else {
            result = wait_status(child, 0);
            if (result == EXIT_SUCCESS &&
                (socket_timeout_value(fd, SO_RCVTIMEO, 0, 70000) != EXIT_SUCCESS ||
                 socket_timeout_value(alias, SO_SNDTIMEO, 0, 90000) != EXIT_SUCCESS))
                result = fail("socket timeout fork shared endpoint");
        }
    }
    if (close(alias) != 0)
        result = fail("socket timeout dup close");
    return result;
}

/// 空 UDP 的三种接收调用必须真正到期，非阻塞请求和负秒则不等待有限期限。
static int socket_timeout_udp(int fd)
{
    struct sockaddr_in address = {
        .sin_family = AF_INET, .sin_port = 0,
        .sin_addr = { .s_addr = htonl(INADDR_LOOPBACK) },
    };
    if (bind(fd, (struct sockaddr *)&address, sizeof(address)) != 0 ||
        socket_timeout_set(fd, SO_RCVTIMEO, 0, 60000) != EXIT_SUCCESS)
        return fail("socket timeout UDP bind");
    unsigned char byte = 0;
    struct iovec vector = { .iov_base = &byte, .iov_len = 1 };
    struct msghdr message = { .msg_iov = &vector, .msg_iovlen = 1 };
    const char *operations[] = { "UDP read deadline", "UDP recv deadline", "UDP recvmsg deadline" };
    for (unsigned method = 0; method < 3; ++method) {
        uint64_t start;
        if (runtime_nanoseconds(CLOCK_MONOTONIC, &start) != EXIT_SUCCESS)
            return EXIT_FAILURE;
        errno = 0;
        ssize_t count = method == 0 ? read(fd, &byte, 1) :
                        method == 1 ? recv(fd, &byte, 1, 0) : recvmsg(fd, &message, 0);
        int error = errno;
        if (count != -1 || error != EAGAIN)
            return fail(operations[method]);
        if (runtime_elapsed(start, 50000000ULL, operations[method]) != EXIT_SUCCESS)
            return EXIT_FAILURE;
    }
    if (socket_timeout_set(fd, SO_RCVTIMEO, 10, 0) != EXIT_SUCCESS)
        return EXIT_FAILURE;
    uint64_t start;
    if (runtime_nanoseconds(CLOCK_MONOTONIC, &start) != EXIT_SUCCESS)
        return EXIT_FAILURE;
    errno = 0;
    if (recv(fd, &byte, 1, MSG_DONTWAIT) != -1 || errno != EAGAIN ||
        runtime_elapsed(start, 0, "UDP MSG_DONTWAIT") != EXIT_SUCCESS)
        return fail("UDP MSG_DONTWAIT priority");
    uint64_t end;
    if (runtime_nanoseconds(CLOCK_MONOTONIC, &end) != EXIT_SUCCESS ||
        5000000000ULL < end - start)
        return fail("UDP MSG_DONTWAIT waited for timeout");
    int flags = fcntl(fd, F_GETFL);
    if (flags < 0 || fcntl(fd, F_SETFL, flags | O_NONBLOCK) != 0 ||
        runtime_nanoseconds(CLOCK_MONOTONIC, &start) != EXIT_SUCCESS)
        return fail("UDP O_NONBLOCK prepare");
    errno = 0;
    ssize_t count = read(fd, &byte, 1);
    int error = errno;
    if (fcntl(fd, F_SETFL, flags) != 0)
        return fail("UDP O_NONBLOCK restore");
    if (count != -1 || error != EAGAIN ||
        runtime_nanoseconds(CLOCK_MONOTONIC, &end) != EXIT_SUCCESS ||
        end < start || 5000000000ULL < end - start)
        return fail("UDP O_NONBLOCK priority");
    if (socket_timeout_set(fd, SO_RCVTIMEO, -1, 0) != EXIT_SUCCESS ||
        runtime_nanoseconds(CLOCK_MONOTONIC, &start) != EXIT_SUCCESS)
        return EXIT_FAILURE;
    errno = 0;
    if (read(fd, &byte, 1) != -1 || errno != EAGAIN ||
        runtime_nanoseconds(CLOCK_MONOTONIC, &end) != EXIT_SUCCESS ||
        end < start || 5000000000ULL < end - start)
        return fail("UDP negative seconds immediate deadline");
    return EXIT_SUCCESS;
}

/// 两次真实握手消费 listener 快照；新 fd 的 NONBLOCK/CLOEXEC 只取 accept4 参数。
static int socket_timeout_tcp(void)
{
    int listener = socket(AF_INET, SOCK_STREAM | SOCK_NONBLOCK, 0);
    if (listener < 0)
        return fail("socket timeout listener");
    int clients[2] = { -1, -1 }, accepted[2] = { -1, -1 };
    int result = EXIT_FAILURE;
    struct sockaddr_in address = {
        .sin_family = AF_INET, .sin_port = 0,
        .sin_addr = { .s_addr = htonl(INADDR_LOOPBACK) },
    };
    socklen_t length = sizeof(address);
    if (socket_timeout_set(listener, SO_RCVTIMEO, 0, 100000) != EXIT_SUCCESS ||
        socket_timeout_set(listener, SO_SNDTIMEO, 0, 150000) != EXIT_SUCCESS ||
        bind(listener, (struct sockaddr *)&address, sizeof(address)) != 0 ||
        getsockname(listener, (struct sockaddr *)&address, &length) != 0 ||
        length != sizeof(address) || address.sin_port == 0 || listen(listener, 2) != 0) {
        result = fail("socket timeout listener prepare");
        goto out;
    }
    for (unsigned round = 0; round < 2; ++round) {
        if (round == 1 &&
            (socket_timeout_set(listener, SO_RCVTIMEO, 0, 200000) != EXIT_SUCCESS ||
             socket_timeout_set(listener, SO_SNDTIMEO, 0, 250000) != EXIT_SUCCESS))
            goto out;
        clients[round] = socket(AF_INET, SOCK_STREAM, 0);
        if (clients[round] < 0 ||
            socket_timeout_set(clients[round], SO_SNDTIMEO, 2, 0) != EXIT_SUCCESS ||
            connect(clients[round], (struct sockaddr *)&address, sizeof(address)) != 0) {
            result = fail("socket timeout TCP handshake");
            goto out;
        }
        struct pollfd ready = { .fd = listener, .events = POLLIN };
        if (poll(&ready, 1, 2000) != 1 || (ready.revents & POLLIN) == 0) {
            result = fail("socket timeout listener readiness");
            goto out;
        }
        accepted[round] = accept4(listener, NULL, NULL,
                                  round == 0 ? 0 : SOCK_NONBLOCK | SOCK_CLOEXEC);
        if (accepted[round] < 0) {
            result = fail("socket timeout accept4");
            goto out;
        }
        int flags = fcntl(accepted[round], F_GETFL);
        int descriptor_flags = fcntl(accepted[round], F_GETFD);
        if (flags < 0 || descriptor_flags < 0 ||
            !!(flags & O_NONBLOCK) != (int)round ||
            !!(descriptor_flags & FD_CLOEXEC) != (int)round ||
            socket_timeout_value(accepted[round], SO_RCVTIMEO, 0,
                                 round == 0 ? 100000 : 200000) != EXIT_SUCCESS ||
            socket_timeout_value(accepted[round], SO_SNDTIMEO, 0,
                                 round == 0 ? 150000 : 250000) != EXIT_SUCCESS) {
            result = fail("socket timeout accepted snapshot and flags");
            goto out;
        }
    }
    result = socket_timeout_value(accepted[0], SO_RCVTIMEO, 0, 100000);
    if (result == EXIT_SUCCESS)
        result = socket_timeout_value(accepted[0], SO_SNDTIMEO, 0, 150000);
out:
    for (unsigned index = 0; index < 2; ++index) {
        if (0 <= accepted[index] && close(accepted[index]) != 0)
            result = fail("socket timeout accepted close");
        if (0 <= clients[index] && close(clients[index]) != 0)
            result = fail("socket timeout client close");
    }
    if (close(listener) != 0)
        result = fail("socket timeout listener close");
    return result;
}

/// 满 FIFO 的每次无进度发送核真正到期；释放物理页槽后核正前缀与原字节。
static int socket_timeout_unix(void)
{
    int pair[2];
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, pair) != 0)
        return fail("socket timeout UNIX pair");
    int result = EXIT_FAILURE;
    unsigned char bytes[4096];
    memset(bytes, 0x5a, sizeof(bytes));
    size_t queued = 0;
    for (;;) {
        ssize_t count = send(pair[0], bytes, sizeof(bytes), MSG_DONTWAIT | MSG_NOSIGNAL);
        if (count == -1 && errno == EAGAIN)
            break;
        if (count <= 0 || 8 * 1024 * 1024 - sizeof(bytes) < queued) {
            result = fail("socket timeout UNIX fill");
            goto out;
        }
        queued += (size_t)count;
    }
    if (queued == 0) {
        result = fail("UNIX new stream has no send capacity");
        goto out;
    }
    if (socket_timeout_set(pair[0], SO_SNDTIMEO, 0, 60000) != EXIT_SUCCESS)
        goto out;
    uint64_t start;
    if (runtime_nanoseconds(CLOCK_MONOTONIC, &start) != EXIT_SUCCESS)
        goto out;
    errno = 0;
    if (write(pair[0], bytes, 1) != -1 || errno != EAGAIN ||
        runtime_elapsed(start, 50000000ULL, "UNIX write deadline") != EXIT_SUCCESS) {
        result = fail("UNIX full write timeout");
        goto out;
    }
    struct iovec vector = { .iov_base = bytes, .iov_len = 1 };
    struct msghdr message = { .msg_iov = &vector, .msg_iovlen = 1 };
    if (runtime_nanoseconds(CLOCK_MONOTONIC, &start) != EXIT_SUCCESS)
        goto out;
    errno = 0;
    if (sendmsg(pair[0], &message, MSG_NOSIGNAL) != -1 || errno != EAGAIN ||
        runtime_elapsed(start, 50000000ULL, "UNIX sendmsg deadline") != EXIT_SUCCESS) {
        result = fail("UNIX full sendmsg timeout");
        goto out;
    }
    if (read(pair[1], bytes, sizeof(bytes)) != (ssize_t)sizeof(bytes)) {
        result = fail("UNIX release physical slot");
        goto out;
    }
    for (size_t index = 0; index < sizeof(bytes); ++index) {
        if (bytes[index] != 0x5a) {
            result = fail("UNIX released slot contents");
            goto out;
        }
    }
    struct iovec prefix_vectors[] = {
        { .iov_base = bytes, .iov_len = sizeof(bytes) },
        { .iov_base = bytes, .iov_len = sizeof(bytes) },
    };
    ssize_t prefix = writev(pair[0], prefix_vectors, 2);
    if (prefix <= 0 || 2 * sizeof(bytes) <= (size_t)prefix) {
        result = fail("UNIX send positive prefix");
        goto out;
    }
    size_t remaining = queued - sizeof(bytes) + (size_t)prefix;
    while (0 < remaining) {
        size_t requested = remaining < sizeof(bytes) ? remaining : sizeof(bytes);
        ssize_t count = read(pair[1], bytes, requested);
        if (count <= 0 || remaining < (size_t)count) {
            result = fail("UNIX timeout drain");
            goto out;
        }
        for (ssize_t index = 0; index < count; ++index) {
            if (bytes[index] != 0x5a) {
                result = fail("UNIX timeout queued contents");
                goto out;
            }
        }
        remaining -= (size_t)count;
    }
    if (socket_timeout_set(pair[0], SO_RCVTIMEO, 0, 60000) != EXIT_SUCCESS ||
        write(pair[1], "utc", 3) != 3 || read(pair[0], bytes, sizeof(bytes)) != 3 ||
        memcmp(bytes, "utc", 3) != 0) {
        result = fail("UNIX receive existing prefix");
        goto out;
    }
    result = EXIT_SUCCESS;
out:
    if (close(pair[0]) != 0)
        result = fail("socket timeout UNIX send close");
    if (close(pair[1]) != 0)
        result = fail("socket timeout UNIX receive close");
    return result;
}

static volatile sig_atomic_t timeout_signals;

/// 普通 SA_RESTART handler 只记录真实交付次数，不主动重发信号或重试系统调用。
static void socket_timeout_handler(int signal_number)
{
    if (signal_number == SIGUSR1)
        ++timeout_signals;
}

/// 子进程先消费 pipe 握手，再发一次信号与真实后继数据，退出前关闭全部拥有。
static int socket_timeout_signal(int finite)
{
    int pair[2], ready[2];
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, pair) != 0)
        return fail("socket signal pair");
    if (pipe(ready) != 0) {
        close(pair[0]);
        close(pair[1]);
        return fail("socket signal ready pipe");
    }
    int result = EXIT_FAILURE;
    pid_t child = -1;
    if (socket_timeout_set(pair[0], SO_RCVTIMEO, finite ? 2 : 0, 0) != EXIT_SUCCESS)
        goto out;
    timeout_signals = 0;
    pid_t parent = getpid();
    child = fork();
    if (child < 0) {
        result = fail("socket signal fork");
        goto out;
    }
    if (child == 0) {
        int status = EXIT_SUCCESS;
        if (close(pair[0]) != 0)
            status = fail("socket signal child receive close");
        if (close(ready[1]) != 0)
            status = fail("socket signal child ready write close");
        unsigned char token;
        struct timespec delay = { 0, 100000000 };
        if (status == EXIT_SUCCESS &&
            (transfer_pipe(ready[0], &token, 1, 0) != EXIT_SUCCESS ||
             token != 0x74 || nanosleep(&delay, NULL) != 0 ||
             kill(parent, SIGUSR1) != 0 || nanosleep(&delay, NULL) != 0 ||
             send(pair[1], "s", 1, MSG_NOSIGNAL) != 1))
            status = fail("socket signal child delivery");
        if (close(pair[1]) != 0)
            status = fail("socket signal child send close");
        if (close(ready[0]) != 0)
            status = fail("socket signal child ready read close");
        _exit(status);
    }
    int send_close = close(pair[1]);
    pair[1] = -1;
    int ready_close = close(ready[0]);
    ready[0] = -1;
    if (send_close != 0 || ready_close != 0) {
        result = fail("socket signal parent unused close");
        goto out;
    }
    unsigned char token = 0x74, byte = 0;
    uint64_t start;
    if (runtime_nanoseconds(CLOCK_MONOTONIC, &start) != EXIT_SUCCESS ||
        transfer_pipe(ready[1], &token, 1, 1) != EXIT_SUCCESS)
        goto out;
    if (timeout_signals != 0) {
        result = fail("socket signal arrived before blocking read");
        goto out;
    }
    errno = 0;
    ssize_t count = read(pair[0], &byte, 1);
    int error = errno;
    if (finite ? (count != -1 || error != EINTR) : (count != 1 || byte != 's')) {
        result = fail(finite ? "finite socket SA_RESTART must return EINTR" :
                              "infinite socket SA_RESTART must receive successor");
        goto out;
    }
    if (timeout_signals != 1 ||
        runtime_elapsed(start, 50000000ULL, "socket signal blocked read") != EXIT_SUCCESS) {
        result = fail("socket signal handler and wait");
        goto out;
    }
    if (finite && (read(pair[0], &byte, 1) != 1 || byte != 's')) {
        result = fail("finite socket real successor after EINTR");
        goto out;
    }
    result = EXIT_SUCCESS;
out:
    if (0 <= ready[1]) {
        if (close(ready[1]) != 0)
            result = fail("socket signal ready close");
        ready[1] = -1;
    }
    if (0 < child && wait_status(child, 0) != EXIT_SUCCESS)
        result = EXIT_FAILURE;
    if (0 <= ready[0] && close(ready[0]) != 0)
        result = fail("socket signal read pipe close");
    if (close(pair[0]) != 0)
        result = fail("socket signal receive close");
    if (0 <= pair[1] && close(pair[1]) != 0)
        result = fail("socket signal send close");
    return result;
}

/// 有限和无限等待使用同一 handler，恢复进入模式前的信号动作和掩码。
static int socket_timeout_restart(void)
{
    struct sigaction action = { .sa_handler = socket_timeout_handler, .sa_flags = SA_RESTART };
    struct sigaction original;
    sigset_t signal_set, original_mask;
    sigemptyset(&action.sa_mask);
    sigemptyset(&signal_set);
    sigaddset(&signal_set, SIGUSR1);
    if (sigaction(SIGUSR1, &action, &original) != 0)
        return fail("socket timeout sigaction");
    int result = EXIT_FAILURE;
    if (sigprocmask(SIG_UNBLOCK, &signal_set, &original_mask) != 0) {
        result = fail("socket timeout signal unblock");
    } else {
        result = socket_timeout_signal(1);
        if (result == EXIT_SUCCESS)
            result = socket_timeout_signal(0);
        if (sigprocmask(SIG_SETMASK, &original_mask, NULL) != 0)
            result = fail("socket timeout signal mask restore");
    }
    if (sigaction(SIGUSR1, &original, NULL) != 0)
        result = fail("socket timeout sigaction restore");
    return result;
}

/// 正式 socket 消费者串行检查控制 ABI、共享属性、协议等待和不可重启中断。
static int runtime_socket_timeouts(void)
{
    int fd = socket(AF_INET, SOCK_DGRAM, 0);
    if (fd < 0)
        return fail("socket timeout UDP create");
    int result = socket_timeout_options(fd);
    if (result == EXIT_SUCCESS)
        result = socket_timeout_shared(fd);
    if (result == EXIT_SUCCESS)
        result = socket_timeout_udp(fd);
    if (close(fd) != 0)
        result = fail("socket timeout UDP close");
    if (result == EXIT_SUCCESS)
        result = socket_timeout_tcp();
    if (result == EXIT_SUCCESS)
        result = socket_timeout_unix();
    if (result == EXIT_SUCCESS)
        result = socket_timeout_restart();
    if (result == EXIT_SUCCESS &&
        (printf("libc-smoke: real socket timeouts and restart behavior passed\n") < 0 ||
         fflush(stdout) != 0))
        return fail("socket timeout output");
    return result;
}

/// 控制信号只结束错误时间域的等待，不把中断当成期限已经到达。
static void runtime_clock_alarm(int signal_number)
{
    (void)signal_number;
}

/// 以 UTC 未来期限等待真实 futex；单次 alarm 只给错误投影提供有界失败出口。
static int runtime_realtime_futex(void)
{
    struct sigaction action = { .sa_handler = runtime_clock_alarm };
    struct sigaction previous;
    struct itimerval alarm = { .it_value = { 0, 500000 } };
    struct itimerval previous_timer;
    struct itimerval stopped = { 0 };
    sigemptyset(&action.sa_mask);
    if (sigaction(SIGALRM, &action, &previous) != 0)
        return fail("UTC futex alarm action");
    if (setitimer(ITIMER_REAL, &alarm, &previous_timer) != 0) {
        sigaction(SIGALRM, &previous, NULL);
        return fail("UTC futex alarm");
    }
    int result = EXIT_FAILURE;
    struct timespec deadline;
    uint64_t start;
    if (clock_gettime(CLOCK_REALTIME, &deadline) != 0 ||
        runtime_nanoseconds(CLOCK_MONOTONIC, &start) != EXIT_SUCCESS) {
        result = fail("UTC futex deadline sample");
    } else {
        deadline.tv_nsec += 80000000;
        if (1000000000L <= deadline.tv_nsec) {
            ++deadline.tv_sec;
            deadline.tv_nsec -= 1000000000L;
        }
        uint32_t word = 0;
        errno = 0;
        long waited = syscall(SYS_futex, &word, 9U | 0x80U | 0x100U, 0,
                              &deadline, NULL, UINT32_MAX);
        int error = errno;
        if (waited == -1 && error == ETIMEDOUT)
            result = runtime_elapsed(start, 50000000ULL, "UTC futex deadline");
        else {
            errno = error;
            result = fail("UTC futex must time out without alarm interruption");
        }
    }
    int timer_stopped = setitimer(ITIMER_REAL, &stopped, NULL);
    int action_restored = sigaction(SIGALRM, &previous, NULL);
    int timer_restored = setitimer(ITIMER_REAL, &previous_timer, NULL);
    if (timer_stopped != 0 || action_restored != 0 || timer_restored != 0)
        result = fail("UTC futex alarm restore");
    return result;
}

/// 两种绝对 sleep 均沿实际时钟建立未来期限，过去期限只允许立即完成。
static int runtime_absolute_sleep(clockid_t clock)
{
    struct timespec deadline, after;
    uint64_t start;
    if (runtime_nanoseconds(CLOCK_MONOTONIC, &start) != EXIT_SUCCESS ||
        clock_gettime(clock, &deadline) != 0)
        return fail("absolute sleep sample");
    deadline.tv_nsec += 80000000;
    if (1000000000L <= deadline.tv_nsec) {
        ++deadline.tv_sec;
        deadline.tv_nsec -= 1000000000L;
    }
    int error = clock_nanosleep(clock, TIMER_ABSTIME, &deadline, NULL);
    if (error != 0) {
        errno = error;
        return fail("future absolute clock_nanosleep");
    }
    if (clock_gettime(clock, &after) != 0 || !file_time_before(deadline, after) ||
        runtime_elapsed(start, 50000000ULL, "future absolute sleep") != EXIT_SUCCESS)
        return fail("absolute sleep deadline");
    struct timespec past = after;
    if (0 < past.tv_sec)
        --past.tv_sec;
    else
        past.tv_nsec = 0;
    if (runtime_nanoseconds(CLOCK_MONOTONIC, &start) != EXIT_SUCCESS)
        return EXIT_FAILURE;
    error = clock_nanosleep(clock, TIMER_ABSTIME, &past, NULL);
    uint64_t end;
    if (error != 0) {
        errno = error;
        return fail("past absolute clock_nanosleep");
    }
    if (runtime_nanoseconds(CLOCK_MONOTONIC, &end) != EXIT_SUCCESS ||
        end < start || 5000000000ULL < end - start)
        return fail("past absolute sleep immediate");
    return EXIT_SUCCESS;
}

/// 普通 sleep 推进墙钟而不计入 CPU 执行量，宽容阈值仍能拒绝把 CPU 时钟接到墙钟。
static int runtime_cpu_sleep(void)
{
    uint64_t wall_before, process_before, thread_before;
    uint64_t wall_after, process_after, thread_after;
    if (runtime_nanoseconds(CLOCK_MONOTONIC, &wall_before) != EXIT_SUCCESS ||
        runtime_nanoseconds(CLOCK_PROCESS_CPUTIME_ID, &process_before) != EXIT_SUCCESS ||
        runtime_nanoseconds(CLOCK_THREAD_CPUTIME_ID, &thread_before) != EXIT_SUCCESS)
        return EXIT_FAILURE;
    struct timespec delay = { 0, 300000000 };
    if (nanosleep(&delay, NULL) != 0 ||
        runtime_nanoseconds(CLOCK_THREAD_CPUTIME_ID, &thread_after) != EXIT_SUCCESS ||
        runtime_nanoseconds(CLOCK_PROCESS_CPUTIME_ID, &process_after) != EXIT_SUCCESS ||
        runtime_nanoseconds(CLOCK_MONOTONIC, &wall_after) != EXIT_SUCCESS)
        return fail("CPU clock ordinary sleep");
    if (wall_after < wall_before || process_after < process_before || thread_after < thread_before)
        return fail("CPU clock monotonic accounting");
    uint64_t wall = wall_after - wall_before;
    uint64_t process = process_after - process_before;
    uint64_t thread = thread_after - thread_before;
    if (wall < 250000000ULL || 10000000000ULL < wall ||
        wall / 2 + 50000000ULL < process || wall / 2 + 50000000ULL < thread) {
        fprintf(stderr, "libc-smoke: sleep wall_ns=%llu process_ns=%llu thread_ns=%llu\n",
                (unsigned long long)wall, (unsigned long long)process, (unsigned long long)thread);
        return fail("CPU clock excludes sleeping wall time");
    }
    return EXIT_SUCCESS;
}

/// 新建文件三时间必须位于同次 UTC 采样区间；显式历史时间保持原有秒与纳秒。
static int runtime_clock_file(void)
{
    char path[128];
    int count = snprintf(path, sizeof(path), "/test/runtime-times/clock-%ld", (long)getpid());
    if (count < 0 || sizeof(path) <= (size_t)count)
        return fail("clock file name");
    struct timespec before, after;
    if (clock_gettime(CLOCK_REALTIME, &before) != 0)
        return fail("clock file UTC before");
    int fd = open(path, O_RDWR | O_CREAT | O_EXCL, 0600);
    if (fd < 0)
        return fail("clock file create");
    int result = EXIT_FAILURE;
    struct stat created, changed;
    if (fstat(fd, &created) != 0 || clock_gettime(CLOCK_REALTIME, &after) != 0 ||
        !file_time_before(before, created.st_atim) || !file_time_before(created.st_atim, after) ||
        !file_time_before(before, created.st_mtim) || !file_time_before(created.st_mtim, after) ||
        !file_time_before(before, created.st_ctim) || !file_time_before(created.st_ctim, after)) {
        result = fail("new file UTC atime mtime ctime");
        goto out;
    }
    const struct timespec history[2] = { { 123456789, 123456789 }, { -123, 456789012 } };
    if (clock_gettime(CLOCK_REALTIME, &before) != 0 || futimens(fd, history) != 0 ||
        clock_gettime(CLOCK_REALTIME, &after) != 0 || fstat(fd, &changed) != 0 ||
        !same_file_time(changed.st_atim, history[0]) ||
        !same_file_time(changed.st_mtim, history[1]) ||
        !file_time_before(before, changed.st_ctim) || !file_time_before(changed.st_ctim, after) ||
        !same_file_attributes(&created, &changed)) {
        result = fail("clock file explicit history preserved");
        goto out;
    }
    result = EXIT_SUCCESS;
out:
    if (close(fd) != 0)
        result = fail("clock file close");
    if (unlink(path) != 0)
        result = fail("clock file unlink");
    return result;
}

/// 六个标准 clock 与 libc 日历入口共用实际 UTC 样本，不校准 RTC 的亚秒相位。
static int runtime_clock(void)
{
    const clockid_t clocks[] = {
        CLOCK_REALTIME, CLOCK_MONOTONIC, CLOCK_PROCESS_CPUTIME_ID,
        CLOCK_THREAD_CPUTIME_ID, CLOCK_REALTIME_COARSE, CLOCK_MONOTONIC_COARSE,
    };
    struct timespec resolutions[6];
    for (size_t index = 0; index < sizeof(clocks) / sizeof(clocks[0]); ++index) {
        uint64_t sample;
        if (runtime_nanoseconds(clocks[index], &sample) != EXIT_SUCCESS ||
            clock_getres(clocks[index], &resolutions[index]) != 0 ||
            resolutions[index].tv_sec != 0 || resolutions[index].tv_nsec <= 0 ||
            10000000L < resolutions[index].tv_nsec)
            return fail("runtime clock IDs and resolution");
    }
    uint64_t real_before, real_after, coarse_real, mono_before, mono_after, coarse_mono;
    struct timeval calendar;
    time_t stored = -1;
    if (runtime_nanoseconds(CLOCK_REALTIME, &real_before) != EXIT_SUCCESS ||
        runtime_nanoseconds(CLOCK_REALTIME_COARSE, &coarse_real) != EXIT_SUCCESS ||
        gettimeofday(&calendar, NULL) != 0)
        return fail("UTC clock and gettimeofday");
    time_t seconds = time(&stored);
    if (seconds == (time_t)-1 || stored != seconds ||
        runtime_nanoseconds(CLOCK_REALTIME, &real_after) != EXIT_SUCCESS ||
        runtime_nanoseconds(CLOCK_MONOTONIC, &mono_before) != EXIT_SUCCESS ||
        runtime_nanoseconds(CLOCK_MONOTONIC_COARSE, &coarse_mono) != EXIT_SUCCESS ||
        runtime_nanoseconds(CLOCK_MONOTONIC, &mono_after) != EXIT_SUCCESS)
        return fail("UTC time and monotonic samples");
    if (real_before / 1000000000ULL < 1577836800ULL || real_after < real_before ||
        mono_after < mono_before || real_before / 1000000000ULL <= mono_after / 1000000000ULL ||
        real_before / 1000000000ULL - mono_after / 1000000000ULL < 1577836800ULL ||
        coarse_real < real_before - (uint64_t)resolutions[4].tv_nsec ||
        real_after < coarse_real || mono_after < coarse_mono ||
        (coarse_mono < mono_before &&
         (uint64_t)resolutions[5].tv_nsec < mono_before - coarse_mono) ||
        calendar.tv_sec < 0 || calendar.tv_usec < 0 || 1000000 <= calendar.tv_usec)
        return fail("UTC and monotonic clock domains");
    uint64_t calendar_ns = (uint64_t)calendar.tv_sec * 1000000000ULL +
                           (uint64_t)calendar.tv_usec * 1000ULL;
    if (calendar_ns + 999ULL < real_before || real_after < calendar_ns ||
        seconds < (time_t)(real_before / 1000000000ULL) ||
        (time_t)(real_after / 1000000000ULL) < seconds)
        return fail("gettimeofday and time UTC interval");
    struct tm utc;
    if (gmtime_r(&seconds, &utc) == NULL || utc.tm_year < 120)
        return fail("UTC calendar year");
    if (runtime_clock_file() != EXIT_SUCCESS ||
        runtime_realtime_futex() != EXIT_SUCCESS ||
        runtime_absolute_sleep(CLOCK_REALTIME) != EXIT_SUCCESS ||
        runtime_absolute_sleep(CLOCK_MONOTONIC) != EXIT_SUCCESS ||
        runtime_cpu_sleep() != EXIT_SUCCESS)
        return EXIT_FAILURE;
    if (printf("libc-smoke: real UTC clocks and deadlines passed epoch=%lld date=%04d-%02d-%02d\n",
               (long long)seconds, utc.tm_year + 1900, utc.tm_mon + 1, utc.tm_mday) < 0 ||
        fflush(stdout) != 0)
        return fail("runtime clock output");
    return EXIT_SUCCESS;
}

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
