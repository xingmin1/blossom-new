#ifndef SIGNAL_PROBE_PRIVATE_H
#define SIGNAL_PROBE_PRIVATE_H
#include "probe.h"

struct SignalInfo {
    int signum, error, code, padding;
    union {
        struct { int pid; unsigned uid; unsigned long value; } sender;
        unsigned long address;
        unsigned char bytes[112];
    } payload;
};
struct SignalAction { unsigned long handler, flags, mask; };
struct SignalStack { unsigned long base; unsigned flags, padding; unsigned long size; };
struct SignalTime { long seconds, nanoseconds; };
struct SignalTimer { long interval_seconds, interval_us, seconds, us; };
_Static_assert(sizeof(struct SignalInfo) == 128, "raw siginfo size");
_Static_assert(sizeof(struct SignalAction) == 24, "raw action has no restorer");
_Static_assert(sizeof(struct SignalStack) == 24, "raw altstack size");
_Static_assert(sizeof(struct SignalTime) == 16, "raw timespec size");
_Static_assert(sizeof(struct SignalTimer) == 32, "raw itimerval size");
_Static_assert(offsetof(struct SignalInfo, payload.sender.value) == 24, "raw sender offset");

extern long signal_pid, signal_tid;
extern unsigned char signal_mode;
extern volatile unsigned signal_state[16];
extern unsigned long signal_alt_base, signal_outer_sp;
extern unsigned long signal_initial_sp;
extern unsigned char signal_child_stack[16384], signal_thread_stack[16384];
extern unsigned char signal_new_stack[16384];
extern unsigned long signal_fp_values[128], signal_fp_output[128];
extern unsigned long signal_fp_clobber[128];
extern unsigned signal_lanes;
extern unsigned long signal_fault_address, signal_fault_pc, signal_resume_pc;
extern long signal_fault_signum, signal_fault_code;
extern unsigned signal_fault_flags;
extern volatile unsigned *signal_checkpoint;

/* /// 安装24字节内核动作，handler由内核RA trampoline返回。 */
void signal_action(int signum, void (*handler)(void), unsigned long flags,
                   unsigned long mask, long check);
/* /// 改变真实blocked mask，保留原始负errno。 */
void signal_mask(int how, unsigned long bits, long check);
/* /// 向明确进程发送信号，不以线程ID替代进程组身份。 */
void signal_send(long pid, int signum, long check);
/* /// 消费真实子事件并将完整child失败编号传给父。 */
void signal_wait_child(long pid);
/* /// 在固定独立新栈上创建真实子执行者。 */
long signal_spawn(unsigned long flags, ProbeChildEntry entry, void *argument);
/* /// 处理三种时间协议及真实CPU账，完成后退出当前线程。 */
_Noreturn void signal_time_mode(void);
/* /// 共同handler按真实入口SP检查信号载荷与帧，不猜测C prologue大小。 */
void signal_handle(long signum, struct SignalInfo *info, unsigned char *context,
                   unsigned long entry_sp);
/* /// 共同帧检查只消费桥在C之前捕获的硬件输出。 */
void signal_fp_compare(long check);
/* /// 查询最宽硬件lane数；RV固定为1。 */
unsigned signal_fp_width(void);
/* /// 三参数信号入口先捕获SP，再调用共同C帧业务。 */
void signal_handler(void);
/* /// 帧业务返回后破坏全部FP、TLS/FCC，避免C ABI恢复掩盖sigreturn。 */
void signal_edit_handler(void);
/* /// 在精确硬件边界生成并观察完整FP现场，成功后终止。 */
_Noreturn void signal_frame_enter(long pid);
/* /// 以任意坏SP发送信号，禁止通过普通C栈调用掩盖frame copyout故障。 */
_Noreturn void signal_bad_stack(long pid);
/* /// 故障指令在桥内保留真实发生PC，handler只能修改帧PC。 */
void signal_fault_load(void *address, long failure);
/* /// 真实写故障未发生时保留原fallthrough检查号。 */
void signal_fault_store(void *address, long failure);
/* /// 非法指令未被内核递送时终止于原检查号。 */
void signal_fault_illegal(long failure);
/* /// SP-8标记位于最初内核栈顶，纯用户计算等待真实timer抢占。 */
_Noreturn void signal_infinite(unsigned long initial_sp);
/* /// 在新恢复PC的第一条C指令前捕获SP/TP及硬件结果。 */
void signal_edited_pc(void);
/* /// LA帧启用cause后检查原帧回写和再次递送的真实FCSR。 */
void signal_inject_handler(void);
/* /// LA内核SIGFPE入口仍使用内核提供的RA。 */
void signal_fpe_handler(void);
/* /// 在完整硬件fork检查后安装cause处理动作。 */
void signal_pending_setup(void);
/* /// 修改真实用户帧中的FCSR，并保留原槽地址供后续回写检查。 */
void signal_inject_context(unsigned char *context);
/* /// 对原帧回写和再次递送帧进行共同字节检查。 */
void signal_fpe_context(long signum, struct SignalInfo *info, unsigned char *context);
/* /// ISA入口已为内核checkpoint保留初始SP下32字节，之后交共同业务分派。 */
_Noreturn void signal_main(const unsigned long *initial_stack);
/* /// LA硬件边界完成普通帧、fork、pending cause之后进入成功终态。 */
_Noreturn void signal_frame_done(void);
#endif
