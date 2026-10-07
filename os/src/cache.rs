//! 元数据视图共用 Disk 的物理页；文件页使用同一稀疏索引和原页状态。

mod index;

use alloc::sync::{Arc, Weak};
use core::convert::TryFrom;

use crate::block::{AccessMode, BlockError, FsClaim, SECTOR_BYTES};
use crate::fs::ext4::Ext4Volume;
use crate::mm::{FolioRead, FolioRef, PAGE_SIZE};
use crate::sync::{SleepMutex, SpinLock};
use index::{Node, Tree};

/// 内容状态从节点自己的 folio 读取；ToWrite 只标记本轮选中的原节点。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FolioMark {
    Dirty,
    Writeback,
    ToWrite,
}

struct FolioIndex {
    entries: SpinLock<Tree>,
    capacity: Option<usize>,
    metadata: bool,
}

impl FolioIndex {
    /// 文件索引不预留固定槽；元数据预算限制的是实际物理页数量。
    const fn new(capacity: Option<usize>, metadata: bool) -> Self {
        Self {
            entries: SpinLock::new(Tree::new()),
            capacity,
            metadata,
        }
    }

    /// 在索引锁内仅移出可回收的静止页；全活、Dirty 或 WB 都不能腾空预算。
    fn evict_if_full(
        &self,
        entries: &mut Tree,
    ) -> Result<Option<alloc::boxed::Box<Node>>, BlockError> {
        if self.capacity.is_none_or(|capacity| entries.len < capacity) {
            return Ok(None);
        }
        let mut cursor = 0;
        while let Some(node) = entries.lower_bound(cursor) {
            let key = node.key;
            if node.folio.can_prune() {
                return Ok(entries.remove(key));
            }
            let Some(next) = key.checked_add(1) else {
                break;
            };
            cursor = next;
        }
        Err(BlockError::NoMemory)
    }

    /// 命中不分配；候选页和 AVL 节点锁外准备，双查后只转移拥有。
    fn get_or_prepare(&self, key: u64) -> Result<(FolioRef, Option<FolioRead>), BlockError> {
        let victim = {
            let mut entries = self.entries.lock();
            if entries.closed {
                return Err(BlockError::QueueStopped);
            }
            if let Some(node) = entries.get(key) {
                return Ok((node.folio.clone(), None));
            }
            self.evict_if_full(&mut entries)?
        };
        drop(victim);
        let candidate = if self.metadata {
            FolioRef::allocate_metadata()
        } else {
            FolioRef::allocate()
        }
        .ok_or(BlockError::NoMemory)?;
        let read = if self.metadata {
            None
        } else {
            Some(candidate.begin_read(0, PAGE_SIZE)?)
        };
        let mut prepared = Some(Node::prepare(key, candidate.clone())?);
        let (existing, victim) = {
            let mut entries = self.entries.lock();
            if entries.closed {
                return Err(BlockError::QueueStopped);
            }
            if let Some(node) = entries.get(key) {
                (Some(node.folio.clone()), None)
            } else {
                let victim = self.evict_if_full(&mut entries)?;
                entries.insert(prepared.take().unwrap());
                (None, victim)
            }
        };
        drop(victim);
        drop(prepared);
        if let Some(folio) = existing {
            drop(read);
            return Ok((folio, None));
        }
        Ok((candidate, read))
    }

    /// 每次只摘一个静止节点，原页和 Box 析构均在短锁外；只扫描当时的有界键前缀。
    fn prune_unused(&self) -> usize {
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
                let victim = if node.folio.can_prune() {
                    entries.remove(key)
                } else {
                    None
                };
                (victim, key.checked_add(1))
            };
            removed += usize::from(victim.is_some());
            drop(victim);
            let Some(next) = next else {
                break;
            };
            cursor = next;
        }
        removed
    }

    /// 读取当前真实节点数，读入中与失败节点仍占元数据预算。
    fn len(&self) -> usize {
        self.entries.lock().len
    }

    /// 有界选择半开页范围，扫描与输出均受调用者片段限制；返回后续扫描起点。
    fn select_range(
        &self,
        start: u64,
        end: u64,
        mark: FolioMark,
        output: &mut [Option<(u64, FolioRef)>],
    ) -> Option<u64> {
        let entries = self.entries.lock();
        let mut cursor = start;
        let mut selected = 0;
        for _ in 0..output.len() {
            let node = entries.lower_bound(cursor)?;
            if end <= node.key {
                return None;
            }
            let matches = match mark {
                FolioMark::Dirty => node.folio.dirty_sectors() != 0,
                FolioMark::Writeback => node.folio.writeback_sectors() != 0,
                FolioMark::ToWrite => node.to_write,
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

/// Disk 弱反向槽取得同一物理元数据域；外部视图保留本缓存和真实 FS 占用。
pub struct MetadataCache {
    index: FolioIndex,
    // 卷弱关联只协调同一 FS 实例的取得；卷持缓存，不形成强环。
    pub(crate) filesystem: SleepMutex<Weak<Ext4Volume>>,
    // 静止索引与弱关联先析构，最后一份 FS 占用才允许 raw writer 进入。
    claim: FsClaim,
}

impl MetadataCache {
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
        })
        .map_err(|_| BlockError::NoMemory)?;
        let cache = claim.disk().intern_metadata_cache(&candidate);
        if cache.capacity() != capacity {
            return Err(BlockError::Busy);
        }
        Ok(cache)
    }

    /// 取得准确 1/2/4KiB 窗口；缺失段逐次认领，已有有效或脏字节不会被重新读盘覆盖。
    pub fn get(self: &Arc<Self>, block: u64, block_size: usize) -> Result<BufferRef, BlockError> {
        let (byte, offset) = self.block_range(block, block_size)?;
        let page = byte - offset as u64;
        let (folio, _) = self.index.get_or_prepare(page)?;
        while let Some(read) = folio.begin_metadata_read(offset, block_size)? {
            self.claim
                .disk()
                .load_folio(page + read.offset() as u64, read)?;
        }
        Ok(BufferRef {
            folio: folio.with_metadata_owner(self.clone()),
            offset,
            len: block_size,
        })
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

    /// 回收仅索引持有且无 Dirty、WB 或内容锁的页。
    pub fn prune_unused(&self) -> usize {
        self.index.prune_unused()
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

    /// 关闭私有域的新取得，摘离全部页后丢弃明确无需落盘的测试或未发表内容。
    ///
    /// # Safety
    /// 调用者必须已停止全部 get/update 生产者并结束全部 IO、WB 和外部视图使用。
    /// 这些字节只能属于未发表或私有 CPU 协议页，不得包含任何已接受的真实 FS 修改。
    pub unsafe fn discard_private(self: &Arc<Self>) -> Result<usize, BlockError> {
        let mut detached = {
            let mut entries = self.index.entries.lock();
            let mut cursor = 0;
            while let Some(node) = entries.lower_bound(cursor) {
                if node.folio.ref_count() != 1 || node.folio.writeback_sectors() != 0 {
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
            // 上面的唯一引用检查拒绝外部视图。内容明确私有且无需落盘，guard 排斥 CPU 访问。
            unsafe {
                content.discard();
            }
            drop(content);
            drop(node);
        }
        Ok(count)
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
        let mut content = self.folio.lock_content();
        Ok(update(content.metadata_bytes_mut(self.offset, self.len)?))
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

/// canonical inode 自己的空稀疏页索引，不将文件工作集封死在预留槽数。
pub struct FileMapping {
    index: FolioIndex,
}

impl FileMapping {
    /// 建立无预留节点的唯一文件页索引，miss 的真实内存不足由节点和页申请返回。
    pub const fn new() -> Self {
        Self {
            index: FolioIndex::new(None, false),
        }
    }

    /// 内核加载者在锁外候选上取得同页唯一填充责任，命中只增加原页引用。
    /// 文件来源须持有对应 InodeRef 到低层填充和使用全部结束；裸页交给外部
    /// 拥有者前必须附同一 InodeRef anchor，普通文件消费者使用 InodeRef 的页出口。
    pub(crate) fn get_or_prepare(
        &self,
        index: u64,
    ) -> Result<(FolioRef, Option<FolioRead>), BlockError> {
        self.index.get_or_prepare(index)
    }

    /// 仅摘除无内容锁、Dirty、WB 和外部使用者的静止页。
    pub fn prune_unused(&self) -> usize {
        self.index.prune_unused()
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

    /// 验收原索引的排序和平衡，不以预期页数替代树中真实结构。
    #[cfg(feature = "ktest")]
    pub(crate) fn ktest_validate(&self) -> (usize, u16) {
        self.index.entries.lock().validate()
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
