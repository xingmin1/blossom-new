//! 原 Task 上的寄存器、深栈、正常返回、离栈观察与迁核身份。

use crate::ktest::execution_core::{baseline, reclaimed, until, Baseline};
use crate::ktest::{spawn_result, spawn_result_on, ChildHandle, ChildToken};
use crate::sync::{Completion, SpinLock};
use crate::{arch, cpu, mm, sched, task};
use ::ktest::{Fixture, TestContext, TestError, TestResult};
use alloc::sync::Arc;
use core::hint::black_box;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

const ROUNDS: usize = 8;
const DEPTH: usize = 8;
const STACK_BYTES: usize = 64 * 1024;

struct Control {
    go: AtomicBool,
    stop: AtomicBool,
    arrived: AtomicUsize,
    turn: AtomicUsize,
    stacks: [AtomicUsize; 2],
    bases: [AtomicUsize; 2],
    ids: [AtomicUsize; 2],
    starts: [AtomicUsize; 2],
    steps: [AtomicUsize; 2],
    active: [AtomicUsize; 2],
    drops: [AtomicUsize; 2],
    frames: AtomicUsize,
    external: SpinLock<Option<Arc<task::Task>>>,
    exit: Completion,
}
struct Tasks {
    initial: Baseline,
    state: Arc<Control>,
    handles: [Option<ChildHandle>; 2],
}
impl Fixture for Tasks {
    /// 在发表真实 Task 前建立每例独立控制拥有与本次停稳基线。
    fn set_up(_: &mut TestContext<'_>, _: &()) -> Result<Self, TestError> {
        let initial = baseline()?;
        let state = Arc::try_new(Control {
            go: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            arrived: AtomicUsize::new(0),
            turn: AtomicUsize::new(0),
            stacks: core::array::from_fn(|_| AtomicUsize::new(0)),
            bases: core::array::from_fn(|_| AtomicUsize::new(0)),
            ids: core::array::from_fn(|_| AtomicUsize::new(0)),
            starts: core::array::from_fn(|_| AtomicUsize::new(0)),
            steps: core::array::from_fn(|_| AtomicUsize::new(0)),
            active: core::array::from_fn(|_| AtomicUsize::new(0)),
            drops: core::array::from_fn(|_| AtomicUsize::new(0)),
            frames: AtomicUsize::new(0),
            external: SpinLock::new(None),
            exit: Completion::new(),
        })
        .map_err(|_| TestError("NoMemory: context controls"))?;
        Ok(Self {
            initial,
            state,
            handles: core::array::from_fn(|_| None),
        })
    }
    /// 失败也开入口门、结束交替等待并放行退出；不释放仍在使用的原 owner。
    fn quiesce(&mut self, _: &mut TestContext<'_>) -> TestResult {
        self.state.stop.store(true, Ordering::Release);
        self.state.go.store(true, Ordering::Release);
        self.state.exit.complete();
        until(
            || {
                self.handles
                    .iter()
                    .flatten()
                    .all(|handle| handle.try_join().is_some())
            },
            "context stop did not reach original off-stack completion",
        )?;
        if self.state.exit.test_waiting() != 0 {
            return Err(TestError("exit source retains live stack nodes"));
        }
        Ok(())
    }
    /// 框架已经原 join 离栈，移出观察者与完成源后核对全部实际资源。
    fn tear_down(self, _: &mut TestContext<'_>) -> TestResult {
        let Self {
            initial,
            state,
            handles,
        } = self;
        if handles
            .iter()
            .flatten()
            .any(|handle| handle.try_join().is_none())
        {
            return Err(TestError("context Task still on execution stack"));
        }
        let external = state.external.lock().take();
        drop(external);
        drop(handles);
        while state.exit.try_wait() {}
        if state.exit.test_waiting() != 0 {
            return Err(TestError("exit source retains wait nodes"));
        }
        drop(state);
        reclaimed(initial)
    }
}
struct LocalDrop {
    state: Arc<Control>,
    slot: usize,
}
impl Drop for LocalDrop {
    /// 只在原入口正常返回或受控子捕获展开时析构一次局部拥有。
    fn drop(&mut self) {
        assert_eq!(
            self.state.drops[self.slot].fetch_add(1, Ordering::AcqRel),
            0
        );
    }
}

/// 递归帧跨真实汇编切换保持，逐层验证返回地址下的原内容。
#[inline(never)]
fn deep_calls(
    state: &Control,
    token: &ChildToken,
    slot: usize,
    depth: usize,
    root: usize,
) -> Result<usize, TestError> {
    let pattern = 0x51a0 + slot * 32 + depth;
    let frame = [pattern; 64];
    let address = black_box(&frame) as *const _ as usize;
    assert_eq!(address & !(STACK_BYTES - 1), root & !(STACK_BYTES - 1));
    let frames = if depth == 0 {
        assert!(address < root, "deep task stack did not descend");
        state.arrived.fetch_add(1, Ordering::Release);
        until(
            || state.go.load(Ordering::Acquire) || token.stop_requested(),
            "task-start-gate",
        )?;
        for round in 0..ROUNDS {
            until(
                || {
                    state.turn.load(Ordering::Acquire) == slot
                        || state.stop.load(Ordering::Acquire)
                        || token.stop_requested()
                },
                "task-turn",
            )?;
            if state.stop.load(Ordering::Acquire) || token.stop_requested() {
                break;
            }
            assert_eq!(state.active[slot].fetch_add(1, Ordering::AcqRel), 0);
            assert_eq!(cpu::current().id, 0);
            assert_eq!(
                task::current_task().id().as_usize(),
                state.ids[slot].load(Ordering::Acquire)
            );
            assert!(
                arch::test_task_context_preservation(),
                "task register preservation failed"
            );
            assert_eq!(state.steps[slot].fetch_add(1, Ordering::AcqRel), round);
            assert_eq!(state.active[slot].fetch_sub(1, Ordering::AcqRel), 1);
            state.turn.store(1 - slot, Ordering::Release);
            sched::yield_now();
        }
        1
    } else {
        deep_calls(state, token, slot, depth - 1, root)? + 1
    };
    assert!(black_box(&frame).iter().all(|value| *value == pattern));
    state.frames.fetch_add(1, Ordering::Relaxed);
    Ok(frames)
}

/// 真实入口记录其原栈块、身份和一次正常 i32 返回值。
fn switch_worker(state: Arc<Control>, token: ChildToken, slot: usize) -> Result<i32, TestError> {
    assert_eq!(state.starts[slot].fetch_add(1, Ordering::AcqRel), 0);
    let local = LocalDrop {
        state: Arc::clone(&state),
        slot,
    };
    let root = black_box(&local) as *const _ as usize;
    let (base, bytes) = task::test_current_stack_range();
    assert_eq!(bytes, STACK_BYTES);
    assert_eq!(base % bytes, 0);
    let physical_root = arch::virt_to_phys(root);
    assert!(base <= physical_root && physical_root < base + bytes);
    state.bases[slot].store(base, Ordering::Release);
    state.stacks[slot].store(root, Ordering::Release);
    state.ids[slot].store(task::current_task().id().as_usize(), Ordering::Release);
    assert_eq!(deep_calls(&state, &token, slot, DEPTH, root)?, DEPTH + 1);
    Ok(20 + slot as i32)
}

/// 描述符属于原可分配物理范围且没有被 SLUB 接管；不声称该getter表示buddy空闲。
fn check_stack_pages(base: usize) {
    for pfn in base / 4096..base / 4096 + STACK_BYTES / 4096 {
        let descriptor = mm::pages()
            .get(pfn)
            .expect("task stack has no page descriptor");
        assert!(mm::pages()
            .free_ranges()
            .iter()
            .any(|range| range.start <= pfn && pfn < range.end));
        assert_eq!(descriptor.slab_head.load(Ordering::Acquire), usize::MAX);
    }
}

/// 三页账的总量只用于活栈释放差，最终仍逐字段比较六域。
fn page_total() -> usize {
    let stats = mm::stats();
    stats.buddy_free_pages + stats.pcp_cached_pages + stats.slab_backing_pages
}

#[::ktest::suite(fixture = Tasks, tags("execution", "context"), timeout_ms = 15000)]
pub(crate) mod context {
    use super::*;
    /// 两个固定CPU0 Task独立深栈交替并保留原20/21观察结果，离栈后归还32页。
    #[test]
    fn deep_switch_return(_: &mut TestContext<'_>, fixture: &mut Tasks) -> TestResult {
        sched::test_migrate_to(0);
        for slot in 0..2 {
            let state = Arc::clone(&fixture.state);
            fixture.handles[slot] = Some(spawn_result_on(0, move |token| {
                switch_worker(state, token, slot)
            })?);
        }
        let state = &fixture.state;
        until(
            || state.arrived.load(Ordering::Acquire) == 2,
            "task-deep-arrival",
        )?;
        let stacks =
            core::array::from_fn::<_, 2, _>(|slot| state.stacks[slot].load(Ordering::Acquire));
        let bases =
            core::array::from_fn::<_, 2, _>(|slot| state.bases[slot].load(Ordering::Acquire));
        let ids = core::array::from_fn::<_, 2, _>(|slot| state.ids[slot].load(Ordering::Acquire));
        assert_ne!(ids[0], ids[1]);
        for slot in 0..2 {
            assert_eq!(
                fixture.handles[slot].as_ref().unwrap().task_id().as_usize(),
                ids[slot]
            );
        }
        assert!(
            STACK_BYTES
                <= (stacks[0] & !(STACK_BYTES - 1)).abs_diff(stacks[1] & !(STACK_BYTES - 1))
        );
        assert!(STACK_BYTES <= bases[0].abs_diff(bases[1]));
        bases.iter().copied().for_each(check_stack_pages);
        let live = page_total();
        state.go.store(true, Ordering::Release);
        until(
            || {
                fixture
                    .handles
                    .iter()
                    .flatten()
                    .all(|handle| handle.try_join().is_some())
            },
            "task-return",
        )?;
        assert_eq!(fixture.handles[0].as_ref().unwrap().try_join(), Some(20));
        assert_eq!(fixture.handles[1].as_ref().unwrap().try_join(), Some(21));
        assert_eq!(page_total(), live + 32);
        for slot in 0..2 {
            assert_eq!(state.starts[slot].load(Ordering::Acquire), 1);
            assert_eq!(state.steps[slot].load(Ordering::Acquire), ROUNDS);
            assert_eq!(state.active[slot].load(Ordering::Acquire), 0);
            assert_eq!(state.drops[slot].load(Ordering::Acquire), 1);
        }
        assert_eq!(state.frames.load(Ordering::Acquire), 2 * (DEPTH + 1));
        bases.iter().copied().for_each(check_stack_pages);
        Ok(())
    }
    /// 外部原Task Arc和完成观察者仍存活时，真实结果36已经晚于栈16页归还。
    #[test]
    fn join_exit_observers(_: &mut TestContext<'_>, fixture: &mut Tasks) -> TestResult {
        sched::test_migrate_to(0);
        let state = Arc::clone(&fixture.state);
        fixture.handles[0] = Some(spawn_result_on(0, move |_| {
            let _local = LocalDrop {
                state: Arc::clone(&state),
                slot: 0,
            };
            assert!(state
                .external
                .lock()
                .replace(task::current_task())
                .is_none());
            state.arrived.store(1, Ordering::Release);
            state.exit.wait();
            Ok((1..=8).sum())
        })?);
        until(
            || {
                fixture.state.arrived.load(Ordering::Acquire) == 1
                    && fixture
                        .state
                        .external
                        .lock()
                        .as_ref()
                        .is_some_and(|task| sched::test_state(task) == task::State::Blocked)
            },
            "exit-worker-blocked",
        )?;
        let live = page_total();
        fixture.state.exit.complete();
        let handle = fixture.handles[0].as_ref().unwrap();
        until(|| handle.try_join().is_some(), "wait-exit-result")?;
        let external = fixture
            .state
            .external
            .lock()
            .take()
            .expect("exit task Arc not retained");
        assert_eq!(sched::test_state(&external), task::State::Exited);
        assert_eq!(Arc::strong_count(&external), 1);
        assert_eq!(
            Arc::strong_count(&external) + handle.try_join().is_some() as usize,
            2
        );
        assert_eq!(fixture.state.drops[0].load(Ordering::Acquire), 1);
        assert_eq!(page_total().checked_sub(live), Some(16));
        assert_eq!(handle.join()?, (1..=8).sum::<i32>());
        drop(external);
        Ok(())
    }
}

#[::ktest::suite(fixture = Tasks, tags("execution", "migration"), min_cpus = 2, timeout_ms = 15000)]
pub(crate) mod migration {
    use super::*;
    /// 同一真实Any子Task往返全部在线核两轮，身份、硬件CPU基址与栈局部地址不变，原join返回31。
    #[test]
    fn same_task_round_trip(_: &mut TestContext<'_>, fixture: &mut Tasks) -> TestResult {
        let state = Arc::clone(&fixture.state);
        fixture.handles[0] = Some(spawn_result(move |token| {
            let local = 0x7301usize;
            let stack = black_box(&local) as *const _ as usize;
            let id = task::current_task().id().as_usize();
            state.ids[0].store(id, Ordering::Release);
            for hop in 0..=2 * cpu::count() {
                if state.stop.load(Ordering::Acquire) || token.stop_requested() {
                    return Ok(0);
                }
                let target = hop % cpu::count();
                sched::test_migrate_to(target);
                assert_eq!(cpu::current().id, target);
                assert_eq!(arch::cpu_local(), cpu::get(target) as *const _);
                assert_eq!(cpu::current() as *const _, arch::cpu_local());
                assert_eq!(task::current_task().id().as_usize(), id);
                assert_eq!(task::current_task_id().map(|id| id.as_usize()), Some(id));
                assert_eq!(black_box(&local) as *const _ as usize, stack);
                assert_eq!(*black_box(&local), 0x7301);
            }
            Ok(31)
        })?);
        let handle = fixture.handles[0].as_ref().unwrap();
        until(|| handle.try_join().is_some(), "task-migration-return")?;
        assert_eq!(handle.try_join(), Some(31));
        assert_eq!(handle.join()?, 31);
        assert_eq!(
            handle.task_id().as_usize(),
            fixture.state.ids[0].load(Ordering::Acquire)
        );
        Ok(())
    }
}

#[::ktest::suite(tags("execution", "fatal"), isolation = "Case", timeout_ms = 5000)]
pub(crate) mod fatal {
    use super::*;
    /// 原IRQ-save guard下直接调用真实调度前提；只能由irq-schedule真实fatal终结独立VM。
    #[test]
    #[ktest(expected_fatal = "irq-schedule")]
    fn irq_guard_schedule() {
        assert!(task::current_task_id().is_some());
        let _guard = crate::sync::IrqSaveGuard::new();
        sched::yield_now();
        panic!("execution fault crossed prohibited switch boundary");
    }
    /// 原Preempt guard下的集中安全边界必须给出preempt-schedule，其他panic不能满足此例。
    #[test]
    #[ktest(expected_fatal = "preempt-schedule")]
    fn preempt_guard_schedule() {
        assert!(task::current_task_id().is_some());
        let _guard = crate::sync::PreemptGuard::new();
        sched::yield_now();
        panic!("execution fault crossed prohibited switch boundary");
    }
}
