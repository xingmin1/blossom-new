#include "write.h"

/// 按同文件输入的原逻辑偏移检查全部图样字节。
static void write_pattern(const volatile unsigned char *p, unsigned long n, unsigned long offset, long check)
{
    for (unsigned long i = 0; i < n; ++i) EQ(check, p[i], (unsigned char)(offset + i));
}

/// 跨坏页写保留正前缀、共享位置、空洞与冷同文件输入的完整检查。
void write_buffered(void)
{
    long fd = write_open(WRITE_USER, 2, 0, 2);
    long address = probe_sys6(SYS_mmap, 0, 8192, 3, 0x22, -1, 0);
    CHECK(3, 0 <= address);
    volatile unsigned char *source = (volatile unsigned char *)address;
    for (unsigned i = 4079; i < 4096; ++i) source[i] = 0x72;
    EQ(4, probe_sys2(SYS_munmap, address + 4096, 4096), 0);
    EQ(5, probe_sys3(SYS_write, fd, source + 4079, 64), 17);
    write_cursor(fd, 17, 6);
    write_size(fd, 17, 7);
    EQ(8, probe_sys3(SYS_write, fd, source + 4096, 64), -14);
    write_cursor(fd, 17, 9);
    EQ(10, probe_sys4(SYS_pwrite64, fd, source + 4079, 64, 4093), 17);
    write_cursor(fd, 17, 11);
    write_size(fd, 4110, 12);
    EQ(13, probe_sys4(SYS_pwrite64, fd, "9", 1, -1), -22);
    write_cursor(fd, 17, 14);
    EQ(15, probe_sys4(SYS_pread64, fd, write_buffer, 4096, 0), 4096);
    write_constant(write_buffer, 17, 0x72, 16);
    write_constant(write_buffer + 17, 4076, 0, 17);
    write_constant(write_buffer + 4093, 3, 0x72, 18);
    EQ(19, probe_sys4(SYS_pread64, fd, write_buffer, 17, 4093), 17);
    write_constant(write_buffer, 17, 0x72, 20);
    long mapped = write_open(WRITE_MAP, 2, 0, 21);
    long mapping = probe_sys6(SYS_mmap, 0, 8192, 1, 2, mapped, 0);
    CHECK(22, 0 <= mapping);
    /* 冷 private 页直接作为同 inode 输入，不能提前触页。 */
    EQ(23, probe_sys4(SYS_pwrite64, mapped, mapping + 4093, 1027, 10240), 1027);
    write_cursor(mapped, 0, 24);
    write_size(mapped, 11267, 25);
    EQ(26, probe_sys4(SYS_pread64, mapped, write_buffer, 1027, 10240), 1027);
    write_pattern(write_buffer, 1027, 4093, 27);
    EQ(32, probe_sys3(SYS_write, fd, write_buffer, 1027), 1027);
    write_cursor(fd, 1044, 33);
    write_size(fd, 4110, 34);
    EQ(35, probe_sys4(SYS_pread64, fd, write_buffer, 1027, 17), 1027);
    write_pattern(write_buffer, 1027, 4093, 36);
    const char *bad_argv[] = { WRITE_MAP, 0 };
    EQ(28, probe_sys3(SYS_execve, WRITE_MAP, bad_argv, 0), -26);
    EQ(29, probe_sys1(SYS_close, mapped), 0);
    EQ(37, probe_sys2(SYS_munmap, mapping, 8192), 0);
    EQ(30, probe_sys3(SYS_execve, WRITE_MAP, bad_argv, 0), -8);
    EQ(31, probe_sys1(SYS_close, fd), 0);
}

/// dup 共享 cursor；两个 append OFD 的 pwrite 仍取 EOF 且不改各自 cursor。
void write_append(void)
{
    long first = write_open(WRITE_APPEND, 0x402, 0, 40);
    long second = write_open(WRITE_APPEND, 0x402, 0, 41);
    EQ(42, probe_sys3(SYS_write, first, "alpha", 5), 5);
    long duplicate = probe_sys1(SYS_dup, first);
    CHECK(43, 0 <= duplicate);
    EQ(44, probe_sys4(SYS_pwrite64, duplicate, "BETA", 4, 0), 4);
    write_cursor(first, 5, 45);
    write_cursor(duplicate, 5, 46);
    write_cursor(second, 0, 47);
    EQ(48, probe_sys3(SYS_write, duplicate, "xyz", 3), 3);
    write_cursor(first, 12, 49);
    EQ(50, probe_sys4(SYS_pwrite64, second, "!?", 2, 123), 2);
    write_cursor(second, 0, 51);
    write_size(first, 14, 52);
    EQ(53, probe_sys4(SYS_pread64, first, write_buffer, 14, 0), 14);
    write_equal(write_buffer, (const unsigned char *)"alphaBETAxyz!?", 14, 54);
}

/// fork 子换真实 ELF，父旧 Mm 仍保留可执行文件的写排斥。
static void exec_child(void *argument, unsigned long entry_sp, unsigned long entry_tp)
{
    (void)argument; (void)entry_sp; (void)entry_tp;
    const char *argv[] = { WRITE_NEXT, "n", 0 };
    probe_sys3(SYS_execve, WRITE_NEXT, argv, 0);
    probe_failure(66);
}

/// 保留旧映像、子映像和最后旧 Mm 释放三个独立执行路径。
void write_exec(char mode)
{
    if (mode == 'n') {
        EQ(67, probe_sys4(SYS_openat, -100, WRITE_SELF, 1, 0), -26);
        return;
    }
    if (mode == 'o') {
        long fd = write_open(WRITE_SELF, 1, 0, 68);
        EQ(69, probe_sys1(SYS_close, fd), 0);
        return;
    }
    EQ(60, probe_sys4(SYS_openat, -100, WRITE_SELF, 1, 0), -26);
    EQ(61, probe_sys4(SYS_openat, -100, WRITE_SELF, 0x201, 0), -26);
    long child = probe_clone(17, 0, 0, 0, 0, exec_child, 0);
    CHECK(62, 0 <= child);
    write_wait(child, 63, 64);
    const char *argv[] = { WRITE_NEXT, "o", 0 };
    probe_sys3(SYS_execve, WRITE_NEXT, argv, 0);
    probe_failure(65);
}
