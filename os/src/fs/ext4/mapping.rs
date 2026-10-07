//! extent 与传统间接树只在内容借用中解码，树下降和真实 I/O 在借用之外。

use crate::fs::ext4::codec::{self, EXTENTS};
use crate::fs::ext4::{BlockMapping, Ext4Volume, Geometry, InodeData};
use crate::fs::FsError;

#[derive(Clone, Copy)]
enum Step {
    Mapping(BlockMapping),
    Child {
        physical: u64,
        depth: u16,
        low: u64,
        high: u64,
    },
}

/// 校验 inode 内根节点或全部传统块指针，不读取额外节点。
pub(crate) fn validate_root(g: &Geometry, inode: &InodeData) -> Result<(), FsError> {
    if inode.flags & EXTENTS != 0 {
        extent_step(g, inode, &inode.block, 0, None, None, 1u64 << 32)?;
    } else {
        validate_pointers(g, &inode.block)?;
    }
    Ok(())
}

/// 依据文件的实际映射格式选择有限树算法。
pub(crate) fn map(
    volume: &Ext4Volume,
    inode: &InodeData,
    logical: u64,
    end: u64,
) -> Result<BlockMapping, FsError> {
    let mapping = if inode.flags & EXTENTS != 0 {
        extent_map(volume, inode, logical)?
    } else {
        indirect_map(volume, inode, logical)?
    };
    Ok(match mapping {
        BlockMapping::Mapped { physical, len } => BlockMapping::Mapped {
            physical,
            len: len.min(end - logical),
        },
        BlockMapping::Unwritten { physical, len } => BlockMapping::Unwritten {
            physical,
            len: len.min(end - logical),
        },
        BlockMapping::Hole { len } => BlockMapping::Hole {
            len: len.min(end - logical),
        },
        BlockMapping::Eof => return Err(FsError::Corrupt),
    })
}

/// 逐层下降 extent 树，每个已读取的外部节点先校验自身 CRC 和全部条目。
fn extent_map(
    volume: &Ext4Volume,
    inode: &InodeData,
    logical: u64,
) -> Result<BlockMapping, FsError> {
    if (1u64 << 32) <= logical {
        return Err(FsError::Corrupt);
    }
    let g = volume.geometry();
    let mut step = extent_step(g, inode, &inode.block, logical, None, None, 1u64 << 32)?;
    let mut visited = [0u64; 32];
    let mut count = 0usize;
    loop {
        match step {
            Step::Mapping(mapping) => return Ok(mapping),
            Step::Child {
                physical,
                depth,
                low,
                high,
            } => {
                if visited.len() <= count || visited[..count].contains(&physical) {
                    return Err(FsError::Corrupt);
                }
                visited[count] = physical;
                count += 1;
                let buffer = volume.metadata_block(physical)?;
                step = buffer.with_read(|data| {
                    extent_step(g, inode, data, logical, Some(depth), Some(low), high)
                })??;
            }
        }
    }
}

/// 解码一个节点并选取下一层，完整验证条目排序、父边界及物理范围。
fn extent_step(
    g: &Geometry,
    inode: &InodeData,
    data: &[u8],
    logical: u64,
    depth: Option<u16>,
    low: Option<u64>,
    high: u64,
) -> Result<Step, FsError> {
    let header = codec::extent_header(data, depth)?;
    if depth.is_some() {
        codec::verify_extent_checksum(g, inode, data, header)?;
    }
    let mut previous_end = low.unwrap_or(0);
    let mut selected = None;
    let mut next_start = high;
    for index in 0..usize::from(header.entries) {
        let offset = 12 + index * 12;
        let start = u64::from(codec::le32(data, offset)?);
        if start < previous_end
            || high <= start
            || (index == 0 && low.is_some_and(|low| low != start))
        {
            return Err(FsError::Corrupt);
        }
        if header.depth == 0 {
            let (len, unwritten) = codec::extent_length(codec::le16(data, offset + 4)?)?;
            let end = start.checked_add(len).ok_or(FsError::Corrupt)?;
            if high < end {
                return Err(FsError::Corrupt);
            }
            let physical = u64::from(codec::le32(data, offset + 8)?)
                | u64::from(codec::le16(data, offset + 6)?) << 32;
            g.validate_blocks(physical, len)?;
            if start <= logical && logical < end {
                let physical = physical + logical - start;
                selected = Some(Step::Mapping(if unwritten {
                    BlockMapping::Unwritten {
                        physical,
                        len: end - logical,
                    }
                } else {
                    BlockMapping::Mapped {
                        physical,
                        len: end - logical,
                    }
                }));
            }
            previous_end = end;
        } else {
            let physical = u64::from(codec::le32(data, offset + 4)?)
                | u64::from(codec::le16(data, offset + 8)?) << 32;
            g.validate_blocks(physical, 1)?;
            if start <= logical {
                selected = Some(Step::Child {
                    physical,
                    depth: header.depth - 1,
                    low: start,
                    high,
                });
            } else if let Some(Step::Child {
                high: selected_high,
                ..
            }) = &mut selected
            {
                *selected_high = (*selected_high).min(start);
            }
            previous_end = start + 1;
        }
        if logical < start {
            next_start = next_start.min(start);
        }
    }
    Ok(selected.unwrap_or(Step::Mapping(BlockMapping::Hole {
        len: next_start - logical,
    })))
}

/// 校验传统指针数组中每个非零地址，零指针保持合法空洞含义。
fn validate_pointers(g: &Geometry, data: &[u8]) -> Result<(), FsError> {
    if data.len() % 4 != 0 {
        return Err(FsError::Corrupt);
    }
    for offset in (0..data.len()).step_by(4) {
        let physical = u64::from(codec::le32(data, offset)?);
        if physical != 0 {
            g.validate_blocks(physical, 1)?;
        }
    }
    Ok(())
}

/// 传统 direct、single、double、triple 树只下降最多三次，并拒绝祖先自引用。
fn indirect_map(
    volume: &Ext4Volume,
    inode: &InodeData,
    logical: u64,
) -> Result<BlockMapping, FsError> {
    let g = volume.geometry();
    validate_pointers(g, &inode.block)?;
    if logical < 12 {
        let physical = u64::from(codec::le32(&inode.block, logical as usize * 4)?);
        return Ok(if physical == 0 {
            BlockMapping::Hole { len: 1 }
        } else {
            BlockMapping::Mapped { physical, len: 1 }
        });
    }
    let pointers = (g.block_size / 4) as u64;
    let mut remaining = logical - 12;
    let mut span = pointers;
    let mut depth = 1usize;
    while span <= remaining && depth < 3 {
        remaining -= span;
        span = span.checked_mul(pointers).ok_or(FsError::Corrupt)?;
        depth += 1;
    }
    if span <= remaining {
        return Err(FsError::Corrupt);
    }
    let mut physical = u64::from(codec::le32(&inode.block, (11 + depth) * 4)?);
    let mut visited = [0u64; 3];
    for level in 0..depth {
        if physical == 0 {
            return Ok(BlockMapping::Hole {
                len: span - remaining,
            });
        }
        if visited[..level].contains(&physical) {
            return Err(FsError::Corrupt);
        }
        visited[level] = physical;
        let buffer = volume.metadata_block(physical)?;
        span /= pointers;
        let index = remaining / span;
        remaining %= span;
        physical = buffer.with_read(|data| {
            validate_pointers(g, data)?;
            codec::le32(data, index as usize * 4).map(u64::from)
        })??;
    }
    if physical == 0 {
        Ok(BlockMapping::Hole { len: 1 })
    } else if visited[..depth].contains(&physical) {
        Err(FsError::Corrupt)
    } else {
        Ok(BlockMapping::Mapped { physical, len: 1 })
    }
}
