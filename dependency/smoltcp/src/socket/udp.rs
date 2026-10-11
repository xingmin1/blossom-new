use core::cmp::min;
#[cfg(feature = "async")]
use core::task::Waker;

use crate::phy::PacketMeta;
use crate::socket::PollAt;
#[cfg(feature = "async")]
use crate::socket::WakerRegistration;
use crate::storage::Empty;
use crate::wire::{IpAddress, IpEndpoint, IpListenEndpoint, IpProtocol, IpRepr, UdpRepr};

/// UDP 单对象处理使用的本机广播判断与源地址选择。
pub trait UdpContext {
    /// 判断目的地址是否为当前介质的广播地址。
    fn is_broadcast(&self, address: &IpAddress) -> bool;

    /// 返回已选择的本机源地址；无可用源时返回 None。
    fn get_source_address(&self, destination: &IpAddress) -> Option<IpAddress>;
}

/// 保存锁外已确定的源地址和当前介质广播地址，不拥有接口或路由状态。
#[derive(Debug, Clone, Copy)]
pub struct UdpContextState {
    source: Option<IpAddress>,
    broadcast: Option<IpAddress>,
}

impl UdpContextState {
    /// 构造当前一次操作所需的小值环境。
    pub const fn new(source: Option<IpAddress>, broadcast: Option<IpAddress>) -> Self {
        Self { source, broadcast }
    }
}

impl UdpContext for UdpContextState {
    /// IPv4 全局广播和当前介质广播均使用同一广播判断。
    fn is_broadcast(&self, address: &IpAddress) -> bool {
        address.is_broadcast() || self.broadcast == Some(*address)
    }

    /// 仅返回与目的协议族一致的已选源地址。
    fn get_source_address(&self, destination: &IpAddress) -> Option<IpAddress> {
        self.source
            .filter(|source| source.version() == destination.version())
    }
}

/// 同一 UDP 消息的远端、本机地址与逐包设备 metadata。
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub struct UdpMetadata {
    pub endpoint: IpEndpoint,
    /// RX 的目的 IP；TX 的真实源 IP。省略 TX 源时沿绑定地址和 Context 选择。
    pub local_address: Option<IpAddress>,
    /// 本条 TX 的真实源端口，保持跨解绑或重新绑定的旧消息身份；RX 保持 None。
    pub local_port: Option<u16>,
    pub meta: PacketMeta,
}

impl<T: Into<IpEndpoint>> From<T> for UdpMetadata {
    /// 从远端构造默认 metadata；未指定的本机字段由实际处理路径决定。
    fn from(value: T) -> Self {
        Self {
            endpoint: value.into(),
            local_address: None,
            local_port: None,
            meta: PacketMeta::default(),
        }
    }
}

impl core::fmt::Display for UdpMetadata {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        #[cfg(feature = "packetmeta-id")]
        return write!(f, "{}, PacketID: {:?}", self.endpoint, self.meta);

        #[cfg(not(feature = "packetmeta-id"))]
        write!(f, "{}", self.endpoint)
    }
}

/// A UDP packet metadata.
pub type PacketMetadata = crate::storage::PacketMetadata<UdpMetadata>;

/// A UDP packet ring buffer.
pub type PacketBuffer<'a> = crate::storage::PacketBuffer<'a, UdpMetadata>;

/// Error returned by [`Socket::bind`]
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum BindError {
    InvalidState,
    Unaddressable,
}

impl core::fmt::Display for BindError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            BindError::InvalidState => write!(f, "invalid state"),
            BindError::Unaddressable => write!(f, "unaddressable"),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for BindError {}

/// Error returned by [`Socket::send`]
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum SendError {
    Unaddressable,
    BufferFull,
}

impl core::fmt::Display for SendError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            SendError::Unaddressable => write!(f, "unaddressable"),
            SendError::BufferFull => write!(f, "buffer full"),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for SendError {}

/// Error returned by [`Socket::recv`]
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub enum RecvError {
    Exhausted,
    Truncated,
}

impl core::fmt::Display for RecvError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            RecvError::Exhausted => write!(f, "exhausted"),
            RecvError::Truncated => write!(f, "truncated"),
        }
    }
}

#[cfg(feature = "std")]
impl std::error::Error for RecvError {}

/// A User Datagram Protocol socket.
///
/// A UDP socket is bound to a specific endpoint, and owns transmit and receive
/// packet buffers.
#[derive(Debug)]
pub struct Socket<'a> {
    endpoint: IpListenEndpoint,
    rx_buffer: PacketBuffer<'a>,
    tx_buffer: PacketBuffer<'a>,
    rx_budget: usize,
    tx_budget: usize,
    /// The time-to-live (IPv4) or hop limit (IPv6) value used in outgoing packets.
    hop_limit: Option<u8>,
    #[cfg(feature = "async")]
    rx_waker: WakerRegistration,
    #[cfg(feature = "async")]
    tx_waker: WakerRegistration,
}

impl<'a> Socket<'a> {
    /// 使用同一对 PacketBuffer 构造 socket，默认预算为各自物理 payload 容量。
    pub fn new(rx_buffer: PacketBuffer<'a>, tx_buffer: PacketBuffer<'a>) -> Socket<'a> {
        Socket {
            endpoint: IpListenEndpoint::default(),
            rx_budget: rx_buffer.payload_capacity(),
            tx_budget: tx_buffer.payload_capacity(),
            rx_buffer,
            tx_buffer,
            hop_limit: None,
            #[cfg(feature = "async")]
            rx_waker: WakerRegistration::new(),
            #[cfg(feature = "async")]
            tx_waker: WakerRegistration::new(),
        }
    }

    /// Register a waker for receive operations.
    ///
    /// The waker is woken on state changes that might affect the return value
    /// of `recv` method calls, such as receiving data, or the socket closing.
    ///
    /// Notes:
    ///
    /// - Only one waker can be registered at a time. If another waker was previously registered,
    ///   it is overwritten and will no longer be woken.
    /// - The Waker is woken only once. Once woken, you must register it again to receive more wakes.
    /// - "Spurious wakes" are allowed: a wake doesn't guarantee the result of `recv` has
    ///   necessarily changed.
    #[cfg(feature = "async")]
    pub fn register_recv_waker(&mut self, waker: &Waker) {
        self.rx_waker.register(waker)
    }

    /// Register a waker for send operations.
    ///
    /// The waker is woken on state changes that might affect the return value
    /// of `send` method calls, such as space becoming available in the transmit
    /// buffer, or the socket closing.
    ///
    /// Notes:
    ///
    /// - Only one waker can be registered at a time. If another waker was previously registered,
    ///   it is overwritten and will no longer be woken.
    /// - The Waker is woken only once. Once woken, you must register it again to receive more wakes.
    /// - "Spurious wakes" are allowed: a wake doesn't guarantee the result of `send` has
    ///   necessarily changed.
    #[cfg(feature = "async")]
    pub fn register_send_waker(&mut self, waker: &Waker) {
        self.tx_waker.register(waker)
    }

    /// Return the bound endpoint.
    #[inline]
    pub fn endpoint(&self) -> IpListenEndpoint {
        self.endpoint
    }

    /// Return the time-to-live (IPv4) or hop limit (IPv6) value used in outgoing packets.
    ///
    /// See also the [set_hop_limit](#method.set_hop_limit) method
    pub fn hop_limit(&self) -> Option<u8> {
        self.hop_limit
    }

    /// Set the time-to-live (IPv4) or hop limit (IPv6) value used in outgoing packets.
    ///
    /// A socket without an explicitly set hop limit value uses the default [IANA recommended]
    /// value (64).
    ///
    /// # Panics
    ///
    /// This function panics if a hop limit value of 0 is given. See [RFC 1122 § 3.2.1.7].
    ///
    /// [IANA recommended]: https://www.iana.org/assignments/ip-parameters/ip-parameters.xhtml
    /// [RFC 1122 § 3.2.1.7]: https://tools.ietf.org/html/rfc1122#section-3.2.1.7
    pub fn set_hop_limit(&mut self, hop_limit: Option<u8>) {
        // A host MUST NOT send a datagram with a hop limit value of 0
        if let Some(0) = hop_limit {
            panic!("the time-to-live value of a packet must not be zero")
        }

        self.hop_limit = hop_limit
    }

    /// Bind the socket to the given endpoint.
    ///
    /// This function returns `Err(Error::Illegal)` if the socket was open
    /// (see [is_open](#method.is_open)), and `Err(Error::Unaddressable)`
    /// if the port in the given endpoint is zero.
    pub fn bind<T: Into<IpListenEndpoint>>(&mut self, endpoint: T) -> Result<(), BindError> {
        let endpoint = endpoint.into();
        if endpoint.port == 0 {
            return Err(BindError::Unaddressable);
        }

        if self.is_open() {
            return Err(BindError::InvalidState);
        }

        self.endpoint = endpoint;

        #[cfg(feature = "async")]
        {
            self.rx_waker.wake();
            self.tx_waker.wake();
        }

        Ok(())
    }

    /// 更新已由调用者持租约确认的 binding；port0 解除绑定，但不删除旧 RX/TX 消息。
    pub fn set_endpoint(&mut self, endpoint: IpListenEndpoint) {
        self.endpoint = endpoint;
        #[cfg(feature = "async")]
        {
            self.rx_waker.wake();
            self.tx_waker.wake();
        }
    }

    /// Close the socket.
    pub fn close(&mut self) {
        // Clear the bound endpoint of the socket.
        self.endpoint = IpListenEndpoint::default();

        // Reset the RX and TX buffers of the socket.
        self.tx_buffer.reset();
        self.rx_buffer.reset();

        #[cfg(feature = "async")]
        {
            self.rx_waker.wake();
            self.tx_waker.wake();
        }
    }

    /// Check whether the socket is open.
    #[inline]
    pub fn is_open(&self) -> bool {
        self.endpoint.port != 0
    }

    /// 查询当前是否能接纳至少一个 payload 字节；零消息使用 can_send_size(0)。
    #[inline]
    pub fn can_send(&self) -> bool {
        self.can_send_size(1)
    }

    /// 同时核对预算、metadata 和原环连续空间；零消息仍占用一个 metadata。
    pub fn can_send_size(&self, size: usize) -> bool {
        self.tx_buffer
            .enqueue_payload_cost(size)
            .is_some_and(|cost| {
                cost <= self
                    .tx_budget
                    .saturating_sub(self.tx_buffer.payload_bytes_count())
                    && self.tx_buffer.payload_bytes_count() <= self.tx_budget
            })
    }

    /// Check whether the receive buffer is not empty.
    #[inline]
    pub fn can_recv(&self) -> bool {
        !self.rx_buffer.is_empty()
    }

    /// Return the maximum number packets the socket can receive.
    #[inline]
    pub fn packet_recv_capacity(&self) -> usize {
        self.rx_buffer.packet_capacity()
    }

    /// Return the maximum number packets the socket can transmit.
    #[inline]
    pub fn packet_send_capacity(&self) -> usize {
        self.tx_buffer.packet_capacity()
    }

    /// Return the maximum number of bytes inside the recv buffer.
    #[inline]
    pub fn payload_recv_capacity(&self) -> usize {
        self.rx_buffer.payload_capacity()
    }

    /// Return the maximum number of bytes inside the transmit buffer.
    #[inline]
    pub fn payload_send_capacity(&self) -> usize {
        self.tx_buffer.payload_capacity()
    }

    /// 返回接收接纳预算，物理容量仍由 payload_recv_capacity 查询。
    pub fn recv_budget(&self) -> usize {
        self.rx_budget
    }

    /// 返回发送接纳预算，包含同一 payload 环预留的 padding。
    pub fn send_budget(&self) -> usize {
        self.tx_budget
    }

    /// 在物理范围内修改后继接收预算，已有消息保持完整。
    pub fn set_recv_budget(&mut self, budget: usize) {
        assert!(1 <= budget && budget <= self.rx_buffer.payload_capacity());
        self.rx_budget = budget;
    }

    /// 在物理范围内修改后继发送预算，不删除已接纳消息。
    pub fn set_send_budget(&mut self, budget: usize) {
        assert!(1 <= budget && budget <= self.tx_buffer.payload_capacity());
        #[cfg(feature = "async")]
        let could_send = self.can_send();
        self.tx_budget = budget;
        #[cfg(feature = "async")]
        if !could_send && self.can_send() {
            self.tx_waker.wake();
        }
    }

    /// 向同一 TX 环接纳完整消息并借出 payload；地址无效或空间不足时不提交记录。
    pub fn send(
        &mut self,
        size: usize,
        meta: impl Into<UdpMetadata>,
    ) -> Result<&mut [u8], SendError> {
        let meta = meta.into();
        if self.endpoint.port == 0 {
            return Err(SendError::Unaddressable);
        }
        if meta.endpoint.addr.is_unspecified() {
            return Err(SendError::Unaddressable);
        }
        if meta.endpoint.port == 0 {
            return Err(SendError::Unaddressable);
        }
        if !self.can_send_size(size) {
            return Err(SendError::BufferFull);
        }

        let payload_buf = self
            .tx_buffer
            .enqueue(size, meta)
            .map_err(|_| SendError::BufferFull)?;

        net_trace!(
            "udp:{}:{}: buffer to send {} octets",
            self.endpoint,
            meta.endpoint,
            size
        );
        Ok(payload_buf)
    }

    /// 按 max_size 预留预算和连续空间，提交闭包实际写入的一条消息。
    pub fn send_with<F>(
        &mut self,
        max_size: usize,
        meta: impl Into<UdpMetadata>,
        f: F,
    ) -> Result<usize, SendError>
    where
        F: FnOnce(&mut [u8]) -> usize,
    {
        let meta = meta.into();
        if self.endpoint.port == 0 {
            return Err(SendError::Unaddressable);
        }
        if meta.endpoint.addr.is_unspecified() {
            return Err(SendError::Unaddressable);
        }
        if meta.endpoint.port == 0 {
            return Err(SendError::Unaddressable);
        }
        if !self.can_send_size(max_size) {
            return Err(SendError::BufferFull);
        }

        let size = self
            .tx_buffer
            .enqueue_with_infallible(max_size, meta, f)
            .map_err(|_| SendError::BufferFull)?;

        net_trace!(
            "udp:{}:{}: buffer to send {} octets",
            self.endpoint,
            meta.endpoint,
            size
        );
        Ok(size)
    }

    /// 将一条完整消息复制到原 TX 环；预算不足时不复制或提交前缀。
    pub fn send_slice(
        &mut self,
        data: &[u8],
        meta: impl Into<UdpMetadata>,
    ) -> Result<(), SendError> {
        self.send(data.len(), meta)?.copy_from_slice(data);
        Ok(())
    }

    /// Dequeue a packet received from a remote endpoint, and return the endpoint as well
    /// as a pointer to the payload.
    ///
    /// This function returns `Err(Error::Exhausted)` if the receive buffer is empty.
    pub fn recv(&mut self) -> Result<(&[u8], UdpMetadata), RecvError> {
        let (remote_endpoint, payload_buf) =
            self.rx_buffer.dequeue().map_err(|_| RecvError::Exhausted)?;

        net_trace!(
            "udp:{}:{}: receive {} buffered octets",
            self.endpoint,
            remote_endpoint.endpoint,
            payload_buf.len()
        );
        Ok((payload_buf, remote_endpoint))
    }

    /// Dequeue a packet received from a remote endpoint, copy the payload into the given slice,
    /// and return the amount of octets copied as well as the endpoint.
    ///
    /// **Note**: when the size of the provided buffer is smaller than the size of the payload,
    /// the packet is dropped and a `RecvError::Truncated` error is returned.
    ///
    /// See also [recv](#method.recv).
    pub fn recv_slice(&mut self, data: &mut [u8]) -> Result<(usize, UdpMetadata), RecvError> {
        let (buffer, endpoint) = self.recv().map_err(|_| RecvError::Exhausted)?;

        if data.len() < buffer.len() {
            return Err(RecvError::Truncated);
        }

        let length = min(data.len(), buffer.len());
        data[..length].copy_from_slice(&buffer[..length]);
        Ok((length, endpoint))
    }

    /// Peek at a packet received from a remote endpoint, and return the endpoint as well
    /// as a pointer to the payload without removing the packet from the receive buffer.
    /// This function otherwise behaves identically to [recv](#method.recv).
    ///
    /// It returns `Err(Error::Exhausted)` if the receive buffer is empty.
    pub fn peek(&mut self) -> Result<(&[u8], &UdpMetadata), RecvError> {
        let endpoint = self.endpoint;
        self.rx_buffer.peek().map_err(|_| RecvError::Exhausted).map(
            |(remote_endpoint, payload_buf)| {
                net_trace!(
                    "udp:{}:{}: peek {} buffered octets",
                    endpoint,
                    remote_endpoint.endpoint,
                    payload_buf.len()
                );
                (payload_buf, remote_endpoint)
            },
        )
    }

    /// Peek at a packet received from a remote endpoint, copy the payload into the given slice,
    /// and return the amount of octets copied as well as the endpoint without removing the
    /// packet from the receive buffer.
    /// This function otherwise behaves identically to [recv_slice](#method.recv_slice).
    ///
    /// **Note**: when the size of the provided buffer is smaller than the size of the payload,
    /// no data is copied into the provided buffer and a `RecvError::Truncated` error is returned.
    ///
    /// See also [peek](#method.peek).
    pub fn peek_slice(&mut self, data: &mut [u8]) -> Result<(usize, &UdpMetadata), RecvError> {
        let (buffer, endpoint) = self.peek()?;

        if data.len() < buffer.len() {
            return Err(RecvError::Truncated);
        }

        let length = min(data.len(), buffer.len());
        data[..length].copy_from_slice(&buffer[..length]);
        Ok((length, endpoint))
    }

    /// Return the amount of octets queued in the transmit buffer.
    ///
    /// Note that the Berkeley sockets interface does not have an equivalent of this API.
    pub fn send_queue(&self) -> usize {
        self.tx_buffer.payload_bytes_count()
    }

    /// Return the amount of octets queued in the receive buffer. This value can be larger than
    /// the slice read by the next `recv` or `peek` call because it includes all queued octets,
    /// and not only the octets that may be returned as a contiguous slice.
    ///
    /// Note that the Berkeley sockets interface does not have an equivalent of this API.
    pub fn recv_queue(&self) -> usize {
        self.rx_buffer.payload_bytes_count()
    }

    /// 在不取得接口状态的情况下判断端口、本机地址和广播匹配。
    pub fn accepts<C: UdpContext>(&self, cx: &mut C, ip_repr: &IpRepr, repr: &UdpRepr) -> bool {
        if self.endpoint.port != repr.dst_port {
            return false;
        }
        if self.endpoint.addr.is_some()
            && self.endpoint.addr != Some(ip_repr.dst_addr())
            && !cx.is_broadcast(&ip_repr.dst_addr())
            && !ip_repr.dst_addr().is_multicast()
        {
            return false;
        }

        true
    }

    /// 把匹配的消息复制入唯一 RX 环；返回真实接纳事实，满时只丢弃本次输入。
    pub fn process<C: UdpContext, P: crate::storage::SliceLike<Item = u8>>(
        &mut self,
        cx: &mut C,
        meta: PacketMeta,
        ip_repr: &IpRepr,
        repr: &UdpRepr,
        payload: P,
    ) -> bool {
        debug_assert!(self.accepts(cx, ip_repr, repr));

        let size = payload.len();

        let remote_endpoint = IpEndpoint {
            addr: ip_repr.src_addr(),
            port: repr.src_port,
        };

        net_trace!(
            "udp:{}:{}: receiving {} octets",
            self.endpoint,
            remote_endpoint,
            size
        );

        let metadata = UdpMetadata {
            endpoint: remote_endpoint,
            local_address: Some(ip_repr.dst_addr()),
            local_port: None,
            meta,
        };

        let fits = self
            .rx_buffer
            .enqueue_payload_cost(size)
            .is_some_and(|cost| {
                cost <= self
                    .rx_budget
                    .saturating_sub(self.rx_buffer.payload_bytes_count())
                    && self.rx_buffer.payload_bytes_count() <= self.rx_budget
            });
        if fits {
            if let Ok(buf) = self.rx_buffer.enqueue(size, metadata) {
                payload.copy_to_slice(buf);
                #[cfg(feature = "async")]
                self.rx_waker.wake();
                return true;
            }
        }
        net_trace!(
            "udp:{}:{}: buffer full, dropped incoming packet",
            self.endpoint,
            remote_endpoint
        );
        false
    }

    /// 借出队首完整消息；emit 失败时保留同一 metadata 和 payload 供后续重试。
    pub fn dispatch<C: UdpContext, F, E>(&mut self, cx: &mut C, emit: F) -> Result<(), E>
    where
        F: FnOnce(&mut C, PacketMeta, (IpRepr, UdpRepr, &[u8])) -> Result<(), E>,
    {
        let endpoint = self.endpoint;
        let hop_limit = self.hop_limit.unwrap_or(64);

        let res = self.tx_buffer.dequeue_with(|packet_meta, payload_buf| {
            let src_addr = if let Some(s) = packet_meta.local_address {
                s
            } else {
                match endpoint.addr {
                    Some(addr) => addr,
                    None => match cx.get_source_address(&packet_meta.endpoint.addr) {
                        Some(addr) => addr,
                        None => {
                            net_trace!(
                                "udp:{}:{}: cannot find suitable source address, dropping.",
                                endpoint,
                                packet_meta.endpoint
                            );
                            return Ok(());
                        }
                    },
                }
            };

            net_trace!(
                "udp:{}:{}: sending {} octets",
                endpoint,
                packet_meta.endpoint,
                payload_buf.len()
            );

            let repr = UdpRepr {
                src_port: packet_meta.local_port.unwrap_or(endpoint.port),
                dst_port: packet_meta.endpoint.port,
            };
            let ip_repr = IpRepr::new(
                src_addr,
                packet_meta.endpoint.addr,
                IpProtocol::Udp,
                repr.header_len() + payload_buf.len(),
                hop_limit,
            );

            emit(cx, packet_meta.meta, (ip_repr, repr, payload_buf))
        });
        match res {
            Err(Empty) => Ok(()),
            Ok(Err(e)) => Err(e),
            Ok(Ok(())) => {
                #[cfg(feature = "async")]
                self.tx_waker.wake();
                Ok(())
            }
        }
    }

    /// 根据真实 TX 记录判断单对象是否有待发送责任。
    pub fn poll_at<C: UdpContext>(&self, _cx: &mut C) -> PollAt {
        if self.tx_buffer.is_empty() {
            PollAt::Ingress
        } else {
            PollAt::Now
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::wire::{IpRepr, UdpRepr};

    fn buffer(packets: usize) -> PacketBuffer<'static> {
        PacketBuffer::new(
            (0..packets)
                .map(|_| PacketMetadata::EMPTY)
                .collect::<Vec<_>>(),
            vec![0; 16 * packets],
        )
    }

    fn socket(
        rx_buffer: PacketBuffer<'static>,
        tx_buffer: PacketBuffer<'static>,
    ) -> Socket<'static> {
        Socket::new(rx_buffer, tx_buffer)
    }

    /// 为原 UDP 行为测试提供已选源地址，不构造 Interface 或设备。
    fn context() -> UdpContextState {
        UdpContextState::new(Some(LOCAL_ADDR.into()), None)
    }

    const LOCAL_PORT: u16 = 53;
    const REMOTE_PORT: u16 = 49500;

    cfg_if::cfg_if! {
        if #[cfg(feature = "proto-ipv4")] {
            use crate::wire::Ipv4Address as IpvXAddress;
            use crate::wire::Ipv4Repr as IpvXRepr;
            use IpRepr::Ipv4 as IpReprIpvX;

            const LOCAL_ADDR: IpvXAddress = IpvXAddress::new(192, 168, 1, 1);
            const REMOTE_ADDR: IpvXAddress = IpvXAddress::new(192, 168, 1, 2);
            const OTHER_ADDR: IpvXAddress = IpvXAddress::new(192, 168, 1, 3);

            const LOCAL_END: IpEndpoint = IpEndpoint {
                addr: IpAddress::Ipv4(LOCAL_ADDR),
                port: LOCAL_PORT,
            };
            const REMOTE_END: IpEndpoint = IpEndpoint {
                addr: IpAddress::Ipv4(REMOTE_ADDR),
                port: REMOTE_PORT,
            };
        } else {
            use crate::wire::Ipv6Address as IpvXAddress;
            use crate::wire::Ipv6Repr as IpvXRepr;
            use IpRepr::Ipv6 as IpReprIpvX;

            const LOCAL_ADDR: IpvXAddress = IpvXAddress::new(0xfe80, 0, 0, 0, 0, 0, 0, 1);
            const REMOTE_ADDR: IpvXAddress = IpvXAddress::new(0xfe80, 0, 0, 0, 0, 0, 0, 2);
            const OTHER_ADDR: IpvXAddress = IpvXAddress::new(0xfe80, 0, 0, 0, 0, 0, 0, 3);

            const LOCAL_END: IpEndpoint = IpEndpoint {
                addr: IpAddress::Ipv6(LOCAL_ADDR),
                port: LOCAL_PORT,
            };
            const REMOTE_END: IpEndpoint = IpEndpoint {
                addr: IpAddress::Ipv6(REMOTE_ADDR),
                port: REMOTE_PORT,
            };
        }
    }

    /// 构造当前实际 RX 消息的完整远端和本机名称。
    fn remote_metadata_with_local() -> UdpMetadata {
        UdpMetadata {
            local_address: Some(LOCAL_ADDR.into()),
            ..REMOTE_END.into()
        }
    }

    pub const LOCAL_IP_REPR: IpRepr = IpReprIpvX(IpvXRepr {
        src_addr: LOCAL_ADDR,
        dst_addr: REMOTE_ADDR,
        next_header: IpProtocol::Udp,
        payload_len: 8 + 6,
        hop_limit: 64,
    });

    pub const REMOTE_IP_REPR: IpRepr = IpReprIpvX(IpvXRepr {
        src_addr: REMOTE_ADDR,
        dst_addr: LOCAL_ADDR,
        next_header: IpProtocol::Udp,
        payload_len: 8 + 6,
        hop_limit: 64,
    });

    pub const BAD_IP_REPR: IpRepr = IpReprIpvX(IpvXRepr {
        src_addr: REMOTE_ADDR,
        dst_addr: OTHER_ADDR,
        next_header: IpProtocol::Udp,
        payload_len: 8 + 6,
        hop_limit: 64,
    });

    const LOCAL_UDP_REPR: UdpRepr = UdpRepr {
        src_port: LOCAL_PORT,
        dst_port: REMOTE_PORT,
    };

    const REMOTE_UDP_REPR: UdpRepr = UdpRepr {
        src_port: REMOTE_PORT,
        dst_port: LOCAL_PORT,
    };

    const PAYLOAD: &[u8] = b"abcdef";

    #[test]
    fn test_bind_unaddressable() {
        let mut socket = socket(buffer(0), buffer(0));
        assert_eq!(socket.bind(0), Err(BindError::Unaddressable));
    }

    #[test]
    fn test_bind_twice() {
        let mut socket = socket(buffer(0), buffer(0));
        assert_eq!(socket.bind(1), Ok(()));
        assert_eq!(socket.bind(2), Err(BindError::InvalidState));
    }

    #[test]
    #[should_panic(expected = "the time-to-live value of a packet must not be zero")]
    fn test_set_hop_limit_zero() {
        let mut s = socket(buffer(0), buffer(1));
        s.set_hop_limit(Some(0));
    }

    #[test]
    fn test_send_unaddressable() {
        let mut socket = socket(buffer(0), buffer(1));

        assert_eq!(
            socket.send_slice(b"abcdef", REMOTE_END),
            Err(SendError::Unaddressable)
        );
        assert_eq!(socket.bind(LOCAL_PORT), Ok(()));
        assert_eq!(
            socket.send_slice(
                b"abcdef",
                IpEndpoint {
                    addr: IpvXAddress::UNSPECIFIED.into(),
                    ..REMOTE_END
                }
            ),
            Err(SendError::Unaddressable)
        );
        assert_eq!(
            socket.send_slice(
                b"abcdef",
                IpEndpoint {
                    port: 0,
                    ..REMOTE_END
                }
            ),
            Err(SendError::Unaddressable)
        );
        assert_eq!(socket.send_slice(b"abcdef", REMOTE_END), Ok(()));
    }

    #[test]
    fn test_send_with_source() {
        let mut socket = socket(buffer(0), buffer(1));

        assert_eq!(socket.bind(LOCAL_PORT), Ok(()));
        assert_eq!(
            socket.send_slice(b"abcdef", remote_metadata_with_local()),
            Ok(())
        );
    }

    /// 验证失败 dispatch 保留同一消息，成功后才释放发送槽。
    #[test]
    fn test_send_dispatch() {
        let cx = &mut context();
        let mut socket = socket(buffer(0), buffer(1));

        assert_eq!(socket.bind(LOCAL_END), Ok(()));

        assert!(socket.can_send());
        assert_eq!(
            socket.dispatch(cx, |_, _, _| unreachable!()),
            Ok::<_, ()>(())
        );

        assert_eq!(socket.send_slice(b"abcdef", REMOTE_END), Ok(()));
        assert_eq!(
            socket.send_slice(b"123456", REMOTE_END),
            Err(SendError::BufferFull)
        );
        assert!(!socket.can_send());

        assert_eq!(
            socket.dispatch(cx, |_, _, (ip_repr, udp_repr, payload)| {
                assert_eq!(ip_repr, LOCAL_IP_REPR);
                assert_eq!(udp_repr, LOCAL_UDP_REPR);
                assert_eq!(payload, PAYLOAD);
                Err(())
            }),
            Err(())
        );
        assert!(!socket.can_send());

        assert_eq!(
            socket.dispatch(cx, |_, _, (ip_repr, udp_repr, payload)| {
                assert_eq!(ip_repr, LOCAL_IP_REPR);
                assert_eq!(udp_repr, LOCAL_UDP_REPR);
                assert_eq!(payload, PAYLOAD);
                Ok::<_, ()>(())
            }),
            Ok(())
        );
        assert!(socket.can_send());
    }

    /// 验证实际收到的首条消息及满槽丢弃。
    #[test]
    fn test_recv_process() {
        let cx = &mut context();

        let mut socket = socket(buffer(1), buffer(0));

        assert_eq!(socket.bind(LOCAL_PORT), Ok(()));

        assert!(!socket.can_recv());
        assert_eq!(socket.recv(), Err(RecvError::Exhausted));

        assert!(socket.accepts(cx, &REMOTE_IP_REPR, &REMOTE_UDP_REPR));
        socket.process(
            cx,
            PacketMeta::default(),
            &REMOTE_IP_REPR,
            &REMOTE_UDP_REPR,
            PAYLOAD,
        );
        assert!(socket.can_recv());

        assert!(socket.accepts(cx, &REMOTE_IP_REPR, &REMOTE_UDP_REPR));
        socket.process(
            cx,
            PacketMeta::default(),
            &REMOTE_IP_REPR,
            &REMOTE_UDP_REPR,
            PAYLOAD,
        );

        assert_eq!(
            socket.recv(),
            Ok((&b"abcdef"[..], remote_metadata_with_local()))
        );
        assert!(!socket.can_recv());
    }

    /// 验证 PEEK 后同一消息仍能完整消费。
    #[test]
    fn test_peek_process() {
        let cx = &mut context();

        let mut socket = socket(buffer(1), buffer(0));

        assert_eq!(socket.bind(LOCAL_PORT), Ok(()));

        assert_eq!(socket.peek(), Err(RecvError::Exhausted));

        socket.process(
            cx,
            PacketMeta::default(),
            &REMOTE_IP_REPR,
            &REMOTE_UDP_REPR,
            PAYLOAD,
        );
        assert_eq!(
            socket.peek(),
            Ok((&b"abcdef"[..], &remote_metadata_with_local(),))
        );
        assert_eq!(
            socket.recv(),
            Ok((&b"abcdef"[..], remote_metadata_with_local(),))
        );
        assert_eq!(socket.peek(), Err(RecvError::Exhausted));
    }

    /// 验证短接收报错并消费整条消息。
    #[test]
    fn test_recv_truncated_slice() {
        let cx = &mut context();

        let mut socket = socket(buffer(1), buffer(0));

        assert_eq!(socket.bind(LOCAL_PORT), Ok(()));

        assert!(socket.accepts(cx, &REMOTE_IP_REPR, &REMOTE_UDP_REPR));
        socket.process(
            cx,
            PacketMeta::default(),
            &REMOTE_IP_REPR,
            &REMOTE_UDP_REPR,
            PAYLOAD,
        );

        let mut slice = [0; 4];
        assert_eq!(socket.recv_slice(&mut slice[..]), Err(RecvError::Truncated));
    }

    /// 验证短 PEEK 不消费，后续短接收消费整条消息。
    #[test]
    fn test_peek_truncated_slice() {
        let cx = &mut context();

        let mut socket = socket(buffer(1), buffer(0));

        assert_eq!(socket.bind(LOCAL_PORT), Ok(()));

        socket.process(
            cx,
            PacketMeta::default(),
            &REMOTE_IP_REPR,
            &REMOTE_UDP_REPR,
            PAYLOAD,
        );

        let mut slice = [0; 4];
        assert_eq!(socket.peek_slice(&mut slice[..]), Err(RecvError::Truncated));
        assert_eq!(socket.recv_slice(&mut slice[..]), Err(RecvError::Truncated));
        assert_eq!(socket.peek_slice(&mut slice[..]), Err(RecvError::Exhausted));
    }

    /// 验证原 hop limit 进入实际发送 repr。
    #[test]
    fn test_set_hop_limit() {
        let cx = &mut context();

        let mut s = socket(buffer(0), buffer(1));

        assert_eq!(s.bind(LOCAL_END), Ok(()));

        s.set_hop_limit(Some(0x2a));
        assert_eq!(s.send_slice(b"abcdef", REMOTE_END), Ok(()));
        assert_eq!(
            s.dispatch(cx, |_, _, (ip_repr, _, _)| {
                assert_eq!(
                    ip_repr,
                    IpReprIpvX(IpvXRepr {
                        src_addr: LOCAL_ADDR,
                        dst_addr: REMOTE_ADDR,
                        next_header: IpProtocol::Udp,
                        payload_len: 8 + 6,
                        hop_limit: 0x2a,
                    })
                );
                Ok::<_, ()>(())
            }),
            Ok(())
        );
    }

    /// 验证仅匹配实际绑定端口。
    #[test]
    fn test_doesnt_accept_wrong_port() {
        let cx = &mut context();

        let mut socket = socket(buffer(1), buffer(0));

        assert_eq!(socket.bind(LOCAL_PORT), Ok(()));

        let mut udp_repr = REMOTE_UDP_REPR;
        assert!(socket.accepts(cx, &REMOTE_IP_REPR, &udp_repr));
        udp_repr.dst_port += 1;
        assert!(!socket.accepts(cx, &REMOTE_IP_REPR, &udp_repr));
    }

    /// 验证通配与具体本机地址的不同接纳条件。
    #[test]
    fn test_doesnt_accept_wrong_ip() {
        let cx = &mut context();

        let mut port_bound_socket = socket(buffer(1), buffer(0));
        assert_eq!(port_bound_socket.bind(LOCAL_PORT), Ok(()));
        assert!(port_bound_socket.accepts(cx, &BAD_IP_REPR, &REMOTE_UDP_REPR));

        let mut ip_bound_socket = socket(buffer(1), buffer(0));
        assert_eq!(ip_bound_socket.bind(LOCAL_END), Ok(()));
        assert!(!ip_bound_socket.accepts(cx, &BAD_IP_REPR, &REMOTE_UDP_REPR));
    }

    #[test]
    fn test_send_large_packet() {
        // buffer(4) creates a payload buffer of size 16*4
        let mut socket = socket(buffer(0), buffer(4));
        assert_eq!(socket.bind(LOCAL_END), Ok(()));

        let too_large = b"0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdefx";
        assert_eq!(
            socket.send_slice(too_large, REMOTE_END),
            Err(SendError::BufferFull)
        );
        assert_eq!(socket.send_slice(&too_large[..16 * 4], REMOTE_END), Ok(()));
    }

    /// 验证零 payload 仍保存并消费一条 metadata。
    #[test]
    fn test_process_empty_payload() {
        let meta = Box::leak(Box::new([PacketMetadata::EMPTY]));
        let recv_buffer = PacketBuffer::new(&mut meta[..], vec![]);
        let mut socket = socket(recv_buffer, buffer(0));

        let cx = &mut context();

        assert_eq!(socket.bind(LOCAL_PORT), Ok(()));

        let repr = UdpRepr {
            src_port: REMOTE_PORT,
            dst_port: LOCAL_PORT,
        };
        socket.process(cx, PacketMeta::default(), &REMOTE_IP_REPR, &repr, &[][..]);
        assert_eq!(socket.recv(), Ok((&[][..], remote_metadata_with_local())));
    }

    /// 验证零消息受 32 个 metadata 槽限制，入队事实和实际消费均包含零长度记录。
    #[test]
    fn test_zero_datagram_record_limit() {
        let cx = &mut context();
        let mut s = socket(buffer(32), buffer(32));
        s.bind(LOCAL_PORT).unwrap();
        for _ in 0..32 {
            assert!(s.process(
                cx,
                PacketMeta::default(),
                &REMOTE_IP_REPR,
                &REMOTE_UDP_REPR,
                &[][..]
            ));
            s.send_slice(&[], REMOTE_END).unwrap();
        }
        assert!(!s.process(
            cx,
            PacketMeta::default(),
            &REMOTE_IP_REPR,
            &REMOTE_UDP_REPR,
            &[][..]
        ));
        assert_eq!(s.send_slice(&[], REMOTE_END), Err(SendError::BufferFull));
        assert!(!s.can_send_size(0));
        for _ in 0..32 {
            assert_eq!(s.recv(), Ok((&[][..], remote_metadata_with_local())));
            s.dispatch(cx, |_, _, (_, udp, bytes)| {
                assert_eq!(udp, LOCAL_UDP_REPR);
                assert!(bytes.is_empty());
                Ok::<_, ()>(())
            })
            .unwrap();
        }
        assert_eq!(s.recv(), Err(RecvError::Exhausted));
        s.dispatch(cx, |_, _, _| unreachable!())
            .unwrap_or_else(|(): ()| unreachable!());
        assert!(s.can_send_size(0));
    }

    /// 验证预算缩小不改旧消息，提高后真实恢复接纳；send_with 也遵守同一预算。
    #[test]
    fn test_payload_budgets_preserve_records() {
        let cx = &mut context();
        let mut s = socket(buffer(4), buffer(4));
        s.bind(LOCAL_PORT).unwrap();
        s.set_recv_budget(12);
        s.set_send_budget(12);
        for bytes in [&b"abcdef"[..], &b"123456"[..]] {
            assert!(s.process(
                cx,
                PacketMeta::default(),
                &REMOTE_IP_REPR,
                &REMOTE_UDP_REPR,
                bytes
            ));
            s.send_slice(bytes, REMOTE_END).unwrap();
        }
        assert!(!s.can_send());
        assert_eq!(s.send(1, REMOTE_END), Err(SendError::BufferFull));
        assert_eq!(
            s.send_with(1, REMOTE_END, |_| unreachable!()),
            Err(SendError::BufferFull)
        );
        assert!(!s.process(
            cx,
            PacketMeta::default(),
            &REMOTE_IP_REPR,
            &REMOTE_UDP_REPR,
            PAYLOAD
        ));
        s.set_recv_budget(4);
        s.set_send_budget(4);
        assert!(!s.can_send_size(0));
        assert_eq!(s.peek().unwrap().0, PAYLOAD);
        assert_eq!(s.recv().unwrap().0, PAYLOAD);
        s.dispatch(cx, |_, _, (_, _, bytes)| {
            assert_eq!(bytes, PAYLOAD);
            Ok::<_, ()>(())
        })
        .unwrap();
        assert!(!s.process(
            cx,
            PacketMeta::default(),
            &REMOTE_IP_REPR,
            &REMOTE_UDP_REPR,
            &[][..]
        ));
        assert_eq!(s.send_slice(&[], REMOTE_END), Err(SendError::BufferFull));
        s.set_recv_budget(12);
        s.set_send_budget(12);
        assert!(s.process(
            cx,
            PacketMeta::default(),
            &REMOTE_IP_REPR,
            &REMOTE_UDP_REPR,
            PAYLOAD
        ));
        assert_eq!(
            s.send_with(6, REMOTE_END, |bytes| {
                bytes.copy_from_slice(PAYLOAD);
                bytes.len()
            }),
            Ok(6)
        );
        for bytes in [&b"123456"[..], PAYLOAD] {
            assert_eq!(s.recv().unwrap().0, bytes);
            s.dispatch(cx, |_, _, (_, _, actual)| {
                assert_eq!(actual, bytes);
                Ok::<_, ()>(())
            })
            .unwrap();
        }
        assert_eq!(s.recv(), Err(RecvError::Exhausted));
        assert!(s.can_send());
    }

    /// 验证绕回 padding 也占用预算，预算拒绝不改变队首消息。
    #[test]
    fn test_payload_budget_includes_padding() {
        let cx = &mut context();
        let mut s = socket(
            PacketBuffer::new(vec![PacketMetadata::EMPTY; 4], vec![0; 16]),
            PacketBuffer::new(vec![PacketMetadata::EMPTY; 4], vec![0; 16]),
        );
        s.bind(LOCAL_PORT).unwrap();
        for bytes in [&b"abcdef"[..], &b"12345678"[..]] {
            assert!(s.process(
                cx,
                PacketMeta::default(),
                &REMOTE_IP_REPR,
                &REMOTE_UDP_REPR,
                bytes
            ));
            s.send_slice(bytes, REMOTE_END).unwrap();
        }
        assert_eq!(s.recv().unwrap().0, b"abcdef");
        s.dispatch(cx, |_, _, (_, _, bytes)| {
            assert_eq!(bytes, b"abcdef");
            Ok::<_, ()>(())
        })
        .unwrap();
        s.set_recv_budget(12);
        s.set_send_budget(12);
        assert!(!s.process(
            cx,
            PacketMeta::default(),
            &REMOTE_IP_REPR,
            &REMOTE_UDP_REPR,
            &b"abcd"[..]
        ));
        assert!(!s.can_send_size(4));
        assert_eq!(
            s.send_slice(b"abcd", REMOTE_END),
            Err(SendError::BufferFull)
        );
        s.set_recv_budget(14);
        s.set_send_budget(14);
        assert!(s.process(
            cx,
            PacketMeta::default(),
            &REMOTE_IP_REPR,
            &REMOTE_UDP_REPR,
            &b"abcd"[..]
        ));
        s.send_slice(b"abcd", REMOTE_END).unwrap();
        for bytes in [&b"12345678"[..], &b"abcd"[..]] {
            assert_eq!(s.recv().unwrap().0, bytes);
            s.dispatch(cx, |_, _, (_, _, actual)| {
                assert_eq!(actual, bytes);
                Ok::<_, ()>(())
            })
            .unwrap();
        }
    }

    /// 验证字节预算恰满时仍可记录零消息，零消息不借用额外 payload 字节。
    #[test]
    fn test_zero_datagram_at_byte_limit() {
        let cx = &mut context();
        let mut s = socket(buffer(4), buffer(4));
        s.bind(LOCAL_PORT).unwrap();
        s.set_recv_budget(6);
        s.set_send_budget(6);
        assert!(s.process(
            cx,
            PacketMeta::default(),
            &REMOTE_IP_REPR,
            &REMOTE_UDP_REPR,
            PAYLOAD
        ));
        s.send_slice(PAYLOAD, REMOTE_END).unwrap();
        assert!(!s.can_send());
        assert!(s.can_send_size(0));
        assert!(s.process(
            cx,
            PacketMeta::default(),
            &REMOTE_IP_REPR,
            &REMOTE_UDP_REPR,
            &[][..]
        ));
        s.send_slice(&[], REMOTE_END).unwrap();
        for bytes in [PAYLOAD, &[][..]] {
            assert_eq!(s.recv().unwrap().0, bytes);
            s.dispatch(cx, |_, _, (_, _, actual)| {
                assert_eq!(actual, bytes);
                Ok::<_, ()>(())
            })
            .unwrap();
        }
        assert_eq!(s.recv(), Err(RecvError::Exhausted));
        assert!(s.can_send());
    }

    /// 验证一族实际发送和消费：默认源、显式源及零消息都保持各自 metadata。
    fn check_datagram_sources(local: IpAddress, remote: IpAddress, explicit: IpAddress) {
        let mut cx = UdpContextState::new(Some(local), None);
        let mut sender = socket(buffer(0), buffer(4));
        let mut receiver = socket(buffer(4), buffer(0));
        sender.bind(LOCAL_PORT).unwrap();
        receiver.bind((remote, REMOTE_PORT)).unwrap();
        let destination = IpEndpoint::new(remote, REMOTE_PORT);
        for (source, bytes, metadata) in [
            (local, PAYLOAD, UdpMetadata::from(destination)),
            (
                explicit,
                &b"123"[..],
                UdpMetadata {
                    local_address: Some(explicit),
                    ..destination.into()
                },
            ),
            (local, &[][..], UdpMetadata::from(destination)),
        ] {
            sender.send_slice(bytes, metadata).unwrap();
            sender
                .dispatch(&mut cx, |cx, meta, (ip, udp, payload)| {
                    assert_eq!(ip.src_addr(), source);
                    assert_eq!(ip.dst_addr(), remote);
                    assert_eq!(udp.src_port, LOCAL_PORT);
                    assert_eq!(udp.dst_port, REMOTE_PORT);
                    assert_eq!(payload, bytes);
                    assert!(receiver.accepts(cx, &ip, &udp));
                    assert!(receiver.process(cx, meta, &ip, &udp, payload));
                    Ok::<_, ()>(())
                })
                .unwrap();
            let (payload, received) = receiver.recv().unwrap();
            assert_eq!(payload, bytes);
            assert_eq!(
                received,
                UdpMetadata {
                    endpoint: IpEndpoint::new(source, LOCAL_PORT),
                    local_address: Some(remote),
                    local_port: None,
                    meta: metadata.meta,
                }
            );
            assert_eq!(receiver.recv(), Err(RecvError::Exhausted));
            sender
                .dispatch(&mut cx, |_, _, _| unreachable!())
                .unwrap_or_else(|(): ()| unreachable!());
        }
    }

    /// 验证 IPv4 单报文和不同源地址的真实消费。
    #[test]
    #[cfg(feature = "proto-ipv4")]
    fn test_ipv4_datagram_sources() {
        use crate::wire::Ipv4Address;
        check_datagram_sources(
            Ipv4Address::new(192, 168, 1, 1).into(),
            Ipv4Address::new(192, 168, 1, 2).into(),
            Ipv4Address::new(192, 168, 1, 3).into(),
        );
    }

    /// 验证 IPv6 单报文和不同源地址的真实消费。
    #[test]
    #[cfg(feature = "proto-ipv6")]
    fn test_ipv6_datagram_sources() {
        use crate::wire::Ipv6Address;
        check_datagram_sources(
            Ipv6Address::new(0xfd00, 0, 0, 0, 0, 0, 0, 1).into(),
            Ipv6Address::new(0xfd00, 0, 0, 0, 0, 0, 0, 2).into(),
            Ipv6Address::new(0xfd00, 0, 0, 0, 0, 0, 0, 3).into(),
        );
    }

    /// 验证失败重试和解绑后重新绑定均保原 TX 源端口，旧 RX 也仍然可读。
    #[test]
    fn test_rebind_preserves_accepted_datagrams() {
        let cx = &mut context();
        let mut s = socket(buffer(4), buffer(4));
        s.bind(LOCAL_END).unwrap();
        assert!(s.process(
            cx,
            PacketMeta::default(),
            &REMOTE_IP_REPR,
            &REMOTE_UDP_REPR,
            PAYLOAD
        ));
        let metadata = UdpMetadata {
            local_address: Some(LOCAL_ADDR.into()),
            local_port: Some(LOCAL_PORT),
            ..REMOTE_END.into()
        };
        s.send_slice(PAYLOAD, metadata).unwrap();
        s.send_slice(PAYLOAD, metadata).unwrap();
        assert_eq!(
            s.dispatch(cx, |_, _, (ip, udp, bytes)| {
                assert_eq!(ip.src_addr(), LOCAL_END.addr);
                assert_eq!(udp.src_port, LOCAL_PORT);
                assert_eq!(bytes, PAYLOAD);
                Err(())
            }),
            Err(())
        );
        s.set_endpoint(IpListenEndpoint::default());
        assert!(!s.is_open());
        s.dispatch(cx, |_, _, (ip, udp, bytes)| {
            assert_eq!(ip.src_addr(), LOCAL_END.addr);
            assert_eq!(udp.src_port, LOCAL_PORT);
            assert_eq!(bytes, PAYLOAD);
            Ok::<_, ()>(())
        })
        .unwrap();
        s.bind(IpEndpoint::new(OTHER_ADDR.into(), LOCAL_PORT + 1))
            .unwrap();
        s.send_slice(
            b"new",
            UdpMetadata {
                local_address: Some(OTHER_ADDR.into()),
                local_port: Some(LOCAL_PORT + 1),
                ..REMOTE_END.into()
            },
        )
        .unwrap();
        assert_eq!(s.recv(), Ok((PAYLOAD, remote_metadata_with_local())));
        for (source, port, bytes) in [
            (LOCAL_END.addr, LOCAL_PORT, PAYLOAD),
            (OTHER_ADDR.into(), LOCAL_PORT + 1, &b"new"[..]),
        ] {
            s.dispatch(cx, |_, _, (ip, udp, payload)| {
                assert_eq!(ip.src_addr(), source);
                assert_eq!(udp.src_port, port);
                assert_eq!(ip.dst_addr(), REMOTE_END.addr);
                assert_eq!(udp.dst_port, REMOTE_PORT);
                assert_eq!(payload, bytes);
                Ok::<_, ()>(())
            })
            .unwrap();
        }
        s.dispatch(cx, |_, _, _| unreachable!())
            .unwrap_or_else(|(): ()| unreachable!());
    }

    #[test]
    fn test_closing() {
        let meta = Box::leak(Box::new([PacketMetadata::EMPTY]));
        let recv_buffer = PacketBuffer::new(&mut meta[..], vec![]);
        let mut socket = socket(recv_buffer, buffer(0));
        assert_eq!(socket.bind(LOCAL_PORT), Ok(()));

        assert!(socket.is_open());
        socket.close();
        assert!(!socket.is_open());
    }
}
