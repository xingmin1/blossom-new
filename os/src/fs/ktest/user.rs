//! 普通磁盘 ELF 复用原设备/路径夹具；用户 wait 与真实离栈 join 分别完成后才释放根。

use crate::fs::ktest::path::PathOwner;
use crate::fs::ktest::*;
use crate::task::exec::{self, Arguments, ExecError, LoadedImage};
use crate::task::process::{TaskGraph, WaitEvent};
use crate::task::{Credentials, Placement, UserRuntime};

mod layout;
mod preparation;

const PROGRAM: &[u8] = b"/test/bin/user-smoke";

struct RunningProgram {
    pid: usize,
    target: Option<usize>,
    coordinator: usize,
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
            join,
        });
        Ok(())
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
        let arguments =
            Arguments::from_kernel(program, &[program, mode], &[]).map_err(|error| {
                context.diagnostic(format_args!("user-named-arguments error={:?}", error));
                TestError("named user arguments failed")
            })?;
        let file = self.file(context, program)?;
        let image = exec::load(file, &arguments, 0, 0).map_err(|error| {
            context.diagnostic(format_args!("user-named-load error={:?}", error));
            TestError("named user image failed")
        })?;
        let affine_group = !matches!(mode.first(), Some(b't' | b'T'));
        self.start_program(context, image, affine_group, reserve_parent_fd, credentials)?;
        let event = self.wait(context)?;
        self.reap_children(context)?;
        self.release_reserved(context);
        context.diagnostic(format_args!(
            "user-named-result program={:?} mode={:?} status={}",
            program, mode, event.status
        ));
        Ok(event)
    }

    /// 初始组退出时转亲的其他子组逐个消费，图内部继续等待它们的全部成员离栈。
    fn reap_children(&self, context: &mut TestContext<'_>) -> TestResult {
        loop {
            match self.graph().wait_root() {
                Ok(event) => {
                    context.expect_eq(event.status, 0);
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
            join,
        } = self.running.take().unwrap();
        context.expect_eq(event.pid, pid);
        context.expect_eq(event.task_id, join.task_id());
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
        })
    }

    /// 有限真实负载结束并 join 后才释放图和根；无法收敛由已有 VM 超时终止，保留原拥有。
    fn quiesce(&mut self, context: &mut TestContext<'_>) -> TestResult {
        self.release_reserved(context);
        if self.running.is_some() {
            self.wait(context)?;
        }
        if self.graph.is_some() {
            self.reap_children(context)?;
        }
        drop(self.graph.take());
        self.release_pressure();
        <PathOwner as Fixture<DeviceInventory>>::quiesce(&mut self.path, context)
    }

    /// 用户与设备都完成真实清理后沿原夹具比较本例完整六域。
    fn tear_down(self, context: &mut TestContext<'_>) -> TestResult {
        assert!(
            self.running.is_none() && self.graph.is_none() && self.reserved.is_none(),
            "user fixture is not quiesced"
        );
        <PathOwner as Fixture<DeviceInventory>>::tear_down(self.path, context)
    }
}

/// 父入口只保留原生 suite 宏的真实锚点。
pub(crate) fn anchor() -> &'static ::ktest::SuiteSlot {
    programs::__ktest_anchor()
}

#[ktest::suite(fixture = UserOwner, suite_resources = DeviceInventory, context = "Thread", tags("user-elf", "user-abi", "fs-path", "fs-oom"), resources("root"), timeout_ms = 90000)]
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
}
