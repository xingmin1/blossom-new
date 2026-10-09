//! 元数据视图共用 Disk 的物理页；文件页使用同一稀疏索引和原页状态。

mod index;

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::convert::TryFrom;
use core::ops::Range;

use crate::block::{
    AccessMode, BioAllocator, BioMemory, BioReserve, BlockError, FsClaim, MetadataIoContext,
    SECTOR_BYTES,
};
use crate::fs::ext4::Ext4Volume;
use crate::fs::{FsError, InodeRef};
use crate::mm::folio::FolioContent;
use crate::mm::reclaim::{ReclaimAttempt, ReclaimStats};
use crate::mm::space::{sync_all, PteDomain, ResidentPages};
use crate::mm::user_table::RetiredTables;
use crate::mm::{FolioRead, FolioRef, PAGE_SIZE};
use crate::sync::{SleepMutex, SleepMutexGuard, SleepRwLock, SpinLock, SpinLockGuard};
use crate::task::{ReclaimMode, ReclaimScope};
use index::{IndexAllocator, Node, NodeRef, RetiredNode, Tree};

/// 内容状态从节点自己的 folio 读取；ToWrite 只标记本轮选中的原节点。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FolioMark {
    Dirty,
    Writeback,
    ToWrite,
}

const ERRSEQ_ERRNO: u32 = 4095;
const ERRSEQ_SEEN: u32 = 1 << 12;
const ERRSEQ_INCREMENT: u32 = 1 << 13;

/// 固定宽度错误序列保留最后具体错误，不建立逐次错误链。
struct WritebackError {
    value: u32,
    error: Option<FsError>,
}

impl WritebackError {
    /// 零值表示从未发生错误，构造不分配存储。
    const fn new() -> Self {
        Self {
            value: 0,
            error: None,
        }
    }

    /// 未观察错误从零采样，使新观察者仍能消费既有未见错误。
    fn sample(&self) -> u32 {
        if self.value & ERRSEQ_SEEN == 0 {
            0
        } else {
            self.value
        }
    }

    /// 仅在已有错误被观察后推进有限计数，保留低十二位真实 errno。
    fn record(&mut self, error: FsError) {
        let mut next = (self.value & !(ERRSEQ_ERRNO | ERRSEQ_SEEN)) | writeback_errno(error);
        if self.value & ERRSEQ_SEEN != 0 {
            next = next.wrapping_add(ERRSEQ_INCREMENT);
        }
        self.value = next;
        self.error = Some(error);
    }

    /// 调用者串行自己的游标，短锁内标 SEEN 并只报告相对游标的新错误。
    fn check_and_advance(&mut self, since: &mut u32) -> Result<(), FsError> {
        if self.value == *since {
            return Ok(());
        }
        self.value |= ERRSEQ_SEEN;
        *since = self.value;
        Err(self.error.expect("writeback errseq has no error"))
    }
}

/// 内部错误保留具体类型，序列低位只编码对应的 Linux errno。
fn writeback_errno(error: FsError) -> u32 {
    match error {
        FsError::NoMemory | FsError::Io(BlockError::NoMemory) => 12,
        FsError::NoSpace => 28,
        FsError::TextBusy => 26,
        FsError::FileTooLarge => 27,
        FsError::Unsupported | FsError::Io(BlockError::Unsupported) => 95,
        FsError::NotFound | FsError::MissingRoot => 2,
        FsError::Exists => 17,
        FsError::NotEmpty => 39,
        FsError::CrossDevice => 18,
        FsError::TooManyLinks => 31,
        FsError::NameTooLong => 36,
        FsError::NotPermitted => 1,
        FsError::NotDirectory => 20,
        FsError::InvalidName
        | FsError::InvalidOperation
        | FsError::Io(BlockError::InvalidRange | BlockError::Misaligned) => 22,
        FsError::Busy | FsError::Io(BlockError::Busy) => 16,
        FsError::TooManySymlinks => 40,
        FsError::BadFd => 9,
        FsError::TooManyFiles => 24,
        FsError::ReadOnly | FsError::Io(BlockError::ReadOnly) => 30,
        FsError::IsDirectory => 21,
        FsError::BadAddress => 14,
        FsError::Permission => 13,
        FsError::Corrupt | FsError::Checksum | FsError::LoadAborted | FsError::Io(_) => 5,
    }
}

/// 验收同一生产算法的未见采样、独立游标、具体错误和有限计数回绕，不产生 IO。
#[cfg(feature = "ktest")]
pub(crate) fn ktest_wb_error_protocol() -> bool {
    let mut error = WritebackError::new();
    let mut first = error.sample();
    if error.check_and_advance(&mut first) != Ok(()) {
        return false;
    }
    error.record(FsError::NoMemory);
    error.record(FsError::NoSpace);
    if error.value != 28 || error.sample() != 0 {
        return false;
    }
    let mut second = error.sample();
    if error.check_and_advance(&mut first) != Err(FsError::NoSpace)
        || error.check_and_advance(&mut second) != Err(FsError::NoSpace)
        || error.check_and_advance(&mut first) != Ok(())
        || first != second
        || error.sample() != first
    {
        return false;
    }
    error.record(FsError::Io(BlockError::ShortIo));
    if error.value != ERRSEQ_INCREMENT | 5
        || error.sample() != 0
        || error.check_and_advance(&mut first) != Err(FsError::Io(BlockError::ShortIo))
    {
        return false;
    }
    // 直接准备最大有限计数，回绕仍调用正式 record，不循环制造设备错误。
    error.value = !(ERRSEQ_ERRNO | ERRSEQ_SEEN) | ERRSEQ_SEEN | 5;
    error.record(FsError::NoMemory);
    error.value == 12
        && error.sample() == 0
        && error.check_and_advance(&mut second) == Err(FsError::NoMemory)
        && error.check_and_advance(&mut second) == Ok(())
}

pub(crate) struct FolioIndex {
    entries: SpinLock<Tree>,
    capacity: Option<usize>,
    metadata: bool,
}

impl FolioIndex {
    /// 文件索引不预留固定槽；元数据预算限制的是实际物理页数量。
    pub(crate) const fn new(capacity: Option<usize>, metadata: bool) -> Self {
        Self {
            entries: SpinLock::new(Tree::new()),
            capacity,
            metadata,
        }
    }

    /// 元数据满预算只摘实际索引及可选源基准以外无拥有的静止页，锁外结束原节点。
    fn evict_if_full(
        &self,
        entries: &mut Tree,
        reserve: Option<&Arc<BioReserve>>,
    ) -> Result<Option<RetiredNode>, BlockError> {
        if self.capacity.is_none_or(|capacity| entries.len < capacity) {
            return Ok(None);
        }
        let mut cursor = 0;
        while let Some(node) = entries.lower_bound(cursor) {
            let key = node.key;
            let baseline = reserve.is_some_and(|source| source.metadata_has_baseline(&node.folio));
            if node.folio.index_unused(baseline, false) {
                return Ok(entries.remove(key));
            }
            let Some(next) = key.checked_add(1) else {
                break;
            };
            cursor = next;
        }
        Err(BlockError::NoMemory)
    }

    /// raw/RO 从原 NOWAIT 来源准备；RW metadata 只从同 cache 原页及控制槽准备。
    pub(crate) fn get_or_prepare(
        &self,
        key: u64,
        reserve: Option<&Arc<BioReserve>>,
    ) -> Result<(FolioRef, Option<FolioRead>), BlockError> {
        let victim = {
            let mut entries = self.entries.lock();
            if entries.closed {
                return Err(BlockError::QueueStopped);
            }
            if let Some(node) = entries.get(key) {
                return Ok((node.folio.clone(), None));
            }
            self.evict_if_full(&mut entries, reserve)?
        };
        if let Some(victim) = victim {
            victim.release();
        }
        let (candidate, allocator, lease) = if let Some(reserve) = reserve {
            assert!(self.metadata, "file/raw index received a metadata source");
            let lease = reserve.try_metadata_slot()?;
            let candidate = lease.folio();
            candidate.reset_metadata_inventory();
            let allocator = IndexAllocator::Metadata {
                reserve: Arc::clone(reserve),
                slot: lease.slot(),
            };
            (candidate, allocator, Some(lease))
        } else {
            let candidate = if self.metadata {
                FolioRef::allocate_metadata()
            } else {
                FolioRef::allocate()
            }
            .ok_or(BlockError::NoMemory)?;
            (candidate, IndexAllocator::Heap, None)
        };
        let read = if self.metadata {
            None
        } else {
            Some(candidate.begin_read(0, PAGE_SIZE)?)
        };
        let mut prepared = Some(Node::prepare(key, candidate.clone(), None, allocator)?);
        drop(lease);
        let (existing, victim) = {
            let mut entries = self.entries.lock();
            if entries.closed {
                return Err(BlockError::QueueStopped);
            }
            if let Some(node) = entries.get(key) {
                (Some(node.folio.clone()), None)
            } else {
                let victim = self.evict_if_full(&mut entries, reserve)?;
                entries.insert(prepared.take().unwrap());
                (None, victim)
            }
        };
        if let Some(victim) = victim {
            victim.release();
        }
        drop(prepared);
        if let Some(folio) = existing {
            drop(read);
            return Ok((folio, None));
        }
        Ok((candidate, read))
    }

    /// 每次只移一个节点，源基准通过真实库存身份计入，原页和控制均在短锁外结束。
    pub(crate) fn prune_unused(&self, reserve: Option<&Arc<BioReserve>>) -> usize {
        let budget = self.len();
        let mut cursor = 0;
        let mut removed = 0;
        for _ in 0..budget {
            let (victim, next) = {
                let mut entries = self.entries.lock();
                let Some(node) = entries.lower_bound(cursor) else {
                    break;
                };
                let key = node.key;
                let baseline =
                    reserve.is_some_and(|source| source.metadata_has_baseline(&node.folio));
                let victim = if node.folio.index_unused(baseline, false) {
                    entries.remove(key)
                } else {
                    None
                };
                (victim, key.checked_add(1))
            };
            removed += usize::from(victim.is_some());
            if let Some(victim) = victim {
                victim.release();
            }
            let Some(next) = next else {
                break;
            };
            cursor = next;
        }
        removed
    }

    /// 读取当前真实节点数，读入中与失败节点仍占元数据预算。
    pub(crate) fn len(&self) -> usize {
        self.entries.lock().len
    }

    /// 单向标记入口有限节点中的限量 Dirty/WB，返回选页数和下一轮起点。
    /// 扫到范围末尾时续点归零，本轮不回绕；零预算保持原起点。
    fn mark_to_write_range(&self, start: u64, end: u64, max_pages: usize) -> (usize, u64) {
        if max_pages == 0 {
            return (0, start);
        }
        let mut entries = self.entries.lock();
        let budget = entries.len;
        let mut cursor = start;
        let mut selected = 0;
        for _ in 0..budget {
            let Some(node) = entries.lower_bound(cursor) else {
                break;
            };
            let key = node.key;
            if end <= key {
                break;
            }
            if selected == max_pages {
                return (selected, cursor);
            }
            let (dirty, writeback) = node.folio.writeback_state();
            let pending = dirty != 0 || writeback != 0;
            entries.set_to_write(key, pending);
            selected += usize::from(pending);
            let Some(next) = key.checked_add(1) else {
                break;
            };
            cursor = next;
        }
        (selected, 0)
    }

    /// 直接核原页的成对状态，索引锁下不复制脏页名单或等待内容锁。
    fn has_dirty_or_writeback(&self) -> bool {
        let entries = self.entries.lock();
        let mut cursor = 0;
        while let Some(node) = entries.lower_bound(cursor) {
            let (dirty, writeback) = node.folio.writeback_state();
            if dirty != 0 || writeback != 0 {
                return true;
            }
            let Some(next) = node.key.checked_add(1) else {
                break;
            };
            cursor = next;
        }
        false
    }

    /// 按半开范围有界选择；ToWrite填满输出片段，检查节点数不超过锁内现存有限数量。
    pub(crate) fn select_range(
        &self,
        start: u64,
        end: u64,
        mark: FolioMark,
        output: &mut [Option<(u64, FolioRef)>],
    ) -> Option<u64> {
        let entries = self.entries.lock();
        let mut cursor = start;
        let mut selected = 0;
        let budget = if mark == FolioMark::ToWrite {
            entries.len
        } else {
            output.len()
        };
        for _ in 0..budget {
            if selected == output.len() {
                break;
            }
            let node = entries.lower_bound(cursor)?;
            if end <= node.key {
                return None;
            }
            let matches = match mark {
                FolioMark::Dirty => node.folio.dirty_sectors() != 0,
                FolioMark::Writeback => node.folio.writeback_sectors() != 0,
                FolioMark::ToWrite => node.to_write(),
            };
            if matches {
                assert!(
                    output[selected].is_none(),
                    "folio scan output retains an owner"
                );
                output[selected] = Some((node.key, node.folio.clone()));
                selected += 1;
            }
            cursor = node.key.checked_add(1)?;
        }
        (cursor < end).then_some(cursor)
    }
}

/// 元数据槽已发表且范围读尚未认领时的锁外观察点，仅验收镜像保存回调。
#[cfg(feature = "ktest")]
pub(crate) type MetadataLoadHook = Arc<dyn Fn(&Arc<MetadataCache>, u64, usize) + Send + Sync>;

/// Disk 弱反向槽取得同一物理元数据域；外部视图保留本缓存和真实 FS 占用。
pub struct MetadataCache {
    index: FolioIndex,
    // 卷弱关联用于取得同一 FS 实例和标脏通知；卷持缓存，不形成强环。
    pub(crate) filesystem: SleepMutex<Weak<Ext4Volume>>,
    // 静止索引与弱关联先析构，最后一份 FS 占用才允许 raw writer 进入。
    claim: FsClaim,
    // 锁内保存下轮后台扫描的物理页 byte key，显式同步不改变它。
    writeback: SleepMutex<u64>,
    wb_error: SpinLock<WritebackError>,
    #[cfg(feature = "ktest")]
    load_hook: SpinLock<Option<MetadataLoadHook>>,
    reserve_init: SleepRwLock<()>,
    reserve: SpinLock<Option<Arc<BioReserve>>>,
}

impl MetadataCache {
    /// 串行共同元数据范围选择及完成，并保护后台续扫位置，不覆盖文件数据 IO。
    pub(crate) fn lock_writeback(&self) -> SleepMutexGuard<'_, u64> {
        self.writeback.lock()
    }

    /// RW 卷发表前准备同一缓存的有限原页及 Arc 控制，现存热页保持原 PFN 和内容。
    pub(crate) fn prepare_reserve(self: &Arc<Self>) -> Result<Arc<BioReserve>, BlockError> {
        let _scope = ReclaimScope::noio();
        let _init = self.reserve_init.write();
        if let Some(reserve) = self.reserve() {
            return Ok(reserve);
        }
        let mut hot = Vec::new();
        hot.try_reserve_exact(self.capacity())
            .map_err(|_| BlockError::NoMemory)?;
        let mut cursor = 0;
        loop {
            let next = {
                let entries = self.index.entries.lock();
                if entries.closed {
                    return Err(BlockError::QueueStopped);
                }
                entries
                    .lower_bound(cursor)
                    .map(|node| (node.key, node.folio.clone()))
            };
            let Some((key, folio)) = next else {
                break;
            };
            hot.push((key, folio));
            let Some(next) = key.checked_add(1) else {
                break;
            };
            cursor = next;
        }
        let reserve =
            BioReserve::try_new(self.capacity(), hot.iter().map(|(_, folio)| folio.clone()))?;
        for slot in 0..self.capacity() {
            Node::prime(&reserve, slot)?;
        }
        // 所有可失败申请已完成。已 prime 的控制使用同一实际 Arc Layout，
        // 下方构造只消费原块；hot 原引用阻止 prune，miss/init 锁排除新发表。
        for (slot, (key, folio)) in hot.into_iter().enumerate() {
            let (selected, old) = {
                let mut entries = self.index.entries.lock();
                let node = entries
                    .get(key)
                    .expect("bootstrap metadata node disappeared");
                assert_eq!(
                    node.folio.phys_addr(),
                    folio.phys_addr(),
                    "bootstrap metadata identity changed"
                );
                let selected = node.to_write();
                let old = entries.remove(key).unwrap();
                (selected, old)
            };
            // 发表前没有 metadata Node observer 出口，旧 Heap 控制只有原索引拥有；
            // 真 Drop 结束旧控制后重建 immutable key，原 baseline/Buffer/IO 不移动原页。
            old.release();
            let node = Node::prepare(
                key,
                folio,
                None,
                IndexAllocator::Metadata {
                    reserve: Arc::clone(&reserve),
                    slot,
                },
            )
            .expect("primed metadata control allocation failed");
            {
                let mut entries = self.index.entries.lock();
                entries.insert(node);
                entries.set_to_write(key, selected);
            }
        }
        reserve.mark_ready();
        *self.reserve.lock() = Some(Arc::clone(&reserve));
        Ok(reserve)
    }

    /// 只取得同缓存真实来源快照，IRQ 最后 put 不取得睡眠初始化锁。
    pub(crate) fn reserve(&self) -> Option<Arc<BioReserve>> {
        self.reserve.lock().clone()
    }

    /// Buffer/Read/WB 最后 put 已退出 PageUse 锁后通知原库存，通知不授予复用资格。
    pub(crate) fn notify_metadata_available(&self) {
        let reserve = self.reserve();
        if let Some(reserve) = reserve {
            reserve.notify_available();
        }
    }

    /// 为新观察者采样共同元数据的有限错误序列。
    pub(crate) fn sample_wb_error(&self) -> u32 {
        self.wb_error.lock().sample()
    }

    /// 推进同一观察者的元数据游标，保留原具体错误。
    pub(crate) fn check_and_advance_wb_error(&self, since: &mut u32) -> Result<(), FsError> {
        self.wb_error.lock().check_and_advance(since)
    }

    /// 在元数据 WB 退役或同步返回之前发布最后真实错误。
    pub(crate) fn record_wb_error(&self, error: FsError) {
        self.wb_error.lock().record(error);
    }

    /// 相同 Disk 复用唯一缓存；活域请求不同页预算明确拒绝，不默默采用首个预算。
    pub fn shared(claim: &FsClaim, capacity: usize) -> Result<Arc<Self>, BlockError> {
        if capacity == 0 {
            return Err(BlockError::InvalidRange);
        }
        if let Some(cache) = claim.disk().metadata_cache() {
            if cache.capacity() != capacity {
                return Err(BlockError::Busy);
            }
            return Ok(cache);
        }
        let candidate = Arc::try_new(Self {
            claim: claim.clone(),
            index: FolioIndex::new(Some(capacity), true),
            filesystem: SleepMutex::new(Weak::new()),
            writeback: SleepMutex::new(0),
            wb_error: SpinLock::new(WritebackError::new()),
            reserve_init: SleepRwLock::new(()),
            reserve: SpinLock::new(None),
            #[cfg(feature = "ktest")]
            load_hook: SpinLock::new(None),
        })
        .map_err(|_| BlockError::NoMemory)?;
        let cache = claim.disk().intern_metadata_cache(&candidate);
        if cache.capacity() != capacity {
            return Err(BlockError::Busy);
        }
        Ok(cache)
    }

    /// 更换槽发表后的观察者，旧回调在短锁外析构，回调不得遗留未结束的读入责任。
    #[cfg(feature = "ktest")]
    pub(crate) fn ktest_set_load_hook(&self, hook: Option<MetadataLoadHook>) {
        let retired = core::mem::replace(&mut *self.load_hook.lock(), hook);
        drop(retired);
    }

    /// 取得准确 1/2/4KiB 窗口；缺失段逐次认领，已有有效或脏字节不会被重新读盘覆盖。
    pub(crate) fn get(
        self: &Arc<Self>,
        block: u64,
        block_size: usize,
        io_context: Option<MetadataIoContext<'_>>,
    ) -> Result<BufferRef, BlockError> {
        let (byte, offset) = self.block_range(block, block_size)?;
        let page = byte - offset as u64;
        let initial = self.reserve();
        let initial_context = match io_context {
            Some(context) => Some(context),
            None => initial.as_ref().map(BioReserve::metadata_io_context),
        };
        if let Some(context) = initial_context {
            context.check()?;
        }
        let (folio, reserve) = {
            let _init = self.reserve_init.read();
            let reserve = self.reserve();
            if let Some(context) = initial_context {
                assert!(
                    reserve
                        .as_ref()
                        .is_some_and(|source| context.belongs_to(source)),
                    "metadata IO source changed"
                );
                context.check()?;
            }
            let (folio, _) = self.index.get_or_prepare(page, reserve.as_ref())?;
            (folio.with_metadata_owner(Arc::clone(self)), reserve)
        };
        let context = match initial_context {
            Some(context) => Some(context),
            None => reserve.as_ref().map(BioReserve::metadata_io_context),
        };
        #[cfg(feature = "ktest")]
        {
            let hook = self.load_hook.lock().clone();
            if let Some(hook) = hook {
                hook(self, block, block_size);
            }
        }
        // 首次 ready 与旧 Heap IO 由同一读写 gate 排斥；读侧不串行不同页的普通 RO IO。
        // hook 已在 gate 外结束，完成/凭证归还均不反取 gate；已捕 Some 的旧边界不重采。
        let _legacy_scope = reserve.is_none().then(ReclaimScope::noio);
        let transition = reserve.is_none().then(|| self.reserve_init.read());
        let published = if transition.is_some() {
            self.reserve()
        } else {
            None
        };
        let source = reserve.as_ref().or(published.as_ref());
        let context = match context {
            Some(context) => Some(context),
            None => source.map(BioReserve::metadata_io_context),
        };
        let _legacy_io = if source.is_none() {
            transition
        } else {
            drop(transition);
            None
        };
        loop {
            if let Some(context) = context {
                context.check()?;
            }
            match folio.range_ready(offset, block_size) {
                Ok(()) => break,
                Err(BlockError::NotReady) => {}
                Err(error) => return Err(error),
            }
            let _scope = source.map(|_| ReclaimScope::noio());
            let lease = match (source, context) {
                (Some(source), Some(context)) => Some(source.acquire_aux(context)?),
                (None, None) => None,
                _ => unreachable!("metadata IO context lost its actual source"),
            };
            let memory = lease.as_ref().map(|lease| lease.memory()).transpose()?;
            if let Some(context) = context {
                context.check()?;
            }
            let Some(read) = folio.begin_metadata_read(offset, block_size)? else {
                break;
            };
            let read = read.with_metadata_owner(Arc::clone(self));
            let memory = memory.unwrap_or_else(|| BioMemory::new_in(BioAllocator::Heap));
            self.claim
                .disk()
                .load_folio(page + read.offset() as u64, read, memory)?;
        }
        Ok(BufferRef {
            folio,
            offset,
            len: block_size,
        })
    }

    /// 只取得已经存在的准确物理窗口，不填充、读盘或申请节点；有效性由使用者另行检查。
    pub(crate) fn cached_buffer(
        self: &Arc<Self>,
        block: u64,
        block_size: usize,
    ) -> Result<Option<BufferRef>, BlockError> {
        let (byte, offset) = self.block_range(block, block_size)?;
        let folio = {
            let entries = self.index.entries.lock();
            entries
                .get(byte - offset as u64)
                .map(|node| node.folio.clone())
        };
        Ok(folio.map(|folio| BufferRef {
            folio: folio.with_metadata_owner(self.clone()),
            offset,
            len: block_size,
        }))
    }

    /// 在发表候选前解释块几何、设备容量和准确页内范围。
    fn block_range(&self, block: u64, block_size: usize) -> Result<(u64, usize), BlockError> {
        if !matches!(block_size, 1024 | 2048 | 4096) {
            return Err(BlockError::InvalidRange);
        }
        let size = u64::try_from(block_size).map_err(|_| BlockError::InvalidRange)?;
        let byte = block.checked_mul(size).ok_or(BlockError::InvalidRange)?;
        let end = byte.checked_add(size).ok_or(BlockError::InvalidRange)?;
        let limits = self.claim.disk().limits();
        let capacity = limits
            .capacity_sectors
            .checked_mul(SECTOR_BYTES)
            .ok_or(BlockError::InvalidRange)?;
        if capacity < end {
            return Err(BlockError::InvalidRange);
        }
        let logical = u64::from(limits.logical_block_size);
        if byte % logical != 0 || size % logical != 0 {
            return Err(BlockError::Misaligned);
        }
        let offset = (byte % PAGE_SIZE as u64) as usize;
        if PAGE_SIZE < offset + block_size {
            return Err(BlockError::InvalidRange);
        }
        Ok((byte, offset))
    }

    /// 摘离无外部 Buffer/Read/WB 的静止节点，基准原页仍归同 source，返回节点数。
    pub fn prune_unused(&self) -> usize {
        let reserve = self.reserve();
        self.index.prune_unused(reserve.as_ref())
    }

    /// 返回在途、错误及有效范围共同占用的真实物理页数。
    pub fn len(&self) -> usize {
        self.index.len()
    }

    /// 检查物理页索引是否为空。
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 标记本次元数据写回选中的原节点，身份与内容不产生第二份拥有。
    pub(crate) fn set_to_write(&self, byte_offset: u64, selected: bool) {
        self.index
            .entries
            .lock()
            .set_to_write(byte_offset, selected);
    }

    /// 持本缓存扫描互斥时选定入口的有限元数据集合，完整同步不限制页数。
    pub(crate) fn mark_to_write_range(&self, start: u64, end: u64) -> usize {
        self.index.mark_to_write_range(start, end, usize::MAX).0
    }

    /// 后台只为最多 max_pages 个原节点建立 ToWrite，并返回下轮单向续扫位置。
    pub(crate) fn mark_to_write_budget(
        &self,
        start: u64,
        end: u64,
        max_pages: usize,
    ) -> (usize, u64) {
        self.index.mark_to_write_range(start, end, max_pages)
    }

    /// 检查共同元数据原页是否仍有实际 Dirty 或 WB 义务。
    pub(crate) fn has_dirty_or_writeback(&self) -> bool {
        self.index.has_dirty_or_writeback()
    }

    /// 按物理 byte key 有界取得真实元数据页，调用者保持本缓存到全部消费结束。
    /// 移交 IO 的外部页须附原缓存 anchor，索引内部的引用不反持缓存。
    pub(crate) fn select_range(
        &self,
        start: u64,
        end: u64,
        mark: FolioMark,
        output: &mut [Option<(u64, FolioRef)>],
    ) -> Option<u64> {
        self.index.select_range(start, end, mark, output)
    }

    /// 返回此 Disk 活元数据域的明确页预算。
    pub fn capacity(&self) -> usize {
        self.index.capacity.unwrap()
    }

    /// 只在 RW 源 ready 后观察原 Arc 控制尾；不在 bootstrap 暴露旧热节点或造假对象。
    #[cfg(feature = "ktest")]
    pub(crate) fn ktest_node(&self, byte_offset: u64) -> Option<Weak<Node, IndexAllocator>> {
        let _init = self.reserve_init.read();
        self.reserve()?;
        self.index.entries.lock().weak_node(byte_offset)
    }

    /// 关闭私有域的新取得，摘离全部页后丢弃明确无需落盘的测试或未发表内容。
    ///
    /// # Safety
    /// 调用者必须已停止全部 get/update 生产者并结束全部 IO、WB 和外部视图使用。
    /// 这些字节只能属于未发表或私有 CPU 协议页，不得包含任何已接受的真实 FS 修改。
    pub unsafe fn discard_private(self: &Arc<Self>) -> Result<usize, BlockError> {
        let reserve = self.reserve();
        let mut detached = {
            let mut entries = self.index.entries.lock();
            let mut cursor = 0;
            while let Some(node) = entries.lower_bound(cursor) {
                let baseline = reserve
                    .as_ref()
                    .is_some_and(|source| source.metadata_has_baseline(&node.folio));
                if !node.folio.index_unused(baseline, true) {
                    return Err(BlockError::Busy);
                }
                let Some(next) = node.key.checked_add(1) else {
                    break;
                };
                cursor = next;
            }
            entries.closed = true;
            entries.take_all()
        };
        let count = detached.len;
        while let Some(node) = detached.lower_bound(0) {
            let key = node.key;
            let node = detached.remove(key).unwrap();
            let mut content = node.folio.lock_content();
            // SAFETY: 调用者已停止全部生产者和在途访问；索引已关闭并摘离全部节点，
            // 上面核索引及真实源基准以外无外部视图。内容明确私有且无需落盘，guard 排斥 CPU 访问。
            unsafe {
                content.discard();
            }
            drop(content);
            drop(node);
        }
        Ok(count)
    }
}

impl Drop for MetadataCache {
    /// 最后强拥有结束时摘本对象的 Disk 弱登记，锁外释放 Weak；原字段随后按声明顺序析构。
    fn drop(&mut self) {
        let identity = self as *const Self;
        let retired = self.claim.disk().unregister_metadata_cache(identity);
        drop(retired);
    }
}

/// 同一元数据页中的准确范围，保活缓存及其 Disk 占用，不复制字节。
#[derive(Clone)]
pub struct BufferRef {
    folio: FolioRef,
    offset: usize,
    len: usize,
}

impl BufferRef {
    /// 内容锁内短借用本视图；闭包不得分配、等待 IO 或进行可能 fault 的用户复制。
    pub fn with_read<T>(&self, consume: impl FnOnce(&[u8]) -> T) -> Result<T, BlockError> {
        let content = self.folio.lock_content();
        Ok(consume(content.metadata_bytes(self.offset, self.len)?))
    }

    /// 同 Disk 的 RW 授权只修改准确视图，Dirty 在字节修改之前记录，已有 WB 不被清除。
    /// 闭包不得等待或用户复制；相邻 inode 记录与校验和在同一真实内容锁内保持一致。
    /// 普通 Task 在内容锁释放后通知所属卷，通知期间不保留卷弱关联锁。
    pub fn update<T>(
        &self,
        claim: &FsClaim,
        update: impl FnOnce(&mut [u8]) -> T,
    ) -> Result<T, BlockError> {
        if claim.mode() != AccessMode::ReadWrite {
            return Err(BlockError::ReadOnly);
        }
        let cache = self
            .folio
            .metadata_owner()
            .expect("metadata buffer lost its cache owner");
        if !core::ptr::eq(
            claim.disk().as_ref().get_ref(),
            cache.claim.disk().as_ref().get_ref(),
        ) {
            return Err(BlockError::InvalidRange);
        }
        let result = {
            let mut content = self.folio.lock_content();
            update(content.metadata_bytes_mut(self.offset, self.len)?)
        };
        let volume = cache.filesystem.lock().upgrade();
        if let Some(volume) = volume {
            volume.writeback_domain().notify_dirty(&volume);
        }
        Ok(result)
    }

    /// 实际块树摘离节点后失效本窗口，等待原页 WB 收权而不影响同页其他元数据块。
    ///
    /// # Safety
    /// 调用者已持真实块映射写保护摘离该节点并阻止旧根新取得，旧消费与 IO 已结束；
    /// 其他视图不得再消费本窗口旧内容，块尚未归还位图，本窗口确实无需落盘。
    /// 同 Disk RW 占用仅证明权限，不能代替上述删除与生命周期条件。
    pub(crate) unsafe fn forget(&self, claim: &FsClaim) -> Result<(), BlockError> {
        if claim.mode() != AccessMode::ReadWrite {
            return Err(BlockError::ReadOnly);
        }
        let cache = self
            .folio
            .metadata_owner()
            .expect("metadata buffer lost its cache owner");
        if !core::ptr::eq(
            claim.disk().as_ref().get_ref(),
            cache.claim.disk().as_ref().get_ref(),
        ) {
            return Err(BlockError::InvalidRange);
        }
        let mut content = self.folio.lock_content();
        // SAFETY: 调用者保证真实摘树与旧消费终结；此 guard 排斥新的内容准备，
        // metadata 内容锁只在全部 WB 实际收权后取得，按本视图范围失效且尚未归块。
        unsafe { content.forget_metadata(self.offset, self.len) }
    }

    /// 返回视图自己的字节长度。
    pub fn len(&self) -> usize {
        self.len
    }

    /// 元数据块视图不接受零长度请求。
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// 返回视图在真实 4KiB 页中的起点。
    pub fn offset(&self) -> usize {
        self.offset
    }

    /// 借用同一原页身份；内容仍只能按本视图或原页角色访问。
    pub fn folio(&self) -> &FolioRef {
        &self.folio
    }
}

/// 每份文件 VMA 的固定来源；驻留页仅弱持来源，私有 COW 后仍保文件范围事实。
pub(crate) struct FileRangeData {
    pub(crate) inode: InodeRef,
    pub(crate) domain: Arc<PteDomain>,
    pub(crate) range: Range<usize>,
    pub(crate) offset: u64,
    pub(crate) private: bool,
}

impl FileRangeData {
    /// 有序键的分配地址由遍历 cursor 的 Arc 保活，防止迭代期间地址复用。
    fn key(this: &Arc<Self>) -> (u64, usize) {
        (this.offset, Arc::as_ptr(this) as usize)
    }
}

/// VMA 独占登记责任；先从 inode 的弱索引摘离，再释放固定来源及 PTE 域拥有。
pub(crate) struct FileRangeRegistration {
    data: Arc<FileRangeData>,
}

impl FileRangeRegistration {
    /// 在候选 VMA 可见前登记；候选无可写叶，失败不会改变现有映射。
    pub(crate) fn try_new(
        inode: InodeRef,
        domain: Arc<PteDomain>,
        range: Range<usize>,
        offset: u64,
        private: bool,
    ) -> Result<Self, BlockError> {
        let data = Arc::try_new(FileRangeData {
            inode,
            domain,
            range,
            offset,
            private,
        })
        .map_err(|_| BlockError::NoMemory)?;
        data.inode.mapping().register_range(&data)?;
        Ok(Self { data })
    }

    /// 借用不可变来源，不允许取得 Arc 可变权限或改变登记键。
    pub(crate) fn data(&self) -> &Arc<FileRangeData> {
        &self.data
    }
}

impl Drop for FileRangeRegistration {
    /// 摘离 Weak 在范围锁外析构；字段的最后强拥有也在索引摘离之后释放。
    fn drop(&mut self) {
        let detached = {
            let mut ranges = self.data.inode.mapping().ranges.lock();
            let key = FileRangeData::key(&self.data);
            let index = ranges
                .binary_search_by_key(&key, |(key, _)| *key)
                .expect("file range registration disappeared");
            ranges.remove(index)
        };
        drop(detached);
    }
}

/// 文件范围失效的真实同步边界；两处观察均仍保有原退休页及表页。
#[cfg(feature = "ktest")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum FileUnmapPhase {
    BeforeSync,
    AfterSync,
}

/// invalidate 外准备的真实命中引用或未发表页/Node/读入责任，失败与输家锁外结束。
pub(crate) enum PreparedFile {
    Cached(FolioRef),
    New { node: NodeRef, read: FolioRead },
}

/// canonical inode 的稀疏页与弱文件范围索引；来源独立于当前驻留 backing。
pub struct FileMapping {
    index: FolioIndex,
    ranges: SpinLock<Vec<((u64, usize), Weak<FileRangeData>)>>,
    // 锁内保存下轮后台扫描的文件页号，显式同步不改变它。
    writeback: SleepMutex<u64>,
    wb_error: SpinLock<WritebackError>,
    #[cfg(feature = "ktest")]
    unmap_hook: SpinLock<Option<Arc<dyn Fn(FileUnmapPhase, usize, usize) + Send + Sync>>>,
}

impl FileMapping {
    /// 串行同一 inode 的同步扫描与完成，并保护后台续扫位置，不排斥其他 inode IO。
    pub(crate) fn lock_writeback(&self) -> SleepMutexGuard<'_, u64> {
        self.writeback.lock()
    }

    /// 新 OFD 保留尚未被任何观察者见到的旧错误。
    pub(crate) fn sample_wb_error(&self) -> u32 {
        self.wb_error.lock().sample()
    }

    /// 在调用者自己的 OFD 游标互斥下推进有限错误观察。
    pub(crate) fn check_and_advance_wb_error(&self, since: &mut u32) -> Result<(), FsError> {
        self.wb_error.lock().check_and_advance(since)
    }

    /// 完成者在撤销本轮 WB 前记录原文件的数据错误。
    pub(crate) fn record_wb_error(&self, error: FsError) {
        self.wb_error.lock().record(error);
    }

    /// 在原索引短锁下检查实际页义务，不复制状态或等待内容锁。
    pub(crate) fn has_dirty_or_writeback(&self) -> bool {
        self.index.has_dirty_or_writeback()
    }

    /// 建立无预留节点的唯一文件页索引，miss 的真实内存不足由节点和页申请返回。
    pub const fn new() -> Self {
        Self {
            index: FolioIndex::new(None, false),
            ranges: SpinLock::new(Vec::new()),
            writeback: SleepMutex::new(0),
            wb_error: SpinLock::new(WritebackError::new()),
            #[cfg(feature = "ktest")]
            unmap_hook: SpinLock::new(None),
        }
    }

    /// 在真实 owner 的 invalidate/root/content 之外准备原页及唯一 Node，有限 OOM 共用请求。
    pub(crate) fn prepare_file(
        &self,
        owner: &InodeRef,
        index: u64,
        attempt: &mut ReclaimAttempt,
    ) -> Result<PreparedFile, BlockError> {
        assert!(
            core::ptr::eq(owner.mapping(), self),
            "file mapping prepare owner changed"
        );
        let cached = {
            let entries = self.index.entries.lock();
            if entries.closed {
                return Err(BlockError::QueueStopped);
            }
            entries.get(index).map(|node| node.folio.clone())
        };
        if let Some(folio) = cached {
            return Ok(PreparedFile::Cached(folio.with_file_owner(owner.clone())));
        }
        attempt.prepare(|| {
            let folio = FolioRef::allocate().ok_or(BlockError::NoMemory)?;
            let read = folio
                .begin_read(0, PAGE_SIZE)?
                .with_file_owner(owner.clone());
            let node = Node::prepare(
                index,
                folio,
                Some(owner.downgrade_for_reclaim()),
                IndexAllocator::Heap,
            )?;
            Ok(PreparedFile::New { node, read })
        })
    }

    /// caller 重核 EOF 后二查 canonical；None 只表示旧命中已退休，私有输家仍由 caller 锁外结束。
    pub(crate) fn publish_file(
        &self,
        owner: &InodeRef,
        index: u64,
        prepared: &mut Option<PreparedFile>,
    ) -> Result<Option<(FolioRef, Option<FolioRead>)>, BlockError> {
        assert!(
            core::ptr::eq(owner.mapping(), self),
            "file mapping publish owner changed"
        );
        let result = {
            let mut entries = self.index.entries.lock();
            if entries.closed {
                return Err(BlockError::QueueStopped);
            }
            if let Some(node) = entries.get(index) {
                match prepared {
                    Some(PreparedFile::Cached(folio))
                        if folio.phys_addr() == node.folio.phys_addr() =>
                    {
                        let Some(PreparedFile::Cached(folio)) = prepared.take() else {
                            unreachable!();
                        };
                        return Ok(Some((folio, None)));
                    }
                    _ => (node.folio.clone(), None),
                }
            } else {
                if !matches!(prepared, Some(PreparedFile::New { .. })) {
                    return Ok(None);
                }
                let PreparedFile::New { node, read } = prepared.take().unwrap() else {
                    unreachable!();
                };
                assert_eq!(node.key, index, "file candidate key changed");
                assert!(
                    node.file.as_ref().is_some_and(|key| key.matches(owner)),
                    "file candidate owner changed"
                );
                let folio = node.folio.clone();
                entries.insert(node);
                (folio, Some(read))
            }
        };
        Ok(Some((result.0.with_file_owner(owner.clone()), result.1)))
    }

    /// 只取得原索引已有页，不填充、读盘或申请节点；调用者保原 inode 到内容检查结束。
    /// 返回引用不附 inode，不能脱离该来源保护交给外部使用者或设备。
    pub(crate) fn cached_folio(&self, index: u64) -> Option<FolioRef> {
        self.index
            .entries
            .lock()
            .get(index)
            .map(|node| node.folio.clone())
    }

    /// 短索引锁只核现有节点的原页身份，不增加 folio 引用或建立第二页缓存。
    pub(crate) fn is_canonical(&self, index: u64, folio: &FolioRef) -> bool {
        self.index
            .entries
            .lock()
            .get(index)
            .is_some_and(|node| node.folio.phys_addr() == folio.phys_addr())
    }

    /// 与全范围失效串行来源换身份及 fork 叶提交；持此 guard 时只允许原 PTE 短锁操作。
    pub(crate) fn lock_ranges(
        &self,
    ) -> SpinLockGuard<'_, Vec<((u64, usize), Weak<FileRangeData>)>> {
        self.ranges.lock()
    }

    /// 撤销文件字节半开范围所覆盖的当前叶，私有匿名 COW 由原文件来源定位。
    /// 不改变 VMA，不取得 mmap；原页和表页统一在范围/PTE 锁外真实 CSD 后释放。
    pub fn unmap_range(&self, range: Range<u64>, even_cows: bool) -> usize {
        if range.end <= range.start {
            return 0;
        }
        let mut pages = ResidentPages::default();
        let mut tables = RetiredTables::default();
        let mut unmapped = 0;
        {
            // ponytail: 同 inode 的短锁覆盖全范围撤叶；实际大映射延迟成为瓶颈时再调整范围锁协议。
            let ranges = self.ranges.lock();
            for ((offset, _), weak) in ranges.iter() {
                if range.end <= *offset {
                    break;
                }
                // SAFETY: 已登记的 Weak 由 FileRangeRegistration 的强 data 拥有支持；
                // registration 必须先持本范围锁摘登记才析构 data，当前锁使其保持存活。
                // 这里只借不可变来源，不增加或析构 Arc，域锁内只移动原退休拥有。
                let data = unsafe { &*weak.as_ptr() };
                // SAFETY: 同一范围锁保来源登记和 fork/换身份互斥；原退休页及表链在此处只接收拥有，
                // 下方在全部范围/PTE 锁外完成真实 CSD 后才析构，不提前归还数据页或目录页。
                unmapped += unsafe {
                    data.domain
                        .unmap_range(data, &range, even_cows, &mut pages, &mut tables)
                };
            }
        }
        self.sync_retired(&pages, &tables);
        drop(pages);
        drop(tables);
        unmapped
    }

    /// 原退休拥有始终覆盖真实全 CPU 同步及既有观察点，调用者随后才析构。
    fn sync_retired(&self, pages: &ResidentPages, tables: &RetiredTables) {
        #[cfg(feature = "ktest")]
        let hook = self.unmap_hook.lock().clone();
        #[cfg(feature = "ktest")]
        if let Some(hook) = &hook {
            hook(
                FileUnmapPhase::BeforeSync,
                pages.count(&(0..usize::MAX)),
                tables.len(),
            );
        }
        sync_all();
        #[cfg(feature = "ktest")]
        if let Some(hook) = &hook {
            hook(
                FileUnmapPhase::AfterSync,
                pages.count(&(0..usize::MAX)),
                tables.len(),
            );
        }
        #[cfg(not(feature = "ktest"))]
        let _ = (pages, tables);
    }

    /// 普通清洁回收只查有限原来源，先 mkclean/CSD，再按同来源、File 与 PFN 撤叶。
    /// 私有匿名 COW 保留；任何预算拒尾仍结束已撤叶的 CSD 和真实拥有，不归还原 file 页。
    fn reclaim_mappings(
        &self,
        inode: &InodeRef,
        node: &Node,
        folio: &FolioRef,
        content: &mut FolioContent<'_>,
        attempt: &mut ReclaimAttempt,
    ) -> bool {
        const SOURCES: usize = 32;
        let start = node
            .key
            .checked_mul(PAGE_SIZE as u64)
            .expect("file reclaim offset overflow");
        let end = start
            .checked_add(PAGE_SIZE as u64)
            .expect("file reclaim end overflow");
        let mut sources: [Option<Arc<FileRangeData>>; SOURCES] = [const { None }; SOURCES];
        let mut count = 0;
        {
            let ranges = self.ranges.lock();
            for ((offset, _), weak) in ranges.iter() {
                if end <= *offset {
                    break;
                }
                if !attempt.rmap() {
                    return false;
                }
                let data = weak
                    .upgrade()
                    .expect("registered reclaim source lost owner");
                if data
                    .offset
                    .saturating_add((data.range.end - data.range.start) as u64)
                    <= start
                {
                    continue;
                }
                if count == SOURCES {
                    return false;
                }
                sources[count] = Some(data);
                count += 1;
            }
        }
        for data in sources[..count].iter().flatten() {
            if !data.private {
                let (observed, _) = data.domain.mkclean_range(data, start, folio.phys_addr());
                if observed {
                    let size = inode
                        .metadata_snapshot()
                        .expect("reclaim inode is not loaded")
                        .size;
                    let valid = size.saturating_sub(start).min(PAGE_SIZE as u64) as usize;
                    if valid != 0 {
                        inode.note_dirty();
                        content
                            .mark_prepared_dirty(valid, inode.filesystem().geometry().block_size)
                            .expect("reclaim prepared dirty range changed");
                    }
                }
            }
        }
        sync_all();
        let (dirty, writeback) = folio.writeback_state();
        if dirty != 0 || writeback != 0 {
            return false;
        }
        let mut pages = ResidentPages::default();
        let mut tables = RetiredTables::default();
        let mut unmapped = 0;
        let mut complete = true;
        {
            let _ranges = self.ranges.lock();
            for data in sources[..count].iter().flatten() {
                if !attempt.rmap() {
                    complete = false;
                    break;
                }
                // SAFETY: 原来源 Arc 保活，其登记/换身份/fork 与本范围锁串行；
                // 只撤实际 File 后备、同来源及同 PFN，旧原页/表拥有移入退休链，
                // 下方锁外无条件全 CPU CSD 完成以后才释放；匿名 COW 不匹配。
                unmapped += unsafe {
                    data.domain.unmap_file_folio(
                        data,
                        start,
                        folio.phys_addr(),
                        &mut pages,
                        &mut tables,
                    )
                };
            }
        }
        self.sync_retired(&pages, &tables);
        drop(pages);
        let freed_tables = tables.release();
        attempt.account(ReclaimStats {
            unmapped,
            freed_tables,
            ..ReclaimStats::default()
        });
        complete
    }

    /// 观察真实撤叶与 CSD 边界，callback 不改变叶或替代硬件同步；旧 callback 锁外释放。
    #[cfg(feature = "ktest")]
    pub(crate) fn ktest_set_unmap_hook(
        &self,
        hook: Option<Arc<dyn Fn(FileUnmapPhase, usize, usize) + Send + Sync>>,
    ) {
        let previous = core::mem::replace(&mut *self.unmap_hook.lock(), hook);
        drop(previous);
    }

    /// 在正式截断协议下附实际 inode 后清理原页，删除义务与本卷页账在同次状态转换中退出。
    ///
    /// # Safety
    /// 调用者须持同 inode 写序列化、invalidate 独占及本 mapping 的 WB 互斥，
    /// 已发表 new_size 且完成第一次完整尾范围 even_cows 撤叶/CSD，阻止新填充和 WB。
    /// 原索引页属于本 inode；旧填充和真实 WB 的完成不得依赖调用者持有的锁。
    /// 返回后仍须第二次 even_cows 撤叶/CSD，再结束旧块别名及设备使用并归还磁盘块；
    /// 部分保留 FS 块的盘上尾零由文件系统负责，删除义务不能当作回写成功。
    pub(crate) unsafe fn truncate_pages(
        &self,
        inode: &InodeRef,
        new_size: u64,
        block_size: usize,
    ) -> Result<(), FsError> {
        assert!(
            core::ptr::eq(inode.mapping(), self),
            "truncate mapping owner changed"
        );
        if !matches!(block_size, 1024 | 2048 | 4096) {
            return Err(FsError::InvalidOperation);
        }
        let first = new_size.div_ceil(PAGE_SIZE as u64);
        loop {
            let next = {
                let entries = self.index.entries.lock();
                entries
                    .lower_bound(first)
                    .map(|node| (node.key, node.folio.clone()))
            };
            let Some((index, folio)) = next else { break };
            let folio = folio.with_file_owner(inode.clone());
            let mut content = folio.lock_content();
            folio.wait_writeback();
            let start = index
                .checked_mul(PAGE_SIZE as u64)
                .expect("file page offset overflow");
            self.unmap_range(start..start.saturating_add(PAGE_SIZE as u64), false);
            let detached = {
                let mut entries = self.index.entries.lock();
                assert!(
                    entries
                        .get(index)
                        .is_some_and(|node| { node.folio.phys_addr() == folio.phys_addr() }),
                    "truncate file page identity changed"
                );
                entries.remove(index).unwrap()
            };
            // SAFETY: 调用者阻止新填充/WB；内容锁已等唯一填充，真实 WB 已结束。
            // 本原节点已摘索引，仍指本 folio 的文件叶已撤且 CSD 完成；
            // 迟到候选同内容锁重核 canonical，私有匿名副本不再访问该原页。
            unsafe { content.discard() };
            drop(content);
            drop(detached);
            drop(folio);
        }
        let valid = (new_size % PAGE_SIZE as u64) as usize;
        if valid == 0 {
            return Ok(());
        }
        let index = new_size / PAGE_SIZE as u64;
        let folio = {
            let entries = self.index.entries.lock();
            entries.get(index).map(|node| node.folio.clone())
        };
        if let Some(folio) = folio {
            let folio = folio.with_file_owner(inode.clone());
            let mut content = folio.lock_content();
            folio.wait_writeback();
            self.mkclean(index, &folio, &mut content);
            // SAFETY: 同 inode 写/invalidate/WB 保护来自调用者；原内容 guard 已等填充，
            // 真实 WB 已结束，mkclean 在短锁外完成全部 CPU 写许可失效。
            // 仅作废删除的整 FS 块，有效前缀及私有匿名页保持原内容。
            unsafe { content.truncate_file_tail(valid, block_size) }?;
            self.set_to_write(index, false);
        }
        Ok(())
    }

    /// 在 canonical 准入短锁内辨别纯缓存拥有，允许脏义务留给后续真实同步。
    pub(crate) fn is_cache_only(&self) -> bool {
        let entries = self.index.entries.lock();
        let mut cursor = 0;
        while let Some(node) = entries.lower_bound(cursor) {
            if !node.folio.is_cache_only() {
                return false;
            }
            let Some(next) = node.key.checked_add(1) else {
                break;
            };
            cursor = next;
        }
        true
    }

    /// 容量不足时在锁外准备替代存储；短锁内重查并仅转移原 Weak，不分配或析构。
    fn register_range(&self, data: &Arc<FileRangeData>) -> Result<(), BlockError> {
        let key = FileRangeData::key(data);
        let mut spare = Vec::new();
        loop {
            let mut ranges = self.ranges.lock();
            if ranges.len() < ranges.capacity() {
                let index = ranges.partition_point(|(existing, _)| *existing < key);
                ranges.insert(index, (key, Arc::downgrade(data)));
                return Ok(());
            }
            let needed = ranges.len().checked_add(1).ok_or(BlockError::NoMemory)?;
            if needed <= spare.capacity() {
                spare.extend(ranges.drain(..));
                core::mem::swap(&mut *ranges, &mut spare);
                let index = ranges.partition_point(|(existing, _)| *existing < key);
                ranges.insert(index, (key, Arc::downgrade(data)));
                drop(ranges);
                drop(spare);
                return Ok(());
            }
            drop(ranges);
            spare
                .try_reserve_exact(needed)
                .map_err(|_| BlockError::NoMemory)?;
        }
    }

    /// 持真实 folio 内容锁逐个枚举 shared 文件范围，降权后在全部短锁外同步全 CPU。
    /// 新写授权也取此内容锁，故候选登记及并发移除无需全局代次或整表快照。
    pub(crate) fn mkclean(
        &self,
        index: u64,
        folio: &FolioRef,
        content: &mut FolioContent<'_>,
    ) -> usize {
        assert_eq!(content.folio().phys_addr(), folio.phys_addr());
        let start = index
            .checked_mul(PAGE_SIZE as u64)
            .expect("file page offset overflow");
        let end = start
            .checked_add(PAGE_SIZE as u64)
            .expect("file page end overflow");
        let mut cursor: Option<Arc<FileRangeData>> = None;
        let mut cleaned = 0;
        loop {
            let next = {
                let ranges = self.ranges.lock();
                let after = cursor.as_ref().map(FileRangeData::key);
                let position = after.map_or(0, |key| {
                    ranges.partition_point(|(existing, _)| *existing <= key)
                });
                ranges
                    .get(position)
                    .filter(|(key, _)| key.0 < end)
                    .map(|(_, weak)| weak.upgrade().expect("registered file range lost owner"))
            };
            let Some(data) = next else { break };
            if !data.private {
                let (observed, changed) =
                    data.domain.mkclean_range(&data, start, folio.phys_addr());
                cleaned += changed;
                if observed {
                    let size = data
                        .inode
                        .metadata_snapshot()
                        .expect("registered file inode is not loaded")
                        .size;
                    let valid = size.saturating_sub(start).min(PAGE_SIZE as u64) as usize;
                    if 0 < valid {
                        let block_size = data.inode.filesystem().geometry().block_size;
                        data.inode.note_dirty();
                        content
                            .mark_prepared_dirty(valid, block_size)
                            .expect("prepared file dirty range is invalid");
                    }
                }
            }
            cursor = Some(data);
        }
        // 范围换身份可已撤 W、尚未结束自己的 CSD；零命中也须结束旧远核翻译。
        sync_all();
        cleaned
    }

    /// 仅摘除无内容锁、Dirty、WB 和外部使用者的静止页。
    pub fn prune_unused(&self) -> usize {
        self.index.prune_unused(None)
    }

    /// 返回已发表的稀疏节点数。
    pub fn len(&self) -> usize {
        self.index.len()
    }

    /// 检查是否仍保有任何页身份。
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 在同一原节点标记或取消本轮待写选择，不创建第二份页拥有。
    pub fn set_to_write(&self, index: u64, selected: bool) {
        self.index.entries.lock().set_to_write(index, selected);
    }

    /// 持同 inode 扫描互斥时选定入口完整集合，显式同步保留原半开范围。
    pub(crate) fn mark_to_write_range(&self, start: u64, end: u64) -> usize {
        self.index.mark_to_write_range(start, end, usize::MAX).0
    }

    /// 后台最多标记 max_pages 个原节点，并返回下轮单向续扫位置。
    pub(crate) fn mark_to_write_budget(
        &self,
        start: u64,
        end: u64,
        max_pages: usize,
    ) -> (usize, u64) {
        self.index.mark_to_write_range(start, end, max_pages)
    }

    /// 锁内只选有界范围和增加原引用；返回值是下一次有界扫描起点。
    /// 文件扫描者保原 InodeRef 到所有选中页和 IO 使用结束，跨拥有交付须附原 anchor。
    pub(crate) fn select_range(
        &self,
        start: u64,
        end: u64,
        mark: FolioMark,
        output: &mut [Option<(u64, FolioRef)>],
    ) -> Option<u64> {
        self.index.select_range(start, end, mark, output)
    }
}

impl Default for FileMapping {
    /// 默认文件映射同样没有预先申请或固定页预算。
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(feature = "ktest")]
pub(crate) mod ktest;

/// 单一申请先收当前清洁原页，只有没有实际归页时才请求一次原卷有限 WB 值进度。
pub(crate) fn reclaim_file(attempt: &mut ReclaimAttempt) {
    let before = attempt.stats();
    let mut writeback = None;
    reclaim_pass(attempt, &mut writeback);
    let after = attempt.stats();
    if before.freed_file + before.freed_tables < after.freed_file + after.freed_tables {
        return;
    }
    let Some(volume) = writeback else {
        return;
    };
    if !attempt.wait_writeback() {
        return;
    }
    let domain = volume.writeback_domain();
    let boundary = domain.request_progress(&volume);
    let progress = domain.wait_progress(boundary);
    attempt.account(ReclaimStats {
        submitted_bytes: progress.submitted_bytes as usize,
        completed_bytes: progress.completed_bytes as usize,
        io_error: progress.error,
        ..ReclaimStats::default()
    });
    if progress.stopped || progress.error {
        return;
    }
    reclaim_pass(attempt, &mut None);
}

/// 本轮只走入口队列前缀；老化后的 active 页不在同轮再次隔离，访问及弱失败也耗预算。
fn reclaim_pass(attempt: &mut ReclaimAttempt, writeback: &mut Option<Arc<Ext4Volume>>) {
    let (inactive, active) = index::lru_lengths();
    let aging = active.min(16).min(attempt.scan_remaining());
    for _ in 0..inactive.min(attempt.scan_remaining() - aging) {
        if !attempt.can_isolate() {
            break;
        }
        let Some(node) = index::isolate_inactive(attempt) else {
            continue;
        };
        reclaim_node(node, attempt, writeback);
    }
    index::age_active(attempt, aging);
}

/// 隔离同票锁外升级真实 canonical owner；所有 guard 结束后才 putback 或实际最后释放。
fn reclaim_node(
    node: NodeRef,
    attempt: &mut ReclaimAttempt,
    writeback: &mut Option<Arc<Ext4Volume>>,
) {
    let Some(inode) = node.file.as_ref().and_then(|key| key.upgrade()) else {
        let freed_file = index::putback(node);
        attempt.account(ReclaimStats {
            freed_file,
            ..ReclaimStats::default()
        });
        return;
    };
    let mapping = inode.mapping();
    let normal = attempt.mode() == ReclaimMode::Normal;
    let mut scanner = None;
    let mut needs_writeback = false;
    let retired = if normal {
        (|| {
            let _scope = ReclaimScope::nofs();
            let _invalidate = inode.lock_reclaim_invalidate().ok()?;
            if !mapping.is_canonical(node.key, &node.folio) {
                return None;
            }
            scanner = Some(node.folio.clone().with_file_owner(inode.clone()));
            let folio = scanner.as_ref().unwrap();
            let (dirty, wb) = folio.writeback_state();
            if dirty != 0 || wb != 0 {
                needs_writeback = true;
                return None;
            }
            let mut content = folio.try_lock_content()?;
            if !mapping.reclaim_mappings(&inode, &node, folio, &mut content, attempt) {
                let (dirty, wb) = folio.writeback_state();
                needs_writeback = dirty != 0 || wb != 0;
                return None;
            }
            let mut entries = mapping.index.entries.lock();
            if entries.closed
                || !entries
                    .get(node.key)
                    .is_some_and(|current| core::ptr::eq(current, &*node))
            {
                return None;
            }
            index::with_lru(|| {
                node.folio.retire_clean(Some(folio), Some(&content), || {
                    entries
                        .remove_isolated(node.key)
                        .expect("reclaim canonical disappeared")
                })
            })
        })()
    } else {
        let mut entries = mapping.index.entries.lock();
        if entries.closed
            || !entries
                .get(node.key)
                .is_some_and(|current| core::ptr::eq(current, &*node))
        {
            None
        } else {
            index::with_lru(|| {
                node.folio.retire_clean(None, None, || {
                    entries
                        .remove_isolated(node.key)
                        .expect("cache-only canonical disappeared")
                })
            })
        }
    };
    if needs_writeback && writeback.is_none() && inode.filesystem().mode() == AccessMode::ReadWrite
    {
        *writeback = Some(Arc::clone(inode.filesystem()));
    }
    let mut freed_file = if let Some(retired) = retired {
        let freed = retired.release();
        freed + index::release_node(node)
    } else {
        index::putback(node)
    };
    if let Some(folio) = scanner {
        freed_file += folio.release();
    }
    attempt.account(ReclaimStats {
        freed_file,
        ..ReclaimStats::default()
    });
}

/// 实际两条 file LRU 的当前长度，不筛目标页、清账或修改状态。
#[cfg(feature = "ktest")]
pub(crate) fn ktest_file_lru() -> (usize, usize) {
    index::lru_lengths()
}

/// 返回本架构实际 Node payload 布局，不猜 Arc 控制头或额外预留控制对象。
#[cfg(feature = "ktest")]
pub(crate) const fn ktest_node_bytes() -> usize {
    core::mem::size_of::<Node>()
}
