#ifndef NETWORK_SMOKE_COMMON_H
#define NETWORK_SMOKE_COMMON_H

#define _GNU_SOURCE
#include <arpa/inet.h>
#include <errno.h>
#include <netinet/tcp.h>
#include <net/if.h>
#include <net/route.h>
#include <fcntl.h>
#include <signal.h>
#include <stddef.h>
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
#include <sys/uio.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>
#include <poll.h>

_Static_assert(sizeof(struct sockaddr_in) == 16, "sockaddr_in ABI");
_Static_assert(offsetof(struct sockaddr_in, sin_family) == 0, "sockaddr family ABI");
_Static_assert(offsetof(struct sockaddr_in, sin_port) == 2, "sockaddr port ABI");
_Static_assert(offsetof(struct sockaddr_in, sin_addr) == 4, "sockaddr address ABI");
_Static_assert(sizeof(struct sockaddr_in6) == 28, "sockaddr_in6 ABI");
_Static_assert(offsetof(struct sockaddr_in6, sin6_family) == 0, "IPv6 family ABI");
_Static_assert(offsetof(struct sockaddr_in6, sin6_port) == 2, "IPv6 port ABI");
_Static_assert(offsetof(struct sockaddr_in6, sin6_flowinfo) == 4, "IPv6 flowinfo ABI");
_Static_assert(offsetof(struct sockaddr_in6, sin6_addr) == 8, "IPv6 address ABI");
_Static_assert(offsetof(struct sockaddr_in6, sin6_scope_id) == 24, "IPv6 scope ABI");
_Static_assert(sizeof(socklen_t) == 4, "socklen ABI");
_Static_assert(sizeof(struct epoll_event) == 16, "natural epoll ABI");
_Static_assert(offsetof(struct epoll_event, events) == 0, "epoll events ABI");
_Static_assert(offsetof(struct epoll_event, data) == 8, "epoll data ABI");
_Static_assert(sizeof(struct iovec) == 16, "iovec ABI");
_Static_assert(offsetof(struct iovec, iov_base) == 0, "iovec base ABI");
_Static_assert(offsetof(struct iovec, iov_len) == 8, "iovec length ABI");
_Static_assert(sizeof(struct msghdr) == 56, "msghdr LP64 ABI");
_Static_assert(offsetof(struct msghdr, msg_name) == 0, "message name ABI");
_Static_assert(offsetof(struct msghdr, msg_namelen) == 8, "message namelen ABI");
_Static_assert(offsetof(struct msghdr, msg_iov) == 16, "message iov ABI");
_Static_assert(offsetof(struct msghdr, msg_iovlen) == 24, "message iovlen ABI");
_Static_assert(offsetof(struct msghdr, msg_control) == 32, "message control ABI");
_Static_assert(offsetof(struct msghdr, msg_controllen) == 40, "message controllen ABI");
_Static_assert(offsetof(struct msghdr, msg_flags) == 48, "message flags ABI");

struct report { uint32_t phase, pid, value; };
#define CHECK(e) do { if (!(e)) fail(#e, __LINE__); } while (0)
#define ERROR(e, n) do { errno = 0; CHECK((e) == -1 && errno == (n)); } while (0)

// 这些进程内状态只在 common.c 定义；fork 后仍按原协议各自维护孩子和信号现场。
extern size_t child_count;
extern int ready_fd, go_fd;
extern volatile sig_atomic_t signals_seen, handler_report;

struct descriptor_list {
    int *items;
    size_t count, capacity;
};

// 共享帮助保留完整 I/O、真实阶段通知和失败回收；宏在调用点取得表达式和行号。
void fail(const char *expression, int line);
struct descriptor_list duplicate_until_full(int base);
void release_descriptors(struct descriptor_list *list);
void write_all(int fd, const void *data, size_t length);
void read_all(int fd, void *data, size_t length);
void report(uint32_t phase, pid_t pid, uint32_t value);
void proceed(void);
pid_t fork_child(void);
void reap(pid_t pid, int code);
struct sockaddr_in address(unsigned port);
int listener(unsigned port, int flags);
void pair(int *client, int *server);
short wait_ready(int fd, short events);
int socket_error(int fd);
unsigned char pattern(size_t position);
void install_handler(int number, int flags);
void short_exchange(int client, int server);

// TCP 模式及同一 ELF 的 exec 内部入口。
int exec_identity(int argc, char **argv);
void identity(const char *program);
void multiple_accept(void);
void nonblock_connect(void);
void stream(void);
void backpressure(void);
void half_close(void);
void address_faults(void);
void interrupted(void);
void epoll_stream(void);

// UDP 共用同一份大消息存储，预算模式不另建缓冲区副本。
int datagram(struct sockaddr_in *local);
void datagram_expect(int fd, const char *text, const struct sockaddr_in *sender);
void udp_address(void);
void udp_message(void);
void large_loopback(void);
void udp_buffer_budget(void);

void unix_pair(void);
void message_vector(void);

int option(int fd, int level, int name);
void set_option(int fd, int level, int name, int value);
void socket_options(void);
void protocol_options(void);
void buffer_hint(int fd, int name, int hint, int expected);
void buffer_budget(void);

struct sockaddr_in6 ipv6_address(void);
void ipv6_name(const struct sockaddr_in6 *actual, socklen_t length, uint16_t port);
int ipv6_bound(int type, struct sockaddr_in6 *local);
void ipv6_tcp(void);
void ipv6_udp(void);
void ipv6_mapped(void);

unsigned service_port(const char *text);
struct sockaddr_in nic_peer(unsigned port);
void nic_tcp(unsigned port);
void nic_udp(unsigned port);

// 配置及恢复标志仅由 control.c 拥有，失败回收也调用同一恢复入口。
int restore_control(void);
void interface_control(const char *proc_root);
void nic_control(const char *proc_root, unsigned tcp_port);

#endif
