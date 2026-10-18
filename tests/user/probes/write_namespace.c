#include "write.h"

static unsigned char ns_stack_a[16384] __attribute__((aligned(16)));
static unsigned char ns_stack_b[16384] __attribute__((aligned(16)));
static int ns_fd_table[1024];

/// 创建普通文件后同步全部 payload 并关闭，检查号来自原各调用点。
static void ns_create_data(const char *path, const char *data, long length, long mode,
                           long opened, long written, long synced, long closed)
{
    long fd = write_open(path, 0xc2, mode, opened);
    EQ(written, probe_sys3(SYS_write, fd, data, length), length);
    EQ(synced, probe_sys1(SYS_fsync, fd), 0);
    EQ(closed, probe_sys1(SYS_close, fd), 0);
}

/// 名字修改使用真实父目录 fd 同步元数据，不替代 inode 或 WB 的父观察。
static void ns_sync_directory(const char *path, long opened, long synced, long closed)
{
    long fd = write_open(path, 0x10000, 0, opened);
    EQ(synced, probe_sys1(SYS_fsync, fd), 0);
    EQ(closed, probe_sys1(SYS_close, fd), 0);
}

/// 读取独立打开对象的完整 payload，cursor 由普通 read 前进。
static void ns_read_data(const char *path, const char *data, long length,
                         long opened, long read_check, long equal_check, long closed)
{
    long fd = write_open(path, 0, 0, opened);
    EQ(read_check, probe_sys3(SYS_read, fd, write_buffer, length), length);
    write_equal(write_buffer, (const unsigned char *)data, length, equal_check);
    EQ(closed, probe_sys1(SYS_close, fd), 0);
}

/// EXCL 子在自己的新栈上等待同一 release gate，并把胜负写回原四字节槽。
static void ns_exclusive_child(void *argument, unsigned long entry_sp, unsigned long entry_tp)
{
    (void)entry_sp; (void)entry_tp;
    unsigned offset = (unsigned long)argument;
    while (probe_load_acquire(write_word(8)) == 0) {}
    long fd = probe_sys4(SYS_openat, -100, NS("ns-race"), 0xc2, 0x180);
    CHECK(offset == 12 ? 305 : 306, 0 <= fd || fd == -17);
    probe_store_release(write_word(offset), 0 <= fd ? 1 : 0);
    if (0 <= fd) EQ(409, probe_sys1(SYS_close, fd), 0);
    probe_exit(0);
}

/// CLONE_FS 子修改共同 umask，仍在新栈执行真实创建和模式读取。
static void ns_shared_umask_child(void *argument, unsigned long entry_sp, unsigned long entry_tp)
{
    (void)argument; (void)entry_sp; (void)entry_tp;
    EQ(410, probe_sys1(SYS_umask, 7), 0x17);
    long fd = write_open(NS("ns-shared"), 0xc2, 0x1b6, 411);
    EQ(413, write_stat(fd, 412).mode & 0xfff, 0x1b0);
    EQ(414, probe_sys3(SYS_write, fd, "SHRD", 4), 4);
    EQ(415, probe_sys1(SYS_fsync, fd), 0);
    EQ(416, probe_sys1(SYS_close, fd), 0);
    probe_exit(0);
}

/// 不共享 FS 的子 umask 不改变父，同时保原文件模式和同步尾部。
static void ns_copy_umask_child(void *argument, unsigned long entry_sp, unsigned long entry_tp)
{
    (void)argument; (void)entry_sp; (void)entry_tp;
    EQ(417, probe_sys1(SYS_umask, 0x3f), 0x17);
    long fd = write_open(NS("ns-copy"), 0xc2, 0x1b6, 418);
    EQ(420, write_stat(fd, 419).mode & 0xfff, 0x180);
    EQ(421, probe_sys3(SYS_write, fd, "COPY", 4), 4);
    EQ(422, probe_sys1(SYS_fsync, fd), 0);
    EQ(423, probe_sys1(SYS_close, fd), 0);
    probe_exit(0);
}

/// N3 保真实 EXCL 竞争、FS umask、SGID、符号链接、非 UTF8 名与表满预留顺序。
void write_namespace_create(void)
{
    EQ(300, probe_sys1(SYS_umask, 0x1017), 0x12);
    long fd = write_open(NS("ns-mode"), 0xc2, 0xfff, 301);
    EQ(303, probe_sys1(SYS_fsync, fd), 0);
    EQ(304, probe_sys1(SYS_close, fd), 0);
    ns_create_data(NS("ns-data"), "MODE", 4, 0x1b6, 645, 646, 647, 648);
    long first = probe_clone(0x311, ns_stack_a + sizeof(ns_stack_a), 0, 0, 0, ns_exclusive_child, (void *)12UL);
    CHECK(305, 0 <= first);
    long second = probe_clone(0x311, ns_stack_b + sizeof(ns_stack_b), 0, 0, 0, ns_exclusive_child, (void *)16UL);
    CHECK(306, 0 <= second);
    probe_store_release(write_word(8), 1);
    write_wait(first, 307, 308);
    write_wait(second, 309, 310);
    EQ(311, probe_load_acquire(write_word(12)) + probe_load_acquire(write_word(16)), 1);
    fd = write_open(NS("ns-race"), 0, 0, 638);
    unsigned long ino = write_stat(fd, 639).ino;
    long other = write_open(NS("ns-race"), 0, 0, 640);
    EQ(642, write_stat(other, 641).ino, ino);
    EQ(643, probe_sys1(SYS_close, other), 0);
    EQ(644, probe_sys1(SYS_close, fd), 0);
    fd = write_open(NS("ns-race"), 2, 0, 312);
    EQ(313, probe_sys3(SYS_write, fd, "RACE", 4), 4);
    EQ(314, probe_sys1(SYS_fsync, fd), 0);
    EQ(315, probe_sys1(SYS_close, fd), 0);
    first = probe_clone(0x311, ns_stack_a + sizeof(ns_stack_a), 0, 0, 0, ns_shared_umask_child, 0);
    CHECK(316, 0 <= first);
    write_wait(first, 317, 318);
    EQ(319, probe_sys1(SYS_umask, 0), 7);
    EQ(320, probe_sys1(SYS_umask, 0x17), 0);
    first = probe_clone(0x111, ns_stack_a + sizeof(ns_stack_a), 0, 0, 0, ns_copy_umask_child, 0);
    CHECK(321, 0 <= first);
    write_wait(first, 322, 323);
    EQ(324, probe_sys1(SYS_umask, 0), 0x17);
    EQ(325, probe_sys1(SYS_umask, 0x17), 0);
    fd = write_open(NS("ns-ro"), 0x40, 0x1b6, 326);
    EQ(328, write_stat(fd, 327).mode & 0xfff, 0x1a0);
    EQ(329, probe_sys3(SYS_write, fd, "MODE", 1), -9);
    EQ(330, probe_sys1(SYS_fsync, fd), 0);
    EQ(331, probe_sys1(SYS_close, fd), 0);
    fd = write_open(NS("ns-mode"), 0x40, 0x180, 332);
    EQ(334, write_stat(fd, 333).mode & 0xfff, 0xfe8);
    EQ(335, probe_sys3(SYS_write, fd, "RACE", 1), -9);
    EQ(336, probe_sys1(SYS_close, fd), 0);
    EQ(337, probe_sys4(SYS_openat, -100, NS("ns-mode"), 0xc2, 0x1b6), -17);
    EQ(338, probe_sys3(SYS_mkdirat, -100, NS("ns-dir"), 0x3ff), 0);
    fd = write_open(NS("ns-dir"), 0x10000, 0, 339);
    struct WriteStat stat = write_stat(fd, 340);
    EQ(341, stat.nlink, 2);
    EQ(342, stat.mode & 0xfff, 0x7e8);
    CHECK(342, 48 <= probe_sys3(SYS_getdents64, fd, write_buffer, 128));
    EQ(645, write_buffer[19], '.');
    EQ(646, write_buffer[20], 0);
    unsigned short reclen;
    memcpy(&reclen, write_buffer + 16, sizeof(reclen));
    EQ(647, reclen, 24);
    EQ(648, write_buffer[18], 4);
    EQ(649, write_buffer[reclen + 19], '.');
    EQ(650, write_buffer[reclen + 20], '.');
    EQ(651, write_buffer[reclen + 21], 0);
    EQ(343, probe_sys1(SYS_fsync, fd), 0);
    EQ(344, probe_sys1(SYS_close, fd), 0);
    EQ(345, probe_sys3(SYS_symlinkat, NS("ns-data"), -100, NS("ns-short")), 0);
    static const char long_target[] = "/test/ns-work"
        "/./././././././././."
        "/./././././././././."
        "/./././././././././."
        "/ns-data";
    EQ(346, probe_sys3(SYS_symlinkat, long_target, -100, NS("ns-long")), 0);
    EQ(347, probe_sys3(SYS_symlinkat, NS("ns-missing"), -100, NS("ns-dangling")), 0);
    ns_create_data(NS("ns-raw-\xff"), "RAW!", 4, 0x180, 348, 349, 350, 351);
    EQ(352, probe_sys4(SYS_openat, -100, NS("ns-absent/../ns-lex"), 0x42, 0x180), -2);
    EQ(353, probe_sys4(SYS_openat, -100, NS("ns-mode/"), 0x20000, 0), -20);
    ns_read_data("/../../test/ns-work/ns-data", "MODE", 4, 354, 355, 356, 357);
    EQ(358, probe_sys4(SYS_openat, -100, NS("ns-permission/denied"), 0xc2, 0x180), -13);
    unsigned count = 0;
    for (;;) {
        fd = probe_sys4(SYS_openat, -100, NS("ns-mode"), 0, 0);
        if (fd == -24) break;
        CHECK(358, 0 <= fd);
        CHECK(358, count < sizeof(ns_fd_table) / sizeof(ns_fd_table[0]));
        ns_fd_table[count++] = fd;
    }
    CHECK(358, count != 0);
    EQ(359, probe_sys4(SYS_openat, -100, NS("ns-missing"), 0xc2, 0x180), -24);
    for (unsigned i = 0; i < count; ++i) EQ(360, probe_sys1(SYS_close, ns_fd_table[i]), 0);
    EQ(361, probe_sys4(SYS_openat, -100, NS("ns-missing"), 0, 0), -2);
    ns_sync_directory("/test/ns-work", 362, 363, 364);
    write_phase(1);
    write_gate(1);
    EQ(365, probe_sys4(SYS_openat, -100, NS("ns-dangling"), 0xc2, 0x180), -17);
    EQ(366, probe_sys4(SYS_openat, -100, NS("ns-dangling"), 0x20000, 0), -40);
    fd = write_open(NS("ns-dangling"), 0x42, 0x180, 367);
    EQ(368, probe_sys3(SYS_write, fd, "DANG", 4), 4);
    EQ(369, probe_sys1(SYS_fsync, fd), 0);
    EQ(370, probe_sys1(SYS_close, fd), 0);
    ns_read_data(NS("ns-short"), "MODE", 4, 371, 372, 373, 374);
    ns_read_data(NS("ns-long"), "MODE", 4, 375, 376, 377, 378);
    ns_read_data(NS("ns-dangling"), "DANG", 4, 379, 380, 381, 382);
    fd = write_open(NS("ns-sgid-child"), 0xc2, 0xfff, 383);
    stat = write_stat(fd, 384);
    EQ(385, stat.gid, 1001);
    EQ(386, stat.mode & 0xfff, 0xfe8);
    EQ(387, probe_sys3(SYS_write, fd, "LINK", 4), 4);
    EQ(388, probe_sys1(SYS_fsync, fd), 0);
    EQ(389, probe_sys1(SYS_close, fd), 0);
    ns_sync_directory("/test/ns-work", 390, 391, 392);
    write_phase(2);
    write_gate(2);
    EQ(393, probe_sys1(SYS_umask, 0x12), 0x17);
    EQ(394, probe_sys3(SYS_unlinkat, -100, NS("ns-race"), 0), 0);
    EQ(395, probe_sys3(SYS_unlinkat, -100, NS("ns-mode"), 0), 0);
    EQ(649, probe_sys3(SYS_unlinkat, -100, NS("ns-data"), 0), 0);
    EQ(396, probe_sys3(SYS_unlinkat, -100, NS("ns-ro"), 0), 0);
    EQ(397, probe_sys3(SYS_unlinkat, -100, NS("ns-shared"), 0), 0);
    EQ(398, probe_sys3(SYS_unlinkat, -100, NS("ns-copy"), 0), 0);
    EQ(399, probe_sys3(SYS_unlinkat, -100, NS("ns-raw-\xff"), 0), 0);
    EQ(400, probe_sys3(SYS_unlinkat, -100, NS("ns-missing"), 0), 0);
    EQ(401, probe_sys3(SYS_unlinkat, -100, NS("ns-sgid-child"), 0), 0);
    EQ(402, probe_sys3(SYS_unlinkat, -100, NS("ns-short"), 0), 0);
    EQ(403, probe_sys3(SYS_unlinkat, -100, NS("ns-long"), 0), 0);
    EQ(404, probe_sys3(SYS_unlinkat, -100, NS("ns-dangling"), 0), 0);
    EQ(405, probe_sys3(SYS_unlinkat, -100, NS("ns-dir"), 0x200), 0);
    ns_sync_directory("/test/ns-work", 406, 407, 408);
}

/// N4 保留旧 fd、shared/private 页到双 unmap，名字重建不能改变旧 inode。
void write_namespace_unlink(void)
{
    long old = write_open(NS("ns-old"), 0xc2, 0x180, 430);
    EQ(431, probe_sys3(SYS_write, old, "OLD!", 4), 4);
    EQ(432, probe_sys1(SYS_fsync, old), 0);
    EQ(433, probe_sys5(SYS_linkat, -100, NS("ns-old"), -100, NS("ns-alias"), 0), 0);
    struct WriteStat stat = write_stat(old, 434);
    unsigned long ino = stat.ino;
    EQ(435, stat.nlink, 2);
    long alias = write_open(NS("ns-alias"), 0, 0, 436);
    EQ(438, write_stat(alias, 437).ino, ino);
    EQ(439, probe_sys1(SYS_close, alias), 0);
    EQ(440, probe_sys6(SYS_mmap, 0x4b000000, 4096, 1, 1, old, 0), 0x4b000000);
    EQ(441, probe_sys6(SYS_mmap, 0x4c000000, 4096, 1, 2, old, 0), 0x4c000000);
    write_equal((volatile unsigned char *)0x4b000000UL, (const unsigned char *)"OLD!", 4, 442);
    write_equal((volatile unsigned char *)0x4c000000UL, (const unsigned char *)"OLD!", 4, 443);
    ns_sync_directory("/test/ns-work", 645, 646, 647);
    write_phase(1);
    write_gate(1);
    EQ(444, probe_sys3(SYS_unlinkat, -100, NS("ns-old"), 0), 0);
    EQ(445, probe_sys3(SYS_unlinkat, -100, NS("ns-alias"), 0), 0);
    stat = write_stat(old, 446);
    EQ(447, stat.nlink, 0);
    EQ(448, stat.size, 4);
    long fresh = write_open(NS("ns-old"), 0xc2, 0x180, 449);
    EQ(450, probe_sys3(SYS_write, fresh, "NEW!", 4), 4);
    EQ(451, probe_sys1(SYS_fsync, fresh), 0);
    stat = write_stat(fresh, 452);
    CHECK(453, stat.ino != ino);
    EQ(454, stat.nlink, 1);
    write_phase(2);
    write_gate(2);
    EQ(455, probe_sys3(SYS_lseek, old, 0, 0), 0);
    EQ(456, probe_sys3(SYS_read, old, write_buffer, 4), 4);
    write_equal(write_buffer, (const unsigned char *)"OLD!", 4, 457);
    write_equal((volatile unsigned char *)0x4b000000UL, (const unsigned char *)"OLD!", 4, 458);
    write_equal((volatile unsigned char *)0x4c000000UL, (const unsigned char *)"OLD!", 4, 459);
    EQ(461, write_stat(old, 460).nlink, 0);
    EQ(462, probe_sys1(SYS_close, old), 0);
    EQ(463, probe_sys2(SYS_munmap, 0x4b000000, 4096), 0);
    EQ(464, probe_sys2(SYS_munmap, 0x4c000000, 4096), 0);
    write_phase(3);
    write_gate(3);
    EQ(465, probe_sys1(SYS_fsync, fresh), 0);
    EQ(466, probe_sys3(SYS_unlinkat, -100, NS("ns-old"), 0), 0);
    ns_sync_directory("/test/ns-work", 467, 468, 469);
    EQ(470, probe_sys1(SYS_close, fresh), 0);
}

/// N5 覆盖和 NOREPLACE 消费真实目录项，旧目标 descriptor 与映射保原内容。
void write_namespace_rename(void)
{
    ns_create_data(NS("ns-source"), "SRC!", 4, 0x180, 480, 481, 482, 483);
    long old = write_open(NS("ns-target"), 0xc2, 0x180, 484);
    EQ(485, probe_sys3(SYS_write, old, "TGT!", 4), 4);
    EQ(486, probe_sys1(SYS_fsync, old), 0);
    EQ(488, write_stat(old, 487).nlink, 1);
    ns_create_data(NS("ns-plain-source"), "PLAIN", 5, 0x180, 489, 490, 491, 492);
    ns_create_data(NS("ns-noreplace-source"), "COPY", 4, 0x180, 493, 494, 495, 496);
    EQ(497, probe_sys6(SYS_mmap, 0x4b000000, 4096, 1, 1, old, 0), 0x4b000000);
    EQ(498, probe_sys6(SYS_mmap, 0x4c000000, 4096, 1, 2, old, 0), 0x4c000000);
    ns_sync_directory("/test/ns-work", 545, 546, 547);
    write_phase(1);
    write_gate(1);
    EQ(499, probe_sys5(SYS_renameat2, -100, NS("ns-source"), -100, NS("ns-target"), 0), 0);
    long fresh = write_open(NS("ns-target"), 0, 0, 500);
    EQ(501, probe_sys3(SYS_read, fresh, write_buffer, 4), 4);
    write_equal(write_buffer, (const unsigned char *)"SRC!", 4, 502);
    EQ(504, write_stat(old, 503).nlink, 0);
    EQ(506, write_stat(fresh, 505).nlink, 1);
    long plain = write_open(NS("ns-plain-source"), 0, 0, 507);
    EQ(508, probe_sys5(SYS_renameat2, -100, NS("ns-plain-source"), -100, NS("ns-plain-target"), 0), 0);
    EQ(509, probe_sys5(SYS_renameat2, -100, NS("ns-noreplace-source"), -100, NS("ns-plain-target"), 1), -17);
    ns_read_data(NS("ns-plain-target"), "PLAIN", 5, 510, 511, 512, 542);
    ns_read_data(NS("ns-noreplace-source"), "COPY", 4, 513, 514, 515, 516);
    EQ(517, probe_sys5(SYS_renameat2, -100, NS("ns-noreplace-source"), -100, NS("ns-noreplace-target"), 1), 0);
    ns_read_data(NS("ns-noreplace-target"), "COPY", 4, 518, 519, 520, 521);
    ns_sync_directory("/test/ns-work", 522, 523, 524);
    write_phase(2);
    write_gate(2);
    EQ(525, probe_sys3(SYS_lseek, old, 0, 0), 0);
    EQ(526, probe_sys3(SYS_read, old, write_buffer, 4), 4);
    write_equal(write_buffer, (const unsigned char *)"TGT!", 4, 527);
    write_equal((volatile unsigned char *)0x4b000000UL, (const unsigned char *)"TGT!", 4, 528);
    write_equal((volatile unsigned char *)0x4c000000UL, (const unsigned char *)"TGT!", 4, 529);
    EQ(530, probe_sys1(SYS_close, old), 0);
    EQ(531, probe_sys2(SYS_munmap, 0x4b000000, 4096), 0);
    EQ(532, probe_sys2(SYS_munmap, 0x4c000000, 4096), 0);
    EQ(534, probe_sys1(SYS_close, fresh), 0);
    EQ(535, probe_sys1(SYS_close, plain), 0);
    EQ(536, probe_sys3(SYS_unlinkat, -100, NS("ns-target"), 0), 0);
    EQ(537, probe_sys3(SYS_unlinkat, -100, NS("ns-plain-target"), 0), 0);
    EQ(538, probe_sys3(SYS_unlinkat, -100, NS("ns-noreplace-target"), 0), 0);
    ns_sync_directory("/test/ns-work", 539, 540, 541);
}

/// 第一个新栈子按原顺序覆盖交叉别名，再把 a2 目录移到另一个父目录。
static void ns_move_a2_child(void *argument, unsigned long entry_sp, unsigned long entry_tp)
{
    (void)argument; (void)entry_sp; (void)entry_tp;
    while (probe_load_acquire(write_word(8)) == 0) {}
    EQ(664, probe_sys5(SYS_renameat2, -100, NS("ns-d1/a"), -100, NS("ns-d1/b"), 0), 0);
#if defined(__riscv)
    __atomic_thread_fence(__ATOMIC_SEQ_CST);
#endif
    EQ(633, probe_sys5(SYS_renameat2, -100, NS("ns-a/a2"), -100, NS("ns-b/a2"), 0), 0);
    probe_store_release(write_word(12), 0);
    probe_exit(0);
}

/// 第二个新栈子执行相反父目录方向的 rename，父随后核拓扑和 nlink。
static void ns_move_y_child(void *argument, unsigned long entry_sp, unsigned long entry_tp)
{
    (void)argument; (void)entry_sp; (void)entry_tp;
    while (probe_load_acquire(write_word(8)) == 0) {}
    EQ(665, probe_sys5(SYS_renameat2, -100, NS("ns-d2/b"), -100, NS("ns-d2/a"), 0), 0);
    EQ(634, probe_sys5(SYS_renameat2, -100, NS("ns-b/y"), -100, NS("ns-a/y"), 0), 0);
    probe_store_release(write_word(16), 0);
    probe_exit(0);
}

/* 这段原 LA 使用独立编号和 read，RV 使用 pread；保留各自可观察 ABI。 */
#if defined(__loongarch64)
#define CROSS_CHECK(number) ((number) + 47)
#else
#define CROSS_CHECK(number) (number)
#endif

/// 检查交叉覆盖后旧 fd 与新目录项的 inode 和 payload，保两 ISA 原 read/pread 差异。
static void ns_cross_identity(long old, const char *path, const char *payload, long base)
{
    struct WriteStat stat = write_stat(old, CROSS_CHECK(base));
    EQ(CROSS_CHECK(base + 1), stat.nlink, 1);
    unsigned long ino = stat.ino;
    long fd = write_open(path, 0, 0, CROSS_CHECK(base + 2));
    EQ(CROSS_CHECK(base + 4), write_stat(fd, CROSS_CHECK(base + 3)).ino, ino);
#if defined(__loongarch64)
    EQ(CROSS_CHECK(base + 5), probe_sys3(SYS_read, fd, write_buffer, 4), 4);
#else
    EQ(CROSS_CHECK(base + 5), probe_sys4(SYS_pread64, fd, write_buffer, 4, 0), 4);
#endif
    write_equal(write_buffer, (const unsigned char *)payload, 4, CROSS_CHECK(base + 6));
    EQ(CROSS_CHECK(base + 7), probe_sys1(SYS_close, fd), 0);
}

/// 检查旧 OFD 的 seek 后内容，LA 保原 read，RV 保原定位读取。
static void ns_cross_old(long fd, const char *payload, long base)
{
    EQ(CROSS_CHECK(base), probe_sys3(SYS_lseek, fd, 0, 0), 0);
#if defined(__loongarch64)
    EQ(CROSS_CHECK(base + 1), probe_sys3(SYS_read, fd, write_buffer, 4), 4);
#else
    EQ(CROSS_CHECK(base + 1), probe_sys4(SYS_pread64, fd, write_buffer, 4, 0), 4);
#endif
    write_equal(write_buffer, (const unsigned char *)payload, 4, CROSS_CHECK(base + 2));
}

/// N6 保同 inode 别名、双新栈反向 rename、目录链接数和真实父项身份。
void write_namespace_directories(void)
{
    ns_create_data(NS("ns-alias-source"), "LINK", 4, 0x180, 550, 551, 552, 553);
    EQ(554, probe_sys5(SYS_linkat, -100, NS("ns-alias-source"), -100, NS("ns-alias-target"), 0), 0);
    long cross_a = write_open(NS("ns-cross-a"), 0xc2, 0x180, 660);
    EQ(661, probe_sys3(SYS_write, cross_a, "AAAA", 4), 4);
    EQ(662, probe_sys1(SYS_fsync, cross_a), 0);
    long cross_b = write_open(NS("ns-cross-b"), 0xc2, 0x180, 663);
    EQ(664, probe_sys3(SYS_write, cross_b, "BBBB", 4), 4);
    EQ(665, probe_sys1(SYS_fsync, cross_b), 0);
    EQ(666, probe_sys3(SYS_mkdirat, -100, NS("ns-d1"), 0x1ff), 0);
    EQ(667, probe_sys3(SYS_mkdirat, -100, NS("ns-d2"), 0x1ff), 0);
    EQ(668, probe_sys5(SYS_linkat, -100, NS("ns-cross-a"), -100, NS("ns-d1/a"), 0), 0);
    EQ(669, probe_sys5(SYS_linkat, -100, NS("ns-cross-a"), -100, NS("ns-d2/a"), 0), 0);
    EQ(670, probe_sys5(SYS_linkat, -100, NS("ns-cross-b"), -100, NS("ns-d1/b"), 0), 0);
    EQ(671, probe_sys5(SYS_linkat, -100, NS("ns-cross-b"), -100, NS("ns-d2/b"), 0), 0);
    EQ(673, write_stat(cross_a, 672).nlink, 3);
    EQ(675, write_stat(cross_b, 674).nlink, 3);
    ns_sync_directory(NS("ns-d1"), 676, 677, 678);
    ns_sync_directory(NS("ns-d2"), 679, 680, 681);
    EQ(555, probe_sys3(SYS_mkdirat, -100, NS("ns-a"), 0x1ff), 0);
    EQ(556, probe_sys3(SYS_mkdirat, -100, NS("ns-b"), 0x1ff), 0);
    EQ(557, probe_sys3(SYS_mkdirat, -100, NS("ns-a/x"), 0x1ff), 0);
    EQ(558, probe_sys3(SYS_mkdirat, -100, NS("ns-a/a2"), 0x1ff), 0);
    EQ(559, probe_sys3(SYS_mkdirat, -100, NS("ns-b/y"), 0x1ff), 0);
    EQ(560, probe_sys3(SYS_mkdirat, -100, NS("ns-cwd"), 0x1ff), 0);
    long fd = write_open(NS("ns-a"), 0x10000, 0, 561);
    EQ(563, write_stat(fd, 562).nlink, 4);
    EQ(564, probe_sys1(SYS_fsync, fd), 0);
    EQ(565, probe_sys1(SYS_close, fd), 0);
    fd = write_open(NS("ns-b"), 0x10000, 0, 566);
    EQ(568, write_stat(fd, 567).nlink, 3);
    EQ(569, probe_sys1(SYS_fsync, fd), 0);
    EQ(570, probe_sys1(SYS_close, fd), 0);
    ns_sync_directory("/test/ns-work", 571, 572, 573);
    write_phase(1);
    write_gate(1);
    EQ(680, probe_sys3(SYS_unlinkat, -100, NS("ns-cross-a"), 0), 0);
    EQ(681, probe_sys3(SYS_unlinkat, -100, NS("ns-cross-b"), 0), 0);
    ns_sync_directory("/test/ns-work", 682, 683, 684);
    EQ(574, probe_sys5(SYS_renameat2, -100, NS("ns-alias-source"), -100, NS("ns-alias-target"), 0), 0);
    EQ(575, probe_sys5(SYS_renameat2, -100, NS("ns-missing"), -100, NS("ns-missing"), 0), -2);
    EQ(716, probe_sys5(SYS_renameat2, -100, NS("ns-a"), -100, NS("ns-a/x"), 0), -22);
    EQ(576, probe_sys5(SYS_renameat2, -100, NS("ns-a"), -100, NS("ns-a/x/moved"), 0), -22);
    EQ(577, probe_sys5(SYS_renameat2, -100, NS("ns-a/x"), -100, NS("ns-b/x"), 0), 0);
    fd = write_open(NS("ns-a"), 0x10000, 0, 578);
    EQ(580, write_stat(fd, 579).nlink, 3);
    EQ(581, probe_sys1(SYS_close, fd), 0);
    fd = write_open(NS("ns-b"), 0x10000, 0, 582);
    struct WriteStat stat = write_stat(fd, 583);
    EQ(584, stat.nlink, 4);
    unsigned long parent_b = stat.ino;
    EQ(585, probe_sys1(SYS_close, fd), 0);
    fd = write_open(NS("ns-b/x/.."), 0x10000, 0, 586);
    EQ(588, write_stat(fd, 587).ino, parent_b);
    EQ(635, probe_sys1(SYS_close, fd), 0);
    EQ(589, probe_sys1(SYS_chdir, NS("ns-cwd")), 0);
    EQ(590, probe_sys3(SYS_unlinkat, -100, NS("ns-cwd"), 0x200), 0);
    EQ(591, probe_sys3(SYS_mkdirat, -100, "child", 0x1ff), -2);
    EQ(592, probe_sys1(SYS_chdir, "/test/ns-work"), 0);
    EQ(593, probe_sys5(SYS_renameat2, -100, NS("ns-alias-source"), -100, NS("ns-alias-target"), 2), -22);
    EQ(594, probe_sys5(SYS_renameat2, -100, NS("ns-alias-source"), -100, NS("ns-alias-target"), 4), -22);
    EQ(595, probe_sys3(SYS_unlinkat, -100, NS("ns-a"), 0x201), -22);
    long first = probe_clone(0x311, ns_stack_a + sizeof(ns_stack_a), 0, 0, 0, ns_move_a2_child, 0);
    CHECK(596, 0 <= first);
    long second = probe_clone(0x311, ns_stack_b + sizeof(ns_stack_b), 0, 0, 0, ns_move_y_child, 0);
    CHECK(597, 0 <= second);
    probe_store_release(write_word(8), 1);
    write_wait(first, 598, 599);
    write_wait(second, 600, 601);
#if defined(__loongarch64)
    EQ(602, probe_load_acquire(write_word(12)), 0);
    EQ(603, probe_load_acquire(write_word(16)), 0);
#else
    EQ(602, write_load64(12), 0);
    EQ(603, write_load64(16), 0);
#endif
    ns_cross_identity(cross_a, NS("ns-d1/b"), "AAAA", 640);
    ns_cross_identity(cross_b, NS("ns-d2/a"), "BBBB", 648);
    ns_cross_old(cross_a, "AAAA", 656);
    ns_cross_old(cross_b, "BBBB", 659);
    fd = write_open(NS("ns-a"), 0x10000, 0, 604);
    stat = write_stat(fd, 605);
    EQ(606, stat.nlink, 3);
    unsigned long parent_a = stat.ino;
    EQ(607, probe_sys1(SYS_fsync, fd), 0);
    EQ(608, probe_sys1(SYS_close, fd), 0);
    fd = write_open(NS("ns-b"), 0x10000, 0, 609);
    stat = write_stat(fd, 610);
    EQ(611, stat.nlink, 4);
    parent_b = stat.ino;
    EQ(612, probe_sys1(SYS_fsync, fd), 0);
    EQ(613, probe_sys1(SYS_close, fd), 0);
    fd = write_open(NS("ns-b/x/.."), 0x10000, 0, 614);
    EQ(616, write_stat(fd, 615).ino, parent_b);
    EQ(636, probe_sys1(SYS_close, fd), 0);
    fd = write_open(NS("ns-a/y/.."), 0x10000, 0, 617);
    EQ(619, write_stat(fd, 618).ino, parent_a);
    EQ(637, probe_sys1(SYS_close, fd), 0);
    ns_sync_directory("/test/ns-work", 620, 621, 622);
    write_phase(2);
    write_gate(2);
    EQ(CROSS_CHECK(662), probe_sys1(SYS_close, cross_a), 0);
    EQ(CROSS_CHECK(663), probe_sys1(SYS_close, cross_b), 0);
    EQ(623, probe_sys3(SYS_unlinkat, -100, NS("ns-alias-source"), 0), 0);
    EQ(624, probe_sys3(SYS_unlinkat, -100, NS("ns-alias-target"), 0), 0);
    EQ(638, probe_sys3(SYS_unlinkat, -100, NS("ns-d1/b"), 0), 0);
    EQ(639, probe_sys3(SYS_unlinkat, -100, NS("ns-d2/a"), 0), 0);
    EQ(625, probe_sys3(SYS_unlinkat, -100, NS("ns-b/x"), 0x200), 0);
    EQ(626, probe_sys3(SYS_unlinkat, -100, NS("ns-b/a2"), 0x200), 0);
    EQ(627, probe_sys3(SYS_unlinkat, -100, NS("ns-a/y"), 0x200), 0);
    EQ(628, probe_sys3(SYS_unlinkat, -100, NS("ns-a"), 0x200), 0);
    EQ(629, probe_sys3(SYS_unlinkat, -100, NS("ns-b"), 0x200), 0);
    EQ(640, probe_sys3(SYS_unlinkat, -100, NS("ns-d1"), 0x200), 0);
    EQ(641, probe_sys3(SYS_unlinkat, -100, NS("ns-d2"), 0x200), 0);
    ns_sync_directory("/test/ns-work", 630, 631, 632);
}
