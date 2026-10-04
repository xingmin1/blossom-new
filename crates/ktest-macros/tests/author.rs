use std::cell::RefCell;
use std::fmt;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::rc::Rc;

use ktest::{
    CaseDesc, CaseOutcome, CaseRunner, Caught, Context, Failure, Fixture, Isolation, JoinOutcome,
    PanicMessage, Phase, Resource, Services, Status, TestContext, TestError, TestResult,
};

#[ktest_macros::suite(context = "Boot", fixture = Device, suite_resources = Base, tags("block"), min_cpus = 2, timeout_ms = 500, resources("scratch"))]
mod typed {
    use super::*;

    pub struct Base(Rc<RefCell<Vec<u32>>>);
    impl Fixture<()> for Base {
        /// 每次执行组重建只读基础 owner，不依赖其它实例修改状态。
        fn set_up(_context: &mut TestContext<'_>, _: &()) -> Result<Self, TestError> {
            Ok(Self(Rc::new(RefCell::new(Vec::new()))))
        }
        /// 所有实例都已完成后检查各 fixture 的独立真实释放。
        fn tear_down(self, _context: &mut TestContext<'_>) -> TestResult {
            assert_eq!(*self.0.borrow(), [1, 2]);
            Ok(())
        }
    }
    pub struct Device {
        owner: Rc<RefCell<Vec<u32>>>,
        value: u32,
    }
    impl Fixture<Base> for Device {
        /// 实例只克隆既有 owner 并创建独立字段。
        fn set_up(_context: &mut TestContext<'_>, base: &Base) -> Result<Self, TestError> {
            Ok(Self {
                owner: base.0.clone(),
                value: 0,
            })
        }
        /// 记录本例结果；主体失败后的静止化同样保留具体对象。
        fn quiesce(&mut self, _context: &mut TestContext<'_>) -> TestResult {
            self.owner.borrow_mut().push(self.value);
            Ok(())
        }
    }

    /// 静态参数各形成一个可独立选择的实例，fixture 字段不能跨例泄漏。
    #[test]
    #[case("one", 1)]
    #[case("two", 2)]
    #[ktest(tags("smp"), timeout_ms = 70, resources("uart"))]
    fn parameter(context: &mut TestContext<'_>, device: &mut Device, value: u32) -> TestResult {
        assert_eq!(device.value, 0);
        assert_eq!(context.seed, 100 + value as u64);
        device.value = value;
        Ok(())
    }
}

#[ktest_macros::suite]
mod simple {
    /// 无参数作者函数保持普通 Rust assert 语义。
    #[test]
    fn plain() {
        assert_eq!(2 + 2, 4);
    }

    /// 静态参数可以单独使用，不要求上下文或 fixture。
    #[test]
    #[case("zero", 0)]
    fn static_parameter(value: usize) {
        assert_eq!(value, 0);
    }

    /// 上下文允许局部捕获，也支持显式单元返回。
    #[test]
    fn contextual(context: &mut ktest::TestContext<'_>) -> () {
        context.expect_panic("local", || panic!("local"));
    }

    /// 普通 Rust 主体预期无需作者手工改写断言。
    #[test]
    #[should_panic(expected = "controlled")]
    fn expected() {
        panic!("controlled");
    }

    /// ignored 与 fatal 静态条件在未执行时仍可完整发现。
    #[test]
    #[ignore = "requires destructive VM"]
    #[ktest(expected_fatal = "panic", fatal_message = "fatal diagnostic")]
    fn fatal_metadata() {
        panic!("fatal diagnostic");
    }
}

struct Runtime {
    diagnostics: RefCell<Vec<String>>,
}
impl Services for Runtime {
    /// 宿主使用真实 catch_unwind 验证 wrapper；目标架构展开由父另证。
    fn catch(&self, callback: &mut dyn FnMut() -> TestResult) -> Caught {
        match catch_unwind(AssertUnwindSafe(callback)) {
            Ok(result) => Caught::Returned(result),
            Err(payload) => {
                let message = payload
                    .downcast_ref::<&str>()
                    .copied()
                    .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
                    .unwrap_or("non-string panic");
                Caught::Panicked(PanicMessage::new(message, false))
            }
        }
    }
    /// 记录同步作者诊断。
    fn diagnostic(&self, message: fmt::Arguments<'_>) {
        self.diagnostics.borrow_mut().push(message.to_string());
    }
    /// 此回归的运行阶段不驱动宿主超时。
    fn phase(&self, _phase: Phase) {}
    /// 这些 fixture 没有异步子任务。
    fn stop_children(&self) {}
    /// 没有子任务的作用域已经收敛。
    fn join_children(&self) -> Result<JoinOutcome, TestError> {
        Ok(JoinOutcome { failure: None })
    }
    /// 清理失败必须使回归失败，不能转成 PASS。
    fn terminate(&self, outcome: &CaseOutcome) -> ! {
        panic!("unexpected termination: {:?}", outcome)
    }
}
struct Runner<'a> {
    runtime: &'a Runtime,
    selected: &'a [usize],
    outcomes: Vec<CaseOutcome>,
}
impl CaseRunner for Runner<'_> {
    /// 在同一受控栈同步执行索引，每实例重新建立期望账本和 seed。
    fn run(
        &mut self,
        cases: &'static [CaseDesc],
        dispatch: &mut dyn FnMut(&mut TestContext<'_>, usize) -> CaseOutcome,
    ) -> TestResult {
        for &index in self.selected {
            let mut context = TestContext::new(self.runtime, &cases[index], 101 + index as u64, 0);
            self.outcomes.push(dispatch(&mut context, index));
        }
        Ok(())
    }
}

/// 宏作者路径真实构造和销毁 typed base/fixture，参数实例完全独立。
#[test]
fn typed_parameter_instances_have_fresh_fixture_and_effective_metadata() {
    let cases = typed::__KTEST_SUITE.cases;
    assert_eq!(
        cases.iter().map(|case| case.parameter).collect::<Vec<_>>(),
        [Some("one"), Some("two")]
    );
    for case in cases {
        assert_eq!(case.attributes.context, Context::Boot);
        assert_eq!(case.attributes.tags, ["block", "smp"]);
        assert_eq!(
            case.attributes.resources,
            [Resource::Scratch, Resource::Uart]
        );
        assert_eq!(case.attributes.min_cpus, 2);
        assert_eq!(case.attributes.timeout_ms, 70);
        assert!(case
            .name
            .starts_with(concat!(module_path!(), "::typed::parameter[")));
        assert!(case.source.ends_with("author.rs"));
    }
    let runtime = Runtime {
        diagnostics: RefCell::new(Vec::new()),
    };
    let mut suite_context = TestContext::new(&runtime, &cases[0], 0, 0);
    let mut runner = Runner {
        runtime: &runtime,
        selected: &[0, 1],
        outcomes: Vec::new(),
    };
    let result = (typed::__KTEST_SUITE.run)(&mut suite_context, &mut runner);
    assert_eq!(result.setup, None);
    assert_eq!(result.execution, None);
    assert_eq!(result.cleanup, None);
    assert_eq!(
        runner
            .outcomes
            .iter()
            .map(|value| value.status)
            .collect::<Vec<_>>(),
        [Status::Pass, Status::Pass]
    );
}

/// 正常无参数、context 与静态参数路径共用同一生命周期入口。
#[test]
fn simple_author_signatures_and_expected_panic_execute() {
    let cases = simple::__KTEST_SUITE.cases;
    let runtime = Runtime {
        diagnostics: RefCell::new(Vec::new()),
    };
    let mut context = TestContext::new(&runtime, &cases[0], 0, 0);
    let mut runner = Runner {
        runtime: &runtime,
        selected: &[0, 1, 2, 3],
        outcomes: Vec::new(),
    };
    let result = (simple::__KTEST_SUITE.run)(&mut context, &mut runner);
    assert_eq!(result.execution, None);
    assert_eq!(
        runner
            .outcomes
            .iter()
            .map(|value| value.status)
            .collect::<Vec<_>>(),
        [Status::Pass; 4]
    );
    let fatal = &cases[4];
    assert_eq!(fatal.attributes.isolation, Isolation::Case);
    assert_eq!(fatal.expected_fatal, Some("panic"));
    assert_eq!(fatal.fatal_message, Some("fatal diagnostic"));
    assert_eq!(fatal.ignore, Some("requires destructive VM"));
    assert_eq!(cases[0].attributes.context, Context::Thread);
    assert!(cases[3].should_panic.is_some());
    assert!(!runner
        .outcomes
        .iter()
        .any(|result| matches!(result.primary, Some(Failure::Panic(_)))));
}
