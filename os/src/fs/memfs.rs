//! 内存文件系统保存真实 inode 和目录内容，名字缓存只保存可退休的查找结果。

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use spin::Once;

use crate::block::AccessMode;
use crate::fs::dentry::{Dentry, DentryCache, DentryRef};
use crate::fs::ext4::Ext4Volume;
use crate::fs::inode::{
    Inode, InodeCache, InodeMetadata, InodeRef, MemoryContent, MemoryDirectory,
};
use crate::fs::types::InodeKind;
use crate::fs::FsError;
use crate::sync::{SleepMutex, SleepMutexGuard, SpinLock};

const INODE_LIMIT: usize = 128;
const PAGE_LIMIT: usize = 4096;
static NEXT_DEVICE: AtomicU64 = AtomicU64::new(1 << 32);

/// 挂载强持真实后端实例，磁盘身份与内存身份各自比较。
#[derive(Clone)]
pub enum Filesystem {
    Ext4(Arc<Ext4Volume>),
    Memory(Arc<MemoryFs>),
}

impl Filesystem {
    /// 返回实际实例的设备身份，内存实例不借用磁盘号。
    pub fn device(&self) -> u64 {
        match self {
            Self::Ext4(fs) => fs.device(),
            Self::Memory(fs) => fs.device(),
        }
    }

    /// 返回后端的名字与普通文件修改权限。
    pub fn mode(&self) -> AccessMode {
        match self {
            Self::Ext4(fs) => fs.mode(),
            Self::Memory(fs) => fs.mode(),
        }
    }

    /// 内存内容已经是权威状态，没有磁盘回写或 FLUSH 义务。
    pub fn sync(&self) -> Result<(), FsError> {
        match self {
            Self::Ext4(fs) => fs.sync(),
            Self::Memory(_) => Ok(()),
        }
    }

    /// 比较同一类实际后端的 Arc 身份。
    pub fn ptr_eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Ext4(a), Self::Ext4(b)) => Arc::ptr_eq(a, b),
            (Self::Memory(a), Self::Memory(b)) => Arc::ptr_eq(a, b),
            _ => false,
        }
    }

    /// 按后端取得共同名字域，内存节点不登记磁盘 canonical 域。
    pub(crate) fn dentry_namespace(
        &self,
        inodes: &Arc<InodeCache>,
        capacity: usize,
    ) -> Result<(Arc<DentryCache>, DentryRef), FsError> {
        match self {
            Self::Ext4(fs) => inodes.dentry_namespace(fs, capacity),
            Self::Memory(fs) => fs.dentry_namespace(capacity),
        }
    }

    /// rename 的拓扑验证和多目录提交在同一实例睡眠互斥下执行。
    pub(crate) fn lock_rename(&self) -> SleepMutexGuard<'_, ()> {
        match self {
            Self::Ext4(fs) => fs.lock_rename(),
            Self::Memory(fs) => fs.rename.lock(),
        }
    }
}

/// 内存后端种类固定，不提供运行时插件登记。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MemoryFsKind {
    Tmpfs,
    Devfs,
    Procfs,
}

/// 特殊 inode 只标识实际设备或动态快照来源，字节操作由具体 OFD 提供。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MemorySpecial {
    Null,
    Zero,
    TtySerial,
    Console,
    ControllingTty,
    Mounts,
    Meminfo,
    Uptime,
}

/// 根与权威目录项持有 inode，inode 只弱指向本实例；外部引用保活本实例。
pub struct MemoryFs {
    kind: MemoryFsKind,
    device: u64,
    root: Once<Arc<Inode>>,
    rename: SleepMutex<()>,
    allocation: SleepMutex<()>,
    index: SpinLock<MemoryIndex>,
}

struct MemoryIndex {
    inodes: Vec<MemorySlot>,
    next_number: u64,
    pages: usize,
    namespace: Weak<DentryCache>,
    root_dentry: Weak<Dentry>,
    /// 名字域首次公开后禁止可信初始化，即使弱 cache/root 此后已退休。
    namespace_published: bool,
}

struct MemorySlot {
    number: u64,
    inode: Weak<Inode>,
}

/// 单个实际原页的预留票由 folio 内容持有到最后归页，不能复制。
pub(crate) struct MemoryPageCharge {
    filesystem: Weak<MemoryFs>,
}

impl Drop for MemoryPageCharge {
    /// 原页最后释放或未发表申请失败时归还本实例额度。
    fn drop(&mut self) {
        if let Some(fs) = self.filesystem.upgrade() {
            let mut index = fs.index.lock();
            assert!(0 < index.pages, "memory page charge lost");
            index.pages -= 1;
        }
    }
}

impl MemoryFs {
    /// 可睡眠构造真实根与固定 canonical 预算，任一分配失败不发表实例。
    pub fn try_new(kind: MemoryFsKind, root_mode: u16) -> Result<Arc<Self>, FsError> {
        let mut inodes = Vec::new();
        inodes
            .try_reserve_exact(INODE_LIMIT)
            .map_err(|_| FsError::NoMemory)?;
        inodes.resize_with(INODE_LIMIT, || MemorySlot {
            number: 0,
            inode: Weak::new(),
        });
        let device = NEXT_DEVICE
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                value.checked_add(1)
            })
            .map_err(|_| FsError::NoSpace)?;
        let fs = Arc::try_new(Self {
            kind,
            device,
            root: Once::new(),
            rename: SleepMutex::new(()),
            allocation: SleepMutex::new(()),
            index: SpinLock::new(MemoryIndex {
                inodes,
                next_number: 1,
                pages: 0,
                namespace: Weak::new(),
                root_dentry: Weak::new(),
                namespace_published: false,
            }),
        })
        .map_err(|_| FsError::NoMemory)?;
        let metadata = InodeMetadata::memory(
            InodeKind::Directory,
            0x4000 | (root_mode & 0x0fff),
            0,
            0,
            2,
            0,
        );
        let root = fs.allocate(
            metadata,
            MemoryContent::Directory(SpinLock::new(MemoryDirectory {
                parent: 1,
                next_cookie: 2,
                entries: Vec::new(),
            })),
            0,
        )?;
        fs.root.call_once(|| root.memory_arc());
        Ok(fs)
    }

    /// 返回实际构造种类，供挂载快照输出固定文件系统名称。
    pub fn kind(&self) -> MemoryFsKind {
        self.kind
    }

    /// 每个内存实例有独立且终身稳定的非零设备号。
    pub fn device(&self) -> u64 {
        self.device
    }

    /// 只有 tmpfs 名字和普通内容可修改；设备数据面不使用此权限。
    pub fn mode(&self) -> AccessMode {
        match self.kind {
            MemoryFsKind::Tmpfs => AccessMode::ReadWrite,
            MemoryFsKind::Devfs | MemoryFsKind::Procfs => AccessMode::ReadOnly,
        }
    }

    /// 外部根引用保活本实例，根内部 Arc 不反向强持文件系统。
    pub fn root(self: &Arc<Self>) -> InodeRef {
        InodeRef::from_memory(
            self.root.get().expect("memory root absent").clone(),
            self.clone(),
        )
    }

    /// 在未发表的 dev/proc 根安装真实特殊节点，不向用户开放可信初始化权限。
    pub(crate) fn initialize_special(
        self: &Arc<Self>,
        name: &[u8],
        special: MemorySpecial,
        mode: u16,
        rdev: u64,
    ) -> Result<(), FsError> {
        let _rename = self.rename.lock();
        if self.kind == MemoryFsKind::Tmpfs || self.index.lock().namespace_published {
            return Err(FsError::InvalidOperation);
        }
        let kind = match (self.kind, special) {
            (
                MemoryFsKind::Devfs,
                MemorySpecial::Null
                | MemorySpecial::Zero
                | MemorySpecial::TtySerial
                | MemorySpecial::Console
                | MemorySpecial::ControllingTty,
            ) => InodeKind::Character,
            (
                MemoryFsKind::Procfs,
                MemorySpecial::Mounts | MemorySpecial::Meminfo | MemorySpecial::Uptime,
            ) => InodeKind::Regular,
            _ => return Err(FsError::InvalidOperation),
        };
        let mode = (mode & 0x0fff)
            | if kind == InodeKind::Character {
                0x2000
            } else {
                0x8000
            };
        let inode = self.allocate(
            InodeMetadata::memory(kind, mode, 0, 0, 1, 0),
            MemoryContent::Special(special),
            rdev,
        )?;
        let root = self.root();
        root.initialize_memory_child(name, &inode)
    }

    /// 睡眠分配锁保持弱槽检查与发表串行，短索引锁不跨堆申请。
    pub(crate) fn allocate(
        self: &Arc<Self>,
        mut metadata: InodeMetadata,
        content: MemoryContent,
        rdev: u64,
    ) -> Result<InodeRef, FsError> {
        let _allocation = self.allocation.lock();
        let (position, number) = {
            let mut index = self.index.lock();
            let position = index
                .inodes
                .iter()
                .position(|slot| slot.inode.strong_count() == 0)
                .ok_or(FsError::NoSpace)?;
            let number = index.next_number;
            index.next_number = number.checked_add(1).ok_or(FsError::NoSpace)?;
            (position, number)
        };
        metadata.number = number;
        metadata.rdev = rdev;
        let inode = InodeRef::try_memory(self, metadata, content)?;
        let weak = Arc::downgrade(&inode.memory_arc());
        let retired = core::mem::replace(
            &mut self.index.lock().inodes[position],
            MemorySlot {
                number,
                inode: weak,
            },
        );
        drop(retired);
        Ok(inode)
    }

    /// 从有限弱 canonical 索引取得原对象，准备中的候选也使用同一身份。
    pub(crate) fn find(self: &Arc<Self>, number: u64) -> Result<InodeRef, FsError> {
        let inode = self
            .index
            .lock()
            .inodes
            .iter()
            .find(|slot| slot.number == number)
            .and_then(|slot| slot.inode.upgrade())
            .ok_or(FsError::NotFound)?;
        Ok(InodeRef::from_memory(inode, self.clone()))
    }

    /// 原页分配前预留额度，失败申请与最终归页都消费同一不可复制票。
    pub(crate) fn reserve_page(self: &Arc<Self>) -> Result<MemoryPageCharge, FsError> {
        let mut index = self.index.lock();
        if PAGE_LIMIT <= index.pages {
            return Err(FsError::NoSpace);
        }
        index.pages += 1;
        Ok(MemoryPageCharge {
            filesystem: Arc::downgrade(self),
        })
    }

    /// 只观察本实例尚未归还的实际原页票，外部 folio 也继续占额。
    pub fn used_pages(&self) -> usize {
        self.index.lock().pages
    }

    /// 名字域只弱登记 cache/root，清名字缓存不清目录权威内容。
    fn dentry_namespace(
        self: &Arc<Self>,
        capacity: usize,
    ) -> Result<(Arc<DentryCache>, DentryRef), FsError> {
        if capacity == 0 {
            return Err(FsError::InvalidOperation);
        }
        let _rename = self.rename.lock();
        let (cache, root) = {
            let index = self.index.lock();
            (
                index.namespace.upgrade(),
                DentryRef::upgrade(&index.root_dentry),
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
            None => DentryRef::try_root(self.root())?,
        };
        let mut index = self.index.lock();
        index.namespace = Arc::downgrade(&cache);
        index.root_dentry = root.downgrade();
        index.namespace_published = true;
        drop(index);
        Ok((cache, root))
    }
}
