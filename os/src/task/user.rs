//! 私有用户现场与独立 MM 引用；反向执行关联不保活 Task 或内核栈。

use alloc::sync::{Arc, Weak};
use core::alloc::AllocError;
use core::cell::{RefCell, RefMut};
use spin::Once;

use crate::arch::{self, UserContext, UserTrap};
use crate::mm::space::{self, Mm};
use crate::sync::{IrqSaveGuard, SpinLock};
use crate::task::{self, Task};

pub struct UserThread {
    executor: Once<Weak<Task>>,
    mm: SpinLock<Option<Arc<Mm>>>,
    context: RefCell<UserContext>,
}

// SAFETY: context 只由弱关联所指定的唯一 Running 普通 Task 访问，其他 CPU 和中断域
// 在借用前被身份与执行上下文检查拒绝；RefCell 排除同一执行者的重叠可变借用。
// 任务迁核由原调度交接串行化，mm 的远端拥有快照另由短锁保护。
unsafe impl Sync for UserThread {}

impl UserThread {
    /// 在发表 Task 前准备完整私有用户现场和地址空间拥有。
    pub(crate) fn try_new(mm: Arc<Mm>, context: UserContext) -> Result<Arc<Self>, AllocError> {
        Arc::try_new(Self {
            executor: Once::new(),
            mm: SpinLock::new(Some(mm)),
            context: RefCell::new(context),
        })
    }

    /// 在唯一 PreparedTask 尚未发表时建立弱反向关联，不产生强引用环。
    pub(crate) fn bind_executor(&self, executor: Weak<Task>) {
        assert!(
            self.executor.get().is_none(),
            "user executor is already bound"
        );
        self.executor.call_once(|| executor);
    }

    /// 取得当前实际资源快照，退出解绑后的诊断引用不再保有 MM。
    pub(crate) fn mm(&self) -> Option<Arc<Mm>> {
        self.mm.lock().clone()
    }

    /// 只由所绑定的 Running Task 短暂访问现场，不得持此 guard 睡眠或调度。
    pub(crate) fn context(&self) -> RefMut<'_, UserContext> {
        self.assert_current();
        self.context.borrow_mut()
    }

    /// 真正执行用户指令并返回原 Rust 调用栈；中断分派结束后才归还现场独占借用。
    pub(crate) fn run_user(&self) -> UserTrap {
        self.assert_current();
        let mm = self.mm().expect("running user thread has no MM");
        let mut context = self.context.borrow_mut();
        let _irq = IrqSaveGuard::new();
        let active = space::current_mm().expect("running user thread has no active MM");
        assert!(
            Arc::ptr_eq(&active, &mm),
            "running user thread has another active MM"
        );
        // SAFETY: 弱关联已核对当前唯一 Running Task，RefCell 的独占借用排除现场别名；
        // IRQ guard 关闭本地中断，MM 与 CPU 活动拥有保证全部根、用户页及内核映射存活。
        // 架构在同一调用栈返回，IRQ 关闭且硬中断 guard 已结束，期间不切换 Task。
        unsafe { arch::run_user(&mut context) }
    }

    /// 用户入口正常结束时先解绑运行资源，最后 MM 拥有在锁外释放。
    pub(crate) fn release_mm(&self) {
        let mm = self.mm.lock().take();
        drop(mm);
    }

    /// 核对稳定执行者身份，并拒绝中断域或没有普通 Task 的调用。
    fn assert_current(&self) {
        let executor = self
            .executor
            .get()
            .and_then(Weak::upgrade)
            .expect("user executor is not alive");
        let current = task::current_task();
        assert_eq!(
            current.id(),
            executor.id(),
            "user context belongs to another task"
        );
    }
}
