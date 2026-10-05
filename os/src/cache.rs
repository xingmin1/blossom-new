//! 元数据块与文件页分别索引原 Folio，读入责任在发表前建立。

use alloc::vec::Vec;
use core::convert::TryFrom;

use crate::block::{BlockError, DiskRef, SECTOR_BYTES};
use crate::mm::{FolioRead, FolioRef, PAGE_SIZE};
use crate::sync::SpinLock;

struct Entry<K> {
    key: K,
    folio: FolioRef,
}

/// 固定预算的页索引只保护身份发表，不跨读入、等待或原页归还。
struct FolioIndex<K> {
    entries: SpinLock<Vec<Entry<K>>>,
    capacity: usize,
}

impl<K: Copy + Eq> FolioIndex<K> {
    /// 在锁外一次预留全部索引槽；零预算无法保存任何页，直接拒绝。
    fn try_new(capacity: usize) -> Result<Self, BlockError> {
        if capacity == 0 {
            return Err(BlockError::InvalidRange);
        }
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(capacity)
            .map_err(|_| BlockError::NoMemory)?;
        Ok(Self {
            entries: SpinLock::new(entries),
            capacity,
        })
    }

    /// 预算已满时先移出仅索引持有的静止页，避免无用候选分配或占满页后的假 OOM。
    fn evict_if_full(&self, entries: &mut Vec<Entry<K>>) -> Result<Option<Entry<K>>, BlockError> {
        if entries.len() < self.capacity {
            return Ok(None);
        }
        let position = entries
            .iter()
            .position(|entry| entry.folio.can_prune())
            .ok_or(BlockError::NoMemory)?;
        Ok(Some(entries.swap_remove(position)))
    }

    /// 先查命中与真实预算；移出的静止页交调用者在候选分配前锁外归还。
    fn prepare_slot(&self, key: K) -> Result<(Option<FolioRef>, Option<Entry<K>>), BlockError> {
        let mut entries = self.entries.lock();
        if let Some(entry) = entries.iter().find(|entry| entry.key == key) {
            return Ok((Some(entry.folio.clone()), None));
        }
        Ok((None, self.evict_if_full(&mut entries)?))
    }

    /// 双查后发表已 Locked 的候选；被替换的独占静止页交调用者锁外释放。
    fn publish(
        &self,
        key: K,
        candidate: &FolioRef,
    ) -> Result<(Option<FolioRef>, Option<Entry<K>>), BlockError> {
        let mut entries = self.entries.lock();
        if let Some(entry) = entries.iter().find(|entry| entry.key == key) {
            return Ok((Some(entry.folio.clone()), None));
        }
        // 锁顺序为索引锁到 PageUse 锁；这里只检查静止状态或增加引用。
        let victim = self.evict_if_full(&mut entries)?;
        // 构造时已经预留全部预算，移出 victim 后必有空位，push 不分配。
        entries.push(Entry {
            key,
            folio: candidate.clone(),
        });
        Ok((None, victim))
    }

    /// 锁外准备候选与唯一读入责任；竞争失败者不提交候选的 I/O。
    fn get_or_prepare(
        &self,
        key: K,
        offset: usize,
        len: usize,
    ) -> Result<(FolioRef, Option<FolioRead>), BlockError> {
        let (existing, victim) = self.prepare_slot(key)?;
        drop(victim);
        if let Some(folio) = existing {
            return Ok((folio, None));
        }
        let candidate = FolioRef::allocate().ok_or(BlockError::NoMemory)?;
        let read = candidate.begin_read(offset, len)?;
        let (existing, victim) = self.publish(key, &candidate)?;
        drop(victim);
        match existing {
            Some(folio) => {
                // 读入尚未交设备；取消 loser 只结束其私有候选，索引中的页不受影响。
                drop(read);
                drop(candidate);
                Ok((folio, None))
            }
            None => Ok((candidate, Some(read))),
        }
    }

    /// 移出仅索引持有的静止页，逐页解锁后归还；并发新发表的页可留待下次回收。
    fn prune_unused(&self) -> usize {
        let mut removed = 0;
        // 反向检查初始槽数，swap_remove 不使尚未检查的低位发生移动。
        for position in (0..self.len()).rev() {
            let victim = {
                let mut entries = self.entries.lock();
                if entries
                    .get(position)
                    .is_some_and(|entry| entry.folio.can_prune())
                {
                    Some(entries.swap_remove(position))
                } else {
                    None
                }
            };
            if victim.is_some() {
                removed += 1;
            }
            drop(victim);
        }
        removed
    }

    /// 观察当前占用的页索引槽数，不把在途页排除在预算之外。
    fn len(&self) -> usize {
        self.entries.lock().len()
    }
}

/// 一个真实设备的元数据块缓存，块大小属于键的一部分。
pub struct MetadataCache {
    disk: DiskRef,
    index: FolioIndex<(u64, usize)>,
}

impl MetadataCache {
    /// 绑定实际设备，并在锁外一次预留固定的元数据页预算。
    pub fn try_new(disk: DiskRef, capacity: usize) -> Result<Self, BlockError> {
        Ok(Self {
            disk,
            index: FolioIndex::try_new(capacity)?,
        })
    }

    /// 读取一个 1/2/4KiB 元数据块；只有新页拥有者提交真实 I/O，其余 Task 等同页结果。
    pub fn get(&self, block: u64, block_size: usize) -> Result<BufferRef, BlockError> {
        let (byte_offset, offset) = self.block_range(block, block_size)?;
        let (folio, read) =
            self.index
                .get_or_prepare((byte_offset, block_size), offset, block_size)?;
        if let Some(read) = read {
            self.disk.load_folio(byte_offset, read)?;
        }
        folio.wait_ready()?;
        Ok(BufferRef {
            folio,
            offset,
            len: block_size,
        })
    }

    /// 检查块到字节的转换、设备边界和页内范围，在发表候选前拒绝错误几何。
    fn block_range(&self, block: u64, block_size: usize) -> Result<(u64, usize), BlockError> {
        if !matches!(block_size, 1024 | 2048 | 4096) {
            return Err(BlockError::InvalidRange);
        }
        let size = u64::try_from(block_size).map_err(|_| BlockError::InvalidRange)?;
        let byte_offset = block.checked_mul(size).ok_or(BlockError::InvalidRange)?;
        let end = byte_offset
            .checked_add(size)
            .ok_or(BlockError::InvalidRange)?;
        let limits = self.disk.limits();
        let capacity = limits
            .capacity_sectors
            .checked_mul(SECTOR_BYTES)
            .ok_or(BlockError::InvalidRange)?;
        if capacity < end {
            return Err(BlockError::InvalidRange);
        }
        let logical = u64::from(limits.logical_block_size);
        if byte_offset % logical != 0 || size % logical != 0 {
            return Err(BlockError::Misaligned);
        }
        let offset = usize::try_from(byte_offset % PAGE_SIZE as u64)
            .map_err(|_| BlockError::InvalidRange)?;
        if PAGE_SIZE
            < offset
                .checked_add(block_size)
                .ok_or(BlockError::InvalidRange)?
        {
            return Err(BlockError::InvalidRange);
        }
        Ok((byte_offset, offset))
    }

    /// 显式归还仅缓存持有的静止元数据页，失败页同样可以回收。
    pub fn prune_unused(&self) -> usize {
        self.index.prune_unused()
    }

    /// 返回当前元数据索引占用，包括仍在真实读入的页。
    pub fn len(&self) -> usize {
        self.index.len()
    }

    /// 观察缓存是否尚未持有任何元数据页。
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 返回构造时固定的页预算，不随底层 Vec 的实际保留空间增大。
    pub fn capacity(&self) -> usize {
        self.index.capacity
    }
}

/// 原 Folio 内已初始化的元数据块视图，不复制内容。
#[derive(Clone)]
pub struct BufferRef {
    folio: FolioRef,
    offset: usize,
    len: usize,
}

impl BufferRef {
    /// 短暂访问本块内容，借用不能跨消费者返回或脱离 Folio 内容保护。
    pub fn with_bytes<T>(&self, consume: impl FnOnce(&[u8]) -> T) -> Result<T, BlockError> {
        self.folio.with_bytes(self.offset, self.len, consume)
    }

    /// 返回本块的字节数，独立于 Folio 的物理页大小。
    pub fn len(&self) -> usize {
        self.len
    }

    /// 元数据块视图总是非空，零长度范围不会进入缓存。
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// 返回本块在原物理页中的字节偏移。
    pub fn offset(&self) -> usize {
        self.offset
    }

    /// 借用实际原页身份，观察或持有该页不会建立第二份内容。
    pub fn folio(&self) -> &FolioRef {
        &self.folio
    }
}

/// 活文件对象自己的页索引；实际文件来源消费唯一读入 owner。
pub struct FileMapping {
    index: FolioIndex<u64>,
}

impl FileMapping {
    /// 在锁外一次预留固定的文件页预算，不建立格式或设备加载器。
    pub fn try_new(capacity: usize) -> Result<Self, BlockError> {
        Ok(Self {
            index: FolioIndex::try_new(capacity)?,
        })
    }

    /// 按文件页号返回同页；Some 是唯一填充责任，None 的消费者必须等待原页结果。
    ///
    /// 新页的 owner 覆盖整页，来源必须完整填充后完成；错误完成保留原原因并唤醒等待者。
    pub fn get_or_prepare(&self, index: u64) -> Result<(FolioRef, Option<FolioRead>), BlockError> {
        self.index.get_or_prepare(index, 0, PAGE_SIZE)
    }

    /// 显式归还仅 mapping 持有的静止页，不替换仍被消费者持有的身份。
    pub fn prune_unused(&self) -> usize {
        self.index.prune_unused()
    }

    /// 返回已发表的文件页数，在途与失败页仍计入预算。
    pub fn len(&self) -> usize {
        self.index.len()
    }

    /// 观察此 mapping 是否尚未持有文件页。
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 返回构造时固定的文件页预算。
    pub fn capacity(&self) -> usize {
        self.index.capacity
    }
}

#[cfg(feature = "ktest")]
pub(crate) mod ktest;
