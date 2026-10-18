#ifndef USER_PROBE_H
#define USER_PROBE_H

#include <stddef.h>
#include <stdint.h>
#include <sys/syscall.h>
#include <errno.h>
#include <fcntl.h>
#include <sys/mman.h>

_Static_assert(sizeof(long) == 8 && sizeof(void *) == 8, "probe requires LP64");
_Static_assert(sizeof(unsigned) == 4, "probe coordination requires 32-bit words");

/* 内核已有的原始 stat 调用契约在两 ISA 上一致，不依赖 libc 是否只声明 statx。 */
enum {
    PROBE_NR_NEWFSTATAT = 79,
    PROBE_NR_FSTAT = 80,
};

#if defined(__riscv) && __riscv_xlen == 64
#define PROBE_SYSCALL "ecall"
#define PROBE_CLOBBERS "memory", "cc"
#elif defined(__loongarch64)
#define PROBE_SYSCALL "syscall 0"
#define PROBE_CLOBBERS "memory", "$t0", "$t1", "$t2", "$t3", "$t4", "$t5", "$t6", "$t7", "$t8"
#else
#error Unsupported probe architecture
#endif

/* /// 按原始 ABI 执行 0 参数系统调用，保留负 errno 并允许内核读写用户内存。 */
#define probe_sys0(number) \
({ \
    register long probe_number __asm__("a7") = (long)(number); \
    register long probe_a0 __asm__("a0"); \
    __asm__ volatile(PROBE_SYSCALL : "=r"(probe_a0) : "r"(probe_number) : PROBE_CLOBBERS); \
    probe_a0; \
})

/* /// 按原始 ABI 执行 1 参数系统调用，保留负 errno 并允许内核读写用户内存。 */
#define probe_sys1(number, p0) \
({ \
    register long probe_number __asm__("a7") = (long)(number); \
    register long probe_a0 __asm__("a0") = (long)(p0); \
    __asm__ volatile(PROBE_SYSCALL : "+r"(probe_a0) : "r"(probe_number) : PROBE_CLOBBERS); \
    probe_a0; \
})

/* /// 按原始 ABI 执行 2 参数系统调用，保留负 errno 并允许内核读写用户内存。 */
#define probe_sys2(number, p0, p1) \
({ \
    register long probe_number __asm__("a7") = (long)(number); \
    register long probe_a0 __asm__("a0") = (long)(p0); \
    register long probe_a1 __asm__("a1") = (long)(p1); \
    __asm__ volatile(PROBE_SYSCALL : "+r"(probe_a0) : "r"(probe_a1), "r"(probe_number) : PROBE_CLOBBERS); \
    probe_a0; \
})

/* /// 按原始 ABI 执行 3 参数系统调用，保留负 errno 并允许内核读写用户内存。 */
#define probe_sys3(number, p0, p1, p2) \
({ \
    register long probe_number __asm__("a7") = (long)(number); \
    register long probe_a0 __asm__("a0") = (long)(p0); \
    register long probe_a1 __asm__("a1") = (long)(p1); \
    register long probe_a2 __asm__("a2") = (long)(p2); \
    __asm__ volatile(PROBE_SYSCALL : "+r"(probe_a0) : "r"(probe_a1), "r"(probe_a2), "r"(probe_number) : PROBE_CLOBBERS); \
    probe_a0; \
})

/* /// 按原始 ABI 执行 4 参数系统调用，保留负 errno 并允许内核读写用户内存。 */
#define probe_sys4(number, p0, p1, p2, p3) \
({ \
    register long probe_number __asm__("a7") = (long)(number); \
    register long probe_a0 __asm__("a0") = (long)(p0); \
    register long probe_a1 __asm__("a1") = (long)(p1); \
    register long probe_a2 __asm__("a2") = (long)(p2); \
    register long probe_a3 __asm__("a3") = (long)(p3); \
    __asm__ volatile(PROBE_SYSCALL : "+r"(probe_a0) : "r"(probe_a1), "r"(probe_a2), "r"(probe_a3), "r"(probe_number) : PROBE_CLOBBERS); \
    probe_a0; \
})

/* /// 按原始 ABI 执行 5 参数系统调用，保留负 errno 并允许内核读写用户内存。 */
#define probe_sys5(number, p0, p1, p2, p3, p4) \
({ \
    register long probe_number __asm__("a7") = (long)(number); \
    register long probe_a0 __asm__("a0") = (long)(p0); \
    register long probe_a1 __asm__("a1") = (long)(p1); \
    register long probe_a2 __asm__("a2") = (long)(p2); \
    register long probe_a3 __asm__("a3") = (long)(p3); \
    register long probe_a4 __asm__("a4") = (long)(p4); \
    __asm__ volatile(PROBE_SYSCALL : "+r"(probe_a0) : "r"(probe_a1), "r"(probe_a2), "r"(probe_a3), "r"(probe_a4), "r"(probe_number) : PROBE_CLOBBERS); \
    probe_a0; \
})

/* /// 按原始 ABI 执行 6 参数系统调用，保留负 errno 并允许内核读写用户内存。 */
#define probe_sys6(number, p0, p1, p2, p3, p4, p5) \
({ \
    register long probe_number __asm__("a7") = (long)(number); \
    register long probe_a0 __asm__("a0") = (long)(p0); \
    register long probe_a1 __asm__("a1") = (long)(p1); \
    register long probe_a2 __asm__("a2") = (long)(p2); \
    register long probe_a3 __asm__("a3") = (long)(p3); \
    register long probe_a4 __asm__("a4") = (long)(p4); \
    register long probe_a5 __asm__("a5") = (long)(p5); \
    __asm__ volatile(PROBE_SYSCALL : "+r"(probe_a0) : "r"(probe_a1), "r"(probe_a2), "r"(probe_a3), "r"(probe_a4), "r"(probe_a5), "r"(probe_number) : PROBE_CLOBBERS); \
    probe_a0; \
})

/* /// 子回调取得 syscall 返回当刻的 SP/TP；回调必须终止，参数与栈保活到真实完成。 */
typedef void (*ProbeChildEntry)(void *argument, unsigned long entry_sp,
                                unsigned long entry_tp);
/* /// 父返回原始 TID/负 errno；子直接进入回调，不返回父 C 帧；桥不改变 syscall SP。 */
long probe_clone(unsigned long flags, void *stack_top, int *parent_tid,
                 unsigned long tls, int *child_tid, ProbeChildEntry entry,
                 void *argument);
/* /// 每族直接消费内核初始栈并选择自身终止协议。 */
_Noreturn void probe_main(const unsigned long *initial_stack);
/* /// 每族保留自身完整检查号与共享失败发表协议。 */
_Noreturn void probe_failure(long check);
#define PROBE_CHECK(check, expression) do { if (!(expression)) probe_failure(check); } while (0)

/* /// 终止当前线程，内核错误返回时立即触发非法指令。 */
static inline _Noreturn void probe_exit(long code)
{
    probe_sys1(SYS_exit, code);
    __builtin_trap();
}

/* /// 终止整个线程组，保留与单线程 exit 不同的资源与信号语义。 */
static inline _Noreturn void probe_exit_group(long code)
{
    probe_sys1(SYS_exit_group, code);
    __builtin_trap();
}

/* /// 从自然对齐的共享 32 位协调字取得发布前的数据，不附加 syscall。 */
static inline unsigned probe_load_acquire(const volatile unsigned *word)
{
    return __atomic_load_n(word, __ATOMIC_ACQUIRE);
}

/* /// 先发布本线程数据，再以真实 32 位存储通知其他观察者。 */
static inline void probe_store_release(volatile unsigned *word, unsigned value)
{
    __atomic_store_n(word, value, __ATOMIC_RELEASE);
}

/* /// 将有效源区间复制到不重叠目标区间。 */
void *memcpy(void *destination, const void *source, size_t count);
/* /// 允许源和目标重叠，复制方向由实际地址关系决定。 */
void *memmove(void *destination, const void *source, size_t count);
/* /// 逐字节填充有效目标区间。 */
void *memset(void *destination, int byte, size_t count);
/* /// 按无符号字节顺序比较有效区间。 */
int memcmp(const void *left, const void *right, size_t count);
/* /// 读取到调用者保证存在的首个 NUL。 */
size_t strlen(const char *text);

/* /// 在已核定有效索引的原始 LP64 初始栈中查看 argv 字节串，不复制或重建栈。 */
static inline const char *probe_initial_argv(const unsigned long *stack, unsigned long index)
{
    return (const char *)stack[index + 1];
}
/* /// 比较包含末尾 NUL 的完整字符串，空指针不等于任何字符串。 */
static inline int probe_string_equal(const char *left, const char *right)
{
    if (!left || !right)
        return 0;
    while (*left == *right) {
        if (!*left)
            return 1;
        ++left;
        ++right;
    }
    return 0;
}

#endif
