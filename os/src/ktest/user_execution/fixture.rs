//! 用户执行用例的原 Task、停止源与离栈后资源拥有。

use alloc::sync::{Arc, Weak};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::ktest::execution_core::{baseline, until, Baseline};
use crate::ktest::execution_services::SuiteBaseline;
use crate::ktest::{prepare_result, ChildHandle, ChildToken};
use crate::sync::{Completion, SleepRwLock, SpinLock};
use crate::task::UserThread;
use crate::{sched, task};
use ::ktest::{Fixture, TestContext, TestError, TestResult};

pub(super) struct Users {
    initial: Baseline,
    stop: Arc<AtomicBool>,
    handles: [Option<ChildHandle>; 4],
    controls: Option<Arc<Controls>>,
}

impl Fixture<SuiteBaseline> for Users {
    /// 页表与执行者准备前保存停稳资源，每例只拥有自己的停止和离栈观察对象。
    fn set_up(_: &mut TestContext<'_>, _: &SuiteBaseline) -> Result<Self, TestError> {
        let initial = baseline()?;
        let stop = Arc::try_new(AtomicBool::new(false))
            .map_err(|_| TestError("NoMemory: user execution controls"))?;
        Ok(Self {
            initial,
            stop,
            handles: core::array::from_fn(|_| None),
            controls: None,
        })
    }

    /// 停止原普通 Task 并通知本例真实等待源；用户只在返回 Rust 后消费停止，离栈前保留全部拥有。
    fn quiesce(&mut self, _: &mut TestContext<'_>) -> TestResult {
        self.stop.store(true, Ordering::Release);
        if let Some(controls) = &self.controls {
            for gate in &controls.gates {
                gate.complete();
            }
        }
        until(
            || {
                self.handles
                    .iter()
                    .flatten()
                    .all(|handle| handle.try_join().is_some())
            },
            "user execution did not leave its original Task stack",
        )
    }

    /// 原框架 join 后释放观察与控制对象，记录并比较完整六域和任务元数据。
    fn tear_down(self, context: &mut TestContext<'_>) -> TestResult {
        let Self {
            initial,
            stop,
            handles,
            controls,
        } = self;
        assert!(handles
            .iter()
            .flatten()
            .all(|handle| handle.try_join().is_some()));
        drop(handles);
        drop(stop);
        drop(controls);
        let actual = baseline()?;
        context.diagnostic(format_args!(
            "USER RESOURCES initial={:?} final={:?}",
            initial, actual
        ));
        if actual == initial {
            Ok(())
        } else {
            Err(TestError("user execution resources did not converge"))
        }
    }
}

impl Users {
    /// 先绑定私有用户对象再用原 scope 发表，同一槽的上次执行者必须已经离栈。
    pub(super) fn start(
        &mut self,
        slot: usize,
        user: Arc<UserThread>,
        body: impl FnOnce(Arc<UserThread>, ChildToken, Arc<AtomicBool>) -> Result<i32, TestError>
            + Send
            + 'static,
    ) -> TestResult {
        assert!(self.handles[slot]
            .as_ref()
            .is_none_or(|handle| handle.try_join().is_some()));
        let owned = Arc::clone(&user);
        let stop = Arc::clone(&self.stop);
        let prepared = prepare_result(move |token| body(owned, token, stop))?;
        prepared.bind_user(user);
        self.handles[slot] = Some(prepared.publish()?);
        Ok(())
    }

    /// 原 JoinState 的实际返回值只能在任务离栈后观察，子错误仍由 scope 单独报告。
    pub(super) fn joined(&self, slot: usize, expected: i32) -> TestResult {
        let handle = self.handles[slot]
            .as_ref()
            .expect("user child was not published");
        until(|| handle.try_join().is_some(), "user child did not finish")?;
        assert_eq!(handle.try_join(), Some(expected));
        Ok(())
    }

    /// 睡眠锁参与者沿同一 scope 发表普通 Task，观察槽仍由 fixture 停止和原 join 收回。
    pub(super) fn start_task(
        &mut self,
        slot: usize,
        body: impl FnOnce(ChildToken, Arc<AtomicBool>) -> Result<i32, TestError> + Send + 'static,
    ) -> TestResult {
        assert!(self.handles[slot]
            .as_ref()
            .is_none_or(|handle| handle.try_join().is_some()));
        let stop = Arc::clone(&self.stop);
        self.handles[slot] = Some(prepare_result(move |token| body(token, stop))?.publish()?);
        Ok(())
    }
}

pub(super) struct Controls {
    pub(super) gates: [Completion; 4],
    pub(super) ready: [AtomicBool; 4],
    tasks: [SpinLock<Option<Weak<task::Task>>>; 4],
    pub(super) phase: AtomicUsize,
    pub(super) lock: SleepRwLock<u64>,
}

impl Controls {
    /// 保存每例真实等待源与弱任务观察，停止时只通知原参与者，不取得执行或离栈责任。
    pub(super) fn new(fixture: &mut Users) -> Result<Arc<Self>, TestError> {
        let controls = Arc::try_new(Self {
            gates: core::array::from_fn(|_| Completion::new()),
            ready: core::array::from_fn(|_| AtomicBool::new(false)),
            tasks: core::array::from_fn(|_| SpinLock::new(None)),
            phase: AtomicUsize::new(0),
            lock: SleepRwLock::new(0x9817),
        })
        .map_err(|_| TestError("NoMemory: user VM controls"))?;
        fixture.controls = Some(Arc::clone(&controls));
        Ok(controls)
    }

    /// 在当前普通 Task 发布弱身份；观察方只能沿调度器读取实际 Blocked 状态。
    pub(super) fn record(&self, slot: usize) {
        *self.tasks[slot].lock() = Some(Arc::downgrade(&task::current_task()));
    }

    /// 释放观察短锁后升级弱引用，真实调度状态决定等待是否已交出执行权。
    pub(super) fn blocked(&self, slot: usize) -> bool {
        let task = self.tasks[slot]
            .lock()
            .clone()
            .and_then(|task| task.upgrade());
        task.is_some_and(|task| sched::test_state(&task) == task::State::Blocked)
    }
}
