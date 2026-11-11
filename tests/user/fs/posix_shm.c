#define _GNU_SOURCE
#include <fcntl.h>
#include <stdlib.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include "kselftest.h"
#include "selftest_io.h"

/// 子进程重新按名字打开并映射同一对象，验证父进程字节后修改跨页边界的数据。
static int child_mapping(const char *name, size_t length, size_t page_size)
{
    int result = -1, fd = -1;
    unsigned char *mapping = MAP_FAILED;
    fd = shm_open(name, O_RDWR, 0);
    ST_CHECK(0 <= fd);
    mapping = mmap(NULL, length, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    ST_CHECK(mapping != MAP_FAILED);
    for (size_t i = 0; i < length; ++i)
        ST_CHECK(mapping[i] == (unsigned char)(i * 17 + 29));
    mapping[page_size - 1] = 0x65;
    mapping[length - 1] = 0x83;
    result = 0;
out:
    if (mapping != MAP_FAILED && munmap(mapping, length) != 0)
        result = -1;
    if (st_close(&fd) != 0)
        result = -1;
    return result;
}

/// 名字删除和重新创建不能改变旧 fd 或映射；关闭全部引用并解除映射后结束对象生命周期。
static int posix_shm_lifecycle(void)
{
    char name[80];
    int result = -1, fd = -1, replacement_fd = -1, probe = -1, named = 0;
    pid_t child = -1;
    long page_size = sysconf(_SC_PAGESIZE);
    size_t length = 0;
    unsigned char *mapping = MAP_FAILED, *replacement = MAP_FAILED;
    unsigned char byte;
    struct stat directory, dev, original, current;
    ST_CHECK(64 <= page_size);
    length = 2 * (size_t)page_size;
    ST_CHECK(snprintf(name, sizeof(name), "/blossom-posix-shm-%ld", (long)getpid()) <
             (int)sizeof(name));
    ST_CHECK(stat("/dev/shm", &directory) == 0 && S_ISDIR(directory.st_mode));
    ST_CHECK((directory.st_mode & 07777) == 01777);
    ST_CHECK(stat("/dev", &dev) == 0 && dev.st_dev != directory.st_dev);
    errno = 0;
    probe = shm_open(name, O_RDWR, 0);
    ST_CHECK(probe == -1 && errno == ENOENT);
    fd = shm_open(name, O_CREAT | O_EXCL | O_RDWR, 0600);
    ST_CHECK(0 <= fd);
    named = 1;
    ST_CHECK(fstat(fd, &original) == 0 && S_ISREG(original.st_mode));
    ST_CHECK(original.st_dev == directory.st_dev && original.st_size == 0 && original.st_nlink == 1);
    errno = 0;
    probe = shm_open(name, O_CREAT | O_EXCL | O_RDWR, 0600);
    ST_CHECK(probe == -1 && errno == EEXIST);
    ST_CHECK(ftruncate(fd, (off_t)length) == 0);
    ST_CHECK(fstat(fd, &current) == 0 && current.st_size == (off_t)length);
    mapping = mmap(NULL, length, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    ST_CHECK(mapping != MAP_FAILED);
    for (size_t i = 0; i < length; ++i) {
        ST_CHECK(mapping[i] == 0);
        mapping[i] = (unsigned char)(i * 17 + 29);
    }

    child = fork();
    ST_CHECK(0 <= child);
    if (child == 0) {
        int ok = st_close(&fd) == 0 && munmap(mapping, length) == 0;
        if (ok)
            ok = child_mapping(name, length, (size_t)page_size) == 0;
        _exit(ok ? 0 : 1);
    }
    ST_CHECK(st_reap(&child, 0) == 0);
    ST_CHECK(mapping[page_size - 1] == 0x65 && mapping[length - 1] == 0x83);
    ST_CHECK(pread(fd, &byte, 1, length - 1) == 1 && byte == 0x83);

    ST_CHECK(shm_unlink(name) == 0);
    named = 0;
    errno = 0;
    probe = shm_open(name, O_RDWR, 0);
    ST_CHECK(probe == -1 && errno == ENOENT);
    ST_CHECK(fstat(fd, &current) == 0 && current.st_nlink == 0 && current.st_ino == original.st_ino);
    ST_CHECK(pwrite(fd, "U", 1, 0) == 1 && mapping[0] == 'U');
    replacement_fd = shm_open(name, O_CREAT | O_EXCL | O_RDWR, 0600);
    ST_CHECK(0 <= replacement_fd);
    named = 1;
    ST_CHECK(fstat(replacement_fd, &current) == 0 && current.st_size == 0);
    ST_CHECK(current.st_dev == original.st_dev && current.st_ino != original.st_ino);
    ST_CHECK(ftruncate(replacement_fd, (off_t)length) == 0);
    replacement = mmap(NULL, length, PROT_READ | PROT_WRITE, MAP_SHARED, replacement_fd, 0);
    ST_CHECK(replacement != MAP_FAILED);
    for (size_t i = 0; i < length; ++i)
        ST_CHECK(replacement[i] == 0);
    ST_CHECK(shm_unlink(name) == 0);
    named = 0;
    ST_CHECK(st_close(&replacement_fd) == 0 && st_close(&fd) == 0);
    mapping[0] = 0xb6;
    replacement[0] = 0x4a;
    ST_CHECK(mapping[0] == 0xb6 && mapping[length - 1] == 0x83 && replacement[0] == 0x4a);
    ST_CHECK(munmap(mapping, length) == 0);
    mapping = MAP_FAILED;
    ST_CHECK(munmap(replacement, length) == 0);
    replacement = MAP_FAILED;
    errno = 0;
    probe = shm_open(name, O_RDWR, 0);
    ST_CHECK(probe == -1 && errno == ENOENT);
    result = 0;
out:
    if (st_stop_child(&child) != 0)
        result = -1;
    if (replacement != MAP_FAILED && munmap(replacement, length) != 0)
        result = -1;
    if (mapping != MAP_FAILED && munmap(mapping, length) != 0)
        result = -1;
    if (st_close(&probe) != 0)
        result = -1;
    if (st_close(&replacement_fd) != 0)
        result = -1;
    if (st_close(&fd) != 0)
        result = -1;
    if (named && shm_unlink(name) != 0)
        result = -1;
    return result;
}

/// 用真实 libc POSIX shm 调用报告完整的命名、跨进程共享与末次引用释放行为。
int main(void)
{
    ksft_print_header();
    ksft_set_plan(1);
    ksft_test_result(posix_shm_lifecycle() == 0,
                     "POSIX shm exclusive create truncate fork shared maps unlink and cleanup\n");
    ksft_finished();
}
