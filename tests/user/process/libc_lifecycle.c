#include "libc_common.h"
#include "fp.h"

static const unsigned char packet_head[] = { 'T', 'L', 'S', 0 };
static const unsigned char packet_tail[] = { 0xff, 'f', 'o', 'r', 'k', '\n' };

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

/// 把十进制用户参数完整解析为真实描述符，不预设表槽编号。
static int parse_fd(const char *text, int *value)
{
    unsigned long parsed = 0;
    if (text == NULL || text[0] == '\0')
        return -1;
    for (const unsigned char *cursor = (const unsigned char *)text; *cursor != '\0'; ++cursor) {
        if (*cursor < '0' || '9' < *cursor || 127UL / 10 < parsed)
            return -1;
        parsed = parsed * 10 + (*cursor - '0');
    }
    if (127UL < parsed)
        return -1;
    *value = (int)parsed;
    return 0;
}

/// 迁核模式通过两个普通 pipe 逐次握手，不用额外 syscall 或共享测试页。
int migration_handshake(const char *ready_text, const char *go_text)
{
    int ready_fd;
    int go_fd;
    if (parse_fd(ready_text, &ready_fd) != 0 || parse_fd(go_text, &go_fd) != 0 ||
        ready_fd == go_fd || ready_fd < 0 || go_fd < 0)
        return fail("migration fd arguments");
    for (unsigned round = 0; round < 2; ++round) {
        unsigned char ready = 'R';
        unsigned char go;
        ssize_t written;
        do {
            written = write(ready_fd, &ready, 1);
        } while (written < 0 && errno == EINTR);
        if (written != 1)
            return fail("migration ready write");
        ssize_t received;
        do {
            received = read(go_fd, &go, 1);
        } while (received < 0 && errno == EINTR);
        if (received != 1 || go != 'M')
            return fail("migration wake read");
        if (check_tls_state() != EXIT_SUCCESS || check_fp() != EXIT_SUCCESS)
            return EXIT_FAILURE;
    }
    if (close(ready_fd) != 0 || close(go_fd) != 0)
        return fail("migration close");
    return EXIT_SUCCESS;
}

/// fork、readv/writev、磁盘 exec、wait 与父子 TLS 隔离共同走真实生命周期。
int run_body(const char *executable)
{
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
        fork_child(descriptors[0], descriptors[1], executable);

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
    if (tls_image != 0x2468ace0U || tls_zero != 0x10203040U || errno != EDOM ||
        read_fp_value() != fp_pattern)
        result = fail("parent state isolation");
    if (result != EXIT_SUCCESS)
        return result;
    if (printf("libc-smoke: main argv TLS malloc stdio readv writev fork exec wait passed\n") < 0 ||
        fflush(stdout) != 0)
        return fail("parent stdio");
    return EXIT_SUCCESS;
}
