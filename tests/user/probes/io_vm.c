#include "io_private.h"

/// 核对保留页字节和重新分配页的零填充，所有 brk 返回保持原始 byte break。
static void check_break(void)
{
    long initial = probe_sys1(SYS_brk, 0);
    PROBE_CHECK(164, 0 < initial);
    unsigned long aligned = ((unsigned long)initial + 4095) & ~4095UL;
    long expanded = (long)(aligned + 8193);
    E(165, probe_sys1(SYS_brk, expanded), expanded);
    io_zero(166, (void *)initial, (unsigned long)(expanded - initial));
    volatile unsigned char *bytes = (void *)aligned;
    bytes[0] = 0x63;
    bytes[4096] = 0x63;
    bytes[8192] = 0x63;
    E(167, probe_sys1(SYS_brk, aligned + 17), (long)(aligned + 17));
    E(168, bytes[0], 0x63);
    E(169, probe_sys1(SYS_brk, aligned + 33), (long)(aligned + 33));
    E(170, bytes[0], 0x63);
    E(171, probe_sys1(SYS_brk, expanded), expanded);
    E(172, bytes[0], 0x63);
    io_zero(173, bytes + 4096, 4097);
    E(174, probe_sys1(SYS_brk, -4096), expanded);
    E(175, probe_sys1(SYS_brk, initial - 1), expanded);
    E(176, probe_sys1(SYS_brk, 0), expanded);
    E(177, probe_sys1(SYS_brk, initial), initial);
}

/// 对同一匿名地址执行 hint、替换、保护及带洞错误，失败不能改旧页或文件游标。
static void check_anonymous(long fd, long mapping)
{
    volatile unsigned char *bytes = (void *)mapping;
    E(113, probe_sys2(SYS_munmap, mapping, 4096), 0);
    E(114, probe_sys6(SYS_mmap, mapping + 17, 8192, PROT_READ | PROT_WRITE,
                      MAP_PRIVATE | MAP_ANONYMOUS, -1, 0), mapping);
    io_zero(115, bytes, 8192);
    bytes[0] = 0x5b;
    E(116, probe_sys6(SYS_mmap, mapping, 8192, PROT_READ | PROT_WRITE,
                      MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED_NOREPLACE, -1, 0), -EEXIST);
    E(117, bytes[0], 0x5b);
    E(118, probe_sys6(SYS_mmap, mapping, 4096, PROT_READ | PROT_WRITE,
                      MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0), mapping);
    io_zero(119, bytes, 4096);
    bytes[0] = 0x71;
    E(120, probe_sys3(SYS_mprotect, mapping + 1, 0, PROT_READ), -EINVAL);
    E(121, probe_sys3(SYS_mprotect, mapping, 0, PROT_READ), 0);
    E(122, probe_sys3(SYS_mprotect, mapping, 1, PROT_READ), 0);
    E(123, probe_sys3(SYS_read, fd, mapping, 1), -EFAULT);
    E(124, probe_sys3(SYS_lseek, fd, 0, 1), 354);
    E(125, bytes[0], 0x71);
    E(126, probe_sys3(SYS_mprotect, mapping, 1, PROT_READ | PROT_WRITE), 0);
    bytes[0] = 0x72;
    E(127, bytes[0], 0x72);
    E(128, probe_sys2(SYS_munmap, mapping + 1, 1), -EINVAL);
    E(129, probe_sys2(SYS_munmap, mapping, 0), -EINVAL);
    E(130, probe_sys2(SYS_munmap, mapping, 1), 0);
    E(131, probe_sys3(SYS_read, fd, mapping, 1), -EFAULT);
    E(132, probe_sys3(SYS_mprotect, mapping, 8192, PROT_READ), -ENOMEM);
    E(133, bytes[4096], 0);
    E(134, probe_sys2(SYS_munmap, mapping, 4097), 0);
    E(135, probe_sys3(SYS_read, fd, mapping + 4096, 1), -EFAULT);
    E(136, probe_sys2(SYS_munmap, mapping, 8192), 0);
    E(137, probe_sys6(SYS_mmap, mapping, 4096, PROT_READ | PROT_WRITE,
                      MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED_NOREPLACE, -1, 0), mapping);
    io_zero(138, bytes, 4096);
    E(139, probe_sys2(SYS_munmap, mapping, 4096), 0);
}

/// 文件私有映射首次 COW 后不能污染 canonical 数据，关闭 fd 后 VMA 仍拥有文件。
static void check_file_mappings(long fd, long independent_fd, long duplicate_fd)
{
    long mapping = probe_sys6(SYS_mmap, 0, 16384, PROT_READ | PROT_WRITE, MAP_PRIVATE, fd, 0);
    PROBE_CHECK(140, 0 < mapping);
    volatile unsigned char *bytes = (void *)mapping;
    io_pattern(141, bytes, 12293, 0);
    io_zero(142, bytes + 12293, 4091);
    bytes[0] = 0x9a;
    bytes[4096] = 0x9a;
    bytes[12292] = 0x9a;
    E(143, bytes[0], 0x9a);
    E(144, probe_sys4(SYS_pread64, fd, io_buffer, 12293, 0), 12293);
    io_pattern(145, io_buffer, 12293, 0);
    E(146, probe_sys3(SYS_lseek, fd, 0, 1), 354);
    E(147, probe_sys2(SYS_munmap, mapping, 16384), 0);
    E(148, probe_sys6(SYS_mmap, 0, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0), -EACCES);
    mapping = probe_sys6(SYS_mmap, 0, 4096, PROT_READ, MAP_SHARED, fd, 0);
    PROBE_CHECK(149, 0 < mapping);
    io_pattern(150, (void *)mapping, 4096, 0);
    E(151, probe_sys3(SYS_mprotect, mapping, 4096, PROT_READ | PROT_WRITE), -EACCES);
    E(152, probe_sys2(SYS_munmap, mapping, 4096), 0);
    mapping = probe_sys6(SYS_mmap, 0, 4096, PROT_READ, MAP_PRIVATE, independent_fd, 4096);
    PROBE_CHECK(153, 0 < mapping);
    bytes = (void *)mapping;
    io_pattern(154, bytes, 4096, 4096);
    E(155, probe_sys1(SYS_close, independent_fd), 0);
    E(156, probe_sys3(SYS_mprotect, mapping, 4096, PROT_READ | PROT_WRITE), 0);
    bytes[3] = 0x44;
    E(157, bytes[3], 0x44);
    E(158, probe_sys4(SYS_pread64, fd, io_buffer, 1, 4099), 1);
    E(159, ((volatile unsigned char *)io_buffer)[0], 3);
    E(160, probe_sys2(SYS_munmap, mapping, 4096), 0);
    E(161, probe_sys6(SYS_mmap, 0, 4096, PROT_READ, MAP_PRIVATE, fd, 1), -EINVAL);
    E(162, probe_sys1(SYS_close, duplicate_fd), 0);
    E(163, probe_sys1(SYS_close, fd), 0);
}

/// 按原先顺序消费匿名地址和三个 OFD，再检查独立的 break 生命周期。
void io_memory(long fd, long independent_fd, long duplicate_fd, long mapping)
{
    check_anonymous(fd, mapping);
    check_file_mappings(fd, independent_fd, duplicate_fd);
    check_break();
}
