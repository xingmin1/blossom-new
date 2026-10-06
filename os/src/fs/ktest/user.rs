//! 普通磁盘 ELF 复用原设备/路径夹具；用户 wait 与真实离栈 join 分别完成后才释放根。

use crate::fs::ktest::path::PathOwner;
use crate::fs::ktest::*;
use crate::mm::space::{Mm, VmError};
use crate::task::exec::{self, Arguments, ExecError, LoadedImage};
use crate::task::futex::{self, FutexSnapshot};
use crate::task::process::{TaskGraph, WaitEvent, WaitSelector};
use crate::task::{Credentials, Placement, TaskId, UserRuntime};

mod layout;
mod lifecycle;
mod preparation;

const PROGRAM: &[u8] = b"/test/bin/user-smoke";
const FUTEX_CONTROL: usize = 0x4400_0000;

struct RunningProgram {
    pid: usize,
    target: Option<usize>,
    coordinator: usize,
    expected_task: Option<TaskId>,
    join: JoinHandle,
}

struct ReservedDescriptor {
    release: Arc<Completion>,
    join: JoinHandle,
}

struct UserOwner {
    path: PathOwner,
    graph: Option<Arc<TaskGraph>>,
    running: Option<RunningProgram>,
    reserved: Option<ReservedDescriptor>,
    observer: Option<JoinHandle>,
}

impl UserOwner {
    /// 每例只用本例设备与根建立身份域，库存由同一 suite 保活到最后一个 case。
    fn prepare(&mut self, context: &TestContext<'_>) -> TestResult {
        self.path.prepare_path(context)?;
        self.graph = Some(TaskGraph::try_new(64).map_err(|error| {
            context.diagnostic(format_args!("user-graph-prepare error={:?}", error));
            TestError("user identity graph preparation failed")
        })?);
        Ok(())
    }

    /// 仅借本例已构造的身份图，不取得调度队列或运行资源拥有。
    fn graph(&self) -> &Arc<TaskGraph> {
        self.graph.as_ref().expect("user graph was not prepared")
    }

    /// 正式逐组件解析并打开真实普通文件，文件引用保活根与同源 FileMapping。
    fn file(&self, context: &TestContext<'_>, pathname: &[u8]) -> Result<FileRef, TestError> {
        let path = checked(
            context,
            self.path
                .vfs()
                .resolve(self.path.context(), pathname, ResolveMode::Follow),
        )?;
        checked(context, crate::fs::open_path(path))
    }

    /// 内核初次参数来自已拥有字节，与普通启动使用相同初始栈契约。
    fn arguments(&self) -> Result<Arguments, ExecError> {
        Arguments::from_kernel(PROGRAM, &[PROGRAM, b"raw"], &[b"USER_SMOKE=1"])
    }

    /// 通过同一正式 loader 准备真实盘 ELF，不把错误转换为默认页或备用程序。
    fn load(&self, context: &TestContext<'_>) -> Result<LoadedImage, TestError> {
        let file = self.file(context, PROGRAM)?;
        let arguments = self.arguments().map_err(|error| {
            context.diagnostic(format_args!("user-arguments-prepare error={:?}", error));
            TestError("user arguments preparation failed")
        })?;
        exec::load(file, &arguments, 0, 0).map_err(|error| {
            context.diagnostic(format_args!("user-image-prepare error={:?}", error));
            TestError("user image preparation failed")
        })
    }

    /// 原首ELF保持真实远核退出断言，具名后续负载复用同一准备与发表。
    fn start(&mut self, context: &TestContext<'_>, image: LoadedImage) -> TestResult {
        self.start_program(context, image, true, false, Credentials::ROOT)
    }

    /// 完整资源和可选并发预留先准备，身份发表后才发布普通用户Task。
    fn start_program(
        &mut self,
        context: &TestContext<'_>,
        image: LoadedImage,
        affine_group: bool,
        reserve_parent_fd: bool,
        credentials: Credentials,
    ) -> TestResult {
        assert!(
            self.running.is_none(),
            "user fixture already has a running child"
        );
        let runtime = checked(
            context,
            UserRuntime::try_new(
                self.path.vfs().clone(),
                self.path.context().clone(),
                credentials,
            ),
        )?;
        let coordinator = cpu::current().id;
        let target = (coordinator + 1) % cpu::count();
        let prepared =
            task::prepare_program(self.graph(), None, image, runtime, Placement::Cpu(target))
                .map_err(|error| {
                    context.diagnostic(format_args!("user-program-prepare error={:?}", error));
                    TestError("user program task preparation failed")
                })?;
        if reserve_parent_fd {
            let files = prepared
                .user()
                .runtime()
                .expect("prepared user has no runtime")
                .files;
            let ready = Arc::try_new(Completion::new())
                .map_err(|_| TestError("reserved ready allocation failed"))?;
            let release = Arc::try_new(Completion::new())
                .map_err(|_| TestError("reserved release allocation failed"))?;
            let worker_ready = ready.clone();
            let worker_release = release.clone();
            let worker = task::prepare_kernel_on(target, move || {
                let reservation = files
                    .reserve()
                    .expect("fresh user table has no reservation slot");
                assert_eq!(
                    reservation.fd(),
                    0,
                    "fresh user reservation is not descriptor zero"
                );
                worker_ready.complete();
                worker_release.wait();
                drop(reservation);
                0
            })
            .map_err(|_| TestError("reserved owner task preparation failed"))?;
            self.reserved = Some(ReservedDescriptor {
                release,
                join: worker.publish(),
            });
            ready.wait();
        }
        let pid = prepared.pid();
        let join = prepared.publish().map_err(|error| {
            context.diagnostic(format_args!("user-program-publish error={:?}", error));
            TestError("user identity publication failed")
        })?;
        self.running = Some(RunningProgram {
            pid,
            target: affine_group.then_some(target),
            coordinator,
            expected_task: Some(join.task_id()),
            join,
        });
        Ok(())
    }

    /// 具名普通程序复用正式ELF loader和原始argv，没有内嵌应用或测试syscall。
    fn load_named(
        &self,
        context: &TestContext<'_>,
        program: &[u8],
        mode: &[u8],
    ) -> Result<LoadedImage, TestError> {
        let arguments =
            Arguments::from_kernel(program, &[program, mode], &[]).map_err(|error| {
                context.diagnostic(format_args!("user-named-arguments error={:?}", error));
                TestError("named user arguments failed")
            })?;
        exec::load(self.file(context, program)?, &arguments, 0, 0).map_err(|error| {
            context.diagnostic(format_args!("user-named-load error={:?}", error));
            TestError("named user image failed")
        })
    }

    /// 普通参数只选确定程序模式，真实退出与整棵树离栈后才返回，不用用户完成字当回收。
    fn run_named(
        &mut self,
        context: &mut TestContext<'_>,
        program: &[u8],
        mode: &[u8],
        reserve_parent_fd: bool,
    ) -> Result<WaitEvent, TestError> {
        self.run_named_as(context, program, mode, reserve_parent_fd, Credentials::ROOT)
    }

    /// 不可变凭据只在准备Runtime时注入，用户请求仍经过正式dispatcher与正常wait。
    fn run_named_as(
        &mut self,
        context: &mut TestContext<'_>,
        program: &[u8],
        mode: &[u8],
        reserve_parent_fd: bool,
        credentials: Credentials,
    ) -> Result<WaitEvent, TestError> {
        let image = self.load_named(context, program, mode)?;
        let affine_group = !matches!(mode.first(), Some(b't' | b'T' | b'g' | b'c'));
        self.start_program(context, image, affine_group, reserve_parent_fd, credentials)?;
        let event = self.wait(context)?;
        let cancelling = event.status != 0;
        if cancelling {
            self.graph().request_root_exit(9);
        }
        self.reap_children(context, cancelling)?;
        self.release_reserved(context);
        context.diagnostic(format_args!(
            "user-named-result program={:?} mode={:?} status={}",
            program, mode, event.status
        ));
        Ok(event)
    }

    /// 逐个消费转亲的其他子组并等全部成员离栈，取消的退出状态不充当主体断言。
    fn reap_children(&self, context: &mut TestContext<'_>, cancelling: bool) -> TestResult {
        loop {
            match self.graph().wait_root() {
                Ok(event) => {
                    if !cancelling {
                        context.expect_eq(event.status, 0);
                    }
                    context.diagnostic(format_args!(
                        "user-child-reaped pid={} task={} status={} interrupts={} timers={} cpu={}",
                        event.pid,
                        event.task_id.as_usize(),
                        event.status,
                        event.user_interrupts,
                        event.user_timer_interrupts,
                        event.cpu
                    ));
                }
                Err(crate::task::process::ProcessError::NoChild) => return Ok(()),
                Err(error) => {
                    context.diagnostic(format_args!("user-child-reap error={:?}", error));
                    return Err(TestError("user process tree reap failed"));
                }
            }
        }
    }

    /// 发出原完成token后等待预留拥有者离栈，不能先丢仍借文件表的worker。
    fn release_reserved(&mut self, context: &mut TestContext<'_>) {
        if let Some(reserved) = self.reserved.take() {
            reserved.release.complete();
            context.expect_eq(reserved.join.join(), 0);
        }
    }

    /// 消费一次用户退出事件，再等待原 Task 离栈；两次观察必须属于同一实际执行者。
    fn wait(&mut self, context: &mut TestContext<'_>) -> Result<WaitEvent, TestError> {
        let running = self.running.as_ref().expect("user fixture has no child");
        let event = self.graph().wait(None, running.pid).map_err(|error| {
            context.diagnostic(format_args!("user-program-wait error={:?}", error));
            TestError("user child wait failed")
        })?;
        let RunningProgram {
            pid,
            target,
            coordinator,
            expected_task,
            join,
        } = self.running.take().unwrap();
        context.expect_eq(event.pid, pid);
        if let Some(expected_task) = expected_task {
            context.expect_eq(event.task_id, expected_task);
        }
        let task_status = join.join();
        if let Some(target) = target {
            context.expect_eq(event.cpu, target);
            context.expect_eq(task_status, event.status);
        }
        context.diagnostic(format_args!(
            "user-program-wait pid={} task={} status={} interrupts={} timers={} cpu={} coordinator_cpu={}",
            event.pid,
            event.task_id.as_usize(),
            event.status,
            event.user_interrupts,
            event.user_timer_interrupts,
            event.cpu,
            coordinator,
        ));
        Ok(event)
    }

    /// 使用原设备拥有者已经预留的真实堆压力对象，不在 OOM 期准备观测存储。
    fn pressure(&mut self) -> &mut crate::cache::ktest::HeapPressure {
        self.path
            .owner
            .pressure
            .as_mut()
            .expect("user pressure was not prepared")
    }

    /// 移出并销毁原压力拥有，之后才能进行可能分配的诊断与恢复准备。
    fn release_pressure(&mut self) {
        drop(self.path.owner.pressure.take());
    }

    /// 保活canonical页的唯一填充责任，确认用户read实际Blocked后发信号，再从原盘完成页。
    fn signal_during_file_wait(&mut self, context: &mut TestContext<'_>) -> TestResult {
        let path = checked(
            context,
            self.path
                .vfs()
                .resolve(self.path.context(), b"/test/data.bin", ResolveMode::Follow),
        )?;
        let inode = checked(context, path.inode())?.clone();
        let data = checked(context, inode.metadata())?;
        let (folio, read) = checked(
            context,
            inode.mapping().get_or_prepare(0).map_err(FsError::Io),
        )?;
        let read = read.ok_or(TestError("fresh canonical file page already filled"))?;
        // 先沿原文件页读取程序，排除发信号前把指令页加载误作data页等待。
        let program = self.file(context, b"/test/bin/user-signal")?;
        let mut bytes = [0u8; 4096];
        let mut offset = 0;
        loop {
            let count = checked(context, program.read_at(offset, &mut bytes))?;
            if count == 0 {
                break;
            }
            offset += count as u64;
        }
        let image = self.load_named(context, b"/test/bin/user-signal", b"r")?;
        #[cfg(target_arch = "riscv64")]
        let marker = image.context.gpr[2] - 8;
        #[cfg(target_arch = "loongarch64")]
        let marker = image.context.gpr[3] - 8;
        let mm = Arc::clone(&image.mm);
        self.start_program(context, image, true, false, Credentials::ROOT)?;
        let pid = self
            .running
            .as_ref()
            .expect("file program is not published")
            .pid;
        let user = self
            .graph()
            .lookup_thread(pid)
            .ok_or(TestError("file reader user binding disappeared"))?;
        let executor = user
            .executor()
            .ok_or(TestError("file reader executor disappeared"))?;
        until("user file reader did not block on canonical page", || {
            let mut marker_bytes = [0u8; 8];
            let progress = mm.copyin(&mut marker_bytes, marker);
            progress.error.is_none()
                && u64::from_le_bytes(marker_bytes) == 0x51a2
                && sched::test_state(&executor) == State::Blocked
        })?;
        self.graph().send_kernel(pid, 14).map_err(|error| {
            context.diagnostic(format_args!("user-file-signal-send error={:?}", error));
            TestError("user file signal request failed")
        })?;
        context.expect_eq(sched::test_state(&executor), State::Blocked);
        let signals = user
            .signals()
            .ok_or(TestError("file reader signals disappeared"))?;
        context.expect_eq(
            signals.interruption_pending(crate::wait::WaitMode::Interruptible),
            true,
        );
        checked(context, inode.filesystem().fill_page(data, 0, read))?;
        checked(context, folio.wait_ready().map_err(FsError::Io))?;
        let event = self.wait(context)?;
        context.expect_eq(event.status, 0);
        self.reap_children(context, false)?;
        context.diagnostic(format_args!(
            "user-file-signal canonical-page-wait=1 dma-inflight-at-send=0 delivered-bytes=65536"
        ));
        Ok(())
    }

    /// 同CPU的普通内核Task只有在用户发表无限循环标记后才确认推进，远核再请求SIGKILL。
    fn preempt_infinite(&mut self, context: &mut TestContext<'_>) -> TestResult {
        let image = self.load_named(context, b"/test/bin/user-signal", b"i")?;
        #[cfg(target_arch = "riscv64")]
        let marker = image.context.gpr[2] - 8;
        #[cfg(target_arch = "loongarch64")]
        let marker = image.context.gpr[3] - 8;
        let mm = Arc::clone(&image.mm);
        let runtime = checked(
            context,
            UserRuntime::try_new(
                self.path.vfs().clone(),
                self.path.context().clone(),
                Credentials::ROOT,
            ),
        )?;
        let coordinator = cpu::current().id;
        let target = (coordinator + 1) % cpu::count();
        let prepared =
            task::prepare_program(self.graph(), None, image, runtime, Placement::Cpu(target))
                .map_err(|_| TestError("infinite user preparation failed"))?;
        let pid = prepared.pid();
        let user = Arc::downgrade(&prepared.user());
        let ready = Arc::try_new(Completion::new())
            .map_err(|_| TestError("infinite observer completion allocation failed"))?;
        let worker_ready = Arc::clone(&ready);
        let observer = task::prepare_kernel_on(target, move || {
            let mut bytes = [0u8; 8];
            let result = loop {
                let progress = mm.copyin(&mut bytes, marker);
                if progress.error.is_some() {
                    break -1;
                }
                if u64::from_le_bytes(bytes) == 0x51a1 {
                    break 1;
                }
                if user.upgrade().is_none_or(|user| user.mm().is_none()) {
                    break -2;
                }
                sched::yield_now();
            };
            worker_ready.complete();
            result
        })
        .map_err(|_| TestError("infinite same-CPU observer preparation failed"))?;
        self.observer = Some(observer.publish());
        // 所有目标CPU的Ready工作在用户开始前发表，观察者发表的IPI不能代替timer抢占。
        let join = prepared
            .publish()
            .map_err(|_| TestError("infinite user publication failed"))?;
        self.running = Some(RunningProgram {
            pid,
            target: Some(target),
            coordinator,
            expected_task: Some(join.task_id()),
            join,
        });
        ready.wait();
        let advanced = self.observer.take().unwrap().join();
        context.expect_eq(advanced, 1);
        struct RemoteKill {
            graph: Arc<TaskGraph>,
            pid: usize,
            sender: usize,
            result: Option<Result<(), crate::task::signal::SignalError>>,
        }
        /// 原协调核在同步CSD中发送信号，不依赖等待后可能迁核的suite Task位置。
        ///
        /// # Safety
        /// argument须指向同步call返回前固定且独占的RemoteKill；回调不睡眠、不访问用户内存。
        unsafe fn send_remote_kill(argument: *mut ()) {
            // SAFETY: call的同步完成Release/Acquire保活栈参数，调用者返回前不访问这些字段。
            let request = unsafe { &mut *argument.cast::<RemoteKill>() };
            request.sender = cpu::current().id;
            request.result = Some(request.graph.send_kernel(request.pid, 9));
        }
        let mut kill = RemoteKill {
            graph: Arc::clone(self.graph()),
            pid,
            sender: usize::MAX,
            result: None,
        };
        // SAFETY: 原协调核在线且不同于用户目标核；当前Task可服务IPI、不持回调所需锁，
        // 栈参数与graph拥有保留至同步call的Acquire完成，回调只发送信号、不睡眠或做uaccess。
        unsafe {
            cpu::call(
                coordinator,
                send_remote_kill,
                (&mut kill as *mut RemoteKill).cast(),
            )
        };
        context.expect_eq(kill.sender, coordinator);
        context.expect_eq(kill.sender == target, false);
        kill.result
            .expect("remote kill callback did not complete")
            .map_err(|error| {
                context.diagnostic(format_args!("user-remote-kill-send error={:?}", error));
                TestError("remote user kill request failed")
            })?;
        let event = self.wait(context)?;
        context.expect_eq(event.status, 9);
        context.expect_eq(0 < event.user_timer_interrupts, true);
        self.reap_children(context, false)?;
        Ok(())
    }

    /// 两个具体信号回归共用原发表入口，控制字在父初始栈内，不能充当退出完成事实。
    fn start_signal_checkpoint(
        &mut self,
        context: &TestContext<'_>,
        mode: &[u8],
    ) -> Result<(Arc<crate::mm::space::Mm>, usize, Arc<task::UserThread>), TestError> {
        let image = self.load_named(context, b"/test/bin/user-signal", mode)?;
        #[cfg(target_arch = "riscv64")]
        let marker = image.context.gpr[2] - 16;
        #[cfg(target_arch = "loongarch64")]
        let marker = image.context.gpr[3] - 16;
        let mm = Arc::clone(&image.mm);
        self.start_program(context, image, true, false, Credentials::ROOT)?;
        let pid = self
            .running
            .as_ref()
            .expect("signal program is not published")
            .pid;
        let user = self
            .graph()
            .lookup_thread(pid)
            .ok_or(TestError("signal checkpoint user disappeared"))?;
        Ok((mm, marker, user))
    }

    /// 正常父资格消费并确认子成员离栈后，才允许用户检查真实零/非零退出信号。
    fn signal_clone_exit_notification(&mut self, context: &mut TestContext<'_>) -> TestResult {
        let (mm, marker, user) = self.start_signal_checkpoint(context, b"e")?;
        let parent = user
            .identity()
            .expect("signal parent has no identity")
            .group();
        for phase in [1u32, 3u32].iter().copied() {
            until("clone exit notification checkpoint was not reached", || {
                mm.load_user_u32(marker + 8) == Ok(phase) || user.mm().is_none()
            })?;
            if mm.load_user_u32(marker + 8) != Ok(phase) {
                break;
            }
            let pid = mm
                .read_usize(marker)
                .map_err(|_| TestError("clone checkpoint PID is inaccessible"))?;
            let child = self
                .graph()
                .wait(Some(&parent), pid)
                .map_err(|_| TestError("clone checkpoint child wait failed"))?;
            context.expect_eq(child.status, 0);
            context.diagnostic(format_args!(
                "signal-exit-notification phase={} pid={} child-status={} joined=1",
                phase, pid, child.status
            ));
            mm.store_user_u32(marker + 8, phase + 1)
                .map_err(|_| TestError("clone checkpoint release failed"))?;
        }
        let event = self.wait(context)?;
        context.expect_eq(event.status, 0);
        self.reap_children(context, event.status != 0)?;
        Ok(())
    }

    /// 先观测真实临时mask/Blocked和组停止事件，再由共享VM的用户父解除参数页。
    fn signal_suspend_restart_mask(&mut self, context: &mut TestContext<'_>) -> TestResult {
        let (mm, marker, user) = self.start_signal_checkpoint(context, b"u")?;
        let parent = user
            .identity()
            .expect("suspend parent has no identity")
            .group();
        until("suspend child checkpoint was not reached", || {
            mm.load_user_u32(marker + 8) == Ok(1) || user.mm().is_none()
        })?;
        if user.mm().is_none() {
            let event = self.wait(context)?;
            context.expect_eq(event.status, 0);
            return Ok(());
        }
        let pid = mm
            .read_usize(marker)
            .map_err(|_| TestError("suspend checkpoint PID is inaccessible"))?;
        let child = self
            .graph()
            .lookup_thread(pid)
            .ok_or(TestError("suspend child user disappeared"))?;
        let signals = child.signals().expect("suspend child has no signals");
        let executor = child
            .executor()
            .ok_or(TestError("suspend child executor disappeared"))?;
        until(
            "sigsuspend did not register its temporary mask and wait",
            || {
                (signals.mask() == 0x800 && sched::test_state(&executor) == State::Blocked)
                    || child.mm().is_none()
            },
        )?;
        if child.mm().is_none() {
            return Err(TestError("suspend child exited before signal checkpoint"));
        }
        self.graph()
            .send_kernel(pid, 19)
            .map_err(|_| TestError("suspend child stop request failed"))?;
        let selector = WaitSelector::Pid(
            self.graph()
                .pid_object(pid)
                .ok_or(TestError("suspend child identity disappeared"))?,
        );
        let mut stopped = None;
        let mut wait_error = None;
        until(
            "suspend child did not acknowledge the group stop",
            || match self.graph().try_wait_event(&parent, &selector, 0x0100_0003) {
                Ok(Some(event)) => {
                    stopped = Some(event);
                    true
                }
                Ok(None) => false,
                Err(error) => {
                    wait_error = Some(error);
                    true
                }
            },
        )?;
        if let Some(error) = wait_error {
            context.diagnostic(format_args!("suspend-stop-wait error={:?}", error));
            return Err(TestError("suspend stopped event lookup failed"));
        }
        let stopped = stopped.ok_or(TestError("suspend stopped event is absent"))?;
        context.expect_eq(stopped.code, 5);
        context.expect_eq(stopped.status, 19);
        mm.store_user_u32(marker + 8, 2)
            .map_err(|_| TestError("suspend parent unmap release failed"))?;
        until(
            "suspend parent did not unmap the shared argument page",
            || mm.load_user_u32(marker + 8) == Ok(3) || user.mm().is_none(),
        )?;
        if mm.load_user_u32(marker + 8) != Ok(3) {
            return Err(TestError("suspend parent exited before unmap checkpoint"));
        }
        context.expect_eq(
            mm.read_u64(0x4300_1000),
            Err(crate::mm::space::VmError::Unmapped),
        );
        context.diagnostic(format_args!(
            "signal-suspend-checkpoint temporary-mask=0x800 stopped={} parent-unmapped=1",
            stopped.status
        ));
        self.graph()
            .send_kernel(pid, 18)
            .map_err(|_| TestError("suspend child continue request failed"))?;
        let event = self.wait(context)?;
        context.expect_eq(event.status, 0);
        self.reap_children(context, event.status != 0)?;
        Ok(())
    }

    /// 用户进度只表示协议位置；真正的等待登记另由桶快照和Task状态确认。
    fn futex_phase(mm: &Mm, root: &task::UserThread, phase: u32) -> TestResult {
        until("futex user did not reach its checkpoint", || {
            mm.load_user_u32(FUTEX_CONTROL + 8) == Ok(phase) || root.mm().is_none()
        })?;
        if mm.load_user_u32(FUTEX_CONTROL + 8) != Ok(phase) {
            return Err(TestError("futex user exited before its checkpoint"));
        }
        Ok(())
    }

    /// 从用户已经发表的真实TID取得原绑定，不建立第二份身份或等待状态。
    fn futex_child(&self, mm: &Mm, slot: usize) -> Result<Arc<task::UserThread>, TestError> {
        let pid = mm
            .read_usize(FUTEX_CONTROL + slot)
            .map_err(|_| TestError("futex checkpoint TID is inaccessible"))?;
        self.graph()
            .lookup_thread(pid)
            .ok_or(TestError("futex checkpoint child disappeared"))
    }

    /// 同时观察实际登记与Blocked，再读取原WaitId/期限；提示字不是睡眠证明。
    fn futex_waiting(
        mm: &Arc<Mm>,
        user: &task::UserThread,
        address: usize,
        private: bool,
        mask: u32,
    ) -> Result<FutexSnapshot, TestError> {
        let executor = user
            .executor()
            .ok_or(TestError("futex checkpoint executor disappeared"))?;
        let mut observed = None;
        until("futex user did not register and block", || {
            if let Ok(snapshot) = futex::test_snapshot(mm, address, private, mask) {
                if snapshot.count == 1 && sched::test_state(&executor) == State::Blocked {
                    observed = Some(snapshot);
                    return true;
                }
            }
            user.mm().is_none()
        })?;
        observed.ok_or(TestError("futex user exited before registering"))
    }

    /// 普通ELF完成实际WAIT/WAKE和waitid；夹具只在真实登记检查点释放用户动作。
    fn futex_program(&mut self, context: &mut TestContext<'_>, mode: &[u8]) -> TestResult {
        let image = self.load_named(context, b"/test/bin/user-futex", mode)?;
        let mm = Arc::clone(&image.mm);
        self.start_program(context, image, true, false, Credentials::ROOT)?;
        let pid = self
            .running
            .as_ref()
            .expect("futex program was not published")
            .pid;
        let root = self
            .graph()
            .lookup_thread(pid)
            .ok_or(TestError("futex root binding disappeared"))?;
        Self::futex_phase(&mm, &root, 1)?;
        let address = if mode[0] == b'w' {
            0x4600_0000
        } else {
            FUTEX_CONTROL + 64
        };
        if mode[0] == b't' {
            Self::futex_waiting(&mm, &root, address, false, u32::MAX)?;
        }
        let child = self.futex_child(&mm, 0)?;
        let child_mm = child.mm().ok_or(TestError("futex child MM disappeared"))?;
        match mode[0] {
            b'w' => {
                Self::futex_waiting(&child_mm, &child, address, true, u32::MAX)?;
                mm.store_user_u32(FUTEX_CONTROL + 12, 1)
                    .map_err(|_| TestError("futex wake checkpoint release failed"))?;
            }
            b'p' | b'n' => {
                let private = mode[0] == b'p';
                let address = if private { address } else { 0x4600_0000 };
                Self::futex_waiting(&child_mm, &child, address, private, u32::MAX)?;
                let isolated = futex::test_snapshot(&mm, address, private, u32::MAX)
                    .map_err(|_| TestError("isolated futex key lookup failed"))?;
                context.expect_eq(isolated.count, 0);
                mm.store_user_u32(FUTEX_CONTROL + 12, 1)
                    .map_err(|_| TestError("futex isolation checkpoint release failed"))?;
                Self::futex_phase(&mm, &root, 2)?;
                let child_pid = child.identity().expect("futex child has no identity").tid();
                self.graph()
                    .send_kernel(child_pid, 10)
                    .map_err(|_| TestError("futex isolation signal request failed"))?;
                mm.store_user_u32(FUTEX_CONTROL + 12, 2)
                    .map_err(|_| TestError("futex isolation signal release failed"))?;
            }
            b'a' => {
                child_mm
                    .test_shared_alias(FUTEX_CONTROL, 0x4500_0000, 4096)
                    .map_err(|error| {
                        context.diagnostic(format_args!("user-futex-alias error={:?}", error));
                        TestError("shared anonymous futex alias preparation failed")
                    })?;
                mm.store_user_u32(FUTEX_CONTROL + 12, 1)
                    .map_err(|_| TestError("futex alias checkpoint release failed"))?;
                Self::futex_phase(&mm, &root, 2)?;
                let alias = Self::futex_waiting(&child_mm, &child, 0x4500_0040, false, u32::MAX)?;
                let original = futex::test_snapshot(&mm, address, false, u32::MAX)
                    .map_err(|_| TestError("shared futex original key lookup failed"))?;
                context.expect_eq(original.count, 1);
                context.expect_eq(original.first_wait_id, alias.first_wait_id);
                mm.store_user_u32(FUTEX_CONTROL + 12, 2)
                    .map_err(|_| TestError("futex shared-key wake release failed"))?;
            }
            b't' => {
                context.expect_eq(
                    mm.load_user_u32(address),
                    Ok(child.identity().expect("join child has no identity").tid() as u32),
                );
                context.expect_eq(mm.user_count(), 2);
                mm.store_user_u32(FUTEX_CONTROL + 12, 1)
                    .map_err(|_| TestError("futex child exit release failed"))?;
            }
            b's' => {
                let first = Self::futex_waiting(&child_mm, &child, address, true, u32::MAX)?;
                let child_pid = child
                    .identity()
                    .expect("restart child has no identity")
                    .tid();
                self.graph()
                    .send_kernel(child_pid, 10)
                    .map_err(|_| TestError("futex restart signal request failed"))?;
                until("futex restart handler did not execute", || {
                    mm.load_user_u32(FUTEX_CONTROL + 32) == Ok(1) || child.mm().is_none()
                })?;
                let restarted = Self::futex_waiting(&child_mm, &child, address, true, u32::MAX)?;
                context.expect_eq(first.first_wait_id == restarted.first_wait_id, false);
                context.expect_eq(restarted.first_deadline, None);
                mm.store_user_u32(FUTEX_CONTROL + 12, 1)
                    .map_err(|_| TestError("futex restarted wake release failed"))?;
            }
            b'd' => {
                let first = Self::futex_waiting(&child_mm, &child, address, true, u32::MAX)?;
                let deadline = first
                    .first_deadline
                    .ok_or(TestError("timed futex registration has no deadline"))?;
                let child_pid = child
                    .identity()
                    .expect("deadline child has no identity")
                    .tid();
                self.graph()
                    .send_kernel(child_pid, 19)
                    .map_err(|_| TestError("futex deadline stop request failed"))?;
                let parent = root
                    .identity()
                    .expect("deadline parent has no identity")
                    .group();
                let selector = WaitSelector::Pid(
                    self.graph()
                        .pid_object(child_pid)
                        .ok_or(TestError("futex child identity disappeared"))?,
                );
                let mut stopped = None;
                let mut wait_error = None;
                until("futex child did not acknowledge group stop", || match self
                    .graph()
                    .try_wait_event(&parent, &selector, 0x0100_0003)
                {
                    Ok(Some(event)) => {
                        stopped = Some(event);
                        true
                    }
                    Ok(None) => false,
                    Err(error) => {
                        wait_error = Some(error);
                        true
                    }
                })?;
                if let Some(error) = wait_error {
                    context.diagnostic(format_args!("futex-stop-wait error={:?}", error));
                    return Err(TestError("futex stopped event lookup failed"));
                }
                let stopped = stopped.ok_or(TestError("futex stopped event is absent"))?;
                context.expect_eq(stopped.code, 5);
                context.expect_eq(stopped.status, 19);
                mm.store_user_u32(FUTEX_CONTROL + 12, 1)
                    .map_err(|_| TestError("futex timeout unmap release failed"))?;
                Self::futex_phase(&mm, &root, 2)?;
                context.expect_eq(mm.read_u64(0x4700_0000), Err(VmError::Unmapped));
                until("futex original deadline was not reached", || {
                    deadline <= crate::time::now()
                })?;
                context.diagnostic(format_args!(
                    "user-futex-deadline first-deadline={} now={} stopped={} timeout-page-unmapped=1",
                    deadline.as_nanos(), crate::time::now().as_nanos(), stopped.status
                ));
                self.graph()
                    .send_kernel(child_pid, 18)
                    .map_err(|_| TestError("futex deadline continue request failed"))?;
                mm.store_user_u32(FUTEX_CONTROL + 12, 2)
                    .map_err(|_| TestError("futex deadline continue release failed"))?;
            }
            b'r' => {
                Self::futex_waiting(&child_mm, &child, address, true, 1)?;
                let second = self.futex_child(&mm, 16)?;
                let first = Self::futex_waiting(&mm, &second, address, true, 2)?;
                let wait_id = first
                    .first_wait_id
                    .ok_or(TestError("bitset wait has no identity"))?;
                let deadline = first
                    .first_deadline
                    .ok_or(TestError("bitset wait has no deadline"))?;
                mm.store_user_u32(FUTEX_CONTROL + 12, 1)
                    .map_err(|_| TestError("futex bitset selection release failed"))?;
                for (phase, current) in [(2u32, address + 4), (3, address)].iter().copied() {
                    Self::futex_phase(&mm, &root, phase)?;
                    let moved = Self::futex_waiting(&mm, &second, current, true, 2)?;
                    context.expect_eq(moved.first_wait_id, Some(wait_id));
                    context.expect_eq(moved.first_deadline, Some(deadline));
                    let previous = if phase == 2 { address } else { address + 4 };
                    let empty = futex::test_snapshot(&mm, previous, true, u32::MAX)
                        .map_err(|_| TestError("requeue previous key lookup failed"))?;
                    context.expect_eq(empty.count, 0);
                    context.diagnostic(format_args!(
                        "user-futex-requeue phase={} wait-id={:?} deadline={} current={:#x}",
                        phase,
                        moved.first_wait_id,
                        deadline.as_nanos(),
                        current
                    ));
                    mm.store_user_u32(FUTEX_CONTROL + 12, phase)
                        .map_err(|_| TestError("futex requeue checkpoint release failed"))?;
                }
                for (phase, mask, signal) in [(4u32, 4u32, true), (6, 8, false)].iter().copied() {
                    Self::futex_phase(&mm, &root, phase)?;
                    let waiting = self.futex_child(&mm, 0)?;
                    let first = Self::futex_waiting(&mm, &waiting, address, true, mask)?;
                    let wait_id = first
                        .first_wait_id
                        .ok_or(TestError("requeue wait has no identity"))?;
                    let deadline = first
                        .first_deadline
                        .ok_or(TestError("requeue wait has no deadline"))?;
                    mm.store_user_u32(FUTEX_CONTROL + 12, phase)
                        .map_err(|_| TestError("futex cancellation requeue release failed"))?;
                    Self::futex_phase(&mm, &root, phase + 1)?;
                    let moved = Self::futex_waiting(&mm, &waiting, address + 4, true, mask)?;
                    context.expect_eq(moved.first_wait_id, Some(wait_id));
                    context.expect_eq(moved.first_deadline, Some(deadline));
                    let empty = futex::test_snapshot(&mm, address, true, u32::MAX)
                        .map_err(|_| TestError("requeue cancelled source key lookup failed"))?;
                    context.expect_eq(empty.count, 0);
                    if signal {
                        let waiting_pid = waiting
                            .identity()
                            .expect("requeue child has no identity")
                            .tid();
                        self.graph()
                            .send_kernel(waiting_pid, 10)
                            .map_err(|_| TestError("futex requeue cancellation signal failed"))?;
                    } else {
                        until("requeued futex deadline was not reached", || {
                            deadline <= crate::time::now()
                        })?;
                    }
                    context.diagnostic(format_args!(
                        "user-futex-requeue-cancel phase={} wait-id={:?} deadline={} signal={}",
                        phase + 1,
                        moved.first_wait_id,
                        deadline.as_nanos(),
                        signal
                    ));
                    mm.store_user_u32(FUTEX_CONTROL + 12, phase + 1)
                        .map_err(|_| {
                            TestError("futex requeue cancellation checkpoint release failed")
                        })?;
                }
            }
            _ => unreachable!("unknown futex fixture mode"),
        }
        let event = self.wait(context)?;
        context.expect_eq(event.status, 0);
        let cancelling = event.status != 0;
        if cancelling {
            self.graph().request_root_exit(9);
        }
        self.reap_children(context, cancelling)?;
        context.expect_eq(mm.user_count(), 0);
        context.diagnostic(format_args!(
            "user-futex-result mode={:?} status={} joined=1 mm-users={}",
            mode,
            event.status,
            mm.user_count()
        ));
        Ok(())
    }
}

impl Fixture<DeviceInventory> for UserOwner {
    /// 设置期保留原设备清理责任；路径、身份与用户映像均在 case 正文准备。
    fn set_up(
        context: &mut TestContext<'_>,
        inventory: &DeviceInventory,
    ) -> Result<Self, TestError> {
        Ok(Self {
            path: <PathOwner as Fixture<DeviceInventory>>::set_up(context, inventory)?,
            graph: None,
            running: None,
            reserved: None,
            observer: None,
        })
    }

    /// 取消时向本例整个身份图请求合法点退出，原wait及离栈join完成后才释放两层资源。
    fn quiesce(&mut self, context: &mut TestContext<'_>) -> TestResult {
        self.release_reserved(context);
        if let Some(graph) = &self.graph {
            graph.request_root_exit(9);
        }
        if self.running.is_some() {
            self.wait(context)?;
        }
        if let Some(observer) = self.observer.take() {
            let _ = observer.join();
        }
        if self.graph.is_some() {
            self.reap_children(context, true)?;
        }
        drop(self.graph.take());
        self.release_pressure();
        <PathOwner as Fixture<DeviceInventory>>::quiesce(&mut self.path, context)
    }

    /// 用户与设备都完成真实清理后沿原夹具比较本例完整六域。
    fn tear_down(self, context: &mut TestContext<'_>) -> TestResult {
        assert!(
            self.running.is_none()
                && self.graph.is_none()
                && self.reserved.is_none()
                && self.observer.is_none(),
            "user fixture is not quiesced"
        );
        <PathOwner as Fixture<DeviceInventory>>::tear_down(self.path, context)
    }
}

/// 父入口只保留原生 suite 宏的真实锚点。
pub(crate) fn anchor() -> &'static ::ktest::SuiteSlot {
    programs::__ktest_anchor()
}

#[ktest::suite(fixture = UserOwner, suite_resources = DeviceInventory, context = "Thread", tags("user-elf", "user-abi", "user-signal", "fs-path", "fs-oom"), resources("root"), timeout_ms = 90000)]
pub(crate) mod programs {
    use super::*;

    /// 真实 ELF 的边缘、BSS、参数和缓存私有性由 loader 与实际用户指令共同验证。
    #[test]
    #[ktest(min_cpus = 2)]
    fn load_boundaries_stack_and_disk_program(
        context: &mut TestContext<'_>,
        fixture: &mut UserOwner,
    ) -> TestResult {
        layout::run(context, fixture)
    }

    /// OOM、坏参数和未发表归还之后仍正常运行原真实程序。
    #[test]
    fn failed_preparation_keeps_original_program(
        context: &mut TestContext<'_>,
        fixture: &mut UserOwner,
    ) -> TestResult {
        preparation::run(context, fixture)
    }

    /// 用户指令核文件真实前缀/cookie、只读边界、映射和堆内容，退出仍由正常图回收。
    #[test]
    fn file_delivery_and_mapping_abi(
        context: &mut TestContext<'_>,
        fixture: &mut UserOwner,
    ) -> TestResult {
        fixture.prepare(context)?;
        let event = fixture.run_named(context, b"/test/bin/user-io", b"raw", false)?;
        context.expect_eq(event.status, 0);
        Ok(())
    }

    /// 非owner用户指令核目录读DAC、逐组件search及拒绝后原游标和最低空槽。
    #[test]
    fn nonroot_directory_search_and_access(
        context: &mut TestContext<'_>,
        fixture: &mut UserOwner,
    ) -> TestResult {
        fixture.prepare(context)?;
        let denied = checked(context, fixture.file(context, b"/test/io-denied")?.stat())?;
        let no_search = checked(
            context,
            fixture.file(context, b"/test/io-no-search")?.stat(),
        )?;
        context.expect_eq(denied.kind, InodeKind::Directory);
        context.expect_eq(denied.mode & 0o005, 0);
        context.expect_eq(no_search.kind, InodeKind::Directory);
        context.expect_eq(no_search.mode & 0o005, 0o004);
        let credentials = Credentials {
            uid: [1, 2, 3]
                .iter()
                .copied()
                .find(|&uid| uid != denied.uid && uid != no_search.uid)
                .unwrap(),
            gid: [1, 2, 3]
                .iter()
                .copied()
                .find(|&gid| gid != denied.gid && gid != no_search.gid)
                .unwrap(),
        };
        context.diagnostic(format_args!(
            "user-access-credentials uid={} gid={} denied_owner={}:{} no_search_owner={}:{}",
            credentials.uid, credentials.gid, denied.uid, denied.gid, no_search.uid, no_search.gid
        ));
        let event =
            fixture.run_named_as(context, b"/test/bin/user-io", b"access", false, credentials)?;
        context.expect_eq(event.status, 0);
        Ok(())
    }

    /// 四个代表路径覆盖私有fork、共享VM、独立组共享动作及线程组，不穷举共享轴。
    #[test]
    #[ktest(min_cpus = 2)]
    fn clone_tls_and_resource_axes(
        context: &mut TestContext<'_>,
        fixture: &mut UserOwner,
    ) -> TestResult {
        fixture.prepare(context)?;
        const MODES: &[&[u8]] = &[b"f0", b"v1", b"s2", b"T3"];
        for &mode in MODES {
            let event = fixture.run_named(context, b"/test/bin/user-clone", mode, false)?;
            context.expect_eq(event.status, 0);
        }
        Ok(())
    }

    /// 另一真实Task保父fd0预留时rawfork，child普通open获得0且原OFD/flags关系不变。
    #[test]
    #[ktest(min_cpus = 2)]
    fn fork_does_not_inherit_reserved_descriptor(
        context: &mut TestContext<'_>,
        fixture: &mut UserOwner,
    ) -> TestResult {
        fixture.prepare(context)?;
        let event = fixture.run_named(context, b"/test/bin/user-clone", b"r0", true)?;
        context.expect_eq(event.status, 0);
        Ok(())
    }

    /// 普通/private/group合并、RT FIFO及真实队列耗尽由raw程序逐实例核对。
    #[test]
    fn signal_pending_and_realtime_queue(
        context: &mut TestContext<'_>,
        fixture: &mut UserOwner,
    ) -> TestResult {
        fixture.prepare(context)?;
        let event = fixture.run_named(context, b"/test/bin/user-signal", b"p", false)?;
        context.expect_eq(event.status, 0);
        Ok(())
    }

    /// 同一SIGHAND和VM共享者仍有各自进程组pending，clone后旧pending不继承。
    #[test]
    fn signal_shared_vm_distinct_group_pending(
        context: &mut TestContext<'_>,
        fixture: &mut UserOwner,
    ) -> TestResult {
        fixture.prepare(context)?;
        let event = fixture.run_named(context, b"/test/bin/user-signal", b"v", false)?;
        context.expect_eq(event.status, 0);
        Ok(())
    }

    /// NODEFER、动作mask、RESETHAND和默认忽略均观察处理函数实际执行次数。
    #[test]
    fn signal_action_mask_and_selected_consumption(
        context: &mut TestContext<'_>,
        fixture: &mut UserOwner,
    ) -> TestResult {
        fixture.prepare(context)?;
        let event = fixture.run_named(context, b"/test/bin/user-signal", b"a", false)?;
        context.expect_eq(event.status, 0);
        let reset = fixture.run_named(context, b"/test/bin/user-signal", b"x", false)?;
        context.expect_eq(reset.status, 10);
        Ok(())
    }

    /// clone已接受的零退出信号不误发SIGCHLD，普通SIGCHLD分支仍真实递送。
    #[test]
    #[ktest(min_cpus = 2)]
    fn signal_clone_exit_notification_semantics(
        context: &mut TestContext<'_>,
        fixture: &mut UserOwner,
    ) -> TestResult {
        fixture.prepare(context)?;
        fixture.signal_clone_exit_notification(context)
    }

    /// 无handler停续后参数EFAULT恢复原mask，后续普通handler不消费残留saved_mask。
    #[test]
    #[ktest(min_cpus = 2)]
    fn signal_sigsuspend_restart_restores_mask(
        context: &mut TestContext<'_>,
        fixture: &mut UserOwner,
    ) -> TestResult {
        fixture.prepare(context)?;
        fixture.signal_suspend_restart_mask(context)
    }

    /// 两架构逐个FP寄存器、FCSR/FCC和用户修改PC/SP由目标ISA验证。
    #[test]
    fn signal_frame_registers_and_edited_context(
        context: &mut TestContext<'_>,
        fixture: &mut UserOwner,
    ) -> TestResult {
        fixture.prepare(context)?;
        let event = fixture.run_named(context, b"/test/bin/user-signal", b"f", false)?;
        context.expect_eq(event.status, 0);
        Ok(())
    }

    /// 同步缺页、权限、整页EOF和非法指令核siginfo与发生时PC，用户改帧后实际续行。
    #[test]
    fn signal_synchronous_fault_info_and_resume(
        context: &mut TestContext<'_>,
        fixture: &mut UserOwner,
    ) -> TestResult {
        fixture.prepare(context)?;
        let event = fixture.run_named(context, b"/test/bin/user-signal", b"z", false)?;
        context.expect_eq(event.status, 0);
        Ok(())
    }

    /// 子进程嵌套altstack后真实父wait，并逐字核父私有页未被内核frame写入污染。
    #[test]
    fn signal_nested_altstack_and_frame_cow(
        context: &mut TestContext<'_>,
        fixture: &mut UserOwner,
    ) -> TestResult {
        fixture.prepare(context)?;
        let event = fixture.run_named(context, b"/test/bin/user-signal", b"n", false)?;
        context.expect_eq(event.status, 0);
        Ok(())
    }

    /// 读取坏sigreturn和写入无效栈均有限SIGSEGV，不把用户异常当框架panic。
    #[test]
    fn signal_invalid_frames_terminate(
        context: &mut TestContext<'_>,
        fixture: &mut UserOwner,
    ) -> TestResult {
        fixture.prepare(context)?;
        let returned = fixture.run_named(context, b"/test/bin/user-signal", b"b", false)?;
        context.expect_eq(returned.status, 11);
        let written = fixture.run_named(context, b"/test/bin/user-signal", b"w", false)?;
        context.expect_eq(written.status, 11);
        let privileged = fixture.run_named(context, b"/test/bin/user-signal", b"k", false)?;
        context.expect_eq(privileged.status, 11);
        Ok(())
    }

    /// REAL中断相对/绝对sleep，waitid分别核SA_RESTART与EINTR，futex信号沿下一批。
    #[test]
    fn signal_sleep_and_wait_restart_policy(
        context: &mut TestContext<'_>,
        fixture: &mut UserOwner,
    ) -> TestResult {
        fixture.prepare(context)?;
        let event = fixture.run_named(context, b"/test/bin/user-signal", b"s", false)?;
        context.expect_eq(event.status, 0);
        Ok(())
    }

    /// stop/continue后restart_syscall仍消费原期限，不把重启当作新相对请求。
    #[test]
    fn signal_sleep_restart_keeps_deadline(
        context: &mut TestContext<'_>,
        fixture: &mut UserOwner,
    ) -> TestResult {
        fixture.prepare(context)?;
        let event = fixture.run_named(context, b"/test/bin/user-signal", b"d", false)?;
        context.expect_eq(event.status, 0);
        Ok(())
    }

    /// waitid的stopped/continued来自真实双成员组，父与独立共享VM组不混合。
    #[test]
    fn signal_group_stop_continue_waitid(
        context: &mut TestContext<'_>,
        fixture: &mut UserOwner,
    ) -> TestResult {
        fixture.prepare(context)?;
        let event = fixture.run_named(context, b"/test/bin/user-signal", b"g", false)?;
        context.expect_eq(event.status, 0);
        Ok(())
    }

    /// 两CPU间发送SIGKILL之前，原CPU必须timer抢占无限用户计算并推进另一个内核Task。
    #[test]
    #[ktest(min_cpus = 2)]
    fn signal_timer_preempts_infinite_user_and_remote_kill(
        context: &mut TestContext<'_>,
        fixture: &mut UserOwner,
    ) -> TestResult {
        fixture.prepare(context)?;
        fixture.preempt_infinite(context)
    }

    /// 用户/内核CPU账、times的USER_HZ和CPU itimer不把offCPU睡眠算成执行。
    #[test]
    fn signal_cpu_time_and_interval_timers(
        context: &mut TestContext<'_>,
        fixture: &mut UserOwner,
    ) -> TestResult {
        fixture.prepare(context)?;
        let event = fixture.run_named(context, b"/test/bin/user-signal", b"c", false)?;
        context.expect_eq(event.status, 0);
        Ok(())
    }

    /// 不可中断原页加载先完成真实盘责任和用户read，再到返回点处理已经pending的信号。
    #[test]
    #[ktest(min_cpus = 2)]
    fn signal_wait_keeps_file_read_responsibility(
        context: &mut TestContext<'_>,
        fixture: &mut UserOwner,
    ) -> TestResult {
        fixture.prepare(context)?;
        fixture.signal_during_file_wait(context)
    }

    /// 不改用户字的真实唤醒、两种Mm隔离和共享匿名不同VA均由普通用户指令验证。
    #[test]
    #[ktest(min_cpus = 2)]
    fn futex_wait_wake_and_mapping_keys(
        context: &mut TestContext<'_>,
        fixture: &mut UserOwner,
    ) -> TestResult {
        fixture.prepare(context)?;
        for mode in [b"w" as &[u8], b"p", b"n", b"a"].iter().copied() {
            fixture.futex_program(context, mode)?;
        }
        Ok(())
    }

    /// 重排保原身份/期限，取消从当前位置消费一次，bitset配额只计被选择者。
    #[test]
    #[ktest(min_cpus = 2)]
    fn futex_bitset_requeue_signal_and_timeout(
        context: &mut TestContext<'_>,
        fixture: &mut UserOwner,
    ) -> TestResult {
        fixture.prepare(context)?;
        fixture.futex_program(context, b"r")
    }

    /// 用户以实际child_tid等待join；旧Mm清零唤醒与坏地址退出均完成正常回收。
    #[test]
    #[ktest(min_cpus = 2)]
    fn futex_clear_child_tid_and_bad_exit_address(
        context: &mut TestContext<'_>,
        fixture: &mut UserOwner,
    ) -> TestResult {
        fixture.prepare(context)?;
        fixture.futex_program(context, b"t")
    }

    /// SA_RESTART恢复原用户请求，停续后的相对等待保首次期限且不重读已解除参数页。
    #[test]
    #[ktest(min_cpus = 2)]
    fn futex_signal_restart_parameters_and_deadline(
        context: &mut TestContext<'_>,
        fixture: &mut UserOwner,
    ) -> TestResult {
        fixture.prepare(context)?;
        for mode in [b"s" as &[u8], b"d"].iter().copied() {
            fixture.futex_program(context, mode)?;
        }
        Ok(())
    }

    /// 非leader沿原Task转移Linux身份；两个实际exec只允许唯一新映像或真实组fatal。
    #[test]
    #[ktest(min_cpus = 2)]
    fn exec_nonleader_and_concurrent_control(
        context: &mut TestContext<'_>,
        fixture: &mut UserOwner,
    ) -> TestResult {
        fixture.prepare(context)?;
        fixture.lifecycle_exec(context)
    }

    /// 外部共享files/sighand不被exec修改，旧根保活，新映像与CLOEXEC由普通程序验证。
    #[test]
    #[ktest(min_cpus = 2)]
    fn exec_shared_resources_and_mm_handoff(
        context: &mut TestContext<'_>,
        fixture: &mut UserOwner,
    ) -> TestResult {
        fixture.prepare(context)?;
        fixture.lifecycle_resources(context)
    }

    /// vfork按旧Mm使用结束恢复父；父被取消后子仍独立退出并完成转亲回收。
    #[test]
    #[ktest(min_cpus = 2)]
    fn vfork_exec_exit_and_parent_cancellation(
        context: &mut TestContext<'_>,
        fixture: &mut UserOwner,
    ) -> TestResult {
        fixture.prepare(context)?;
        fixture.lifecycle_vfork(context)
    }

    /// leader先退出保同组父资格；组退出、转亲和角色释放后才复用号码。
    #[test]
    #[ktest(min_cpus = 2)]
    fn exit_group_parentage_and_pid_role_reuse(
        context: &mut TestContext<'_>,
        fixture: &mut UserOwner,
    ) -> TestResult {
        fixture.prepare(context)?;
        fixture.lifecycle_roles(context)
    }

    /// 两个真实阻塞wait者可重复观察，但只能消费一次；坏copyout不恢复僵尸。
    #[test]
    #[ktest(min_cpus = 2)]
    fn wait_observation_consumption_and_copyout(
        context: &mut TestContext<'_>,
        fixture: &mut UserOwner,
    ) -> TestResult {
        fixture.prepare(context)?;
        fixture.lifecycle_wait(context)
    }
}
