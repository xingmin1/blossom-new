#include "libc_common.h"

/// 控制信号只结束错误时间域的等待，不把中断当成期限已经到达。
static void runtime_clock_alarm(int signal_number)
{
    (void)signal_number;
}

/// 以 UTC 未来期限等待真实 futex；单次 alarm 只给错误投影提供有界失败出口。
static int runtime_realtime_futex(void)
{
    struct sigaction action = { .sa_handler = runtime_clock_alarm };
    struct sigaction previous;
    struct itimerval alarm = { .it_value = { 0, 500000 } };
    struct itimerval previous_timer;
    struct itimerval stopped = { 0 };
    sigemptyset(&action.sa_mask);
    if (sigaction(SIGALRM, &action, &previous) != 0)
        return fail("UTC futex alarm action");
    if (setitimer(ITIMER_REAL, &alarm, &previous_timer) != 0) {
        sigaction(SIGALRM, &previous, NULL);
        return fail("UTC futex alarm");
    }
    int result = EXIT_FAILURE;
    struct timespec deadline;
    uint64_t start;
    if (clock_gettime(CLOCK_REALTIME, &deadline) != 0 ||
        runtime_nanoseconds(CLOCK_MONOTONIC, &start) != EXIT_SUCCESS) {
        result = fail("UTC futex deadline sample");
    } else {
        deadline.tv_nsec += 80000000;
        if (1000000000L <= deadline.tv_nsec) {
            ++deadline.tv_sec;
            deadline.tv_nsec -= 1000000000L;
        }
        uint32_t word = 0;
        errno = 0;
        long waited = syscall(SYS_futex, &word, 9U | 0x80U | 0x100U, 0,
                              &deadline, NULL, UINT32_MAX);
        int error = errno;
        if (waited == -1 && error == ETIMEDOUT)
            result = runtime_elapsed(start, 50000000ULL, "UTC futex deadline");
        else {
            errno = error;
            result = fail("UTC futex must time out without alarm interruption");
        }
    }
    int timer_stopped = setitimer(ITIMER_REAL, &stopped, NULL);
    int action_restored = sigaction(SIGALRM, &previous, NULL);
    int timer_restored = setitimer(ITIMER_REAL, &previous_timer, NULL);
    if (timer_stopped != 0 || action_restored != 0 || timer_restored != 0)
        result = fail("UTC futex alarm restore");
    return result;
}

/// 两种绝对 sleep 均沿实际时钟建立未来期限，过去期限只允许立即完成。
static int runtime_absolute_sleep(clockid_t clock)
{
    struct timespec deadline, after;
    uint64_t start;
    if (runtime_nanoseconds(CLOCK_MONOTONIC, &start) != EXIT_SUCCESS ||
        clock_gettime(clock, &deadline) != 0)
        return fail("absolute sleep sample");
    deadline.tv_nsec += 80000000;
    if (1000000000L <= deadline.tv_nsec) {
        ++deadline.tv_sec;
        deadline.tv_nsec -= 1000000000L;
    }
    int error = clock_nanosleep(clock, TIMER_ABSTIME, &deadline, NULL);
    if (error != 0) {
        errno = error;
        return fail("future absolute clock_nanosleep");
    }
    if (clock_gettime(clock, &after) != 0 || !file_time_before(deadline, after) ||
        runtime_elapsed(start, 50000000ULL, "future absolute sleep") != EXIT_SUCCESS)
        return fail("absolute sleep deadline");
    struct timespec past = after;
    if (0 < past.tv_sec)
        --past.tv_sec;
    else
        past.tv_nsec = 0;
    if (runtime_nanoseconds(CLOCK_MONOTONIC, &start) != EXIT_SUCCESS)
        return EXIT_FAILURE;
    error = clock_nanosleep(clock, TIMER_ABSTIME, &past, NULL);
    uint64_t end;
    if (error != 0) {
        errno = error;
        return fail("past absolute clock_nanosleep");
    }
    if (runtime_nanoseconds(CLOCK_MONOTONIC, &end) != EXIT_SUCCESS ||
        end < start || 5000000000ULL < end - start)
        return fail("past absolute sleep immediate");
    return EXIT_SUCCESS;
}

/// 普通 sleep 推进墙钟而不计入 CPU 执行量，宽容阈值仍能拒绝把 CPU 时钟接到墙钟。
static int runtime_cpu_sleep(void)
{
    uint64_t wall_before, process_before, thread_before;
    uint64_t wall_after, process_after, thread_after;
    if (runtime_nanoseconds(CLOCK_MONOTONIC, &wall_before) != EXIT_SUCCESS ||
        runtime_nanoseconds(CLOCK_PROCESS_CPUTIME_ID, &process_before) != EXIT_SUCCESS ||
        runtime_nanoseconds(CLOCK_THREAD_CPUTIME_ID, &thread_before) != EXIT_SUCCESS)
        return EXIT_FAILURE;
    struct timespec delay = { 0, 300000000 };
    if (nanosleep(&delay, NULL) != 0 ||
        runtime_nanoseconds(CLOCK_THREAD_CPUTIME_ID, &thread_after) != EXIT_SUCCESS ||
        runtime_nanoseconds(CLOCK_PROCESS_CPUTIME_ID, &process_after) != EXIT_SUCCESS ||
        runtime_nanoseconds(CLOCK_MONOTONIC, &wall_after) != EXIT_SUCCESS)
        return fail("CPU clock ordinary sleep");
    if (wall_after < wall_before || process_after < process_before || thread_after < thread_before)
        return fail("CPU clock monotonic accounting");
    uint64_t wall = wall_after - wall_before;
    uint64_t process = process_after - process_before;
    uint64_t thread = thread_after - thread_before;
    if (wall < 250000000ULL || 10000000000ULL < wall ||
        wall / 2 + 50000000ULL < process || wall / 2 + 50000000ULL < thread) {
        fprintf(stderr, "libc-smoke: sleep wall_ns=%llu process_ns=%llu thread_ns=%llu\n",
                (unsigned long long)wall, (unsigned long long)process, (unsigned long long)thread);
        return fail("CPU clock excludes sleeping wall time");
    }
    return EXIT_SUCCESS;
}

/// 新建文件三时间必须位于同次 UTC 采样区间；显式历史时间保持原有秒与纳秒。
static int runtime_clock_file(void)
{
    char path[128];
    int count = snprintf(path, sizeof(path), "/test/runtime-times/clock-%ld", (long)getpid());
    if (count < 0 || sizeof(path) <= (size_t)count)
        return fail("clock file name");
    struct timespec before, after;
    if (clock_gettime(CLOCK_REALTIME, &before) != 0)
        return fail("clock file UTC before");
    int fd = open(path, O_RDWR | O_CREAT | O_EXCL, 0600);
    if (fd < 0)
        return fail("clock file create");
    int result = EXIT_FAILURE;
    struct stat created, changed;
    if (fstat(fd, &created) != 0 || clock_gettime(CLOCK_REALTIME, &after) != 0 ||
        !file_time_before(before, created.st_atim) || !file_time_before(created.st_atim, after) ||
        !file_time_before(before, created.st_mtim) || !file_time_before(created.st_mtim, after) ||
        !file_time_before(before, created.st_ctim) || !file_time_before(created.st_ctim, after)) {
        result = fail("new file UTC atime mtime ctime");
        goto out;
    }
    const struct timespec history[2] = { { 123456789, 123456789 }, { -123, 456789012 } };
    if (clock_gettime(CLOCK_REALTIME, &before) != 0 || futimens(fd, history) != 0 ||
        clock_gettime(CLOCK_REALTIME, &after) != 0 || fstat(fd, &changed) != 0 ||
        !same_file_time(changed.st_atim, history[0]) ||
        !same_file_time(changed.st_mtim, history[1]) ||
        !file_time_before(before, changed.st_ctim) || !file_time_before(changed.st_ctim, after) ||
        !same_file_attributes(&created, &changed)) {
        result = fail("clock file explicit history preserved");
        goto out;
    }
    result = EXIT_SUCCESS;
out:
    if (close(fd) != 0)
        result = fail("clock file close");
    if (unlink(path) != 0)
        result = fail("clock file unlink");
    return result;
}

/// 六个标准 clock 与 libc 日历入口共用实际 UTC 样本，不校准 RTC 的亚秒相位。
int runtime_clock(void)
{
    const clockid_t clocks[] = {
        CLOCK_REALTIME, CLOCK_MONOTONIC, CLOCK_PROCESS_CPUTIME_ID,
        CLOCK_THREAD_CPUTIME_ID, CLOCK_REALTIME_COARSE, CLOCK_MONOTONIC_COARSE,
    };
    struct timespec resolutions[6];
    for (size_t index = 0; index < sizeof(clocks) / sizeof(clocks[0]); ++index) {
        uint64_t sample;
        if (runtime_nanoseconds(clocks[index], &sample) != EXIT_SUCCESS ||
            clock_getres(clocks[index], &resolutions[index]) != 0 ||
            resolutions[index].tv_sec != 0 || resolutions[index].tv_nsec <= 0 ||
            10000000L < resolutions[index].tv_nsec)
            return fail("runtime clock IDs and resolution");
    }
    uint64_t real_before, real_after, coarse_real, mono_before, mono_after, coarse_mono;
    struct timeval calendar;
    time_t stored = -1;
    if (runtime_nanoseconds(CLOCK_REALTIME, &real_before) != EXIT_SUCCESS ||
        runtime_nanoseconds(CLOCK_REALTIME_COARSE, &coarse_real) != EXIT_SUCCESS ||
        gettimeofday(&calendar, NULL) != 0)
        return fail("UTC clock and gettimeofday");
    time_t seconds = time(&stored);
    if (seconds == (time_t)-1 || stored != seconds ||
        runtime_nanoseconds(CLOCK_REALTIME, &real_after) != EXIT_SUCCESS ||
        runtime_nanoseconds(CLOCK_MONOTONIC, &mono_before) != EXIT_SUCCESS ||
        runtime_nanoseconds(CLOCK_MONOTONIC_COARSE, &coarse_mono) != EXIT_SUCCESS ||
        runtime_nanoseconds(CLOCK_MONOTONIC, &mono_after) != EXIT_SUCCESS)
        return fail("UTC time and monotonic samples");
    if (real_before / 1000000000ULL < 1577836800ULL || real_after < real_before ||
        mono_after < mono_before || real_before / 1000000000ULL <= mono_after / 1000000000ULL ||
        real_before / 1000000000ULL - mono_after / 1000000000ULL < 1577836800ULL ||
        coarse_real < real_before - (uint64_t)resolutions[4].tv_nsec ||
        real_after < coarse_real || mono_after < coarse_mono ||
        (coarse_mono < mono_before &&
         (uint64_t)resolutions[5].tv_nsec < mono_before - coarse_mono) ||
        calendar.tv_sec < 0 || calendar.tv_usec < 0 || 1000000 <= calendar.tv_usec)
        return fail("UTC and monotonic clock domains");
    uint64_t calendar_ns = (uint64_t)calendar.tv_sec * 1000000000ULL +
                           (uint64_t)calendar.tv_usec * 1000ULL;
    if (calendar_ns + 999ULL < real_before || real_after < calendar_ns ||
        seconds < (time_t)(real_before / 1000000000ULL) ||
        (time_t)(real_after / 1000000000ULL) < seconds)
        return fail("gettimeofday and time UTC interval");
    struct tm utc;
    if (gmtime_r(&seconds, &utc) == NULL || utc.tm_year < 120)
        return fail("UTC calendar year");
    if (runtime_clock_file() != EXIT_SUCCESS ||
        runtime_realtime_futex() != EXIT_SUCCESS ||
        runtime_absolute_sleep(CLOCK_REALTIME) != EXIT_SUCCESS ||
        runtime_absolute_sleep(CLOCK_MONOTONIC) != EXIT_SUCCESS ||
        runtime_cpu_sleep() != EXIT_SUCCESS)
        return EXIT_FAILURE;
    if (printf("libc-smoke: real UTC clocks and deadlines passed epoch=%lld date=%04d-%02d-%02d\n",
               (long long)seconds, utc.tm_year + 1900, utc.tm_mon + 1, utc.tm_mday) < 0 ||
        fflush(stdout) != 0)
        return fail("runtime clock output");
    return EXIT_SUCCESS;
}
