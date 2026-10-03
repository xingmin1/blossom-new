//! 普通任务逐轮消费精确 timer 与共享 worker 的完成事件，显式停止后核对完整资源账。

use alloc::sync::Arc;
use core::pin::Pin;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::boot::MAX_CPUS;
use crate::klog::{self as log, Level};
use crate::sync::{Completion, SpinLock};
use crate::task::{self, JoinHandle, PreparedTask, State, Task};
use crate::time::Duration;
use crate::timer::{self, Timer, TimerKind};
use crate::workqueue::{self, Work, WorkQueue, WorkQueueConfig};
use crate::{cpu, mm, sched};

use super::{baseline, reclaimed, until};

const ROUNDS: usize = 8;

struct Slot {
    task: SpinLock<Option<Arc<Task>>>,
    timer_done: Completion,
    work_done: Completion,
    timer_callbacks: AtomicUsize,
    work_callbacks: AtomicUsize,
    waited: AtomicUsize,
    finished: AtomicBool,
    drops: AtomicUsize,
}

impl Slot {
    /// 固定容量观察和完成源在异步发表前已有稳定存储，回调上下文只保存槽号。
    const fn new() -> Self {
        Self {
            task: SpinLock::new(None),
            timer_done: Completion::new(),
            work_done: Completion::new(),
            timer_callbacks: AtomicUsize::new(0),
            work_callbacks: AtomicUsize::new(0),
            waited: AtomicUsize::new(0),
            finished: AtomicBool::new(false),
            drops: AtomicUsize::new(0),
        }
    }
}

static SLOTS: [Slot; MAX_CPUS] = [const { Slot::new() }; MAX_CPUS];

struct LocalDrop(usize);

impl Drop for LocalDrop {
    /// 正常入口返回析构一次，计数与完成消费分别证明栈局部和业务已经结束。
    fn drop(&mut self) {
        assert_eq!(SLOTS[self.0].drops.fetch_add(1, Ordering::AcqRel), 0);
    }
}

/// 精确硬回调在所属 CPU 记录有限标量，然后发布本轮业务完成 token。
fn timer_callback(timer: Pin<&Timer>) {
    let index = timer.context();
    let slot = &SLOTS[index];
    assert_eq!(timer.cpu(), index);
    assert_eq!(cpu::current().id, index);
    assert!(0 < cpu::current().hardirq_depth.load(Ordering::Relaxed));
    let round = slot.timer_callbacks.load(Ordering::Acquire);
    assert!(round < ROUNDS);
    crate::klog!(Level::Info, "X28 timer slot={} round={}", index, round);
    assert_eq!(slot.timer_callbacks.fetch_add(1, Ordering::AcqRel), round);
    slot.timer_done.complete();
}

/// 共享池的真实 worker 在固定 CPU 记录本轮，再通知普通任务消费业务完成。
fn work_callback(work: Pin<&Work>) {
    let index = work.context();
    let slot = &SLOTS[index];
    assert_eq!(work.cpu(), index);
    assert_eq!(cpu::current().id, index);
    assert_eq!(cpu::current().hardirq_depth.load(Ordering::Relaxed), 0);
    let worker = task::current_task_id().expect("mixed work outside ordinary task");
    let state = workqueue::testing::snapshot(work);
    assert!(state.running);
    assert_eq!(state.worker, worker.as_usize());
    let round = slot.work_callbacks.load(Ordering::Acquire);
    assert!(round < ROUNDS);
    crate::klog!(Level::Info, "X28 work slot={} round={}", index, round);
    assert_eq!(slot.work_callbacks.fetch_add(1, Ordering::AcqRel), round);
    slot.work_done.complete();
}

/// 只有已经消费上一轮两个事件的普通任务才登记下一轮精确期限。
fn arm(timer: Pin<&Timer>) {
    assert!(timer
        .arm(timer::after(Duration::from_millis(1)).expect("mixed timer deadline overflow"))
        .expect("mixed timer closed during production"));
}

/// 每个预备任务消费八轮真实完成；入口拥有 Timer/Work 克隆在正常离栈时归还。
fn participant(index: usize, timer: Pin<Arc<Timer>>, work: Pin<Arc<Work>>) -> i32 {
    let _local = LocalDrop(index);
    let slot = &SLOTS[index];
    assert_eq!(cpu::current().id, index);
    assert!(task::current_worker().is_none());
    let previous = slot.task.lock().replace(task::current_task());
    assert!(previous.is_none());
    drop(previous);
    for round in 0..ROUNDS {
        slot.timer_done.wait();
        slot.waited.fetch_add(1, Ordering::Relaxed);
        assert_eq!(slot.timer_callbacks.load(Ordering::Acquire), round + 1);
        assert_eq!(cpu::current().id, index);
        assert!(work
            .as_ref()
            .queue()
            .expect("mixed work closed during production"));
        slot.work_done.wait();
        slot.waited.fetch_add(1, Ordering::Relaxed);
        assert_eq!(slot.work_callbacks.load(Ordering::Acquire), round + 1);
        assert_eq!(cpu::current().id, index);
        if round + 1 < ROUNDS {
            arm(timer.as_ref());
        }
    }
    // Release 发布最后一次源使用；此后只正常返回，不再 arm 或 queue。
    slot.finished.store(true, Ordering::Release);
    ROUNDS as i32
}

/// 首轮尚无任何生产者，真实 Blocked 与完成源成员登记可保持到协调者统一 arm。
fn initially_blocked(index: usize, handle: &JoinHandle) -> bool {
    let slot = &SLOTS[index];
    let observed = slot.task.lock().as_ref().map(Arc::clone);
    observed.is_some_and(|task| {
        assert_eq!(task.id(), handle.task_id());
        sched::test_state(&task) == State::Blocked && slot.timer_done.test_waiting() == 1
    })
}

/// 真实 close 清状态之后，仍等待锁外归还的异步 Arc 票，不将清位误作拥有已释放。
fn wait_owned_tail<T>(object: &Pin<Arc<T>>) {
    // SAFETY: 仅克隆并观察 Arc 控制块，绝不移出 T；原 Pin 和本地 observer 全程保活同一固定分配。
    let observed = unsafe { Pin::into_inner_unchecked(object.clone()) };
    until(
        || Arc::strong_count(&observed) == 2,
        "mixed asynchronous ownership returned",
    );
    drop(observed);
}

/// 在生产者结束源使用且所有回调已同步关闭后读取真实成员与 token；不消费残留或代替清理。
fn empty_sources(cpus: usize) -> usize {
    let mut waiters = 0;
    for slot in &SLOTS[..cpus] {
        waiters += slot.timer_done.test_waiting() + slot.work_done.test_waiting();
        assert!(!slot.timer_done.is_ready());
        assert!(!slot.work_done.is_ready());
    }
    assert_eq!(waiters, 0);
    waiters
}

/// 完整预备每核一个任务和两个源，首次真实登记后开始，停止、join、归还拥有再比较两基线。
pub(super) fn run(suite_baseline: (mm::MemoryStats, workqueue::testing::MetadataStats)) {
    let initial = baseline();
    let cpus = cpu::count();
    let queue = WorkQueue::try_new(WorkQueueConfig {
        max_active: cpus,
        ordered: false,
        reclaim: false,
    })
    .expect("mixed work domain allocation");
    let timers: [Option<Pin<Arc<Timer>>>; MAX_CPUS] = core::array::from_fn(|index| {
        (index < cpus).then(|| {
            Timer::try_new(index, TimerKind::Precise, timer_callback, index)
                .expect("mixed timer allocation")
        })
    });
    let works: [Option<Pin<Arc<Work>>>; MAX_CPUS] = core::array::from_fn(|index| {
        (index < cpus).then(|| {
            Work::try_new(&queue, index, work_callback, index).expect("mixed work allocation")
        })
    });
    let prepared: [Option<PreparedTask>; MAX_CPUS] = core::array::from_fn(|index| {
        (index < cpus).then(|| {
            let timer = timers[index].as_ref().unwrap().clone();
            let work = works[index].as_ref().unwrap().clone();
            task::prepare_kernel_on(index, move || participant(index, timer, work))
                .expect("mixed participant allocation")
        })
    });
    let mut handles: [Option<JoinHandle>; MAX_CPUS] = [const { None }; MAX_CPUS];
    let mut tasks = 0;
    for (index, prepared) in IntoIterator::into_iter(prepared).enumerate() {
        if let Some(prepared) = prepared {
            handles[index] = Some(prepared.publish());
            tasks += 1;
        }
    }
    until(
        || (0..cpus).all(|index| initially_blocked(index, handles[index].as_ref().unwrap())),
        "mixed initial source registration and blocked tasks",
    );
    let blocked = (0..cpus)
        .filter(|&index| initially_blocked(index, handles[index].as_ref().unwrap()))
        .count();
    assert_eq!(blocked, cpus);
    log::testing::wait_service_blocked();
    let seq_first = log::next_sequence();
    for timer in timers[..cpus].iter().flatten() {
        arm(timer.as_ref());
    }
    until(
        || {
            SLOTS[..cpus]
                .iter()
                .all(|slot| slot.finished.load(Ordering::Acquire))
        },
        "mixed participants stopped production",
    );
    for timer in timers[..cpus].iter().flatten() {
        timer.as_ref().close_sync();
    }
    for work in works[..cpus].iter().flatten() {
        work.as_ref().close_sync();
    }
    queue.flush();
    let mut stopped = 0;
    let mut pending = 0;
    // 域尚未关闭，Work 的 closed 快照只能来自对象自身的永久准入关闭。
    for index in 0..cpus {
        let timer = timer::testing::snapshot(timers[index].as_ref().unwrap().as_ref());
        let work = workqueue::testing::snapshot(works[index].as_ref().unwrap().as_ref());
        stopped += usize::from(timer.closed) + usize::from(work.closed);
        pending += usize::from(timer.queued)
            + usize::from(timer.running)
            + usize::from(work.queued)
            + usize::from(work.running);
    }
    assert_eq!(stopped, 2 * cpus);
    assert_eq!(pending, 0);
    queue.close_sync();
    let waiters = empty_sources(cpus);
    for handle in handles[..cpus].iter_mut() {
        assert_eq!(handle.take().unwrap().join(), ROUNDS as i32);
    }
    for slot in &SLOTS[..cpus] {
        let observed = slot.task.lock().take();
        drop(observed);
    }
    for timer in timers[..cpus].iter().flatten() {
        wait_owned_tail(timer);
    }
    for work in works[..cpus].iter().flatten() {
        wait_owned_tail(work);
    }
    drop((timers, works, queue, handles));
    let timer_callbacks = SLOTS[..cpus]
        .iter()
        .map(|slot| slot.timer_callbacks.load(Ordering::Acquire))
        .sum::<usize>();
    let work_callbacks = SLOTS[..cpus]
        .iter()
        .map(|slot| slot.work_callbacks.load(Ordering::Acquire))
        .sum::<usize>();
    let waited = SLOTS[..cpus]
        .iter()
        .map(|slot| slot.waited.load(Ordering::Acquire))
        .sum::<usize>();
    let drops = SLOTS[..cpus]
        .iter()
        .map(|slot| slot.drops.load(Ordering::Acquire))
        .sum::<usize>();
    assert_eq!(tasks, cpus);
    assert_eq!(timer_callbacks, cpus * ROUNDS);
    assert_eq!(work_callbacks, cpus * ROUNDS);
    assert_eq!(waited, 2 * cpus * ROUNDS);
    assert_eq!(drops, cpus);
    let catchup = log::next_sequence();
    let log_records = catchup.checked_sub(seq_first).unwrap();
    let seq_last = catchup.checked_sub(1).unwrap();
    assert_eq!(log_records, (timer_callbacks + work_callbacks) as u64);
    log::testing::catch_up(catchup);
    log::testing::wait_service_blocked();
    assert_eq!(log::testing::snapshot().next_seq, catchup);
    let recovered = reclaimed(initial);
    let suite_recovered = reclaimed(suite_baseline);
    crate::test_println!("PASS execution-X28 cpus={} tasks={} rounds={} timer_callbacks={} work_callbacks={} log_records={} waited={} blocked={} drops={} stopped={} pending={} waiters={} reclaimed={} suite_reclaimed={} seq_first={} seq_last={} catchup={}", cpus, tasks, ROUNDS, timer_callbacks, work_callbacks, log_records, waited, blocked, drops, stopped, pending, waiters, recovered, suite_recovered, seq_first, seq_last, catchup);
}
