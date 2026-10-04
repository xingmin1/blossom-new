//! 显式批次只保留固定生成者与准备槽，设备访问始终使用原页的有界 SG。

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::convert::TryFrom;

use virtio_drivers::{BufferDirection, MappedSegment};

use crate::block::bio::BioInner;
use crate::block::disk::{ActiveBio, PreparingSlot};
use crate::block::{Bio, BlkOperation, BlockError, Disk, QueueLimits, MAX_BATCH, SECTOR_BYTES};
use crate::mm::dma::{DmaError, MappedPages};
use crate::mm::{alloc_pages, PAGE_SIZE};
use crate::task;

/// 当前位置只引用父 Bio 的段编号，跨 child 保留游标避免从首页反复扫描。
#[derive(Clone, Copy, Default)]
struct SegmentCursor {
    index: usize,
    within: usize,
}

impl SegmentCursor {
    /// 按真实原段消耗字节；游标从不复制页拥有权或访问 payload。
    fn advance(&mut self, bio: &BioInner, mut bytes: usize) -> Result<(), BlockError> {
        while 0 < bytes {
            let segment = bio.segment(self.index).ok_or(BlockError::InvalidRange)?;
            let available = (segment.length as usize)
                .checked_sub(self.within)
                .ok_or(BlockError::InvalidRange)?;
            if available == 0 {
                return Err(BlockError::InvalidRange);
            }
            let consumed = available.min(bytes);
            self.within += consumed;
            bytes -= consumed;
            if self.within == segment.length as usize {
                self.index += 1;
                self.within = 0;
            }
        }
        Ok(())
    }
}

/// 尚未发表的计划只保存固定元数据，发表成功后才推进各父的已生成位置。
struct GenerationPlan {
    indices: [usize; MAX_BATCH],
    lengths: [usize; MAX_BATCH],
    cursors: [SegmentCursor; MAX_BATCH],
    count: usize,
    #[cfg(any(feature = "storage-tests", feature = "ktest"))]
    splits: usize,
}

impl Disk {
    /// 普通 Task 同步生成全部 child；返回后仍须由 BioCompletion 等待真实完成。
    pub(crate) fn submit_bio(&self, bio: Bio) {
        drop(task::current_task());
        let mut active: [Option<ActiveBio<'_>>; MAX_BATCH] = [const { None }; MAX_BATCH];
        match self.admit(bio) {
            Ok(accepted) => active[0] = Some(accepted),
            Err((bio, error)) => {
                self.unplug();
                bio.inner.reject(error);
                return;
            }
        }
        self.submit_accepted(&mut active);
    }

    /// 消耗显式批次，每组最多八个生成者，组内相邻普通范围才允许合并。
    pub(crate) fn submit_batch(&self, bios: Vec<Bio>) {
        drop(task::current_task());
        let mut pending = bios.into_iter();
        loop {
            let mut active: [Option<ActiveBio<'_>>; MAX_BATCH] = [const { None }; MAX_BATCH];
            for entry in &mut active {
                let Some(bio) = pending.next() else {
                    break;
                };
                match self.admit(bio) {
                    Ok(accepted) => *entry = Some(accepted),
                    Err((bio, error)) => {
                        self.unplug();
                        bio.inner.reject(error);
                    }
                }
            }
            self.submit_accepted(&mut active);
            if pending.len() == 0 {
                return;
            }
        }
    }

    /// 每个 accepted root 都在通知已发表前缀后结束生成责任，失败不取消旧 child。
    fn submit_accepted(&self, active: &mut [Option<ActiveBio<'_>>; MAX_BATCH]) {
        let mut generated = [false; MAX_BATCH];
        let error = self.generate_children(active, &mut generated).err();
        self.unplug();
        for (index, entry) in active.iter_mut().enumerate() {
            if let Some(accepted) = entry.take() {
                let failure = if generated[index] {
                    None
                } else {
                    error.map(|error| (accepted.offset, error))
                };
                accepted.finish(failure);
            }
        }
    }

    /// 准备与发表按 child 顺序执行；描述符等待由同一个 PreparingSlot 保有 SG。
    fn generate_children(
        &self,
        active: &mut [Option<ActiveBio<'_>>; MAX_BATCH],
        generated: &mut [bool; MAX_BATCH],
    ) -> Result<(), BlockError> {
        let mut cursors = [SegmentCursor::default(); MAX_BATCH];
        let mut next = next_active(active, 0);
        while next < MAX_BATCH {
            let mut slot = self.reserve_slot()?;
            let plan = self.prepare_child(&mut slot, active, cursors, next)?;
            slot.publish()?;
            #[cfg(any(feature = "storage-tests", feature = "ktest"))]
            self.record_generation(plan.splits, plan.count - 1);
            for part in 0..plan.count {
                let index = plan.indices[part];
                let accepted = active[index].as_mut().expect("planned Bio is absent");
                accepted.offset += plan.lengths[part];
                cursors[index] = plan.cursors[index];
                generated[index] = accepted.bio.operation() == BlkOperation::Flush
                    || accepted.offset == accepted.bio.len();
                next = if generated[index] {
                    next_active(active, index + 1)
                } else {
                    index
                };
            }
        }
        Ok(())
    }

    /// 锁外填充一个固定槽，逐父裁剪到逻辑块边界后才保存对应完成义务。
    fn prepare_child(
        &self,
        slot: &mut PreparingSlot<'_>,
        active: &[Option<ActiveBio<'_>>; MAX_BATCH],
        cursors: [SegmentCursor; MAX_BATCH],
        first: usize,
    ) -> Result<GenerationPlan, BlockError> {
        let root = active[first].as_ref().expect("first Bio is absent");
        let operation = root.bio.operation();
        let sector = root
            .bio
            .sector()
            .checked_add((root.offset as u64) / SECTOR_BYTES)
            .ok_or(BlockError::InvalidRange)?;
        let mut plan = GenerationPlan {
            indices: [0; MAX_BATCH],
            lengths: [0; MAX_BATCH],
            cursors,
            count: 0,
            #[cfg(any(feature = "storage-tests", feature = "ktest"))]
            splits: 0,
        };
        if operation == BlkOperation::Flush {
            slot.push_part(Arc::clone(&root.bio), 0, 0)?;
            slot.configure(operation, 0, 0)?;
            plan.indices[0] = first;
            plan.count = 1;
            return Ok(plan);
        }
        let limits = self.limits();
        let mut bytes = 0;
        let mut index = first;
        while index < MAX_BATCH && slot.part_count() < MAX_BATCH {
            let accepted = active[index].as_ref().expect("merge Bio is absent");
            let bio = &accepted.bio;
            let current_sector = bio
                .sector()
                .checked_add((accepted.offset as u64) / SECTOR_BYTES)
                .ok_or(BlockError::InvalidRange)?;
            let end_sector = sector
                .checked_add((bytes as u64) / SECTOR_BYTES)
                .ok_or(BlockError::InvalidRange)?;
            if bio.operation() != operation || current_sector != end_sector {
                break;
            }
            // 可睡回调可能等待相邻父；合并到同一槽会让它阻塞后续 part 的完成。
            if plan.count != 0 && (root.bio.needs_task() || bio.needs_task()) {
                break;
            }
            let remaining = bio
                .len()
                .checked_sub(accepted.offset)
                .ok_or(BlockError::InvalidRange)?;
            let budget = remaining.min(limits.max_bytes - bytes);
            let length =
                append_prefix(bio, &mut plan.cursors[index], slot.sg_mut(), limits, budget)?;
            if length == 0 {
                if plan.count == 0 {
                    return Err(BlockError::Unsupported);
                }
                break;
            }
            slot.push_part(Arc::clone(bio), accepted.offset, length)?;
            plan.indices[plan.count] = index;
            plan.lengths[plan.count] = length;
            plan.count += 1;
            #[cfg(any(feature = "storage-tests", feature = "ktest"))]
            {
                plan.splits += usize::from(0 < accepted.offset);
            }
            bytes += length;
            if length < remaining || bytes == limits.max_bytes {
                break;
            }
            index = next_active(active, index + 1);
        }
        debug_assert!(0 < bytes && !slot.sg().is_empty());
        slot.configure(operation, sector, bytes)?;
        Ok(plan)
    }

    /// 普通 Task 用请求自有零页完成字节范围读取，只有全长成功才修改调用者缓冲区。
    /// 非空范围必须符合设备逻辑块对齐；文件页填充直接使用自己的页提交 Bio。
    pub fn read_exact_at(&self, offset: u64, dst: &mut [u8]) -> Result<(), BlockError> {
        drop(task::current_task());
        let result = self.read_with_pages(offset, dst);
        self.unplug();
        result
    }

    /// 所有 fallible 页与 Vec 准备都在队列锁外，错误完成仍通过原拥有者归还原阶。
    fn read_with_pages(&self, offset: u64, dst: &mut [u8]) -> Result<(), BlockError> {
        let limits = self.limits();
        let logical = u64::from(limits.logical_block_size);
        let capacity = limits
            .capacity_sectors
            .checked_mul(SECTOR_BYTES)
            .ok_or(BlockError::InvalidRange)?;
        let end = offset
            .checked_add(u64::try_from(dst.len()).map_err(|_| BlockError::InvalidRange)?)
            .ok_or(BlockError::InvalidRange)?;
        if capacity < end {
            return Err(BlockError::InvalidRange);
        }
        if dst.is_empty() {
            return Ok(());
        }
        if offset % logical != 0 || (dst.len() as u64) % logical != 0 {
            return Err(BlockError::Misaligned);
        }
        let total = dst.len();
        let page_count = total / PAGE_SIZE + usize::from(total % PAGE_SIZE != 0);
        let mut memory = Vec::new();
        memory
            .try_reserve_exact(page_count)
            .map_err(|_| BlockError::NoMemory)?;
        let mut remaining = total;
        while 0 < remaining {
            let length = remaining.min(PAGE_SIZE);
            let pages = alloc_pages(0, true).ok_or(BlockError::NoMemory)?;
            let mapped = MappedPages::new(pages, 0, length, BufferDirection::DeviceToDriver)
                .map_err(map_error)?;
            memory.push(mapped);
            remaining -= length;
        }
        // SAFETY: 每个映射持有独占原零页，全部 CPU 别名在移入前结束；读请求无回调，
        // 页在 Bio 最终完成前不被 CPU 使用，失败与短读也不将未成功数据复制到 dst。
        let bio = unsafe {
            Bio::try_new(
                self,
                BlkOperation::Read,
                offset / SECTOR_BYTES,
                memory,
                0,
                None,
            )
        }?;
        let completion = bio.completion();
        self.submit_bio(bio);
        self.unplug();
        let result = completion.wait();
        let memory = completion.take_memory()?;
        if let Some(error) = result.error {
            return Err(error);
        }
        if result.completed != total {
            return Err(BlockError::ShortIo);
        }
        let mut copied = 0;
        for mapped in &memory {
            // SAFETY: 原页分配时整页清零，整个读取范围已全长成功；wait/take_memory
            // 已结束所有 child 的 DMA 并收回 CPU 拥有权，局部拥有者禁止并发写与提前归还。
            let bytes = unsafe { mapped.bytes() };
            let length = mapped.len();
            dst[copied..copied + length].copy_from_slice(bytes);
            copied += length;
        }
        debug_assert_eq!(copied, dst.len());
        Ok(())
    }
}

/// 略过被 admission 拒绝的 root，不为批次建立第二份拥有队列。
fn next_active(active: &[Option<ActiveBio<'_>>; MAX_BATCH], start: usize) -> usize {
    (start..MAX_BATCH)
        .find(|index| active[*index].is_some())
        .unwrap_or(MAX_BATCH)
}

/// 先取有限最小值再加一，完整边界掩码与地址零不能使边界额度溢出。
fn segment_limit(limits: QueueLimits, address: u64) -> usize {
    let before_end = limits.dma_boundary_mask - (address & limits.dma_boundary_mask);
    (before_end.min(u64::from(limits.max_segment_bytes) - 1) + 1) as usize
}

/// 将父原地址写入已预留 SG，最终向下对齐并裁掉尾段，不建立任何 payload bounce。
fn append_prefix(
    bio: &BioInner,
    cursor: &mut SegmentCursor,
    sg: &mut Vec<MappedSegment>,
    limits: QueueLimits,
    budget: usize,
) -> Result<usize, BlockError> {
    debug_assert!(limits.max_segments <= sg.capacity());
    let mut scan = *cursor;
    let mut bytes = 0;
    while bytes < budget {
        let source = bio.segment(scan.index).ok_or(BlockError::InvalidRange)?;
        let available = (source.length as usize)
            .checked_sub(scan.within)
            .ok_or(BlockError::InvalidRange)?;
        if available == 0 {
            return Err(BlockError::InvalidRange);
        }
        let address = source
            .address
            .checked_add(scan.within)
            .ok_or(BlockError::InvalidRange)?;
        let remaining = available.min(budget - bytes);
        let mut length = 0;
        if let Some(last) = sg.last_mut() {
            let contiguous = last.direction == source.direction
                && last.address.checked_add(last.length as usize) == Some(address);
            if contiguous {
                let room = segment_limit(limits, last.address.value())
                    .checked_sub(last.length as usize)
                    .ok_or(BlockError::InvalidRange)?;
                length = room.min(remaining);
                last.length += length as u32;
            }
        }
        if length == 0 {
            if sg.len() == limits.max_segments {
                break;
            }
            length = segment_limit(limits, address.value()).min(remaining);
            if length == 0 {
                return Err(BlockError::Unsupported);
            }
            sg.push(MappedSegment {
                address,
                length: length as u32,
                direction: source.direction,
            });
        }
        scan.advance(bio, length)?;
        bytes += length;
    }
    let logical = limits.logical_block_size as usize;
    let prefix = bytes / logical * logical;
    trim_tail(sg, bytes - prefix);
    cursor.advance(bio, prefix)?;
    Ok(prefix)
}

/// 裁掉本轮追加范围的尾部，包括与旧尾段合并的部分，保留既有 parts 的完整字节。
fn trim_tail(sg: &mut Vec<MappedSegment>, mut bytes: usize) {
    while 0 < bytes {
        let last = sg.last_mut().expect("SG prefix tail is absent");
        let removed = bytes.min(last.length as usize);
        last.length -= removed as u32;
        bytes -= removed;
        if last.length == 0 {
            sg.pop();
        }
    }
}

/// 页准备失败保留原内存错误，地址或映射跨度失败归为输入范围错误。
fn map_error(error: DmaError) -> BlockError {
    match error {
        DmaError::NoMemory => BlockError::NoMemory,
        DmaError::InvalidRange | DmaError::AddressLimit => BlockError::InvalidRange,
    }
}
