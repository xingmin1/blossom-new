//! 私有用户现场与独立 MM 引用；反向执行关联不保活 Task 或内核栈。

use alloc::sync::{Arc, Weak};
use core::alloc::AllocError;
use core::cell::{Cell, RefCell, RefMut};
use core::sync::atomic::Ordering;
use spin::Once;

use crate::arch::{self, UserContext, UserTrap};
use crate::fs::{FileTable, FsContext, FsError, Vfs};
use crate::mm::space::{self, Mm, VmError};
use crate::sync::{IrqSaveGuard, SleepMutex, SpinLock};
use crate::task::exec::{ExecError, LoadedImage};
use crate::task::process::{PreparedIdentity, ProcessError, TaskGraph, ThreadGroup, UserIdentity};
use crate::task::{self, JoinHandle, Placement, PreparedTask, Task};

const USER_DESCRIPTOR_CAPACITY: usize = 128;

/// 当前用户的不可变凭据；目录和文件权限判断使用真实 inode uid/gid/mode。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Credentials {
    pub(crate) uid: u32,
    pub(crate) gid: u32,
}

impl Credentials {
    pub(crate) const ROOT: Self = Self { uid: 0, gid: 0 };
}

/// 同一文件系统上下文保留完整挂载视图以及可独立共享的 root/cwd。
pub(crate) struct UserFs {
    pub(crate) vfs: Arc<Vfs>,
    pub(crate) context: FsContext,
}

/// 文件表和文件系统上下文各有独立共享引用，不由线程组代持运行资源。
#[derive(Clone)]
pub(crate) struct UserRuntime {
    pub(crate) files: Arc<FileTable>,
    pub(crate) fs: Arc<SleepMutex<UserFs>>,
    pub(crate) credentials: Credentials,
}

impl UserRuntime {
    /// 在普通任务锁外准备有限文件表和完整文件系统拥有，失败不发表用户身份。
    pub(crate) fn try_new(
        vfs: Arc<Vfs>,
        context: FsContext,
        credentials: Credentials,
    ) -> Result<Self, crate::fs::FsError> {
        let files = FileTable::try_new(USER_DESCRIPTOR_CAPACITY)?;
        let files = Arc::try_new(files).map_err(|_| crate::fs::FsError::NoMemory)?;
        let fs = Arc::try_new(SleepMutex::new(UserFs { vfs, context }))
            .map_err(|_| crate::fs::FsError::NoMemory)?;
        Ok(Self {
            files,
            fs,
            credentials,
        })
    }
}

struct UserProcess {
    identity: Arc<UserIdentity>,
    runtime: UserRuntime,
}

pub struct UserThread {
    executor: Once<Weak<Task>>,
    mm: SpinLock<Option<Arc<Mm>>>,
    context: RefCell<UserContext>,
    process: SpinLock<Option<UserProcess>>,
    user_timer_interrupts: Cell<usize>,
}

// SAFETY: context 与 user_timer_interrupts 只由弱关联指定的唯一 Running 普通 Task 访问；
// 其他 CPU 和中断域在访问前被身份与执行上下文检查拒绝，RefCell 排除重叠现场借用。
// 任务迁核由原调度交接串行化，mm 的远端拥有快照另由短锁保护。
unsafe impl Sync for UserThread {}

impl UserThread {
    /// 在发表 Task 前准备完整私有用户现场和地址空间拥有。
    pub(crate) fn try_new(mm: Arc<Mm>, context: UserContext) -> Result<Arc<Self>, AllocError> {
        Arc::try_new(Self {
            executor: Once::new(),
            mm: SpinLock::new(Some(mm)),
            context: RefCell::new(context),
            process: SpinLock::new(None),
            user_timer_interrupts: Cell::new(0),
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

    /// 完整进程资源在原 PreparedTask 发布之前绑定，受控架构入口不需要 Linux 身份。
    pub(crate) fn bind_process(&self, identity: Arc<UserIdentity>, runtime: UserRuntime) {
        let mut process = self.process.lock();
        assert!(process.is_none(), "user process is already bound");
        *process = Some(UserProcess { identity, runtime });
    }

    /// 在关系和信号操作前取得身份快照，不跨图锁保存用户资源锁。
    pub(crate) fn identity(&self) -> Option<Arc<UserIdentity>> {
        self.process
            .lock()
            .as_ref()
            .map(|process| Arc::clone(&process.identity))
    }

    /// 短锁中只克隆独立文件/FS引用，I/O、用户访问与最后拥有释放由锁外调用者完成。
    pub(crate) fn runtime(&self) -> Option<UserRuntime> {
        self.process
            .lock()
            .as_ref()
            .map(|process| process.runtime.clone())
    }

    /// 当前用户循环先解除运行拥有，再用返回的身份发表退出；CPU仍独立保活活动根。
    pub(crate) fn release_process(&self) -> Arc<UserIdentity> {
        self.assert_current();
        let mm = self.mm.lock().take();
        let process = self
            .process
            .lock()
            .take()
            .expect("exiting user thread has no process resources");
        let UserProcess { identity, runtime } = process;
        drop(runtime);
        drop(mm);
        identity
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
        let local = crate::cpu::current();
        let timer_before = local.timer_interrupts.load(Ordering::Relaxed);
        // SAFETY: 弱关联已核对当前唯一 Running Task，RefCell 的独占借用排除现场别名；
        // IRQ guard 关闭本地中断，MM 与 CPU 活动拥有保证全部根、用户页及内核映射存活。
        // 架构在同一调用栈返回，IRQ 关闭且硬中断 guard 已结束，期间不切换 Task。
        let trap = unsafe { arch::run_user(&mut context) };
        if local.timer_interrupts.load(Ordering::Relaxed) != timer_before {
            self.user_timer_interrupts.set(
                self.user_timer_interrupts
                    .get()
                    .checked_add(1)
                    .expect("user timer interrupt count overflow"),
            );
        }
        trap
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

/// 尚未发表的完整程序只保留本次身份、用户对象和原调度任务，失败由普通拥有关系归还。
pub(crate) struct PreparedProgram {
    identity: PreparedIdentity,
    user: Arc<UserThread>,
    task: PreparedTask,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProgramError {
    Image(ExecError),
    Identity(ProcessError),
}

impl From<ExecError> for ProgramError {
    /// 保留映像准备的格式、参数、文件与 VM 原因。
    fn from(error: ExecError) -> Self {
        Self::Image(error)
    }
}

impl From<FsError> for ProgramError {
    /// 根文件准备错误沿映像文件原因传递，不替换成缺失程序。
    fn from(error: FsError) -> Self {
        Self::Image(ExecError::File(error))
    }
}

impl From<ProcessError> for ProgramError {
    /// 身份预算及父关系错误保持可区分。
    fn from(error: ProcessError) -> Self {
        Self::Identity(error)
    }
}

impl From<AllocError> for ProgramError {
    /// 未发表用户对象、入口和执行栈的分配失败按真实 NoMemory 返回。
    fn from(_: AllocError) -> Self {
        Self::Image(ExecError::NoMemory)
    }
}

impl PreparedProgram {
    /// 在发表前取得稳定用户号码，供协调者之后选择同一子进程事件。
    pub(crate) fn pid(&self) -> usize {
        self.identity.identity().tid()
    }

    /// 身份短锁发表成功后才移交 Ready；失败时没有可运行的半创建 Task。
    pub(crate) fn publish(self) -> Result<JoinHandle, ProcessError> {
        let Self {
            identity,
            user,
            task,
        } = self;
        identity.publish(Arc::downgrade(&user), task.task_id())?;
        Ok(task.publish())
    }
}

/// 在普通任务锁外完成身份、私有现场、独立运行资源和内核栈的全部 fallible 准备。
pub(crate) fn prepare_program(
    graph: &Arc<TaskGraph>,
    parent: Option<&Arc<ThreadGroup>>,
    image: LoadedImage,
    runtime: UserRuntime,
    placement: Placement,
) -> Result<PreparedProgram, ProgramError> {
    let identity = graph.prepare_process(parent)?;
    let user = UserThread::try_new(image.mm, image.context)?;
    user.bind_process(identity.identity(), runtime);
    let task = task::prepare_user(Arc::clone(&user), placement, run_program)?;
    Ok(PreparedProgram {
        identity,
        user,
        task,
    })
}

/// 同一真实 Task 循环执行用户现场；只有返回 Rust 并结束 IRQ/现场借用后才允许等待或调度。
fn run_program(user: Arc<UserThread>) -> i32 {
    let mut interrupts = 0usize;
    let status = loop {
        crate::sched::cond_resched();
        match user.run_user() {
            UserTrap::Interrupt => {
                interrupts = interrupts
                    .checked_add(1)
                    .expect("user interrupt count overflow");
            }
            UserTrap::Syscall => {
                let request = crate::syscall::Request::snapshot(&user.context());
                match crate::syscall::dispatch(&user, request) {
                    crate::syscall::Outcome::Return(result) => {
                        request.return_to(&mut user.context(), result);
                    }
                    crate::syscall::Outcome::Exit(status) => break status,
                }
            }
            UserTrap::PageFault { address, access } => {
                let result = user
                    .mm()
                    .expect("user fault has no MM")
                    .fault(address, access);
                if let Err(error) = result {
                    let signal = fault_signal(error);
                    crate::klog!(
                        crate::klog::Level::Warn,
                        "USER fault address={:#x} access={:?} cause={:?} signal={}",
                        address,
                        access,
                        error,
                        signal
                    );
                    break signal;
                }
            }
            UserTrap::Exception { code, address } => {
                let signal = exception_signal(code);
                crate::klog!(
                    crate::klog::Level::Warn,
                    "USER exception code={} address={:#x} signal={}",
                    code,
                    address,
                    signal
                );
                break signal;
            }
        }
    };
    let identity = user.release_process();
    identity.graph().exit(
        &identity,
        status,
        interrupts,
        user.user_timer_interrupts.get(),
    );
    status
}

/// 尚无捕获处置时按真实用户 fault 原因终止；文件 EOF/I/O 与地址/权限错误分开。
fn fault_signal(error: VmError) -> i32 {
    match error {
        VmError::NoMemory
        | VmError::File(FsError::NoMemory | FsError::Io(crate::block::BlockError::NoMemory)) => 9,
        VmError::BeyondEof | VmError::File(_) => 7,
        VmError::InvalidRange | VmError::Occupied | VmError::Unmapped | VmError::Permission => 11,
    }
}

/// 将本目标的实际同步用户异常映为默认终止信号，不影响内核异常的 fatal 路径。
#[cfg(target_arch = "riscv64")]
fn exception_signal(code: usize) -> i32 {
    match code {
        0 | 4 | 6 => 7,
        1 | 5 | 7 => 11,
        3 => 5,
        _ => 4,
    }
}

/// 对齐未启用扩展、地址对齐及断点的实际异常；内核异常仍由架构独立处理。
#[cfg(target_arch = "loongarch64")]
fn exception_signal(code: usize) -> i32 {
    match code {
        9 => 7,
        12 => 5,
        _ => 4,
    }
}
