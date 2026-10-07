//! 目录按物理字节游标线性枚举；索引节点只验证结构与 CRC，不生成伪项。

use alloc::vec::Vec;

use crate::cache::BufferRef;
use crate::fs::ext4::codec::{self, INDEX, METADATA_CSUM};
use crate::fs::ext4::{BlockMapping, DirectoryEntry, Ext4Volume, Geometry, InodeKind};
use crate::fs::inode::InodeMetadata;
use crate::fs::FsError;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DirectoryLayout {
    Leaf { end: usize },
    Root,
    Index,
}

#[derive(Clone, Copy)]
struct Record {
    number: u64,
    name_offset: usize,
    name_len: usize,
    kind: Option<InodeKind>,
    next: usize,
}

/// 检查目录类型及完整块大小，避免将不完整尾块解释成目录项。
fn check_directory(volume: &Ext4Volume, inode: &InodeMetadata) -> Result<(), FsError> {
    if inode.kind != InodeKind::Directory {
        return Err(FsError::NotDirectory);
    }
    if inode.size == 0 || inode.size % volume.geometry().block_size as u64 != 0 {
        return Err(FsError::Corrupt);
    }
    if inode.flags & INDEX != 0 && volume.geometry().features_compat & 0x20 == 0 {
        return Err(FsError::Corrupt);
    }
    Ok(())
}

/// 读取一个真实目录块，空洞或未写入块不能充当空目录。
fn directory_block(
    volume: &Ext4Volume,
    inode: &InodeMetadata,
    map_root: &[u8; 60],
    logical: u64,
) -> Result<BufferRef, FsError> {
    let physical = match volume.map_block(inode, map_root, logical)? {
        BlockMapping::Mapped { physical, .. } => physical,
        _ => return Err(FsError::Corrupt),
    };
    volume.metadata_block(physical)
}

/// 从根的真实索引引用判定节点角色，空叶块不能靠记录长度被误判成 dx 节点。
// ponytail: 极深目录每块重扫有限索引节点；实际吞吐受限时由目录游标保存遍历位置。
fn index_block(
    volume: &Ext4Volume,
    inode: &InodeMetadata,
    map_root: &[u8; 60],
    target: u64,
) -> Result<bool, FsError> {
    if inode.flags & INDEX == 0 || target == 0 {
        return Ok(false);
    }
    let g = volume.geometry();
    let root = directory_block(volume, inode, map_root, 0)?;
    let (levels, count) = root.with_read(|data| {
        codec::verify_directory(g, inode, 0, data, false)?;
        Ok::<_, FsError>((data[30], usize::from(codec::le16(data, 34)?)))
    })??;
    if levels == 0 {
        return Ok(false);
    }
    for index in 0..count {
        let child = root.with_read(|data| {
            codec::le32(data, 36 + index * 8).map(|value| u64::from(value & 0x0fff_ffff))
        })??;
        if child == target {
            return Ok(true);
        }
        if levels == 2 {
            let node = directory_block(volume, inode, map_root, child)?;
            let found = node.with_read(|data| {
                codec::verify_directory(g, inode, child, data, true)?;
                let count = usize::from(codec::le16(data, 10)?);
                for index in 0..count {
                    let grandchild = u64::from(codec::le32(data, 12 + index * 8)? & 0x0fff_ffff);
                    if grandchild == target {
                        return Ok(true);
                    }
                }
                Ok::<_, FsError>(false)
            })??;
            if found {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

/// 验证普通目录尾或 dx 根/节点及其正式校验范围，返回可枚举区域。
pub(crate) fn verify_directory(
    g: &Geometry,
    inode: &InodeMetadata,
    logical: u64,
    data: &[u8],
    index: bool,
) -> Result<codec::DirectoryLayout, FsError> {
    let data = codec::bytes(data, 0, g.block_size)?;
    let indexed = inode.flags & INDEX != 0;
    let root = indexed && logical == 0;
    let node = indexed && !root && index;
    if root || node {
        verify_dx(g, inode, logical, data, root)?;
        if root {
            let dot = record(g, data, 0, data.len())?;
            let parent = record(g, data, 12, data.len())?;
            if dot.number != inode.number
                || dot.next != 12
                || parent.next != data.len()
                || dot.name_len != 1
                || data[8] != b'.'
                || parent.name_len != 2
                || &data[20..22] != b".."
                || parent.number == 0
            {
                return Err(FsError::Corrupt);
            }
            return Ok(DirectoryLayout::Root);
        }
        return Ok(DirectoryLayout::Index);
    }
    let end = if g.features_ro_compat & METADATA_CSUM != 0 {
        let end = data.len() - 12;
        if codec::le32(data, end)? != 0
            || codec::le16(data, end + 4)? != 12
            || data[end + 6] != 0
            || data[end + 7] != 0xde
        {
            return Err(FsError::Corrupt);
        }
        if codec::le32(data, end + 8)? != codec::crc32c(inode.checksum_seed, &data[..end]) {
            return Err(FsError::Checksum);
        }
        end
    } else {
        data.len()
    };
    let mut offset = 0;
    while offset < end {
        offset = record(g, data, offset, end)?.next;
    }
    if offset != end {
        return Err(FsError::Corrupt);
    }
    Ok(DirectoryLayout::Leaf { end })
}

/// 验证 dx 布局、条目排序与逻辑块地址，CRC 不包含未使用索引槽。
fn verify_dx(
    g: &Geometry,
    inode: &InodeMetadata,
    logical: u64,
    data: &[u8],
    root: bool,
) -> Result<(), FsError> {
    let offset = if root {
        if codec::le16(data, 4)? != 12
            || codec::le16(data, 16)? as usize != data.len() - 12
            || codec::le32(data, 24)? != 0
            || data[29] != 8
            || data[31] != 0
        {
            return Err(FsError::Corrupt);
        }
        if 2 < data[28] {
            return Err(FsError::Unsupported);
        }
        let max_levels = if g.features_incompat & 0x4000 != 0 {
            2
        } else {
            1
        };
        if max_levels < data[30] {
            return Err(FsError::Corrupt);
        }
        32
    } else {
        if codec::le32(data, 0)? != 0
            || codec::le16(data, 6)? != 0
            || codec::le16(data, 4)? as usize != data.len()
        {
            return Err(FsError::Corrupt);
        }
        8
    };
    let limit = usize::from(codec::le16(data, offset)?);
    let count = usize::from(codec::le16(data, offset + 2)?);
    let tail_len = if g.features_ro_compat & METADATA_CSUM != 0 {
        8
    } else {
        0
    };
    if limit != (data.len() - offset - tail_len) / 8 || count == 0 || limit < count {
        return Err(FsError::Corrupt);
    }
    let blocks = inode.size / g.block_size as u64;
    let mut previous_hash = 0u32;
    for index in 0..count {
        let entry = offset + index * 8;
        if index != 0 {
            let hash = codec::le32(data, entry)?;
            if hash < previous_hash {
                return Err(FsError::Corrupt);
            }
            previous_hash = hash;
        }
        let block = u64::from(codec::le32(data, entry + 4)? & 0x0fff_ffff);
        if block == 0 || blocks <= block || block == logical {
            return Err(FsError::Corrupt);
        }
    }
    if tail_len != 0 {
        let tail = offset + limit * 8;
        if codec::le32(data, tail)? != 0 {
            return Err(FsError::Corrupt);
        }
        let crc = codec::crc32c(inode.checksum_seed, &data[..offset + count * 8]);
        let crc = codec::crc32c(codec::crc32c(crc, &data[tail..tail + 4]), &[0; 4]);
        if crc != codec::le32(data, tail + 4)? {
            return Err(FsError::Checksum);
        }
    }
    Ok(())
}

/// 解码一个真实记录并限制名称及下一记录边界，保留非 UTF-8 字节。
fn record(g: &Geometry, data: &[u8], offset: usize, end: usize) -> Result<Record, FsError> {
    let number = u64::from(codec::le32(data, offset)?);
    let len = usize::from(codec::le16(data, offset + 4)?);
    let filetype = g.features_incompat & 2 != 0;
    let header = codec::bytes(data, offset, 8)?;
    let name_len = if filetype {
        usize::from(header[6])
    } else {
        usize::from(codec::le16(data, offset + 6)?)
    };
    let next = offset.checked_add(len).ok_or(FsError::Corrupt)?;
    if len < 12
        || len & 3 != 0
        || 255 < name_len
        || len < ((8 + name_len + 3) & !3)
        || end < next
        || (next < end && end - next < 12)
        || g.inodes_count < number
    {
        return Err(FsError::Corrupt);
    }
    let name_offset = offset + 8;
    let name = codec::bytes(data, name_offset, name_len)?;
    if number != 0 && (name.is_empty() || name.contains(&0) || name.contains(&b'/')) {
        return Err(FsError::Corrupt);
    }
    let kind = if filetype {
        match header[7] {
            0 => None,
            1 => Some(InodeKind::Regular),
            2 => Some(InodeKind::Directory),
            3..=6 => Some(InodeKind::Other),
            7 => Some(InodeKind::Symlink),
            _ => return Err(FsError::Corrupt),
        }
    } else {
        None
    };
    Ok(Record {
        number,
        name_offset,
        name_len,
        kind,
        next,
    })
}

/// 取指定真实游标之后的首个活记录，索引伪项及尾记录不进入结果。
fn scan_next(
    g: &Geometry,
    data: &[u8],
    layout: DirectoryLayout,
    requested: usize,
) -> Result<Option<Record>, FsError> {
    if layout == DirectoryLayout::Index {
        return Ok(None);
    }
    let end = match layout {
        DirectoryLayout::Leaf { end } => end,
        _ => data.len(),
    };
    let mut offset = 0usize;
    while offset < end {
        let entry = record(g, data, offset, end)?;
        if offset < requested && requested < entry.next {
            return Err(FsError::InvalidOperation);
        }
        if requested <= offset && entry.number != 0 {
            return Ok(Some(entry));
        }
        offset = entry.next;
    }
    if end < requested && requested != data.len() {
        return Err(FsError::InvalidOperation);
    }
    Ok(None)
}

/// 在每个块的校验完成之后比较借用的名称，查找不创建目录项 Vec。
pub(crate) fn lookup(
    volume: &Ext4Volume,
    inode: &InodeMetadata,
    map_root: &[u8; 60],
    name: &[u8],
) -> Result<Option<u64>, FsError> {
    check_directory(volume, inode)?;
    if name.is_empty() || 255 < name.len() || name.contains(&0) || name.contains(&b'/') {
        return Err(FsError::InvalidName);
    }
    let g = volume.geometry();
    for logical in 0..inode.size / g.block_size as u64 {
        let buffer = directory_block(volume, inode, map_root, logical)?;
        let index = index_block(volume, inode, map_root, logical)?;
        let found = buffer.with_read(|data| {
            let layout = codec::verify_directory(g, inode, logical, data, index)?;
            if layout == DirectoryLayout::Index {
                return Ok(None);
            }
            let end = match layout {
                DirectoryLayout::Leaf { end } => end,
                _ => data.len(),
            };
            let mut offset = 0usize;
            while offset < end {
                let entry = record(g, data, offset, end)?;
                if entry.number != 0
                    && &data[entry.name_offset..entry.name_offset + entry.name_len] == name
                {
                    return Ok(Some(entry.number));
                }
                offset = entry.next;
            }
            Ok::<_, FsError>(None)
        })??;
        if found.is_some() {
            return Ok(found);
        }
    }
    Ok(None)
}

/// 枚举可拥有的下一记录，名称只进行一次 fallible 预留和一次复制。
pub(crate) fn next(
    volume: &Ext4Volume,
    inode: &InodeMetadata,
    map_root: &[u8; 60],
    mut offset: u64,
) -> Result<Option<DirectoryEntry>, FsError> {
    check_directory(volume, inode)?;
    let g = volume.geometry();
    while offset < inode.size {
        let logical = offset / g.block_size as u64;
        let base = logical * g.block_size as u64;
        let buffer = directory_block(volume, inode, map_root, logical)?;
        let index = index_block(volume, inode, map_root, logical)?;
        let entry = buffer.with_read(|data| {
            let layout = codec::verify_directory(g, inode, logical, data, index)?;
            scan_next(g, data, layout, (offset - base) as usize)
        })??;
        if let Some(entry) = entry {
            let mut name = Vec::new();
            name.try_reserve_exact(entry.name_len)
                .map_err(|_| FsError::NoMemory)?;
            buffer.with_read(|data| {
                name.extend_from_slice(&data[entry.name_offset..entry.name_offset + entry.name_len])
            })?;
            return Ok(Some(DirectoryEntry {
                number: entry.number,
                name,
                kind: entry.kind,
                next_offset: base + entry.next as u64,
            }));
        }
        offset = base + g.block_size as u64;
    }
    Ok(None)
}
