//! Task 先保存到本 CPU 静态调度栈，完成交接以后才重新进入独立 ready 队列。

pub mod context;

use alloc::sync::Arc;
use core::hint::spin_loop;
use core::ptr;
use core::sync::atomic::Ordering;

use crate::cpu;
use crate::sync::{IrqSaveGuard, SpinLock};
use crate::task::{Placement, Resources, State, SwitchIntent, Task};

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
            State::SwitchingOut(cpu, SwitchIntent::Exit(result)) if cpu == owner => {
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
fn assert_switchable() {
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

/// 在专用 IRQ-off 段提交意图，放锁后才保存 ctx，恢复路径不使用旧 CPU 引用或 IRQ 值。
fn switch_out(intent: SwitchIntent) {
    assert_switchable();
    crate::arch::irq_save();
    let (task_ctx, scheduler_ctx) = {
        let local = cpu::current();
        // SAFETY: IRQ 关闭，唯一所属 CPU 读取 current；Running 对象由活登记保持有效。
        let task = unsafe { *local.current_task.get() };
        assert!(!task.is_null(), "switching task is missing");
        {
            let _scheduler = SCHED_LOCK.lock();
            // SAFETY: 调度锁独占 state；当前 Task 仍有唯一 Running 执行权，不访问其他 CPU 的 ctx。
            let data = unsafe { &mut *(*task).scheduling.get() };
            assert_eq!(
                data.state,
                State::Running(local.id),
                "switching task has no execution ownership"
            );
            data.state = State::SwitchingOut(local.id, intent);
        }
        // SAFETY: 活登记保护 task，SwitchingOut 不可分派；只取独立 ctx 的裸指针，不构造 &mut Task。
        (
            unsafe { (*task).ctx.get() },
            local.scheduler_ctx.get().cast_const(),
        )
    };
    // SAFETY: IRQ 关闭、普通锁已释放；Task 的 SwitchingOut 执行者与本 CPU 独占两份 ctx。
    // scheduler ctx 来自同核 run 的已保存现场；恢复后只执行重新取得目的 CPU 的 resume_task。
    unsafe { crate::arch::switch_context(task_ctx, scheduler_ctx) };
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
        task.completion.complete(result);
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
