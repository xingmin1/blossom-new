//! 网络域拥有本地报文、索引和后台执行；唯一服务拥有者承担同步停止。

pub mod address;
pub mod core;
pub mod packet;
pub mod route;
pub mod socket;
pub mod table;
pub mod tcp;
pub mod udp;
pub mod unix;

use ::core::pin::Pin;
use ::core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use alloc::collections::VecDeque;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
#[cfg(feature = "ktest")]
use smoltcp::wire::TcpControl;
use smoltcp::wire::{IpProtocol, Ipv4Address, Ipv4Cidr};
use spin::Once;

use crate::cpu;
use crate::fs::FsError;
use crate::net::address::SocketAddress;
use crate::net::core::TcpCore;
use crate::net::packet::OwnedPacket;
use crate::net::route::RouteTable;
use crate::net::table::{NetTables, TcpTuple};
use crate::net::tcp::TcpEndpoint;
use crate::net::udp::UdpEndpoint;
use crate::net::unix::{UnixEndpoint, UnixPair};
use crate::sync::{SleepMutex, SleepMutexGuard, SpinLock};
use crate::time;
use crate::workqueue::{Work, WorkQueue, WorkQueueConfig};

pub const LOOPBACK_CAPACITY: usize = 32;
pub const LOOPBACK_MTU: usize = 1500;
pub(crate) const ENDPOINT_CAPACITY: usize = 128;
pub(crate) const PROTOCOL_BUDGET: usize = 16;

/// Runtime、打开对象和系统拥有者持同一域，不按进程复制网络状态。
pub type NetStackRef = Arc<NetStack>;

/// 流进度、等待和 EOF 分开；Ready(0) 专用于真正的零长操作。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum IoProgress {
    Ready(usize),
    Blocked,
    Eof,
}

/// 报文长度和 source 属于同一次接收，截断后的 copied 不冒充完整消息长度。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MessageResult {
    pub copied: usize,
    pub original_len: usize,
    pub source: Option<SocketAddress>,
    pub flags: u32,
}

/// 只能由可睡普通任务持有，最后一次析构等待本域全部回调尾部。
pub struct NetStackOwner {
    net: NetStackRef,
}

impl NetStackOwner {
    /// 借用服务的同一引用，Runtime 和文件可克隆此引用但不取得停止责任。
    pub fn stack(&self) -> &NetStackRef {
        &self.net
    }
}

impl Drop for NetStackOwner {
    /// 服务拥有者在普通任务作用域结束时关闭一次准入并等待全部后台尾部。
    fn drop(&mut self) {
        self.net.stop_sync();
    }
}

/// 额度覆盖准备中、表中、退休及外部引用；最后节点释放才归还。
pub struct EndpointSlot {
    net: Weak<NetStack>,
}

impl Drop for EndpointSlot {
    /// 弱域不形成拥有环；域已销毁时不再存在可复用的本域额度。
    fn drop(&mut self) {
        if let Some(net) = self.net.upgrade() {
            let previous = net.endpoints.fetch_sub(1, Ordering::AcqRel);
            assert!(0 < previous, "network endpoint slot underflow");
        }
    }
}

/// 队列容量和等待登记使用同一短锁，关闭也在这把锁内拒绝新的包。
struct Egress {
    open: bool,
    packets: VecDeque<OwnedPacket>,
    waiting: Vec<u64>,
    #[cfg(feature = "ktest")]
    packet_control: Option<TestPacketControl>,
}

/// 只记录所选实际普通段的丢失及同 seq 再到达，时间来自生产 monotonic 域。
#[cfg(feature = "ktest")]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TestLossObservation {
    pub dropped: usize,
    pub sequence: Option<i32>,
    pub retransmissions: usize,
    pub first_ns: u64,
    pub repeated_ns: u64,
}

/// 控制只作用于本域 lo 交付窗口，held 保存原包而不合成握手应答。
#[cfg(feature = "ktest")]
enum TestPacketControl {
    HoldSyn {
        destination_port: u16,
        held: Option<OwnedPacket>,
        released: bool,
    },
    DropData {
        source_port: u16,
        destination_port: u16,
        observation: TestLossObservation,
    },
    DropHandshakeAck {
        source_port: u16,
        destination_port: u16,
        dropped: bool,
    },
}

/// 控制发表与停止串行，逐连接协议处理不持这把控制互斥。
pub struct NetStack {
    tables: NetTables,
    routes: RouteTable,
    admission: SleepMutex<()>,
    egress: SpinLock<Egress>,
    retired: SpinLock<Vec<Arc<TcpEndpoint>>>,
    queue: Once<WorkQueue>,
    ingress: Once<Pin<Arc<Work>>>,
    reaper: Once<Pin<Arc<Work>>>,
    stopping: AtomicBool,
    stopped: AtomicBool,
    endpoints: AtomicUsize,
    next_connection: AtomicU64,
    next_protocol_cpu: AtomicUsize,
    seed: u64,
}

impl NetStack {
    /// 在普通任务中准备全部队列和稳定回调上下文，失败关闭未接受票的执行域。
    pub fn try_new_lo() -> Result<NetStackOwner, FsError> {
        let mut packets = VecDeque::new();
        packets
            .try_reserve_exact(LOOPBACK_CAPACITY)
            .map_err(|_| FsError::NoMemory)?;
        let mut waiting = Vec::new();
        waiting
            .try_reserve_exact(ENDPOINT_CAPACITY)
            .map_err(|_| FsError::NoMemory)?;
        let mut retired = Vec::new();
        retired
            .try_reserve_exact(ENDPOINT_CAPACITY)
            .map_err(|_| FsError::NoMemory)?;
        let net = Arc::try_new(Self {
            tables: NetTables::try_new()?,
            routes: RouteTable::new_loopback(),
            admission: SleepMutex::new(()),
            egress: SpinLock::new(Egress {
                open: true,
                packets,
                waiting,
                #[cfg(feature = "ktest")]
                packet_control: None,
            }),
            retired: SpinLock::new(retired),
            queue: Once::new(),
            ingress: Once::new(),
            reaper: Once::new(),
            stopping: AtomicBool::new(false),
            stopped: AtomicBool::new(true),
            endpoints: AtomicUsize::new(0),
            next_connection: AtomicU64::new(1),
            next_protocol_cpu: AtomicUsize::new(0),
            seed: time::now().as_nanos(),
        })
        .map_err(|_| FsError::NoMemory)?;
        let queue = WorkQueue::try_new(WorkQueueConfig {
            max_active: cpu::count() + 2,
            ordered: false,
            reclaim: false,
        })
        .map_err(|_| FsError::NoMemory)?;
        net.queue.call_once(|| queue);
        if let Err(error) = net.prepare_workers() {
            net.work_queue().close_sync();
            return Err(error);
        }
        net.stopped.store(false, Ordering::Release);
        Ok(NetStackOwner { net })
    }

    /// 此时唯一构造引用保活上下文，两个 Work 完全准备后才可被外部安排。
    fn prepare_workers(self: &NetStackRef) -> Result<(), FsError> {
        let context = Arc::as_ptr(self) as usize;
        let ingress = Work::try_new(self.work_queue(), 0, run_ingress, context)
            .map_err(|_| FsError::NoMemory)?;
        let reaper = Work::try_new(self.work_queue(), 0, run_retired, context)
            .map_err(|_| FsError::NoMemory)?;
        self.ingress.call_once(|| ingress);
        self.reaper.call_once(|| reaper);
        Ok(())
    }

    /// 控制操作先取得串行准入，再检查关门状态；不跨协议收发持有。
    pub(crate) fn admission(&self) -> Result<SleepMutexGuard<'_, ()>, FsError> {
        let guard = self.admission.lock();
        if self.is_stopping() {
            return Err(FsError::NotConnected);
        }
        Ok(guard)
    }

    /// 返回本域索引，表短锁仅发表或转移真正拥有。
    pub(crate) fn tables(&self) -> &NetTables {
        &self.tables
    }

    /// 返回本域实际路由配置，不从目的地址伪造接口。
    pub(crate) fn routes(&self) -> &RouteTable {
        &self.routes
    }

    /// 所有连接和域 Work 共用这份执行域及原 per-CPU pools。
    pub(crate) fn work_queue(&self) -> &WorkQueue {
        self.queue.get().expect("network queue is not prepared")
    }

    /// 连接编号不回绕，失败准备也不重新使用已经分配的身份。
    pub(crate) fn allocate_identity(&self) -> u64 {
        self.next_connection
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
            .expect("network connection identity exhausted")
    }

    /// TCP 工作与期限按节点准备次序轮转，调度位置独立于端点和租约身份。
    pub(crate) fn allocate_protocol_cpu(&self) -> usize {
        self.next_protocol_cpu.fetch_add(1, Ordering::Relaxed) % cpu::count()
    }

    /// 关门后所有外部生产和期限重排都停止。
    pub(crate) fn is_stopping(&self) -> bool {
        self.stopping.load(Ordering::Acquire)
    }

    /// 取得实际节点额度，准备失败由这个值本身归还。
    pub fn claim_endpoint_slot(self: &NetStackRef) -> Result<EndpointSlot, FsError> {
        if self.is_stopping() {
            return Err(FsError::NotConnected);
        }
        self.endpoints
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                (count < ENDPOINT_CAPACITY).then_some(count + 1)
            })
            .map_err(|_| FsError::NoMemory)?;
        Ok(EndpointSlot {
            net: Arc::downgrade(self),
        })
    }

    /// 从真实容量和本域身份准备核心，节点所有资源齐备后才发表表拥有。
    pub fn try_new_tcp(
        self: &NetStackRef,
        rx_len: usize,
        tx_len: usize,
    ) -> Result<Arc<TcpEndpoint>, FsError> {
        let _admission = self.admission()?;
        let slot = self.claim_endpoint_slot()?;
        let id = self.allocate_identity();
        let core = self.new_core(id, rx_len, tx_len)?;
        let endpoint = TcpEndpoint::try_new(self, id, core, slot)?;
        self.tables.insert_tcp(endpoint.clone())?;
        Ok(endpoint)
    }

    /// child 和普通连接共享同一环境来源，只复制实际查询到的缓冲容量。
    pub(crate) fn new_core(
        &self,
        id: u64,
        rx_len: usize,
        tx_len: usize,
    ) -> Result<TcpCore, FsError> {
        let seed = self.seed.wrapping_add(id.wrapping_mul(0x9e3779b97f4a7c15));
        TcpCore::try_new(rx_len, tx_len, LOOPBACK_MTU, seed)
    }

    /// UDP 与 TCP 取得同一节点额度，但各自拥有独立端口索引。
    pub fn try_new_udp(self: &NetStackRef) -> Result<Arc<UdpEndpoint>, FsError> {
        let _admission = self.admission()?;
        let slot = self.claim_endpoint_slot()?;
        let id = self.allocate_identity();
        let endpoint = UdpEndpoint::try_new(self, id, slot)?;
        self.tables.insert_udp(endpoint.clone())?;
        Ok(endpoint)
    }

    /// 在同一准入内准备两份节点额度和身份，完整 pair 成功后才交根表拥有。
    pub fn try_new_unix_pair(self: &NetStackRef) -> Result<[Arc<UnixEndpoint>; 2], FsError> {
        let _admission = self.admission()?;
        let slots = [self.claim_endpoint_slot()?, self.claim_endpoint_slot()?];
        let ids = [self.allocate_identity(), self.allocate_identity()];
        let (pair, endpoints) = UnixPair::try_new(self, ids, slots)?;
        self.tables.insert_unix(pair)?;
        Ok(endpoints)
    }

    /// 返回当前唯一 lo 的真实地址和掩码。
    pub fn loopback_cidr(&self) -> Ipv4Cidr {
        Ipv4Cidr::new(Ipv4Address::new(127, 0, 0, 1), 8)
    }

    /// 软件报文和连接使用同一真实 IP MTU。
    pub fn loopback_mtu(&self) -> usize {
        LOOPBACK_MTU
    }

    /// 校验路由后只移动 owned 包，未接受时将完整拥有交回发送者。
    pub fn try_emit(&self, packet: OwnedPacket) -> Result<(), (FsError, OwnedPacket)> {
        if LOOPBACK_MTU < packet.len() {
            return Err((FsError::MessageTooLarge, packet));
        }
        let bytes = packet.bytes();
        let source = [bytes[12], bytes[13], bytes[14], bytes[15]];
        if let Err(error) = self.routes.resolve(packet.destination(), source) {
            return Err((error, packet));
        }
        {
            let mut egress = self.egress.lock();
            if !egress.open {
                return Err((FsError::NotConnected, packet));
            }
            if egress.packets.len() == LOOPBACK_CAPACITY {
                return Err((FsError::WouldBlock, packet));
            }
            egress.packets.push_back(packet);
        }
        let _ = self
            .ingress
            .get()
            .expect("network ingress is not prepared")
            .as_ref()
            .queue();
        Ok(())
    }

    /// 普通 owned 发送失败释放未接受包，保留实际路由或容量错误。
    pub fn send_ipv4(&self, packet: OwnedPacket) -> Result<(), FsError> {
        self.try_emit(packet).map_err(|(error, _packet)| error)
    }

    /// 容量和满队列登记使用同一短锁，关门返回真实错误，不访问协议或持续源。
    pub(crate) fn egress_ready(&self, id: u64) -> Result<bool, FsError> {
        let mut egress = self.egress.lock();
        if !egress.open {
            return Err(FsError::NotConnected);
        }
        if egress.packets.len() < LOOPBACK_CAPACITY {
            return Ok(true);
        }
        if !egress.waiting.contains(&id) {
            assert!(egress.waiting.len() < ENDPOINT_CAPACITY);
            egress.waiting.push(id);
        }
        Ok(false)
    }

    /// 末关闭只撤本端点身份，不影响同一共享容量上的其它等待者。
    pub(crate) fn cancel_egress_wait(&self, id: u64) {
        self.egress.lock().waiting.retain(|waiting| *waiting != id);
    }

    /// 表锁只克隆本身份拥有，解锁后定向安排 TCP Work 或通知 UDP 的同一源。
    pub(crate) fn notify_egress(&self, id: u64) {
        if self.is_stopping() {
            return;
        }
        if let Some(endpoint) = self.tables.tcp_by_id(id) {
            endpoint.schedule();
        } else if let Some(endpoint) = self.tables.udp_by_id(id) {
            endpoint.notify_egress();
        }
    }

    /// 从表移动同一 Arc 到预留退休队列，当前 callback 不等待自己的 Work。
    pub(crate) fn retire_tcp(&self, id: u64) {
        {
            let mut retired = self.retired.lock();
            if self.is_stopping() {
                return;
            }
            if let Some(endpoint) = self.tables.take_tcp(id) {
                assert!(retired.len() < ENDPOINT_CAPACITY);
                retired.push(endpoint);
            }
        }
        self.cancel_egress_wait(id);
        let _ = self
            .reaper
            .get()
            .expect("network reaper is not prepared")
            .as_ref()
            .queue();
    }

    /// 仅返回当前已经接受的软件包数量，不以查询推动协议。
    pub fn loopback_pending(&self) -> usize {
        self.egress.lock().packets.len()
    }

    /// 测试只观察真实容量等待登记，查询不安排或修改连接进度。
    #[cfg(feature = "ktest")]
    pub(crate) fn egress_waiting(&self, id: u64) -> bool {
        self.egress.lock().waiting.contains(&id)
    }

    /// 只读实际 EndpointSlot 数，包含准备中、表、退休和外部真实拥有。
    #[cfg(feature = "ktest")]
    pub fn ktest_live_endpoints(&self) -> usize {
        self.endpoints.load(Ordering::Acquire)
    }

    /// 为选定目的端口关闭 SYN 交付窗口；本域同时仅能存在一个具体控制。
    #[cfg(feature = "ktest")]
    pub fn ktest_hold_syn(&self, destination_port: u16) -> Result<(), FsError> {
        let mut egress = self.egress.lock();
        if !egress.open {
            return Err(FsError::NotConnected);
        }
        if egress.packet_control.is_some() {
            return Err(FsError::Busy);
        }
        egress.packet_control = Some(TestPacketControl::HoldSyn {
            destination_port,
            held: None,
            released: false,
        });
        Ok(())
    }

    /// 只观察原 SYN 包是否已在真正交付窗口移入 held。
    #[cfg(feature = "ktest")]
    pub fn ktest_has_held_syn(&self) -> bool {
        matches!(
            self.egress.lock().packet_control.as_ref(),
            Some(TestPacketControl::HoldSyn { held: Some(_), .. })
        )
    }

    /// 在同一队列短锁内移动原包回 lo；未接受时 held 的原拥有保持不变。
    #[cfg(feature = "ktest")]
    pub fn ktest_release_syn(&self) -> Result<(), FsError> {
        {
            let mut egress = self.egress.lock();
            if !egress.open {
                return Err(FsError::NotConnected);
            }
            if egress.packets.len() == LOOPBACK_CAPACITY {
                return Err(FsError::WouldBlock);
            }
            let packet = match egress.packet_control.as_mut() {
                Some(TestPacketControl::HoldSyn { held, released, .. }) => {
                    let packet = held.take().ok_or(FsError::InvalidOperation)?;
                    *released = true;
                    packet
                }
                _ => return Err(FsError::InvalidOperation),
            };
            egress.packets.push_back(packet);
        }
        let _ = self.ingress.get().unwrap().as_ref().queue();
        Ok(())
    }

    /// 选择一笔真实带 payload 的普通 TCP 段丢失，不修改 raw 时钟或重传算法。
    #[cfg(feature = "ktest")]
    pub fn ktest_drop_next_data(
        &self,
        source_port: u16,
        destination_port: u16,
    ) -> Result<(), FsError> {
        let mut egress = self.egress.lock();
        if !egress.open {
            return Err(FsError::NotConnected);
        }
        if egress.packet_control.is_some() {
            return Err(FsError::Busy);
        }
        egress.packet_control = Some(TestPacketControl::DropData {
            source_port,
            destination_port,
            observation: TestLossObservation::default(),
        });
        Ok(())
    }

    /// 丢失所选端口的首笔真实空 ACK，后续交付不变；关门或已有控制时拒绝配置。
    #[cfg(feature = "ktest")]
    pub fn ktest_drop_handshake_ack(
        &self,
        source_port: u16,
        destination_port: u16,
    ) -> Result<(), FsError> {
        let mut egress = self.egress.lock();
        if !egress.open {
            return Err(FsError::NotConnected);
        }
        if egress.packet_control.is_some() {
            return Err(FsError::Busy);
        }
        egress.packet_control = Some(TestPacketControl::DropHandshakeAck {
            source_port,
            destination_port,
            dropped: false,
        });
        Ok(())
    }

    /// 只读取原 ACK 在 lo 交付边界实际丢失的事实，不安排协议进度。
    #[cfg(feature = "ktest")]
    pub fn ktest_handshake_ack_dropped(&self) -> bool {
        matches!(
            self.egress.lock().packet_control.as_ref(),
            Some(TestPacketControl::DropHandshakeAck { dropped: true, .. })
        )
    }

    /// 返回实际交付边界的计数快照，查询本身不推动包或协议。
    #[cfg(feature = "ktest")]
    pub fn ktest_loss_observation(&self) -> TestLossObservation {
        match self.egress.lock().packet_control.as_ref() {
            Some(TestPacketControl::DropData { observation, .. }) => *observation,
            _ => TestLossObservation::default(),
        }
    }

    /// 仅允许结束已经没有 held 包的控制；控制拥有的最终析构在短锁外。
    #[cfg(feature = "ktest")]
    pub fn ktest_clear_packet_control(&self) -> Result<(), FsError> {
        let control = {
            let mut egress = self.egress.lock();
            if matches!(
                egress.packet_control.as_ref(),
                Some(TestPacketControl::HoldSyn { held: Some(_), .. })
            ) {
                return Err(FsError::Busy);
            }
            egress.packet_control.take()
        };
        drop(control);
        Ok(())
    }

    /// 报文解析在短锁外，锁内只移动原 SYN 或记录所选 data、空 ACK 的真实到达事实。
    #[cfg(feature = "ktest")]
    fn controlled_loopback_packet(&self, packet: OwnedPacket) -> Option<OwnedPacket> {
        if packet.protocol() != IpProtocol::Tcp {
            return Some(packet);
        }
        let (_, repr) = packet.tcp().expect("validated loopback TCP packet");
        let (source, destination, control, payload, sequence, acknowledged) = (
            repr.src_port,
            repr.dst_port,
            repr.control,
            repr.payload.len(),
            repr.seq_number.0,
            repr.ack_number.is_some(),
        );
        let now = time::now().as_nanos();
        let mut egress = self.egress.lock();
        match egress.packet_control.as_mut() {
            Some(TestPacketControl::HoldSyn {
                destination_port,
                held,
                released,
            }) if destination == *destination_port && control == TcpControl::Syn && !*released => {
                if held.is_none() {
                    *held = Some(packet);
                    return None;
                }
                drop(egress);
                drop(packet);
                return None;
            }
            Some(TestPacketControl::DropHandshakeAck {
                source_port,
                destination_port,
                dropped,
            }) if source == *source_port
                && destination == *destination_port
                && control == TcpControl::None
                && payload == 0
                && acknowledged
                && !*dropped =>
            {
                *dropped = true;
                drop(egress);
                drop(packet);
                return None;
            }
            Some(TestPacketControl::DropData {
                source_port,
                destination_port,
                observation,
            }) if source == *source_port
                && destination == *destination_port
                && 0 < payload
                && matches!(control, TcpControl::None | TcpControl::Psh) =>
            {
                if observation.dropped == 0 {
                    observation.dropped = 1;
                    observation.sequence = Some(sequence);
                    observation.first_ns = now;
                    drop(egress);
                    drop(packet);
                    return None;
                }
                if observation.sequence == Some(sequence) {
                    observation.retransmissions += 1;
                    if observation.retransmissions == 1 {
                        observation.repeated_ns = now;
                    }
                }
            }
            _ => {}
        }
        drop(egress);
        Some(packet)
    }

    /// 有界领取后释放队列锁，再通知原等待者并处理同一个 IPv4 路径。
    fn receive_loopback(&self) {
        for _ in 0..PROTOCOL_BUDGET {
            let mut waiting = [0u64; ENDPOINT_CAPACITY];
            let (packet, count) = {
                let mut egress = self.egress.lock();
                if !egress.open {
                    return;
                }
                let packet = egress.packets.pop_front();
                let count = if packet.is_some() {
                    egress.waiting.len()
                } else {
                    0
                };
                waiting[..count].copy_from_slice(&egress.waiting[..count]);
                if packet.is_some() {
                    egress.waiting.clear();
                }
                (packet, count)
            };
            for id in &waiting[..count] {
                self.notify_egress(*id);
            }
            let Some(packet) = packet else {
                return;
            };
            #[cfg(feature = "ktest")]
            let Some(packet) = self.controlled_loopback_packet(packet) else {
                continue;
            };
            if let Err(error) = self.deliver_ipv4(packet) {
                crate::klog!(
                    crate::klog::Level::Debug,
                    "loopback delivery failed: {:?}",
                    error
                );
            }
        }
        if self.loopback_pending() != 0 && !self.is_stopping() {
            let _ = self.ingress.get().unwrap().as_ref().queue();
        }
    }

    /// 表只查找并克隆真正拥有；协议、报文编码和队列接收均在表锁外。
    fn deliver_ipv4(&self, packet: OwnedPacket) -> Result<(), FsError> {
        match packet.protocol() {
            IpProtocol::Tcp => {
                let (peer, local) = packet.tcp_endpoints()?;
                let endpoint = self
                    .tables
                    .lookup_tcp(TcpTuple { local, peer })
                    .or_else(|| self.tables.lookup_listener(local));
                if let Some(endpoint) = endpoint {
                    return endpoint.enqueue(packet);
                }
                if let Some(reset) = packet.try_tcp_reset(LOOPBACK_MTU)? {
                    self.send_ipv4(reset)?;
                }
            }
            IpProtocol::Udp => {
                let (peer, local) = packet.udp_endpoints()?;
                if let Some(endpoint) = self.tables.lookup_udp(local, peer) {
                    return endpoint.enqueue(packet);
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// 具体退休拥有在短锁外覆盖 Timer 和 Work 的完整尾部。
    fn reap_retired(&self) {
        for _ in 0..PROTOCOL_BUDGET {
            if self.is_stopping() {
                return;
            }
            let endpoint = self.retired.lock().pop();
            let Some(endpoint) = endpoint else {
                return;
            };
            endpoint.stop_sync();
        }
        let pending = !self.retired.lock().is_empty();
        if pending && !self.is_stopping() {
            let _ = self.reaper.get().unwrap().as_ref().queue();
        }
    }

    /// 唯一拥有者先停生产源，再按域、节点、执行域次序等待真实尾部。
    fn stop_sync(&self) {
        {
            let _admission = self.admission.lock();
            let mut egress = self.egress.lock();
            assert!(
                !self.stopping.swap(true, Ordering::AcqRel),
                "network domain stopped twice"
            );
            egress.open = false;
        }
        self.ingress.get().unwrap().as_ref().close_sync();
        self.reaper.get().unwrap().as_ref().close_sync();
        let (tcp, udp, unix) = self.tables.take_all();
        let retired = ::core::mem::take(&mut *self.retired.lock());
        for endpoint in tcp.into_iter().chain(retired) {
            endpoint.stop_sync();
        }
        for endpoint in udp {
            endpoint.stop_sync();
        }
        for pair in unix {
            pair.stop_sync();
        }
        #[cfg(feature = "ktest")]
        {
            let control = self.egress.lock().packet_control.take();
            drop(control);
        }
        let packets = {
            let mut egress = self.egress.lock();
            egress.waiting.clear();
            ::core::mem::take(&mut egress.packets)
        };
        drop(packets);
        self.work_queue().close_sync();
        self.stopped.store(true, Ordering::Release);
    }
}

impl Drop for NetStack {
    /// Arc 的最后释放不承担 join，服务必须已由唯一普通任务拥有者停稳。
    fn drop(&mut self) {
        assert!(
            self.stopped.load(Ordering::Acquire),
            "network domain dropped before stop"
        );
    }
}

/// 根拥有者从准备开始一直保活 NetStack，直到 ingress Work 同步尾部结束。
fn run_ingress(work: Pin<&Work>) {
    // SAFETY: context 来自稳定 Arc payload；唯一服务拥有者等待本 Work 全尾后才释放域。
    let net = unsafe { &*(work.context() as *const NetStack) };
    net.receive_loopback();
}

/// 退休 Work 不同步关闭自身，只等待从具体退休队列移出的连接。
fn run_retired(work: Pin<&Work>) {
    // SAFETY: context 来自稳定 Arc payload；根拥有者在本 Work 同步全尾结束前保活域。
    let net = unsafe { &*(work.context() as *const NetStack) };
    net.reap_retired();
}
