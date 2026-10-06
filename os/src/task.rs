//! 普通内核任务拥有执行栈与入口；完成观察者只拥有独立结果。

mod user;
pub(crate) use user::UserThread;

use alloc::boxed::Box;
use alloc::sync::Arc;
use core::alloc::AllocError;
use core::cell::UnsafeCell;
use core::pin::pin;
use core::ptr;
#[cfg(feature = "ktest")]
use core::sync::atomic::AtomicPtr;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, AtomicUsize, Ordering};

use crate::arch::UserContext;
use crate::mm::space::Mm;
use crate::mm::{alloc_pages, OwnedPages};
use crate::sched::context::TaskContext;
use crate::sync::{IrqSaveGuard, SpinLock};
use crate::time::Duration;
use crate::wait::{WaitId, WaitNode, WaitQueue, Waiter};

static NEXT_ID: AtomicUsize = AtomicUsize::new(1);
#[cfg(feature = "ktest")]
static LIVE_TASKS: AtomicUsize = AtomicUsize::new(0);

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
    #[cfg(feature = "ktest")]
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
    completed: AtomicBool,
    waiters: WaitQueue,
}

impl JoinState {
    /// 栈与入口已锁外释放后发布一次粘性的结果，观察与原 join 不消费完成状态。
    pub(crate) fn complete(&self, result: i32) {
        self.result.store(result, Ordering::Relaxed);
        self.completed.store(true, Ordering::Release);
        self.waiters.wake_all();
    }

    /// Acquire 观察真实离栈结果，任何 join 以后仍保留该完成事实。
    fn try_join(&self) -> Option<i32> {
        self.completed
            .load(Ordering::Acquire)
            .then(|| self.result.load(Ordering::Relaxed))
    }

    /// 原等待源先登记后复查，多个观察者均真实 park，不能竞争消费一次性结果。
    fn wait_result(&self) -> i32 {
        crate::sched::assert_switchable();
        loop {
            if let Some(result) = self.try_join() {
                return result;
            }
            let waiter = Waiter::new();
            let mut node = pin!(WaitNode::new(waiter.waker()));
            // SAFETY: 当前共享借用保活原 JoinState/等待源；节点在原执行栈固定，
            // registration 先于 node/waiter 析构撤链，登记后的复查关闭完成通知窗口。
            let _registration = unsafe { self.waiters.register(node.as_mut()) };
            if let Some(result) = self.try_join() {
                return result;
            }
            waiter.park();
        }
    }
}

pub struct Task {
    id: TaskId,
    execution_ns: AtomicU64,
    pub(crate) ctx: UnsafeCell<TaskContext>,
    pub(crate) scheduling: UnsafeCell<Scheduling>,
    pub(crate) resources: UnsafeCell<Resources>,
    pub(crate) join: Arc<JoinState>,
    pub(crate) worker: Option<Arc<crate::workqueue::Worker>>,
    pub(crate) user: SpinLock<Option<Arc<UserThread>>>,
    #[cfg(feature = "ktest")]
    pub(crate) ktest_boundary: AtomicPtr<()>,
    #[cfg(feature = "ktest")]
    pub(crate) ktest_scope: AtomicPtr<()>,
}

// SAFETY: scheduling 仅在调度锁内访问；ctx 仅由 Running 的唯一 CPU 与其离栈交接访问。
// resources 仅由该执行者及 ctx 保存后的调度栈访问；入口是 Send，页拥有权只移动一次。
// user 的绑定只在准备期安装、离栈后摘离，短锁保护远端取得拥有的读者。
unsafe impl Send for Task {}
// SAFETY: 对外只共享不可变身份与原子结果；独立 UnsafeCell 的写者遵守上述执行权和锁协议。
unsafe impl Sync for Task {}

impl Task {
    /// 取得单一用户绑定的拥有快照，内核任务和已退出任务没有运行绑定。
    pub(crate) fn user_thread(&self) -> Option<Arc<UserThread>> {
        self.user.lock().clone()
    }

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

#[cfg(feature = "ktest")]
impl Drop for Task {
    /// 真正最后强拥有或未发表 Arc 分配失败归还 payload，克隆及弱观察不计。
    fn drop(&mut self) {
        LIVE_TASKS.fetch_sub(1, Ordering::Relaxed);
    }
}

/// 仅供停稳后的真实 metadata 收敛观察，不代表弱控制块或栈 backing 已释放。
#[cfg(feature = "ktest")]
pub(crate) fn test_live_tasks() -> usize {
    LIVE_TASKS.load(Ordering::Relaxed)
}

pub struct JoinHandle {
    id: TaskId,
    join: Arc<JoinState>,
}

impl JoinHandle {
    /// 非阻塞地观察正常返回结果；Some 保证任务已经离栈并归还执行栈。
    pub fn try_join(&self) -> Option<i32> {
        self.join.try_join()
    }

    /// 消耗唯一句柄并真实等待离栈，结果在执行栈归还后永久可观察。
    pub fn join(self) -> i32 {
        self.join.wait_result()
    }

    /// 即使任务已退出，观察者仍保留原任务身份。
    pub fn task_id(&self) -> TaskId {
        self.id
    }
    /// 创建仅保留原完成状态的观察者，不延长 Task 或执行栈寿命。
    #[cfg(feature = "ktest")]
    pub(crate) fn observer(&self) -> JoinObserver {
        JoinObserver {
            id: self.id,
            join: Arc::clone(&self.join),
        }
    }
}

/// 测试观察者保留原结果与身份；唯一原 JoinHandle 仍由作用域消费。
#[cfg(feature = "ktest")]
pub(crate) struct JoinObserver {
    id: TaskId,
    join: Arc<JoinState>,
}
#[cfg(feature = "ktest")]
impl JoinObserver {
    /// Some 只来自原粘性完成事实，已确认执行栈归还。
    pub(crate) fn try_join(&self) -> Option<i32> {
        self.join.try_join()
    }
    /// 等待原完成事实，不消费作用域的唯一 join 责任。
    pub(crate) fn join(&self) -> i32 {
        self.join.wait_result()
    }
    /// 已退出后仍返回实际原任务身份。
    pub(crate) fn task_id(&self) -> TaskId {
        self.id
    }
}

/// 完成全部分配但尚未进入活登记；丢弃时由普通拥有关系归还入口与执行栈。
pub(crate) struct PreparedTask {
    task: Arc<Task>,
    handle: JoinHandle,
}

impl PreparedTask {
    /// 在尚未发表时安装唯一用户绑定，弱反向关联不保留 Task 栈。
    pub(crate) fn bind_user(&self, user: Arc<UserThread>) {
        user.bind_executor(Arc::downgrade(&self.task));
        let mut slot = self.task.user.lock();
        assert!(slot.is_none(), "prepared task already has a user binding");
        *slot = Some(user);
    }

    /// 发表前即可将稳定身份登记到服务元数据，不保留 Task 强引用。
    pub(crate) fn task_id(&self) -> TaskId {
        self.task.id()
    }

    /// 将预备的唯一任务拥有移交调度器，不再执行任何可失败的分配。
    pub(crate) fn publish(self) -> JoinHandle {
        crate::sched::publish(self.task);
        self.handle
    }
}

/// 在任意在线 CPU 执行拥有数据的入口；所有分配成功以后才发表任务。
pub fn spawn_kernel(
    entry: impl FnOnce() -> i32 + Send + 'static,
) -> Result<JoinHandle, AllocError> {
    prepare(Placement::Any, None, entry).map(PreparedTask::publish)
}

/// 预备可由原调度器选择 CPU 的入口，不提前发表或固定其执行位置。
#[cfg(feature = "ktest")]
pub(crate) fn prepare_kernel(
    entry: impl FnOnce() -> i32 + Send + 'static,
) -> Result<PreparedTask, AllocError> {
    prepare(Placement::Any, None, entry)
}

/// 将入口固定到已构造的有效 CPU；允许引导过程在该 CPU 上线之前准备任务。
pub fn spawn_kernel_on(
    cpu: usize,
    entry: impl FnOnce() -> i32 + Send + 'static,
) -> Result<JoinHandle, AllocError> {
    prepare_kernel_on(cpu, entry).map(PreparedTask::publish)
}

/// 预备固定 CPU 的普通入口，未发表时可以安全放弃全部资源。
pub(crate) fn prepare_kernel_on(
    cpu: usize,
    entry: impl FnOnce() -> i32 + Send + 'static,
) -> Result<PreparedTask, AllocError> {
    assert!(cpu < crate::cpu::count(), "task CPU is out of range");
    prepare(Placement::Cpu(cpu), None, entry)
}

/// 预备拥有不可变工作者身份的固定 CPU 入口，调度器只读取该身份。
pub(crate) fn prepare_worker_on(
    cpu: usize,
    worker: Arc<crate::workqueue::Worker>,
    entry: impl FnOnce() -> i32 + Send + 'static,
) -> Result<PreparedTask, AllocError> {
    assert!(cpu < crate::cpu::count(), "worker CPU is out of range");
    prepare(Placement::Cpu(cpu), Some(worker), entry)
}

/// 准备入口、结果、连续栈与控制对象，失败对象按正常拥有关系在锁外析构。
fn prepare(
    placement: Placement,
    worker: Option<Arc<crate::workqueue::Worker>>,
    entry: impl FnOnce() -> i32 + Send + 'static,
) -> Result<PreparedTask, AllocError> {
    let entry: Entry = Box::try_new(entry)?;
    let join = Arc::try_new(JoinState {
        result: AtomicI32::new(0),
        completed: AtomicBool::new(false),
        waiters: WaitQueue::new(),
    })?;
    let mut stack = alloc_pages(4, false).ok_or(AllocError)?;
    let stack_top = stack.as_mut_ptr() as usize + stack.len();
    let id = NEXT_ID
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
        .expect("task ID space exhausted");
    let id = TaskId(id);
    let value = Task {
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
            #[cfg(feature = "ktest")]
            next_cpu: None,
        }),
        resources: UnsafeCell::new(Resources {
            stack: Some(stack),
            entry: Some(entry),
        }),
        join: Arc::clone(&join),
        worker,
        user: SpinLock::new(None),
        #[cfg(feature = "ktest")]
        ktest_boundary: AtomicPtr::new(ptr::null_mut()),
        #[cfg(feature = "ktest")]
        ktest_scope: AtomicPtr::new(ptr::null_mut()),
    };
    // 完整 value 在 fallible Arc 分配前计入，失败时普通 Drop 精确配对。
    #[cfg(feature = "ktest")]
    LIVE_TASKS.fetch_add(1, Ordering::Relaxed);
    let task = Arc::try_new(value)?;
    Ok(PreparedTask {
        task,
        handle: JoinHandle { id, join },
    })
}

/// 先准备私有用户绑定和普通 Task，再由调用者统一发表；失败正常归还全部未发表拥有。
pub(crate) fn prepare_user(
    mm: Arc<Mm>,
    context: UserContext,
    entry: impl FnOnce(Arc<UserThread>) -> i32 + Send + 'static,
) -> Result<PreparedTask, AllocError> {
    let user = UserThread::try_new(mm, context)?;
    let binding = Arc::clone(&user);
    let prepared = prepare(Placement::Any, None, move || entry(user))?;
    prepared.bind_user(binding);
    Ok(prepared)
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

/// 只克隆本 CPU 当前任务的不可变工作者身份，不取调度锁或申请存储。
pub(crate) fn current_worker() -> Option<Arc<crate::workqueue::Worker>> {
    let _irq = IrqSaveGuard::new();
    let local = crate::cpu::current();
    // SAFETY: IRQ 关闭且不切换，本 CPU 独占 current_task；非空当前任务由活登记保活，
    // worker 构造后不再修改，克隆现存 Arc 不分配，也不延长任何 CPU 私有借用。
    unsafe { (*local.current_task.get()).as_ref() }
        .and_then(|task| task.worker.as_ref().map(Arc::clone))
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
#[cfg(feature = "ktest")]
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
