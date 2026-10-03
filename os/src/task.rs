//! 普通内核任务拥有执行栈与入口；完成观察者只拥有独立结果。

use alloc::boxed::Box;
use alloc::sync::Arc;
use core::alloc::AllocError;
use core::cell::UnsafeCell;
use core::ptr;
use core::sync::atomic::{AtomicI32, AtomicU64, AtomicUsize, Ordering};

use crate::mm::{alloc_pages, OwnedPages};
use crate::sched::context::TaskContext;
use crate::sync::{Completion, IrqSaveGuard};
use crate::time::Duration;
use crate::wait::WaitId;

static NEXT_ID: AtomicUsize = AtomicUsize::new(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TaskId(usize);

impl TaskId {
    /// 返回诊断使用的稳定整数，零留给没有普通任务的执行段。
    pub fn as_usize(self) -> usize {
        self.0
    }

    /// 将本 CPU 已发布的非零诊断标量恢复为任务身份。
    pub(crate) fn from_raw(value: usize) -> Option<Self> {
        if value == 0 {
            None
        } else {
            Some(Self(value))
        }
    }
}

pub(crate) enum Placement {
    Any,
    Cpu(usize),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SwitchIntent {
    Yield,
    Park(WaitId),
    Exit(i32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum State {
    Ready,
    Running(usize),
    SwitchingOut(usize, SwitchIntent),
    Blocked,
    Exited,
}

/// 所有字段只在调度锁中访问；两组链接代表独立成员关系。
pub(crate) struct Scheduling {
    pub state: State,
    pub placement: Placement,
    pub last_wait_id: WaitId,
    pub active_wait: Option<ActiveWait>,
    pub live_previous: *const Task,
    pub live_next: *const Task,
    pub ready_previous: *const Task,
    pub ready_next: *const Task,
    #[cfg(feature = "execution-tests")]
    pub next_cpu: Option<usize>,
}

#[derive(Clone, Copy)]
pub(crate) struct ActiveWait {
    pub id: WaitId,
    pub notified: bool,
}

pub(crate) type Entry = Box<dyn FnOnce() -> i32 + Send + 'static>;

/// 运行者取走入口；栈只在最后回到调度栈以后摘离。
pub(crate) struct Resources {
    pub stack: Option<OwnedPages>,
    pub entry: Option<Entry>,
}

pub(crate) struct JoinState {
    result: AtomicI32,
    completed: Completion,
}

impl JoinState {
    /// 栈与入口已在锁外释放后发布一次结果，由完成 token 保证结果可见性。
    pub(crate) fn complete(&self, result: i32) {
        self.result.store(result, Ordering::Relaxed);
        self.completed.complete();
    }
}

pub struct Task {
    id: TaskId,
    execution_ns: AtomicU64,
    pub(crate) ctx: UnsafeCell<TaskContext>,
    pub(crate) scheduling: UnsafeCell<Scheduling>,
    pub(crate) resources: UnsafeCell<Resources>,
    pub(crate) join: Arc<JoinState>,
}

// SAFETY: scheduling 仅在调度锁内访问；ctx 仅由 Running 的唯一 CPU 与其离栈交接访问。
// resources 仅由该执行者及 ctx 保存后的调度栈访问；入口是 Send，页拥有权只移动一次。
unsafe impl Send for Task {}
// SAFETY: 对外只共享不可变身份与原子结果；独立 UnsafeCell 的写者遵守上述执行权和锁协议。
unsafe impl Sync for Task {}

impl Task {
    /// 返回与调度位置、迁核和完成状态无关的唯一身份。
    pub fn id(&self) -> TaskId {
        self.id
    }

    /// 读取各次真实在 CPU 执行并已结算的总时长，不包含离 CPU 的等待。
    pub fn execution_time(&self) -> Duration {
        Duration::from_nanos(self.execution_ns.load(Ordering::Relaxed))
    }

    /// 唯一执行拥有者在 IRQ 关闭的片段边界增加总账，迁核交接以前完成写入。
    pub(crate) fn account_execution(&self, delta_ns: u64) {
        let total = self.execution_ns.load(Ordering::Relaxed);
        self.execution_ns.store(
            total
                .checked_add(delta_ns)
                .expect("task execution time overflow"),
            Ordering::Relaxed,
        );
    }
}

pub struct JoinHandle {
    id: TaskId,
    join: Arc<JoinState>,
}

impl JoinHandle {
    /// 非阻塞地观察正常返回结果；Some 保证任务已经离栈并归还执行栈。
    pub fn try_join(&self) -> Option<i32> {
        self.join
            .completed
            .is_ready()
            .then(|| self.join.result.load(Ordering::Relaxed))
    }

    /// 消耗唯一句柄并真实等待正常返回，完成 token 在执行栈归还后才发布。
    pub fn join(self) -> i32 {
        self.join.completed.wait();
        self.join.result.load(Ordering::Relaxed)
    }

    /// 即使任务已退出，观察者仍保留原任务身份。
    pub fn task_id(&self) -> TaskId {
        self.id
    }
}

/// 在任意在线 CPU 执行拥有数据的入口；所有分配成功以后才发表任务。
pub fn spawn_kernel(
    entry: impl FnOnce() -> i32 + Send + 'static,
) -> Result<JoinHandle, AllocError> {
    spawn(Placement::Any, entry)
}

/// 将入口固定到已构造的有效 CPU；允许引导过程在该 CPU 上线之前准备任务。
pub fn spawn_kernel_on(
    cpu: usize,
    entry: impl FnOnce() -> i32 + Send + 'static,
) -> Result<JoinHandle, AllocError> {
    assert!(cpu < crate::cpu::count(), "task CPU is out of range");
    spawn(Placement::Cpu(cpu), entry)
}

/// 准备入口、结果、连续栈与控制对象，失败对象按正常拥有关系在锁外析构。
fn spawn(
    placement: Placement,
    entry: impl FnOnce() -> i32 + Send + 'static,
) -> Result<JoinHandle, AllocError> {
    let entry: Entry = Box::try_new(entry)?;
    let join = Arc::try_new(JoinState {
        result: AtomicI32::new(0),
        completed: Completion::new(),
    })?;
    let mut stack = alloc_pages(4, false).ok_or(AllocError)?;
    let stack_top = stack.as_mut_ptr() as usize + stack.len();
    let id = NEXT_ID
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
        .expect("task ID space exhausted");
    let id = TaskId(id);
    let task = Arc::try_new(Task {
        id,
        execution_ns: AtomicU64::new(0),
        ctx: UnsafeCell::new(TaskContext::initial(stack_top)),
        scheduling: UnsafeCell::new(Scheduling {
            state: State::Ready,
            placement,
            last_wait_id: WaitId::initial(),
            active_wait: None,
            live_previous: ptr::null(),
            live_next: ptr::null(),
            ready_previous: ptr::null(),
            ready_next: ptr::null(),
            #[cfg(feature = "execution-tests")]
            next_cpu: None,
        }),
        resources: UnsafeCell::new(Resources {
            stack: Some(stack),
            entry: Some(entry),
        }),
        join: Arc::clone(&join),
    })?;
    crate::sched::publish(task);
    Ok(JoinHandle { id, join })
}

/// 借用普通任务的强引用；调度栈或硬 IRQ/BH 调用直接报告上下文错误。
pub fn current_task() -> Arc<Task> {
    let _irq = IrqSaveGuard::new();
    let local = crate::cpu::current();
    assert_eq!(
        local.hardirq_depth.load(Ordering::Relaxed),
        0,
        "current_task in hard IRQ"
    );
    assert_eq!(
        local.bh_depth.load(Ordering::Relaxed),
        0,
        "current_task in bottom half"
    );
    // SAFETY: IRQ 关闭且本函数不会切换；本 CPU 唯一修改 current_task，非空任务由活登记强拥有。
    let task = unsafe { *local.current_task.get() };
    assert!(!task.is_null(), "current_task outside a task");
    // SAFETY: 非空指针来自 Arc::into_raw 的活登记，当前 Running 阻止摘离；先增加一个强拥有再恢复该拥有。
    unsafe {
        Arc::increment_strong_count(task);
        Arc::from_raw(task)
    }
}

/// 无锁读取本 CPU 的普通任务身份，IRQ 可诊断被打断者，调度栈返回 None。
pub fn current_task_id() -> Option<TaskId> {
    TaskId::from_raw(
        crate::cpu::current()
            .current_task_id
            .load(Ordering::Relaxed),
    )
}

/// 在当前普通任务中取得真实执行栈的物理范围，不导出栈借用或拥有权。
#[cfg(feature = "execution-tests")]
pub fn test_current_stack_range() -> (usize, usize) {
    let _irq = IrqSaveGuard::new();
    let task = current_task();
    // SAFETY: 当前任务仍 Running，本核关 IRQ 且不会切换；只有本执行者访问其资源，
    // stack 在最后离栈以前不被摘离，返回值仅为整数，不延长任何裸指针访问权。
    let stack = unsafe { &*task.resources.get() }
        .stack
        .as_ref()
        .expect("running task has no stack");
    (stack.phys_addr(), stack.len())
}
