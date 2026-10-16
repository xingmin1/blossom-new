#include "common.h"

/// 服务端口只接受完整十进制数字，范围之外的 argv 不能成为截断后的目的端口。
unsigned service_port(const char *text)
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
struct sockaddr_in nic_peer(unsigned port)
{
    struct sockaddr_in peer = address(port);
    peer.sin_addr.s_addr = htonl(0x0a000202);
    return peer;
}

/// 单连接真实提交和接收短前缀后继续核验双向全文，写半关闭必须交付宿主 EOF。
void nic_tcp(unsigned port)
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
void nic_udp(unsigned port)
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

