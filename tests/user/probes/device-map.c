#include "probe.h"
#include "io_wire.h"

/* 原载荷保留实际文件数据与零区，不让无状态程序改变被测 ELF 段的前提。 */
static volatile unsigned char file_byte = 0x51;
static volatile unsigned char zero_byte;

/// rtc_time 固定为九个 32 位有符号字段，前后哨兵核对 36 字节复制边界。
struct RtcSample {
    /// 日历字段之前的复制越界哨兵。
    unsigned before;
    /// 按秒、分、时、日、月、年、星期、年内日、夏令时排列的 ABI 字段。
    int fields[9];
    /// 日历字段之后的复制越界哨兵。
    unsigned after;
};
_Static_assert(sizeof(((struct RtcSample *)0)->fields) == 36, "RTC time ABI size");

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

/* /// 通过真实 RTC 节点核对硬件日历、控制错误和 dup 共享的独占打开寿命。 */
static void check_rtc(void)
{
    const char *path = "/plain/rtc0";
    const unsigned long read_time = 0x80247009UL;
    struct StorageStat stat;
    struct RtcSample sample = { .before = 0x5137ac29, .after = 0x9e8264bf };
    long fd = probe_sys4(SYS_openat, AT_FDCWD, path, O_RDONLY, 0);
    PROBE_CHECK(8, 0 <= fd);
    PROBE_CHECK(9, probe_sys2(PROBE_NR_FSTAT, fd, &stat) == 0);
    PROBE_CHECK(9, (stat.mode & 0170000) == 0020000 && stat.rdev != 0);
    PROBE_CHECK(10, probe_sys3(SYS_ioctl, fd, read_time, sample.fields) == 0);
    PROBE_CHECK(10, sample.before == 0x5137ac29 && sample.after == 0x9e8264bf);

    const int *time = sample.fields;
    PROBE_CHECK(11, 0 <= time[0] && time[0] < 60
                    && 0 <= time[1] && time[1] < 60
                    && 0 <= time[2] && time[2] < 24
                    && 0 <= time[4] && time[4] < 12 && 70 <= time[5]);
    long year = (long)time[5] + 1900;
    int leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    static const unsigned char month_days[] = {
        31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31
    };
    int days = month_days[time[4]] + (time[4] == 1 && leap);
    PROBE_CHECK(11, 1 <= time[3] && time[3] <= days);
    PROBE_CHECK(12, probe_sys3(SYS_ioctl, fd, read_time, 0) == -EFAULT);
    PROBE_CHECK(12, probe_sys3(SYS_ioctl, fd, read_time, -1) == -EFAULT);
    long pages = probe_sys6(SYS_mmap, 0, 8192, PROT_READ | PROT_WRITE,
                            MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    PROBE_CHECK(18, 0 <= pages);
    PROBE_CHECK(18, probe_sys2(SYS_munmap, pages + 4096, 4096) == 0);
    PROBE_CHECK(19, probe_sys3(SYS_ioctl, fd, read_time, pages + 4096 - 16) == -EFAULT);
    PROBE_CHECK(19, probe_sys2(SYS_munmap, pages, 4096) == 0);
    PROBE_CHECK(13, probe_sys3(SYS_ioctl, fd, 0xffffffffUL, sample.fields) == -ENOTTY);
    PROBE_CHECK(13, probe_sys3(SYS_ioctl, fd, 0x4024700aUL, sample.fields) == -ENOTTY);
    PROBE_CHECK(14, probe_sys4(SYS_openat, AT_FDCWD, path, O_RDONLY, 0) == -EBUSY);

    long duplicate = probe_sys1(SYS_dup, fd);
    PROBE_CHECK(15, 0 <= duplicate && duplicate != fd);
    PROBE_CHECK(15, probe_sys1(SYS_close, fd) == 0);
    PROBE_CHECK(16, probe_sys4(SYS_openat, AT_FDCWD, path, O_RDONLY, 0) == -EBUSY);
    PROBE_CHECK(16, probe_sys3(SYS_ioctl, duplicate, read_time, sample.fields) == 0);
    PROBE_CHECK(16, probe_sys1(SYS_close, duplicate) == 0);
    fd = probe_sys4(SYS_openat, AT_FDCWD, path, O_RDONLY, 0);
    PROBE_CHECK(17, 0 <= fd);
    PROBE_CHECK(17, probe_sys3(SYS_ioctl, fd, read_time, sample.fields) == 0);
    PROBE_CHECK(17, probe_sys1(SYS_close, fd) == 0);
}

/* /// 消费父已准备的真实 devfs 与 /pattern，不创建替代设备或 fixture。 */
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
    check_rtc();
    probe_exit(0);
}
