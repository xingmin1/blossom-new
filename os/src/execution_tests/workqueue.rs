//! 以真实任务、工作域、定时器与唯一 MM 验证工作进度和一次停止责任。

use alloc::alloc::{alloc, dealloc};
use alloc::sync::Arc;
use core::alloc::Layout;
use core::hint::spin_loop;
use core::mem::MaybeUninit;
use core::pin::Pin;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::sync::{Completion, SpinLock};
use crate::task::{self, State, Task};
use crate::time::Duration;
use crate::wait::Waiter;
use crate::workqueue::delayed::testing as delayed_testing;
use crate::workqueue::{
    testing, DelayedError, DelayedWork, QueueError, Work, WorkQueue, WorkQueueConfig,
};
use crate::{arch, cpu, mm, sched};

const SLOTS: usize = 8;
const DEPTH: usize = 3;
const PRESSURE_CAPACITY: usize = 65536;
static mut PRESSURE: [MaybeUninit<mm::OwnedPages>; PRESSURE_CAPACITY] =
    [const { MaybeUninit::uninit() }; PRESSURE_CAPACITY];
static mut OBJECTS: [MaybeUninit<(*mut u8, Layout)>; PRESSURE_CAPACITY] =
    [const { MaybeUninit::uninit() }; PRESSURE_CAPACITY];
static OPERATION_DROPS: AtomicUsize = AtomicUsize::new(0);
static ACCESS_AFTER_CLOSE: AtomicUsize = AtomicUsize::new(0);
static DELAY_SOURCE_CLOSED: AtomicBool = AtomicBool::new(false);

struct Entry {
    probe: usize,
    index: usize,
}

struct Probe {
    entries: [Entry; SLOTS],
    works: [SpinLock<Option<Pin<Arc<Work>>>>; SLOTS],
    tasks: [SpinLock<Option<Arc<Task>>>; SLOTS],
    gates: [Completion; SLOTS],
    counts: [AtomicUsize; SLOTS],
    active: AtomicUsize,
    maximum: AtomicUsize,
    submitted: AtomicUsize,
    consumed: AtomicUsize,
    callbacks: AtomicUsize,
    release: AtomicBool,
    source: AtomicBool,
    started: AtomicBool,
    done: AtomicBool,
    pages: AtomicUsize,
    objects: AtomicUsize,
    rescuer: AtomicUsize,
    ordinary: AtomicUsize,
    stops: AtomicUsize,
}

impl Probe {
    /// 全部观察、完成源和稳定上下文在发表任何异步票前一次预备。
    fn new() -> Arc<Self> {
        let raw = Arc::into_raw(Arc::new(Self {
            entries: core::array::from_fn(|index| Entry { probe: 0, index }),
            works: [const { SpinLock::new(None) }; SLOTS],
            tasks: [const { SpinLock::new(None) }; SLOTS],
            gates: [const { Completion::new() }; SLOTS],
            counts: [const { AtomicUsize::new(0) }; SLOTS],
            active: AtomicUsize::new(0),
            maximum: AtomicUsize::new(0),
            submitted: AtomicUsize::new(0),
            consumed: AtomicUsize::new(0),
            callbacks: AtomicUsize::new(0),
            release: AtomicBool::new(false),
            source: AtomicBool::new(true),
            started: AtomicBool::new(false),
            done: AtomicBool::new(false),
            pages: AtomicUsize::new(0),
            objects: AtomicUsize::new(0),
            rescuer: AtomicUsize::new(0),
            ordinary: AtomicUsize::new(0),
            stops: AtomicUsize::new(0),
        }));
        for index in 0..SLOTS {
            // SAFETY: raw 来自尚未发表的唯一 Arc::into_raw；没有导出共享借用，
            // 只在固定分配内初始化 Entry 的自引用，之后所有访问均为共享访问。
            unsafe {
                let entry = core::ptr::addr_of_mut!((*raw.cast_mut()).entries)
                    .cast::<Entry>()
                    .add(index);
                core::ptr::addr_of_mut!((*entry).probe).write(raw as usize);
            }
        }
        // SAFETY: 使用上面唯一且原样保存的 into_raw 指针恢复构造拥有，期间未释放或增加票。
        unsafe { Arc::from_raw(raw) }
    }

    /// 创建固定函数和工作域的实际 Work，测试表只克隆现有强引用。
    fn work(
        &self,
        queue: &WorkQueue,
        index: usize,
        owner: usize,
        callback: fn(Pin<&Work>),
    ) -> Pin<Arc<Work>> {
        let work = Work::try_new(
            queue,
            owner,
            callback,
            &self.entries[index] as *const Entry as usize,
        )
        .expect("work test allocation");
        assert!(self.works[index].lock().replace(work.clone()).is_none());
        work
    }

    /// 不分配地取得下一层已准备的真实工作。
    fn next_work(&self, index: usize) -> Pin<Arc<Work>> {
        self.works[index]
            .lock()
            .as_ref()
            .expect("prepared work")
            .clone()
    }

    /// 发布真实 callback Task 身份，观察锁不跨生产调度锁。
    fn publish_task(&self, index: usize) {
        let current = task::current_task();
        assert!(self.tasks[index].lock().replace(current).is_none());
    }

    /// 读取已发表 Task 的真实状态，不在观察锁内进入调度器。
    fn blocked(&self, index: usize) -> bool {
        let observed = self.tasks[index].lock().as_ref().map(Arc::clone);
        observed.is_some_and(|task| sched::test_state(&task) == State::Blocked)
    }

    /// 在所有回调与取消任务结束后移出观察拥有，最终析构均在短锁外。
    fn clear(&self) {
        for index in 0..SLOTS {
            let work = self.works[index].lock().take();
            let task = self.tasks[index].lock().take();
            drop((work, task));
        }
    }
}

impl Drop for Probe {
    /// 只观察稳定 operation 的一次析构，不在 Drop 再次同步关闭。
    fn drop(&mut self) {
        assert_eq!(self.active.load(Ordering::Acquire), 0);
        assert!(self.works.iter().all(|work| work.lock().is_none()));
        assert!(self.tasks.iter().all(|task| task.lock().is_none()));
        OPERATION_DROPS.fetch_add(1, Ordering::AcqRel);
    }
}

/// 从构造时已固定的 Entry 读取共享业务上下文，借用不能超过真实 callback 的同步尾部。
///
/// # Safety
/// address 必须来自仍存活的 Probe Arc 内 Entry；调用者必须保持该 Arc 到本借用结束，
/// 且直到所有使用该 Entry 的 Task 已 join、Work/DelayedWork 已同步关闭并归还异步强票。
/// Entry 发表后不得修改；返回值不能逃逸至 owner 停止后的任何访问。
unsafe fn context<'a>(address: usize) -> (&'a Probe, usize) {
    // SAFETY: 调用者保证 Entry 及其 Probe 的真实 Arc 分配在整个借用期间存活且只读。
    let entry = unsafe { &*(address as *const Entry) };
    // SAFETY: probe 是未发表构造阶段保存的原 Arc raw 地址，实际 owner 保活且不产生独占重借。
    (unsafe { &*(entry.probe as *const Probe) }, entry.index)
}

/// 以真实硬件计数建立一次总期限，不靠重试延长或周期扫描提供生产进度。
fn raw_deadline() -> u64 {
    arch::read_counter()
        .checked_add(arch::counter_frequency().checked_mul(10).unwrap())
        .unwrap()
}

/// 有限轮询仅协调测试参与者，实际阻塞和唤醒始终经过生产等待源。
fn until(mut predicate: impl FnMut() -> bool, name: &str) {
    let end = raw_deadline();
    while !predicate() {
        assert!(
            arch::read_counter() < end,
            "work execution timeout: {}",
            name
        );
        sched::yield_now();
    }
}

/// 远核 BH 停点不能睡眠；观察端只忙等已经武装的一次原子握手。
fn spin_until(mut predicate: impl FnMut() -> bool, name: &str) {
    let end = raw_deadline();
    while !predicate() {
        assert!(arch::read_counter() < end, "work pause timeout: {}", name);
        spin_loop();
    }
}

/// 真实常驻 worker、manager、timer 服务均已等待，动态 worker 已退出才允许取基线。
pub(super) fn wait_services_blocked() {
    testing::wait_services_blocked();
    super::timer::wait_services_blocked();
}

/// 基础服务真 Blocked 且退休者已释放后排空缓存，独立保留空闲页、PCP 与 SLUB 实账。
fn baseline() -> (mm::MemoryStats, testing::MetadataStats) {
    wait_services_blocked();
    mm::shrink_all();
    mm::drain_all_pages();
    (mm::stats(), testing::metadata_stats())
}

/// 所有临时源停止并归还强票后，完整页账与 Task/Worker/Domain 最后强析构必须分别收敛。
fn reclaimed(expected: (mm::MemoryStats, testing::MetadataStats)) -> usize {
    let actual = baseline();
    assert_eq!(
        actual.0, expected.0,
        "temporary work/task memory fields did not converge"
    );
    assert_eq!(
        actual.1, expected.1,
        "temporary task/worker/domain payloads did not converge"
    );
    1
}

/// 只有测试 owner 留下的真实强引用才允许结束上下文寿命。
fn unique_work(work: &Pin<Arc<Work>>) {
    // SAFETY: 克隆仅用于观察控制块，不移动 Work；原 Pin 和观察期间的强拥有保持节点稳定。
    let observer = unsafe { Pin::into_inner_unchecked(work.clone()) };
    until(
        || Arc::strong_count(&observer) == 2,
        "work asynchronous reference returned",
    );
    drop(observer);
}

/// 不清理 D 的状态就观察真实异步强票归还，不能用 close/cancel 掩盖到期孤票。
fn unique_delayed(work: &Pin<Arc<DelayedWork>>) -> usize {
    // SAFETY: 克隆 Arc 仅观察控制块，不移动 D；原 Pin 与 owner 在整个观察期间保活。
    let observer = unsafe { Pin::into_inner_unchecked(work.clone()) };
    until(
        || Arc::strong_count(&observer) == 2,
        "delayed asynchronous tickets returned",
    );
    let orphan = Arc::strong_count(&observer) - 2;
    assert_eq!(orphan, 0);
    orphan
}

/// 全域有序 callback 验证实际 CPU、FIFO 顺序与未结束 callback 的唯一准入。
fn ordered_callback(work: Pin<&Work>) {
    // SAFETY: ordered_case 持有 Probe Arc，逐项 close、归还强票并 join 提交者后才释放。
    let (probe, index) = unsafe { context(work.context()) };
    assert_eq!(cpu::current().id, 0);
    assert_eq!(work.cpu(), 0);
    let active = probe.active.fetch_add(1, Ordering::AcqRel) + 1;
    probe.maximum.fetch_max(active, Ordering::AcqRel);
    assert_eq!(active, 1);
    assert_eq!(probe.consumed.fetch_add(1, Ordering::AcqRel), index);
    sched::yield_now();
    assert_eq!(probe.active.fetch_sub(1, Ordering::AcqRel), 1);
}

/// 真实异 CPU Task 向同一 ordered 域提交，绑定规范化和全域 FIFO 都由回调断言。
fn ordered_case() -> (usize, usize) {
    let queue = WorkQueue::try_new(WorkQueueConfig {
        max_active: 4,
        ordered: true,
        reclaim: false,
    })
    .unwrap();
    let probe = Probe::new();
    let works: [Pin<Arc<Work>>; 4] = core::array::from_fn(|index| {
        probe.work(&queue, index, index % cpu::count(), ordered_callback)
    });
    let submissions = works.clone();
    let submit_cpu = cpu::count() - 1;
    let handle = task::spawn_kernel_on(submit_cpu, move || {
        assert_eq!(cpu::current().id, submit_cpu);
        for work in &submissions {
            assert!(work.as_ref().queue().unwrap());
        }
        4
    })
    .unwrap();
    assert_eq!(handle.join(), 4);
    queue.flush();
    assert_eq!(probe.consumed.load(Ordering::Acquire), 4);
    assert_eq!(probe.maximum.load(Ordering::Acquire), 1);
    for work in &works {
        work.as_ref().close_sync();
    }
    queue.close_sync();
    probe.clear();
    for work in &works {
        unique_work(work);
    }
    drop((works, queue, probe));
    (1, 1)
}

/// ordered A 先排别域 B 再排自身；第二轮真睡眠让 B 在另一执行者中同步等待 A。
fn ordered_association_callback(work: Pin<&Work>) {
    // SAFETY: ordered_association_case 保留 Probe Arc，全部回调完成、Work/domain 同步关闭及归票后才释放。
    let (probe, index) = unsafe { context(work.context()) };
    assert_eq!(cpu::current().id, 0);
    assert_eq!(work.cpu(), 0);
    assert!(!testing::is_rescuer());
    if index == 0 {
        let active = probe.active.fetch_add(1, Ordering::AcqRel) + 1;
        probe.maximum.fetch_max(active, Ordering::AcqRel);
        assert_eq!(active, 1);
        let round = probe.counts[0].fetch_add(1, Ordering::AcqRel);
        assert_eq!(probe.consumed.fetch_add(1, Ordering::AcqRel), round);
        if round == 0 {
            probe.publish_task(0);
            assert!(probe.source.load(Ordering::Acquire));
            assert!(probe.next_work(1).as_ref().queue().unwrap());
            assert!(work.queue().unwrap());
        } else {
            assert_eq!(round, 1);
            let original = probe.tasks[0].lock().as_ref().unwrap().id();
            assert_eq!(task::current_task_id(), Some(original));
            probe.gates[0].wait();
        }
        assert_eq!(probe.active.fetch_sub(1, Ordering::AcqRel), 1);
    } else {
        assert_eq!(index, 1);
        probe.publish_task(1);
        let original = probe.tasks[0].lock().as_ref().unwrap().id();
        assert_ne!(
            task::current_task_id(),
            Some(original),
            "ordered association handed its executor to cross-domain flush"
        );
        assert_eq!(probe.counts[1].fetch_add(1, Ordering::AcqRel), 0);
        assert!(probe.next_work(0).as_ref().flush());
        assert_eq!(probe.consumed.fetch_add(1, Ordering::AcqRel), 2);
        assert_eq!(probe.counts[1].fetch_add(1, Ordering::AcqRel), 1);
    }
}

/// 单核也使用真实两个域与阻塞调度，不能用异核停点代替 ordered 自关联进度。
fn ordered_association_case() {
    let initial = baseline();
    let ordered = WorkQueue::try_new(WorkQueueConfig {
        max_active: 4,
        ordered: true,
        reclaim: false,
    })
    .unwrap();
    let unrelated = WorkQueue::try_new(WorkQueueConfig {
        max_active: 1,
        ordered: false,
        reclaim: false,
    })
    .unwrap();
    let probe = Probe::new();
    let a = probe.work(&ordered, 0, 0, ordered_association_callback);
    let b = probe.work(&unrelated, 1, 0, ordered_association_callback);
    assert!(a.as_ref().queue().unwrap());
    until(
        || probe.blocked(0) && probe.blocked(1),
        "ordered next round and cross-domain flush truly blocked",
    );
    assert_eq!(probe.counts[0].load(Ordering::Acquire), 2);
    assert_eq!(probe.counts[1].load(Ordering::Acquire), 1);
    assert_eq!(probe.consumed.load(Ordering::Acquire), 2);
    assert_eq!(probe.maximum.load(Ordering::Acquire), 1);
    let a_state = testing::snapshot(a.as_ref());
    let b_state = testing::snapshot(b.as_ref());
    assert!(a_state.running && !a_state.queued);
    assert!(b_state.running && !b_state.queued);
    assert_ne!(a_state.worker, b_state.worker);
    probe.source.store(false, Ordering::Release);
    probe.gates[0].complete();
    until(
        || probe.counts[1].load(Ordering::Acquire) == 2,
        "ordered next round completed and legal cross-domain flush returned",
    );
    ordered.flush();
    unrelated.flush();
    assert_eq!(probe.consumed.load(Ordering::Acquire), 3);
    assert_eq!(probe.active.load(Ordering::Acquire), 0);
    a.as_ref().close_sync();
    b.as_ref().close_sync();
    ordered.close_sync();
    unrelated.close_sync();
    probe.clear();
    unique_work(&a);
    unique_work(&b);
    drop((a, b, ordered, unrelated, probe));
    assert_eq!(reclaimed(initial), 1);
}

/// 业务计数独立于 pending；第一轮可睡，下一轮与 running 同存但绝不重入。
fn duplicate_callback(work: Pin<&Work>) {
    // SAFETY: test_duplicates 持有 Probe Arc 到 gate 放行、Work 同步关闭及异步强票归还。
    let (probe, _) = unsafe { context(work.context()) };
    let active = probe.active.fetch_add(1, Ordering::AcqRel) + 1;
    probe.maximum.fetch_max(active, Ordering::AcqRel);
    assert_eq!(active, 1);
    let round = probe.callbacks.fetch_add(1, Ordering::AcqRel);
    let submitted = probe.submitted.load(Ordering::Acquire);
    let consumed = probe.consumed.swap(submitted, Ordering::AcqRel);
    assert!(consumed < submitted);
    if round == 0 {
        probe.publish_task(0);
        probe.gates[0].wait();
    }
    assert_eq!(probe.active.fetch_sub(1, Ordering::AcqRel), 1);
}

/// claim 清 pending 后停点释放，实际业务再次核验同一 Work 的串行下一轮。
fn started_callback(work: Pin<&Work>) {
    // SAFETY: test_duplicates 在解除真实停点、同步关闭 started 并归还其强票后才释放 Probe Arc。
    let (probe, index) = unsafe { context(work.context()) };
    let active = probe.active.fetch_add(1, Ordering::AcqRel) + 1;
    assert_eq!(active, 1);
    probe.maximum.fetch_max(active, Ordering::AcqRel);
    probe.counts[index].fetch_add(1, Ordering::AcqRel);
    assert_eq!(probe.active.fetch_sub(1, Ordering::AcqRel), 1);
}

/// X18 覆盖运行前及可睡运行中重复提交、真实业务合并和跨 CPU ordered FIFO。
fn test_duplicates() {
    let initial = baseline();
    let queue = WorkQueue::try_new(WorkQueueConfig {
        max_active: 4,
        ordered: false,
        reclaim: false,
    })
    .unwrap();
    let probe = Probe::new();
    let work = probe.work(&queue, 0, 0, duplicate_callback);
    let started = if 1 < cpu::count() {
        Some(probe.work(&queue, 1, 1, started_callback))
    } else {
        None
    };
    probe.submitted.store(8, Ordering::Release);
    assert!(work.as_ref().queue().unwrap());
    let mut before_duplicates = 0;
    for _ in 0..3 {
        before_duplicates += usize::from(!work.as_ref().queue().unwrap());
    }
    assert_eq!(before_duplicates, 3);
    until(|| probe.blocked(0), "first duplicate callback blocked");
    probe.submitted.fetch_add(8, Ordering::Release);
    assert!(work.as_ref().queue().unwrap());
    let state = testing::snapshot(work.as_ref());
    let overlap = usize::from(state.running && state.queued);
    assert_eq!(overlap, 1);
    {
        let observed = Arc::clone(probe.tasks[0].lock().as_ref().unwrap());
        assert_eq!(state.worker, observed.id().as_usize());
    }
    let mut running_duplicates = 0;
    for _ in 0..3 {
        running_duplicates += usize::from(!work.as_ref().queue().unwrap());
    }
    assert_eq!(running_duplicates, 3);
    probe.gates[0].complete();
    work.as_ref().flush();
    queue.flush();
    let submitted = probe.submitted.load(Ordering::Acquire);
    let consumed = probe.consumed.load(Ordering::Acquire);
    let callbacks = probe.callbacks.load(Ordering::Acquire);
    let maximum = probe.maximum.load(Ordering::Acquire);
    assert_eq!((submitted, consumed, callbacks, maximum), (16, 16, 2, 1));
    if let Some(started) = &started {
        testing::arm_pause(1, testing::PausePoint::Started);
        assert!(started.as_ref().queue().unwrap());
        spin_until(
            || testing::is_paused(1, testing::PausePoint::Started),
            "real claim cleared pending",
        );
        let state = testing::snapshot(started.as_ref());
        assert!(state.running && !state.queued && state.worker != 0);
        assert!(started.as_ref().queue().unwrap());
        assert!(!started.as_ref().queue().unwrap());
        let state = testing::snapshot(started.as_ref());
        assert!(state.running && state.queued);
        testing::release_pause(1, testing::PausePoint::Started);
        started.as_ref().flush();
        assert_eq!(probe.counts[1].load(Ordering::Acquire), 2);
        started.as_ref().close_sync();
    }
    work.as_ref().close_sync();
    queue.close_sync();
    probe.clear();
    unique_work(&work);
    if let Some(started) = &started {
        unique_work(started);
    }
    drop((work, started, queue, probe));
    let (ordered, ordered_fifo) = ordered_case();
    let recovered = reclaimed(initial);
    crate::println!("PASS execution-X18 before_duplicates={} running_duplicates={} submitted={} consumed={} callbacks={} max_active={} overlap={} ordered={} ordered_fifo={} reclaimed={}", before_duplicates, running_duplicates, submitted, consumed, callbacks, maximum, overlap, ordered, ordered_fifo, recovered);
}

/// 基础执行者与同域占位者真睡眠；关联票只在第一轮 callback 内 selfqueue。
fn association_callback(work: Pin<&Work>) {
    // SAFETY: association_case 持有 Probe Arc，控制者 join、全部 Work/domain 关闭及归票后才释放。
    let (probe, index) = unsafe { context(work.context()) };
    assert_eq!(cpu::current().id, work.cpu());
    assert!(!testing::is_rescuer());
    match index {
        0 | 2 => {
            probe.publish_task(index);
            probe.gates[index].wait();
        }
        1 => {
            let round = probe.counts[1].fetch_add(1, Ordering::AcqRel);
            if round == 0 {
                probe.publish_task(1);
                assert!(work.queue().unwrap());
            } else {
                assert_eq!(round, 1);
                let original = probe.tasks[1].lock().as_ref().unwrap().id();
                assert_eq!(task::current_task_id(), Some(original));
            }
        }
        3 => {
            probe.publish_task(3);
            let original = probe.tasks[1].lock().as_ref().unwrap().id();
            assert_ne!(
                task::current_task_id(),
                Some(original),
                "unrelated flush callback stole the associated executor"
            );
            probe.counts[3].store(1, Ordering::Release);
            assert!(probe.next_work(1).as_ref().flush());
            probe.counts[3].store(2, Ordering::Release);
        }
        _ => unreachable!(),
    }
}

/// 同一真实关联交错分别覆盖跨域同步、登记后取消和首次退休检查后的登记前取消。
enum AssociationAction {
    Flush,
    CancelBlocked,
    CancelBeforeWait,
}

/// 独立 CPU1 控制者协调 CPU0 完成停点，不用测试轮询替生产 worker 提供进度。
fn association_case(action: AssociationAction) {
    assert!(1 < cpu::count());
    let (cancel, before_wait) = match action {
        AssociationAction::Flush => (false, false),
        AssociationAction::CancelBlocked => (true, false),
        AssociationAction::CancelBeforeWait => (true, true),
    };
    let initial = baseline();
    let before = testing::pool_stats(0);
    assert_eq!(before.workers, 1);
    let domain = WorkQueue::try_new(WorkQueueConfig {
        max_active: 1,
        ordered: false,
        reclaim: false,
    })
    .unwrap();
    let unrelated = WorkQueue::try_new(WorkQueueConfig {
        max_active: 2,
        ordered: false,
        reclaim: false,
    })
    .unwrap();
    let probe = Probe::new();
    let works: [Pin<Arc<Work>>; 4] = core::array::from_fn(|index| {
        probe.work(
            if index == 1 || index == 2 {
                &domain
            } else {
                &unrelated
            },
            index,
            usize::from(index == 2),
            association_callback,
        )
    });
    let controller_probe = probe.clone();
    let controller = task::prepare_kernel_on(1, move || {
        let probe = controller_probe;
        assert_eq!(cpu::current().id, 1);
        probe.gates[4].wait();
        testing::arm_pause(0, testing::PausePoint::Finished);
        assert!(probe.next_work(1).as_ref().queue().unwrap());
        until(
            || testing::is_paused(0, testing::PausePoint::Finished),
            "associated callback finished before next claim",
        );
        let work = probe.next_work(1);
        let state = testing::snapshot(work.as_ref());
        assert!(state.queued && !state.running);
        assert_eq!(probe.counts[1].load(Ordering::Acquire), 1);
        let original = probe.tasks[1].lock().as_ref().unwrap().clone();
        let base = probe.tasks[0].lock().as_ref().unwrap().id();
        assert_ne!(original.id(), base);
        assert!(original.worker.is_some());
        let paused = testing::pool_stats(0);
        assert_eq!(paused.workers, before.workers + 1);
        assert_eq!(paused.created, before.created + 1);
        assert!(probe.next_work(2).as_ref().queue().unwrap());
        until(
            || probe.blocked(2),
            "remote same-domain active slot blocked",
        );
        assert!(testing::snapshot(probe.next_work(2).as_ref()).running);
        if !cancel {
            assert!(probe.next_work(3).as_ref().queue().unwrap());
        }
        if before_wait {
            testing::arm_pause(0, testing::PausePoint::BeforeWait);
        }
        testing::release_pause(0, testing::PausePoint::Finished);
        if before_wait {
            until(
                || testing::is_paused(0, testing::PausePoint::BeforeWait),
                "dynamic executor passed first retirement check before ready registration",
            );
            assert!(!probe.blocked(1));
        } else {
            until(
                || probe.blocked(1),
                "original executor blocked with deferred associated ticket",
            );
        }
        assert!(probe.blocked(0) && probe.blocked(2));
        assert_eq!(testing::pool_stats(0).retired, before.retired);
        assert!(testing::snapshot(work.as_ref()).queued);
        assert!(!testing::snapshot(work.as_ref()).running);
        if cancel {
            assert!(work.as_ref().cancel_sync());
            let state = testing::snapshot(work.as_ref());
            assert!(!state.queued && !state.running);
            assert!(probe.blocked(0) && probe.blocked(2));
            assert_eq!(probe.counts[3].load(Ordering::Acquire), 0);
            if before_wait {
                assert!(testing::is_paused(0, testing::PausePoint::BeforeWait));
                assert_eq!(testing::pool_stats(0).retired, before.retired);
                assert_eq!(testing::pool_stats(0).workers, before.workers + 1);
                testing::release_pause(0, testing::PausePoint::BeforeWait);
            }
            until(
                || {
                    sched::test_state(&original) == State::Exited
                        && testing::pool_stats(0).retired == before.retired + 1
                        && testing::pool_stats(0).workers == before.workers
                },
                "cancelled last association retired and manager joined without later work",
            );
            assert_eq!(sched::test_state(&original), State::Exited);
            assert_eq!(testing::pool_stats(0).workers, before.workers);
            assert_eq!(probe.counts[1].load(Ordering::Acquire), 1);
            assert_eq!(probe.counts[3].load(Ordering::Acquire), 0);
            assert!(probe.blocked(0) && probe.blocked(2));
            assert!(testing::snapshot(probe.next_work(0).as_ref()).running);
            assert!(testing::snapshot(probe.next_work(2).as_ref()).running);
        } else {
            until(
                || probe.blocked(3),
                "different-domain callback really blocked in associated flush",
            );
            assert_eq!(probe.counts[3].load(Ordering::Acquire), 1);
            let flusher = probe.tasks[3].lock().as_ref().unwrap().id();
            assert_ne!(flusher, original.id());
            assert!(probe.blocked(0) && probe.blocked(2));
        }
        probe.gates[2].complete();
        if !cancel {
            until(
                || probe.counts[3].load(Ordering::Acquire) == 2,
                "original executor continued and cross-domain flush returned",
            );
            assert_eq!(probe.counts[1].load(Ordering::Acquire), 2);
        }
        probe.gates[0].complete();
        0
    })
    .unwrap();
    let controller = controller.publish();
    assert!(works[0].as_ref().queue().unwrap());
    until(
        || probe.blocked(0),
        "base executor held before dynamic creation",
    );
    probe.gates[4].complete();
    assert_eq!(controller.join(), 0);
    for work in &works {
        work.as_ref().close_sync();
    }
    domain.close_sync();
    unrelated.close_sync();
    probe.clear();
    for work in &works {
        unique_work(work);
    }
    drop((works, domain, unrelated, probe));
    assert_eq!(reclaimed(initial), 1);
}

/// 三层真正阻塞依赖交给动态 worker，叶子仅等待测试解除后从最深层逐级唤醒。
fn chain_callback(work: Pin<&Work>) {
    // SAFETY: test_deep_blocking 保持 Probe Arc，所有依赖放行并逐项 close、归票后才释放上下文。
    let (probe, index) = unsafe { context(work.context()) };
    probe.publish_task(index);
    if index == 0 {
        let waiter = Waiter::new();
        assert!(waiter.waker().notify());
        waiter.park();
    }
    if index < DEPTH {
        assert!(probe.next_work(index + 1).as_ref().queue().unwrap());
        probe.gates[index].wait();
        probe.consumed.fetch_add(1, Ordering::AcqRel);
        if 0 < index {
            probe.gates[index - 1].complete();
        }
    } else {
        until(
            || probe.release.load(Ordering::Acquire),
            "deep chain release",
        );
        probe.gates[index - 1].complete();
    }
}

/// X19 在三层依赖同时 Blocked 时观察真实创建、早通知配对和动态退休。
fn test_deep_blocking() {
    let initial = baseline();
    let before = testing::pool_stats(0);
    let queue = WorkQueue::try_new(WorkQueueConfig {
        max_active: DEPTH + 1,
        ordered: false,
        reclaim: false,
    })
    .unwrap();
    let probe = Probe::new();
    let works: [Pin<Arc<Work>>; DEPTH + 1] =
        core::array::from_fn(|index| probe.work(&queue, index, 0, chain_callback));
    assert!(works[0].as_ref().queue().unwrap());
    until(
        || (0..DEPTH).all(|index| probe.blocked(index)) && probe.tasks[DEPTH].lock().is_some(),
        "three blocked worker dependencies",
    );
    let blocked = (0..DEPTH).filter(|&index| probe.blocked(index)).count();
    let creators = testing::pool_stats(0).created - before.created;
    assert_eq!(blocked, DEPTH);
    assert!(DEPTH - 1 <= creators);
    probe.release.store(true, Ordering::Release);
    queue.flush();
    let released = probe.consumed.load(Ordering::Acquire);
    assert_eq!(released, DEPTH);
    for work in &works {
        work.as_ref().close_sync();
    }
    queue.close_sync();
    probe.clear();
    for work in &works {
        unique_work(work);
    }
    drop((works, queue, probe));
    wait_services_blocked();
    let after = testing::pool_stats(0);
    let sleeping = after.sleeping - before.sleeping;
    let running = after.running - before.running;
    let paired = usize::from(sleeping == running && DEPTH < sleeping && after.nr_running == 0);
    let early_wake = after.early_wake - before.early_wake;
    let retired = after.retired - before.retired;
    assert_eq!(paired, 1);
    assert!(1 <= early_wake && 1 <= retired);
    if 1 < cpu::count() {
        association_case(AssociationAction::CancelBlocked);
        association_case(AssociationAction::CancelBeforeWait);
    }
    let recovered = reclaimed(initial);
    crate::println!("PASS execution-X19 depth={} blocked={} creators={} released={} paired={} early_wake={} retired={} reclaimed={}", DEPTH, blocked, creators, released, paired, early_wake, retired, recovered);
}

/// 用现有唯一页入口从大块到单页真正摘尽 RAM；容量耗尽是测试错误而非 OOM。
fn exhaust_pages() -> usize {
    mm::drain_all_pages();
    let mut count = 0;
    for order in (0..usize::BITS as usize).rev() {
        loop {
            assert!(count < PRESSURE_CAPACITY, "work pressure page storage full");
            let Some(pages) = mm::alloc_pages(order, false) else {
                break;
            };
            // SAFETY: 压力构造仅由 CPU0 协调者执行；count 未初始化且在界内，移动保存唯一页拥有权。
            unsafe {
                core::ptr::addr_of_mut!(PRESSURE)
                    .cast::<MaybeUninit<mm::OwnedPages>>()
                    .add(count)
                    .write(MaybeUninit::new(pages));
            }
            count += 1;
        }
    }
    assert!(0 < count);
    assert!(mm::alloc_pages(0, false).is_none());
    count
}

/// 页已摘尽后继续真实消费每个生产 SLUB 类的残余对象，直到 GlobalAlloc 返回 null。
fn exhaust_objects() -> (usize, usize) {
    let mut count = 0;
    let mut nulls = 0;
    for size in mm::SLUB_SIZES {
        let layout = Layout::from_size_align(size, 8).unwrap();
        loop {
            assert!(
                count < PRESSURE_CAPACITY,
                "work pressure object storage full"
            );
            // SAFETY: layout 有效且 MM 已初始化；成功分配的唯一归还权立即保存到压力表。
            let pointer = unsafe { alloc(layout) };
            if pointer.is_null() {
                nulls += 1;
                break;
            }
            // SAFETY: 协调者独占尚未初始化的 count 槽，保存原 live 指针及原 Layout。
            unsafe {
                core::ptr::addr_of_mut!(OBJECTS)
                    .cast::<MaybeUninit<(*mut u8, Layout)>>()
                    .add(count)
                    .write(MaybeUninit::new((pointer, layout)));
            }
            count += 1;
        }
    }
    assert_eq!(nulls, mm::SLUB_SIZES.len());
    (count, nulls)
}

/// 已发布压力前缀只由实际 rescuer callback 消费一次；归还全部发生在池锁和 MM 锁外。
fn release_pressure(page_count: usize, object_count: usize) {
    for index in (0..object_count).rev() {
        // SAFETY: 两个完整前缀由协调者初始化后 Release 发表给唯一回收 callback；
        // 协调者已结束压力表访问，每槽在此移动一次，未发生过归还。
        let (pointer, layout) = unsafe {
            core::ptr::addr_of_mut!(OBJECTS)
                .cast::<MaybeUninit<(*mut u8, Layout)>>()
                .add(index)
                .read()
                .assume_init()
        };
        // SAFETY: 刚取得原分配的唯一归还权，pointer/layout 完全匹配，压力对象没有其他访问者。
        unsafe { dealloc(pointer, layout) };
    }
    for index in (0..page_count).rev() {
        // SAFETY: 已初始化原前缀的唯一页拥有者只在此移动消费，callback 外没有并发压力表访问。
        let pages = unsafe {
            core::ptr::addr_of_mut!(PRESSURE)
                .cast::<MaybeUninit<mm::OwnedPages>>()
                .add(index)
                .read()
                .assume_init()
        };
        drop(pages);
    }
}

/// 保持基础普通 worker 真正 Blocked，不能由它执行下一项受控解压工作。
fn pressure_blocker(work: Pin<&Work>) {
    // SAFETY: test_reclaim 在解除 blocker、同步关闭两项 Work 与 rescuer join/归票前保留 Probe Arc。
    let (probe, _) = unsafe { context(work.context()) };
    assert!(!testing::is_rescuer());
    probe.publish_task(0);
    probe.gates[0].wait();
}

/// 唯一受控回收 work 核验实际 worker 身份，再归还先前准备的真实压力资源。
fn reclaim_callback(work: Pin<&Work>) {
    // SAFETY: test_reclaim 在真实压力释放回调完成、rescuer join 且 recovery 归票前保留 Probe Arc。
    let (probe, _) = unsafe { context(work.context()) };
    if testing::is_rescuer() {
        probe.rescuer.fetch_add(1, Ordering::AcqRel);
    } else {
        probe.ordinary.fetch_add(1, Ordering::AcqRel);
    }
    assert!(
        testing::is_rescuer(),
        "ordinary worker bypassed real OOM rescue"
    );
    let pages = probe.pages.load(Ordering::Acquire);
    let objects = probe.objects.load(Ordering::Acquire);
    assert!(0 < pages);
    release_pressure(pages, objects);
    probe.done.store(true, Ordering::Release);
}

/// X20 对象、工作与预留 rescuer 全部先准备，真实 OOM 中只提交现有节点并观察生产创建失败。
fn test_reclaim() {
    let initial = baseline();
    let queue = WorkQueue::try_new(WorkQueueConfig {
        max_active: 4,
        ordered: false,
        reclaim: true,
    })
    .unwrap();
    let probe = Probe::new();
    let blocker = probe.work(&queue, 0, 0, pressure_blocker);
    let recovery = probe.work(&queue, 1, 0, reclaim_callback);
    assert!(blocker.as_ref().queue().unwrap());
    until(|| probe.blocked(0), "ordinary worker blocked before OOM");
    let before = testing::pool_stats(0);
    assert_eq!(before.workers, 1);
    assert_eq!(
        before.idle, 0,
        "ordinary idle worker could bypass OOM rescue"
    );
    mm::shrink_all();
    let page_count = exhaust_pages();
    let (objects, heap_null) = exhaust_objects();
    let page_null = usize::from(mm::alloc_pages(0, false).is_none());
    assert_eq!(page_null, 1);
    probe.pages.store(page_count, Ordering::Release);
    probe.objects.store(objects, Ordering::Release);
    assert!(recovery.as_ref().queue().unwrap());
    until(
        || probe.done.load(Ordering::Acquire),
        "reserved rescuer real OOM progress",
    );
    recovery.as_ref().flush();
    let after = testing::pool_stats(0);
    let create_failures = after.create_failures - before.create_failures;
    let rescuer_callbacks = probe.rescuer.load(Ordering::Acquire);
    let worker_callbacks = probe.ordinary.load(Ordering::Acquire);
    assert!(1 <= create_failures && 1 <= after.rescued - before.rescued);
    assert_eq!((rescuer_callbacks, worker_callbacks), (1, 0));
    let recovered_page = mm::alloc_pages(0, false).expect("page allocation did not recover");
    drop(recovered_page);
    let mut heap_recovered = 0;
    for size in mm::SLUB_SIZES {
        let layout = Layout::from_size_align(size, 8).unwrap();
        // SAFETY: 有效布局由唯一协调者使用，刚释放的压力应让每个生产类重新成功分配。
        let pointer = unsafe { alloc(layout) };
        assert!(!pointer.is_null(), "heap class did not recover");
        heap_recovered += usize::from(!pointer.is_null());
        // SAFETY: pointer 是本轮同 layout 成功分配的原地址，唯一协调者未交给任何其他访问者。
        unsafe { dealloc(pointer, layout) };
    }
    let recovered = usize::from(heap_recovered == mm::SLUB_SIZES.len());
    assert_eq!(recovered, 1);
    probe.gates[0].complete();
    queue.flush();
    blocker.as_ref().close_sync();
    recovery.as_ref().close_sync();
    queue.close_sync();
    probe.clear();
    unique_work(&blocker);
    unique_work(&recovery);
    drop((blocker, recovery, queue, probe));
    let settled = reclaimed(initial);
    crate::println!("PASS execution-X20 page_null={} heap_null={} create_failures={} rescuer_callbacks={} worker_callbacks={} recovered={} reclaimed={}", page_null, heap_null, create_failures, rescuer_callbacks, worker_callbacks, recovered, settled);
}

/// 较早 work 在指定真实 CPU 阻塞，较晚项完成后，未来项继续独立占据 pending/running。
fn boundary_callback(work: Pin<&Work>) {
    // SAFETY: test_flush_boundary 先切生产源、解除全部 gate、逐项 close 并归票后才释放 Probe Arc。
    let (probe, index) = unsafe { context(work.context()) };
    assert_eq!(cpu::current().id, work.cpu());
    if index == 0 {
        probe.publish_task(0);
        probe.gates[0].wait();
        probe.counts[0].fetch_add(1, Ordering::Release);
    } else if index == 1 {
        probe.counts[1].fetch_add(1, Ordering::Release);
    } else if index == 2 && probe.counts[6].load(Ordering::Acquire) < 4 {
        assert!(probe.source.load(Ordering::Acquire));
        assert!(work.queue().unwrap());
        probe.counts[6].fetch_add(1, Ordering::AcqRel);
    } else {
        probe.publish_task(index);
        probe.gates[index].wait();
        probe.counts[index].fetch_add(1, Ordering::Release);
    }
}

/// X21 等待旧颜色而非最大完成号；持续有限未来队列不能延长已经切定的 flush 边界。
fn test_flush_boundary() {
    let initial = baseline();
    let queue = WorkQueue::try_new(WorkQueueConfig {
        max_active: SLOTS,
        ordered: false,
        reclaim: false,
    })
    .unwrap();
    let probe = Probe::new();
    let early_cpu = if cpu::count() == 1 { 0 } else { 1 };
    let works: [Pin<Arc<Work>>; 6] = core::array::from_fn(|index| {
        probe.work(
            &queue,
            index,
            if index == 0 { early_cpu } else { 0 },
            boundary_callback,
        )
    });
    assert!(works[0].as_ref().queue().unwrap());
    until(|| probe.blocked(0), "earlier work sleeping on bound CPU");
    let earlier_blocked = usize::from(probe.blocked(0));
    let remote = usize::from(works[0].cpu() != cpu::current().id);
    assert_eq!(remote, usize::from(1 < cpu::count()));
    assert!(works[1].as_ref().queue().unwrap());
    until(
        || probe.counts[1].load(Ordering::Acquire) == 1,
        "later work completed",
    );
    let later_done = probe.counts[1].load(Ordering::Acquire);
    let flush_probe = probe.clone();
    let queue_address = &queue as *const WorkQueue as usize;
    let flusher = task::spawn_kernel_on(0, move || {
        let probe = flush_probe;
        probe.publish_task(7);
        probe.started.store(true, Ordering::Release);
        // SAFETY: 唯一协调者在 flusher 真正退出前保持 queue 原址，不移动或关闭；此借用只调用共享 flush。
        let queue = unsafe { &*(queue_address as *const WorkQueue) };
        queue.flush();
        probe.done.store(true, Ordering::Release);
        21
    })
    .unwrap();
    until(
        || probe.started.load(Ordering::Acquire) && probe.blocked(7),
        "flush old boundary actually blocked",
    );
    assert!(!probe.done.load(Ordering::Acquire));
    assert_eq!(probe.counts[0].load(Ordering::Acquire), 0);
    let mut future_submitted = 0;
    for work in &works[2..] {
        future_submitted += usize::from(work.as_ref().queue().unwrap());
    }
    assert_eq!(future_submitted, 4);
    until(
        || probe.counts[6].load(Ordering::Acquire) == 4,
        "finite future callback production",
    );
    future_submitted += probe.counts[6].load(Ordering::Acquire);
    probe.source.store(false, Ordering::Release);
    until(
        || {
            works[2..].iter().all(|work| {
                let state = testing::snapshot(work.as_ref());
                state.queued || state.running
            })
        },
        "future accepted work retained",
    );
    probe.gates[0].complete();
    until(
        || probe.done.load(Ordering::Acquire),
        "finite old color flush returned",
    );
    assert_eq!(flusher.join(), 21);
    let future_running = works[2..]
        .iter()
        .filter(|work| {
            let state = testing::snapshot(work.as_ref());
            state.running || state.queued
        })
        .count();
    let boundary_done = usize::from(
        probe.counts[0].load(Ordering::Acquire) == 1 && probe.done.load(Ordering::Acquire),
    );
    assert_eq!(boundary_done, 1);
    assert!(1 <= future_running);
    for index in 2..6 {
        probe.gates[index].complete();
    }
    queue.flush();
    assert!((0..6).all(|index| probe.counts[index].load(Ordering::Acquire) == 1));
    for work in &works {
        work.as_ref().close_sync();
    }
    queue.close_sync();
    probe.clear();
    for work in &works {
        unique_work(work);
    }
    drop((works, queue, probe));
    let (ordered, fifo) = ordered_case();
    assert_eq!(fifo, 1);
    if 1 < cpu::count() {
        association_case(AssociationAction::Flush);
    }
    ordered_association_case();
    let settled = reclaimed(initial);
    crate::println!("PASS execution-X21 remote={} earlier_blocked={} later_done={} future_submitted={} boundary_done={} future_running={} ordered={} reclaimed={}", remote, earlier_blocked, later_done, future_submitted, boundary_done, future_running, ordered, settled);
}

/// 业务回调只能在绑定 worker 上执行；关闭哨兵先于 context 解引用，记录真实迟到访问。
fn delayed_callback(work: Pin<&DelayedWork>) {
    if DELAY_SOURCE_CLOSED.load(Ordering::Acquire) {
        ACCESS_AFTER_CLOSE.fetch_add(1, Ordering::AcqRel);
        return;
    }
    // SAFETY: test_delayed 对每个 D 完成显式 close 与真实异步归票后才释放同一个 Probe Arc。
    let (probe, _) = unsafe { context(work.context()) };
    assert_eq!(cpu::current().id, work.cpu());
    assert_eq!(cpu::current().bh_depth.load(Ordering::Relaxed), 0);
    let active = probe.active.fetch_add(1, Ordering::AcqRel) + 1;
    probe.maximum.fetch_max(active, Ordering::AcqRel);
    assert_eq!(active, 1);
    probe.callbacks.fetch_add(1, Ordering::Release);
    assert_eq!(probe.active.fetch_sub(1, Ordering::AcqRel), 1);
}

/// 远 CPU 的真实 timer 移交停点使 cancel Task 必须 Blocked，不能提前结束 operation 存储寿命。
fn paused_delayed_stop(
    work: &Pin<Arc<DelayedWork>>,
    probe: &Arc<Probe>,
    point: delayed_testing::PausePoint,
) -> (usize, usize) {
    assert!(1 < cpu::count());
    probe.started.store(false, Ordering::Relaxed);
    probe.done.store(false, Ordering::Relaxed);
    let stop_work = work.clone();
    let stop_probe = probe.clone();
    let handle = task::spawn_kernel_on(0, move || {
        let probe = stop_probe;
        probe.publish_task(7);
        probe.gates[7].wait();
        probe.started.store(true, Ordering::Release);
        stop_work.as_ref().cancel_sync();
        let state = delayed_testing::snapshot(stop_work.as_ref());
        assert!(!state.running && !state.timer_pending && !state.work_pending);
        probe.done.store(true, Ordering::Release);
        22
    })
    .unwrap();
    until(|| probe.blocked(7), "delayed stopper prepared");
    delayed_testing::configure(work.as_ref(), point);
    assert!(work.as_ref().queue(Duration::from_millis(10)).unwrap());
    spin_until(
        delayed_testing::arrived,
        "real timer to pool transfer pause",
    );
    let inflight = usize::from(delayed_testing::arrived());
    probe.gates[7].complete();
    until(
        || probe.started.load(Ordering::Acquire) && probe.blocked(7),
        "delayed sync cancel waiting real timer tail",
    );
    let sync_wait = usize::from(probe.blocked(7));
    assert!(!probe.done.load(Ordering::Acquire));
    assert!(delayed_testing::arrived());
    delayed_testing::release();
    until(
        || handle.try_join().is_some(),
        "delayed cancel tail returned",
    );
    assert_eq!(handle.join(), 22);
    assert!(probe.done.load(Ordering::Acquire));
    let task = probe.tasks[7].lock().take().unwrap();
    assert_eq!(sched::test_state(&task), State::Exited);
    assert_eq!(Arc::strong_count(&task), 1);
    drop(task);
    assert_eq!((sync_wait, inflight), (1, 1));
    (sync_wait, inflight)
}

/// 域关闭不能代关 timer：真实到期被拒后须自行撤 pending 并归还 D 票，之后才允许清理 D。
fn rejected_delayed_transfer(
    queue: &WorkQueue,
    work: &Pin<Arc<DelayedWork>>,
    probe: &Probe,
) -> usize {
    let callbacks = probe.callbacks.load(Ordering::Acquire);
    if 1 < cpu::count() {
        delayed_testing::configure(work.as_ref(), delayed_testing::PausePoint::BeforeTransfer);
        assert!(work.as_ref().queue(Duration::from_millis(10)).unwrap());
        spin_until(delayed_testing::arrived, "closed domain due timer paused");
        let pending = delayed_testing::snapshot(work.as_ref());
        assert!(pending.timer_pending && !pending.work_pending && !pending.running);
        queue.close_sync();
        assert!(delayed_testing::arrived());
        delayed_testing::release();
    } else {
        assert!(work.as_ref().queue(Duration::from_secs(1)).unwrap());
        assert!(delayed_testing::snapshot(work.as_ref()).timer_pending);
        queue.close_sync();
        // 正式睡眠只让真实粗 timer 有执行机会；完成仍由下方 pending 与真实强票尾部共同证明。
        crate::timer::sleep(Duration::from_secs(1));
    }
    until(
        || {
            let state = delayed_testing::snapshot(work.as_ref());
            !state.timer_pending && !state.work_pending && !state.running
        },
        "closed domain rejected due timer removed pending",
    );
    let orphan = unique_delayed(work);
    // 真实服务重新 Blocked 证明粗 timer 的生产尾部已完成，不能用 D 清理操作替它收尾。
    super::timer::wait_services_blocked();
    assert_eq!(probe.callbacks.load(Ordering::Acquire), callbacks);
    // 上述断言全部先于任何 D 同步操作；旧拒绝分支遗留的 pending/强票会在此之前失败。
    work.as_ref().close_sync();
    assert!(delayed_testing::snapshot(work.as_ref()).closed);
    orphan
}

/// X22 覆盖延迟修改、真实移交前后同步尾部，以及 operation 存储结束后没有访问或孤票。
fn test_delayed() {
    let initial = baseline();
    let drops_before = OPERATION_DROPS.load(Ordering::Acquire);
    DELAY_SOURCE_CLOSED.store(false, Ordering::Release);
    let queue = WorkQueue::try_new(WorkQueueConfig {
        max_active: 4,
        ordered: false,
        reclaim: false,
    })
    .unwrap();
    let probe = Probe::new();
    let owner = if cpu::count() == 1 { 0 } else { 1 };
    let work = DelayedWork::try_new(
        &queue,
        owner,
        delayed_callback,
        &probe.entries[0] as *const Entry as usize,
    )
    .unwrap();
    let rejected_queue = WorkQueue::try_new(WorkQueueConfig {
        max_active: 1,
        ordered: false,
        reclaim: false,
    })
    .unwrap();
    let rejected_work = DelayedWork::try_new(
        &rejected_queue,
        owner,
        delayed_callback,
        &probe.entries[1] as *const Entry as usize,
    )
    .unwrap();
    let mut delayed = usize::from(work.as_ref().queue(Duration::from_secs(1)).unwrap());
    assert!(delayed_testing::snapshot(work.as_ref()).timer_pending);
    let modified = usize::from(work.as_ref().modify(Duration::from_millis(10)).unwrap());
    assert_eq!(modified, 1);
    assert!(work.as_ref().flush());
    assert_eq!(probe.callbacks.load(Ordering::Acquire), 1);
    delayed += usize::from(work.as_ref().queue(Duration::from_millis(10)).unwrap());
    until(
        || probe.callbacks.load(Ordering::Acquire) == 2,
        "second delayed timer transfer callback",
    );
    work.as_ref().flush();
    let transfers = probe.callbacks.load(Ordering::Acquire);
    assert_eq!((delayed, transfers), (2, 2));
    assert!(work.as_ref().queue(Duration::from_secs(1)).unwrap());
    let cancelled = usize::from(work.as_ref().cancel_sync());
    assert_eq!(cancelled, 1);
    let idle = delayed_testing::snapshot(work.as_ref());
    assert!(!idle.timer_pending && !idle.work_pending && !idle.running);
    let mut sync_wait = 0;
    let mut inflight = 0;
    if 1 < cpu::count() {
        for point in [
            delayed_testing::PausePoint::BeforeTransfer,
            delayed_testing::PausePoint::AfterTransfer,
        ] {
            let (waited, active) = paused_delayed_stop(&work, &probe, point);
            sync_wait += waited;
            inflight += active;
        }
    }
    assert_eq!(sync_wait, if cpu::count() == 1 { 0 } else { 2 });
    assert_eq!(inflight, sync_wait);
    let rejected_orphan = rejected_delayed_transfer(&rejected_queue, &rejected_work, &probe);
    work.as_ref().close_sync();
    DELAY_SOURCE_CLOSED.store(true, Ordering::Release);
    assert_eq!(
        work.as_ref().queue(Duration::from_millis(1)),
        Err(DelayedError::Closed)
    );
    let closed = delayed_testing::snapshot(work.as_ref());
    assert!(closed.closed && !closed.running && !closed.timer_pending && !closed.work_pending);
    queue.close_sync();
    let orphan = rejected_orphan + unique_delayed(&work);
    assert_eq!(orphan, 0);
    probe.clear();
    drop((work, rejected_work, queue, rejected_queue, probe));
    assert_eq!(OPERATION_DROPS.load(Ordering::Acquire) - drops_before, 1);
    crate::timer::sleep(Duration::from_millis(30));
    let access_after_close = ACCESS_AFTER_CLOSE.load(Ordering::Acquire);
    assert_eq!(access_after_close, 0);
    let settled = reclaimed(initial);
    crate::println!("PASS execution-X22 delayed={} modified={} transfers={} cancelled={} sync_wait={} inflight={} orphan={} access_after_close={} reclaimed={}", delayed, modified, transfers, cancelled, sync_wait, inflight, orphan, access_after_close, settled);
}

static OWNER_DROPS: AtomicUsize = AtomicUsize::new(0);

struct SourceOwner {
    probe: Arc<Probe>,
    work: Pin<Arc<Work>>,
}

impl SourceOwner {
    /// 唯一 owner 先切断真实生产源，再显式一次同步关闭，普通 Drop 不重复停止。
    fn close(&self) {
        assert_eq!(self.probe.stops.load(Ordering::Acquire), 0);
        self.probe.source.store(false, Ordering::Release);
        self.work.as_ref().close_sync();
        assert_eq!(self.probe.stops.fetch_add(1, Ordering::AcqRel), 0);
    }
}

impl Drop for SourceOwner {
    /// 只核已经显式关闭的责任并计一次析构，不等待或再次调用 stop。
    fn drop(&mut self) {
        assert_eq!(self.probe.stops.load(Ordering::Acquire), 1);
        assert!(testing::snapshot(self.work.as_ref()).closed);
        assert_eq!(self.probe.active.load(Ordering::Acquire), 0);
        OWNER_DROPS.fetch_add(1, Ordering::AcqRel);
    }
}

/// 两次 self-requeue 来自实际业务 callback，running 与下一轮票共存而不并行。
fn requeue_callback(work: Pin<&Work>) {
    // SAFETY: SourceOwner 同持 Probe Arc 与 Work；close 先切生产源，同步尾部与强票归还后才析构。
    let (probe, _) = unsafe { context(work.context()) };
    assert!(probe.source.load(Ordering::Acquire));
    let active = probe.active.fetch_add(1, Ordering::AcqRel) + 1;
    probe.maximum.fetch_max(active, Ordering::AcqRel);
    assert_eq!(active, 1);
    let round = probe.callbacks.fetch_add(1, Ordering::AcqRel);
    if round < 2 {
        assert!(work.queue().unwrap());
        let state = testing::snapshot(work);
        assert!(state.running && state.queued);
        probe.submitted.fetch_add(1, Ordering::AcqRel);
    }
    sched::yield_now();
    assert_eq!(probe.active.fetch_sub(1, Ordering::AcqRel), 1);
}

/// X23 cancel 后同 Work 实际复用，owner 关闭一次且 Drop 不再次停止或访问退役存储。
fn test_owner() {
    let initial = baseline();
    let drops_before = OWNER_DROPS.load(Ordering::Acquire);
    let queue = WorkQueue::try_new(WorkQueueConfig {
        max_active: 4,
        ordered: false,
        reclaim: false,
    })
    .unwrap();
    let probe = Probe::new();
    let work = probe.work(&queue, 0, 0, requeue_callback);
    let owner = SourceOwner { probe, work };
    assert!(owner.work.as_ref().queue().unwrap());
    until(
        || owner.probe.callbacks.load(Ordering::Acquire) == 3,
        "self requeue real callbacks",
    );
    owner.work.as_ref().cancel_sync();
    let idle = testing::snapshot(owner.work.as_ref());
    assert!(!idle.running && !idle.queued && !idle.closed);
    let reused = usize::from(owner.work.as_ref().queue().unwrap());
    assert_eq!(reused, 1);
    owner.work.as_ref().flush();
    let callbacks = owner.probe.callbacks.load(Ordering::Acquire);
    let self_requeue = owner.probe.submitted.load(Ordering::Acquire);
    let max_active = owner.probe.maximum.load(Ordering::Acquire);
    assert_eq!((self_requeue, callbacks, max_active), (2, 4, 1));
    owner.close();
    let closed = usize::from(testing::snapshot(owner.work.as_ref()).closed);
    let rejected = usize::from(owner.work.as_ref().queue() == Err(QueueError::Closed));
    let stops = owner.probe.stops.load(Ordering::Acquire);
    assert_eq!((closed, rejected, stops), (1, 1, 1));
    queue.close_sync();
    owner.probe.clear();
    unique_work(&owner.work);
    drop((owner, queue));
    let drops = OWNER_DROPS.load(Ordering::Acquire) - drops_before;
    assert_eq!(drops, 1);
    let settled = reclaimed(initial);
    crate::println!("PASS execution-X23 self_requeue={} callbacks={} max_active={} reused={} closed={} rejected={} stops={} drops={} reclaimed={}", self_requeue, callbacks, max_active, reused, closed, rejected, stops, drops, settled);
}

/// 原定时器用例之后运行六项真实工作场景，末尾再次确认常驻基础服务稳态。
pub(super) fn run() {
    assert_eq!(cpu::current().id, 0);
    let initial = baseline();
    test_duplicates();
    test_deep_blocking();
    test_reclaim();
    test_flush_boundary();
    test_delayed();
    test_owner();
    assert_eq!(reclaimed(initial), 1);
}
