#ifndef LIBC_SMOKE_FP_H
#define LIBC_SMOKE_FP_H

#include "libc_common.h"

// 写入和读回必须内联到观测者，普通调用的 ABI 序言会保存并恢复被测寄存器。
static const unsigned long long fp_pattern = 0x400921fb54442d18ULL;

/// 在程序初始化边界把图样写入 ABI callee-saved 浮点寄存器。
static inline __attribute__((always_inline)) void set_fp_value(void)
{
#if defined(__riscv)
    __asm__ volatile("fmv.d.x fs0, %0" : : "r"(fp_pattern) : "fs0", "memory");
#elif defined(__loongarch__)
    __asm__ volatile("movgr2fr.d $f24, %0" : : "r"(fp_pattern) : "$f24", "memory");
#endif
}

/// 从 ABI callee-saved 浮点寄存器读取当前真实位图，不重新写入图样。
static inline __attribute__((always_inline)) unsigned long long read_fp_value(void)
{
    unsigned long long result = 0;
#if defined(__riscv)
    __asm__ volatile("fmv.x.d %0, fs0" : "=r"(result) : : "memory");
#elif defined(__loongarch__)
    __asm__ volatile("movfr2gr.d %0, $f24" : "=r"(result) : : "memory");
#endif
    return result;
}

/// 检查用户 ABI 保留的真实浮点寄存器，不依赖内核填充观测值。
static inline int check_fp(void)
{
    if (read_fp_value() != fp_pattern)
        return fail("callee-saved floating point");
    return EXIT_SUCCESS;
}

#endif
