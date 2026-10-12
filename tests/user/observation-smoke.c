#define _GNU_SOURCE
#include <blossom-trace.h>
#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

#define CHECK(e) do { if (!(e)) fail(#e, __LINE__); } while (0)
#define ERROR(e, n) do { errno = 0; CHECK((e) == -1 && errno == (n)); } while (0)
static char first_capture[131072], replay_capture[131072], next_capture[131072];

/// 断言失败保留 syscall 的真实 errno，由原用户身份图回收所有打开对象。
static void fail(const char *expression, int line)
{
    fprintf(stderr, "observation-smoke line=%d check=%s errno=%d\n", line, expression, errno);
    exit(1);
}

/// 普通私有 pipe 按实际短写发送完整控制字节。
static void write_all(int fd, const void *data, size_t length)
{
    const unsigned char *bytes = data;
    size_t done = 0;
    while (done < length) {
        ssize_t count = write(fd, bytes + done, length - done);
        CHECK(0 < count);
        done += (size_t)count;
    }
}

/// 控制回复必须完整，EOF 与零进度均不能代替实际完成。
static void read_all(int fd, void *data, size_t length)
{
    unsigned char *bytes = data;
    size_t done = 0;
    while (done < length) {
        ssize_t count = read(fd, bytes + done, length - done);
        CHECK(0 < count);
        done += (size_t)count;
    }
}

/// 内核回复实际首条日志序号，普通协调只借继承的 fd 3 和 fd 4。
static uint64_t command(char operation)
{
    uint64_t sequence;
    write_all(3, &operation, 1);
    read_all(4, &sequence, sizeof sequence);
    return sequence;
}

/// 直接调用双架构已有 ppoll ABI，避免依赖不存在的旧 poll syscall。
static short ready(int fd, short events, long seconds)
{
    struct pollfd item = { .fd = fd, .events = events };
    struct timespec timeout = { .tv_sec = seconds };
    int count = (int)syscall(SYS_ppoll, &item, 1, &timeout, NULL, 8);
    CHECK(0 <= count && count <= 1);
    CHECK((count == 0) == (item.revents == 0));
    return item.revents;
}

/// 每次 kmsg read 必须交付一整行，头字段和所有转义字节由真实 syscall 结果解析。
static uint64_t record(int fd, unsigned expected_priority, const char *expected_body)
{
    char bytes[2200];
    ssize_t count = read(fd, bytes, sizeof bytes - 1);
    CHECK(0 < count);
    bytes[count] = '\0';
    unsigned priority;
    unsigned long long sequence, timestamp;
    int header = 0;
    CHECK(sscanf(bytes, "%u,%llu,%llu,-;%n", &priority, &sequence, &timestamp, &header) == 3);
    CHECK(0 < header && header < count && priority == expected_priority && 0 < sequence);
    CHECK(bytes[count - 1] == '\n');
    for (ssize_t i = header; i < count - 1; ++i)
        CHECK(32 <= (unsigned char)bytes[i] && (unsigned char)bytes[i] < 127);
    CHECK(strcmp(bytes + header, expected_body) == 0);
    return (uint64_t)sequence;
}

/// 独立游标、dup、错误消费与覆盖恢复均走真实 syscall；epoll 只订阅覆盖事件。
static void kmsg(void)
{
    int fd = open("/dev/kmsg", O_RDONLY | O_NONBLOCK);
    int independent = open("/dev/kmsg", O_RDONLY | O_NONBLOCK);
    CHECK(0 <= fd && 0 <= independent);
    int alias = dup(fd);
    CHECK(0 <= alias && alias != fd);
    struct stat a, b;
    CHECK(fstat(fd, &a) == 0 && fstat(independent, &b) == 0);
    CHECK(S_ISCHR(a.st_mode) && a.st_ino == b.st_ino && a.st_dev == b.st_dev);
    CHECK(lseek(fd, 0, SEEK_END) == 0 && lseek(independent, 0, SEEK_END) == 0);
    char byte;
    ERROR(read(fd, &byte, 1), EAGAIN);
    CHECK(ready(fd, POLLIN, 0) == 0);
    uint64_t first = command('F');
    const char *escaped = "observation format\\x09\\x0a\\x0d\\x5c\\x7f\\xc3\\xa9\n";
    CHECK((ready(fd, POLLIN | POLLRDNORM, 0) & (POLLIN | POLLRDNORM)) == (POLLIN | POLLRDNORM));
    CHECK(record(fd, 6, escaped) == first);
    CHECK(record(independent, 6, escaped) == first);
    CHECK(record(alias, 4, "observation shared second\n") == first + 1);
    CHECK(record(fd, 3, "observation shared third\n") == first + 2);
    ERROR(read(alias, &byte, 1), EAGAIN);
    CHECK(record(independent, 4, "observation shared second\n") == first + 1);
    CHECK(record(independent, 3, "observation shared third\n") == first + 2);

    CHECK(lseek(fd, 0, SEEK_END) == 0);
    int epoll = epoll_create1(EPOLL_CLOEXEC);
    int errors = epoll_create1(EPOLL_CLOEXEC);
    CHECK(0 <= epoll && 0 <= errors);
    struct epoll_event interest = { .events = EPOLLPRI, .data.u64 = 1 };
    CHECK(epoll_ctl(epoll, EPOLL_CTL_ADD, fd, &interest) == 0);
    interest.events = 0;
    interest.data.u64 = 2;
    CHECK(epoll_ctl(errors, EPOLL_CTL_ADD, alias, &interest) == 0);
    struct epoll_event event;
    CHECK(epoll_pwait(epoll, &event, 1, 0, NULL) == 0);
    CHECK(epoll_pwait(errors, &event, 1, 0, NULL) == 0);
    first = command('O');
    short covered = ready(fd, POLLIN | POLLRDNORM | POLLPRI, 0);
    CHECK((covered & (POLLIN | POLLRDNORM | POLLPRI | POLLERR)) == (POLLIN | POLLRDNORM | POLLPRI | POLLERR));
    CHECK(epoll_pwait(epoll, &event, 1, 5000, NULL) == 1);
    CHECK(event.data.u64 == 1 && event.events == (EPOLLPRI | EPOLLERR));
    CHECK(epoll_pwait(errors, &event, 1, 5000, NULL) == 1);
    CHECK(event.data.u64 == 2 && event.events == EPOLLERR);
    char buffer[2200];
    ERROR(read(fd, buffer, sizeof buffer), EPIPE);
    CHECK(epoll_pwait(epoll, &event, 1, 0, NULL) == 0);
    CHECK(epoll_pwait(errors, &event, 1, 0, NULL) == 0);
    CHECK(close(epoll) == 0 && close(errors) == 0);
    CHECK(lseek(independent, 0, SEEK_SET) == 0);
    CHECK(record(fd, 7, "observation cover 1\n") == first + 1);
    CHECK(record(independent, 7, "observation cover 1\n") == first + 1);
    CHECK(record(alias, 7, "observation cover 2\n") == first + 2);
    CHECK(lseek(fd, 0, SEEK_END) == 0);

    first = command('D');
    ERROR(read(fd, &byte, 1), EINVAL);
    CHECK(record(alias, 6, "observation after small\n") == first + 1);
    first = command('E');
    unsigned char *mapping = mmap(NULL, 8192, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    CHECK(mapping != MAP_FAILED && munmap(mapping + 4096, 4096) == 0);
    ERROR(syscall(SYS_read, fd, mapping + 4093, sizeof buffer), EFAULT);
    CHECK(record(alias, 6, "observation after fault\n") == first + 1);
    CHECK(munmap(mapping, 4096) == 0);
    ERROR(read(fd, buffer, sizeof buffer), EAGAIN);
    CHECK(ready(alias, POLLIN, 0) == 0);

    write_all(3, "P", 1);
    CHECK(ready(fd, POLLIN, 5) & POLLIN);
    read_all(4, &first, sizeof first);
    CHECK(record(fd, 6, "observation poll wake\n") == first);
    ERROR(read(alias, buffer, sizeof buffer), EAGAIN);
    CHECK(close(alias) == 0 && close(independent) == 0 && close(fd) == 0);
}

/// 真实 lo TCP 完成握手、双向字节、FIN 与最后 OFD 关闭，不直接生成 trace 事件。
static void loopback(void)
{
    int listener = socket(AF_INET, SOCK_STREAM, 0);
    CHECK(0 <= listener);
    struct sockaddr_in address = { .sin_family = AF_INET, .sin_addr.s_addr = htonl(INADDR_LOOPBACK) };
    CHECK(bind(listener, (struct sockaddr *)&address, sizeof address) == 0);
    CHECK(listen(listener, 1) == 0);
    socklen_t length = sizeof address;
    CHECK(getsockname(listener, (struct sockaddr *)&address, &length) == 0 && length == sizeof address);
    int client = socket(AF_INET, SOCK_STREAM, 0);
    CHECK(0 <= client && connect(client, (struct sockaddr *)&address, sizeof address) == 0);
    int server = accept(listener, NULL, NULL);
    CHECK(0 <= server && close(listener) == 0);
    char bytes[8];
    write_all(client, "request", 7);
    CHECK(shutdown(client, SHUT_WR) == 0);
    read_all(server, bytes, 7);
    CHECK(memcmp(bytes, "request", 7) == 0 && read(server, bytes, 1) == 0);
    write_all(server, "reply", 5);
    CHECK(shutdown(server, SHUT_WR) == 0);
    read_all(client, bytes, 5);
    CHECK(memcmp(bytes, "reply", 5) == 0 && read(client, bytes, 1) == 0);
    CHECK(close(client) == 0 && close(server) == 0);
}

/// 快照按实际短读收集；固定容量足以容纳四 CPU 各 128 条 ABI 文本记录。
static size_t snapshot(int fd, char *bytes, size_t capacity)
{
    size_t done = 0;
    while (done < capacity - 1) {
        ssize_t count = read(fd, bytes + done, capacity - 1 - done);
        CHECK(0 <= count);
        if (count == 0) {
            bytes[done] = '\0';
            return done;
        }
        done += (size_t)count;
    }
    fail("trace snapshot exceeds fixed capacity", __LINE__);
    return 0;
}

/// 固定文本按每 CPU 序号和覆盖量解析；正常生产点必须留下调度、等待和 TCP 变迁。
static void trace_records(const char *bytes, unsigned required_kinds)
{
    unsigned cpus, capacity;
    int consumed = 0;
    CHECK(sscanf(bytes, "ktrace version=1 unit=ns cpus=%u capacity=%u%n", &cpus, &capacity, &consumed) == 2);
    CHECK(0 < cpus && cpus <= 4 && capacity == 128 && bytes[consumed] == '\n');
    const char *line = bytes + consumed + 1;
    unsigned counts[4] = {0}, kinds = 0;
    unsigned long long next[4] = {0}, previous[4] = {0};
    unsigned summaries = 0;
    while (*line) {
        unsigned cpu;
        unsigned long long sequence, overwritten;
        consumed = 0;
        if (sscanf(line, "cpu id=%u next_seq=%llu overwritten=%llu%n", &cpu, &sequence, &overwritten, &consumed) == 3) {
            CHECK(cpu < cpus && next[cpu] == 0 && 0 < sequence && line[consumed] == '\n');
            CHECK(overwritten == (128 < sequence - 1 ? sequence - 1 - 128 : 0));
            next[cpu] = sequence;
            ++summaries;
        } else {
            unsigned kind;
            unsigned long long ns, object, value;
            char task[24];
            CHECK(sscanf(line, "record seq=%llu ns=%llu cpu=%u task=%23s kind=%u object=%llu value=%llu%n", &sequence, &ns, &cpu, task, &kind, &object, &value, &consumed) == 7);
            CHECK(cpu < cpus && 0 < next[cpu] && previous[cpu] < sequence && sequence < next[cpu]);
            CHECK(line[consumed] == '\n' && 1 <= kind && kind <= 6);
            CHECK(sequence == (counts[cpu] == 0 ? (128 < next[cpu] - 1 ? next[cpu] - 128 : 1) : previous[cpu] + 1));
            previous[cpu] = sequence;
            ++counts[cpu];
            if (kind == 1 || kind == 2) {
                char *end;
                unsigned long long identity = strtoull(task, &end, 10);
                CHECK(*end == '\0' && 0 < identity && object == identity);
                CHECK(kind == 1 ? value == 0 : 0 < value);
            }
            if (kind == 5) {
                CHECK(0 < object && (value >> 8) <= 10 && (value & 255) <= 10 && (value >> 8) != (value & 255));
            }
            kinds |= 1u << kind;
        }
        line += consumed + 1;
    }
    CHECK(summaries == cpus);
    for (unsigned cpu = 0; cpu < cpus; ++cpu)
        CHECK(counts[cpu] == (next[cpu] - 1 < 128 ? next[cpu] - 1 : 128));
    CHECK((kinds & required_kinds) == required_kinds);
}

/// 控制、采集中 Busy、冻结后字节及旧 OFD 跨后继捕获的独立性都走普通 ioctl/read。
static void trace(void)
{
    int old = open("/dev/ktrace", O_RDONLY), current = open("/dev/ktrace", O_RDONLY);
    CHECK(0 <= old && 0 <= current);
    size_t initial = snapshot(old, next_capture, sizeof next_capture);
    CHECK(0 < initial);
    trace_records(next_capture, 0);
    ERROR(ioctl(current, 0x4203, 0), ENOTTY);
    CHECK(ioctl(current, BLOSSOM_TRACE_START, 0) == 0);
    ERROR(ioctl(current, BLOSSOM_TRACE_START, 0), EBUSY);
    ERROR(read(old, replay_capture, 1), EBUSY);
    ERROR(read(current, replay_capture, 1), EBUSY);
    CHECK(command('T') == 0);
    loopback();
    CHECK(ioctl(current, BLOSSOM_TRACE_STOP, 0) == 0);
    CHECK(ioctl(current, BLOSSOM_TRACE_STOP, 0) == 0);
    size_t first = snapshot(current, first_capture, sizeof first_capture);
    CHECK(0 < first);
    trace_records(first_capture, (1u << 1) | (1u << 2) | (1u << 5));
    CHECK(lseek(current, 0, SEEK_SET) == 0);
    size_t replay = snapshot(current, replay_capture, sizeof replay_capture);
    CHECK(replay == first && memcmp(first_capture, replay_capture, first) == 0);
    CHECK(command('T') == 0);
    int frozen = open("/dev/ktrace", O_RDONLY);
    CHECK(0 <= frozen);
    replay = snapshot(frozen, replay_capture, sizeof replay_capture);
    CHECK(replay == first && memcmp(first_capture, replay_capture, first) == 0);
    CHECK(close(frozen) == 0);
    CHECK(ioctl(current, BLOSSOM_TRACE_START, 0) == 0);
    ERROR(read(current, replay_capture, 1), EBUSY);
    CHECK(command('T') == 0);
    CHECK(ioctl(current, BLOSSOM_TRACE_STOP, 0) == 0);
    CHECK(lseek(current, 0, SEEK_SET) == 0);
    replay = snapshot(current, replay_capture, sizeof replay_capture);
    CHECK(replay == first && memcmp(first_capture, replay_capture, first) == 0);
    CHECK(lseek(old, 0, SEEK_SET) == 0);
    replay = snapshot(old, replay_capture, sizeof replay_capture);
    CHECK(replay == initial && memcmp(next_capture, replay_capture, initial) == 0);
    int fresh = open("/dev/ktrace", O_RDONLY);
    CHECK(0 <= fresh);
    size_t next = snapshot(fresh, next_capture, sizeof next_capture);
    CHECK(0 < next);
    trace_records(next_capture, (1u << 1) | (1u << 2));
    CHECK(close(fresh) == 0 && close(current) == 0 && close(old) == 0);
}

/// 两个具名模式只使用普通 syscall；完成标记让原协调任务退出并独立 join。
int main(int argc, char **argv)
{
    CHECK(argc == 2);
    CHECK(getuid() == 0);
    if (strcmp(argv[1], "kmsg") == 0) kmsg();
    else if (strcmp(argv[1], "trace") == 0) trace();
    else fail("unknown observation mode", __LINE__);
    CHECK(command('Q') == 0);
    CHECK(close(3) == 0 && close(4) == 0);
    printf("observation-smoke mode=%s passed\n", argv[1]);
    return 0;
}
