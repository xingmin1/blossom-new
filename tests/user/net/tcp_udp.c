#define _GNU_SOURCE
#include <arpa/inet.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include "kselftest.h"
#include "selftest_io.h"

static unsigned char datagram_input[65508], datagram_output[65508];

/// 环回端点由系统分配独立端口，名称来自真正的 getsockname。
static int bound_socket(int type, struct sockaddr_in *local)
{
    int fd = -1;
    *local = (struct sockaddr_in){ .sin_family = AF_INET,
        .sin_addr.s_addr = htonl(INADDR_LOOPBACK) };
    fd = socket(AF_INET, type, 0);
    ST_CHECK(0 <= fd);
    ST_CHECK(bind(fd, (struct sockaddr *)local, sizeof(*local)) == 0);
    socklen_t length = sizeof(*local);
    ST_CHECK(getsockname(fd, (struct sockaddr *)local, &length) == 0);
    ST_CHECK(length == sizeof(*local) && local->sin_family == AF_INET && local->sin_port != 0);
    ST_CHECK(local->sin_addr.s_addr == htonl(INADDR_LOOPBACK));
    return fd;
out:
    if (st_close(&fd) != 0)
        fprintf(stderr, "bound socket cleanup errno=%d\n", errno);
    return -1;
}

/// 普通 socket/bind/listen/connect/accept 自建 TCP 连接，不消费协调者预装 fd。
static int tcp_pair(int *client, int *server)
{
    int result = -1, listener = -1;
    struct sockaddr_in local;
    listener = bound_socket(SOCK_STREAM, &local);
    ST_CHECK(0 <= listener);
    ST_CHECK(listen(listener, 8) == 0);
    *client = socket(AF_INET, SOCK_STREAM, 0);
    ST_CHECK(0 <= *client);
    ST_CHECK(connect(*client, (struct sockaddr *)&local, sizeof(local)) == 0);
    *server = accept(listener, NULL, NULL);
    ST_CHECK(0 <= *server);
    result = 0;
out:
    if (st_close(&listener) != 0)
        result = -1;
    return result;
}

/// 两个方向各自在 FIN 后取得 EOF，shutdown 后的发送用 libc errno 核 EPIPE。
static int tcp_short(void)
{
    int result = -1, client = -1, server = -1;
    unsigned char output[32];
    static const unsigned char request[] = { 'r', 0, 0xff, 'q' };
    static const unsigned char response[] = { 'a', 0xfe, 0, 's', 'w' };
    ST_CHECK(tcp_pair(&client, &server) == 0);
    ST_CHECK(st_write_all(client, request, sizeof(request)) == 0);
    ST_CHECK(shutdown(client, SHUT_WR) == 0);
    ST_CHECK(st_read_all(server, output, sizeof(request)) == 0);
    ST_CHECK(memcmp(output, request, sizeof(request)) == 0);
    ST_CHECK(read(server, output, 1) == 0);
    ST_CHECK(st_write_all(server, response, sizeof(response)) == 0);
    ST_CHECK(shutdown(server, SHUT_WR) == 0);
    ST_CHECK(st_read_all(client, output, sizeof(response)) == 0);
    ST_CHECK(memcmp(output, response, sizeof(response)) == 0);
    ST_CHECK(read(client, output, 1) == 0);
    errno = 0;
    ST_CHECK(send(client, "x", 1, MSG_NOSIGNAL) == -1 && errno == EPIPE);
    result = 0;
out:
    if (st_close(&client) != 0)
        result = -1;
    if (st_close(&server) != 0)
        result = -1;
    return result;
}

/// 图样随流中的位置变化，不能把重复前缀、环绕或少读误判为完整收发。
static unsigned char stream_byte(size_t position)
{
    return (unsigned char)(position * 37 + position / 251 + 19);
}

/// 子进程只持发送端，有限大流跨发送环容量并在全部字节后发送 FIN。
static int tcp_writer(int client, int server, size_t total)
{
    unsigned char bytes[997];
    int result = 1;
    ST_CHECK(st_close(&server) == 0);
    size_t done = 0;
    while (done < total) {
        size_t count = total - done;
        if (sizeof(bytes) < count)
            count = sizeof(bytes);
        for (size_t i = 0; i < count; ++i)
            bytes[i] = stream_byte(done + i);
        ST_CHECK(st_write_all(client, bytes, count) == 0);
        done += count;
    }
    ST_CHECK(shutdown(client, SHUT_WR) == 0);
    result = 0;
out:
    if (st_close(&client) != 0)
        result = 1;
    if (st_close(&server) != 0)
        result = 1;
    return result;
}

/// PEEK 不消费数据，真实短读推进全文游标，父 wait 后才释放最后接收端。
static int tcp_stream(void)
{
    int result = -1, client = -1, server = -1;
    pid_t child = -1;
    const size_t total = 131145;
    unsigned char peeked[733], output[113];
    ST_CHECK(tcp_pair(&client, &server) == 0);
    child = fork();
    ST_CHECK(0 <= child);
    if (child == 0)
        _exit(tcp_writer(client, server, total));
    ST_CHECK(st_close(&client) == 0);
    size_t done = 0;
    while (done < total) {
        ssize_t peek_count = recv(server, peeked, sizeof(peeked), MSG_PEEK);
        if (peek_count == -1 && errno == EINTR)
            continue;
        ST_CHECK(0 < peek_count && (size_t)peek_count <= total - done);
        for (size_t i = 0; i < (size_t)peek_count; ++i)
            ST_CHECK(peeked[i] == stream_byte(done + i));
        size_t wanted = (size_t)peek_count;
        if (sizeof(output) < wanted)
            wanted = sizeof(output);
        ssize_t count = read(server, output, wanted);
        if (count == -1 && errno == EINTR)
            continue;
        ST_CHECK(0 < count && (size_t)count <= wanted);
        ST_CHECK(memcmp(output, peeked, (size_t)count) == 0);
        done += (size_t)count;
    }
    ST_CHECK(read(server, output, 1) == 0);
    ST_CHECK(st_reap(&child, 0) == 0);
    result = 0;
out:
    if (st_stop_child(&child) != 0)
        result = -1;
    if (st_close(&client) != 0)
        result = -1;
    if (st_close(&server) != 0)
        result = -1;
    return result;
}

/// 单次 recvfrom 只消费一条消息，连同长度、全文和该记录的来源一起比较。
static int udp_receive(int fd, const void *expected, size_t size,
                       const struct sockaddr_in *sender)
{
    int result = -1;
    struct sockaddr_in source;
    socklen_t length = sizeof(source);
    ST_CHECK(recvfrom(fd, datagram_output, sizeof(datagram_output), 0,
                      (struct sockaddr *)&source, &length) == (ssize_t)size);
    ST_CHECK(memcmp(expected, datagram_output, size) == 0);
    ST_CHECK(length == sizeof(source) && source.sin_family == AF_INET);
    ST_CHECK(source.sin_port == sender->sin_port && source.sin_addr.s_addr == sender->sin_addr.s_addr);
    result = 0;
out:
    return result;
}

/// UDP 的零消息、截短、PEEK、大消息与错误均由独立环回端点产生和消费。
static int udp_messages(void)
{
    int result = -1, sender = -1, receiver = -1;
    struct sockaddr_in own, destination;
    unsigned char prefix[4];
    sender = bound_socket(SOCK_DGRAM, &own);
    ST_CHECK(0 <= sender);
    receiver = bound_socket(SOCK_DGRAM, &destination);
    ST_CHECK(0 <= receiver);
    errno = 0;
    ST_CHECK(send(sender, "x", 1, 0) == -1 && errno == EDESTADDRREQ);
    ST_CHECK(connect(sender, (struct sockaddr *)&destination, sizeof(destination)) == 0);
    ST_CHECK(send(sender, "", 0, 0) == 0);
    ST_CHECK(send(sender, "next", 4, 0) == 4);
    ST_CHECK(udp_receive(receiver, "", 0, &own) == 0);
    ST_CHECK(udp_receive(receiver, "next", 4, &own) == 0);
    ST_CHECK(send(sender, "short-tail", 10, 0) == 10);
    ST_CHECK(send(sender, "kept", 4, 0) == 4);
    ST_CHECK(recv(receiver, prefix, 3, 0) == 3 && memcmp(prefix, "sho", 3) == 0);
    ST_CHECK(udp_receive(receiver, "kept", 4, &own) == 0);
    ST_CHECK(send(sender, "peek-full", 9, 0) == 9);
    ST_CHECK(recv(receiver, prefix, sizeof(prefix), MSG_PEEK | MSG_TRUNC) == 9);
    ST_CHECK(memcmp(prefix, "peek", sizeof(prefix)) == 0);
    ST_CHECK(udp_receive(receiver, "peek-full", 9, &own) == 0);
    for (size_t i = 0; i < sizeof(datagram_input); ++i)
        datagram_input[i] = stream_byte(i);
    const size_t sizes[] = { 2049, 65507 };
    for (size_t i = 0; i < sizeof(sizes) / sizeof(sizes[0]); ++i) {
        ST_CHECK(send(sender, datagram_input, sizes[i], 0) == (ssize_t)sizes[i]);
        ST_CHECK(udp_receive(receiver, datagram_input, sizes[i], &own) == 0);
    }
    errno = 0;
    ST_CHECK(send(sender, datagram_input, sizeof(datagram_input), 0) == -1 && errno == EMSGSIZE);
    errno = 0;
    ST_CHECK(recv(receiver, prefix, sizeof(prefix), MSG_DONTWAIT) == -1 && errno == EAGAIN);
    ST_CHECK(send(sender, "after-error", 11, 0) == 11);
    ST_CHECK(udp_receive(receiver, "after-error", 11, &own) == 0);
    result = 0;
out:
    if (st_close(&sender) != 0)
        result = -1;
    if (st_close(&receiver) != 0)
        result = -1;
    return result;
}

/// 三项默认 TAP 只覆盖 Linux 同源可运行语义，NIC 和协议私有观察仍由内核 fixture 保留。
int main(int argc, char **argv)
{
    ksft_print_header();
    if (argc != 1)
        ksft_exit_fail_msg("usage: %s\n", argv[0]);
    ksft_set_plan(3);
    ksft_test_result(tcp_short() == 0, "loopback TCP binary request response half-close EOF and EPIPE\n");
    ksft_test_result(tcp_stream() == 0, "fork TCP full stream short reads PEEK FIN and child cleanup\n");
    ksft_test_result(udp_messages() == 0, "loopback UDP source zero truncate PEEK maximum boundary and errors\n");
    ksft_finished();
}
