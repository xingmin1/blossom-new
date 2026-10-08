//! 每次打开拥有独立位置，描述符复制只共享同一个具体文件对象。

use alloc::sync::Arc;
use core::ops::Range;
use core::sync::atomic::{AtomicU32, Ordering};

use crate::arch;
use crate::block::{AccessMode, BlockError};
use crate::fs::{writeback, DirectoryEntry, FsError, InodeKind, InodeTime, Path, WriteAccess};
use crate::mm::space::{Mm, VmError};
use crate::mm::FolioRef;
use crate::sync::{SleepMutex, SpinLock};

/// 共享一个打开实例及其位置；最后引用释放该实例持有的路径。
pub type FileRef = Arc<dyn FileLike>;

const O_ACCMODE: u32 = 0o3;
const O_WRONLY: u32 = 0o1;
const O_CREAT: u32 = 0o100;
const O_TRUNC: u32 = 0o1000;
const O_APPEND: u32 = 0o2000;
const O_NONBLOCK: u32 = 0o4000;
pub(crate) const O_DSYNC: u32 = 0x1000;
pub(crate) const O_SYNC: u32 = 0x101000;
pub(crate) const O_DIRECTORY: u32 = 0o200000;
pub(crate) const O_NOFOLLOW: u32 = 0o400000;
const O_CLOEXEC: u32 = 0o2000000;
const MUTABLE_FLAGS: u32 = O_APPEND | O_NONBLOCK;
const OPEN_FLAGS: u32 = MUTABLE_FLAGS | O_SYNC | 0o100000 | O_DIRECTORY | O_NOFOLLOW | 0o400;

/// 同一文件页循环的两个接收端，实际交付量独立于请求长度。
pub struct ReadBuffer<'a> {
    target: ReadTarget<'a>,
    written: usize,
}

/// 内核切片遵守独占借用；用户地址只由绑定的 Mm 执行逐页原始复制。
enum ReadTarget<'a> {
    Kernel(&'a mut [u8]),
    User {
        mm: &'a Mm,
        address: usize,
        length: usize,
    },
}

impl<'a> ReadBuffer<'a> {
    /// 将内核消费者绑定到同一文件页交付循环。
    pub fn kernel(bytes: &'a mut [u8]) -> Self {
        Self {
            target: ReadTarget::Kernel(bytes),
            written: 0,
        }
    }

    /// 一次验证非零用户范围；零长请求不要求地址可访问。
    pub fn user(mm: &'a Mm, address: usize, length: usize) -> Result<Self, FsError> {
        if length != 0 {
            let end = address.checked_add(length).ok_or(FsError::BadAddress)?;
            if arch::USER_ADDRESS_END < end {
                return Err(FsError::BadAddress);
            }
        }
        Ok(Self {
            target: ReadTarget::User {
                mm,
                address,
                length,
            },
            written: 0,
        })
    }

    /// 返回本次接收端的完整容量，不包含已交付进度。
    pub fn len(&self) -> usize {
        match &self.target {
            ReadTarget::Kernel(bytes) => bytes.len(),
            ReadTarget::User { length, .. } => *length,
        }
    }

    /// 观察已经真正写入接收端的前缀，故障不会撤回这段进度。
    pub fn copied(&self) -> usize {
        self.written
    }

    /// 从独占内核片段交付字节；可能处理用户 fault，调用时不得持源文件页内容锁。
    pub fn push(&mut self, bytes: &[u8]) -> Result<usize, FsError> {
        let count = bytes.len().min(self.len() - self.written);
        let bytes = &bytes[..count];
        match &mut self.target {
            ReadTarget::Kernel(dst) => {
                dst[self.written..self.written + count].copy_from_slice(bytes);
                self.written += count;
                Ok(count)
            }
            ReadTarget::User { mm, address, .. } => {
                let progress = mm.copyout(*address + self.written, bytes);
                self.written += progress.copied;
                match progress.error {
                    None => Ok(progress.copied),
                    Some(_) => Err(FsError::BadAddress),
                }
            }
        }
    }
}

/// 写入端只保存有限输入准备进度，文件实际接受量由 inode 单独提交。
pub struct WriteBuffer<'a> {
    source: WriteSource<'a>,
    prepared: usize,
    error: Option<FsError>,
}

/// 用户字节只能由指定 Mm 复制到独占片段，不建立用户内容的 Rust 借用。
enum WriteSource<'a> {
    Kernel(&'a [u8]),
    User {
        mm: &'a Mm,
        address: usize,
        length: usize,
    },
}

impl<'a> WriteBuffer<'a> {
    /// 将已初始化的内核字节绑定到正式 inode 写循环。
    pub fn kernel(bytes: &'a [u8]) -> Self {
        Self {
            source: WriteSource::Kernel(bytes),
            prepared: 0,
            error: None,
        }
    }

    /// 验证非零用户范围并绑定真实 Mm；零长请求不访问地址。
    pub fn user(mm: &'a Mm, address: usize, length: usize) -> Result<Self, FsError> {
        if length != 0 {
            let end = address.checked_add(length).ok_or(FsError::BadAddress)?;
            if arch::USER_ADDRESS_END < end {
                return Err(FsError::BadAddress);
            }
        }
        Ok(Self {
            source: WriteSource::User {
                mm,
                address,
                length,
            },
            prepared: 0,
            error: None,
        })
    }

    /// 返回输入的完整长度，不把准备片段当成已接受的文件字节。
    pub fn len(&self) -> usize {
        match &self.source {
            WriteSource::Kernel(bytes) => bytes.len(),
            WriteSource::User { length, .. } => *length,
        }
    }

    /// 返回已取得的输入前缀，仅供剩余输入计算。
    pub fn copied(&self) -> usize {
        self.prepared
    }

    /// 只准备目的片段容量内的输入，调用时不能持目的 folio、块根写锁或 PTE 锁。
    /// 部分 copyin 先交出可提交前缀，首个错误留待下一次调用，不能吞掉已有字节。
    pub fn pull(&mut self, dst: &mut [u8]) -> Result<usize, FsError> {
        if let Some(error) = self.error {
            return Err(error);
        }
        let count = dst.len().min(self.len() - self.prepared);
        if count == 0 {
            return Ok(0);
        }
        let copied = match &self.source {
            WriteSource::Kernel(bytes) => {
                dst[..count].copy_from_slice(&bytes[self.prepared..self.prepared + count]);
                count
            }
            WriteSource::User { mm, address, .. } => {
                let progress = mm.copyin(&mut dst[..count], *address + self.prepared);
                self.error = progress.error.map(|error| match error {
                    VmError::NoMemory => FsError::NoMemory,
                    VmError::File(error) => error,
                    _ => FsError::BadAddress,
                });
                progress.copied
            }
        };
        self.prepared += copied;
        if copied == 0 {
            if let Some(error) = self.error {
                return Err(error);
            }
        }
        Ok(copied)
    }
}

/// 可表示为有符号64位的非负文件位置；相对位移失败时保留原位置。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SeekFrom {
    Start(u64),
    Current(i64),
    End(i64),
}

/// 真实 inode 当前属性与卷几何的小快照，不包含块映射根。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FileStat {
    pub inode: u64,
    pub kind: InodeKind,
    pub mode: u16,
    pub size: u64,
    pub uid: u32,
    pub gid: u32,
    pub links: u32,
    /// 已分配空间按 512 字节计数。
    pub blocks: u64,
    pub device: u64,
    pub block_size: u32,
    pub atime: InodeTime,
    pub mtime: InodeTime,
    pub ctime: InodeTime,
}

/// 普通可睡眠任务使用的文件操作；调用者须在描述符表锁外持有实例引用。
pub trait FileLike: Send + Sync {
    /// 内核顺序读取使用与用户读取相同的页交付和位置提交协议。
    fn read(&self, dst: &mut [u8]) -> Result<usize, FsError> {
        self.read_into(&mut ReadBuffer::kernel(dst))
    }

    /// 内核按位置读取使用同一页循环，不取得共享位置锁。
    fn read_at(&self, offset: u64, dst: &mut [u8]) -> Result<usize, FsError> {
        self.read_at_into(offset, &mut ReadBuffer::kernel(dst))
    }

    /// 从共享位置向具体接收端交付，只提交已交付的正前缀。
    fn read_into(&self, target: &mut ReadBuffer<'_>) -> Result<usize, FsError>;

    /// 按指定位置向接收端交付，不获取或修改共享位置。
    fn read_at_into(&self, offset: u64, target: &mut ReadBuffer<'_>) -> Result<usize, FsError>;

    /// 借用保活挂载与 inode 的完整路径，dirfd 不暴露裸身份。
    fn path(&self) -> &Path;

    /// 读取此 OFD 的访问模式和状态，CLOEXEC 只在描述符槽保存。
    fn flags(&self) -> u32;

    /// 只改变支持的可变状态位，不修改访问模式与打开时路径约束。
    fn set_status_flags(&self, raw: u32) -> Result<(), FsError>;

    /// 同步半开字节范围及其必要元数据和设备缓存，并推进原 OFD 的错误观察游标。
    /// 调用者不得持描述符表、位置、inode 写锁或 mmap/PTE 保护；本操作可能等待真实 I/O。
    fn sync_range(&self, range: Range<u64>, data_only: bool) -> Result<(), FsError>;

    /// 在普通任务中等待唯一 inode 的缓存页，不获取位置锁；EOF 与私有尾页由映射调用者处理。
    fn file_page(&self, index: u64) -> Result<FolioRef, FsError>;

    /// 更新共享字节位置，负值或溢出不修改原位置。
    fn seek(&self, from: SeekFrom) -> Result<u64, FsError>;

    /// 读取 canonical inode 的属性快照，不获取位置锁。
    fn stat(&self) -> Result<FileStat, FsError>;

    /// 返回下一项及其原始名字，成功取得项后才推进目录位置。
    fn readdir(&self) -> Result<Option<DirectoryEntry>, FsError>;

    /// 回调完整交付一项并返回 true 后提交其 cookie；false 或错误保留该项位置。
    fn for_each_dirent(
        &self,
        emit: &mut dyn FnMut(&DirectoryEntry) -> Result<bool, FsError>,
    ) -> Result<(), FsError>;

    /// 内核顺序写与用户写共用同一输入准备及文件提交协议。
    fn write(&self, src: &[u8]) -> Result<usize, FsError> {
        self.write_from(&mut WriteBuffer::kernel(src))
    }

    /// 内核按位置写不取得共享位置锁，追加状态仍由 inode 仲裁。
    fn write_at(&self, offset: u64, src: &[u8]) -> Result<usize, FsError> {
        self.write_at_from(offset, &mut WriteBuffer::kernel(src))
    }

    /// 持共享位置直到实际写结束，只提交 inode 返回的起点和接受前缀。
    fn write_from(&self, source: &mut WriteBuffer<'_>) -> Result<usize, FsError>;

    /// 按明确位置写入，不取得或修改共享位置。
    fn write_at_from(&self, offset: u64, source: &mut WriteBuffer<'_>) -> Result<usize, FsError>;
}

/// 普通文件的打开实例，路径保活挂载和唯一 inode 的文件页索引。
pub struct InodeFile {
    path: Path,
    cursor: SleepMutex<u64>,
    flags: AtomicU32,
    write_access: Option<WriteAccess>,
    /// 同一 OFD 的数据与共同元数据错误样本，短锁只覆盖检查与推进。
    wb_errors: SpinLock<(u32, u32)>,
}

/// 目录的打开实例，枚举位置使用后端已有的字节记录边界。
pub struct DirectoryFile {
    path: Path,
    cursor: SleepMutex<u64>,
    flags: AtomicU32,
    wb_error: SpinLock<u32>,
}

/// 内核默认只读打开与用户带标志打开共用唯一构造。
pub fn open_path(path: Path) -> Result<FileRef, FsError> {
    open_path_with_flags(path, 0)
}

/// 按真实访问模式建立 OFD，同步状态固定于打开实例，修改名字与截断仍明确拒绝。
pub fn open_path_with_flags(path: Path, flags: u32) -> Result<FileRef, FsError> {
    let access = flags & O_ACCMODE;
    if access == O_ACCMODE {
        return Err(FsError::InvalidOperation);
    }
    if access != 0 || flags & (O_CREAT | O_TRUNC) != 0 {
        path.check_writable()?;
    }
    let flags = flags & !O_CLOEXEC;
    if flags & !(OPEN_FLAGS | O_ACCMODE | O_CREAT | O_TRUNC) != 0 {
        return Err(FsError::Unsupported);
    }
    // 独立的完整同步位同样隐含数据同步位，写后路径只需据 O_DSYNC 判断是否同步。
    let flags = if flags & (O_SYNC & !O_DSYNC) != 0 {
        flags | O_DSYNC
    } else {
        flags
    };
    let inode = path.inode()?;
    let kind = inode.metadata_snapshot()?.kind;
    if flags & O_DIRECTORY != 0 && kind != InodeKind::Directory {
        return Err(FsError::NotDirectory);
    }
    match kind {
        InodeKind::Regular => {
            let write_access = if access != 0 || flags & O_TRUNC != 0 {
                Some(inode.acquire_write()?)
            } else {
                None
            };
            // 截断先经过写排斥检查，不能将活映像的 ETXTBSY 掩盖成未实现。
            if flags & (O_CREAT | O_TRUNC) != 0 {
                return Err(FsError::Unsupported);
            }
            let wb_errors = (
                inode.mapping().sample_wb_error(),
                inode.filesystem().metadata_cache().sample_wb_error(),
            );
            Ok(Arc::try_new(InodeFile {
                path,
                cursor: SleepMutex::new(0),
                flags: AtomicU32::new(flags),
                write_access,
                wb_errors: SpinLock::new(wb_errors),
            })
            .map_err(|_| FsError::NoMemory)?)
        }
        InodeKind::Directory => {
            if access != 0 || flags & O_TRUNC != 0 {
                return Err(FsError::IsDirectory);
            }
            if flags & O_CREAT != 0 {
                return Err(FsError::Unsupported);
            }
            let wb_error = inode.filesystem().metadata_cache().sample_wb_error();
            Ok(Arc::try_new(DirectoryFile {
                path,
                cursor: SleepMutex::new(0),
                flags: AtomicU32::new(flags),
                wb_error: SpinLock::new(wb_error),
            })
            .map_err(|_| FsError::NoMemory)?)
        }
        InodeKind::Symlink => Err(FsError::TooManySymlinks),
        InodeKind::Other => Err(FsError::Unsupported),
    }
}

impl FileLike for InodeFile {
    /// 位置锁跨实际交付保持独占，用户故障的正前缀仍按真实长度推进。
    fn read_into(&self, target: &mut ReadBuffer<'_>) -> Result<usize, FsError> {
        require_read(self.flags())?;
        let mut cursor = self.cursor.lock();
        let bytes = self.path.inode()?.read_into(*cursor, target)?;
        // inode 限制交付不超过 size-cursor，EOF 与零长返回零。
        *cursor += bytes as u64;
        Ok(bytes)
    }

    /// 按位置读取唯一 inode 的文件页，不参与共享位置的串行化。
    fn read_at_into(&self, offset: u64, target: &mut ReadBuffer<'_>) -> Result<usize, FsError> {
        require_read(self.flags())?;
        self.path.inode()?.read_into(offset, target)
    }

    /// 由完整路径保活挂载，调用者可克隆此路径作为 dirfd 起点。
    fn path(&self) -> &Path {
        &self.path
    }

    /// OFD 状态由原子快照读取，不获取位置锁。
    fn flags(&self) -> u32 {
        self.flags.load(Ordering::Relaxed)
    }

    /// 更新同一 OFD 的可变状态，dup 的所有引用观察相同状态。
    fn set_status_flags(&self, raw: u32) -> Result<(), FsError> {
        set_status_flags(&self.flags, raw)
    }

    /// 在所有写入与位置保护之外同步实际范围，直接错误优先于后续阶段，样本仍全部推进。
    fn sync_range(&self, range: Range<u64>, _data_only: bool) -> Result<(), FsError> {
        if range.end < range.start {
            return Err(FsError::InvalidOperation);
        }
        let inode = self.path.inode()?;
        let volume = inode.filesystem();
        // 只读卷没有可提交的写义务；只读 OFD 或挂载视图仍须同步 RW 卷上的共同内容。
        let writable = volume.mode() == AccessMode::ReadWrite;
        let data = if writable {
            writeback::sync_inode_range(&inode, range)
        } else {
            Ok(())
        };
        // 共同元数据包含当前 inode 记录、块树及分配依赖；当前不存在新名字的父目录义务。
        // fdatasync 可写出更多共同元数据，但不能绕过使数据可寻址的记录和设备 FLUSH。
        // 未确认设备停止时保持原在途拥有，禁止继续提交元数据或 FLUSH。
        let metadata = if writable && !matches!(data, Err(FsError::Io(BlockError::StopUnconfirmed)))
        {
            writeback::sync_metadata(volume)
        } else {
            Ok(())
        };
        let mut samples = self.wb_errors.lock();
        let data_error = inode.mapping().check_and_advance_wb_error(&mut samples.0);
        let metadata_error = inode
            .filesystem()
            .metadata_cache()
            .check_and_advance_wb_error(&mut samples.1);
        data.and(data_error).and(metadata).and(metadata_error)
    }

    /// 从完整路径保活的 inode 取得同源缓存页，不读取或修改打开实例的位置。
    fn file_page(&self, index: u64) -> Result<FolioRef, FsError> {
        require_read(self.flags())?;
        self.path.inode()?.file_page(index)
    }

    /// 与顺序读取共用位置锁，允许定位到文件末尾之后。
    fn seek(&self, from: SeekFrom) -> Result<u64, FsError> {
        seek_path(&self.path, &self.cursor, from)
    }

    /// 只复制属性字段，不复制完整元数据或文件内容。
    fn stat(&self) -> Result<FileStat, FsError> {
        stat_path(&self.path)
    }

    /// 普通文件不能作为目录枚举。
    fn readdir(&self) -> Result<Option<DirectoryEntry>, FsError> {
        Err(FsError::NotDirectory)
    }

    /// 普通文件不提供目录流，不调用消费者。
    fn for_each_dirent(
        &self,
        _emit: &mut dyn FnMut(&DirectoryEntry) -> Result<bool, FsError>,
    ) -> Result<(), FsError> {
        Err(FsError::NotDirectory)
    }

    /// 接受前缀先提交共享位置并释放写入保护，再执行打开时要求的同步；同步错误不回滚前缀。
    fn write_from(&self, source: &mut WriteBuffer<'_>) -> Result<usize, FsError> {
        let flags = self.flags();
        require_write(flags)?;
        self.path.check_writable()?;
        let mut cursor = self.cursor.lock();
        check_write_range(*cursor, source.len() - source.copied())?;
        let inode = self.path.inode()?;
        let access = self
            .write_access
            .as_ref()
            .expect("writable OFD has no write access");
        let (start, bytes) = access.write_from(
            inode.filesystem().claim(),
            *cursor,
            flags & O_APPEND != 0,
            source,
        )?;
        if 0 < bytes {
            *cursor = start
                .checked_add(bytes as u64)
                .expect("inode write position overflow");
        }
        drop(cursor);
        sync_written(self, flags, start, bytes)?;
        Ok(bytes)
    }

    /// pwrite 不改变共享位置，追加起点由 inode 仲裁，接受前缀在写锁外完成同步。
    fn write_at_from(&self, offset: u64, source: &mut WriteBuffer<'_>) -> Result<usize, FsError> {
        let flags = self.flags();
        require_write(flags)?;
        self.path.check_writable()?;
        check_write_range(offset, source.len() - source.copied())?;
        let inode = self.path.inode()?;
        let access = self
            .write_access
            .as_ref()
            .expect("writable OFD has no write access");
        let (start, bytes) = access.write_from(
            inode.filesystem().claim(),
            offset,
            flags & O_APPEND != 0,
            source,
        )?;
        sync_written(self, flags, start, bytes)?;
        Ok(bytes)
    }
}

/// 仅对真实正接受量执行同步，O_SYNC 同时包含 O_DSYNC，失败返回原错误而非假正前缀。
fn sync_written(file: &InodeFile, flags: u32, start: u64, bytes: usize) -> Result<(), FsError> {
    if bytes == 0 || flags & O_DSYNC == 0 {
        return Ok(());
    }
    let end = start
        .checked_add(bytes as u64)
        .expect("accepted write range overflow");
    file.sync_range(start..end, flags & O_SYNC != O_SYNC)
}

/// 调用者位置和本次输入范围均须可表示为非负 loff_t，追加也不能跳过 ABI 范围检查。
fn check_write_range(offset: u64, length: usize) -> Result<(), FsError> {
    let end = offset
        .checked_add(length as u64)
        .ok_or(FsError::InvalidOperation)?;
    if (i64::MAX as u64) < end {
        return Err(FsError::InvalidOperation);
    }
    Ok(())
}

impl FileLike for DirectoryFile {
    /// 目录字节由目录项操作消费，不能按普通文件读取。
    fn read_into(&self, _target: &mut ReadBuffer<'_>) -> Result<usize, FsError> {
        Err(FsError::IsDirectory)
    }

    /// 指定位置的普通字节读取同样拒绝目录。
    fn read_at_into(&self, _offset: u64, _target: &mut ReadBuffer<'_>) -> Result<usize, FsError> {
        Err(FsError::IsDirectory)
    }

    /// 目录实例保活完整路径，可作为相对路径起点。
    fn path(&self) -> &Path {
        &self.path
    }

    /// 读取此目录 OFD 状态，不获取枚举位置锁。
    fn flags(&self) -> u32 {
        self.flags.load(Ordering::Relaxed)
    }

    /// 目录状态与描述符属性分离，不改变枚举位置。
    fn set_status_flags(&self, raw: u32) -> Result<(), FsError> {
        set_status_flags(&self.flags, raw)
    }

    /// 目录同步沿其真实共同元数据与 FLUSH，不取得枚举位置锁或虚构名字创建依赖。
    fn sync_range(&self, range: Range<u64>, _data_only: bool) -> Result<(), FsError> {
        if range.end < range.start {
            return Err(FsError::InvalidOperation);
        }
        let inode = self.path.inode()?;
        let volume = inode.filesystem();
        let result = if volume.mode() == AccessMode::ReadWrite {
            writeback::sync_metadata(volume)
        } else {
            Ok(())
        };
        let error = inode
            .filesystem()
            .metadata_cache()
            .check_and_advance_wb_error(&mut self.wb_error.lock());
        result.and(error)
    }

    /// 目录不能提供普通文件映射页，不建立缓存页或改变枚举位置。
    fn file_page(&self, _index: u64) -> Result<FolioRef, FsError> {
        Err(FsError::IsDirectory)
    }

    /// 设置目录字节位置，后续枚举由后端检查该位置的记录边界。
    fn seek(&self, from: SeekFrom) -> Result<u64, FsError> {
        seek_path(&self.path, &self.cursor, from)
    }

    /// 目录属性直接来自真实 inode，不获取枚举位置锁。
    fn stat(&self) -> Result<FileStat, FsError> {
        stat_path(&self.path)
    }

    /// 内核单项枚举保留原拥有语义，与流式枚举共用完整项提交。
    fn readdir(&self) -> Result<Option<DirectoryEntry>, FsError> {
        let mut cursor = self.cursor.lock();
        let entry = self.path.inode()?.dir_next(*cursor)?;
        if let Some(entry) = &entry {
            commit_dirent(&mut cursor, entry);
        }
        Ok(entry)
    }

    /// 锁内串行真实目录流，消费者在完整记录交付后才授权推进 cookie。
    fn for_each_dirent(
        &self,
        emit: &mut dyn FnMut(&DirectoryEntry) -> Result<bool, FsError>,
    ) -> Result<(), FsError> {
        let mut cursor = self.cursor.lock();
        let inode = self.path.inode()?;
        while let Some(entry) = inode.dir_next(*cursor)? {
            if !emit(&entry)? {
                break;
            }
            commit_dirent(&mut cursor, &entry);
        }
        Ok(())
    }

    /// 目录不接受字节写入，访问模式检查与挂载能力检查保持各自错误。
    fn write_from(&self, _source: &mut WriteBuffer<'_>) -> Result<usize, FsError> {
        require_write(self.flags())?;
        self.path.check_writable()?;
        Err(FsError::IsDirectory)
    }

    /// 按位置写同样拒绝目录，不获取枚举位置锁。
    fn write_at_from(&self, _offset: u64, source: &mut WriteBuffer<'_>) -> Result<usize, FsError> {
        self.write_from(source)
    }
}

/// 写模式 OFD 不授予读取或文件映射能力，空请求同样核访问模式。
fn require_read(flags: u32) -> Result<(), FsError> {
    if flags & O_ACCMODE == O_WRONLY {
        Err(FsError::BadFd)
    } else {
        Ok(())
    }
}

/// 只读 OFD 不授予写能力，挂载检查不能替代打开时访问模式。
fn require_write(flags: u32) -> Result<(), FsError> {
    if flags & O_ACCMODE == 0 {
        Err(FsError::BadFd)
    } else {
        Ok(())
    }
}

/// 同一原子更新保留全部不可变打开位，只提交支持的可变状态位。
fn set_status_flags(flags: &AtomicU32, raw: u32) -> Result<(), FsError> {
    if raw & !(OPEN_FLAGS | O_ACCMODE | O_CLOEXEC) != 0 {
        return Err(FsError::Unsupported);
    }
    let _ = flags.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |old| {
        Some((old & !MUTABLE_FLAGS) | (raw & MUTABLE_FLAGS))
    });
    Ok(())
}

/// 已完整交付目录项的拥有值或字节后，提交后端提供的下一记录边界。
fn commit_dirent(cursor: &mut u64, entry: &DirectoryEntry) {
    *cursor = entry.next_offset;
}

/// 同一位置锁内计算并校验非负loff_t，再提交共享位置，失败不改变游标。
fn seek_path(path: &Path, cursor: &SleepMutex<u64>, from: SeekFrom) -> Result<u64, FsError> {
    let mut cursor = cursor.lock();
    let position = match from {
        SeekFrom::Start(position) => Some(position),
        SeekFrom::Current(delta) => cursor.checked_add_signed(delta),
        SeekFrom::End(delta) => path
            .inode()?
            .metadata_snapshot()?
            .size
            .checked_add_signed(delta),
    }
    .ok_or(FsError::InvalidOperation)?;
    if (i64::MAX as u64) < position {
        return Err(FsError::InvalidOperation);
    }
    *cursor = position;
    Ok(position)
}

/// 从 canonical inode 一次读取当前属性快照，未打开的符号链接同样能取得属性。
pub fn stat_path(path: &Path) -> Result<FileStat, FsError> {
    let inode = path.inode()?;
    let data = inode.metadata_snapshot()?;
    if data.kind == InodeKind::Other {
        return Err(FsError::Unsupported);
    }
    let volume = inode.filesystem();
    Ok(FileStat {
        inode: inode.number(),
        kind: data.kind,
        mode: data.mode,
        size: data.size,
        uid: data.uid,
        gid: data.gid,
        links: data.links,
        blocks: data.blocks,
        device: volume.device(),
        block_size: volume.geometry().block_size as u32,
        atime: data.atime,
        mtime: data.mtime,
        ctime: data.ctime,
    })
}
