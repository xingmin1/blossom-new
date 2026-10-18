#include "write.h"

/// dup 位置、尾零、错误优先级和 O_RDONLY|O_TRUNC 全部由真实 45/46/open 消费。
void write_truncate(void)
{
    long fd = write_open(WRITE_SHARED, 2, 0, 150);
    EQ(151, probe_sys2(SYS_ftruncate, fd, 0), 0);
    EQ(152, probe_sys3(SYS_write, fd, "123456789", 9), 9);
    EQ(153, probe_sys3(SYS_lseek, fd, 123, 0), 123);
    long duplicate = probe_sys1(SYS_dup, fd);
    CHECK(154, 0 <= duplicate);
    EQ(155, probe_sys2(SYS_ftruncate, fd, 3), 0);
    write_size(fd, 3, 156);
    write_cursor(fd, 123, 157);
    write_cursor(duplicate, 123, 158);
    EQ(159, probe_sys2(SYS_ftruncate, fd, 9), 0);
    write_size(fd, 9, 160);
    EQ(161, probe_sys4(SYS_pread64, fd, write_buffer, 9, 0), 9);
    write_equal(write_buffer, (const unsigned char *)"123\0\0\0\0\0\0", 9, 162);
    EQ(163, probe_sys2(SYS_truncate, WRITE_SHARED, 0), 0);
    write_size(fd, 0, 164);
    write_cursor(duplicate, 123, 165);
    EQ(166, probe_sys2(SYS_truncate, WRITE_SHARED, 9), 0);
    write_size(fd, 9, 167);
    EQ(168, probe_sys4(SYS_pread64, fd, write_buffer, 9, 0), 9);
    write_constant(write_buffer, 9, 0, 169);
    EQ(170, probe_sys2(SYS_ftruncate, -1, -1), -22);
    EQ(171, probe_sys2(SYS_ftruncate, -1, 0), -9);
    EQ(172, probe_sys2(SYS_truncate, 0, -1), -22);
    EQ(173, probe_sys2(SYS_truncate, 0, 0), -14);
    EQ(174, probe_sys2(SYS_ftruncate, fd, -1), -22);
    EQ(175, probe_sys2(SYS_ftruncate, fd, 0x7fffffffffffffffUL), -27);
    EQ(176, probe_sys2(SYS_truncate, WRITE_SHARED, 0x7fffffffffffffffUL), -27);
    write_size(fd, 9, 177);
    write_cursor(fd, 123, 178);
    long readonly = write_open(WRITE_SHARED, 0, 0, 179);
    EQ(180, probe_sys2(SYS_ftruncate, readonly, 0), -22);
    long directory = write_open("/test", 0, 0, 181);
    EQ(182, probe_sys2(SYS_ftruncate, directory, 0), -22);
    EQ(183, probe_sys2(SYS_truncate, "/test", 0), -21);
    EQ(184, probe_sys2(SYS_truncate, WRITE_SELF, 0), -26);
    EQ(185, probe_sys4(SYS_openat, -100, WRITE_SELF, 0x200, 0), -26);
    readonly = write_open(WRITE_SHARED, 0x200, 0, 186);
    EQ(187, probe_sys3(SYS_fcntl, readonly, 3, 0), 0);
    write_size(readonly, 0, 188);
    write_size(fd, 0, 189);
    write_cursor(duplicate, 123, 190);
    EQ(191, probe_sys3(SYS_write, readonly, "9", 1), -9);
    EQ(192, probe_sys2(SYS_ftruncate, readonly, 0), -22);
    EQ(193, probe_sys4(SYS_pwrite64, fd, "9", 1, 0), 1);
    long truncated = write_open(WRITE_SHARED, 0x202, 0, 194);
    write_size(truncated, 0, 195);
    write_cursor(fd, 123, 196);
}

/// 只读卷保 EROFS，原只读 OFD 的 ftruncate 仍先核访问模式。
void write_truncate_ro(void)
{
    long fd = write_open(WRITE_MAP, 0, 0, 197);
    write_cursor(fd, 0, 222);
    EQ(198, probe_sys2(SYS_ftruncate, fd, 0), -22);
    EQ(199, probe_sys2(SYS_truncate, WRITE_MAP, 0), -30);
    EQ(200, probe_sys4(SYS_openat, -100, WRITE_MAP, 0x200, 0), -30);
    EQ(201, probe_sys4(SYS_openat, -100, WRITE_MAP, 0x202, 0), -30);
    write_cursor(fd, 0, 223);
}

/// 非 owner 凭据来自父 fixture，路径截断失败后保真实长度及全部原内容。
void write_truncate_dac(void)
{
    long fd = write_open(WRITE_SHARED, 0, 0, 202);
    write_size(fd, 9, 203);
    EQ(204, probe_sys2(SYS_truncate, WRITE_SHARED, 0), -13);
    EQ(205, probe_sys4(SYS_openat, -100, WRITE_SHARED, 0x200, 0), -13);
    EQ(206, probe_sys4(SYS_openat, -100, WRITE_SHARED, 0x202, 0), -13);
    write_size(fd, 9, 207);
    EQ(208, probe_sys4(SYS_pread64, fd, write_buffer, 9, 0), 9);
    write_equal(write_buffer, (const unsigned char *)"123456789", 9, 209);
}

/// 子 Mm 保活到父完成最终失效，沿原 exit_group 结束。
static void truncate_cow_child(void *argument, unsigned long entry_sp, unsigned long entry_tp)
{
    (void)argument; (void)entry_sp; (void)entry_tp;
    probe_store_release(write_word(8), 3);
    write_gate(3);
    probe_exit_group(0);
}

/// private 写 fault 与父两次失效协调，完成 COW 后再真实 fork。
void write_truncate_cow(void)
{
    long fd = write_open(WRITE_SHARED, 0, 0, 210);
    EQ(211, probe_sys6(SYS_mmap, 0x48000000, 4096, 3, 0x12, fd, write_load64(24)), 0x48000000);
    write_phase(1);
    write_gate(1);
    volatile unsigned char *p = (volatile unsigned char *)0x48000000UL;
    p[0] = 0xb6;
    EQ(212, p[0], 0xb6);
    write_phase(2);
    write_gate(2);
    long child = probe_clone(17, 0, 0, 0, 0, truncate_cow_child, 0);
    CHECK(213, 0 <= child);
    write_store64(16, child);
    write_phase(3);
    write_gate(3);
    write_size(fd, 4096, 214);
    write_wait(child, 215, 216);
}

/// 普通 ftruncate 在真实 WB 完成暂停中等待，返回后大小为零且 OFD 位置保持。
void write_truncate_wait(void)
{
    long fd = write_open(WRITE_SHARED, 2, 0, 217);
    EQ(218, probe_sys3(SYS_lseek, fd, 7, 0), 7);
    write_phase(1);
    write_gate(1);
    EQ(219, probe_sys2(SYS_ftruncate, fd, 0), 0);
    write_size(fd, 0, 220);
    write_cursor(fd, 7, 221);
    write_phase(2);
    write_gate(2);
}

/// 两个 Mm 在回收后分别 refault 原文件页并核私有匿名修改。
static void reclaim_resume(long fd, unsigned long address, int parent)
{
    write_gate(1);
    EQ(208, *(volatile unsigned char *)address, 0x62);
    EQ(209, *(volatile unsigned char *)0x4c000000UL, 0xa5);
    EQ(210, probe_sys1(SYS_close, fd), 0);
    if (!parent) {
        probe_store_release(write_word(8), 2);
        write_gate(2);
        probe_exit_group(0);
    }
    write_phase(2);
    write_gate(2);
}

/// 子 Mm 撤旧 VA 后在不同 VA 建同文件映射，发表真实首次读取完成。
static void reclaim_child(void *argument, unsigned long entry_sp, unsigned long entry_tp)
{
    (void)entry_sp; (void)entry_tp;
    long fd = *(long *)argument;
    EQ(205, probe_sys2(SYS_munmap, 0x4b000000, 4096), 0);
    EQ(206, probe_sys6(SYS_mmap, 0x4d000000, 4096, 1, 0x11, fd, 0), 0x4d000000);
    EQ(207, *(volatile unsigned char *)0x4d000000UL, 0x62);
    probe_store_release(write_word(8), 1);
    reclaim_resume(fd, 0x4d000000, 0);
    __builtin_unreachable();
}

/// 两个真实 fork Mm 的同源只读叶和 private COW 保留到父完成回收检查。
void write_reclaim(void)
{
    long fd = write_open(WRITE_SHARED, 0, 0, 200);
    EQ(201, probe_sys6(SYS_mmap, 0x4b000000, 4096, 1, 0x11, fd, 0), 0x4b000000);
    EQ(202, *(volatile unsigned char *)0x4b000000UL, 0x62);
    EQ(203, probe_sys6(SYS_mmap, 0x4c000000, 4096, 3, 0x12, fd, 0), 0x4c000000);
    *(volatile unsigned char *)0x4c000000UL = 0xa5;
    long child = probe_clone(17, 0, 0, 0, 0, reclaim_child, &fd);
    CHECK(204, 0 <= child);
    write_store64(16, child);
    while (probe_load_acquire(write_word(8)) < 1) {}
    write_phase(1);
    reclaim_resume(fd, 0x4b000000, 1);
    write_wait(child, 95, 96);
}
