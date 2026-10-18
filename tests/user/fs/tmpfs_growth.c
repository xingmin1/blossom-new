#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/vfs.h>
#include <sys/wait.h>
#include <unistd.h>

#include "kselftest.h"

#define CHECK(expression) do { if (!(expression)) { \
    fprintf(stderr, "# tmpfs-growth: line=%d errno=%d\n", __LINE__, errno); \
    return 1; } } while (0)

/// 不改变原挂载身份，重配未出现的字段由内核保留。
static int reconfigure(const char *path, const char *options)
{
    return mount(NULL, path, NULL, MS_REMOUNT, options);
}

/// 真正写满 64MiB，并在 17MiB 边界及最后字节核对原数据。
static int growth(const char *root)
{
    CHECK(chdir(root) == 0);
    char name[32];
    for (int index = 0; index < 160; ++index) {
        snprintf(name, sizeof(name), "file-%03d", index);
        int fd = open(name, O_CREAT | O_EXCL | O_RDWR, 0600);
        CHECK(0 <= fd && close(fd) == 0);
    }
    struct statfs before, live;
    CHECK(statfs(".", &before) == 0 && 128 < before.f_files);
    int fd = open("bytes", O_CREAT | O_EXCL | O_RDWR, 0600);
    CHECK(0 <= fd);
    unsigned char block[4096];
    memset(block, 0x5d, sizeof(block));
    for (size_t offset = 0; offset < 64u * 1024 * 1024; offset += sizeof(block)) {
        CHECK(write(fd, block, sizeof(block)) == sizeof(block));
        if (offset + sizeof(block) == 17u * 1024 * 1024) {
            struct stat intermediate;
            CHECK(fstat(fd, &intermediate) == 0 && intermediate.st_size == 17u * 1024 * 1024);
        }
    }
    unsigned char byte = 0;
    CHECK(pread(fd, &byte, 1, 17u * 1024 * 1024) == 1 && byte == 0x5d);
    CHECK(pread(fd, &byte, 1, 64u * 1024 * 1024 - 1) == 1 && byte == 0x5d);
    CHECK(statfs(".", &live) == 0 && before.f_bfree - live.f_bfree == 64u * 1024 * 1024 / before.f_bsize);
    CHECK(ftruncate(fd, 0) == 0 && close(fd) == 0);
    CHECK(statfs(".", &live) == 0 && live.f_bfree == before.f_bfree);
    CHECK(unlink("bytes") == 0);
    for (int index = 0; index < 160; ++index) {
        snprintf(name, sizeof(name), "file-%03d", index);
        CHECK(unlink(name) == 0);
    }
    return 0;
}

/// 每个子进程只写自己的 inode，两个写者竞争同一实例实际页票。
static int quota_writer(const char *name, int start, int report)
{
    char token;
    CHECK(read(start, &token, 1) == 1);
    int fd = open(name, O_CREAT | O_EXCL | O_RDWR, 0600);
    CHECK(0 <= fd);
    unsigned char bytes[4096];
    memset(bytes, 0x39, sizeof(bytes));
    unsigned pages = 0;
    while (pages < 16) {
        ssize_t count = write(fd, bytes, sizeof(bytes));
        if (count == -1) {
            CHECK(errno == ENOSPC);
            break;
        }
        CHECK(count == sizeof(bytes));
        ++pages;
    }
    CHECK(pages < 16 && close(fd) == 0);
    CHECK(write(report, &pages, sizeof(pages)) == sizeof(pages));
    return 0;
}

/// pipe 同时放行两个真实进程，成功页总数必须精确等于共享额度并在删除后全部归还。
static int concurrent_quota(const char *root)
{
    CHECK(reconfigure(root, "nr_blocks=8,nr_inodes=8") == 0 && chdir(root) == 0);
    int start[2], report[2];
    CHECK(pipe(start) == 0 && pipe(report) == 0);
    pid_t children[2];
    const char *names[] = {"writer-a", "writer-b"};
    for (int index = 0; index < 2; ++index) {
        children[index] = fork();
        CHECK(0 <= children[index]);
        if (children[index] == 0) {
            close(start[1]);
            close(report[0]);
            _exit(quota_writer(names[index], start[0], report[1]));
        }
    }
    CHECK(close(start[0]) == 0 && close(report[1]) == 0);
    CHECK(write(start[1], "RR", 2) == 2 && close(start[1]) == 0);
    unsigned pages[2];
    for (int index = 0; index < 2; ++index)
        CHECK(read(report[0], &pages[index], sizeof(pages[index])) == sizeof(pages[index]));
    CHECK(close(report[0]) == 0 && pages[0] + pages[1] == 8);
    for (int index = 0; index < 2; ++index) {
        int status;
        CHECK(waitpid(children[index], &status, 0) == children[index] && WIFEXITED(status) && WEXITSTATUS(status) == 0);
    }
    struct statfs live;
    CHECK(statfs(".", &live) == 0 && live.f_bfree == 0);
    CHECK(unlink(names[0]) == 0 && unlink(names[1]) == 0);
    CHECK(statfs(".", &live) == 0 && live.f_bfree == 8 && live.f_ffree == 7);
    return 0;
}

/// 小额度核对实际页、失败前缀、未链接映射末拥有和原实例原子重配。
static int quota(const char *root)
{
    CHECK(concurrent_quota(root) == 0);
    CHECK(reconfigure(root, "nr_blocks=2,nr_inodes=3") == 0 && chdir(root) == 0);
    int fd = open("kept", O_CREAT | O_EXCL | O_RDWR, 0600);
    CHECK(0 <= fd);
    struct stat identity, current;
    struct statfs before, after;
    CHECK(fstat(fd, &identity) == 0 && fstatfs(fd, &before) == 0);
    unsigned char data[3 * 4096];
    memset(data, 0x67, sizeof(data));
    CHECK(write(fd, data, 0) == 0);
    CHECK(write(fd, data, sizeof(data)) == 8192);
    errno = 0;
    CHECK(write(fd, data, 1) == -1 && errno == ENOSPC);
    CHECK(fstat(fd, &current) == 0 && current.st_size == 8192);
    CHECK(fstatfs(fd, &after) == 0 && after.f_bfree == 0);
    errno = 0;
    CHECK(reconfigure(root, "nr_blocks=4,nr_inodes=1") == -1 && errno == EINVAL);
    CHECK(fstatfs(fd, &after) == 0 && after.f_blocks == 2 && after.f_files == 3);
    errno = 0;
    CHECK(reconfigure(root, "nr_blocks=1,nr_inodes=8") == -1 && errno == EINVAL);
    CHECK(fstatfs(fd, &after) == 0 && after.f_blocks == 2 && after.f_files == 3);
    CHECK(reconfigure(root, "nr_blocks=4,mode=0700") == 0);
    struct stat root_stat;
    CHECK(stat(".", &root_stat) == 0 && (root_stat.st_mode & 07777) == 01777);
    CHECK(fstatfs(fd, &after) == 0 && after.f_blocks == 4 && after.f_files == 3);
    CHECK(fstat(fd, &current) == 0 && identity.st_dev == current.st_dev && identity.st_ino == current.st_ino);
    unsigned char retained = 0;
    CHECK(pread(fd, &retained, 1, 0) == 1 && retained == 0x67);
    int other = open("other", O_CREAT | O_EXCL | O_RDWR, 0600);
    CHECK(0 <= other);
    errno = 0;
    CHECK(open("full", O_CREAT | O_EXCL | O_RDWR, 0600) == -1 && errno == ENOSPC);
    CHECK(close(other) == 0 && unlink("other") == 0);
    void *shared = mmap(NULL, 8192, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    void *private = mmap(NULL, 8192, PROT_READ | PROT_WRITE, MAP_PRIVATE, fd, 0);
    CHECK(shared != MAP_FAILED && private != MAP_FAILED);
    ((unsigned char *)private)[0] = 0x29;
    ((unsigned char *)shared)[0] = 0x73;
    CHECK(((unsigned char *)private)[0] == 0x29);
    CHECK(unlink("kept") == 0);
    errno = 0;
    CHECK(umount(root) == -1 && errno == EBUSY);
    CHECK(close(fd) == 0 && ((unsigned char *)shared)[0] == 0x73);
    CHECK(statfs(".", &after) == 0 && after.f_bfree == 2);
    CHECK(munmap(shared, 8192) == 0 && munmap(private, 8192) == 0);
    CHECK(statfs(".", &after) == 0 && after.f_bfree == 4 && after.f_ffree == 2);
    CHECK(reconfigure(root, "nr_blocks=0,nr_inodes=0") == 0);
    CHECK(statfs(".", &after) == 0 && after.f_blocks == 0 && after.f_bfree == 0 && after.f_files == 0 && after.f_ffree == 0);
    errno = 0;
    CHECK(reconfigure(root, "nr_blocks=8") == -1 && errno == EINVAL);
    errno = 0;
    CHECK(reconfigure(root, "nr_inodes=8") == -1 && errno == EINVAL);
    return 0;
}

/// 单位、base0、百分比和重复末值均作用于同一实例，不预分配声明额度。
static int options(const char *root)
{
    struct statfs original, full, changed;
    CHECK(statfs(root, &original) == 0);
    CHECK(reconfigure(root, "size=100%") == 0 && statfs(root, &full) == 0);
    CHECK(full.f_blocks / 2 == original.f_blocks);
    CHECK(reconfigure(root, "size=1k,nr_blocks=0x10,size=8193,nr_inodes=010,mode=0700") == 0);
    CHECK(statfs(root, &changed) == 0 && changed.f_blocks == 3 && changed.f_files == 8);
    CHECK(reconfigure(root, "size=1M,nr_blocks=020") == 0);
    CHECK(statfs(root, &changed) == 0 && changed.f_blocks == 16 && changed.f_files == 8);
    CHECK(reconfigure(root, "size=25%") == 0);
    CHECK(statfs(root, &changed) == 0 && changed.f_blocks == (full.f_blocks + 3) / 4);
    CHECK(reconfigure(root, "size=0,nr_inodes=0") == 0);
    CHECK(statfs(root, &changed) == 0 && changed.f_blocks == 0 && changed.f_files == 0);
    return 0;
}

/// 按选择执行原挂载、字节与额度检查，清理失败仍返回失败，初次挂载无权限返回跳过。
static int tmpfs_checks(const char *selection)
{
    char root[] = "/tmp/tmpfs-growth-XXXXXX";
    CHECK(mkdtemp(root) != NULL);
    if (mount("tmpfs", root, "tmpfs", 0, "mode=0700,mode=01777") != 0) {
        int denied = errno == EPERM || errno == EACCES;
        CHECK(rmdir(root) == 0);
        return denied ? 4 : 1;
    }
    int result;
    if (strcmp(selection, "growth") == 0) result = growth(root);
    else if (strcmp(selection, "quota") == 0) result = quota(root);
    else if (strcmp(selection, "options") == 0) result = options(root);
    else {
        result = growth(root);
        if (result == 0) result = options(root);
        /* 无限额度不能再改有限，重新挂载供小额度例消费。 */
        CHECK(chdir("/") == 0 && umount(root) == 0);
        CHECK(mount("tmpfs", root, "tmpfs", 0, "mode=01777") == 0);
        if (result == 0) result = quota(root);
    }
    CHECK(chdir("/") == 0 && umount(root) == 0 && rmdir(root) == 0);
    if (result == 0) ksft_print_msg("tmpfs-growth: %s passed\n", selection);
    return result;
}

/// 用单个 TAP 记录汇报所选完整序列，权限跳过保留退出码 4。
int main(int argc, char **argv)
{
    const char *selection = argc == 2 ? argv[1] : "all";
    ksft_print_header();
    ksft_set_plan(1);
    int result = tmpfs_checks(selection);
    if (result == KSFT_SKIP)
        ksft_test_result_skip("tmpfs-growth: mounting tmpfs requires permission\n");
    else
        ksft_test_result(result == 0, "tmpfs-growth: %s\n", selection);
    return result;
}
