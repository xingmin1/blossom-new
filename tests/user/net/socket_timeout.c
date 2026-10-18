#include "libc_common.h"

/// 查询同一端点的实际量化 timeval，同时检查完整 LP64 输出长度。
static int socket_timeout_value(int fd, int option, time_t seconds, suseconds_t usec)
{
    struct timeval value = { -1, -1 };
    socklen_t length = sizeof(value);
    if (getsockopt(fd, SOL_SOCKET, option, &value, &length) != 0 ||
        length != sizeof(value) || value.tv_sec != seconds || value.tv_usec != usec)
        return fail(option == SO_RCVTIMEO ? "SO_RCVTIMEO value" : "SO_SNDTIMEO value");
    return EXIT_SUCCESS;
}

/// 修改真实 socket 控制属性，不改变 fd 状态或用用户空间 timer 代替期限。
static int socket_timeout_set(int fd, int option, time_t seconds, suseconds_t usec)
{
    struct timeval value = { seconds, usec };
    if (setsockopt(fd, SOL_SOCKET, option, &value, sizeof(value)) != 0)
        return fail(option == SO_RCVTIMEO ? "SO_RCVTIMEO set" : "SO_SNDTIMEO set");
    return EXIT_SUCCESS;
}

/// 一组控制请求覆盖真实复制、截断和转换错误；失败 setter 必须保留旧属性。
static int socket_timeout_options(int fd)
{
    _Static_assert(sizeof(struct timeval) == 16, "socket timeval LP64 ABI");
    _Static_assert(sizeof(socklen_t) == 4, "socket optlen ABI");
    const int options[] = { SO_RCVTIMEO, SO_SNDTIMEO };
    for (size_t index = 0; index < sizeof(options) / sizeof(options[0]); ++index) {
        int option = options[index];
        struct timeval input = { 0, 50001 };
        struct {
            struct timeval value;
            uint64_t guard;
        } output;
        memset(&output, 0xa5, sizeof(output));
        socklen_t length = sizeof(output);
        if (socket_timeout_value(fd, option, 0, 0) != EXIT_SUCCESS ||
            setsockopt(fd, SOL_SOCKET, option, &input, sizeof(input)) != 0 ||
            getsockopt(fd, SOL_SOCKET, option, &output, &length) != 0 ||
            length != 16 || output.value.tv_sec != 0 || output.value.tv_usec != 60000 ||
            output.guard != UINT64_C(0xa5a5a5a5a5a5a5a5))
            return fail("socket timeout full GET and quantization");
        memset(&output, 0xa5, sizeof(output));
        length = 8;
        if (getsockopt(fd, SOL_SOCKET, option, &output, &length) != 0 ||
            length != 8 || output.value.tv_sec != 0 ||
            (uint64_t)output.value.tv_usec != UINT64_C(0xa5a5a5a5a5a5a5a5))
            return fail("socket timeout short GET");
        length = 0;
        if (getsockopt(fd, SOL_SOCKET, option, &output, &length) != 0 || length != 0)
            return fail("socket timeout zero GET length");
        errno = 0;
        if (setsockopt(fd, SOL_SOCKET, option, &input, 15) != -1 || errno != EINVAL)
            return fail("socket timeout short SET EINVAL");
        errno = 0;
        if (syscall(SYS_setsockopt, fd, SOL_SOCKET, option, 1UL, 16UL) != -1 ||
            errno != EFAULT)
            return fail("socket timeout SET EFAULT");
        length = 16;
        errno = 0;
        if (syscall(SYS_getsockopt, fd, SOL_SOCKET, option, 1UL, &length) != -1 ||
            errno != EFAULT)
            return fail("socket timeout GET EFAULT");
        errno = 0;
        if (syscall(SYS_getsockopt, fd, SOL_SOCKET, option, &output, 1UL) != -1 ||
            errno != EFAULT)
            return fail("socket timeout optlen EFAULT");
        length = (socklen_t)-1;
        errno = 0;
        if (getsockopt(fd, SOL_SOCKET, option, &output, &length) != -1 || errno != EINVAL)
            return fail("socket timeout negative optlen EINVAL");
        const suseconds_t invalid[] = { -1, 1000000 };
        for (size_t bad = 0; bad < sizeof(invalid) / sizeof(invalid[0]); ++bad) {
            input.tv_sec = -1;
            input.tv_usec = invalid[bad];
            errno = 0;
            if (setsockopt(fd, SOL_SOCKET, option, &input, sizeof(input)) != -1 ||
                errno != 33 || errno != EDOM)
                return fail("socket timeout usec EDOM33");
        }
        if (socket_timeout_value(fd, option, 0, 60000) != EXIT_SUCCESS)
            return fail("socket timeout failed SET preserved");
        input.tv_sec = 1;
        input.tv_usec = 1;
        memcpy(&output.value, &input, sizeof(input));
        if (setsockopt(fd, SOL_SOCKET, option, &output, sizeof(output)) != 0 ||
            socket_timeout_value(fd, option, 1, 10000) != EXIT_SUCCESS ||
            socket_timeout_set(fd, option, -1, 1) != EXIT_SUCCESS ||
            socket_timeout_value(fd, option, 0, 0) != EXIT_SUCCESS ||
            socket_timeout_set(fd, option, 0, 0) != EXIT_SUCCESS)
            return fail("socket timeout signed and extended SET");
    }
    return EXIT_SUCCESS;
}

/// dup 和 fork 沿同一 OFD 修改端点，wait 后父进程必须见到孩子提交的两个期限。
static int socket_timeout_shared(int fd)
{
    int alias = dup(fd);
    if (alias < 0)
        return fail("socket timeout dup");
    int result = socket_timeout_set(alias, SO_RCVTIMEO, 0, 80000);
    if (result == EXIT_SUCCESS)
        result = socket_timeout_value(fd, SO_RCVTIMEO, 0, 80000);
    pid_t child = -1;
    if (result == EXIT_SUCCESS) {
        child = fork();
        if (child < 0) {
            result = fail("socket timeout fork");
        } else if (child == 0) {
            int status = socket_timeout_value(alias, SO_RCVTIMEO, 0, 80000);
            if (status == EXIT_SUCCESS)
                status = socket_timeout_set(fd, SO_RCVTIMEO, 0, 70000);
            if (status == EXIT_SUCCESS)
                status = socket_timeout_set(alias, SO_SNDTIMEO, 0, 90000);
            if (close(alias) != 0)
                status = fail("socket timeout child dup close");
            if (close(fd) != 0)
                status = fail("socket timeout child original close");
            _exit(status);
        } else {
            result = wait_status(child, 0);
            if (result == EXIT_SUCCESS &&
                (socket_timeout_value(fd, SO_RCVTIMEO, 0, 70000) != EXIT_SUCCESS ||
                 socket_timeout_value(alias, SO_SNDTIMEO, 0, 90000) != EXIT_SUCCESS))
                result = fail("socket timeout fork shared endpoint");
        }
    }
    if (close(alias) != 0)
        result = fail("socket timeout dup close");
    return result;
}

/// 空 UDP 的三种接收调用必须真正到期，非阻塞请求和负秒则不等待有限期限。
static int socket_timeout_udp(int fd)
{
    struct sockaddr_in address = {
        .sin_family = AF_INET, .sin_port = 0,
        .sin_addr = { .s_addr = htonl(INADDR_LOOPBACK) },
    };
    if (bind(fd, (struct sockaddr *)&address, sizeof(address)) != 0 ||
        socket_timeout_set(fd, SO_RCVTIMEO, 0, 60000) != EXIT_SUCCESS)
        return fail("socket timeout UDP bind");
    unsigned char byte = 0;
    struct iovec vector = { .iov_base = &byte, .iov_len = 1 };
    struct msghdr message = { .msg_iov = &vector, .msg_iovlen = 1 };
    const char *operations[] = { "UDP read deadline", "UDP recv deadline", "UDP recvmsg deadline" };
    for (unsigned method = 0; method < 3; ++method) {
        uint64_t start;
        if (runtime_nanoseconds(CLOCK_MONOTONIC, &start) != EXIT_SUCCESS)
            return EXIT_FAILURE;
        errno = 0;
        ssize_t count = method == 0 ? read(fd, &byte, 1) :
                        method == 1 ? recv(fd, &byte, 1, 0) : recvmsg(fd, &message, 0);
        int error = errno;
        if (count != -1 || error != EAGAIN)
            return fail(operations[method]);
        if (runtime_elapsed(start, 50000000ULL, operations[method]) != EXIT_SUCCESS)
            return EXIT_FAILURE;
    }
    if (socket_timeout_set(fd, SO_RCVTIMEO, 10, 0) != EXIT_SUCCESS)
        return EXIT_FAILURE;
    uint64_t start;
    if (runtime_nanoseconds(CLOCK_MONOTONIC, &start) != EXIT_SUCCESS)
        return EXIT_FAILURE;
    errno = 0;
    if (recv(fd, &byte, 1, MSG_DONTWAIT) != -1 || errno != EAGAIN ||
        runtime_elapsed(start, 0, "UDP MSG_DONTWAIT") != EXIT_SUCCESS)
        return fail("UDP MSG_DONTWAIT priority");
    uint64_t end;
    if (runtime_nanoseconds(CLOCK_MONOTONIC, &end) != EXIT_SUCCESS ||
        5000000000ULL < end - start)
        return fail("UDP MSG_DONTWAIT waited for timeout");
    int flags = fcntl(fd, F_GETFL);
    if (flags < 0 || fcntl(fd, F_SETFL, flags | O_NONBLOCK) != 0 ||
        runtime_nanoseconds(CLOCK_MONOTONIC, &start) != EXIT_SUCCESS)
        return fail("UDP O_NONBLOCK prepare");
    errno = 0;
    ssize_t count = read(fd, &byte, 1);
    int error = errno;
    if (fcntl(fd, F_SETFL, flags) != 0)
        return fail("UDP O_NONBLOCK restore");
    if (count != -1 || error != EAGAIN ||
        runtime_nanoseconds(CLOCK_MONOTONIC, &end) != EXIT_SUCCESS ||
        end < start || 5000000000ULL < end - start)
        return fail("UDP O_NONBLOCK priority");
    if (socket_timeout_set(fd, SO_RCVTIMEO, -1, 0) != EXIT_SUCCESS ||
        runtime_nanoseconds(CLOCK_MONOTONIC, &start) != EXIT_SUCCESS)
        return EXIT_FAILURE;
    errno = 0;
    if (read(fd, &byte, 1) != -1 || errno != EAGAIN ||
        runtime_nanoseconds(CLOCK_MONOTONIC, &end) != EXIT_SUCCESS ||
        end < start || 5000000000ULL < end - start)
        return fail("UDP negative seconds immediate deadline");
    return EXIT_SUCCESS;
}

/// 两次真实握手消费 listener 快照；新 fd 的 NONBLOCK/CLOEXEC 只取 accept4 参数。
static int socket_timeout_tcp(void)
{
    int listener = socket(AF_INET, SOCK_STREAM | SOCK_NONBLOCK, 0);
    if (listener < 0)
        return fail("socket timeout listener");
    int clients[2] = { -1, -1 }, accepted[2] = { -1, -1 };
    int result = EXIT_FAILURE;
    struct sockaddr_in address = {
        .sin_family = AF_INET, .sin_port = 0,
        .sin_addr = { .s_addr = htonl(INADDR_LOOPBACK) },
    };
    socklen_t length = sizeof(address);
    if (socket_timeout_set(listener, SO_RCVTIMEO, 0, 100000) != EXIT_SUCCESS ||
        socket_timeout_set(listener, SO_SNDTIMEO, 0, 150000) != EXIT_SUCCESS ||
        bind(listener, (struct sockaddr *)&address, sizeof(address)) != 0 ||
        getsockname(listener, (struct sockaddr *)&address, &length) != 0 ||
        length != sizeof(address) || address.sin_port == 0 || listen(listener, 2) != 0) {
        result = fail("socket timeout listener prepare");
        goto out;
    }
    for (unsigned round = 0; round < 2; ++round) {
        if (round == 1 &&
            (socket_timeout_set(listener, SO_RCVTIMEO, 0, 200000) != EXIT_SUCCESS ||
             socket_timeout_set(listener, SO_SNDTIMEO, 0, 250000) != EXIT_SUCCESS))
            goto out;
        clients[round] = socket(AF_INET, SOCK_STREAM, 0);
        if (clients[round] < 0 ||
            socket_timeout_set(clients[round], SO_SNDTIMEO, 2, 0) != EXIT_SUCCESS ||
            connect(clients[round], (struct sockaddr *)&address, sizeof(address)) != 0) {
            result = fail("socket timeout TCP handshake");
            goto out;
        }
        struct pollfd ready = { .fd = listener, .events = POLLIN };
        if (poll(&ready, 1, 2000) != 1 || (ready.revents & POLLIN) == 0) {
            result = fail("socket timeout listener readiness");
            goto out;
        }
        accepted[round] = accept4(listener, NULL, NULL,
                                  round == 0 ? 0 : SOCK_NONBLOCK | SOCK_CLOEXEC);
        if (accepted[round] < 0) {
            result = fail("socket timeout accept4");
            goto out;
        }
        int flags = fcntl(accepted[round], F_GETFL);
        int descriptor_flags = fcntl(accepted[round], F_GETFD);
        if (flags < 0 || descriptor_flags < 0 ||
            !!(flags & O_NONBLOCK) != (int)round ||
            !!(descriptor_flags & FD_CLOEXEC) != (int)round ||
            socket_timeout_value(accepted[round], SO_RCVTIMEO, 0,
                                 round == 0 ? 100000 : 200000) != EXIT_SUCCESS ||
            socket_timeout_value(accepted[round], SO_SNDTIMEO, 0,
                                 round == 0 ? 150000 : 250000) != EXIT_SUCCESS) {
            result = fail("socket timeout accepted snapshot and flags");
            goto out;
        }
    }
    result = socket_timeout_value(accepted[0], SO_RCVTIMEO, 0, 100000);
    if (result == EXIT_SUCCESS)
        result = socket_timeout_value(accepted[0], SO_SNDTIMEO, 0, 150000);
out:
    for (unsigned index = 0; index < 2; ++index) {
        if (0 <= accepted[index] && close(accepted[index]) != 0)
            result = fail("socket timeout accepted close");
        if (0 <= clients[index] && close(clients[index]) != 0)
            result = fail("socket timeout client close");
    }
    if (close(listener) != 0)
        result = fail("socket timeout listener close");
    return result;
}

/// 满 FIFO 的每次无进度发送核真正到期；释放物理页槽后核正前缀与原字节。
static int socket_timeout_unix(void)
{
    int pair[2];
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, pair) != 0)
        return fail("socket timeout UNIX pair");
    int result = EXIT_FAILURE;
    unsigned char bytes[4096];
    memset(bytes, 0x5a, sizeof(bytes));
    size_t queued = 0;
    for (;;) {
        ssize_t count = send(pair[0], bytes, sizeof(bytes), MSG_DONTWAIT | MSG_NOSIGNAL);
        if (count == -1 && errno == EAGAIN)
            break;
        if (count <= 0 || 8 * 1024 * 1024 - sizeof(bytes) < queued) {
            result = fail("socket timeout UNIX fill");
            goto out;
        }
        queued += (size_t)count;
    }
    if (queued == 0) {
        result = fail("UNIX new stream has no send capacity");
        goto out;
    }
    if (socket_timeout_set(pair[0], SO_SNDTIMEO, 0, 60000) != EXIT_SUCCESS)
        goto out;
    uint64_t start;
    if (runtime_nanoseconds(CLOCK_MONOTONIC, &start) != EXIT_SUCCESS)
        goto out;
    errno = 0;
    if (write(pair[0], bytes, 1) != -1 || errno != EAGAIN ||
        runtime_elapsed(start, 50000000ULL, "UNIX write deadline") != EXIT_SUCCESS) {
        result = fail("UNIX full write timeout");
        goto out;
    }
    struct iovec vector = { .iov_base = bytes, .iov_len = 1 };
    struct msghdr message = { .msg_iov = &vector, .msg_iovlen = 1 };
    if (runtime_nanoseconds(CLOCK_MONOTONIC, &start) != EXIT_SUCCESS)
        goto out;
    errno = 0;
    if (sendmsg(pair[0], &message, MSG_NOSIGNAL) != -1 || errno != EAGAIN ||
        runtime_elapsed(start, 50000000ULL, "UNIX sendmsg deadline") != EXIT_SUCCESS) {
        result = fail("UNIX full sendmsg timeout");
        goto out;
    }
    if (read(pair[1], bytes, sizeof(bytes)) != (ssize_t)sizeof(bytes)) {
        result = fail("UNIX release physical slot");
        goto out;
    }
    for (size_t index = 0; index < sizeof(bytes); ++index) {
        if (bytes[index] != 0x5a) {
            result = fail("UNIX released slot contents");
            goto out;
        }
    }
    struct iovec prefix_vectors[] = {
        { .iov_base = bytes, .iov_len = sizeof(bytes) },
        { .iov_base = bytes, .iov_len = sizeof(bytes) },
    };
    ssize_t prefix = writev(pair[0], prefix_vectors, 2);
    if (prefix <= 0 || 2 * sizeof(bytes) <= (size_t)prefix) {
        result = fail("UNIX send positive prefix");
        goto out;
    }
    size_t remaining = queued - sizeof(bytes) + (size_t)prefix;
    while (0 < remaining) {
        size_t requested = remaining < sizeof(bytes) ? remaining : sizeof(bytes);
        ssize_t count = read(pair[1], bytes, requested);
        if (count <= 0 || remaining < (size_t)count) {
            result = fail("UNIX timeout drain");
            goto out;
        }
        for (ssize_t index = 0; index < count; ++index) {
            if (bytes[index] != 0x5a) {
                result = fail("UNIX timeout queued contents");
                goto out;
            }
        }
        remaining -= (size_t)count;
    }
    if (socket_timeout_set(pair[0], SO_RCVTIMEO, 0, 60000) != EXIT_SUCCESS ||
        write(pair[1], "utc", 3) != 3 || read(pair[0], bytes, sizeof(bytes)) != 3 ||
        memcmp(bytes, "utc", 3) != 0) {
        result = fail("UNIX receive existing prefix");
        goto out;
    }
    result = EXIT_SUCCESS;
out:
    if (close(pair[0]) != 0)
        result = fail("socket timeout UNIX send close");
    if (close(pair[1]) != 0)
        result = fail("socket timeout UNIX receive close");
    return result;
}

static volatile sig_atomic_t timeout_signals;

/// 普通 SA_RESTART handler 只记录真实交付次数，不主动重发信号或重试系统调用。
static void socket_timeout_handler(int signal_number)
{
    if (signal_number == SIGUSR1)
        ++timeout_signals;
}

/// 子进程先消费 pipe 握手，再发一次信号与真实后继数据，退出前关闭全部拥有。
static int socket_timeout_signal(int finite)
{
    int pair[2], ready[2];
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, pair) != 0)
        return fail("socket signal pair");
    if (pipe(ready) != 0) {
        close(pair[0]);
        close(pair[1]);
        return fail("socket signal ready pipe");
    }
    int result = EXIT_FAILURE;
    pid_t child = -1;
    if (socket_timeout_set(pair[0], SO_RCVTIMEO, finite ? 2 : 0, 0) != EXIT_SUCCESS)
        goto out;
    timeout_signals = 0;
    pid_t parent = getpid();
    child = fork();
    if (child < 0) {
        result = fail("socket signal fork");
        goto out;
    }
    if (child == 0) {
        int status = EXIT_SUCCESS;
        if (close(pair[0]) != 0)
            status = fail("socket signal child receive close");
        if (close(ready[1]) != 0)
            status = fail("socket signal child ready write close");
        unsigned char token;
        struct timespec delay = { 0, 100000000 };
        if (status == EXIT_SUCCESS &&
            (transfer_pipe(ready[0], &token, 1, 0) != EXIT_SUCCESS ||
             token != 0x74 || nanosleep(&delay, NULL) != 0 ||
             kill(parent, SIGUSR1) != 0 || nanosleep(&delay, NULL) != 0 ||
             send(pair[1], "s", 1, MSG_NOSIGNAL) != 1))
            status = fail("socket signal child delivery");
        if (close(pair[1]) != 0)
            status = fail("socket signal child send close");
        if (close(ready[0]) != 0)
            status = fail("socket signal child ready read close");
        _exit(status);
    }
    int send_close = close(pair[1]);
    pair[1] = -1;
    int ready_close = close(ready[0]);
    ready[0] = -1;
    if (send_close != 0 || ready_close != 0) {
        result = fail("socket signal parent unused close");
        goto out;
    }
    unsigned char token = 0x74, byte = 0;
    uint64_t start;
    if (runtime_nanoseconds(CLOCK_MONOTONIC, &start) != EXIT_SUCCESS ||
        transfer_pipe(ready[1], &token, 1, 1) != EXIT_SUCCESS)
        goto out;
    if (timeout_signals != 0) {
        result = fail("socket signal arrived before blocking read");
        goto out;
    }
    errno = 0;
    ssize_t count = read(pair[0], &byte, 1);
    int error = errno;
    if (finite ? (count != -1 || error != EINTR) : (count != 1 || byte != 's')) {
        result = fail(finite ? "finite socket SA_RESTART must return EINTR" :
                              "infinite socket SA_RESTART must receive successor");
        goto out;
    }
    if (timeout_signals != 1 ||
        runtime_elapsed(start, 50000000ULL, "socket signal blocked read") != EXIT_SUCCESS) {
        result = fail("socket signal handler and wait");
        goto out;
    }
    if (finite && (read(pair[0], &byte, 1) != 1 || byte != 's')) {
        result = fail("finite socket real successor after EINTR");
        goto out;
    }
    result = EXIT_SUCCESS;
out:
    if (0 <= ready[1]) {
        if (close(ready[1]) != 0)
            result = fail("socket signal ready close");
        ready[1] = -1;
    }
    if (0 < child && wait_status(child, 0) != EXIT_SUCCESS)
        result = EXIT_FAILURE;
    if (0 <= ready[0] && close(ready[0]) != 0)
        result = fail("socket signal read pipe close");
    if (close(pair[0]) != 0)
        result = fail("socket signal receive close");
    if (0 <= pair[1] && close(pair[1]) != 0)
        result = fail("socket signal send close");
    return result;
}

/// 有限和无限等待使用同一 handler，恢复进入模式前的信号动作和掩码。
static int socket_timeout_restart(void)
{
    struct sigaction action = { .sa_handler = socket_timeout_handler, .sa_flags = SA_RESTART };
    struct sigaction original;
    sigset_t signal_set, original_mask;
    sigemptyset(&action.sa_mask);
    sigemptyset(&signal_set);
    sigaddset(&signal_set, SIGUSR1);
    if (sigaction(SIGUSR1, &action, &original) != 0)
        return fail("socket timeout sigaction");
    int result = EXIT_FAILURE;
    if (sigprocmask(SIG_UNBLOCK, &signal_set, &original_mask) != 0) {
        result = fail("socket timeout signal unblock");
    } else {
        result = socket_timeout_signal(1);
        if (result == EXIT_SUCCESS)
            result = socket_timeout_signal(0);
        if (sigprocmask(SIG_SETMASK, &original_mask, NULL) != 0)
            result = fail("socket timeout signal mask restore");
    }
    if (sigaction(SIGUSR1, &original, NULL) != 0)
        result = fail("socket timeout sigaction restore");
    return result;
}

/// 正式 socket 消费者串行检查控制 ABI、共享属性、协议等待和不可重启中断。
int runtime_socket_timeouts(void)
{
    int fd = socket(AF_INET, SOCK_DGRAM, 0);
    if (fd < 0)
        return fail("socket timeout UDP create");
    int result = socket_timeout_options(fd);
    if (result == EXIT_SUCCESS)
        result = socket_timeout_shared(fd);
    if (result == EXIT_SUCCESS)
        result = socket_timeout_udp(fd);
    if (close(fd) != 0)
        result = fail("socket timeout UDP close");
    if (result == EXIT_SUCCESS)
        result = socket_timeout_tcp();
    if (result == EXIT_SUCCESS)
        result = socket_timeout_unix();
    if (result == EXIT_SUCCESS)
        result = socket_timeout_restart();
    if (result == EXIT_SUCCESS &&
        (printf("libc-smoke: real socket timeouts and restart behavior passed\n") < 0 ||
         fflush(stdout) != 0))
        return fail("socket timeout output");
    return result;
}
