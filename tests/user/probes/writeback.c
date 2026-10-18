#include "io_wire.h"

static unsigned char buffer[8229] __attribute__((aligned(8)));
static struct StorageStat stat_buffer;
static const char directory_path[] = "/test/writeback-dir";

/// 该族使用 exit_group 保留整个客体终止策略和完整检查号。
_Noreturn void probe_failure(long check)
{
    probe_exit_group(check);
}

/// 创建图样值只依赖文件偏移；两处真实共享写覆盖对应字节。
static unsigned char expected_byte(unsigned long offset)
{
    if (offset == 17)
        return 0x79;
    if (offset == 4127)
        return 0x53;
    return (unsigned char)(71 + 13 * offset);
}

/// 两页共享写经过 msync，随后私有 COW 的同步不能污染文件。
static void modify_mappings(long fd)
{
    long mapping = probe_sys6(SYS_mmap, 0, 8192, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    PROBE_CHECK(7, 0 <= mapping);
    volatile unsigned char *bytes = (void *)mapping;
    bytes[17] = 0x79;
    bytes[4127] = 0x53;
    E(8, probe_sys3(SYS_msync, mapping, 8192, MS_SYNC), 0);
    E(9, bytes[17], 0x79);
    E(10, bytes[4127], 0x53);
    E(11, probe_sys2(SYS_munmap, mapping, 8192), 0);
    mapping = probe_sys6(SYS_mmap, 0, 4096, PROT_READ | PROT_WRITE, MAP_PRIVATE, fd, 0);
    PROBE_CHECK(12, 0 <= mapping);
    bytes = (void *)mapping;
    E(13, bytes[17], 0x79);
    bytes[17] = 0xfb;
    E(14, probe_sys3(SYS_msync, mapping, 4096, MS_SYNC), 0);
    E(15, bytes[17], 0xfb);
    E(16, probe_sys2(SYS_munmap, mapping, 4096), 0);
}

/// 只在首次 mkdir 成功后创建 pending，完成文件及两个目录的实际同步。
static void write_result(void)
{
    long directory = probe_sys4(SYS_openat, -100, directory_path, O_DIRECTORY, 0);
    PROBE_CHECK(3, 0 <= directory);
    long fd = probe_sys4(SYS_openat, directory, "pending", O_CREAT | O_EXCL | O_RDWR, 0644);
    PROBE_CHECK(4, 0 <= fd);
    for (unsigned long i = 0; i < sizeof(buffer); ++i)
        buffer[i] = (unsigned char)(71 + 13 * i);
    E(5, probe_sys3(SYS_write, fd, buffer, sizeof(buffer)), 8229);
    E(6, probe_sys1(SYS_fdatasync, fd), 0);
    modify_mappings(fd);
    E(17, probe_sys5(SYS_renameat2, directory, "pending", directory, "result", 0), 0);
    E(18, probe_sys5(SYS_linkat, directory, "result", directory, "hard", 0), 0);
    E(19, probe_sys3(SYS_symlinkat, "result", directory, "link"), 0);
    E(20, probe_sys3(SYS_mkdirat, directory, "temporary", 0755), 0);
    E(21, probe_sys3(SYS_unlinkat, directory, "temporary", AT_REMOVEDIR), 0);
    E(22, probe_sys1(SYS_fsync, fd), 0);
    E(23, probe_sys1(SYS_close, fd), 0);
    E(24, probe_sys1(SYS_fsync, directory), 0);
    long parent = probe_sys4(SYS_openat, -100, "/test", O_DIRECTORY, 0);
    PROBE_CHECK(25, 0 <= parent);
    E(26, probe_sys1(SYS_fsync, parent), 0);
    E(27, probe_sys1(SYS_close, parent), 0);
    E(28, probe_sys1(SYS_close, directory), 0);
}

/// 三个路径必须读到同一非零 dev/inode、完整8229字节及两处共享修改。
static void verify_file(long directory, const char *name,
                        unsigned long *inode, unsigned long *device)
{
    long fd = probe_sys4(SYS_openat, directory, name, O_RDONLY, 0);
    PROBE_CHECK(32, 0 <= fd);
    E(33, probe_sys2(PROBE_NR_FSTAT, fd, &stat_buffer), 0);
    PROBE_CHECK(34, stat_buffer.ino != 0 && stat_buffer.dev != 0);
    if (*inode == 0) {
        *inode = stat_buffer.ino;
        *device = stat_buffer.dev;
    }
    E(34, stat_buffer.ino, *inode);
    E(34, stat_buffer.dev, *device);
    E(35, stat_buffer.mode & 0xf000, 0x8000);
    E(36, stat_buffer.nlink, 2);
    E(37, stat_buffer.size, 8229);
    E(38, probe_sys4(SYS_pread64, fd, buffer, 8229, 0), 8229);
    const volatile unsigned char *bytes = buffer;
    for (unsigned long i = 0; i < sizeof(buffer); ++i)
        E(39, bytes[i], expected_byte(i));
    E(40, probe_sys4(SYS_pread64, fd, buffer, 1, 8229), 0);
    E(41, probe_sys3(SYS_lseek, fd, 0, 1), 0);
    E(42, probe_sys1(SYS_close, fd), 0);
}

/// 在记录内部匹配含 NUL 的候选名称，不越过当前 reclen。
static int name_matches(const char *actual, unsigned capacity, const char *expected)
{
    unsigned i = 0;
    while (i < capacity) {
        if (actual[i] != expected[i])
            return 0;
        if (actual[i] == 0)
            return 1;
        ++i;
    }
    return 0;
}

/// 每批最多128字节，核验唯一名称、类型、真实 inode、长度及目录 cookie。
static void verify_directory(long directory, unsigned long directory_inode,
                             unsigned long file_inode, unsigned long link_inode)
{
    const char *names[] = {".", "..", "result", "hard", "link"};
    const unsigned types[] = {4, 4, 8, 8, 10};
    unsigned seen = 0;
    long cookie = 0;
    for (;;) {
        long count = probe_sys3(SYS_getdents64, directory, buffer, 128);
        PROBE_CHECK(50, 0 <= count);
        if (count == 0)
            break;
        PROBE_CHECK(51, count <= 128);
        unsigned long offset = 0;
        while (offset < (unsigned long)count) {
            PROBE_CHECK(51, 24 <= (unsigned long)count - offset);
            const struct StorageDirent *entry = (const void *)(buffer + offset);
            unsigned length = entry->reclen;
            PROBE_CHECK(51, 24 <= length && length <= (unsigned long)count - offset &&
                            (length & 7) == 0);
            PROBE_CHECK(52, entry->ino != 0);
            PROBE_CHECK(53, entry->cookie != cookie && 0 <= entry->cookie);
            cookie = entry->cookie;
            unsigned candidate = 0;
            while (candidate < 5 && !name_matches(entry->name, length - 19, names[candidate]))
                ++candidate;
            PROBE_CHECK(54, candidate < 5);
            PROBE_CHECK(55, (seen & (1U << candidate)) == 0);
            seen |= 1U << candidate;
            E(56, entry->type, types[candidate]);
            E(57, (strlen(names[candidate]) + 27) & ~7UL, length);
            if (candidate == 0)
                E(58, entry->ino, directory_inode);
            if (candidate == 2 || candidate == 3)
                E(59, entry->ino, file_inode);
            if (candidate == 4)
                E(60, entry->ino, link_inode);
            offset += length;
        }
        E(61, probe_sys3(SYS_lseek, directory, 0, 1), cookie);
    }
    E(62, seen, 31);
    E(63, probe_sys1(SYS_close, directory), 0);
}

/// 只读检查文件别名、软链本体和目录内容，不修复或重建既存结果。
static void read_result(void)
{
    long directory = probe_sys4(SYS_openat, -100, directory_path, O_DIRECTORY, 0);
    PROBE_CHECK(30, 0 <= directory);
    E(31, probe_sys2(PROBE_NR_FSTAT, directory, &stat_buffer), 0);
    unsigned long directory_inode = stat_buffer.ino;
    E(43, stat_buffer.nlink, 2);
    unsigned long inode = 0, device = 0;
    verify_file(directory, "result", &inode, &device);
    verify_file(directory, "hard", &inode, &device);
    verify_file(directory, "link", &inode, &device);
    E(44, probe_sys4(PROBE_NR_NEWFSTATAT, directory, "link", &stat_buffer, AT_SYMLINK_NOFOLLOW), 0);
    unsigned long link_inode = stat_buffer.ino;
    E(45, stat_buffer.mode & 0xf000, 0xa000);
    E(46, stat_buffer.nlink, 1);
    E(47, stat_buffer.size, 6);
    E(48, probe_sys4(SYS_readlinkat, directory, "link", buffer, 16), 6);
    E(49, memcmp(buffer, "result", 6), 0);
    verify_directory(directory, directory_inode, inode, link_inode);
}

/// 参数恰为一个单字节 w/r/a；a 遇 EEXIST 时只验证，w 不容许重复创建。
_Noreturn void probe_main(const unsigned long *initial_stack)
{
    E(1, initial_stack[0], 2);
    const char *mode = (const void *)initial_stack[2];
    PROBE_CHECK(1, mode != 0);
    E(1, mode[1], 0);
    PROBE_CHECK(1, mode[0] == 'w' || mode[0] == 'r' || mode[0] == 'a');
    if (mode[0] != 'r') {
        long result = probe_sys3(SYS_mkdirat, -100, directory_path, 0755);
        if (result == 0)
            write_result();
        else
            PROBE_CHECK(2, mode[0] == 'a' && result == -EEXIST);
    }
    read_result();
    probe_exit_group(0);
}
