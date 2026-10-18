#include "net_common.h"

static int control_fd = -1, control_route_deleted, control_lo_changed;
static struct ifreq control_lo;
static struct rtentry control_default;

/// 每次实际修改只尝试一次恢复，恢复失败按原用户退出错误报告。
int restore_control(void)
{
    int result = 0;
    if (control_fd < 0) return 0;
    if (control_route_deleted) {
        control_route_deleted = 0;
        if (ioctl(control_fd, SIOCADDRT, &control_default) == -1) {
            fprintf(stderr, "network-smoke cleanup route errno=%d\n", errno);
            result = -1;
        }
    }
    if (control_lo_changed) {
        control_lo_changed = 0;
        if (ioctl(control_fd, SIOCSIFFLAGS, &control_lo) == -1) {
            fprintf(stderr, "network-smoke cleanup lo flags errno=%d\n", errno);
            result = -1;
        }
    }
    return result;
}

/// 普通路径由 argv 指定 proc 根，实际节点由 VFS 解析，不依赖内核路径特判。
static int proc_open(const char *root, const char *node)
{
    char path[256];
    int length = snprintf(path, sizeof path, "%s/net/%s", root, node);
    CHECK(0 < length && (size_t)length < sizeof path);
    int fd = open(path, O_RDONLY | O_CLOEXEC);
    CHECK(0 <= fd);
    return fd;
}

/// 一份打开快照从头读到真实 EOF，文本容量只覆盖这两个固定小信息文件。
static size_t proc_text(int fd, char *text, size_t capacity)
{
    CHECK(lseek(fd, 0, SEEK_SET) == 0);
    size_t length = 0;
    for (;;) {
        CHECK(length < capacity - 1);
        ssize_t count = read(fd, text + length, capacity - 1 - length);
        CHECK(0 <= count);
        if (count == 0) break;
        length += (size_t)count;
    }
    text[length] = '\0';
    return length;
}

/// dup 的交错顺序读取必须推进同一游标，配置变化后原 OFD 仍交付原全文。
static void proc_unchanged(int fd, const char *expected, size_t length)
{
    char text[2048], byte;
    CHECK(20 <= length && length < sizeof text);
    int alias = dup(fd);
    CHECK(0 <= alias);
    CHECK(lseek(fd, 0, SEEK_SET) == 0);
    read_all(fd, text, 7);
    read_all(alias, text + 7, 13);
    read_all(fd, text + 20, length - 20);
    CHECK(memcmp(text, expected, length) == 0);
    CHECK(read(alias, &byte, 1) == 0);
    CHECK(proc_text(alias, text, sizeof text) == length);
    CHECK(memcmp(text, expected, length) == 0);
    CHECK(close(alias) == 0);
}

/// 标准路由行按十一列解析，地址与 mask 比较本次实际配置对应的 proc 小端值。
static int proc_route_flags(const char *text, const char *interface,
    unsigned long destination, unsigned long gateway, unsigned long mask)
{
    const char *line = strchr(text, '\n');
    CHECK(line != NULL);
    while (*++line) {
        char name[16];
        unsigned long actual_destination, actual_gateway, actual_mask;
        unsigned flags, references, use, metric, mtu, window, irtt;
        CHECK(sscanf(line, "%15s %lx %lx %x %u %u %u %lx %u %u %u",
            name, &actual_destination, &actual_gateway, &flags, &references,
            &use, &metric, &actual_mask, &mtu, &window, &irtt) == 11);
        if (strcmp(name, interface) == 0 && actual_destination == destination) {
            CHECK(actual_gateway == gateway && actual_mask == mask);
            return (int)flags;
        }
        line = strchr(line, '\n');
        CHECK(line != NULL);
    }
    return -1;
}

/// lo 的真实帧计数从标准 dev 行读取，未监测字段不参与业务断言。
static void proc_lo_counts(const char *text, unsigned long long counts[4])
{
    const char *line = strstr(text, "lo:");
    CHECK(line != NULL);
    CHECK(sscanf(line, "lo: %llu %llu %*u %*u %*u %*u %*u %*u %llu %llu",
        &counts[0], &counts[1], &counts[2], &counts[3]) == 4);
}

/// getter 使用本次 socket 的 ioctl 获取真实 IPv4 字段，setter 复用同一头文件布局。
static struct ifreq interface_address(int fd, const char *name,
    unsigned long request, uint32_t expected)
{
    struct ifreq value = {0};
    CHECK(strlen(name) < sizeof value.ifr_name);
    strcpy(value.ifr_name, name);
    CHECK(ioctl(fd, request, &value) == 0);
    const struct sockaddr_in *actual = (const struct sockaddr_in *)&value.ifr_addr;
    CHECK(actual->sin_family == AF_INET && actual->sin_addr.s_addr == htonl(expected));
    return value;
}

/// 配置完成后的两族都传递真实字节，IPv6 不只依赖 bind 或地址常量存在。
static void control_loopback(void)
{
    int client, server;
    pair(&client, &server);
    short_exchange(client, server);
    int receiver = socket(AF_INET6, SOCK_DGRAM, IPPROTO_UDP);
    int sender = socket(AF_INET6, SOCK_DGRAM, IPPROTO_UDP);
    CHECK(0 <= receiver && 0 <= sender);
    struct sockaddr_in6 local = ipv6_address();
    CHECK(bind(receiver, (struct sockaddr *)&local, sizeof local) == 0);
    socklen_t length = sizeof local;
    CHECK(getsockname(receiver, (struct sockaddr *)&local, &length) == 0);
    ipv6_name(&local, length, 0);
    CHECK(sendto(sender, "control-v6", 10, 0, (struct sockaddr *)&local, sizeof local) == 10);
    CHECK(wait_ready(receiver, POLLIN) & POLLIN);
    char text[10];
    struct sockaddr_in6 source;
    length = sizeof source;
    CHECK(recvfrom(receiver, text, sizeof text, 0, (struct sockaddr *)&source, &length) == 10);
    CHECK(memcmp(text, "control-v6", sizeof text) == 0);
    ipv6_name(&source, length, 0);
    CHECK(close(sender) == 0 && close(receiver) == 0);
}

/// 普通 root 消费所选 lo 控制、基础 errno 与真实 proc 快照，再恢复 flags 并验证双族业务。
void interface_control(const char *proc_root)
{
    control_fd = socket(AF_INET, SOCK_DGRAM, IPPROTO_UDP);
    CHECK(0 <= control_fd);
    memset(&control_lo, 0, sizeof control_lo);
    strcpy(control_lo.ifr_name, "lo");
    CHECK(ioctl(control_fd, SIOCGIFFLAGS, &control_lo) == 0);
    CHECK((control_lo.ifr_flags & (IFF_UP | IFF_LOOPBACK)) == (IFF_UP | IFF_LOOPBACK));
    struct ifreq value = interface_address(control_fd, "lo", SIOCGIFADDR, INADDR_LOOPBACK);
    CHECK(ioctl(control_fd, SIOCSIFADDR, &value) == 0);
    value = interface_address(control_fd, "lo", SIOCGIFNETMASK, 0xff000000);
    CHECK(ioctl(control_fd, SIOCSIFNETMASK, &value) == 0);
    ERROR(ioctl(control_fd, SIOCGIFFLAGS, NULL), EFAULT);
    ((struct sockaddr_in *)&value.ifr_netmask)->sin_addr.s_addr = htonl(0xff00ff00);
    ERROR(ioctl(control_fd, SIOCSIFNETMASK, &value), EINVAL);
    ERROR(ioctl(control_fd, 0x89ff, NULL), ENOTTY);
    interface_address(control_fd, "lo", SIOCGIFADDR, INADDR_LOOPBACK);
    interface_address(control_fd, "lo", SIOCGIFNETMASK, 0xff000000);

    char before[2048], after[2048];
    int old = proc_open(proc_root, "route");
    size_t before_length = proc_text(old, before, sizeof before);
    CHECK(proc_route_flags(before, "lo", 0x0000007f, 0, 0x000000ff) == RTF_UP);
    value = control_lo;
    value.ifr_flags &= ~IFF_UP;
    CHECK(ioctl(control_fd, SIOCSIFFLAGS, &value) == 0);
    control_lo_changed = 1;
    CHECK(ioctl(control_fd, SIOCGIFFLAGS, &value) == 0);
    CHECK((value.ifr_flags & IFF_UP) == 0 && (value.ifr_flags & IFF_LOOPBACK) != 0);
    int fresh = proc_open(proc_root, "route");
    proc_text(fresh, after, sizeof after);
    CHECK(proc_route_flags(after, "lo", 0x0000007f, 0, 0x000000ff) == 0);
    proc_unchanged(old, before, before_length);
    CHECK(close(fresh) == 0 && close(old) == 0);
    CHECK(restore_control() == 0);
    value = control_lo;
    CHECK(ioctl(control_fd, SIOCGIFFLAGS, &value) == 0);
    CHECK(value.ifr_flags == control_lo.ifr_flags);

    old = proc_open(proc_root, "dev");
    before_length = proc_text(old, before, sizeof before);
    unsigned long long first[4], second[4];
    proc_lo_counts(before, first);
    control_loopback();
    fresh = proc_open(proc_root, "dev");
    proc_text(fresh, after, sizeof after);
    proc_lo_counts(after, second);
    for (size_t i = 0; i < 4; ++i) CHECK(first[i] < second[i]);
    proc_unchanged(old, before, before_length);
    CHECK(close(fresh) == 0 && close(old) == 0);
    CHECK(close(control_fd) == 0);
    control_fd = -1;
    printf("network-smoke control lo-get-set=1 up=1 ipv6=1 proc-open-dup=1 errno=1\n");
}

/// 默认项删除影响新路由选择，重复删除给 ESRCH；恢复后原 NIC peer 完成正常全文业务。
void nic_control(const char *proc_root, unsigned tcp_port)
{
    interface_control(proc_root);
    control_fd = socket(AF_INET, SOCK_DGRAM, IPPROTO_UDP);
    CHECK(0 <= control_fd);
    struct ifreq value = interface_address(control_fd, "eth0", SIOCGIFADDR, 0x0a00020f);
    CHECK(ioctl(control_fd, SIOCSIFADDR, &value) == 0);
    value = interface_address(control_fd, "eth0", SIOCGIFNETMASK, 0xffffff00);
    CHECK(ioctl(control_fd, SIOCSIFNETMASK, &value) == 0);
    memset(&value, 0, sizeof value);
    strcpy(value.ifr_name, "eth0");
    CHECK(ioctl(control_fd, SIOCGIFFLAGS, &value) == 0);
    CHECK((value.ifr_flags & (IFF_UP | IFF_BROADCAST)) == (IFF_UP | IFF_BROADCAST));
    CHECK(ioctl(control_fd, SIOCSIFFLAGS, &value) == 0);
    memset(&control_default, 0, sizeof control_default);
    struct sockaddr_in destination = { .sin_family = AF_INET };
    struct sockaddr_in gateway = nic_peer(0);
    memcpy(&control_default.rt_dst, &destination, sizeof destination);
    memcpy(&control_default.rt_gateway, &gateway, sizeof gateway);
    memcpy(&control_default.rt_genmask, &destination, sizeof destination);
    control_default.rt_flags = RTF_UP | RTF_GATEWAY;
    control_default.rt_dev = "eth0";
    char before[2048], after[2048];
    int old = proc_open(proc_root, "route");
    size_t before_length = proc_text(old, before, sizeof before);
    CHECK(proc_route_flags(before, "eth0", 0, 0x0202000a, 0) == (RTF_UP | RTF_GATEWAY));
    CHECK(ioctl(control_fd, SIOCDELRT, &control_default) == 0);
    control_route_deleted = 1;
    ERROR(ioctl(control_fd, SIOCDELRT, &control_default), ESRCH);
    int fresh = proc_open(proc_root, "route");
    proc_text(fresh, after, sizeof after);
    CHECK(proc_route_flags(after, "eth0", 0, 0x0202000a, 0) == -1);
    CHECK(proc_route_flags(after, "eth0", 0x0002000a, 0, 0x00ffffff) == RTF_UP);
    proc_unchanged(old, before, before_length);
    CHECK(close(fresh) == 0 && close(old) == 0);
    struct sockaddr_in remote = address(tcp_port);
    remote.sin_addr.s_addr = htonl(0xc0000201);
    int routed = socket(AF_INET, SOCK_DGRAM, IPPROTO_UDP);
    CHECK(0 <= routed);
    ERROR(connect(routed, (struct sockaddr *)&remote, sizeof remote), ENETUNREACH);
    CHECK(ioctl(control_fd, SIOCADDRT, &control_default) == 0);
    control_route_deleted = 0;
    fresh = proc_open(proc_root, "route");
    proc_text(fresh, after, sizeof after);
    CHECK(proc_route_flags(after, "eth0", 0, 0x0202000a, 0) == (RTF_UP | RTF_GATEWAY));
    CHECK(close(fresh) == 0);
    CHECK(connect(routed, (struct sockaddr *)&remote, sizeof remote) == 0);
    struct sockaddr_in local;
    socklen_t length = sizeof local;
    CHECK(getsockname(routed, (struct sockaddr *)&local, &length) == 0);
    CHECK(length == sizeof local && local.sin_addr.s_addr == htonl(0x0a00020f));
    CHECK(close(routed) == 0);
    CHECK(restore_control() == 0 && close(control_fd) == 0);
    control_fd = -1;
    nic_tcp(tcp_port);
    printf("network-smoke nic-control default-delete-add=1 missing-esrch=1 route-selection=1\n");
}

