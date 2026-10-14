#define _GNU_SOURCE

#include <errno.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

/// 在真实子进程内依次替换根和工作目录，原样执行命令参数；目录边界由调用者校验。
int main(int argc, char **argv)
{
    if (argc < 4) {
        fputs("Usage: contest-exec ROOT CWD COMMAND [ARG...]\n", stderr);
        return 125;
    }
    if (chroot(argv[1]) == -1) {
        fprintf(stderr, "contest-exec: cannot change root to '%s': %s\n",
                argv[1], strerror(errno));
        return 125;
    }
    if (chdir(argv[2]) == -1) {
        fprintf(stderr, "contest-exec: cannot change directory to '%s': %s\n",
                argv[2], strerror(errno));
        return 125;
    }
    execvp(argv[3], argv + 3);
    int error = errno;
    fprintf(stderr, "contest-exec: cannot execute '%s': %s\n",
            argv[3], strerror(error));
    return error == ENOENT ? 127 : 126;
}
