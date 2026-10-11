//! 打开对象持有具体协议端点；流前缀与完整报文分别在真实接受点提交。

use alloc::sync::Arc;
use core::ops::Range;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use crate::fs::epoll::WatchHead;
use crate::fs::poll::{PollContext, PollMask, PollNotify, PollTable, Pollable};
use crate::fs::{DirectoryEntry, FileStat, FsError, InodeKind, InodeRef, InodeTime, Path};
use crate::fs::{FileIoContext, FileLike, ReadBuffer, SeekFrom, WriteBuffer};
use crate::net::address::{InetEndpoint, SocketAddress};
use crate::net::tcp::TcpEndpoint;
use crate::net::udp::UdpEndpoint;
use crate::net::unix::UnixEndpoint;
use crate::net::{IoProgress, MessageResult, NetStackRef};
use crate::wait::WaitResult;

pub const TCP_BUFFER_LEN: usize = 16 * 1024;
const O_RDWR: u32 = 2;
const O_APPEND: u32 = 0o2000;
const O_NONBLOCK: u32 = 0o4000;
const O_CLOEXEC: u32 = 0o2000000;
const MSG_PEEK: u32 = 0x2;
const MSG_TRUNC: u32 = 0x20;
const MSG_DONTWAIT: u32 = 0x40;
const MSG_NOSIGNAL: u32 = 0x4000;
const STREAM_PREFIX_LEN: usize = 4096;
const DATAGRAM_PREFIX_LEN: usize = 1472;

/// 一个打开实例只选择一种真实协议端点，不引入另一层 socket 表。
#[derive(Clone)]
enum Endpoint {
    Tcp(Arc<TcpEndpoint>),
    Udp(Arc<UdpEndpoint>),
    Unix(Arc<UnixEndpoint>),
}

impl Endpoint {
    /// 构造失败时撤销未发表端点，TCP 保留原协议终止责任。
    fn abandon(&self) {
        match self {
            Self::Tcp(endpoint) => endpoint.abandon(),
            Self::Udp(endpoint) => endpoint.close(),
            Self::Unix(endpoint) => endpoint.close(),
        }
    }

    /// 末 OFD 请求正常关闭，不同步等待对端或协议 Work。
    fn close(&self) {
        match self {
            Self::Tcp(endpoint) => endpoint.close(),
            Self::Udp(endpoint) => endpoint.close(),
            Self::Unix(endpoint) => endpoint.close(),
        }
    }
}

/// 打开对象持稳定协议端点、网络域和创建者凭据，并保存共享 OFD 状态位。
/// 构造只准备打开实例，不发表用户 fd。
pub struct SocketFile {
    endpoint: Endpoint,
    net: NetStackRef,
    uid: u32,
    gid: u32,
    flags: AtomicU32,
    watches: Arc<WatchHead>,
    /// 阻塞 connect 尚未领取完成时保留该事实，信号重启仍消费原握手结果。
    connect_unclaimed: AtomicBool,
    opened: bool,
}

impl SocketFile {
    /// 从同一网络域准备默认有界 TCP 核心与新的打开实例，CLOEXEC 留给描述符槽。
    pub fn try_new_tcp(
        net: NetStackRef,
        flags: u32,
        uid: u32,
        gid: u32,
    ) -> Result<Arc<Self>, FsError> {
        if flags & !(O_NONBLOCK | O_CLOEXEC) != 0 {
            return Err(FsError::InvalidOperation);
        }
        let endpoint = net.try_new_tcp(TCP_BUFFER_LEN, TCP_BUFFER_LEN)?;
        Self::allocate(net, Endpoint::Tcp(endpoint), flags, uid, gid)
    }

    /// 从本域准备 UDP 打开实例，零长消息仍由真实报文端点提交。
    pub fn try_new_udp(
        net: NetStackRef,
        flags: u32,
        uid: u32,
        gid: u32,
    ) -> Result<Arc<Self>, FsError> {
        if flags & !(O_NONBLOCK | O_CLOEXEC) != 0 {
            return Err(FsError::InvalidOperation);
        }
        let endpoint = net.try_new_udp()?;
        Self::allocate(net, Endpoint::Udp(endpoint), flags, uid, gid)
    }

    /// 完整准备真实匿名 pair 的两个 OFD，任一分配失败关闭尚未发表的两侧。
    pub fn try_new_unix_pair(
        net: NetStackRef,
        flags: u32,
        uid: u32,
        gid: u32,
    ) -> Result<[Arc<Self>; 2], FsError> {
        if flags & !(O_NONBLOCK | O_CLOEXEC) != 0 {
            return Err(FsError::InvalidOperation);
        }
        let [first, second] = net.try_new_unix_pair()?;
        let first = match Self::allocate(net.clone(), Endpoint::Unix(first), flags, uid, gid) {
            Ok(first) => first,
            Err(error) => {
                second.close();
                return Err(error);
            }
        };
        let second = Self::allocate(net, Endpoint::Unix(second), flags, uid, gid)?;
        Ok([first, second])
    }

    /// 为已拥有的本域协议端点构造独立 OFD，不搬走 raw 核心或替换稳定 listener。
    pub fn from_endpoint(
        net: NetStackRef,
        endpoint: Arc<TcpEndpoint>,
        flags: u32,
        uid: u32,
        gid: u32,
    ) -> Result<Arc<Self>, FsError> {
        if flags & !(O_NONBLOCK | O_CLOEXEC) != 0 {
            return Err(FsError::InvalidOperation);
        }
        if !endpoint.belongs_to(&net) {
            return Err(FsError::InvalidOperation);
        }
        Self::allocate(net, Endpoint::Tcp(endpoint), flags, uid, gid)
    }

    /// 分配失败撤回未交给打开实例的协议拥有，成功后才赋予末 OFD 关闭责任。
    fn allocate(
        net: NetStackRef,
        endpoint: Endpoint,
        flags: u32,
        uid: u32,
        gid: u32,
    ) -> Result<Arc<Self>, FsError> {
        let watches = match Arc::try_new(WatchHead::new()) {
            Ok(watches) => watches,
            Err(_) => {
                endpoint.abandon();
                return Err(FsError::NoMemory);
            }
        };
        let result = Arc::try_new(Self {
            endpoint: endpoint.clone(),
            net,
            uid,
            gid,
            flags: AtomicU32::new(O_RDWR | (flags & O_NONBLOCK)),
            watches,
            connect_unclaimed: AtomicBool::new(false),
            opened: false,
        });
        match result {
            Ok(mut file) => {
                Arc::get_mut(&mut file)
                    .expect("unpublished socket file is shared")
                    .opened = true;
                Ok(file)
            }
            Err(_) => {
                endpoint.abandon();
                Err(FsError::NoMemory)
            }
        }
    }

    /// 仅向原 TCP 内核消费者借用稳定端点，其他协议不能被误当 TCP。
    pub fn tcp_endpoint(&self) -> Result<&Arc<TcpEndpoint>, FsError> {
        match &self.endpoint {
            Endpoint::Tcp(endpoint) => Ok(endpoint),
            _ => Err(FsError::Unsupported),
        }
    }

    /// 返回本打开实例的真实流或报文类型。
    pub fn socket_type(&self) -> u32 {
        match &self.endpoint {
            Endpoint::Udp(_) => 2,
            Endpoint::Tcp(_) | Endpoint::Unix(_) => 1,
        }
    }

    /// 返回实际地址域，匿名 pair 保持 AF_UNIX。
    pub fn domain(&self) -> u16 {
        match &self.endpoint {
            Endpoint::Unix(_) => 1,
            Endpoint::Tcp(_) | Endpoint::Udp(_) => 2,
        }
    }

    /// 返回实际协议编号，匿名 UNIX stream 使用默认协议零。
    pub fn protocol(&self) -> i32 {
        match &self.endpoint {
            Endpoint::Tcp(_) => 6,
            Endpoint::Udp(_) => 17,
            Endpoint::Unix(_) => 0,
        }
    }

    /// 每个真实端点拥有独立且不复用的本域身份。
    pub fn id(&self) -> u64 {
        match &self.endpoint {
            Endpoint::Tcp(endpoint) => endpoint.id(),
            Endpoint::Udp(endpoint) => endpoint.id(),
            Endpoint::Unix(endpoint) => endpoint.id(),
        }
    }

    /// 返回构造时的同一网络域；dup 和 fork 共享打开实例及这个引用。
    pub fn net(&self) -> &NetStackRef {
        &self.net
    }

    /// 访问模式固定为双向流，非阻塞状态属于真正的 OFD。
    pub fn flags(&self) -> u32 {
        self.flags.load(Ordering::Acquire)
    }

    /// 沿文件对象已有规则保存可变状态位，descriptor 的 CLOEXEC 不进入 OFD。
    pub fn set_status_flags(&self, raw: u32) -> Result<(), FsError> {
        self.flags
            .store(O_RDWR | (raw & (O_APPEND | O_NONBLOCK)), Ordering::Release);
        Ok(())
    }

    /// 按实际地址域绑定本端点，匿名 UNIX 不提供路径绑定。
    pub fn bind(&self, local: SocketAddress) -> Result<(), FsError> {
        match (&self.endpoint, local) {
            (Endpoint::Tcp(endpoint), SocketAddress::Inet(local)) => {
                endpoint.bind(local).map(|_| ())
            }
            (Endpoint::Udp(endpoint), SocketAddress::Inet(local)) => {
                endpoint.bind(local).map(|_| ())
            }
            (Endpoint::Unix(_), _) => Err(FsError::Unsupported),
            _ => Err(FsError::AddressFamilyNotSupported),
        }
    }

    /// 只有 TCP 能更新稳定 listener 的 backlog，其他端点明确拒绝监听。
    pub fn listen(&self, backlog: usize) -> Result<(), FsError> {
        self.tcp_endpoint()?.listen(backlog)
    }

    /// UDP 更新默认 peer 或断开，TCP 阻塞重启只领取既有握手的实际结果。
    /// 中断或等待资源准备失败保留未领取事实，成功或连接失败才清除。
    pub fn connect(&self, peer: SocketAddress, caller: FileIoContext<'_>) -> Result<(), FsError> {
        let endpoint = match (&self.endpoint, peer) {
            (Endpoint::Udp(endpoint), SocketAddress::Inet(peer)) => {
                return endpoint.connect(Some(peer))
            }
            (Endpoint::Udp(endpoint), SocketAddress::Unspecified) => return endpoint.connect(None),
            (Endpoint::Tcp(endpoint), SocketAddress::Inet(_)) => endpoint,
            (Endpoint::Unix(_), _) => return Err(FsError::Unsupported),
            _ => return Err(FsError::AddressFamilyNotSupported),
        };
        let SocketAddress::Inet(peer) = peer else {
            unreachable!("TCP connection requires an Inet address");
        };
        let nonblock = self.flags() & O_NONBLOCK != 0;
        match endpoint.connect(peer) {
            Ok(()) if nonblock => return Err(FsError::InProgress),
            Ok(()) => self.connect_unclaimed.store(true, Ordering::Release),
            Err(FsError::AlreadyInProgress) if !nonblock => {
                self.connect_unclaimed.store(true, Ordering::Release);
            }
            Err(FsError::AlreadyConnected)
                if !nonblock && self.connect_unclaimed.load(Ordering::Acquire) => {}
            Err(error) => return Err(error),
        }
        let mut waiting: Option<(Arc<PollContext>, PollTable)> = None;
        let mut interrupted = false;
        loop {
            if let Some((context, _)) = &waiting {
                context.clear_triggered();
            }
            match endpoint.connection_result() {
                Ok(Some(())) => {
                    self.connect_unclaimed.store(false, Ordering::Release);
                    return Ok(());
                }
                Err(error) => {
                    self.connect_unclaimed.store(false, Ordering::Release);
                    return Err(error);
                }
                Ok(None) => {}
            }
            if interrupted {
                return Err(FsError::Interrupted);
            }
            if let Some((context, _)) = &waiting {
                interrupted = context.park_until(matches!(caller, FileIoContext::User(_)), None)
                    == WaitResult::Interrupted;
            } else {
                waiting = Some(match self.prepare_wait() {
                    Ok(waiting) => waiting,
                    Err(error) => {
                        return Err(error);
                    }
                });
            }
        }
    }

    /// 从稳定 listener 领取成熟 child，为它准备新 OFD；新状态位只来自 accept4。
    pub fn accept(
        &self,
        flags: u32,
        uid: u32,
        gid: u32,
        caller: FileIoContext<'_>,
    ) -> Result<Arc<SocketFile>, FsError> {
        if flags & !(O_NONBLOCK | O_CLOEXEC) != 0 {
            return Err(FsError::InvalidOperation);
        }
        let endpoint = self.tcp_endpoint()?;
        let mut waiting: Option<(Arc<PollContext>, PollTable)> = None;
        let mut interrupted = false;
        loop {
            if let Some((context, _)) = &waiting {
                context.clear_triggered();
            }
            if let Some(child) = endpoint.accept()? {
                return Self::from_endpoint(self.net.clone(), child, flags, uid, gid);
            }
            if self.flags() & O_NONBLOCK != 0 {
                return Err(FsError::WouldBlock);
            }
            if interrupted {
                return Err(FsError::Interrupted);
            }
            if let Some((context, _)) = &waiting {
                interrupted = context.park_until(matches!(caller, FileIoContext::User(_)), None)
                    == WaitResult::Interrupted;
            } else {
                waiting = Some(self.prepare_wait()?);
            }
        }
    }

    /// 流交付连续前缀，UDP 领取同条消息及元数据；TRUNC 只允许用于 UDP。
    pub fn receive(
        &self,
        target: &mut ReadBuffer<'_>,
        flags: u32,
        caller: FileIoContext<'_>,
    ) -> Result<MessageResult, FsError> {
        let allowed = MSG_DONTWAIT
            | MSG_PEEK
            | if matches!(&self.endpoint, Endpoint::Udp(_)) {
                MSG_TRUNC
            } else {
                0
            };
        if flags & !allowed != 0 {
            return Err(FsError::Unsupported);
        }
        if target.copied() == target.len() && !matches!(&self.endpoint, Endpoint::Udp(_)) {
            return Ok(Self::stream_result(0));
        }
        let mut waiting: Option<(Arc<PollContext>, PollTable)> = None;
        let mut interrupted = false;
        loop {
            if let Some((context, _)) = &waiting {
                context.clear_triggered();
            }
            match self.receive_once(target, flags & MSG_PEEK != 0)? {
                Some(result) => return Ok(result),
                None => {}
            }
            if self.nonblocking(flags) {
                return Err(FsError::WouldBlock);
            }
            if interrupted {
                return Err(FsError::Interrupted);
            }
            if let Some((context, _)) = &waiting {
                interrupted = context.park_until(matches!(caller, FileIoContext::User(_)), None)
                    == WaitResult::Interrupted;
            } else {
                waiting = Some(self.prepare_wait()?);
            }
        }
    }

    /// 单次数据面查询保留流的正交付前缀，UDP fault 不转换成部分消息成功。
    fn receive_once(
        &self,
        target: &mut ReadBuffer<'_>,
        peek: bool,
    ) -> Result<Option<MessageResult>, FsError> {
        let start = target.copied();
        let result = match &self.endpoint {
            Endpoint::Tcp(endpoint) => endpoint.receive(target, peek),
            Endpoint::Unix(endpoint) => endpoint.receive(target, peek),
            Endpoint::Udp(endpoint) => return endpoint.receive(target, peek),
        };
        match result {
            Ok(IoProgress::Ready(count)) => Ok(Some(Self::stream_result(count))),
            Ok(IoProgress::Eof) => Ok(Some(Self::stream_result(0))),
            Ok(IoProgress::Blocked) => Ok(None),
            Err(error) => Self::progress(target.copied() - start, error)
                .map(Self::stream_result)
                .map(Some),
        }
    }

    /// 锁外准备同一流前缀或完整 UDP 消息，返回协议实际接受量。
    pub fn send(
        &self,
        source: &mut WriteBuffer<'_>,
        destination: Option<SocketAddress>,
        flags: u32,
        caller: FileIoContext<'_>,
    ) -> Result<usize, FsError> {
        if flags & !(MSG_DONTWAIT | MSG_NOSIGNAL) != 0 {
            return Err(FsError::Unsupported);
        }
        match &self.endpoint {
            Endpoint::Udp(endpoint) => {
                let destination = match destination {
                    Some(SocketAddress::Inet(peer)) => Some(peer),
                    None => None,
                    _ => return Err(FsError::AddressFamilyNotSupported),
                };
                return self.send_datagram(endpoint, source, destination, flags, caller);
            }
            Endpoint::Unix(_) if destination.is_some() => return Err(FsError::AlreadyConnected),
            Endpoint::Tcp(_) | Endpoint::Unix(_) => {}
        }
        let mut prefix = [0u8; STREAM_PREFIX_LEN];
        let prepared = source.pull(&mut prefix)?;
        let mut waiting: Option<(Arc<PollContext>, PollTable)> = None;
        let mut interrupted = false;
        loop {
            if let Some((context, _)) = &waiting {
                context.clear_triggered();
            }
            // 空前缀仍交给端点核对真实发送关闭，不能在 OFD 层吞掉 EPIPE。
            let result = match &self.endpoint {
                Endpoint::Tcp(endpoint) => {
                    endpoint.send(&mut WriteBuffer::kernel(&prefix[..prepared]))
                }
                Endpoint::Unix(endpoint) => endpoint.send(&prefix[..prepared]),
                Endpoint::Udp(_) => unreachable!("datagram send uses complete packets"),
            };
            match result {
                Ok(IoProgress::Ready(count)) => return Ok(count),
                Ok(IoProgress::Blocked) => {}
                Ok(IoProgress::Eof) => unreachable!("stream send returned receive EOF"),
                Err(FsError::BrokenPipe) => {
                    if flags & MSG_NOSIGNAL == 0 {
                        if let FileIoContext::User(user) = caller {
                            user.send_kernel_signal(13);
                        }
                    }
                    return Err(FsError::BrokenPipe);
                }
                Err(error) => return Err(error),
            }
            if self.nonblocking(flags) {
                return Err(FsError::WouldBlock);
            }
            if interrupted {
                return Err(FsError::Interrupted);
            }
            if let Some((context, _)) = &waiting {
                interrupted = context.park_until(matches!(caller, FileIoContext::User(_)), None)
                    == WaitResult::Interrupted;
            } else {
                waiting = Some(self.prepare_wait()?);
            }
        }
    }

    /// 一次完整 copyin 后保留原包和 Lease，容量等待不重新取用户输入或改变目的。
    fn send_datagram(
        &self,
        endpoint: &Arc<UdpEndpoint>,
        source: &mut WriteBuffer<'_>,
        destination: Option<InetEndpoint>,
        flags: u32,
        caller: FileIoContext<'_>,
    ) -> Result<usize, FsError> {
        let length = source.len() - source.copied();
        if DATAGRAM_PREFIX_LEN < length {
            return Err(FsError::MessageTooLarge);
        }
        let mut prefix = [0u8; DATAGRAM_PREFIX_LEN];
        let prepared = source.pull(&mut prefix[..length])?;
        if prepared < length {
            // WriteBuffer 保留首个 copyin 错误；UDP 不提交已经取得的部分输入。
            source.pull(&mut [])?;
            unreachable!("incomplete datagram copy has no stored error");
        }
        let (mut packet, _lease) = endpoint.prepare_send(&prefix[..length], destination)?;
        let mut waiting: Option<(Arc<PollContext>, PollTable)> = None;
        let mut interrupted = false;
        loop {
            if let Some((context, _)) = &waiting {
                context.clear_triggered();
            }
            endpoint.check_send()?;
            if self.net.egress_ready(endpoint.id())? {
                match self.net.try_emit(packet) {
                    Ok(()) => return Ok(length),
                    Err((FsError::WouldBlock, returned)) => packet = returned,
                    Err((error, _packet)) => return Err(error),
                }
            }
            if self.nonblocking(flags) {
                return Err(FsError::WouldBlock);
            }
            if interrupted {
                return Err(FsError::Interrupted);
            }
            if let Some((context, _)) = &waiting {
                interrupted = context.park_until(matches!(caller, FileIoContext::User(_)), None)
                    == WaitResult::Interrupted;
            } else {
                waiting = Some(self.prepare_wait()?);
            }
        }
    }

    /// 关闭协议实际方向，流发送关闭不妨碍反向接收。
    pub fn shutdown(&self, how: u32) -> Result<(), FsError> {
        match &self.endpoint {
            Endpoint::Tcp(endpoint) => endpoint.shutdown(how),
            Endpoint::Udp(endpoint) => endpoint.shutdown(how),
            Endpoint::Unix(endpoint) => endpoint.shutdown(how),
        }
    }

    /// 查询实际绑定或路由选源，匿名 UNIX 返回无路径名称。
    pub fn local_address(&self) -> Result<SocketAddress, FsError> {
        match &self.endpoint {
            Endpoint::Tcp(endpoint) => endpoint.local_address().map(SocketAddress::Inet),
            Endpoint::Udp(endpoint) => endpoint.local_address().map(SocketAddress::Inet),
            Endpoint::Unix(endpoint) => endpoint.local_address(),
        }
    }

    /// 返回真实连接的 peer 或匿名名称，未连接错误由协议对象保留。
    pub fn peer_address(&self) -> Result<SocketAddress, FsError> {
        match &self.endpoint {
            Endpoint::Tcp(endpoint) => endpoint.peer_address().map(SocketAddress::Inet),
            Endpoint::Udp(endpoint) => endpoint.peer_address().map(SocketAddress::Inet),
            Endpoint::Unix(endpoint) => endpoint.peer_address(),
        }
    }

    /// 一次消费协议记录的实际错误，查询不主动推进收发。
    pub fn take_error(&self) -> Option<FsError> {
        match &self.endpoint {
            Endpoint::Tcp(endpoint) => endpoint.take_error(),
            Endpoint::Udp(endpoint) => endpoint.take_error(),
            Endpoint::Unix(endpoint) => endpoint.take_error(),
        }
    }

    /// 只有真实 TCP listener 才提供接受连接能力。
    pub fn is_listener(&self) -> bool {
        match &self.endpoint {
            Endpoint::Tcp(endpoint) => endpoint.is_listener(),
            Endpoint::Udp(_) | Endpoint::Unix(_) => false,
        }
    }

    /// 返回实际协议接收存储的字节容量，不套用 skb 记账翻倍规则。
    pub fn receive_capacity(&self) -> usize {
        match &self.endpoint {
            Endpoint::Tcp(endpoint) => endpoint.receive_capacity(),
            Endpoint::Udp(endpoint) => endpoint.receive_capacity(),
            Endpoint::Unix(endpoint) => endpoint.receive_capacity(),
        }
    }

    /// 返回实际发送存储容量，UDP 读取本域共享 egress 的完整 IP 字节容量。
    pub fn send_capacity(&self) -> usize {
        match &self.endpoint {
            Endpoint::Tcp(endpoint) => endpoint.send_capacity(),
            Endpoint::Udp(endpoint) => endpoint.send_capacity(),
            Endpoint::Unix(endpoint) => endpoint.send_capacity(),
        }
    }

    /// 查询协议保存的地址复用事实，不改变绑定索引或产生通知。
    pub fn reuse_address(&self) -> bool {
        match &self.endpoint {
            Endpoint::Tcp(endpoint) => endpoint.reuse_address(),
            Endpoint::Udp(endpoint) => endpoint.reuse_address(),
            Endpoint::Unix(endpoint) => endpoint.reuse_address(),
        }
    }

    /// 沿协议端点更新复用策略，绑定表和 listener 约束由同域控制处理。
    pub fn set_reuse_address(&self, reuse: bool) -> Result<(), FsError> {
        match &self.endpoint {
            Endpoint::Tcp(endpoint) => endpoint.set_reuse_address(reuse),
            Endpoint::Udp(endpoint) => endpoint.set_reuse_address(reuse),
            Endpoint::Unix(endpoint) => endpoint.set_reuse_address(reuse),
        }
    }

    /// TCP_NODELAY 只查询 TCP 的实际 Nagle 状态，其他协议明确不支持。
    pub fn nodelay(&self) -> Result<bool, FsError> {
        self.tcp_endpoint()?.nodelay()
    }

    /// 修改 TCP 的实际 Nagle 状态，由原端点安排协议 Work。
    pub fn set_nodelay(&self, enabled: bool) -> Result<(), FsError> {
        self.tcp_endpoint()?.set_nodelay(enabled)
    }

    /// 真实 Blocked 后才准备原等待上下文与一源表，登记结束后由调用者重新扫描。
    fn prepare_wait(&self) -> Result<(Arc<PollContext>, PollTable), FsError> {
        let context = PollContext::try_new()?;
        let sink: Arc<dyn PollNotify> = context.clone();
        let mut table = PollTable::new(sink, 1)?;
        self.poll(Some(&mut table))?;
        Ok((context, table))
    }

    /// 每次扫描共同消费 OFD 状态和本次消息的非阻塞要求。
    fn nonblocking(&self, flags: u32) -> bool {
        self.flags() & O_NONBLOCK != 0 || flags & MSG_DONTWAIT != 0
    }

    /// 流消息元数据只描述本次交付前缀，不构造报文来源。
    fn stream_result(count: usize) -> MessageResult {
        MessageResult {
            copied: count,
            original_len: count,
            source: None,
            flags: 0,
        }
    }

    /// 已交付正前缀优先于后续 fault 或协议错误，零进度保留原错误。
    fn progress(count: usize, error: FsError) -> Result<usize, FsError> {
        if count == 0 {
            Err(error)
        } else {
            Ok(count)
        }
    }

    /// 匿名 socket 属性使用本域稳定身份和创建者凭据，不伪装 regular 或磁盘路径。
    pub fn stat(&self) -> FileStat {
        let zero = InodeTime {
            seconds: 0,
            nanoseconds: 0,
        };
        FileStat {
            inode: self.id(),
            kind: InodeKind::Socket,
            mode: 0o140777,
            size: 0,
            uid: self.uid,
            gid: self.gid,
            links: 1,
            blocks: 0,
            device: 0,
            rdev: 0,
            block_size: self.net.loopback_mtu() as u32,
            atime: zero,
            mtime: zero,
            ctime: zero,
        }
    }
}

impl Pollable for SocketFile {
    /// 首轮登记端点唯一持续源，其后只查询原协议状态，不收包或 dispatch。
    fn poll(&self, table: Option<&mut PollTable>) -> Result<PollMask, FsError> {
        match &self.endpoint {
            Endpoint::Tcp(endpoint) => endpoint.poll(table),
            Endpoint::Udp(endpoint) => endpoint.poll(table),
            Endpoint::Unix(endpoint) => endpoint.poll(table),
        }
    }
}

impl FileLike for SocketFile {
    /// 普通零长 read 不消费报文，非空请求沿同一协议交付和等待路径。
    fn read_into(
        &self,
        target: &mut ReadBuffer<'_>,
        caller: FileIoContext<'_>,
    ) -> Result<usize, FsError> {
        if target.copied() == target.len() {
            return Ok(0);
        }
        self.receive(target, 0, caller).map(|result| result.copied)
    }

    /// TCP 流没有可定位位置，pread 不访问用户数据即返回 ESPIPE。
    fn read_at_into(
        &self,
        _offset: u64,
        _target: &mut ReadBuffer<'_>,
        _caller: FileIoContext<'_>,
    ) -> Result<usize, FsError> {
        Err(FsError::IllegalSeek)
    }

    /// 匿名 socket 不提供目录路径。
    fn path(&self) -> Option<&Path> {
        None
    }

    /// 流数据不提供文件映射 inode。
    fn mapping_inode(&self) -> Option<&InodeRef> {
        None
    }

    /// 事件扫描器直接观察本 OFD 对应的端点源。
    fn pollable(&self) -> Option<&dyn Pollable> {
        Some(self)
    }

    /// 网络控制只取得此真实 SocketFile，不从 stat 类型猜协议对象。
    fn socket(&self) -> Option<&SocketFile> {
        Some(self)
    }

    /// 复制描述符共享同一反向 watch 和末 OFD 注销责任。
    fn watch_head(&self) -> Option<&Arc<WatchHead>> {
        Some(&self.watches)
    }

    /// 读取共享 OFD 状态，CLOEXEC 始终归描述符槽。
    fn flags(&self) -> u32 {
        SocketFile::flags(self)
    }

    /// F_SETFL 修改原 OFD 状态，不改变双向访问模式。
    fn set_status_flags(&self, raw: u32) -> Result<(), FsError> {
        SocketFile::set_status_flags(self, raw)
    }

    /// 网络流没有文件长度可截断。
    fn truncate(&self, _new_size: u64) -> Result<(), FsError> {
        Err(FsError::InvalidOperation)
    }

    /// 网络流没有文件存储同步范围。
    fn sync_range(&self, _range: Range<u64>, _data_only: bool) -> Result<(), FsError> {
        Err(FsError::InvalidOperation)
    }

    /// 流没有共享字节位置，不创建虚假游标。
    fn seek(&self, _from: SeekFrom) -> Result<u64, FsError> {
        Err(FsError::IllegalSeek)
    }

    /// fstat 使用真实创建者、本域稳定身份及 S_IFSOCK0777。
    fn stat(&self) -> Result<FileStat, FsError> {
        Ok(SocketFile::stat(self))
    }

    /// 匿名 socket 不能读取目录项。
    fn readdir(&self) -> Result<Option<DirectoryEntry>, FsError> {
        Err(FsError::NotDirectory)
    }

    /// 匿名 socket 不授予目录枚举能力。
    fn for_each_dirent(
        &self,
        _emit: &mut dyn FnMut(&DirectoryEntry) -> Result<bool, FsError>,
    ) -> Result<(), FsError> {
        Err(FsError::NotDirectory)
    }

    /// 普通 write/writev 使用默认 peer，UDP 空输入仍提交真实零长消息。
    fn write_from(
        &self,
        source: &mut WriteBuffer<'_>,
        caller: FileIoContext<'_>,
    ) -> Result<usize, FsError> {
        self.send(source, None, 0, caller)
    }

    /// TCP 流不能执行 pwrite。
    fn write_at_from(
        &self,
        _offset: u64,
        _source: &mut WriteBuffer<'_>,
        _caller: FileIoContext<'_>,
    ) -> Result<usize, FsError> {
        Err(FsError::IllegalSeek)
    }
}

impl Drop for SocketFile {
    /// 末打开实例先同步撤反向 watch，再请求正常协议关闭，不等待 peer 或后台 Work。
    fn drop(&mut self) {
        self.watches.release();
        if self.opened {
            self.endpoint.close();
        }
    }
}
