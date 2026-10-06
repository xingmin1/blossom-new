//! 缓存域保存唯一 inode，元数据只发布一次，文件页由该 inode 的 mapping 拥有。

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::marker::PhantomPinned;
use core::ops::Deref;
use core::pin::{pin, Pin};
#[cfg(feature = "ktest")]
use core::sync::atomic::{AtomicUsize, Ordering};
use spin::Once;

use crate::cache::FileMapping;
use crate::fs::ext4::{DirectoryEntry, Ext4Volume, InodeData, InodeKind};
use crate::fs::FsError;
use crate::mm::{FolioRef, PAGE_SIZE};
use crate::sched;
use crate::sync::SpinLock;
use crate::wait::{WaitNode, WaitQueue, Waiter};

#[cfg(feature = "ktest")]
type LoadHook = Arc<dyn Fn(&InodeRef) + Send + Sync>;

/// 独立缓存域强持 inode；后端只由 inode 保活，不反向拥有缓存域。
/// ponytail: 固定预算内线性查找，预算显著增大且查找成为瓶颈时改为有限散列表。
pub struct InodeCache {
    entries: SpinLock<Vec<InodeRef>>,
    capacity: usize,
    file_page_capacity: usize,
    #[cfg(feature = "ktest")]
    load_hook: SpinLock<Option<LoadHook>>,
    #[cfg(feature = "ktest")]
    loads: AtomicUsize,
}

/// 私有 Arc 保证等待源从发表到最后引用结束始终留在原分配中。
/// 仅提供共享借用，不允许取回 Arc、取得可变借用或移出 inode。
#[derive(Clone)]
pub struct InodeRef(Arc<Inode>);

/// 同一卷实例与 inode 号的只读对象，尚未发布 once 结果时处于 Loading。
pub struct Inode {
    filesystem: Arc<Ext4Volume>,
    number: u64,
    data: Once<Result<InodeData, FsError>>,
    mapping: FileMapping,
    waiters: WaitQueue,
    _pinned: PhantomPinned,
}

/// 只有胜出的候选取得此责任；提前退出发布取消结果并解除自己的占位。
struct LoadGuard<'a> {
    cache: &'a InodeCache,
    inode: &'a InodeRef,
    active: bool,
}

impl InodeCache {
    /// 锁外一次预留有限索引；每个 inode 的页预算在候选构造时预留。
    pub fn try_new(inode_capacity: usize, file_page_capacity: usize) -> Result<Self, FsError> {
        if inode_capacity == 0 || file_page_capacity == 0 {
            return Err(FsError::InvalidOperation);
        }
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(inode_capacity)
            .map_err(|_| FsError::NoMemory)?;
        Ok(Self {
            entries: SpinLock::new(entries),
            capacity: inode_capacity,
            file_page_capacity,
            #[cfg(feature = "ktest")]
            load_hook: SpinLock::new(None),
            #[cfg(feature = "ktest")]
            loads: AtomicUsize::new(0),
        })
    }

    /// 普通可切换 Task 获取同身份对象；加载与等待均在索引锁和 once 初始化之外。
    pub fn iget(&self, filesystem: &Arc<Ext4Volume>, number: u64) -> Result<InodeRef, FsError> {
        sched::assert_switchable();
        let (existing, full) = {
            let entries = self.entries.lock();
            (
                find(&entries, filesystem, number),
                self.capacity <= entries.len(),
            )
        };
        if let Some(inode) = existing {
            inode.metadata()?;
            return Ok(inode);
        }
        if full {
            self.prune_unused();
            // 回收期间可能发表同 key，须先查身份再判断预算，避免命中也报 OOM。
            let entries = self.entries.lock();
            let existing = find(&entries, filesystem, number);
            let full = self.capacity <= entries.len();
            drop(entries);
            if let Some(inode) = existing {
                inode.metadata()?;
                return Ok(inode);
            }
            if full {
                return Err(FsError::NoMemory);
            }
        }
        let candidate = InodeRef::try_new(filesystem, number, self.file_page_capacity)?;
        let mut loader = LoadGuard {
            cache: self,
            inode: &candidate,
            active: false,
        };
        let existing = match self.publish(&candidate, &mut loader) {
            Err(FsError::NoMemory) => {
                // 候选准备期间预算可能被占满；只做一次锁外回收，再查身份并发表。
                self.prune_unused();
                self.publish(&candidate, &mut loader)?
            }
            result => result?,
        };
        if let Some(inode) = existing {
            // 竞争失败者没有加载责任，不改变赢家的 once 或等待队列。
            drop(loader);
            drop(candidate);
            inode.metadata()?;
            return Ok(inode);
        }
        #[cfg(feature = "ktest")]
        {
            self.loads.fetch_add(1, Ordering::Relaxed);
            let hook = self.load_hook.lock().clone();
            if let Some(hook) = hook {
                hook(&candidate);
            }
        }
        let result = filesystem.load_inode(number);
        loader.complete(result);
        drop(loader);
        candidate.metadata()?;
        Ok(candidate)
    }

    /// 逐个移出 cache-only 且已静止的 inode，文件页和后端引用均在索引锁外释放。
    pub fn prune_unused(&self) -> usize {
        let mut removed = 0;
        for position in (0..self.len()).rev() {
            let candidate = {
                let entries = self.entries.lock();
                entries
                    .get(position)
                    .filter(|inode| Arc::strong_count(&inode.0) == 1 && inode.data.get().is_some())
                    .cloned()
            };
            let Some(candidate) = candidate else {
                continue;
            };
            // mapping 可把 folio 或 IO owner 交给外部；有活页时不得重造另一个 inode。
            candidate.mapping.prune_unused();
            let victim = {
                let mut entries = self.entries.lock();
                let same = entries
                    .get(position)
                    .is_some_and(|inode| inode.ptr_eq(&candidate));
                // 只有缓存和本次扫描引用时才查 mapping；索引到页索引的短锁顺序
                // 不跨页释放。页填充和页回收从不反向取得 inode 索引锁。
                if same && Arc::strong_count(&candidate.0) == 2 && candidate.mapping.is_empty() {
                    Some(entries.swap_remove(position))
                } else {
                    None
                }
            };
            if victim.is_some() {
                removed += 1;
            }
            drop(victim);
            drop(candidate);
        }
        removed
    }

    /// 返回占用槽数，Loading 也占用构造时固定的预算。
    pub fn len(&self) -> usize {
        self.entries.lock().len()
    }

    /// 观察本缓存域是否没有已发表的 inode。
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 观察包含加载中对象的固定槽预算，名字索引可先释放静止绑定持有的 inode 引用。
    pub(crate) fn is_full(&self) -> bool {
        self.capacity <= self.len()
    }

    /// 双查同身份后发表 Loading；锁内只增加引用和占槽，不分配、加载或释放页。
    fn publish(
        &self,
        candidate: &InodeRef,
        loader: &mut LoadGuard<'_>,
    ) -> Result<Option<InodeRef>, FsError> {
        let mut entries = self.entries.lock();
        if let Some(inode) = find(&entries, &candidate.filesystem, candidate.number) {
            return Ok(Some(inode));
        }
        if self.capacity <= entries.len() {
            return Err(FsError::NoMemory);
        }
        // 构造时预留全部槽位，push 不分配；guard 先于 Loading 对外发表承担清理责任。
        loader.active = true;
        entries.push(candidate.clone());
        Ok(None)
    }

    /// 只摘除加载失败的原对象，后续同 key 的新加载者不受旧 guard 影响。
    fn remove_failed(&self, inode: &InodeRef) {
        let victim = {
            let mut entries = self.entries.lock();
            entries
                .iter()
                .position(|entry| entry.ptr_eq(inode))
                .map(|position| entries.swap_remove(position))
        };
        drop(victim);
    }

    /// 在真实 Loading 发表之后、读取 inode 之前运行锁外停点；旧回调锁外析构。
    #[cfg(feature = "ktest")]
    pub(crate) fn ktest_set_load_hook(&self, hook: Option<LoadHook>) {
        let old = core::mem::replace(&mut *self.load_hook.lock(), hook);
        drop(old);
    }

    /// 返回真正取得加载责任的次数，不计命中与私有竞争失败候选。
    #[cfg(feature = "ktest")]
    pub(crate) fn ktest_load_count(&self) -> usize {
        self.loads.load(Ordering::Relaxed)
    }

    /// 借用原对象读取等待源成员数，观测不制造另一份等待状态。
    #[cfg(feature = "ktest")]
    pub(crate) fn ktest_waiting(&self, filesystem: &Arc<Ext4Volume>, number: u64) -> usize {
        let inode = find(&self.entries.lock(), filesystem, number);
        inode.map_or(0, |inode| inode.waiters.test_members())
    }
}

impl InodeRef {
    /// 锁外准备尚未发表的稳定对象与唯一 mapping，任何分配失败均无索引占位。
    fn try_new(
        filesystem: &Arc<Ext4Volume>,
        number: u64,
        file_page_capacity: usize,
    ) -> Result<Self, FsError> {
        let mapping = FileMapping::try_new(file_page_capacity)?;
        let inode = Arc::try_new(Inode {
            filesystem: Arc::clone(filesystem),
            number,
            data: Once::new(),
            mapping,
            waiters: WaitQueue::new(),
            _pinned: PhantomPinned,
        })
        .map_err(|_| FsError::NoMemory)?;
        Ok(Self(inode))
    }

    /// 比较原对象身份，不以相同卷号、元数据内容或盘上 inode 号代替实例身份。
    pub fn ptr_eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Deref for InodeRef {
    type Target = Inode;

    /// 只借用原固定对象；没有 DerefMut 或公开 Arc 提供移动固定等待源的入口。
    fn deref(&self) -> &Inode {
        &self.0
    }
}

impl Inode {
    /// 返回该对象的盘上 inode 号；跨卷身份还须包括 filesystem 实例。
    pub fn number(&self) -> u64 {
        self.number
    }

    /// 借用强持的实际后端实例，不建立新的文件系统身份层。
    pub fn filesystem(&self) -> &Arc<Ext4Volume> {
        &self.filesystem
    }

    /// 等待一次加载结果并借用已解码元数据；命中不重新解析或复制 inode。
    pub fn metadata(&self) -> Result<&InodeData, FsError> {
        sched::assert_switchable();
        let inode = self.pinned().get_ref();
        loop {
            if let Some(result) = inode.data.get() {
                return result.as_ref().map_err(|error| *error);
            }
            let waiter = Waiter::new();
            let mut node = pin!(WaitNode::new(waiter.waker()));
            // SAFETY: 私有 Arc 只给共享借用，inode 和内嵌等待源固定且由调用者引用保活；
            // pin! 固定原栈节点，registration 不被遗忘，退出先撤链再释放节点和 waiter。
            let _registration = unsafe { inode.waiters.register(node.as_mut()) };
            if let Some(result) = inode.data.get() {
                return result.as_ref().map_err(|error| *error);
            }
            waiter.park();
        }
    }

    /// 借用 inode 自己唯一的文件页索引，页填充仍须遵守后端与 FolioRead 协议。
    pub fn mapping(&self) -> &FileMapping {
        &self.mapping
    }

    /// 读取普通文件的有效字节，后页失败时返回已交付前缀，零长和 EOF 不建立页。
    pub fn read_at(&self, offset: u64, dst: &mut [u8]) -> Result<usize, FsError> {
        if dst.is_empty() {
            return Ok(0);
        }
        let data = self.metadata()?;
        if data.kind != InodeKind::Regular {
            return Err(FsError::InvalidOperation);
        }
        if data.size <= offset {
            return Ok(0);
        }
        let len = (data.size - offset).min(dst.len() as u64) as usize;
        let mut copied = 0;
        while copied < len {
            // copied 不超过 size-offset，因此本次位置及页内有效范围不会溢出。
            let position = offset + copied as u64;
            let index = position / PAGE_SIZE as u64;
            let in_page = (position % PAGE_SIZE as u64) as usize;
            let count = (len - copied).min(PAGE_SIZE - in_page);
            let result = self.read_page(data, index).and_then(|folio| {
                folio
                    .with_bytes(in_page, count, |bytes| {
                        dst[copied..copied + count].copy_from_slice(bytes);
                    })
                    .map_err(FsError::from)
            });
            if let Err(error) = result {
                return if copied == 0 { Err(error) } else { Ok(copied) };
            }
            copied += count;
        }
        Ok(copied)
    }

    /// 普通任务取得正式元数据后等待同一 mapping 的页；映射调用者另行检查 EOF 和私有尾页。
    pub(crate) fn file_page(&self, index: u64) -> Result<FolioRef, FsError> {
        let data = self.metadata()?;
        if data.kind != InodeKind::Regular {
            return Err(FsError::InvalidOperation);
        }
        self.read_page(data, index)
    }

    /// 通过共同 mapping 协调同页唯一填充；不同页没有 inode 级读入锁。
    fn read_page(&self, data: &InodeData, index: u64) -> Result<FolioRef, FsError> {
        let (folio, read) = self.mapping.get_or_prepare(index)?;
        if let Some(read) = read {
            self.filesystem.fill_page(data, index, read)?;
        }
        folio.wait_ready()?;
        Ok(folio)
    }

    /// 原始单组件名字经真实目录后端查找，再在同缓存域获取硬链接的唯一对象。
    pub fn lookup(&self, cache: &InodeCache, name: &[u8]) -> Result<InodeRef, FsError> {
        let number = self
            .filesystem
            .lookup(self.metadata()?, name)?
            .ok_or(FsError::NotFound)?;
        cache.iget(&self.filesystem, number)
    }

    /// 沿盘上字节位置枚举下一个有效目录项，不构造整目录副本。
    pub fn dir_next(&self, offset: u64) -> Result<Option<DirectoryEntry>, FsError> {
        self.filesystem.dir_next(self.metadata()?, offset)
    }

    /// 由后端按真实 inode 存储规则返回符号链接的原始字节。
    pub fn read_link(&self) -> Result<Vec<u8>, FsError> {
        self.filesystem.read_link(self.metadata()?)
    }

    /// 私有拥有类型禁止移动内嵌等待源，借用期间最后一个 Arc 不能析构。
    fn pinned(&self) -> Pin<&Self> {
        // SAFETY: Inode 只能由私有构造放入 Arc 后发表，InodeRef 不暴露 Arc 或可变借用；
        // 本模块从不 unwrap、移出或替换对象，Arc 的原分配保持到本共享借用终止之后。
        unsafe { Pin::new_unchecked(self) }
    }
}

impl LoadGuard<'_> {
    /// I/O 已结束后只将结果移入 once，失败解除原占位，再锁外唤醒全部等待者。
    fn complete(&mut self, result: Result<InodeData, FsError>) {
        self.publish(result);
        self.active = false;
    }

    /// 复用首次结果，展开清理不能覆盖已经发布的真实加载原因。
    fn publish(&self, result: Result<InodeData, FsError>) {
        let result = self.inode.data.call_once(|| result);
        if result.is_err() {
            self.cache.remove_failed(self.inode);
        }
        self.inode.waiters.wake_all();
    }
}

impl Drop for LoadGuard<'_> {
    /// 尚未完成的唯一加载者退出时发布取消，等待者仍持原对象直到消费错误。
    fn drop(&mut self) {
        if self.active {
            self.publish(Err(FsError::LoadAborted));
        }
    }
}

/// 索引锁内只增加现有对象引用，卷身份使用实际 Arc 实例地址。
fn find(entries: &[InodeRef], filesystem: &Arc<Ext4Volume>, number: u64) -> Option<InodeRef> {
    entries
        .iter()
        .find(|inode| inode.number == number && Arc::ptr_eq(&inode.filesystem, filesystem))
        .cloned()
}
