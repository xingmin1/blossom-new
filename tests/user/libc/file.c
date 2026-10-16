#include "common.h"
#include "fp.h"

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
int abi_check(unsigned uid, unsigned gid)
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
int runtime_view(void)
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
