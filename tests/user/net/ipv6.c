#include "net_common.h"

/// 基础 IPv6 地址只使用真实 sockaddr_in6 的 ::1，flowinfo 与 scope 保零。
struct sockaddr_in6 ipv6_address(void)
{
    struct sockaddr_in6 value = { .sin6_family = AF_INET6,
        .sin6_addr = IN6ADDR_LOOPBACK_INIT };
    return value;
}

/// 名称与消息来源必须为完整 ::1 端点；零端口不能冒充已经绑定的名称。
void ipv6_name(const struct sockaddr_in6 *actual, socklen_t length, uint16_t port)
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
int ipv6_bound(int type, struct sockaddr_in6 *local)
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
void ipv6_tcp(void)
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
void ipv6_udp(void)
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
void ipv6_mapped(void)
{
    mapped_tcp();
    mapped_udp();
    printf("network-smoke ipv6-mapped dual-listener=1 names=28 v6only=1 udp-source=1\n");
}

