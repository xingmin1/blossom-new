//! 普通 Task、等待与迁核内存的逐例拥有和停稳账务。

mod context;
mod memory;
mod wait;

use crate::{cpu, mm, sched};
use ::ktest::{TestError, TestResult};
use core::sync::atomic::Ordering;

pub(crate) type Baseline = (mm::MemoryStats, crate::workqueue::testing::MetadataStats);

/// 协调者用生产安全点等待真实条件；超时保留原资源交给具体 fixture 静止化。
pub(crate) fn until(mut predicate: impl FnMut() -> bool, reason: &'static str) -> TestResult {
    let end = crate::time::now()
        .checked_add(crate::time::Duration::from_secs(5))
        .ok_or(TestError("execution deadline overflow"))?;
    while !predicate() {
        if end <= crate::time::now() {
            return Err(TestError(reason));
        }
        sched::yield_now();
    }
    Ok(())
}

/// 常驻服务真实阻塞、远核调度尾部结束后才读取完整六域和元数据。
pub(crate) fn baseline() -> Result<Baseline, TestError> {
    crate::klog::testing::wait_service_blocked();
    crate::workqueue::testing::wait_services_blocked();
    until(
        || {
            (0..cpu::count())
                .all(|id| id == cpu::current().id || cpu::get(id).idle.load(Ordering::Acquire))
        },
        "remote scheduler resource tail",
    )?;
    mm::shrink_all();
    mm::drain_all_pages();
    Ok((mm::stats(), crate::workqueue::testing::metadata_stats()))
}

/// 原 owner 全部释放以后逐字段比较，页账与元数据不得互相抵消。
pub(crate) fn reclaimed(expected: Baseline) -> TestResult {
    let actual = baseline()?;
    if actual.0 != expected.0 {
        return Err(TestError("temporary execution memory did not converge"));
    }
    if actual.1 != expected.1 {
        return Err(TestError("temporary execution metadata did not converge"));
    }
    Ok(())
}

/// 聚合实际只读 suite 槽，父注册入口保留这些真实描述符。
pub(crate) fn anchors() -> [&'static ::ktest::SuiteSlot; 6] {
    [
        context::context::__ktest_anchor(),
        context::migration::__ktest_anchor(),
        context::fatal::__ktest_anchor(),
        wait::queue::__ktest_anchor(),
        wait::mutex::__ktest_anchor(),
        memory::memory::__ktest_anchor(),
    ]
}
