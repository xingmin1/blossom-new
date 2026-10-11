#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <netinet/tcp.h>
#include <fcntl.h>
#include <signal.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>
#include <poll.h>

_Static_assert(sizeof(struct sockaddr_in) == 16, "sockaddr_in ABI");
_Static_assert(offsetof(struct sockaddr_in, sin_family) == 0, "sockaddr family ABI");
_Static_assert(offsetof(struct sockaddr_in, sin_port) == 2, "sockaddr port ABI");
_Static_assert(offsetof(struct sockaddr_in, sin_addr) == 4, "sockaddr address ABI");
_Static_assert(sizeof(socklen_t) == 4, "socklen ABI");
_Static_assert(sizeof(struct epoll_event) == 16, "natural epoll ABI");
_Static_assert(offsetof(struct epoll_event, events) == 0, "epoll events ABI");
_Static_assert(offsetof(struct epoll_event, data) == 8, "epoll data ABI");
_Static_assert(sizeof(struct iovec) == 16, "iovec ABI");
_Static_assert(offsetof(struct iovec, iov_base) == 0, "iovec base ABI");
_Static_assert(offsetof(struct iovec, iov_len) == 8, "iovec length ABI");
_Static_assert(sizeof(struct msghdr) == 56, "msghdr LP64 ABI");
_Static_assert(offsetof(struct msghdr, msg_name) == 0, "message name ABI");
_Static_assert(offsetof(struct msghdr, msg_namelen) == 8, "message namelen ABI");
_Static_assert(offsetof(struct msghdr, msg_iov) == 16, "message iov ABI");
_Static_assert(offsetof(struct msghdr, msg_iovlen) == 24, "message iovlen ABI");
_Static_assert(offsetof(struct msghdr, msg_control) == 32, "message control ABI");
_Static_assert(offsetof(struct msghdr, msg_controllen) == 40, "message controllen ABI");
_Static_assert(offsetof(struct msghdr, msg_flags) == 48, "message flags ABI");

static pid_t children[8];
static size_t child_count;
static int ready_fd = -1, go_fd = -1;
static volatile sig_atomic_t signals_seen;
static volatile sig_atomic_t handler_report;
struct report { uint32_t phase, pid, value; };

/// 失败时终止并回收本进程已经发表的孩子，描述符由正常退出释放。
static void fail(const char *expression, int line)
{
    int saved = errno;
    fprintf(stderr, "network-smoke line=%d check=%s errno=%d\n", line, expression, saved);
    for (size_t i = 0; i < child_count; ++i) {
        if (kill(children[i], SIGKILL) == -1 && errno != ESRCH)
            fprintf(stderr, "network-smoke cleanup kill pid=%d errno=%d\n", children[i], errno);
    }
    for (size_t i = 0; i < child_count; ++i) {
        pid_t reaped;
        do { reaped = waitpid(children[i], NULL, 0); } while (reaped == -1 && errno == EINTR);
        if (reaped != children[i] && errno != ECHILD)
            fprintf(stderr, "network-smoke cleanup wait pid=%d errno=%d\n", children[i], errno);
    }
    exit(1);
}
#define CHECK(e) do { if (!(e)) fail(#e, __LINE__); } while (0)
#define ERROR(e, n) do { errno = 0; CHECK((e) == -1 && errno == (n)); } while (0)

/// 按真实短写推进完整字节，零进度和中断均保留为失败。
static void write_all(int fd, const void *data, size_t length)
{
    const unsigned char *bytes = data;
    size_t done = 0;
    while (done < length) {
        ssize_t count = write(fd, bytes + done, length - done);
        CHECK(0 < count);
        done += (size_t)count;
    }
}

/// 读取准确记录长度，避免把 EOF 或短读当成完成。
static void read_all(int fd, void *data, size_t length)
{
    unsigned char *bytes = data;
    size_t done = 0;
    while (done < length) {
        ssize_t count = read(fd, bytes + done, length - done);
        CHECK(0 < count);
        done += (size_t)count;
    }
}

/// 原管道交付真实 errno 或阻塞阶段，协调者不从延时推断握手。
static void report(uint32_t phase, pid_t pid, uint32_t value)
{
    CHECK(0 <= ready_fd);
    struct report message = { phase, (uint32_t)pid, value };
    write_all(ready_fd, &message, sizeof message);
}

/// 协调者观察真实协议与调度事实后只交付一个放行字节。
static void proceed(void)
{
    unsigned char byte;
    read_all(go_fd, &byte, 1);
    CHECK(byte == 'G');
}

/// 孩子继承普通描述符，父保存 PID 供所有错误路径回收。
static pid_t fork_child(void)
{
    CHECK(child_count < sizeof children / sizeof children[0]);
    pid_t pid = fork();
    CHECK(0 <= pid);
    if (pid == 0) child_count = 0;
    else children[child_count++] = pid;
    return pid;
}

/// 真实 wait 状态必须为指定退出值，不能只检查已经结束。
static void reap(pid_t pid, int code)
{
    int status;
    CHECK(waitpid(pid, &status, 0) == pid);
    CHECK(WIFEXITED(status) && WEXITSTATUS(status) == code);
    for (size_t i = 0; i < child_count; ++i) {
        if (children[i] == pid) {
            children[i] = children[--child_count];
            break;
        }
    }
}

/// 地址使用头文件真实布局和网络字节序。
static struct sockaddr_in address(unsigned port)
{
    struct sockaddr_in value = { .sin_family = AF_INET,
        .sin_port = htons((uint16_t)port), .sin_addr.s_addr = htonl(INADDR_LOOPBACK) };
    return value;
}

/// listener 使用正式 socket/bind/listen，不依赖测试专用网络入口。
static int listener(unsigned port, int flags)
{
    int fd = socket(AF_INET, SOCK_STREAM | flags, IPPROTO_TCP);
    CHECK(0 <= fd);
    struct sockaddr_in local = address(port);
    CHECK(bind(fd, (struct sockaddr *)&local, sizeof local) == 0);
    CHECK(listen(fd, 8) == 0);
    return fd;
}

/// 同进程建立真实环回连接，握手由后台推进并先于 accept 完成。
static void pair(int *client, int *server)
{
    int listening = listener(0, 0);
    struct sockaddr_in local;
    socklen_t length = sizeof local;
    CHECK(getsockname(listening, (struct sockaddr *)&local, &length) == 0);
    CHECK(length == sizeof local && local.sin_port != 0);
    *client = socket(AF_INET, SOCK_STREAM, 0);
    CHECK(0 <= *client);
    CHECK(connect(*client, (struct sockaddr *)&local, sizeof local) == 0);
    *server = accept(listening, NULL, NULL);
    CHECK(0 <= *server);
    CHECK(close(listening) == 0);
}

/// 单次 ppoll 直接消费目标 syscall 的就绪集合。
static short wait_ready(int fd, short events)
{
    struct pollfd item = { .fd = fd, .events = events };
    struct timespec timeout = { .tv_sec = 5 };
    CHECK(syscall(SYS_ppoll, &item, 1, &timeout, NULL, 8) == 1);
    return item.revents;
}

/// 内核消费一次 SO_ERROR，长度必须为实际 int ABI。
static int socket_error(int fd)
{
    int error = -1;
    socklen_t length = sizeof error;
    CHECK(getsockopt(fd, SOL_SOCKET, SO_ERROR, &error, &length) == 0);
    CHECK(length == sizeof error);
    return error;
}

/// exec 后检查未关闭别名仍共享 OFD，原 CLOEXEC 槽已经被撤下。
static int exec_identity(int argc, char **argv)
{
    CHECK(argc == 4);
    int kept = atoi(argv[2]), closed = atoi(argv[3]);
    ERROR(fcntl(closed, F_GETFD), EBADF);
    CHECK((fcntl(kept, F_GETFL) & O_NONBLOCK) == 0);
    struct stat stat;
    CHECK(fstat(kept, &stat) == 0 && S_ISSOCK(stat.st_mode));
    CHECK(stat.st_uid == 1000 && stat.st_gid == 2000);
    return 37;
}

/// TCP 与 UDP 身份核真实凭据，dup/fork 共享 OFD，exec 只撤逐槽 CLOEXEC。
static void identity(const char *program)
{
    CHECK(getuid() == 1000 && getgid() == 2000);
    int fd = socket(AF_INET, SOCK_STREAM | SOCK_NONBLOCK | SOCK_CLOEXEC, 0);
    CHECK(0 <= fd);
    int alias = dup(fd);
    CHECK(0 <= alias && alias != fd);
    struct stat first, second;
    CHECK(fstat(fd, &first) == 0 && fstat(alias, &second) == 0);
    CHECK(S_ISSOCK(first.st_mode) && (first.st_mode & 0777) == 0777);
    CHECK(first.st_uid == 1000 && first.st_gid == 2000);
    CHECK(first.st_ino == second.st_ino && first.st_dev == second.st_dev);
    CHECK(fcntl(fd, F_GETFD) == FD_CLOEXEC && fcntl(alias, F_GETFD) == 0);
    CHECK((fcntl(alias, F_GETFL) & O_NONBLOCK) != 0);
    const int options[] = {SO_TYPE, SO_DOMAIN, SO_PROTOCOL, SO_ACCEPTCONN, SO_RCVBUF, SO_SNDBUF};
    for (size_t i = 0; i < sizeof options / sizeof options[0]; ++i) {
        int value = -1;
        socklen_t length = sizeof value;
        CHECK(getsockopt(fd, SOL_SOCKET, options[i], &value, &length) == 0 && length == sizeof value);
        const int expected[] = {SOCK_STREAM, AF_INET, IPPROTO_TCP, 0, 16384, 16384};
        CHECK(value == expected[i]);
    }
    ERROR(lseek(fd, 0, SEEK_SET), ESPIPE);
    int udp = socket(AF_INET, SOCK_DGRAM | SOCK_NONBLOCK | SOCK_CLOEXEC, 0);
    CHECK(0 <= udp);
    CHECK(fstat(udp, &second) == 0 && S_ISSOCK(second.st_mode));
    CHECK(second.st_uid == 1000 && second.st_gid == 2000 && second.st_ino != first.st_ino);
    CHECK(fcntl(udp, F_GETFD) == FD_CLOEXEC && (fcntl(udp, F_GETFL) & O_NONBLOCK) != 0);
    const int udp_options[] = {SO_TYPE, SO_DOMAIN, SO_PROTOCOL};
    const int udp_expected[] = {SOCK_DGRAM, AF_INET, IPPROTO_UDP};
    for (size_t i = 0; i < 3; ++i) {
        int value = -1;
        socklen_t length = sizeof value;
        CHECK(getsockopt(udp, SOL_SOCKET, udp_options[i], &value, &length) == 0);
        CHECK(length == sizeof value && value == udp_expected[i]);
    }
    CHECK(close(udp) == 0);
    int channel[2];
    CHECK(pipe(channel) == 0);
    pid_t child = fork_child();
    if (child == 0) {
        CHECK(close(channel[0]) == 0);
        CHECK(fcntl(alias, F_SETFL, 0) == 0);
        write_all(channel[1], "F", 1);
        CHECK(close(channel[1]) == 0);
        char kept[20], closed[20];
        snprintf(kept, sizeof kept, "%d", alias);
        snprintf(closed, sizeof closed, "%d", fd);
        execl(program, program, "exec-identity", kept, closed, (char *)NULL);
        fail("exec identity", __LINE__);
    }
    CHECK(close(channel[1]) == 0);
    unsigned char byte;
    read_all(channel[0], &byte, 1);
    CHECK(byte == 'F' && (fcntl(fd, F_GETFL) & O_NONBLOCK) == 0);
    CHECK(read(channel[0], &byte, 1) == 0);
    reap(child, 37);
    CHECK(close(channel[0]) == 0 && close(alias) == 0 && close(fd) == 0);
}

/// 原 listener 及别名跨三次 accept 保 Listen，新 OFD 的身份和 flags 独立。
static void multiple_accept(void)
{
    int fd = listener(0, SOCK_NONBLOCK), alias = dup(fd);
    CHECK(0 <= alias);
    struct sockaddr_in local;
    socklen_t length = sizeof local;
    CHECK(getsockname(fd, (struct sockaddr *)&local, &length) == 0);
    ino_t previous = 0;
    for (unsigned i = 0; i < 3; ++i) {
        int client = socket(AF_INET, SOCK_STREAM, 0);
        CHECK(0 <= client);
        CHECK(connect(client, (struct sockaddr *)&local, sizeof local) == 0);
        CHECK(wait_ready(alias, POLLIN) & POLLIN);
        CHECK(listen(alias, 8) == 0);
        int server = accept4(alias, NULL, NULL, i == 1 ? SOCK_NONBLOCK | SOCK_CLOEXEC : 0);
        CHECK(0 <= server);
        struct stat stat;
        CHECK(fstat(server, &stat) == 0 && S_ISSOCK(stat.st_mode));
        CHECK(stat.st_ino != previous);
        previous = stat.st_ino;
        CHECK(((fcntl(server, F_GETFL) & O_NONBLOCK) != 0) == (i == 1));
        CHECK(fcntl(server, F_GETFD) == (i == 1 ? FD_CLOEXEC : 0));
        unsigned char byte = (unsigned char)(0x90 + i), output;
        write_all(client, &byte, 1);
        CHECK(wait_ready(server, POLLIN) & POLLIN);
        read_all(server, &output, 1);
        CHECK(output == byte);
        CHECK(close(client) == 0 && close(server) == 0);
    }
    int accepting = 0;
    length = sizeof accepting;
    CHECK(getsockopt(fd, SOL_SOCKET, SO_ACCEPTCONN, &accepting, &length) == 0 && accepting == 1);
    ERROR(accept(alias, NULL, NULL), EAGAIN);
    CHECK(close(fd) == 0 && close(alias) == 0);
}

/// held SYN 保证第二次 connect 确实仍进行中，拒绝的 SO_ERROR 只被消费一次。
static void nonblock_connect(void)
{
    int fd = listener(45204, 0);
    int client = socket(AF_INET, SOCK_STREAM | SOCK_NONBLOCK, 0);
    CHECK(0 <= client);
    struct sockaddr_in local = address(45204);
    ERROR(connect(client, (struct sockaddr *)&local, sizeof local), EINPROGRESS);
    ERROR(connect(client, (struct sockaddr *)&local, sizeof local), EALREADY);
    report('C', getpid(), EALREADY);
    proceed();
    CHECK(wait_ready(client, POLLOUT) & POLLOUT);
    CHECK(socket_error(client) == 0);
    ERROR(connect(client, (struct sockaddr *)&local, sizeof local), EISCONN);
    int server = accept(fd, NULL, NULL);
    CHECK(0 <= server);
    write_all(client, "\0\xffok", 4);
    unsigned char output[4];
    read_all(server, output, sizeof output);
    CHECK(memcmp(output, "\0\xffok", 4) == 0);
    CHECK(close(server) == 0 && close(client) == 0 && close(fd) == 0);
    client = socket(AF_INET, SOCK_STREAM | SOCK_NONBLOCK, 0);
    CHECK(0 <= client);
    local = address(45205);
    ERROR(connect(client, (struct sockaddr *)&local, sizeof local), EINPROGRESS);
    CHECK(wait_ready(client, POLLOUT) & (POLLOUT | POLLERR));
    CHECK(socket_error(client) == ECONNREFUSED);
    CHECK(socket_error(client) == 0);
    CHECK(close(client) == 0);
}

/// 图样随逻辑位置变化，环回绕和前缀丢失不能满足逐字节比较。
static unsigned char pattern(size_t position)
{
    return (unsigned char)((position * 37 + position / 251 + 19) & 255);
}

/// 大流由 fork 发送，短读和 PEEK 保完整顺序，FIN 只能在全部字节之后返回 EOF。
static void stream(void)
{
    int client, server;
    pair(&client, &server);
    const size_t total = 131145;
    pid_t child = fork_child();
    if (child == 0) {
        CHECK(close(server) == 0);
        unsigned char input[997];
        size_t done = 0;
        while (done < total) {
            size_t count = total - done;
            if (sizeof input < count) count = sizeof input;
            for (size_t i = 0; i < count; ++i) input[i] = pattern(done + i);
            struct iovec vectors[2] = {{input, count / 2}, {input + count / 2, count - count / 2}};
            ssize_t accepted = writev(client, vectors, 2);
            CHECK(0 < accepted && (size_t)accepted <= count);
            done += (size_t)accepted;
        }
        CHECK(shutdown(client, SHUT_WR) == 0 && close(client) == 0);
        _exit(0);
    }
    CHECK(close(client) == 0);
    unsigned char peeked[733], output[733];
    size_t done = 0;
    while (done < total) {
        ssize_t count = recv(server, peeked, sizeof peeked, MSG_PEEK);
        CHECK(0 < count && (size_t)count <= total - done);
        for (ssize_t i = 0; i < count; ++i) CHECK(peeked[i] == pattern(done + (size_t)i));
        size_t short_count = (size_t)count < 113 ? (size_t)count : 113;
        struct iovec vectors[2] = {{output, short_count / 2}, {output + short_count / 2, short_count - short_count / 2}};
        ssize_t received = readv(server, vectors, 2);
        CHECK(0 < received && (size_t)received <= short_count);
        CHECK(memcmp(output, peeked, (size_t)received) == 0);
        done += (size_t)received;
    }
    CHECK(read(server, output, sizeof output) == 0);
    reap(child, 0);
    CHECK(close(server) == 0);
}

/// 满接收窗口和满发送环先产生 EAGAIN，再让阻塞 writer 的真实 Task 被协调者观察。
static void backpressure(void)
{
    int client, server;
    pair(&client, &server);
    CHECK(fcntl(client, F_SETFL, O_NONBLOCK) == 0);
    unsigned char input[4096], output[733];
    memset(input, 0x6d, sizeof input);
    size_t total = 0;
    for (;;) {
        ssize_t count = send(client, input, sizeof input, MSG_NOSIGNAL);
        if (count == -1) {
            CHECK(errno == EAGAIN);
            struct pollfd item = { .fd = client, .events = POLLOUT };
            struct timespec timeout = { .tv_nsec = 200000000 };
            int ready = (int)syscall(SYS_ppoll, &item, 1, &timeout, NULL, 8);
            CHECK(0 <= ready && ready <= 1);
            if (ready == 0) break;
            CHECK(item.revents & POLLOUT);
            continue;
        }
        CHECK(0 < count);
        total += (size_t)count;
    }
    CHECK(0 < total);
    CHECK(fcntl(client, F_SETFL, 0) == 0);
    int finished[2];
    CHECK(pipe(finished) == 0);
    pid_t child = fork_child();
    if (child == 0) {
        CHECK(close(server) == 0 && close(finished[0]) == 0);
        report('B', getpid(), 0);
        write_all(client, "resume!", 7);
        write_all(finished[1], "D", 1);
        CHECK(close(finished[1]) == 0 && close(client) == 0);
        _exit(0);
    }
    CHECK(close(finished[1]) == 0);
    proceed();
    size_t done = 0;
    while (done < total) {
        size_t count = total - done;
        if (sizeof output < count) count = sizeof output;
        ssize_t received = read(server, output, count);
        CHECK(0 < received);
        for (ssize_t i = 0; i < received; ++i) CHECK(output[i] == 0x6d);
        done += (size_t)received;
    }
    read_all(server, output, 7);
    CHECK(memcmp(output, "resume!", 7) == 0);
    read_all(finished[0], output, 1);
    CHECK(output[0] == 'D');
    reap(child, 0);
    CHECK(close(finished[0]) == 0 && close(client) == 0 && close(server) == 0);
}

/// handler 只保存调用线程实际收到的信号；重启窗口可用原 pipe 报告 handler 已执行。
static void signal_handler(int number)
{
    signals_seen += 1;
    if (handler_report) {
        struct report message = { 'H', (uint32_t)getpid(), (uint32_t)number };
        ssize_t count = write(ready_fd, &message, sizeof message);
        if (count != sizeof message) _exit(2);
    }
}

/// 安装真实 sigaction，是否重启由原信号返回路径决定。
static void install_handler(int number, int flags)
{
    struct sigaction action = { .sa_handler = signal_handler, .sa_flags = flags };
    CHECK(sigemptyset(&action.sa_mask) == 0);
    CHECK(sigaction(number, &action, NULL) == 0);
}

/// SHUT_WR 保接收方向，NOSIGNAL 和普通发送分别验证 SIGPIPE 的抑制与实际递送。
static void half_close(void)
{
    int client, server;
    pair(&client, &server);
    install_handler(SIGPIPE, 0);
    write_all(client, "request", 7);
    CHECK(shutdown(client, SHUT_WR) == 0);
    unsigned char output[16];
    read_all(server, output, 7);
    CHECK(memcmp(output, "request", 7) == 0 && read(server, output, 1) == 0);
    write_all(server, "response", 8);
    CHECK(shutdown(server, SHUT_WR) == 0);
    read_all(client, output, 8);
    CHECK(memcmp(output, "response", 8) == 0 && read(client, output, 1) == 0);
    ERROR(send(client, "x", 1, MSG_NOSIGNAL), EPIPE);
    CHECK(signals_seen == 0);
    ERROR(send(client, "x", 1, 0), EPIPE);
    CHECK(signals_seen == 1);
    ERROR(send(client, NULL, 0, MSG_NOSIGNAL), EPIPE);
    CHECK(signals_seen == 1);
    ERROR(send(client, NULL, 0, 0), EPIPE);
    CHECK(signals_seen == 2);
    CHECK(close(client) == 0 && close(server) == 0);
}

/// 地址容量只写可容纳前缀，真实未映射页提供 copyin/copyout 部分错误。
static void address_faults(void)
{
    int client, server;
    pair(&client, &server);
    unsigned char short_address[24];
    memset(short_address, 0xa5, sizeof short_address);
    socklen_t length = 2;
    CHECK(getpeername(client, (struct sockaddr *)short_address, &length) == 0);
    CHECK(length == 16);
    sa_family_t family;
    memcpy(&family, short_address, sizeof family);
    CHECK(family == AF_INET);
    for (size_t i = 2; i < sizeof short_address; ++i) CHECK(short_address[i] == 0xa5);
    length = 0;
    CHECK(getsockname(client, (struct sockaddr *)1, &length) == 0 && length == 16);
    ERROR(getsockname(client, (struct sockaddr *)short_address, (socklen_t *)1), EFAULT);
    unsigned char *mapping = mmap(NULL, 8192, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    CHECK(mapping != MAP_FAILED);
    CHECK(munmap(mapping + 4096, 4096) == 0);
    length = 16;
    ERROR(getpeername(client, (struct sockaddr *)(mapping + 4096 - 3), &length), EFAULT);
    write_all(client, "partial-fault", 13);
    CHECK(wait_ready(server, POLLIN) & POLLIN);
    CHECK(recv(server, mapping + 4096 - 5, 13, MSG_PEEK) == 5);
    CHECK(memcmp(mapping + 4096 - 5, "parti", 5) == 0);
    CHECK(recv(server, mapping + 4096 - 5, 13, 0) == 5);
    CHECK(memcmp(mapping + 4096 - 5, "parti", 5) == 0);
    unsigned char output[32];
    read_all(server, output, 8);
    CHECK(memcmp(output, "al-fault", 8) == 0);
    ERROR(recv(server, (void *)1, 1, MSG_DONTWAIT), EAGAIN);
    write_all(client, "kept", 4);
    CHECK(wait_ready(server, POLLIN) & POLLIN);
    ERROR(recv(server, (void *)1, 4, 0), EFAULT);
    read_all(server, output, 4);
    CHECK(memcmp(output, "kept", 4) == 0);
    write_all(client, "no source", 9);
    length = 2;
    memset(short_address, 0xa5, sizeof short_address);
    CHECK(recvfrom(server, output, 9, 0, (struct sockaddr *)short_address, &length) == 9);
    CHECK(length == 0 && memcmp(output, "no source", 9) == 0);
    for (size_t i = 0; i < sizeof short_address; ++i) CHECK(short_address[i] == 0xa5);
    write_all(client, "null", 4);
    CHECK(recvfrom(server, output, 4, 0, NULL, (socklen_t *)1) == 4);
    CHECK(memcmp(output, "null", 4) == 0);
    memcpy(mapping + 4096 - 7, "copy-in", 7);
    CHECK(send(server, mapping + 4096 - 7, 15, MSG_NOSIGNAL) == 7);
    read_all(client, output, 7);
    CHECK(memcmp(output, "copy-in", 7) == 0);
    ERROR(send(server, (void *)1, 1, MSG_NOSIGNAL), EFAULT);
    CHECK(close(client) == 0 && close(server) == 0);
    int fd = listener(0, SOCK_NONBLOCK);
    struct sockaddr_in local;
    length = sizeof local;
    CHECK(getsockname(fd, (struct sockaddr *)&local, &length) == 0);
    client = socket(AF_INET, SOCK_STREAM, 0);
    CHECK(0 <= client && connect(client, (struct sockaddr *)&local, sizeof local) == 0);
    CHECK(wait_ready(fd, POLLIN) & POLLIN);
    int duplicates[128];
    size_t count = 0;
    for (;;) {
        int next = dup(fd);
        if (next == -1) { CHECK(errno == EMFILE); break; }
        CHECK(count < sizeof duplicates / sizeof duplicates[0]);
        duplicates[count++] = next;
    }
    ERROR(accept(fd, NULL, NULL), EMFILE);
    CHECK(0 < count);
    CHECK(close(duplicates[--count]) == 0);
    server = accept(fd, NULL, (socklen_t *)1);
    CHECK(0 <= server);
    write_all(client, "ready survives", 14);
    read_all(server, output, 14);
    CHECK(memcmp(output, "ready survives", 14) == 0);
    while (0 < count) CHECK(close(duplicates[--count]) == 0);
    CHECK(close(server) == 0 && close(client) == 0 && close(fd) == 0);
    fd = listener(0, 0);
    length = sizeof local;
    CHECK(getsockname(fd, (struct sockaddr *)&local, &length) == 0);
    int first = socket(AF_INET, SOCK_STREAM, 0);
    int next = socket(AF_INET, SOCK_STREAM, 0);
    CHECK(0 <= first && 0 <= next);
    CHECK(connect(first, (struct sockaddr *)&local, sizeof local) == 0);
    CHECK(connect(next, (struct sockaddr *)&local, sizeof local) == 0);
    CHECK(wait_ready(fd, POLLIN) & POLLIN);
    length = sizeof local;
    ERROR(accept(fd, (struct sockaddr *)(mapping + 4096 - 3), &length), EFAULT);
    server = accept(fd, NULL, (socklen_t *)1);
    CHECK(0 <= server);
    write_all(next, "after fault", 11);
    read_all(server, output, 11);
    CHECK(memcmp(output, "after fault", 11) == 0);
    CHECK(close(first) == 0 && close(next) == 0 && close(server) == 0 && close(fd) == 0);
    CHECK(munmap(mapping, 4096) == 0);
}

/// 单调时长用于消费一次绝对期限，不将多次通知当成新 timeout。
static uint64_t now_ns(void)
{
    struct timespec value;
    CHECK(clock_gettime(CLOCK_MONOTONIC, &value) == 0);
    return (uint64_t)value.tv_sec * 1000000000 + (uint64_t)value.tv_nsec;
}

/// 无兴趣的数据通知反复到达，ppoll/pwait 仍必须在原期限内结束。
static void deadline(int epoll_mode)
{
    int client, server;
    pair(&client, &server);
    int epfd = -1;
    if (epoll_mode) {
        epfd = epoll_create1(0);
        CHECK(0 <= epfd);
        struct epoll_event event = { .events = EPOLLPRI, .data.u64 = 0x123456789abcdef0ULL };
        CHECK(epoll_ctl(epfd, EPOLL_CTL_ADD, server, &event) == 0);
    }
    pid_t child = fork_child();
    if (child == 0) {
        CHECK(close(server) == 0);
        if (0 <= epfd) CHECK(close(epfd) == 0);
        struct timespec pause = { .tv_nsec = 20000000 };
        for (unsigned i = 0; i < 12; ++i) {
            write_all(client, "n", 1);
            CHECK(nanosleep(&pause, NULL) == 0);
        }
        CHECK(close(client) == 0);
        _exit(0);
    }
    uint64_t started = now_ns();
    if (epoll_mode) {
        struct epoll_event output;
        CHECK(syscall(SYS_epoll_pwait, epfd, &output, 1, 200, NULL, 8) == 0);
    } else {
        struct pollfd item = { .fd = server, .events = POLLPRI };
        struct timespec timeout = { .tv_nsec = 200000000 };
        CHECK(syscall(SYS_ppoll, &item, 1, &timeout, NULL, 8) == 0 && item.revents == 0);
    }
    uint64_t elapsed = now_ns() - started;
    CHECK(180000000 <= elapsed && elapsed < 350000000);
    unsigned char output[12];
    read_all(server, output, sizeof output);
    for (size_t i = 0; i < sizeof output; ++i) CHECK(output[i] == 'n');
    reap(child, 0);
    if (0 <= epfd) CHECK(close(epfd) == 0);
    CHECK(close(client) == 0 && close(server) == 0);
}

/// 真实空读和 held SYN connect 分别经历 EINTR 与 SA_RESTART，仍消费原连接字节。
static void interrupted(void)
{
    install_handler(SIGUSR1, 0);
    int client, server;
    pair(&client, &server);
    unsigned char output[16];
    report('R', getpid(), 0);
    ERROR(read(server, output, 1), EINTR);
    CHECK(signals_seen == 1);
    report('I', getpid(), EINTR);
    proceed();
    install_handler(SIGUSR1, SA_RESTART);
    handler_report = 1;
    pid_t child = fork_child();
    if (child == 0) {
        CHECK(close(client) == 0);
        report('T', getpid(), 0);
        read_all(server, output, 7);
        CHECK(memcmp(output, "restart", 7) == 0);
        CHECK(signals_seen == 2);
        CHECK(close(server) == 0);
        _exit(0);
    }
    proceed();
    write_all(client, "restart", 7);
    reap(child, 0);
    handler_report = 0;
    CHECK(close(client) == 0 && close(server) == 0);
    int fd = listener(45214, 0);
    client = socket(AF_INET, SOCK_STREAM, 0);
    CHECK(0 <= client);
    struct sockaddr_in local = address(45214);
    install_handler(SIGUSR1, 0);
    report('K', getpid(), 0);
    ERROR(connect(client, (struct sockaddr *)&local, sizeof local), EINTR);
    report('J', getpid(), EINTR);
    proceed();
    CHECK(connect(client, (struct sockaddr *)&local, sizeof local) == 0);
    server = accept(fd, NULL, NULL);
    CHECK(0 <= server);
    write_all(server, "same SYN", 8);
    read_all(client, output, 8);
    CHECK(memcmp(output, "same SYN", 8) == 0);
    CHECK(close(client) == 0 && close(server) == 0 && close(fd) == 0);
    fd = listener(45215, 0);
    client = socket(AF_INET, SOCK_STREAM, 0);
    CHECK(0 <= client);
    local = address(45215);
    report('V', getpid(), 0);
    proceed();
    install_handler(SIGUSR1, SA_RESTART);
    handler_report = 1;
    report('W', getpid(), 0);
    CHECK(connect(client, (struct sockaddr *)&local, sizeof local) == 0);
    handler_report = 0;
    server = accept(fd, NULL, NULL);
    CHECK(0 <= server);
    write_all(client, "one handshake", 13);
    read_all(server, output, 13);
    CHECK(memcmp(output, "one handshake", 13) == 0);
    CHECK(close(client) == 0 && close(server) == 0 && close(fd) == 0);
    report('Z', getpid(), 0);
    deadline(0);
    deadline(1);
}

/// 单个自然 epoll_event 保留高位 data，输出计数和就绪位均来自实际 syscall。
static int epoll_events(int epfd, struct epoll_event *output, int maximum, int timeout)
{
    int count = (int)syscall(SYS_epoll_pwait, epfd, output, maximum, timeout, NULL, 8);
    CHECK(0 <= count);
    return count;
}

/// ctl 使用真实 OFD 与整数 fd 的组合键，标记值可以区分旧键与数值复用的新键。
static void watch(int epfd, int operation, int fd, uint32_t events, uint64_t data)
{
    struct epoll_event event = { .events = events, .data.u64 = data };
    CHECK(epoll_ctl(epfd, operation, fd, operation == EPOLL_CTL_DEL ? NULL : &event) == 0);
}

/// LT 短读、ET 新通知、ONESHOT 重 arm 和 dup/fd 复用均消费真实 TCP 字节。
static void epoll_stream(void)
{
    int client, server;
    pair(&client, &server);
    int epfd = epoll_create1(EPOLL_CLOEXEC);
    CHECK(0 <= epfd);
    const uint64_t marker = 0xfedcba9876543210ULL;
    struct epoll_event events[4];
    unsigned char output[16];
    write_all(client, "abcd", 4);
    CHECK(wait_ready(server, POLLIN) & POLLIN);
    watch(epfd, EPOLL_CTL_ADD, server, EPOLLIN, marker);
    CHECK(epoll_events(epfd, events, 4, 0) == 1 && events[0].data.u64 == marker && (events[0].events & EPOLLIN));
    CHECK(read(server, output, 1) == 1 && output[0] == 'a');
    CHECK(epoll_events(epfd, events, 4, 0) == 1);
    read_all(server, output, 3);
    CHECK(memcmp(output, "bcd", 3) == 0);
    CHECK(epoll_events(epfd, events, 4, 0) == 0);
    watch(epfd, EPOLL_CTL_MOD, server, EPOLLIN | EPOLLET, marker);
    write_all(client, "EF", 2);
    CHECK(epoll_events(epfd, events, 4, 5000) == 1);
    CHECK(read(server, output, 1) == 1 && output[0] == 'E');
    CHECK(epoll_events(epfd, events, 4, 0) == 0);
    write_all(client, "GH", 2);
    CHECK(epoll_events(epfd, events, 4, 5000) == 1);
    read_all(server, output, 3);
    CHECK(memcmp(output, "FGH", 3) == 0);
    watch(epfd, EPOLL_CTL_MOD, server, EPOLLIN | EPOLLONESHOT, marker);
    write_all(client, "single!", 7);
    CHECK(epoll_events(epfd, events, 4, 5000) == 1);
    CHECK(epoll_events(epfd, events, 4, 0) == 0);
    watch(epfd, EPOLL_CTL_MOD, server, EPOLLIN | EPOLLONESHOT, marker + 1);
    CHECK(epoll_events(epfd, events, 4, 0) == 1 && events[0].data.u64 == marker + 1);
    read_all(server, output, 7);
    CHECK(memcmp(output, "single!", 7) == 0);
    watch(epfd, EPOLL_CTL_DEL, server, 0, 0);
    write_all(client, "del", 3);
    CHECK(wait_ready(server, POLLIN) & POLLIN);
    CHECK(epoll_events(epfd, events, 4, 0) == 0);
    read_all(server, output, 3);
    CHECK(memcmp(output, "del", 3) == 0);
    int alias = dup(server);
    CHECK(0 <= alias);
    watch(epfd, EPOLL_CTL_ADD, server, EPOLLIN, marker);
    watch(epfd, EPOLL_CTL_ADD, alias, EPOLLIN, marker + 1);
    int old_fd = server;
    CHECK(close(server) == 0);
    int new_client, new_server;
    pair(&new_client, &new_server);
    if (new_server != old_fd) {
        CHECK(dup2(new_server, old_fd) == old_fd);
        CHECK(close(new_server) == 0);
        new_server = old_fd;
    }
    struct epoll_event event = { .events = EPOLLIN };
    ERROR(epoll_ctl(epfd, EPOLL_CTL_MOD, old_fd, &event), ENOENT);
    watch(epfd, EPOLL_CTL_ADD, new_server, EPOLLIN, marker + 2);
    write_all(client, "old", 3);
    write_all(new_client, "new", 3);
    CHECK(wait_ready(alias, POLLIN) & POLLIN);
    CHECK(wait_ready(new_server, POLLIN) & POLLIN);
    CHECK(epoll_events(epfd, events, 4, 0) == 3);
    unsigned seen = 0;
    for (unsigned i = 0; i < 3; ++i) {
        CHECK(marker <= events[i].data.u64 && events[i].data.u64 <= marker + 2);
        unsigned bit = 1u << (unsigned)(events[i].data.u64 - marker);
        CHECK((seen & bit) == 0);
        seen |= bit;
    }
    CHECK(seen == 7);
    read_all(alias, output, 3);
    CHECK(memcmp(output, "old", 3) == 0);
    read_all(new_server, output, 3);
    CHECK(memcmp(output, "new", 3) == 0);
    CHECK(close(alias) == 0);
    CHECK(epoll_events(epfd, events, 4, 0) == 0);
    write_all(new_client, "last", 4);
    CHECK(epoll_events(epfd, events, 4, 5000) == 1 && events[0].data.u64 == marker + 2);
    read_all(new_server, output, 4);
    CHECK(memcmp(output, "last", 4) == 0);
    CHECK(close(new_server) == 0);
    CHECK(epoll_events(epfd, events, 4, 0) == 0);
    CHECK(close(new_client) == 0 && close(client) == 0 && close(epfd) == 0);
}

/// 绑定 UDP 后用实际名称取得自动端口，后续断言比较同条报文的来源。
static int datagram(struct sockaddr_in *local)
{
    int fd = socket(AF_INET, SOCK_DGRAM, 0);
    CHECK(0 <= fd);
    *local = address(0);
    CHECK(bind(fd, (struct sockaddr *)local, sizeof *local) == 0);
    socklen_t length = sizeof *local;
    CHECK(getsockname(fd, (struct sockaddr *)local, &length) == 0);
    CHECK(length == sizeof *local && local->sin_port != 0);
    return fd;
}

/// 等待真实消息就绪后只接收一次，完整比较消息字节与同条记录的发送者。
static void datagram_expect(int fd, const char *text, const struct sockaddr_in *sender)
{
    unsigned char bytes[64];
    struct sockaddr_in source = {0};
    socklen_t length = sizeof source;
    size_t size = strlen(text);
    CHECK(wait_ready(fd, POLLIN) & POLLIN);
    CHECK(recvfrom(fd, bytes, sizeof bytes, 0, (struct sockaddr *)&source, &length) == (ssize_t)size);
    CHECK(memcmp(bytes, text, size) == 0 && length == sizeof source);
    CHECK(source.sin_family == AF_INET && source.sin_port == sender->sin_port);
    CHECK(source.sin_addr.s_addr == sender->sin_addr.s_addr);
}

/// 每消息目的不改默认 peer，connected 过滤与显式、自动绑定断 peer 各核正常后继。
static void udp_address(void)
{
    struct sockaddr_in a, b, local;
    int first = datagram(&a), second = datagram(&b), fd = datagram(&local);
    ERROR(send(fd, "x", 1, 0), EDESTADDRREQ);
    CHECK(sendto(fd, "one", 3, 0, (struct sockaddr *)&a, sizeof a) == 3);
    datagram_expect(first, "one", &local);
    CHECK(sendto(fd, "two", 3, 0, (struct sockaddr *)&b, sizeof b) == 3);
    datagram_expect(second, "two", &local);
    CHECK(connect(fd, (struct sockaddr *)&a, sizeof a) == 0);
    CHECK(sendto(fd, "override", 8, 0, (struct sockaddr *)&b, sizeof b) == 8);
    datagram_expect(second, "override", &local);
    CHECK(send(fd, "default", 7, 0) == 7);
    datagram_expect(first, "default", &local);
    struct sockaddr_in peer;
    socklen_t length = sizeof peer;
    CHECK(getpeername(fd, (struct sockaddr *)&peer, &length) == 0);
    CHECK(length == sizeof peer && peer.sin_port == a.sin_port && peer.sin_addr.s_addr == a.sin_addr.s_addr);
    CHECK(sendto(second, "filtered", 8, 0, (struct sockaddr *)&local, sizeof local) == 8);
    CHECK(sendto(first, "accepted", 8, 0, (struct sockaddr *)&local, sizeof local) == 8);
    datagram_expect(fd, "accepted", &a);
    unsigned char byte;
    ERROR(recv(fd, &byte, 1, MSG_DONTWAIT), EAGAIN);
    struct sockaddr disconnect = {.sa_family = AF_UNSPEC};
    CHECK(connect(fd, &disconnect, sizeof disconnect.sa_family) == 0);
    length = sizeof peer;
    ERROR(getpeername(fd, (struct sockaddr *)&peer, &length), ENOTCONN);
    CHECK(getsockname(fd, (struct sockaddr *)&peer, &length) == 0 && peer.sin_port == local.sin_port);
    CHECK(sendto(second, "unfiltered", 10, 0, (struct sockaddr *)&local, sizeof local) == 10);
    datagram_expect(fd, "unfiltered", &b);
    CHECK(close(fd) == 0);
    fd = socket(AF_INET, SOCK_DGRAM, 0);
    CHECK(0 <= fd && connect(fd, (struct sockaddr *)&a, sizeof a) == 0);
    length = sizeof local;
    CHECK(getsockname(fd, (struct sockaddr *)&local, &length) == 0 && local.sin_port != 0);
    CHECK(send(fd, "auto", 4, 0) == 4);
    datagram_expect(first, "auto", &local);
    CHECK(connect(fd, &disconnect, sizeof disconnect.sa_family) == 0);
    length = sizeof peer;
    CHECK(getsockname(fd, (struct sockaddr *)&peer, &length) == 0);
    CHECK(peer.sin_port == 0 && peer.sin_addr.s_addr == 0);
    int reuse = socket(AF_INET, SOCK_DGRAM, 0);
    CHECK(0 <= reuse && bind(reuse, (struct sockaddr *)&local, sizeof local) == 0);
    CHECK(sendto(fd, "after disconnect", 16, 0, (struct sockaddr *)&b, sizeof b) == 16);
    length = sizeof peer;
    CHECK(getsockname(fd, (struct sockaddr *)&peer, &length) == 0 && peer.sin_port != 0);
    CHECK(peer.sin_addr.s_addr == 0);
    // 未连接自动绑定名称保通配，报文来源必须是本次路由选出的环回地址。
    peer.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    datagram_expect(second, "after disconnect", &peer);
    CHECK(close(reuse) == 0 && close(fd) == 0 && close(first) == 0 && close(second) == 0);
}

/// 零长消息仍占记录，read0 与 recv0 消费不同；短尾、PEEK 和 TRUNC 都比较后继消息。
static void udp_message(void)
{
    struct sockaddr_in sender, destination;
    int fd = datagram(&sender), receiver = datagram(&destination);
    CHECK(connect(fd, (struct sockaddr *)&destination, sizeof destination) == 0);
    struct iovec empty[2] = {{(void *)1, 0}, {(void *)1, 0}};
    struct msghdr message = {.msg_iov = empty, .msg_iovlen = 2};
    CHECK(sendto(fd, NULL, 0, 0, (struct sockaddr *)&destination, sizeof destination) == 0);
    CHECK(sendmsg(fd, &message, 0) == 0);
    CHECK(writev(fd, empty, 2) == 0);
    CHECK(send(fd, "after zeros", 11, 0) == 11);
    CHECK(wait_ready(receiver, POLLIN) & POLLIN);
    CHECK(read(receiver, (void *)1, 0) == 0);
    for (unsigned i = 0; i < 3; ++i) datagram_expect(receiver, "", &sender);
    datagram_expect(receiver, "after zeros", &sender);
    CHECK(send(fd, "discard", 7, 0) == 7 && send(fd, "kept", 4, 0) == 4);
    CHECK(recv(receiver, (void *)1, 0, 0) == 0);
    datagram_expect(receiver, "kept", &sender);
    unsigned char output[32];
    CHECK(send(fd, "short-tail", 10, 0) == 10 && send(fd, "next", 4, 0) == 4);
    CHECK(recv(receiver, output, 3, 0) == 3 && memcmp(output, "sho", 3) == 0);
    datagram_expect(receiver, "next", &sender);
    CHECK(send(fd, "peek-full", 9, 0) == 9);
    CHECK(recv(receiver, output, 4, MSG_PEEK) == 4 && memcmp(output, "peek", 4) == 0);
    CHECK(recv(receiver, output, 4, MSG_PEEK | MSG_TRUNC) == 9 && memcmp(output, "peek", 4) == 0);
    datagram_expect(receiver, "peek-full", &sender);
    CHECK(send(fd, "truncated", 9, 0) == 9 && send(fd, "following", 9, 0) == 9);
    struct sockaddr_in source;
    socklen_t length = sizeof source;
    CHECK(recvfrom(receiver, output, 3, MSG_TRUNC, (struct sockaddr *)&source, &length) == 9);
    CHECK(memcmp(output, "tru", 3) == 0 && length == sizeof source && source.sin_port == sender.sin_port);
    datagram_expect(receiver, "following", &sender);
    unsigned char large[1473];
    for (size_t i = 0; i < sizeof large; ++i) large[i] = (unsigned char)(i * 29u + 7u);
    ERROR(send(fd, large, sizeof large, 0), EMSGSIZE);
    CHECK(send(fd, large, sizeof large - 1, 0) == sizeof large - 1);
    unsigned char received[1472];
    CHECK(recv(receiver, received, sizeof received, 0) == sizeof received);
    CHECK(memcmp(received, large, sizeof received) == 0);
    install_handler(SIGPIPE, 0);
    CHECK(shutdown(fd, SHUT_WR) == 0);
    ERROR(send(fd, NULL, 0, 0), EPIPE);
    CHECK(signals_seen == 0);
    CHECK(sendto(receiver, "reply", 5, 0, (struct sockaddr *)&sender, sizeof sender) == 5);
    datagram_expect(fd, "reply", &destination);
    CHECK(close(fd) == 0 && close(receiver) == 0);
}

/// 两槽必须同时准备；满表与 fd 数组 fault 后，原最小空槽仍可正常 dup 及建 pair。
static void unix_slots(int base)
{
    int copies[128], result[2] = {-1, -1};
    size_t count = 0;
    for (;;) {
        int fd = dup(base);
        if (fd == -1) { CHECK(errno == EMFILE); break; }
        CHECK(count < 128);
        copies[count++] = fd;
    }
    ERROR(socketpair(AF_UNIX, SOCK_STREAM, 0, result), EMFILE);
    CHECK(result[0] == -1 && result[1] == -1 && 2 <= count);
    int last = copies[--count];
    CHECK(close(last) == 0);
    ERROR(socketpair(AF_UNIX, SOCK_STREAM, 0, result), EMFILE);
    CHECK(dup(base) == last);
    CHECK(close(last) == 0);
    int other = copies[--count];
    CHECK(close(other) == 0);
    unsigned char *mapping = mmap(NULL, 8192, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    CHECK(mapping != MAP_FAILED && munmap(mapping + 4096, 4096) == 0);
    ERROR(syscall(SYS_socketpair, AF_UNIX, SOCK_STREAM, 0, mapping + 4093), EFAULT);
    int lower = other < last ? other : last, upper = other < last ? last : other;
    CHECK(dup(base) == lower && dup(base) == upper);
    CHECK(close(lower) == 0 && close(upper) == 0);
    CHECK(socketpair(AF_UNIX, SOCK_STREAM, 0, result) == 0);
    CHECK(result[0] == lower && result[1] == upper);
    write_all(result[0], "slots survived", 14);
    unsigned char output[14];
    read_all(result[1], output, sizeof output);
    CHECK(memcmp(output, "slots survived", sizeof output) == 0);
    CHECK(close(result[0]) == 0 && close(result[1]) == 0 && munmap(mapping, 4096) == 0);
    while (0 < count) CHECK(close(copies[--count]) == 0);
}

/// 双向全文超过环容量并回绕；dup/fork 共享身份，半关闭、零 EPIPE 和未读末关闭保真实错误。
static void unix_pair(void)
{
    int fds[2];
    CHECK(socketpair(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0, fds) == 0);
    int alias = dup(fds[0]);
    CHECK(0 <= alias);
    struct stat first, duplicate, peer;
    CHECK(fstat(fds[0], &first) == 0 && fstat(alias, &duplicate) == 0 && fstat(fds[1], &peer) == 0);
    CHECK(S_ISSOCK(first.st_mode) && first.st_ino == duplicate.st_ino && first.st_ino != peer.st_ino);
    CHECK(fcntl(fds[0], F_GETFD) == FD_CLOEXEC && fcntl(alias, F_GETFD) == 0);
    struct sockaddr name;
    socklen_t length = sizeof name;
    CHECK(getsockname(alias, &name, &length) == 0 && length == 2 && name.sa_family == AF_UNIX);
    length = sizeof name;
    CHECK(getpeername(alias, &name, &length) == 0 && length == 2 && name.sa_family == AF_UNIX);
    unix_slots(alias);
    unsigned char input[40001], output[40001];
    for (size_t i = 0; i < sizeof input; ++i)
        input[i] = (unsigned char)(i * 43u + (i >> 8) * 19u + (i >> 12) * 7u + 19u);
    int notice[2];
    CHECK(pipe(notice) == 0);
    pid_t child = fork_child();
    if (child == 0) {
        CHECK(close(notice[0]) == 0 && close(fds[0]) == 0);
        CHECK(fcntl(alias, F_SETFL, O_NONBLOCK) == 0);
        write_all(notice[1], "shared", 6);
        CHECK(close(alias) == 0 && close(notice[1]) == 0);
        report('U', getpid(), 0);
        write_all(fds[1], input, sizeof input);
        read_all(fds[1], output, sizeof output);
        CHECK(memcmp(output, input, sizeof input) == 0);
        CHECK(shutdown(fds[1], SHUT_WR) == 0);
        CHECK(read(fds[1], output, 1) == 0);
        CHECK(close(fds[1]) == 0);
        _exit(0);
    }
    CHECK(close(notice[1]) == 0 && close(fds[1]) == 0);
    read_all(notice[0], output, 6);
    CHECK(memcmp(output, "shared", 6) == 0 && (fcntl(fds[0], F_GETFL) & O_NONBLOCK) != 0);
    CHECK(close(notice[0]) == 0 && fcntl(alias, F_SETFL, 0) == 0);
    proceed();
    read_all(alias, output, sizeof output);
    CHECK(memcmp(output, input, sizeof input) == 0);
    write_all(alias, input, sizeof input);
    CHECK(read(alias, output, 1) == 0 && shutdown(alias, SHUT_WR) == 0);
    install_handler(SIGPIPE, 0);
    ERROR(send(alias, NULL, 0, MSG_NOSIGNAL), EPIPE);
    CHECK(signals_seen == 0);
    ERROR(send(alias, NULL, 0, 0), EPIPE);
    CHECK(signals_seen == 1);
    reap(child, 0);
    CHECK(close(alias) == 0 && close(fds[0]) == 0);
    CHECK(socketpair(AF_UNIX, SOCK_STREAM, 0, fds) == 0);
    CHECK(shutdown(fds[0], SHUT_WR) == 0);
    write_all(fds[1], "still receives", 14);
    read_all(fds[0], output, 14);
    CHECK(memcmp(output, "still receives", 14) == 0);
    CHECK(close(fds[0]) == 0 && close(fds[1]) == 0);
    CHECK(socketpair(AF_UNIX, SOCK_STREAM, 0, fds) == 0);
    write_all(fds[1], "unread", 6);
    write_all(fds[0], "before reset", 12);
    CHECK(close(fds[0]) == 0);
    read_all(fds[1], output, 12);
    CHECK(memcmp(output, "before reset", 12) == 0);
    CHECK(socket_error(fds[1]) == ECONNRESET && socket_error(fds[1]) == 0);
    ERROR(send(fds[1], NULL, 0, MSG_NOSIGNAL), EPIPE);
    CHECK(close(fds[1]) == 0);
}

/// 只允许 recvmsg 输出已选字段，其余头字节及 padding 保持原用户值。
static void unchanged_header(const struct msghdr *before, const struct msghdr *after, int named)
{
    const unsigned char *a = (const unsigned char *)before, *b = (const unsigned char *)after;
    for (size_t i = 0; i < sizeof *before; ++i) {
        if ((named && 8 <= i && i < 12) || (40 <= i && i < 48) || (48 <= i && i < 52)) continue;
        CHECK(a[i] == b[i]);
    }
}

/// 向量按一次消息消费，真实窗口限制和后段用户 fault 不得把准备字节计为接受字节。
static void message_vector(void)
{
    int fds[2];
    CHECK(socketpair(AF_UNIX, SOCK_STREAM | SOCK_NONBLOCK, 0, fds) == 0);
    unsigned char fill[16381], output[16384];
    memset(fill, 0x39, sizeof fill);
    write_all(fds[0], fill, sizeof fill);
    struct iovec input[] = {{(void *)"abcdef", 6}, {(void *)1, 8}};
    struct msghdr message = {.msg_iov = input, .msg_iovlen = 2, .msg_flags = MSG_OOB};
    CHECK(sendmsg(fds[0], &message, MSG_NOSIGNAL) == 3);
    read_all(fds[1], output, sizeof output);
    CHECK(memcmp(output, fill, sizeof fill) == 0 && memcmp(output + sizeof fill, "abc", 3) == 0);
    input[0] = (struct iovec){(void *)"prefix!", 7};
    CHECK(sendmsg(fds[0], &message, MSG_NOSIGNAL) == 7);
    read_all(fds[1], output, 7);
    CHECK(memcmp(output, "prefix!", 7) == 0);
    ERROR(recv(fds[1], output, 1, MSG_DONTWAIT), EAGAIN);
    input[0] = (struct iovec){(void *)1, 1};
    ERROR(sendmsg(fds[0], &message, MSG_NOSIGNAL), EFAULT);
    input[0] = (struct iovec){(void *)"cross-", 6};
    input[1] = (struct iovec){(void *)"iov-full", 8};
    CHECK(sendmsg(fds[0], &message, 0) == 14);
    unsigned char first[3], second[11];
    struct iovec target[] = {{first, 3}, {second, 11}};
    struct msghdr header;
    memset(&header, 0xa5, sizeof header);
    header.msg_name = NULL;
    header.msg_namelen = (socklen_t)-1;
    header.msg_iov = target;
    header.msg_iovlen = 2;
    header.msg_control = (void *)1;
    header.msg_controllen = 0;
    // musl 会清长度高位并整头复制返回；保留头字节的断言直接观察 raw syscall。
    // 内核两长度为 LP64 unsigned long，完整初始化八字节，真正 padding 仍保留哨兵。
    const uint64_t kernel_iovlen = 2, kernel_controllen = 0;
    memcpy((unsigned char *)&header + offsetof(struct msghdr, msg_iovlen), &kernel_iovlen, sizeof kernel_iovlen);
    memcpy((unsigned char *)&header + offsetof(struct msghdr, msg_controllen), &kernel_controllen, sizeof kernel_controllen);
    struct msghdr saved;
    memcpy(&saved, &header, sizeof saved);
    CHECK(syscall(SYS_recvmsg, fds[1], &header, 0) == 14);
    CHECK(memcmp(first, "cro", 3) == 0 && memcmp(second, "ss-iov-full", 11) == 0);
    CHECK(header.msg_flags == 0 && header.msg_controllen == 0);
    unchanged_header(&saved, &header, 0);
    write_all(fds[0], "read-fault", 10);
    target[0] = (struct iovec){output, 4};
    target[1] = (struct iovec){(void *)1, 6};
    CHECK(recvmsg(fds[1], &header, 0) == 4 && memcmp(output, "read", 4) == 0);
    read_all(fds[1], output, 6);
    CHECK(memcmp(output, "-fault", 6) == 0);
    CHECK(close(fds[0]) == 0 && close(fds[1]) == 0);
    struct sockaddr_in local, sender, source;
    int fd = datagram(&sender), receiver = datagram(&local);
    input[0] = (struct iovec){(void *)"vector-", 7};
    input[1] = (struct iovec){(void *)"datagram", 8};
    message.msg_name = &local;
    message.msg_namelen = sizeof local;
    CHECK(sendmsg(fd, &message, 0) == 15);
    target[0] = (struct iovec){first, 3};
    target[1] = (struct iovec){second, 5};
    header.msg_name = &source;
    header.msg_namelen = sizeof source;
    memcpy(&saved, &header, sizeof saved);
    CHECK(syscall(SYS_recvmsg, receiver, &header, MSG_PEEK | MSG_TRUNC) == 15);
    CHECK(header.msg_flags == MSG_TRUNC && header.msg_namelen == sizeof source && header.msg_controllen == 0);
    CHECK(source.sin_port == sender.sin_port && source.sin_addr.s_addr == sender.sin_addr.s_addr);
    CHECK(memcmp(first, "vec", 3) == 0 && memcmp(second, "tor-d", 5) == 0);
    unchanged_header(&saved, &header, 1);
    datagram_expect(receiver, "vector-datagram", &sender);
    CHECK(sendmsg(fd, &message, 0) == 15);
    CHECK(recvmsg(receiver, &header, 0) == 8);
    CHECK(header.msg_flags == MSG_TRUNC);
    CHECK(memcmp(first, "vec", 3) == 0 && memcmp(second, "tor-d", 5) == 0);
    ERROR(recv(receiver, output, sizeof output, MSG_DONTWAIT), EAGAIN);
    input[1] = (struct iovec){(void *)1, 8};
    ERROR(sendmsg(fd, &message, 0), EFAULT);
    ERROR(recv(receiver, output, sizeof output, MSG_DONTWAIT), EAGAIN);
    input[1] = (struct iovec){(void *)"datagram", 8};
    CHECK(sendmsg(fd, &message, 0) == 15);
    target[1] = (struct iovec){(void *)1, 5};
    ERROR(recvmsg(receiver, &header, MSG_PEEK), EFAULT);
    CHECK(memcmp(first, "vec", 3) == 0);
    ERROR(recvmsg(receiver, &header, 0), EFAULT);
    ERROR(recv(receiver, output, sizeof output, MSG_DONTWAIT), EAGAIN);
    target[1] = (struct iovec){second, 5};
    // libc 发送前会复制 control；非空控制不支持检查必须提供真实可读输入。
    message.msg_control = output;
    message.msg_controllen = 1;
    ERROR(sendmsg(fd, &message, 0), EOPNOTSUPP);
    message.msg_controllen = 0;
    message.msg_iovlen = 1025;
    ERROR(sendmsg(fd, &message, 0), EMSGSIZE);
    message.msg_iovlen = 2;
    CHECK(sendmsg(fd, &message, 0) == 15);
    header.msg_name = (void *)1;
    header.msg_namelen = sizeof source;
    ERROR(recvmsg(receiver, &header, 0), EFAULT);
    ERROR(recv(receiver, output, sizeof output, MSG_DONTWAIT), EAGAIN);
    CHECK(sendto(fd, "healthy", 7, 0, (struct sockaddr *)&local, sizeof local) == 7);
    datagram_expect(receiver, "healthy", &sender);
    CHECK(close(fd) == 0 && close(receiver) == 0);
}

/// int 选项通过实际 syscall 查询，不用本地保存的设置值充当内核事实。
static int option(int fd, int level, int name)
{
    int value = -1;
    socklen_t length = sizeof value;
    CHECK(getsockopt(fd, level, name, &value, &length) == 0 && length == sizeof value);
    return value;
}

/// 双方 reuse 才允许重叠，listener 仍互斥，UDP 查找真选 connected，TCP child 继承独立选项。
static void socket_options(void)
{
    int enabled = -7, disabled = 0;
    struct sockaddr_in local;
    int first = datagram(&local), second = socket(AF_INET, SOCK_DGRAM | SOCK_NONBLOCK, 0);
    CHECK(0 <= second);
    CHECK(setsockopt(second, SOL_SOCKET, SO_REUSEADDR, &enabled, sizeof enabled) == 0);
    ERROR(bind(second, (struct sockaddr *)&local, sizeof local), EADDRINUSE);
    CHECK(setsockopt(first, SOL_SOCKET, SO_REUSEADDR, &enabled, sizeof enabled) == 0);
    CHECK(bind(second, (struct sockaddr *)&local, sizeof local) == 0);
    CHECK(option(first, SOL_SOCKET, SO_REUSEADDR) == 1 && option(second, SOL_SOCKET, SO_REUSEADDR) == 1);
    struct sockaddr_in sender;
    int fd = datagram(&sender);
    CHECK(connect(first, (struct sockaddr *)&sender, sizeof sender) == 0);
    CHECK(sendto(fd, "connected wins", 14, 0, (struct sockaddr *)&local, sizeof local) == 14);
    datagram_expect(first, "connected wins", &sender);
    unsigned char output[32];
    ERROR(recv(second, output, sizeof output, MSG_DONTWAIT), EAGAIN);
    struct sockaddr disconnect = {.sa_family = AF_UNSPEC};
    CHECK(connect(first, &disconnect, 2) == 0);
    CHECK(sendto(fd, "recent wins", 11, 0, (struct sockaddr *)&local, sizeof local) == 11);
    datagram_expect(first, "recent wins", &sender);
    ERROR(recv(second, output, sizeof output, MSG_DONTWAIT), EAGAIN);
    CHECK(close(first) == 0);
    CHECK(sendto(fd, "overlap survivor", 16, 0, (struct sockaddr *)&local, sizeof local) == 16);
    datagram_expect(second, "overlap survivor", &sender);
    ERROR(setsockopt(second, IPPROTO_TCP, TCP_NODELAY, &enabled, sizeof enabled), ENOPROTOOPT);
    CHECK(close(fd) == 0 && close(second) == 0);
    first = socket(AF_INET, SOCK_STREAM, 0);
    second = socket(AF_INET, SOCK_STREAM, 0);
    CHECK(0 <= first && 0 <= second);
    CHECK(setsockopt(first, SOL_SOCKET, SO_REUSEADDR, &enabled, sizeof enabled) == 0);
    local = address(0);
    CHECK(bind(first, (struct sockaddr *)&local, sizeof local) == 0);
    socklen_t length = sizeof local;
    CHECK(getsockname(first, (struct sockaddr *)&local, &length) == 0);
    ERROR(bind(second, (struct sockaddr *)&local, sizeof local), EADDRINUSE);
    CHECK(setsockopt(second, SOL_SOCKET, SO_REUSEADDR, &enabled, sizeof enabled) == 0);
    CHECK(bind(second, (struct sockaddr *)&local, sizeof local) == 0);
    CHECK(option(first, IPPROTO_TCP, TCP_NODELAY) == 0);
    CHECK(setsockopt(first, IPPROTO_TCP, TCP_NODELAY, &enabled, sizeof enabled) == 0);
    CHECK(option(first, IPPROTO_TCP, TCP_NODELAY) == 1);
    CHECK(listen(first, 4) == 0);
    ERROR(listen(second, 4), EADDRINUSE);
    CHECK(close(second) == 0);
    second = socket(AF_INET, SOCK_STREAM, 0);
    CHECK(0 <= second);
    CHECK(setsockopt(second, SOL_SOCKET, SO_REUSEADDR, &enabled, sizeof enabled) == 0);
    ERROR(bind(second, (struct sockaddr *)&local, sizeof local), EADDRINUSE);
    CHECK(close(second) == 0);
    fd = socket(AF_INET, SOCK_STREAM, 0);
    CHECK(0 <= fd && connect(fd, (struct sockaddr *)&local, sizeof local) == 0);
    int child = accept(first, NULL, NULL);
    CHECK(0 <= child && option(child, IPPROTO_TCP, TCP_NODELAY) == 1);
    CHECK(option(child, SOL_SOCKET, SO_REUSEADDR) == 1);
    CHECK(setsockopt(child, IPPROTO_TCP, TCP_NODELAY, &disabled, sizeof disabled) == 0);
    CHECK(option(child, IPPROTO_TCP, TCP_NODELAY) == 0 && option(first, IPPROTO_TCP, TCP_NODELAY) == 1);
    CHECK(setsockopt(child, SOL_SOCKET, SO_REUSEADDR, &disabled, sizeof disabled) == 0);
    CHECK(option(first, SOL_SOCKET, SO_REUSEADDR) == 1);
    write_all(fd, "option child", 12);
    read_all(child, output, 12);
    CHECK(memcmp(output, "option child", 12) == 0);
    CHECK(close(fd) == 0 && close(child) == 0 && close(first) == 0);
}

/// 普通 argv 描述符完整解析，非法字符串不能默认为标准流。
static int descriptor(const char *text)
{
    char *end;
    errno = 0;
    long value = strtol(text, &end, 10);
    CHECK(errno == 0 && *text && *end == '\0' && 0 <= value && value < 128);
    return (int)value;
}

/// 正常 CRT 调用 main，未知模式失败，各模式均以真实 syscall 和退出状态验收。
int main(int argc, char **argv)
{
    CHECK(2 <= argc && argv[argc] == NULL && argv[0][0] == '/');
    if (strcmp(argv[1], "exec-identity") == 0) return exec_identity(argc, argv);
    if (argc == 4) { ready_fd = descriptor(argv[2]); go_fd = descriptor(argv[3]); }
    else CHECK(argc == 2);
    if (strcmp(argv[1], "identity") == 0) identity(argv[0]);
    else if (strcmp(argv[1], "listener") == 0) multiple_accept();
    else if (strcmp(argv[1], "connect") == 0) nonblock_connect();
    else if (strcmp(argv[1], "stream") == 0) stream();
    else if (strcmp(argv[1], "backpressure") == 0) backpressure();
    else if (strcmp(argv[1], "half-close") == 0) half_close();
    else if (strcmp(argv[1], "address") == 0) address_faults();
    else if (strcmp(argv[1], "signal") == 0) interrupted();
    else if (strcmp(argv[1], "epoll") == 0) epoll_stream();
    else if (strcmp(argv[1], "udp-address") == 0) udp_address();
    else if (strcmp(argv[1], "udp-message") == 0) udp_message();
    else if (strcmp(argv[1], "unix-pair") == 0) unix_pair();
    else if (strcmp(argv[1], "message-vector") == 0) message_vector();
    else if (strcmp(argv[1], "socket-options") == 0) socket_options();
    else fail("unknown mode", __LINE__);
    if (strcmp(argv[1], "udp-address") == 0 || strcmp(argv[1], "udp-message") == 0
        || strcmp(argv[1], "unix-pair") == 0 || strcmp(argv[1], "message-vector") == 0
        || strcmp(argv[1], "socket-options") == 0) {
        report('4', getpid(), 0);
        proceed();
    }
    if (0 <= ready_fd) CHECK(close(ready_fd) == 0 && close(go_fd) == 0);
    CHECK(child_count == 0);
    printf("network-smoke mode=%s passed\n", argv[1]);
    return 0;
}
