//! 动态信息在每次打开时取实际来源快照，OFD 不反向保有 VFS。

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::fmt::{self, Write};
use core::ops::Range;
use core::sync::atomic::{AtomicU32, Ordering};

use crate::fs::file::{require_read, set_status_flags, stat_path};
use crate::fs::memfs::{MemoryFs, MemoryFsKind, MemorySpecial};
use crate::fs::{
    DirectoryEntry, FileIoContext, FileLike, FileRef, FileStat, FsError, InodeRef, Path,
    ReadBuffer, SeekFrom, Vfs, WriteBuffer,
};
use crate::sync::SleepMutex;
use crate::{accounting, cpu, mm, time};

const SNAPSHOT_LIMIT: usize = 64 * 1024;

/// 每个实际打开实例拥有本次文本和共享游标，重新打开才观察新的系统状态。
pub struct SnapshotFile {
    path: Path,
    bytes: Vec<u8>,
    cursor: SleepMutex<u64>,
    flags: AtomicU32,
}

/// 格式化只允许有限 fallible 增长，原始分配错误保留到格式化边界返回。
struct SnapshotText {
    bytes: Vec<u8>,
    error: Option<FsError>,
}

impl SnapshotText {
    /// 预留小信息文件所需空间，拒绝无法建立本次快照的打开。
    fn try_new() -> Result<Self, FsError> {
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(256)
            .map_err(|_| FsError::NoMemory)?;
        Ok(Self { bytes, error: None })
    }

    /// 完整格式化才移交文本，不交出已截断或分配失败的内容。
    fn finish(self, result: fmt::Result) -> Result<Vec<u8>, FsError> {
        match result {
            Ok(()) => Ok(self.bytes),
            Err(_) => Err(self
                .error
                .expect("snapshot formatting failed without source error")),
        }
    }
}

impl Write for SnapshotText {
    /// 每个格式片段先检查上限并准备容量，然后无失败追加。
    fn write_str(&mut self, text: &str) -> fmt::Result {
        let result = if SNAPSHOT_LIMIT - self.bytes.len() < text.len() {
            Err(FsError::FileTooLarge)
        } else {
            self.bytes
                .try_reserve_exact(text.len())
                .map_err(|_| FsError::NoMemory)
        };
        if let Err(error) = result {
            self.error = Some(error);
            return Err(fmt::Error);
        }
        self.bytes.extend_from_slice(text.as_bytes());
        Ok(())
    }
}

/// 建立受保护 procfs 命名空间，固定项保存角色而非预生成文本。
pub fn try_new(root_mode: u16) -> Result<Arc<MemoryFs>, FsError> {
    let fs = MemoryFs::try_new(MemoryFsKind::Procfs, root_mode)?;
    fs.initialize_special(b"mounts", MemorySpecial::Mounts, 0o444, 0)?;
    fs.initialize_special(b"meminfo", MemorySpecial::Meminfo, 0o444, 0)?;
    fs.initialize_special(b"uptime", MemorySpecial::Uptime, 0o444, 0)?;
    Ok(fs)
}

/// 在普通可睡眠上下文生成本次快照；借用 VFS 只持续到文本准备完成。
pub fn open(path: Path, flags: u32, special: MemorySpecial, vfs: &Vfs) -> Result<FileRef, FsError> {
    if flags & 0o3 != 0 || flags & 0o1000 != 0 {
        return Err(FsError::ReadOnly);
    }
    let bytes = match special {
        MemorySpecial::Mounts => vfs.mounts_text()?,
        MemorySpecial::Meminfo => meminfo()?,
        MemorySpecial::Uptime => uptime()?,
        _ => return Err(FsError::InvalidOperation),
    };
    if SNAPSHOT_LIMIT < bytes.len() {
        return Err(FsError::FileTooLarge);
    }
    Ok(Arc::try_new(SnapshotFile {
        path,
        bytes,
        cursor: SleepMutex::new(0),
        flags: AtomicU32::new(flags),
    })
    .map_err(|_| FsError::NoMemory)?)
}

/// 只输出现有账务能给出的可管理 RAM、空页与 slab backing，不估计可用量。
fn meminfo() -> Result<Vec<u8>, FsError> {
    let stats = mm::stats();
    let managed = stats.present_pages - stats.reserved_pages - stats.metadata_pages;
    let free = stats.buddy_free_pages + stats.pcp_cached_pages;
    let kb_per_page = mm::PAGE_SIZE / 1024;
    let mut text = SnapshotText::try_new()?;
    let result = write!(
        text,
        "MemTotal: {:8} kB\nMemFree:  {:8} kB\nSlab:     {:8} kB\n",
        managed * kb_per_page,
        free * kb_per_page,
        stats.slab_backing_pages * kb_per_page
    );
    text.finish(result)
}

/// 单调运行时间与所有 CPU 已结算 idle 总账分别输出，idle 不钳制到运行时间。
fn uptime() -> Result<Vec<u8>, FsError> {
    let elapsed = time::now().as_nanos();
    let idle = (0..cpu::count())
        .map(|owner| u128::from(accounting::snapshot(owner).idle_ns))
        .sum::<u128>();
    let mut text = SnapshotText::try_new()?;
    let result = write!(
        text,
        "{}.{:02} {}.{:02}\n",
        elapsed / 1_000_000_000,
        elapsed / 10_000_000 % 100,
        idle / 1_000_000_000,
        idle / 10_000_000 % 100
    );
    text.finish(result)
}

impl SnapshotFile {
    /// 不持源内容锁交付不可变文本，用户故障返回已交付的本次前缀。
    fn read_snapshot(&self, offset: u64, target: &mut ReadBuffer<'_>) -> Result<usize, FsError> {
        if self.bytes.len() as u64 <= offset || target.copied() == target.len() {
            return Ok(0);
        }
        let start = target.copied();
        let source = &self.bytes[offset as usize..];
        match target.push(source) {
            Ok(count) => Ok(count),
            Err(error) if target.copied() == start => Err(error),
            Err(_) => Ok(target.copied() - start),
        }
    }
}

impl FileLike for SnapshotFile {
    /// 顺序读取与复制描述符共享游标，实际交付后才推进位置。
    fn read_into(
        &self,
        target: &mut ReadBuffer<'_>,
        _caller: FileIoContext<'_>,
    ) -> Result<usize, FsError> {
        require_read(self.flags())?;
        let mut cursor = self.cursor.lock();
        let count = self.read_snapshot(*cursor, target)?;
        *cursor += count as u64;
        Ok(count)
    }

    /// 显式位置读取只消费本次快照，不改变共享游标。
    fn read_at_into(
        &self,
        offset: u64,
        target: &mut ReadBuffer<'_>,
        _caller: FileIoContext<'_>,
    ) -> Result<usize, FsError> {
        require_read(self.flags())?;
        self.read_snapshot(offset, target)
    }

    /// 保留实际 proc 节点路径以支持属性和挂载寿命。
    fn path(&self) -> Option<&Path> {
        Some(&self.path)
    }

    /// 动态文本快照没有普通文件页映射能力。
    fn mapping_inode(&self) -> Option<&InodeRef> {
        None
    }

    /// 独立打开各自保存状态，复制 OFD 共享原子字段。
    fn flags(&self) -> u32 {
        self.flags.load(Ordering::Relaxed)
    }

    /// 只改变公共协议允许的状态，不改变只读访问模式。
    fn set_status_flags(&self, raw: u32) -> Result<(), FsError> {
        set_status_flags(&self.flags, raw)
    }

    /// 动态信息不能截断。
    fn truncate(&self, _new_size: u64) -> Result<(), FsError> {
        Err(FsError::ReadOnly)
    }

    /// proc 快照没有可提交的持久化操作。
    fn sync_range(&self, _range: Range<u64>, _data_only: bool) -> Result<(), FsError> {
        Err(FsError::InvalidOperation)
    }

    /// 对本次文本长度计算非负 loff_t，失败不修改位置。
    fn seek(&self, from: SeekFrom) -> Result<u64, FsError> {
        let mut cursor = self.cursor.lock();
        let position = match from {
            SeekFrom::Start(position) => Some(position),
            SeekFrom::Current(delta) => cursor.checked_add_signed(delta),
            SeekFrom::End(delta) => (self.bytes.len() as u64).checked_add_signed(delta),
        }
        .ok_or(FsError::InvalidOperation)?;
        if (i64::MAX as u64) < position {
            return Err(FsError::InvalidOperation);
        }
        *cursor = position;
        Ok(position)
    }

    /// inode 提供身份和权限，长度准确反映本 OFD 已生成的完整快照。
    fn stat(&self) -> Result<FileStat, FsError> {
        let mut stat = stat_path(&self.path)?;
        stat.size = self.bytes.len() as u64;
        stat.blocks = 0;
        Ok(stat)
    }

    /// 普通动态文本节点不提供目录枚举。
    fn readdir(&self) -> Result<Option<DirectoryEntry>, FsError> {
        Err(FsError::NotDirectory)
    }

    /// 快照 OFD 不能作为目录起点。
    fn for_each_dirent(
        &self,
        _emit: &mut dyn FnMut(&DirectoryEntry) -> Result<bool, FsError>,
    ) -> Result<(), FsError> {
        Err(FsError::NotDirectory)
    }

    /// 只读 OFD 即使空写也返回描述符访问模式错误。
    fn write_from(
        &self,
        _source: &mut WriteBuffer<'_>,
        _caller: FileIoContext<'_>,
    ) -> Result<usize, FsError> {
        Err(FsError::BadFd)
    }

    /// 按位置写同样不访问输入端。
    fn write_at_from(
        &self,
        _offset: u64,
        _source: &mut WriteBuffer<'_>,
        _caller: FileIoContext<'_>,
    ) -> Result<usize, FsError> {
        Err(FsError::BadFd)
    }
}
