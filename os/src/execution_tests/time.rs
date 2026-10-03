//! 在真实任务、硬中断、单调时钟与 idle 入口上验证时间和执行账。

use super::{baseline, reclaimed};
use alloc::sync::Arc;
use core::convert::TryFrom;
use core::hint::{black_box, spin_loop};
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use crate::execution_tests::until;
use crate::sync::{Completion, IrqSaveGuard, SpinLock};
use crate::task::{self, State, Task};
use crate::time::{self as clock, Duration, MonotonicInstant};
use crate::{accounting, arch, cpu, sched};

const READ_ROUNDS: usize = 128;
const KICK_ROUNDS: usize = 4;
static PEER_RAN: AtomicBool = AtomicBool::new(false);
static TIME_GO: AtomicBool = AtomicBool::new(false);
static TIME_ARRIVED: AtomicUsize = AtomicUsize::new(0);
static WRITE_PHASE: AtomicUsize = AtomicUsize::new(0);
static READ_PROGRESS: AtomicUsize = AtomicUsize::new(0);
static READERS_PASSED: AtomicUsize = AtomicUsize::new(0);
static ACCOUNT_TASK: SpinLock<Option<Arc<Task>>> = SpinLock::new(None);
static ACCOUNT_PHASE: AtomicUsize = AtomicUsize::new(0);
static ACCOUNT_WAKE: Completion = Completion::new();
static TASK_NS: AtomicU64 = AtomicU64::new(0);
static WALL_NS: AtomicU64 = AtomicU64::new(0);
static MIGRATIONS: AtomicUsize = AtomicUsize::new(0);
static CSD_ACTIONS: AtomicUsize = AtomicUsize::new(0);

struct KickSlot {
    task: SpinLock<Option<Arc<Task>>>,
    wake: Completion,
    rounds: AtomicUsize,
    timer_before: AtomicUsize,
    stop: AtomicBool,
}

impl KickSlot {
    /// 为每个绑定执行者准备通知和观察对象，不在提交提醒时分配。
    const fn new() -> Self {
        Self {
            task: SpinLock::new(None),
            wake: Completion::new(),
            rounds: AtomicUsize::new(0),
            timer_before: AtomicUsize::new(0),
            stop: AtomicBool::new(false),
        }
    }

    /// 克隆现存真实任务控制块，调度状态观察不保留测试锁。
    fn task(&self) -> Arc<Task> {
        Arc::clone(self.task.lock().as_ref().expect("kick task not published"))
    }
}

static KICKS: [KickSlot; 4] = [const { KickSlot::new() }; 4];

/// 以硬件 counter 限制不可调度观察段，不用时钟快照、yield 或同步 call 补进度。
fn spin_until(predicate: impl FnMut() -> bool, name: &str) {
    spin_until_before(predicate, raw_deadline(), name);
}

/// 从唯一单调时钟构造有限绝对期限。
fn deadline(duration: Duration) -> MonotonicInstant {
    clock::now()
        .checked_add(duration)
        .expect("time test deadline overflow")
}

/// 以硬件 counter 给跨 CPU 停点建立期限，等待期间不进入被暂停的序列快照。
fn raw_deadline() -> u64 {
    arch::read_counter()
        .checked_add(
            arch::counter_frequency()
                .checked_mul(5)
                .expect("raw timeout interval overflow"),
        )
        .expect("raw timeout deadline overflow")
}

/// 复用整轮绝对截止，合法跳过硬件等待的重试不能重置期限。
fn spin_until_before(mut predicate: impl FnMut() -> bool, end: u64, name: &str) {
    while !predicate() {
        assert!(
            arch::read_counter() < end,
            "time execution timeout: {}",
            name
        );
        spin_loop();
    }
}

/// 在普通任务保持运行权期间制造真实忙段，允许硬 IRQ 服务但不调用安全点。
fn busy(duration: Duration) {
    let end = deadline(duration);
    while clock::now() < end {
        black_box(arch::read_counter());
        spin_loop();
    }
}

/// 新任务的量子 IRQ 只能置请求，同核竞争者到普通安全点才取得运行权。
fn deferred_worker() -> i32 {
    assert_eq!(cpu::current().id, 0);
    busy(Duration::from_millis(20));
    let local = cpu::current();
    let peer = task::spawn_kernel_on(0, || {
        assert_eq!(cpu::current().id, 0);
        assert!(!PEER_RAN.swap(true, Ordering::AcqRel));
        101
    })
    .expect("reschedule peer allocation failed");
    assert!(!PEER_RAN.load(Ordering::Acquire));
    let before = {
        let _irq = IrqSaveGuard::new();
        // SAFETY: 当前 CPU0 普通任务持有本核 IRQ-save，未切换且未持有事件锁；
        // 唯一真实 clockevent 重武装以后在同一 IRQ-off 区间采 timer 基线。
        unsafe { clock::testing::rearm_current_quantum() };
        let before = local.timer_interrupts.load(Ordering::Acquire);
        local.need_resched.store(false, Ordering::Release);
        before
    };
    spin_until(
        || before < local.timer_interrupts.load(Ordering::Acquire),
        "task-quantum-irq",
    );
    assert!(!PEER_RAN.load(Ordering::Acquire));
    assert!(local.need_resched.load(Ordering::Acquire));
    assert_eq!(task::current_task_id(), Some(task::current_task().id()));
    let safe_points = usize::from(sched::cond_resched());
    assert_eq!(safe_points, 1);
    assert!(PEER_RAN.load(Ordering::Acquire));
    until(|| peer.try_join().is_some(), "reschedule-peer-return");
    assert_eq!(peer.join(), 101);
    // SAFETY: 当前绑定 CPU0 的普通任务不持锁；异常入口、永久 CpuLocal 和真实 timer
    // 已安装，受控整数测试不跨调度，退出恢复 IRQ 和当前 clockevent。
    let registers = unsafe { arch::test_integer_register_preservation() };
    assert!(if cfg!(target_arch = "riscv64") {
        2 <= registers
    } else {
        1 <= registers
    });
    let irq = local.timer_interrupts.load(Ordering::Acquire) - before;
    assert!(1 <= irq);
    crate::test_println!(
        "PASS execution-X10 irq={} deferred=1 safe_points={} registers={}",
        irq,
        safe_points,
        registers
    );
    110
}

/// 在同一生产固定点算术上核对有限差值、长 counter、回绕、分段余数与拒绝边界。
fn arithmetic_cases() -> (usize, usize) {
    let frequencies = [1_000_003u64, 10_000_000, 24_000_000, 1_000_000_000];
    let mut cases = 0usize;
    let mut fraction_seen = false;
    for frequency in frequencies {
        for (mask, initial) in [
            (u64::MAX, u64::MAX - 40),
            (0xffff, 0xfff0),
            (u64::MAX, 1u64 << 63),
        ] {
            let keeper = clock::testing::Timekeeper::new(frequency, mask, initial);
            let scale = keeper.scale();
            let delta = 100u64;
            let raw = initial.wrapping_add(delta) & mask;
            let exact = (u128::from(delta) * u128::from(scale.mult)) >> scale.shift;
            assert_eq!(keeper.read(|| initial).as_nanos(), 0);
            assert_eq!(keeper.read(|| raw).as_nanos(), exact as u64);
            assert_eq!(keeper.update(|| raw).as_nanos(), exact as u64);
            assert_eq!(keeper.read(|| raw).as_nanos(), exact as u64);
            let ideal = u128::from(delta) * 1_000_000_000 / u128::from(frequency);
            assert!(exact.abs_diff(ideal) <= (u128::from(delta) >> scale.shift) + 1);
            cases += 1;
        }
        let keeper = clock::testing::Timekeeper::new(frequency, u64::MAX, 1u64 << 63);
        let scale = keeper.scale();
        let mut previous = 0u64;
        for step in 1..=4096u64 {
            let fixed = u128::from(step) * u128::from(scale.mult);
            let expected = (fixed >> scale.shift) as u64;
            let raw = (1u64 << 63) + step;
            let read = keeper.read(|| raw).as_nanos();
            assert_eq!(read, expected);
            assert!(previous <= read);
            assert_eq!(keeper.update(|| raw).as_nanos(), read);
            assert_eq!(keeper.read(|| raw).as_nanos(), read);
            let fraction = (fixed & ((1u128 << scale.shift) - 1)) as u64;
            assert_eq!(keeper.fraction(), fraction);
            fraction_seen |= fraction != 0;
            previous = read;
        }
        cases += 1;
        let fixed = u128::from(scale.max_delta) * u128::from(scale.mult);
        let (ns, fraction) = scale
            .convert(scale.max_delta, 0)
            .expect("max delta rejected");
        assert_eq!(u128::from(ns), fixed >> scale.shift);
        assert_eq!(u128::from(fraction), fixed & ((1u128 << scale.shift) - 1));
        let residue = (1u64 << scale.shift) - 1;
        let fixed = fixed + u128::from(residue);
        let (ns, fraction) = scale
            .convert(scale.max_delta, residue)
            .expect("max delta with residue rejected");
        assert_eq!(u128::from(ns), fixed >> scale.shift);
        assert_eq!(u128::from(fraction), fixed & u128::from(residue));
        assert!(scale
            .convert(
                scale.max_delta.checked_add(1).expect("max delta overflow"),
                0
            )
            .is_none());
        cases += 1;
    }
    assert!(fraction_seen);
    (frequencies.len(), cases)
}

/// 每个真实 reader 核对同一 production now，SMP 时与另一 CPU 的维护写者重叠。
fn time_reader() -> i32 {
    TIME_ARRIVED.fetch_add(1, Ordering::Release);
    until(|| TIME_GO.load(Ordering::Acquire), "time-reader-start");
    let mut previous = clock::now();
    for round in 1..=READ_ROUNDS {
        until(
            || round <= WRITE_PHASE.load(Ordering::Acquire),
            "time-update-published",
        );
        let now = clock::now();
        assert!(previous <= now);
        READ_PROGRESS.fetch_add(1, Ordering::Release);
        previous = now;
        sched::yield_now();
    }
    READERS_PASSED.fetch_add(1, Ordering::Release);
    111
}

/// 真维护写者只推进唯一 production timekeeper，不另外累加每 CPU 时间。
fn time_writer() -> i32 {
    TIME_ARRIVED.fetch_add(1, Ordering::Release);
    until(|| TIME_GO.load(Ordering::Acquire), "time-writer-start");
    let mut previous = clock::now();
    let readers = if cpu::count() == 1 { 1 } else { 2 };
    for round in 1..=READ_ROUNDS {
        let now = clock::testing::maintain();
        assert!(previous <= now);
        previous = now;
        WRITE_PHASE.store(round, Ordering::Release);
        until(
            || {
                let now = clock::testing::maintain();
                assert!(previous <= now);
                previous = now;
                round * readers <= READ_PROGRESS.load(Ordering::Acquire)
            },
            "time-concurrent-readers",
        );
    }
    112
}

/// CPU0 真实读者暂停同一次 sample，由 CPU1 普通写者发布新一代并触发序号拒绝重试。
fn snapshot_overlap() -> (usize, usize) {
    if cpu::count() == 1 {
        return (0, 0);
    }
    assert_eq!(cpu::current().id, 0);
    let writer = task::spawn_kernel_on(1, || {
        assert_eq!(cpu::current().id, 1);
        spin_until(clock::testing::reader_paused, "snapshot-reader-paused");
        clock::testing::maintain();
        115
    })
    .expect("snapshot writer allocation failed");
    let result = {
        let _irq = IrqSaveGuard::new();
        let previous = clock::now();
        // SAFETY: 指定读者是本核 CPU0 普通任务且 IRQ 已关闭；已预备的 CPU1 写者
        // 可独立前进，读者不持写者所需锁，并在本区间完成 sample 和关闭握手。
        unsafe {
            clock::testing::arm_snapshot_overlap(
                task::current_task_id().expect("snapshot reader missing"),
                writer.task_id(),
            )
        };
        let now = clock::now();
        assert!(previous <= now);
        let result = clock::testing::snapshot_overlap_result();
        assert!(1 <= result.0 && 1 <= result.1);
        clock::testing::finish_snapshot_overlap();
        result
    };
    until(|| writer.try_join().is_some(), "snapshot-writer-return");
    assert_eq!(writer.join(), 115);
    result
}

/// 先核算同实现边界，再用真实 reader/writer Task 验证跨 CPU 序列快照。
fn test_timekeeper() {
    let (frequencies, cases) = arithmetic_cases();
    let before = clock::testing::updates();
    let (overlaps, retries) = snapshot_overlap();
    let readers = if cpu::count() == 1 { 1 } else { 2 };
    let writer = task::spawn_kernel_on(0, time_writer).expect("time writer allocation failed");
    let first = task::spawn_kernel_on(if cpu::count() == 1 { 0 } else { 1 }, time_reader)
        .expect("time reader allocation failed");
    let second = if readers == 2 {
        Some(
            task::spawn_kernel_on(cpu::count() - 1, time_reader)
                .expect("second time reader allocation failed"),
        )
    } else {
        None
    };
    until(
        || TIME_ARRIVED.load(Ordering::Acquire) == readers + 1,
        "time-task-arrival",
    );
    TIME_GO.store(true, Ordering::Release);
    until(
        || {
            writer.try_join().is_some()
                && first.try_join().is_some()
                && second
                    .as_ref()
                    .map_or(true, |handle| handle.try_join().is_some())
        },
        "time-readers-return",
    );
    assert_eq!(writer.join(), 112);
    assert_eq!(first.join(), 111);
    if let Some(second) = second {
        assert_eq!(second.join(), 111);
    }
    assert_eq!(READERS_PASSED.load(Ordering::Acquire), readers);
    let updates = clock::testing::updates() - before;
    assert!(READ_ROUNDS as u64 <= updates);
    crate::test_println!("PASS execution-X11 frequencies={} cases={} updates={} readers={} overlaps={} retries={} monotonic=1 wrap=1 fraction=1 max_delta=1", frequencies, cases, updates, readers, overlaps, retries);
}

/// 在同一 Task 的普通忙段、真 park 与迁核后分别结算累计执行时间。
fn accounting_worker() -> i32 {
    sched::test_migrate_to(0);
    let current = task::current_task();
    let id = current.id();
    let local = 0x7312usize;
    let stack = black_box(&local) as *const _ as usize;
    let old = ACCOUNT_TASK.lock().replace(Arc::clone(&current));
    assert!(old.is_none());
    let start = clock::now();
    let initial = accounting::current_execution_time();
    busy(Duration::from_millis(20));
    ACCOUNT_PHASE.store(1, Ordering::Release);
    ACCOUNT_WAKE.wait();
    assert_eq!(
        current.id(),
        task::current_task_id().expect("account task missing")
    );
    let mut previous = accounting::current_execution_time();
    assert!(initial < previous);
    if 1 < cpu::count() {
        for target in [1usize, 0] {
            let hop_start = clock::now();
            let hop_execution = accounting::current_execution_time();
            sched::test_migrate_to(target);
            assert_eq!(cpu::current().id, target);
            assert_eq!(arch::cpu_local(), cpu::get(target) as *const _);
            assert_eq!(task::current_task_id(), Some(id));
            assert_eq!(black_box(&local) as *const _ as usize, stack);
            assert_eq!(local, 0x7312);
            busy(Duration::from_millis(2));
            let execution = accounting::current_execution_time();
            let hop_end = clock::now();
            assert!(
                execution
                    .checked_sub(hop_execution)
                    .expect("migration accounting regressed")
                    <= hop_end
                        .checked_duration_since(hop_start)
                        .expect("migration wall regressed")
            );
            assert!(previous < execution);
            previous = execution;
            MIGRATIONS.fetch_add(1, Ordering::Relaxed);
        }
    }
    let execution = accounting::current_execution_time()
        .checked_sub(initial)
        .expect("task accounting regressed");
    let end = clock::now();
    let wall = end
        .checked_duration_since(start)
        .expect("wall time regressed");
    assert!(Duration::ZERO < execution && execution < wall);
    TASK_NS.store(
        u64::try_from(execution.as_nanos()).expect("task time overflow"),
        Ordering::Release,
    );
    WALL_NS.store(
        u64::try_from(wall.as_nanos()).expect("wall time overflow"),
        Ordering::Release,
    );
    113
}

/// 返回已经发布的计账 Task，不保留测试锁跨观察或通知。
fn account_task() -> Arc<Task> {
    Arc::clone(
        ACCOUNT_TASK
            .lock()
            .as_ref()
            .expect("account task not published"),
    )
}

/// 绑定 Task 真等待后由远端 ready 提醒执行，每轮核对目标 timer IRQ 未提供进度。
fn kick_worker(target: usize) -> i32 {
    let slot = &KICKS[target];
    let current = task::current_task();
    let old = slot.task.lock().replace(current);
    assert!(old.is_none());
    loop {
        slot.wake.wait();
        if slot.stop.load(Ordering::Acquire) {
            break;
        }
        assert_eq!(cpu::current().id, target);
        assert_eq!(task::current_task_id(), Some(slot.task().id()));
        assert_eq!(
            cpu::current().timer_interrupts.load(Ordering::Acquire),
            slot.timer_before.load(Ordering::Acquire)
        );
        slot.rounds.fetch_add(1, Ordering::Release);
    }
    114
}

/// 入口计数只证明 idle 准备，真实 ISA 等待必须在本轮唤醒后核对完成数。
fn remote_idle(target: usize, after: usize, end: u64) {
    let slot = &KICKS[target];
    spin_until_before(
        || {
            let published = slot.task.lock().is_some();
            published
                && sched::test_state(&slot.task()) == State::Blocked
                && cpu::get(target).idle.load(Ordering::Acquire)
                && after < cpu::test_idle_waits(target)
        },
        end,
        "remote-task-idle",
    );
}

/// CSD 回调只在真实目标硬 IRQ 写入动作计数，完成依旧由原 release 协议返回。
///
/// # Safety
/// 参数必须为空；调用者必须通过同步 cpu::call 等待动作完成。
unsafe fn csd_action(argument: *mut ()) {
    assert!(argument.is_null());
    assert!(0 < cpu::current().hardirq_depth.load(Ordering::Relaxed));
    CSD_ACTIONS.fetch_add(1, Ordering::Release);
}

/// 提交一轮真实 Completion ready 提醒，唤醒后返回本轮实际硬等待完成增量。
fn kick_attempt(target: usize, attempt: usize, after: usize, end: u64) -> (usize, usize) {
    remote_idle(target, after, end);
    let local = cpu::get(target);
    let before = local.timer_interrupts.load(Ordering::Acquire);
    spin_until_before(
        || !local.kick_pending() && !local.need_resched.load(Ordering::Acquire),
        end,
        "idle-pending-drained",
    );
    busy(Duration::from_millis(1));
    assert!(local.idle.load(Ordering::Acquire));
    assert_eq!(local.timer_interrupts.load(Ordering::Acquire), before);
    let idle = cpu::test_idle_waits(target);
    if attempt == 2 {
        cpu::test_arm_pause(target, cpu::TestPause::Idle);
        cpu::kick(target, cpu::KICK_RESCHEDULE);
        spin_until_before(
            || cpu::test_is_paused(target, cpu::TestPause::Idle),
            end,
            "pending-before-idle",
        );
    }
    if attempt == 3 {
        let reprograms = cpu::test_timer_reprograms(target);
        cpu::test_arm_pause(target, cpu::TestPause::Ipi);
        cpu::kick(target, cpu::KICK_RESCHEDULE);
        spin_until_before(
            || cpu::test_is_paused(target, cpu::TestPause::Ipi),
            end,
            "ipi-processing-pause",
        );
        cpu::kick(target, cpu::KICK_TIMER_REPROGRAM);
        assert_eq!(
            cpu::test_pending_kicks(target) & cpu::KICK_TIMER_REPROGRAM,
            cpu::KICK_TIMER_REPROGRAM
        );
        cpu::test_release_pause(target, cpu::TestPause::Ipi);
        spin_until_before(
            || reprograms < cpu::test_timer_reprograms(target),
            end,
            "ipi-new-reason",
        );
        spin_until_before(
            || local.idle.load(Ordering::Acquire),
            end,
            "ipi-return-idle",
        );
    }
    let waits_before = cpu::test_wait_completions(target);
    KICKS[target].timer_before.store(before, Ordering::Release);
    KICKS[target].wake.complete();
    if attempt == 2 {
        cpu::test_release_pause(target, cpu::TestPause::Idle);
    }
    spin_until_before(
        || KICKS[target].rounds.load(Ordering::Acquire) == attempt,
        end,
        "remote-ready-kick",
    );
    assert_eq!(local.timer_interrupts.load(Ordering::Acquire), before);
    let waits = cpu::test_wait_completions(target)
        .checked_sub(waits_before)
        .expect("idle completion count regressed");
    (idle, waits)
}

/// 先备所有绑定执行者，每个远端在有限截止内核验四轮真硬等待，再停止并 join。
fn test_idle_kicks() -> (usize, usize) {
    if cpu::count() == 1 {
        return (0, 0);
    }
    assert_eq!(cpu::current().id, 0);
    let before_idle: [usize; 4] = core::array::from_fn(|id| {
        if id < cpu::count() {
            cpu::test_idle_waits(id)
        } else {
            0
        }
    });
    let mut handles: [Option<task::JoinHandle>; 4] = core::array::from_fn(|id| {
        if 0 < id && id < cpu::count() {
            Some(
                task::spawn_kernel_on(id, move || kick_worker(id))
                    .expect("idle kick task allocation failed"),
            )
        } else {
            None
        }
    });
    let mut rounds = 0usize;
    let mut waits = 0usize;
    for target in 1..cpu::count() {
        let end = raw_deadline();
        let mut idle = before_idle[target];
        remote_idle(target, idle, end);
        let timer_before = cpu::get(target).timer_interrupts.load(Ordering::Acquire);
        let mut attempt = 0usize;
        let mut verified = 0usize;
        let mut completed_waits = 0usize;
        while verified < KICK_ROUNDS || attempt < 3 {
            assert!(
                arch::read_counter() < end,
                "hard idle wait timeout: cpu={}",
                target
            );
            attempt = attempt.checked_add(1).expect("idle attempt count overflow");
            let (entered, completed) = kick_attempt(target, attempt, idle, end);
            idle = entered;
            completed_waits += completed;
            // 两个停点轮次验证通知协议；其中新增原因可能先唤醒 ISA，不计入 ready 硬等待。
            if 0 < completed && attempt != 2 && attempt != 3 && verified < KICK_ROUNDS {
                verified += 1;
            }
        }
        assert_eq!(verified, KICK_ROUNDS);
        assert!(KICK_ROUNDS <= completed_waits);
        assert_eq!(
            cpu::get(target).timer_interrupts.load(Ordering::Acquire),
            timer_before
        );
        rounds += verified;
        waits += completed_waits;
        KICKS[target].stop.store(true, Ordering::Release);
        KICKS[target].wake.complete();
        let handle = handles[target].take().expect("idle kick handle missing");
        until(|| handle.try_join().is_some(), "kick-task-return");
        assert_eq!(handle.join(), 114);
        let actions = CSD_ACTIONS.load(Ordering::Acquire);
        // SAFETY: 动作不解引用参数且不睡眠；当前 CPU0 Task IRQ 可服务且不持锁，
        // 原同步 CSD 协议等待远端动作的 release 完成后才返回。
        unsafe { cpu::call(target, csd_action, core::ptr::null_mut()) };
        assert_eq!(CSD_ACTIONS.load(Ordering::Acquire), actions + 1);
        let observer = KICKS[target].task.lock().take();
        drop(observer);
    }
    (rounds, waits)
}

/// 用真实 CPU 四账和同 Task 的受控 offCPU 区间验证执行时间排除阻塞和迁核间隙。
fn test_accounting() {
    let before: [accounting::CpuSnapshot; 4] =
        core::array::from_fn(|id| accounting::snapshot(if id < cpu::count() { id } else { 0 }));
    let handle = task::spawn_kernel(accounting_worker).expect("account task allocation failed");
    until(
        || {
            ACCOUNT_PHASE.load(Ordering::Acquire) == 1
                && sched::test_state(&account_task()) == State::Blocked
        },
        "account-task-parked",
    );
    let observer = account_task();
    let execution = observer.execution_time();
    let blocked_start = clock::now();
    busy(Duration::from_millis(15));
    assert_eq!(observer.execution_time(), execution);
    let offcpu_ns = clock::now()
        .checked_duration_since(blocked_start)
        .expect("offCPU time regressed")
        .as_nanos() as u64;
    assert!(0 < offcpu_ns);
    ACCOUNT_WAKE.complete();
    until(|| handle.try_join().is_some(), "account-task-return");
    assert_eq!(handle.join(), 113);
    let task_ns = TASK_NS.load(Ordering::Acquire);
    let wall_ns = WALL_NS.load(Ordering::Acquire);
    assert!(0 < task_ns && task_ns < wall_ns);
    assert!(task_ns.checked_add(offcpu_ns).expect("interval overflow") <= wall_ns);
    let (kick_rounds, waits) = test_idle_kicks();
    let mut irq_ns = 0u64;
    let mut scheduler_ns = 0u64;
    let mut idle_ns = 0u64;
    for id in 0..cpu::count() {
        let after = accounting::snapshot(id);
        irq_ns += after
            .irq_ns
            .checked_sub(before[id].irq_ns)
            .expect("IRQ accounting regressed");
        scheduler_ns += after
            .scheduler_ns
            .checked_sub(before[id].scheduler_ns)
            .expect("scheduler accounting regressed");
        idle_ns += after
            .idle_ns
            .checked_sub(before[id].idle_ns)
            .expect("idle accounting regressed");
    }
    assert!(0 < irq_ns && 0 < scheduler_ns);
    let migrations = MIGRATIONS.load(Ordering::Acquire);
    let no_tick = if cpu::count() == 1 {
        assert_eq!((migrations, kick_rounds, waits), (0, 0, 0));
        0
    } else {
        assert!(2 <= migrations && 4 <= kick_rounds && kick_rounds <= waits && 0 < idle_ns);
        1
    };
    crate::test_println!("PASS execution-X12 task_ns={} wall_ns={} offcpu_ns={} irq_ns={} scheduler_ns={} idle_ns={} migrations={} kick_rounds={} waits={} no_tick={}", task_ns, wall_ns, offcpu_ns, irq_ns, scheduler_ns, idle_ns, migrations, kick_rounds, waits, no_tick);
    let published = ACCOUNT_TASK.lock().take();
    drop((published, observer));
}

/// 仅正常 execution 镜像顺序追加时间、执行账与远端 idle 提醒验收。
pub(crate) fn run() {
    let initial = baseline();
    let deferred =
        task::spawn_kernel_on(0, deferred_worker).expect("deferred task allocation failed");
    until(|| deferred.try_join().is_some(), "deferred-task-return");
    assert_eq!(deferred.join(), 110);
    test_timekeeper();
    test_accounting();
    assert!(ACCOUNT_TASK.lock().is_none());
    assert!(KICKS.iter().all(|slot| slot.task.lock().is_none()));
    assert_eq!(ACCOUNT_WAKE.test_waiting(), 0);
    assert!(!ACCOUNT_WAKE.is_ready());
    for slot in &KICKS {
        assert_eq!(slot.wake.test_waiting(), 0);
        assert!(!slot.wake.is_ready());
    }
    assert_eq!(reclaimed(initial), 1);
}
