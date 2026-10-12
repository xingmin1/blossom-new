//! 固定字符设备保留真实路径；字节交付不依赖磁盘页或回写占用。

use alloc::sync::Arc;
use core::ops::Range;
use core::sync::atomic::{AtomicU32, Ordering};

use crate::fs::file::{require_read, require_write, set_status_flags, stat_path};
use crate::fs::memfs::{MemoryFs, MemoryFsKind, MemorySpecial};
use crate::fs::{
    DirectoryEntry, FileIoContext, FileLike, FileRef, FileStat, FsError, InodeRef, Path,
    ReadBuffer, SeekFrom, WriteBuffer,
};

/// 空设备每次打开只拥有路径与共享状态，不维护没有语义的字节位置。
pub struct NullFile {
    path: Path,
    flags: AtomicU32,
}

/// 零设备使用有界栈片段交付数据，不建立可映射文件页。
pub struct ZeroFile {
    path: Path,
    flags: AtomicU32,
}

/// 建立独立未发表的 devfs 实例，在真实根下安装空设备与唯一 UART 别名。
pub fn try_new(root_mode: u16) -> Result<Arc<MemoryFs>, FsError> {
    let fs = MemoryFs::try_new(MemoryFsKind::Devfs, root_mode)?;
    let root = fs.root();
    fs.initialize_special(&root, b"null", MemorySpecial::Null, 0o666, 0x103)?;
    fs.initialize_special(&root, b"zero", MemorySpecial::Zero, 0o666, 0x105)?;
    fs.initialize_special(&root, b"ttyS0", MemorySpecial::TtySerial, 0o666, 0x440)?;
    fs.initialize_special(&root, b"console", MemorySpecial::Console, 0o666, 0x501)?;
    fs.initialize_special(&root, b"tty", MemorySpecial::ControllingTty, 0o666, 0x500)?;
    Ok(fs)
}

/// 从 inode 的实际特殊角色建立 OFD，调用者已经完成公共打开标志和访问检查。
pub fn open(
    path: Path,
    flags: u32,
    _caller: FileIoContext<'_>,
    special: MemorySpecial,
) -> Result<FileRef, FsError> {
    match special {
        MemorySpecial::Null => Ok(Arc::try_new(NullFile {
            path,
            flags: AtomicU32::new(flags),
        })
        .map_err(|_| FsError::NoMemory)?),
        MemorySpecial::Zero => Ok(Arc::try_new(ZeroFile {
            path,
            flags: AtomicU32::new(flags),
        })
        .map_err(|_| FsError::NoMemory)?),
        _ => Err(FsError::InvalidOperation),
    }
}

/// 丢弃设备仍从输入端取得真实字节，故障只返回已经消费的正前缀。
fn discard(source: &mut WriteBuffer<'_>) -> Result<usize, FsError> {
    let start = source.copied();
    let mut bytes = [0; 512];
    while source.copied() < source.len() {
        match source.pull(&mut bytes) {
            Ok(0) => break,
            Ok(_) => {}
            Err(error) if source.copied() == start => return Err(error),
            Err(_) => break,
        }
    }
    Ok(source.copied() - start)
}

/// 零片段不持任何内容锁，用户 fault 可以正常取得自己的 MM 和文件锁。
fn zeros(target: &mut ReadBuffer<'_>) -> Result<usize, FsError> {
    let start = target.copied();
    let bytes = [0; 512];
    while target.copied() < target.len() {
        match target.push(&bytes) {
            Ok(0) => break,
            Ok(_) => {}
            Err(error) if target.copied() == start => return Err(error),
            Err(_) => break,
        }
    }
    Ok(target.copied() - start)
}

impl FileLike for NullFile {
    /// 空设备接受显式位置，位置值不改变EOF和丢弃写入语义。
    fn positioned_io(&self) -> bool {
        true
    }

    /// 空设备即使接收端地址不可访问也只返回 EOF，不访问接收端。
    fn read_into(
        &self,
        _target: &mut ReadBuffer<'_>,
        _caller: FileIoContext<'_>,
    ) -> Result<usize, FsError> {
        require_read(self.flags())?;
        Ok(0)
    }

    /// 空设备按位置读取同样返回 EOF，位置没有数据含义。
    fn read_at_into(
        &self,
        _offset: u64,
        target: &mut ReadBuffer<'_>,
        caller: FileIoContext<'_>,
    ) -> Result<usize, FsError> {
        self.read_into(target, caller)
    }

    /// 打开的真实设备路径保活挂载与 inode。
    fn path(&self) -> Option<&Path> {
        Some(&self.path)
    }

    /// 空设备没有普通文件映射能力。
    fn mapping_inode(&self) -> Option<&InodeRef> {
        None
    }

    /// 同一 OFD 的复制引用共享原子状态。
    fn flags(&self) -> u32 {
        self.flags.load(Ordering::Relaxed)
    }

    /// 只更新公共文件协议允许改变的状态位。
    fn set_status_flags(&self, raw: u32) -> Result<(), FsError> {
        set_status_flags(&self.flags, raw)
    }

    /// 字符设备没有普通文件长度，不能截断。
    fn truncate(&self, _new_size: u64) -> Result<(), FsError> {
        Err(FsError::InvalidOperation)
    }

    /// 设备没有 fsync 操作，不伪造磁盘同步成功。
    fn sync_range(&self, _range: Range<u64>, _data_only: bool) -> Result<(), FsError> {
        Err(FsError::InvalidOperation)
    }

    /// 所有受支持的 lseek 起点和位移均归零，包括负相对位移。
    fn seek(&self, _from: SeekFrom) -> Result<u64, FsError> {
        Ok(0)
    }

    /// 设备号和文件系统身份来自 canonical inode 的不同属性。
    fn stat(&self) -> Result<FileStat, FsError> {
        stat_path(&self.path)
    }

    /// 字符设备不提供目录项。
    fn readdir(&self) -> Result<Option<DirectoryEntry>, FsError> {
        Err(FsError::NotDirectory)
    }

    /// 字符设备不能作为目录枚举起点。
    fn for_each_dirent(
        &self,
        _emit: &mut dyn FnMut(&DirectoryEntry) -> Result<bool, FsError>,
    ) -> Result<(), FsError> {
        Err(FsError::NotDirectory)
    }

    /// 有界复制并丢弃输入，零长度不会访问源地址。
    fn write_from(
        &self,
        source: &mut WriteBuffer<'_>,
        _caller: FileIoContext<'_>,
    ) -> Result<usize, FsError> {
        require_write(self.flags())?;
        discard(source)
    }

    /// 空设备按位置写入仍消费输入，不改变设备位置。
    fn write_at_from(
        &self,
        _offset: u64,
        source: &mut WriteBuffer<'_>,
        caller: FileIoContext<'_>,
    ) -> Result<usize, FsError> {
        self.write_from(source, caller)
    }
}

impl FileLike for ZeroFile {
    /// 零设备允许按位置调用，内容不随位置改变，访问权限仍按原打开模式检查。
    fn positioned_io(&self) -> bool {
        true
    }

    /// 按接收端容量填零；用户故障保留本次已经交付的正前缀。
    fn read_into(
        &self,
        target: &mut ReadBuffer<'_>,
        _caller: FileIoContext<'_>,
    ) -> Result<usize, FsError> {
        require_read(self.flags())?;
        zeros(target)
    }

    /// 零设备内容不受显式位置影响。
    fn read_at_into(
        &self,
        _offset: u64,
        target: &mut ReadBuffer<'_>,
        caller: FileIoContext<'_>,
    ) -> Result<usize, FsError> {
        self.read_into(target, caller)
    }

    /// 保留真实设备路径，不构造匿名或磁盘替身。
    fn path(&self) -> Option<&Path> {
        Some(&self.path)
    }

    /// 本设备没有普通文件映射能力。
    fn mapping_inode(&self) -> Option<&InodeRef> {
        None
    }

    /// 打开状态由原子字段提供共享快照。
    fn flags(&self) -> u32 {
        self.flags.load(Ordering::Relaxed)
    }

    /// 复制描述符观察相同的可变状态位。
    fn set_status_flags(&self, raw: u32) -> Result<(), FsError> {
        set_status_flags(&self.flags, raw)
    }

    /// 零设备没有可修改的文件长度。
    fn truncate(&self, _new_size: u64) -> Result<(), FsError> {
        Err(FsError::InvalidOperation)
    }

    /// 设备不提供 fsync 操作。
    fn sync_range(&self, _range: Range<u64>, _data_only: bool) -> Result<(), FsError> {
        Err(FsError::InvalidOperation)
    }

    /// 零设备与空设备共享归零 lseek 语义。
    fn seek(&self, _from: SeekFrom) -> Result<u64, FsError> {
        Ok(0)
    }

    /// 属性查询消费实际 inode 身份与真实 rdev。
    fn stat(&self) -> Result<FileStat, FsError> {
        stat_path(&self.path)
    }

    /// 零设备不是目录。
    fn readdir(&self) -> Result<Option<DirectoryEntry>, FsError> {
        Err(FsError::NotDirectory)
    }

    /// 零设备不产生目录项。
    fn for_each_dirent(
        &self,
        _emit: &mut dyn FnMut(&DirectoryEntry) -> Result<bool, FsError>,
    ) -> Result<(), FsError> {
        Err(FsError::NotDirectory)
    }

    /// 写入丢弃真实输入，仍执行输入端的访问和前缀协议。
    fn write_from(
        &self,
        source: &mut WriteBuffer<'_>,
        _caller: FileIoContext<'_>,
    ) -> Result<usize, FsError> {
        require_write(self.flags())?;
        discard(source)
    }

    /// 显式偏移不改变写入消费行为。
    fn write_at_from(
        &self,
        _offset: u64,
        source: &mut WriteBuffer<'_>,
        caller: FileIoContext<'_>,
    ) -> Result<usize, FsError> {
        self.write_from(source, caller)
    }
}
