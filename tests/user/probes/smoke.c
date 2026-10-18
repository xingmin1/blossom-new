#include "probe.h"
#include <elf.h>

extern unsigned char __image_start[], __phdr_start[];
extern unsigned char __data_start[], __data_end[];
extern unsigned char __bss_start[], __bss_end[];
extern void _start(void);

static volatile unsigned char data_pattern[8193] = {
    [0 ... 8191] = 0x96, [8192] = 0x69
};
static volatile unsigned char bss_pattern[12293];

/* /// 所有检查号经原始线程 exit 交给真实父观察者。 */
_Noreturn void probe_failure(long check)
{
    probe_exit(check);
}

/* /// 初始栈在任何 C 栈帧建立前由入口捕获，逐串核对包含末尾 NUL 的字节。 */
static void check_initial_stack(const unsigned long *stack)
{
    PROBE_CHECK(1, ((uintptr_t)stack & 15) == 0);
    PROBE_CHECK(2, stack[0] == 2);
    PROBE_CHECK(3, probe_string_equal(probe_initial_argv(stack, 0), "/test/bin/user-smoke"));
    PROBE_CHECK(4, probe_string_equal(probe_initial_argv(stack, 1), "raw"));
    PROBE_CHECK(5, stack[3] == 0 && stack[5] == 0);
    PROBE_CHECK(6, probe_string_equal((const char *)stack[4], "USER_SMOKE=1"));
}

#if defined(__loongarch64)
/* /// 独立读取用户 CPUCFG2，按 FPU、LSX、LASX 的硬件依赖推导可声明的浮点能力。 */
static unsigned long loongarch_fp_hwcap(void)
{
    unsigned long config;
    unsigned long index = 2;
    __asm__ volatile("cpucfg %0, %1" : "=r"(config) : "r"(index));
    PROBE_CHECK(10, (config & 1) != 0);
    unsigned long expected = 8;
    if (config & (1UL << 6)) {
        expected |= 16;
        if (config & (1UL << 7))
            expected |= 32;
    }
    return expected;
}
#endif

/* /// 两 ISA 各自核硬件能力位，LA 的完整浮点声明必须与独立 CPUCFG 观察精确一致。 */
static void check_hwcap(unsigned long value)
{
#if defined(__riscv)
    PROBE_CHECK(10, (value & 0x28) == 0x28 && (value & 0x200000) == 0);
#else
    PROBE_CHECK(10, (value & 0x38) == loongarch_fp_hwcap());
#endif
}

/* /// 最多消费 32 对 auxv；必需字段及 HWCAP 必须各出现一次，AT_NULL 值必须为零。 */
static void check_auxv(const unsigned long *auxv)
{
    const unsigned long expected[13][2] = {
        {3, (unsigned long)__phdr_start}, {4, 56}, {5, 3}, {6, 4096},
        {7, 0}, {8, 0}, {9, (unsigned long)_start}, {11, 0}, {12, 0},
        {13, 0}, {14, 0}, {17, 100}, {23, 0}
    };
    unsigned seen = 0;
    for (unsigned pair = 0; pair < 32; ++pair) {
        unsigned long type = auxv[pair * 2];
        unsigned long value = auxv[pair * 2 + 1];
        if (type == AT_NULL) {
            PROBE_CHECK(7, value == 0);
            PROBE_CHECK(8, seen == 16383);
            return;
        }
        PROBE_CHECK(9, type != 33);
        if (type == AT_HWCAP) {
            PROBE_CHECK(7, (seen & 8192) == 0);
            seen |= 8192;
            check_hwcap(value);
            continue;
        }
        for (unsigned field = 0; field < 13; ++field) {
            if (type != expected[field][0])
                continue;
            unsigned bit = 1U << field;
            PROBE_CHECK(7, (seen & bit) == 0);
            seen |= bit;
            PROBE_CHECK(8, value == expected[field][1]);
            break;
        }
    }
    probe_failure(7);
}

/* /// 从实际映射 ELF 读取程序头、入口和 RX/RW 两个 LOAD 身份，不用编译期布局替代。 */
static void check_program_headers(void)
{
    const volatile Elf64_Ehdr *header = (const volatile Elf64_Ehdr *)__image_start;
    const volatile Elf64_Phdr *programs = (const volatile Elf64_Phdr *)__phdr_start;
    _Static_assert(sizeof(Elf64_Phdr) == 56, "ELF64 program header size");
    PROBE_CHECK(8, (uintptr_t)__image_start + header->e_phoff == (uintptr_t)__phdr_start);
    PROBE_CHECK(8, header->e_phentsize == 56 && header->e_phnum == 3);
    PROBE_CHECK(8, header->e_entry == (uintptr_t)_start);
    PROBE_CHECK(8, programs[0].p_vaddr == (uintptr_t)__phdr_start);
    PROBE_CHECK(8, programs[0].p_filesz == 168);
    PROBE_CHECK(8, programs[1].p_type == PT_LOAD && programs[1].p_flags == (PF_R | PF_X));
    PROBE_CHECK(8, programs[2].p_type == PT_LOAD && programs[2].p_flags == (PF_R | PF_W));
}

/* /// volatile 逐字节观察实际文件数据和 BSS，再以实际地址 xor 图样覆盖并读回整个零区。 */
static void check_loaded_bytes(void)
{
    /* BSS 通过链接器边界访问，显式引用对象使链接 GC 保留原 12293 字节载荷。 */
    __asm__ volatile("" : : "r"(bss_pattern) : "memory");
    PROBE_CHECK(11, *(volatile unsigned char *)__data_start == 0x3c);
    for (unsigned i = 0; i < 8192; ++i)
        PROBE_CHECK(11, data_pattern[i] == 0x96);
    PROBE_CHECK(11, data_pattern[8192] == 0x69);
    PROBE_CHECK(11, *(volatile unsigned char *)((uintptr_t)__data_end - 1) == 0xc3);

    for (uintptr_t address = (uintptr_t)__bss_start; address < (uintptr_t)__bss_end; ++address)
        PROBE_CHECK(12, *(volatile unsigned char *)address == 0);
    for (uintptr_t address = (uintptr_t)__bss_start; address < (uintptr_t)__bss_end; ++address)
        *(volatile unsigned char *)address = (unsigned char)(address ^ 0x5a);
    for (uintptr_t address = (uintptr_t)__bss_start; address < (uintptr_t)__bss_end; ++address)
        PROBE_CHECK(13, *(volatile unsigned char *)address == (unsigned char)(address ^ 0x5a));
}

/* /// 纯用户寄存器递减保持原长计算窗口，禁止优化器删除，期间不以 syscall 人工让出 CPU。 */
static void timer_preemption_window(void)
{
    unsigned long remaining = 0x40000000;
    do {
        __asm__ volatile("" : "+r"(remaining));
        --remaining;
    } while (remaining);
}

/* /// 依次核原始装载事实、身份、负 errno 与真实 timer 抢占后的同一 PID。 */
_Noreturn void probe_main(const unsigned long *initial_stack)
{
    check_initial_stack(initial_stack);
    check_auxv(initial_stack + 6);
    check_program_headers();
    check_loaded_bytes();
    long pid = probe_sys0(SYS_getpid);
    PROBE_CHECK(14, 0 < pid);
    long tid = probe_sys0(SYS_gettid);
    PROBE_CHECK(15, 0 < tid && tid == pid);
    PROBE_CHECK(16, probe_sys0(SYS_sched_yield) == 0);
    PROBE_CHECK(17, probe_sys0(0x7fffffff) == -ENOSYS);
    timer_preemption_window();
    PROBE_CHECK(14, probe_sys0(SYS_getpid) == pid);
    probe_exit(0);
}
