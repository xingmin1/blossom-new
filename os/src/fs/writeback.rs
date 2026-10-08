//! 文件与元数据回写共用原页、有限 ToWrite 选择和唯一 FS Bio 批次。

use alloc::vec::Vec;
use core::ops::Range;

use crate::block::{BlockError, FsClaim, MAX_BATCH};
use crate::cache::FolioMark;
use crate::fs::{BlockMapping, Ext4Volume, FsError, InodeRef};
use crate::mm::{FolioRef, FolioWriteback, PAGE_SIZE};

const SECTORS_PER_PAGE: usize = PAGE_SIZE / 512;
const WINDOW_RANGES: usize = MAX_BATCH * SECTORS_PER_PAGE;

/// 一个有界准备窗口只保存原页引用和真实块地址，不复制页内容。
struct FilePagePlan {
    index: u64,
    folio: FolioRef,
    targets: [Option<u64>; SECTORS_PER_PAGE],
}

/// 同步 inode 的全部文件范围，等待已有 WB 并只结束本轮选择。
pub(crate) fn sync_inode(inode: &InodeRef) -> Result<(), FsError> {
    sync_inode_range(inode, 0..u64::MAX)
}

/// 同步半开文件字节范围；数据互斥结束后再按 invalidate→WB 顺序重核脏义务。
pub(crate) fn sync_inode_range(inode: &InodeRef, range: Range<u64>) -> Result<(), FsError> {
    let result = {
        let _writeback = inode.mapping().lock_writeback();
        sync_inode_locked(inode, &range)
    };
    if result == Err(FsError::Io(BlockError::StopUnconfirmed)) {
        return result;
    }
    let refreshed = inode.refresh_dirty_obligation();
    result.and(refreshed)
}

/// 只协调共同元数据阶段及协商的真实 FLUSH，不串行不同 inode 的数据 IO。
pub(crate) fn sync_metadata(volume: &Ext4Volume) -> Result<(), FsError> {
    let cache = volume.metadata_cache();
    let _writeback = cache.lock_writeback();
    let result = sync_metadata_locked(volume);
    if result == Err(FsError::Io(BlockError::StopUnconfirmed)) {
        return result;
    }
    let flush = volume.claim().flush().map_err(FsError::from);
    if let Err(error) = flush {
        cache.record_wb_error(error);
    }
    result.and(flush)
}

/// 调用者持共同元数据 WB 互斥；只结束有限原页选择及真实 IO，不代替正式同步的 FLUSH。
pub(crate) fn sync_metadata_locked(volume: &Ext4Volume) -> Result<(), FsError> {
    let cache = volume.metadata_cache();
    let mut remaining = cache.mark_to_write_range(0, u64::MAX);
    let mut cursor = 0;
    let mut first_error = None;
    let mut ranges = if remaining == 0 {
        Vec::new()
    } else {
        match reserve_ranges() {
            Ok(ranges) => ranges,
            Err(error) => {
                first_error = Some(error);
                Vec::new()
            }
        }
    };
    while remaining != 0 {
        let mut selected = [const { None }; MAX_BATCH];
        let next = cache.select_range(cursor, u64::MAX, FolioMark::ToWrite, &mut selected);
        // 尚未建立本窗口 WB，旧 WB 等待不会阻止本窗口已接受的请求获得通知。
        for (_, folio) in selected.iter().flatten() {
            folio.wait_writeback();
        }
        for entry in &mut selected {
            let Some((byte_offset, folio)) = entry.take() else {
                continue;
            };
            remaining -= 1;
            let folio = folio.with_metadata_owner(cache.clone());
            if ranges.capacity() != 0 {
                let result =
                    prepare_metadata_page(volume.claim(), byte_offset, &folio, &mut ranges);
                remember_error(&mut first_error, result);
            }
            cache.set_to_write(byte_offset, false);
        }
        remember_error(&mut first_error, submit_ranges(volume.claim(), &mut ranges));
        let Some(next) = next else { break };
        cursor = next;
    }
    first_error.map_or(Ok(()), Err)
}

/// 调用者已持本 mapping 的 WB 互斥；地址准备结束后才建立 WB 和提交父批次。
pub(crate) fn sync_inode_locked(inode: &InodeRef, range: &Range<u64>) -> Result<(), FsError> {
    let (start, end) = page_window(range)?;
    if start == end {
        return Ok(());
    }
    let mapping = inode.mapping();
    let mut remaining = mapping.mark_to_write_range(start, end);
    let mut cursor = start;
    let mut first_error = None;
    let mut ranges = if remaining == 0 {
        Vec::new()
    } else {
        match reserve_ranges() {
            Ok(ranges) => ranges,
            Err(error) => {
                first_error = Some(error);
                Vec::new()
            }
        }
    };
    while remaining != 0 {
        let mut selected = [const { None }; MAX_BATCH];
        let next = mapping.select_range(cursor, end, FolioMark::ToWrite, &mut selected);
        let mut plans: [Option<FilePagePlan>; MAX_BATCH] = [const { None }; MAX_BATCH];
        for (slot, entry) in selected.iter_mut().enumerate() {
            let Some((index, folio)) = entry.take() else {
                continue;
            };
            remaining -= 1;
            let folio = folio.with_file_owner(inode.clone());
            folio.wait_writeback();
            if ranges.capacity() == 0 || folio.dirty_sectors() == 0 {
                mapping.set_to_write(index, false);
                continue;
            }
            match prepare_file_targets(inode, index, range) {
                Ok((_, targets)) => {
                    plans[slot] = Some(FilePagePlan {
                        index,
                        folio,
                        targets,
                    })
                }
                Err(error) => {
                    mapping.set_to_write(index, false);
                    remember_error(&mut first_error, Err(error));
                }
            }
        }
        for plan in IntoIterator::into_iter(plans).flatten() {
            let result = prepare_file_page(inode, &plan, &mut ranges);
            remember_error(&mut first_error, result);
            mapping.set_to_write(plan.index, false);
        }
        remember_error(
            &mut first_error,
            submit_ranges(inode.filesystem().claim(), &mut ranges),
        );
        let Some(next) = next else { break };
        cursor = next;
    }
    first_error.map_or(Ok(()), Err)
}

/// 回写和填充共用真实块地址及同根尺寸；根读保护不跨数据 IO、额度或完成等待。
pub(crate) fn prepare_file_targets(
    inode: &InodeRef,
    index: u64,
    range: &Range<u64>,
) -> Result<(u64, [Option<u64>; SECTORS_PER_PAGE]), FsError> {
    let volume = inode.filesystem();
    let size = volume.geometry().block_size;
    let page = index
        .checked_mul(PAGE_SIZE as u64)
        .ok_or(FsError::FileTooLarge)?;
    inode.with_map_read(|metadata, root| {
        let mut targets = [None; SECTORS_PER_PAGE];
        for offset in (0..PAGE_SIZE).step_by(size) {
            let position = page
                .checked_add(offset as u64)
                .ok_or(FsError::FileTooLarge)?;
            if metadata.size <= position || range.end <= position {
                break;
            }
            if position.saturating_add(size as u64) <= range.start {
                continue;
            }
            if let BlockMapping::Mapped { physical, .. } =
                volume.map_block(metadata, root, position / size as u64)?
            {
                targets[offset / size] =
                    Some(physical.checked_mul(size as u64).ok_or(FsError::Corrupt)?);
            }
        }
        Ok((metadata.size, targets))
    })
}

/// 当前内容锁不能立即取得时，先提交并通知已准备的前缀再进入真实等待。
fn prepare_file_page(
    inode: &InodeRef,
    plan: &FilePagePlan,
    ranges: &mut Vec<(u64, FolioWriteback)>,
) -> Result<(), FsError> {
    let mut content = match plan.folio.try_lock_content() {
        Some(content) => content,
        None => {
            submit_ranges(inode.filesystem().claim(), ranges)?;
            plan.folio.lock_content()
        }
    };
    inode
        .mapping()
        .mkclean(plan.index, &plan.folio, &mut content);
    let size = inode.filesystem().geometry().block_size;
    let dirty = plan.folio.dirty_sectors();
    for offset in (0..PAGE_SIZE).step_by(size) {
        if dirty & range_mask(offset, size) == 0 {
            continue;
        }
        let Some(target) = plan.targets[offset / size] else {
            continue;
        };
        // SAFETY: 本内容 guard 排斥许可提交；mkclean 已撤同源共享 W 并完成全 CPU CSD。
        // 原页使用 coherent ToDevice，之后的原始写另记 Dirty，不导出 Rust 内容切片；
        // 返回凭证移入唯一父 Bio，设备及生成者全尾前始终保活 inode 和原页。
        if let Some(writeback) = unsafe { content.begin_writeback(offset, size)? } {
            ranges.push((target, writeback));
        }
    }
    Ok(())
}

/// 元数据 WB 只覆盖真实有效的 512B 窗口；内容借用不得跨其 DMA。
fn prepare_metadata_page(
    claim: &FsClaim,
    byte_offset: u64,
    folio: &FolioRef,
    ranges: &mut Vec<(u64, FolioWriteback)>,
) -> Result<(), FsError> {
    let mut content = match folio.try_lock_content() {
        Some(content) => content,
        None => {
            submit_ranges(claim, ranges)?;
            folio.wait_writeback();
            folio.lock_content()
        }
    };
    let mut pending = folio.dirty_sectors();
    while pending != 0 {
        let start = pending.trailing_zeros() as usize;
        let count = (pending >> start).trailing_ones() as usize;
        let offset = start * 512;
        let len = count * 512;
        let target = byte_offset
            .checked_add(offset as u64)
            .ok_or(BlockError::InvalidRange)?;
        // SAFETY: 元数据终身不授给用户，content guard 排斥可变借用；begin_writeback
        // 校验准确窗口已初始化并清脏，原 cache/claim 由凭证保活到真实设备全尾。
        if let Some(writeback) = unsafe { content.begin_writeback(offset, len)? } {
            ranges.push((target, writeback));
        }
        pending &= !range_mask(offset, len);
    }
    Ok(())
}

/// 为最多八页的准确 sector 范围预留存储，内容锁内 push 不分配。
fn reserve_ranges() -> Result<Vec<(u64, FolioWriteback)>, FsError> {
    let mut ranges = Vec::new();
    ranges
        .try_reserve_exact(WINDOW_RANGES)
        .map_err(|_| FsError::NoMemory)?;
    Ok(ranges)
}

/// 父批次沿原核心统一 kick 并观察全部完成后返回首错。
fn submit_ranges(claim: &FsClaim, ranges: &mut Vec<(u64, FolioWriteback)>) -> Result<(), FsError> {
    if ranges.is_empty() {
        return Ok(());
    }
    claim.writeback_batch(ranges).map_err(FsError::from)
}

/// 保留本轮首次错误，后续父仍完成自己的全部尾部。
fn remember_error(first: &mut Option<FsError>, result: Result<(), FsError>) {
    if first.is_none() {
        *first = result.err();
    }
}

/// 换算半开 byte 范围，整文件哨兵和非页对齐尾都不溢出。
fn page_window(range: &Range<u64>) -> Result<(u64, u64), FsError> {
    if range.end < range.start {
        return Err(FsError::InvalidOperation);
    }
    let start = range.start / PAGE_SIZE as u64;
    if range.start == range.end {
        return Ok((start, start));
    }
    let end = range.end / PAGE_SIZE as u64 + u64::from(range.end % PAGE_SIZE as u64 != 0);
    Ok((start, end))
}

/// 把已经校验的页内准确范围转换为原 PageDesc 的八位 sector mask。
fn range_mask(offset: usize, len: usize) -> u8 {
    (((1u16 << (len / 512)) - 1) << (offset / 512)) as u8
}
