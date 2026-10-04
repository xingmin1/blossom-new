use std::cell::RefCell;
use std::fmt;
use std::panic::{catch_unwind, panic_any, AssertUnwindSafe};
use std::sync::atomic::{AtomicUsize, Ordering};

use ktest::{
    CaseDesc, CaseOutcome, Caught, Context, Defaults, Failure, Fixture, Isolation, JoinOutcome,
    PanicMessage, Phase, Services, Status, TestContext, TestError, TestResult,
};

static CASE: CaseDesc = CaseDesc {
    name: "host::lifecycle",
    parameter: None,
    source: file!(),
    line: line!(),
    attributes: Defaults {
        context: Context::Boot,
        isolation: Isolation::Suite,
        tags: &[],
        min_cpus: 1,
        timeout_ms: 1000,
        resources: &[],
    },
    ignore: None,
    should_panic: None,
    expected_fatal: None,
    fatal_message: None,
};
static EXPECTED: CaseDesc = CaseDesc {
    name: "host::expected",
    should_panic: Some("controlled"),
    ..CASE
};
static EXPECTED_FATAL: CaseDesc = CaseDesc {
    name: "host::expected_fatal",
    expected_fatal: Some("oom"),
    ..CASE
};

struct Runtime {
    phases: RefCell<Vec<Phase>>,
    messages: RefCell<Vec<String>>,
    child: Option<Failure>,
}
impl Runtime {
    /// 为每次回归创建独立宿主服务，避免依赖全局当前用例。
    fn new() -> Self {
        Self {
            phases: RefCell::new(Vec::new()),
            messages: RefCell::new(Vec::new()),
            child: None,
        }
    }
}
impl Services for Runtime {
    /// 宿主自然展开仅证明 API 行为，不作为双目标内核展开证据。
    fn catch(&self, callback: &mut dyn FnMut() -> TestResult) -> Caught {
        match catch_unwind(AssertUnwindSafe(callback)) {
            Ok(value) => Caught::Returned(value),
            Err(payload) => {
                // 真实展开的有界消息载荷保留捕获来源的截断标志。
                if let Some(message) = payload.downcast_ref::<PanicMessage>() {
                    return Caught::Panicked(*message);
                }
                let message = payload
                    .downcast_ref::<&str>()
                    .copied()
                    .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
                    .unwrap_or("non-string host panic");
                Caught::Panicked(PanicMessage::new(message, false))
            }
        }
    }
    /// 保存真实失败诊断用于回归检查。
    fn diagnostic(&self, message: fmt::Arguments<'_>) {
        self.messages.borrow_mut().push(message.to_string());
    }
    /// 保存各阶段的实际发生顺序。
    fn phase(&self, phase: Phase) {
        self.phases.borrow_mut().push(phase);
    }
    /// 此宿主回归没有创建后台任务。
    fn stop_children(&self) {}
    /// 返回已经完成的子结果以验证主体预期不能掩盖子错误。
    fn join_children(&self) -> Result<JoinOutcome, TestError> {
        Ok(JoinOutcome {
            failure: self.child,
        })
    }
    /// 清理失败回归在资源消费完成后将完整结果交给外层检查。
    fn terminate(&self, outcome: &CaseOutcome) -> ! {
        panic_any(*outcome)
    }
}

struct Counters {
    quiesces: AtomicUsize,
    drops: AtomicUsize,
}
// Fixture 无法借用只在 set_up 参数中的短生命周期；实际 fixture 拥有其计数 owner。
struct FixtureOwner(std::sync::Arc<Counters>);
impl Fixture<std::sync::Arc<Counters>> for FixtureOwner {
    /// 为实例克隆已存在的 owner，不构造半初始化对象。
    fn set_up(
        _context: &mut TestContext<'_>,
        counters: &std::sync::Arc<Counters>,
    ) -> Result<Self, TestError> {
        Ok(Self(counters.clone()))
    }
    /// 主体 panic 后仍恰执行一次实际 fixture 静止化责任。
    fn quiesce(&mut self, _context: &mut TestContext<'_>) -> TestResult {
        self.0.quiesces.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}
impl Drop for FixtureOwner {
    /// 记录真实 Rust 析构，不用手动补清理计数。
    fn drop(&mut self) {
        self.0.drops.fetch_add(1, Ordering::SeqCst);
    }
}

/// 预期 fatal 的主体正常返回也必须真实清理；原始 Err 和 expect 账本独立保留。
#[test]
fn expected_fatal_return_fails_after_real_fixture_cleanup() {
    for (result, expectation, primary) in [
        (
            Ok(()),
            false,
            Failure::Returned(TestError("expected fatal did not occur")),
        ),
        (
            Ok(()),
            true,
            Failure::Returned(TestError("expected fatal did not occur")),
        ),
        (
            Err(TestError("body failed before fatal")),
            true,
            Failure::Returned(TestError("body failed before fatal")),
        ),
    ] {
        let runtime = Runtime::new();
        let counters = std::sync::Arc::new(Counters {
            quiesces: AtomicUsize::new(0),
            drops: AtomicUsize::new(0),
        });
        let mut context = TestContext::new(&runtime, &EXPECTED_FATAL, 0, 0);
        let outcome =
            ktest::run_case::<FixtureOwner, _>(&mut context, &counters, |context, owner| {
                assert!(std::sync::Arc::ptr_eq(&owner.0, &counters));
                assert_eq!(counters.drops.load(Ordering::SeqCst), 0);
                if expectation {
                    ktest::expect!(context, false, "independent fatal-return expectation");
                }
                result
            });
        assert_eq!(counters.quiesces.load(Ordering::SeqCst), 1);
        assert_eq!(counters.drops.load(Ordering::SeqCst), 1);
        assert_eq!(std::sync::Arc::strong_count(&counters), 1);
        assert_eq!(outcome.status, Status::Fail);
        assert_eq!(outcome.primary, Some(primary));
        assert_eq!(outcome.child, None);
        assert_eq!(outcome.cleanup, None);
        assert_eq!(outcome.expectations, u32::from(expectation));
        assert_eq!(
            *runtime.phases.borrow(),
            [
                Phase::Setup,
                Phase::Body,
                Phase::Quiesce,
                Phase::Join,
                Phase::Cleanup,
            ]
        );
    }
}

/// 对象已真实消费后的清理错误，不能洗掉缺失 fatal 或更早主体错误及独立 expect。
#[test]
fn expected_fatal_return_preserves_primary_through_cleanup_error() {
    struct CleanupError(FixtureOwner);
    impl Fixture<std::sync::Arc<Counters>> for CleanupError {
        /// 持有真正的 RAII owner，避免以手动计数伪造资源清理。
        fn set_up(
            context: &mut TestContext<'_>,
            counters: &std::sync::Arc<Counters>,
        ) -> Result<Self, TestError> {
            FixtureOwner::set_up(context, counters).map(Self)
        }
        /// 使用 owner 的实际静止化责任。
        fn quiesce(&mut self, context: &mut TestContext<'_>) -> TestResult {
            self.0.quiesce(context)
        }
        /// 返回错误前先实际消费完整 owner；terminate 只收集结果，不承担资源清理。
        fn tear_down(self, _context: &mut TestContext<'_>) -> TestResult {
            drop(self);
            Err(TestError("cleanup failed after fatal return"))
        }
    }
    for (result, primary) in [
        (
            Ok(()),
            Failure::Returned(TestError("expected fatal did not occur")),
        ),
        (
            Err(TestError("body failed before fatal")),
            Failure::Returned(TestError("body failed before fatal")),
        ),
    ] {
        let runtime = Runtime::new();
        let counters = std::sync::Arc::new(Counters {
            quiesces: AtomicUsize::new(0),
            drops: AtomicUsize::new(0),
        });
        let mut context = TestContext::new(&runtime, &EXPECTED_FATAL, 0, 0);
        let terminated = catch_unwind(AssertUnwindSafe(|| {
            ktest::run_case::<CleanupError, _>(&mut context, &counters, |context, _| {
                ktest::expect!(context, false, "independent fatal-return expectation");
                result
            })
        }));
        let outcome = terminated.unwrap_err().downcast::<CaseOutcome>().unwrap();
        assert_eq!(counters.quiesces.load(Ordering::SeqCst), 1);
        assert_eq!(counters.drops.load(Ordering::SeqCst), 1);
        assert_eq!(std::sync::Arc::strong_count(&counters), 1);
        assert_eq!(outcome.status, Status::Error);
        assert_eq!(outcome.primary, Some(primary));
        assert_eq!(
            outcome.cleanup,
            Some(Failure::Returned(TestError(
                "cleanup failed after fatal return"
            )))
        );
        assert_eq!(outcome.expectations, 1);
    }
}

/// 主体 assert 会展开局部对象，成功 fixture 则保留到静止化之后才析构。
#[test]
fn assertion_unwinds_locals_and_preserves_fixture_until_cleanup() {
    let runtime = Runtime::new();
    let counters = std::sync::Arc::new(Counters {
        quiesces: AtomicUsize::new(0),
        drops: AtomicUsize::new(0),
    });
    let local_drops = AtomicUsize::new(0);
    let mut context = TestContext::new(&runtime, &CASE, 7, 0);
    let outcome = ktest::run_case::<FixtureOwner, _>(&mut context, &counters, |_, _| {
        struct Local<'a>(&'a AtomicUsize);
        impl Drop for Local<'_> {
            /// RAII 局部对象在真实捕获之前展开。
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let _local = Local(&local_drops);
        assert!(false, "controlled assertion");
        Ok(())
    });
    assert_eq!(outcome.status, Status::Fail);
    assert!(matches!(outcome.primary, Some(Failure::Panic(_))));
    assert_eq!(local_drops.load(Ordering::SeqCst), 1);
    assert_eq!(counters.quiesces.load(Ordering::SeqCst), 1);
    assert_eq!(counters.drops.load(Ordering::SeqCst), 1);
    assert_eq!(
        *runtime.phases.borrow(),
        [
            Phase::Setup,
            Phase::Body,
            Phase::Quiesce,
            Phase::Join,
            Phase::Cleanup
        ]
    );
    let mut next = TestContext::new(&runtime, &CASE, 8, 0);
    assert_eq!(
        ktest::run_case::<(), _>(&mut next, &(), |_, _| Ok(())).status,
        Status::Pass
    );
}

/// 部分 setup 失败只释放局部对象，禁止运行不存在对象的主体和清理方法。
#[test]
fn failed_setup_does_not_call_body_or_cleanup() {
    struct Never;
    impl Fixture<AtomicUsize> for Never {
        /// 构造失败前的局部资源由 Rust 正常释放，不产生可用 fixture。
        fn set_up(_context: &mut TestContext<'_>, drops: &AtomicUsize) -> Result<Self, TestError> {
            struct Partial<'a>(&'a AtomicUsize);
            impl Drop for Partial<'_> {
                /// 部分构造对象在返回错误时自然释放。
                fn drop(&mut self) {
                    self.0.fetch_add(1, Ordering::SeqCst);
                }
            }
            let _partial = Partial(drops);
            Err(TestError("setup failed"))
        }
        /// 若框架错误调用此方法，回归立即失败。
        fn quiesce(&mut self, _context: &mut TestContext<'_>) -> TestResult {
            panic!("nonexistent fixture quiesced")
        }
        /// 若框架错误消费不存在对象，回归立即失败。
        fn tear_down(self, _context: &mut TestContext<'_>) -> TestResult {
            panic!("nonexistent fixture destroyed")
        }
    }
    let runtime = Runtime::new();
    let mut context = TestContext::new(&runtime, &EXPECTED, 0, 0);
    let drops = AtomicUsize::new(0);
    let outcome = ktest::run_case::<Never, _>(&mut context, &drops, |_, _| {
        panic!("body ran after setup failure")
    });
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(outcome.status, Status::Error);
    assert_eq!(
        outcome.primary,
        Some(Failure::Returned(TestError("setup failed")))
    );
    assert_eq!(*runtime.phases.borrow(), [Phase::Setup, Phase::Join]);
}

/// setup panic 不属于主体的预期，即使诊断消息完全匹配。
#[test]
fn setup_panic_cannot_satisfy_body_expectation() {
    struct Panicking;
    impl Fixture<()> for Panicking {
        /// 成功对象尚未存在时触发受控构造 panic。
        fn set_up(_context: &mut TestContext<'_>, _resources: &()) -> Result<Self, TestError> {
            panic!("controlled setup");
        }
    }
    let runtime = Runtime::new();
    let mut context = TestContext::new(&runtime, &EXPECTED, 0, 0);
    let outcome =
        ktest::run_case::<Panicking, _>(&mut context, &(), |_, _| panic!("body must not run"));
    assert_eq!(outcome.status, Status::Error);
    assert!(
        matches!(outcome.primary, Some(Failure::Panic(message)) if message.as_str() == "controlled setup")
    );
    assert_eq!(*runtime.phases.borrow(), [Phase::Setup, Phase::Join]);
}

/// 主体预期只匹配主体，安全 join 的子失败仍使实例失败。
#[test]
fn expected_body_panic_does_not_cover_child_failure() {
    let mut runtime = Runtime::new();
    runtime.child = Some(Failure::Returned(TestError("child failed")));
    let mut context = TestContext::new(&runtime, &EXPECTED, 0, 0);
    let outcome = ktest::run_case::<(), _>(&mut context, &(), |_, _| panic!("controlled body"));
    assert_eq!(outcome.status, Status::Fail);
    assert_eq!(outcome.primary, None);
    assert_eq!(outcome.child, runtime.child);
}

/// 局部预期 panic 使用真实展开，expect 失败继续执行但不能最终 PASS。
#[test]
fn expectations_continue_and_local_panic_does_not_escape() {
    let runtime = Runtime::new();
    let mut context = TestContext::new(&runtime, &CASE, 0, 0);
    let reached = AtomicUsize::new(0);
    let outcome = ktest::run_case::<(), _>(&mut context, &(), |context, _| {
        context.expect_panic("controlled", || panic!("controlled local"));
        ktest::expect!(context, false, "independent field");
        context.expect_eq(1, 2);
        reached.store(1, Ordering::SeqCst);
        Ok(())
    });
    assert_eq!(reached.load(Ordering::SeqCst), 1);
    assert_eq!(outcome.status, Status::Fail);
    assert_eq!(outcome.primary, Some(Failure::Expectation));
    assert_eq!(outcome.expectations, 2);
}

/// 局部 panic 的正确短前缀若标记截断，仍记失败；完整正文匹配则通过。
#[test]
fn local_panic_requires_complete_message() {
    for (truncated, status, primary, expectations) in [
        (false, Status::Pass, None, 0),
        (true, Status::Fail, Some(Failure::Expectation), 1),
    ] {
        let runtime = Runtime::new();
        let mut context = TestContext::new(&runtime, &CASE, 0, 0);
        let mut reached = false;
        let outcome = ktest::run_case::<(), _>(&mut context, &(), |context, _| {
            context.expect_panic("controlled", || {
                panic_any(PanicMessage::new("controlled prefix", truncated))
            });
            reached = true;
            Ok(())
        });
        assert!(reached);
        assert_eq!(outcome.status, status);
        assert_eq!(outcome.primary, primary);
        assert_eq!(outcome.expectations, expectations);
    }
}

/// 主体 panic 的匹配前缀不能掩盖截断；保留消息和标志供最终失败报告。
#[test]
fn body_panic_requires_complete_message() {
    for (truncated, status, primary) in [
        (false, Status::Pass, None),
        (
            true,
            Status::Fail,
            Some(Failure::ExpectedPanicMismatch(PanicMessage::new(
                "controlled prefix",
                true,
            ))),
        ),
    ] {
        let runtime = Runtime::new();
        let mut context = TestContext::new(&runtime, &EXPECTED, 0, 0);
        let outcome = ktest::run_case::<(), _>(&mut context, &(), |_, _| {
            panic_any(PanicMessage::new("controlled prefix", truncated))
        });
        assert_eq!(outcome.status, status);
        assert_eq!(outcome.primary, primary);
        assert_eq!(outcome.expectations, 0);
    }
}

/// 没有 panic 和消息不符分别保留可区分错误。
#[test]
fn expected_panic_missing_and_mismatch_are_distinct() {
    let runtime = Runtime::new();
    let mut missing = TestContext::new(&runtime, &EXPECTED, 0, 0);
    assert_eq!(
        ktest::run_case::<(), _>(&mut missing, &(), |_, _| Ok(())).primary,
        Some(Failure::ExpectedPanicMissing)
    );
    let mut mismatch = TestContext::new(&runtime, &EXPECTED, 0, 0);
    assert!(matches!(
        ktest::run_case::<(), _>(&mut mismatch, &(), |_, _| panic!("different")).primary,
        Some(Failure::ExpectedPanicMismatch(_))
    ));
}

/// 已消费对象的 cleanup 错误不能覆盖主体返回错误或已安全 join 的子错误。
#[test]
fn cleanup_error_preserves_original_failures() {
    struct CleanupError;
    impl Fixture<()> for CleanupError {
        /// 完整构造无异步对象。
        fn set_up(_context: &mut TestContext<'_>, _resources: &()) -> Result<Self, TestError> {
            Ok(Self)
        }
        /// 消费对象后报告销毁失败。
        fn tear_down(self, _context: &mut TestContext<'_>) -> TestResult {
            Err(TestError("cleanup failed"))
        }
    }
    let mut runtime = Runtime::new();
    runtime.child = Some(Failure::Returned(TestError("child failed")));
    let mut context = TestContext::new(&runtime, &CASE, 0, 0);
    let result = catch_unwind(AssertUnwindSafe(|| {
        ktest::run_case::<CleanupError, _>(&mut context, &(), |_, _| Err(TestError("body failed")))
    }));
    let outcome = result.unwrap_err().downcast::<CaseOutcome>().unwrap();
    assert_eq!(outcome.status, Status::Error);
    assert_eq!(
        outcome.primary,
        Some(Failure::Returned(TestError("body failed")))
    );
    assert_eq!(outcome.child, runtime.child);
    assert_eq!(
        outcome.cleanup,
        Some(Failure::Returned(TestError("cleanup failed")))
    );
}

/// 多字节消息截断不得生成非法 UTF-8，且标志可见。
#[test]
fn panic_message_preserves_utf8_boundary() {
    let text = "界".repeat(100);
    let message = PanicMessage::new(&text, false);
    assert_eq!(message.as_str(), "界".repeat(85));
    assert!(message.truncated);
    let mut message = PanicMessage::new(&"a".repeat(255), false);
    core::fmt::Write::write_str(&mut message, "界").unwrap();
    core::fmt::Write::write_str(&mut message, "x").unwrap();
    assert_eq!(message.as_str(), "a".repeat(255));
}
