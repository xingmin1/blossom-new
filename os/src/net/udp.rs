//! UDP 内核端点持有完整报文记录，索引与接收容量均属于同一网络域。

use alloc::collections::VecDeque;
use alloc::sync::{Arc, Weak};

use crate::fs::poll::{PollMask, PollTable, PollWaitHead};
use crate::fs::{FsError, ReadBuffer};
use crate::net::address::{InetEndpoint, SocketAddress};
use crate::net::packet::OwnedPacket;
use crate::net::table::{BindLease, TransportProtocol};
use crate::net::{
    EndpointSlot, MessageResult, NetStack, NetStackRef, LOOPBACK_CAPACITY, LOOPBACK_MTU,
};
use crate::sync::{SleepMutex, SpinLock};

const INBOX_RECORDS: usize = 32;
const MAX_PAYLOAD: usize = LOOPBACK_MTU - 28;
const INBOX_BYTES: usize = INBOX_RECORDS * MAX_PAYLOAD;
const MSG_TRUNC: u32 = 0x20;

struct Inbox {
    packets: VecDeque<OwnedPacket>,
    payload_bytes: usize,
    binding: Option<Arc<BindLease>>,
    peer: Option<InetEndpoint>,
    route_local: Option<InetEndpoint>,
    explicit_bind: bool,
    reuse: bool,
    read_closed: bool,
    write_closed: bool,
    closed: bool,
}

/// 一个记录同时拥有数据、源地址和原始长度；弱域引用不延长服务寿命。
pub struct UdpEndpoint {
    id: u64,
    stack: Weak<NetStack>,
    inbox: SpinLock<Inbox>,
    bind_serial: SleepMutex<()>,
    receive_serial: SleepMutex<()>,
    source: Arc<PollWaitHead>,
    _slot: EndpointSlot,
}

impl UdpEndpoint {
    /// 域先取得真实节点额度，再在发表之前准备全部记录槽位。
    pub(crate) fn try_new(
        net: &NetStackRef,
        id: u64,
        slot: EndpointSlot,
    ) -> Result<Arc<Self>, FsError> {
        let mut packets = VecDeque::new();
        packets
            .try_reserve_exact(INBOX_RECORDS)
            .map_err(|_| FsError::NoMemory)?;
        Arc::try_new(Self {
            id,
            stack: Arc::downgrade(net),
            inbox: SpinLock::new(Inbox {
                packets,
                payload_bytes: 0,
                binding: None,
                peer: None,
                route_local: None,
                explicit_bind: false,
                reuse: false,
                read_closed: false,
                write_closed: false,
                closed: false,
            }),
            bind_serial: SleepMutex::new(()),
            receive_serial: SleepMutex::new(()),
            source: Arc::try_new(PollWaitHead::new()).map_err(|_| FsError::NoMemory)?,
            _slot: slot,
        })
        .map_err(|_| FsError::NoMemory)
    }

    /// 返回域内不复用的端点编号，关闭后外部引用仍保有这个身份。
    pub fn id(&self) -> u64 {
        self.id
    }

    /// 查询本端发送关门事实，已准备或等待容量的消息不得绕过后来的写关闭。
    pub fn check_send(&self) -> Result<(), FsError> {
        let inbox = self.inbox.lock();
        if inbox.closed {
            return Err(FsError::NotConnected);
        }
        if inbox.write_closed {
            return Err(FsError::BrokenPipe);
        }
        Ok(())
    }

    /// 普通任务串行绑定并取得根准入，显式通配地址保持原值直到实际连接选定路由源。
    pub fn bind(self: &Arc<Self>, requested: InetEndpoint) -> Result<InetEndpoint, FsError> {
        let _serial = self.bind_serial.lock();
        let net = self.stack.upgrade().ok_or(FsError::NotConnected)?;
        let _admission = net.admission()?;
        let reuse = {
            let inbox = self.inbox.lock();
            if inbox.closed {
                return Err(FsError::NotConnected);
            }
            if inbox.binding.is_some() {
                return Err(FsError::InvalidOperation);
            }
            inbox.reuse
        };
        let lease = net.tables().reserve_binding(
            &net,
            self.id,
            TransportProtocol::Udp,
            requested,
            reuse,
        )?;
        let local = lease.local();
        {
            let mut inbox = self.inbox.lock();
            if inbox.closed {
                return Err(FsError::NotConnected);
            }
            net.tables().publish_udp(self.id, local, None)?;
            inbox.binding = Some(Arc::clone(&lease));
            inbox.explicit_bind = true;
        }
        Ok(local)
    }

    /// 默认目的与本次发送目的独立；断开自动绑定时只撤索引并放下端点自己的 Lease。
    pub fn connect(self: &Arc<Self>, peer: Option<InetEndpoint>) -> Result<(), FsError> {
        let _serial = self.bind_serial.lock();
        let net = self.stack.upgrade().ok_or(FsError::NotConnected)?;
        let _admission = net.admission()?;
        let Some(peer) = peer else {
            let released = {
                let mut inbox = self.inbox.lock();
                if inbox.closed {
                    return Err(FsError::NotConnected);
                }
                if inbox.explicit_bind {
                    let local = inbox
                        .binding
                        .as_ref()
                        .expect("explicit UDP binding")
                        .local();
                    net.tables().publish_udp(self.id, local, None)?;
                    inbox.peer = None;
                    inbox.route_local = None;
                    None
                } else {
                    net.tables().unpublish_udp(self.id);
                    inbox.peer = None;
                    inbox.route_local = None;
                    inbox.binding.take()
                }
            };
            // 准备中的发送另持 Lease；端口要到最后一份真实拥有结束才归还。
            drop(released);
            return Ok(());
        };
        if peer.port == 0 {
            return Err(FsError::InvalidOperation);
        }
        let (binding, reuse) = {
            let inbox = self.inbox.lock();
            if inbox.closed {
                return Err(FsError::NotConnected);
            }
            (inbox.binding.clone(), inbox.reuse)
        };
        let bound_source = binding
            .as_ref()
            .map_or([0; 4], |lease| lease.local().address);
        let route = net.routes().resolve(peer.address, bound_source)?;
        let lease = match binding {
            Some(lease) => lease,
            None => net.tables().reserve_binding(
                &net,
                self.id,
                TransportProtocol::Udp,
                InetEndpoint {
                    address: [0; 4],
                    port: 0,
                },
                reuse,
            )?,
        };
        let local = InetEndpoint {
            address: route.source,
            port: lease.local().port,
        };
        {
            let mut inbox = self.inbox.lock();
            if inbox.closed {
                return Err(FsError::NotConnected);
            }
            net.tables().publish_udp(self.id, local, Some(peer))?;
            inbox.binding = Some(Arc::clone(&lease));
            inbox.route_local = Some(local);
            inbox.peer = Some(peer);
        }
        Ok(())
    }

    /// 准备完整消息及它的端口拥有，阻塞发送保留同一包和 Lease，不重新选择目的。
    pub fn prepare_send(
        self: &Arc<Self>,
        payload: &[u8],
        destination: Option<InetEndpoint>,
    ) -> Result<(OwnedPacket, Arc<BindLease>), FsError> {
        let _serial = self.bind_serial.lock();
        let net = self.stack.upgrade().ok_or(FsError::NotConnected)?;
        let _admission = net.admission()?;
        let (destination, binding, reuse) = {
            let inbox = self.inbox.lock();
            if inbox.closed {
                return Err(FsError::NotConnected);
            }
            if inbox.write_closed {
                return Err(FsError::BrokenPipe);
            }
            let destination = destination
                .or(inbox.peer)
                .ok_or(FsError::DestinationRequired)?;
            (destination, inbox.binding.clone(), inbox.reuse)
        };
        if destination.port == 0 {
            return Err(FsError::InvalidOperation);
        }
        let bound_source = binding
            .as_ref()
            .map_or([0; 4], |lease| lease.local().address);
        let route = net.routes().resolve(destination.address, bound_source)?;
        if route.mtu.saturating_sub(28) < payload.len() {
            return Err(FsError::MessageTooLarge);
        }
        let lease = match binding {
            Some(lease) => lease,
            None => {
                let lease = net.tables().reserve_binding(
                    &net,
                    self.id,
                    TransportProtocol::Udp,
                    InetEndpoint {
                        address: [0; 4],
                        port: 0,
                    },
                    reuse,
                )?;
                {
                    let mut inbox = self.inbox.lock();
                    if inbox.closed {
                        return Err(FsError::NotConnected);
                    }
                    if inbox.write_closed {
                        return Err(FsError::BrokenPipe);
                    }
                    net.tables()
                        .publish_udp(self.id, lease.local(), inbox.peer)?;
                    inbox.binding = Some(Arc::clone(&lease));
                }
                lease
            }
        };
        let local = InetEndpoint {
            address: route.source,
            port: lease.local().port,
        };
        let packet = OwnedPacket::try_from_udp(local, destination, payload, route.mtu)?;
        self.check_send()?;
        Ok((packet, lease))
    }

    /// 只观察真正绑定的值，通配地址保持全零，不安排后台执行。
    pub fn local(&self) -> Option<InetEndpoint> {
        self.inbox
            .lock()
            .binding
            .as_ref()
            .map(|lease| lease.local())
    }

    /// 新报文只取接收短锁；旧索引查找到的包仍须按当前默认 peer 过滤。
    pub fn enqueue(&self, packet: OwnedPacket) -> Result<(), FsError> {
        let payload_len = packet.udp_payload()?.len();
        let (peer, _) = packet.udp_endpoints()?;
        if MAX_PAYLOAD < payload_len {
            return Err(FsError::MessageTooLarge);
        }
        {
            let mut inbox = self.inbox.lock();
            if inbox.closed || inbox.read_closed {
                return Err(FsError::NotConnected);
            }
            if inbox.peer.is_some_and(|expected| expected != peer) {
                return Ok(());
            }
            if INBOX_RECORDS <= inbox.packets.len()
                || INBOX_BYTES - inbox.payload_bytes < payload_len
            {
                return Err(FsError::WouldBlock);
            }
            inbox.payload_bytes += payload_len;
            inbox.packets.push_back(packet);
        }
        self.source.notify(PollMask::POLLIN | PollMask::POLLRDNORM);
        Ok(())
    }

    /// 接收序列化覆盖窥视、复制与消费，短锁外的 fault 不会把报文改成成功前缀。
    pub fn receive(
        &self,
        target: &mut ReadBuffer<'_>,
        peek: bool,
    ) -> Result<Option<MessageResult>, FsError> {
        let _serial = self.receive_serial.lock();
        if peek {
            let mut prefix = [0u8; MAX_PAYLOAD];
            let snapshot = {
                let inbox = self.inbox.lock();
                let Some(packet) = inbox.packets.front() else {
                    return Self::empty_receive(&inbox);
                };
                let payload = packet.udp_payload()?;
                let original_len = payload.len();
                let count = original_len.min(target.len() - target.copied());
                prefix[..count].copy_from_slice(&payload[..count]);
                (count, original_len, packet.udp_endpoints()?.0)
            };
            let (count, original_len, source) = snapshot;
            return Self::copy_datagram(target, &prefix[..count], original_len, source).map(Some);
        }
        let packet = {
            let mut inbox = self.inbox.lock();
            let Some(packet) = Self::pop_datagram(&mut inbox) else {
                return Self::empty_receive(&inbox);
            };
            packet
        };
        let payload = packet.udp_payload()?;
        Self::copy_datagram(target, payload, payload.len(), packet.udp_endpoints()?.0).map(Some)
    }

    /// 空消息保有真实 source，读关闭后的空 EOF 没有 source，未关闭空队列才是等待。
    fn empty_receive(inbox: &Inbox) -> Result<Option<MessageResult>, FsError> {
        if inbox.closed {
            return Err(FsError::NotConnected);
        }
        Ok(inbox.read_closed.then_some(MessageResult {
            copied: 0,
            original_len: 0,
            source: None,
            flags: 0,
        }))
    }

    /// 整条复制的错误直接传播，即使目标已得到正前缀也不得报告 UDP 成功。
    fn copy_datagram(
        target: &mut ReadBuffer<'_>,
        payload: &[u8],
        original_len: usize,
        source: InetEndpoint,
    ) -> Result<MessageResult, FsError> {
        let copied = target.push(payload)?;
        Ok(MessageResult {
            copied,
            original_len,
            source: Some(SocketAddress::Inet(source)),
            flags: if copied < original_len { MSG_TRUNC } else { 0 },
        })
    }

    /// 只在接收短锁内移动完整记录并归还预算，不通知新的读事件。
    fn pop_datagram(inbox: &mut Inbox) -> Option<OwnedPacket> {
        let packet = inbox.packets.pop_front()?;
        inbox.payload_bytes -= packet
            .udp_payload()
            .expect("UDP inbox protocol invariant")
            .len();
        Some(packet)
    }

    /// 原内核消费者领取同一拥有记录，不能在另一接收者窥视期间弹出队头。
    pub fn take_datagram(&self) -> Option<OwnedPacket> {
        let _serial = self.receive_serial.lock();
        Self::pop_datagram(&mut self.inbox.lock())
    }

    /// 同一短锁更新已有绑定复用策略；未绑定只记录将来实际预留采用的值。
    pub fn set_reuse_address(&self, reuse: bool) -> Result<(), FsError> {
        let _serial = self.bind_serial.lock();
        let net = self.stack.upgrade().ok_or(FsError::NotConnected)?;
        let _admission = net.admission()?;
        let mut inbox = self.inbox.lock();
        if inbox.closed {
            return Err(FsError::NotConnected);
        }
        if let Some(lease) = inbox.binding.as_ref() {
            net.tables().set_reuse(lease.id(), reuse)?;
        }
        inbox.reuse = reuse;
        Ok(())
    }

    /// 查询本端点的真实复用策略，不改变绑定或协议索引。
    pub fn reuse_address(&self) -> bool {
        self.inbox.lock().reuse
    }

    /// 按弱域的控制块身份判断归属，不因临时升级延长根停止责任。
    pub fn belongs_to(&self, net: &NetStackRef) -> bool {
        Weak::ptr_eq(&self.stack, &Arc::downgrade(net))
    }

    /// 连接名称采用真实路由源，未连接名称保持显式或自动通配绑定的原地址。
    pub fn local_address(&self) -> Result<InetEndpoint, FsError> {
        let inbox = self.inbox.lock();
        if inbox.closed {
            return Err(FsError::NotConnected);
        }
        Ok(inbox
            .route_local
            .or_else(|| inbox.binding.as_ref().map(|lease| lease.local()))
            .unwrap_or(InetEndpoint {
                address: [0; 4],
                port: 0,
            }))
    }

    /// 只查询默认目的；sendto 的独立目的从不成为此名称。
    pub fn peer_address(&self) -> Result<InetEndpoint, FsError> {
        let inbox = self.inbox.lock();
        if inbox.closed {
            return Err(FsError::NotConnected);
        }
        inbox.peer.ok_or(FsError::NotConnected)
    }

    /// 返回原 inbox 实际 payload 预算，零长包仍独立占用记录槽。
    pub fn receive_capacity(&self) -> usize {
        INBOX_BYTES
    }

    /// 返回本域共享软件 egress 的完整 IP 字节容量，不虚构端点独占发送环。
    pub fn send_capacity(&self) -> usize {
        LOOPBACK_CAPACITY * LOOPBACK_MTU
    }

    /// 当前 UDP 没有记录 ICMP 错误的生产路径，查询不合成错误或消费通知。
    pub fn take_error(&self) -> Option<FsError> {
        None
    }

    /// 未连接也提交真实方向关闭再报错，已有报文仍先于读 EOF 交付。
    pub fn shutdown(&self, how: u32) -> Result<(), FsError> {
        if 2 < how {
            return Err(FsError::InvalidOperation);
        }
        let (connected, notification) = {
            let mut inbox = self.inbox.lock();
            if inbox.closed {
                return Err(FsError::NotConnected);
            }
            let mut notification = PollMask::default();
            if (how == 0 || how == 2) && !inbox.read_closed {
                inbox.read_closed = true;
                notification |= PollMask::POLLIN | PollMask::POLLRDNORM | PollMask::POLLRDHUP;
            }
            if (how == 1 || how == 2) && !inbox.write_closed {
                inbox.write_closed = true;
                notification |= PollMask::POLLOUT | PollMask::POLLWRNORM;
            }
            if !notification.is_empty() && inbox.read_closed && inbox.write_closed {
                notification |= PollMask::POLLHUP;
            }
            (inbox.peer.is_some(), notification)
        };
        if !notification.is_empty() {
            self.source.notify(notification);
        }
        if connected {
            Ok(())
        } else {
            Err(FsError::NotConnected)
        }
    }

    /// 登记唯一源后查询真实接收事实与同锁登记的共享 egress 容量，不驱动协议。
    pub fn poll(&self, table: Option<&mut PollTable>) -> Result<PollMask, FsError> {
        if let Some(table) = table {
            table.register(&self.source)?;
        }
        let (mut mask, can_send) = {
            let inbox = self.inbox.lock();
            if inbox.closed {
                return Ok(PollMask::POLLERR | PollMask::POLLHUP);
            }
            let mut mask = PollMask::default();
            if !inbox.packets.is_empty() || inbox.read_closed {
                mask |= PollMask::POLLIN | PollMask::POLLRDNORM;
            }
            if inbox.read_closed {
                mask |= PollMask::POLLRDHUP;
            }
            if inbox.write_closed {
                mask |= PollMask::POLLOUT | PollMask::POLLWRNORM;
            }
            if inbox.read_closed && inbox.write_closed {
                mask |= PollMask::POLLHUP;
            }
            (mask, !inbox.write_closed)
        };
        if can_send {
            let Some(net) = self.stack.upgrade() else {
                return Ok(mask | PollMask::POLLERR | PollMask::POLLHUP);
            };
            // 与关闭共用 Inbox 短锁，撤等待 id 后不能再发表这个端点的容量等待。
            let inbox = self.inbox.lock();
            if inbox.closed {
                return Ok(mask | PollMask::POLLERR | PollMask::POLLHUP);
            }
            if inbox.write_closed {
                return Ok(mask | PollMask::POLLOUT | PollMask::POLLWRNORM);
            }
            match net.egress_ready(self.id) {
                Ok(true) => mask |= PollMask::POLLOUT | PollMask::POLLWRNORM,
                Ok(false) => {}
                Err(FsError::NotConnected) => mask |= PollMask::POLLERR | PollMask::POLLHUP,
                Err(error) => return Err(error),
            }
        }
        Ok(mask)
    }

    /// 容量恢复只重查本端发送事实和实际 egress，已满时由同一入口重新登记。
    pub fn notify_egress(&self) {
        let Some(net) = self.stack.upgrade() else {
            return;
        };
        let can_send = {
            let inbox = self.inbox.lock();
            !inbox.closed && !inbox.write_closed && matches!(net.egress_ready(self.id), Ok(true))
        };
        if can_send {
            self.source.notify(PollMask::POLLOUT | PollMask::POLLWRNORM);
        }
    }

    /// 结束接收与本身份容量等待，再锁外撤表拥有；外部引用继续覆盖端点额度。
    pub fn close(&self) {
        self.stop_sync();
        if let Some(net) = self.stack.upgrade() {
            let endpoint = net.tables().take_udp(self.id);
            drop(endpoint);
        }
    }

    /// 根停止与末 OFD 同接收序列化，移出的报文和 Lease 全部在短锁外释放。
    pub(crate) fn stop_sync(&self) {
        let _serial = self.receive_serial.lock();
        let (packets, lease, changed) = {
            let mut inbox = self.inbox.lock();
            let changed = !inbox.closed;
            inbox.closed = true;
            inbox.read_closed = true;
            inbox.write_closed = true;
            inbox.peer = None;
            inbox.route_local = None;
            inbox.payload_bytes = 0;
            (
                core::mem::take(&mut inbox.packets),
                inbox.binding.take(),
                changed,
            )
        };
        if let Some(net) = self.stack.upgrade() {
            net.cancel_egress_wait(self.id);
            net.tables().unpublish_udp(self.id);
        }
        if changed {
            self.source.notify(PollMask::POLLERR | PollMask::POLLHUP);
        }
        drop(packets);
        drop(lease);
    }
}
