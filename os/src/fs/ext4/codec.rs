//! 磁盘结构仅从有界小端字节解码，校验和与布局共用同一实现。

pub(crate) use crate::fs::ext4::crc::{crc16, crc32c};
pub(crate) use crate::fs::ext4::directory::{verify_directory, DirectoryLayout};
use crate::fs::ext4::{Geometry, InodeData, InodeKind, InodeTime};
use crate::fs::FsError;

pub(crate) const EXTENTS: u32 = 0x0008_0000;
pub(crate) const INDEX: u32 = 0x1000;
pub(crate) const METADATA_CSUM: u32 = 0x400;

/// 借用明确的字节范围，拒绝短输入与加法溢出。
pub(crate) fn bytes(data: &[u8], offset: usize, len: usize) -> Result<&[u8], FsError> {
    data.get(offset..offset.checked_add(len).ok_or(FsError::Corrupt)?)
        .ok_or(FsError::Corrupt)
}

/// 解码有界的十六位小端值。
pub(crate) fn le16(data: &[u8], offset: usize) -> Result<u16, FsError> {
    let b = bytes(data, offset, 2)?;
    Ok(u16::from_le_bytes([b[0], b[1]]))
}

/// 解码有界的三十二位小端值。
pub(crate) fn le32(data: &[u8], offset: usize) -> Result<u32, FsError> {
    let b = bytes(data, offset, 4)?;
    Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

/// 向上取整块数，不让外部计数加法溢出。
pub(crate) fn ceil_div(value: u64, unit: u64) -> Result<u64, FsError> {
    if unit == 0 {
        return Err(FsError::Corrupt);
    }
    Ok(value / unit + u64::from(value % unit != 0))
}

/// 从当前盘面数据建立只读几何；日志状态不参与解码，也不执行日志恢复。
pub(crate) fn decode_super(data: &[u8], device_bytes: u64) -> Result<Geometry, FsError> {
    let data = bytes(data, 0, 1024)?;
    if le16(data, 56)? != 0xef53 {
        return Err(FsError::Corrupt);
    }
    let features_compat = le32(data, 92)?;
    let features_incompat = le32(data, 96)?;
    let features_ro_compat = le32(data, 100)?;
    if features_ro_compat & METADATA_CSUM != 0 {
        if data[373] != 1 {
            return Err(FsError::Unsupported);
        }
        if crc32c(!0, &data[..1020]) != le32(data, 1020)? {
            return Err(FsError::Checksum);
        }
    }
    // 只读兼容位可忽略未知值，已知扩展读取格式仍须明确拒绝。
    let supported_incompat = 0x2 | 0x4 | 0x40 | 0x80 | 0x200 | 0x400 | 0x2000 | 0x4000;
    if features_incompat & !supported_incompat != 0
        || features_ro_compat & (0x4 | 0x200 | 0x8000) != 0
    {
        return Err(FsError::Unsupported);
    }
    if features_incompat & 0x2000 != 0 && features_ro_compat & METADATA_CSUM == 0 {
        return Err(FsError::Corrupt);
    }
    if le32(data, 72)? != 0 || 1 < le32(data, 76)? {
        return Err(FsError::Unsupported);
    }
    let log = le32(data, 24)?;
    if 2 < log {
        return Err(FsError::Unsupported);
    }
    let block_size = 1024usize << log;
    if le32(data, 28)? != log || le32(data, 36)? != le32(data, 32)? {
        return Err(FsError::Corrupt);
    }
    let first_data_block = u64::from(le32(data, 20)?);
    if first_data_block != u64::from(block_size == 1024) {
        return Err(FsError::Corrupt);
    }
    let has64 = features_incompat & 0x80 != 0;
    let blocks_count = u64::from(le32(data, 4)?)
        | if has64 {
            u64::from(le32(data, 336)?) << 32
        } else {
            0
        };
    let inodes_count = u64::from(le32(data, 0)?);
    let blocks_per_group = u64::from(le32(data, 32)?);
    let inodes_per_group = u64::from(le32(data, 40)?);
    let inode_size = if le32(data, 76)? == 0 {
        128
    } else {
        usize::from(le16(data, 88)?)
    };
    if !matches!(inode_size, 128 | 256) {
        return Err(FsError::Unsupported);
    }
    let descriptor_size = if has64 {
        usize::from(le16(data, 254)?)
    } else {
        32
    };
    if has64 && descriptor_size != 64 {
        return Err(FsError::Unsupported);
    }
    if blocks_count <= first_data_block
        || inodes_count == 0
        || blocks_per_group == 0
        || inodes_per_group == 0
        || block_size as u64 * 8 < blocks_per_group
        || block_size as u64 * 8 < inodes_per_group
        || inodes_per_group % (block_size / inode_size) as u64 != 0
    {
        return Err(FsError::Corrupt);
    }
    if device_bytes
        < blocks_count
            .checked_mul(block_size as u64)
            .ok_or(FsError::Corrupt)?
    {
        return Err(FsError::Corrupt);
    }
    let groups_count = ceil_div(blocks_count - first_data_block, blocks_per_group)?;
    if u64::from(u32::MAX) < groups_count
        || groups_count != ceil_div(inodes_count, inodes_per_group)?
    {
        return Err(FsError::Corrupt);
    }
    let descriptor_blocks = ceil_div(
        groups_count
            .checked_mul(descriptor_size as u64)
            .ok_or(FsError::Corrupt)?,
        block_size as u64,
    )?;
    if blocks_count <= first_data_block + descriptor_blocks {
        return Err(FsError::Corrupt);
    }
    let mut uuid = [0u8; 16];
    uuid.copy_from_slice(bytes(data, 104, 16)?);
    let checksum_seed = if features_incompat & 0x2000 != 0 {
        le32(data, 624)?
    } else {
        crc32c(!0, &uuid)
    };
    let log_groups_per_flex = data[372];
    if 31 < log_groups_per_flex {
        return Err(FsError::Corrupt);
    }
    Ok(Geometry {
        block_size,
        inode_size,
        descriptor_size,
        blocks_count,
        inodes_count,
        features_compat,
        features_incompat,
        features_ro_compat,
        first_data_block,
        blocks_per_group,
        inodes_per_group,
        groups_count,
        descriptor_blocks,
        checksum_seed,
        uuid,
        reserved_gdt_blocks: u64::from(le16(data, 206)?),
        backup_groups: [le32(data, 588)?, le32(data, 592)?],
        log_groups_per_flex,
    })
}

/// 校验组描述符并提取完整 inode 表地址；flex 组允许表位于其他块组。
pub(crate) fn decode_group(g: &Geometry, group: u64, data: &[u8]) -> Result<u64, FsError> {
    let data = bytes(data, 0, g.descriptor_size)?;
    if g.groups_count <= group {
        return Err(FsError::Corrupt);
    }
    let number = (group as u32).to_le_bytes();
    let stored = le16(data, 30)?;
    if g.features_ro_compat & METADATA_CSUM != 0 {
        let crc = crc32c(g.checksum_seed, &number);
        let crc = crc32c(crc32c(crc32c(crc, &data[..30]), &[0, 0]), &data[32..]);
        if crc as u16 != stored {
            return Err(FsError::Checksum);
        }
    } else if g.features_ro_compat & 0x10 != 0 {
        let crc = crc16(crc16(crc16(!0, &g.uuid), &number), &data[..30]);
        let crc = if g.descriptor_size == 64 {
            crc16(crc, &data[32..])
        } else {
            crc
        };
        if crc != stored {
            return Err(FsError::Checksum);
        }
    }
    let first = if g.features_incompat & 0x200 != 0 {
        g.first_data_block
    } else {
        g.first_data_block + group * g.blocks_per_group
    };
    let end = if g.features_incompat & 0x200 != 0 {
        g.blocks_count
    } else {
        core::cmp::min(first + g.blocks_per_group, g.blocks_count)
    };
    let mut pointers = [0u64; 3];
    for (index, value) in pointers.iter_mut().enumerate() {
        *value = u64::from(le32(data, index * 4)?);
        if g.descriptor_size == 64 {
            *value |= u64::from(le32(data, 32 + index * 4)?) << 32;
        }
        g.validate_blocks(*value, 1)?;
        if *value < first || end <= *value || *value <= g.first_data_block + g.descriptor_blocks {
            return Err(FsError::Corrupt);
        }
    }
    let table_blocks = ceil_div(
        g.inodes_per_group
            .checked_mul(g.inode_size as u64)
            .ok_or(FsError::Corrupt)?,
        g.block_size as u64,
    )?;
    let table_end = pointers[2]
        .checked_add(table_blocks)
        .ok_or(FsError::Corrupt)?;
    if end < table_end
        || pointers[0] == pointers[1]
        || (pointers[2] <= pointers[0] && pointers[0] < table_end)
        || (pointers[2] <= pointers[1] && pointers[1] < table_end)
    {
        return Err(FsError::Corrupt);
    }
    Ok(pointers[2])
}

/// 解码并校验完整 inode，保留块计数的统一 512 字节单位。
pub(crate) fn decode_inode(g: &Geometry, number: u64, data: &[u8]) -> Result<InodeData, FsError> {
    let data = bytes(data, 0, g.inode_size)?;
    if number == 0 || g.inodes_count < number {
        return Err(FsError::Corrupt);
    }
    let extra_isize = if g.inode_size == 128 {
        0
    } else {
        usize::from(le16(data, 128)?)
    };
    if g.inode_size < 128 + extra_isize || extra_isize & 3 != 0 {
        return Err(FsError::Corrupt);
    }
    let generation = le32(data, 100)?;
    let checksum_seed = crc32c(
        crc32c(g.checksum_seed, &(number as u32).to_le_bytes()),
        &generation.to_le_bytes(),
    );
    if g.features_ro_compat & METADATA_CSUM != 0 {
        let mut crc = crc32c(
            crc32c(crc32c(checksum_seed, &data[..124]), &[0, 0]),
            &data[126..128],
        );
        let mut stored = u32::from(le16(data, 124)?);
        if g.inode_size != 128 {
            crc = crc32c(crc, &data[128..130]);
            if 4 <= extra_isize {
                crc = crc32c(crc32c(crc, &[0, 0]), &data[132..]);
                stored |= u32::from(le16(data, 130)?) << 16;
            } else {
                crc = crc32c(crc, &data[130..]);
            }
        }
        if g.inode_size == 128 || extra_isize < 4 {
            crc &= 0xffff;
        }
        if stored != crc {
            return Err(FsError::Checksum);
        }
    }
    let mode = le16(data, 0)?;
    let kind = match mode & 0xf000 {
        0x8000 => InodeKind::Regular,
        0x4000 => InodeKind::Directory,
        0xa000 => InodeKind::Symlink,
        0x1000 | 0x2000 | 0x6000 | 0xc000 => InodeKind::Other,
        _ => return Err(FsError::Corrupt),
    };
    let flags = le32(data, 32)?;
    if flags & (0x4 | 0x800 | 0x100000 | 0x10000000 | 0x40000000) != 0 {
        return Err(FsError::Unsupported);
    }
    if flags & EXTENTS != 0 && g.features_incompat & 0x40 == 0 {
        return Err(FsError::Corrupt);
    }
    if flags & 0x200000 != 0 && g.features_incompat & 0x400 == 0 {
        return Err(FsError::Corrupt);
    }
    if kind == InodeKind::Symlink && flags & (0x10 | 0x20) != 0 {
        return Err(FsError::Corrupt);
    }
    let size = u64::from(le32(data, 4)?)
        | if kind == InodeKind::Regular || g.features_incompat & 0x4000 != 0 {
            u64::from(le32(data, 108)?) << 32
        } else {
            0
        };
    if (i64::MAX as u64) < size {
        return Err(FsError::Corrupt);
    }
    let mut blocks = u64::from(le32(data, 28)?);
    if g.features_ro_compat & 8 != 0 {
        blocks |= u64::from(le16(data, 116)?) << 32;
        if flags & 0x40000 != 0 {
            blocks = blocks
                .checked_mul((g.block_size / 512) as u64)
                .ok_or(FsError::Corrupt)?;
        }
    } else if flags & 0x40000 != 0 {
        return Err(FsError::Corrupt);
    }
    let file_acl = u64::from(le32(data, 104)?)
        | if g.features_incompat & 0x80 != 0 {
            u64::from(le16(data, 118)?) << 32
        } else {
            0
        };
    if file_acl != 0 {
        g.validate_blocks(file_acl, 1)?;
    }
    let ea_blocks = if file_acl == 0 {
        0
    } else {
        (g.block_size / 512) as u64
    };
    let fast_symlink = kind == InodeKind::Symlink
        && if flags & 0x200000 == 0 {
            blocks.checked_sub(ea_blocks).ok_or(FsError::Corrupt)? == 0
        } else {
            0 < size && size < 60
        };
    if fast_symlink && 60 <= size {
        return Err(FsError::Corrupt);
    }
    let mut block = [0u8; 60];
    block.copy_from_slice(bytes(data, 40, 60)?);
    Ok(InodeData {
        number,
        kind,
        mode,
        size,
        uid: u32::from(le16(data, 2)?) | u32::from(le16(data, 120)?) << 16,
        gid: u32::from(le16(data, 24)?) | u32::from(le16(data, 122)?) << 16,
        links: u32::from(le16(data, 26)?),
        blocks,
        atime: decode_time(data, 8, 140, extra_isize)?,
        mtime: decode_time(data, 16, 136, extra_isize)?,
        ctime: decode_time(data, 12, 132, extra_isize)?,
        generation,
        flags,
        block,
        checksum_seed,
        fast_symlink,
        file_acl,
        extra_isize,
    })
}

/// 基础秒有符号扩展；只有 extra_isize 覆盖该字段时才消费 epoch 和纳秒。
fn decode_time(
    data: &[u8],
    base: usize,
    extra: usize,
    extra_isize: usize,
) -> Result<InodeTime, FsError> {
    let seconds = i64::from(le32(data, base)? as i32);
    let extension = if extra + 4 <= 128 + extra_isize {
        le32(data, extra)?
    } else {
        0
    };
    Ok(InodeTime {
        seconds: seconds + (i64::from(extension & 3) << 32),
        nanoseconds: extension >> 2,
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ExtentHeader {
    pub entries: u16,
    pub max: u16,
    pub depth: u16,
}

/// 解码 extent 头，并在下降时要求深度严格匹配。
pub(crate) fn extent_header(
    data: &[u8],
    expected_depth: Option<u16>,
) -> Result<ExtentHeader, FsError> {
    if le16(data, 0)? != 0xf30a {
        return Err(FsError::Corrupt);
    }
    let h = ExtentHeader {
        entries: le16(data, 2)?,
        max: le16(data, 4)?,
        depth: le16(data, 6)?,
    };
    if 32 < h.depth
        || h.max == 0
        || h.max < h.entries
        || data.len() < 12 + usize::from(h.max) * 12
        || (h.entries == 0 && h.depth != 0)
        || expected_depth.is_some_and(|depth| depth != h.depth)
    {
        return Err(FsError::Corrupt);
    }
    Ok(h)
}

/// 0x8000 仍是已初始化的 32768 块，只有更大的编码表示未写入。
pub(crate) fn extent_length(raw: u16) -> Result<(u64, bool), FsError> {
    if raw == 0 {
        return Err(FsError::Corrupt);
    }
    let unwritten = 32768 < raw;
    Ok((
        u64::from(if unwritten { raw - 32768 } else { raw }),
        unwritten,
    ))
}

/// 验证已解码外部 extent 节点的正式尾 CRC，根节点由 inode CRC 保护。
pub(crate) fn verify_extent_checksum(
    g: &Geometry,
    inode: &InodeData,
    data: &[u8],
    header: ExtentHeader,
) -> Result<(), FsError> {
    if g.features_ro_compat & METADATA_CSUM != 0 {
        let tail = 12 + usize::from(header.max) * 12;
        if le32(data, tail)? != crc32c(inode.checksum_seed, bytes(data, 0, tail)?) {
            return Err(FsError::Checksum);
        }
    }
    Ok(())
}
