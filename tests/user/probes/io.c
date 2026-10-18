#include "io_private.h"

unsigned char io_buffer[16384] __attribute__((aligned(8)));
static struct StorageStat data_stat, dir_stat, first_stat, second_stat, link_stat, temporary;
static unsigned char file_segment = 0x57;

/// 失败直接保留该族 sys_exit 的原始检查号，不经过 libc 或共享默认终止策略。
_Noreturn void probe_failure(long check)
{
    probe_exit(check);
}

/// 对实际用户字节逐个加载，保留跨页及零尾检查的硬件观察。
void io_pattern(long check, const volatile unsigned char *bytes, unsigned long count,
                unsigned long offset)
{
    for (unsigned long i = 0; i < count; ++i)
        E(check, bytes[i], (unsigned char)(offset + i));
}

/// 对有效映射范围逐字节加载，不以 C 的静态零初始化代替缺页后的内容。
void io_zero(long check, const volatile unsigned char *bytes, unsigned long count)
{
    for (unsigned long i = 0; i < count; ++i)
        E(check, bytes[i], 0);
}

/// 比较实际返回长度以内的字节，readlink 不要求 NUL 结束。
static void equal(long check, const void *left, const void *right, unsigned long count)
{
    E(check, memcmp(left, right, count), 0);
}

/// 检查完整 stat128 的字段语义；目录大小由真实文件系统决定。
static void check_stat(long check, const struct StorageStat *stat, unsigned type, long size)
{
    PROBE_CHECK(check, stat->dev != 0 && stat->ino != 0);
    E(check, stat->mode & 0xf000, type);
    if (type == 0x4000)
        PROBE_CHECK(check, 2 <= stat->nlink);
    else
        E(check, stat->nlink, 1);
    E(check, stat->rdev, 0);
    E(check, stat->pad1, 0);
    if (size != -1)
        E(check, stat->size, size);
    PROBE_CHECK(check, 1024 <= stat->blksize && stat->blksize <= 4096);
    E(check, stat->blksize & (stat->blksize - 1), 0);
    E(check, stat->pad2, 0);
    PROBE_CHECK(check, 0 <= stat->blocks);
    if (size == 12293)
        PROBE_CHECK(check, 25 <= stat->blocks);
    PROBE_CHECK(check, stat->atime_nsec < 1000000000UL);
    PROBE_CHECK(check, stat->mtime_nsec < 1000000000UL);
    PROBE_CHECK(check, stat->ctime_nsec < 1000000000UL);
    E(check, stat->unused, 0);
}

/// 检查跨坏页正前缀、pread 的文件边界与不变的共享 cursor。
static long check_reads(long fd)
{
    long mapping = probe_sys6(SYS_mmap, 0, 8192, PROT_READ | PROT_WRITE,
                             MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    PROBE_CHECK(2, 0 < mapping);
    E(3, probe_sys2(SYS_munmap, mapping + 4096, 4096), 0);
    E(4, probe_sys3(SYS_read, fd, mapping + 4079, 64), 17);
    io_pattern(5, (void *)(mapping + 4079), 17, 0);
    E(6, probe_sys3(SYS_lseek, fd, 0, 1), 17);
    E(7, probe_sys3(SYS_read, fd, io_buffer, 300), 300);
    io_pattern(8, io_buffer, 300, 17);
    E(9, probe_sys4(SYS_pread64, fd, io_buffer, 513, 4093), 513);
    io_pattern(10, io_buffer, 513, 4093);
    E(11, probe_sys3(SYS_lseek, fd, 0, 1), 317);
    E(178, probe_sys4(SYS_pread64, fd, io_buffer, 16, 12290), 3);
    io_pattern(179, io_buffer, 3, 12290);
    E(180, probe_sys4(SYS_pread64, fd, io_buffer, 1, 12293), 0);
    E(181, probe_sys3(SYS_lseek, fd, 0, 1), 317);
    return mapping;
}

/// 保留 dup 的共享 OFD、独立 fd 标志和另一次 open 的独立 cursor。
static void check_descriptors(long fd, long *duplicate, long *independent)
{
    long dup = probe_sys1(SYS_dup, fd);
    PROBE_CHECK(12, 0 <= dup);
    E(13, probe_sys3(SYS_read, dup, io_buffer, 37), 37);
    io_pattern(14, io_buffer, 37, 317);
    E(15, probe_sys3(SYS_lseek, fd, 0, 1), 354);
    long other = probe_sys4(SYS_openat, -100, IO_DATA, O_RDONLY, 0);
    PROBE_CHECK(16, 0 <= other);
    E(17, probe_sys3(SYS_read, other, io_buffer, 29), 29);
    io_pattern(18, io_buffer, 29, 0);
    E(19, probe_sys2(SYS_fcntl, fd, F_GETFD), 1);
    E(20, probe_sys2(SYS_fcntl, dup, F_GETFD), 0);
    E(21, probe_sys3(SYS_fcntl, dup, F_SETFD, FD_CLOEXEC), 0);
    E(22, probe_sys2(SYS_fcntl, fd, F_GETFD), 1);
    E(23, probe_sys3(SYS_fcntl, dup, F_SETFD, 0), 0);
    E(24, probe_sys2(SYS_fcntl, dup, F_GETFD), 0);
    E(25, probe_sys3(SYS_fcntl, fd, F_SETFL, O_APPEND | O_NONBLOCK), 0);
    E(26, probe_sys2(SYS_fcntl, dup, F_GETFL) & 0xc03, 0xc00);
    E(27, probe_sys2(SYS_fcntl, other, F_GETFL) & 0xc03, 0);
    E(28, probe_sys2(SYS_fcntl, fd, F_GETFL) & O_CLOEXEC, 0);
    E(29, probe_sys3(SYS_fcntl, fd, F_DUPFD, 40), 40);
    E(30, probe_sys2(SYS_fcntl, 40, F_GETFD), 0);
    E(31, probe_sys3(SYS_lseek, 40, 0, 1), 354);
    E(32, probe_sys1(SYS_close, 40), 0);
    E(33, probe_sys3(SYS_fcntl, fd, F_DUPFD_CLOEXEC, 40), 40);
    E(34, probe_sys2(SYS_fcntl, 40, F_GETFD), 1);
    E(35, probe_sys1(SYS_close, 40), 0);
    E(36, probe_sys3(SYS_dup3, fd, 41, O_CLOEXEC), 41);
    E(37, probe_sys2(SYS_fcntl, 41, F_GETFD), 1);
    E(38, probe_sys1(SYS_close, 41), 0);
    E(39, probe_sys3(SYS_dup3, fd, fd, 0), -EINVAL);
    *duplicate = dup;
    *independent = other;
}

/// 坏 fd、只读挂载和坏用户指针按各自原始优先级失败，stat 快照比较完整128字节。
static void check_errors_and_stat(long fd, long mapping)
{
    E(40, probe_sys3(SYS_write, fd, io_buffer, 1), -EBADF);
    E(41, probe_sys3(SYS_read, -1, 0, 1), -EBADF);
    E(42, probe_sys4(SYS_openat, -100, IO_DATA, O_WRONLY, 0), -EROFS);
    E(43, probe_sys4(SYS_openat, -100, IO_DATA, O_RDWR, 0), -EROFS);
    E(44, probe_sys4(SYS_openat, -100, IO_DATA, O_CREAT | O_TRUNC, 0), -EROFS);
    E(45, probe_sys3(SYS_read, fd, 0, 0), 0);
    E(46, probe_sys3(SYS_read, fd, 0, 1), -EFAULT);
    E(47, probe_sys3(SYS_lseek, fd, 0, 1), 354);
    E(48, probe_sys2(PROBE_NR_FSTAT, fd, &data_stat), 0);
    check_stat(49, &data_stat, 0x8000, 12293);
    E(50, probe_sys4(PROBE_NR_NEWFSTATAT, -999, IO_DATA, &temporary, 0), 0);
    equal(51, &data_stat, &temporary, 128);
    E(52, probe_sys2(PROBE_NR_FSTAT, fd, mapping + 4032), -EFAULT);
}

/// 使用真实 dirfd 和 cwd 检查组件遍历、绝对路径、链接跟随及普通文件拒绝。
static long check_paths(long fd)
{
    long directory = probe_sys4(SYS_openat, -100, "/test/io-dir", O_DIRECTORY, 0);
    PROBE_CHECK(53, 0 <= directory);
    E(54, probe_sys2(PROBE_NR_FSTAT, directory, &dir_stat), 0);
    check_stat(55, &dir_stat, 0x4000, -1);
    E(56, probe_sys4(PROBE_NR_NEWFSTATAT, directory, "first", &first_stat, 0), 0);
    check_stat(57, &first_stat, 0x8000, 11);
    E(58, probe_sys4(PROBE_NR_NEWFSTATAT, directory, "second", &second_stat, 0), 0);
    check_stat(59, &second_stat, 0x8000, 12);
    E(60, probe_sys4(PROBE_NR_NEWFSTATAT, directory, "link", &link_stat, AT_SYMLINK_NOFOLLOW), 0);
    check_stat(61, &link_stat, 0xa000, 5);
    E(62, probe_sys4(PROBE_NR_NEWFSTATAT, directory, "link", &temporary, 0), 0);
    equal(63, &first_stat, &temporary, 128);
    long item = probe_sys4(SYS_openat, directory, "first", O_RDONLY, 0);
    PROBE_CHECK(64, 0 <= item);
    E(65, probe_sys3(SYS_read, item, io_buffer, 32), 11);
    equal(66, io_buffer, "first-data\n", 11);
    E(67, probe_sys1(SYS_close, item), 0);
    item = probe_sys4(SYS_openat, -999, IO_DATA, O_RDONLY, 0);
    PROBE_CHECK(68, 0 <= item);
    E(69, probe_sys1(SYS_close, item), 0);
    E(70, probe_sys4(SYS_openat, -999, "first", O_RDONLY, 0), -EBADF);
    E(71, probe_sys4(SYS_openat, fd, "first", O_RDONLY, 0), -ENOTDIR);
    E(72, probe_sys4(SYS_openat, directory, "first/child", O_RDONLY, 0), -ENOTDIR);
    E(73, probe_sys4(SYS_openat, -100, IO_DATA, O_DIRECTORY, 0), -ENOTDIR);
    E(74, probe_sys4(SYS_openat, directory, "link", O_NOFOLLOW, 0), -ELOOP);
    E(75, probe_sys1(SYS_chdir, IO_DATA), -ENOTDIR);
    E(76, probe_sys1(SYS_chdir, "/test/io-dir"), 0);
    item = probe_sys4(SYS_openat, -100, "./first", O_RDONLY, 0);
    PROBE_CHECK(77, 0 <= item);
    E(78, probe_sys2(PROBE_NR_FSTAT, item, &temporary), 0);
    equal(79, &first_stat, &temporary, 128);
    E(80, probe_sys1(SYS_close, item), 0);
    E(81, probe_sys1(SYS_chdir, ".."), 0);
    item = probe_sys4(SYS_openat, -100, "io-data.bin", O_RDONLY, 0);
    PROBE_CHECK(82, 0 <= item);
    E(83, probe_sys2(PROBE_NR_FSTAT, item, &temporary), 0);
    equal(84, &data_stat, &temporary, 128);
    E(85, probe_sys1(SYS_close, item), 0);
    E(86, probe_sys1(SYS_chdir, "/"), 0);
    return directory;
}

/// readlink 仅写实际目标字节，短读与完整读均必须保留尾哨兵。
static void check_readlink(long directory)
{
    volatile unsigned char *bytes = io_buffer;
    bytes[3] = 0x5a;
    bytes[5] = 0x5a;
    E(87, probe_sys4(SYS_readlinkat, directory, "link", io_buffer, 3), 3);
    equal(88, io_buffer, "first", 3);
    E(89, bytes[3], 0x5a);
    E(90, probe_sys4(SYS_readlinkat, directory, "link", io_buffer, 16), 5);
    equal(91, io_buffer, "first", 5);
    E(92, bytes[5], 0x5a);
    E(93, probe_sys4(SYS_readlinkat, directory, "first", io_buffer, 16), -EINVAL);
}

/// 解析真实目录记录；名称和 inode 不依赖盘面顺序，seen 跨分批返回保留。
static void parse_dirents(long check, const unsigned char *bytes, unsigned long count,
                          unsigned *seen, long *last_cookie)
{
    const char *names[] = {".", "..", "first", "second", "link"};
    const unsigned types[] = {4, 4, 8, 8, 10};
    const unsigned long inodes[] = {dir_stat.ino, 0, first_stat.ino, second_stat.ino, link_stat.ino};
    unsigned long offset = 0;
    while (offset < count) {
        PROBE_CHECK(check, 24 <= count - offset);
        const struct StorageDirent *entry = (const void *)(bytes + offset);
        unsigned length = entry->reclen;
        PROBE_CHECK(check, 24 <= length && length <= count - offset && (length & 7) == 0);
        PROBE_CHECK(check, entry->ino != 0 && 0 < entry->cookie);
        *last_cookie = entry->cookie;
        unsigned name_length = 0;
        while (19 + name_length < length && entry->name[name_length] != 0)
            ++name_length;
        PROBE_CHECK(check, name_length != 0 && 19 + name_length < length);
        E(check, (name_length + 27) & ~7U, length);
        unsigned candidate = 0;
        while (candidate < 5) {
            if (strlen(names[candidate]) == name_length &&
                memcmp(entry->name, names[candidate], name_length) == 0)
                break;
            ++candidate;
        }
        PROBE_CHECK(check, candidate < 5);
        E(check, entry->type, types[candidate]);
        PROBE_CHECK(check, (*seen & (1U << candidate)) == 0);
        *seen |= 1U << candidate;
        if (inodes[candidate] != 0)
            E(check, entry->ino, inodes[candidate]);
        offset += length;
    }
}

/// 跨坏页只提交完整首项；零项 EFAULT 和过短缓冲不能推进目录 cookie。
static void check_directory(long directory, long mapping)
{
    unsigned seen = 0;
    long last_cookie = 0;
    long count = probe_sys3(SYS_getdents64, directory, io_buffer, 4096);
    PROBE_CHECK(94, 0 < count);
    const struct StorageDirent *first = (const void *)io_buffer;
    unsigned first_length = first->reclen;
    long first_cookie = first->cookie;
    parse_dirents(95, io_buffer, count, &seen, &last_cookie);
    E(96, seen, 31);
    E(97, probe_sys3(SYS_getdents64, directory, io_buffer, 4096), 0);
    E(98, probe_sys3(SYS_lseek, directory, 0, 0), 0);
    E(99, probe_sys3(SYS_getdents64, directory, io_buffer, first_length - 1), -EINVAL);
    E(100, probe_sys3(SYS_lseek, directory, 0, 1), 0);
    long edge = mapping + 4096 - first_length;
    E(101, probe_sys3(SYS_getdents64, directory, edge, 4096), first_length);
    seen = 0;
    parse_dirents(102, (void *)edge, first_length, &seen, &last_cookie);
    long cursor = probe_sys3(SYS_lseek, directory, 0, 1);
    E(103, cursor, first_cookie);
    E(104, cursor, last_cookie);
    E(105, probe_sys3(SYS_getdents64, directory, mapping + 4096, 4096), -EFAULT);
    E(106, probe_sys3(SYS_lseek, directory, 0, 1), first_cookie);
    count = probe_sys3(SYS_getdents64, directory, io_buffer, 4096);
    PROBE_CHECK(107, 0 < count);
    parse_dirents(108, io_buffer, count, &seen, &last_cookie);
    E(109, seen, 31);
    E(110, probe_sys3(SYS_lseek, directory, 0, 1), last_cookie);
    E(111, probe_sys3(SYS_getdents64, directory, io_buffer, 4096), 0);
    E(112, probe_sys1(SYS_close, directory), 0);
}

/// 只有完整 access 字符串选择权限测试，其余参数保持原 raw 全流程。
_Noreturn void probe_main(const unsigned long *initial_stack)
{
    /* 保留实际文件段字节，链接布局不能被未引用的静态对象消去。 */
    __asm__ volatile("" : : "r"(&file_segment) : "memory");
    if (2 <= initial_stack[0] &&
        probe_string_equal((const char *)initial_stack[2], "access")) {
        io_access();
        probe_exit(0);
    }
    long fd = probe_sys4(SYS_openat, -100, IO_DATA, O_CLOEXEC, 0);
    PROBE_CHECK(1, 0 <= fd);
    long mapping = check_reads(fd);
    long duplicate, independent;
    check_descriptors(fd, &duplicate, &independent);
    check_errors_and_stat(fd, mapping);
    long directory = check_paths(fd);
    check_readlink(directory);
    check_directory(directory, mapping);
    io_memory(fd, independent, duplicate, mapping);
    probe_exit(0);
}
