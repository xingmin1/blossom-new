//! stack 与 socket_native 共用的生产后台网络域和真实停点拥有。

use ::core::sync::atomic::{AtomicBool, Ordering};
use ::ktest::{Fixture, TestContext, TestError, TestResult};
use alloc::sync::Arc;

use super::support::operation;
use crate::ktest::execution_services::{baseline, Baseline, SuiteBaseline};
use crate::net::{NetStack, NetStackOwner, NetStackRef};

pub(super) struct StackOwner {
    before: Baseline,
    pub(super) owner: Option<NetStackOwner>,
    pub(super) net: NetStackRef,
    pub(super) release: Arc<AtomicBool>,
}

impl Fixture<SuiteBaseline> for StackOwner {
    /// 每例基线之后创建独立服务 owner，设置失败仍由真实 owner 在普通任务停止。
    fn set_up(context: &mut TestContext<'_>, _: &SuiteBaseline) -> Result<Self, TestError> {
        let before = baseline();
        let owner = operation(context, NetStack::try_new_lo())?;
        let net = Arc::clone(owner.stack());
        let release = Arc::try_new(AtomicBool::new(false))
            .map_err(|_| TestError("network stack release allocation failed"))?;
        Ok(Self {
            before,
            owner: Some(owner),
            net,
            release,
        })
    }

    /// 先解除本例真实 CPU 停点，框架随后合作停止并 join 原子任务。
    fn quiesce(&mut self, _: &mut TestContext<'_>) -> TestResult {
        self.release.store(true, Ordering::Release);
        Ok(())
    }

    /// 原子任务已离栈后才停止域全尾，释放所有观察拥有，再比较完整 MM 和 metadata。
    fn tear_down(mut self, context: &mut TestContext<'_>) -> TestResult {
        let before = self.before;
        drop(self.owner.take());
        drop(self);
        let after = baseline();
        context.diagnostic(format_args!(
            "network-stack resources-before={:?} resources-after={:?}",
            before, after
        ));
        context.expect_eq(after.0, before.0);
        context.expect_eq(after.1, before.1);
        if after != before {
            return Err(TestError("network stack resources did not converge"));
        }
        Ok(())
    }
}
