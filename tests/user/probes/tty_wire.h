#ifndef PROBE_TTY_WIRE_H
#define PROBE_TTY_WIRE_H

#include "probe.h"

/* 两目标共同使用通用 Linux termios 的36字节布局，不含 libc 扩展字段。 */
typedef struct {
    uint32_t iflag;
    uint32_t oflag;
    uint32_t cflag;
    uint32_t lflag;
    uint8_t line;
    uint8_t cc[19];
} TtyTermios;

_Static_assert(sizeof(TtyTermios) == 36, "termios wire size");
_Static_assert(offsetof(TtyTermios, cc[5]) == 22, "VTIME wire offset");
_Static_assert(offsetof(TtyTermios, cc[6]) == 23, "VMIN wire offset");

#define TTY_CONFIG_INITIALIZER \
    {0, 0, 0xbf, 0, 0, {3, 28, 127, 21, 4, 0, 1, 0, 17, 19, 26, 0, 18, 15, 23, 22, 0, 0, 0}}

typedef struct {
    long seconds;
    long nanoseconds;
} TtyTime;

typedef struct {
    int fd;
    uint16_t events;
    uint16_t revents;
} TtyPoll;

_Static_assert(sizeof(TtyTime) == 16, "timespec wire size");
_Static_assert(sizeof(TtyPoll) == 8, "pollfd wire size");

#endif
