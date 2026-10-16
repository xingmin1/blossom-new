#include "common.h"

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
int runtime_file_times(const char *program)
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
