//! 原生协议消费者按同步 wire、后台网络域和 SocketFile 拥有分别组织。

mod owner;
mod socket_helpers;
mod stack_helpers;
mod support;
mod wire;

// suite 宏直接扫描 inline 模块；在原命名空间展开完整套件，保留全名和归档锚点。
include!("network/core.rs");
include!("network/stack.rs");
include!("network/socket_native.rs");

/// 保留两个具名套件的真实宏锚点，不增加入口或中央分派器。
pub(super) fn anchors() -> [&'static ::ktest::SuiteSlot; 2] {
    [core::__ktest_anchor(), stack::__ktest_anchor()]
}
