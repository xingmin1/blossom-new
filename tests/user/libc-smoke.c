#define _POSIX_C_SOURCE 200809L

#include <errno.h>
#include <sched.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <unistd.h>

static _Thread_local volatile unsigned tls_image = 0x13579bdfU;
static _Thread_local volatile unsigned tls_zero;
static const unsigned char packet_head[] = { 'T', 'L', 'S', 0 };
static const unsigned char packet_tail[] = { 0xff, 'f', 'o', 'r', 'k', '\n' };

enum { EXEC_CHILD_STATUS = 37 };

/// 输出实际失败原因；成功消息只由已消费子进程状态的父进程产生。
static int fail(const char *operation)
{
    fprintf(stderr, "libc-smoke: %s failed (errno=%d)\n", operation, errno);
    return EXIT_FAILURE;
}

/// 经可观察的内存访问检查小块分配，释放后不保留指针。
static int check_malloc(void)
{
    volatile unsigned char *memory = malloc(257);
    if (memory == NULL)
        return fail("malloc");
    for (size_t i = 0; i < 257; ++i)
        memory[i] = (unsigned char)(i ^ 0xa5);
    for (size_t i = 0; i < 257; ++i) {
        if (memory[i] != (unsigned char)(i ^ 0xa5)) {
            free((void *)memory);
            return fail("malloc contents");
        }
    }
    free((void *)memory);
    return EXIT_SUCCESS;
}

/// 修改模板与零初始化 TLS，在实际调度系统调用返回后逐次核对。
static int check_tls(void)
{
    tls_image = 0x2468ace0U;
    tls_zero = 0x10203040U;
    errno = EDOM;
    for (unsigned i = 0; i < 64; ++i) {
        if (sched_yield() != 0)
            return fail("sched_yield");
        if (tls_image != 0x2468ace0U || tls_zero != 0x10203040U || errno != EDOM)
            return fail("TLS after syscall");
    }
    return EXIT_SUCCESS;
}

/// 处理合法短读，核对跨 iovec 边界的二进制内容，并读取真实 EOF。
static int read_packet(int fd)
{
    unsigned char head[sizeof(packet_head)];
    unsigned char tail[sizeof(packet_tail)];
    struct iovec vectors[] = {
        { .iov_base = head, .iov_len = sizeof(head) },
        { .iov_base = tail, .iov_len = sizeof(tail) },
    };
    size_t index = 0;
    while (index < 2) {
        ssize_t count = readv(fd, vectors + index, (int)(2 - index));
        if (count < 0 && errno == EINTR)
            continue;
        if (count <= 0)
            return fail("readv packet");
        size_t consumed = (size_t)count;
        while (index < 2 && vectors[index].iov_len <= consumed) {
            consumed -= vectors[index].iov_len;
            ++index;
        }
        if (index < 2) {
            vectors[index].iov_base = (unsigned char *)vectors[index].iov_base + consumed;
            vectors[index].iov_len -= consumed;
        }
    }
    if (memcmp(head, packet_head, sizeof(head)) != 0 ||
        memcmp(tail, packet_tail, sizeof(tail)) != 0)
        return fail("readv contents");

    unsigned char extra;
    struct iovec eof = { .iov_base = &extra, .iov_len = 1 };
    ssize_t count;
    do {
        count = readv(fd, &eof, 1);
    } while (count < 0 && errno == EINTR);
    if (count != 0)
        return fail("pipe EOF");
    return EXIT_SUCCESS;
}

/// fork 子进程检查继承 TLS，发送一笔原子小 writev，再执行磁盘上的同一普通程序。
static void fork_child(int read_fd, int write_fd, const char *executable)
{
    if (close(read_fd) != 0)
        _exit(fail("child close read end"));
    if (tls_image != 0x2468ace0U || tls_zero != 0x10203040U)
        _exit(fail("fork TLS inheritance"));
    tls_image = 7;
    tls_zero = 9;

    struct iovec vectors[] = {
        { .iov_base = (void *)packet_head, .iov_len = sizeof(packet_head) },
        { .iov_base = (void *)packet_tail, .iov_len = sizeof(packet_tail) },
    };
    ssize_t count;
    do {
        count = writev(write_fd, vectors, 2);
    } while (count < 0 && errno == EINTR);
    if (count != (ssize_t)(sizeof(packet_head) + sizeof(packet_tail)))
        _exit(fail("writev packet"));
    if (close(write_fd) != 0)
        _exit(fail("child close write end"));
    execl(executable, executable, "--exec-child", "stage7", (char *)NULL);
    _exit(fail("exec"));
}

/// 普通 C 入口检查 argv、TLS、分配、stdio 与 fork/exec/wait；exec 新映像以独立状态退出。
int main(int argc, char **argv)
{
    if (argc < 1 || argv == NULL || argv[0] == NULL || argv[0][0] != '/' ||
        argv[argc] != NULL)
        return fail("main argv");
    int exec_child = argc == 3 && strcmp(argv[1], "--exec-child") == 0 &&
                     strcmp(argv[2], "stage7") == 0;
    if (!exec_child && !(argc == 2 && strcmp(argv[1], "stage7") == 0))
        return fail("main arguments");
    if (tls_image != 0x13579bdfU || tls_zero != 0)
        return fail("initial TLS template");
    if (check_malloc() != EXIT_SUCCESS || check_tls() != EXIT_SUCCESS)
        return EXIT_FAILURE;
    if (exec_child) {
        if (printf("libc-smoke: exec child initialized\n") < 0 || fflush(stdout) != 0)
            return fail("child stdio");
        return EXEC_CHILD_STATUS;
    }

    int descriptors[2];
    if (pipe(descriptors) != 0)
        return fail("pipe");
    pid_t child = fork();
    if (child < 0) {
        int saved_errno = errno;
        close(descriptors[0]);
        close(descriptors[1]);
        errno = saved_errno;
        return fail("fork");
    }
    if (child == 0)
        fork_child(descriptors[0], descriptors[1], argv[0]);

    int result = EXIT_SUCCESS;
    if (close(descriptors[1]) != 0)
        result = fail("parent close write end");
    if (read_packet(descriptors[0]) != EXIT_SUCCESS)
        result = EXIT_FAILURE;
    if (close(descriptors[0]) != 0)
        result = fail("parent close read end");

    int status;
    pid_t reaped;
    do {
        reaped = waitpid(child, &status, 0);
    } while (reaped < 0 && errno == EINTR);
    if (reaped != child || !WIFEXITED(status) || WEXITSTATUS(status) != EXEC_CHILD_STATUS)
        result = fail("waitpid exec child status");
    if (tls_image != 0x2468ace0U || tls_zero != 0x10203040U)
        result = fail("parent TLS isolation");
    if (result != EXIT_SUCCESS)
        return result;
    if (printf("libc-smoke: main argv TLS malloc stdio readv writev fork exec wait passed\n") < 0 ||
        fflush(stdout) != 0)
        return fail("parent stdio");
    return EXIT_SUCCESS;
}
