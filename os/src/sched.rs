//! Task 先保存到本 CPU 静态调度栈，完成交接以后才重新进入独立 ready 队列。

pub mod context;

use alloc::sync::Arc;
use core::hint::spin_loop;
use core::ptr;
use core::sync::atomic::Ordering;

use crate::cpu;
use crate::sync::{IrqSaveGuard, SpinLock};
use crate::task::{ActiveWait, Placement, Resources, State, SwitchIntent, Task};
use crate::wait::WaitId;

struct Scheduler {
    live_head: *const Task,
    ready_head: *const Task,
    ready_tail: *const Task,
}

// SAFETY: 全部链接与状态只在 SCHED_LOCK 中访问；live 中每个指针各代表一个 Arc 强拥有。
unsafe impl Send for Scheduler {}

static SCHED_LOCK: SpinLock<Scheduler> = SpinLock::new(Scheduler::new());

impl Scheduler {
    /// 两组 intrusive 链表无需引导时或锁内分配。
    const fn new() -> Self {
        Self {
            live_head: ptr::null(),
            ready_head: ptr::null(),
            ready_tail: ptr::null(),
        }
    }

    /// 接收一次原始强拥有并登记活任务，随后以独立节点加入 ready 尾部。
    fn publish(&mut self, task: *const Task) {
        // SAFETY: task 是新 Arc::into_raw 的唯一活登记拥有，尚未发表；锁独占全部链接。
        unsafe {
            let data = &mut *(*task).scheduling.get();
            data.live_next = self.live_head;
            if !self.live_head.is_null() {
                (*(*self.live_head).scheduling.get()).live_previous = task;
            }
            self.live_head = task;
        }
        self.push_ready(task);
    }

    /// 对不在 ready 中的活 Ready 任务尾插，不转移活登记拥有权。
    fn push_ready(&mut self, task: *const Task) {
        // SAFETY: 私有调用点保证 task 活着且为 Ready、不在 ready；锁独占独立 ready 链接。
        unsafe {
            let data = &mut *(*task).scheduling.get();
            assert_eq!(data.state, State::Ready, "enqueue task is not ready");
            data.ready_previous = self.ready_tail;
            data.ready_next = ptr::null();
            if self.ready_tail.is_null() {
                self.ready_head = task;
            } else {
                (*(*self.ready_tail).scheduling.get()).ready_next = task;
            }
            self.ready_tail = task;
        }
    }

    /// 在锁内摘离现有 ready 成员，节点不承担强拥有的释放。
    fn remove_ready(&mut self, task: *const Task) {
        // SAFETY: pick 只传仍在本 ready 链的活节点，锁独占头尾和相邻节点。
        unsafe {
            let data = &mut *(*task).scheduling.get();
            if data.ready_previous.is_null() {
                self.ready_head = data.ready_next;
            } else {
                (*(*data.ready_previous).scheduling.get()).ready_next = data.ready_next;
            }
            if data.ready_next.is_null() {
                self.ready_tail = data.ready_previous;
            } else {
                (*(*data.ready_next).scheduling.get()).ready_previous = data.ready_previous;
            }
            data.ready_previous = ptr::null();
            data.ready_next = ptr::null();
        }
    }

    /// 按 FIFO 寻找本在线 CPU 可执行的节点，原子取得唯一 Running 执行权。
    fn pick(&mut self, owner: usize) -> Option<*const Task> {
        let mut task = self.ready_head;
        while !task.is_null() {
            // SAFETY: ready 只包含活登记节点，锁保护状态和链接，不访问 ctx。
            let data = unsafe { &mut *(*task).scheduling.get() };
            let allowed = match data.placement {
                Placement::Any => true,
                Placement::Cpu(cpu) => cpu == owner,
            };
            #[cfg(feature = "execution-tests")]
            let allowed = allowed && data.next_cpu.is_none_or(|cpu| cpu == owner);
            if allowed {
                self.remove_ready(task);
                // SAFETY: task 仍由 live 强拥有且锁尚未释放；摘离 ready 后只给本 CPU 执行权。
                let data = unsafe { &mut *(*task).scheduling.get() };
                data.state = State::Running(owner);
                #[cfg(feature = "execution-tests")]
                {
                    data.next_cpu = None;
                }
                return Some(task);
            }
            task = data.ready_next;
        }
        None
    }

    /// ctx 已保存以后结束 SwitchingOut，退出资源仅移出而不在锁内析构。
    fn finish(&mut self, task: *const Task, owner: usize) -> Option<(Resources, i32)> {
        // SAFETY: 本 CPU 刚回调度栈，活登记保留 task，旧 CPU 已结束 ctx 保存；锁独占状态。
        let data = unsafe { &mut *(*task).scheduling.get() };
        match data.state {
            State::SwitchingOut(cpu, SwitchIntent::Yield) if cpu == owner => {
                data.state = State::Ready;
                self.push_ready(task);
                None
            }
            State::SwitchingOut(cpu, SwitchIntent::Park(id)) if cpu == owner => {
                let active = data.active_wait.expect("park has no active wait");
                assert_eq!(active.id, id, "park wait identity changed");
                data.state = if active.notified {
                    State::Ready
                } else {
                    State::Blocked
                };
                if active.notified {
                    self.push_ready(task);
                }
                None
            }
            State::SwitchingOut(cpu, SwitchIntent::Exit(result)) if cpu == owner => {
                assert!(
                    data.active_wait.is_none(),
                    "exiting task has an active wait"
                );
                data.state = State::Exited;
                // SAFETY: 活节点尚未释放且退出任务不会再执行；同锁覆盖活链接和资源摘离。
                let resources = unsafe {
                    if data.live_previous.is_null() {
                        self.live_head = data.live_next;
                    } else {
                        (*(*data.live_previous).scheduling.get()).live_next = data.live_next;
                    }
                    if !data.live_next.is_null() {
                        (*(*data.live_next).scheduling.get()).live_previous = data.live_previous;
                    }
                    data.live_previous = ptr::null();
                    data.live_next = ptr::null();
                    let resources = &mut *(*task).resources.get();
                    Resources {
                        stack: resources.stack.take(),
                        entry: resources.entry.take(),
                    }
                };
                Some((resources, result))
            }
            _ => panic!("task switch completion has invalid state"),
        }
    }
}

/// 发表前已完成全部 fallible 分配；活链接管恰好一个 Arc 强拥有。
pub(crate) fn publish(task: Arc<Task>) {
    let task = Arc::into_raw(task);
    SCHED_LOCK.lock().publish(task);
}

/// 集中检查显式安全点，普通 IRQ-save 锁通过 IRQ 关闭自然被拒绝。
pub(crate) fn assert_switchable() {
    assert!(crate::arch::irq_enabled(), "task switch with IRQ disabled");
    let local = cpu::current();
    assert_eq!(
        local.hardirq_depth.load(Ordering::Relaxed),
        0,
        "task switch in hard IRQ"
    );
    assert_eq!(
        local.bh_depth.load(Ordering::Relaxed),
        0,
        "task switch in bottom half"
    );
    assert_eq!(
        local.preempt_depth.load(Ordering::Relaxed),
        0,
        "task switch with preemption disabled"
    );
    assert!(
        crate::task::current_task_id().is_some(),
        "task switch outside a task"
    );
}

/// 普通任务显式让出执行权，恢复可能发生在另一 CPU，普通 guard 不跨交接。
pub fn yield_now() {
    switch_out(SwitchIntent::Yield);
    resume_task();
}

/// 在专用 IRQ-off 段提交意图，通知已到达的 Park 不交出执行权。
fn prepare_switch(
    intent: SwitchIntent,
) -> Option<(*mut context::TaskContext, *const context::TaskContext)> {
    assert_switchable();
    crate::arch::irq_save();
    let contexts = {
        let local = cpu::current();
        // SAFETY: IRQ 关闭，唯一所属 CPU 读取 current；Running 对象由活登记保持有效。
        let task = unsafe { *local.current_task.get() };
        assert!(!task.is_null(), "switching task is missing");
        let committed = {
            let _scheduler = SCHED_LOCK.lock();
            // SAFETY: 调度锁独占 state；当前 Task 仍有唯一 Running 执行权，不访问其他 CPU 的 ctx。
            let data = unsafe { &mut *(*task).scheduling.get() };
            assert_eq!(
                data.state,
                State::Running(local.id),
                "switching task has no execution ownership"
            );
            let notified = if let SwitchIntent::Park(id) = intent {
                let active = data.active_wait.expect("park has no active wait");
                assert_eq!(active.id, id, "park wait identity changed");
                active.notified
            } else {
                false
            };
            if !notified {
                data.state = State::SwitchingOut(local.id, intent);
            }
            !notified
        };
        if committed {
            // SAFETY: 活登记保护 task，SwitchingOut 不可分派；只取独立 ctx 裸指针，不构造 &mut Task。
            Some((
                unsafe { (*task).ctx.get() },
                local.scheduler_ctx.get().cast_const(),
            ))
        } else {
            None
        }
    };
    if contexts.is_none() {
        crate::arch::enable_interrupts();
    }
    contexts
}

/// 提交成功后保存 ctx，普通锁和本 CPU 引用都不跨越汇编切换。
fn switch_out(intent: SwitchIntent) {
    let (task_ctx, scheduler_ctx) =
        prepare_switch(intent).expect("non-park switch was not committed");
    // SAFETY: IRQ 关闭、普通锁已释放；SwitchingOut 执行者与本 CPU 独占两份有效 ctx，
    // scheduler ctx 来自同核 run 的保存现场；调用者恢复后重新获取目的 CPU。
    unsafe { crate::arch::switch_context(task_ctx, scheduler_ctx) };
}

/// 同一等待轮次只有真正缺少通知时才阻塞，恢复后重新开启目的 CPU 的 IRQ。
pub(crate) fn park(id: WaitId) {
    if let Some((task_ctx, scheduler_ctx)) = prepare_switch(SwitchIntent::Park(id)) {
        // SAFETY: prepare_switch 已提交 SwitchingOut 且放锁，IRQ 关闭，两份 ctx 的执行权唯一。
        unsafe { crate::arch::switch_context(task_ctx, scheduler_ctx) };
        resume_task();
    }
}

/// 为当前 Running Task 开始唯一等待，序号耗尽不允许回绕。
pub(crate) fn begin_wait(task: &Task) -> WaitId {
    let _scheduler = SCHED_LOCK.lock();
    assert_eq!(
        crate::task::current_task_id(),
        Some(task.id()),
        "wait belongs to another task"
    );
    // SAFETY: 调度锁独占 scheduling；调用者强引用保护 task，当前身份核对其 Running 执行权。
    let data = unsafe { &mut *task.scheduling.get() };
    assert_eq!(data.state, State::Running(cpu::current().id));
    assert!(
        data.active_wait.is_none(),
        "task already has an active wait"
    );
    let id = data.last_wait_id.next();
    data.last_wait_id = id;
    data.active_wait = Some(ActiveWait {
        id,
        notified: false,
    });
    id
}

/// 唯一 Waiter 析构核对本轮身份并清除等待，不改变业务源链接。
pub(crate) fn end_wait(task: &Task, id: WaitId) {
    let _scheduler = SCHED_LOCK.lock();
    assert_eq!(crate::task::current_task_id(), Some(task.id()));
    // SAFETY: 调度锁独占 scheduling；Waiter 的强引用保护 task，析构只能发生在同一任务。
    let data = unsafe { &mut *task.scheduling.get() };
    assert_eq!(data.state, State::Running(cpu::current().id));
    assert_eq!(data.active_wait.expect("ending absent wait").id, id);
    data.active_wait = None;
}

/// 接受本轮首次通知；Ready 保留已有成员，切换保存前仅记通知，只有 Blocked 需要入队。
pub(crate) fn notify(task: &Task, id: WaitId) -> bool {
    let mut scheduler = SCHED_LOCK.lock();
    // SAFETY: 调度锁独占状态和等待；调用者在锁外升级的强引用覆盖整个访问。
    let data = unsafe { &mut *task.scheduling.get() };
    let Some(active) = data.active_wait.as_mut() else {
        return false;
    };
    if active.id != id || active.notified {
        return false;
    }
    assert!(
        matches!(
            data.state,
            State::Ready | State::Running(_) | State::SwitchingOut(_, _) | State::Blocked
        ),
        "unnotified wait has invalid state"
    );
    active.notified = true;
    if data.state == State::Blocked {
        data.state = State::Ready;
        scheduler.push_ready(task);
    }
    true
}

/// 在真实 SwitchingOut 放锁后、ctx 保存前通知，并继续同一汇编与 finish 路径。
#[cfg(feature = "execution-tests")]
pub(crate) fn test_park_notify(task: &Task, id: WaitId, waker: &crate::wait::Waker) {
    let (task_ctx, scheduler_ctx) =
        prepare_switch(SwitchIntent::Park(id)).expect("test park was already notified");
    assert!(waker.notify(), "switching-out notification was rejected");
    assert!(matches!(
        test_state(task),
        State::SwitchingOut(_, SwitchIntent::Park(active)) if active == id
    ));
    assert_eq!(test_ready_occurrences(task), 0);
    // SAFETY: prepare_switch 已提交 SwitchingOut 且放锁，通知没有转移执行权；IRQ 关闭，两份 ctx 独占。
    unsafe { crate::arch::switch_context(task_ctx, scheduler_ctx) };
    resume_task();
}

/// 在唯一调度锁内读取真实状态，不向测试导出写权限。
#[cfg(feature = "execution-tests")]
pub(crate) fn test_state(task: &Task) -> State {
    let _scheduler = SCHED_LOCK.lock();
    // SAFETY: 调度锁保护 scheduling，调用者借用保持 task 有效，只复制状态值。
    unsafe { (*task.scheduling.get()).state }
}

/// 遍历实际 ready 链并计算同一 Task 的成员次数。
#[cfg(feature = "execution-tests")]
pub(crate) fn test_ready_occurrences(task: &Task) -> usize {
    let scheduler = SCHED_LOCK.lock();
    let mut member = scheduler.ready_head;
    let mut count = 0;
    while !member.is_null() {
        if ptr::eq(member, task) {
            count += 1;
        }
        // SAFETY: ready 节点由同锁活登记保活，遍历期间链接不会变化。
        member = unsafe { (*(*member).scheduling.get()).ready_next };
    }
    count
}

/// 首次入口与普通恢复都重新识别目的 CPU，再开启本核 IRQ，不恢复旧核 saved 状态。
fn resume_task() {
    assert!(!crate::arch::irq_enabled(), "task resume has IRQ enabled");
    assert!(
        crate::task::current_task_id().is_some(),
        "task resume has no current task"
    );
    crate::arch::enable_interrupts();
}

/// 以正常 Rust 调用消耗 FnOnce，入口局部析构结束后才提交退出意图。
pub(crate) extern "C" fn task_trampoline() -> ! {
    resume_task();
    let entry = {
        let _irq = IrqSaveGuard::new();
        let local = cpu::current();
        // SAFETY: 首次 Running 任务由活登记保持，IRQ 关闭且本核独占资源；入口只取走一次。
        unsafe {
            (*(*local.current_task.get()))
                .resources
                .get()
                .as_mut()
                .unwrap()
                .entry
                .take()
        }
        .expect("task entry has already been consumed")
    };
    let result = entry();
    switch_out(SwitchIntent::Exit(result));
    panic!("exited task resumed");
}

/// 在调度栈上清除本 CPU 当前身份，再完成旧 Task 的 ctx 交接与锁外资源回收。
fn finish_current() {
    let (task, owner) = {
        let local = cpu::current();
        // SAFETY: 从 Task 回到本 CPU 调度栈且 IRQ 关闭，只有所属 CPU 访问 current_task。
        let task = unsafe { local.current_task.get().replace(ptr::null()) };
        local.current_task_id.store(0, Ordering::Relaxed);
        (task, local.id)
    };
    assert!(!task.is_null(), "scheduler returned without a task");
    let retired = SCHED_LOCK.lock().finish(task, owner);
    if let Some((resources, result)) = retired {
        // SAFETY: finish 已摘离活登记；task 对应此前 into_raw 的那一份强拥有，仅在此恢复一次。
        let task = unsafe { Arc::from_raw(task) };
        crate::arch::enable_interrupts();
        drop(resources);
        task.join.complete(result);
        drop(task);
        crate::arch::irq_save();
    }
}

/// 每 CPU 从自己的永久 boot 栈进入相同调度循环，无普通任务时身份为空。
pub fn run() -> ! {
    assert!(
        cpu::current().online.load(Ordering::Acquire),
        "scheduler CPU is offline"
    );
    crate::arch::irq_save();
    loop {
        let local = cpu::current();
        let next = SCHED_LOCK.lock().pick(local.id);
        if let Some(task) = next {
            // SAFETY: pick 将唯一 Running 执行权交给本核，活登记保护指针；本 CPU 在 IRQ 关闭时发表身份。
            unsafe { *local.current_task.get() = task };
            // SAFETY: 活 Task 不可被其他 CPU 摘离，身份为不可变字段。
            let id = unsafe { (*task).id().as_usize() };
            local.current_task_id.store(id, Ordering::Relaxed);
            // SAFETY: 本核独占 scheduler ctx；pick 独占目标 Task ctx，二者有效且 IRQ 关闭、调度锁已释放。
            unsafe {
                crate::arch::switch_context(
                    local.scheduler_ctx.get(),
                    (*task).ctx.get().cast_const(),
                )
            };
            finish_current();
        } else {
            // 当前尚无等待型 idle；开放 IRQ 的短自旋不依赖周期 timer 补救漏醒。
            crate::arch::enable_interrupts();
            spin_loop();
            crate::arch::irq_save();
        }
    }
}

/// Any 任务只将下一次真实分派指定为在线目标 CPU，pick 后立即恢复 Any 选择。
#[cfg(feature = "execution-tests")]
pub fn test_migrate_to(target: usize) {
    assert_switchable();
    assert!(target < cpu::count(), "migration CPU is out of range");
    assert!(
        cpu::get(target).online.load(Ordering::Acquire),
        "migration CPU is offline"
    );
    {
        let _irq = IrqSaveGuard::new();
        let local = cpu::current();
        let _scheduler = SCHED_LOCK.lock();
        // SAFETY: 本核 IRQ 关闭且调度锁保护 placement/next_cpu；活登记保护当前 Running 节点。
        let data = unsafe { &mut *(*(*local.current_task.get())).scheduling.get() };
        assert!(
            matches!(data.placement, Placement::Any),
            "migration test requires an Any task"
        );
        assert!(
            data.next_cpu.is_none(),
            "task already has a migration target"
        );
        data.next_cpu = Some(target);
    }
    yield_now();
}
