//! UDP 内核端点持有完整报文记录，索引与接收容量均属于同一网络域。

use alloc::collections::VecDeque;
use alloc::sync::{Arc, Weak};

use crate::fs::FsError;
use crate::net::address::InetEndpoint;
use crate::net::packet::OwnedPacket;
use crate::net::table::{BindLease, TransportProtocol};
use crate::net::{EndpointSlot, NetStack, NetStackRef, LOOPBACK_MTU};
use crate::sync::{SleepMutex, SpinLock};

const INBOX_RECORDS: usize = 32;
const MAX_PAYLOAD: usize = LOOPBACK_MTU - 28;
const INBOX_BYTES: usize = INBOX_RECORDS * MAX_PAYLOAD;

struct Inbox {
    packets: VecDeque<OwnedPacket>,
    payload_bytes: usize,
    binding: Option<Arc<BindLease>>,
    closed: bool,
}

/// 一个记录同时拥有数据、源地址和原始长度；弱域引用不延长服务寿命。
pub struct UdpEndpoint {
    id: u64,
    stack: Weak<NetStack>,
    inbox: SpinLock<Inbox>,
    bind_serial: SleepMutex<()>,
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
                closed: false,
            }),
            bind_serial: SleepMutex::new(()),
            _slot: slot,
        })
        .map_err(|_| FsError::NoMemory)
    }

    /// 返回域内不复用的端点编号，关闭后外部引用仍保有这个身份。
    pub fn id(&self) -> u64 {
        self.id
    }

    /// 普通任务先串行绑定并取得根准入，再短锁发表索引；关闭与发表使用同一接收状态锁。
    pub fn bind(self: &Arc<Self>, requested: InetEndpoint) -> Result<InetEndpoint, FsError> {
        let _serial = self.bind_serial.lock();
        let net = self.stack.upgrade().ok_or(FsError::NotConnected)?;
        let _admission = net.admission()?;
        {
            let inbox = self.inbox.lock();
            if inbox.closed {
                return Err(FsError::NotConnected);
            }
            if inbox.binding.is_some() {
                return Err(FsError::InvalidOperation);
            }
        }
        let lease =
            net.tables()
                .reserve_binding(&net, self.id, TransportProtocol::Udp, requested)?;
        let local = lease.local();
        let published = {
            let mut inbox = self.inbox.lock();
            if inbox.closed {
                Err(FsError::NotConnected)
            } else {
                net.tables().publish_udp(self.id, local).map(|()| {
                    inbox.binding = Some(Arc::clone(&lease));
                })
            }
        };
        // 最后 Lease 析构会撤绑定，必须在接收短锁之外进行。
        published?;
        Ok(local)
    }

    /// 只观察真正绑定的值，通配地址保持全零，不安排后台执行。
    pub fn local(&self) -> Option<InetEndpoint> {
        self.inbox
            .lock()
            .binding
            .as_ref()
            .map(|lease| lease.local())
    }

    /// 移入已验证的完整 UDP 包；零长负载仍占一条记录，满时不接受该包。
    pub fn enqueue(&self, packet: OwnedPacket) -> Result<(), FsError> {
        let payload_len = packet.udp_payload()?.len();
        if MAX_PAYLOAD < payload_len {
            return Err(FsError::MessageTooLarge);
        }
        let mut inbox = self.inbox.lock();
        if inbox.closed {
            return Err(FsError::NotConnected);
        }
        if INBOX_RECORDS <= inbox.packets.len() || INBOX_BYTES - inbox.payload_bytes < payload_len {
            return Err(FsError::WouldBlock);
        }
        inbox.payload_bytes += payload_len;
        inbox.packets.push_back(packet);
        Ok(())
    }

    /// 领取同一拥有记录并归还它的实际字节预算，源地址和长度继续由该记录提供。
    pub fn take_datagram(&self) -> Option<OwnedPacket> {
        let mut inbox = self.inbox.lock();
        let packet = inbox.packets.pop_front()?;
        inbox.payload_bytes -= packet
            .udp_payload()
            .expect("UDP inbox protocol invariant")
            .len();
        Some(packet)
    }

    /// 先拒绝新包并移出记录与 Lease，再锁外移除表拥有及释放实际存储。
    pub fn close(&self) {
        self.stop_sync();
        if let Some(net) = self.stack.upgrade() {
            let endpoint = net.tables().take_udp(self.id);
            drop(endpoint);
        }
    }

    /// 根已移出表拥有后关闭接收责任；UDP 没有另一个异步回调需要等待。
    pub(crate) fn stop_sync(&self) {
        let (packets, lease) = {
            let mut inbox = self.inbox.lock();
            inbox.closed = true;
            inbox.payload_bytes = 0;
            (core::mem::take(&mut inbox.packets), inbox.binding.take())
        };
        drop(packets);
        drop(lease);
    }
}
