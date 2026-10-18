#include "io_private.h"

/// 在保留空表和只读根前提下，验证 search 拒绝不发表 fd 或推进已有 OFD。
void io_access(void)
{
    long fd = probe_sys4(SYS_openat, AT_FDCWD, IO_DATA, O_RDONLY, 0);
    E(201, fd, 0);
    E(202, probe_sys3(SYS_read, fd, io_buffer, 17), 17);
    io_pattern(203, io_buffer, 17, 0);
    long directory = probe_sys4(SYS_openat, AT_FDCWD, "/test/io-no-search", O_DIRECTORY, 0);
    E(204, directory, 1);
    const char *absolute[] = {
        "/test/io-denied", "/test/io-denied/item", "/test/io-denied/./item",
        "/test/io-denied/../io-data.bin", "/test/io-no-search/item",
        "/test/io-no-search/./item", "/test/io-no-search/../io-data.bin"
    };
    for (unsigned i = 0; i < 7; ++i)
        E(205 + i, probe_sys4(SYS_openat, AT_FDCWD, absolute[i], O_RDONLY, 0), -EACCES);
    const char *relative[] = {"item", "./item", "../io-data.bin", ".", ".."};
    for (unsigned i = 0; i < 5; ++i)
        E(212 + i, probe_sys4(SYS_openat, directory, relative[i], O_RDONLY, 0), -EACCES);
    E(217, probe_sys1(SYS_chdir, "/test/io-no-search"), -EACCES);

    volatile unsigned char *bytes = io_buffer;
    for (unsigned i = 0; i < 8; ++i)
        bytes[i] = 0x5a;
    E(218, probe_sys3(SYS_read, directory, io_buffer, 8), -EISDIR);
    for (unsigned i = 0; i < 8; ++i)
        E(219, bytes[i], 0x5a);
    E(220, probe_sys3(SYS_read, 2, io_buffer, 8), -EBADF);
    for (unsigned i = 0; i < 8; ++i)
        E(221, bytes[i], 0x5a);
    E(222, probe_sys3(SYS_lseek, fd, 0, 1), 17);
    E(223, probe_sys3(SYS_write, fd, io_buffer, 1), -EBADF);
    E(224, probe_sys4(SYS_openat, AT_FDCWD, IO_DATA, O_WRONLY, 0), -EROFS);
    long independent = probe_sys4(SYS_openat, AT_FDCWD, IO_DATA, O_RDONLY, 0);
    E(225, independent, 2);
    E(226, probe_sys3(SYS_read, independent, io_buffer, 1), 1);
    io_pattern(227, io_buffer, 1, 0);
    E(228, probe_sys3(SYS_read, fd, io_buffer, 1), 1);
    io_pattern(229, io_buffer, 1, 17);
    E(230, probe_sys1(SYS_close, independent), 0);
    E(231, probe_sys1(SYS_close, directory), 0);
    E(232, probe_sys1(SYS_close, fd), 0);
    fd = probe_sys4(SYS_openat, AT_FDCWD, IO_DATA, O_RDONLY, 0);
    E(233, fd, 0);
    E(234, probe_sys1(SYS_close, fd), 0);
}
