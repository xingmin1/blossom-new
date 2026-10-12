#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <netinet/tcp.h>
#include <net/if.h>
#include <net/route.h>
#include <fcntl.h>
#include <signal.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/ioctl.h>
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
_Static_assert(sizeof(struct sockaddr_in6) == 28, "sockaddr_in6 ABI");
_Static_assert(offsetof(struct sockaddr_in6, sin6_family) == 0, "IPv6 family ABI");
_Static_assert(offsetof(struct sockaddr_in6, sin6_port) == 2, "IPv6 port ABI");
_Static_assert(offsetof(struct sockaddr_in6, sin6_flowinfo) == 4, "IPv6 flowinfo ABI");
_Static_assert(offsetof(struct sockaddr_in6, sin6_addr) == 8, "IPv6 address ABI");
_Static_assert(offsetof(struct sockaddr_in6, sin6_scope_id) == 24, "IPv6 scope ABI");
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
static int control_fd = -1, control_route_deleted, control_lo_changed;
static struct ifreq control_lo;
static struct rtentry control_default;

/// 每次实际修改只尝试一次恢复，恢复失败按原用户退出错误报告。
static int restore_control(void)
{
    int result = 0;
    if (control_fd < 0) return 0;
    if (control_route_deleted) {
        control_route_deleted = 0;
        if (ioctl(control_fd, SIOCADDRT, &control_default) == -1) {
            fprintf(stderr, "network-smoke cleanup route errno=%d\n", errno);
            result = -1;
        }
    }
    if (control_lo_changed) {
        control_lo_changed = 0;
        if (ioctl(control_fd, SIOCSIFFLAGS, &control_lo) == -1) {
            fprintf(stderr, "network-smoke cleanup lo flags errno=%d\n", errno);
            result = -1;
        }
    }
    return result;
}

/// 失败时恢复本例配置并终止、回收已发表的孩子，描述符由正常退出释放。
static void fail(const char *expression, int line)
{
    int saved = errno;
    fprintf(stderr, "network-smoke line=%d check=%s errno=%d\n", line, expression, saved);
    (void)restore_control();
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
        const int expected[] = {SOCK_STREAM, AF_INET, IPPROTO_TCP, 0, 131072, 131072};
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
    unsigned char large[2049], received[2049];
    for (size_t i = 0; i < sizeof large; ++i) large[i] = (unsigned char)(i * 29u + 7u);
    CHECK(send(fd, large, sizeof large, 0) == sizeof large);
    length = sizeof source;
    CHECK(recvfrom(receiver, received, sizeof received, 0, (struct sockaddr *)&source, &length) == sizeof received);
    CHECK(memcmp(received, large, sizeof received) == 0);
    CHECK(length == sizeof source && source.sin_family == AF_INET);
    CHECK(source.sin_port == sender.sin_port && source.sin_addr.s_addr == sender.sin_addr.s_addr);
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

/// 基础 IPv6 地址只使用真实 sockaddr_in6 的 ::1，flowinfo 与 scope 保零。
static struct sockaddr_in6 ipv6_address(void)
{
    struct sockaddr_in6 value = { .sin6_family = AF_INET6,
        .sin6_addr = IN6ADDR_LOOPBACK_INIT };
    return value;
}

/// 名称与消息来源必须为完整 ::1 端点；零端口不能冒充已经绑定的名称。
static void ipv6_name(const struct sockaddr_in6 *actual, socklen_t length, uint16_t port)
{
    const struct in6_addr loopback = IN6ADDR_LOOPBACK_INIT;
    CHECK(length == 28 && actual->sin6_family == AF_INET6);
    CHECK(memcmp(&actual->sin6_addr, &loopback, sizeof loopback) == 0);
    CHECK(actual->sin6_flowinfo == 0 && actual->sin6_scope_id == 0);
    CHECK(actual->sin6_port != 0);
    if (port != 0) CHECK(actual->sin6_port == port);
}

/// 域、类型、协议及 socket 属性均查询正式 OFD，不从本地创建参数推定。
static void ipv6_identity(int fd, int type)
{
    CHECK(option(fd, SOL_SOCKET, SO_DOMAIN) == 10);
    CHECK(option(fd, SOL_SOCKET, SO_TYPE) == type);
    CHECK(option(fd, SOL_SOCKET, SO_PROTOCOL) == (type == SOCK_STREAM ? 6 : 17));
    struct stat stat;
    CHECK(fstat(fd, &stat) == 0 && S_ISSOCK(stat.st_mode));
}

/// 正式名称查询还核四字节容量的真实前缀，未容纳区域必须保留原 guard。
static struct sockaddr_in6 ipv6_query(int fd, int peer, uint16_t port)
{
    struct sockaddr_in6 actual = {0};
    socklen_t length = sizeof actual;
    CHECK((peer ? getpeername(fd, (struct sockaddr *)&actual, &length)
        : getsockname(fd, (struct sockaddr *)&actual, &length)) == 0);
    ipv6_name(&actual, length, port);
    unsigned char short_name[36];
    memset(short_name, 0xa5, sizeof short_name);
    length = 4;
    CHECK((peer ? getpeername(fd, (struct sockaddr *)short_name, &length)
        : getsockname(fd, (struct sockaddr *)short_name, &length)) == 0);
    CHECK(length == 28 && memcmp(short_name, &actual, 4) == 0);
    for (size_t i = 4; i < sizeof short_name; ++i) CHECK(short_name[i] == 0xa5);
    return actual;
}

/// port0 绑定经正式 ABI 分配端口，短输入和异族输入失败后仍能正常绑定。
static int ipv6_bound(int type, struct sockaddr_in6 *local)
{
    int fd = socket(AF_INET6, type, 0);
    CHECK(0 <= fd);
    ipv6_identity(fd, type);
    *local = ipv6_address();
    ERROR(bind(fd, (struct sockaddr *)local, 27), EINVAL);
    struct sockaddr_in foreign = address(0);
    ERROR(bind(fd, (struct sockaddr *)&foreign, sizeof foreign), EAFNOSUPPORT);
    CHECK(bind(fd, (struct sockaddr *)local, sizeof *local) == 0);
    *local = ipv6_query(fd, 0, 0);
    return fd;
}

/// PEEK 的真实短前缀与后继完整读取共用相同图样，随后必须观察数据后的 EOF。
static void ipv6_stream_expect(int fd, const unsigned char *expected, size_t size)
{
    unsigned char peeked[31], output[16397];
    CHECK(size <= sizeof output);
    CHECK(wait_ready(fd, POLLIN) & POLLIN);
    ssize_t count = recv(fd, peeked, sizeof peeked, MSG_PEEK);
    CHECK(0 < count && (size_t)count <= sizeof peeked && (size_t)count <= size);
    CHECK(memcmp(peeked, expected, (size_t)count) == 0);
    read_all(fd, output, size);
    CHECK(memcmp(output, expected, size) == 0);
    CHECK(read(fd, output, 1) == 0);
}

/// 真实 fork/connect/accept 建立新 IPv6 OFD，半关闭后仍能交付反向全文并正常 reap。
static void ipv6_tcp(void)
{
    struct sockaddr_in6 local;
    int listening = ipv6_bound(SOCK_STREAM, &local);
    CHECK(fcntl(listening, F_SETFL, O_NONBLOCK) == 0);
    CHECK(listen(listening, 8) == 0);
    unsigned char request[16397], response[8197];
    for (size_t i = 0; i < sizeof request; ++i) request[i] = pattern(i);
    for (size_t i = 0; i < sizeof response; ++i) response[i] = pattern(i) ^ 0xa5;
    pid_t child = fork_child();
    if (child == 0) {
        CHECK(close(listening) == 0);
        int client = socket(AF_INET6, SOCK_STREAM, IPPROTO_TCP);
        CHECK(0 <= client);
        ipv6_identity(client, SOCK_STREAM);
        ERROR(connect(client, (struct sockaddr *)&local, 27), EINVAL);
        CHECK(connect(client, (struct sockaddr *)&local, sizeof local) == 0);
        struct sockaddr_in6 own = ipv6_query(client, 0, 0);
        ipv6_query(client, 1, local.sin6_port);
        write_all(client, &own.sin6_port, sizeof own.sin6_port);
        write_all(client, request, sizeof request);
        CHECK(shutdown(client, SHUT_WR) == 0);
        ipv6_stream_expect(client, response, sizeof response);
        CHECK(close(client) == 0);
        _exit(0);
    }
    CHECK(wait_ready(listening, POLLIN) & POLLIN);
    struct sockaddr_in6 accepted = {0};
    socklen_t length = sizeof accepted;
    int server = accept4(listening, (struct sockaddr *)&accepted, &length, SOCK_CLOEXEC);
    CHECK(0 <= server);
    ipv6_name(&accepted, length, 0);
    ipv6_identity(server, SOCK_STREAM);
    ipv6_query(server, 0, local.sin6_port);
    ipv6_query(server, 1, accepted.sin6_port);
    struct stat listener_stat, server_stat, alias_stat;
    CHECK(fstat(listening, &listener_stat) == 0 && fstat(server, &server_stat) == 0);
    CHECK(server_stat.st_ino != listener_stat.st_ino);
    CHECK((fcntl(server, F_GETFL) & O_NONBLOCK) == 0);
    CHECK((fcntl(listening, F_GETFL) & O_NONBLOCK) != 0);
    int alias = dup(server);
    CHECK(0 <= alias && alias != server);
    CHECK(fstat(alias, &alias_stat) == 0 && alias_stat.st_ino == server_stat.st_ino);
    CHECK(alias_stat.st_dev == server_stat.st_dev);
    CHECK(fcntl(server, F_GETFD) == FD_CLOEXEC && fcntl(alias, F_GETFD) == 0);
    CHECK(fcntl(alias, F_SETFL, O_NONBLOCK) == 0);
    CHECK((fcntl(server, F_GETFL) & O_NONBLOCK) != 0);
    CHECK(fcntl(server, F_SETFL, 0) == 0);
    CHECK((fcntl(alias, F_GETFL) & O_NONBLOCK) == 0);
    uint16_t client_port;
    read_all(alias, &client_port, sizeof client_port);
    CHECK(client_port == accepted.sin6_port);
    ipv6_stream_expect(alias, request, sizeof request);
    write_all(alias, response, sizeof response);
    CHECK(shutdown(alias, SHUT_WR) == 0);
    CHECK(close(alias) == 0 && close(server) == 0 && close(listening) == 0);
    reap(child, 0);
    printf("network-smoke ipv6-tcp request=%zu response=%zu names=28 peek=1 fin=1 reaped=0\n",
        sizeof request, sizeof response);
}

/// UDP 就绪由原 ppoll 观察，每次只领取一条并核全文及同一记录的 IPv6 来源。
static void ipv6_datagram_expect(int fd, const void *expected, size_t size,
    const struct sockaddr_in6 *sender)
{
    unsigned char output[2049];
    struct sockaddr_in6 source = {0};
    socklen_t length = sizeof source;
    CHECK(size <= sizeof output);
    CHECK(wait_ready(fd, POLLIN) & POLLIN);
    CHECK(recvfrom(fd, output, sizeof output, 0, (struct sockaddr *)&source, &length) == (ssize_t)size);
    CHECK(memcmp(output, expected, size) == 0);
    ipv6_name(&source, length, sender->sin6_port);
}

/// 两目的、默认 peer、零消息、PEEK/TRUNC、大 lo 消息与 LP64 向量均经真实 IPv6 消息入口。
static void ipv6_udp(void)
{
    struct sockaddr_in6 a, b, local;
    int first = ipv6_bound(SOCK_DGRAM, &a), second = ipv6_bound(SOCK_DGRAM, &b);
    int fd = ipv6_bound(SOCK_DGRAM, &local);
    CHECK(a.sin6_port != b.sin6_port && local.sin6_port != a.sin6_port && local.sin6_port != b.sin6_port);
    ERROR(send(fd, "x", 1, 0), EDESTADDRREQ);
    CHECK(sendto(fd, "one", 3, 0, (struct sockaddr *)&a, sizeof a) == 3);
    ipv6_datagram_expect(first, "one", 3, &local);
    CHECK(sendto(fd, "two", 3, 0, (struct sockaddr *)&b, sizeof b) == 3);
    ipv6_datagram_expect(second, "two", 3, &local);
    CHECK(connect(fd, (struct sockaddr *)&a, sizeof a) == 0);
    CHECK(sendto(fd, "override", 8, 0, (struct sockaddr *)&b, sizeof b) == 8);
    ipv6_datagram_expect(second, "override", 8, &local);
    ipv6_query(fd, 1, a.sin6_port);
    CHECK(send(fd, "default", 7, 0) == 7);
    ipv6_datagram_expect(first, "default", 7, &local);

    unsigned char output[2049];
    struct sockaddr_in6 source = {0};
    socklen_t length = sizeof source;
    CHECK(send(fd, NULL, 0, 0) == 0);
    CHECK(wait_ready(first, POLLIN) & POLLIN);
    CHECK(recvfrom(first, output, sizeof output, MSG_PEEK, (struct sockaddr *)&source, &length) == 0);
    ipv6_name(&source, length, local.sin6_port);
    ipv6_datagram_expect(first, "", 0, &local);
    ERROR(recv(first, output, sizeof output, MSG_DONTWAIT), EAGAIN);
    CHECK(send(fd, "short-tail", 10, 0) == 10);
    CHECK(send(fd, "next", 4, 0) == 4);
    CHECK(wait_ready(first, POLLIN) & POLLIN);
    length = sizeof source;
    CHECK(recvfrom(first, output, 3, 0, (struct sockaddr *)&source, &length) == 3);
    CHECK(memcmp(output, "sho", 3) == 0);
    ipv6_name(&source, length, local.sin6_port);
    ipv6_datagram_expect(first, "next", 4, &local);
    CHECK(send(fd, "peek-full", 9, 0) == 9);
    CHECK(wait_ready(first, POLLIN) & POLLIN);
    length = sizeof source;
    CHECK(recvfrom(first, output, 4, MSG_PEEK, (struct sockaddr *)&source, &length) == 4);
    CHECK(memcmp(output, "peek", 4) == 0);
    ipv6_name(&source, length, local.sin6_port);
    length = sizeof source;
    CHECK(recvfrom(first, output, 4, MSG_PEEK | MSG_TRUNC, (struct sockaddr *)&source, &length) == 9);
    CHECK(memcmp(output, "peek", 4) == 0);
    ipv6_name(&source, length, local.sin6_port);
    ipv6_datagram_expect(first, "peek-full", 9, &local);
    CHECK(send(fd, "truncated", 9, 0) == 9);
    CHECK(send(fd, "following", 9, 0) == 9);
    CHECK(wait_ready(first, POLLIN) & POLLIN);
    length = sizeof source;
    CHECK(recvfrom(first, output, 3, MSG_TRUNC, (struct sockaddr *)&source, &length) == 9);
    CHECK(memcmp(output, "tru", 3) == 0);
    ipv6_name(&source, length, local.sin6_port);
    ipv6_datagram_expect(first, "following", 9, &local);

    unsigned char large[2049];
    for (size_t i = 0; i < sizeof large; ++i) large[i] = pattern(i);
    CHECK(send(fd, large, sizeof large, 0) == sizeof large);
    ipv6_datagram_expect(first, large, sizeof large, &local);
    ERROR(recv(first, output, sizeof output, MSG_DONTWAIT), EAGAIN);

    struct iovec input[] = {{(void *)"vector-", 7}, {(void *)"datagram", 8}};
    struct msghdr message = {.msg_name = &b, .msg_namelen = sizeof b,
        .msg_iov = input, .msg_iovlen = 2};
    CHECK(sendmsg(fd, &message, 0) == 15);
    unsigned char prefix[3], tail[12];
    struct iovec target[] = {{prefix, sizeof prefix}, {tail, sizeof tail}};
    struct msghdr header = {.msg_name = &source, .msg_namelen = sizeof source,
        .msg_iov = target, .msg_iovlen = 2};
    CHECK(wait_ready(second, POLLIN) & POLLIN);
    CHECK(recvmsg(second, &header, 0) == 15);
    CHECK(memcmp(prefix, "vec", sizeof prefix) == 0 && memcmp(tail, "tor-datagram", sizeof tail) == 0);
    CHECK(header.msg_flags == 0 && header.msg_controllen == 0);
    ipv6_name(&source, header.msg_namelen, local.sin6_port);
    ipv6_query(fd, 1, a.sin6_port);
    CHECK(send(fd, "after vector", 12, 0) == 12);
    ipv6_datagram_expect(first, "after vector", 12, &local);

    struct sockaddr disconnect = {.sa_family = AF_UNSPEC};
    CHECK(connect(fd, &disconnect, sizeof disconnect.sa_family) == 0);
    length = sizeof source;
    ERROR(getpeername(fd, (struct sockaddr *)&source, &length), ENOTCONN);
    ERROR(send(fd, "x", 1, 0), EDESTADDRREQ);
    CHECK(sendto(second, "recent", 6, 0, (struct sockaddr *)&local, sizeof local) == 6);
    ipv6_datagram_expect(fd, "recent", 6, &b);
    ERROR(send(fd, "x", 1, 0), EDESTADDRREQ);
    CHECK(close(fd) == 0 && close(first) == 0 && close(second) == 0);
    printf("network-smoke ipv6-udp destinations=2 names=28 zero=1 peek=1 trunc=1 large=2049 vector=15 disconnected=1\n");
}

/// int 设置沿正式 ABI 提交，查询断言由各行为单独消费。
static void set_option(int fd, int level, int name, int value)
{
    CHECK(setsockopt(fd, level, name, &value, sizeof value) == 0);
}

/// 通配地址保留 ::，port0 的实际分配名称仍按 AF_INET6 返回。
static int wildcard6(int type, int only, uint16_t port, struct sockaddr_in6 *local)
{
    int fd = socket(AF_INET6, type, 0);
    CHECK(0 <= fd);
    CHECK(option(fd, IPPROTO_IPV6, IPV6_V6ONLY) == 0);
    if (only) set_option(fd, IPPROTO_IPV6, IPV6_V6ONLY, 1);
    CHECK(option(fd, IPPROTO_IPV6, IPV6_V6ONLY) == only);
    *local = (struct sockaddr_in6){.sin6_family = AF_INET6, .sin6_port = port};
    CHECK(bind(fd, (struct sockaddr *)local, sizeof *local) == 0);
    socklen_t length = sizeof *local;
    CHECK(getsockname(fd, (struct sockaddr *)local, &length) == 0);
    const struct in6_addr any = IN6ADDR_ANY_INIT;
    CHECK(length == 28 && local->sin6_family == AF_INET6 && local->sin6_port != 0);
    CHECK(memcmp(&local->sin6_addr, &any, sizeof any) == 0);
    CHECK(local->sin6_flowinfo == 0 && local->sin6_scope_id == 0);
    return fd;
}

/// 映射端点在用户边界保 AF_INET6 身份，IPv4 字节只放在最后四字节。
static struct sockaddr_in6 mapped_address(uint16_t port)
{
    struct sockaddr_in6 value = {.sin6_family = AF_INET6, .sin6_port = port};
    const unsigned char bytes[16] = {0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 255, 255, 127, 0, 0, 1};
    memcpy(&value.sin6_addr, bytes, sizeof bytes);
    return value;
}

/// 名称与来源的映射输出必须含完整 28 字节及真实端口。
static void mapped_name(const struct sockaddr_in6 *actual, socklen_t length, uint16_t port)
{
    struct sockaddr_in6 expected = mapped_address(port);
    CHECK(length == 28 && actual->sin6_family == AF_INET6);
    CHECK(actual->sin6_port == port && port != 0);
    CHECK(actual->sin6_flowinfo == 0 && actual->sin6_scope_id == 0);
    CHECK(memcmp(&actual->sin6_addr, &expected.sin6_addr, sizeof expected.sin6_addr) == 0);
}

/// 已连接映射 OFD 的 local/peer 均由正式名称 syscall 取得。
static void mapped_query(int fd, int peer, uint16_t port)
{
    struct sockaddr_in6 actual;
    socklen_t length = sizeof actual;
    CHECK((peer ? getpeername(fd, (struct sockaddr *)&actual, &length)
        : getsockname(fd, (struct sockaddr *)&actual, &length)) == 0);
    mapped_name(&actual, length, port);
}

/// 短双向流在各自 FIN 后领取 EOF，关闭双方前不遗留未读字节。
static void short_exchange(int client, int server)
{
    unsigned char output[32];
    write_all(client, "mapped\0request\xff", 15);
    CHECK(shutdown(client, SHUT_WR) == 0);
    read_all(server, output, 15);
    CHECK(memcmp(output, "mapped\0request\xff", 15) == 0);
    CHECK(read(server, output, 1) == 0);
    write_all(server, "native\0response\xfe", 16);
    CHECK(shutdown(server, SHUT_WR) == 0);
    read_all(client, output, 16);
    CHECK(memcmp(output, "native\0response\xfe", 16) == 0);
    CHECK(read(client, output, 1) == 0);
    CHECK(close(client) == 0 && close(server) == 0);
}

/// 双面通配 listener 及 dup 共用身份，原生隔离与显式 mapped 都用真实 TCP 连接。
static void mapped_tcp(void)
{
    struct sockaddr_in6 local;
    int listening = wildcard6(SOCK_STREAM, 0, 0, &local);
    CHECK(listen(listening, 4) == 0);
    int alias = dup(listening);
    CHECK(0 <= alias);
    struct stat original, duplicate, accepted;
    CHECK(fstat(listening, &original) == 0 && fstat(alias, &duplicate) == 0);
    CHECK(original.st_ino == duplicate.st_ino && original.st_dev == duplicate.st_dev);
    int v4 = socket(AF_INET, SOCK_STREAM, 0);
    CHECK(0 <= v4);
    struct sockaddr_in destination = address(ntohs(local.sin6_port));
    ERROR(bind(v4, (struct sockaddr *)&destination, sizeof destination), EADDRINUSE);
    CHECK(close(v4) == 0);
    int client = socket(AF_INET, SOCK_STREAM, 0);
    CHECK(0 <= client && connect(client, (struct sockaddr *)&destination, sizeof destination) == 0);
    struct sockaddr_in own;
    socklen_t length = sizeof own;
    CHECK(getsockname(client, (struct sockaddr *)&own, &length) == 0);
    CHECK(length == 16 && own.sin_family == AF_INET && own.sin_addr.s_addr == htonl(INADDR_LOOPBACK));
    struct sockaddr_in6 peer;
    length = sizeof peer;
    int server = accept(alias, (struct sockaddr *)&peer, &length);
    CHECK(0 <= server);
    mapped_name(&peer, length, own.sin_port);
    ipv6_identity(server, SOCK_STREAM);
    mapped_query(server, 0, local.sin6_port);
    mapped_query(server, 1, own.sin_port);
    CHECK(fstat(server, &accepted) == 0 && accepted.st_ino != original.st_ino);
    int enabled = 1;
    ERROR(setsockopt(listening, IPPROTO_IPV6, IPV6_V6ONLY, &enabled, sizeof enabled), EINVAL);
    ERROR(setsockopt(server, IPPROTO_IPV6, IPV6_V6ONLY, &enabled, sizeof enabled), EINVAL);
    short_exchange(client, server);
    CHECK(option(alias, SOL_SOCKET, SO_ACCEPTCONN) == 1 && option(alias, SOL_SOCKET, SO_DOMAIN) == AF_INET6);
    CHECK(close(alias) == 0 && close(listening) == 0);

    listening = wildcard6(SOCK_STREAM, 1, 0, &local);
    CHECK(listen(listening, 4) == 0 && fcntl(listening, F_SETFL, O_NONBLOCK) == 0);
    int v4_listener = listener(ntohs(local.sin6_port), 0);
    destination = address(ntohs(local.sin6_port));
    client = socket(AF_INET, SOCK_STREAM, 0);
    CHECK(0 <= client && connect(client, (struct sockaddr *)&destination, sizeof destination) == 0);
    server = accept(v4_listener, NULL, NULL);
    CHECK(0 <= server && option(server, SOL_SOCKET, SO_DOMAIN) == AF_INET);
    ERROR(accept(listening, NULL, NULL), EAGAIN);
    short_exchange(client, server);
    struct sockaddr_in6 native = ipv6_address();
    native.sin6_port = local.sin6_port;
    client = socket(AF_INET6, SOCK_STREAM, 0);
    CHECK(0 <= client && connect(client, (struct sockaddr *)&native, sizeof native) == 0);
    CHECK(wait_ready(listening, POLLIN) & POLLIN);
    length = sizeof peer;
    server = accept(listening, (struct sockaddr *)&peer, &length);
    CHECK(0 <= server);
    ipv6_name(&peer, length, 0);
    ipv6_identity(server, SOCK_STREAM);
    CHECK(option(server, IPPROTO_IPV6, IPV6_V6ONLY) == 1);
    short_exchange(client, server);
    CHECK(close(listening) == 0);

    struct sockaddr_in6 mapped = mapped_address(destination.sin_port);
    client = socket(AF_INET6, SOCK_STREAM, 0);
    CHECK(0 <= client && connect(client, (struct sockaddr *)&mapped, sizeof mapped) == 0);
    ipv6_identity(client, SOCK_STREAM);
    mapped_query(client, 1, destination.sin_port);
    length = sizeof peer;
    CHECK(getsockname(client, (struct sockaddr *)&peer, &length) == 0);
    mapped_name(&peer, length, peer.sin6_port);
    ERROR(setsockopt(client, IPPROTO_IPV6, IPV6_V6ONLY, &enabled, sizeof enabled), EINVAL);
    server = accept(v4_listener, NULL, NULL);
    CHECK(0 <= server);
    short_exchange(client, server);
    CHECK(close(v4_listener) == 0);

    client = socket(AF_INET6, SOCK_STREAM, 0);
    CHECK(0 <= client);
    set_option(client, IPPROTO_IPV6, IPV6_V6ONLY, 1);
    ERROR(bind(client, (struct sockaddr *)&mapped, sizeof mapped), EINVAL);
    ERROR(connect(client, (struct sockaddr *)&mapped, sizeof mapped), ENETUNREACH);
    CHECK(close(client) == 0);
    client = socket(AF_INET, SOCK_STREAM, 0);
    CHECK(0 <= client);
    ERROR(bind(client, (struct sockaddr *)&native, sizeof native), EAFNOSUPPORT);
    ERROR(connect(client, (struct sockaddr *)&native, sizeof native), EAFNOSUPPORT);
    CHECK(option(client, SOL_SOCKET, SO_DOMAIN) == AF_INET && close(client) == 0);
}

/// UDP 双面通配按同条记录返回 mapped source，V6ONLY 后原生与 IPv4 各自领取。
static void mapped_udp(void)
{
    struct sockaddr_in6 local, source;
    int receiver = wildcard6(SOCK_DGRAM, 0, 0, &local);
    struct sockaddr_in sender, destination = address(ntohs(local.sin6_port));
    int fd = datagram(&sender);
    CHECK(sendto(fd, "udp-mapped", 10, 0, (struct sockaddr *)&destination, sizeof destination) == 10);
    unsigned char output[32];
    socklen_t length = sizeof source;
    CHECK(recvfrom(receiver, output, sizeof output, 0, (struct sockaddr *)&source, &length) == 10);
    CHECK(memcmp(output, "udp-mapped", 10) == 0);
    mapped_name(&source, length, sender.sin_port);
    ipv6_identity(receiver, SOCK_DGRAM);
    CHECK(sendto(receiver, "mapped-reply", 12, 0, (struct sockaddr *)&source, sizeof source) == 12);
    datagram_expect(fd, "mapped-reply", &destination);
    CHECK(close(receiver) == 0);

    receiver = wildcard6(SOCK_DGRAM, 1, 0, &local);
    destination = address(ntohs(local.sin6_port));
    int v4 = socket(AF_INET, SOCK_DGRAM, 0);
    CHECK(0 <= v4 && bind(v4, (struct sockaddr *)&destination, sizeof destination) == 0);
    CHECK(sendto(fd, "v4-only", 7, 0, (struct sockaddr *)&destination, sizeof destination) == 7);
    datagram_expect(v4, "v4-only", &sender);
    ERROR(recv(receiver, output, sizeof output, MSG_DONTWAIT), EAGAIN);
    struct sockaddr_in6 native_sender;
    int native = ipv6_bound(SOCK_DGRAM, &native_sender);
    struct sockaddr_in6 native_destination = ipv6_address();
    native_destination.sin6_port = local.sin6_port;
    CHECK(sendto(native, "v6-only", 7, 0, (struct sockaddr *)&native_destination, sizeof native_destination) == 7);
    ipv6_datagram_expect(receiver, "v6-only", 7, &native_sender);
    ERROR(recv(v4, output, sizeof output, MSG_DONTWAIT), EAGAIN);
    CHECK(close(native) == 0 && close(v4) == 0 && close(fd) == 0 && close(receiver) == 0);
}

/// 同一用户网络域消费有限 mapped/V6ONLY 行为，不复制两族 flags 矩阵。
static void ipv6_mapped(void)
{
    mapped_tcp();
    mapped_udp();
    printf("network-smoke ipv6-mapped dual-listener=1 names=28 v6only=1 udp-source=1\n");
}

/// 大消息的拥有存储位于 BSS，函数栈只保存 ABI 名称及少量前缀。
static unsigned char datagram_input[65508], datagram_output[65508];

/// 同条 datagram 的 source 按发送者实际族和端口比较，不比较 sockaddr padding。
static void message_source(const struct sockaddr_storage *source, socklen_t length,
    const struct sockaddr_storage *sender, socklen_t sender_length)
{
    CHECK(length == sender_length && source->ss_family == sender->ss_family);
    if (sender->ss_family == AF_INET) {
        const struct sockaddr_in *actual = (const struct sockaddr_in *)source;
        const struct sockaddr_in *expected = (const struct sockaddr_in *)sender;
        CHECK(actual->sin_port == expected->sin_port);
        CHECK(actual->sin_addr.s_addr == expected->sin_addr.s_addr);
    } else {
        const struct sockaddr_in6 *actual = (const struct sockaddr_in6 *)source;
        const struct sockaddr_in6 *expected = (const struct sockaddr_in6 *)sender;
        ipv6_name(actual, length, expected->sin6_port);
    }
}

/// 每族两种具名尺寸各发一次，PEEK/TRUNC 后仍一次领取全文，越界不产生后继消息。
static void large_loopback_family(int family)
{
    struct sockaddr_storage sender = {0}, destination = {0}, source;
    socklen_t length;
    int fd, receiver;
    size_t maximum;
    if (family == AF_INET) {
        struct sockaddr_in own, target;
        fd = datagram(&own);
        receiver = datagram(&target);
        memcpy(&sender, &own, sizeof own);
        memcpy(&destination, &target, sizeof target);
        length = sizeof own;
        maximum = 65507;
    } else {
        struct sockaddr_in6 own, target;
        fd = ipv6_bound(SOCK_DGRAM, &own);
        receiver = ipv6_bound(SOCK_DGRAM, &target);
        memcpy(&sender, &own, sizeof own);
        memcpy(&destination, &target, sizeof target);
        length = sizeof own;
        maximum = 65487;
    }
    const size_t sizes[] = {8193, maximum};
    for (size_t message = 0; message < sizeof sizes / sizeof sizes[0]; ++message) {
        size_t size = sizes[message];
        for (size_t i = 0; i < size; ++i)
            datagram_input[i] = pattern(i + message * 113 + (size_t)family * 257);
        CHECK(sendto(fd, datagram_input, size, 0, (struct sockaddr *)&destination, length) == (ssize_t)size);
        if (message == 0) {
            unsigned char prefix[31];
            socklen_t source_length = sizeof source;
            CHECK(recvfrom(receiver, prefix, sizeof prefix, MSG_PEEK | MSG_TRUNC,
                (struct sockaddr *)&source, &source_length) == (ssize_t)size);
            CHECK(memcmp(prefix, datagram_input, sizeof prefix) == 0);
            message_source(&source, source_length, &sender, length);
        }
        socklen_t source_length = sizeof source;
        CHECK(recvfrom(receiver, datagram_output, sizeof datagram_output, 0,
            (struct sockaddr *)&source, &source_length) == (ssize_t)size);
        CHECK(memcmp(datagram_output, datagram_input, size) == 0);
        message_source(&source, source_length, &sender, length);
        ERROR(recv(receiver, datagram_output, sizeof datagram_output, MSG_DONTWAIT), EAGAIN);
        printf("network-smoke large-loopback family=%d size=%zu single-message=1 source=%u\n",
            family, size, source_length);
    }
    ERROR(sendto(fd, datagram_input, maximum + 1, 0, (struct sockaddr *)&destination, length), EMSGSIZE);
    ERROR(recv(receiver, datagram_output, sizeof datagram_output, MSG_DONTWAIT), EAGAIN);
    CHECK(close(fd) == 0 && close(receiver) == 0);
}

/// 大 lo 的协议上界只在一个 mode 验收，不把本地消息等同 NIC 分片。
static void large_loopback(void)
{
    large_loopback_family(AF_INET);
    large_loopback_family(AF_INET6);
}

/// Reno 读取固定 16 字节名字槽，截短只改交付前缀，setter 消费有界名称。
static void congestion_option(int fd)
{
    const char reno[16] = "reno";
    const socklen_t capacities[] = {0, 4, 16, 24};
    for (size_t i = 0; i < sizeof capacities / sizeof capacities[0]; ++i) {
        unsigned char output[24];
        memset(output, 0xa5, sizeof output);
        socklen_t length = capacities[i], copied = length < 16 ? length : 16;
        CHECK(getsockopt(fd, IPPROTO_TCP, TCP_CONGESTION, output, &length) == 0);
        CHECK(length == copied && memcmp(output, reno, copied) == 0);
        for (size_t byte = copied; byte < sizeof output; ++byte) CHECK(output[byte] == 0xa5);
    }
    CHECK(setsockopt(fd, IPPROTO_TCP, TCP_CONGESTION, "reno", 4) == 0);
    CHECK(setsockopt(fd, IPPROTO_TCP, TCP_CONGESTION, "reno\0ignored", 12) == 0);
    ERROR(setsockopt(fd, IPPROTO_TCP, TCP_CONGESTION, "cubic", 5), ENOENT);
    ERROR(setsockopt(fd, IPPROTO_TCP, TCP_CONGESTION, "", 0), EINVAL);
    ERROR(setsockopt(fd, IPPROTO_TCP, TCP_CONGESTION, (void *)1, 4), EFAULT);
    unsigned char *mapping = mmap(NULL, 8192, PROT_READ | PROT_WRITE,
        MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    CHECK(mapping != MAP_FAILED && munmap(mapping + 4096, 4096) == 0);
    // 名称到首 NUL 或最多十五字节；optlen 不能让读取跨进未映射页。
    memcpy(mapping + 4091, "reno", 5);
    CHECK(setsockopt(fd, IPPROTO_TCP, TCP_CONGESTION, mapping + 4091, 32) == 0);
    memset(mapping + 4081, 'x', 15);
    ERROR(setsockopt(fd, IPPROTO_TCP, TCP_CONGESTION, mapping + 4081, 16), ENOENT);
    CHECK(munmap(mapping, 4096) == 0);
    socklen_t length = 16;
    ERROR(getsockopt(fd, IPPROTO_TCP, TCP_CONGESTION, (void *)1, &length), EFAULT);
    length = 0;
    CHECK(getsockopt(fd, IPPROTO_TCP, TCP_CONGESTION, (void *)1, &length) == 0 && length == 0);
    unsigned char output[16];
    length = sizeof output;
    CHECK(getsockopt(fd, IPPROTO_TCP, TCP_CONGESTION, output, &length) == 0);
    CHECK(length == 16 && memcmp(output, reno, 16) == 0);
}

/// NODELAY 与 KEEPALIVE 的非零/零开关消费正式查询，短 int 输出保剩余 guard。
static void protocol_switches(int fd)
{
    CHECK(option(fd, IPPROTO_TCP, TCP_NODELAY) == 0);
    CHECK(option(fd, SOL_SOCKET, SO_KEEPALIVE) == 0);
    set_option(fd, IPPROTO_TCP, TCP_NODELAY, -7);
    set_option(fd, SOL_SOCKET, SO_KEEPALIVE, -7);
    CHECK(option(fd, IPPROTO_TCP, TCP_NODELAY) == 1);
    CHECK(option(fd, SOL_SOCKET, SO_KEEPALIVE) == 1);
    unsigned char output[8];
    memset(output, 0xa5, sizeof output);
    const int enabled = 1;
    socklen_t length = 2;
    CHECK(getsockopt(fd, IPPROTO_TCP, TCP_NODELAY, output, &length) == 0 && length == 2);
    CHECK(memcmp(output, &enabled, 2) == 0);
    for (size_t i = 2; i < sizeof output; ++i) CHECK(output[i] == 0xa5);
    ERROR(setsockopt(fd, IPPROTO_TCP, TCP_NODELAY, &enabled, 3), EINVAL);
    ERROR(setsockopt(fd, SOL_SOCKET, SO_KEEPALIVE, (void *)1, sizeof enabled), EFAULT);
    ERROR(getsockopt(fd, IPPROTO_TCP, TCP_NODELAY, output, (socklen_t *)1), EFAULT);
    ERROR(setsockopt(fd, IPPROTO_TCP, 0x7fff, &enabled, sizeof enabled), ENOPROTOOPT);
    length = sizeof enabled;
    ERROR(getsockopt(fd, IPPROTO_TCP, 0x7fff, output, &length), ENOPROTOOPT);
    set_option(fd, IPPROTO_TCP, TCP_NODELAY, 0);
    set_option(fd, SOL_SOCKET, SO_KEEPALIVE, 0);
    CHECK(option(fd, IPPROTO_TCP, TCP_NODELAY) == 0);
    CHECK(option(fd, SOL_SOCKET, SO_KEEPALIVE) == 0);
}

/// listener 设置在握手前交给 child，两族 MSS 都查询已协商的 lo 连接。
static void protocol_family(int family)
{
    struct sockaddr_storage destination = {0};
    socklen_t length;
    int listening;
    if (family == AF_INET) {
        listening = listener(0, 0);
        struct sockaddr_in local;
        length = sizeof local;
        CHECK(getsockname(listening, (struct sockaddr *)&local, &length) == 0);
        memcpy(&destination, &local, sizeof local);
    } else {
        struct sockaddr_in6 local;
        listening = ipv6_bound(SOCK_STREAM, &local);
        CHECK(listen(listening, 4) == 0);
        length = sizeof local;
        memcpy(&destination, &local, sizeof local);
    }
    CHECK(option(listening, SOL_SOCKET, SO_SNDBUF) == 131072);
    CHECK(option(listening, SOL_SOCKET, SO_RCVBUF) == 131072);
    protocol_switches(listening);
    set_option(listening, IPPROTO_TCP, TCP_NODELAY, 1);
    set_option(listening, SOL_SOCKET, SO_KEEPALIVE, 1);
    int client = socket(family, SOCK_STREAM, 0);
    CHECK(0 <= client);
    protocol_switches(client);
    CHECK(connect(client, (struct sockaddr *)&destination, length) == 0);
    int server = accept(listening, NULL, NULL);
    CHECK(0 <= server);
    CHECK(option(server, IPPROTO_TCP, TCP_NODELAY) == 1);
    CHECK(option(server, SOL_SOCKET, SO_KEEPALIVE) == 1);
    set_option(server, IPPROTO_TCP, TCP_NODELAY, 0);
    set_option(server, SOL_SOCKET, SO_KEEPALIVE, 0);
    CHECK(option(server, IPPROTO_TCP, TCP_NODELAY) == 0 && option(server, SOL_SOCKET, SO_KEEPALIVE) == 0);
    CHECK(option(listening, IPPROTO_TCP, TCP_NODELAY) == 1 && option(listening, SOL_SOCKET, SO_KEEPALIVE) == 1);
    congestion_option(client);
    congestion_option(server);
    int mss = option(client, IPPROTO_TCP, TCP_MAXSEG);
    CHECK(mss == option(server, IPPROTO_TCP, TCP_MAXSEG));
    CHECK(mss == (family == AF_INET ? 65495 : 65475));
    ERROR(setsockopt(client, IPPROTO_TCP, TCP_MAXSEG, &mss, sizeof mss), ENOPROTOOPT);
    short_exchange(client, server);
    CHECK(close(listening) == 0);
    printf("network-smoke protocol-options family=%d mss=%d reno-slot=16 keepalive-switch=1 nodelay-switch=1 inherited=1\n",
        family, mss);
}

/// 通用 KEEPALIVE 布尔属性不赋予 UDP/UNIX TCP 协议选项。
static void non_tcp_options(int fd)
{
    CHECK(option(fd, SOL_SOCKET, SO_KEEPALIVE) == 0);
    set_option(fd, SOL_SOCKET, SO_KEEPALIVE, -1);
    CHECK(option(fd, SOL_SOCKET, SO_KEEPALIVE) == 1);
    set_option(fd, SOL_SOCKET, SO_KEEPALIVE, 0);
    CHECK(option(fd, SOL_SOCKET, SO_KEEPALIVE) == 0);
    const int names[] = {TCP_NODELAY, TCP_MAXSEG, TCP_CONGESTION};
    int value = 1;
    for (size_t i = 0; i < sizeof names / sizeof names[0]; ++i) {
        socklen_t length = sizeof value;
        ERROR(getsockopt(fd, IPPROTO_TCP, names[i], &value, &length), ENOPROTOOPT);
        ERROR(setsockopt(fd, IPPROTO_TCP, names[i], &value, sizeof value), ENOPROTOOPT);
    }
}

/// 开关、MSS 及 Reno 通过普通选项 ABI 消费；7200 秒 probe 留给库的可控 Instant。
static void protocol_options(void)
{
    protocol_family(AF_INET);
    protocol_family(AF_INET6);
    int udp = socket(AF_INET, SOCK_DGRAM, 0), fds[2];
    CHECK(0 <= udp && socketpair(AF_UNIX, SOCK_STREAM, 0, fds) == 0);
    CHECK(option(udp, SOL_SOCKET, SO_SNDBUF) == 131072);
    CHECK(option(udp, SOL_SOCKET, SO_RCVBUF) == 131072);
    non_tcp_options(udp);
    non_tcp_options(fds[0]);
    CHECK(close(udp) == 0 && close(fds[0]) == 0 && close(fds[1]) == 0);
}

/// 小预算在同一 OFD 及其别名生效，hint 只通过正式 setsockopt 换算。
static void buffer_hint(int fd, int name, int hint, int expected)
{
    set_option(fd, SOL_SOCKET, name, hint);
    CHECK(option(fd, SOL_SOCKET, name) == expected);
}

/// 普通 pipe 控制 peer 暂停读取；真实 EAGAIN 前缀在缩小、提高和 FIN 后仍完整交付。
static void tcp_buffer_budget(void)
{
    int listening = listener(0, 0);
    buffer_hint(listening, SO_RCVBUF, 1024, 2048);
    buffer_hint(listening, SO_SNDBUF, 1024, 2048);
    struct sockaddr_in destination;
    socklen_t length = sizeof destination;
    CHECK(getsockname(listening, (struct sockaddr *)&destination, &length) == 0);
    int client = socket(AF_INET, SOCK_STREAM, 0);
    CHECK(0 <= client);
    buffer_hint(client, SO_SNDBUF, 1024, 2048);
    buffer_hint(client, SO_RCVBUF, 1024, 2048);
    CHECK(connect(client, (struct sockaddr *)&destination, sizeof destination) == 0);
    int server = accept(listening, NULL, NULL), alias = dup(client);
    CHECK(0 <= server && 0 <= alias);
    CHECK(option(server, SOL_SOCKET, SO_RCVBUF) == 2048);
    CHECK(option(server, SOL_SOCKET, SO_SNDBUF) == 2048);
    CHECK(close(listening) == 0);
    int control[2], notice[2];
    CHECK(pipe(control) == 0 && pipe(notice) == 0);
    pid_t child = fork_child();
    if (child == 0) {
        CHECK(close(client) == 0 && close(alias) == 0);
        CHECK(close(control[1]) == 0 && close(notice[0]) == 0);
        size_t initially_accepted, total;
        read_all(control[0], &initially_accepted, sizeof initially_accepted);
        CHECK(0 < initially_accepted);
        unsigned char output[733], byte;
        CHECK(recv(server, &byte, 1, MSG_PEEK) == 1 && byte == pattern(0));
        buffer_hint(server, SO_RCVBUF, 1, 1024);
        write_all(notice[1], "R", 1);
        read_all(control[0], &total, sizeof total);
        CHECK(initially_accepted < total);
        buffer_hint(server, SO_RCVBUF, 4096, 8192);
        write_all(notice[1], "G", 1);
        size_t done = 0;
        while (done < total) {
            size_t requested = total - done;
            if (sizeof output < requested) requested = sizeof output;
            ssize_t count = read(server, output, requested);
            CHECK(0 < count && (size_t)count <= requested);
            for (size_t i = 0; i < (size_t)count; ++i) CHECK(output[i] == pattern(done + i));
            done += (size_t)count;
        }
        CHECK(read(server, &byte, 1) == 0);
        write_all(server, "budget-restored", 15);
        CHECK(shutdown(server, SHUT_WR) == 0);
        CHECK(close(server) == 0 && close(control[0]) == 0 && close(notice[1]) == 0);
        _exit(0);
    }
    CHECK(close(server) == 0 && close(control[0]) == 0 && close(notice[1]) == 0);
    CHECK(fcntl(client, F_SETFL, O_NONBLOCK) == 0);
    unsigned char input[4096], output[32], byte;
    size_t accepted = 0;
    for (;;) {
        CHECK(accepted < 262144);
        for (size_t i = 0; i < sizeof input; ++i) input[i] = pattern(accepted + i);
        ssize_t count = send(client, input, sizeof input, MSG_NOSIGNAL);
        if (count == -1) {
            CHECK(errno == EAGAIN);
            break;
        }
        CHECK(0 < count && (size_t)count <= sizeof input);
        accepted += (size_t)count;
    }
    CHECK(0 < accepted);
    buffer_hint(alias, SO_SNDBUF, 1, 1024);
    CHECK(option(client, SOL_SOCKET, SO_SNDBUF) == 1024);
    write_all(control[1], &accepted, sizeof accepted);
    read_all(notice[0], &byte, 1);
    CHECK(byte == 'R');
    buffer_hint(alias, SO_SNDBUF, 4096, 8192);
    CHECK(option(client, SOL_SOCKET, SO_SNDBUF) == 8192);
    for (size_t i = 0; i < sizeof input; ++i) input[i] = pattern(accepted + i);
    ssize_t resumed = send(client, input, sizeof input, MSG_NOSIGNAL);
    CHECK(0 < resumed && (size_t)resumed <= sizeof input);
    size_t done = accepted + (size_t)resumed, total = done + 16397;
    write_all(control[1], &total, sizeof total);
    read_all(notice[0], &byte, 1);
    CHECK(byte == 'G' && fcntl(alias, F_SETFL, 0) == 0);
    while (done < total) {
        size_t count = total - done;
        if (sizeof input < count) count = sizeof input;
        for (size_t i = 0; i < count; ++i) input[i] = pattern(done + i);
        write_all(client, input, count);
        done += count;
    }
    CHECK(shutdown(alias, SHUT_WR) == 0);
    read_all(client, output, 15);
    CHECK(memcmp(output, "budget-restored", 15) == 0 && read(client, &byte, 1) == 0);
    CHECK(read(notice[0], &byte, 1) == 0);
    reap(child, 0);
    CHECK(close(control[1]) == 0 && close(notice[0]) == 0);
    CHECK(close(alias) == 0 && close(client) == 0);
    printf("network-smoke buffer-budget tcp-initial=%zu resumed=%zd total=%zu shrink-preserved=1 fin=1 reaped=0\n",
        accepted, resumed, total);
}

/// 一次 recvfrom 领取预算调整前已排队的完整消息，并核同条记录的 IPv4 来源。
static void budget_datagram_expect(int fd, size_t size, const struct sockaddr_in *sender)
{
    struct sockaddr_in source;
    socklen_t length = sizeof source;
    CHECK(recvfrom(fd, datagram_output, sizeof datagram_output, 0,
        (struct sockaddr *)&source, &length) == (ssize_t)size);
    CHECK(memcmp(datagram_output, datagram_input, size) == 0);
    CHECK(length == 16 && source.sin_family == AF_INET);
    CHECK(source.sin_port == sender->sin_port && source.sin_addr.s_addr == sender->sin_addr.s_addr);
}

/// UDP 单消息按真实预算接纳，提高允许大消息，缩小不撤销已经就绪的记录。
static void udp_buffer_budget(void)
{
    struct sockaddr_in sender, destination;
    int fd = datagram(&sender), receiver = datagram(&destination), alias = dup(receiver);
    CHECK(0 <= alias);
    buffer_hint(fd, SO_SNDBUF, 1024, 2048);
    buffer_hint(fd, SO_RCVBUF, 1024, 2048);
    buffer_hint(receiver, SO_SNDBUF, 1024, 2048);
    buffer_hint(receiver, SO_RCVBUF, 1024, 2048);
    for (size_t i = 0; i < 8193; ++i) datagram_input[i] = pattern(i + 31);
    ERROR(sendto(fd, datagram_input, 2049, 0, (struct sockaddr *)&destination, sizeof destination), EMSGSIZE);
    ERROR(recv(receiver, datagram_output, sizeof datagram_output, MSG_DONTWAIT), EAGAIN);
    CHECK(sendto(fd, datagram_input, 1537, 0, (struct sockaddr *)&destination, sizeof destination) == 1537);
    CHECK(wait_ready(receiver, POLLIN) & POLLIN);
    buffer_hint(alias, SO_RCVBUF, 1, 1024);
    CHECK(option(receiver, SOL_SOCKET, SO_RCVBUF) == 1024);
    budget_datagram_expect(receiver, 1537, &sender);

    buffer_hint(fd, SO_SNDBUF, 8192, 16384);
    buffer_hint(alias, SO_RCVBUF, 8192, 16384);
    CHECK(option(receiver, SOL_SOCKET, SO_RCVBUF) == 16384);
    for (size_t i = 0; i < 8193; ++i) datagram_input[i] = pattern(i + 97);
    CHECK(sendto(fd, datagram_input, 8193, 0, (struct sockaddr *)&destination, sizeof destination) == 8193);
    CHECK(wait_ready(receiver, POLLIN) & POLLIN);
    buffer_hint(alias, SO_RCVBUF, 1, 1024);
    buffer_hint(fd, SO_SNDBUF, 1024, 2048);
    budget_datagram_expect(receiver, 8193, &sender);
    ERROR(sendto(fd, datagram_input, 8193, 0, (struct sockaddr *)&destination, sizeof destination), EMSGSIZE);
    ERROR(recv(receiver, datagram_output, sizeof datagram_output, MSG_DONTWAIT), EAGAIN);
    for (size_t i = 0; i < 1023; ++i) datagram_input[i] = pattern(i + 193);
    CHECK(sendto(fd, datagram_input, 1023, 0, (struct sockaddr *)&destination, sizeof destination) == 1023);
    CHECK(wait_ready(receiver, POLLIN) & POLLIN);
    budget_datagram_expect(receiver, 1023, &sender);
    ERROR(recv(receiver, datagram_output, sizeof datagram_output, MSG_DONTWAIT), EAGAIN);
    buffer_hint(fd, SO_SNDBUF, 100000, 131072);
    buffer_hint(alias, SO_RCVBUF, 100000, 131072);
    CHECK(option(receiver, SOL_SOCKET, SO_RCVBUF) == 131072);
    CHECK(close(alias) == 0 && close(receiver) == 0 && close(fd) == 0);
    printf("network-smoke buffer-budget udp-small=1537 udp-grown=8193 shrink-preserved=1 next-large=EMSGSIZE source=16\n");
}

/// TCP 流满后的接受恢复与 UDP 单消息接纳分别消费同一正式预算接口。
static void buffer_budget(void)
{
    tcp_buffer_budget();
    udp_buffer_budget();
}

/// 服务端口只接受完整十进制数字，范围之外的 argv 不能成为截断后的目的端口。
static unsigned service_port(const char *text)
{
    CHECK(*text != '\0');
    unsigned value = 0;
    for (const unsigned char *digit = (const unsigned char *)text; *digit; ++digit) {
        CHECK('0' <= *digit && *digit <= '9');
        value = value * 10 + (*digit - '0');
        CHECK(value <= 65535);
    }
    CHECK(0 < value);
    return value;
}

/// 两个 NIC 模式只访问明确的宿主网关地址，端口来自本次普通 argv。
static struct sockaddr_in nic_peer(unsigned port)
{
    struct sockaddr_in peer = address(port);
    peer.sin_addr.s_addr = htonl(0x0a000202);
    return peer;
}

/// 单连接真实提交和接收短前缀后继续核验双向全文，写半关闭必须交付宿主 EOF。
static void nic_tcp(unsigned port)
{
    static unsigned char input[98317], output[65539];
    for (size_t i = 0; i < sizeof input; ++i)
        input[i] = (unsigned char)((i * 37 + (i >> 8) + 19) & 255);
    int fd = socket(AF_INET, SOCK_STREAM, IPPROTO_TCP);
    CHECK(0 <= fd);
    struct sockaddr_in peer = nic_peer(port);
    CHECK(connect(fd, (struct sockaddr *)&peer, sizeof peer) == 0);
    struct sockaddr_in actual;
    socklen_t length = sizeof actual;
    CHECK(getsockname(fd, (struct sockaddr *)&actual, &length) == 0);
    CHECK(length == sizeof actual && actual.sin_family == AF_INET);
    CHECK(actual.sin_addr.s_addr == htonl(0x0a00020f) && actual.sin_port != 0);
    length = sizeof actual;
    CHECK(getpeername(fd, (struct sockaddr *)&actual, &length) == 0);
    CHECK(length == sizeof actual && actual.sin_family == AF_INET);
    CHECK(actual.sin_addr.s_addr == peer.sin_addr.s_addr && actual.sin_port == peer.sin_port);
    ssize_t accepted = write(fd, input, sizeof input);
    CHECK(0 < accepted && (size_t)accepted < sizeof input);
    write_all(fd, input + accepted, sizeof input - (size_t)accepted);
    CHECK(shutdown(fd, SHUT_WR) == 0);
    ssize_t received = read(fd, output, sizeof output);
    CHECK(0 < received && (size_t)received < sizeof output);
    read_all(fd, output + received, sizeof output - (size_t)received);
    for (size_t i = 0; i < sizeof output; ++i)
        CHECK(output[i] == (unsigned char)((i * 53 + (i >> 7) + 91) & 255));
    unsigned char byte;
    CHECK(read(fd, &byte, 1) == 0);
    CHECK(close(fd) == 0);
    printf("network-smoke nic-tcp sent=%zu received=%zu first-write=%zd first-read=%zd peer-port=%u\n",
        sizeof input, sizeof output, accepted, received, port);
}

/// 同一 UDP socket 领取四条完整应答，大 IPv4 消息经 NIC 分片，协议越界仍拒绝。
static void nic_udp(unsigned port)
{
    static unsigned char input[65508], output[8193];
    for (size_t i = 0; i < sizeof input; ++i)
        input[i] = (unsigned char)((i * 29 + (i >> 4) + 45) & 255);
    int fd = socket(AF_INET, SOCK_DGRAM, IPPROTO_UDP);
    CHECK(0 <= fd);
    struct sockaddr_in peer = nic_peer(port);
    const size_t sizes[] = {17, 0, 1472, 8193};
    for (size_t message = 0; message < sizeof sizes / sizeof sizes[0]; ++message) {
        size_t size = sizes[message];
        ssize_t sent = sendto(fd, input, size, 0, (struct sockaddr *)&peer, sizeof peer);
        CHECK(sent == (ssize_t)size);
        CHECK(wait_ready(fd, POLLIN) & POLLIN);
        struct sockaddr_in source;
        socklen_t length = sizeof source;
        ssize_t received = recvfrom(fd, output, sizeof output, 0,
            (struct sockaddr *)&source, &length);
        CHECK(received == (ssize_t)size);
        CHECK(length == sizeof source && source.sin_family == AF_INET);
        CHECK(source.sin_addr.s_addr == peer.sin_addr.s_addr && source.sin_port == peer.sin_port);
        for (size_t i = 0; i < size; ++i) CHECK(output[i] == (unsigned char)(input[i] ^ 0xa5));
        printf("network-smoke nic-udp message=%zu sent=%zd received=%zd source-port=%u\n",
            message, sent, received, ntohs(source.sin_port));
    }
    ERROR(sendto(fd, input, sizeof input, 0, (struct sockaddr *)&peer, sizeof peer), EMSGSIZE);
    CHECK(close(fd) == 0);
}

/// 普通路径由 argv 指定 proc 根，实际节点由 VFS 解析，不依赖内核路径特判。
static int proc_open(const char *root, const char *node)
{
    char path[256];
    int length = snprintf(path, sizeof path, "%s/net/%s", root, node);
    CHECK(0 < length && (size_t)length < sizeof path);
    int fd = open(path, O_RDONLY | O_CLOEXEC);
    CHECK(0 <= fd);
    return fd;
}

/// 一份打开快照从头读到真实 EOF，文本容量只覆盖这两个固定小信息文件。
static size_t proc_text(int fd, char *text, size_t capacity)
{
    CHECK(lseek(fd, 0, SEEK_SET) == 0);
    size_t length = 0;
    for (;;) {
        CHECK(length < capacity - 1);
        ssize_t count = read(fd, text + length, capacity - 1 - length);
        CHECK(0 <= count);
        if (count == 0) break;
        length += (size_t)count;
    }
    text[length] = '\0';
    return length;
}

/// dup 的交错顺序读取必须推进同一游标，配置变化后原 OFD 仍交付原全文。
static void proc_unchanged(int fd, const char *expected, size_t length)
{
    char text[2048], byte;
    CHECK(20 <= length && length < sizeof text);
    int alias = dup(fd);
    CHECK(0 <= alias);
    CHECK(lseek(fd, 0, SEEK_SET) == 0);
    read_all(fd, text, 7);
    read_all(alias, text + 7, 13);
    read_all(fd, text + 20, length - 20);
    CHECK(memcmp(text, expected, length) == 0);
    CHECK(read(alias, &byte, 1) == 0);
    CHECK(proc_text(alias, text, sizeof text) == length);
    CHECK(memcmp(text, expected, length) == 0);
    CHECK(close(alias) == 0);
}

/// 标准路由行按十一列解析，地址与 mask 比较本次实际配置对应的 proc 小端值。
static int proc_route_flags(const char *text, const char *interface,
    unsigned long destination, unsigned long gateway, unsigned long mask)
{
    const char *line = strchr(text, '\n');
    CHECK(line != NULL);
    while (*++line) {
        char name[16];
        unsigned long actual_destination, actual_gateway, actual_mask;
        unsigned flags, references, use, metric, mtu, window, irtt;
        CHECK(sscanf(line, "%15s %lx %lx %x %u %u %u %lx %u %u %u",
            name, &actual_destination, &actual_gateway, &flags, &references,
            &use, &metric, &actual_mask, &mtu, &window, &irtt) == 11);
        if (strcmp(name, interface) == 0 && actual_destination == destination) {
            CHECK(actual_gateway == gateway && actual_mask == mask);
            return (int)flags;
        }
        line = strchr(line, '\n');
        CHECK(line != NULL);
    }
    return -1;
}

/// lo 的真实帧计数从标准 dev 行读取，未监测字段不参与业务断言。
static void proc_lo_counts(const char *text, unsigned long long counts[4])
{
    const char *line = strstr(text, "lo:");
    CHECK(line != NULL);
    CHECK(sscanf(line, "lo: %llu %llu %*u %*u %*u %*u %*u %*u %llu %llu",
        &counts[0], &counts[1], &counts[2], &counts[3]) == 4);
}

/// getter 使用本次 socket 的 ioctl 获取真实 IPv4 字段，setter 复用同一头文件布局。
static struct ifreq interface_address(int fd, const char *name,
    unsigned long request, uint32_t expected)
{
    struct ifreq value = {0};
    CHECK(strlen(name) < sizeof value.ifr_name);
    strcpy(value.ifr_name, name);
    CHECK(ioctl(fd, request, &value) == 0);
    const struct sockaddr_in *actual = (const struct sockaddr_in *)&value.ifr_addr;
    CHECK(actual->sin_family == AF_INET && actual->sin_addr.s_addr == htonl(expected));
    return value;
}

/// 配置完成后的两族都传递真实字节，IPv6 不只依赖 bind 或地址常量存在。
static void control_loopback(void)
{
    int client, server;
    pair(&client, &server);
    short_exchange(client, server);
    int receiver = socket(AF_INET6, SOCK_DGRAM, IPPROTO_UDP);
    int sender = socket(AF_INET6, SOCK_DGRAM, IPPROTO_UDP);
    CHECK(0 <= receiver && 0 <= sender);
    struct sockaddr_in6 local = ipv6_address();
    CHECK(bind(receiver, (struct sockaddr *)&local, sizeof local) == 0);
    socklen_t length = sizeof local;
    CHECK(getsockname(receiver, (struct sockaddr *)&local, &length) == 0);
    ipv6_name(&local, length, 0);
    CHECK(sendto(sender, "control-v6", 10, 0, (struct sockaddr *)&local, sizeof local) == 10);
    CHECK(wait_ready(receiver, POLLIN) & POLLIN);
    char text[10];
    struct sockaddr_in6 source;
    length = sizeof source;
    CHECK(recvfrom(receiver, text, sizeof text, 0, (struct sockaddr *)&source, &length) == 10);
    CHECK(memcmp(text, "control-v6", sizeof text) == 0);
    ipv6_name(&source, length, 0);
    CHECK(close(sender) == 0 && close(receiver) == 0);
}

/// 普通 root 消费所选 lo 控制、基础 errno 与真实 proc 快照，再恢复 flags 并验证双族业务。
static void interface_control(const char *proc_root)
{
    control_fd = socket(AF_INET, SOCK_DGRAM, IPPROTO_UDP);
    CHECK(0 <= control_fd);
    memset(&control_lo, 0, sizeof control_lo);
    strcpy(control_lo.ifr_name, "lo");
    CHECK(ioctl(control_fd, SIOCGIFFLAGS, &control_lo) == 0);
    CHECK((control_lo.ifr_flags & (IFF_UP | IFF_LOOPBACK)) == (IFF_UP | IFF_LOOPBACK));
    struct ifreq value = interface_address(control_fd, "lo", SIOCGIFADDR, INADDR_LOOPBACK);
    CHECK(ioctl(control_fd, SIOCSIFADDR, &value) == 0);
    value = interface_address(control_fd, "lo", SIOCGIFNETMASK, 0xff000000);
    CHECK(ioctl(control_fd, SIOCSIFNETMASK, &value) == 0);
    ERROR(ioctl(control_fd, SIOCGIFFLAGS, NULL), EFAULT);
    ((struct sockaddr_in *)&value.ifr_netmask)->sin_addr.s_addr = htonl(0xff00ff00);
    ERROR(ioctl(control_fd, SIOCSIFNETMASK, &value), EINVAL);
    ERROR(ioctl(control_fd, 0x89ff, NULL), ENOTTY);
    interface_address(control_fd, "lo", SIOCGIFADDR, INADDR_LOOPBACK);
    interface_address(control_fd, "lo", SIOCGIFNETMASK, 0xff000000);

    char before[2048], after[2048];
    int old = proc_open(proc_root, "route");
    size_t before_length = proc_text(old, before, sizeof before);
    CHECK(proc_route_flags(before, "lo", 0x0000007f, 0, 0x000000ff) == RTF_UP);
    value = control_lo;
    value.ifr_flags &= ~IFF_UP;
    CHECK(ioctl(control_fd, SIOCSIFFLAGS, &value) == 0);
    control_lo_changed = 1;
    CHECK(ioctl(control_fd, SIOCGIFFLAGS, &value) == 0);
    CHECK((value.ifr_flags & IFF_UP) == 0 && (value.ifr_flags & IFF_LOOPBACK) != 0);
    int fresh = proc_open(proc_root, "route");
    proc_text(fresh, after, sizeof after);
    CHECK(proc_route_flags(after, "lo", 0x0000007f, 0, 0x000000ff) == 0);
    proc_unchanged(old, before, before_length);
    CHECK(close(fresh) == 0 && close(old) == 0);
    CHECK(restore_control() == 0);
    value = control_lo;
    CHECK(ioctl(control_fd, SIOCGIFFLAGS, &value) == 0);
    CHECK(value.ifr_flags == control_lo.ifr_flags);

    old = proc_open(proc_root, "dev");
    before_length = proc_text(old, before, sizeof before);
    unsigned long long first[4], second[4];
    proc_lo_counts(before, first);
    control_loopback();
    fresh = proc_open(proc_root, "dev");
    proc_text(fresh, after, sizeof after);
    proc_lo_counts(after, second);
    for (size_t i = 0; i < 4; ++i) CHECK(first[i] < second[i]);
    proc_unchanged(old, before, before_length);
    CHECK(close(fresh) == 0 && close(old) == 0);
    CHECK(close(control_fd) == 0);
    control_fd = -1;
    printf("network-smoke control lo-get-set=1 up=1 ipv6=1 proc-open-dup=1 errno=1\n");
}

/// 默认项删除影响新路由选择，重复删除给 ESRCH；恢复后原 NIC peer 完成正常全文业务。
static void nic_control(const char *proc_root, unsigned tcp_port)
{
    interface_control(proc_root);
    control_fd = socket(AF_INET, SOCK_DGRAM, IPPROTO_UDP);
    CHECK(0 <= control_fd);
    struct ifreq value = interface_address(control_fd, "eth0", SIOCGIFADDR, 0x0a00020f);
    CHECK(ioctl(control_fd, SIOCSIFADDR, &value) == 0);
    value = interface_address(control_fd, "eth0", SIOCGIFNETMASK, 0xffffff00);
    CHECK(ioctl(control_fd, SIOCSIFNETMASK, &value) == 0);
    memset(&value, 0, sizeof value);
    strcpy(value.ifr_name, "eth0");
    CHECK(ioctl(control_fd, SIOCGIFFLAGS, &value) == 0);
    CHECK((value.ifr_flags & (IFF_UP | IFF_BROADCAST)) == (IFF_UP | IFF_BROADCAST));
    CHECK(ioctl(control_fd, SIOCSIFFLAGS, &value) == 0);
    memset(&control_default, 0, sizeof control_default);
    struct sockaddr_in destination = { .sin_family = AF_INET };
    struct sockaddr_in gateway = nic_peer(0);
    memcpy(&control_default.rt_dst, &destination, sizeof destination);
    memcpy(&control_default.rt_gateway, &gateway, sizeof gateway);
    memcpy(&control_default.rt_genmask, &destination, sizeof destination);
    control_default.rt_flags = RTF_UP | RTF_GATEWAY;
    control_default.rt_dev = "eth0";
    char before[2048], after[2048];
    int old = proc_open(proc_root, "route");
    size_t before_length = proc_text(old, before, sizeof before);
    CHECK(proc_route_flags(before, "eth0", 0, 0x0202000a, 0) == (RTF_UP | RTF_GATEWAY));
    CHECK(ioctl(control_fd, SIOCDELRT, &control_default) == 0);
    control_route_deleted = 1;
    ERROR(ioctl(control_fd, SIOCDELRT, &control_default), ESRCH);
    int fresh = proc_open(proc_root, "route");
    proc_text(fresh, after, sizeof after);
    CHECK(proc_route_flags(after, "eth0", 0, 0x0202000a, 0) == -1);
    CHECK(proc_route_flags(after, "eth0", 0x0002000a, 0, 0x00ffffff) == RTF_UP);
    proc_unchanged(old, before, before_length);
    CHECK(close(fresh) == 0 && close(old) == 0);
    struct sockaddr_in remote = address(tcp_port);
    remote.sin_addr.s_addr = htonl(0xc0000201);
    int routed = socket(AF_INET, SOCK_DGRAM, IPPROTO_UDP);
    CHECK(0 <= routed);
    ERROR(connect(routed, (struct sockaddr *)&remote, sizeof remote), ENETUNREACH);
    CHECK(ioctl(control_fd, SIOCADDRT, &control_default) == 0);
    control_route_deleted = 0;
    fresh = proc_open(proc_root, "route");
    proc_text(fresh, after, sizeof after);
    CHECK(proc_route_flags(after, "eth0", 0, 0x0202000a, 0) == (RTF_UP | RTF_GATEWAY));
    CHECK(close(fresh) == 0);
    CHECK(connect(routed, (struct sockaddr *)&remote, sizeof remote) == 0);
    struct sockaddr_in local;
    socklen_t length = sizeof local;
    CHECK(getsockname(routed, (struct sockaddr *)&local, &length) == 0);
    CHECK(length == sizeof local && local.sin_addr.s_addr == htonl(0x0a00020f));
    CHECK(close(routed) == 0);
    CHECK(restore_control() == 0 && close(control_fd) == 0);
    control_fd = -1;
    nic_tcp(tcp_port);
    printf("network-smoke nic-control default-delete-add=1 missing-esrch=1 route-selection=1\n");
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
    if (strcmp(argv[1], "control") == 0) {
        CHECK(argc == 2 || argc == 3);
        interface_control(argc == 3 ? argv[2] : "/proc");
        printf("network-smoke mode=%s passed\n", argv[1]);
        return 0;
    }
    if (strcmp(argv[1], "nic-control") == 0) {
        CHECK(argc == 4 || argc == 5);
        unsigned tcp_port = service_port(argv[2]);
        (void)service_port(argv[3]);
        nic_control(argc == 5 ? argv[4] : "/proc", tcp_port);
        printf("network-smoke mode=%s passed\n", argv[1]);
        return 0;
    }
    if (strcmp(argv[1], "nic-tcp") == 0 || strcmp(argv[1], "nic-udp") == 0) {
        CHECK(argc == 4);
        unsigned tcp_port = service_port(argv[2]), udp_port = service_port(argv[3]);
        if (strcmp(argv[1], "nic-tcp") == 0) nic_tcp(tcp_port);
        else nic_udp(udp_port);
        printf("network-smoke mode=%s passed\n", argv[1]);
        return 0;
    }
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
    else if (strcmp(argv[1], "ipv6-tcp") == 0) ipv6_tcp();
    else if (strcmp(argv[1], "ipv6-udp") == 0) ipv6_udp();
    else if (strcmp(argv[1], "ipv6-mapped") == 0) { CHECK(argc == 2); ipv6_mapped(); }
    else if (strcmp(argv[1], "buffer-budget") == 0) { CHECK(argc == 2); buffer_budget(); }
    else if (strcmp(argv[1], "protocol-options") == 0) { CHECK(argc == 2); protocol_options(); }
    else if (strcmp(argv[1], "large-loopback") == 0) { CHECK(argc == 2); large_loopback(); }
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
