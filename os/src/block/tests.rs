//! 只验证父完成账本的受控顺序，不冒充硬件反序 used 或磁盘 I/O。

use alloc::vec::Vec;
use virtio_drivers::BufferDirection;

use super::{Bio, BlkOperation, BlockError, Disk, IoResult};
use crate::mm;
use crate::mm::dma::MappedPages;

/// 用真实原零页预备受控 Read，地址始终不发表给设备。
fn read_bio(disk: &Disk, bytes: usize) -> Bio {
    let mut memory = Vec::new();
    memory
        .try_reserve_exact(bytes.div_ceil(mm::PAGE_SIZE))
        .expect("controlled bio owner vector");
    let mut remaining = bytes;
    while remaining != 0 {
        let length = remaining.min(mm::PAGE_SIZE);
        let pages = mm::alloc_pages(0, true).expect("controlled bio original page");
        memory.push(
            MappedPages::new(pages, 0, length, BufferDirection::DeviceToDriver)
                .expect("controlled bio mapping"),
        );
        remaining -= length;
    }
    // SAFETY: 原页只由此局部拥有，CPU 无裸别名，没有回调；本受控账本不发表任何
    // descriptor，因此全部模拟 child 结束后设备并无引用，最后收权不依赖伪造硬件证据。
    unsafe { Bio::try_new(disk, BlkOperation::Read, 32, memory, 0, None) }
        .expect("controlled bio construction")
}

/// 覆盖生成者保活、反序短前缀和生成失败仍等待旧 child 三个消费边界。
pub(crate) fn aggregation(disk: &Disk) {
    let unit = disk.limits().logical_block_size as usize;
    let short = unit / 4;
    let bio = read_bio(disk, 3 * unit);
    let completion = bio.completion();
    bio.inner.begin();
    bio.inner.register_child(0, unit);
    bio.inner.register_child(unit, unit);
    bio.inner.register_child(2 * unit, unit);
    bio.inner.finish_child(2 * unit, unit, unit, None);
    bio.inner.finish_child(0, unit, unit, None);
    bio.inner.finish_generation(None, None);
    assert!(!completion.is_complete());
    assert_eq!(completion.take_memory().err(), Some(BlockError::Busy));
    bio.inner
        .finish_child(unit, unit, short, Some(BlockError::Io));
    assert_eq!(
        completion.wait(),
        IoResult {
            completed: unit + short,
            error: Some(BlockError::Io)
        }
    );
    let memory = completion
        .take_memory()
        .expect("controlled short bio memory");
    assert!(memory.iter().all(|mapped| !mapped.is_device_owned()));
    drop(memory);
    assert_eq!(completion.take_memory().err(), Some(BlockError::Busy));

    let bio = read_bio(disk, 2 * unit);
    let completion = bio.completion();
    bio.inner.begin();
    bio.inner.register_child(0, unit);
    bio.inner
        .finish_generation(Some((unit, BlockError::NoMemory)), None);
    assert!(!completion.is_complete());
    bio.inner.finish_child(0, unit, unit, None);
    assert_eq!(
        completion.wait(),
        IoResult {
            completed: unit,
            error: Some(BlockError::NoMemory)
        }
    );
    drop(
        completion
            .take_memory()
            .expect("controlled generation failure memory"),
    );

    let bio = read_bio(disk, unit);
    let completion = bio.completion();
    bio.inner.begin();
    bio.inner.register_child(0, unit);
    bio.inner.finish_child(0, unit, unit, None);
    assert!(!completion.is_complete());
    bio.inner.finish_generation(None, None);
    assert_eq!(
        completion.wait(),
        IoResult {
            completed: unit,
            error: None
        }
    );
    drop(
        completion
            .take_memory()
            .expect("controlled producer obligation memory"),
    );
    crate::early_println!("PASS block-aggregation scope=controlled-ledger prefix={} generation-prefix={} logical={} producer-hold=1", unit + short, unit, unit);
}

/// 只触发生产故障退休，实际状态零仍由真实设备读回确认。
pub(crate) fn retire(disk: &Disk) {
    disk.request_retire(BlockError::DeviceFault);
}

/// 观察生产 reset 认领者已确认零且执行 DMA 屏障，不伪造控制器状态。
pub(crate) fn dma_stopped(disk: &Disk) -> bool {
    let queue = disk.queue.lock();
    queue.phase == super::disk::QueuePhase::DmaStopped
        && queue
            .device
            .as_ref()
            .is_some_and(|device| !device.requires_reset() && device.status().is_empty())
}

/// 只读真实设备 used.idx，固定布局与同一控制区 owner 保活，不消费或合成元素。
pub(crate) fn used_index(disk: &Disk, legacy: bool) -> u16 {
    let queue = disk.queue.lock();
    let device = queue.device.as_ref().expect("live controlled disk");
    let layout = virtio_drivers::queue::RingLayout::new(disk.limits.queue_depth, legacy)
        .expect("controlled real ring layout");
    let ring = device.control_regions().0;
    crate::arch::dma_read_barrier();
    // SAFETY: 同一 queue 锁防止停止与释放；used.idx 是真实布局的对齐 u16，
    // coherent 设备拥有该字节，此处仅 volatile 读取，不创建 DMA 区的 Rust 引用。
    u16::from_le(unsafe {
        core::ptr::read_volatile(
            ring.cpu
                .as_ptr()
                .add(layout.used_offset() + 2)
                .cast::<u16>(),
        )
    })
}

/// 准入与生成尾部使用生产 API；原零页不发表给设备，只控制两个 child 的完成顺序。
pub(crate) fn generation_pair(disk: &Disk, first: Bio, second: Bio) {
    let first = disk
        .admit(first)
        .unwrap_or_else(|(_, error)| panic!("controlled first admission: {:?}", error));
    let second = disk
        .admit(second)
        .unwrap_or_else(|(_, error)| panic!("controlled second admission: {:?}", error));
    for active in [&first, &second] {
        let length = active.bio.len();
        active.bio.register_child(0, length);
        active.bio.finish_child(0, length, length, None);
    }
    first.finish(None);
    second.finish(None);
}

/// 仅控制无设备引用的 child 先结束；生产准入与生成者最后完成仍使用真实实现。
pub(crate) fn generation_last(disk: &Disk, bio: Bio) {
    let active = disk
        .admit(bio)
        .unwrap_or_else(|(_, error)| panic!("controlled generation admission: {:?}", error));
    let length = active.bio.len();
    active.bio.register_child(0, length);
    active.bio.finish_child(0, length, length, None);
    active.finish(None);
}
