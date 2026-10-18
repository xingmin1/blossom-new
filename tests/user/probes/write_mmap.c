#include "write.h"

/// 真实共享尾页的 store 与父 fixture 增长协同，EOF 竞争仍由内核观察者安排。
void write_shared(void)
{
    long fd = write_open(WRITE_SHARED, 0, 0, 75);
    unsigned long offset = write_load64(24);
    EQ(76, probe_sys6(SYS_mmap, 0x48000000, 8192, 3, 0x11, fd, offset), -13);
    EQ(77, probe_sys1(SYS_close, fd), 0);
    fd = write_open(WRITE_SHARED, 2, 0, 80);
    EQ(81, probe_sys6(SYS_mmap, 0x48000000, 8192, 3, 0x11, fd, offset), 0x48000000);
    volatile unsigned char *p = (volatile unsigned char *)0x48000000UL;
    p[0] = 0x61;
    p[128] = 0xde;
    write_phase(1);
    write_gate(1);
    EQ(82, p[1], 0x62);
    EQ(83, p[128], 0);
    EQ(84, (unsigned long)write_stat(fd, 84).size, offset + 513);
    p[128] = 0x74;
    write_phase(2);
    write_gate(2);
}

/// fork 子先等待真实许可，随后在继承 shared 页执行需要 mkwrite 的 store。
static void shared_child(void *argument, unsigned long entry_sp, unsigned long entry_tp)
{
    (void)argument; (void)entry_sp; (void)entry_tp;
    probe_store_release(write_word(8), 3);
    write_gate(3);
    ((volatile unsigned char *)0x48000000UL)[2] = 0x83;
    probe_exit(0);
}

/// 首 store 暖真实远核 W 翻译；忙等后次 store 观察同步撤权或同址 remap。
void write_remote(char mode)
{
    long fd = write_open(WRITE_MAP, 2, 0, 85);
    EQ(86, probe_sys6(SYS_mmap, 0x48000000, 4096, 3, 0x11, fd, 0), 0x48000000);
    volatile unsigned char *p = (volatile unsigned char *)0x48000000UL;
    p[0] = 0x81;
    write_phase(1);
    write_gate(1);
    if (mode != 'c') {
        p[1] = 0x82;
        write_phase(2);
        write_gate(2);
        return;
    }
    long child = probe_clone(17, 0, 0, 0, 0, shared_child, 0);
    CHECK(87, 0 <= child);
    write_store64(16, child);
    write_phase(2);
    write_gate(4);
    write_wait(child, 95, 96);
}

struct FileFutexChild { long fd; char mode; };

/// 子 Mm 在不同 VA 上 WAIT，q 换 inode，p 使用 PRIVATE key，u 保原 timeout 取消。
static void file_futex_child(void *argument, unsigned long entry_sp, unsigned long entry_tp)
{
    (void)entry_sp; (void)entry_tp;
    struct FileFutexChild child = *(struct FileFutexChild *)argument;
    EQ(97, probe_sys2(SYS_munmap, 0x48000000, 4096), 0);
    if (child.mode == 'q') child.fd = write_open(WRITE_SHARED, 2, 0, 98);
    EQ(99, probe_sys6(SYS_mmap, 0x4a000000, 4096, 3, 0x11, child.fd, 0), 0x4a000000);
    write_phase(1);
    const long timeout[2] = { 5, 0 };
    long result = probe_sys6(SYS_futex, 0x4a000040, child.mode == 'p' ? 128 : 0, 0, timeout, 0, 0);
    if (child.mode == 'f') EQ(100, result, 0);
    else EQ(101, result, -110);
    probe_exit(0);
}

/// 父 WAKE 真实跨 Mm 文件 key，同时保不同 inode、PRIVATE 和旧节点取消隔离。
void write_file_futex(char mode)
{
    long fd = write_open(WRITE_MAP, 2, 0, 90);
    EQ(91, probe_sys6(SYS_mmap, 0x48000000, 4096, 3, 0x11, fd, 0), 0x48000000);
    struct FileFutexChild argument = { fd, mode };
    long child = probe_clone(17, 0, 0, 0, 0, file_futex_child, &argument);
    CHECK(92, 0 <= child);
    write_store64(16, child);
    write_gate(1);
    long result = probe_sys6(SYS_futex, 0x48000040, mode == 'p' ? 129 : 1, 1, 0, 0, 0);
    if (mode == 'f') EQ(93, result, 1);
    else EQ(94, result, 0);
    write_wait(child, 95, 96);
}
