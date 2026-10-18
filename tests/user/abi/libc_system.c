#include "libc_common.h"
#include "fp.h"

enum {
    ORPHAN_LIVE_STATUS = 41,
    ORPHAN_ZOMBIE_STATUS = 42,
    ORPHAN_PARENT_STATUS = 43,
};

struct orphan_pids {
    pid_t live;
    pid_t zombie;
};

/// 标准流必须指向同一真实 console，保非 CLOEXEC 的 RDWR 属性及实际 termios。
static int check_console(void)
{
    struct stat first;
    if (fstat(STDIN_FILENO, &first) != 0 || !S_ISCHR(first.st_mode) ||
        device_major((uint64_t)first.st_rdev) != 5 ||
        device_minor((uint64_t)first.st_rdev) != 1)
        return fail("standard console device");
    for (int fd = 0; fd < 3; ++fd) {
        struct stat current;
        struct termios attributes;
        if (fstat(fd, &current) != 0 || !S_ISCHR(current.st_mode) ||
            current.st_dev != first.st_dev || current.st_ino != first.st_ino ||
            current.st_rdev != first.st_rdev || fcntl(fd, F_GETFD) != 0 ||
            (fcntl(fd, F_GETFL) & O_ACCMODE) != O_RDWR ||
            tcgetattr(fd, &attributes) != 0)
            return fail("standard console attributes");
    }
    if (printf("libc-smoke: console fds=0,1,2 RDWR character=5:1\n") < 0 ||
        fflush(stdout) != 0)
        return fail("root role stdio");
    return EXIT_SUCCESS;
}

/// 权限优先于 magic 与命令检查，每项都消费正式 syscall 的返回和 errno。
static int check_reboot_errors(int system_root)
{
    unsigned uid = (unsigned)getuid();
    unsigned gid = (unsigned)getgid();
    if ((uid != 0 && uid != 1000) || gid != (uid == 0 ? 0U : 2000U) ||
        (unsigned)geteuid() != uid || (unsigned)getegid() != gid ||
        getauxval(AT_UID) != uid || getauxval(AT_EUID) != uid ||
        getauxval(AT_GID) != gid || getauxval(AT_EGID) != gid ||
        (system_root && uid != 0))
        return fail("root role credentials");
    const uint32_t invalid[][3] = {
        { 0, REBOOT_MAGIC2, 0 },
        { 0xfee1deadU, 0, 0 },
        { 0xfee1deadU, REBOOT_MAGIC2, 0x12345678U },
        { 0xfee1deadU, REBOOT_MAGIC2, 0x89abcdefU },
    };
    for (size_t i = 0; i < sizeof(invalid) / sizeof(invalid[0]); ++i) {
        errno = 0;
        if (syscall(REBOOT_NUMBER, invalid[i][0], invalid[i][1], invalid[i][2], 0) != -1 ||
            errno != (uid == 0 ? EINVAL : EPERM))
            return fail("reboot invalid argument errno");
    }
    if (!system_root) {
        errno = 0;
        if (syscall(REBOOT_NUMBER, 0xfee1deadU, REBOOT_MAGIC2, REBOOT_POWER_OFF, 0) != -1 ||
            errno != (uid == 0 ? EINVAL : EPERM))
            return fail("fixture reboot role errno");
    }
    const uint32_t second_magics[] = { REBOOT_MAGIC2, 85072278U, 369367448U, 537993216U };
    for (size_t i = 0; i < sizeof(second_magics) / sizeof(second_magics[0]); ++i) {
        errno = 0;
        long result = syscall(REBOOT_NUMBER, 0xfee1deadU, second_magics[i], 0, 0);
        if ((uid == 0 && result != 0) ||
            (uid != 0 && (result != -1 || errno != EPERM)))
            return fail("CAD_OFF permission or magic");
    }
    return EXIT_SUCCESS;
}

/// 普通子组的默认 TERM 必须终止真实执行，系统根的角色不会被 fork 子继承。
static int check_default_signal(void)
{
    pid_t child = fork();
    if (child < 0)
        return fail("default signal fork");
    if (child == 0) {
        if (kill(getpid(), SIGTERM) != 0)
            _exit(fail("default signal kill"));
        _exit(fail("default signal continued"));
    }
    return wait_status(child, SIGTERM);
}

/// 中间父先保一个活子，再用 WNOWAIT 确认另一子已经退出；最后成员退出才交出亲缘。
static void orphan_parent(int report[2], int release[2])
{
    if (close(report[0]) != 0 || close(release[1]) != 0)
        _exit(fail("orphan parent pipe close"));
    struct orphan_pids children;
    children.live = fork();
    if (children.live < 0)
        _exit(fail("live orphan fork"));
    if (children.live == 0) {
        if (close(report[1]) != 0)
            _exit(fail("live orphan report close"));
        unsigned char go;
        if (transfer_pipe(release[0], &go, 1, 0) != EXIT_SUCCESS || go != 'G' ||
            getppid() != 1 || tls_image != 0x2468ace0U || tls_zero != 0x10203040U ||
            check_fp() != EXIT_SUCCESS || close(release[0]) != 0)
            _exit(fail("live orphan adopted state"));
        _exit(ORPHAN_LIVE_STATUS);
    }
    if (close(release[0]) != 0)
        _exit(fail("orphan parent release close"));
    children.zombie = fork();
    if (children.zombie < 0)
        _exit(fail("zombie orphan fork"));
    if (children.zombie == 0)
        _exit(ORPHAN_ZOMBIE_STATUS);
    siginfo_t information;
    int result;
    do {
        result = waitid(P_PID, (id_t)children.zombie, &information, WEXITED | WNOWAIT);
    } while (result < 0 && errno == EINTR);
    if (result != 0 || information.si_pid != children.zombie ||
        information.si_code != CLD_EXITED || information.si_status != ORPHAN_ZOMBIE_STATUS)
        _exit(fail("zombie WNOWAIT observation"));
    if (transfer_pipe(report[1], &children, sizeof(children), 1) != EXIT_SUCCESS ||
        close(report[1]) != 0)
        _exit(fail("orphan parent report"));
    _exit(ORPHAN_PARENT_STATUS);
}

/// PID1 等中间父完整退出后消费已转亲 zombie，再放行仍活的孤儿并消费它的真实状态。
static int check_adopted_orphans(void)
{
    int report[2];
    int release[2];
    if (pipe(report) != 0)
        return fail("orphan report pipe");
    if (pipe(release) != 0) {
        close(report[0]);
        close(report[1]);
        return fail("orphan release pipe");
    }
    pid_t parent = fork();
    if (parent < 0) {
        close(report[0]);
        close(report[1]);
        close(release[0]);
        close(release[1]);
        return fail("orphan parent fork");
    }
    if (parent == 0)
        orphan_parent(report, release);
    struct orphan_pids children;
    if (close(report[1]) != 0 || close(release[0]) != 0 ||
        transfer_pipe(report[0], &children, sizeof(children), 0) != EXIT_SUCCESS ||
        close(report[0]) != 0 || children.live <= 1 || children.zombie <= 1 ||
        children.live == children.zombie || children.live == parent || children.zombie == parent ||
        wait_status(parent, ORPHAN_PARENT_STATUS << 8) != EXIT_SUCCESS ||
        wait_status(children.zombie, ORPHAN_ZOMBIE_STATUS << 8) != EXIT_SUCCESS)
        return fail("zombie orphan adoption");
    unsigned char go = 'G';
    if (transfer_pipe(release[1], &go, 1, 1) != EXIT_SUCCESS || close(release[1]) != 0 ||
        wait_status(children.live, ORPHAN_LIVE_STATUS << 8) != EXIT_SUCCESS)
        return fail("live orphan adoption");
    int status;
    errno = 0;
    if (waitpid(-1, &status, WNOHANG) != -1 || errno != ECHILD)
        return fail("PID1 final child ownership");
    if (printf("libc-smoke: PID1 adopted live=%ld status=%d zombie=%ld status=%d parent=%ld status=%d\n",
               (long)children.live, ORPHAN_LIVE_STATUS << 8,
               (long)children.zombie, ORPHAN_ZOMBIE_STATUS << 8,
               (long)parent, ORPHAN_PARENT_STATUS << 8) < 0 || fflush(stdout) != 0)
        return fail("PID1 adoption stdio");
    return EXIT_SUCCESS;
}

/// 数值身份与角色分别核验；系统根在 KILL、STOP、默认 TERM 后继续普通用户指令。
int root_role_body(int system_root)
{
    if (getpid() != 1 || getppid() != 0)
        return fail("root pid and ppid");
    if (check_console() != EXIT_SUCCESS || check_reboot_errors(system_root) != EXIT_SUCCESS ||
        check_default_signal() != EXIT_SUCCESS)
        return EXIT_FAILURE;
    if (system_root) {
        volatile unsigned continued = 0;
        if (kill(getpid(), SIGKILL) != 0)
            return fail("PID1 KILL");
        continued += 1;
        if (kill(getpid(), SIGSTOP) != 0)
            return fail("PID1 STOP");
        continued += 2;
        if (kill(getpid(), SIGTERM) != 0)
            return fail("PID1 default TERM");
        continued += 4;
        if (continued != 7 || tls_image != 0x2468ace0U || tls_zero != 0x10203040U)
            return fail("PID1 continued user state");
        return check_adopted_orphans();
    }
    return EXIT_SUCCESS;
}
