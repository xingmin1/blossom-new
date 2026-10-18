//! 原生协议消费者按 wire、后台网络域及七个 socket 行为拥有分别组织。

mod owner;
mod socket_helpers;
mod stack_helpers;
mod support;
mod wire;

// suite 宏直接扫描 inline 模块，每个行为套件独立拥有资源基线和清理边界。
include!("network/core.rs");
include!("network/stack.rs");
include!("network/socket_unix_stream.rs");
include!("network/socket_tcp_listener.rs");
include!("network/socket_tcp_progress.rs");
include!("network/socket_unix_seqpacket.rs");
include!("network/socket_unix_owner.rs");
include!("network/socket_udp_lease.rs");
include!("network/socket_udp_retirement.rs");

/// 显式保活全部真实宏注册槽，不增加中央用例分派器或旧套件代理。
pub(super) fn anchors() -> [&'static ::ktest::SuiteSlot; 9] {
    [
        core::__ktest_anchor(),
        stack::__ktest_anchor(),
        socket_unix_stream::__ktest_anchor(),
        socket_tcp_listener::__ktest_anchor(),
        socket_tcp_progress::__ktest_anchor(),
        socket_unix_seqpacket::__ktest_anchor(),
        socket_unix_owner::__ktest_anchor(),
        socket_udp_lease::__ktest_anchor(),
        socket_udp_retirement::__ktest_anchor(),
    ]
}
