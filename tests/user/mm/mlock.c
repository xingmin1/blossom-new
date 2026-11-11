#define _GNU_SOURCE
#include <fcntl.h>
#include <sched.h>
#include <stdint.h>
#include <stdlib.h>
#include <sys/mman.h>
#include <sys/resource.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/vfs.h>
#include "kselftest.h"
#include "selftest_io.h"

#define TMPFS_MAGIC 0x01021994

/*
 * 通过 MS_INVALIDATE 的实际返回观察每个虚拟区间的锁定状态；通过文件修改和
 * fork 后的真实字节访问观察预故障及 COW。限额绕过由零软限额下的合法 mlock
 * 判定，普通限额分支只在隔离子进程中降权。所有子任务先回收，再释放其参数、
 * 栈和映射；临时文件在打开所需 fd 后立即删除名字。
 */

/// 查询同一 MEMLOCK 资源的两个字节限额，保留 getrlimit 的真实错误。
static int limit_matches(const struct rlimit *expected)
{
    struct rlimit actual;
    return getrlimit(RLIMIT_MEMLOCK, &actual) == 0 &&
           actual.rlim_cur == expected->rlim_cur && actual.rlim_max == expected->rlim_max;
}

/// 仅更改软限额，硬限额仍由当前进程真实持有的资源配置决定。
static int set_soft_limit(rlim_t bytes)
{
    struct rlimit limit;
    if (getrlimit(RLIMIT_MEMLOCK, &limit) != 0)
        return -1;
    limit.rlim_cur = bytes;
    return setrlimit(RLIMIT_MEMLOCK, &limit);
}

/// 不触发写回，直接区分已锁定 VMA 的 EBUSY 与普通映射的成功返回。
static int invalidation_is(void *address, size_t length, int expected_errno)
{
    errno = 0;
    int result = msync(address, length, MS_INVALIDATE);
    return expected_errno == 0 ? result == 0 : result == -1 && errno == expected_errno;
}

/// 在 fork 副本中检查继承值、硬限额降低及实际的硬限额提升权限。
static int limit_child(const struct rlimit *inherited, size_t page)
{
    int result = -1;
    ST_CHECK(limit_matches(inherited));
    struct rlimit lower = { .rlim_cur = page, .rlim_max = 2 * page };
    ST_CHECK(setrlimit(RLIMIT_MEMLOCK, &lower) == 0 && limit_matches(&lower));
    struct rlimit raised = { .rlim_cur = page, .rlim_max = 3 * page };
    errno = 0;
    int changed = setrlimit(RLIMIT_MEMLOCK, &raised);
    if (changed == -1) {
        ST_CHECK(errno == EPERM && limit_matches(&lower));
        ksft_print_msg("MEMLOCK hard-limit raise denied with EPERM\n");
    } else {
        ST_CHECK(changed == 0 && limit_matches(&raised));
        ksft_print_msg("MEMLOCK hard-limit raise admitted\n");
    }
    result = 0;
out:
    return result;
}

/// 非页整数的软限额必须精确保存；子进程更改不能修改父进程的资源配置。
static int limit_contract(size_t page)
{
    int result = -1, saved = 0;
    pid_t child = -1;
    struct rlimit original;
    ST_CHECK(getrlimit(RLIMIT_MEMLOCK, &original) == 0);
    saved = 1;
    struct rlimit next = original;
    next.rlim_cur = page + 7;
    ST_CHECK(setrlimit(RLIMIT_MEMLOCK, &next) == 0 && limit_matches(&next));
    struct rlimit invalid = { .rlim_cur = 2 * page, .rlim_max = page };
    errno = 0;
    ST_CHECK(setrlimit(RLIMIT_MEMLOCK, &invalid) == -1 && errno == EINVAL);
    ST_CHECK(limit_matches(&next));
    child = fork();
    ST_CHECK(0 <= child);
    if (child == 0)
        _exit(limit_child(&next, page) == 0 ? 0 : 1);
    ST_CHECK(st_reap(&child, 0) == 0);
    ST_CHECK(limit_matches(&next));
    result = 0;
out:
    if (st_stop_child(&child) != 0)
        result = -1;
    if (saved && (setrlimit(RLIMIT_MEMLOCK, &original) != 0 || !limit_matches(&original)))
        result = -1;
    return result;
}

/// 零软限额下实际探测锁页特权，并在已有锁的情况下检查无权限要求的 munlock。
static int lock_permissions(size_t page)
{
    int result = -1, saved = 0;
    unsigned char *mapping = MAP_FAILED;
    struct rlimit original;
    ST_CHECK(getrlimit(RLIMIT_MEMLOCK, &original) == 0);
    saved = 1;
    mapping = mmap(NULL, page, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ST_CHECK(mapping != MAP_FAILED);
    ST_CHECK(set_soft_limit(0) == 0);
    errno = 0;
    int admitted = mlock(mapping, page);
    int bypass = admitted == 0;
    if (bypass) {
        ST_CHECK(invalidation_is(mapping, page, EBUSY));
        ST_CHECK(munlock(mapping, page) == 0);
    } else {
        ST_CHECK(admitted == -1 && errno == EPERM);
        ST_CHECK(invalidation_is(mapping, page, 0));
    }
    ksft_print_msg("MEMLOCK zero-limit admission=%s\n", bypass ? "privileged bypass" : "EPERM");
    ST_CHECK(set_soft_limit(page) == 0);
    ST_CHECK(mlock(mapping, page) == 0 && invalidation_is(mapping, page, EBUSY));
    ST_CHECK(set_soft_limit(0) == 0);
    ST_CHECK(munlock(mapping + 1, 0) == 0 && invalidation_is(mapping, page, 0));
    errno = 0;
    admitted = mlock(mapping, 0);
    if (bypass)
        ST_CHECK(admitted == 0);
    else
        ST_CHECK(admitted == -1 && errno == EPERM);
    ST_CHECK(munlock(mapping, 0) == 0);
    result = 0;
out:
    if (mapping != MAP_FAILED && munmap(mapping, page) != 0)
        result = -1;
    if (saved && (setrlimit(RLIMIT_MEMLOCK, &original) != 0 || !limit_matches(&original)))
        result = -1;
    return result;
}

/// 起点向下取整；零长度先加页内偏移，因此未对齐的零长度仍锁定一页。
static int unaligned_ranges(size_t page)
{
    int result = -1;
    unsigned char *mapping = mmap(NULL, 3 * page, PROT_READ | PROT_WRITE,
                                  MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ST_CHECK(mapping != MAP_FAILED);
    ST_CHECK(mlock(mapping, 0) == 0 && invalidation_is(mapping, page, 0));
    ST_CHECK(mlock(mapping + 1, 0) == 0);
    ST_CHECK(invalidation_is(mapping, page, EBUSY));
    ST_CHECK(invalidation_is(mapping + page, page, 0));
    ST_CHECK(munlock(mapping + 1, 0) == 0 && invalidation_is(mapping, page, 0));
    ST_CHECK(mlock(mapping + page - 1, 2) == 0);
    ST_CHECK(invalidation_is(mapping, page, EBUSY));
    ST_CHECK(invalidation_is(mapping + page, page, EBUSY));
    ST_CHECK(invalidation_is(mapping + 2 * page, page, 0));
    ST_CHECK(munlock(mapping + page, 0) == 0);
    ST_CHECK(invalidation_is(mapping + page, page, EBUSY));
    ST_CHECK(munlock(mapping + page - 1, 1) == 0);
    ST_CHECK(invalidation_is(mapping, page, 0));
    ST_CHECK(invalidation_is(mapping + page, page, EBUSY));
    ST_CHECK(munlock(mapping + page + 1, 0) == 0);
    ST_CHECK(invalidation_is(mapping + page, page, 0));
    result = 0;
out:
    if (mapping != MAP_FAILED && munmap(mapping, 3 * page) != 0)
        result = -1;
    return result;
}

/// 长度归整与地址末端回绕分别验证；洞和预故障失败保留此前已提交的 VMA 状态。
static int range_failures(size_t page)
{
    int result = -1;
    unsigned char *mapping = mmap(NULL, 5 * page, PROT_READ | PROT_WRITE,
                                  MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ST_CHECK(mapping != MAP_FAILED);
    /* 原始 ABI 避免 libc 把回绕长度诊断为超大对象访问；这两次归整后的长度均为零。 */
    ST_CHECK(syscall(SYS_mlock, mapping + 1, SIZE_MAX) == 0);
    ST_CHECK(syscall(SYS_munlock, mapping, SIZE_MAX) == 0);
    ST_CHECK(invalidation_is(mapping, page, 0));
    void *last_page = (void *)(UINTPTR_MAX - page + 1);
    errno = 0;
    ST_CHECK(mlock(last_page, 2 * page) == -1 && errno == EINVAL);
    errno = 0;
    ST_CHECK(munlock(last_page, 2 * page) == -1 && errno == EINVAL);
    ST_CHECK(munmap(mapping + page, page) == 0);
    ST_CHECK(munmap(mapping + 4 * page, page) == 0);
    ST_CHECK(mlock(mapping + page, 0) == 0);
    ST_CHECK(munlock(mapping + page, 0) == 0);
    errno = 0;
    ST_CHECK(mlock(mapping + page, page) == -1 && errno == ENOMEM);
    errno = 0;
    ST_CHECK(munlock(mapping + page, page) == -1 && errno == ENOMEM);
    errno = 0;
    ST_CHECK(mlock(mapping, 3 * page) == -1 && errno == ENOMEM);
    ST_CHECK(invalidation_is(mapping, page, EBUSY));
    ST_CHECK(invalidation_is(mapping + 2 * page, page, 0));
    ST_CHECK(mlock(mapping + 2 * page, page) == 0);
    errno = 0;
    ST_CHECK(munlock(mapping, 3 * page) == -1 && errno == ENOMEM);
    ST_CHECK(invalidation_is(mapping, page, 0));
    ST_CHECK(invalidation_is(mapping + 2 * page, page, EBUSY));
    ST_CHECK(munlock(mapping + 2 * page, page) == 0);
    errno = 0;
    ST_CHECK(mlock(mapping + 3 * page, 2 * page) == -1 && errno == ENOMEM);
    ST_CHECK(invalidation_is(mapping + 3 * page, page, EBUSY));
    ST_CHECK(munlock(mapping + 3 * page, page) == 0);
    ST_CHECK(mprotect(mapping + 2 * page, page, PROT_NONE) == 0);
    errno = 0;
    ST_CHECK(mlock(mapping + 2 * page, page) == -1 && errno == ENOMEM);
    ST_CHECK(invalidation_is(mapping + 2 * page, page, EBUSY));
    ST_CHECK(mprotect(mapping + 2 * page, page, PROT_READ | PROT_WRITE) == 0);
    ST_CHECK(invalidation_is(mapping + 2 * page, page, EBUSY));
    result = 0;
out:
    if (mapping != MAP_FAILED && munmap(mapping, 5 * page) != 0)
        result = -1;
    return result;
}

/// 重叠锁不叠加，mprotect 保留锁；固定覆盖和重新映射只解除被替换页的锁。
static int overlap_and_protection(size_t page)
{
    int result = -1;
    unsigned char *mapping = mmap(NULL, 4 * page, PROT_READ | PROT_WRITE,
                                  MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ST_CHECK(mapping != MAP_FAILED);
    ST_CHECK(mlock(mapping, 2 * page) == 0);
    ST_CHECK(mlock(mapping, 2 * page) == 0);
    ST_CHECK(mlock(mapping + page, 2 * page) == 0);
    ST_CHECK(invalidation_is(mapping, 3 * page, EBUSY));
    ST_CHECK(invalidation_is(mapping + 3 * page, page, 0));
    ST_CHECK(mprotect(mapping + page, page, PROT_READ) == 0);
    ST_CHECK(invalidation_is(mapping + page, page, EBUSY));
    ST_CHECK(mprotect(mapping + page, page, PROT_NONE) == 0);
    ST_CHECK(invalidation_is(mapping + page, page, EBUSY));
    ST_CHECK(mprotect(mapping + page, page, PROT_READ | PROT_WRITE) == 0);
    ST_CHECK(munlock(mapping + page, page) == 0);
    ST_CHECK(munlock(mapping + page, page) == 0);
    ST_CHECK(invalidation_is(mapping, page, EBUSY));
    ST_CHECK(invalidation_is(mapping + page, page, 0));
    ST_CHECK(invalidation_is(mapping + 2 * page, page, EBUSY));
    ST_CHECK(mlock(mapping + page, page) == 0);
    mapping[page] = 0xa1;
    ST_CHECK(mmap(mapping + page, page, PROT_READ | PROT_WRITE,
                  MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0) == mapping + page);
    ST_CHECK(*(volatile unsigned char *)(mapping + page) == 0);
    ST_CHECK(invalidation_is(mapping, page, EBUSY));
    ST_CHECK(invalidation_is(mapping + page, page, 0));
    ST_CHECK(invalidation_is(mapping + 2 * page, page, EBUSY));
    ST_CHECK(munmap(mapping, page) == 0);
    ST_CHECK(mmap(mapping, page, PROT_READ | PROT_WRITE,
                  MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0) == mapping);
    ST_CHECK(invalidation_is(mapping, page, 0));
    ST_CHECK(invalidation_is(mapping + 2 * page, page, EBUSY));
    ST_CHECK(munlock(mapping + 2 * page, page) == 0);
    result = 0;
out:
    if (mapping != MAP_FAILED && munmap(mapping, 4 * page) != 0)
        result = -1;
    return result;
}

/// 只有实测零限额返回 EPERM 后才断言普通额度；降权不能完成时只跳过此独立分支。
static int quota_child(size_t page)
{
    int result = KSFT_FAIL;
    unsigned char *mapping = mmap(NULL, 5 * page, PROT_READ | PROT_WRITE,
                                  MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ST_CHECK(mapping != MAP_FAILED);
    ST_CHECK(set_soft_limit(0) == 0);
    errno = 0;
    int admitted = mlock(mapping, page);
    if (admitted == 0) {
        ST_CHECK(invalidation_is(mapping, page, EBUSY));
        ST_CHECK(munlock(mapping, page) == 0);
        errno = 0;
        int dropped = setuid((uid_t)65534);
        if (dropped == -1) {
            ST_CHECK(errno == EPERM || errno == ENOSYS);
            ksft_print_msg("ordinary MEMLOCK quota requires a child able to drop lock privilege; setuid errno=%d\n", errno);
            result = KSFT_SKIP;
            goto out;
        }
        ST_CHECK(dropped == 0);
        errno = 0;
        admitted = mlock(mapping, page);
        if (admitted == 0) {
            ST_CHECK(invalidation_is(mapping, page, EBUSY));
            ST_CHECK(munlock(mapping, page) == 0);
            ksft_print_msg("ordinary MEMLOCK quota unavailable: zero-limit bypass retained after setuid\n");
            result = KSFT_SKIP;
            goto out;
        }
    }
    ST_CHECK(admitted == -1 && errno == EPERM);
    ST_CHECK(invalidation_is(mapping, page, 0));
    ST_CHECK(set_soft_limit(3 * page - 1) == 0);
    ST_CHECK(mlock(mapping, 2 * page) == 0);
    ST_CHECK(mlock(mapping, 2 * page) == 0);
    ST_CHECK(mlock(mapping + page, page) == 0);
    errno = 0;
    ST_CHECK(mlock(mapping + page, 2 * page) == -1 && errno == ENOMEM);
    ST_CHECK(invalidation_is(mapping, 2 * page, EBUSY));
    ST_CHECK(invalidation_is(mapping + 2 * page, page, 0));
    ST_CHECK(munlock(mapping + 1, 0) == 0);
    ST_CHECK(mlock(mapping + page, 2 * page) == 0);
    errno = 0;
    ST_CHECK(mlock(mapping, 1) == -1 && errno == ENOMEM);
    ST_CHECK(munlock(mapping + page, 1) == 0);
    ST_CHECK(mlock(mapping, 1) == 0);
    mapping[2 * page] = 0x93;
    ST_CHECK(mprotect(mapping + 2 * page, page, PROT_READ) == 0);
    ST_CHECK(invalidation_is(mapping + 2 * page, page, EBUSY));
    ST_CHECK(mmap(mapping + 2 * page, page, PROT_READ | PROT_WRITE,
                  MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0) == mapping + 2 * page);
    ST_CHECK(*(volatile unsigned char *)(mapping + 2 * page) == 0);
    ST_CHECK(mlock(mapping + 3 * page, page) == 0);
    ST_CHECK(munmap(mapping, page) == 0);
    ST_CHECK(mlock(mapping + 4 * page, page) == 0);
    ST_CHECK(set_soft_limit(0) == 0);
    ST_CHECK(munlock(mapping + 3 * page, 2 * page) == 0);
    ST_CHECK(invalidation_is(mapping + 3 * page, 2 * page, 0));
    result = KSFT_PASS;
out:
    if (mapping != MAP_FAILED && munmap(mapping, 5 * page) != 0)
        result = KSFT_FAIL;
    return result;
}

/// 降权与软限额缩减只发生在已 wait 的隔离子进程，不改变其余 TAP 项的身份或额度。
static int ordinary_quota(size_t page)
{
    int result = KSFT_FAIL, status;
    pid_t child = fork();
    ST_CHECK(0 <= child);
    if (child == 0)
        _exit(quota_child(page));
    pid_t waited;
    do {
        waited = waitpid(child, &status, 0);
    } while (waited == -1 && errno == EINTR);
    ST_CHECK(waited == child);
    child = -1;
    ST_CHECK(WIFEXITED(status));
    ST_CHECK(WEXITSTATUS(status) == KSFT_PASS || WEXITSTATUS(status) == KSFT_SKIP);
    result = WEXITSTATUS(status);
out:
    if (st_stop_child(&child) != 0)
        result = KSFT_FAIL;
    return result;
}

/// fork 子进程从未锁定 VMA 开始，私有写发生 COW，共享匿名写仍传播给父进程。
static int fork_child(unsigned char *private, unsigned char *shared, size_t page)
{
    int result = -1;
    ST_CHECK(invalidation_is(private, 2 * page, 0));
    ST_CHECK(invalidation_is(shared, page, 0));
    ST_CHECK(munlock(private, 2 * page) == 0);
    ST_CHECK(mlock(private, page) == 0 && invalidation_is(private, page, EBUSY));
    private[0] = 0x52;
    private[2 * page - 1] = 0x53;
    shared[0] = 0x62;
    ST_CHECK(*(volatile unsigned char *)private == 0x52);
    ST_CHECK(munlock(private, page) == 0);
    result = 0;
out:
    if (munmap(private, 2 * page) != 0)
        result = -1;
    if (munmap(shared, page) != 0)
        result = -1;
    return result;
}

/// 子进程锁页和退出均不得改变父进程的锁；同时读取真实私有及共享页的结果。
static int fork_locks(size_t page)
{
    int result = -1;
    pid_t child = -1;
    unsigned char *private = MAP_FAILED, *shared = MAP_FAILED;
    private = mmap(NULL, 2 * page, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ST_CHECK(private != MAP_FAILED);
    shared = mmap(NULL, page, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    ST_CHECK(shared != MAP_FAILED);
    private[0] = 0x31;
    private[2 * page - 1] = 0x32;
    shared[0] = 0x44;
    ST_CHECK(mlock(private, 2 * page) == 0 && mlock(shared, page) == 0);
    child = fork();
    ST_CHECK(0 <= child);
    if (child == 0)
        _exit(fork_child(private, shared, page) == 0 ? 0 : 1);
    ST_CHECK(st_reap(&child, 0) == 0);
    ST_CHECK(*(volatile unsigned char *)private == 0x31);
    ST_CHECK(*(volatile unsigned char *)(private + 2 * page - 1) == 0x32);
    ST_CHECK(*(volatile unsigned char *)shared == 0x62);
    ST_CHECK(invalidation_is(private, page, EBUSY));
    ST_CHECK(invalidation_is(private + page, page, EBUSY));
    ST_CHECK(invalidation_is(shared, page, EBUSY));
    result = 0;
out:
    if (st_stop_child(&child) != 0)
        result = -1;
    if (private != MAP_FAILED && munmap(private, 2 * page) != 0)
        result = -1;
    if (shared != MAP_FAILED && munmap(shared, page) != 0)
        result = -1;
    return result;
}

/// 参数保持到 clone 子任务退出，父子 fd 表独立，两个 pipe 只承担有限阶段同步。
struct vm_lock {
    /// 同一 Mm 中的两页匿名映射，由父进程最终解除。
    unsigned char *mapping;
    /// 单页字节数。
    size_t page;
    /// 子任务向父进程报告锁定变更的 pipe 两端。
    int ready[2];
    /// 父进程允许子任务观察后续锁定状态的 pipe 两端。
    int gate[2];
};

/// 子任务更改同一个 Mm 的锁；退出时将仍有效的锁交给存活父进程清理。
static int shared_vm_child(void *argument)
{
    const struct vm_lock *state = argument;
    int result = -1;
    int ready[2] = { state->ready[0], state->ready[1] };
    int gate[2] = { state->gate[0], state->gate[1] };
    unsigned char byte;
    ST_CHECK(st_close(&ready[0]) == 0 && st_close(&gate[1]) == 0);
    ST_CHECK(invalidation_is(state->mapping, state->page, EBUSY));
    ST_CHECK(munlock(state->mapping, state->page) == 0);
    ST_CHECK(mlock(state->mapping + state->page, state->page) == 0);
    state->mapping[0] = 0x71;
    ST_CHECK(st_write_all(ready[1], "L", 1) == 0);
    ST_CHECK(st_read_all(gate[0], &byte, 1) == 0 && byte == 'G');
    ST_CHECK(invalidation_is(state->mapping, state->page, EBUSY));
    ST_CHECK(invalidation_is(state->mapping + state->page, state->page, EBUSY));
    result = 0;
out:
    if (st_close(&ready[0]) != 0)
        result = -1;
    if (st_close(&ready[1]) != 0)
        result = -1;
    if (st_close(&gate[0]) != 0)
        result = -1;
    if (st_close(&gate[1]) != 0)
        result = -1;
    return result == 0 ? 0 : 1;
}

/// CLONE_VM 双向观察锁定变更，子任务退出后仍由同一存活 Mm 持有已发布的锁。
static int shared_vm(size_t page)
{
    int result = -1, ready[2] = { -1, -1 }, gate[2] = { -1, -1 };
    pid_t child = -1;
    unsigned char *mapping = MAP_FAILED, *stack = MAP_FAILED;
    size_t stack_size = 16 * page;
    unsigned char byte;
    mapping = mmap(NULL, 2 * page, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ST_CHECK(mapping != MAP_FAILED);
    stack = mmap(NULL, stack_size, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ST_CHECK(stack != MAP_FAILED);
    ST_CHECK(pipe(ready) == 0 && pipe(gate) == 0);
    ST_CHECK(mlock(mapping, page) == 0);
    struct vm_lock state = {
        .mapping = mapping, .page = page,
        .ready = { ready[0], ready[1] }, .gate = { gate[0], gate[1] },
    };
    child = clone(shared_vm_child, stack + stack_size, CLONE_VM | SIGCHLD, &state);
    ST_CHECK(0 <= child);
    ST_CHECK(st_close(&ready[1]) == 0 && st_close(&gate[0]) == 0);
    ST_CHECK(st_read_all(ready[0], &byte, 1) == 0 && byte == 'L');
    ST_CHECK(*(volatile unsigned char *)mapping == 0x71);
    ST_CHECK(invalidation_is(mapping, page, 0));
    ST_CHECK(invalidation_is(mapping + page, page, EBUSY));
    ST_CHECK(mlock(mapping, page) == 0);
    ST_CHECK(st_write_all(gate[1], "G", 1) == 0);
    ST_CHECK(st_reap(&child, 0) == 0);
    ST_CHECK(invalidation_is(mapping, page, EBUSY));
    ST_CHECK(invalidation_is(mapping + page, page, EBUSY));
    result = 0;
out:
    if (st_stop_child(&child) != 0)
        result = -1;
    if (stack != MAP_FAILED && munmap(stack, stack_size) != 0)
        result = -1;
    if (mapping != MAP_FAILED && munmap(mapping, 2 * page) != 0)
        result = -1;
    if (st_close(&ready[0]) != 0)
        result = -1;
    if (st_close(&ready[1]) != 0)
        result = -1;
    if (st_close(&gate[0]) != 0)
        result = -1;
    if (st_close(&gate[1]) != 0)
        result = -1;
    return result;
}

/// 映射在锁定前不读取；随后修改源文件可检验 mlock 和写权限提升已经建立私有页。
static int private_file_cow(size_t page)
{
    char path[] = "mlock-private-XXXXXX";
    int result = -1, fd = -1, readonly = -1, created = 0;
    unsigned char *writable = MAP_FAILED, *upgraded = MAP_FAILED;
    unsigned char byte;
    fd = mkstemp(path);
    ST_CHECK(0 <= fd);
    created = 1;
    readonly = open(path, O_RDONLY | O_CLOEXEC);
    ST_CHECK(0 <= readonly);
    ST_CHECK(unlink(path) == 0);
    created = 0;
    ST_CHECK(ftruncate(fd, 2 * (off_t)page) == 0);
    ST_CHECK(pwrite(fd, "\x31", 1, 17) == 1);
    ST_CHECK(pwrite(fd, "\x41", 1, (off_t)page + 17) == 1);
    writable = mmap(NULL, page, PROT_READ | PROT_WRITE, MAP_PRIVATE, readonly, 0);
    ST_CHECK(writable != MAP_FAILED);
    upgraded = mmap(NULL, page, PROT_READ, MAP_PRIVATE, readonly, (off_t)page);
    ST_CHECK(upgraded != MAP_FAILED);
    ST_CHECK(mlock(writable, page) == 0 && mlock(upgraded, page) == 0);
    ST_CHECK(mprotect(upgraded, page, PROT_READ | PROT_WRITE) == 0);
    ST_CHECK(invalidation_is(writable, page, EBUSY));
    ST_CHECK(invalidation_is(upgraded, page, EBUSY));
    ST_CHECK(pwrite(fd, "\x72", 1, 17) == 1);
    ST_CHECK(pwrite(fd, "\x82", 1, (off_t)page + 17) == 1);
    ST_CHECK(*(volatile unsigned char *)(writable + 17) == 0x31);
    ST_CHECK(*(volatile unsigned char *)(upgraded + 17) == 0x41);
    writable[page - 1] = 0xa5;
    upgraded[page - 1] = 0xb6;
    ST_CHECK(msync(writable, page, MS_SYNC) == 0 && msync(upgraded, page, MS_SYNC) == 0);
    ST_CHECK(pread(fd, &byte, 1, (off_t)page - 1) == 1 && byte == 0);
    ST_CHECK(pread(fd, &byte, 1, 2 * (off_t)page - 1) == 1 && byte == 0);
    ST_CHECK(pread(fd, &byte, 1, 17) == 1 && byte == 0x72);
    ST_CHECK(pread(fd, &byte, 1, (off_t)page + 17) == 1 && byte == 0x82);
    result = 0;
out:
    if (writable != MAP_FAILED && munmap(writable, page) != 0)
        result = -1;
    if (upgraded != MAP_FAILED && munmap(upgraded, page) != 0)
        result = -1;
    if (st_close(&readonly) != 0)
        result = -1;
    if (st_close(&fd) != 0)
        result = -1;
    if (created && unlink(path) != 0)
        result = -1;
    return result;
}

/// 只读 fd 的共享映射也能预故障；锁定别名分别计状态，共享字节仍随文件和 store 更新。
static int shared_file_pages(size_t page)
{
    char path[] = "mlock-shared-XXXXXX";
    int result = -1, fd = -1, readonly = -1, created = 0;
    unsigned char *shared = MAP_FAILED, *alias = MAP_FAILED;
    unsigned char byte;
    fd = mkstemp(path);
    ST_CHECK(0 <= fd);
    created = 1;
    readonly = open(path, O_RDONLY | O_CLOEXEC);
    ST_CHECK(0 <= readonly);
    ST_CHECK(unlink(path) == 0);
    created = 0;
    ST_CHECK(ftruncate(fd, (off_t)page) == 0);
    ST_CHECK(pwrite(fd, "\x35", 1, 17) == 1);
    shared = mmap(NULL, page, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    ST_CHECK(shared != MAP_FAILED);
    alias = mmap(NULL, page, PROT_READ, MAP_SHARED, readonly, 0);
    ST_CHECK(alias != MAP_FAILED);
    ST_CHECK(mlock(shared, page) == 0 && mlock(alias, page) == 0);
    ST_CHECK(pwrite(fd, "\x79", 1, 17) == 1);
    ST_CHECK(*(volatile unsigned char *)(shared + 17) == 0x79);
    ST_CHECK(*(volatile unsigned char *)(alias + 17) == 0x79);
    shared[page - 1] = 0xab;
    ST_CHECK(*(volatile unsigned char *)(alias + page - 1) == 0xab);
    ST_CHECK(msync(shared, page, MS_SYNC) == 0);
    ST_CHECK(pread(readonly, &byte, 1, (off_t)page - 1) == 1 && byte == 0xab);
    ST_CHECK(invalidation_is(shared, page, EBUSY));
    ST_CHECK(invalidation_is(alias, page, EBUSY));
    ST_CHECK(munlock(alias, page) == 0 && invalidation_is(alias, page, 0));
    ST_CHECK(invalidation_is(shared, page, EBUSY));
    ST_CHECK(st_close(&readonly) == 0 && st_close(&fd) == 0);
    ST_CHECK(*(volatile unsigned char *)(alias + page - 1) == 0xab);
    ST_CHECK(munlock(shared, page) == 0 && invalidation_is(shared, page, 0));
    result = 0;
out:
    if (shared != MAP_FAILED && munmap(shared, page) != 0)
        result = -1;
    if (alias != MAP_FAILED && munmap(alias, page) != 0)
        result = -1;
    if (st_close(&readonly) != 0)
        result = -1;
    if (st_close(&fd) != 0)
        result = -1;
    if (created && unlink(path) != 0)
        result = -1;
    return result;
}

/// tmpfs 截断必须撤销锁页的旧 PTE；零长度时子进程访问得到 SIGBUS，重新扩展后页内容全零。
static int tmpfs_truncate(size_t page)
{
    char path[] = "/dev/shm/mlock-XXXXXX";
    int result = -1, fd = -1, created = 0, status;
    pid_t child = -1;
    struct statfs filesystem;
    unsigned char *shared = MAP_FAILED, *alias = MAP_FAILED;
    fd = mkstemp(path);
    ST_CHECK(0 <= fd);
    created = 1;
    ST_CHECK(fstatfs(fd, &filesystem) == 0 && (unsigned long)filesystem.f_type == TMPFS_MAGIC);
    ST_CHECK(unlink(path) == 0);
    created = 0;
    ST_CHECK(ftruncate(fd, 2 * (off_t)page) == 0);
    shared = mmap(NULL, 2 * page, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    ST_CHECK(shared != MAP_FAILED);
    alias = mmap(NULL, 2 * page, PROT_READ, MAP_SHARED, fd, 0);
    ST_CHECK(alias != MAP_FAILED);
    ST_CHECK(mlock(shared, 2 * page) == 0 && mlock(alias, 2 * page) == 0);
    shared[page - 1] = 0x51;
    shared[2 * page - 1] = 0x61;
    ST_CHECK(*(volatile unsigned char *)(alias + page - 1) == 0x51);
    ST_CHECK(*(volatile unsigned char *)(alias + 2 * page - 1) == 0x61);
    ST_CHECK(ftruncate(fd, 0) == 0);
    ST_CHECK(invalidation_is(shared, 2 * page, EBUSY));
    ST_CHECK(invalidation_is(alias, 2 * page, EBUSY));
    child = fork();
    ST_CHECK(0 <= child);
    if (child == 0) {
        struct rlimit core = { .rlim_cur = 0, .rlim_max = 0 };
        /* 可选的 core 限额未实现时仍实际访问已截断页，由父进程核对 SIGBUS。 */
        errno = 0;
        if (setrlimit(RLIMIT_CORE, &core) != 0 && errno != ENOSYS)
            _exit(1);
        unsigned char byte = *(volatile unsigned char *)shared;
        (void)byte;
        _exit(2);
    }
    pid_t waited;
    do {
        waited = waitpid(child, &status, 0);
    } while (waited == -1 && errno == EINTR);
    ST_CHECK(waited == child);
    child = -1;
    ST_CHECK(WIFSIGNALED(status) && WTERMSIG(status) == SIGBUS);
    ST_CHECK(ftruncate(fd, 2 * (off_t)page) == 0);
    for (size_t index = 0; index < 2 * page; ++index) {
        ST_CHECK(*(volatile unsigned char *)(shared + index) == 0);
        ST_CHECK(*(volatile unsigned char *)(alias + index) == 0);
    }
    shared[page + 17] = 0x87;
    ST_CHECK(*(volatile unsigned char *)(alias + page + 17) == 0x87);
    unsigned char byte;
    ST_CHECK(pread(fd, &byte, 1, (off_t)page + 17) == 1 && byte == 0x87);
    ST_CHECK(munlock(shared, 2 * page) == 0 && invalidation_is(shared, 2 * page, 0));
    ST_CHECK(invalidation_is(alias, 2 * page, EBUSY));
    ST_CHECK(munlock(alias, 2 * page) == 0 && invalidation_is(alias, 2 * page, 0));
    result = 0;
out:
    if (st_stop_child(&child) != 0)
        result = -1;
    if (shared != MAP_FAILED && munmap(shared, 2 * page) != 0)
        result = -1;
    if (alias != MAP_FAILED && munmap(alias, 2 * page) != 0)
        result = -1;
    if (st_close(&fd) != 0)
        result = -1;
    if (created && unlink(path) != 0)
        result = -1;
    return result;
}

/// 单一 TAP 程序使用可恢复的六页软限额，执行后恢复调用者的完整 MEMLOCK 配置。
int main(void)
{
    ksft_print_header();
    long page_size = sysconf(_SC_PAGESIZE);
    if (page_size <= 0)
        ksft_exit_fail_msg("page size errno=%d\n", errno);
    size_t page = (size_t)page_size;
    struct rlimit original;
    if (getrlimit(RLIMIT_MEMLOCK, &original) != 0)
        ksft_exit_fail_msg("getrlimit MEMLOCK errno=%d\n", errno);
    struct rlimit available = original;
    available.rlim_cur = 6 * page;
    if (available.rlim_max < available.rlim_cur)
        available.rlim_max = available.rlim_cur;
    errno = 0;
    if (setrlimit(RLIMIT_MEMLOCK, &available) != 0) {
        if (errno == EPERM && original.rlim_max < available.rlim_max)
            ksft_exit_skip("requires a six-page MEMLOCK budget or permission to raise the hard limit\n");
        ksft_exit_fail_msg("setrlimit MEMLOCK budget errno=%d\n", errno);
    }
    ksft_set_plan(11);
    ksft_test_result(limit_contract(page) == 0, "MEMLOCK exact limits fork inheritance and hard-limit admission\n");
    ksft_test_result(lock_permissions(page) == 0, "zero-limit admission and permission-free munlock\n");
    ksft_test_result(unaligned_ranges(page) == 0, "unaligned start and zero-length page rounding\n");
    ksft_test_result(range_failures(page) == 0, "overflow holes and committed locks after population failure\n");
    ksft_test_result(overlap_and_protection(page) == 0, "non-stacking overlaps mprotect fixed replacement and unmap\n");
    int quota = ordinary_quota(page);
    if (quota == KSFT_SKIP)
        ksft_test_result_skip("ordinary quota needs a child without zero-limit lock privilege\n");
    else
        ksft_test_result(quota == KSFT_PASS, "ordinary byte quota overlap accounting and unmap replacement refunds\n");
    ksft_test_result(fork_locks(page) == 0, "fork clears child locks preserves parent locks and private COW\n");
    ksft_test_result(shared_vm(page) == 0, "CLONE_VM shares lock changes and preserves surviving Mm locks\n");
    ksft_test_result(private_file_cow(page) == 0, "private file mlock and writable mprotect prefault COW\n");
    ksft_test_result(shared_file_pages(page) == 0, "read-only shared file locks aliases data and descriptor lifetime\n");
    ksft_test_result(tmpfs_truncate(page) == 0, "tmpfs locked aliases truncate SIGBUS and zeroed regrowth\n");
    if (setrlimit(RLIMIT_MEMLOCK, &original) != 0 || !limit_matches(&original))
        ksft_exit_fail_msg("restore MEMLOCK limits errno=%d\n", errno);
    ksft_finished();
}
