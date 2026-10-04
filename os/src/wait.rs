//! 一次等待只拥有当前 Task 的现存控制块；通知通过弱引用核对唯一代次。

pub mod queue;

pub use queue::{QueueRegistration, WaitNode, WaitQueue};

use alloc::sync::{Arc, Weak};
use core::marker::PhantomData;

use crate::sched;
use crate::task::{self, Task};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WaitId(usize);

impl WaitId {
    /// 新任务从未使用的零序号仅作为下一次等待的初始值。
    pub(crate) const fn initial() -> Self {
        Self(0)
    }

    /// 每次开始等待取得新身份，耗尽后直接暴露错误而不重用旧代次。
    pub(crate) fn next(self) -> Self {
        Self(self.0.checked_add(1).expect("wait ID space exhausted"))
    }
}

/// 等待拥有者不能转交其他 Task，但可随当前 Task 迁移到另一 CPU。
pub struct Waiter {
    task: Arc<Task>,
    id: WaitId,
    _task_local: PhantomData<*mut ()>,
}

impl Waiter {
    /// 在合法普通 Task 中开始唯一等待，克隆现存控制块而不分配等待对象。
    pub fn new() -> Self {
        sched::assert_switchable();
        let task = task::current_task();
        let id = sched::begin_wait(&task);
        Self {
            task,
            id,
            _task_local: PhantomData,
        }
    }

    /// 返回本次等待身份，供通知来源区分已结束的旧轮次。
    pub fn id(&self) -> WaitId {
        self.id
    }

    /// 创建不保活任务或执行栈的通知引用，不分配新的控制块。
    pub fn waker(&self) -> Waker {
        Waker {
            task: Arc::downgrade(&self.task),
            id: self.id,
        }
    }

    /// 只允许原 Task 阻塞本轮等待，已经接受通知时立即返回。
    pub fn park(&self) {
        assert_eq!(task::current_task_id(), Some(self.task.id()));
        sched::park(self.id);
    }

    /// 真实提交 SwitchingOut 后通知，在保存 ctx 前验证没有提前发表 ready。
    #[cfg(any(feature = "execution-tests", feature = "ktest"))]
    pub fn test_park_notify(&self) {
        assert_eq!(task::current_task_id(), Some(self.task.id()));
        let waker = self.waker();
        sched::test_park_notify(&self.task, self.id, &waker);
    }
}

impl Drop for Waiter {
    /// 唯一清理责任只清本轮 active wait，强引用随后在调度锁外析构。
    fn drop(&mut self) {
        sched::end_wait(&self.task, self.id);
    }
}

#[derive(Clone)]
pub struct Waker {
    task: Weak<Task>,
    id: WaitId,
}

impl Waker {
    /// 返回本轮是否首次接受通知，不代表业务条件成立；升级与临时强引用析构均在锁外。
    pub fn notify(&self) -> bool {
        let Some(task) = self.task.upgrade() else {
            return false;
        };
        sched::notify(&task, self.id)
    }
}
