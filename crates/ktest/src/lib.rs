#![no_std]

use core::fmt::{self, Write};

#[cfg(feature = "macros")]
pub use ktest_macros::suite;

/// 测试作者显式返回的错误；静态消息不依赖被测分配器。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TestError(pub &'static str);
pub type TestResult = Result<(), TestError>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Context {
    Boot,
    Thread,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Isolation {
    Suite,
    Case,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resource {
    Uart,
    Scratch,
    Root,
    Extra,
    ApFreeze,
    VirtioAccessPlatform,
    NetUser,
}
impl Resource {
    /// 返回宿主资源合同中的固定名字。
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Uart => "uart",
            Self::Scratch => "scratch",
            Self::Root => "root",
            Self::Extra => "extra",
            Self::ApFreeze => "ap-freeze",
            Self::VirtioAccessPlatform => "virtio-access-platform",
            Self::NetUser => "net-user",
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Defaults {
    pub context: Context,
    pub isolation: Isolation,
    pub tags: &'static [&'static str],
    pub min_cpus: u32,
    pub timeout_ms: u64,
    pub resources: &'static [Resource],
}
#[derive(Clone, Copy, Debug)]
pub struct CaseDesc {
    pub name: &'static str,
    pub parameter: Option<&'static str>,
    pub source: &'static str,
    pub line: u32,
    pub attributes: Defaults,
    pub ignore: Option<&'static str>,
    pub should_panic: Option<&'static str>,
    pub expected_fatal: Option<&'static str>,
    pub fatal_message: Option<&'static str>,
}
pub struct SuiteDesc {
    pub name: &'static str,
    pub defaults: Defaults,
    pub cases: &'static [CaseDesc],
    pub run: fn(&mut TestContext<'_>, &mut dyn CaseRunner) -> SuiteOutcome,
}

/// 链接脚本按指针步长遍历此只读槽；对象文件抽取另由导出锚点负责。
#[repr(C)]
pub struct SuiteSlot(pub &'static SuiteDesc);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    Setup,
    Body,
    Quiesce,
    Join,
    Cleanup,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    Pass,
    Fail,
    Skip,
    Error,
}

/// 有界 UTF-8 panic 消息；截断仍保留显式标志，不使用堆分配。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PanicMessage {
    bytes: [u8; 256],
    len: u16,
    pub truncated: bool,
    sealed: bool,
}
impl PanicMessage {
    /// 从真实捕获记录复制消息；超过容量时只保留完整 UTF-8 前缀。
    pub fn new(message: &str, truncated: bool) -> Self {
        let mut value = Self {
            bytes: [0; 256],
            len: 0,
            truncated,
            sealed: false,
        };
        let _ = value.write_str(message);
        value
    }
    /// 借用消息有效部分。
    pub fn as_str(&self) -> &str {
        // SAFETY: write_str 仅复制完整 UTF-8 前缀，len 不超过固定数组。
        unsafe { core::str::from_utf8_unchecked(&self.bytes[..self.len as usize]) }
    }
}
impl Write for PanicMessage {
    /// 追加完整 UTF-8 前缀，并记录任何容量截断。
    fn write_str(&mut self, value: &str) -> fmt::Result {
        if self.sealed {
            return Ok(());
        }
        let available = self.bytes.len() - self.len as usize;
        let mut count = value.len().min(available);
        while !value.is_char_boundary(count) {
            count -= 1;
        }
        let start = self.len as usize;
        self.bytes[start..start + count].copy_from_slice(&value.as_bytes()[..count]);
        self.len += count as u16;
        self.sealed = count < value.len();
        self.truncated |= self.sealed;
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Caught {
    Returned(TestResult),
    Panicked(PanicMessage),
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    Returned(TestError),
    Panic(PanicMessage),
    ExpectedPanicMissing,
    ExpectedPanicMismatch(PanicMessage),
    Expectation,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CaseOutcome {
    pub status: Status,
    pub primary: Option<Failure>,
    pub child: Option<Failure>,
    pub cleanup: Option<Failure>,
    pub expectations: u32,
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SuiteOutcome {
    pub setup: Option<Failure>,
    pub execution: Option<Failure>,
    pub child: Option<Failure>,
    pub cleanup: Option<Failure>,
    pub expectations: u32,
}
/// 成功返回表示所有子任务已离栈；错误账本不等同于未能收敛。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JoinOutcome {
    pub failure: Option<Failure>,
}

/// 运行器的小服务桥；捕获必须使用当前真实 Task/Boot 展开边界。
/// callback 必须同步恰调用一次，禁止跨 C、trap、切栈或未知 worker 展开。
/// stop/join 处理本例全部登记子任务，join 成功必须已离开实际任务栈。
pub trait Services {
    /// 在当前受控 Rust 栈中执行一次回调并捕获真实 panic。
    fn catch(&self, callback: &mut dyn FnMut() -> TestResult) -> Caught;
    /// 同步向唯一可靠诊断通道写入消息。
    fn diagnostic(&self, message: fmt::Arguments<'_>);
    /// 在开始相应操作前公布真实生命周期阶段。
    fn phase(&self, phase: Phase);
    /// 请求当前作用域的子任务合作停止，不取消或释放活栈。
    fn stop_children(&self);
    /// 等待全部登记任务离栈；成功保留子失败，Err 表示无法安全完成收敛。
    fn join_children(&self) -> Result<JoinOutcome, TestError>;
    /// 保存原失败和清理错误后结束 VM，不再析构可能仍有异步访问的资源。
    fn terminate(&self, outcome: &CaseOutcome) -> !;
}

/// 用例上下文只含身份、seed 和窄运行服务，不保存生产子系统对象。
pub struct TestContext<'a> {
    services: &'a dyn Services,
    pub case: &'static CaseDesc,
    pub seed: u64,
    pub repeat: u32,
    expectations: u32,
}
impl<'a> TestContext<'a> {
    /// 建立本例独立的报告状态，服务生命周期覆盖整个实例。
    pub fn new(
        services: &'a dyn Services,
        case: &'static CaseDesc,
        seed: u64,
        repeat: u32,
    ) -> Self {
        Self {
            services,
            case,
            seed,
            repeat,
            expectations: 0,
        }
    }
    /// 同步输出作者诊断；不经过可丢弃日志队列。
    pub fn diagnostic(&self, message: fmt::Arguments<'_>) {
        self.services.diagnostic(message);
    }
    /// 记录独立检查失败并继续主体。
    pub fn expect(
        &mut self,
        condition: bool,
        source: &str,
        line: u32,
        message: fmt::Arguments<'_>,
    ) {
        if !condition {
            self.expectations = self.expectations.saturating_add(1);
            self.diagnostic(format_args!(
                "{}:{}: expectation failed: {}",
                source, line, message
            ));
        }
    }
    /// 比较值并在不相等时记录双方 Debug 内容。
    #[track_caller]
    pub fn expect_eq<T: PartialEq + fmt::Debug>(&mut self, actual: T, expected: T) {
        let location = core::panic::Location::caller();
        self.expect(
            actual == expected,
            location.file(),
            location.line(),
            format_args!("actual={:?}, expected={:?}", actual, expected),
        );
    }
    /// 在同一真实窄捕获接口内检查局部 panic；仅完整消息可匹配，不回滚对象状态。
    #[track_caller]
    pub fn expect_panic<T>(&mut self, expected: &str, body: impl FnOnce() -> T) {
        let services = self.services;
        let result = capture(services, || {
            body();
            Ok(())
        });
        let matched = matches!(&result, Err(Failure::Panic(message)) if !message.truncated && message.as_str().contains(expected));
        let location = core::panic::Location::caller();
        self.expect(
            matched,
            location.file(),
            location.line(),
            format_args!("expected panic containing {:?}, got {:?}", expected, result),
        );
    }
}

/// 具体资源类型在主体 catch 外保活；成功构造前的局部释放归 set_up 自己负责。
/// quiesce 的责任是一次真实停止异步生产，tear_down 只在全部子任务离栈后消费对象。
pub trait Fixture<R = ()>: Sized {
    /// 用真实生产 API 构造完整对象；错误不能返回半初始化 fixture。
    fn set_up(context: &mut TestContext<'_>, resources: &R) -> Result<Self, TestError>;
    /// 无异步责任的对象无需额外静止化；有责任的实现必须真实停止并等待自己的生产源。
    fn quiesce(&mut self, _context: &mut TestContext<'_>) -> TestResult {
        Ok(())
    }
    /// 在所有使用者退出后消费对象；额外销毁责任可显式实现。
    fn tear_down(self, _context: &mut TestContext<'_>) -> TestResult {
        drop(self);
        Ok(())
    }
}
impl<R> Fixture<R> for () {
    /// 无 fixture 的实例不取得任何资源。
    fn set_up(_context: &mut TestContext<'_>, _resources: &R) -> Result<Self, TestError> {
        Ok(())
    }
}

/// 父运行器在 suite 的真实 Boot/Thread 栈中逐例同步分派并建立独立上下文。
/// 禁止将 borrowed dispatch 或 suite 资源伪装成 static 发送新 Task。
/// 返回前必须完成本次选择全部实例或报告运行错误，不能持有 dispatch/资源借用。
pub trait CaseRunner {
    /// 运行选择中的索引，使用与目录完全相同的静态描述符。
    fn run(
        &mut self,
        cases: &'static [CaseDesc],
        dispatch: &mut dyn FnMut(&mut TestContext<'_>, usize) -> CaseOutcome,
    ) -> TestResult;
}

/// 将作者允许的两种返回类型转成同一错误通路。
pub trait IntoTestResult {
    /// 将有限作者返回类型转换为显式结果。
    fn into_test_result(self) -> TestResult;
}
impl IntoTestResult for () {
    /// 单元返回表示没有显式错误，expect 账本仍独立生效。
    fn into_test_result(self) -> TestResult {
        Ok(())
    }
}
impl IntoTestResult for TestResult {
    /// 保留作者的显式错误。
    fn into_test_result(self) -> TestResult {
        self
    }
}

/// 用 Option 保存具体返回值，真实 catch 外的 slot 不承受主体展开。
fn capture<T>(
    services: &dyn Services,
    body: impl FnOnce() -> Result<T, TestError>,
) -> Result<T, Failure> {
    let mut body = Some(body);
    let mut value = None;
    let result = services.catch(&mut || {
        value = Some(body.take().expect("catch callback invoked twice")()?);
        Ok(())
    });
    match result {
        Caught::Returned(Ok(())) => Ok(value.expect("catch callback was not invoked")),
        Caught::Returned(Err(error)) => Err(Failure::Returned(error)),
        Caught::Panicked(message) => Err(Failure::Panic(message)),
    }
}

/// 清理失败终止 VM；不得 Drop 仍可能被后台使用的 fixture。
fn cleanup_failure(
    context: &TestContext<'_>,
    primary: Option<Failure>,
    child: Option<Failure>,
    error: Failure,
) -> ! {
    let outcome = CaseOutcome {
        status: Status::Error,
        primary,
        child,
        cleanup: Some(error),
        expectations: context.expectations,
    };
    context.diagnostic(format_args!(
        "cleanup failed: {:?}; primary: {:?}; child: {:?}",
        error, primary, child
    ));
    context.services.terminate(&outcome)
}

/// 执行一个具体实例，setup/body/参数构造/静止化/join/销毁各有真实捕获边界。
/// 主体预期仅接受未截断消息；setup、子任务及清理错误保持独立。
/// 声明 expected_fatal 的主体正常返回即失败，但仍完成真实静止化、join 和销毁。
pub fn run_case<F: Fixture<R>, R>(
    context: &mut TestContext<'_>,
    resources: &R,
    body: impl FnOnce(&mut TestContext<'_>, &mut F) -> TestResult,
) -> CaseOutcome {
    let services = context.services;
    services.phase(Phase::Setup);
    let fixture = capture(services, || F::set_up(context, resources));
    let mut primary = None;
    let mut setup_failed = false;
    let mut fixture = match fixture {
        Ok(value) => Some(value),
        Err(error) => {
            primary = Some(error);
            setup_failed = true;
            None
        }
    };
    if let Some(value) = fixture.as_mut() {
        services.phase(Phase::Body);
        let result = capture(services, || body(context, value));
        primary = match (context.case.should_panic, result) {
            (Some(expected), Err(Failure::Panic(message)))
                if !message.truncated && message.as_str().contains(expected) =>
            {
                None
            }
            (Some(_), Err(Failure::Panic(message))) => {
                Some(Failure::ExpectedPanicMismatch(message))
            }
            (Some(_), Ok(())) => Some(Failure::ExpectedPanicMissing),
            (_, Err(error)) => Some(error),
            (None, Ok(())) if context.case.expected_fatal.is_some() => {
                Some(Failure::Returned(TestError("expected fatal did not occur")))
            }
            (None, Ok(())) => None,
        };
    }
    services.stop_children();
    if let Some(value) = fixture.as_mut() {
        services.phase(Phase::Quiesce);
        if let Err(error) = capture(services, || value.quiesce(context)) {
            cleanup_failure(context, primary, None, error);
        }
    }
    services.phase(Phase::Join);
    let child = match capture(services, || services.join_children()) {
        Ok(joined) => joined.failure,
        Err(error) => cleanup_failure(context, primary, None, error),
    };
    if let Some(value) = fixture {
        services.phase(Phase::Cleanup);
        if let Err(error) = capture(services, || value.tear_down(context)) {
            cleanup_failure(context, primary, child, error);
        }
    }
    if primary.is_none() && context.expectations != 0 {
        primary = Some(Failure::Expectation);
    }
    CaseOutcome {
        status: if setup_failed {
            Status::Error
        } else if primary.is_some() || child.is_some() {
            Status::Fail
        } else {
            Status::Pass
        },
        primary,
        child,
        cleanup: None,
        expectations: context.expectations,
    }
}

/// suite 基础资源按具体类型包住整个选择；case 只获得只读基础资源借用。
pub fn run_suite<R: Fixture<()>>(
    context: &mut TestContext<'_>,
    runner: &mut dyn CaseRunner,
    cases: &'static [CaseDesc],
    dispatch: fn(&mut TestContext<'_>, &R, usize) -> CaseOutcome,
) -> SuiteOutcome {
    let services = context.services;
    services.phase(Phase::Setup);
    let mut resources = match capture(services, || R::set_up(context, &())) {
        Ok(resources) => resources,
        Err(error) => {
            services.stop_children();
            services.phase(Phase::Join);
            let child = match capture(services, || services.join_children()) {
                Ok(joined) => joined.failure,
                Err(cleanup) => cleanup_failure(context, Some(error), None, cleanup),
            };
            return SuiteOutcome {
                setup: Some(error),
                execution: None,
                child,
                cleanup: None,
                expectations: context.expectations,
            };
        }
    };
    let execution = capture(services, || {
        runner.run(cases, &mut |case_context, index| {
            dispatch(case_context, &resources, index)
        })
    });
    let primary = execution.err();
    services.stop_children();
    services.phase(Phase::Quiesce);
    if let Err(error) = capture(services, || resources.quiesce(context)) {
        cleanup_failure(context, primary, None, error);
    }
    services.phase(Phase::Join);
    let child = match capture(services, || services.join_children()) {
        Ok(joined) => joined.failure,
        Err(error) => cleanup_failure(context, primary, None, error),
    };
    services.phase(Phase::Cleanup);
    if let Err(error) = capture(services, || resources.tear_down(context)) {
        cleanup_failure(context, primary, child, error);
    }
    let primary = primary.or(if context.expectations != 0 {
        Some(Failure::Expectation)
    } else {
        None
    });
    SuiteOutcome {
        setup: None,
        execution: primary,
        child,
        cleanup: None,
        expectations: context.expectations,
    }
}

/// 记录期望失败并保留原始文件行号；表达式只执行一次。
#[macro_export]
macro_rules! expect {
    ($context:expr, $condition:expr $(,)?) => { $context.expect($condition, file!(), line!(), core::format_args!("{}", core::stringify!($condition))) };
    ($context:expr, $condition:expr, $($message:tt)+) => { $context.expect($condition, file!(), line!(), core::format_args!($($message)+)) };
}
/// 引用比较避免移动作者的值，双方表达式各求值一次。
#[macro_export]
macro_rules! expect_eq {
    ($context:expr, $actual:expr, $expected:expr $(,)?) => {{
        match (&$actual, &$expected) {
            (actual, expected) => $context.expect(
                actual == expected,
                file!(),
                line!(),
                core::format_args!("actual={:?}, expected={:?}", actual, expected),
            ),
        }
    }};
}
