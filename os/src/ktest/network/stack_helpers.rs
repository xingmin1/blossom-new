//! 真实协议端点的准备与握手观察，不替后台执行协议。

use ::ktest::{TestContext, TestError};
use alloc::sync::Arc;
use alloc::vec::Vec;
use smoltcp::socket::tcp::State;
use smoltcp::socket::udp as raw_udp;

use super::support::{observe, operation, SERVER};
use crate::net::address::{AddressFamily, InetEndpoint};
use crate::net::tcp::TcpEndpoint;
use crate::net::NetStackRef;

/// 原 PacketBuffer 在基线后 fallible 准备 metadata 与字节后备，测试不另建 payload 队列。
pub(super) fn udp_buffer(
    records: usize,
    bytes: usize,
) -> Result<raw_udp::PacketBuffer<'static>, TestError> {
    let mut metadata = Vec::new();
    metadata
        .try_reserve_exact(records)
        .map_err(|_| TestError("UDP metadata allocation failed"))?;
    metadata.resize(records, raw_udp::PacketMetadata::EMPTY);
    let mut payload = Vec::new();
    payload
        .try_reserve_exact(bytes)
        .map_err(|_| TestError("UDP payload allocation failed"))?;
    payload.resize(bytes, 0);
    Ok(raw_udp::PacketBuffer::new(metadata, payload))
}

/// listener 先构造并保持原对象，两个 client 的编号轮转不受动态 child 构造影响。
pub(super) fn listener(
    context: &TestContext<'_>,
    net: &NetStackRef,
    port: u16,
) -> Result<Arc<TcpEndpoint>, TestError> {
    let endpoint = operation(context, net.try_new_tcp(AddressFamily::Ipv4, 256, 256))?;
    operation(context, endpoint.bind(InetEndpoint { port, ..SERVER }))?;
    operation(context, endpoint.listen(4))?;
    Ok(endpoint)
}

/// 完整握手由后台进行；accept 只消费真正 ready child，失败不会伪造连接。
pub(super) fn accepted(
    context: &mut TestContext<'_>,
    listener: &Arc<TcpEndpoint>,
    client: &Arc<TcpEndpoint>,
) -> Result<Arc<TcpEndpoint>, TestError> {
    observe(
        || client.state() == State::Established,
        "background loopback handshake did not finish",
    )?;
    let mut child = None;
    let mut failure = None;
    observe(
        || {
            match listener.accept() {
                Ok(value) => child = value,
                Err(error) => failure = Some(error),
            }
            child.is_some() || failure.is_some()
        },
        "background listener child did not become ready",
    )?;
    if let Some(error) = failure {
        return operation(context, Err(error));
    }
    context.expect_eq(listener.state(), State::Listen);
    let child = child.ok_or(TestError("listener ready child missing"))?;
    context.expect_eq(child.state(), State::Established);
    Ok(child)
}
