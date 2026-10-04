//! 非槽 Bio 完成回调占满原域时，真实设备完成仍必须能解除合法跨父等待。

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};
use virtio_drivers::{BufferDirection, MappedSegment};

use crate::block::{Bio, BioCompletion, BlkOperation, BlockError, Disk, DiskRef, EndIo, IoResult};
use crate::mm::dma::MappedPages;
use crate::sync::SpinLock;
use crate::task::{self, State, Task};
use crate::{mm, sched, time, timer};

// EndIo 是函数指针；唯一串行槽只定位本例拥有者，不承担跨例同步或执行顺序。
static ACTIVE: SpinLock<Option<Arc<Fixture>>> = SpinLock::new(None);

#[derive(Clone, Copy)]
enum Origin {
    Reject,
    Generation,
}

impl Origin {
    /// 日志分别标明真实 admission 拒绝与不发表地址的受控父账本。
    fn name(self) -> &'static str {
        match self {
            Self::Reject => "reject",
            Self::Generation => "generation-last",
        }
    }
}

struct Fixture {
    peer: BioCompletion,
    expected: IoResult,
    originals: Vec<MappedSegment>,
    tasks: SpinLock<Vec<Option<Arc<Task>>>>,
    entered: AtomicUsize,
    returned: AtomicUsize,
    peer_calls: AtomicUsize,
    unit: usize,
}

impl Fixture {
    /// 先移出每个短锁借用，再向真实调度器查询全部回调参与者的阻塞状态。
    fn blocked(&self) -> usize {
        (0..self.originals.len())
            .filter(|&ordinal| {
                let observed = self.tasks.lock()[ordinal].clone();
                observed.is_some_and(|task| sched::test_state(&task) == State::Blocked)
            })
            .count()
    }
}

/// 借稳定串行槽取得本例强拥有，锁外执行全部业务和等待。
fn fixture() -> Arc<Fixture> {
    ACTIVE
        .lock()
        .as_ref()
        .expect("finalizer fixture is absent")
        .clone()
}

/// 每个非槽回调只等待独立 Read B，不等待自身、stop 或任何复用槽。
fn waiting_callback(result: &IoResult, memory: &[MappedPages]) {
    let owner = fixture();
    assert_eq!(*result, owner.expected);
    assert_eq!(memory.len(), 1);
    assert!(!memory[0].is_device_owned());
    assert_eq!(memory[0].len(), owner.unit);
    // SAFETY: 本例原页分配时全部清零；reject 从未交权，受控 generation 不发表
    // descriptor 且已结束全部父义务。回调只借 CPU-owned 原页，没有并发写者。
    assert!(unsafe { memory[0].bytes() }.iter().all(|byte| *byte == 0));
    let address = memory[0].segment().address;
    let ordinal = owner
        .originals
        .iter()
        .position(|original| original.address == address)
        .expect("finalizer callback received a different original page");
    let observed = task::current_task();
    {
        let mut tasks = owner.tasks.lock();
        assert!(
            tasks[ordinal].replace(observed).is_none(),
            "finalizer callback ran twice"
        );
    }
    owner.entered.fetch_add(1, Ordering::Release);
    assert_eq!(
        owner.peer.wait(),
        IoResult {
            completed: owner.unit,
            error: None
        }
    );
    owner.returned.fetch_add(1, Ordering::Release);
}

/// B 的 Task 回调不睡眠，仍必须经真实 slot 完成 Work 调度并且只调用一次。
fn peer_callback(result: &IoResult, memory: &[MappedPages]) {
    let owner = fixture();
    sched::assert_switchable();
    assert_eq!(
        *result,
        IoResult {
            completed: owner.unit,
            error: None
        }
    );
    assert_eq!(memory.len(), 1);
    assert!(!memory[0].is_device_owned());
    assert_eq!(owner.peer_calls.fetch_add(1, Ordering::AcqRel), 0);
}

/// 独立原零页构造一份 payload，CPU 不保留任何裸别名或额外页拥有者。
fn bio(
    disk: &Disk,
    operation: BlkOperation,
    callback: fn(&IoResult, &[MappedPages]),
) -> (Bio, MappedSegment) {
    let unit = disk.limits().logical_block_size as usize;
    assert!(
        unit <= mm::PAGE_SIZE,
        "finalizer fixture requires logical block at most one page"
    );
    let direction = if operation == BlkOperation::Write {
        BufferDirection::DriverToDevice
    } else {
        BufferDirection::DeviceToDriver
    };
    let page = mm::alloc_pages(0, true).expect("finalizer fixture original zero page");
    let mapped = MappedPages::new(page, 0, unit, direction).expect("finalizer fixture mapping");
    let original = mapped.segment();
    let mut memory = Vec::new();
    memory
        .try_reserve_exact(1)
        .expect("finalizer fixture original owner vector");
    memory.push(mapped);
    // SAFETY: 独立原零页已初始化 Write 全范围，构造后无 CPU 裸别名；两种 Task
    // 回调都只借原页，waiting_callback 仅等待独立 B，不等待自身或本盘 stop。
    let bio = unsafe { Bio::try_new(disk, operation, 400, memory, 0, Some(EndIo::Task(callback))) }
        .expect("finalizer fixture Bio construction");
    (bio, original)
}

/// 有限期限等待确切谓词；timer 只让出 CPU，不被当成回调已经阻塞的证据。
fn until(label: &str, ready: impl Fn() -> bool) {
    let deadline = time::now()
        .checked_add(time::Duration::from_secs(5))
        .expect("finalizer fixture deadline overflow");
    while !ready() {
        assert!(
            time::now() < deadline,
            "finalizer fixture deadline: {}",
            label
        );
        timer::sleep(time::Duration::from_millis(1));
    }
}

/// 完成后只领取同一原页一次，核对 DMA 地址、长度与方向，不以复制页替代原拥有。
fn take(
    completion: &BioCompletion,
    original: MappedSegment,
    expected: IoResult,
) -> Vec<MappedPages> {
    assert_eq!(completion.result(), Some(expected));
    let memory = completion
        .take_memory()
        .expect("finalizer fixture original memory claim");
    assert_eq!(memory.len(), 1);
    assert!(!memory[0].is_device_owned());
    assert_eq!(memory[0].segment().address, original.address);
    assert_eq!(memory[0].segment().length, original.length);
    assert_eq!(memory[0].segment().direction, original.direction);
    assert_eq!(completion.take_memory().err(), Some(BlockError::Busy));
    memory
}

/// 先预备所有父、观察拥有与外部 Task，再占满非槽回调准入并发表真实 Read B。
fn saturation(disk: &DiskRef, legacy: bool, index: usize, origin: Origin, tag: u8) {
    assert!(ACTIVE.lock().is_none());
    let unit = disk.limits().logical_block_size as usize;
    let count = usize::from(disk.limits().tag_count) + 1;
    let operation = match origin {
        Origin::Reject => BlkOperation::Write,
        Origin::Generation => BlkOperation::Read,
    };
    let expected = match origin {
        Origin::Reject => IoResult {
            completed: 0,
            error: Some(BlockError::ReadOnly),
        },
        Origin::Generation => IoResult {
            completed: unit,
            error: None,
        },
    };
    let (peer, peer_original) = bio(disk, BlkOperation::Read, peer_callback);
    let peer_completion = peer.completion();
    let mut pending = Vec::new();
    let mut completions = Vec::new();
    let mut originals = Vec::new();
    let mut tasks = Vec::new();
    pending
        .try_reserve_exact(count)
        .expect("finalizer fixture prepared Bios");
    completions
        .try_reserve_exact(count)
        .expect("finalizer fixture completion owners");
    originals
        .try_reserve_exact(count)
        .expect("finalizer fixture original observations");
    tasks
        .try_reserve_exact(count)
        .expect("finalizer fixture Task observation owners");
    for _ in 0..count {
        let (bio, original) = bio(disk, operation, waiting_callback);
        completions.push(bio.completion());
        originals.push(original);
        tasks.push(None);
        pending.push(bio);
    }
    let owner = Arc::try_new(Fixture {
        peer: peer_completion.clone(),
        expected,
        originals,
        tasks: SpinLock::new(tasks),
        entered: AtomicUsize::new(0),
        returned: AtomicUsize::new(0),
        peer_calls: AtomicUsize::new(0),
        unit,
    })
    .expect("finalizer fixture stable observation owner");
    let producer_disk = disk.clone();
    let producer = task::prepare_kernel_on(crate::cpu::current().id, move || {
        for bio in pending {
            match origin {
                Origin::Reject => producer_disk.submit_bio(bio),
                Origin::Generation => crate::block::tests::generation_last(&producer_disk, bio),
            }
        }
        0
    })
    .expect("finalizer fixture external producer preparation");
    let peer_disk = disk.clone();
    let submitter = task::prepare_kernel_on(crate::cpu::current().id, move || {
        peer_disk.submit_bio(peer);
        0
    })
    .expect("finalizer fixture external B submitter preparation");
    *ACTIVE.lock() = Some(owner.clone());
    let claim = match origin {
        Origin::Reject => {
            Some(Disk::claim_readonly(disk).expect("finalizer fixture read-only claim"))
        }
        Origin::Generation => None,
    };
    let before = disk.test_snapshot();
    let producer = producer.publish();
    until(
        "all non-slot callbacks entered and genuinely Blocked",
        || owner.entered.load(Ordering::Acquire) == count && owner.blocked() == count,
    );
    assert!(completions
        .iter()
        .all(|completion| !completion.is_complete()));
    assert_eq!(owner.returned.load(Ordering::Acquire), 0);
    assert_eq!(disk.test_snapshot().submitted, before.submitted);
    assert_eq!(disk.test_snapshot().inflight, 0);
    until("producer has left its stack", || {
        producer.try_join() == Some(0)
    });
    crate::early_println!(
        "BLOCK REVIEW REPRO index={} case=finalizer-quota origin={} scope={} quota={} entered={} real-blocked={} submitted-delta=0",
        index, origin.name(), if matches!(origin, Origin::Reject) { "real-admission" } else { "controlled-ledger-no-descriptors" },
        count, owner.entered.load(Ordering::Acquire), owner.blocked()
    );

    // 只暂停真实 used 消费，以先记录硬件已经完成 B；恢复仍走生产 progress/slot Work。
    disk.test_suspend_completions();
    let submitter = submitter.publish();
    until("external B publication", || {
        disk.test_snapshot().submitted == before.submitted + 1
    });
    let target = (before.submitted + 1) as u16;
    until("real device B used", || {
        crate::block::tests::used_index(disk, legacy) == target
    });
    // 必须先结束 B 生成者，恢复后最后义务才确定来自真实 slot 而非 Bio finalizer。
    until("external B submitter has left its stack", || {
        submitter.try_join() == Some(0)
    });
    assert!(!peer_completion.is_complete());
    assert_eq!(owner.peer_calls.load(Ordering::Acquire), 0);
    assert_eq!(owner.blocked(), count);
    crate::early_println!(
        "BLOCK REVIEW REPRO index={} case=finalizer-quota origin={} quota={} real-blocked={} B-published=1 real-used={} B-task-callback=0 awaiting-resume=1",
        index, origin.name(), count, owner.blocked(), target
    );
    disk.test_resume_via_progress();
    until(
        "B Task completion behind saturated non-slot finalizers",
        || peer_completion.is_complete(),
    );
    until("all non-slot callback tails", || {
        owner.returned.load(Ordering::Acquire) == count
            && completions.iter().all(BioCompletion::is_complete)
    });
    assert_eq!(producer.join(), 0);
    assert_eq!(submitter.join(), 0);
    until("real B slot release", || disk.test_snapshot().inflight == 0);
    assert_eq!(owner.entered.load(Ordering::Acquire), count);
    assert_eq!(owner.returned.load(Ordering::Acquire), count);
    assert_eq!(owner.peer_calls.load(Ordering::Acquire), 1);
    let after = disk.test_snapshot();
    assert_eq!(after.submitted - before.submitted, 1);
    assert_eq!(after.completed - before.completed, 1);
    for (completion, original) in completions.iter().zip(&owner.originals) {
        drop(take(completion, *original, expected));
    }
    let memory = take(
        &peer_completion,
        peer_original,
        IoResult {
            completed: unit,
            error: None,
        },
    );
    // SAFETY: 原页清零且真实设备 Read 全长成功；CPU 已收权，唯一领取者借用期间
    // 没有设备引用或并发 CPU 写者。逐字节检查同一实际盘的固定 sector400 图样。
    for (offset, actual) in unsafe { memory[0].bytes() }.iter().enumerate() {
        let at = 400 * 512 + offset as u64;
        let expected = tag
            .wrapping_mul(29)
            .wrapping_add(((at / 512) as u8).wrapping_mul(17))
            .wrapping_add((at % 512) as u8);
        assert_eq!(
            *actual, expected,
            "finalizer B wrong disk/sector/original-page data"
        );
    }
    drop(memory);
    // ready 发布早于 Work 票尾部；先真正 flush 每个 Bio finalizer，再移出观察强拥有。
    for completion in &completions {
        completion.test_flush_finalizer();
    }
    peer_completion.test_flush_finalizer();
    disk.test_flush_completions();
    drop(claim);
    let observed = core::mem::take(&mut *owner.tasks.lock());
    assert_eq!(observed.iter().filter(|task| task.is_some()).count(), count);
    drop(observed);
    let active = ACTIVE
        .lock()
        .take()
        .expect("finalizer fixture active owner disappeared");
    assert!(Arc::ptr_eq(&active, &owner));
    drop((active, owner, completions, peer_completion));
    crate::early_println!(
        "PASS block-finalizer-quota index={} origin={} quota={} real-blocked={} B-real-data=1 B-task-once=1 original-pages-once=1 normal-join=1",
        index, origin.name(), count, count
    );
}

/// 父在独盘 tag_count+1 的原共享域上串行调用，盘保持 Live，停止与域关闭仍归父。
pub(super) fn run(disk: &DiskRef, legacy: bool, index: usize) {
    let unit = disk.limits().logical_block_size as usize;
    assert!(unit <= mm::PAGE_SIZE);
    assert!(
        !disk.limits().readonly,
        "finalizer admission regression requires a writable fixture"
    );
    let mut first = [0_u8; mm::PAGE_SIZE];
    disk.read_exact_at(0, &mut first[..unit])
        .expect("finalizer fixture identity read");
    let tag = match first[0] {
        29 => 1,
        58 => 2,
        _ => panic!("finalizer regression requires a known real patterned fixture"),
    };
    until("identity slot release", || {
        disk.test_snapshot().inflight == 0
    });
    disk.test_flush_completions();
    let original = super::settled();
    for origin in [Origin::Reject, Origin::Generation] {
        saturation(disk, legacy, index, origin, tag);
        until("all six fixture memory domains returned", || {
            super::settled() == original
        });
        crate::early_println!(
            "PASS block-finalizer-quota-resources index={} origin={} reclaimed-six-domains=1",
            index,
            origin.name()
        );
    }
}
