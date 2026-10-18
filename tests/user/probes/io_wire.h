#ifndef PROBE_IO_WIRE_H
#define PROBE_IO_WIRE_H

#include "probe.h"

/* 两个存储客体直接消费 asm-generic 的 128 字节 stat，不使用 libc 布局。 */
struct StorageStat {
    unsigned long dev;
    unsigned long ino;
    unsigned mode;
    unsigned nlink;
    unsigned uid;
    unsigned gid;
    unsigned long rdev;
    unsigned long pad1;
    long size;
    unsigned blksize;
    unsigned pad2;
    long blocks;
    long atime;
    unsigned long atime_nsec;
    long mtime;
    unsigned long mtime_nsec;
    long ctime;
    unsigned long ctime_nsec;
    unsigned long unused;
};
_Static_assert(sizeof(struct StorageStat) == 128, "stat ABI size");
_Static_assert(__builtin_offsetof(struct StorageStat, size) == 48, "stat size offset");
_Static_assert(__builtin_offsetof(struct StorageStat, unused) == 120, "stat tail offset");

struct __attribute__((may_alias)) StorageDirent {
    unsigned long ino;
    long cookie;
    unsigned short reclen;
    unsigned char type;
    char name[];
};
_Static_assert(__builtin_offsetof(struct StorageDirent, name) == 19, "dirent name offset");

#define E(number, actual, expected) PROBE_CHECK(number, (actual) == (expected))

#endif
