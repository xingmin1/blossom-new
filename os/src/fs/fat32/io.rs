//! 将 FAT 字节流转换为现有 `FsClaim`、`Bio` 与 `MappedPages` 上的同步扇区 I/O。
//!
//! 每次读写最多处理一个 512 字节扇区，短传输由 fatfs 的 `read_exact`、`write_all` 继续推进，
//! 不建立额外缓存或请求队列。所有实际 I/O 都可能睡眠，调用方不得持有自旋锁或关闭抢占。
//! 同一卷的访问由卷级睡眠互斥锁串行化；非整扇区写入的读改写期间，不得有其他路径修改该扇区。

use alloc::vec::Vec;
use core::convert::TryInto;

use crate::block::{
    AccessMode, Bio, BioAllocator, BlkOperation, BlockError, FsClaim, SECTOR_BYTES,
};
use crate::mm::{alloc_pages, dma::MappedPages};
use fatfs::{IoBase, Read, Seek, SeekFrom, Write};
use virtio_drivers::BufferDirection;

/// 保留底层块错误，并表达 fatfs 精确传输与有界定位所需的字节流错误。
#[derive(Clone, Copy, Debug)]
pub(crate) enum IoError {
    /// 底层设备、内存分配或块请求返回的原始错误。
    Block(BlockError),
    /// 精确读取尚未填满目标缓冲区就到达卷末尾。
    UnexpectedEof,
    /// 完整写入尚有剩余数据，但卷末尾已无法继续写入。
    WriteZero,
    /// 定位目标小于零或超过卷的字节长度。
    InvalidSeek,
}

impl From<BlockError> for IoError {
    /// 包装原始块错误，不改变错误分类。
    fn from(error: BlockError) -> Self {
        Self::Block(error)
    }
}

impl fatfs::IoError for IoError {
    /// 块层同步操作不返回可由 fatfs 自动重试的中断错误。
    fn is_interrupted(&self) -> bool {
        false
    }

    /// 为 fatfs 的精确读取保留提前到达卷末尾的错误。
    fn new_unexpected_eof_error() -> Self {
        Self::UnexpectedEof
    }

    /// 为 fatfs 的完整写入保留无法继续推进的错误。
    fn new_write_zero_error() -> Self {
        Self::WriteZero
    }
}

/// 持有文件系统占用凭证的有界 FAT 字节流，不共享或隐式复制游标。
pub(crate) struct FatIo {
    /// 保活底层磁盘、卷扇区范围和挂载访问模式的占用凭证。
    claim: FsClaim,
    /// 下一次读写相对于卷起点的字节偏移，始终位于 `0..=length`。
    position: u64,
    /// 卷的固定字节长度，等于占用视图的扇区数乘以 512。
    length: u64,
}

impl FatIo {
    /// 消费已有占用凭证，以卷起点初始化游标；仅接受逻辑块大小为 512 字节的设备。
    pub(crate) fn new(claim: FsClaim) -> Result<Self, BlockError> {
        if u64::from(claim.view().limits().logical_block_size) != SECTOR_BYTES {
            return Err(BlockError::Unsupported);
        }
        let length = claim
            .view()
            .sectors()
            .checked_mul(SECTOR_BYTES)
            .ok_or(BlockError::InvalidRange)?;
        Ok(Self {
            claim,
            position: 0,
            length,
        })
    }
}

impl IoBase for FatIo {
    type Error = IoError;
}

impl Read for FatIo {
    /// 读取当前偏移所在的完整扇区，仅交付目标缓冲区能容纳的扇区内片段；失败时不推进游标。
    fn read(&mut self, dst: &mut [u8]) -> Result<usize, IoError> {
        if dst.is_empty() || self.position == self.length {
            return Ok(0);
        }
        let sector = self.position / SECTOR_BYTES * SECTOR_BYTES;
        let within = (self.position - sector) as usize;
        let mut bytes = [0_u8; SECTOR_BYTES as usize];
        let count = dst.len().min(bytes.len() - within);
        self.claim.read_exact_at(sector, &mut bytes)?;
        dst[..count].copy_from_slice(&bytes[within..within + count]);
        self.position += count as u64;
        Ok(count)
    }
}

impl Write for FatIo {
    /// 同步写入至多一个扇区内的片段；非整扇区先读取旧内容，成功完成后才推进游标。
    fn write(&mut self, src: &[u8]) -> Result<usize, IoError> {
        if src.is_empty() {
            return Ok(0);
        }
        if self.claim.mode() == AccessMode::ReadOnly {
            return Err(BlockError::ReadOnly.into());
        }
        if self.position == self.length {
            return Ok(0);
        }
        let sector = self.position / SECTOR_BYTES * SECTOR_BYTES;
        let within = (self.position - sector) as usize;
        let count = src.len().min(SECTOR_BYTES as usize - within);
        if within == 0 && count == SECTOR_BYTES as usize {
            let bytes = src[..count]
                .try_into()
                .expect("full sector write has an invalid length");
            write_sector(&self.claim, sector, bytes)?;
        } else {
            let mut bytes = [0_u8; SECTOR_BYTES as usize];
            self.claim.read_exact_at(sector, &mut bytes)?;
            bytes[within..within + count].copy_from_slice(&src[..count]);
            write_sector(&self.claim, sector, &bytes)?;
        }
        self.position += count as u64;
        Ok(count)
    }

    /// 通过占用凭证提交并等待真实 `Flush`；设备不支持刷盘或刷盘失败时保留底层错误。
    fn flush(&mut self) -> Result<(), IoError> {
        self.claim
            .flush(Vec::new_in(BioAllocator::Heap))
            .map_err(Into::into)
    }
}

impl Seek for FatIo {
    /// 在卷范围内定位，允许恰好停在卷末尾；越界或负偏移不会改变原游标。
    fn seek(&mut self, from: SeekFrom) -> Result<u64, IoError> {
        let next = seek_position(self.position, self.length, from)?;
        self.position = next;
        Ok(next)
    }
}

/// 使用足够宽的有符号运算计算定位结果，避免负偏移或卷长度超过 `i64::MAX` 时溢出。
fn seek_position(position: u64, length: u64, from: SeekFrom) -> Result<u64, IoError> {
    let next = match from {
        SeekFrom::Start(offset) => i128::from(offset),
        SeekFrom::Current(offset) => i128::from(position) + i128::from(offset),
        SeekFrom::End(offset) => i128::from(length) + i128::from(offset),
    };
    if next < 0 || i128::from(length) < next {
        return Err(IoError::InvalidSeek);
    }
    Ok(next as u64)
}

/// 在普通可睡眠任务中同步写入卷内一个完整扇区，等待设备完成并取回物理页后才返回。
fn write_sector(
    claim: &FsClaim,
    offset: u64,
    data: &[u8; SECTOR_BYTES as usize],
) -> Result<(), BlockError> {
    // Step 1: 校验卷范围，为单个扇区准备固定容量的物理页容器，并逐字节初始化映射负载。
    let absolute = claim.view().absolute_byte(offset, data.len())?;
    let mut memory = Vec::new_in(BioAllocator::Heap);
    memory
        .try_reserve_exact(1)
        .map_err(|_| BlockError::NoMemory)?;
    let pages = alloc_pages(0, false).ok_or(BlockError::NoMemory)?;
    let mut mapped = MappedPages::new(pages, 0, data.len(), BufferDirection::DriverToDevice)
        .map_err(BlockError::from)?;
    for (target, value) in mapped.bytes_mut().iter_mut().zip(data) {
        target.write(*value);
    }
    memory.push(mapped);

    // Step 2: 将独占页面移交给已有 Bio，提交后等待设备释放访问权，再回收缓冲区。
    // SAFETY:
    // 1. `mapped` 独占刚分配的物理页，映射长度严格为一个扇区，全部 512 字节已通过 `MaybeUninit::write` 初始化；未映射的页内字节不参与传输。
    // 2. 初始化借用已结束，`memory` 按值移交唯一 `Bio`，没有外部切片、裸指针或完成回调访问该物理页。
    // 3. 下方先等待所有子请求完成并归还设备访问权，再通过 `take_memory()` 取回并释放页面；提交期间 CPU 不访问负载。
    let bio = unsafe {
        Bio::try_new(
            claim.disk(),
            BlkOperation::Write,
            absolute / SECTOR_BYTES,
            memory,
            0,
            None,
        )
    }?;
    let completion = bio.completion();
    claim.submit_bio(bio);
    let result = completion.wait();
    drop(completion.take_memory()?);
    if let Some(error) = result.error {
        return Err(error);
    }
    if result.completed != data.len() {
        return Err(BlockError::ShortIo);
    }
    Ok(())
}

#[cfg(feature = "ktest")]
#[::ktest::suite(context = "Boot", tags("fat32", "fat32-io"), timeout_ms = 1000)]
mod checks {
    use super::{seek_position, IoError, SeekFrom};

    /// 测试目标：验证卷内定位的首尾边界和宽整数运算。
    /// 场景步骤：分别按卷起点、当前偏移和卷末尾定位，并传入负结果、越界及最大整数偏移。
    /// 预期结果：合法位置精确返回，越界位置返回 `InvalidSeek`，加减运算不溢出。
    #[test]
    fn seek_bounds_and_large_offsets() {
        assert_eq!(seek_position(0, 1024, SeekFrom::Start(0)).unwrap(), 0);
        assert_eq!(seek_position(0, 1024, SeekFrom::Start(1024)).unwrap(), 1024);
        assert_eq!(seek_position(0, 1024, SeekFrom::End(0)).unwrap(), 1024);
        assert_eq!(
            seek_position(512, 1024, SeekFrom::Current(-512)).unwrap(),
            0
        );
        for from in [
            SeekFrom::Start(1025),
            SeekFrom::Current(-1),
            SeekFrom::End(1),
            SeekFrom::End(-1025),
        ] {
            assert!(matches!(
                seek_position(0, 1024, from),
                Err(IoError::InvalidSeek)
            ));
        }
        assert_eq!(
            seek_position(u64::MAX, u64::MAX, SeekFrom::Current(i64::MIN)).unwrap(),
            i64::MAX as u64
        );
        assert!(matches!(
            seek_position(u64::MAX, u64::MAX, SeekFrom::Current(i64::MAX)),
            Err(IoError::InvalidSeek)
        ));
    }
}
