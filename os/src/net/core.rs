//! 成熟 TCP 算法拥有连接自己的 ring 与环境；本层只处理包拥有、复制进度和时间域转换。

use ::core::convert::TryFrom;
use smoltcp::socket::tcp::TcpContext;
use smoltcp::socket::{tcp, PollAt};
use smoltcp::time::Instant;

use crate::fs::{FsError, ReadBuffer, WriteBuffer};
use crate::net::address::InetEndpoint;
use crate::net::packet::{zeroed_bytes, OwnedPacket};
use crate::net::IoProgress;
use crate::time::MonotonicInstant;

/// 立即工作、绝对定时和仅等待输入保持不同责任，egress 阻塞由真实容量源解除。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProtocolDeadline {
    Now,
    At(MonotonicInstant),
    Ingress,
}

/// 单连接算法和缓冲均由本对象拥有，不借全局 Interface 或 SocketSet。
pub struct TcpCore {
    socket: tcp::Socket<'static>,
    context: tcp::TcpContextState,
}

impl TcpCore {
    /// 在发表前准备两个 fallible ring 和独立随机环境，失败只释放未发表的存储。
    pub fn try_new(
        rx_len: usize,
        tx_len: usize,
        ip_mtu: usize,
        seed: u64,
    ) -> Result<Self, FsError> {
        if rx_len == 0 || tx_len == 0 || (1usize << 30) < rx_len {
            return Err(FsError::InvalidOperation);
        }
        if ip_mtu < 68 || (u16::MAX as usize) < ip_mtu {
            return Err(FsError::InvalidOperation);
        }
        let rx = tcp::SocketBuffer::new(zeroed_bytes(rx_len)?);
        let tx = tcp::SocketBuffer::new(zeroed_bytes(tx_len)?);
        let socket = tcp::Socket::new(rx, tx);
        Ok(Self {
            socket,
            context: tcp::TcpContextState::new(Instant::ZERO, ip_mtu, seed),
        })
    }

    /// 源地址和端口已由网络域选择；本核心只开始这个明确四元组的握手。
    pub fn connect(
        &mut self,
        local: InetEndpoint,
        peer: InetEndpoint,
        now: MonotonicInstant,
    ) -> Result<(), FsError> {
        self.context.set_now(raw_time(now));
        self.socket
            .connect(&mut self.context, peer.raw(), local.raw())
            .map_err(|error| match error {
                tcp::ConnectError::InvalidState => FsError::InvalidOperation,
                tcp::ConnectError::Unaddressable => FsError::AddressNotAvailable,
            })
    }

    /// 监听保留通配地址，不分配临时端口；绑定冲突属于同一网络域的表。
    pub fn listen(&mut self, local: InetEndpoint) -> Result<(), FsError> {
        self.socket
            .listen(local.raw_listen())
            .map_err(|error| match error {
                tcp::ListenError::InvalidState => FsError::InvalidOperation,
                tcp::ListenError::Unaddressable => FsError::AddressNotAvailable,
            })
    }

    /// 在连接保护内消费完整 IPv4/TCP 输入；立即回应在结束 repr 借用前编码为 owned 包。
    pub fn process(
        &mut self,
        packet: &OwnedPacket,
        now: MonotonicInstant,
    ) -> Result<Option<OwnedPacket>, FsError> {
        let (ip, tcp) = packet.tcp()?;
        self.context.set_now(raw_time(now));
        if !self.socket.accepts(&ip, &tcp) {
            return Err(FsError::InvalidOperation);
        }
        let ip_mtu = self.context.ip_mtu();
        self.socket
            .process(&mut self.context, &ip, &tcp, tcp.payload)
            .map(|(ip, tcp)| OwnedPacket::try_from_tcp(&ip, &tcp, ip_mtu))
            .transpose()
    }

    /// 同步 emitter 先接受真正拥有的包，成功后 raw 才提交正常发送进度。
    /// raw 的 emit 前重传和计时副作用遵循算法原有次序，错误不伪造全状态回滚。
    pub fn dispatch<F>(&mut self, now: MonotonicInstant, emit: F) -> Result<(), FsError>
    where
        F: FnOnce(OwnedPacket) -> Result<(), FsError>,
    {
        self.context.set_now(raw_time(now));
        let ip_mtu = self.context.ip_mtu();
        self.socket.dispatch(&mut self.context, |_, (ip, tcp)| {
            emit(OwnedPacket::try_from_tcp(&ip, &tcp, ip_mtu)?)
        })
    }

    /// 将算法三态期限映射到同一个 monotonic 时间域，不将 Now 当成周期重排许可。
    pub fn deadline(&mut self, now: MonotonicInstant) -> ProtocolDeadline {
        self.context.set_now(raw_time(now));
        match self.socket.poll_at(&mut self.context) {
            PollAt::Now => ProtocolDeadline::Now,
            PollAt::Ingress => ProtocolDeadline::Ingress,
            PollAt::Time(at) => {
                let micros = u64::try_from(at.total_micros()).expect("negative TCP deadline");
                let nanos = micros.checked_mul(1000).expect("TCP deadline overflow");
                ProtocolDeadline::At(MonotonicInstant::from_nanos(nanos))
            }
        }
    }

    /// 普通任务持本连接 SleepMutex 交付连续前缀，copyout 故障只消费实际 copied 差量。
    /// PEEK 可交付连续短前缀但不移动 ring。
    pub fn receive(
        &mut self,
        target: &mut ReadBuffer<'_>,
        peek: bool,
    ) -> Result<IoProgress, FsError> {
        if target.copied() == target.len() {
            return Ok(IoProgress::Ready(0));
        }
        let before = target.copied();
        if peek {
            let bytes = match self.socket.peek(target.len() - before) {
                Ok(bytes) => bytes,
                Err(error) => return self.receive_error(error),
            };
            let copied = target.push(bytes);
            return copy_progress(target.copied() - before, copied);
        }
        match self.socket.recv(|bytes| {
            let copied = target.push(bytes);
            let consumed = target.copied() - before;
            (consumed, copy_progress(consumed, copied))
        }) {
            Ok(progress) => progress,
            Err(error) => self.receive_error(error),
        }
    }

    /// 输入直接复制到本连接连续 TX 空段，只有闭包返回的真实差量成为 raw 已接受字节。
    pub fn send(&mut self, source: &mut WriteBuffer<'_>) -> Result<IoProgress, FsError> {
        if source.copied() == source.len() {
            return Ok(IoProgress::Ready(0));
        }
        let before = source.copied();
        match self.socket.send(|bytes| {
            let copied = source.pull(bytes);
            let accepted = source.copied() - before;
            (accepted, copy_progress(accepted, copied))
        }) {
            Ok(progress) => progress,
            Err(tcp::SendError::InvalidState) => match self.socket.state() {
                tcp::State::SynSent | tcp::State::SynReceived => Ok(IoProgress::Blocked),
                tcp::State::FinWait1
                | tcp::State::FinWait2
                | tcp::State::Closing
                | tcp::State::LastAck
                | tcp::State::TimeWait => Err(FsError::BrokenPipe),
                _ => Err(FsError::NotConnected),
            },
        }
    }

    /// 将 raw 的握手等待、FIN 后完整 EOF 和未连接错误保留为不同结果。
    fn receive_error(&self, error: tcp::RecvError) -> Result<IoProgress, FsError> {
        match error {
            tcp::RecvError::Finished => Ok(IoProgress::Eof),
            tcp::RecvError::InvalidState => match self.socket.state() {
                tcp::State::SynSent | tcp::State::SynReceived => Ok(IoProgress::Blocked),
                _ => Err(FsError::NotConnected),
            },
        }
    }

    /// 请求发送 FIN，已接受的发送字节仍沿原算法排出。
    pub fn close(&mut self) {
        self.socket.close();
    }

    /// 请求立即终止，后续 dispatch 按原算法发送 RST 并结束 tuple。
    pub fn abort(&mut self) {
        self.socket.abort();
    }

    /// 状态只有成熟协议核心的一份，不在适配层建立第二份状态机。
    pub fn state(&self) -> tcp::State {
        self.socket.state()
    }

    /// 连接建立后返回明确的本地端点值，监听地址由 listen_endpoint 查询。
    pub fn local_endpoint(&self) -> Option<InetEndpoint> {
        self.socket.local_endpoint().map(InetEndpoint::from_raw)
    }

    /// 返回这个核心真正拥有的远端地址快照。
    pub fn remote_endpoint(&self) -> Option<InetEndpoint> {
        self.socket.remote_endpoint().map(InetEndpoint::from_raw)
    }

    /// 监听通配地址保持零地址，不把通配静默改成 loopback。
    pub fn listen_endpoint(&self) -> InetEndpoint {
        let endpoint = self.socket.listen_endpoint();
        let address = endpoint
            .addr
            .map(|address| {
                let smoltcp::wire::IpAddress::Ipv4(address) = address;
                address.octets()
            })
            .unwrap_or([0; 4]);
        InetEndpoint {
            address,
            port: endpoint.port,
        }
    }

    /// 返回当前可交付字节数，不能以它替代 FIN 或错误查询。
    pub fn receive_queue(&self) -> usize {
        self.socket.recv_queue()
    }

    /// 返回已被协议接受但尚未 ACK 释放的发送 ring 字节数。
    pub fn send_queue(&self) -> usize {
        self.socket.send_queue()
    }

    /// 就绪判断直接取原算法的接收能力，不推测用户是否已经读取。
    pub fn can_receive(&self) -> bool {
        self.socket.can_recv()
    }

    /// 就绪判断直接取原算法的发送能力和真实 ring 容量。
    pub fn can_send(&self) -> bool {
        self.socket.can_send()
    }
}

/// 零进度的非空请求等待状态变化；故障后的正前缀保持成功提交。
fn copy_progress(copied: usize, result: Result<usize, FsError>) -> Result<IoProgress, FsError> {
    if 0 < copied {
        return Ok(IoProgress::Ready(copied));
    }
    result.map(|_| IoProgress::Blocked)
}

/// 单调纳秒转 raw 微秒；u64 纳秒除以一千后始终可表示为 i64。
fn raw_time(now: MonotonicInstant) -> Instant {
    Instant::from_micros((now.as_nanos() / 1000) as i64)
}
