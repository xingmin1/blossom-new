#include "write.h"

/// 独立 open 在已有 WB 上执行真实 fsync/fdatasync，父只在 Blocked 后恢复完成。
void write_sync_wait(void)
{
    long fd = write_open(WRITE_SHARED, 2, 0, 110);
    write_phase(1);
    write_gate(1);
    EQ(111, probe_sys1(SYS_fsync, fd), 0);
    write_phase(2);
    write_gate(2);
    EQ(112, probe_sys1(SYS_fdatasync, fd), 0);
}

/// 保同步位与原双阶段门；全局 sync 不报告 FLUSH 错误，各 OFD 随后的 fsync 独立消费错误。
void write_sync_write(char mode)
{
    long flags = mode == 'z' ? 0x1002 : 0x100002;
    long fd = write_open(WRITE_SHARED, flags, 0, mode == 'z' ? 114 : 113);
    long observer = write_open(WRITE_SHARED, 0, 0, 230);
    write_store64(16, fd);
    EQ(115, probe_sys3(SYS_fcntl, fd, 4, 0), 0);
    EQ(116, probe_sys2(SYS_fcntl, fd, 3), mode == 'z' ? 0x1002 : 0x101002);
    write_phase(1);
    write_gate(1);
    EQ(117, probe_sys3(SYS_write, fd, "9", 1), -5);
    write_cursor(fd, 1, 118);
    write_phase(2);
    write_gate(2);
    EQ(231, probe_sys0(SYS_sync), 0);
    EQ(232, probe_sys1(SYS_fsync, observer), -EIO);
    EQ(233, probe_sys1(SYS_fsync, observer), 0);
    EQ(234, probe_sys1(SYS_fsync, fd), -EIO);
    EQ(235, probe_sys1(SYS_fsync, fd), 0);
    EQ(236, probe_sys1(SYS_close, observer), 0);
    EQ(119, probe_sys4(SYS_pwrite64, fd, "9", 1, 2), 1);
    write_cursor(fd, 1, 120);
}

/// 逐项核 msync 参数、孔洞、文件偏移与 private COW，全部调用保原编号。
void write_sync_abi(void)
{
    long fd = write_open(WRITE_SHARED, 2, 0, 121);
    EQ(122, probe_sys6(SYS_mmap, 0x48000000, 4096, 3, 0x11, fd, 4096), 0x48000000);
    ((volatile unsigned char *)0x48000000UL)[29] = 0x79;
    write_phase(1);
    write_gate(1);
    EQ(123, probe_sys3(SYS_msync, 0x48000001, 4096, 4), -22);
    EQ(124, probe_sys3(SYS_msync, 0x48000000, 4096, 8), -22);
    EQ(125, probe_sys3(SYS_msync, 0x48000000, 4096, 5), -22);
    EQ(126, probe_sys3(SYS_msync, 0x48000000, 4096, 0), 0);
    EQ(127, probe_sys3(SYS_msync, 0x48000000, 4096, 1), 0);
    EQ(128, probe_sys3(SYS_msync, 0x48000000, 4096, 2), 0);
    EQ(129, probe_sys3(SYS_msync, 0x48000000, 4096, 3), 0);
    EQ(130, probe_sys3(SYS_msync, 0x48000000, 0, 4), 0);
    EQ(131, probe_sys3(SYS_msync, 0x48000000, -1, 0), 0);
    write_phase(2);
    write_gate(2);
    EQ(132, probe_sys3(SYS_msync, 0x48000000, 4096, 4), 0);
    EQ(133, probe_sys3(SYS_msync, 0x48000000, 8192, 4), -12);
    EQ(134, probe_sys3(SYS_msync, 0xfffffffffffff000UL, 8192, 0), -12);
    EQ(135, probe_sys3(SYS_msync, 0x48001000, 4096, 0), -12);
    EQ(136, probe_sys6(SYS_mmap, 0x4a000000, 4096, 3, 0x12, fd, 0), 0x4a000000);
    ((volatile unsigned char *)0x4a000000UL)[29] = 0xaf;
    EQ(137, probe_sys3(SYS_msync, 0x4a000000, 4096, 4), 0);
    EQ(138, probe_sys4(SYS_pread64, fd, write_buffer, 1, 29), 1);
    EQ(139, write_buffer[0], 0x66);
    EQ(140, probe_sys4(SYS_pread64, fd, write_buffer, 1, 4125), 1);
    EQ(141, write_buffer[0], 0x79);
    EQ(142, probe_sys1(SYS_fsync, fd), 0);
    EQ(143, probe_sys1(SYS_fdatasync, fd), 0);
    EQ(144, probe_sys1(SYS_fsync, -1), -9);
    EQ(145, probe_sys1(SYS_fdatasync, -1), -9);
    long directory = write_open("/test", 0, 0, 146);
    EQ(147, probe_sys1(SYS_fsync, directory), 0);
    EQ(148, probe_sys1(SYS_fdatasync, directory), 0);
}
