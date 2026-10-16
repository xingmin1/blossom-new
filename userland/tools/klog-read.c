#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <unistd.h>

/// 非阻塞读取完整内核记录，覆盖后沿设备恢复的游标继续，读空时正常退出。
int main(void)
{
    /* 正文最多 512 字节，每字节转义最多占四字节，另留固定记录头空间。 */
    char record[512 * 4 + 96];
    int result = 0;
    int fd = open("/dev/kmsg", O_RDONLY | O_NONBLOCK);
    if (fd == -1) {
        perror("klog-read: open /dev/kmsg");
        return 1;
    }

    for (;;) {
        ssize_t count = read(fd, record, sizeof(record));
        if (count == -1) {
            if (errno == EPIPE)
                continue;
            if (errno == EAGAIN)
                break;
            perror("klog-read: read /dev/kmsg");
            result = 1;
            break;
        }
        if (count == 0) {
            fprintf(stderr, "klog-read: unexpected end of /dev/kmsg\n");
            result = 1;
            break;
        }

        size_t done = 0;
        while (done < (size_t)count) {
            ssize_t written = write(STDOUT_FILENO, record + done, (size_t)count - done);
            if (written == -1) {
                perror("klog-read: write stdout");
                result = 1;
                goto out;
            }
            if (written == 0) {
                fprintf(stderr, "klog-read: stdout write made no progress\n");
                result = 1;
                goto out;
            }
            done += (size_t)written;
        }
    }

out:
    if (close(fd) == -1) {
        perror("klog-read: close /dev/kmsg");
        result = 1;
    }
    return result;
}
