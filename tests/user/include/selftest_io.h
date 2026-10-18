#ifndef BLOSSOM_SELFTEST_IO_H
#define BLOSSOM_SELFTEST_IO_H

#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

/* 调用点保留错误现场，统一跳到本函数拥有的资源清理路径。 */
#define ST_CHECK(expression) do { \
    if (!(expression)) { \
        fprintf(stderr, "%s:%d: %s errno=%d\n", __FILE__, __LINE__, \
                #expression, errno); \
        goto out; \
    } \
} while (0)

/// 完整交付有限字节，合法短写和 EINTR 不丢弃尚未发送的后缀。
static inline int st_write_all(int fd, const void *data, size_t length)
{
    const unsigned char *bytes = data;
    size_t done = 0;
    while (done < length) {
        ssize_t count = write(fd, bytes + done, length - done);
        if (count == -1 && errno == EINTR)
            continue;
        if (count <= 0)
            return -1;
        done += (size_t)count;
    }
    return 0;
}

/// 接收指定长度，提前 EOF 不得当作完整记录。
static inline int st_read_all(int fd, void *data, size_t length)
{
    unsigned char *bytes = data;
    size_t done = 0;
    while (done < length) {
        ssize_t count = read(fd, bytes + done, length - done);
        if (count == -1 && errno == EINTR)
            continue;
        if (count <= 0)
            return -1;
        done += (size_t)count;
    }
    return 0;
}

/// 清理只关闭本次持有的描述符，close 错误保留且不重试可能已释放的槽。
static inline int st_close(int *fd)
{
    if (*fd < 0)
        return 0;
    int owned = *fd;
    *fd = -1;
    return close(owned);
}

/// 等待明确子 PID，消费真实退出类型和值并撤销调用方的 PID 所有权。
static inline int st_reap(pid_t *child, int expected)
{
    int status;
    pid_t reaped;
    do {
        reaped = waitpid(*child, &status, 0);
    } while (reaped == -1 && errno == EINTR);
    if (reaped != *child)
        return -1;
    *child = -1;
    return WIFEXITED(status) && WEXITSTATUS(status) == expected ? 0 : -1;
}

/// 错误路径终止仍归本调用者拥有的孩子，再真实 wait，不能遗留后台任务。
static inline int st_stop_child(pid_t *child)
{
    if (*child <= 0)
        return 0;
    int result = kill(*child, SIGKILL);
    if (result == -1 && errno != ESRCH)
        return -1;
    int status;
    pid_t reaped;
    do {
        reaped = waitpid(*child, &status, 0);
    } while (reaped == -1 && errno == EINTR);
    if (reaped != *child)
        return -1;
    *child = -1;
    return 0;
}

#endif
