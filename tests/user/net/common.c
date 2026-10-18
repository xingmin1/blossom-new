#include "net_common.h"

static pid_t children[8];
size_t child_count;
int ready_fd = -1, go_fd = -1;
volatile sig_atomic_t signals_seen;
volatile sig_atomic_t handler_report;

/// 失败时恢复本例配置并终止、回收已发表的孩子，描述符由正常退出释放。
void fail(const char *expression, int line)
{
    int saved = errno;
    fprintf(stderr, "network-smoke line=%d check=%s errno=%d\n", line, expression, saved);
    (void)restore_control();
    for (size_t i = 0; i < child_count; ++i) {
        if (kill(children[i], SIGKILL) == -1 && errno != ESRCH)
            fprintf(stderr, "network-smoke cleanup kill pid=%d errno=%d\n", children[i], errno);
    }
    for (size_t i = 0; i < child_count; ++i) {
        pid_t reaped;
        do { reaped = waitpid(children[i], NULL, 0); } while (reaped == -1 && errno == EINTR);
        if (reaped != children[i] && errno != ECHILD)
            fprintf(stderr, "network-smoke cleanup wait pid=%d errno=%d\n", children[i], errno);
    }
    exit(1);
}

/// 保存本例实际取得的每个别名直到真实 EMFILE，容量只随已有描述符增长。
struct descriptor_list duplicate_until_full(int base)
{
    struct descriptor_list list = {0};
    for (;;) {
        int fd = dup(base);
        if (fd == -1) {
            CHECK(errno == EMFILE);
            return list;
        }
        if (list.count == list.capacity) {
            CHECK(list.capacity <= SIZE_MAX / sizeof *list.items / 2);
            size_t capacity = list.capacity ? list.capacity * 2 : 16;
            int *items = realloc(list.items, capacity * sizeof *items);
            CHECK(items != NULL);
            list.items = items;
            list.capacity = capacity;
        }
        list.items[list.count++] = fd;
    }
}

/// 只归还列表尚未移交的别名，全部 close 成功后释放对应的用户存储。
void release_descriptors(struct descriptor_list *list)
{
    while (0 < list->count) CHECK(close(list->items[--list->count]) == 0);
    free(list->items);
    *list = (struct descriptor_list){0};
}

/// 按真实短写推进完整字节，零进度和中断均保留为失败。
void write_all(int fd, const void *data, size_t length)
{
    const unsigned char *bytes = data;
    size_t done = 0;
    while (done < length) {
        ssize_t count = write(fd, bytes + done, length - done);
        CHECK(0 < count);
        done += (size_t)count;
    }
}

/// 读取准确记录长度，避免把 EOF 或短读当成完成。
void read_all(int fd, void *data, size_t length)
{
    unsigned char *bytes = data;
    size_t done = 0;
    while (done < length) {
        ssize_t count = read(fd, bytes + done, length - done);
        CHECK(0 < count);
        done += (size_t)count;
    }
}

/// 原管道交付真实 errno 或阻塞阶段，协调者不从延时推断握手。
void report(uint32_t phase, pid_t pid, uint32_t value)
{
    CHECK(0 <= ready_fd);
    struct report message = { phase, (uint32_t)pid, value };
    write_all(ready_fd, &message, sizeof message);
}

/// 协调者观察真实协议与调度事实后只交付一个放行字节。
void proceed(void)
{
    unsigned char byte;
    read_all(go_fd, &byte, 1);
    CHECK(byte == 'G');
}

/// 孩子继承普通描述符，父保存 PID 供所有错误路径回收。
pid_t fork_child(void)
{
    CHECK(child_count < sizeof children / sizeof children[0]);
    pid_t pid = fork();
    CHECK(0 <= pid);
    if (pid == 0) child_count = 0;
    else children[child_count++] = pid;
    return pid;
}

/// 真实 wait 状态必须为指定退出值，不能只检查已经结束。
void reap(pid_t pid, int code)
{
    int status;
    CHECK(waitpid(pid, &status, 0) == pid);
    CHECK(WIFEXITED(status) && WEXITSTATUS(status) == code);
    for (size_t i = 0; i < child_count; ++i) {
        if (children[i] == pid) {
            children[i] = children[--child_count];
            break;
        }
    }
}

/// 地址使用头文件真实布局和网络字节序。
struct sockaddr_in address(unsigned port)
{
    struct sockaddr_in value = { .sin_family = AF_INET,
        .sin_port = htons((uint16_t)port), .sin_addr.s_addr = htonl(INADDR_LOOPBACK) };
    return value;
}

/// listener 使用正式 socket/bind/listen，不依赖测试专用网络入口。
int listener(unsigned port, int flags)
{
    int fd = socket(AF_INET, SOCK_STREAM | flags, IPPROTO_TCP);
    CHECK(0 <= fd);
    struct sockaddr_in local = address(port);
    CHECK(bind(fd, (struct sockaddr *)&local, sizeof local) == 0);
    CHECK(listen(fd, 8) == 0);
    return fd;
}

/// 同进程建立真实环回连接，握手由后台推进并先于 accept 完成。
void pair(int *client, int *server)
{
    int listening = listener(0, 0);
    struct sockaddr_in local;
    socklen_t length = sizeof local;
    CHECK(getsockname(listening, (struct sockaddr *)&local, &length) == 0);
    CHECK(length == sizeof local && local.sin_port != 0);
    *client = socket(AF_INET, SOCK_STREAM, 0);
    CHECK(0 <= *client);
    CHECK(connect(*client, (struct sockaddr *)&local, sizeof local) == 0);
    *server = accept(listening, NULL, NULL);
    CHECK(0 <= *server);
    CHECK(close(listening) == 0);
}

/// 单次 ppoll 直接消费目标 syscall 的就绪集合。
short wait_ready(int fd, short events)
{
    struct pollfd item = { .fd = fd, .events = events };
    struct timespec timeout = { .tv_sec = 5 };
    CHECK(syscall(SYS_ppoll, &item, 1, &timeout, NULL, 8) == 1);
    return item.revents;
}

/// 内核消费一次 SO_ERROR，长度必须为实际 int ABI。
int socket_error(int fd)
{
    int error = -1;
    socklen_t length = sizeof error;
    CHECK(getsockopt(fd, SOL_SOCKET, SO_ERROR, &error, &length) == 0);
    CHECK(length == sizeof error);
    return error;
}

/// 图样随逻辑位置变化，环回绕和前缀丢失不能满足逐字节比较。
unsigned char pattern(size_t position)
{
    return (unsigned char)((position * 37 + position / 251 + 19) & 255);
}

/// handler 只保存调用线程实际收到的信号；重启窗口可用原 pipe 报告 handler 已执行。
static void signal_handler(int number)
{
    signals_seen += 1;
    if (handler_report) {
        struct report message = { 'H', (uint32_t)getpid(), (uint32_t)number };
        ssize_t count = write(ready_fd, &message, sizeof message);
        if (count != sizeof message) _exit(2);
    }
}

/// 安装真实 sigaction，是否重启由原信号返回路径决定。
void install_handler(int number, int flags)
{
    struct sigaction action = { .sa_handler = signal_handler, .sa_flags = flags };
    CHECK(sigemptyset(&action.sa_mask) == 0);
    CHECK(sigaction(number, &action, NULL) == 0);
}

/// 短双向流在各自 FIN 后领取 EOF，关闭双方前不遗留未读字节。
void short_exchange(int client, int server)
{
    unsigned char output[32];
    write_all(client, "mapped\0request\xff", 15);
    CHECK(shutdown(client, SHUT_WR) == 0);
    read_all(server, output, 15);
    CHECK(memcmp(output, "mapped\0request\xff", 15) == 0);
    CHECK(read(server, output, 1) == 0);
    write_all(server, "native\0response\xfe", 16);
    CHECK(shutdown(server, SHUT_WR) == 0);
    read_all(client, output, 16);
    CHECK(memcmp(output, "native\0response\xfe", 16) == 0);
    CHECK(read(client, output, 1) == 0);
    CHECK(close(client) == 0 && close(server) == 0);
}

