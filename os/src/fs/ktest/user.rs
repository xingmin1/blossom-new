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
    target: usize,
    coordinator: usize,
    join: JoinHandle,
}

struct UserOwner {
    path: PathOwner,
    graph: Option<Arc<TaskGraph>>,
    running: Option<RunningProgram>,
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

    /// 完整资源先准备、身份先发表，然后在原调度器指定的真实远核运行。
    fn start(&mut self, context: &TestContext<'_>, image: LoadedImage) -> TestResult {
        assert!(
            self.running.is_none(),
            "user fixture already has a running child"
        );
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
                .map_err(|error| {
                    context.diagnostic(format_args!("user-program-prepare error={:?}", error));
                    TestError("user program task preparation failed")
                })?;
        let pid = prepared.pid();
        let join = prepared.publish().map_err(|error| {
            context.diagnostic(format_args!("user-program-publish error={:?}", error));
            TestError("user identity publication failed")
        })?;
        self.running = Some(RunningProgram {
            pid,
            target,
            coordinator,
            join,
        });
        Ok(())
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
        context.expect_eq(event.cpu, target);
        context.expect_eq(join.join(), event.status);
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
        })
    }

    /// 有限真实负载结束并 join 后才释放图和根；无法收敛由已有 VM 超时终止，保留原拥有。
    fn quiesce(&mut self, context: &mut TestContext<'_>) -> TestResult {
        if self.running.is_some() {
            self.wait(context)?;
        }
        drop(self.graph.take());
        self.release_pressure();
        <PathOwner as Fixture<DeviceInventory>>::quiesce(&mut self.path, context)
    }

    /// 用户与设备都完成真实清理后沿原夹具比较本例完整六域。
    fn tear_down(self, context: &mut TestContext<'_>) -> TestResult {
        assert!(
            self.running.is_none() && self.graph.is_none(),
            "user fixture is not quiesced"
        );
        <PathOwner as Fixture<DeviceInventory>>::tear_down(self.path, context)
    }
}

/// 父入口只保留原生 suite 宏的真实锚点。
pub(crate) fn anchor() -> &'static ::ktest::SuiteSlot {
    programs::__ktest_anchor()
}

#[ktest::suite(fixture = UserOwner, suite_resources = DeviceInventory, context = "Thread", tags("user-elf", "fs-path", "fs-oom"), resources("root"), timeout_ms = 90000)]
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
}
