//! 真实用户指令、私有现场、权限异常及页表拥有的逐例回归。

mod fixture;
mod probe;

include!("user_execution/entry.rs");
include!("user_execution/vm.rs");

/// 只保留宏产生的真实注册槽，不建立用户用例中央列表。
pub(crate) fn anchors() -> [&'static ::ktest::SuiteSlot; 2] {
    [entry::__ktest_anchor(), vm::__ktest_anchor()]
}
