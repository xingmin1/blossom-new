//! 文件与元数据回写共用原页、范围 WB 凭证和唯一受限 FS Bio 提交。

use crate::block::BlockError;
use crate::cache::{FileMapping, FolioMark, MetadataCache};
use crate::fs::{BlockMapping, Ext4Volume, FsError, InodeRef};
use crate::mm::{FolioRef, PAGE_SIZE};

/// 调用者停止该 inode 的新写入后排空本轮文件义务，不把此入口当完整用户 fsync。
/// 块映射读保护延续到真实 IO 全尾；文件数据完成后仍须同步卷元数据和设备缓存。
pub(crate) fn sync_inode(inode: &InodeRef) -> Result<(), FsError> {
    let mapping = inode.mapping();
    let budget = mapping.len();
    select_inode_work(mapping, budget, FolioMark::Dirty);
    select_inode_work(mapping, budget, FolioMark::Writeback);
    let mut cursor = 0;
    for _ in 0..budget {
        let mut selected = [None];
        let next = mapping.select_range(cursor, u64::MAX, FolioMark::ToWrite, &mut selected);
        if let Some((index, folio)) = selected[0].take() {
            let folio = folio.with_file_owner(inode.clone());
            folio.wait_writeback();
            sync_file_page(inode, index, &folio)?;
            mapping.set_to_write(index, false);
        }
        let Some(next) = next else {
            break;
        };
        cursor = next;
    }
    Ok(())
}

/// 卷新写者及所有文件数据义务停止后写出共同元数据并执行真实 FLUSH。
/// 本入口不遍历另一份 inode 库存，调用者须先同步各个实际修改的 inode。
pub(crate) fn sync_volume(volume: &Ext4Volume) -> Result<(), FsError> {
    let cache = volume.metadata_cache();
    let budget = cache.len();
    select_metadata_work(cache, budget, FolioMark::Dirty);
    select_metadata_work(cache, budget, FolioMark::Writeback);
    let mut cursor = 0;
    for _ in 0..budget {
        let mut selected = [None];
        let next = cache.select_range(cursor, u64::MAX, FolioMark::ToWrite, &mut selected);
        if let Some((byte_offset, folio)) = selected[0].take() {
            let folio = folio.with_metadata_owner(cache.clone());
            folio.wait_writeback();
            sync_metadata_page(volume, byte_offset, &folio)?;
            cache.set_to_write(byte_offset, false);
        }
        let Some(next) = next else {
            break;
        };
        cursor = next;
    }
    volume.claim().flush()?;
    Ok(())
}

/// 按开始时的节点数标记有限工作集，扫描不随新 Dirty 无限扩大。
fn select_inode_work(mapping: &FileMapping, budget: usize, mark: FolioMark) {
    let mut cursor = 0;
    for _ in 0..budget {
        let mut selected = [None];
        let next = mapping.select_range(cursor, u64::MAX, mark, &mut selected);
        if let Some((index, _folio)) = selected[0].take() {
            mapping.set_to_write(index, true);
        }
        let Some(next) = next else {
            break;
        };
        cursor = next;
    }
}

/// 物理 byte key 沿同一稀疏索引建立有限元数据工作集，不保存页内容快照。
fn select_metadata_work(cache: &MetadataCache, budget: usize, mark: FolioMark) {
    let mut cursor = 0;
    for _ in 0..budget {
        let mut selected = [None];
        let next = cache.select_range(cursor, u64::MAX, mark, &mut selected);
        if let Some((byte_offset, _folio)) = selected[0].take() {
            cache.set_to_write(byte_offset, true);
        }
        let Some(next) = next else {
            break;
        };
        cursor = next;
    }
}

/// 在当前块根读侧逐 FS 块交原 folio；持内容锁撤共享 W 并完成 CSD 后才清脏。
fn sync_file_page(inode: &InodeRef, index: u64, folio: &FolioRef) -> Result<(), FsError> {
    inode.with_map_read(|metadata, root| {
        let volume = inode.filesystem();
        let block_size = volume.geometry().block_size;
        let page_start = index
            .checked_mul(PAGE_SIZE as u64)
            .ok_or(FsError::FileTooLarge)?;
        for offset in (0..PAGE_SIZE).step_by(block_size) {
            let position = page_start
                .checked_add(offset as u64)
                .ok_or(FsError::FileTooLarge)?;
            if metadata.size <= position {
                break;
            }
            let mask = range_mask(offset, block_size);
            if folio.dirty_sectors() & mask == 0 {
                continue;
            }
            let logical = position / block_size as u64;
            let BlockMapping::Mapped { physical, .. } =
                volume.map_block(metadata, root, logical)?
            else {
                return Err(FsError::Corrupt);
            };
            let byte_offset = physical
                .checked_mul(block_size as u64)
                .ok_or(FsError::Corrupt)?;
            let writeback = {
                let mut content = folio.lock_content();
                inode.mapping().mkclean(index, folio, &mut content);
                // SAFETY: mkclean 已在本内容 guard 内撤全部同源共享 W 并完成全 CPU 同步；
                // guard 继续排斥 mkwrite 的新授权，root 读侧保护物理块到真实 IO 全尾。
                // WB 期间最终授权拒绝该页；凭证由唯一 Bio 保活到设备收权才完成。
                unsafe { content.begin_writeback(offset, block_size)? }
            };
            if let Some(writeback) = writeback {
                volume.claim().write_folio(byte_offset, writeback)?;
            }
        }
        Ok(())
    })
}

/// 只写真实有效且 Dirty 的连续 sector，不扩大到页内未加载或设备之外的字节。
fn sync_metadata_page(
    volume: &Ext4Volume,
    byte_offset: u64,
    folio: &FolioRef,
) -> Result<(), FsError> {
    let mut pending = folio.dirty_sectors();
    while pending != 0 {
        let start = pending.trailing_zeros() as usize;
        let count = (pending >> start).trailing_ones() as usize;
        let offset = start * 512;
        let len = count * 512;
        let target = byte_offset
            .checked_add(offset as u64)
            .ok_or(BlockError::InvalidRange)?;
        let writeback = {
            let mut content = folio.lock_content();
            // SAFETY: 终身 Metadata 角色不授给用户，内容 guard 排斥可变切片；
            // 本轮窗口只含已加载的真实 Dirty sector，凭证阻止新可变借用跨设备访问，
            // 原 cache/claim 保活，Bio 全部 child 及生成者实际收权后才结束 WB。
            unsafe { content.begin_writeback(offset, len)? }
        };
        if let Some(writeback) = writeback {
            volume.claim().write_folio(target, writeback)?;
        }
        pending &= !range_mask(offset, len);
    }
    Ok(())
}

/// 把已验证页内 FS 块或 sector 窗口转换为实际八位范围。
fn range_mask(offset: usize, len: usize) -> u8 {
    let sectors = len / 512;
    (((1u16 << sectors) - 1) << (offset / 512)) as u8
}
