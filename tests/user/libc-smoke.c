#define _GNU_SOURCE
#define _POSIX_C_SOURCE 200809L

#include <stdint.h>
#include <errno.h>
#include <fcntl.h>
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/auxv.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <termios.h>
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

/// 触碰超过旧内核栈上限的每一页，保留真实 volatile 主栈访问。
static int check_stack(void)
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
    if (!script_mode && !exec_child && !migration && !abi && !abi_user && !errors &&
        !pid1 && !fixture_root &&
        !(argc == 2 && strcmp(argv[1], "stage7") == 0))
        return fail("main arguments");
    if (tls_image != 0x13579bdfU || tls_zero != 0)
        return fail("initial TLS template");
    if (check_malloc() != EXIT_SUCCESS || check_stack() != EXIT_SUCCESS)
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
