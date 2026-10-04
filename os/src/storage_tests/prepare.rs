//! 固定请求表已准备后，用有界真实原页预算触发 SG reserve 失败并检查原 live 状态。

use virtio_drivers::transport::DeviceStatus;

use crate::block::{BlockError, Disk};
use crate::drivers::virtio::{VirtioDevice, VirtioInitError};
use crate::workqueue::{WorkQueue, WorkQueueConfig};
use crate::{irq, mm};

// 每轮只释放一个原阶整块，最多尝试六个预算；不摘除已有 slab 对象或注入分配失败。
const MAX_BUDGET_ORDER: usize = 5;

/// 每次耗尽前完整预备容器、完成域、预算原块与唯一设备，放压后再诊断和清理。
/// 调用者串行独占 prepare 所指硬件，此过程中不得另行 start 或登记该设备的 IRQ。
pub(super) fn run(
    prepare: &mut impl FnMut() -> Result<VirtioDevice, VirtioInitError>,
    index: usize,
) {
    let original = super::settled();
    let mut held = super::pressure_storage();
    let initial = prepare().expect("SG pressure backend preparation failed");
    let identity = initial.identity();
    let interrupt = initial.interrupt();
    let info = initial.info();
    let queue = WorkQueue::try_new(WorkQueueConfig {
        max_active: usize::from(info.tag_count) + 1,
        ordered: false,
        reclaim: true,
    })
    .expect("SG pressure work domain preparation failed");
    drop(initial);
    let resident = super::settled();
    let actions = irq::test_action_count(interrupt);
    let mut failure = None;

    for order in 0..=MAX_BUDGET_ORDER {
        let reserved = mm::alloc_pages(order, false).expect("SG pressure original budget block");
        let device = prepare().expect("SG pressure unique backend preparation failed");
        assert_eq!(device.identity(), identity);
        assert_eq!(device.info().tag_count, info.tag_count);
        assert_eq!(device.info().queue_depth, info.queue_depth);
        let mut observation = None;
        super::exhaust_pages(&mut held);
        drop(reserved);
        mm::drain_all_pages();
        let result = Disk::test_prepare_observed(device, &queue, &mut observation);
        // 成功结果也保住唯一 owner；放压之前不打印、不申请清理容器或重建设备。
        held.clear();
        let error = match result {
            Ok(disk) => {
                disk.stop().expect("unpublished pressure disk stop failed");
                drop(disk);
                None
            }
            Err(error) => Some(error),
        };
        assert_eq!(
            irq::test_action_count(interrupt),
            actions,
            "SG pressure preparation registered an IRQ action"
        );
        assert_eq!(
            super::settled(),
            resident,
            "SG pressure preparation did not return all six memory domains"
        );
        if let Some((tag, snapshot)) = observation {
            assert_eq!(error, Some(BlockError::NoMemory));
            assert_eq!(snapshot.identity, identity);
            assert!(tag < info.tag_count);
            assert_eq!(
                snapshot.irq_actions, actions,
                "live SG reserve failure had registered an IRQ action"
            );
            assert!(
                !snapshot.status.contains(DeviceStatus::DRIVER_OK),
                "SG reserve failure published DRIVER_OK"
            );
            assert!(
                !snapshot.requires_reset,
                "SG reserve failure had already published control DMA"
            );
            assert_eq!(
                snapshot.avail_index, 0,
                "SG reserve failure published avail entries"
            );
            assert_eq!(
                snapshot.available_descriptors,
                usize::from(info.queue_depth)
            );
            failure = Some((order, tag, snapshot.status));
            break;
        }
        assert_eq!(
            error,
            Some(BlockError::NoMemory),
            "bounded page budget reached success before a real SG reserve failure"
        );
    }
    let (order, tag, status) =
        failure.expect("no real SG reserve failure within bounded page budgets");

    let device = prepare().expect("SG pressure backend did not recover after releasing pages");
    assert_eq!(device.identity(), identity);
    let snapshot = device.test_prepare_snapshot();
    assert!(!snapshot.status.contains(DeviceStatus::DRIVER_OK));
    assert_eq!(snapshot.avail_index, 0);
    let disk = Disk::prepare(device, &queue).expect("fixed request/SG preparation did not recover");
    assert_eq!(irq::test_action_count(interrupt), actions);
    disk.stop().expect("recovered unpublished disk stop failed");
    drop(disk);
    assert_eq!(
        super::settled(),
        resident,
        "recovered request/SG preparation leaked memory"
    );
    queue.close_sync();
    drop(queue);
    drop(held);
    assert_eq!(super::settled(), original, "SG OOM scenario leaked memory");
    crate::early_println!(
        "PASS B03 request-sg-oom index={} budget-order={} budget-pages={} failed-tag={} live-status={:#x} unpublished=1 irq-unregistered=1 avail-empty=1 recovered=1 reclaimed-six-domains=1",
        index, order, 1usize << order, tag, status.bits()
    );
}
