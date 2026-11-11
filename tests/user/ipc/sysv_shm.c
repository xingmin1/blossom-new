#define _GNU_SOURCE
#include <sched.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/shm.h>
#include <sys/syscall.h>
#include "kselftest.h"
#include "selftest_io.h"

/// 清理本函数拥有的连接，失败仍撤销本地句柄，避免重复操作同一地址。
static int detach(void **address)
{
    if (*address == (void *)-1)
        return 0;
    void *owned = *address;
    *address = (void *)-1;
    return shmdt(owned);
}

/// 清理本次创建的段；已随末连接销毁的 ID 不再承担删除义务。
static int remove_segment(int *id)
{
    if (*id < 0)
        return 0;
    int owned = *id;
    *id = -1;
    int result = shmctl(owned, IPC_RMID, NULL);
    return result == 0 || (result == -1 && errno == EINVAL) ? 0 : -1;
}

/// 已退休的 ID 必须拒绝新连接；意外成功时先清理该连接再报告失败。
static int id_absent(int id)
{
    errno = 0;
    void *address = shmat(id, NULL, 0);
    if (address != (void *)-1) {
        shmdt(address);
        return 0;
    }
    return errno == EINVAL;
}

/// 命名查找保原始大小、CREAT/EXCL 规则，失败连接不得阻止未连接段的立即删除。
static int keys_and_rollback(size_t page)
{
    int result = -1, id = -1, replacement = -1;
    void *mapping = MAP_FAILED, *first = (void *)-1, *late = (void *)-1;
    key_t key = (key_t)(0x53000000U | ((unsigned)getpid() & 0xffffffU));
    errno = 0;
    ST_CHECK(shmget(key, 0, 0) == -1 && errno == ENOENT);
    id = shmget(key, page + 7, IPC_CREAT | IPC_EXCL | 0600);
    ST_CHECK(0 <= id);
    ST_CHECK(shmget(key, 0, 0600) == id);
    ST_CHECK(shmget(key, page + 7, 0600) == id);
    errno = 0;
    ST_CHECK(shmget(key, page + 8, 0600) == -1 && errno == EINVAL);
    errno = 0;
    ST_CHECK(shmget(key, 1, IPC_CREAT | IPC_EXCL | 0600) == -1 && errno == EEXIST);
    mapping = mmap(NULL, 2 * page, PROT_READ | PROT_WRITE,
                   MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ST_CHECK(mapping != MAP_FAILED);
    errno = 0;
    ST_CHECK(shmat(id, mapping, 0) == (void *)-1 && errno == EINVAL);
    ST_CHECK(shmctl(id, IPC_RMID, NULL) == 0);
    ST_CHECK(id_absent(id));
    id = -1;

    id = shmget(key, page, IPC_CREAT | IPC_EXCL | 0600);
    ST_CHECK(0 <= id);
    first = shmat(id, NULL, 0);
    ST_CHECK(first != (void *)-1);
    *(volatile unsigned *)first = 0x13579bdf;
    ST_CHECK(shmctl(id, IPC_RMID, NULL) == 0);
    errno = 0;
    ST_CHECK(shmget(key, 0, 0) == -1 && errno == ENOENT);
    replacement = shmget(key, page, IPC_CREAT | IPC_EXCL | 0600);
    ST_CHECK(0 <= replacement && replacement != id);
    late = shmat(id, NULL, 0);
    ST_CHECK(late != (void *)-1);
    ST_CHECK(*(volatile unsigned *)late == 0x13579bdf);
    ST_CHECK(detach(&late) == 0);
    ST_CHECK(detach(&first) == 0);
    ST_CHECK(id_absent(id));
    id = -1;
    ST_CHECK(remove_segment(&replacement) == 0);
    result = 0;
out:
    if (detach(&late) != 0)
        result = -1;
    if (detach(&first) != 0)
        result = -1;
    if (remove_segment(&id) != 0)
        result = -1;
    if (remove_segment(&replacement) != 0)
        result = -1;
    if (mapping != MAP_FAILED && munmap(mapping, 2 * page) != 0)
        result = -1;
    return result;
}

/// 两次连接共享页，立即 RMID 后仍可连接；四个 fork 子进程修改必须在父进程可见。
static int shared_fork(size_t page)
{
    int result = -1, id = -1;
    void *first = (void *)-1, *second = (void *)-1, *late = (void *)-1;
    pid_t children[4] = { -1, -1, -1, -1 };
    id = shmget(IPC_PRIVATE, page + 7, IPC_CREAT | 0600);
    ST_CHECK(0 <= id);
    first = shmat(id, NULL, 0);
    ST_CHECK(first != (void *)-1);
    second = shmat(id, NULL, SHM_RDONLY);
    ST_CHECK(second != (void *)-1);
    volatile unsigned *writer = first, *reader = second;
    ST_CHECK(reader[0] == 0 && reader[4] == 0);
    writer[0] = 0x2468ace0;
    ST_CHECK(reader[0] == 0x2468ace0);
    errno = 0;
    ST_CHECK(mprotect(second, page, PROT_READ | PROT_WRITE) == -1 && errno == EACCES);
    ST_CHECK(shmctl(id, IPC_RMID, NULL) == 0);
    late = shmat(id, NULL, 0);
    ST_CHECK(late != (void *)-1 && *(volatile unsigned *)late == 0x2468ace0);
    ST_CHECK(detach(&late) == 0);
    for (unsigned index = 0; index < 4; ++index) {
        children[index] = fork();
        ST_CHECK(0 <= children[index]);
        if (children[index] == 0) {
            writer[index + 1] = 0x55000000U + index;
            _exit(reader[index + 1] == 0x55000000U + index ? 0 : 1);
        }
    }
    for (unsigned index = 0; index < 4; ++index)
        ST_CHECK(st_reap(&children[index], 0) == 0);
    for (unsigned index = 0; index < 4; ++index)
        ST_CHECK(reader[index + 1] == 0x55000000U + index);
    ST_CHECK(detach(&first) == 0);
    ST_CHECK(reader[0] == 0x2468ace0);
    ST_CHECK(detach(&second) == 0);
    ST_CHECK(id_absent(id));
    id = -1;
    result = 0;
out:
    for (unsigned index = 0; index < 4; ++index)
        if (st_stop_child(&children[index]) != 0)
            result = -1;
    if (detach(&late) != 0)
        result = -1;
    if (detach(&first) != 0)
        result = -1;
    if (detach(&second) != 0)
        result = -1;
    if (remove_segment(&id) != 0)
        result = -1;
    return result;
}

/// mprotect 和部分 munmap 后按原基址解除残片，保留洞中新映射、两侧邻居和另一次连接。
static int fragmented_detach(size_t page)
{
    int result = -1, id = -1;
    void *region = MAP_FAILED, *first = (void *)-1, *second = (void *)-1;
    region = mmap(NULL, 5 * page, PROT_READ | PROT_WRITE,
                  MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ST_CHECK(region != MAP_FAILED);
    unsigned char *base = region;
    base[0] = 0x31;
    base[4 * page] = 0x73;
    ST_CHECK(munmap(base + page, 3 * page) == 0);
    id = shmget(IPC_PRIVATE, 3 * page, 0600);
    ST_CHECK(0 <= id);
    first = shmat(id, base + page, 0);
    ST_CHECK(first == base + page);
    second = shmat(id, NULL, 0);
    ST_CHECK(second != (void *)-1);
    volatile unsigned char *alias = second;
    alias[page] = 0x5a;
    alias[2 * page] = 0xa5;
    ST_CHECK(shmctl(id, IPC_RMID, NULL) == 0);
    ST_CHECK(mprotect(base + 2 * page, page, PROT_READ) == 0);
    ST_CHECK(munmap(first, page) == 0);
    ST_CHECK(mmap(base + page, page, PROT_READ | PROT_WRITE,
                  MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0) == base + page);
    base[page] = 0x19;
    ST_CHECK(shmdt(first) == 0);
    first = (void *)-1;
    ST_CHECK(base[0] == 0x31 && base[page] == 0x19 && base[4 * page] == 0x73);
    ST_CHECK(alias[page] == 0x5a && alias[2 * page] == 0xa5);
    errno = 0;
    ST_CHECK(shmdt(base + page) == -1 && errno == EINVAL);
    ST_CHECK(munmap(second, 3 * page) == 0);
    second = (void *)-1;
    ST_CHECK(id_absent(id));
    id = -1;
    result = 0;
out:
    if (detach(&first) != 0)
        result = -1;
    if (detach(&second) != 0)
        result = -1;
    if (remove_segment(&id) != 0)
        result = -1;
    if (region != MAP_FAILED && munmap(region, 5 * page) != 0)
        result = -1;
    return result;
}

/// CLONE_VM 子任务只等待父进程的普通 pipe，不创建或复制连接。
static int shared_vm_child(void *argument)
{
    int fd = *(int *)argument;
    unsigned char byte;
    return st_read_all(fd, &byte, 1) == 0 && byte == 'G' ? 0 : 1;
}

/// 父子共享 Mm 时，父 shmdt 必须立即销毁末连接，而非等共享地址空间中的另一个任务退出。
static int shared_vm(size_t page)
{
    int result = -1, id = -1, gate[2] = { -1, -1 };
    pid_t child = -1;
    void *address = (void *)-1, *stack = MAP_FAILED;
    size_t stack_size = 16 * page;
    ST_CHECK(pipe(gate) == 0);
    stack = mmap(NULL, stack_size, PROT_READ | PROT_WRITE,
                 MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ST_CHECK(stack != MAP_FAILED);
    id = shmget(IPC_PRIVATE, page, 0600);
    ST_CHECK(0 <= id);
    address = shmat(id, NULL, 0);
    ST_CHECK(address != (void *)-1);
    ST_CHECK(shmctl(id, IPC_RMID, NULL) == 0);
    child = clone(shared_vm_child, (unsigned char *)stack + stack_size,
                  CLONE_VM | SIGCHLD, &gate[0]);
    ST_CHECK(0 <= child);
    ST_CHECK(detach(&address) == 0);
    ST_CHECK(id_absent(id));
    id = -1;
    ST_CHECK(st_write_all(gate[1], "G", 1) == 0);
    ST_CHECK(st_reap(&child, 0) == 0);
    result = 0;
out:
    if (st_stop_child(&child) != 0)
        result = -1;
    if (detach(&address) != 0)
        result = -1;
    if (remove_segment(&id) != 0)
        result = -1;
    if (stack != MAP_FAILED && munmap(stack, stack_size) != 0)
        result = -1;
    if (st_close(&gate[0]) != 0)
        result = -1;
    if (st_close(&gate[1]) != 0)
        result = -1;
    return result;
}

/// 子进程单独持有已删除段，再按请求正常退出、等待取消或替换为同一个程序的新映像。
static int retirement_child(const char *program, int mode, int output, int gate, size_t page)
{
    int id = shmget(IPC_PRIVATE, page, 0600);
    if (id < 0)
        return 1;
    void *address = shmat(id, NULL, 0);
    if (address == (void *)-1 || shmctl(id, IPC_RMID, NULL) != 0) {
        remove_segment(&id);
        return 1;
    }
    *(volatile unsigned *)address = 0xabcdef;
    unsigned char byte;
    if (st_write_all(output, &id, sizeof(id)) != 0 ||
        st_read_all(gate, &byte, 1) != 0 || byte != 'G')
        return 1;
    if (mode == 2) {
        char text[32];
        if (snprintf(text, sizeof(text), "%d", id) <= 0)
            return 1;
        execl(program, program, "--exec-check", text, (char *)NULL);
        return 1;
    }
    return 0;
}

/// 父进程不继承孩子稍后创建的段，检查最后连接在 exit、SIGKILL 和 exec 时自动退役。
static int retirement(const char *program, int mode, size_t page)
{
    int result = -1, id = -1, report[2] = { -1, -1 }, gate[2] = { -1, -1 };
    pid_t child = -1;
    void *probe = (void *)-1;
    ST_CHECK(pipe(report) == 0 && pipe(gate) == 0);
    child = fork();
    ST_CHECK(0 <= child);
    if (child == 0) {
        close(report[0]);
        close(gate[1]);
        _exit(retirement_child(program, mode, report[1], gate[0], page));
    }
    ST_CHECK(st_close(&report[1]) == 0 && st_close(&gate[0]) == 0);
    ST_CHECK(st_read_all(report[0], &id, sizeof(id)) == 0 && 0 <= id);
    probe = shmat(id, NULL, SHM_RDONLY);
    ST_CHECK(probe != (void *)-1 && *(volatile unsigned *)probe == 0xabcdef);
    ST_CHECK(detach(&probe) == 0);
    if (mode == 1) {
        ST_CHECK(st_stop_child(&child) == 0);
    } else {
        ST_CHECK(st_write_all(gate[1], "G", 1) == 0);
        ST_CHECK(st_reap(&child, 0) == 0);
    }
    ST_CHECK(id_absent(id));
    id = -1;
    result = 0;
out:
    if (st_stop_child(&child) != 0)
        result = -1;
    if (detach(&probe) != 0)
        result = -1;
    if (remove_segment(&id) != 0)
        result = -1;
    if (st_close(&report[0]) != 0)
        result = -1;
    if (st_close(&report[1]) != 0)
        result = -1;
    if (st_close(&gate[0]) != 0)
        result = -1;
    if (st_close(&gate[1]) != 0)
        result = -1;
    return result;
}

/// 始终验证显式舍入零地址的 shmat 结果；只有实际零页策略同时拒绝 mmap 和 shmat 时跳过数据覆盖。
static int rounded_zero(size_t page, int *mapping_result)
{
    int result = -1, coverage = KSFT_FAIL, id = -1;
    int channel[2] = { -1, -1 }, owns_zero = 0;
    void *reservation = MAP_FAILED, *rejected = (void *)-1;
    void *alias = (void *)-1, *rounded = (void *)-1;
    unsigned expected = 0x76543210, actual = 0;
    id = shmget(IPC_PRIVATE, page, 0600);
    ST_CHECK(0 <= id);

    errno = 0;
    reservation = mmap(NULL, page, PROT_NONE,
                       MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0);
    int reservation_errno = errno;
    owns_zero = reservation == NULL;
    errno = 0;
    rejected = shmat(id, (void *)1, SHM_RND | SHM_REMAP);
    int remap_errno = errno;
    errno = 0;
    rounded = shmat(id, (void *)1, SHM_RND);
    int attach_errno = errno;
    if (rejected == NULL || rounded == NULL)
        owns_zero = 1;
    ST_CHECK(rejected == (void *)-1 && remap_errno == EINVAL);

    if (reservation == MAP_FAILED) {
        ST_CHECK(reservation_errno == EPERM || reservation_errno == EACCES);
        ST_CHECK(rounded == (void *)-1 && attach_errno == reservation_errno);
        ksft_print_msg("zero-page policy mmap errno=%d shmat errno=%d\n",
                       reservation_errno, attach_errno);
        coverage = KSFT_SKIP;
        result = 0;
        goto out;
    }

    ST_CHECK(reservation == NULL && rounded == NULL);
    ST_CHECK(pipe(channel) == 0);
    alias = shmat(id, NULL, 0);
    ST_CHECK(alias != (void *)-1 && alias != NULL);
    *(volatile unsigned *)alias = expected;
    ST_CHECK(syscall(SYS_write, channel[1], (void *)0, sizeof(actual)) == (long)sizeof(actual));
    ST_CHECK(st_read_all(channel[0], &actual, sizeof(actual)) == 0 && actual == expected);
    ST_CHECK(shmdt(NULL) == 0);
    owns_zero = 0;
    rounded = (void *)-1;
    ST_CHECK(detach(&alias) == 0);
    ST_CHECK(remove_segment(&id) == 0);
    coverage = KSFT_PASS;
    result = 0;
out:
    if (rejected != (void *)-1 && rejected != NULL && rejected != rounded &&
        shmdt(rejected) != 0)
        result = -1;
    if (rounded != (void *)-1 && rounded != NULL && shmdt(rounded) != 0)
        result = -1;
    if (detach(&alias) != 0)
        result = -1;
    if (owns_zero && munmap(NULL, page) != 0)
        result = -1;
    if (reservation != MAP_FAILED && reservation != NULL && munmap(reservation, page) != 0)
        result = -1;
    if (remove_segment(&id) != 0)
        result = -1;
    if (st_close(&channel[0]) != 0)
        result = -1;
    if (st_close(&channel[1]) != 0)
        result = -1;
    *mapping_result = result == 0 ? coverage : KSFT_FAIL;
    return result;
}

/// 正式入口输出七条 TAP；零页策略仅影响覆盖与别名数据子项，内部 exec 模式检查旧连接退役。
int main(int argc, char **argv)
{
    if (argc == 3 && strcmp(argv[1], "--exec-check") == 0) {
        char *end;
        long id = strtol(argv[2], &end, 10);
        return *end == '\0' && 0 <= id && id <= INT32_MAX && id_absent((int)id) ? 0 : 1;
    }
    ksft_print_header();
    if (argc != 1)
        ksft_exit_fail_msg("usage: %s\n", argv[0]);
    long page = sysconf(_SC_PAGESIZE);
    char *program = realpath(argv[0], NULL);
    if (page <= 0 || program == NULL)
        ksft_exit_fail_msg("page size or executable path errno=%d\n", errno);
    ksft_set_plan(7);
    ksft_test_result(keys_and_rollback((size_t)page) == 0,
                     "keys exact size EXCL failed attach rollback and RMID key reuse\n");
    int zero_mapping = KSFT_FAIL;
    int zero_contract = rounded_zero((size_t)page, &zero_mapping);
    ksft_test_result(zero_contract == 0,
                     "SHM_RND fixed-zero address result permissions and REMAP rejection\n");
    ksft_test_result_code(zero_mapping, "fixed-zero overwrite and alias data",
                          "%s\n", zero_mapping == KSFT_SKIP ?
                          "zero-page mapping denied by memory policy" : "");
    ksft_test_result(shared_fork((size_t)page) == 0,
                     "two attachments four fork writers and attach after immediate RMID\n");
    ksft_test_result(fragmented_detach((size_t)page) == 0,
                     "mprotect partial munmap shmdt preserves replacement neighbors and aliases\n");
    ksft_test_result(shared_vm((size_t)page) == 0,
                     "CLONE_VM shares one attachment lifetime\n");
    ksft_test_result(retirement(program, 0, (size_t)page) == 0 &&
                     retirement(program, 1, (size_t)page) == 0 &&
                     retirement(program, 2, (size_t)page) == 0,
                     "last attachment retires on exit cancellation and exec\n");
    free(program);
    ksft_finished();
}
