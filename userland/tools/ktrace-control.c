#include <blossom-trace.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
#include <unistd.h>

/// 通过真实设备的无指针 ioctl 开始或停止固定捕获，所有失败保留非零退出状态。
int main(int argc, char **argv)
{
    int request;
    if (argc == 2 && strcmp(argv[1], "start") == 0)
        request = BLOSSOM_TRACE_START;
    else if (argc == 2 && strcmp(argv[1], "stop") == 0)
        request = BLOSSOM_TRACE_STOP;
    else {
        fprintf(stderr, "usage: ktrace-control start|stop\n");
        return 1;
    }

    int fd = open("/dev/ktrace", O_RDONLY);
    if (fd == -1) {
        perror("ktrace-control: open /dev/ktrace");
        return 1;
    }
    int result = 0;
    if (ioctl(fd, request, 0) == -1) {
        perror("ktrace-control: ioctl /dev/ktrace");
        result = 1;
    }
    if (close(fd) == -1) {
        perror("ktrace-control: close /dev/ktrace");
        result = 1;
    }
    return result;
}
