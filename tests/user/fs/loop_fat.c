#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stddef.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <sys/vfs.h>
#include <unistd.h>

#include "../include/selftest_io.h"

#define LOOP_SET_FD 0x4c00
#define LOOP_CLR_FD 0x4c01
#define LOOP_GET_STATUS64 0x4c05
#define LO_FLAGS_READ_ONLY 1U
#define LO_FLAGS_AUTOCLEAR 4U
#define EXT4_SUPER_MAGIC 0xef53UL
#define FAT_SUPER_MAGIC 0x4d44UL
#define STATFS_RDONLY 1UL
#define SECTOR_SIZE 512U

/* 固定宽度字段与偏移来自 loop 设备的用户 ABI，不引用工具链内核头。 */
struct loop_info64_local {
    uint64_t device;
    uint64_t inode;
    uint64_t rdevice;
    uint64_t offset;
    uint64_t sizelimit;
    uint32_t number;
    uint32_t encrypt_type;
    uint32_t encrypt_key_size;
    uint32_t flags;
    uint8_t file_name[64];
    uint8_t crypt_name[64];
    uint8_t encrypt_key[32];
    uint64_t init[2];
};

_Static_assert(sizeof(struct loop_info64_local) == 232, "loop_info64 size");
_Static_assert(offsetof(struct loop_info64_local, flags) == 52, "loop_info64 flags");
_Static_assert(offsetof(struct loop_info64_local, file_name) == 56, "loop_info64 name");
_Static_assert(offsetof(struct loop_info64_local, crypt_name) == 120, "loop_info64 crypt name");
_Static_assert(offsetof(struct loop_info64_local, encrypt_key) == 184, "loop_info64 key");
_Static_assert(offsetof(struct loop_info64_local, init) == 216, "loop_info64 init");

static int failed;
static int first_errno;

/// 记录首个失败并保留清理阶段的实际错误。
static void report_failure(const char *what, int line, int error)
{
    if (!failed) {
        failed = 1;
        first_errno = error;
    }
    fprintf(stderr, "FAIL loop_fat:%d: %s: errno=%d (%s)\n",
            line, what, error, error ? strerror(error) : "condition false");
    errno = error;
}

#define CHECK(expression) do { \
    errno = 0; \
    if (!(expression)) { \
        report_failure(#expression, __LINE__, errno); \
        goto out; \
    } \
} while (0)

#define CLEAN(expression) do { \
    errno = 0; \
    if (!(expression)) \
        report_failure(#expression, __LINE__, errno); \
} while (0)

/// 完整复制输入文件，短写和信号不丢弃已读取的字节。
static int copy_image(int source, int target)
{
    unsigned char buffer[16384];
    for (;;) {
        ssize_t count = read(source, buffer, sizeof(buffer));
        if (count < 0 && errno == EINTR)
            continue;
        if (count < 0)
            return -1;
        if (count == 0)
            return 0;
        if (st_write_all(target, buffer, (size_t)count) != 0)
            return -1;
    }
}

/// 检查实际读取的引导扇区包含 FAT32 BPB 所需的基本字段。
static int fat32_bpb(const unsigned char *bpb)
{
    unsigned int bytes_per_sector = bpb[11] | ((unsigned int)bpb[12] << 8);
    unsigned int sectors_per_cluster = bpb[13];
    return bpb[510] == 0x55 && bpb[511] == 0xaa &&
           512 <= bytes_per_sector && bytes_per_sector <= 4096 &&
           (bytes_per_sector & (bytes_per_sector - 1)) == 0 &&
           sectors_per_cluster != 0 &&
           (sectors_per_cluster & (sectors_per_cluster - 1)) == 0 &&
           (bpb[14] != 0 || bpb[15] != 0) && bpb[16] != 0 &&
           bpb[17] == 0 && bpb[18] == 0 &&
           bpb[19] == 0 && bpb[20] == 0 &&
           bpb[22] == 0 && bpb[23] == 0 &&
           (bpb[32] != 0 || bpb[33] != 0 || bpb[34] != 0 || bpb[35] != 0) &&
           (bpb[36] != 0 || bpb[37] != 0 || bpb[38] != 0 || bpb[39] != 0);
}

/// 以独立测试副本检查 loop0、FAT 挂载、大文件截断和最后使用者释放。
int main(int argc, char **argv)
{
    const char *device = "/dev/loop0";
    const char *slash;
    const char copy_name[] = ".loop-fat-XXXXXX";
    struct loop_info64_local info, other;
    struct stat input_stat, copy_stat, device_stat, alt_stat;
    struct stat root_before, root_after, target_before, target_after;
    struct stat first_dir, second_dir;
    struct statfs fs, target_fs_before, target_fs_after;
    unsigned char bpb[SECTOR_SIZE], original[SECTOR_SIZE];
    unsigned char pattern[SECTOR_SIZE], observed[SECTOR_SIZE];
    size_t dir_length, alt_length;
    off_t sector_offset = 0;
    char *copy_path = NULL, *alt_path = NULL;
    int source_fd = -1, copy_fd = -1, verify_fd = -1;
    int readonly_fd = -1;
    int loop_fd = -1, alt_fd = -1, root_fd = -1;
    int created = 0, bound = 0, clear_requested = 0, restore_needed = 0;
    int dev_mounted = 0, fat_mounted = 0, bad_mounted = 0;
    int result, saved_error;

    if (argc != 4) {
        fprintf(stderr, "FAIL usage: %s FAT32_IMAGE FAT_MOUNT_DIR DEV_MOUNT_DIR\n", argv[0]);
        return 2;
    }

    CHECK(stat(argv[2], &first_dir) == 0 && S_ISDIR(first_dir.st_mode));
    CHECK(stat(argv[3], &second_dir) == 0 && S_ISDIR(second_dir.st_mode));
    CHECK(first_dir.st_dev != second_dir.st_dev || first_dir.st_ino != second_dir.st_ino);
    source_fd = open(argv[1], O_RDONLY);
    CHECK(0 <= source_fd);
    CHECK(fstat(source_fd, &input_stat) == 0 && S_ISREG(input_stat.st_mode));
    CHECK(fstatfs(source_fd, &fs) == 0 && (unsigned long)fs.f_type == EXT4_SUPER_MAGIC);
    CHECK((off_t)(2 * SECTOR_SIZE) <= input_stat.st_size);

    /* 所有写入仅针对新建副本；其所在 ext4 也必须是独立测试工作盘。 */
    slash = strrchr(argv[1], '/');
    dir_length = slash ? (size_t)(slash - argv[1]) + 1 : 0;
    CHECK(dir_length <= SIZE_MAX - sizeof(copy_name));
    copy_path = malloc(dir_length + sizeof(copy_name));
    CHECK(copy_path != NULL);
    memcpy(copy_path, argv[1], dir_length);
    memcpy(copy_path + dir_length, copy_name, sizeof(copy_name));
    copy_fd = mkstemp(copy_path);
    CHECK(0 <= copy_fd);
    created = 1;
    CHECK(fstat(copy_fd, &copy_stat) == 0 && S_ISREG(copy_stat.st_mode));
    CHECK(copy_stat.st_dev != input_stat.st_dev || copy_stat.st_ino != input_stat.st_ino);
    CHECK(fstatfs(copy_fd, &fs) == 0 && (unsigned long)fs.f_type == EXT4_SUPER_MAGIC);
    CHECK(copy_image(source_fd, copy_fd) == 0);
    CHECK(fsync(copy_fd) == 0);
    CHECK(fstat(copy_fd, &copy_stat) == 0 && copy_stat.st_size == input_stat.st_size);
    CHECK(lseek(copy_fd, 0, SEEK_SET) == 0);
    CHECK(read(copy_fd, bpb, sizeof(bpb)) == (ssize_t)sizeof(bpb));
    CHECK(fat32_bpb(bpb));
    sector_offset = (copy_stat.st_size / SECTOR_SIZE - 1) * SECTOR_SIZE;
    CHECK(st_close(&source_fd) == 0);

    CHECK(stat(device, &device_stat) == 0 && S_ISBLK(device_stat.st_mode));
    CHECK(major(device_stat.st_rdev) == 7 && minor(device_stat.st_rdev) == 0);
    loop_fd = open(device, O_RDWR);
    CHECK(0 <= loop_fd);
    CHECK(ioctl(loop_fd, LOOP_GET_STATUS64, &info) == -1 && errno == ENXIO);

    CHECK(mount("devfs", argv[3], "devfs", 0, NULL) == 0);
    dev_mounted = 1;
    alt_length = strlen(argv[3]);
    CHECK(alt_length <= SIZE_MAX - sizeof("/loop0"));
    alt_path = malloc(alt_length + sizeof("/loop0"));
    CHECK(alt_path != NULL);
    memcpy(alt_path, argv[3], alt_length);
    memcpy(alt_path + alt_length, "/loop0", sizeof("/loop0"));
    CHECK(stat(alt_path, &alt_stat) == 0 && S_ISBLK(alt_stat.st_mode));
    CHECK(alt_stat.st_rdev == device_stat.st_rdev);
    alt_fd = open(alt_path, O_RDWR);
    CHECK(0 <= alt_fd);
    CHECK(ioctl(alt_fd, LOOP_GET_STATUS64, &other) == -1 && errno == ENXIO);

    CHECK(ioctl(loop_fd, LOOP_SET_FD, (unsigned long)copy_fd) == 0);
    bound = 1;
    CHECK(st_close(&copy_fd) == 0);
    CHECK(ioctl(loop_fd, LOOP_GET_STATUS64, &info) == 0);
    CHECK(info.device == (uint64_t)copy_stat.st_dev && info.inode == (uint64_t)copy_stat.st_ino);
    CHECK(info.rdevice == 0 && info.offset == 0 && info.sizelimit == 0 && info.number == 0);
    CHECK((info.flags & (LO_FLAGS_READ_ONLY | LO_FLAGS_AUTOCLEAR)) == 0);
    CHECK(ioctl(alt_fd, LOOP_GET_STATUS64, &other) == 0);
    CHECK(memcmp(&info, &other, sizeof(info)) == 0);
    CHECK(lseek(loop_fd, 0, SEEK_SET) == 0);
    CHECK(read(loop_fd, observed, sizeof(observed)) == (ssize_t)sizeof(observed));
    CHECK(memcmp(observed, bpb, sizeof(bpb)) == 0 && fat32_bpb(observed));
    CHECK(lseek(alt_fd, 0, SEEK_SET) == 0);
    CHECK(read(alt_fd, observed, sizeof(observed)) == (ssize_t)sizeof(observed));
    CHECK(memcmp(observed, bpb, sizeof(bpb)) == 0);
    verify_fd = open(copy_path, O_RDWR);
    CHECK(0 <= verify_fd);
    CHECK(ioctl(loop_fd, LOOP_SET_FD, (unsigned long)verify_fd) == -1 && errno == EBUSY);
    CHECK(st_close(&alt_fd) == 0);
    CHECK(umount(argv[3]) == 0);
    dev_mounted = 0;

    CHECK(lseek(loop_fd, sector_offset, SEEK_SET) == sector_offset);
    CHECK(read(loop_fd, original, sizeof(original)) == (ssize_t)sizeof(original));
    CHECK(lseek(verify_fd, sector_offset, SEEK_SET) == sector_offset);
    CHECK(read(verify_fd, observed, sizeof(observed)) == (ssize_t)sizeof(observed));
    CHECK(memcmp(observed, original, sizeof(original)) == 0);
    for (size_t i = 0; i < sizeof(pattern); ++i)
        pattern[i] = original[i] ^ 0xa5;
    CHECK(lseek(loop_fd, sector_offset, SEEK_SET) == sector_offset);
    restore_needed = 1;
    CHECK(write(loop_fd, pattern, sizeof(pattern)) == (ssize_t)sizeof(pattern));
    CHECK(fsync(loop_fd) == 0);
    CHECK(lseek(loop_fd, sector_offset, SEEK_SET) == sector_offset);
    CHECK(read(loop_fd, observed, sizeof(observed)) == (ssize_t)sizeof(observed));
    CHECK(memcmp(observed, pattern, sizeof(pattern)) == 0);
    CHECK(lseek(verify_fd, sector_offset, SEEK_SET) == sector_offset);
    CHECK(read(verify_fd, observed, sizeof(observed)) == (ssize_t)sizeof(observed));
    CHECK(memcmp(observed, pattern, sizeof(pattern)) == 0);
    CHECK(lseek(loop_fd, sector_offset, SEEK_SET) == sector_offset);
    CHECK(write(loop_fd, original, sizeof(original)) == (ssize_t)sizeof(original));
    CHECK(fsync(loop_fd) == 0);
    CHECK(lseek(loop_fd, sector_offset, SEEK_SET) == sector_offset);
    CHECK(read(loop_fd, observed, sizeof(observed)) == (ssize_t)sizeof(observed));
    CHECK(memcmp(observed, original, sizeof(original)) == 0);
    CHECK(lseek(verify_fd, sector_offset, SEEK_SET) == sector_offset);
    CHECK(read(verify_fd, observed, sizeof(observed)) == (ssize_t)sizeof(observed));
    CHECK(memcmp(observed, original, sizeof(original)) == 0);
    restore_needed = 0;

    CHECK(mount(device, argv[2], "vfat", 0, NULL) == 0);
    fat_mounted = 1;
    CHECK(statfs(argv[2], &fs) == 0 && (unsigned long)fs.f_type == FAT_SUPER_MAGIC);
    CHECK(((unsigned long)fs.f_flags & STATFS_RDONLY) == 0);
    root_fd = open(argv[2], O_RDONLY | O_DIRECTORY);
    CHECK(0 <= root_fd);
    CHECK(fstat(root_fd, &root_before) == 0 && S_ISDIR(root_before.st_mode));

    errno = 0;
    result = umount(argv[2]);
    saved_error = errno;
    if (result == 0)
        fat_mounted = 0;
    if (result != -1 || saved_error != EBUSY) {
        report_failure("umount with an open FAT root must return EBUSY", __LINE__, saved_error);
        goto out;
    }
    CHECK(ioctl(loop_fd, LOOP_CLR_FD, 0UL) == 0);
    clear_requested = 1;
    CHECK(ioctl(loop_fd, LOOP_GET_STATUS64, &info) == 0);
    CHECK(info.device == (uint64_t)copy_stat.st_dev && info.inode == (uint64_t)copy_stat.st_ino);
    CHECK((info.flags & LO_FLAGS_AUTOCLEAR) != 0 && (info.flags & LO_FLAGS_READ_ONLY) == 0);
    CHECK(fstat(root_fd, &root_after) == 0 && S_ISDIR(root_after.st_mode));
    CHECK(root_before.st_dev == root_after.st_dev && root_before.st_ino == root_after.st_ino);
    CHECK(lseek(loop_fd, 0, SEEK_SET) == 0);
    CHECK(read(loop_fd, observed, sizeof(observed)) == (ssize_t)sizeof(observed));
    CHECK(fat32_bpb(observed) && memcmp(observed + 11, bpb + 11, 42) == 0);
    CHECK(st_close(&root_fd) == 0);
    CHECK(umount(argv[2]) == 0);
    fat_mounted = 0;
    CHECK(st_close(&loop_fd) == 0);
    loop_fd = open(device, O_RDWR);
    CHECK(0 <= loop_fd);
    CHECK(ioctl(loop_fd, LOOP_GET_STATUS64, &info) == -1 && errno == ENXIO);
    bound = 0;
    clear_requested = 0;

    /* 单独打开只读 backing，保留可写描述符供后续损坏测试。 */
    readonly_fd = open(copy_path, O_RDONLY);
    CHECK(0 <= readonly_fd);
    CHECK(ioctl(loop_fd, LOOP_SET_FD, (unsigned long)readonly_fd) == 0);
    bound = 1;
    CHECK(st_close(&readonly_fd) == 0);
    CHECK(ioctl(loop_fd, LOOP_GET_STATUS64, &info) == 0);
    CHECK(info.device == (uint64_t)copy_stat.st_dev && info.inode == (uint64_t)copy_stat.st_ino);
    CHECK((info.flags & LO_FLAGS_READ_ONLY) != 0);

    errno = 0;
    result = mount(device, argv[2], "vfat", 0, NULL);
    saved_error = errno;
    if (result == 0)
        fat_mounted = 1;
    if (result != -1 || saved_error != EROFS) {
        report_failure("writable FAT mount on read-only backing must return EROFS",
                       __LINE__, saved_error);
        goto out;
    }
    CHECK(mount(device, argv[2], "vfat", MS_RDONLY, NULL) == 0);
    fat_mounted = 1;
    CHECK(statfs(argv[2], &fs) == 0 && (unsigned long)fs.f_type == FAT_SUPER_MAGIC);
    CHECK(((unsigned long)fs.f_flags & STATFS_RDONLY) != 0);
    CHECK(umount(argv[2]) == 0);
    fat_mounted = 0;
    CHECK(ioctl(loop_fd, LOOP_CLR_FD, 0UL) == 0);
    clear_requested = 1;
    CHECK(st_close(&loop_fd) == 0);
    loop_fd = open(device, O_RDWR);
    CHECK(0 <= loop_fd);
    CHECK(ioctl(loop_fd, LOOP_GET_STATUS64, &info) == -1 && errno == ENXIO);
    bound = 0;
    clear_requested = 0;

    /* 截断完整工作副本后恢复镜像，保留后续坏盘与最终删除场景。 */
    CHECK(fstat(verify_fd, &copy_stat) == 0 &&
          copy_stat.st_size == input_stat.st_size &&
          (off_t)(64 * 1024 * 1024) <= copy_stat.st_size);
    CHECK(ftruncate(verify_fd, 0) == 0);
    CHECK(fstat(verify_fd, &copy_stat) == 0 && copy_stat.st_size == 0);
    source_fd = open(argv[1], O_RDONLY);
    CHECK(0 <= source_fd);
    CHECK(lseek(source_fd, 0, SEEK_SET) == 0);
    CHECK(lseek(verify_fd, 0, SEEK_SET) == 0);
    CHECK(copy_image(source_fd, verify_fd) == 0);
    CHECK(fsync(verify_fd) == 0);
    CHECK(fstat(verify_fd, &copy_stat) == 0 && copy_stat.st_size == input_stat.st_size);
    CHECK(lseek(verify_fd, 0, SEEK_SET) == 0);
    CHECK(read(verify_fd, observed, sizeof(observed)) == (ssize_t)sizeof(observed));
    CHECK(memcmp(observed, bpb, sizeof(bpb)) == 0);
    CHECK(st_close(&source_fd) == 0);

    /* 损坏副本的扇区大小字段，挂载失败后仍检查原挂载点的身份。 */
    memcpy(pattern, bpb, sizeof(pattern));
    pattern[11] = 0;
    pattern[12] = 0;
    CHECK(lseek(verify_fd, 0, SEEK_SET) == 0);
    CHECK(write(verify_fd, pattern, sizeof(pattern)) == (ssize_t)sizeof(pattern));
    CHECK(fsync(verify_fd) == 0);
    CHECK(ioctl(loop_fd, LOOP_SET_FD, (unsigned long)verify_fd) == 0);
    bound = 1;
    CHECK(st_close(&verify_fd) == 0);
    CHECK(ioctl(loop_fd, LOOP_GET_STATUS64, &info) == 0);
    CHECK(info.device == (uint64_t)copy_stat.st_dev && info.inode == (uint64_t)copy_stat.st_ino);
    CHECK(lseek(loop_fd, 0, SEEK_SET) == 0);
    CHECK(read(loop_fd, observed, sizeof(observed)) == (ssize_t)sizeof(observed));
    CHECK(memcmp(observed, pattern, sizeof(pattern)) == 0);
    CHECK(stat(argv[3], &target_before) == 0);
    CHECK(statfs(argv[3], &target_fs_before) == 0);
    errno = 0;
    result = mount(device, argv[3], "vfat", 0, NULL);
    saved_error = errno;
    if (result == 0)
        bad_mounted = 1;
    if (result != -1 || (saved_error != EINVAL && saved_error != EIO)) {
        report_failure("invalid FAT mount must fail with EINVAL or EIO", __LINE__, saved_error);
        goto out;
    }
    CHECK(stat(argv[3], &target_after) == 0);
    CHECK(statfs(argv[3], &target_fs_after) == 0);
    CHECK(target_before.st_dev == target_after.st_dev && target_before.st_ino == target_after.st_ino);
    CHECK(target_fs_before.f_type == target_fs_after.f_type);
    CHECK(ioctl(loop_fd, LOOP_CLR_FD, 0UL) == 0);
    clear_requested = 1;
    CHECK(st_close(&loop_fd) == 0);
    loop_fd = open(device, O_RDWR);
    CHECK(0 <= loop_fd);
    CHECK(ioctl(loop_fd, LOOP_GET_STATUS64, &info) == -1 && errno == ENXIO);
    bound = 0;
    CHECK(st_close(&loop_fd) == 0);

out:
    if (restore_needed && 0 <= loop_fd) {
        /* 即使首个写入只完成前缀，也尝试恢复整个原始扇区并同步。 */
        errno = 0;
        if (lseek(loop_fd, sector_offset, SEEK_SET) != sector_offset) {
            report_failure("cleanup seek original sector", __LINE__, errno);
        } else {
            CLEAN(write(loop_fd, original, sizeof(original)) == (ssize_t)sizeof(original));
        }
        CLEAN(fsync(loop_fd) == 0);
    }
    CLEAN(st_close(&root_fd) == 0);
    if (bad_mounted)
        CLEAN(umount(argv[3]) == 0);
    if (fat_mounted)
        CLEAN(umount(argv[2]) == 0);
    CLEAN(st_close(&alt_fd) == 0);
    if (dev_mounted)
        CLEAN(umount(argv[3]) == 0);
    if (bound && 0 <= loop_fd) {
        errno = 0;
        result = ioctl(loop_fd, LOOP_CLR_FD, 0UL);
        saved_error = errno;
        if (result == 0 || (clear_requested && result == -1 && saved_error == ENXIO))
            clear_requested = 1;
        else
            report_failure("cleanup LOOP_CLR_FD", __LINE__, saved_error);
    }
    CLEAN(st_close(&loop_fd) == 0);
    if (bound && clear_requested) {
        loop_fd = open(device, O_RDWR);
        if (loop_fd < 0) {
            report_failure("cleanup open loop0", __LINE__, errno);
        } else {
            CLEAN(ioctl(loop_fd, LOOP_GET_STATUS64, &info) == -1 && errno == ENXIO);
            CLEAN(st_close(&loop_fd) == 0);
        }
    }
    CLEAN(st_close(&readonly_fd) == 0);
    CLEAN(st_close(&verify_fd) == 0);
    CLEAN(st_close(&copy_fd) == 0);
    CLEAN(st_close(&source_fd) == 0);
    if (created)
        CLEAN(unlink(copy_path) == 0);
    free(alt_path);
    free(copy_path);
    if (failed) {
        fprintf(stderr, "FAIL loop_fat: first errno=%d\n", first_errno);
        errno = first_errno;
        return 1;
    }
    puts("PASS loop_fat: real loop0, FAT32 mount, deferred clear, invalid image and cleanup");
    return 0;
}
