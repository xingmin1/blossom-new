#ifndef LIBC_SMOKE_COMMON_H
#define LIBC_SMOKE_COMMON_H

#define _GNU_SOURCE
#define _POSIX_C_SOURCE 200809L

#include <stdint.h>
#include <errno.h>
#include <fcntl.h>
#include <sched.h>
#include <netinet/in.h>
#include <poll.h>
#include <signal.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/auxv.h>
#include <sys/mount.h>
#include <sys/resource.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <termios.h>
#include <time.h>
#include <unistd.h>

extern char **environ;

// 所有场景引用同一份 CRT 初始化的 TLS；定义仅位于 libc_common.c。
extern _Thread_local volatile unsigned tls_image;
extern _Thread_local volatile unsigned tls_zero;

enum { EXEC_CHILD_STATUS = 37 };

enum {
    REBOOT_NUMBER = 142,
    REBOOT_MAGIC2 = 672274793,
    REBOOT_POWER_OFF = 0x4321fedc,
};

/// 输出实际失败原因；成功消息只由已消费子进程状态的父进程产生。
int fail(const char *operation);

/// 经可观察的内存访问检查小块分配，释放后不保留指针。
int check_malloc(void);

/// 修改模板与零初始化 TLS，在实际调度系统调用返回后逐次核对。
int check_tls(void);

/// 在迁核 pipe 窗口只核 TLS/errno，不以重复 yield 代替真实阻塞。
int check_tls_state(void);

/// 触碰大栈的探测保持独立栈帧，未调用的受限启动模式不预留这128KiB。
__attribute__((noinline)) int check_stack(void);

/// 按 Linux dev_t 编码取得原 fstat 的 major，独立对照 statx 的显式设备字段。
uint32_t device_major(uint64_t device);

/// 按 Linux dev_t 编码取得原 fstat 的 minor。
uint32_t device_minor(uint64_t device);

/// 阻塞 pipe 的有限消息保留短读写和 EINTR 语义，EOF 不能当作成功握手。
int transfer_pipe(int fd, void *bytes, size_t length, int writing);

/// 精确消费子 PID 与完整 wait 编码，不用截出的 exit byte 掩盖信号或停止事件。
int wait_status(pid_t child, int expected);

/// 比较标准 timespec 的有符号秒与纳秒，负秒不按无符号时间解释。
int same_file_time(struct timespec left, struct timespec right);

/// 时钟前后夹住 NOW 的实际值，不能仅凭时间非零判断提交成功。
int file_time_before(struct timespec left, struct timespec right);

/// 时间修改保留文件身份、权限、内容长度和块计数，不比较 stat 的填充字节。
int same_file_attributes(const struct stat *left, const struct stat *right);

/// 把实际时钟样本转换为纳秒，拒绝无效字段和不能表示的时间。
int runtime_nanoseconds(clockid_t clock, uint64_t *value);

/// 期限检查容许调度迟到，只排除提前完成和数量级错误的等待。
int runtime_elapsed(uint64_t start, uint64_t minimum, const char *operation);

/// 迁核模式通过两个普通 pipe 逐次握手，不用额外 syscall 或共享测试页。
int migration_handshake(const char *ready_text, const char *go_text);

/// fork、readv/writev、磁盘 exec、wait 与父子 TLS 隔离共同走真实生命周期。
int run_body(const char *executable);

/// 以真实 AT_EXECFN 和环境核对脚本每层形成的拥有式 argv。
int check_script_argv(int argc, char **argv);

/// 真实消费解释器深度、缺失、权限、格式和参数预算错误。
int script_errors(void);

/// 核对 root 与非 root 两种真实凭据，并用 fchdir 恢复实际 cwd。
int abi_check(unsigned uid, unsigned gid);

/// 普通 fork 消费独立 FS 视图，拒绝换根不提交，父视图和创建掩码保持原值。
int runtime_view(void);

/// 数值身份与角色分别核验；系统根在 KILL、STOP、默认 TERM 后继续普通用户指令。
int root_role_body(int system_root);

/// 新 exec 栈的最低页能交付 pipe，guard 页返回 EFAULT，不能用限额 getter 代替范围验证。
int limited_stack(void);

/// 正反限额请求均由实际用户 ABI 消费，父在完整 wait 后保持原组限额。
int runtime_limits(const char *program);

/// 独立 RW 目录保存普通测试文件，成功与失败均撤销本次创建的名字和 fd。
int runtime_file_times(const char *program);

/// 正式 socket 消费者串行检查控制 ABI、共享属性、协议等待和不可重启中断。
int runtime_socket_timeouts(void);

/// 六个标准 clock 与 libc 日历入口共用实际 UTC 样本，不校准 RTC 的亚秒相位。
int runtime_clock(void);

#endif
