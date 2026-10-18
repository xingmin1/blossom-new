//! IDENTIFY 的容量和能力与命令 FIS 编码，不包含请求或内存运行时。

use crate::block::{BlkOperation, BlockError, QueueLimits};

pub(super) const IDENTIFY: u8 = 0xec;
pub(super) const MAX_BYTES: usize = 64 * 1024;
pub(super) const MAX_PRDT: usize = 56;
pub(super) const PRDT_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone, Copy)]
pub(super) struct Identify {
    pub(super) logical_bytes: u32,
    pub(super) logical_sectors: u64,
    pub(super) lba48: bool,
    pub(super) flush: Option<u8>,
}

impl Identify {
    /// 只接受正常 ATA DMA/LBA 设备，扩展字段须带有效位；容量按设备 logical sector 保存。
    pub(super) fn parse(words: &[u16; 256]) -> Result<Self, BlockError> {
        if words[0] & 0x8000 != 0 || words[49] & 0x300 != 0x300 {
            return Err(BlockError::Unsupported);
        }
        let extended_valid = words[83] & 0xc000 == 0x4000;
        let lba48 = extended_valid && words[83] & (1 << 10) != 0;
        let logical_sectors = if lba48 {
            u64::from(words[100])
                | (u64::from(words[101]) << 16)
                | (u64::from(words[102]) << 32)
                | (u64::from(words[103]) << 48)
        } else {
            u64::from(words[60]) | (u64::from(words[61]) << 16)
        };
        let address_limit = if lba48 { 1_u64 << 48 } else { 1_u64 << 28 };
        if logical_sectors == 0 || address_limit < logical_sectors {
            return Err(BlockError::Corrupt);
        }
        let logical_bytes = if words[106] & 0xd000 == 0x5000 {
            let word_count = u32::from(words[117]) | (u32::from(words[118]) << 16);
            word_count.checked_mul(2).ok_or(BlockError::Unsupported)?
        } else {
            512
        };
        if logical_bytes < 512
            || MAX_BYTES < logical_bytes as usize
            || !logical_bytes.is_power_of_two()
        {
            return Err(BlockError::Unsupported);
        }
        logical_sectors
            .checked_mul(u64::from(logical_bytes / 512))
            .ok_or(BlockError::Corrupt)?;
        // FLUSH 和 LBA48 是独立能力，不能从寻址方式推定持久化命令可用。
        let flush = if extended_valid && words[83] & (1 << 13) != 0 {
            Some(0xea)
        } else if extended_valid && words[83] & (1 << 12) != 0 {
            Some(0xe7)
        } else {
            None
        };
        Ok(Self {
            logical_bytes,
            logical_sectors,
            lba48,
            flush,
        })
    }

    /// 暴露一个硬件命令额度和八个软件完成槽，避免可睡回调占住唯一硬件通道。
    pub(super) fn limits(self) -> QueueLimits {
        QueueLimits {
            capacity_sectors: self.logical_sectors * u64::from(self.logical_bytes / 512),
            max_bytes: MAX_BYTES,
            max_segments: MAX_PRDT,
            max_segment_bytes: PRDT_BYTES as u32,
            logical_block_size: self.logical_bytes,
            dma_boundary_mask: u64::MAX,
            readonly: false,
            flush: self.flush.is_some(),
            queue_depth: 1,
            tag_count: 8,
        }
    }

    /// 将公共 512B sector 换成设备 LBA，只生成非 NCQ DMA 与真实受支持的 FLUSH。
    pub(super) fn command(
        self,
        operation: BlkOperation,
        sector: u64,
        bytes: usize,
    ) -> Result<[u8; 20], BlockError> {
        if operation == BlkOperation::Flush {
            if sector != 0 || bytes != 0 {
                return Err(BlockError::InvalidRange);
            }
            return self.flush.map(fis).ok_or(BlockError::Unsupported);
        }
        let offset = sector.checked_mul(512).ok_or(BlockError::InvalidRange)?;
        let logical = u64::from(self.logical_bytes);
        if offset % logical != 0 || bytes as u64 % logical != 0 {
            return Err(BlockError::Misaligned);
        }
        let lba = offset / logical;
        let count = bytes as u64 / logical;
        let max_count = if self.lba48 { 65536 } else { 256 };
        if count == 0 || max_count < count || MAX_BYTES < bytes {
            return Err(BlockError::InvalidRange);
        }
        let end = lba.checked_add(count).ok_or(BlockError::InvalidRange)?;
        if self.logical_sectors < end {
            return Err(BlockError::InvalidRange);
        }
        let opcode = match (operation, self.lba48) {
            (BlkOperation::Read, true) => 0x25,
            (BlkOperation::Write, true) => 0x35,
            (BlkOperation::Read, false) => 0xc8,
            (BlkOperation::Write, false) => 0xca,
            (BlkOperation::Flush, _) => unreachable!(),
        };
        let mut command = fis(opcode);
        command[4] = lba as u8;
        command[5] = (lba >> 8) as u8;
        command[6] = (lba >> 16) as u8;
        command[7] = 1 << 6;
        if self.lba48 {
            command[8] = (lba >> 24) as u8;
            command[9] = (lba >> 32) as u8;
            command[10] = (lba >> 40) as u8;
            command[13] = (count >> 8) as u8;
        } else {
            command[7] |= ((lba >> 24) & 0xf) as u8;
        }
        command[12] = count as u8;
        Ok(command)
    }
}

/// Register H2D FIS 的 C 位表示 ATA 命令；其余字节由具体命令填充。
pub(super) fn fis(opcode: u8) -> [u8; 20] {
    let mut command = [0; 20];
    command[0] = 0x27;
    command[1] = 0x80;
    command[2] = opcode;
    command
}

/// 以真实协议函数检查 4Kn、LBA48 高位、独立 FLUSH 与不支持字段，供原内核测试调用。
#[cfg(feature = "ktest")]
pub(super) fn self_check() {
    let mut words = [0; 256];
    words[49] = 0x300;
    words[83] = 0x4000 | (1 << 10);
    words[102] = 1;
    words[106] = 0x5000;
    words[117] = 2048;
    let id = Identify::parse(&words).unwrap();
    assert_eq!(id.logical_bytes, 4096);
    assert_eq!(id.limits().capacity_sectors, (1_u64 << 32) * 8);
    assert!(!id.limits().flush);
    assert_eq!(
        id.command(BlkOperation::Flush, 0, 0),
        Err(BlockError::Unsupported)
    );
    let lba = 0x0102_0304_u64;
    let read = id.command(BlkOperation::Read, lba * 8, 8192).unwrap();
    assert_eq!(&read[4..11], &[4, 3, 2, 0x40, 1, 0, 0]);
    assert_eq!(&read[12..14], &[2, 0]);
    assert_eq!(
        id.command(BlkOperation::Read, 1, 4096),
        Err(BlockError::Misaligned)
    );
    words[83] |= 1 << 12;
    assert_eq!(
        Identify::parse(&words)
            .unwrap()
            .command(BlkOperation::Flush, 0, 0)
            .unwrap()[2],
        0xe7
    );
    words[83] |= 1 << 13;
    assert_eq!(
        Identify::parse(&words)
            .unwrap()
            .command(BlkOperation::Flush, 0, 0)
            .unwrap()[2],
        0xea
    );
    words[117] = 0;
    assert!(Identify::parse(&words).is_err());
}
