#include "net_common.h"

/// int 选项通过实际 syscall 查询，不用本地保存的设置值充当内核事实。
int option(int fd, int level, int name)
{
    int value = -1;
    socklen_t length = sizeof value;
    CHECK(getsockopt(fd, level, name, &value, &length) == 0 && length == sizeof value);
    return value;
}

/// 双方 reuse 才允许重叠，listener 仍互斥，UDP 查找真选 connected，TCP child 继承独立选项。
void socket_options(void)
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

/// int 设置沿正式 ABI 提交，查询断言由各行为单独消费。
void set_option(int fd, int level, int name, int value)
{
    CHECK(setsockopt(fd, level, name, &value, sizeof value) == 0);
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
void protocol_options(void)
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
void buffer_hint(int fd, int name, int hint, int expected)
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

/// TCP 流满后的接受恢复与 UDP 单消息接纳分别消费同一正式预算接口。
void buffer_budget(void)
{
    tcp_buffer_budget();
    udp_buffer_budget();
}

