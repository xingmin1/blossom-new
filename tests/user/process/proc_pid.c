#define _GNU_SOURCE
#include <ctype.h>
#include <dirent.h>
#include <fcntl.h>
#include <limits.h>
#include <pthread.h>
#include <stdlib.h>
#include <stdint.h>
#include <string.h>
#include <sys/stat.h>
#include <time.h>
#include "kselftest.h"
#include "selftest_io.h"

/// 父进程持有子进程及两个同步管道，清理时只回收自己创建的资源。
struct child {
    /// 尚未回收的子进程编号。
    pid_t pid;
    /// 父写子读的命令管道。
    int command[2];
    /// 子写父读的就绪管道。
    int ready[2];
    /// exec 参数中携带的命令读端编号。
    char input[32];
    /// exec 参数中携带的就绪写端编号。
    char output[32];
};

/// 解析后的进程统计保留所有数字字段，字段二单独保存原始进程名。
struct process_stat {
    /// 第一个字段中的进程编号。
    long pid;
    /// 括号内的进程名字节。
    char comm[256];
    /// 单字符调度状态。
    char state;
    /// 按字段编号索引的数字文本，允许无符号最大值。
    char number[53][32];
};

/// 初始化所有资源为未拥有状态。
static void child_init(struct child *child)
{
    *child = (struct child){ .pid = -1, .command = { -1, -1 }, .ready = { -1, -1 } };
}

/// 无论检查在哪一步失败，都结束本次子进程并关闭全部本地管道。
static int child_cleanup(struct child *child)
{
    int result = st_stop_child(&child->pid);
    for (int i = 0; i < 2; ++i) {
        if (st_close(&child->command[i]) != 0)
            result = -1;
        if (st_close(&child->ready[i]) != 0)
            result = -1;
    }
    return result;
}

/// 读取整个有限 proc 文件并检测超过缓冲区的内容，不依赖单次 read 完成交付。
static ssize_t read_proc(pid_t pid, const char *name, void *buffer, size_t capacity)
{
    char path[80];
    int fd = -1;
    ssize_t result = -1;
    size_t used = 0;
    if (snprintf(path, sizeof(path), "/proc/%ld/%s", (long)pid, name) <= 0)
        return -1;
    fd = open(path, O_RDONLY);
    if (fd < 0)
        return -1;
    while (used < capacity) {
        ssize_t count = read(fd, (char *)buffer + used, capacity - used);
        if (count < 0 && errno == EINTR)
            continue;
        if (count < 0)
            goto out;
        if (count == 0) {
            result = (ssize_t)used;
            goto out;
        }
        used += (size_t)count;
    }
    errno = EOVERFLOW;
out:
    if (close(fd) != 0)
        result = -1;
    return result;
}

/// 按最后一个右括号解析名字，再精确检查状态与后续四十九个数字字段。
static int read_stat(pid_t pid, struct process_stat *stat)
{
    char text[4096], *end, *left, *right;
    ssize_t count = read_proc(pid, "stat", text, sizeof(text) - 1);
    if (count <= 0)
        return -1;
    text[count] = '\0';
    left = strchr(text, '(');
    right = strrchr(text, ')');
    stat->pid = strtol(text, &end, 10);
    if (end == text || left == NULL || right == NULL || right <= left || end + 1 != left ||
        sizeof(stat->comm) <= (size_t)(right - left - 1) || right[1] != ' ' ||
        right[2] == '\0' || right[3] != ' ')
        return -1;
    memcpy(stat->comm, left + 1, (size_t)(right - left - 1));
    stat->comm[right - left - 1] = '\0';
    stat->state = right[2];
    char *cursor = right + 4;
    for (int field = 4; field <= 52; ++field) {
        char *start = cursor;
        if (*cursor == '-')
            ++cursor;
        if (!isdigit((unsigned char)*cursor))
            return -1;
        while (isdigit((unsigned char)*cursor))
            ++cursor;
        size_t length = (size_t)(cursor - start);
        if (sizeof(stat->number[field]) <= length)
            return -1;
        memcpy(stat->number[field], start, length);
        stat->number[field][length] = '\0';
        if (field < 52) {
            if (*cursor++ != ' ')
                return -1;
        }
    }
    return cursor[0] == '\n' && cursor[1] == '\0' ? 0 : -1;
}

/// 返回指定字段的有符号数值，只用于本用例中可由 long 表示的 PID 字段。
static long stat_number(const struct process_stat *stat, int field)
{
    return strtol(stat->number[field], NULL, 10);
}

/// 枚举一次 proc 根，核对所有数字项均唯一且目标恰好出现 expected 次。
static int enumerate(pid_t target, int expected)
{
    DIR *directory = opendir("/proc");
    pid_t *seen = NULL;
    size_t count = 0, capacity = 0;
    int result = -1, found = 0;
    if (directory == NULL)
        return -1;
    errno = 0;
    struct dirent *entry;
    while ((entry = readdir(directory)) != NULL) {
        const char *name = entry->d_name;
        if (name[0] == '\0')
            continue;
        size_t length = strspn(name, "0123456789");
        if (name[length] != '\0')
            continue;
        long value = strtol(name, NULL, 10);
        if (value <= 0 || INT_MAX < value)
            goto out;
        for (size_t i = 0; i < count; ++i)
            if (seen[i] == value)
                goto out;
        if (count == capacity) {
            size_t next = capacity == 0 ? 64 : capacity * 2;
            if (next < capacity || SIZE_MAX / sizeof(*seen) < next)
                goto out;
            pid_t *grown = realloc(seen, next * sizeof(*seen));
            if (grown == NULL)
                goto out;
            seen = grown;
            capacity = next;
        }
        seen[count++] = (pid_t)value;
        found += value == target;
        errno = 0;
    }
    if (errno == 0 && found == expected)
        result = 0;
out:
    free(seen);
    if (closedir(directory) != 0)
        result = -1;
    return result;
}

/// 比较命令行的全部原始字节，包含每个参数末尾的 NUL。
static int check_cmdline(pid_t pid, char *const argv[])
{
    unsigned char bytes[PATH_MAX + 256];
    ssize_t count = read_proc(pid, "cmdline", bytes, sizeof(bytes));
    size_t offset = 0;
    if (count < 0)
        return -1;
    for (size_t i = 0; argv[i] != NULL; ++i) {
        size_t length = strlen(argv[i]) + 1;
        if ((size_t)count - offset < length || memcmp(bytes + offset, argv[i], length) != 0)
            return -1;
        offset += length;
    }
    return offset == (size_t)count ? 0 : -1;
}

/// 等待一个就绪字节，所有子模式都在产生该字节后等待命令或标准输入。
static int ready(struct child *child)
{
    char byte;
    return st_read_all(child->ready[0], &byte, 1) == 0 && byte == 'R' ? 0 : -1;
}

/// 经 exec 启动自己、长名链接或脚本，并保持父子管道的唯一读写端。
static int spawn(struct child *child, const char *program, const char *mode, int script)
{
    if (pipe(child->command) != 0 || pipe(child->ready) != 0)
        return -1;
    if (snprintf(child->input, sizeof(child->input), "%d", child->command[0]) <= 0 ||
        snprintf(child->output, sizeof(child->output), "%d", child->ready[1]) <= 0)
        return -1;
    child->pid = fork();
    if (child->pid < 0)
        return -1;
    if (child->pid == 0) {
        if (close(child->command[1]) != 0 || close(child->ready[0]) != 0)
            _exit(1);
        char *environment[] = { NULL };
        if (script) {
            if (dup2(child->command[0], STDIN_FILENO) < 0 ||
                dup2(child->ready[1], STDOUT_FILENO) < 0)
                _exit(1);
            char *arguments[] = { (char *)program, NULL };
            execve(program, arguments, environment);
        } else {
            char *arguments[] = { "proc-pid-child", (char *)mode, child->input, child->output, NULL };
            execve(program, arguments, environment);
        }
        _exit(1);
    }
    if (st_close(&child->command[0]) != 0 || st_close(&child->ready[1]) != 0)
        return -1;
    return ready(child);
}

/// 在真实组首退出后，由唯一工作线程保持进程及参数地址空间存活。
static void *blocked_worker(void *opaque)
{
    int *fds = opaque;
    char byte;
    if (st_write_all(fds[1], "R", 1) != 0 || st_read_all(fds[0], &byte, 1) != 0)
        _exit(1);
    return NULL;
}

/// 子模式只接受本次传入的两个描述符，setsid 完成后发送新的同步字节。
static int run_child(const char *mode, const char *input, const char *output)
{
    int fds[2] = { atoi(input), atoi(output) };
    if (strcmp(mode, "--thread-child") == 0) {
        static int worker_fds[2];
        worker_fds[0] = fds[0];
        worker_fds[1] = fds[1];
        pthread_t worker;
        if (pthread_create(&worker, NULL, blocked_worker, worker_fds) != 0)
            return 1;
        pthread_exit(NULL);
    }
    if (st_write_all(fds[1], "R", 1) != 0)
        return 1;
    char byte;
    while (st_read_all(fds[0], &byte, 1) == 0) {
        if (byte != 'S' || setsid() < 0 || st_write_all(fds[1], "R", 1) != 0)
            return 1;
    }
    return 0;
}

/// 受控脚本解释器只通知就绪并等待标准输入，不修改进程名或参数存储。
static int run_script_child(void)
{
    char byte;
    if (st_write_all(STDOUT_FILENO, "R", 1) != 0)
        return 1;
    return st_read_all(STDIN_FILENO, &byte, 1) == 0 ? 0 : 1;
}

/// 在有限的单调期限内等待目标状态，避免将主线程退出调度窗口当成固定延时。
static int await_state(pid_t pid, char expected, struct process_stat *stat)
{
    struct timespec start, now, pause = { .tv_sec = 0, .tv_nsec = 1000000 };
    if (clock_gettime(CLOCK_MONOTONIC, &start) != 0)
        return -1;
    for (;;) {
        if (read_stat(pid, stat) != 0)
            return -1;
        if (stat->state == expected)
            return 0;
        if (clock_gettime(CLOCK_MONOTONIC, &now) != 0 || 5 <= now.tv_sec - start.tv_sec)
            return -1;
        if (nanosleep(&pause, NULL) != 0 && errno != EINTR)
            return -1;
    }
}

/// 计算执行路径 basename 的十五字节进程名，不使用 argv[0]。
static void expected_comm(const char *path, char comm[16])
{
    const char *basename = strrchr(path, '/');
    basename = basename == NULL ? path : basename + 1;
    size_t length = strlen(basename);
    if (15 < length)
        length = 15;
    memcpy(comm, basename, length);
    comm[length] = '\0';
}

/// 核对普通子进程枚举、完整 stat/cmdline，并在 setsid 后再次观察进程组与会话。
static int basic_and_session(const char *program, int session)
{
    struct child child;
    struct process_stat stat;
    char comm[16];
    int result = -1;
    child_init(&child);
    ST_CHECK(spawn(&child, program, "--child", 0) == 0);
    expected_comm(program, comm);
    ST_CHECK(enumerate(child.pid, 1) == 0);
    ST_CHECK(read_stat(child.pid, &stat) == 0 && stat.pid == child.pid);
    ST_CHECK(strcmp(stat.comm, comm) == 0 && (stat.state == 'R' || stat.state == 'S'));
    ST_CHECK(stat_number(&stat, 4) == getpid());
    ST_CHECK(stat_number(&stat, 5) == getpgid(child.pid) && stat_number(&stat, 6) == getsid(child.pid));
    char *arguments[] = { "proc-pid-child", "--child", child.input, child.output, NULL };
    ST_CHECK(check_cmdline(child.pid, arguments) == 0);
    if (session) {
        ST_CHECK(st_write_all(child.command[1], "S", 1) == 0 && ready(&child) == 0);
        ST_CHECK(read_stat(child.pid, &stat) == 0);
        ST_CHECK(stat_number(&stat, 5) == child.pid && stat_number(&stat, 6) == child.pid);
    }
    result = 0;
out:
    if (child_cleanup(&child) != 0)
        result = -1;
    return result;
}

/// 经长名链接和 shebang 脚本执行后，分别核对原执行名与最终解释器参数字节。
static int exec_name(const char *program, int script)
{
    char directory[] = "proc-pid-XXXXXX", path[PATH_MAX], absolute[PATH_MAX], comm[16];
    char body[PATH_MAX + 32];
    struct child child;
    struct process_stat stat;
    int result = -1, fd = -1, made = 0, created = 0;
    child_init(&child);
    ST_CHECK(mkdtemp(directory) != NULL);
    made = 1;
    ST_CHECK(0 < snprintf(path, sizeof(path), "%s/%s", directory,
                          script ? "proc-script-name" : "proc-long-executable-name"));
    if (script) {
        fd = open(path, O_WRONLY | O_CREAT | O_EXCL, 0700);
        ST_CHECK(0 <= fd);
        created = 1;
        int length = snprintf(body, sizeof(body), "#!%s --script-child\n", program);
        ST_CHECK(0 < length && (size_t)length < sizeof(body));
        ST_CHECK(st_write_all(fd, body, (size_t)length) == 0 && st_close(&fd) == 0);
        ST_CHECK(realpath(path, absolute) != NULL);
    } else {
        ST_CHECK(symlink(program, path) == 0);
        created = 1;
        char cwd[PATH_MAX];
        ST_CHECK(getcwd(cwd, sizeof(cwd)) != NULL);
        int length = snprintf(absolute, sizeof(absolute), "%s/%s", cwd, path);
        ST_CHECK(0 < length && (size_t)length < sizeof(absolute));
    }
    ST_CHECK(spawn(&child, absolute, "--child", script) == 0);
    expected_comm(absolute, comm);
    ST_CHECK(read_stat(child.pid, &stat) == 0 && strcmp(stat.comm, comm) == 0);
    char *arguments[] = { "proc-pid-child", "--child", child.input, child.output, NULL };
    char *script_arguments[] = { (char *)program, "--script-child", absolute, NULL };
    ST_CHECK(check_cmdline(child.pid, script ? script_arguments : arguments) == 0);
    result = 0;
out:
    if (child_cleanup(&child) != 0 || st_close(&fd) != 0)
        result = -1;
    if (created && unlink(path) != 0)
        result = -1;
    if (made && rmdir(directory) != 0)
        result = -1;
    return result;
}

/// 旧进程目录相对查找必须失败，仅接受路径缺项或原进程身份已消失两种错误。
static int retired_lookup_failed(int fd, int error)
{
    return fd == -1 && (error == ENOENT || error == ESRCH);
}

/// 保持旧目录 fd，观察僵尸后回收，再检查路径、枚举和旧目录相对查找均失效。
static int zombie_and_reuse(const char *program, int replacement)
{
    struct child first, second;
    struct process_stat stat;
    char directory[64], path[80];
    int result = -1, old = -1, fd = -1, status;
    child_init(&first);
    child_init(&second);
    ST_CHECK(spawn(&first, program, "--child", 0) == 0);
    ST_CHECK(0 < snprintf(directory, sizeof(directory), "/proc/%ld", (long)first.pid));
    old = open(directory, O_RDONLY | O_DIRECTORY);
    ST_CHECK(0 <= old);
    ST_CHECK(kill(first.pid, SIGKILL) == 0 && await_state(first.pid, 'Z', &stat) == 0);
    pid_t retired = first.pid;
    ST_CHECK(waitpid(first.pid, &status, 0) == first.pid);
    first.pid = -1;
    ST_CHECK(WIFSIGNALED(status) && WTERMSIG(status) == SIGKILL);
    ST_CHECK(0 < snprintf(path, sizeof(path), "%s/stat", directory));
    errno = 0;
    fd = open(path, O_RDONLY);
    ST_CHECK(fd == -1 && errno == ENOENT);
    ST_CHECK(enumerate(retired, 0) == 0);
    errno = 0;
    fd = openat(old, "stat", O_RDONLY);
    ST_CHECK(retired_lookup_failed(fd, errno));
    if (replacement) {
        ST_CHECK(spawn(&second, program, "--child", 0) == 0);
        char *arguments[] = { "proc-pid-child", "--child", second.input, second.output, NULL };
        ST_CHECK(check_cmdline(second.pid, arguments) == 0);
        errno = 0;
        fd = openat(old, "cmdline", O_RDONLY);
        ST_CHECK(retired_lookup_failed(fd, errno));
    }
    result = 0;
out:
    if (child_cleanup(&first) != 0 || child_cleanup(&second) != 0 ||
        st_close(&old) != 0 || st_close(&fd) != 0)
        result = -1;
    return result;
}

/// 主线程退出但工作线程仍阻塞时，组首状态为 Z、进程名保留且 cmdline 为空。
static int leader_exit(const char *program)
{
    struct child child;
    struct process_stat stat;
    char comm[16], byte;
    int result = -1;
    child_init(&child);
    ST_CHECK(spawn(&child, program, "--thread-child", 0) == 0);
    ST_CHECK(await_state(child.pid, 'Z', &stat) == 0);
    expected_comm(program, comm);
    ST_CHECK(strcmp(stat.comm, comm) == 0);
    ST_CHECK(read_proc(child.pid, "cmdline", &byte, 1) == 0);
    result = 0;
out:
    if (child_cleanup(&child) != 0)
        result = -1;
    return result;
}

/// 默认入口产生七项 TAP；内部子模式只执行同步协议，不重复登记自测项。
int main(int argc, char **argv)
{
    if (argc == 4 && (strcmp(argv[1], "--child") == 0 || strcmp(argv[1], "--thread-child") == 0))
        return run_child(argv[1], argv[2], argv[3]);
    if (argc == 3 && strcmp(argv[1], "--script-child") == 0)
        return run_script_child();
    ksft_print_header();
    if (argc != 1)
        ksft_exit_fail_msg("usage: %s\n", argv[0]);
    char *program = realpath(argv[0], NULL);
    if (program == NULL)
        ksft_exit_fail_msg("realpath executable errno=%d\n", errno);
    ksft_set_plan(7);
    ksft_test_result(basic_and_session(program, 0) == 0, "proc enumeration stat fields and exact cmdline\n");
    ksft_test_result(basic_and_session(program, 1) == 0, "setsid updates process group and session\n");
    ksft_test_result(exec_name(program, 0) == 0, "long symlink execution preserves truncated filename comm\n");
    ksft_test_result(exec_name(program, 1) == 0, "shebang script name and controlled interpreter command line\n");
    ksft_test_result(zombie_and_reuse(program, 0) == 0, "zombie visibility and post-wait path invalidation\n");
    ksft_test_result(leader_exit(program) == 0, "leader exit retains comm and empties cmdline\n");
    ksft_test_result(zombie_and_reuse(program, 1) == 0, "retired directory cannot address replacement process\n");
    free(program);
    ksft_finished();
}
