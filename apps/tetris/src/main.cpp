#include <cstdio>
#include <unistd.h>
#include <termios.h>
#include <fcntl.h>
#include "game.h"
#include <cstdlib>
#include <sys/wait.h>

static struct termios orig_termios;

void disableRawMode() {
    tcsetattr(STDIN_FILENO, TCSAFLUSH, &orig_termios);
}

void enableRawMode() {
    struct termios raw;
    tcgetattr(STDIN_FILENO, &orig_termios);
    atexit(disableRawMode);

    raw = orig_termios;
    raw.c_lflag &= ~(ECHO | ICANON); // 关闭回显，关闭行缓冲
    raw.c_cc[VMIN] = 0;
    raw.c_cc[VTIME] = 1; // 0.1秒超时
    tcsetattr(STDIN_FILENO, TCSAFLUSH, &raw);

    // 设置非阻塞读取
    int flags = fcntl(STDIN_FILENO, F_GETFL, 0);
    fcntl(STDIN_FILENO, F_SETFL, flags | O_NONBLOCK);
}

void clear_screen() {
    pid_t pid = fork();  // 创建子进程
    if (pid == -1) {
        // 处理fork失败（可选添加错误提示）
        return;
    }
    
    if (pid == 0) {
        // 子进程：执行/musl/busybox clear
        char *argv[] = {"/musl/busybox", "clear", NULL};  // 使用完整路径
        execvp(argv[0], argv);  // 第一个参数是可执行文件路径
        
        // 如果execvp执行失败才会走到这里
        _exit(1);  // 子进程退出
    } else {
        // 父进程：等待子进程完成清屏
        waitpid(pid, NULL, 0);
    }
}

int main() {
    enableRawMode();
    Game game;
    clear_screen();
    printf("Welcome to Tetris!\n");
    printf("Pess any key(except q) to start...\n");
    printf("usage: w(rotate), a(left), d(right), s(down), q(quit)\n");
    while (true) {
        // 根据 level 控制游戏速度，这里简化为固定时间循环
        for (int i = 0; game.level < 10 && i < 10 - game.level; ++i) {
            char input = 0;
            ssize_t nread = read(STDIN_FILENO, &input, 1);
            if (nread == 1) {
                if (input == 'q') {
                    clear_screen();
                    printf("Exiting Tetris. Goodbye!\n");
                    return 0;  // q退出
                }    
                game.trasformTetromino(input);
            }
            game.render();
            usleep(50000); // 50ms 小延迟
        }
        game.updateState();
    }

    return 0;
}
