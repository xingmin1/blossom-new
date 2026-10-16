#include "network/common.h"

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
