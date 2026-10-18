#include "write.h"

unsigned char write_buffer[8192] __attribute__((aligned(16)));

/// 原普通程序失败通过 exit_group 传原检查号，不改变 fixture 的退出观察协议。
_Noreturn void probe_failure(long check)
{
    probe_exit_group(check);
}

/// 只解析原 argv 首字和 N 的次字，业务分别归各自职责文件。
_Noreturn void probe_main(const unsigned long *initial_stack)
{
    CHECK(1, 2 <= initial_stack[0]);
    const unsigned char *mode = (const unsigned char *)initial_stack[2];
    switch (mode[0]) {
    case 'w': write_buffered(); break;
    case 'a': write_append(); break;
    case 'e': case 'n': case 'o': write_exec(mode[0]); break;
    case 's': write_shared(); break;
    case 'r': case 'm': case 'c': write_remote(mode[0]); break;
    case 'f': case 'p': case 'q': case 'u': write_file_futex(mode[0]); break;
    case 'y': write_sync_abi(); break;
    case 'b': write_sync_wait(); break;
    case 'd': case 'z': write_sync_write(mode[0]); break;
    case 't': write_truncate(); break;
    case 'v': write_truncate_ro(); break;
    case 'h': write_truncate_dac(); break;
    case 'k': write_truncate_cow(); break;
    case 'j': write_truncate_wait(); break;
    case 'R': write_reclaim(); break;
    case 'N':
        switch (mode[1]) {
        case '3': write_namespace_create(); break;
        case '4': write_namespace_unlink(); break;
        case '5': write_namespace_rename(); break;
        case '6': write_namespace_directories(); break;
        default: probe_failure(1);
        }
        break;
    default: probe_failure(1);
    }
    probe_exit_group(0);
}
