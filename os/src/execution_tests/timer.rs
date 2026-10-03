//! 在唯一生产定时器、真实 Task、硬中断与服务 BH 上核验睡眠及停止协议。

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::hint::spin_loop;
use core::pin::Pin;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use crate::sync::{Completion, IrqSaveGuard, SpinLock};
use crate::task::{self, State, Task};
use crate::time::{self as clock, Duration, MonotonicInstant};
use crate::timer::{self as timers, testing, ArmError, CancelResult, Timer, TimerKind};
use crate::wait::{Waiter, Waker};
use crate::{arch, cpu, mm, sched};

static SLEEP_TASK: SpinLock<Option<Arc<Task>>> = SpinLock::new(None);
static SLEEP_WAKER: SpinLock<Option<Waker>> = SpinLock::new(None);
static SLEEP_ARMS: AtomicUsize = AtomicUsize::new(0);
static SLEEP_EARLY: AtomicUsize = AtomicUsize::new(0);
static CALLBACKS: AtomicUsize = AtomicUsize::new(0);
static BUSY: AtomicUsize = AtomicUsize::new(0);
static ACTIVE: AtomicUsize = AtomicUsize::new(0);
static MAX_ACTIVE: AtomicUsize = AtomicUsize::new(0);
static SELF_REARM: AtomicBool = AtomicBool::new(false);
static REARMED: AtomicUsize = AtomicUsize::new(0);
static CANCEL_TASK: SpinLock<Option<Arc<Task>>> = SpinLock::new(None);
static CANCEL_GATE: Completion = Completion::new();
static CANCEL_STARTED: AtomicBool = AtomicBool::new(false);
static CANCEL_DONE: AtomicBool = AtomicBool::new(false);
static REMOTE_FIRED: AtomicUsize = AtomicUsize::new(0);
static REMOTE_EARLY: AtomicUsize = AtomicUsize::new(0);
static REMOTE_DEADLINE: AtomicU64 = AtomicU64::new(0);
static COARSE_FIRED: AtomicUsize = AtomicUsize::new(0);
static COARSE_EARLY: AtomicUsize = AtomicUsize::new(0);
static COARSE_DEADLINES: [AtomicU64; 256] = [const { AtomicU64::new(0) }; 256];
static COARSE_HITS: [AtomicUsize; 256] = [const { AtomicUsize::new(0) }; 256];

/// 用真实硬件计数构造不可重置的有限观察期限。
fn raw_deadline() -> u64 {
    arch::read_counter()
        .checked_add(
            arch::counter_frequency()
                .checked_mul(5)
                .expect("timer raw interval"),
        )
        .expect("timer raw deadline")
}

/// 只协调测试参与者；生产睡眠、取消和服务进度仍由各自源登记提供。
fn wait_before(mut predicate: impl FnMut() -> bool, end: u64, yielding: bool, name: &str) {
    while !predicate() {
        assert!(
            arch::read_counter() < end,
            "timer execution timeout: {}",
            name
        );
        if yielding {
            sched::yield_now();
        } else {
            spin_loop();
        }
    }
}

/// 取得唯一单调时间域中的有限未来截止。
fn deadline(delay: Duration) -> MonotonicInstant {
    timers::after(delay).expect("timer deadline overflow")
}

/// 服务观察引用取得后已释放 base 锁，再读唯一调度状态，避免锁序反转。
pub(super) fn wait_services_blocked() {
    let end = raw_deadline();
    for id in 0..cpu::count() {
        wait_before(
            || {
                testing::service_task(id)
                    .is_some_and(|service| sched::test_state(&service) == State::Blocked)
            },
            end,
            true,
            "timer service initial block",
        );
    }
}

/// 只在全部临时参与者和服务停稳之后核算唯一 MM 的可归还页。
fn pages() -> usize {
    let stats = mm::stats();
    stats.buddy_free_pages + stats.pcp_cached_pages + stats.slab_backing_pages
}

/// 克隆现存 timer 控制块，不分配另一种节点或排队后端。
fn clone_timer(timer: &Pin<Arc<Timer>>) -> Pin<Arc<Timer>> {
    timer.clone()
}

/// 运行引用在真实回调尾部锁外归还；最后只剩测试拥有者，不能用页总数掩盖 slab 引用泄漏。
fn assert_unique(timer: &Pin<Arc<Timer>>) {
    // SAFETY: 只取克隆 Arc 作为强引用观察，不移动或暴露可移动的 Timer；原 Pin 保持节点固定。
    let observer = unsafe { Pin::into_inner_unchecked(timer.clone()) };
    wait_before(
        || Arc::strong_count(&observer) == 2,
        raw_deadline(),
        true,
        "timer final running/queued reference returned",
    );
    assert_eq!(Arc::strong_count(&observer), 2);
    drop(observer);
}

/// 所有测试拥有对象在启动真实交错前准备稳定存储。
fn new_timer(
    cpu: usize,
    kind: TimerKind,
    callback: fn(Pin<&Timer>),
    context: usize,
) -> Pin<Arc<Timer>> {
    Timer::try_new(cpu, kind, callback, context).expect("test timer allocation")
}

/// hook 每轮保存真实生产 Waker；旧弱观察在短测试锁外释放。
fn sleep_armed(waker: Waker) {
    let old = SLEEP_WAKER.lock().replace(waker);
    drop(old);
    SLEEP_ARMS.fetch_add(1, Ordering::Release);
}

/// 在同一普通 Task 检验零/已过期返回不开始额外等待轮次。
fn immediate_sleep(delay: Option<MonotonicInstant>) -> usize {
    let before = {
        let waiter = Waiter::new();
        waiter.id()
    };
    match delay {
        Some(expiry) => timers::sleep_until(expiry),
        None => timers::sleep(Duration::ZERO),
    }
    let after = {
        let waiter = Waiter::new();
        waiter.id()
    };
    assert_eq!(
        after,
        before.next(),
        "immediate sleep opened an unnecessary wait"
    );
    usize::from(after == before.next())
}

/// 三次提前通知只能产生新等待身份，返回时仍不得早于原绝对截止。
fn sleep_worker(expiry: MonotonicInstant) -> i32 {
    let old = SLEEP_TASK.lock().replace(task::current_task());
    assert!(old.is_none());
    timers::test_sleep_until(expiry, sleep_armed);
    SLEEP_EARLY.fetch_add(usize::from(clock::now() < expiry), Ordering::Relaxed);
    113
}

/// 以生产 checked 算术验证相位、零周期、溢出及 after 的实际时间区间。
fn arithmetic() -> (usize, usize) {
    let previous = MonotonicInstant::from_nanos(7);
    let period = Duration::from_nanos(5);
    let late = MonotonicInstant::from_nanos(28);
    let next = timers::periodic_deadline(previous, period, late).expect("periodic phase");
    assert_eq!(next, MonotonicInstant::from_nanos(32));
    assert!(late < next);
    assert_eq!(
        timers::periodic_deadline(previous, period, MonotonicInstant::from_nanos(12)),
        Some(MonotonicInstant::from_nanos(17))
    );
    assert_eq!(
        timers::periodic_deadline(previous, Duration::ZERO, late),
        None
    );
    assert_eq!(
        timers::periodic_deadline(
            MonotonicInstant::from_nanos(u64::MAX - 1),
            period,
            MonotonicInstant::from_nanos(u64::MAX)
        ),
        None
    );
    let phase = usize::from(next.as_nanos() == 32);
    let delay = Duration::from_millis(1);
    let before = clock::now();
    let after = timers::after(delay).expect("checked after");
    let observed = clock::now();
    assert!(before.checked_add(delay).unwrap() <= after);
    assert!(after <= observed.checked_add(delay).unwrap());
    assert_eq!(timers::after(Duration::MAX), None);
    (phase, usize::from(before < after))
}

/// 记录真实四次 Blocked，前三次通知恢复后继续同一个截止，最后等待硬 IRQ。
fn test_sleep() {
    let zero = immediate_sleep(None);
    let past = immediate_sleep(Some(MonotonicInstant::from_nanos(0)));
    let (phase, after) = arithmetic();
    let expiry = deadline(Duration::from_millis(250));
    let end = raw_deadline();
    let handle = task::spawn_kernel_on(0, move || sleep_worker(expiry)).expect("sleep test task");
    let mut blocked = 0;
    let mut wakes = 0;
    let mut kept = 0;
    for round in 1..=4 {
        wait_before(
            || {
                let observed = SLEEP_TASK.lock().as_ref().map(Arc::clone);
                SLEEP_ARMS.load(Ordering::Acquire) == round
                    && observed.is_some_and(|task| sched::test_state(&task) == State::Blocked)
            },
            end,
            true,
            "fixed deadline sleeper block",
        );
        blocked += 1;
        let actual_expiry = testing::sleep_deadline();
        assert_eq!(actual_expiry, expiry);
        kept += usize::from(actual_expiry == expiry);
        assert!(
            clock::now() < expiry,
            "early notifications missed original deadline"
        );
        if round <= 3 {
            let waker = SLEEP_WAKER.lock().as_ref().unwrap().clone();
            assert!(waker.notify());
            wakes += 1;
        }
    }
    wait_before(
        || handle.try_join().is_some(),
        end,
        true,
        "sleep final IRQ wake",
    );
    assert_eq!(handle.join(), 113);
    let early = SLEEP_EARLY.load(Ordering::Acquire);
    let future = usize::from(expiry <= clock::now());
    let deadline_kept = usize::from(
        kept == blocked && SLEEP_ARMS.load(Ordering::Acquire) == blocked && future == 1,
    );
    assert_eq!(
        (zero, past, future, deadline_kept, early, phase, after),
        (1, 1, 1, 1, 0, 1, 1)
    );
    assert_eq!(wakes, 3);
    let old_task = SLEEP_TASK.lock().take().unwrap();
    assert_eq!(sched::test_state(&old_task), State::Exited);
    assert_eq!(Arc::strong_count(&old_task), 1);
    let old_waker = SLEEP_WAKER.lock().take();
    drop((old_task, old_waker));
    crate::println!("PASS execution-X13 zero={} past={} future={} blocked={} wakes={} deadline_kept={} early={} phase={} after={}", zero, past, future, blocked, wakes, deadline_kept, early, phase, after);
}

/// 硬回调内部必须报告 Busy；自重排仍由同一拥有对象和同一个 base 执行。
fn lifecycle_callback(timer: Pin<&Timer>) {
    assert!(0 < cpu::current().hardirq_depth.load(Ordering::Acquire));
    assert_eq!(cpu::current().id, timer.cpu());
    let active = ACTIVE.fetch_add(1, Ordering::AcqRel) + 1;
    MAX_ACTIVE.fetch_max(active, Ordering::AcqRel);
    assert_eq!(active, 1);
    assert_eq!(timer.try_cancel(), CancelResult::Busy);
    BUSY.fetch_add(1, Ordering::Relaxed);
    CALLBACKS.fetch_add(1, Ordering::Release);
    if SELF_REARM.swap(false, Ordering::AcqRel) {
        assert!(timer.arm(deadline(Duration::from_millis(10))).unwrap());
        REARMED.fetch_add(1, Ordering::Release);
    }
    assert_eq!(ACTIVE.fetch_sub(1, Ordering::AcqRel), 1);
}

/// 真实同步停止者通过现有 Completion 起跑，再在永久 finished 源上阻塞。
fn cancel_worker(timer: Pin<Arc<Timer>>, close: bool) -> i32 {
    let old = CANCEL_TASK.lock().replace(task::current_task());
    assert!(old.is_none());
    CANCEL_GATE.wait();
    CANCEL_STARTED.store(true, Ordering::Release);
    if close {
        timer.as_ref().close_sync();
    } else {
        timer.as_ref().cancel_sync();
    }
    assert!(!testing::snapshot(timer.as_ref()).running);
    CANCEL_DONE.store(true, Ordering::Release);
    114
}

/// 在真实回调摘离或尾部窗口确认同步取消任务已 Blocked，释放尾部之后才允许返回。
fn paused_stop(
    timer: &Pin<Arc<Timer>>,
    point: testing::PausePoint,
    close: bool,
    overlap: bool,
) -> (usize, usize) {
    assert!(1 < cpu::count());
    CANCEL_STARTED.store(false, Ordering::Relaxed);
    CANCEL_DONE.store(false, Ordering::Relaxed);
    let worker_timer = clone_timer(timer);
    let handle = task::spawn_kernel_on(0, move || cancel_worker(worker_timer, close))
        .expect("sync cancel task");
    let end = raw_deadline();
    wait_before(
        || {
            let observed = CANCEL_TASK.lock().as_ref().map(Arc::clone);
            observed.is_some_and(|task| sched::test_state(&task) == State::Blocked)
        },
        end,
        true,
        "sync canceller prepared",
    );
    testing::arm_pause(timer.as_ref(), point);
    assert!(timer
        .as_ref()
        .arm(deadline(Duration::from_millis(10)))
        .unwrap());
    wait_before(testing::is_paused, end, false, "callback pause");
    let running = testing::snapshot(timer.as_ref());
    assert!(running.running && !running.queued);
    let inflight = usize::from(running.running);
    if overlap {
        assert!(timer
            .as_ref()
            .arm(deadline(Duration::from_secs(1)))
            .unwrap());
        let state = testing::snapshot(timer.as_ref());
        assert!(state.running && state.queued);
    }
    assert_eq!(timer.as_ref().try_cancel(), CancelResult::Busy);
    CANCEL_GATE.complete();
    wait_before(
        || {
            if !CANCEL_STARTED.load(Ordering::Acquire) {
                return false;
            }
            let observed = Arc::clone(CANCEL_TASK.lock().as_ref().unwrap());
            sched::test_state(&observed) == State::Blocked
        },
        end,
        true,
        "sync canceller waits callback tail",
    );
    let sync_wait = {
        let observed = Arc::clone(CANCEL_TASK.lock().as_ref().unwrap());
        usize::from(sched::test_state(&observed) == State::Blocked)
    };
    assert!(!CANCEL_DONE.load(Ordering::Acquire));
    assert!(testing::snapshot(timer.as_ref()).running);
    if close {
        assert!(testing::snapshot(timer.as_ref()).closed);
        assert_eq!(
            timer.as_ref().arm(deadline(Duration::from_secs(1))),
            Err(ArmError::Closed)
        );
    }
    testing::release_pause();
    wait_before(
        || handle.try_join().is_some(),
        end,
        true,
        "sync cancel final return",
    );
    assert_eq!(handle.join(), 114);
    assert!(CANCEL_DONE.load(Ordering::Acquire));
    let state = testing::snapshot(timer.as_ref());
    assert!(!state.running && !state.queued);
    let observed = CANCEL_TASK.lock().take().unwrap();
    assert_eq!(sched::test_state(&observed), State::Exited);
    assert_eq!(Arc::strong_count(&observed), 1);
    drop(observed);
    assert_eq!((inflight, sync_wait), (1, 1));
    (inflight, sync_wait)
}

/// 单核只核验本核回调内 Busy，多核同时核验摘离与尾部两个真实阻塞窗口。
fn test_cancel() {
    let owner = if cpu::count() == 1 { 0 } else { 1 };
    let timer = new_timer(owner, TimerKind::Precise, lifecycle_callback, 0);
    assert!(timer
        .as_ref()
        .arm(deadline(Duration::from_secs(1)))
        .unwrap());
    assert!(testing::snapshot(timer.as_ref()).queued);
    let queued = usize::from(timer.as_ref().try_cancel() == CancelResult::Cancelled);
    assert_eq!(queued, 1);
    let before = CALLBACKS.load(Ordering::Acquire);
    let busy_before = BUSY.load(Ordering::Acquire);
    let mut inflight = 0;
    let mut sync_wait = 0;
    if 1 < cpu::count() {
        for point in [testing::PausePoint::Dequeued, testing::PausePoint::Tail] {
            let (in_flight, waiting) = paused_stop(&timer, point, false, false);
            inflight += in_flight;
            sync_wait += waiting;
        }
    } else {
        assert!(timer
            .as_ref()
            .arm(deadline(Duration::from_millis(10)))
            .unwrap());
        wait_before(
            || before < CALLBACKS.load(Ordering::Acquire),
            raw_deadline(),
            true,
            "single busy callback",
        );
        timer.as_ref().cancel_sync();
    }
    let state = testing::snapshot(timer.as_ref());
    assert!(!state.running && !state.queued);
    let after = usize::from(timer.as_ref().try_cancel() == CancelResult::Idle);
    let tail = usize::from(!state.running);
    let busy = BUSY.load(Ordering::Acquire) - busy_before;
    assert!(1 <= busy);
    assert_eq!((after, tail), (1, 1));
    assert_eq!(inflight, sync_wait);
    assert_eq!(inflight, if cpu::count() == 1 { 0 } else { 2 });
    timer.as_ref().close_sync();
    assert_unique(&timer);
    drop(timer);
    crate::println!(
        "PASS execution-X14 queued={} after={} busy={} inflight={} sync_wait={} tail={}",
        queued,
        after,
        busy,
        inflight,
        sync_wait,
        tail
    );
}

/// 同一节点自重排、取消后复用，并在运行加排队重叠时先关入口再等尾部。
fn test_rearm_close() {
    let owner = if cpu::count() == 1 { 0 } else { 1 };
    let timer = new_timer(owner, TimerKind::Precise, lifecycle_callback, 0);
    let callbacks_before = CALLBACKS.load(Ordering::Acquire);
    let rearmed_before = REARMED.load(Ordering::Acquire);
    SELF_REARM.store(true, Ordering::Release);
    assert!(timer
        .as_ref()
        .arm(deadline(Duration::from_millis(10)))
        .unwrap());
    let end = raw_deadline();
    wait_before(
        || callbacks_before + 2 <= CALLBACKS.load(Ordering::Acquire),
        end,
        true,
        "self rearm second callback",
    );
    timer.as_ref().cancel_sync();
    assert_eq!(timer.as_ref().try_cancel(), CancelResult::Idle);
    assert!(timer
        .as_ref()
        .arm(deadline(Duration::from_secs(1)))
        .unwrap());
    assert_eq!(timer.as_ref().try_cancel(), CancelResult::Cancelled);
    assert!(timer
        .as_ref()
        .arm(deadline(Duration::from_millis(10)))
        .unwrap());
    wait_before(
        || callbacks_before + 3 <= CALLBACKS.load(Ordering::Acquire),
        end,
        true,
        "cancelled timer reused",
    );
    timer.as_ref().cancel_sync();
    let reused = usize::from(CALLBACKS.load(Ordering::Acquire) == callbacks_before + 3);
    let overlap = if 1 < cpu::count() {
        let (running, blocked) = paused_stop(&timer, testing::PausePoint::Tail, true, true);
        assert_eq!(running, blocked);
        running
    } else {
        timer.as_ref().close_sync();
        0
    };
    let state = testing::snapshot(timer.as_ref());
    assert!(state.closed && !state.running && !state.queued);
    let closed = usize::from(state.closed);
    let rejected = usize::from(
        timer.as_ref().arm(deadline(Duration::from_millis(1))) == Err(ArmError::Closed),
    );
    let rearmed = REARMED.load(Ordering::Acquire) - rearmed_before;
    let callbacks = CALLBACKS.load(Ordering::Acquire) - callbacks_before;
    let max_active = MAX_ACTIVE.load(Ordering::Acquire);
    assert!(1 <= rearmed && 2 <= callbacks);
    assert_eq!((max_active, reused, closed, rejected), (1, 1, 1, 1));
    assert_unique(&timer);
    drop(timer);
    crate::println!("PASS execution-X15 rearmed={} callbacks={} max_active={} reused={} closed={} rejected={} overlap={}", rearmed, callbacks, max_active, reused, closed, rejected, overlap);
}

/// 较早 precise 回调在真实目标硬 IRQ 检查绝对期限，并记录相同源中的实际顺序。
fn remote_callback(timer: Pin<&Timer>) {
    assert_eq!(cpu::current().id, timer.cpu());
    assert!(0 < cpu::current().hardirq_depth.load(Ordering::Acquire));
    if timer.context() == 0 {
        assert_eq!(REMOTE_FIRED.fetch_add(1, Ordering::AcqRel), 0);
        REMOTE_EARLY.fetch_add(
            usize::from(clock::now().as_nanos() < REMOTE_DEADLINE.load(Ordering::Acquire)),
            Ordering::Release,
        );
    } else {
        panic!("old long timer fired before explicit stop");
    }
}

/// 单核只核本地先到；多核在真实 program 停点插早期节点，handler 返回后再核 ISA 完成。
fn test_remote() {
    let mut prepared = Vec::with_capacity(cpu::count());
    for target in 0..cpu::count() {
        prepared.push((
            new_timer(target, TimerKind::Precise, remote_callback, 0),
            new_timer(target, TimerKind::Precise, remote_callback, 1),
        ));
    }
    let mut remote = 0;
    let mut kicks = 0;
    let mut idle = 0;
    let mut first = 0;
    let mut stale = 0;
    let start = if cpu::count() == 1 { 0 } else { 1 };
    for (target, (early_timer, long_timer)) in prepared.iter().enumerate().skip(start) {
        REMOTE_FIRED.store(0, Ordering::Relaxed);
        REMOTE_EARLY.store(0, Ordering::Relaxed);
        let end = raw_deadline();
        let long = deadline(Duration::from_secs(10));
        assert!(long_timer.as_ref().arm(long).unwrap());
        if 0 < target {
            let before_kicks = cpu::test_timer_reprograms(target);
            loop {
                assert!(arch::read_counter() < end, "remote ISA idle retry timeout");
                wait_before(
                    || {
                        cpu::get(target).idle.load(Ordering::Acquire)
                            && !cpu::get(target).kick_pending()
                            && cpu::get(target).hardirq_depth.load(Ordering::Acquire) == 0
                            && clock::testing::last_programmed_deadline(target) == long
                    },
                    end,
                    false,
                    "remote long timer installed in idle",
                );
                // 无新请求的准备窗口只让目标有机会进入 WFI/IDLE；既不是返回握手，也不是 ISA 证明。
                let quiet_end = arch::read_counter()
                    .checked_add(arch::counter_frequency().div_ceil(1000))
                    .expect("remote idle quiet interval overflow");
                wait_before(
                    || quiet_end <= arch::read_counter(),
                    end,
                    false,
                    "remote idle quiet preparation",
                );
                if !cpu::get(target).idle.load(Ordering::Acquire)
                    || cpu::get(target).kick_pending()
                    || cpu::get(target).hardirq_depth.load(Ordering::Acquire) != 0
                    || clock::testing::last_programmed_deadline(target) != long
                {
                    continue;
                }
                assert!(0 < cpu::test_idle_waits(target));
                assert!(cpu::get(target).idle.load(Ordering::Acquire));
                let round_kicks = cpu::test_timer_reprograms(target);
                let completed = cpu::test_wait_completions(target);
                clock::testing::arm_program_pause(target);
                cpu::kick(target, cpu::KICK_TIMER_REPROGRAM);
                wait_before(
                    || clock::testing::program_paused(target),
                    end,
                    false,
                    "remote protected program pause",
                );
                // 停点内入口数不会推进；下一入口只证明本次 handler/等待调用已经返回，不算 ISA。
                let entered = cpu::test_idle_waits(target);
                let early = deadline(Duration::from_millis(100));
                REMOTE_DEADLINE.store(early.as_nanos(), Ordering::Release);
                assert!(early_timer.as_ref().arm(early).unwrap());
                assert_eq!(testing::snapshot(early_timer.as_ref()).expiry, early);
                clock::testing::release_program_pause(target);
                wait_before(
                    || clock::testing::last_programmed_deadline(target) == early,
                    end,
                    false,
                    "earlier deadline survives old program",
                );
                wait_before(
                    || {
                        round_kicks < cpu::test_timer_reprograms(target)
                            && !cpu::get(target).kick_pending()
                            && cpu::get(target).hardirq_depth.load(Ordering::Acquire) == 0
                            && cpu::get(target).idle.load(Ordering::Acquire)
                            && entered < cpu::test_idle_waits(target)
                    },
                    end,
                    false,
                    "remote reprogram kicks and idle call completed",
                );
                // RV 在开 IRQ 前计数，LA 在 handler 返回后的 IDLE 尾部计数；两者都只采真实增量。
                let waits = cpu::test_wait_completions(target) - completed;
                // 回滚窗口可能在 release 后才开始新 IDLE 并等到 early 到期；这种增量不能证明插入前等待。
                if 0 < waits && REMOTE_FIRED.load(Ordering::Acquire) == 0 && clock::now() < early {
                    let final_program = clock::testing::last_programmed_deadline(target);
                    stale += usize::from(final_program == long);
                    assert_eq!(
                        final_program, early,
                        "old long program overwrote earlier node"
                    );
                    idle += waits;
                    break;
                }
                // 合法 skip 完整撤掉本轮登记，下一轮沿同一节点重排；不能吞掉未返回的 ISA 完成。
                early_timer.as_ref().cancel_sync();
                assert_eq!(REMOTE_EARLY.load(Ordering::Acquire), 0);
                let state = testing::snapshot(early_timer.as_ref());
                assert!(!state.queued && !state.running && !state.closed);
                REMOTE_FIRED.store(0, Ordering::Relaxed);
                REMOTE_EARLY.store(0, Ordering::Relaxed);
            }
            kicks += cpu::test_timer_reprograms(target) - before_kicks;
            remote += 1;
        } else {
            let early = deadline(Duration::from_millis(10));
            REMOTE_DEADLINE.store(early.as_nanos(), Ordering::Release);
            assert!(early_timer.as_ref().arm(early).unwrap());
        }
        wait_before(
            || REMOTE_FIRED.load(Ordering::Acquire) == 1,
            end,
            true,
            "earlier timer real IRQ",
        );
        early_timer.as_ref().cancel_sync();
        let early_count = REMOTE_EARLY.load(Ordering::Acquire);
        assert_eq!(early_count, 0);
        assert!(testing::snapshot(long_timer.as_ref()).queued);
        first += usize::from(REMOTE_FIRED.load(Ordering::Acquire) == 1 && early_count == 0);
        early_timer.as_ref().close_sync();
        long_timer.as_ref().close_sync();
    }
    assert_eq!(remote, cpu::count() - 1);
    assert!(remote <= kicks && remote <= idle);
    assert_eq!(stale, 0);
    assert_eq!(first, if cpu::count() == 1 { 1 } else { remote });
    let first = usize::from(0 < first);
    for (early, long) in prepared {
        early.as_ref().close_sync();
        long.as_ref().close_sync();
        assert_unique(&early);
        assert_unique(&long);
    }
    crate::println!(
        "PASS execution-X16 remote={} kicks={} first={} stale={} idle={}",
        remote,
        kicks,
        first,
        stale,
        idle
    );
}

/// 在真实树中混合相同期限、逆序插入、任意撤链及更新，逐步核验颜色、黑高和缓存首项。
fn tree_cases() {
    let mut nodes = Vec::with_capacity(48);
    for _ in 0..48 {
        nodes.push(new_timer(0, TimerKind::Precise, remote_callback, 1));
    }
    let base = deadline(Duration::from_secs(10));
    for pattern in 0..4 {
        for step in 0..48 {
            let index = match pattern {
                0 => (step * 17) % 48,
                1 => 47 - step,
                _ => step,
            };
            let offset = if pattern == 3 { 0 } else { index % 13 };
            let expiry = base
                .checked_add(Duration::from_millis(offset as u64))
                .unwrap();
            assert!(nodes[index].as_ref().arm(expiry).unwrap());
            let (members, black_height) = testing::validate_tree(0);
            assert_eq!(members, step + 1);
            assert!(0 < black_height);
        }
        for step in 0..24 {
            let index = (step * 17) % 48;
            assert!(!nodes[index].as_ref().arm(base).unwrap());
            assert_eq!(testing::validate_tree(0).0, 48);
        }
        for step in 0..48 {
            let index = match pattern {
                1 => step,
                2 => 47 - step,
                _ => (step * 29) % 48,
            };
            assert_eq!(nodes[index].as_ref().try_cancel(), CancelResult::Cancelled);
            assert_eq!(testing::validate_tree(0).0, 47 - step);
        }
    }
    for node in nodes {
        node.as_ref().close_sync();
        assert_unique(&node);
    }
    assert_eq!(testing::validate_tree(0).0, 0);
}

/// 粗回调只能在真正服务 BH 内运行，每个未取消节点最多执行一次且不能提前。
fn coarse_callback(timer: Pin<&Timer>) {
    assert_eq!(cpu::current().id, 0);
    assert_eq!(cpu::current().hardirq_depth.load(Ordering::Acquire), 0);
    assert!(0 < cpu::current().bh_depth.load(Ordering::Acquire));
    let index = timer.context();
    assert_eq!(COARSE_HITS[index].fetch_add(1, Ordering::AcqRel), 0);
    COARSE_EARLY.fetch_add(
        usize::from(clock::now().as_nanos() < COARSE_DEADLINES[index].load(Ordering::Acquire)),
        Ordering::Relaxed,
    );
    COARSE_FIRED.fetch_add(1, Ordering::Release);
}

/// 真实 counter 跨桶积压，绝大多数取消后仍剩三批；所有节点和临时任务归还后才发布账务。
fn test_coarse(baseline: usize) {
    tree_cases();
    let wheel_cases = testing::self_check_wheel();
    assert!(12 <= wheel_cases);
    let mut nodes = Vec::with_capacity(256);
    for index in 0..256 {
        nodes.push(new_timer(0, TimerKind::Coarse, coarse_callback, index));
    }
    let before = testing::stats(0);
    let mut armed = 0;
    let mut cancelled = 0;
    let tick = testing::TICK_NS;
    let beyond_range = testing::MAX_TIMEOUT_TICKS
        .checked_add(2)
        .and_then(|ticks| ticks.checked_mul(tick))
        .expect("coarse range boundary overflow");
    assert_eq!(
        nodes[0]
            .as_ref()
            .arm(deadline(Duration::from_nanos(beyond_range))),
        Err(ArmError::OutOfRange),
    );
    let rejected = testing::snapshot(nodes[0].as_ref());
    assert!(!rejected.queued && !rejected.running);
    let jump_start;
    let jump_end;
    {
        let _irq = IrqSaveGuard::new();
        let base = deadline(Duration::from_nanos(tick * 10));
        for (index, node) in nodes.iter().enumerate() {
            let expiry = base
                .checked_add(Duration::from_nanos(tick * (index % 3) as u64))
                .unwrap();
            COARSE_DEADLINES[index].store(expiry.as_nanos(), Ordering::Release);
            assert!(node.as_ref().arm(expiry).unwrap());
            armed += 1;
        }
        for node in nodes.iter().take(160) {
            assert_eq!(node.as_ref().try_cancel(), CancelResult::Cancelled);
            cancelled += 1;
        }
        // 所有 arm/cancel 完成时最早请求仍在未来，才证明这三组未走 past 分支坍成一个当前桶。
        assert!(
            clock::now() < base,
            "coarse registration crossed earliest deadline"
        );
        jump_start = clock::now().as_nanos() / tick;
        let end = raw_deadline();
        let advance = arch::counter_frequency().checked_mul(15).unwrap() / 100;
        let raw_end = arch::read_counter().checked_add(advance).unwrap();
        wait_before(
            || raw_end <= arch::read_counter(),
            end,
            false,
            "real counter coarse bucket jump",
        );
        jump_end = clock::now().as_nanos() / tick;
        assert!(
            base.checked_add(Duration::from_nanos(tick * 3)).unwrap() < clock::now(),
            "coarse jump did not cross all requested buckets"
        );
        assert_eq!(COARSE_FIRED.load(Ordering::Acquire), 0);
    }
    let jump_ticks = jump_end.checked_sub(jump_start).unwrap();
    assert!(2 <= jump_ticks);
    let expected = armed - cancelled;
    wait_before(
        || COARSE_FIRED.load(Ordering::Acquire) == expected,
        raw_deadline(),
        true,
        "coarse BH budget continuation",
    );
    for (index, node) in nodes.iter().enumerate() {
        node.as_ref().close_sync();
        let state = testing::snapshot(node.as_ref());
        assert!(state.closed && !state.running && !state.queued);
        assert_eq!(
            COARSE_HITS[index].load(Ordering::Acquire),
            usize::from(160 <= index)
        );
        assert_unique(node);
    }
    let fired = COARSE_FIRED.load(Ordering::Acquire);
    assert_eq!(COARSE_EARLY.load(Ordering::Acquire), 0);
    wait_services_blocked();
    let after = testing::stats(0);
    let batches = after.bh_batches - before.bh_batches;
    let continuations = after.bh_continuations - before.bh_continuations;
    let max_batch = after.max_bh_callbacks;
    let budget = testing::BH_BUDGET;
    assert_eq!(after.bh_callbacks - before.bh_callbacks, fired);
    assert!(128 <= armed && armed / 2 < cancelled);
    assert_eq!(fired, armed - cancelled);
    assert!(2 <= batches && 1 <= continuations && 1 <= max_batch && max_batch <= budget);
    drop(nodes);
    wait_services_blocked();
    assert_eq!(testing::validate_tree(0).0, 0);
    let returned = pages();
    assert_eq!(
        returned, baseline,
        "temporary timer/task pages not reclaimed"
    );
    let reclaimed = usize::from(returned == baseline);
    crate::println!("PASS execution-X17 armed={} cancelled={} fired={} batches={} continuations={} max_batch={} budget={} wheel_cases={} jump_ticks={} reclaimed={}", armed, cancelled, fired, batches, continuations, max_batch, budget, wheel_cases, jump_ticks, reclaimed);
}

/// 顺序追加五项真实定时器验收；协调者始终固定 CPU0，不迁移 init。
pub(super) fn run() {
    assert_eq!(cpu::current().id, 0);
    wait_services_blocked();
    let baseline = pages();
    test_sleep();
    test_cancel();
    test_rearm_close();
    test_remote();
    test_coarse(baseline);
}
