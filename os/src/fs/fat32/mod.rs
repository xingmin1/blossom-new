//! 基于固定上游 fatfs 的 FAT32 卷与挂载根命名空间。
//!
//! `LoopMountLease` 保持绑定有效，`FatIo` 持有同一 Disk 的 `FsClaim`。挂载前实际验证
//! BPB、FSInfo、FAT32 类型及根目录的首个有效项或 EOF；本模块不提供普通文件的 VFS 操作。
//! 所有 fatfs `File`、`Dir` 和迭代器都只在核心睡眠锁内临时存在。
//!
//! 命名空间沿 `rename` 睡眠锁、`namespace` 自旋锁的顺序访问；堆分配与旧弱引用析构在
//! 自旋锁外完成。卷只弱持名字缓存和根 Dentry，根 inode 可以强持卷而不形成引用环。
//! 字段按声明顺序析构，核心及其 `FsClaim` 必须先于最后的 loop 租约释放。

use alloc::sync::{Arc, Weak};

use crate::block::loop_device::LoopMountLease;
use crate::block::{AccessMode, BlockError, BlockView, Disk, FsClaim};
use crate::fs::dentry::{Dentry, DentryCache, DentryRef};
use crate::fs::inode::InodeRef;
use crate::fs::memfs::{allocate_device, FilesystemStat};
use crate::fs::FsError;
use crate::sync::{SleepMutex, SleepMutexGuard, SpinLock};
use fatfs::{FatType, FileSystem, FsOptions};
#[cfg(feature = "ktest")]
use fatfs::{Read, Seek, SeekFrom, Write};

mod io;
use io::{FatIo, IoError};

/// FAT 文件系统的 `statfs` 类型标识，与 Linux `MSDOS_SUPER_MAGIC` 一致。
const FAT_SUPER_MAGIC: u64 = 0x4d44;

/// 通过睡眠锁独占访问的真实 FAT32 核心，也供内核测试使用真实 `FsClaim` 打开。
pub(crate) struct Fat32Core {
    /// 所有上游内部可变状态及临时文件借用均由此锁保护；析构时先释放其中的占用凭证。
    filesystem: SleepMutex<FileSystem<FatIo>>,
    /// 创建核心时确定的占用模式，不允许通过同步或测试入口升级权限。
    mode: AccessMode,
    /// 与 `FatIo` 占用相同的有界块视图，仅增加 Disk 引用而不复制占用凭证。
    view: BlockView,
    /// 从上游验证后的 BPB 取得的 FAT cluster 大小，单位为字节。
    block_size: u32,
}

/// 名字缓存与根目录项的弱登记，由卷的 `namespace` 自旋锁保护。
struct FatNamespace {
    /// 当前仍被 VFS 或挂载对象持有的名字缓存；活缓存的容量不可改变。
    cache: Weak<DentryCache>,
    /// 当前根目录项；卷不强持根，以免根 inode 反向保活卷时形成引用环。
    root: Weak<Dentry>,
}

/// 持有一个 loop 挂载租约的 FAT32 卷，负责真实核心和根命名空间的共同生命周期。
pub struct Fat32Volume {
    /// 必须排在 `_lease` 之前，使上游析构和实际 I/O 完成后才释放 loop 挂载占用。
    core: Fat32Core,
    /// 与内存卷共用唯一分配器取得的文件系统实例身份，不是 loop 的块设备号。
    device: u64,
    /// 串行化名字域创建和目录拓扑观察，供 VFS 沿已有 rename 锁协议使用。
    rename: SleepMutex<()>,
    /// 仅保存当前名字缓存和根目录项的弱引用。
    namespace: SpinLock<FatNamespace>,
    /// 最后释放；其析构可能等待 loop 管理工作，而该工作需要所有 `FsClaim` 已退出。
    _lease: LoopMountLease,
}

impl Fat32Volume {
    /// 在普通可睡眠任务中验证并打开租约对应的 FAT32 卷；失败时先释放候选占用，再释放租约。
    pub fn open(lease: LoopMountLease, mode: AccessMode) -> Result<Arc<Self>, FsError> {
        let view = lease.view();
        let claim = Disk::claim_fs(&view, mode)?;
        let core = Fat32Core::open(claim)?;
        let device = allocate_device()?;
        Arc::try_new(Self {
            core,
            device,
            rename: SleepMutex::new(()),
            namespace: SpinLock::new(FatNamespace {
                cache: Weak::new(),
                root: Weak::new(),
            }),
            _lease: lease,
        })
        .map_err(|_| FsError::NoMemory)
    }

    /// 返回由真实文件系统占用确定的访问模式。
    pub fn mode(&self) -> AccessMode {
        self.core.mode
    }

    /// 返回卷在运行期唯一的 `st_dev` 和 `f_fsid` 身份。
    pub fn device(&self) -> u64 {
        self.device
    }

    /// 返回真实 FAT cluster 的字节数，供根 inode 的块大小属性使用。
    pub(crate) fn block_size(&self) -> u32 {
        self.core.block_size
    }

    /// 在核心睡眠锁内读取真实数据簇和空闲簇统计；FAT 没有独立 inode 配额。
    pub fn statfs(&self) -> Result<FilesystemStat, FsError> {
        let filesystem = self.core.filesystem.lock();
        let stats = filesystem.stats().map_err(fat_error)?;
        Ok(FilesystemStat {
            fs_type: FAT_SUPER_MAGIC,
            block_size: u64::from(stats.cluster_size()),
            blocks: u64::from(stats.total_clusters()),
            free: u64::from(stats.free_clusters()),
            available: u64::from(stats.free_clusters()),
            files: 0,
            freefiles: 0,
            device: self.device(),
        })
    }

    /// 借用本卷正在占用的真实块视图，不创建第二个 FAT 核心。
    pub fn view(&self) -> &BlockView {
        &self.core.view
    }

    /// 同步可写核心及底层存储；只读卷没有主动写回或刷盘义务。
    pub(crate) fn sync(&self) -> Result<(), FsError> {
        self.core.sync()
    }

    /// 完成可报错的卸载同步；调用方须已排除新访问，失败时仍持有原卷与租约。
    pub(crate) fn finish_unmount(&self) -> Result<(), FsError> {
        self.core.finish_unmount()
    }

    /// 取得已有的卷级 rename 睡眠锁，禁止在自旋锁或中断上下文调用。
    pub(crate) fn lock_rename(&self) -> SleepMutexGuard<'_, ()> {
        self.rename.lock()
    }

    /// 取得或创建同一卷的名字缓存和真实根 inode；只在弱登记失效后重新创建根。
    pub(crate) fn dentry_namespace(
        self: &Arc<Self>,
        capacity: usize,
    ) -> Result<(Arc<DentryCache>, DentryRef), FsError> {
        if capacity == 0 {
            return Err(FsError::InvalidOperation);
        }
        let _rename = self.rename.lock();
        let (cache, root) = {
            let namespace = self.namespace.lock();
            (
                namespace.cache.upgrade(),
                DentryRef::upgrade(&namespace.root),
            )
        };
        if let Some(cache) = &cache {
            if cache.capacity() != capacity {
                return Err(FsError::InvalidOperation);
            }
        }

        let cache = match cache {
            Some(cache) => cache,
            None => DentryCache::try_new(capacity)?,
        };
        let root = match root {
            Some(root) => root,
            None => DentryRef::try_root(InodeRef::try_fat_root(self)?)?,
        };
        let published = FatNamespace {
            cache: Arc::downgrade(&cache),
            root: root.downgrade(),
        };
        let previous = core::mem::replace(&mut *self.namespace.lock(), published);
        drop(previous);
        Ok((cache, root))
    }
}

impl Fat32Core {
    /// 消费一个真实占用，调用上游初始化并在核心睡眠锁内读取根目录首项或实际 EOF。
    fn open(claim: FsClaim) -> Result<Self, FsError> {
        let mode = claim.mode();
        let view = claim.view().clone();
        let filesystem =
            FileSystem::new(FatIo::new(claim)?, FsOptions::new()).map_err(fat_error)?;
        let block_size = filesystem.cluster_size();
        let core = Self {
            filesystem: SleepMutex::new(filesystem),
            mode,
            view,
            block_size,
        };
        {
            let filesystem = core.filesystem.lock();
            if filesystem.fat_type() != FatType::Fat32 {
                return Err(FsError::Unsupported);
            }
            let _ = filesystem
                .root_dir()
                .iter()
                .next()
                .transpose()
                .map_err(fat_error)?;
        }
        Ok(core)
    }

    /// 在同一核心锁内同步上游簿记和实际存储；只读占用不提交写入或刷盘。
    pub(crate) fn sync(&self) -> Result<(), FsError> {
        if self.mode == AccessMode::ReadOnly {
            return Ok(());
        }
        self.filesystem.lock().flush().map_err(fat_error)
    }

    /// 保留对象地完成卸载簿记及真实 Flush，供 VFS 和冷重开测试共同使用。
    pub(crate) fn finish_unmount(&self) -> Result<(), FsError> {
        if self.mode == AccessMode::ReadOnly {
            return Ok(());
        }
        self.filesystem.lock().finish_unmount().map_err(fat_error)
    }

    /// 用真实盘的占用初始化同一核心；调用方须保证该视图没有另一个存活的 FAT 核心。
    #[cfg(feature = "ktest")]
    pub(crate) fn ktest_open(claim: FsClaim) -> Result<Self, FsError> {
        Self::open(claim)
    }

    /// 实际读取根目录相对路径，支持 `HELLO.TXT` 和子目录长文件名；返回已交付字节数。
    ///
    /// 缓冲区大于预期文件长度时，返回的短读包含实际 EOF；不建立 VFS 文件节点。
    #[cfg(feature = "ktest")]
    pub(crate) fn ktest_read_file(&self, path: &str, target: &mut [u8]) -> Result<usize, FsError> {
        let filesystem = self.filesystem.lock();
        let root = filesystem.root_dir();
        let mut file = root.open_file(path).map_err(fat_error)?;
        let mut read = 0;
        while read < target.len() {
            let count = file.read(&mut target[read..]).map_err(fat_error)?;
            if count == 0 {
                break;
            }
            read += count;
        }
        if self.mode == AccessMode::ReadWrite {
            file.flush().map_err(fat_error)?;
        }
        Ok(read)
    }

    /// 实际创建或打开文件，写入完整数据后截断到指定长度，并同步文件、卷及底层存储。
    ///
    /// 截断长度不得超过本次写入长度。冷重开验证须随后完成卸载并释放核心，再取得新占用打开。
    #[cfg(feature = "ktest")]
    pub(crate) fn ktest_write_truncate(
        &self,
        path: &str,
        source: &[u8],
        length: u64,
    ) -> Result<(), FsError> {
        if self.mode == AccessMode::ReadOnly {
            return Err(FsError::ReadOnly);
        }
        if u64::from(u32::MAX) < source.len() as u64 {
            return Err(FsError::FileTooLarge);
        }
        if (source.len() as u64) < length {
            return Err(FsError::InvalidOperation);
        }
        let filesystem = self.filesystem.lock();
        {
            let root = filesystem.root_dir();
            let mut file = root.create_file(path).map_err(fat_error)?;
            file.write_all(source).map_err(fat_error)?;
            file.seek(SeekFrom::Start(length)).map_err(fat_error)?;
            file.truncate().map_err(fat_error)?;
            file.flush().map_err(fat_error)?;
        }
        filesystem.flush().map_err(fat_error)
    }
}

/// 保留块层真实错误，并将上游目录、容量和格式错误转换为已有的文件系统错误。
fn fat_error(error: fatfs::Error<IoError>) -> FsError {
    match error {
        fatfs::Error::Io(IoError::Block(error)) => error.into(),
        fatfs::Error::Io(IoError::UnexpectedEof)
        | fatfs::Error::UnexpectedEof
        | fatfs::Error::CorruptedFileSystem => FsError::Corrupt,
        fatfs::Error::Io(IoError::WriteZero) | fatfs::Error::WriteZero => {
            FsError::Io(BlockError::ShortIo)
        }
        fatfs::Error::Io(IoError::InvalidSeek) | fatfs::Error::InvalidInput => {
            FsError::InvalidOperation
        }
        fatfs::Error::NotFound => FsError::NotFound,
        fatfs::Error::AlreadyExists => FsError::Exists,
        fatfs::Error::DirectoryIsNotEmpty => FsError::NotEmpty,
        fatfs::Error::NotEnoughSpace => FsError::NoSpace,
        fatfs::Error::InvalidFileNameLength | fatfs::Error::UnsupportedFileNameCharacter => {
            FsError::InvalidName
        }
        _ => FsError::Unsupported,
    }
}
