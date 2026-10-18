#define _GNU_SOURCE
#include <fcntl.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include "kselftest.h"
#include "selftest_io.h"

/// 在当前 collection 内创建唯一临时文件，覆盖位置读写、EOF 和非法参数。
static int file_io(void)
{
    char path[] = "file-io-XXXXXX";
    unsigned char input[8229], output[8229];
    int result = -1, fd = -1, created = 0;
    struct stat metadata;
    for (size_t i = 0; i < sizeof(input); ++i)
        input[i] = (unsigned char)(i * 13 + 71);
    fd = mkstemp(path);
    ST_CHECK(0 <= fd);
    created = 1;
    ST_CHECK(st_write_all(fd, input, sizeof(input)) == 0);
    ST_CHECK(fstat(fd, &metadata) == 0 && metadata.st_size == (off_t)sizeof(input));
    ST_CHECK(lseek(fd, 0, SEEK_SET) == 0);
    ST_CHECK(st_read_all(fd, output, sizeof(output)) == 0);
    ST_CHECK(memcmp(input, output, sizeof(input)) == 0);
    ST_CHECK(read(fd, output, 1) == 0);
    ST_CHECK(pread(fd, output, 41, 4091) == 41);
    ST_CHECK(memcmp(output, input + 4091, 41) == 0);
    ST_CHECK(lseek(fd, 0, SEEK_CUR) == (off_t)sizeof(input));
    ST_CHECK(pwrite(fd, "\0\xffZ", 3, 4095) == 3);
    ST_CHECK(pread(fd, output, 3, 4095) == 3 && memcmp(output, "\0\xffZ", 3) == 0);
    errno = 0;
    ST_CHECK(pread(fd, output, 1, -1) == -1 && errno == EINVAL);
    errno = 0;
    ST_CHECK(fsync(-1) == -1 && errno == EBADF);
    ST_CHECK(pread(fd, output, 1, sizeof(input)) == 0);
    ST_CHECK(write(fd, input, 0) == 0);
    ST_CHECK(fsync(fd) == 0);
    ST_CHECK(st_close(&fd) == 0);
    fd = open(path, O_RDONLY);
    ST_CHECK(0 <= fd);
    errno = 0;
    ST_CHECK(write(fd, input, 1) == -1 && errno == EBADF);
    ST_CHECK(pread(fd, output, 3, 4095) == 3 && memcmp(output, "\0\xffZ", 3) == 0);
    result = 0;
out:
    if (st_close(&fd) != 0)
        result = -1;
    if (created && unlink(path) != 0)
        result = -1;
    return result;
}

/// 真实 fork 的共享 store 经 MS_SYNC 写回；私有 COW store 不得改变文件或父私有页。
static int file_maps(void)
{
    char path[] = "file-map-XXXXXX";
    int result = -1, fd = -1, created = 0;
    pid_t child = -1;
    long page_size = sysconf(_SC_PAGESIZE);
    size_t length = 0;
    unsigned char *shared = MAP_FAILED, *private = MAP_FAILED;
    unsigned char byte;
    ST_CHECK(64 <= page_size);
    length = 2 * (size_t)page_size;
    fd = mkstemp(path);
    ST_CHECK(0 <= fd);
    created = 1;
    ST_CHECK(ftruncate(fd, (off_t)length) == 0);
    shared = mmap(NULL, length, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    ST_CHECK(shared != MAP_FAILED);
    private = mmap(NULL, length, PROT_READ | PROT_WRITE, MAP_PRIVATE, fd, 0);
    ST_CHECK(private != MAP_FAILED);
    shared[17] = 0x79;
    ST_CHECK(msync(shared, length, MS_SYNC) == 0);
    ST_CHECK(private[17] == 0x79);
    child = fork();
    ST_CHECK(0 <= child);
    if (child == 0) {
        shared[page_size + 31] = 0x53;
        private[17] = 0xfb;
        int ok = msync(shared, length, MS_SYNC) == 0 &&
                 msync(private, length, MS_SYNC) == 0 && private[17] == 0xfb;
        _exit(ok ? 0 : 1);
    }
    ST_CHECK(st_reap(&child, 0) == 0);
    ST_CHECK(shared[17] == 0x79 && shared[page_size + 31] == 0x53);
    ST_CHECK(private[17] == 0x79);
    ST_CHECK(pread(fd, &byte, 1, 17) == 1 && byte == 0x79);
    ST_CHECK(pread(fd, &byte, 1, page_size + 31) == 1 && byte == 0x53);
    errno = 0;
    ST_CHECK(msync(shared + 1, length - 1, MS_SYNC) == -1 && errno == EINVAL);
    ST_CHECK(fsync(fd) == 0);
    result = 0;
out:
    if (st_stop_child(&child) != 0)
        result = -1;
    if (private != MAP_FAILED && munmap(private, length) != 0)
        result = -1;
    if (shared != MAP_FAILED && munmap(shared, length) != 0)
        result = -1;
    if (st_close(&fd) != 0)
        result = -1;
    if (created && unlink(path) != 0)
        result = -1;
    return result;
}

/// 持久模式只接受当前目录中的一个名字与有限 token，不覆盖已有文件。
static int persist_arguments(const char *name, const char *token)
{
    return name[0] != '\0' && strchr(name, '/') == NULL &&
           strcmp(name, ".") != 0 && strcmp(name, "..") != 0 &&
           strlen(name) <= 128 && token[0] != '\0' && strlen(token) <= 128 &&
           strchr(token, '\n') == NULL && strchr(token, '\r') == NULL;
}

/// 写模式同步文件及父目录后保留原文件，读模式只读验证同一 token 的跨页全文。
static int persistence(const char *name, const char *token, int writing)
{
    unsigned char expected[8229], actual[8229];
    int result = -1, fd = -1, directory = -1, created = 0;
    unsigned char *mapping = MAP_FAILED;
    struct stat metadata;
    size_t token_length = strlen(token);
    for (size_t i = 0; i < sizeof(expected); ++i)
        expected[i] = (unsigned char)(token[i % token_length] ^ (i * 13 + 71));
    int prefix = snprintf((char *)expected, 256, "blossom-selftests:%s\n", token);
    ST_CHECK(0 < prefix && prefix < 256);
    /* 这两个共享修改与默认测试相同，重启后按最终字节验证。 */
    expected[317] = 0x79;
    expected[4127] = 0x53;
    if (writing) {
        directory = open(".", O_RDONLY | O_DIRECTORY);
        ST_CHECK(0 <= directory);
        fd = open(name, O_RDWR | O_CREAT | O_EXCL, 0600);
        ST_CHECK(0 <= fd);
        created = 1;
        memcpy(actual, expected, sizeof(actual));
        actual[317] = 0;
        actual[4127] = 0;
        ST_CHECK(st_write_all(fd, actual, sizeof(actual)) == 0);
        mapping = mmap(NULL, 8192, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
        ST_CHECK(mapping != MAP_FAILED);
        mapping[317] = expected[317];
        mapping[4127] = expected[4127];
        ST_CHECK(msync(mapping, 8192, MS_SYNC) == 0);
        ST_CHECK(munmap(mapping, 8192) == 0);
        mapping = MAP_FAILED;
        ST_CHECK(fsync(fd) == 0);
        ST_CHECK(st_close(&fd) == 0);
        ST_CHECK(fsync(directory) == 0);
        ST_CHECK(st_close(&directory) == 0);
        sync();
    }
    fd = open(name, O_RDONLY);
    ST_CHECK(0 <= fd);
    ST_CHECK(fstat(fd, &metadata) == 0 && metadata.st_size == (off_t)sizeof(expected));
    ST_CHECK(st_read_all(fd, actual, sizeof(actual)) == 0);
    ST_CHECK(memcmp(actual, expected, sizeof(actual)) == 0);
    ST_CHECK(read(fd, actual, 1) == 0);
    ST_CHECK(st_close(&fd) == 0);
    result = 0;
out:
    if (mapping != MAP_FAILED && munmap(mapping, 8192) != 0)
        result = -1;
    if (st_close(&fd) != 0)
        result = -1;
    if (st_close(&directory) != 0)
        result = -1;
    if (result != 0 && created && unlink(name) != 0)
        fprintf(stderr, "persistence cleanup errno=%d\n", errno);
    return result;
}

/// 默认仅执行自动清理的文件行为；持久写读通过显式 argv 选择且不增加安装清单项。
int main(int argc, char **argv)
{
    ksft_print_header();
    if (argc == 4 &&
        (strcmp(argv[1], "--persist-write") == 0 || strcmp(argv[1], "--persist-read") == 0) &&
        persist_arguments(argv[2], argv[3])) {
        ksft_set_plan(1);
        ksft_test_result(persistence(argv[2], argv[3], strcmp(argv[1], "--persist-write") == 0) == 0,
                         "persistent file %s\n", argv[1]);
    } else if (argc == 1) {
        ksft_set_plan(2);
        ksft_test_result(file_io() == 0, "file bytes positions boundaries errno and cleanup\n");
        ksft_test_result(file_maps() == 0, "fork shared msync private isolation fsync and cleanup\n");
    } else {
        ksft_exit_fail_msg("usage: %s [--persist-write|--persist-read NAME TOKEN]\n", argv[0]);
    }
    ksft_finished();
}
