#ifndef USER_WRITE_H
#define USER_WRITE_H
#include "probe.h"

#define WRITE_USER "/test/write-user"
#define WRITE_SHARED "/test/write-reserve"
#define WRITE_MAP "/test/write-map"
#define WRITE_APPEND "/test/write-append"
#define WRITE_SELF "/test/bin/user-write"
#define WRITE_NEXT "/test/bin/user-write-next"
#define NS(name) "/test/ns-work/" name
#define CHECK(number, expression) PROBE_CHECK(number, expression)
#define EQ(number, actual, expected) CHECK(number, (actual) == (expected))

/* 两目标使用同一 asm-generic 128 字节 stat，字段各自按 wire 宽度读取。 */
struct WriteStat {
    unsigned long dev, ino;
    unsigned mode, nlink, uid, gid;
    unsigned long rdev, pad;
    long size;
    int blksize, pad2;
    long blocks, atime;
    unsigned long atime_nsec;
    long mtime;
    unsigned long mtime_nsec;
    long ctime;
    unsigned long ctime_nsec;
    unsigned unused[2];
};
_Static_assert(sizeof(struct WriteStat) == 128, "stat wire size");
_Static_assert(offsetof(struct WriteStat, size) == 48, "stat size offset");
extern unsigned char write_buffer[8192];

/// 取得已由 fixture 映射的协调页字，不触碰未分配的控制字段。
static inline volatile unsigned *write_word(unsigned offset)
{
    return (volatile unsigned *)(0x49000000UL + offset);
}
/// 按原单调许可忙等，期间不发 syscall，保留真实活跃 TLB。
static inline void write_gate(unsigned phase)
{
    while (probe_load_acquire(write_word(4)) < phase) {}
}
/// 在普通共享页发表完成阶段，之前的用户 store 对观察者可见。
static inline void write_phase(unsigned phase)
{
    probe_store_release(write_word(0), phase);
}
/// 从可能非自然对齐的槽读取完整八字节，不作未对齐 long 访问。
static inline unsigned long write_load64(unsigned offset)
{
    volatile const unsigned char *p = (volatile const unsigned char *)(0x49000000UL + offset);
    unsigned long value = 0;
    for (unsigned i = 0; i < 8; ++i) value |= (unsigned long)p[i] << (i * 8);
    return value;
}
/// 向真实协议槽发表完整 TID 或 fd，随后阶段 release 负责排序。
static inline void write_store64(unsigned offset, unsigned long value)
{
    volatile unsigned char *p = (volatile unsigned char *)(0x49000000UL + offset);
    for (unsigned i = 0; i < 8; ++i) p[i] = value >> (i * 8);
}
/// open 的零描述符合法，保留每个调用点的失败编号。
static inline long write_open(const char *path, long flags, long mode, long check)
{
    long fd = probe_sys4(SYS_openat, -100, path, flags, mode);
    CHECK(check, 0 <= fd);
    return fd;
}
/// 读取真实 inode stat，不用用户维护的尺寸代替内核结果。
static inline struct WriteStat write_stat(long fd, long check)
{
    struct WriteStat stat;
    EQ(check, probe_sys2(PROBE_NR_FSTAT, fd, &stat), 0);
    return stat;
}
/// 观察 OFD 的真实共享位置。
static inline void write_cursor(long fd, long position, long check)
{
    EQ(check, probe_sys3(SYS_lseek, fd, 0, 1), position);
}
/// 检查生产 inode 的实际字节长度。
static inline void write_size(long fd, long size, long check)
{
    EQ(check, write_stat(fd, check).size, size);
}
/// 完整检查常量区，volatile 保留实际映射的加载。
static inline void write_constant(const volatile unsigned char *p, unsigned long n, unsigned char byte, long check)
{
    for (unsigned long i = 0; i < n; ++i) EQ(check, p[i], byte);
}
/// 核对独立字节输入，保留原检查号及全部字节。
static inline void write_equal(const volatile unsigned char *p, const unsigned char *q, unsigned long n, long check)
{
    for (unsigned long i = 0; i < n; ++i) EQ(check, p[i], q[i]);
}
/// 消费真实 wait4 的 TID 和原始 wait status。
static inline void write_wait(long tid, long tid_check, long status_check)
{
    int status;
    EQ(tid_check, probe_sys4(SYS_wait4, tid, &status, 0, 0), tid);
    EQ(status_check, status, 0);
}

void write_buffered(void);
void write_append(void);
void write_exec(char mode);
void write_shared(void);
void write_remote(char mode);
void write_file_futex(char mode);
void write_sync_abi(void);
void write_sync_wait(void);
void write_sync_write(char mode);
void write_truncate(void);
void write_truncate_ro(void);
void write_truncate_dac(void);
void write_truncate_cow(void);
void write_truncate_wait(void);
void write_reclaim(void);
void write_namespace_create(void);
void write_namespace_unlink(void);
void write_namespace_rename(void);
void write_namespace_directories(void);
#endif
