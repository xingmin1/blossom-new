//! 真实 QEMU 请求覆盖多在途、额度等待、拆分合并与完成和停机尾部。

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};
use virtio_drivers::{BufferDirection, MappedSegment};

use crate::block::{Bio, BioCompletion, BlkOperation, BlockError, Disk, DiskRef, EndIo, IoResult};
use crate::drivers::virtio::{VirtioDevice, VirtioInitError};
use crate::mm::dma::MappedPages;
use crate::sync::SpinLock;
use crate::task::{self, State, Task};
use crate::wait::{WaitNode, WaitQueue, Waiter};
use crate::workqueue::{WorkQueue, WorkQueueConfig};
use crate::{cpu, mm, sched, time, timer};

static HEAVY_ENTERED: AtomicBool = AtomicBool::new(false);
static HEAVY_RELEASED: AtomicBool = AtomicBool::new(false);
static HEAVY_TASK: SpinLock<Option<Arc<Task>>> = SpinLock::new(None);
static HEAVY_WAIT: WaitQueue = WaitQueue::new();
static PEER: SpinLock<Option<BioCompletion>> = SpinLock::new(None);
static PEER_TASK: SpinLock<Option<Arc<Task>>> = SpinLock::new(None);

/// 回调仅等待另一个父，既不等自身完成，也不等本盘 stop。
fn peer_callback(result: &IoResult, memory: &[MappedPages]) {
    let bytes = memory.iter().map(MappedPages::len).sum();
    assert_eq!(
        *result,
        IoResult {
            completed: bytes,
            error: None
        }
    );
    assert!(memory.iter().all(|mapped| !mapped.is_device_owned()));
    assert!(PEER_TASK.lock().replace(task::current_task()).is_none());
    let peer = PEER.lock().as_ref().expect("peer callback target").clone();
    assert_eq!(
        peer.wait(),
        IoResult {
            completed: bytes,
            error: None
        }
    );
}

/// 两个真实 used 已发表后恢复进度 Work；A 的可睡回调不能阻止 B 的分派。
fn progress_peer(disk: &DiskRef, tag: u8, legacy: bool, batch: bool, index: usize) {
    assert!(PEER.lock().is_none() && PEER_TASK.lock().is_none());
    let unit = disk.limits().logical_block_size as usize;
    let second_sector = if batch { 400 + unit as u64 / 512 } else { 416 };
    let first = read_bio(disk, 400, unit, 0, Some(EndIo::Task(peer_callback)));
    let second = read_bio(disk, second_sector, unit, 0, None);
    let first_completion = first.completion();
    let second_completion = second.completion();
    *PEER.lock() = Some(second_completion.clone());
    disk.test_suspend_completions();
    let before = disk.test_snapshot().submitted;
    if batch {
        let mut bios = Vec::new();
        bios.try_reserve_exact(2)
            .expect("Task callback dependency batch");
        bios.push(first);
        bios.push(second);
        disk.submit_batch(bios);
    } else {
        disk.submit_bio(first);
        disk.submit_bio(second);
    }
    let submitted = disk.test_snapshot().submitted;
    let target = submitted as u16;
    until(|| crate::block::tests::used_index(disk, legacy) == target);
    crate::early_println!(
        "BLOCK REVIEW REPRO index={} case=progress-peer batch={} parents=2 real-used={}",
        index,
        batch as usize,
        submitted - before
    );
    disk.test_resume_via_progress();
    until(|| first_completion.is_complete() && second_completion.is_complete());
    check_read(first_completion, tag, 400, unit);
    check_read(second_completion, tag, second_sector, unit);
    until(|| disk.test_snapshot().inflight == 0);
    let peer = PEER.lock().take();
    let observed = PEER_TASK.lock().take();
    drop((peer, observed));
    crate::early_println!(
        "PASS block-peer index={} origin=progress batch={} real-data=1 no-dispatch-deadlock=1",
        index,
        batch as usize
    );
}

/// 强制 child 先结束再收生成者，复用生产准入和完成；此顺序检查不冒充磁盘 used。
fn generation_peer(disk: &DiskRef, index: usize) {
    let unit = disk.limits().logical_block_size as usize;
    let first = read_bio(disk, 400, unit, 0, Some(EndIo::Task(peer_callback)));
    let second = read_bio(disk, 416, unit, 0, None);
    let first_completion = first.completion();
    let second_completion = second.completion();
    *PEER.lock() = Some(second_completion.clone());
    let owner = disk.clone();
    let producer = task::spawn_kernel(move || {
        crate::block::tests::generation_pair(&owner, first, second);
        0
    })
    .expect("controlled generation-pair producer");
    crate::early_println!(
        "BLOCK REVIEW REPRO index={} case=generation-peer scope=controlled-ledger",
        index
    );
    until(|| first_completion.is_complete() && second_completion.is_complete());
    assert_eq!(producer.join(), 0);
    for completion in [first_completion, second_completion] {
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
                .expect("controlled generation-pair original memory"),
        );
    }
    let peer = PEER.lock().take();
    let observed = PEER_TASK.lock().take();
    drop((peer, observed));
    crate::early_println!("PASS block-peer index={} origin=generation scope=controlled-ledger no-generator-deadlock=1", index);
}

/// 有界协调只等测试参与者；生产额度与完成等待都使用真实等待源。
fn until(ready: impl Fn() -> bool) {
    let deadline = time::now()
        .checked_add(time::Duration::from_secs(5))
        .expect("block scenario deadline overflow");
    while !ready() {
        assert!(time::now() < deadline, "block scenario did not progress");
        timer::sleep(time::Duration::from_millis(1));
    }
}

/// 每个片段消耗独立 order0 原零页，不申请覆盖整个请求的连续 bounce。
fn memory(bytes: usize, first_offset: usize, direction: BufferDirection) -> Vec<MappedPages> {
    let count = (bytes + first_offset).div_ceil(mm::PAGE_SIZE);
    let mut owned = Vec::new();
    owned
        .try_reserve_exact(count)
        .expect("block scenario memory owners");
    let mut remaining = bytes;
    let mut offset = first_offset;
    while remaining != 0 {
        let length = remaining.min(mm::PAGE_SIZE - offset);
        let pages = mm::alloc_pages(0, true).expect("block scenario original page");
        owned.push(
            MappedPages::new(pages, offset, length, direction)
                .expect("block scenario original-page mapping"),
        );
        remaining -= length;
        offset = 0;
    }
    owned
}

/// 唯一原零页消除 CPU 裸别名；Write 测试会在构造前填入真实待写字节。
fn read_bio(disk: &Disk, sector: u64, bytes: usize, offset: usize, end_io: Option<EndIo>) -> Bio {
    let owned = memory(bytes, offset, BufferDirection::DeviceToDriver);
    // SAFETY: owned 消耗独立原零页，无其他 CPU 别名。传入的唯一 Task 回调使用下方
    // 固定等待源，不调用本盘 stop 或等待自己的父完成；None 没有额外回调责任。
    unsafe { Bio::try_new(disk, BlkOperation::Read, sector, owned, 0, end_io) }
        .expect("block scenario read construction")
}

/// 完整真实成功与收权后逐字节检查源盘图样，不把非空或计数增加当正确数据。
fn check_read(completion: BioCompletion, tag: u8, sector: u64, bytes: usize) {
    assert_eq!(
        completion.wait(),
        IoResult {
            completed: bytes,
            error: None
        }
    );
    let owned = completion
        .take_memory()
        .expect("block scenario read ownership");
    let mut logical = 0;
    for mapped in &owned {
        assert!(!mapped.is_device_owned());
        // SAFETY: 所有原页在交权前零填，完整真实 Read 的 child 都已结束并收权；
        // 此处只借唯一完成拥有者，期间没有设备或其他 CPU 写入。
        for actual in unsafe { mapped.bytes() } {
            let at = sector * 512 + logical as u64;
            let expected = tag
                .wrapping_mul(29)
                .wrapping_add(((at / 512) as u8).wrapping_mul(17));
            let expected = expected.wrapping_add((at % 512) as u8);
            assert_eq!(
                *actual, expected,
                "block scenario wrong disk/sector/SG data"
            );
            logical += 1;
        }
    }
    assert_eq!(logical, bytes);
}

/// 同一映射地址用于入口反例，但这些非法描述符从不发表给设备。
fn entry_boundaries(disk: &Disk, index: usize) {
    let limits = disk.limits();
    let unit = limits.logical_block_size as usize;
    let owned = memory(unit, 0, BufferDirection::DeviceToDriver);
    let segment = owned[0].segment();
    assert_eq!(
        limits.validate(BlkOperation::Read, u64::MAX, unit, [segment], 0),
        Err(BlockError::InvalidRange)
    );
    assert_eq!(
        limits.validate(
            BlkOperation::Read,
            limits.capacity_sectors,
            unit,
            [segment],
            0
        ),
        Err(BlockError::InvalidRange)
    );
    assert_eq!(
        limits.validate(BlkOperation::Read, 0, unit + 512, [segment], 0),
        Err(BlockError::InvalidRange)
    );
    assert_eq!(
        limits.validate(BlkOperation::Read, 0, 0, [], 0),
        Err(BlockError::InvalidRange)
    );
    assert_eq!(
        limits.validate(BlkOperation::Read, 0, unit, [segment], 1),
        Err(BlockError::Unsupported)
    );
    let wrong_direction = MappedSegment {
        direction: BufferDirection::DriverToDevice,
        ..segment
    };
    assert_eq!(
        limits.validate(BlkOperation::Read, 0, unit, [wrong_direction], 0),
        Err(BlockError::InvalidRange)
    );
    let short = MappedSegment {
        length: 511,
        ..segment
    };
    assert_eq!(
        limits.validate(BlkOperation::Read, 0, 511, [short], 0),
        Err(BlockError::Misaligned)
    );
    let mut larger_logical = limits;
    larger_logical.logical_block_size = 4096;
    assert_eq!(
        larger_logical.validate(BlkOperation::Read, 1, unit, [segment], 0),
        Err(BlockError::Misaligned)
    );
    let mut readonly = limits;
    readonly.readonly = true;
    assert_eq!(
        readonly.validate(BlkOperation::Write, 0, unit, [wrong_direction], 0),
        Err(BlockError::ReadOnly)
    );
    let mut no_flush = limits;
    no_flush.flush = false;
    assert_eq!(
        no_flush.validate(BlkOperation::Flush, 0, 0, [], 0),
        Err(BlockError::Unsupported)
    );
    let mut supports_flush = limits;
    supports_flush.flush = true;
    assert_eq!(
        supports_flush.validate(BlkOperation::Flush, 1, 0, [], 0),
        Err(BlockError::InvalidRange)
    );
    assert_eq!(
        supports_flush.validate(BlkOperation::Flush, 0, unit, [segment], 0),
        Err(BlockError::InvalidRange)
    );
    let mut destination = [0x73_u8; 512];
    assert_eq!(
        disk.read_exact_at(1, &mut destination),
        Err(BlockError::Misaligned)
    );
    assert!(destination.iter().all(|byte| *byte == 0x73));
    crate::early_println!(
        "PASS block-entry index={} scope=controlled-invalid-input unchanged-dst=1",
        index
    );
}

/// 八个不同真实 Task 在不同 sector 发表两页请求，暂停只禁止本队列回收真实 used。
fn multi_task(disk: &DiskRef, tag: u8, index: usize) {
    let mut prepared = Vec::new();
    prepared
        .try_reserve_exact(8)
        .expect("block multi-task preparation");
    let before = disk.test_snapshot();
    for ordinal in 0..8 {
        let sector = 64 + ordinal as u64 * 32;
        let bio = read_bio(disk, sector, 2 * mm::PAGE_SIZE, 0, None);
        let completion = bio.completion();
        let owner = disk.clone();
        prepared.push(
            task::prepare_kernel_on(ordinal % cpu::count(), move || {
                owner.submit_bio(bio);
                check_read(completion, tag, sector, 2 * mm::PAGE_SIZE);
                0
            })
            .expect("block multi-task actual Task"),
        );
    }
    disk.test_suspend_completions();
    let mut joins = Vec::new();
    joins
        .try_reserve_exact(prepared.len())
        .expect("block multi-task join storage");
    for task in prepared {
        joins.push(task.publish());
    }
    until(|| disk.test_snapshot().submitted == before.submitted + 8);
    assert_eq!(disk.test_snapshot().inflight, 8);
    assert!(joins.iter().all(|handle| handle.try_join().is_none()));
    disk.test_resume_completions();
    for handle in joins {
        assert_eq!(handle.join(), 0);
    }
    until(|| disk.test_snapshot().inflight == 0);
    let after = disk.test_snapshot();
    assert!(after.peak_inflight > 1);
    crate::early_println!(
        "PASS B04 block index={} tasks=8 pages-per-bio=2 peak={} real-data=1",
        index,
        after.peak_inflight
    );
}

/// 先用真实已发表请求耗尽一种额度，额外提交者必须在生产调度器中进入 Blocked。
fn backpressure(disk: &DiskRef, tag: u8, bytes: usize, fill: usize, kind: &str, index: usize) {
    let mut completions = Vec::new();
    completions
        .try_reserve_exact(fill)
        .expect("block backpressure completed owners");
    let observed = Arc::new(SpinLock::new(None::<Arc<Task>>));
    let next = read_bio(disk, 1536, bytes, 0, None);
    let completion = next.completion();
    let owner = disk.clone();
    let task_observed = observed.clone();
    let extra = task::prepare_kernel_on((cpu::current().id + 1) % cpu::count(), move || {
        *task_observed.lock() = Some(task::current_task());
        owner.submit_bio(next);
        check_read(completion, tag, 1536, bytes);
        0
    })
    .expect("block backpressure submitter preparation");
    let before = disk.test_snapshot();
    disk.test_suspend_completions();
    for ordinal in 0..fill {
        let bio = read_bio(disk, 512 + ordinal as u64 * 40, bytes, 0, None);
        completions.push(bio.completion());
        disk.submit_bio(bio);
    }
    assert_eq!(disk.test_snapshot().submitted, before.submitted + fill);
    assert_eq!(disk.test_snapshot().inflight, fill);
    let extra = extra.publish();
    until(|| {
        let seen = observed.lock().as_ref().map(Arc::clone);
        seen.is_some_and(|task| sched::test_state(&task) == State::Blocked)
            && disk.test_snapshot().resource_parks > before.resource_parks
    });
    assert!(extra.try_join().is_none());
    assert!(disk.test_snapshot().notifications > before.notifications);
    disk.test_resume_completions();
    for (ordinal, completion) in completions.into_iter().enumerate() {
        check_read(completion, tag, 512 + ordinal as u64 * 40, bytes);
    }
    assert_eq!(extra.join(), 0);
    let task = observed.lock().take();
    drop(task);
    until(|| disk.test_snapshot().inflight == 0);
    let after = disk.test_snapshot();
    assert_eq!(after.submitted - before.submitted, fill + 1);
    assert_eq!(after.completed - before.completed, fill + 1);
    if kind == "tag" {
        assert!(after.budget_continuations > before.budget_continuations);
        crate::early_println!(
            "PASS B08 block index={} flags-paused=1 rearm=1 budget-continuations={} real-used=1",
            index,
            after.budget_continuations - before.budget_continuations
        );
    }
    crate::early_println!(
        "PASS B07 block index={} quota={} accepted={} real-blocked=1 wake=1 kicked=1",
        index,
        kind,
        fill
    );
}

/// 回调真正在预建 Work 的 Task 上睡眠，释放门控以前不可完成父或回收此槽。
fn heavy_callback(result: &IoResult, memory: &[MappedPages]) {
    assert_eq!(
        *result,
        IoResult {
            completed: memory.iter().map(MappedPages::len).sum(),
            error: None
        }
    );
    assert!(memory.iter().all(|mapped| !mapped.is_device_owned()));
    let current = task::current_task();
    assert!(
        current.worker.is_some(),
        "heavy finalizer did not use prepared Work"
    );
    assert!(HEAVY_TASK.lock().replace(current).is_none());
    HEAVY_ENTERED.store(true, Ordering::Release);
    while !HEAVY_RELEASED.load(Ordering::Acquire) {
        let waiter = Waiter::new();
        let mut node = core::pin::pin!(WaitNode::new(waiter.waker()));
        // SAFETY: 静态源永久存在，节点固定到本轮 registration 撤链后；回调真实 Task 保活。
        let registration = unsafe { HEAVY_WAIT.register(node.as_mut()) };
        if !HEAVY_RELEASED.load(Ordering::Acquire) {
            waiter.park();
        }
        drop(registration);
    }
}

/// 保住生成者到其结束，再由真实 used 驱动重型 callback 和槽尾归还。
fn heavy_tail(disk: &DiskRef, tag: u8, index: usize) {
    HEAVY_ENTERED.store(false, Ordering::Release);
    HEAVY_RELEASED.store(false, Ordering::Release);
    assert!(HEAVY_TASK.lock().is_none());
    let before = disk.test_snapshot();
    disk.test_suspend_completions();
    let unit = disk.limits().logical_block_size as usize;
    let bio = read_bio(disk, 384, unit, 0, Some(EndIo::Task(heavy_callback)));
    let completion = bio.completion();
    disk.submit_bio(bio);
    assert!(!completion.is_complete());
    disk.test_resume_completions();
    until(|| {
        let seen = HEAVY_TASK.lock().as_ref().map(Arc::clone);
        HEAVY_ENTERED.load(Ordering::Acquire)
            && seen.is_some_and(|task| sched::test_state(&task) == State::Blocked)
    });
    assert!(!completion.is_complete());
    assert_eq!(disk.test_snapshot().inflight, 1);
    assert_eq!(disk.test_snapshot().completed, before.completed);
    HEAVY_RELEASED.store(true, Ordering::Release);
    HEAVY_WAIT.wake_all();
    check_read(completion, tag, 384, unit);
    until(|| disk.test_snapshot().inflight == 0);
    let observed = HEAVY_TASK.lock().take();
    drop(observed);
    crate::early_println!(
        "PASS block-finalizer index={} task-work=1 real-blocked=1 slot-held=1 tail-free=1",
        index
    );
}

/// 不同父的相邻范围在一个实际硬件请求中完成，每个父仍返回自己的原页。
fn merge(disk: &DiskRef, tag: u8, index: usize) {
    let unit = disk.limits().logical_block_size as usize;
    let mut batch = Vec::new();
    let mut completions = Vec::new();
    batch.try_reserve_exact(3).expect("block merge batch");
    completions
        .try_reserve_exact(3)
        .expect("block merge obligations");
    for ordinal in 0..3 {
        let bio = read_bio(disk, 256 + ordinal * unit as u64 / 512, unit, 0, None);
        completions.push(bio.completion());
        batch.push(bio);
    }
    let before = disk.test_snapshot();
    disk.submit_batch(batch);
    for (ordinal, completion) in completions.into_iter().enumerate() {
        check_read(
            completion,
            tag,
            256 + ordinal as u64 * unit as u64 / 512,
            unit,
        );
    }
    until(|| disk.test_snapshot().inflight == 0);
    let after = disk.test_snapshot();
    assert_eq!(after.submitted - before.submitted, 1);
    assert_eq!(after.merges - before.merges, 2);
    crate::early_println!(
        "PASS B09 block index={} merged-parents=3 requests=1 notified=1",
        index
    );
}

/// 活挂载拒绝原始写和 stop；释放 claim 后 scratch 真实写入、读回以及 FLUSH。
fn scratch(disk: &DiskRef, index: usize) {
    let unit = disk.limits().logical_block_size as usize;
    let claim = Disk::claim_readonly(disk).expect("block readonly mount claim");
    assert_eq!(disk.stop(), Err(BlockError::Busy));
    if !disk.limits().readonly {
        let mut owned = memory(unit, 0, BufferDirection::DriverToDevice);
        for (offset, byte) in owned[0].bytes_mut().iter_mut().enumerate() {
            byte.write((offset as u8).wrapping_mul(13).wrapping_add(71));
        }
        // SAFETY: 原页全范围已初始化，无 CPU 别名或回调，完成持有每个 child 的原页。
        let bio = unsafe { Bio::try_new(disk, BlkOperation::Write, 2048, owned, 0, None) }
            .expect("block scratch protected write construction");
        let completion = bio.completion();
        disk.submit_bio(bio);
        assert_eq!(
            completion.wait(),
            IoResult {
                completed: 0,
                error: Some(BlockError::ReadOnly)
            }
        );
        let mut owned = completion
            .take_memory()
            .expect("block protected write original memory");
        drop(claim);
        // SAFETY: 拒绝写从未交给设备，原范围仍初始化且无别名；挂载保护已解除。
        let bio = unsafe {
            Bio::try_new(
                disk,
                BlkOperation::Write,
                2048,
                core::mem::take(&mut owned),
                0,
                None,
            )
        }
        .expect("block scratch write construction");
        let completion = bio.completion();
        disk.submit_bio(bio);
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
                .expect("block scratch write original memory"),
        );
        let mut read_back = [0_u8; mm::PAGE_SIZE];
        let read_back = &mut read_back[..unit];
        disk.read_exact_at(2048 * 512, read_back)
            .expect("block scratch real read-back");
        for (offset, byte) in read_back.iter().copied().enumerate() {
            assert_eq!(byte, (offset as u8).wrapping_mul(13).wrapping_add(71));
        }
    } else {
        drop(claim);
    }
    if disk.limits().flush {
        // SAFETY: Flush 无 payload，无裸别名或 callback，能力来自本设备的实际协商。
        let bio = unsafe { Bio::try_new(disk, BlkOperation::Flush, 0, Vec::new(), 0, None) }
            .expect("block scratch flush construction");
        let completion = bio.completion();
        disk.submit_bio(bio);
        assert_eq!(
            completion.wait(),
            IoResult {
                completed: 0,
                error: None
            }
        );
        drop(
            completion
                .take_memory()
                .expect("block scratch flush memory"),
        );
    }
    crate::early_println!(
        "PASS B10 block index={} write={} read-back={} flush={} mount-protected=1 busy-stop=1",
        index,
        usize::from(!disk.limits().readonly),
        usize::from(!disk.limits().readonly),
        usize::from(disk.limits().flush)
    );
}

/// 单父多原页按真实限额分成合法 child，父仍统一收权和报告完整真实图样。
fn split(disk: &DiskRef, tag: u8, index: usize) {
    let before = disk.test_snapshot();
    disk.test_suspend_completions();
    let bio = read_bio(disk, 320, 3 * mm::PAGE_SIZE, 0, None);
    let completion = bio.completion();
    disk.submit_bio(bio);
    assert!(!completion.is_complete());
    let submitted = disk.test_snapshot();
    assert!(submitted.submitted - before.submitted > 1);
    assert!(submitted.splits > before.splits);
    disk.test_resume_completions();
    check_read(completion, tag, 320, 3 * mm::PAGE_SIZE);
    until(|| disk.test_snapshot().inflight == 0);
    crate::early_println!("PASS B05 block index={} pages=3 children={} max-bytes={} max-segments={} segment-bytes={} boundary-mask={} original-pages=1", index,
        submitted.submitted - before.submitted, disk.limits().max_bytes, disk.limits().max_segments,
        disk.limits().max_segment_bytes, disk.limits().dma_boundary_mask);
}

/// 已发表首父后，尾父 DMA 边界不能容纳一个 logical block；首父必须被 unplug。
fn failing_batch_tail(disk: &DiskRef, tag: u8, index: usize) {
    let unit = disk.limits().logical_block_size as usize;
    let first = read_bio(disk, 448, unit, 0, None);
    let second = read_bio(disk, 448 + unit as u64 / 512, unit, unit / 2, None);
    let first_completion = first.completion();
    let second_completion = second.completion();
    let mut batch = Vec::new();
    batch
        .try_reserve_exact(2)
        .expect("block failing batch tail");
    batch.push(first);
    batch.push(second);
    let before = disk.test_snapshot();
    disk.submit_batch(batch);
    check_read(first_completion, tag, 448, unit);
    assert_eq!(
        second_completion.wait(),
        IoResult {
            completed: 0,
            error: Some(BlockError::Unsupported)
        }
    );
    drop(
        second_completion
            .take_memory()
            .expect("block failed tail original memory"),
    );
    until(|| disk.test_snapshot().inflight == 0);
    let after = disk.test_snapshot();
    assert_eq!(after.submitted - before.submitted, 1);
    assert!(after.notifications > before.notifications);
    crate::early_println!(
        "PASS B09 block-tail index={} accepted-prefix=1 failed-tail=1 unplugged=1 completed=1",
        index
    );
}

/// 受控故障入口仍执行生产闭门和真实 reset，不伪造 used/status 或重放任何写。
fn fault(disk: &DiskRef, index: usize) {
    disk.test_suspend_completions();
    let bio = read_bio(
        disk,
        480,
        disk.limits().logical_block_size as usize,
        0,
        None,
    );
    let completion = bio.completion();
    disk.submit_bio(bio);
    assert!(!completion.is_complete());
    let published = disk.test_snapshot().submitted;
    crate::block::tests::retire(disk);
    until(|| crate::block::tests::dma_stopped(disk));
    assert_eq!(
        completion.wait(),
        IoResult {
            completed: 0,
            error: Some(BlockError::DeviceFault)
        }
    );
    drop(
        completion
            .take_memory()
            .expect("block fault original memory"),
    );
    assert_eq!(disk.stop(), Err(BlockError::DeviceFault));
    assert_eq!(disk.stop(), Err(BlockError::DeviceFault));
    let after = disk.test_snapshot();
    assert_eq!(after.submitted, published);
    assert_eq!(after.completed, after.submitted);
    assert_eq!(after.inflight, 0);
    crate::early_println!("PASS B11 block-fault index={} scope=controlled-retirement real-reset-zero=1 failed-once=1 replay=0", index);
}

/// 设备停止与所有异步票关闭后才换同一硬件的下一次原控制区拥有者。
fn finish(disk: DiskRef) {
    until(|| disk.test_snapshot().inflight == 0);
    disk.stop().expect("block scenario normal confirmed stop");
    let bio = read_bio(
        &disk,
        32,
        disk.limits().logical_block_size as usize,
        0,
        None,
    );
    let completion = bio.completion();
    disk.submit_bio(bio);
    assert_eq!(
        completion.wait(),
        IoResult {
            completed: 0,
            error: Some(BlockError::QueueStopped)
        }
    );
    drop(
        completion
            .take_memory()
            .expect("block stopped admission original memory"),
    );
    drop(disk);
}

/// 从真实协商能力准备一个完整共享域，串行独占同一硬件完成所有块层场景。
pub(super) fn run(
    prepare: &mut impl FnMut() -> Result<VirtioDevice, VirtioInitError>,
    legacy: bool,
    index: usize,
) {
    let original = super::settled();
    let device = prepare().expect("block scenario backend preparation");
    let info = device.info();
    let unit = info.logical_block_size as usize;
    assert!(
        unit <= mm::PAGE_SIZE,
        "block fixture requires logical block at most one page"
    );
    let queue = WorkQueue::try_new(WorkQueueConfig {
        max_active: usize::from(info.tag_count) + 1,
        ordered: false,
        reclaim: true,
    })
    .expect("block scenario completion domain preparation");
    let disk = Disk::prepare(device, &queue).expect("block scenario fixed request preparation");
    disk.start().expect("block scenario actual publication");
    crate::block::tests::aggregation(&disk);
    entry_boundaries(&disk, index);
    let mut first = [0_u8; mm::PAGE_SIZE];
    disk.read_exact_at(0, &mut first[..unit])
        .expect("block scenario fixture identity read");
    let tag = match first[0] {
        29 => 1,
        58 => 2,
        _ => panic!("block scenarios require a known real patterned fixture"),
    };
    if !info.readonly {
        super::finalizer::run(&disk, legacy, index);
    }
    progress_peer(&disk, tag, legacy, false, index);
    progress_peer(&disk, tag, legacy, true, index);
    generation_peer(&disk, index);
    multi_task(&disk, tag, index);
    backpressure(&disk, tag, unit, usize::from(info.tag_count), "tag", index);
    heavy_tail(&disk, tag, index);
    merge(&disk, tag, index);
    scratch(&disk, index);
    finish(disk);

    assert!(info.max_segments >= 4 && info.max_segment_bytes >= mm::PAGE_SIZE as u32);
    let disk = Disk::test_prepare_with_limits(
        prepare().expect("descriptor quota backend"),
        &queue,
        4 * mm::PAGE_SIZE,
        usize::from(info.max_segments),
        mm::PAGE_SIZE as u32,
        u64::MAX,
    )
    .expect("descriptor quota fixed requests");
    disk.start().expect("descriptor quota actual publication");
    let fill = usize::from(info.queue_depth) / 6;
    assert!(
        fill < usize::from(info.tag_count),
        "descriptor test exhausted tags first"
    );
    backpressure(&disk, tag, 4 * mm::PAGE_SIZE, fill, "descriptor", index);
    finish(disk);

    let disk = Disk::test_prepare_with_limits(
        prepare().expect("split backend"),
        &queue,
        mm::PAGE_SIZE,
        2,
        (unit * 3 / 2) as u32,
        (unit * 4 - 1) as u64,
    )
    .expect("split fixed request limits");
    disk.start().expect("split actual publication");
    split(&disk, tag, index);
    finish(disk);

    let disk = Disk::test_prepare_with_limits(
        prepare().expect("batch failure backend"),
        &queue,
        unit * 2,
        1,
        unit as u32,
        (unit - 1) as u64,
    )
    .expect("batch failure fixed request limits");
    disk.start().expect("batch failure actual publication");
    failing_batch_tail(&disk, tag, index);
    finish(disk);

    let disk =
        Disk::prepare(prepare().expect("fault backend"), &queue).expect("fault fixed requests");
    disk.start().expect("fault actual publication");
    fault(&disk, index);
    drop(disk);
    queue.close_sync();
    drop(queue);
    until(|| super::settled() == original);
    crate::early_println!(
        "PASS block-suite index={} real-data=1 normal-stop=1 fault-stop=1 reclaimed=1",
        index
    );
}
