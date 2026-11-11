#define _GNU_SOURCE
#include <arpa/inet.h>
#include <time.h>
#include <linux/errqueue.h>
#include <net/if.h>
#include <net/route.h>
#include <netinet/ip_icmp.h>
#include <poll.h>
#include <stddef.h>
#include <stdint.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include "kselftest.h"
#include "selftest_io.h"

/*
 * 测试使用独立的 IPv4 UDP 回环端点，要求 lo 可用且本机允许生成 ICMP 端口不可达。
 * 目标端口存活时先绑定不同的源端口，再关闭目标，避免自动绑定占用原目标端口。
 * 网关子项只查询现有配置并执行 UDP connect；发往非直连地址的数据必须被拒绝。
 * 测试不修改接口、路由或防火墙，错误队列缺失或 ICMP 被过滤均按失败报告。
 */

_Static_assert(sizeof(long) == 8 && sizeof(struct cmsghdr) == 16,
               "IPv4 error tests require LP64 socket ABI");
_Static_assert(sizeof(struct sock_extended_err) == 16 && sizeof(struct sockaddr_in) == 16,
               "unexpected IPv4 extended error ABI");
_Static_assert(sizeof(struct iovec) == 16 && offsetof(struct iovec, iov_len) == 8,
               "unexpected LP64 iovec ABI");
_Static_assert(sizeof(struct msghdr) == 56 &&
               offsetof(struct msghdr, msg_name) == 0 &&
               offsetof(struct msghdr, msg_namelen) == 8 &&
               offsetof(struct msghdr, msg_iov) == 16 &&
               offsetof(struct msghdr, msg_iovlen) == 24 &&
               offsetof(struct msghdr, msg_control) == 32 &&
               offsetof(struct msghdr, msg_controllen) == 40 &&
               offsetof(struct msghdr, msg_flags) == 48,
               "unexpected LP64 message ABI");

static const unsigned char payload[] = {
    0x69, 0x70, 0x76, 0x34, 0x00, 0xff, 0x75, 0x64,
    0x70, 0x2d, 0x65, 0x72, 0x72, 0x6f, 0x72, 0x00,
    0x81, 0x39, 0xa7, 0x52, 0x10, 0x00, 0xfe, 0x63,
};

#define ERROR_DATA_SIZE (sizeof(struct sock_extended_err) + sizeof(struct sockaddr_in))
#define ERROR_CONTROL_SIZE CMSG_SPACE(ERROR_DATA_SIZE)

/// 描述符持有独立端口，返回的地址和端口均来自真实的 getsockname。
static int bound_udp(uint32_t address, struct sockaddr_in *local)
{
    int fd = -1;
    *local = (struct sockaddr_in){ .sin_family = AF_INET,
        .sin_addr.s_addr = htonl(address) };
    fd = socket(AF_INET, SOCK_DGRAM | SOCK_NONBLOCK | SOCK_CLOEXEC, 0);
    ST_CHECK(0 <= fd);
    ST_CHECK(bind(fd, (struct sockaddr *)local, sizeof(*local)) == 0);
    socklen_t length = sizeof(*local);
    ST_CHECK(getsockname(fd, (struct sockaddr *)local, &length) == 0);
    ST_CHECK(length == sizeof(*local) && local->sin_family == AF_INET);
    ST_CHECK(local->sin_port != 0 && local->sin_addr.s_addr == htonl(address));
    return fd;
out:
    if (st_close(&fd) != 0)
        fprintf(stderr, "bound UDP cleanup errno=%d\n", errno);
    return -1;
}

/// 先占有目标和源端口，再关闭目标；发送端不会把关闭的目标端口选作自己的源端口。
static int closed_destination(int connected, struct sockaddr_in *destination)
{
    int sender = -1, target = -1;
    struct sockaddr_in source;
    target = bound_udp(INADDR_LOOPBACK, destination);
    ST_CHECK(0 <= target);
    sender = bound_udp(INADDR_LOOPBACK, &source);
    ST_CHECK(0 <= sender && source.sin_port != destination->sin_port);
    if (connected)
        ST_CHECK(connect(sender, (struct sockaddr *)destination, sizeof(*destination)) == 0);
    ST_CHECK(st_close(&target) == 0);
    return sender;
out:
    if (st_close(&target) != 0)
        fprintf(stderr, "target UDP cleanup errno=%d\n", errno);
    if (st_close(&sender) != 0)
        fprintf(stderr, "sender UDP cleanup errno=%d\n", errno);
    return -1;
}

/// 等待真实就绪事件，EINTR 重入沿同一单调时钟期限，不延长等待时间。
static int wait_ready(int fd, short events, short expected)
{
    int result = -1;
    struct timespec now;
    ST_CHECK(clock_gettime(CLOCK_MONOTONIC, &now) == 0);
    int64_t deadline = (int64_t)now.tv_sec * 1000 + now.tv_nsec / 1000000 + 3000;
    struct pollfd ready = { .fd = fd, .events = events };
    for (;;) {
        ST_CHECK(clock_gettime(CLOCK_MONOTONIC, &now) == 0);
        int64_t remaining = deadline - ((int64_t)now.tv_sec * 1000 + now.tv_nsec / 1000000);
        if (remaining <= 0) {
            errno = ETIMEDOUT;
            ST_CHECK(0);
        }
        int count = poll(&ready, 1, (int)remaining);
        if (count == -1 && errno == EINTR)
            continue;
        if (count == 0)
            errno = ETIMEDOUT;
        if (count != 1 || ready.revents != expected)
            ksft_print_msg("poll count=%d events=%#x expected=%#x\n",
                           count, (unsigned short)ready.revents, (unsigned short)expected);
        ST_CHECK(count == 1 && ready.revents == expected);
        break;
    }
    result = 0;
out:
    return result;
}

/// 数据和错误均已消费时，未请求的 POLLERR 也必须从就绪结果中消失。
static int no_ready(int fd)
{
    int result = -1;
    struct pollfd ready = { .fd = fd, .events = POLLIN };
    ST_CHECK(poll(&ready, 1, 0) == 0 && ready.revents == 0);
    result = 0;
out:
    return result;
}

/// SO_ERROR 消费真实错误；非零预期允许等待错误入队之后独立发表的 socket 错误。
static int socket_error(int fd, int expected)
{
    int result = -1;
    struct timespec now;
    ST_CHECK(clock_gettime(CLOCK_MONOTONIC, &now) == 0);
    int64_t deadline = (int64_t)now.tv_sec * 1000 + now.tv_nsec / 1000000 + 3000;
    for (;;) {
        int value = -1;
        socklen_t length = sizeof(value);
        ST_CHECK(getsockopt(fd, SOL_SOCKET, SO_ERROR, &value, &length) == 0);
        ST_CHECK(length == sizeof(value));
        if (value == expected)
            break;
        if (expected == 0 || value != 0) {
            ksft_print_msg("SO_ERROR value=%d expected=%d\n", value, expected);
            ST_CHECK(value == expected);
        }
        ST_CHECK(clock_gettime(CLOCK_MONOTONIC, &now) == 0);
        if (deadline <= (int64_t)now.tv_sec * 1000 + now.tv_nsec / 1000000) {
            errno = ETIMEDOUT;
            ST_CHECK(value == expected);
        }
        int paused = poll(NULL, 0, 1);
        ST_CHECK(paused == 0 || (paused == -1 && errno == EINTR));
    }
    result = 0;
out:
    return result;
}

/// 空错误队列始终通过 MSG_ERRQUEUE 返回 EAGAIN，不消费独立的 SO_ERROR。
static int empty_queue(int fd)
{
    int result = -1;
    unsigned char data[sizeof(payload)];
    struct iovec vector = { .iov_base = data, .iov_len = sizeof(data) };
    struct msghdr message = { .msg_iov = &vector, .msg_iovlen = 1 };
    errno = 0;
    ST_CHECK(recvmsg(fd, &message, MSG_ERRQUEUE | MSG_DONTWAIT) == -1 && errno == EAGAIN);
    result = 0;
out:
    return result;
}

/// 单次发送保留二进制载荷，connected 和 unconnected 分别使用 send 与 sendto。
static int send_payload(int fd, int connected, const struct sockaddr_in *destination)
{
    int result = -1;
    ssize_t count;
    if (connected)
        count = send(fd, payload, sizeof(payload), 0);
    else
        count = sendto(fd, payload, sizeof(payload), 0,
                       (const struct sockaddr *)destination, sizeof(*destination));
    ST_CHECK(count == (ssize_t)sizeof(payload));
    result = 0;
out:
    return result;
}

/// 只有关闭端口发回的真实错误才满足等待，错误队列不能被当作普通接收数据。
static int queue_error(int fd, int connected, const struct sockaddr_in *destination)
{
    int result = -1;
    ST_CHECK(send_payload(fd, connected, destination) == 0);
    ST_CHECK(wait_ready(fd, POLLIN, POLLERR) == 0);
    result = 0;
out:
    return result;
}

/// 消费并核对一条错误；要求返回载荷且队列为空时返回 1。
static int receive_error_data(int fd, const struct sockaddr_in *destination,
                              size_t data_capacity, size_t control_capacity, int flags,
                              unsigned char *received)
{
    int result = -1;
    unsigned char data[sizeof(payload) + 1];
    union {
        struct cmsghdr alignment;
        unsigned char bytes[ERROR_CONTROL_SIZE];
    } control;
    struct sockaddr_in name;
    ST_CHECK(data_capacity <= sizeof(payload) && control_capacity <= sizeof(control.bytes));
    memset(data, 0xa5, sizeof(data));
    memset(&control, 0xa5, sizeof(control));
    memset(&name, 0xa5, sizeof(name));
    struct iovec vector = { .iov_base = data, .iov_len = data_capacity };
    struct msghdr message = {
        .msg_name = &name, .msg_namelen = sizeof(name),
        .msg_iov = &vector, .msg_iovlen = 1,
        .msg_control = control_capacity ? control.bytes : NULL,
        .msg_controllen = control_capacity,
    };
    ssize_t count = recvmsg(fd, &message, MSG_ERRQUEUE | MSG_DONTWAIT | flags);
    if (count == -1 && errno == EAGAIN && received != NULL) {
        result = 1;
        goto out;
    }
    ST_CHECK(count == (ssize_t)data_capacity);
    if (received == NULL)
        ST_CHECK(memcmp(data, payload, data_capacity) == 0);
    for (size_t i = data_capacity; i < sizeof(data); ++i)
        ST_CHECK(data[i] == 0xa5);
    ST_CHECK(message.msg_namelen == sizeof(name));
    ST_CHECK(memcmp(&name, destination, sizeof(name)) == 0);
    int expected_flags = MSG_ERRQUEUE;
    if (data_capacity < sizeof(payload))
        expected_flags |= MSG_TRUNC;
    if (control_capacity < CMSG_LEN(ERROR_DATA_SIZE))
        expected_flags |= MSG_CTRUNC;
    ST_CHECK(message.msg_flags == expected_flags);
    size_t used = control_capacity < sizeof(struct cmsghdr) ? 0 : control_capacity;
    ST_CHECK(message.msg_controllen == used);
    for (size_t i = control_capacity; i < sizeof(control.bytes); ++i)
        ST_CHECK(control.bytes[i] == 0xa5);
    if (used != 0) {
        struct cmsghdr *header = CMSG_FIRSTHDR(&message);
        ST_CHECK(header != NULL && header->cmsg_level == IPPROTO_IP);
        ST_CHECK(header->cmsg_type == IP_RECVERR);
        ST_CHECK(header->cmsg_len == used && CMSG_NXTHDR(&message, header) == NULL);
        if (CMSG_LEN(sizeof(struct sock_extended_err)) <= used) {
            struct sock_extended_err *error = (void *)CMSG_DATA(header);
            if (error->ee_errno != ECONNREFUSED || error->ee_origin != SO_EE_ORIGIN_ICMP ||
                error->ee_type != ICMP_DEST_UNREACH || error->ee_code != ICMP_PORT_UNREACH)
                ksft_print_msg("extended error errno=%u origin=%u type=%u code=%u\n",
                               error->ee_errno, error->ee_origin, error->ee_type, error->ee_code);
            ST_CHECK(error->ee_errno == ECONNREFUSED && error->ee_origin == SO_EE_ORIGIN_ICMP);
            ST_CHECK(error->ee_type == ICMP_DEST_UNREACH && error->ee_code == ICMP_PORT_UNREACH);
            ST_CHECK(error->ee_pad == 0 && error->ee_info == 0 && error->ee_data == 0);
            if (CMSG_LEN(ERROR_DATA_SIZE) <= used) {
                const struct sockaddr_in *offender = (const void *)SO_EE_OFFENDER(error);
                struct sockaddr_in expected = { .sin_family = AF_INET,
                    .sin_addr.s_addr = htonl(INADDR_LOOPBACK) };
                ST_CHECK(memcmp(offender, &expected, sizeof(expected)) == 0);
            }
        }
    }
    if (received != NULL)
        memcpy(received, data, data_capacity);
    result = 0;
out:
    return result;
}

/// 固定载荷的测试复用相同的错误记录和截断检查。
static int receive_error(int fd, const struct sockaddr_in *destination,
                         size_t data_capacity, size_t control_capacity, int flags)
{
    return receive_error_data(fd, destination, data_capacity, control_capacity, flags, NULL);
}

/// 清除 SO_ERROR、普通 recv 和直接消费错误队列分别检查其独立的消费语义。
static int error_delivery(int connected)
{
    int result = -1, sender = -1, enabled = 1;
    struct sockaddr_in destination;
    unsigned char data[sizeof(payload)];
    sender = closed_destination(connected, &destination);
    ST_CHECK(0 <= sender);
    ST_CHECK(empty_queue(sender) == 0 && socket_error(sender, 0) == 0 && no_ready(sender) == 0);
    ST_CHECK(setsockopt(sender, IPPROTO_IP, IP_RECVERR, &enabled, sizeof(enabled)) == 0);
    ST_CHECK(empty_queue(sender) == 0);

    ST_CHECK(queue_error(sender, connected, &destination) == 0);
    ST_CHECK(socket_error(sender, ECONNREFUSED) == 0 && socket_error(sender, 0) == 0);
    ST_CHECK(wait_ready(sender, POLLIN, POLLERR) == 0);
    errno = 0;
    ST_CHECK(recv(sender, data, sizeof(data), MSG_DONTWAIT) == -1 && errno == EAGAIN);
    ST_CHECK(receive_error(sender, &destination, sizeof(payload), ERROR_CONTROL_SIZE, 0) == 0);
    ST_CHECK(empty_queue(sender) == 0 && socket_error(sender, 0) == 0 && no_ready(sender) == 0);

    ST_CHECK(queue_error(sender, connected, &destination) == 0);
    errno = 0;
    ST_CHECK(recv(sender, data, sizeof(data), MSG_DONTWAIT) == -1 && errno == ECONNREFUSED);
    ST_CHECK(socket_error(sender, 0) == 0 && wait_ready(sender, POLLIN, POLLERR) == 0);
    ST_CHECK(receive_error(sender, &destination, sizeof(payload), ERROR_CONTROL_SIZE, 0) == 0);
    ST_CHECK(empty_queue(sender) == 0 && no_ready(sender) == 0);

    ST_CHECK(queue_error(sender, connected, &destination) == 0);
    ST_CHECK(receive_error(sender, &destination, sizeof(payload), ERROR_CONTROL_SIZE, 0) == 0);
    ST_CHECK(socket_error(sender, 0) == 0 && empty_queue(sender) == 0 && no_ready(sender) == 0);
    result = 0;
out:
    if (st_close(&sender) != 0)
        result = -1;
    return result;
}

/// 关闭 IP_RECVERR 丢弃已存在的错误记录，但原 SO_ERROR 仍须单独消费。
static int error_purge(int connected)
{
    int result = -1, sender = -1, enabled = 1;
    struct sockaddr_in destination;
    sender = closed_destination(connected, &destination);
    ST_CHECK(0 <= sender);
    ST_CHECK(setsockopt(sender, IPPROTO_IP, IP_RECVERR, &enabled, sizeof(enabled)) == 0);
    ST_CHECK(queue_error(sender, connected, &destination) == 0);
    ST_CHECK(setsockopt(sender, IPPROTO_IP, IP_RECVERR, NULL, 0) == 0);
    ST_CHECK(empty_queue(sender) == 0 && wait_ready(sender, POLLIN, POLLERR) == 0);
    ST_CHECK(socket_error(sender, ECONNREFUSED) == 0 && socket_error(sender, 0) == 0);
    ST_CHECK(no_ready(sender) == 0);
    ST_CHECK(setsockopt(sender, IPPROTO_IP, IP_RECVERR, &enabled, sizeof(enabled)) == 0);
    ST_CHECK(queue_error(sender, connected, &destination) == 0);
    ST_CHECK(receive_error(sender, &destination, sizeof(payload), ERROR_CONTROL_SIZE, 0) == 0);
    ST_CHECK(socket_error(sender, 0) == 0 && empty_queue(sender) == 0 && no_ready(sender) == 0);
    result = 0;
out:
    if (st_close(&sender) != 0)
        result = -1;
    return result;
}

/// 截短和零容量 iov 都消费真实记录；输入 MSG_TRUNC 不把返回值改成原 UDP 长度。
static int payload_truncation(void)
{
    int result = -1, sender = -1, enabled = 1;
    struct sockaddr_in destination;
    const size_t capacities[] = { 3, 3, 0 };
    const int flags[] = { 0, MSG_TRUNC, 0 };
    sender = closed_destination(0, &destination);
    ST_CHECK(0 <= sender);
    ST_CHECK(setsockopt(sender, IPPROTO_IP, IP_RECVERR, &enabled, sizeof(enabled)) == 0);
    for (size_t i = 0; i < sizeof(capacities) / sizeof(capacities[0]); ++i) {
        ST_CHECK(queue_error(sender, 0, &destination) == 0);
        ST_CHECK(receive_error(sender, &destination, capacities[i], ERROR_CONTROL_SIZE, flags[i]) == 0);
        ST_CHECK(empty_queue(sender) == 0 && socket_error(sender, 0) == 0 && no_ready(sender) == 0);
    }
    result = 0;
out:
    if (st_close(&sender) != 0)
        result = -1;
    return result;
}

/// 缺失或截短的控制缓冲区返回 MSG_CTRUNC，不能把记录留在错误队列中。
static int control_truncation(void)
{
    int result = -1, sender = -1, enabled = 1;
    struct sockaddr_in destination;
    const size_t capacities[] = { 0, sizeof(struct cmsghdr) - 1,
        CMSG_LEN(sizeof(struct sock_extended_err)) };
    sender = closed_destination(1, &destination);
    ST_CHECK(0 <= sender);
    ST_CHECK(setsockopt(sender, IPPROTO_IP, IP_RECVERR, &enabled, sizeof(enabled)) == 0);
    for (size_t i = 0; i < sizeof(capacities) / sizeof(capacities[0]); ++i) {
        ST_CHECK(queue_error(sender, 1, &destination) == 0);
        ST_CHECK(receive_error(sender, &destination, sizeof(payload), capacities[i], 0) == 0);
        ST_CHECK(empty_queue(sender) == 0 && socket_error(sender, 0) == 0 && no_ready(sender) == 0);
    }
    result = 0;
out:
    if (st_close(&sender) != 0)
        result = -1;
    return result;
}

/// IP 层布尔查询的短缓冲区只返回一个字节，SOL_SOCKET 保留调用者请求的短长度。
static int option_get_lengths(int fd, int level, int option, int expected)
{
    int result = -1;
    const socklen_t capacities[] = { 0, 1, 3, 4 };
    for (size_t i = 0; i < sizeof(capacities) / sizeof(capacities[0]); ++i) {
        unsigned char bytes[sizeof(int)];
        memset(bytes, 0xa5, sizeof(bytes));
        socklen_t length = capacities[i];
        socklen_t copied = level == IPPROTO_IP && 0 < length && length < sizeof(int) ? 1 : length;
        ST_CHECK(getsockopt(fd, level, option, bytes, &length) == 0 && length == copied);
        ST_CHECK(memcmp(bytes, &expected, copied) == 0);
        for (size_t j = copied; j < sizeof(bytes); ++j)
            ST_CHECK(bytes[j] == 0xa5);
    }
    result = 0;
out:
    return result;
}

/// 选项长度区别通过真实错误交付和队列清除验证，查询只补充复制长度的 ABI 断言。
static int option_lengths(void)
{
    int result = -1, sender = -1, enabled = 1;
    struct sockaddr_in destination;
    const socklen_t lengths[] = { 1, 3, 4 };
    unsigned char short_value[] = { 0x80, 0x00, 0x00 };
    int integer = 256;
    sender = closed_destination(0, &destination);
    ST_CHECK(0 <= sender);
    ST_CHECK(option_get_lengths(sender, IPPROTO_IP, IP_RECVERR, 0) == 0);
    ST_CHECK(option_get_lengths(sender, SOL_SOCKET, SO_DONTROUTE, 0) == 0);
    ST_CHECK(setsockopt(sender, SOL_SOCKET, SO_DONTROUTE, &enabled, sizeof(enabled)) == 0);
    ST_CHECK(option_get_lengths(sender, SOL_SOCKET, SO_DONTROUTE, 1) == 0);
    for (socklen_t length = 0; length < sizeof(int); ++length) {
        if (length == 2)
            continue;
        errno = 0;
        ST_CHECK(setsockopt(sender, SOL_SOCKET, SO_DONTROUTE, &enabled, length) == -1 && errno == EINVAL);
    }
    for (size_t i = 0; i < sizeof(lengths) / sizeof(lengths[0]); ++i) {
        const void *value = lengths[i] == sizeof(int) ? (const void *)&integer : short_value;
        ST_CHECK(setsockopt(sender, IPPROTO_IP, IP_RECVERR, value, lengths[i]) == 0);
        ST_CHECK(option_get_lengths(sender, IPPROTO_IP, IP_RECVERR, 1) == 0);
        ST_CHECK(queue_error(sender, 0, &destination) == 0);
        ST_CHECK(receive_error(sender, &destination, sizeof(payload), ERROR_CONTROL_SIZE, 0) == 0);
        ST_CHECK(empty_queue(sender) == 0 && socket_error(sender, 0) == 0 && no_ready(sender) == 0);
    }

    ST_CHECK(queue_error(sender, 0, &destination) == 0);
    unsigned char zero_first[] = { 0, 1, 1 };
    ST_CHECK(setsockopt(sender, IPPROTO_IP, IP_RECVERR, zero_first, sizeof(zero_first)) == 0);
    ST_CHECK(option_get_lengths(sender, IPPROTO_IP, IP_RECVERR, 0) == 0);
    ST_CHECK(empty_queue(sender) == 0 && socket_error(sender, ECONNREFUSED) == 0);
    ST_CHECK(socket_error(sender, 0) == 0 && no_ready(sender) == 0);

    ST_CHECK(setsockopt(sender, IPPROTO_IP, IP_RECVERR, &enabled, sizeof(enabled)) == 0);
    ST_CHECK(queue_error(sender, 0, &destination) == 0);
    ST_CHECK(setsockopt(sender, IPPROTO_IP, IP_RECVERR, NULL, 0) == 0);
    ST_CHECK(empty_queue(sender) == 0 && socket_error(sender, ECONNREFUSED) == 0);
    ST_CHECK(option_get_lengths(sender, IPPROTO_IP, IP_RECVERR, 0) == 0 && no_ready(sender) == 0);
    result = 0;
out:
    if (st_close(&sender) != 0)
        result = -1;
    return result;
}

/// 保护页区分首字节读取、完整 int 读取及两个选项层不同的输出复制顺序。
static int option_copy_boundaries(void)
{
    int result = -1, sender = -1, enabled = 1;
    long page = sysconf(_SC_PAGESIZE);
    unsigned char *mapping = MAP_FAILED;
    struct sockaddr_in destination;
    ST_CHECK(0 < page);
    mapping = mmap(NULL, 2 * (size_t)page, PROT_READ | PROT_WRITE,
                   MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ST_CHECK(mapping != MAP_FAILED);
    unsigned char *guard = mapping + page, *edge = guard - 1;
    ST_CHECK(mprotect(guard, (size_t)page, PROT_NONE) == 0);
    sender = closed_destination(0, &destination);
    ST_CHECK(0 <= sender);
    ST_CHECK(setsockopt(sender, SOL_SOCKET, SO_DONTROUTE, &enabled, sizeof(enabled)) == 0);
    *edge = 1;
    ST_CHECK(setsockopt(sender, IPPROTO_IP, IP_RECVERR, edge, 3) == 0);
    errno = 0;
    ST_CHECK(setsockopt(sender, IPPROTO_IP, IP_RECVERR, edge, 4) == -1 && errno == EFAULT);
    errno = 0;
    ST_CHECK(setsockopt(sender, IPPROTO_IP, IP_RECVERR, guard, 1) == -1 && errno == EFAULT);
    ST_CHECK(queue_error(sender, 0, &destination) == 0);
    ST_CHECK(receive_error(sender, &destination, sizeof(payload), ERROR_CONTROL_SIZE, 0) == 0);

    for (socklen_t length = 0; length < sizeof(int); ++length) {
        if (length == 2)
            continue;
        errno = 0;
        ST_CHECK(setsockopt(sender, SOL_SOCKET, SO_DONTROUTE, guard, length) == -1 && errno == EINVAL);
    }
    errno = 0;
    ST_CHECK(setsockopt(sender, SOL_SOCKET, SO_DONTROUTE, edge, 4) == -1 && errno == EFAULT);
    ST_CHECK(option_get_lengths(sender, SOL_SOCKET, SO_DONTROUTE, 1) == 0);

    socklen_t length = 3;
    *edge = 0xa5;
    ST_CHECK(getsockopt(sender, IPPROTO_IP, IP_RECVERR, edge, &length) == 0);
    ST_CHECK(length == 1 && *edge == 1);
    length = 3;
    errno = 0;
    ST_CHECK(getsockopt(sender, IPPROTO_IP, IP_RECVERR, guard, &length) == -1 && errno == EFAULT);
    ST_CHECK(length == 1);
    length = 4;
    errno = 0;
    ST_CHECK(getsockopt(sender, IPPROTO_IP, IP_RECVERR, edge, &length) == -1 && errno == EFAULT);
    ST_CHECK(length == 4);
    length = 3;
    errno = 0;
    ST_CHECK(getsockopt(sender, SOL_SOCKET, SO_DONTROUTE, edge, &length) == -1 && errno == EFAULT);
    ST_CHECK(length == 3);
    length = 0;
    ST_CHECK(getsockopt(sender, IPPROTO_IP, IP_RECVERR, guard, &length) == 0 && length == 0);
    ST_CHECK(getsockopt(sender, SOL_SOCKET, SO_DONTROUTE, guard, &length) == 0 && length == 0);
    int value = -1;
    errno = 0;
    ST_CHECK(getsockopt(sender, IPPROTO_IP, IP_RECVERR, &value, (socklen_t *)guard) == -1 && errno == EFAULT);
    ST_CHECK(value == -1);

    ST_CHECK(queue_error(sender, 0, &destination) == 0);
    ST_CHECK(setsockopt(sender, IPPROTO_IP, IP_RECVERR, guard, 0) == 0);
    ST_CHECK(empty_queue(sender) == 0 && socket_error(sender, ECONNREFUSED) == 0);
    ST_CHECK(no_ready(sender) == 0);
    ST_CHECK(setsockopt(sender, IPPROTO_IP, IP_RECVERR, &enabled, sizeof(enabled)) == 0);

    socklen_t *read_only_length = (socklen_t *)(guard - sizeof(socklen_t));
    *read_only_length = 3;
    ST_CHECK(mprotect(mapping, (size_t)page, PROT_READ) == 0);
    unsigned char output[sizeof(int)];
    memset(output, 0xa5, sizeof(output));
    errno = 0;
    ST_CHECK(getsockopt(sender, IPPROTO_IP, IP_RECVERR, output, read_only_length) == -1 && errno == EFAULT);
    for (size_t i = 0; i < sizeof(output); ++i)
        ST_CHECK(output[i] == 0xa5);
    errno = 0;
    ST_CHECK(getsockopt(sender, SOL_SOCKET, SO_DONTROUTE, output, read_only_length) == -1 && errno == EFAULT);
    ST_CHECK(memcmp(output, &enabled, 3) == 0 && output[3] == 0xa5);
    ST_CHECK(*read_only_length == 3);
    result = 0;
out:
    if (st_close(&sender) != 0)
        result = -1;
    if (mapping != MAP_FAILED && munmap(mapping, 2 * (size_t)page) != 0)
        result = -1;
    return result;
}

/// 直接使用 recvmsg 系统调用核对各复制阶段及其队列消费点。
static int receive_copy_boundaries(void)
{
    int result = -1, sender = -1, enabled = 1;
    long page = sysconf(_SC_PAGESIZE);
    void *guard = MAP_FAILED;
    struct sockaddr_in destination;
    ST_CHECK(0 < page);
    guard = mmap(NULL, (size_t)page, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ST_CHECK(guard != MAP_FAILED);
    sender = closed_destination(0, &destination);
    ST_CHECK(0 <= sender);
    ST_CHECK(setsockopt(sender, IPPROTO_IP, IP_RECVERR, &enabled, sizeof(enabled)) == 0);
    for (int stage = 0; stage < 5; ++stage) {
        unsigned char data[sizeof(payload)];
        union {
            struct cmsghdr alignment;
            unsigned char bytes[ERROR_CONTROL_SIZE];
        } control;
        struct sockaddr_in name;
        struct iovec vector = { .iov_base = data, .iov_len = sizeof(data) };
        struct msghdr message = {
            .msg_name = &name, .msg_namelen = sizeof(name),
            .msg_iov = &vector, .msg_iovlen = 1,
            .msg_control = control.bytes, .msg_controllen = sizeof(control.bytes),
        };
        // 内核按八字节读取两个长度，字段较窄时高位也必须初始化。
        const uint64_t kernel_iovlen = 1, kernel_controllen = sizeof(control.bytes);
        memcpy((unsigned char *)&message + offsetof(struct msghdr, msg_iovlen),
               &kernel_iovlen, sizeof(kernel_iovlen));
        memcpy((unsigned char *)&message + offsetof(struct msghdr, msg_controllen),
               &kernel_controllen, sizeof(kernel_controllen));
        memset(data, 0xa5, sizeof(data));
        ST_CHECK(queue_error(sender, 0, &destination) == 0);
        switch (stage) {
        case 1:
            message.msg_iov = guard;
            break;
        case 2:
            vector.iov_base = guard;
            break;
        case 3:
            message.msg_name = guard;
            break;
        case 4:
            message.msg_control = guard;
            break;
        }
        errno = 0;
        ssize_t count = syscall(SYS_recvmsg, sender, stage == 0 ? guard : &message,
                                MSG_ERRQUEUE | MSG_DONTWAIT);
        if (stage == 4) {
            ST_CHECK(count == (ssize_t)sizeof(payload));
            ST_CHECK(message.msg_flags == MSG_ERRQUEUE && message.msg_controllen == 0);
            ST_CHECK(message.msg_namelen == sizeof(name) && memcmp(&name, &destination, sizeof(name)) == 0);
        } else {
            ST_CHECK(count == -1 && errno == EFAULT);
        }
        if (3 <= stage)
            ST_CHECK(memcmp(data, payload, sizeof(payload)) == 0);
        if (stage < 2) {
            ST_CHECK(wait_ready(sender, POLLIN, POLLERR) == 0);
            ST_CHECK(receive_error(sender, &destination, sizeof(payload), ERROR_CONTROL_SIZE, 0) == 0);
        }
        ST_CHECK(empty_queue(sender) == 0 && socket_error(sender, 0) == 0 && no_ready(sender) == 0);
    }
    result = 0;
out:
    if (st_close(&sender) != 0)
        result = -1;
    if (guard != MAP_FAILED && munmap(guard, (size_t)page) != 0)
        result = -1;
    return result;
}

/// 普通接收核对整个二进制消息和源端口，不用发送成功代替直连交付。
static int receive_payload(int fd, const struct sockaddr_in *source)
{
    int result = -1;
    unsigned char data[sizeof(payload) + 1];
    struct sockaddr_in name;
    socklen_t length = sizeof(name);
    ST_CHECK(wait_ready(fd, POLLIN, POLLIN) == 0);
    ST_CHECK(recvfrom(fd, data, sizeof(data), MSG_DONTWAIT,
                      (struct sockaddr *)&name, &length) == (ssize_t)sizeof(payload));
    ST_CHECK(memcmp(data, payload, sizeof(payload)) == 0);
    ST_CHECK(length == sizeof(name) && memcmp(&name, source, sizeof(name)) == 0);
    result = 0;
out:
    return result;
}

/// SO_DONTROUTE 允许真实的直连双向交付，也适用于设置选项前已连接的 UDP 端点。
static int direct_delivery(int connected)
{
    int result = -1, sender = -1, target = -1, enabled = 1;
    struct sockaddr_in source, destination;
    target = bound_udp(INADDR_LOOPBACK, &destination);
    ST_CHECK(0 <= target);
    sender = bound_udp(INADDR_LOOPBACK, &source);
    ST_CHECK(0 <= sender && source.sin_port != destination.sin_port);
    if (connected)
        ST_CHECK(connect(sender, (struct sockaddr *)&destination, sizeof(destination)) == 0);
    ST_CHECK(setsockopt(sender, SOL_SOCKET, SO_DONTROUTE, &enabled, sizeof(enabled)) == 0);
    ST_CHECK(send_payload(sender, connected, &destination) == 0);
    ST_CHECK(receive_payload(target, &source) == 0);
    ST_CHECK(send_payload(target, 0, &source) == 0);
    ST_CHECK(receive_payload(sender, &destination) == 0);
    ST_CHECK(socket_error(sender, 0) == 0 && empty_queue(sender) == 0 && no_ready(sender) == 0);
    result = 0;
out:
    if (st_close(&sender) != 0)
        result = -1;
    if (st_close(&target) != 0)
        result = -1;
    return result;
}

/// 用完整的序号区分长度相同的真实 UDP 报文。
static void tagged_payload(unsigned char *data, uint32_t sequence)
{
    memcpy(data, payload, sizeof(payload));
    data[0] = sequence >> 24;
    data[1] = sequence >> 16;
    data[2] = sequence >> 8;
    data[3] = sequence;
}

/// 每次只发送一个报文，消费该报文触发的 SO_ERROR 后才发送下一个。
static int send_tagged_error(int fd, const struct sockaddr_in *destination, uint32_t sequence)
{
    int result = -1;
    unsigned char data[sizeof(payload)];
    tagged_payload(data, sequence);
    ST_CHECK(sendto(fd, data, sizeof(data), 0,
                    (const struct sockaddr *)destination, sizeof(*destination)) == (ssize_t)sizeof(data));
    ST_CHECK(socket_error(fd, ECONNREFUSED) == 0 && socket_error(fd, 0) == 0);
    result = 0;
out:
    return result;
}

/// 已确认全部发送错误后，按返回序号核对队列顺序及每次出队后的状态。
static int drain_tagged_errors(int fd, const struct sockaddr_in *destination, uint32_t sent,
                               uint32_t *count, uint32_t *last)
{
    int result = -1;
    *count = 0;
    *last = 0;
    for (;;) {
        unsigned char data[sizeof(payload)], expected[sizeof(payload)];
        int status = receive_error_data(fd, destination, sizeof(data), ERROR_CONTROL_SIZE, 0, data);
        if (status == 1)
            break;
        ST_CHECK(status == 0);
        uint32_t sequence = ((uint32_t)data[0] << 24) | ((uint32_t)data[1] << 16) |
                            ((uint32_t)data[2] << 8) | data[3];
        ST_CHECK(sequence < sent && (*count == 0 || *last < sequence));
        tagged_payload(expected, sequence);
        ST_CHECK(memcmp(data, expected, sizeof(data)) == 0);
        *last = sequence;
        ++*count;

        struct pollfd ready = { .fd = fd, .events = POLLIN };
        int available = poll(&ready, 1, 0);
        ST_CHECK((available == 0 && ready.revents == 0) ||
                 (available == 1 && ready.revents == POLLERR));
        if (available == 1) {
            ST_CHECK(socket_error(fd, ECONNREFUSED) == 0 && socket_error(fd, 0) == 0);
            ST_CHECK(wait_ready(fd, POLLIN, POLLERR) == 0);
        } else {
            ST_CHECK(socket_error(fd, 0) == 0 && no_ready(fd) == 0);
        }
    }
    ST_CHECK(empty_queue(fd) == 0 && socket_error(fd, 0) == 0 && no_ready(fd) == 0);
    result = 0;
out:
    return result;
}

/// 连续确认三个真实端口不可达，再检查未提前消费的错误记录及其出队状态。
static int error_queue_fifo(void)
{
    int result = -1, sender = -1, enabled = 1, buffer = 65536;
    struct sockaddr_in destination;
    uint32_t count, last;
    sender = closed_destination(0, &destination);
    ST_CHECK(0 <= sender);
    ST_CHECK(setsockopt(sender, SOL_SOCKET, SO_RCVBUF, &buffer, sizeof(buffer)) == 0);
    ST_CHECK(setsockopt(sender, IPPROTO_IP, IP_RECVERR, &enabled, sizeof(enabled)) == 0);
    for (uint32_t sequence = 0; sequence < 3; ++sequence)
        ST_CHECK(send_tagged_error(sender, &destination, sequence) == 0);
    ST_CHECK(drain_tagged_errors(sender, &destination, 3, &count, &last) == 0);
    ST_CHECK(count == 3 && last == 2);
    result = 0;
out:
    if (st_close(&sender) != 0)
        result = -1;
    return result;
}

/// 限制接收缓冲并观察记录是否丢失，检查保留顺序和清空后的收发。
static int error_queue_recovery(int *loss)
{
    int result = -1, sender = -1, peer = -1, enabled = 1, buffer = 1;
    struct sockaddr_in destination, source, peer_address;
    uint32_t count, last;
    peer = bound_udp(INADDR_LOOPBACK, &peer_address);
    ST_CHECK(0 <= peer);
    sender = closed_destination(0, &destination);
    ST_CHECK(0 <= sender);
    socklen_t length = sizeof(source);
    ST_CHECK(getsockname(sender, (struct sockaddr *)&source, &length) == 0);
    ST_CHECK(length == sizeof(source));
    ST_CHECK(setsockopt(sender, SOL_SOCKET, SO_RCVBUF, &buffer, sizeof(buffer)) == 0);
    length = sizeof(buffer);
    ST_CHECK(getsockopt(sender, SOL_SOCKET, SO_RCVBUF, &buffer, &length) == 0);
    ST_CHECK(length == sizeof(buffer) && 0 < buffer);
    ST_CHECK(setsockopt(sender, IPPROTO_IP, IP_RECVERR, &enabled, sizeof(enabled)) == 0);

    const uint32_t attempts = 128;
    for (uint32_t sequence = 0; sequence < attempts; ++sequence)
        ST_CHECK(send_tagged_error(sender, &destination, sequence) == 0);
    ST_CHECK(drain_tagged_errors(sender, &destination, attempts, &count, &last) == 0);

    ST_CHECK(send_payload(sender, 0, &peer_address) == 0);
    ST_CHECK(receive_payload(peer, &source) == 0);
    ST_CHECK(send_payload(peer, 0, &source) == 0);
    ST_CHECK(receive_payload(sender, &peer_address) == 0);
    ST_CHECK(send_tagged_error(sender, &destination, attempts) == 0);
    unsigned char data[sizeof(payload)], expected[sizeof(payload)];
    ST_CHECK(receive_error_data(sender, &destination, sizeof(data), ERROR_CONTROL_SIZE, 0, data) == 0);
    tagged_payload(expected, attempts);
    ST_CHECK(memcmp(data, expected, sizeof(data)) == 0);
    ST_CHECK(empty_queue(sender) == 0 && socket_error(sender, 0) == 0 && no_ready(sender) == 0);

    *loss = count < attempts;
    ksft_print_msg("retained=%u sent=%u; reason for any loss is unverified\n", count, attempts);
    result = 0;
out:
    if (st_close(&sender) != 0)
        result = -1;
    if (st_close(&peer) != 0)
        result = -1;
    return result;
}

/// 固定测试地址的最长前缀路由必须经过网关，直连或缺路由时独立报告环境不适用。
static int gateway_destination(struct sockaddr_in *destination)
{
    int result = -1, found = 0, via_gateway = 0;
    uint32_t best_mask = 0;
    unsigned best_metric = UINT32_MAX;
    FILE *routes = fopen("/proc/net/route", "r");
    char line[512];
    *destination = (struct sockaddr_in){ .sin_family = AF_INET, .sin_port = htons(9) };
    ST_CHECK(inet_pton(AF_INET, "192.0.2.1", &destination->sin_addr) == 1);
    ST_CHECK(routes != NULL && fgets(line, sizeof(line), routes) != NULL);
    uint32_t address = ntohl(destination->sin_addr.s_addr);
    while (fgets(line, sizeof(line), routes) != NULL) {
        char interface[IFNAMSIZ];
        unsigned long network_value, gateway_value, mask_value;
        unsigned flags, references, use, metric, mtu, window, irtt;
        ST_CHECK(strchr(line, '\n') != NULL);
        ST_CHECK(sscanf(line, "%15s %lx %lx %x %u %u %u %lx %u %u %u",
                        interface, &network_value, &gateway_value, &flags, &references,
                        &use, &metric, &mask_value, &mtu, &window, &irtt) == 11);
        ST_CHECK(network_value <= UINT32_MAX && gateway_value <= UINT32_MAX && mask_value <= UINT32_MAX);
        uint32_t network = ntohl((uint32_t)network_value);
        uint32_t mask = ntohl((uint32_t)mask_value);
        ST_CHECK((mask | (mask - 1)) == UINT32_MAX);
        if ((flags & RTF_UP) == 0 || (address & mask) != network)
            continue;
        if (found && mask < best_mask)
            continue;
        if (found && mask == best_mask && best_metric <= metric)
            continue;
        found = 1;
        best_mask = mask;
        best_metric = metric;
        via_gateway = (flags & RTF_GATEWAY) != 0 && gateway_value != 0;
    }
    ST_CHECK(!ferror(routes));
    if (!found || !via_gateway) {
        ksft_print_msg("192.0.2.1 has no selected up IPv4 gateway route\n");
        result = KSFT_SKIP;
        goto out;
    }
    result = 0;
out:
    if (routes != NULL && fclose(routes) != 0)
        result = -1;
    return result;
}

/// 普通 connect 只解析现有网关路由，SO_DONTROUTE 必须拒绝后续发送和新连接。
static int gateway_restriction(void)
{
    int result = -1, sender = -1, second = -1, enabled = 1, disabled = 0;
    struct sockaddr_in destination, local;
    sender = bound_udp(INADDR_ANY, &local);
    ST_CHECK(0 <= sender);
    int selected = gateway_destination(&destination);
    if (selected == KSFT_SKIP) {
        result = KSFT_SKIP;
        goto out;
    }
    ST_CHECK(selected == 0);
    if (connect(sender, (struct sockaddr *)&destination, sizeof(destination)) != 0) {
        if (errno == ENETUNREACH) {
            ksft_print_msg("normal IPv4 connect has no usable gateway route\n");
            result = KSFT_SKIP;
            goto out;
        }
        ST_CHECK(0);
    }
    socklen_t length = sizeof(local);
    ST_CHECK(getsockname(sender, (struct sockaddr *)&local, &length) == 0);
    ST_CHECK(length == sizeof(local) && local.sin_family == AF_INET && local.sin_addr.s_addr != htonl(INADDR_ANY));
    ST_CHECK(setsockopt(sender, SOL_SOCKET, SO_DONTROUTE, &enabled, sizeof(enabled)) == 0);
    errno = 0;
    ST_CHECK(send(sender, payload, sizeof(payload), 0) == -1 && errno == ENETUNREACH);
    ST_CHECK(socket_error(sender, 0) == 0 && empty_queue(sender) == 0);
    ST_CHECK(setsockopt(sender, SOL_SOCKET, SO_DONTROUTE, &disabled, sizeof(disabled)) == 0);
    ST_CHECK(connect(sender, (struct sockaddr *)&destination, sizeof(destination)) == 0);

    second = bound_udp(INADDR_ANY, &local);
    ST_CHECK(0 <= second);
    ST_CHECK(setsockopt(second, SOL_SOCKET, SO_DONTROUTE, &enabled, sizeof(enabled)) == 0);
    errno = 0;
    ST_CHECK(sendto(second, payload, sizeof(payload), 0,
                    (struct sockaddr *)&destination, sizeof(destination)) == -1 && errno == ENETUNREACH);
    errno = 0;
    ST_CHECK(connect(second, (struct sockaddr *)&destination, sizeof(destination)) == -1 && errno == ENETUNREACH);
    ST_CHECK(socket_error(second, 0) == 0 && empty_queue(second) == 0);
    result = 0;
out:
    if (st_close(&second) != 0)
        result = -1;
    if (st_close(&sender) != 0)
        result = -1;
    return result;
}

/// 子进程以非零 uid 重做真实交付，降权接口缺失或拒绝时单独跳过，父进程负责回收。
static int unprivileged_delivery(void)
{
    int result = -1;
    pid_t child = -1;
    ST_CHECK(fflush(stdout) == 0);
    child = fork();
    ST_CHECK(0 <= child);
    if (child == 0) {
        if (geteuid() == 0 && (setgid(65534) != 0 || setuid(65534) != 0)) {
            int failure = errno;
            fprintf(stderr, "unprivileged UDP identity change errno=%d\n", failure);
            _exit(failure == ENOSYS || failure == EPERM ? KSFT_SKIP : KSFT_FAIL);
        }
        if (getuid() == 0 || geteuid() == 0)
            _exit(KSFT_FAIL);
        _exit(error_delivery(0) == 0 && error_delivery(1) == 0 &&
              direct_delivery(0) == 0 && direct_delivery(1) == 0 ? KSFT_PASS : KSFT_FAIL);
    }
    int status;
    pid_t reaped;
    do {
        reaped = waitpid(child, &status, 0);
    } while (reaped == -1 && errno == EINTR);
    ST_CHECK(reaped == child);
    child = -1;
    ST_CHECK(WIFEXITED(status));
    int code = WEXITSTATUS(status);
    ST_CHECK(code == KSFT_PASS || code == KSFT_SKIP);
    result = code;
out:
    if (st_stop_child(&child) != 0)
        result = -1;
    return result;
}

/// 同一程序由 host 和 guest 执行，环境或实际队列观察不足的子项独立报告。
int main(int argc, char **argv)
{
    ksft_print_header();
    if (argc != 1)
        ksft_exit_fail_msg("usage: %s\n", argv[0]);
    ksft_set_plan(16);
    ksft_test_result(error_delivery(0) == 0, "unconnected UDP ICMP payload destination offender POLLERR and independent SO_ERROR\n");
    ksft_test_result(error_delivery(1) == 0, "connected UDP ICMP payload destination offender POLLERR and independent SO_ERROR\n");
    ksft_test_result(error_purge(0) == 0, "unconnected IP_RECVERR disable purges queue but preserves SO_ERROR\n");
    ksft_test_result(error_purge(1) == 0, "connected IP_RECVERR disable purges queue but preserves SO_ERROR\n");
    ksft_test_result(payload_truncation() == 0, "MSG_ERRQUEUE payload truncation zero capacity return lengths and MSG_TRUNC\n");
    ksft_test_result(control_truncation() == 0, "MSG_ERRQUEUE control truncation MSG_CTRUNC and record consumption\n");
    ksft_test_result(option_lengths() == 0, "IP_RECVERR and SO_DONTROUTE lengths 0 1 3 4 with real ICMP delivery and purge\n");
    ksft_test_result(option_copy_boundaries() == 0, "option guard pages byte versus int copies EFAULT and output ordering\n");
    ksft_test_result(receive_copy_boundaries() == 0, "recvmsg header iovec payload name and control copy consumption boundaries\n");
    ksft_test_result(error_queue_fifo() == 0, "queued UDP errors preserve payload destination FIFO and pending state\n");
    int loss = 0;
    int recovery = error_queue_recovery(&loss);
    ksft_test_result(recovery == 0, "restricted UDP error queue empty-then-receive and ordinary delivery\n");
    if (recovery != 0)
        ksft_test_result_fail("restricted UDP error queue loss observation did not complete\n");
    else if (!loss)
        ksft_test_result_skip("no UDP error record was observed missing under restricted SO_RCVBUF\n");
    else
        ksft_test_result_pass("missing UDP error records had independently observed SO_ERROR\n");
    ksft_test_result(direct_delivery(0) == 0, "unconnected SO_DONTROUTE direct loopback binary roundtrip\n");
    ksft_test_result(direct_delivery(1) == 0, "connected SO_DONTROUTE direct loopback binary roundtrip\n");
    int gateway = gateway_restriction();
    if (gateway == KSFT_SKIP)
        ksft_test_result_skip("SO_DONTROUTE gateway restriction requires a usable gateway-only IPv4 route\n");
    else
        ksft_test_result(gateway == 0, "SO_DONTROUTE rejects gateway send sendto and connect including cached route\n");
    int unprivileged = unprivileged_delivery();
    if (unprivileged == KSFT_SKIP)
        ksft_test_result_skip("unprivileged UDP identity change unavailable with ENOSYS or EPERM\n");
    else
        ksft_test_result(unprivileged == KSFT_PASS, "unprivileged connected and unconnected IP_RECVERR and SO_DONTROUTE delivery\n");
    ksft_finished();
}
