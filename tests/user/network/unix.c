#include "common.h"

/// 按实际别名耗尽表；满表、单空槽与数组 fault 后仍核双槽原子发表及最小空号。
static void unix_slots(int base)
{
    struct descriptor_list copies = duplicate_until_full(base);
    int result[2] = {-1, -1};
    ERROR(socketpair(AF_UNIX, SOCK_STREAM, 0, result), EMFILE);
    CHECK(result[0] == -1 && result[1] == -1 && 2 <= copies.count);
    int last = copies.items[--copies.count];
    CHECK(close(last) == 0);
    ERROR(socketpair(AF_UNIX, SOCK_STREAM, 0, result), EMFILE);
    CHECK(dup(base) == last);
    CHECK(close(last) == 0);
    int other = copies.items[--copies.count];
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
    release_descriptors(&copies);
}

/// 双向全文超过环容量并回绕；dup/fork 共享身份，半关闭、零 EPIPE 和未读末关闭保真实错误。
void unix_pair(void)
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
void message_vector(void)
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

