//! 在真实 Scratch 块设备上验证 FAT32 文件内容、写入截断和释放全部占用后的冷重开。
//!
//! `--disk scratch=PATH` 必须提供 `tools/create_basic_fat32.sh` 生成的 64 MiB 镜像；
//! 现有运行器将其复制为工作盘，客体只修改工作盘。每个用例实例独占一个工作盘，
//! 保证待创建文件最初不存在。设备发现、两层资源核对和 Disk 停止继续由原有
//! `DeviceInventory` 与 `Owner` 夹具负责，测试不创建其他磁盘或 VFS 挂载。
//! 两个核心依次拥有真实 `FsClaim`；临时文件借用结束后，先卸载、析构核心并等待
//! 占用全部释放，再从同一 Disk 申请新占用。

use crate::block::{AccessMode, BlkOperation, BlockError, BlockView, Disk, SECTOR_BYTES};
use crate::drivers::DeviceInventory;
use crate::fs::fat32::Fat32Core;
use crate::fs::ktest::{checked, Owner};
use crate::fs::FsError;
use ::ktest::{TestContext, TestError, TestResult};

/// 用大于文件长度的缓冲区读取至实际 EOF，核对长度、全文和未交付区域。
fn expect_file(
    context: &mut TestContext<'_>,
    core: &Fat32Core,
    path: &str,
    expected: &[u8],
) -> TestResult {
    let mut bytes = [0xa5; 128];
    let count = checked(context, core.ktest_read_file(path, &mut bytes))?;
    context.expect_eq(count, expected.len());
    context.expect_eq(&bytes[..count], expected);
    context.expect_eq(bytes[count..].iter().all(|byte| *byte == 0xa5), true);
    Ok(())
}

#[ktest::suite(fixture = Owner, suite_resources = DeviceInventory, context = "Thread", isolation = "Case", tags("fat32", "fs-fat32-disk", "fs-sync"), resources("scratch"), timeout_ms = 60000)]
pub(crate) mod core_disk {
    use super::*;

    /// 测试目标：验证真实 FAT32 盘的短名、子目录长名和新文件截断后的持久化内容。
    /// 场景步骤：读取宿主夹具的两个文件，创建跨簇文件并截断，卸载和释放第一核心，
    /// 等待其全部占用结束后，从同一 Disk 的新占用冷重开并读取截断结果至 EOF。
    /// 预期结果：每次读取的长度及每个字节准确，冷重开产生实际磁盘 I/O，最后再次
    /// 卸载并归还全部占用，原有 Owner 继续完成两层资源核对和设备停止。
    #[test]
    fn real_disk_read_write_truncate_cold_reopen(
        context: &mut TestContext<'_>,
        fixture: &mut Owner,
    ) -> TestResult {
        const CREATED: &str = "CORETEST.TXT";
        const RETAINED: usize = 37;

        let disk = fixture.disk()?;
        context.expect_eq(
            disk.limits().capacity_sectors,
            64 * 1024 * 1024 / SECTOR_BYTES,
        );
        let before = disk.test_snapshot();
        let claim = checked(
            context,
            Disk::claim_fs(&BlockView::whole(disk.clone()), AccessMode::ReadWrite)
                .map_err(FsError::from),
        )?;
        let first = checked(context, Fat32Core::ktest_open(claim))?;

        // Step 1: 直接读取盘内原文件，随后确认新文件名尚未占用。
        expect_file(context, &first, "HELLO.TXT", b"BLOSSOM FAT32 fixture\n")?;
        expect_file(
            context,
            &first,
            "SUBDIR/long file name.txt",
            b"directory and long name\n",
        )?;
        match first.ktest_read_file(CREATED, &mut [0u8; 1]) {
            Err(FsError::NotFound) => {}
            result => {
                checked(context, result)?;
                return Err(TestError("FAT32 fixture test file already exists"));
            }
        }

        // Step 2: 夹具每簇 512 字节，1025 字节写入跨过三个簇，截断后只保留首簇。
        let mut source = [0u8; 1025];
        for (index, byte) in source.iter_mut().enumerate() {
            *byte = (index as u8).wrapping_mul(37).wrapping_add(11);
        }
        checked(
            context,
            first.ktest_write_truncate(CREATED, &source, RETAINED as u64),
        )?;
        checked(context, first.finish_unmount())?;
        drop(first);
        disk.wait_no_fs_claims();
        let released = disk.test_snapshot();
        context.expect_eq(released.inflight, 0);
        context.expect_eq(before.flush_submitted < released.flush_submitted, true);

        // Step 3: 首个核心及在途请求已释放占用，重新申请凭证并从同一磁盘读取。
        let reads_before = released.submitted;
        let claim = checked(
            context,
            Disk::claim_fs(&BlockView::whole(disk.clone()), AccessMode::ReadWrite)
                .map_err(FsError::from),
        )?;
        let reopened = checked(context, Fat32Core::ktest_open(claim))?;
        expect_file(context, &reopened, CREATED, &source[..RETAINED])?;
        context.expect_eq(reads_before < disk.test_snapshot().submitted, true);
        checked(context, reopened.finish_unmount())?;
        drop(reopened);
        disk.wait_no_fs_claims();
        context.expect_eq(disk.test_snapshot().inflight, 0);
        Ok(())
    }

    /// 在真实 FAT32 盘的读、写和刷盘完成时注入软件错误，并核对上层返回及恢复。
    #[test]
    fn io_completion_errors_reach_fat_core(
        context: &mut TestContext<'_>,
        fixture: &mut Owner,
    ) -> TestResult {
        let disk = fixture.disk()?;
        if !disk.limits().flush {
            return Err(TestError("FAT32 error test requires FLUSH"));
        }
        let claim = checked(
            context,
            Disk::claim_fs(&BlockView::whole(disk.clone()), AccessMode::ReadWrite)
                .map_err(FsError::from),
        )?;
        let core = checked(context, Fat32Core::ktest_open(claim))?;

        disk.test_fail_next_operation(BlkOperation::Read, BlockError::Io);
        let mut bytes = [0xa5; 64];
        let result = core.ktest_read_file("HELLO.TXT", &mut bytes);
        context.expect_eq(result, Err(FsError::Io(BlockError::Io)));
        context.expect_eq(bytes, [0xa5; 64]);
        if disk.test_completion_error_pending() {
            return Err(TestError("FAT32 read error was not consumed"));
        }
        expect_file(context, &core, "HELLO.TXT", b"BLOSSOM FAT32 fixture\n")?;

        disk.test_fail_next_operation(BlkOperation::Write, BlockError::Io);
        let result = core.ktest_write_truncate("FAULT.TXT", b"data", 4);
        context.expect_eq(result, Err(FsError::Io(BlockError::Io)));
        if disk.test_completion_error_pending() {
            return Err(TestError("FAT32 write error was not consumed"));
        }
        checked(context, core.sync())?;

        disk.test_fail_next_flush(BlockError::Io);
        context.expect_eq(core.sync(), Err(FsError::Io(BlockError::Io)));
        context.expect_eq(disk.test_completion_error_pending(), false);
        checked(context, core.sync())?;
        checked(context, core.finish_unmount())?;
        drop(core);
        disk.wait_no_fs_claims();
        context.expect_eq(disk.test_snapshot().inflight, 0);
        Ok(())
    }
}
