#ifndef PROBE_IO_PRIVATE_H
#define PROBE_IO_PRIVATE_H

#include "io_wire.h"

#define IO_DATA "/test/io-data.bin"
extern unsigned char io_buffer[16384];

/// 对真实加载或 syscall 写入的范围逐字节核验周期图样。
void io_pattern(long check, const volatile unsigned char *bytes, unsigned long count,
                unsigned long offset);
/// 对映射范围执行真实加载，核验整段零页及文件尾部。
void io_zero(long check, const volatile unsigned char *bytes, unsigned long count);
/// 消费原 fd 和坏页映射，完成匿名、文件 COW 与 brk 检查并释放 fd。
void io_memory(long fd, long independent_fd, long duplicate_fd, long mapping);
/// 在 fixture 给予的非 owner 身份和空 fd 表下检查真实访问拒绝。
void io_access(void);

#endif
