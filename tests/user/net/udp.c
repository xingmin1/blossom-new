#include "net_common.h"

/// 绑定 UDP 后用实际名称取得自动端口，后续断言比较同条报文的来源。
int datagram(struct sockaddr_in *local)
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
void datagram_expect(int fd, const char *text, const struct sockaddr_in *sender)
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
void udp_address(void)
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
void udp_message(void)
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
void large_loopback(void)
{
    large_loopback_family(AF_INET);
    large_loopback_family(AF_INET6);
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
void udp_buffer_budget(void)
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

