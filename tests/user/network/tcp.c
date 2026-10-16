#include "common.h"

/// exec 后检查未关闭别名仍共享 OFD，原 CLOEXEC 槽已经被撤下。
int exec_identity(int argc, char **argv)
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
void identity(const char *program)
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
void multiple_accept(void)
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
void nonblock_connect(void)
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

/// 大流由 fork 发送，短读和 PEEK 保完整顺序，FIN 只能在全部字节之后返回 EOF。
void stream(void)
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
void backpressure(void)
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

/// SHUT_WR 保接收方向，NOSIGNAL 和普通发送分别验证 SIGPIPE 的抑制与实际递送。
void half_close(void)
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

/// 地址容量只写可容纳前缀；按实际取得的别名耗尽表，再核 accept 与用户页 fault。
void address_faults(void)
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
    struct descriptor_list duplicates = duplicate_until_full(fd);
    ERROR(accept(fd, NULL, NULL), EMFILE);
    CHECK(0 < duplicates.count);
    CHECK(close(duplicates.items[--duplicates.count]) == 0);
    server = accept(fd, NULL, (socklen_t *)1);
    CHECK(0 <= server);
    write_all(client, "ready survives", 14);
    read_all(server, output, 14);
    CHECK(memcmp(output, "ready survives", 14) == 0);
    release_descriptors(&duplicates);
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
void interrupted(void)
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
void epoll_stream(void)
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

