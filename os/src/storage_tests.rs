//! 在生产任务与原页分配器上检查块设备准备、真实发表和确认停止。

mod block_io;
mod finalizer;
mod prepare;

use alloc::vec::Vec;
use core::sync::atomic::Ordering;
use virtio_drivers::device::blk::PDU_STRIDE;
use virtio_drivers::queue::RingLayout;
use virtio_drivers::transport::DeviceStatus;

use crate::drivers::virtio::{VirtioDevice, VirtioInitError};
use crate::mm::dma::DmaError;
use crate::{cpu, drivers, mm, sched, time, timer};

/// 只协调现有服务的调度尾部；设备停止等待始终使用真实 timer 睡眠。
fn settled() -> mm::MemoryStats {
    let end = time::now()
        .checked_add(time::Duration::from_secs(5))
        .expect("storage resource deadline overflow");
    while !(0..cpu::count())
        .all(|id| id == cpu::current().id || cpu::get(id).idle.load(Ordering::Acquire))
    {
        assert!(time::now() < end, "storage service tail did not settle");
        sched::yield_now();
    }
    mm::shrink_all();
    mm::drain_all_pages();
    mm::stats()
}

/// 预先准备最大页凭证容量，随后只消费真实单页，不在内存耗尽时扩容。
fn pressure_storage() -> Vec<mm::OwnedPages> {
    let mut held = Vec::new();
    held.try_reserve_exact(mm::stats().present_pages)
        .expect("storage pressure owner preparation failed");
    held
}

/// 将全部真实可用页留在预备容器内，最后一次普通页申请必须真实失败。
fn exhaust_pages(held: &mut Vec<mm::OwnedPages>) {
    assert!(held.is_empty());
    mm::shrink_all();
    mm::drain_all_pages();
    while let Some(pages) = mm::alloc_pages(0, false) {
        assert!(held.len() < held.capacity(), "storage pressure capacity");
        held.push(pages);
    }
    mm::drain_all_pages();
    assert!(
        mm::alloc_pages(0, false).is_none(),
        "storage pressure was not real OOM"
    );
}

/// 从失败时仍 live 的 transport 原始状态证明真实 OOM 发生在 DRIVER_OK 发表前。
fn assert_unpublished_oom(error: VirtioInitError) {
    let status = match error {
        VirtioInitError::NoMemory { status }
        | VirtioInitError::Dma {
            error: DmaError::NoMemory,
            status,
        } => status,
        other => panic!("unexpected storage preparation error: {:?}", other),
    };
    assert!(
        !status.contains(DeviceStatus::DRIVER_OK),
        "failed storage preparation published DRIVER_OK"
    );
}

/// 持有原阶控制区的释放权，未发表的 ring/PDU 失败不能改变完整稳态页账。
fn preparation_oom(
    prepare: &mut impl FnMut() -> Result<VirtioDevice, VirtioInitError>,
    legacy: bool,
    depth: u16,
    index: usize,
) {
    let mut held = pressure_storage();
    let original = settled();
    exhaust_pages(&mut held);
    let ring_error = prepare()
        .err()
        .expect("ring preparation succeeded under real OOM");
    held.clear();
    assert_unpublished_oom(ring_error);
    assert_eq!(settled(), original, "failed ring preparation leaked memory");

    let layout = RingLayout::new(depth, legacy).expect("storage ring geometry");
    let order = layout
        .byte_len()
        .div_ceil(4096)
        .next_power_of_two()
        .trailing_zeros() as usize;
    let reserved_ring = mm::alloc_pages(order, false).expect("storage ring reservation failed");
    exhaust_pages(&mut held);
    drop(reserved_ring);
    mm::drain_all_pages();
    let pdu_error = prepare()
        .err()
        .expect("PDU preparation succeeded without remaining pages");
    held.clear();
    assert_unpublished_oom(pdu_error);
    assert_eq!(settled(), original, "failed PDU preparation leaked memory");
    drop(held);
    crate::early_println!(
        "PASS B03 backend index={} ring-oom=1 pdu-oom=1 unpublished=1 reclaimed=1",
        index
    );
}

/// 真实 status0 读回之前保住整个控制区，超时是失败而非允许归页。
fn stop(device: &mut VirtioDevice) {
    device.request_reset();
    let end = time::now()
        .checked_add(time::Duration::from_secs(2))
        .expect("storage reset deadline overflow");
    while !device.reset_complete() {
        assert!(time::now() < end, "storage reset did not complete");
        timer::sleep(time::Duration::from_millis(1));
    }
    assert!(
        device.driver().status().is_empty(),
        "storage reset status was not zero"
    );
}

/// 同一硬件每次只有一个控制区拥有者，全部预备资源稳定后才发表 queue 与 DRIVER_OK。
fn backend(
    mut prepare: impl FnMut() -> Result<VirtioDevice, VirtioInitError>,
    legacy: bool,
    index: usize,
) {
    let baseline = settled();
    let mut device = prepare().expect("storage backend preparation failed");
    let info = device.info();
    let (ring, pdu) = device.control_regions();
    let layout = RingLayout::new(info.queue_depth, legacy).expect("storage ring layout");
    assert!(layout.byte_len() <= ring.len && info.tag_count as usize * PDU_STRIDE <= pdu.len);
    assert_eq!(ring.device.value() & 4095, 0);
    assert_eq!(pdu.device.value() & 4095, 0);
    assert!(
        ring.device.value() + ring.len as u64 <= pdu.device.value()
            || pdu.device.value() + pdu.len as u64 <= ring.device.value()
    );
    assert!(!device.driver().status().contains(DeviceStatus::DRIVER_OK));
    device
        .publish()
        .expect("storage backend publication failed");
    let status = device.driver().status();
    assert!(status.contains(DeviceStatus::DRIVER_OK));
    assert_eq!(status.contains(DeviceStatus::FEATURES_OK), !legacy);
    assert_eq!(
        device.driver().available_descriptors(),
        info.queue_depth as usize
    );
    // SAFETY: 本 Task 是唯一设备 owner；这里只消费空队列，不移出或替换后端，
    // ring/PDU 始终保活，reset 尚未开始且没有其他队列访问者。
    let driver = unsafe { device.driver_mut() };
    assert!(driver
        .pop_used()
        .expect("empty used ring invalid")
        .is_none());
    crate::early_println!(
        "STORAGE BACKEND index={} legacy={} capacity={} depth={} tags={} max-segments={} max-segment-bytes={} logical-block={} readonly={} flush={} ring={:#x}/{} pdu={:#x}/{} status={:#x}",
        index, legacy as usize, info.capacity_sectors, info.queue_depth, info.tag_count,
        info.max_segments, info.max_segment_bytes, info.logical_block_size,
        info.readonly as usize, info.flush as usize, ring.device.value(), ring.len,
        pdu.device.value(), pdu.len, status.bits()
    );
    stop(&mut device);
    drop(device);
    assert_eq!(
        settled(),
        baseline,
        "published backend controls did not return"
    );
    preparation_oom(&mut prepare, legacy, info.queue_depth, index);
    prepare::run(&mut prepare, index);
    block_io::run(&mut prepare, legacy, index);
    let mut device = prepare().expect("backend did not recover after preparation OOM");
    device
        .publish()
        .expect("backend republish after OOM failed");
    stop(&mut device);
    drop(device);
    assert_eq!(
        settled(),
        baseline,
        "backend OOM recovery did not reclaim memory"
    );
    crate::early_println!(
        "PASS storage-backend index={} stable=1 reset=1 recovered=1 reclaimed=1",
        index
    );
}

/// 从一次真实库存检查两种后端及实际块数据路径，文件通路仍不属于本批。
pub(crate) fn run() -> i32 {
    crate::early_println!(
        "STORAGE READY arch={} cpus={} suite=block mode={}",
        crate::arch::NAME,
        cpu::count(),
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        }
    );
    let inventory = drivers::discover().expect("storage resource discovery failed");
    let mut count = 0;
    for candidate in &inventory.mmio_blocks {
        // SAFETY: 一次库存来自真实 BootInfo 永久映射；本测试串行拥有此槽，
        // 每次重新准备前上一拥有者已经真实 reset 完成并销毁，没有其他硬件拥有者。
        backend(
            || unsafe { VirtioDevice::prepare_mmio(candidate) },
            candidate.version == 1,
            count,
        );
        count += 1;
    }
    for device in
        inventory.pci.devices().iter().filter(|device| {
            device.vendor_id == 0x1af4 && matches!(device.device_id, 0x1001 | 0x1042)
        })
    {
        // SAFETY: 此对象属于本库存的已配置真实功能，永久强序映射已在 AP 前建立；
        // 串行测试期间无第二 owner，各次复构前已确认上一控制区停止并释放。
        backend(
            || unsafe { VirtioDevice::prepare_pci(&inventory.pci, device) },
            false,
            count,
        );
        count += 1;
    }
    assert!(0 < count, "storage backend suite requires an attached disk");
    drop(inventory);
    crate::early_println!(
        "STORAGE BLOCK PASS devices={} scope=real-io-with-controlled-ledger",
        count
    );
    crate::arch::shutdown()
}
