//! canonical inode 的加载结果、运行属性和块映射分别受实际拥有与同步协议保护。

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::marker::PhantomPinned;
use core::ops::Deref;
use core::pin::{pin, Pin};
#[cfg(feature = "ktest")]
use core::sync::atomic::AtomicUsize;
use core::sync::atomic::{AtomicBool, Ordering};
use spin::Once;

use crate::block::{AccessMode, FsClaim};
use crate::cache::{BufferRef, FileMapping};
use crate::fs::ext4::{
    codec, finish_error, BlockMapping, DirectoryEntry, Ext4Volume, InodeData, InodeKind, InodeTime,
    PreparedInodeRelease,
};
use crate::fs::writeback;
use crate::fs::{FsError, ReadBuffer, WriteBuffer};
use crate::mm::folio::FolioContent;
use crate::mm::{FolioRef, PAGE_SIZE};
use crate::sched;
use crate::sync::{ReadGuard, SleepMutex, SleepRwLock, SpinLock};
use crate::time;
use crate::wait::{WaitNode, WaitQueue, Waiter};
use crate::workqueue::{Work, WorkQueue, WorkQueueConfig};

#[cfg(feature = "ktest")]
type LoadHook = Arc<dyn Fn(&InodeRef) + Send + Sync>;

/// 独立缓存拥有有限 canonical 域；最后使用和淘汰都在同一索引锁内交给具体回收工作。
/// ponytail: 固定预算内线性查找，预算显著扩大且实测查找成为瓶颈时改为有限散列表。
pub struct InodeCache {
    domain: Arc<InodeDomain>,
    #[cfg(feature = "ktest")]
    load_hook: SpinLock<Option<LoadHook>>,
    #[cfg(feature = "ktest")]
    loads: AtomicUsize,
}

/// 固定槽同时保存缓存或待回收的强拥有，外部使用由 InodeRef 在此索引锁内登记。
pub(crate) struct InodeDomain {
    index: SpinLock<InodeIndex>,
    work: Once<Pin<Arc<Work>>>,
    waiters: WaitQueue,
    retained: AtomicBool,
    retained_next: SpinLock<Option<Arc<InodeDomain>>>,
}

struct InodeIndex {
    slots: Vec<Option<InodeSlot>>,
    cache_open: bool,
    evicted: usize,
    /// 同一索引锁内预留的 Work 发表责任，归零前不能宣布整个域已退场。
    dispatching: usize,
}

struct InodeSlot {
    inode: Arc<Inode>,
    users: usize,
    eviction: Eviction,
}

#[derive(Clone, Copy)]
enum Eviction {
    Cached,
    Requested,
    Running,
    Failed(FsError),
}

/// 常驻域仅供具体 inode 回收使用，执行者仍来自原共享 pool。
static EVICTION_QUEUE: Once<WorkQueue> = Once::new();
/// 失败的删除义务由独立服务强持，链上节点不反向拥有服务或自身。
static FAILED_EVICTIONS: SpinLock<Option<Arc<InodeDomain>>> = SpinLock::new(None);

/// BSP 在原共享 pool 准备后、发布服务运行许可之前准备具体 inode 回收域。
pub(crate) fn initialize_inode_eviction() -> Result<(), FsError> {
    assert!(
        EVICTION_QUEUE.get().is_none(),
        "inode eviction initialized twice"
    );
    let queue = WorkQueue::try_new(WorkQueueConfig {
        max_active: 8,
        ordered: false,
        reclaim: false,
    })
    .map_err(|_| FsError::NoMemory)?;
    EVICTION_QUEUE.call_once(|| queue);
    Ok(())
}

impl InodeDomain {
    /// 缓存发表之前准备固定 canonical 槽和单个 Work，提交与最后 put 不再分配。
    fn try_new(capacity: usize) -> Result<Arc<Self>, FsError> {
        let mut slots = Vec::new();
        slots
            .try_reserve_exact(capacity)
            .map_err(|_| FsError::NoMemory)?;
        slots.resize_with(capacity, || None);
        let domain = Arc::try_new(Self {
            index: SpinLock::new(InodeIndex {
                slots,
                cache_open: true,
                evicted: 0,
                dispatching: 0,
            }),
            work: Once::new(),
            waiters: WaitQueue::new(),
            retained: AtomicBool::new(false),
            retained_next: SpinLock::new(None),
        })
        .map_err(|_| FsError::NoMemory)?;
        let work = Work::try_new(
            EVICTION_QUEUE
                .get()
                .expect("inode eviction service is not prepared"),
            0,
            evict_inodes,
            Arc::as_ptr(&domain) as usize,
        )
        .map_err(|_| FsError::NoMemory)?;
        domain.work.call_once(|| work);
        Ok(domain)
    }

    /// 每个被 Work 接受的票另持一份真实域拥有，重复提交只归还本次未接受的票。
    fn queue(self: &Arc<Self>) {
        let owner = Arc::into_raw(self.clone());
        if !self
            .work
            .get()
            .expect("inode eviction work is not prepared")
            .as_ref()
            .queue()
            .expect("permanent inode eviction queue closed")
        {
            // SAFETY: owner 来自本次 into_raw，未被 Work 接受；这里只有本次票的唯一归还者。
            drop(unsafe { Arc::from_raw(owner) });
        }
        {
            let mut index = self.index.lock();
            assert!(0 < index.dispatching, "inode eviction dispatch lost");
            index.dispatching -= 1;
        }
        self.waiters.wake_all();
    }

    /// 等待已接受回收工作的真实尾部；失败仍占原槽并保留具体原因。
    fn flush(&self) -> Result<(), FsError> {
        self.work
            .get()
            .expect("inode eviction work is not prepared")
            .as_ref()
            .flush();
        let index = self.index.lock();
        for slot in index.slots.iter().flatten() {
            if let Eviction::Failed(error) = slot.eviction {
                return Err(error);
            }
        }
        Ok(())
    }

    /// 原缓存预算固定本轮扫描上界，不通过弱升级重造 inode 或遗失待回收义务。
    pub(crate) fn dirty_inode_limit(&self) -> usize {
        self.index.lock().slots.len()
    }

    /// 索引锁内取得仍可用的原身份；已进入真实回收的对象不再接受新的 WB 使用。
    pub(crate) fn dirty_inode_at(self: &Arc<Self>, position: usize) -> Option<InodeRef> {
        let mut index = self.index.lock();
        let slot = index.slots.get_mut(position)?.as_mut()?;
        if !slot.inode.dirty_obligation.load(Ordering::Acquire)
            || matches!(slot.eviction, Eviction::Running | Eviction::Failed(_))
            || matches!(slot.eviction, Eviction::Requested) && slot.users == 0
        {
            return None;
        }
        Some(use_slot(self, slot, false))
    }

    /// 所有后端和名字查找都复用此 canonical 加载路径，回收中的原身份先等真实结束。
    pub(crate) fn iget(
        self: &Arc<Self>,
        filesystem: &Arc<Ext4Volume>,
        number: u64,
        _cache: Option<&InodeCache>,
    ) -> Result<InodeRef, FsError> {
        sched::assert_switchable();
        filesystem.bind_inode_domain(self)?;
        if let Some(inode) = self.find_loaded(filesystem, number)? {
            inode.wait_loaded()?;
            return Ok(inode);
        }
        if self.is_full() {
            // 回收工作不得同步等待自己的 Work；隐藏 EA 准备不足时保原删除义务并返回真 OOM。
            if _cache.is_none() {
                return Err(FsError::NoMemory);
            }
            self.prune_unused();
            if let Some(inode) = self.find_loaded(filesystem, number)? {
                inode.wait_loaded()?;
                return Ok(inode);
            }
            if self.is_full() {
                return Err(FsError::NoMemory);
            }
        }
        let candidate = InodeRef::try_new(filesystem, number, self)?;
        let mut loader = LoadGuard {
            domain: self,
            inode: &candidate,
            active: false,
        };
        let existing = self.publish(&candidate, &mut loader)?;
        if let Some(inode) = existing {
            drop(loader);
            drop(candidate);
            inode.wait_loaded()?;
            return Ok(inode);
        }
        #[cfg(feature = "ktest")]
        if let Some(cache) = _cache {
            cache.loads.fetch_add(1, Ordering::Relaxed);
            let hook = cache.load_hook.lock().clone();
            if let Some(hook) = hook {
                hook(&candidate);
            }
        }
        loader.complete(filesystem.load_inode(number));
        drop(loader);
        candidate.wait_loaded()?;
        Ok(candidate)
    }

    /// 在索引锁内判定身份与回收状态，登记后复查避免回收通知早于睡眠。
    fn find_loaded(
        self: &Arc<Self>,
        filesystem: &Arc<Ext4Volume>,
        number: u64,
    ) -> Result<Option<InodeRef>, FsError> {
        loop {
            let result = find(&mut self.index.lock(), self, filesystem, number);
            if result.as_ref().err() != Some(&FsError::Busy) {
                return result;
            }
            let waiter = Waiter::new();
            let mut node = pin!(WaitNode::new(waiter.waker()));
            // SAFETY: self 强持固定域等待源，pin! 固定原栈节点，登记先于节点与 waiter 析构。
            let registration = unsafe { self.waiters.register(node.as_mut()) };
            let result = find(&mut self.index.lock(), self, filesystem, number);
            if result.as_ref().err() != Some(&FsError::Busy) {
                return result;
            }
            waiter.park();
            drop(registration);
        }
    }

    /// 双查同身份后发表 Loading；槽强拥有不计作外部使用，唯一加载者先承担失败清理。
    fn publish(
        self: &Arc<Self>,
        candidate: &InodeRef,
        loader: &mut LoadGuard<'_>,
    ) -> Result<Option<InodeRef>, FsError> {
        loop {
            let mut index = self.index.lock();
            match find(&mut index, self, &candidate.filesystem, candidate.number) {
                Ok(Some(inode)) => return Ok(Some(inode)),
                Ok(None) => {}
                Err(FsError::Busy) => {
                    drop(index);
                    if let Some(inode) =
                        self.find_loaded(&candidate.filesystem, candidate.number)?
                    {
                        return Ok(Some(inode));
                    }
                    continue;
                }
                Err(error) => return Err(error),
            }
            let position = index
                .slots
                .iter()
                .position(Option::is_none)
                .ok_or(FsError::NoMemory)?;
            loader.active = true;
            candidate.wb_slot.call_once(|| position);
            index.slots[position] = Some(InodeSlot {
                inode: candidate.0.clone(),
                users: 1,
                eviction: Eviction::Cached,
            });
            return Ok(None);
        }
    }

    /// 已取得临时位图拥有的分配者在初始化新记录前等旧身份结束，不返回不可消费的新 token。
    pub(crate) fn wait_inode_reuse(
        &self,
        filesystem: &Ext4Volume,
        number: u64,
    ) -> Result<(), FsError> {
        sched::assert_switchable();
        if self.inode_reusable(filesystem, number)? {
            return Ok(());
        }
        loop {
            let waiter = Waiter::new();
            let mut node = pin!(WaitNode::new(waiter.waker()));
            // SAFETY: 调用者的 Arc 保域等待源固定存活，栈节点固定且登记先于节点/waiter 析构。
            let registration = unsafe { self.waiters.register(node.as_mut()) };
            if self.inode_reusable(filesystem, number)? {
                return Ok(());
            }
            waiter.park();
            drop(registration);
        }
    }

    /// 原索引锁内区分正常退场、真实失败和非法的位图/cache 身份冲突，不增加外部使用。
    fn inode_reusable(&self, filesystem: &Ext4Volume, number: u64) -> Result<bool, FsError> {
        let index = self.index.lock();
        let Some(slot) = index.slots.iter().flatten().find(|slot| {
            slot.inode.number == number && core::ptr::eq(slot.inode.filesystem.as_ref(), filesystem)
        }) else {
            return Ok(true);
        };
        match slot.eviction {
            Eviction::Requested | Eviction::Running => Ok(false),
            Eviction::Failed(error) => Err(error),
            Eviction::Cached => Err(FsError::Corrupt),
        }
    }

    /// 失败加载没有内容义务，核原对象后摘槽，缓存和等待者的最后析构都在短锁外。
    fn remove_failed(&self, inode: &InodeRef) {
        let victim = {
            let mut index = self.index.lock();
            let position = *inode
                .wb_slot
                .get()
                .expect("loaded inode has no canonical slot");
            if index.slots[position]
                .as_ref()
                .is_some_and(|slot| Arc::ptr_eq(&slot.inode, &inode.0))
            {
                index.slots[position].take()
            } else {
                None
            }
        };
        self.waiters.wake_all();
        drop(victim);
    }

    /// 普通 Task 将纯缓存拥有交给原 Work 并等真实尾部；失败保槽，不能伪报已经回收。
    fn prune_unused(self: &Arc<Self>) -> usize {
        sched::assert_switchable();
        let (before, requested) = {
            let mut index = self.index.lock();
            let before = index.evicted;
            let mut requested = false;
            for slot in index.slots.iter_mut().flatten() {
                if slot.users == 0
                    && slot.inode.data.get().is_some()
                    && slot.inode.mapping.is_cache_only()
                {
                    slot.eviction = Eviction::Requested;
                    requested = true;
                }
            }
            if requested {
                index.dispatching = index
                    .dispatching
                    .checked_add(1)
                    .expect("inode eviction dispatch overflow");
            }
            (before, requested)
        };
        if requested {
            self.queue();
            // 原 usize 接口只报告真正归还的槽；具体失败仍留在 slot、WB errseq 与观察出口。
            let _ = self.flush();
        }
        self.index.lock().evicted - before
    }

    /// Loading 和失败待回收对象均占固定槽，预算不足不能跳过未结束的原拥有。
    fn is_full(&self) -> bool {
        self.index.lock().slots.iter().all(Option::is_some)
    }
}

/// 私有 Arc 固定原 inode；第二份拥有保护所属索引，Clone/Drop 交接全部外部使用。
pub struct InodeRef(Arc<Inode>, Arc<InodeDomain>, bool);

/// 同一 FS 实例中的唯一运行对象；域拥有 inode，inode 不反向强持域。
pub struct Inode {
    filesystem: Arc<Ext4Volume>,
    number: u64,
    data: Once<Result<InodeState, FsError>>,
    mapping: FileMapping,
    waiters: WaitQueue,
    /// canonical 固定槽同时承担有界 WB 扫描、外部使用和回收的身份交接。
    wb_slot: Once<usize>,
    dirty_obligation: AtomicBool,
    /// 后端 inode 位已实际归还；失败回收只能结束剩余元数据 WB，不能再次释放原存储。
    storage_released: AtomicBool,
    #[cfg(feature = "ktest")]
    truncate_hook: SpinLock<Option<TruncateHook>>,
    _pinned: PhantomPinned,
}

/// 停点只观察真实截断顺序，不改变用户 ABI 或后端结果。
#[cfg(feature = "ktest")]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum TruncatePhase {
    AfterFirstUnmap,
    BeforeSecondUnmap,
    BeforeBlockRelease,
    AfterStorageRelease,
}

#[cfg(feature = "ktest")]
type TruncateHook = Arc<dyn Fn(&InodeRef, TruncatePhase) + Send + Sync>;

/// 回收收据只保原域，不增加 inode、文件页或映射使用；正式关闭者可等全域真实退场。
pub(crate) struct InodeCacheWatch(Arc<InodeDomain>);

impl InodeCacheWatch {
    /// 等待调用边界已接受的 Work 全尾，仍有外部使用时不宣称其 inode 已回收。
    pub(crate) fn wait(&self) -> Result<(), FsError> {
        self.0.flush()
    }

    /// cache owner 已退出后等待全部真实使用、发表责任和回收 Work；失败继续强保并返回原错误。
    pub(crate) fn wait_retired(&self) -> Result<(), FsError> {
        sched::assert_switchable();
        loop {
            let waiter = Waiter::new();
            let mut node = pin!(WaitNode::new(waiter.waker()));
            // SAFETY: 收据强持固定域等待源，栈节点固定，registration 先于 node/waiter 析构。
            let registration = unsafe { self.0.waiters.register(node.as_mut()) };
            let retired = {
                let index = self.0.index.lock();
                if index.cache_open {
                    return Err(FsError::InvalidOperation);
                }
                for slot in index.slots.iter().flatten() {
                    if let Eviction::Failed(error) = slot.eviction {
                        return Err(error);
                    }
                }
                index.dispatching == 0 && index.slots.iter().all(Option::is_none)
            };
            if retired {
                drop(registration);
                // 摘除队列登记不会结束 Task 等待票，下一次 Work 等待前必须析构 Waiter。
                drop(waiter);
                return self.0.flush();
            }
            waiter.park();
            drop(registration);
        }
    }

    /// 只观察原 canonical 槽，不取得新的使用或改变待回收状态。
    #[cfg(feature = "ktest")]
    pub(crate) fn contains(&self, number: u64) -> bool {
        self.0
            .index
            .lock()
            .slots
            .iter()
            .flatten()
            .any(|slot| slot.inode.number == number)
    }
}

/// 有界属性快照不含块根；size 只在 canonical 属性锁内保存一份运行值。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InodeMetadata {
    pub number: u64,
    pub kind: InodeKind,
    pub mode: u16,
    pub size: u64,
    pub uid: u32,
    pub gid: u32,
    pub links: u32,
    pub blocks: u64,
    pub atime: InodeTime,
    pub mtime: InodeTime,
    pub ctime: InodeTime,
    pub generation: u32,
    pub flags: u32,
    pub checksum_seed: u32,
    pub fast_symlink: bool,
    pub file_acl: u64,
    pub extra_isize: usize,
}

struct InodeState {
    write: SleepMutex<()>,
    invalidate: SleepRwLock<()>,
    metadata: SpinLock<InodeMetadata>,
    root: SleepRwLock<[u8; 60]>,
    writecount: SpinLock<isize>,
}

impl InodeMetadata {
    /// 将盘面加载值拆为运行属性，块根单独进入睡眠读写保护。
    pub(crate) fn from_data(data: &InodeData) -> Self {
        Self {
            number: data.number,
            kind: data.kind,
            mode: data.mode,
            size: data.size,
            uid: data.uid,
            gid: data.gid,
            links: data.links,
            blocks: data.blocks,
            atime: data.atime,
            mtime: data.mtime,
            ctime: data.ctime,
            generation: data.generation,
            flags: data.flags,
            checksum_seed: data.checksum_seed,
            fast_symlink: data.fast_symlink,
            file_acl: data.file_acl,
            extra_isize: data.extra_isize,
        }
    }

    /// 仅为当次真实块映射查询组装 codec 输入，不保存第二份可变状态。
    pub(crate) fn with_block(&self, block: &[u8; 60]) -> InodeData {
        InodeData {
            number: self.number,
            kind: self.kind,
            mode: self.mode,
            size: self.size,
            uid: self.uid,
            gid: self.gid,
            links: self.links,
            blocks: self.blocks,
            atime: self.atime,
            mtime: self.mtime,
            ctime: self.ctime,
            generation: self.generation,
            flags: self.flags,
            checksum_seed: self.checksum_seed,
            fast_symlink: self.fast_symlink,
            file_acl: self.file_acl,
            extra_isize: self.extra_isize,
            block: *block,
        }
    }
}

impl InodeState {
    /// 完成真实加载后一次安装同步容器，初始化不执行额外 IO。
    fn new(data: InodeData) -> Self {
        Self {
            write: SleepMutex::new(()),
            invalidate: SleepRwLock::new(()),
            metadata: SpinLock::new(InodeMetadata::from_data(&data)),
            root: SleepRwLock::new(data.block),
            writecount: SpinLock::new(0),
        }
    }
}

/// 锁外准备完成的同源页及当时事实，不保留 inode、块根或内容锁的 guard。
pub(crate) struct PreparedMkwrite {
    folio: FolioRef,
    index: u64,
    size: u64,
    prepared_sectors: u8,
}

impl PreparedMkwrite {
    /// 借用原 canonical 页，最终授权必须重核该页仍属于当前 inode 索引。
    pub(crate) fn folio(&self) -> &FolioRef {
        &self.folio
    }
}

/// 尾零的所有可失败准备在 size 修改前完成，只保原页及真实 Mapped 块位。
struct PreparedTail {
    folio: FolioRef,
    index: u64,
    start: usize,
    end: usize,
    mapped: u8,
}

/// 真正写打开持有的 inode 责任；dup 共享 OFD 不复制本 token。
pub struct WriteAccess {
    inode: InodeRef,
}

/// 每个活执行地址空间保有独立 deny 责任，最后实际映像使用结束才释放。
pub struct ExecAccess {
    inode: InodeRef,
}

impl WriteAccess {
    /// 写能力始终绑定取得时的 canonical inode，不能借给其他文件。
    pub fn write_from(
        &self,
        claim: &FsClaim,
        offset: u64,
        append: bool,
        source: &mut WriteBuffer<'_>,
    ) -> Result<(u64, usize), FsError> {
        self.inode.write_from(claim, offset, append, source)
    }

    /// 三个外部截断入口共用原写能力和同一实现，不取得或修改 OFD 位置。
    pub fn truncate(&self, claim: &FsClaim, new_size: u64) -> Result<(), FsError> {
        self.inode.truncate(claim, new_size)
    }
}

impl Drop for WriteAccess {
    /// 只归还原 inode 的一次写拥有，计数短锁内不分配或等待。
    fn drop(&mut self) {
        let mut count = self.inode.loaded_state().writecount.lock();
        assert!(0 < *count, "write access count lost");
        *count -= 1;
    }
}

impl Clone for ExecAccess {
    /// fork 增加同 inode 的独立映像拥有，活 deny 期间不可能出现正写计数。
    fn clone(&self) -> Self {
        let mut count = self.inode.loaded_state().writecount.lock();
        assert!(*count < 0, "exec access count lost");
        *count = count.checked_sub(1).expect("exec access count overflow");
        Self {
            inode: self.inode.clone(),
        }
    }
}

impl Drop for ExecAccess {
    /// 地址空间最后退出时归还本次 deny，不能替其他映像释放。
    fn drop(&mut self) {
        let mut count = self.inode.loaded_state().writecount.lock();
        assert!(*count < 0, "exec access count lost");
        *count += 1;
    }
}

/// 只有胜出的候选取得此责任；提前退出发布取消结果并解除自己的占位。
struct LoadGuard<'a> {
    domain: &'a Arc<InodeDomain>,
    inode: &'a InodeRef,
    active: bool,
}

impl InodeCache {
    /// 准备独立的有限 canonical 域和具体回收 Work，不预留每文件固定页槽。
    pub fn try_new(inode_capacity: usize) -> Result<Self, FsError> {
        if inode_capacity == 0 {
            return Err(FsError::InvalidOperation);
        }
        Ok(Self {
            domain: InodeDomain::try_new(inode_capacity)?,
            #[cfg(feature = "ktest")]
            load_hook: SpinLock::new(None),
            #[cfg(feature = "ktest")]
            loads: AtomicUsize::new(0),
        })
    }

    /// 名字查找与内部 EA 取得共享同一 canonical 域，真实加载和回收等待均在短锁外。
    pub fn iget(&self, filesystem: &Arc<Ext4Volume>, number: u64) -> Result<InodeRef, FsError> {
        self.domain.iget(filesystem, number, Some(self))
    }

    /// 把 cache-only 页和真实脏义务交给原 pool 中的 Task，返回已经结束回收的槽数。
    pub fn prune_unused(&self) -> usize {
        self.domain.prune_unused()
    }

    /// 返回 Loading、缓存以及待回收对象共同占用的槽数。
    pub fn len(&self) -> usize {
        self.domain.index.lock().slots.iter().flatten().count()
    }

    /// 观察本缓存域是否没有已发表的 inode。
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 名字索引先释放静止绑定，真实 inode 回收完成后才可重用 canonical 预算。
    pub(crate) fn is_full(&self) -> bool {
        self.domain.is_full()
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

    /// 只观察原 Loading 的等待源，不增加外部使用或改变回收状态。
    #[cfg(feature = "ktest")]
    pub(crate) fn ktest_waiting(&self, filesystem: &Arc<Ext4Volume>, number: u64) -> usize {
        let index = self.domain.index.lock();
        index
            .slots
            .iter()
            .flatten()
            .find(|slot| {
                slot.inode.number == number && Arc::ptr_eq(&slot.inode.filesystem, filesystem)
            })
            .map_or(0, |slot| slot.inode.waiters.test_members())
    }

    /// 取得正式关闭收据；收据保所属索引，外部使用和缓存 owner 的退出仍各自结束。
    pub(crate) fn watch(&self) -> InodeCacheWatch {
        InodeCacheWatch(self.domain.clone())
    }
}

impl Drop for InodeCache {
    /// cache owner 退出只交接原槽；真实同步、页清理及返盘块由预建 Work 在 Task 中完成。
    fn drop(&mut self) {
        let requested = {
            let mut index = self.domain.index.lock();
            index.cache_open = false;
            let mut requested = false;
            for slot in index.slots.iter_mut().flatten() {
                if matches!(slot.eviction, Eviction::Cached | Eviction::Requested) {
                    slot.eviction = Eviction::Requested;
                    requested |= slot.users == 0 && slot.inode.mapping.is_cache_only();
                }
            }
            if requested {
                index.dispatching = index
                    .dispatching
                    .checked_add(1)
                    .expect("inode eviction dispatch overflow");
            }
            requested
        };
        if requested {
            self.domain.queue();
        }
    }
}

impl Clone for InodeRef {
    /// 复制真实外部使用，在同一 canonical 锁内与末使用和回收取得互斥。
    fn clone(&self) -> Self {
        if let Some(position) = self.wb_slot.get().copied() {
            let mut index = self.1.index.lock();
            if let Some(slot) = index.slots[position].as_mut() {
                if Arc::ptr_eq(&slot.inode, &self.0) {
                    slot.users = slot.users.checked_add(1).expect("inode use count overflow");
                }
            }
        }
        Self(self.0.clone(), self.1.clone(), false)
    }
}

impl Drop for InodeRef {
    /// 最后使用在原索引锁内转交回收责任，短锁和 IRQ Drop 都不等待设备或释放盘块。
    fn drop(&mut self) {
        let Some(position) = self.wb_slot.get().copied() else {
            return;
        };
        let requested = {
            let mut index = self.1.index.lock();
            let cache_open = index.cache_open;
            let Some(slot) = index.slots[position].as_mut() else {
                return;
            };
            if !Arc::ptr_eq(&slot.inode, &self.0) {
                return;
            }
            assert!(0 < slot.users, "inode use count lost");
            slot.users -= 1;
            let delete = slot.inode.data.get().is_some_and(|result| {
                result
                    .as_ref()
                    .is_ok_and(|state| state.metadata.lock().links == 0)
            });
            let wanted = !cache_open
                || delete
                || matches!(slot.eviction, Eviction::Requested)
                || !self.2 && matches!(slot.eviction, Eviction::Failed(_));
            if wanted
                && slot.users == 0
                && !matches!(slot.eviction, Eviction::Running)
                && (!self.2 || !matches!(slot.eviction, Eviction::Failed(_)))
                && slot.inode.mapping.is_cache_only()
            {
                slot.eviction = Eviction::Requested;
                index.dispatching = index
                    .dispatching
                    .checked_add(1)
                    .expect("inode eviction dispatch overflow");
                true
            } else {
                false
            }
        };
        if requested {
            self.1.queue();
        }
    }
}

impl InodeRef {
    /// 锁外准备尚未发表的稳定对象与唯一 mapping，任何分配失败均无索引占位。
    fn try_new(
        filesystem: &Arc<Ext4Volume>,
        number: u64,
        domain: &Arc<InodeDomain>,
    ) -> Result<Self, FsError> {
        let mapping = FileMapping::new();
        let inode = Arc::try_new(Inode {
            filesystem: Arc::clone(filesystem),
            #[cfg(feature = "ktest")]
            truncate_hook: SpinLock::new(None),
            number,
            data: Once::new(),
            mapping,
            waiters: WaitQueue::new(),
            wb_slot: Once::new(),
            dirty_obligation: AtomicBool::new(false),
            storage_released: AtomicBool::new(false),
            _pinned: PhantomPinned,
        })
        .map_err(|_| FsError::NoMemory)?;
        Ok(Self(inode, domain.clone(), false))
    }

    /// 比较原对象身份，不以相同卷号、元数据内容或盘上 inode 号代替实例身份。
    pub fn ptr_eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

    /// 在 invalidate 保护及页内容锁内先发布卷义务，再使原页 Dirty 可见；不分配。
    pub(crate) fn note_dirty(&self) {
        self.dirty_obligation.store(true, Ordering::Release);
    }

    /// 数据回写已放开 WB 互斥，按 invalidate→WB 次序排除新写授权并重核原页义务。
    pub(crate) fn refresh_dirty_obligation(&self) -> Result<(), FsError> {
        let _invalidate = self.wait_loaded()?.invalidate.write();
        let _writeback = self.mapping.lock_writeback();
        self.refresh_dirty_obligation_locked();
        Ok(())
    }

    /// 调用者持 invalidate 独占和本 mapping WB 互斥，范围外 Dirty 和真实 WB 都继续保留。
    fn refresh_dirty_obligation_locked(&self) {
        self.dirty_obligation.store(false, Ordering::Release);
        if self.mapping.has_dirty_or_writeback() {
            self.note_dirty();
        }
    }

    /// 同一短计数锁线性化写打开与 exec deny；隐藏 EA 的 ctime 承载引用计数，禁止普通写修改。
    pub fn acquire_write(&self) -> Result<WriteAccess, FsError> {
        let state = self.wait_loaded()?;
        if state.metadata.lock().flags & codec::EA_INODE != 0 {
            return Err(FsError::InvalidOperation);
        }
        let mut count = state.writecount.lock();
        if *count < 0 {
            return Err(FsError::TextBusy);
        }
        *count = count.checked_add(1).expect("write access count overflow");
        Ok(WriteAccess {
            inode: self.clone(),
        })
    }

    /// 第一次映像读取前取得 deny，任何真正写 owner 活时明确 TextBusy。
    pub fn deny_writes(&self) -> Result<ExecAccess, FsError> {
        let state = self.wait_loaded()?;
        let mut count = state.writecount.lock();
        if 0 < *count {
            return Err(FsError::TextBusy);
        }
        *count = count.checked_sub(1).expect("exec access count overflow");
        Ok(ExecAccess {
            inode: self.clone(),
        })
    }

    /// 类型、权限来源和尺寸均在修改前检查；已取得的写 token 排斥所有执行使用。
    fn truncate(&self, claim: &FsClaim, new_size: u64) -> Result<(), FsError> {
        self.filesystem.require_write(claim)?;
        let state = self.wait_loaded()?;
        let _write = state.write.lock();
        let old = *state.metadata.lock();
        if old.kind != InodeKind::Regular || old.flags & codec::EA_INODE != 0 {
            return Err(FsError::InvalidOperation);
        }
        if write_limit(self.filesystem.geometry(), old.flags) < new_size {
            return Err(FsError::FileTooLarge);
        }
        assert!(0 < *state.writecount.lock(), "truncate has no write access");
        let _invalidate = state.invalidate.write();
        self.truncate_locked(state, claim, new_size, false)
    }

    /// 写序列化和 invalidate 独占来自调用者，先发表尺寸再处理旧页和真实 WB，最后摘树。
    /// 页清理失败仍做第二遍撤叶，保新尺寸、原块根和未消费的删除义务，不回滚已发表事实。
    fn truncate_locked(
        &self,
        state: &InodeState,
        claim: &FsClaim,
        new_size: u64,
        deleting: bool,
    ) -> Result<(), FsError> {
        let volume = self.filesystem();
        let g = volume.geometry();
        let old = *state.metadata.lock();
        let now = time::now().as_nanos();
        let mut size_next = write_metadata(g, &old, 0, 0, 0, 0, now)?;
        size_next.size = new_size;
        let tail = if old.size < new_size {
            self.prepare_tail(old.size, new_size)?
        } else if new_size % g.block_size as u64 != 0 {
            self.prepare_tail(
                new_size,
                new_size.div_ceil(g.block_size as u64) * g.block_size as u64,
            )?
        } else {
            None
        };
        // 块边界上的部分页无需读冷页；已有错误尾页必须在尺寸发表前解释。
        if new_size <= old.size
            && new_size % g.block_size as u64 == 0
            && new_size % PAGE_SIZE as u64 != 0
        {
            if let Some(folio) = self.mapping.cached_folio(new_size / PAGE_SIZE as u64) {
                folio.lock_content().range_ready(0, PAGE_SIZE)?;
            }
        }
        // EA 共享引用与所有元数据 WB 使用同一串行保护，隐藏值准备也在父尺寸修改之前。
        let _metadata_writeback = volume.metadata_cache().lock_writeback();
        let (record, record_offset) = volume.inode_record(old.number)?;
        let (prepared, inode_release, old_root) = {
            let root = state.root.write();
            let current = record.with_read(|bytes| {
                codec::decode_inode(
                    g,
                    old.number,
                    &bytes[record_offset..record_offset + g.inode_size],
                )
            })??;
            if InodeMetadata::from_data(&current) != old || current.block != *root {
                return Err(FsError::Corrupt);
            }
            let prepared = volume.prepare_truncate(&old, &root, new_size)?;
            let inode_release: Option<PreparedInodeRelease> = if deleting {
                Some(volume.prepare_inode_release(&current)?)
            } else {
                None
            };
            (prepared, inode_release, *root)
        };
        let mut next = write_metadata(g, &old, 0, 0, 0, prepared.removed_blocks, now)?;
        next.size = new_size;
        record
            .update(claim, |bytes| {
                codec::patch_inode_mapping(
                    g,
                    &mut bytes[record_offset..record_offset + g.inode_size],
                    &size_next,
                    &old_root,
                )
            })
            .expect("prepared truncate size record");
        *state.metadata.lock() = size_next;
        let first = new_size.div_ceil(PAGE_SIZE as u64) * PAGE_SIZE as u64;
        self.mapping.unmap_range(first..u64::MAX, true);
        #[cfg(feature = "ktest")]
        self.truncate_phase(TruncatePhase::AfterFirstUnmap);
        // 块根已放开；旧数据回写可准备地址和完成全尾，不能带 root 等 WB 互斥。
        let _writeback = self.mapping.lock_writeback();
        if let Some(tail) = &tail {
            tail.folio.wait_writeback();
            self.apply_tail(tail);
        }
        self.note_dirty();
        // SAFETY: 同 inode 写序列化、invalidate 独占和 mapping WB 互斥均仍持有；
        // 新 size 已发表，首遍完整尾范围撤叶/CSD 已结束；普通读和新填充受 invalidate 排斥。
        // 旧唯一填充/WB 由原页等待，root 不跨数据等待；错误后也必须执行下一遍 even_cows。
        let pages = unsafe { self.mapping.truncate_pages(new_size, g.block_size) };
        #[cfg(feature = "ktest")]
        self.truncate_phase(TruncatePhase::BeforeSecondUnmap);
        self.mapping.unmap_range(first..u64::MAX, true);
        if let Err(error) = pages {
            return Err(error);
        }
        let mut root = state.root.write();
        assert_eq!(
            *root, old_root,
            "truncate block root changed under write serialization"
        );
        let next_root = prepared.root;
        let retired = prepared.publish(volume, old.checksum_seed);
        record
            .update(claim, |bytes| {
                codec::patch_inode_mapping(
                    g,
                    &mut bytes[record_offset..record_offset + g.inode_size],
                    &next,
                    &next_root,
                )
            })
            .expect("prepared truncate mapping record");
        *root = next_root;
        *state.metadata.lock() = next;
        #[cfg(feature = "ktest")]
        self.truncate_phase(TruncatePhase::BeforeBlockRelease);
        for block in retired {
            // SAFETY: 两遍撤叶/CSD、文件页清理和全部旧数据 WB 已结束；
            // root 写侧已排空旧块查询，元数据 WB 串行排空并阻止旧窗口新提交；
            // 原树已经摘离该块，具体 token 保释放窗口，forget 完成后才允许位图复用。
            unsafe { volume.release_detached_block(block) };
        }
        drop(root);
        drop(tail);
        if let Some(inode_release) = inode_release {
            // SAFETY: 仅真实末使用回收进入 deleting，canonical 槽处于 Running 且没有外部使用；
            // root 写侧已排空后放开，原页和所有旧 data/metadata IO、别名均结束；
            // 元数据 WB 串行仍阻止新提交，EA 原身份和记录由准备窗口保活，inode 位图最后释放。
            unsafe { volume.release_inode(inode_release) };
            self.storage_released.store(true, Ordering::Release);
        }
        self.refresh_dirty_obligation_locked();
        Ok(())
    }

    /// 回收工作已禁止新取得；真正返盘块后仍写出剩余元数据，最后卷拥有不能带 Dirty 退出。
    fn evict(&self) -> Result<(), FsError> {
        sched::assert_switchable();
        let state = self.wait_loaded()?;
        let _write = state.write.lock();
        let old = *state.metadata.lock();
        let writable = self.filesystem.mode() == AccessMode::ReadWrite;
        let deleting = old.links == 0 && writable;
        assert_eq!(
            *state.writecount.lock(),
            0,
            "eviction retains write or exec access"
        );
        if deleting {
            if !self.storage_released.load(Ordering::Acquire) {
                {
                    let _invalidate = state.invalidate.write();
                    self.truncate_locked(state, self.filesystem.claim(), 0, true)?;
                }
                // 后端位已归还，root、invalidate、mapping/metadata WB 和 allocator 保护均已放开。
                #[cfg(feature = "ktest")]
                self.truncate_phase(TruncatePhase::AfterStorageRelease);
            }
        } else if writable {
            writeback::sync_inode(self)?;
        }
        if writable {
            let _metadata_writeback = self.filesystem.metadata_cache().lock_writeback();
            writeback::sync_metadata_locked(self.filesystem())?;
        }
        if deleting {
            return Ok(());
        }
        let _invalidate = state.invalidate.write();
        let _writeback = self.mapping.lock_writeback();
        self.mapping.prune_unused();
        if !self.mapping.is_empty() {
            return Err(FsError::Busy);
        }
        self.refresh_dirty_obligation_locked();
        Ok(())
    }

    /// 父 EA 准备已保当前记录及原身份，元数据 WB 串行排除其他引用修改；最后引用仅清 nlink。
    pub(crate) fn release_ea_references(
        &self,
        claim: &FsClaim,
        record: &BufferRef,
        offset: usize,
        count: u64,
    ) {
        self.filesystem
            .require_write(claim)
            .expect("prepared EA inode write claim");
        let state = self.loaded_state();
        let _write = state.write.lock();
        let _root = state.root.write();
        let mut next = *state.metadata.lock();
        assert!(
            next.flags & codec::EA_INODE != 0,
            "EA reference targets ordinary inode"
        );
        let g = self.filesystem.geometry();
        record
            .update(claim, |bytes| {
                codec::patch_ea_inode_references(
                    g,
                    &mut bytes[offset..offset + g.inode_size],
                    &mut next,
                    count,
                )
            })
            .expect("prepared EA inode reference record");
        *state.metadata.lock() = next;
    }

    /// callback 借真实阶段事实；调用时没有短锁，持有的睡眠锁与生产截断完全一致。
    #[cfg(feature = "ktest")]
    fn truncate_phase(&self, phase: TruncatePhase) {
        let hook = self.truncate_hook.lock().clone();
        if let Some(hook) = hook {
            hook(self, phase);
        }
    }

    /// 只设置既有截断顺序的观察点，旧 callback 在短锁外释放。
    #[cfg(feature = "ktest")]
    pub(crate) fn ktest_set_truncate_hook(&self, hook: Option<TruncateHook>) {
        let previous = core::mem::replace(&mut *self.truncate_hook.lock(), hook);
        drop(previous);
    }

    /// 普通 Task 在 mmap/PTE 保护之外准备当前 EOF 内全部 FS 块，不增长文件长度。
    /// 已发表的块与时间修改在后块失败时保留；未发表资源由原 PreparedMapping 显式取消。
    pub(crate) fn prepare_mkwrite(&self, index: u64) -> Result<PreparedMkwrite, FsError> {
        let volume = self.filesystem();
        volume.require_write(volume.claim())?;
        let _access = self.acquire_write()?;
        let state = self.wait_loaded()?;
        let _write = state.write.lock();
        let _invalidate = state.invalidate.read();
        let metadata = *state.metadata.lock();
        if metadata.kind != InodeKind::Regular {
            return Err(FsError::InvalidOperation);
        }
        let start = index
            .checked_mul(PAGE_SIZE as u64)
            .ok_or(FsError::FileTooLarge)?;
        if metadata.size <= start {
            return Err(FsError::InvalidOperation);
        }
        let folio = self.read_page(index)?;
        let block_size = volume.geometry().block_size;
        for offset in (0..PAGE_SIZE).step_by(block_size) {
            let position = start
                .checked_add(offset as u64)
                .ok_or(FsError::FileTooLarge)?;
            if metadata.size <= position {
                break;
            }
            self.prepare_mkwrite_block(state, &folio, position)?;
        }
        let prepared_sectors = folio.lock_content().file_prepared_sectors();
        Ok(PreparedMkwrite {
            folio,
            index,
            size: metadata.size,
            prepared_sectors,
        })
    }

    /// inode.write 与 invalidate 读侧已经持有；块准备、发表和旧节点归还沿原写后端。
    fn prepare_mkwrite_block(
        &self,
        state: &InodeState,
        folio: &FolioRef,
        position: u64,
    ) -> Result<(), FsError> {
        let volume = self.filesystem();
        let block_size = volume.geometry().block_size;
        let old = *state.metadata.lock();
        let (record, record_offset) = volume.inode_record(old.number)?;
        let prepared = {
            let root = state.root.write();
            let current = record.with_read(|bytes| {
                codec::decode_inode(
                    volume.geometry(),
                    old.number,
                    &bytes[record_offset..record_offset + volume.geometry().inode_size],
                )
            })??;
            if InodeMetadata::from_data(&current) != old || current.block != *root {
                return Err(FsError::Corrupt);
            }
            volume.prepare_write(&old, &root, position / block_size as u64)?
        };
        let next = match write_metadata(
            volume.geometry(),
            &old,
            0,
            0,
            prepared.added_blocks,
            prepared.removed_blocks,
            time::now().as_nanos(),
        ) {
            Ok(next) => next,
            Err(error) => {
                prepared.cancel(volume);
                return Err(error);
            }
        };
        let mut root = state.root.write();
        let mut content = folio.lock_content();
        let offset = (position % PAGE_SIZE as u64) as usize;
        if prepared.zero {
            content
                .zero_file_range(offset, block_size)
                .expect("prepared mkwrite zero range");
            self.note_dirty();
            content
                .mark_dirty(offset, block_size)
                .expect("prepared mkwrite dirty range");
        }
        let next_root = prepared.root;
        let retired = prepared.publish(volume, old.checksum_seed);
        record
            .update(volume.claim(), |bytes| {
                codec::patch_inode_mapping(
                    volume.geometry(),
                    &mut bytes[record_offset..record_offset + volume.geometry().inode_size],
                    &next,
                    &next_root,
                )
            })
            .expect("prepared writable mkwrite inode record");
        *root = next_root;
        *state.metadata.lock() = next;
        content
            .mark_file_prepared(offset, block_size)
            .expect("published mkwrite block");
        drop(content);
        for block in retired {
            // SAFETY: root 写侧已排空旧读者，发表的新树已摘除原节点，写侧禁止新使用；
            // release 等旧元数据真实 WB 并失效准确窗口，然后才归还物理位图。
            unsafe { volume.release_detached_block(block) };
        }
        Ok(())
    }

    /// 最终授权在 mmap 之后、folio 之前取得读侧，增长与失效使用同一锁的写侧。
    pub(crate) fn lock_invalidate(&self) -> Result<ReadGuard<'_, ()>, FsError> {
        Ok(self.wait_loaded()?.invalidate.read())
    }

    /// 持 mmap→invalidate 读侧→本页内容锁重核准备事实，不等待 WB 或取得块根。
    /// 真实 size、页身份或准备事实变化返回 false；coherent 文件原页允许 Dirty 与 WB 并存。
    pub(crate) fn finish_mkwrite(
        &self,
        prepared: &PreparedMkwrite,
        content: &mut FolioContent<'_>,
    ) -> Result<bool, FsError> {
        let metadata = *self.loaded_state().metadata.lock();
        let start = prepared
            .index
            .checked_mul(PAGE_SIZE as u64)
            .ok_or(FsError::FileTooLarge)?;
        if metadata.size != prepared.size
            || metadata.size <= start
            || content.folio().phys_addr() != prepared.folio.phys_addr()
            || !self.mapping.is_canonical(prepared.index, &prepared.folio)
            || content.file_prepared_sectors() != prepared.prepared_sectors
        {
            return Ok(false);
        }
        let block_size = self.filesystem.geometry().block_size;
        self.note_dirty();
        content.mark_prepared_dirty(
            (metadata.size - start).min(PAGE_SIZE as u64) as usize,
            block_size,
        )?;
        Ok(true)
    }

    /// 唯一普通写循环只由绑定本 inode 的 WriteAccess 调用；输入 fault 不持块根或内容锁。
    fn write_from(
        &self,
        claim: &FsClaim,
        offset: u64,
        append: bool,
        source: &mut WriteBuffer<'_>,
    ) -> Result<(u64, usize), FsError> {
        self.filesystem.require_write(claim)?;
        let state = self.wait_loaded()?;
        let _write = state.write.lock();
        let initial = *state.metadata.lock();
        if initial.kind == InodeKind::Directory {
            return Err(FsError::IsDirectory);
        }
        if initial.kind != InodeKind::Regular {
            return Err(FsError::InvalidOperation);
        }
        let start = if append { initial.size } else { offset };
        let limit = write_limit(self.filesystem.geometry(), initial.flags);
        let remaining = source.len() - source.copied();
        let mut accepted = 0usize;
        let mut input = [0u8; 512];
        while accepted < remaining {
            let position = match start.checked_add(accepted as u64) {
                Some(position) if position < limit => position,
                _ => return write_result(start, accepted, FsError::FileTooLarge),
            };
            let size = self.filesystem.geometry().block_size;
            let count = (remaining - accepted)
                .min(input.len())
                .min(size - (position % size as u64) as usize)
                .min((limit - position) as usize);
            let pulled = match source.pull(&mut input[..count]) {
                Ok(0) => break,
                Ok(count) => count,
                Err(error) => return write_result(start, accepted, error),
            };
            let old = *state.metadata.lock();
            // 输入已真正取得后才进入失效域，用户源 fault 不携带目的 invalidate guard。
            let end = position + pulled as u64;
            let _invalidate = state.invalidate.write();
            if old.size < end {
                if let Err(error) = self.zero_eof_tail(&old, position) {
                    return write_result(start, accepted, error);
                }
            }
            let (record, record_offset) = match self.filesystem.inode_record(old.number) {
                Ok(record) => record,
                Err(error) => return write_result(start, accepted, error),
            };
            let prepared = {
                let root = state.root.write();
                let current = record.with_read(|bytes| {
                    crate::fs::ext4::codec::decode_inode(
                        self.filesystem.geometry(),
                        old.number,
                        &bytes
                            [record_offset..record_offset + self.filesystem.geometry().inode_size],
                    )
                });
                let valid = match current {
                    Ok(Ok(current))
                        if InodeMetadata::from_data(&current) == old && current.block == *root =>
                    {
                        Ok(())
                    }
                    Ok(Ok(_)) => Err(FsError::Corrupt),
                    Ok(Err(error)) => Err(error),
                    Err(error) => Err(error.into()),
                };
                if let Err(error) = valid {
                    return write_result(start, accepted, error);
                }
                self.filesystem
                    .prepare_write(&old, &root, position / size as u64)
            };
            let prepared = match prepared {
                Ok(prepared) => prepared,
                Err(error) => return write_result(start, accepted, error),
            };
            let next = match write_metadata(
                self.filesystem.geometry(),
                &old,
                position,
                pulled,
                prepared.added_blocks,
                prepared.removed_blocks,
                time::now().as_nanos(),
            ) {
                Ok(next) => next,
                Err(error) => {
                    prepared.cancel(&self.filesystem);
                    return write_result(start, accepted, error);
                }
            };
            // 尚未发表的映射保持旧根；filefault 可取得读侧完成目的页初始化。
            let folio = match self.read_page(position / PAGE_SIZE as u64) {
                Ok(folio) => folio,
                Err(error) => {
                    prepared.cancel(&self.filesystem);
                    return write_result(start, accepted, error);
                }
            };
            let mut root = state.root.write();
            let mut content = folio.lock_content();
            let block_offset = ((position / size as u64 * size as u64) % PAGE_SIZE as u64) as usize;
            self.note_dirty();
            if prepared.zero {
                let zeros = [0u8; 512];
                for part in (block_offset..block_offset + size).step_by(zeros.len()) {
                    content
                        .copy_from(part, &zeros)
                        .expect("prepared writable file block");
                }
            }
            content
                .copy_from((position % PAGE_SIZE as u64) as usize, &input[..pulled])
                .expect("prepared writable file bytes");
            content
                .mark_dirty(block_offset, size)
                .expect("prepared file block dirty range");
            let next_root = prepared.root;
            let retired = prepared.publish(&self.filesystem, old.checksum_seed);
            record
                .update(claim, |bytes| {
                    crate::fs::ext4::codec::patch_inode_mapping(
                        self.filesystem.geometry(),
                        &mut bytes
                            [record_offset..record_offset + self.filesystem.geometry().inode_size],
                        &next,
                        &next_root,
                    )
                })
                .expect("prepared writable inode record");
            *root = next_root;
            *state.metadata.lock() = next;
            content
                .mark_file_prepared(block_offset, size)
                .expect("published buffered write block");
            drop(content);
            for block in retired {
                // SAFETY: root 写侧已排空旧块树读者，发表后的根和父节点均已去掉旧节点；
                // 写侧仍阻止新读者，释放窗口保原页，forget 等真实元数据 IO/WB 收权后失效，
                // 此后旧窗口不再被映射查询使用，位图最后才允许物理块复用。
                unsafe {
                    self.filesystem.release_detached_block(block);
                }
            }
            drop(root);
            accepted += pulled;
        }
        Ok((start, accepted))
    }
    /// 内核按位置读取与用户接收端使用同一页交付循环。
    pub fn read_at(&self, offset: u64, dst: &mut [u8]) -> Result<usize, FsError> {
        self.read_into(offset, &mut ReadBuffer::kernel(dst))
    }

    /// invalidate 共享侧保护本片选择、填充和消费；用户接收端 fault 在放锁后发生。
    pub(crate) fn read_into(
        &self,
        offset: u64,
        target: &mut ReadBuffer<'_>,
    ) -> Result<usize, FsError> {
        let initial = target.copied();
        let remaining = target.len() - initial;
        if remaining == 0 {
            return Ok(0);
        }
        let state = self.wait_loaded()?;
        let data = *state.metadata.lock();
        if data.kind == InodeKind::Directory {
            return Err(FsError::IsDirectory);
        }
        if data.kind != InodeKind::Regular {
            return Err(FsError::InvalidOperation);
        }
        if data.size <= offset {
            return Ok(0);
        }
        let len = (data.size - offset).min(remaining as u64) as usize;
        let mut copied = 0;
        let mut staging = [0u8; 512];
        while copied < len {
            let position = offset + copied as u64;
            let in_page = (position % PAGE_SIZE as u64) as usize;
            let mut count = (len - copied).min(staging.len()).min(PAGE_SIZE - in_page);
            let result = {
                let _invalidate = state.invalidate.read();
                let current = *state.metadata.lock();
                if current.size <= position {
                    return Ok(copied);
                }
                count = count.min((current.size - position) as usize);
                self.read_page(position / PAGE_SIZE as u64)
                    .and_then(|folio| {
                        folio
                            .copy_to(in_page, &mut staging[..count])
                            .map_err(FsError::from)
                    })
            };
            let result = result.and_then(|()| target.push(&staging[..count]));
            copied = target.copied() - initial;
            if let Err(error) = result {
                return if copied == 0 { Err(error) } else { Ok(copied) };
            }
        }
        Ok(copied)
    }

    /// invalidate 共享侧保护原页选择及唯一填充，映射提交仍须另核 EOF、canonical 和当前叶。
    pub(crate) fn file_page(&self, index: u64) -> Result<FolioRef, FsError> {
        let _invalidate = self.lock_invalidate()?;
        let data = self.metadata_snapshot()?;
        if data.kind != InodeKind::Regular {
            return Err(FsError::InvalidOperation);
        }
        self.read_page(index)
    }

    /// 普通增长写先准备尾页和物理块事实，再撤旧共享 W 并清将变有效的 gap。
    fn zero_eof_tail(&self, old: &InodeMetadata, position: u64) -> Result<(), FsError> {
        if let Some(tail) = self.prepare_tail(old.size, position)? {
            self.apply_tail(&tail);
        }
        Ok(())
    }

    /// 写序列化和 invalidate 保护下准备准确尾区间；本阶段不改原字节或脏义务。
    fn prepare_tail(&self, start: u64, end: u64) -> Result<Option<PreparedTail>, FsError> {
        let offset = (start % PAGE_SIZE as u64) as usize;
        if offset == 0 {
            return Ok(None);
        }
        let index = start / PAGE_SIZE as u64;
        let folio = self.read_page(index)?;
        let size = self.filesystem.geometry().block_size;
        let end = end
            .saturating_sub(index * PAGE_SIZE as u64)
            .min(PAGE_SIZE as u64) as usize;
        let end = end.max(offset);
        let mapped = self.with_map_read(|metadata, root| {
            let mut mapped = 0u8;
            for in_page in (offset / size * size..end).step_by(size) {
                let logical = (index * PAGE_SIZE as u64 + in_page as u64) / size as u64;
                if matches!(
                    self.filesystem.map_block(metadata, root, logical)?,
                    BlockMapping::Mapped { .. }
                ) {
                    mapped |= 1 << (in_page / size);
                }
            }
            Ok(mapped)
        })?;
        Ok(Some(PreparedTail {
            folio,
            index,
            start: offset,
            end,
            mapped,
        }))
    }

    /// 撤旧共享 W/CSD 后零化原页尾；仅真实 Mapped 块产生 Dirty，原准备位不扩成新分配。
    fn apply_tail(&self, tail: &PreparedTail) {
        let mut content = tail.folio.lock_content();
        self.mapping.mkclean(tail.index, &tail.folio, &mut content);
        if tail.start == tail.end {
            return;
        }
        content
            .zero_file_range(tail.start, tail.end - tail.start)
            .expect("prepared file tail zero range");
        let size = self.filesystem.geometry().block_size;
        for offset in (tail.start / size * size..tail.end).step_by(size) {
            if tail.mapped & (1 << (offset / size)) != 0 {
                self.note_dirty();
                content
                    .mark_dirty(offset, size)
                    .expect("prepared mapped tail dirty range");
            }
        }
    }

    /// 调用者持 invalidate 共享或独占侧；根只准备地址，原 IO owner 保 inode 到设备真实全尾。
    fn read_page(&self, index: u64) -> Result<FolioRef, FsError> {
        let (folio, read) = self.mapping.get_or_prepare(index)?;
        if let Some(read) = read {
            let read = read.with_file_owner(self.clone());
            let (size, targets) = match writeback::prepare_file_targets(self, index, &(0..u64::MAX))
            {
                Ok(prepared) => prepared,
                Err(error) => return Err(finish_error(read, error)),
            };
            self.filesystem.fill_page(index, size, &targets, read)?;
        }
        folio.wait_ready()?;
        Ok(folio.with_file_owner(self.clone()))
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

    /// 等待唯一加载结果；属性与块根的访问必须继续取得各自规定的锁。
    fn wait_loaded(&self) -> Result<&InodeState, FsError> {
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

    /// token 只能在加载成功后构造，析构路径不再等待加载或创建新的责任。
    fn loaded_state(&self) -> &InodeState {
        self.data
            .get()
            .expect("access token precedes inode load")
            .as_ref()
            .expect("access token retains a failed inode")
    }

    /// 短属性锁内复制当前运行值，不将 Spin guard 带入 IO、等待或用户复制。
    pub fn metadata_snapshot(&self) -> Result<InodeMetadata, FsError> {
        Ok(*self.wait_loaded()?.metadata.lock())
    }

    /// 睡眠 data-sem 读侧保持块映射有效；属性快照的短锁在调用闭包前已经释放。
    pub fn with_map_read<T>(
        &self,
        consume: impl FnOnce(&InodeMetadata, &[u8; 60]) -> Result<T, FsError>,
    ) -> Result<T, FsError> {
        let state = self.wait_loaded()?;
        let root = state.root.read();
        let metadata = *state.metadata.lock();
        consume(&metadata, &root)
    }

    /// 在块映射读保护下执行真实后端查询，不导出永久块根借用。
    pub fn map_block(&self, logical: u64) -> Result<BlockMapping, FsError> {
        self.with_map_read(|metadata, root| self.filesystem.map_block(metadata, root, logical))
    }

    /// inode 写序列化与 data-sem 写侧保护受控属性修改，并字段级更新共同盘面记录。
    /// 此入口不分配块、不截断文件；大小和块布局变更须由对应内容协议实现。
    pub fn update_metadata<T>(
        &self,
        claim: &FsClaim,
        update: impl FnOnce(&mut InodeMetadata) -> T,
    ) -> Result<T, FsError> {
        if claim.mode() != AccessMode::ReadWrite || self.filesystem.mode() != AccessMode::ReadWrite
        {
            return Err(FsError::ReadOnly);
        }
        if !core::ptr::eq(
            claim.disk().as_ref().get_ref(),
            self.filesystem.disk().as_ref().get_ref(),
        ) {
            return Err(FsError::InvalidOperation);
        }
        let state = self.wait_loaded()?;
        let _write = state.write.lock();
        let _root = state.root.write();
        let old = *state.metadata.lock();
        if old.flags & codec::EA_INODE != 0 {
            return Err(FsError::InvalidOperation);
        }
        let mut next = old;
        let result = update(&mut next);
        self.filesystem.update_inode_metadata(claim, &old, &next)?;
        *state.metadata.lock() = next;
        Ok(result)
    }

    /// 内核加载或范围扫描借用唯一页索引，须保对应 InodeRef 到裸页使用与 IO 全尾结束。
    /// 普通文件页消费使用 InodeRef 的出口，交给外部的页引用不能缺少原 inode anchor。
    pub(crate) fn mapping(&self) -> &FileMapping {
        &self.mapping
    }

    /// 原始单组件名字经真实目录后端查找，再在同缓存域获取硬链接的唯一对象。
    pub fn lookup(&self, cache: &InodeCache, name: &[u8]) -> Result<InodeRef, FsError> {
        let number = self
            .with_map_read(|metadata, root| self.filesystem.lookup(metadata, root, name))?
            .ok_or(FsError::NotFound)?;
        cache.iget(&self.filesystem, number)
    }

    /// 沿盘上字节位置枚举下一个有效目录项，不构造整目录副本。
    pub fn dir_next(&self, offset: u64) -> Result<Option<DirectoryEntry>, FsError> {
        self.with_map_read(|metadata, root| self.filesystem.dir_next(metadata, root, offset))
    }

    /// 由后端按真实 inode 存储规则返回符号链接的原始字节。
    pub fn read_link(&self) -> Result<Vec<u8>, FsError> {
        self.with_map_read(|metadata, root| self.filesystem.read_link(metadata, root))
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
        let result = self.inode.data.call_once(|| result.map(InodeState::new));
        if result.is_err() {
            self.domain.remove_failed(self.inode);
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

/// 只有同一索引锁内取得的使用才能公开；已经运行的回收禁止重取原身份。
fn find(
    index: &mut InodeIndex,
    domain: &Arc<InodeDomain>,
    filesystem: &Arc<Ext4Volume>,
    number: u64,
) -> Result<Option<InodeRef>, FsError> {
    let Some(slot) = index.slots.iter_mut().flatten().find(|slot| {
        slot.inode.number == number && Arc::ptr_eq(&slot.inode.filesystem, filesystem)
    }) else {
        return Ok(None);
    };
    match slot.eviction {
        Eviction::Cached => {}
        Eviction::Requested => {
            // 排队尚未取得回收执行责任；有链接的缓存命中可在同锁取消这次淘汰请求。
            let linked = slot.inode.data.get().is_some_and(|result| {
                result
                    .as_ref()
                    .is_ok_and(|state| 0 < state.metadata.lock().links)
            });
            if !linked {
                return Err(FsError::Busy);
            }
            slot.eviction = Eviction::Cached;
        }
        Eviction::Running => return Err(FsError::Busy),
        Eviction::Failed(error) => return Err(error),
    }
    Ok(Some(use_slot(domain, slot, false)))
}

/// 调用者持 canonical 索引锁，缓存强引用不计作使用；回收者与普通使用共用准确计数。
fn use_slot(domain: &Arc<InodeDomain>, slot: &mut InodeSlot, worker: bool) -> InodeRef {
    slot.users = slot.users.checked_add(1).expect("inode use count overflow");
    InodeRef(slot.inode.clone(), domain.clone(), worker)
}

/// 失败义务移到具体服务的拥有链，既不弱升级丢责任，也不建立 domain 自保持环。
fn retain_failed(domain: &Arc<InodeDomain>) {
    let mut head = FAILED_EVICTIONS.lock();
    if !domain.retained.swap(true, Ordering::Relaxed) {
        *domain.retained_next.lock() = head.take();
        *head = Some(domain.clone());
    }
}

/// 最后失败槽已经结束后摘原域，链只移动现有 Arc，最后析构在服务锁外执行。
fn release_retained(domain: &Arc<InodeDomain>) {
    let mut head = FAILED_EVICTIONS.lock();
    if !domain.retained.load(Ordering::Relaxed) {
        return;
    }
    let mut kept = None;
    let mut retired = None;
    while let Some(entry) = head.take() {
        *head = entry.retained_next.lock().take();
        if Arc::ptr_eq(&entry, domain) {
            entry.retained.store(false, Ordering::Relaxed);
            retired = Some(entry);
        } else {
            *entry.retained_next.lock() = kept.take();
            kept = Some(entry);
        }
    }
    *head = kept;
    drop(head);
    drop(retired);
}

/// 每份被接受的 Work 票消费一份真实域拥有；旧 IO 全尾和后端释放均在原 pool Task 中执行。
fn evict_inodes(work: Pin<&Work>) {
    // SAFETY: queue 在 Work 接受前用 into_raw 移交一次真实域强引用；
    // callback 的这一票是唯一 from_raw 消费者，域和整数 context 指向的原分配全程存活。
    let domain = unsafe { Arc::from_raw(work.context() as *const InodeDomain) };
    loop {
        let selected = {
            let mut index = domain.index.lock();
            index
                .slots
                .iter_mut()
                .enumerate()
                .find_map(|(position, slot)| {
                    let slot = slot.as_mut()?;
                    if slot.users != 0
                        || !matches!(slot.eviction, Eviction::Requested)
                        || !slot.inode.mapping.is_cache_only()
                    {
                        return None;
                    }
                    slot.eviction = Eviction::Running;
                    Some((position, use_slot(&domain, slot, true)))
                })
        };
        let Some((position, inode)) = selected else {
            break;
        };
        let result = inode.evict();
        let victim = {
            let mut index = domain.index.lock();
            let slot = index.slots[position]
                .as_mut()
                .expect("running inode eviction lost its slot");
            assert!(
                Arc::ptr_eq(&slot.inode, &inode.0),
                "inode eviction identity changed"
            );
            match result {
                Ok(()) => {
                    assert_eq!(slot.users, 1, "evicted inode retains external users");
                    assert!(
                        slot.inode.mapping.is_empty(),
                        "evicted inode retains cached pages"
                    );
                    index.evicted = index
                        .evicted
                        .checked_add(1)
                        .expect("inode eviction count overflow");
                    index.slots[position].take()
                }
                Err(error) => {
                    slot.eviction = Eviction::Failed(error);
                    None
                }
            }
        };
        if result.is_err() {
            retain_failed(&domain);
        }
        domain.waiters.wake_all();
        drop(victim);
        drop(inode);
    }
    let failed = domain
        .index
        .lock()
        .slots
        .iter()
        .flatten()
        .any(|slot| matches!(slot.eviction, Eviction::Failed(_)));
    if !failed {
        release_retained(&domain);
    }
}

/// 正前缀已经被文件接受后保留它；零前缀保留具体的首次失败原因。
fn write_result(start: u64, accepted: usize, error: FsError) -> Result<(u64, usize), FsError> {
    if accepted == 0 {
        Err(error)
    } else {
        Ok((start, accepted))
    }
}

/// 有限最大文件位置由实际布局决定，传统树容量与 extent 的 32 位逻辑块号分别计算。
fn write_limit(g: &crate::fs::ext4::Geometry, flags: u32) -> u64 {
    let blocks = if flags & crate::fs::ext4::codec::EXTENTS != 0 {
        1u64 << 32
    } else {
        let n = (g.block_size / 4) as u64;
        12 + n + n * n + n * n * n
    };
    (blocks * g.block_size as u64).min(i64::MAX as u64)
}

/// 根据真实接受范围、块净增量与当前时间计算下一属性，未发表失败不修改原状态。
fn write_metadata(
    g: &crate::fs::ext4::Geometry,
    old: &InodeMetadata,
    position: u64,
    count: usize,
    added: u64,
    removed: u64,
    now: u64,
) -> Result<InodeMetadata, FsError> {
    let sectors = (g.block_size / 512) as u64;
    let blocks = old
        .blocks
        .checked_add(added * sectors)
        .ok_or(FsError::FileTooLarge)?;
    let blocks = blocks
        .checked_sub(removed * sectors)
        .ok_or(FsError::Corrupt)?;
    let (_, huge) = codec::blocks_encoding(g, blocks)?;
    let (mtime, ctime) = codec::prepare_inode_write_times(now, old.extra_isize)?;
    let mut next = *old;
    next.size = old.size.max(
        position
            .checked_add(count as u64)
            .ok_or(FsError::FileTooLarge)?,
    );
    next.blocks = blocks;
    next.flags = (old.flags & !0x40000) | if huge { 0x40000 } else { 0 };
    next.mtime = mtime;
    next.ctime = ctime;
    Ok(next)
}
