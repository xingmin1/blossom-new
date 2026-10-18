#include "probe.h"

/* 原载荷保留实际文件数据与零区，不让无状态程序改变被测 ELF 段的前提。 */
static volatile unsigned char file_byte = 0x51;
static volatile unsigned char zero_byte;

/* /// 七组检查保留各自编号，直接由原始 exit 向父交付结果。 */
_Noreturn void probe_failure(long check)
{
    probe_exit(check);
}

/* /// 先实际打开 RO devfs 的字符设备，再核映射能力与访问权限的错误优先级。 */
static void check_mapping(long check, const char *path, long flags,
                          long protection, long mapping, long error)
{
    long fd = probe_sys4(SYS_openat, AT_FDCWD, path, flags, 0);
    PROBE_CHECK(check, 0 <= fd);
    PROBE_CHECK(check, probe_sys6(SYS_mmap, 0, 4096, protection, mapping, fd, 0) == error);
    PROBE_CHECK(check, probe_sys1(SYS_close, fd) == 0);
}

/* /// SEEK_SET 的负编码交给真实设备或普通文件处理，不在用户层预先改写。 */
static void check_negative_seek(long check, const char *path, long error)
{
    long fd = probe_sys4(SYS_openat, AT_FDCWD, path, O_RDONLY, 0);
    PROBE_CHECK(check, 0 <= fd);
    PROBE_CHECK(check, probe_sys3(SYS_lseek, fd, -1, 0) == error);
    PROBE_CHECK(check, probe_sys1(SYS_close, fd) == 0);
}

/* /// 只消费父已准备的 /plain/null、/plain/zero 和 /pattern，不创建替代设备或 fixture。 */
_Noreturn void probe_main(const unsigned long *initial_stack)
{
    (void)initial_stack;
    __asm__ volatile("" : : "r"(&file_byte), "r"(&zero_byte) : "memory");
    check_mapping(1, "/plain/null", O_RDWR, PROT_READ | PROT_WRITE, MAP_SHARED, -ENODEV);
    check_mapping(2, "/plain/zero", O_RDWR, PROT_READ | PROT_WRITE, MAP_SHARED, -ENODEV);
    check_mapping(3, "/plain/null", O_RDONLY, PROT_READ | PROT_WRITE, MAP_SHARED, -EACCES);
    check_mapping(4, "/plain/zero", O_WRONLY, PROT_READ, MAP_PRIVATE, -EACCES);
    check_negative_seek(5, "/plain/null", 0);
    check_negative_seek(6, "/plain/zero", 0);
    check_negative_seek(7, "/pattern", -EINVAL);
    probe_exit(0);
}
