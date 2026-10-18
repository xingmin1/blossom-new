#include "signal.h"

long signal_pid, signal_tid;
unsigned char signal_mode;
volatile unsigned signal_state[16];
unsigned long signal_initial_sp, signal_alt_base, signal_outer_sp;
volatile unsigned *signal_checkpoint;
unsigned signal_lanes;
unsigned long signal_fault_address, signal_fault_pc, signal_resume_pc;
long signal_fault_signum, signal_fault_code;
unsigned signal_fault_flags;
unsigned char signal_child_stack[16384] __attribute__((aligned(32)));
unsigned char signal_thread_stack[16384] __attribute__((aligned(32)));
unsigned char signal_new_stack[16384] __attribute__((aligned(32)));
unsigned long signal_fp_values[128] __attribute__((aligned(32)));
unsigned long signal_fp_output[128] __attribute__((aligned(32)));
unsigned long signal_fp_clobber[128] __attribute__((aligned(32)));
static unsigned char read_buffer[65536];
#if defined(__loongarch64)
static unsigned long original_fcsr;
#endif
static struct SignalStack altstack;
static const struct SignalTime zero_time;

/* /// 保留原始退出编号；真实默认死亡仍由内核wait的低七位区别。 */
_Noreturn void probe_failure(long check)
{
    probe_exit(check);
}

/* /// 安装无restorer字段的内核raw action，RA由内核设置。 */
void signal_action(int signum, void (*handler)(void), unsigned long flags,
                   unsigned long mask, long check)
{
    struct SignalAction action = { (unsigned long)handler, flags, mask };
    PROBE_CHECK(check, probe_sys4(SYS_rt_sigaction, signum, &action, 0, 8) == 0);
}

/* /// 向内核交付完整8字节mask，不能用libc sigset_t的尺寸。 */
void signal_mask(int how, unsigned long bits, long check)
{
    PROBE_CHECK(check, probe_sys4(SYS_rt_sigprocmask, how, &bits, 0, 8) == 0);
}

/* /// 明确保留kill的组目标语义。 */
void signal_send(long pid, int signum, long check)
{
    PROBE_CHECK(check, probe_sys2(SYS_kill, pid, signum) == 0);
}

/* /// waitid取得一次真实事件，child的完整检查号不能改写成统一错误。 */
void signal_wait_child(long pid)
{
    struct SignalInfo info;
    PROBE_CHECK(137, probe_sys5(SYS_waitid, 1, pid, &info, 4, 0) == 0);
    PROBE_CHECK(138, info.code == 1);
    PROBE_CHECK(139, info.payload.sender.pid == pid);
    int status;
    memcpy(&status, (unsigned char *)&info + 24, sizeof(status));
    if (status != 0)
        probe_failure(status);
}

/* /// VM child使用独立栈；fork也由同一clone桥分流，不返回父C帧。 */
long signal_spawn(unsigned long flags, ProbeChildEntry entry, void *argument)
{
    return probe_clone(flags, (flags & 0x100) ? signal_child_stack + 16384 : 0,
                       0, 0, 0, entry, argument);
}

/* /// 按原发送者构造queueinfo，填满时调用方仍取得真实EAGAIN。 */
static long queue_signal(int signum, unsigned long value)
{
    struct SignalInfo info = { .signum = signum, .code = -1,
        .payload.sender = { .pid = signal_pid, .uid = 0, .value = value } };
    return probe_sys3(SYS_rt_sigqueueinfo, signal_pid, signum, &info);
}

/* /// 领取单个pending实例，读取原始128字节而不复用宿主信号结构。 */
static long take_signal(unsigned long mask, struct SignalInfo *info)
{
    return probe_sys4(SYS_rt_sigtimedwait, &mask, info, &zero_time, 8);
}

/* /// 队列实例保留首次完整sender和值，各字段沿原检查号独立断言。 */
static void take_value(int signum, unsigned long value, long check)
{
    struct SignalInfo info;
    PROBE_CHECK(check, take_signal(1UL << (signum - 1), &info) == signum);
    PROBE_CHECK(check + 1, info.code == -1);
    PROBE_CHECK(check + 2, info.payload.sender.pid == signal_pid);
    PROBE_CHECK(check + 3, info.payload.sender.uid == 0);
    PROBE_CHECK(check + 4, info.payload.sender.value == value);
}

/* /// private先于group领取，两者的info来源不能合并。 */
static void take_sender(long pid, int code)
{
    struct SignalInfo info;
    PROBE_CHECK(13, take_signal(0x200, &info) == 10);
    PROBE_CHECK(14, info.code == code);
    PROBE_CHECK(15, info.payload.sender.pid == pid);
}

/* /// 普通信号合并保首次info；RT逐实例FIFO，copyout失败仍消费。 */
static void pending(void)
{
    struct SignalInfo info;
    unsigned long limit[2];
    unsigned long old_limit[2];
    PROBE_CHECK(1, probe_sys4(261, 0, 11, 0, old_limit) == 0);
    limit[0] = 256;
    limit[1] = old_limit[1];
    PROBE_CHECK(1, probe_sys4(261, 0, 11, limit, 0) == 0);
    signal_mask(0, 0x80000a00, 2);
    PROBE_CHECK(3, queue_signal(10, 0x31) == 0);
    PROBE_CHECK(4, queue_signal(10, 0x32) == 0);
    take_value(10, 0x31, 5);
    PROBE_CHECK(10, take_signal(0x200, &info) == -EAGAIN);
    PROBE_CHECK(11, probe_sys3(SYS_tgkill, signal_pid, signal_tid, 10) == 0);
    signal_send(signal_pid, 10, 12);
    take_sender(signal_pid, -6);
    take_sender(signal_pid, 0);
    PROBE_CHECK(15, queue_signal(32, 0x41) == 0);
    PROBE_CHECK(16, queue_signal(32, 0x42) == 0);
    PROBE_CHECK(17, queue_signal(32, 0x43) == 0);
    take_value(32, 0x41, 18);
    take_value(32, 0x42, 23);
    take_value(32, 0x43, 28);
    unsigned long count = 0;
    while (count < 65536) {
        long result = queue_signal(32, count);
        if (result == -EAGAIN)
            break;
        PROBE_CHECK(33, result == 0);
        ++count;
    }
    PROBE_CHECK(34, count < 65536);
    PROBE_CHECK(35, count != 0);
    for (unsigned long index = 0; index < count; ++index) {
        PROBE_CHECK(36, take_signal(0x80000000, &info) == 32);
        PROBE_CHECK(37, info.payload.sender.value == index);
    }
    PROBE_CHECK(38, queue_signal(32, 0x71) == 0);
    PROBE_CHECK(39, take_signal(0x80000000, (void *)-8L) == -EFAULT);
    PROBE_CHECK(40, take_signal(0x80000000, &info) == -EAGAIN);
    PROBE_CHECK(41, probe_sys4(261, 0, 11, old_limit, 0) == 0);
}

/* /// C handler正常返回到内核安装的RA，sigreturn本身不经libc。 */
static void count_handler(void)
{
    ++signal_state[3];
}

/* /// NODEFER仅解除本号自动阻断，action.mask中的USR2仍延迟。 */
static void nodefer_handler(void)
{
    ++signal_state[0];
    if (signal_state[0] == 1) {
        signal_state[2] = 1;
        signal_send(signal_pid, 12, 141);
        PROBE_CHECK(142, signal_state[1] == 0);
        signal_send(signal_pid, 10, 143);
        signal_state[2] = 0;
    }
}

/* /// USR2只有在外层状态解除后才允许进入。 */
static void usr2_handler(void)
{
    PROBE_CHECK(144, signal_state[2] == 0);
    ++signal_state[1];
}

/* /// 当前实例完成原handler，动作更新只影响下一次递送。 */
static void swap_handler(void)
{
    signal_action(10, count_handler, 4, 0, 175);
    ++signal_state[3];
}

/* /// 保留所有动作规则与真实第二次默认终止。 */
static void actions(void)
{
    struct SignalAction old;
    unsigned long mask;
    if (signal_mode == 'x') {
        signal_action(10, count_handler, 0x80000004UL, 0, 197);
        signal_send(signal_pid, 10, 198);
        signal_send(signal_pid, 10, 199);
        probe_failure(200);
    }
    signal_action(10, nodefer_handler, 0x40000004UL, 0x800, 41);
    signal_action(12, usr2_handler, 4, 0, 42);
    signal_send(signal_pid, 10, 43);
    PROBE_CHECK(44, signal_state[0] == 2);
    PROBE_CHECK(45, signal_state[1] == 1);
    PROBE_CHECK(46, signal_state[2] == 0);
    signal_send(signal_pid, 17, 47);
    signal_send(signal_pid, 28, 48);
    signal_action(10, count_handler, 0x80000004UL, 0, 49);
    signal_send(signal_pid, 10, 50);
    PROBE_CHECK(51, probe_sys4(SYS_rt_sigaction, 10, 0, &old, 8) == 0);
    PROBE_CHECK(52, old.handler == 0);
    PROBE_CHECK(53, signal_state[3] == 1);
    signal_action(10, swap_handler, 4, 0, 155);
    signal_send(signal_pid, 10, 156);
    PROBE_CHECK(157, signal_state[3] == 2);
    signal_send(signal_pid, 10, 158);
    PROBE_CHECK(159, signal_state[3] == 3);
    signal_mask(2, 0x40100, 160);
    PROBE_CHECK(161, probe_sys4(SYS_rt_sigprocmask, 2, 0, &mask, 8) == 0);
    PROBE_CHECK(162, mask == 0);
    signal_action(12, (void (*)(void))1, 0, 0, 194);
    signal_send(signal_pid, 12, 195);
    PROBE_CHECK(196, signal_state[1] == 1);
}

/* /// 从用户帧按ABI宽度读机器字，不依赖宿主ucontext定义。 */
static unsigned long word(const unsigned char *context, unsigned offset)
{
    unsigned long value;
    memcpy(&value, context + offset, sizeof(value));
    return value;
}

/* /// 读取扩展记录中的32位字段，避免错误的long符号扩展。 */
static unsigned field(const unsigned char *context, unsigned offset)
{
    unsigned value;
    memcpy(&value, context + offset, sizeof(value));
    return value;
}

/* /// 用户合法修改自己的恢复PC/SP，内核候选导入仍负责完整校验。 */
static void put_word(unsigned char *context, unsigned offset, unsigned long value)
{
    memcpy(context + offset, &value, sizeof(value));
}

/* /// 比较桥在任何C调用之前保存的全部有效硬件lanes。 */
void signal_fp_compare(long check)
{
    for (unsigned index = 0; index < 32 * signal_lanes; ++index)
        PROBE_CHECK(check, signal_fp_output[index] == signal_fp_values[index]);
}

/* /// 验证内核帧完整FP内容与扩展头，随后只修改合法恢复字段。 */
static void edit_frame(long signum, struct SignalInfo *info, unsigned char *context)
{
    PROBE_CHECK(145, signum == 10);
    PROBE_CHECK(146, info->signum == 10);
    PROBE_CHECK(176, info->code == 0);
    PROBE_CHECK(177, info->payload.sender.pid == signal_pid);
#if defined(__riscv)
    PROBE_CHECK(178, field(context, 688) == 0x20);
    PROBE_CHECK(179, field(context, 948) == 0);
    PROBE_CHECK(180, field(context, 952) == 0);
    PROBE_CHECK(181, field(context, 956) == 0);
    unsigned offset = 432;
    put_word(context, 192, (unsigned long)(signal_new_stack + 16384));
#else
    unsigned bytes = 256 * signal_lanes;
    unsigned magic = signal_lanes == 4 ? 0x41535801 :
                     signal_lanes == 2 ? 0x53580001 : 0x46505501;
    PROBE_CHECK(178, field(context, 472 + bytes) == 0x100);
    PROBE_CHECK(223, word(context, 464 + bytes) == 0x0100010001000100UL);
    PROBE_CHECK(179, field(context, 448) == magic);
    unsigned size = field(context, 452);
    PROBE_CHECK(180, bytes + 32 <= size);
    unsigned padding = size - bytes - 32;
    if (padding != 0) {
        PROBE_CHECK(180, signal_lanes == 4);
        PROBE_CHECK(180, padding == 16);
    }
    PROBE_CHECK(181, field(context, 448 + size) == 0);
    PROBE_CHECK(181, field(context, 452 + size) == 0);
    PROBE_CHECK(181, ((unsigned long)(context + 464) & (signal_lanes == 4 ? 31 : 15)) == 0);
    unsigned offset = 464;
    put_word(context, 208, (unsigned long)(signal_new_stack + 16384));
    put_word(context, 352, 0x76543000);
#endif
    for (unsigned index = 0; index < 32 * signal_lanes; ++index)
        PROBE_CHECK(182, word(context, offset + 8 * index) == signal_fp_values[index]);
    put_word(context, 176, (unsigned long)signal_edited_pc);
}

/* /// 薄入口提供的SP是handler原始SP，所有帧与嵌套规则在共同C检查。 */
void signal_handle(long signum, struct SignalInfo *info, unsigned char *context,
                   unsigned long entry_sp)
{
    if (signal_mode == 'r') {
        ++signal_state[5];
        return;
    }
    if (signal_mode == 'f') {
        edit_frame(signum, info, context);
        return;
    }
    if (signal_mode == 'z') {
#if defined(__riscv)
        PROBE_CHECK(240, signum == signal_fault_signum);
        PROBE_CHECK(241, (unsigned long)info == entry_sp);
        PROBE_CHECK(242, (unsigned long)context == entry_sp + 128);
        PROBE_CHECK(243, info->signum == signal_fault_signum);
        PROBE_CHECK(244, info->error == 0);
        PROBE_CHECK(245, info->code == signal_fault_code);
        PROBE_CHECK(246, info->payload.address == signal_fault_address);
        PROBE_CHECK(247, word(context, 176) == signal_fault_pc);
        ++signal_state[0];
#else
        PROBE_CHECK(248, signum == signal_fault_signum);
        PROBE_CHECK(249, info->signum == signal_fault_signum);
        PROBE_CHECK(250, info->error == 0);
        PROBE_CHECK(251, info->code == signal_fault_code);
        PROBE_CHECK(252, info->payload.address == signal_fault_address);
        PROBE_CHECK(253, (unsigned long)context == entry_sp + 128);
        PROBE_CHECK(254, word(context, 176) == signal_fault_pc);
        PROBE_CHECK(255, (field(context, 440) & 0xc0000000U) == signal_fault_flags);
        ++signal_state[3];
#endif
        put_word(context, 176, signal_resume_pc);
        return;
    }
    if (signal_mode == 'b' || signal_mode == 'k') {
        if (signal_mode == 'k')
            put_word(context, 176, -4096UL);
        else {
            unsigned corrupt = 1;
#if defined(__riscv)
            memcpy(context + 948, &corrupt, 4);
#else
            memcpy(context + 448, &corrupt, 4);
#endif
        }
        return;
    }
    if (signum == 10) {
        PROBE_CHECK(147, signal_alt_base < entry_sp && entry_sp <= signal_alt_base + 16384);
        signal_outer_sp = entry_sp;
        struct SignalStack old;
        PROBE_CHECK(148, probe_sys2(SYS_sigaltstack, 0, &old) == 0);
        PROBE_CHECK(149, old.flags == 1);
        PROBE_CHECK(150, probe_sys2(SYS_sigaltstack, &altstack, 0) == -EPERM);
        signal_send(signal_pid, 12, 151);
    } else {
        PROBE_CHECK(152, signal_alt_base < entry_sp && entry_sp < signal_outer_sp);
        PROBE_CHECK(224, word(context, 16) == signal_alt_base);
        PROBE_CHECK(225, field(context, 24) == 0);
        PROBE_CHECK(226, word(context, 32) == 16384);
        signal_state[4] = 2;
    }
}

/* /// fork child只消费自己的COW备用栈。 */
static void nested_child(void *argument, unsigned long sp, unsigned long tp)
{
    (void)argument; (void)sp; (void)tp;
    signal_pid = probe_sys0(SYS_getpid);
    signal_send(signal_pid, 10, 67);
    PROBE_CHECK(68, signal_state[4] == 2);
    probe_exit(0);
}

/* /// 私有备用栈先真实触页，child帧copyout后父逐字比较全部16KiB。 */
static void nested(void)
{
    long address = probe_sys6(SYS_mmap, 0, 16384, PROT_READ | PROT_WRITE,
                               MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    PROBE_CHECK(61, 0 <= address);
    signal_alt_base = address;
    volatile unsigned long *words = (void *)address;
    for (unsigned index = 0; index < 2048; ++index)
        words[index] = 0x5a5a5a5a5a5a5a5aUL;
    altstack = (struct SignalStack){ .base = address, .size = 16384 };
    PROBE_CHECK(62, probe_sys2(SYS_sigaltstack, &altstack, 0) == 0);
    signal_action(10, signal_handler, 0x08000004, 0, 63);
    signal_action(12, signal_handler, 0x08000004, 0, 64);
    long child = signal_spawn(17, nested_child, 0);
    PROBE_CHECK(65, 0 <= child);
    signal_wait_child(child);
    for (unsigned index = 0; index < 2048; ++index)
        PROBE_CHECK(66, words[index] == 0x5a5a5a5a5a5a5a5aUL);
}

/* /// 为每条真实故障设置siginfo期望，不以伪造信号替代访问。 */
static void fault_expect(long signum, long code, void *address, unsigned flags)
{
    signal_fault_signum = signum;
    signal_fault_code = code;
    signal_fault_address = (unsigned long)address;
    signal_fault_flags = flags;
}

/* /// 两架构原有检查号保留；故障发生PC由指令薄桥写入。 */
static void synchronous_faults(void)
{
#if defined(__riscv)
#define ISA_CHECK(rv, la) (rv)
#define FAULT_COUNT signal_state[0]
#else
#define ISA_CHECK(rv, la) (la)
#define FAULT_COUNT signal_state[3]
#endif
    signal_action(11, signal_handler, 4, 0, 227);
    signal_action(7, signal_handler, 4, 0, ISA_CHECK(228, 227));
    signal_action(4, signal_handler, 4, 0, ISA_CHECK(229, 227));
    long page = probe_sys6(SYS_mmap, 0, 4096, PROT_READ | PROT_WRITE,
                           MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    PROBE_CHECK(ISA_CHECK(230, 228), 0 <= page);
#if defined(__loongarch64)
    *(volatile unsigned long *)page = 0;
#endif
    PROBE_CHECK(ISA_CHECK(231, 229), probe_sys2(SYS_munmap, page, 4096) == 0);
    fault_expect(11, 1, (void *)page, 0x40000000);
    signal_fault_load((void *)page, ISA_CHECK(249, 230));
    PROBE_CHECK(ISA_CHECK(249, 231), FAULT_COUNT == 1);
    page = probe_sys6(SYS_mmap, 0, 4096, PROT_READ | PROT_WRITE,
                      MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    PROBE_CHECK(232, 0 <= page);
    *(volatile unsigned long *)page = 0;
    PROBE_CHECK(233, probe_sys3(SYS_mprotect, page, 4096, PROT_NONE) == 0);
    fault_expect(11, 2, (void *)page, 0x40000000);
    signal_fault_load((void *)page, ISA_CHECK(250, 234));
    PROBE_CHECK(ISA_CHECK(250, 235), FAULT_COUNT == 2);
    PROBE_CHECK(ISA_CHECK(234, 236), probe_sys3(SYS_mprotect, page, 4096, PROT_READ) == 0);
    fault_expect(11, 2, (void *)page, 0x80000000);
    signal_fault_store((void *)page, ISA_CHECK(251, 237));
    PROBE_CHECK(ISA_CHECK(251, 238), FAULT_COUNT == 3);
    PROBE_CHECK(ISA_CHECK(235, 239), probe_sys3(SYS_mprotect, page, 4096, PROT_READ | PROT_WRITE) == 0);
    *(volatile unsigned long *)page = 0x51a3;
#if defined(__loongarch64)
    PROBE_CHECK(240, *(volatile unsigned long *)page == 0x51a3);
#endif
    PROBE_CHECK(ISA_CHECK(236, 240), probe_sys2(SYS_munmap, page, 4096) == 0);
    long fd = probe_sys4(SYS_openat, AT_FDCWD, "/test/io-data.bin", O_RDONLY, 0);
    PROBE_CHECK(ISA_CHECK(237, 241), 0 <= fd);
    page = probe_sys6(SYS_mmap, 0, 4096, PROT_READ, MAP_PRIVATE, fd, 16384);
    PROBE_CHECK(ISA_CHECK(238, 242), 0 <= page);
#if defined(__loongarch64)
    PROBE_CHECK(243, probe_sys1(SYS_close, fd) == 0);
#endif
    fault_expect(7, 2, (void *)page, 0x40000000);
    signal_fault_load((void *)page, ISA_CHECK(252, 244));
    PROBE_CHECK(ISA_CHECK(252, 245), FAULT_COUNT == 4);
    PROBE_CHECK(ISA_CHECK(239, 245), probe_sys2(SYS_munmap, page, 4096) == 0);
#if defined(__riscv)
    PROBE_CHECK(239, probe_sys1(SYS_close, fd) == 0);
#endif
    fault_expect(4, 1, 0, 0);
    signal_fault_illegal(ISA_CHECK(253, 246));
    PROBE_CHECK(ISA_CHECK(253, 247), FAULT_COUNT == 5);
#undef ISA_CHECK
#undef FAULT_COUNT
}

/* /// e模式的child由内核控制者wait/join，不能在用户侧提前消费。 */
static void exit_child(void *argument, unsigned long sp, unsigned long tp)
{
    (void)argument; (void)sp; (void)tp;
    probe_exit(0);
}

/* /// 初始SP下的两槽保持原PID64、phase32偏移，等内核真实离栈后再查pending。 */
static void exit_notification(void)
{
    signal_mask(0, 0x10000, 2);
    for (unsigned phase = 1; phase <= 3; phase += 2) {
        long child = signal_spawn(phase == 1 ? 0x100 : 0x111, exit_child, 0);
#if defined(__riscv)
        PROBE_CHECK(3, 0 <= child);
#else
        PROBE_CHECK(phase == 1 ? 3 : 6, 0 <= child);
#endif
        memcpy((void *)signal_checkpoint, &child, 8);
        probe_store_release(signal_checkpoint + 2, phase);
        while (probe_load_acquire(signal_checkpoint + 2) != phase + 1) { }
        unsigned long mask;
#if defined(__riscv)
        PROBE_CHECK(4, probe_sys2(SYS_rt_sigpending, &mask, 8) == 0);
        PROBE_CHECK(phase + 5, (mask & 0x10000) == (phase == 1 ? 0 : 0x10000));
#else
        PROBE_CHECK(phase == 1 ? 4 : 7, probe_sys2(SYS_rt_sigpending, &mask, 8) == 0);
        PROBE_CHECK(phase == 1 ? 5 : 8, (mask & 0x10000) == (phase == 1 ? 0 : 0x10000));
#endif
    }
}

/* /// sigsuspend原参数页被父解除后，重放EFAULT必须已恢复原mask。 */
static void suspend_child(void *argument, unsigned long sp, unsigned long tp)
{
    (void)argument; (void)sp; (void)tp;
    signal_pid = probe_sys0(SYS_getpid);
#if defined(__riscv)
#define SUSPEND_CHECK(rv, la) (rv)
#else
#define SUSPEND_CHECK(rv, la) (la)
#endif
    unsigned long mask;
    signal_mask(2, 0x10200, SUSPEND_CHECK(23, 12));
    PROBE_CHECK(SUSPEND_CHECK(24, 13), probe_sys2(SYS_rt_sigsuspend, 0x43001000, 8) == -EFAULT);
    PROBE_CHECK(SUSPEND_CHECK(25, 14), probe_sys4(SYS_rt_sigprocmask, 2, 0, &mask, 8) == 0);
    PROBE_CHECK(SUSPEND_CHECK(26, 15), mask == 0x10200);
    signal_mask(2, 0x200, SUSPEND_CHECK(27, 16));
    signal_action(12, count_handler, 4, 0, SUSPEND_CHECK(28, 17));
    signal_send(signal_pid, 12, SUSPEND_CHECK(29, 18));
#if defined(__loongarch64)
    PROBE_CHECK(19, signal_state[3] == 1);
#endif
    PROBE_CHECK(SUSPEND_CHECK(30, 20), probe_sys4(SYS_rt_sigprocmask, 2, 0, &mask, 8) == 0);
    PROBE_CHECK(SUSPEND_CHECK(31, 21), mask == 0x200);
    probe_exit(0);
#undef SUSPEND_CHECK
}

/* /// 内核消费STOP事件后才释放父解除页，父phase3后等待原child。 */
static void suspend_unmap(void)
{
    long address = probe_sys6(SYS_mmap, 0x43001000, 4096, PROT_READ | PROT_WRITE,
                               MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0);
#if defined(__riscv)
    PROBE_CHECK(20, address == 0x43001000);
#else
    PROBE_CHECK(9, address == 0x43001000);
#endif
    *(volatile unsigned long *)address = 0x800;
    long child = signal_spawn(0x111, suspend_child, 0);
#if defined(__riscv)
    PROBE_CHECK(21, 0 <= child);
#else
    PROBE_CHECK(10, 0 <= child);
#endif
    memcpy((void *)signal_checkpoint, &child, 8);
    probe_store_release(signal_checkpoint + 2, 1);
    while (probe_load_acquire(signal_checkpoint + 2) != 2) { }
#if defined(__riscv)
    PROBE_CHECK(22, probe_sys2(SYS_munmap, address, 4096) == 0);
#else
    PROBE_CHECK(11, probe_sys2(SYS_munmap, address, 4096) == 0);
#endif
    probe_store_release(signal_checkpoint + 2, 3);
    signal_wait_child(child);
}

/* /// VM+SIGHAND不合并两个进程组pending，也不继承父已排队实例。 */
static void pending_child(void *argument, unsigned long sp, unsigned long tp)
{
    (void)argument; (void)sp; (void)tp;
    struct SignalInfo info;
    PROBE_CHECK(114, take_signal(0x200, &info) == -EAGAIN);
    long pid = probe_sys0(SYS_getpid);
    signal_send(pid, 10, 115);
    take_sender(pid, 0);
    probe_store_release(signal_state + 11, 1);
    while (probe_load_acquire(signal_state + 9) == 0) { }
    PROBE_CHECK(116, take_signal(0x200, &info) == -EAGAIN);
    probe_exit(0);
}

/* /// 两组在共享地址空间中分别领取自己的信号实例。 */
static void vm_pending(void)
{
    signal_mask(0, 0x200, 106);
    PROBE_CHECK(107, queue_signal(10, 0x91) == 0);
    long child = signal_spawn(0x911, pending_child, 0);
    PROBE_CHECK(108, 0 <= child);
    while (probe_load_acquire(signal_state + 11) == 0) { }
    take_value(10, 0x91, 109);
    probe_store_release(signal_state + 9, 1);
    signal_wait_child(child);
}

/* /// 原read标记保持在初始SP-8；父可观察真正Blocked与canonical填充责任。 */
static void read_pending(void)
{
    signal_action(14, signal_handler, 4, 0, 117);
    long fd = probe_sys4(SYS_openat, AT_FDCWD, "/test/data.bin", O_RDONLY, 0);
    PROBE_CHECK(118, fd == 0);
    *(volatile unsigned long *)(signal_initial_sp - 8) = 0x51a2;
    __atomic_thread_fence(__ATOMIC_SEQ_CST);
    PROBE_CHECK(120, probe_sys3(SYS_read, fd, read_buffer, 65536) == 65536);
    PROBE_CHECK(171, signal_state[5] == 1);
    for (unsigned index = 0; index < 65536; ++index) {
        unsigned byte = ((index >> 9) * 17 + (index & 511) + 37) & 255;
        PROBE_CHECK(121, read_buffer[index] == byte);
    }
    PROBE_CHECK(122, probe_sys3(SYS_lseek, fd, 0, 1) == 65536);
    PROBE_CHECK(123, probe_sys1(SYS_close, fd) == 0);
}

#if defined(__loongarch64)
/* /// LA pending cause处理读取原帧与新帧，不允许下一条用户指令提前提交标记。 */
void signal_inject_context(unsigned char *context)
{
    original_fcsr = (unsigned long)(context + 472 + 256 * signal_lanes);
    unsigned value = 0x01000101;
    memcpy((void *)original_fcsr, &value, 4);
    put_word(context, 208, word(context, 208) - 4096);
}

/* /// cause必须先清原槽再递送SIGFPE，新帧同样只含清除后的FCSR。 */
void signal_fpe_context(long signum, struct SignalInfo *info, unsigned char *context)
{
    PROBE_CHECK(188, signum == 8);
    PROBE_CHECK(189, info->signum == 8);
    PROBE_CHECK(190, info->code == 128);
    PROBE_CHECK(191, signal_state[13] == 0);
    PROBE_CHECK(192, *(volatile unsigned *)original_fcsr == 0x101);
    PROBE_CHECK(193, field(context, 472 + 256 * signal_lanes) == 0x101);
    signal_state[12] = 1;
}
/* /// 完成fork硬件现场检查后，安装启用cause及再次SIGFPE的真实处理函数。 */
void signal_pending_setup(void)
{
    signal_action(8, signal_fpe_handler, 4, 0, 183);
    signal_action(10, signal_inject_handler, 4, 0, 184);
}
#endif

/* /// 硬件桥只有完成最后cause检查才可报告成功。 */
_Noreturn void signal_frame_done(void)
{
    probe_exit(0);
}

/* /// 由ISA入口预留初始SP下32字节，C调用栈不得覆盖内核checkpoint。 */
_Noreturn void signal_main(const unsigned long *initial_stack)
{
    PROBE_CHECK(1, initial_stack[0] == 2);
    signal_initial_sp = (unsigned long)initial_stack;
    signal_checkpoint = (void *)(signal_initial_sp - 16);
    signal_mode = *(const unsigned char *)initial_stack[2];
    if (signal_mode == 'i')
        signal_infinite(signal_initial_sp);
    signal_pid = probe_sys0(SYS_getpid);
    signal_tid = probe_sys0(SYS_gettid);
    const unsigned long *auxv = initial_stack + 5;
    while (auxv[0] != 17) {
        PROBE_CHECK(153, auxv[0] != 0);
        auxv += 2;
    }
    PROBE_CHECK(154, auxv[1] == 100);
    signal_lanes = signal_fp_width();
    switch (signal_mode) {
    case 'p': pending(); break;
    case 'a': case 'x': actions(); break;
    case 'f':
        for (unsigned index = 0; index < 128; ++index)
            signal_fp_values[index] = index < 32 ? 0x3ff0000000000000UL + index * 0x10001UL :
                                                 0x8190000000000000UL + index;
        signal_action(10, signal_edit_handler, 4, 0, 54);
        signal_frame_enter(signal_pid);
    case 'z': synchronous_faults(); break;
    case 'n': nested(); break;
    case 'b': case 'k':
        signal_action(10, signal_handler, 4, 0, 69);
        signal_send(signal_pid, 10, 70);
        probe_failure(71);
    case 'w':
        signal_action(10, count_handler, 4, 0, 72);
        signal_bad_stack(signal_pid);
    case 'v': vm_pending(); break;
    case 'e':
        memset((void *)signal_checkpoint, 0, 16);
        exit_notification(); break;
    case 'u':
        memset((void *)signal_checkpoint, 0, 16);
        suspend_unmap(); break;
    case 'r': read_pending(); break;
    case 's': case 'd': case 'g': case 'c': signal_time_mode();
    default: probe_failure(1);
    }
    probe_exit(0);
}
