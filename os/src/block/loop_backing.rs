//! loop0 持有的普通 ext4 文件与定位 I/O。
//!
//! 绑定保留原始 OFD 的强引用及容量快照，不使用或修改共享文件位置。仅接受物理块设备上
//! 的普通 ext4 文件，避免文件后端递归依赖。读写与同步均可能睡眠，调用方不得持有设备
//! 自旋锁；最后一个引用必须在独立管理任务完成设备停止与文件同步后释放。

use crate::block::{DiskIdentity, SECTOR_BYTES};
use crate::fs::{
    FileLike, FileRef, FileStat, FsError, InodeKind, O_ACCMODE, O_APPEND, O_RDONLY, O_WRONLY,
};

/// 一次 loop0 绑定持有的文件、固定容量及实际访问能力。
pub(crate) struct LoopBacking {
    /// 保留已打开文件及其挂载、节点和写准入凭证，不依赖用户描述符继续存在。
    file: FileRef,
    /// 向下取整为完整 512 字节扇区的绑定容量，单位为字节。
    bytes: u64,
    /// 由 loop 打开能力与 backing OFD 打开能力共同确定的只读属性。
    readonly: bool,
    /// 绑定时的真实文件属性，供控制面报告 backing 的设备号与节点号。
    stat: FileStat,
}

impl LoopBacking {
    /// 在普通任务中校验物理 ext4 普通文件，并保留其 OFD；只写文件不能作为可读块设备的来源。
    pub(crate) fn new(file: FileRef, loop_writable: bool) -> Result<Self, FsError> {
        crate::sched::assert_switchable();
        let regular = file.inode_file().ok_or(FsError::Unsupported)?;
        let inode = regular.mapping_inode().ok_or(FsError::Unsupported)?;
        let volume = inode.disk_volume()?;
        match volume.view().disk().identity() {
            DiskIdentity::Virtio(_) | DiskIdentity::Sdmmc { .. } | DiskIdentity::Ahci { .. } => {}
            DiskIdentity::Ram { .. } | DiskIdentity::Loop0 => return Err(FsError::Unsupported),
        }
        let stat = file.stat()?;
        if stat.kind != InodeKind::Regular {
            return Err(FsError::InvalidOperation);
        }
        let flags = file.flags();
        if flags & O_ACCMODE == O_WRONLY || flags & O_ACCMODE == O_ACCMODE {
            return Err(FsError::BadFd);
        }
        let readonly = !loop_writable || flags & O_ACCMODE == O_RDONLY;
        if !readonly {
            if flags & O_APPEND != 0 {
                return Err(FsError::InvalidOperation);
            }
            regular.transfer_check_write()?;
        }
        let bytes = stat.size / SECTOR_BYTES * SECTOR_BYTES;
        if bytes == 0 {
            return Err(FsError::InvalidOperation);
        }
        Ok(Self {
            file,
            bytes,
            readonly,
            stat,
        })
    }

    /// 返回绑定时确定的完整扇区容量，后续文件截断不会改变块设备几何参数。
    pub(crate) fn bytes(&self) -> u64 {
        self.bytes
    }

    /// 返回绑定实际授予的只读属性，后续状态 ioctl 不能扩大该能力。
    pub(crate) fn readonly(&self) -> bool {
        self.readonly
    }

    /// 返回用于 loop 状态查询的 backing 身份快照。
    pub(crate) fn backing_stat(&self) -> FileStat {
        self.stat
    }

    /// 在绑定容量内执行定位读取，保留真实 EOF 与短读，不访问或推进 OFD 的共享位置。
    pub(crate) fn read_at(&self, offset: u64, dst: &mut [u8]) -> Result<usize, FsError> {
        let length = self.bytes.saturating_sub(offset).min(dst.len() as u64) as usize;
        self.file.read_at(offset, &mut dst[..length])
    }

    /// 定位写入绑定范围，并执行原 OFD 的同步写与脏页节流；追加状态在每次写入时重新拒绝。
    pub(crate) fn write_at(&self, offset: u64, src: &[u8]) -> Result<usize, FsError> {
        if self.readonly {
            return Err(FsError::ReadOnly);
        }
        let end = offset
            .checked_add(src.len() as u64)
            .ok_or(FsError::InvalidOperation)?;
        if self.bytes < end {
            return Err(FsError::InvalidOperation);
        }
        let regular = self
            .file
            .inode_file()
            .expect("validated loop backing lost its regular OFD");
        let flags = self.file.flags();
        let accepted = regular.transfer_write_at(offset, src, FsError::InvalidOperation)?;
        regular.transfer_finish(flags, offset, accepted)?;
        Ok(accepted)
    }

    /// 将 backing 文件的脏数据、必要元数据及设备缓存同步，保留实际回写错误。
    pub(crate) fn flush(&self) -> Result<(), FsError> {
        self.file.sync_range(0..u64::MAX, false)
    }
}
