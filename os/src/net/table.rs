//! 本域绑定、协议拥有与查找键分别保存；短锁不执行协议或最后对象析构。

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::mem;
use smoltcp::wire::{IpAddress, Ipv6Address};

use crate::fs::FsError;
use crate::net::address::InetEndpoint;
use crate::net::tcp::TcpEndpoint;
use crate::net::udp::UdpEndpoint;
use crate::net::unix::UnixPair;
use crate::net::{NetStack, NetStackRef};
use crate::sync::SpinLock;

const TABLE_CAPACITY: usize = 128;
const FIRST_EPHEMERAL_PORT: u16 = 49152;

/// TCP 和 UDP 使用互不冲突的端口空间。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TransportProtocol {
    Tcp,
    Udp,
}

/// 已连接 TCP 以完整本地与远端端点查找，不与监听键混用。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TcpTuple {
    pub local: InetEndpoint,
    pub peer: InetEndpoint,
}

/// 最后真实预留拥有结束时撤绑定；监听孩子共享同一个 Arc。
pub struct BindLease {
    protocol: TransportProtocol,
    local: InetEndpoint,
    owner_id: u64,
    lease_id: u64,
    dual_stack: bool,
    net: Weak<NetStack>,
    /// 准备中的 Arc 尚未发表，失败析构不能撤其它真实记录。
    published: bool,
}

impl BindLease {
    /// 返回绑定时选定的地址和端口，通配地址保持零值。
    pub fn local(&self) -> InetEndpoint {
        self.local
    }

    /// 返回原端口预留者身份；孩子不获得新的绑定身份。
    pub fn owner_id(&self) -> u64 {
        self.owner_id
    }

    /// 每次预留使用独立不复用身份，旧发送的最后租约不能撤除后继绑定。
    pub fn id(&self) -> u64 {
        self.lease_id
    }

    /// 同一 IPv6 通配租约可额外覆盖 IPv4 接收面，不创建第二份端口拥有。
    pub fn dual_stack(&self) -> bool {
        self.dual_stack
    }
}

impl Drop for BindLease {
    /// 只撤已经发表的预留，弱域升级及最终域引用析构均在表锁外。
    fn drop(&mut self) {
        if self.published {
            if let Some(net) = self.net.upgrade() {
                net.tables().release_binding(self.lease_id);
            }
        }
    }
}

#[derive(Clone, Copy)]
struct Binding {
    protocol: TransportProtocol,
    local: InetEndpoint,
    owner_id: u64,
    lease_id: u64,
    reuse: bool,
    dual_stack: bool,
}

struct TablesState {
    bindings: Vec<Binding>,
    tcp: Vec<Arc<TcpEndpoint>>,
    tuples: Vec<(u64, TcpTuple)>,
    listeners: Vec<(u64, InetEndpoint, bool)>,
    udp: Vec<Arc<UdpEndpoint>>,
    udp_keys: Vec<(u64, InetEndpoint, Option<InetEndpoint>, bool)>,
    unix: Vec<Arc<UnixPair>>,
    next_port: u16,
    closed: bool,
}

/// ponytail: 128 项有限线性索引，实际查找竞争影响吞吐时再按真实键拆桶。
pub struct NetTables {
    state: SpinLock<TablesState>,
}

impl NetTables {
    /// 在构造期锁外 fallible 预留各表，发表后插入不扩容。
    pub fn try_new() -> Result<Self, FsError> {
        Ok(Self {
            state: SpinLock::new(TablesState {
                bindings: reserved_entries()?,
                tcp: reserved_entries()?,
                tuples: reserved_entries()?,
                listeners: reserved_entries()?,
                udp: reserved_entries()?,
                udp_keys: reserved_entries()?,
                unix: reserved_entries()?,
                next_port: FIRST_EPHEMERAL_PORT,
                closed: false,
            }),
        })
    }

    /// 锁外准备独立租约身份；选端口、复用冲突与发表记录在同一短锁完成。
    pub fn reserve_binding(
        &self,
        net: &NetStackRef,
        owner_id: u64,
        protocol: TransportProtocol,
        requested: InetEndpoint,
        reuse: bool,
        dual_stack: bool,
    ) -> Result<Arc<BindLease>, FsError> {
        if dual_stack && requested.address != IpAddress::Ipv6(Ipv6Address::UNSPECIFIED) {
            return Err(FsError::InvalidOperation);
        }
        if !requested.is_unspecified() && !net.is_local(requested.address) {
            return Err(FsError::AddressNotAvailable);
        }
        let mut lease = Arc::try_new(BindLease {
            protocol,
            local: requested,
            owner_id,
            lease_id: net.allocate_identity(),
            dual_stack,
            net: Arc::downgrade(net),
            published: false,
        })
        .map_err(|_| FsError::NoMemory)?;
        {
            let mut state = self.state.lock();
            if state.closed {
                return Err(FsError::InvalidOperation);
            }
            if state.bindings.len() == TABLE_CAPACITY {
                return Err(FsError::NoMemory);
            }
            let local = if requested.port == 0 {
                choose_ephemeral(&mut state, protocol, requested, dual_stack)?
            } else {
                if binding_conflicts(&state.bindings, protocol, requested, reuse, dual_stack)
                    || (protocol == TransportProtocol::Tcp
                        && state
                            .listeners
                            .iter()
                            .any(|entry| overlaps(entry.1, entry.2, requested, dual_stack)))
                {
                    return Err(FsError::AddressInUse);
                }
                requested
            };
            // 尚无外部强引用或弱引用，只有准备者能修改最终端点。
            let prepared = Arc::get_mut(&mut lease).expect("unpublished binding lease shared");
            prepared.local = local;
            state.bindings.push(Binding {
                protocol: prepared.protocol,
                local,
                owner_id,
                lease_id: prepared.lease_id,
                reuse,
                dual_stack,
            });
            prepared.published = true;
        }
        Ok(lease)
    }

    /// 按当前租约身份更新已有策略，不影响同 owner 的旧在途发送租约。
    pub fn set_reuse(&self, lease_id: u64, reuse: bool) -> Result<(), FsError> {
        let mut state = self.state.lock();
        if state.closed {
            return Err(FsError::NotConnected);
        }
        if let Some(binding) = state
            .bindings
            .iter_mut()
            .find(|entry| entry.lease_id == lease_id)
        {
            binding.reuse = reuse;
        }
        Ok(())
    }

    /// 最后租约只撤自己的绑定记录，旧租约释放不撤同 owner 的后继绑定。
    pub fn release_binding(&self, lease_id: u64) {
        let mut state = self.state.lock();
        if let Some(index) = state
            .bindings
            .iter()
            .position(|entry| entry.lease_id == lease_id)
        {
            state.bindings.swap_remove(index);
        }
    }

    /// 完整准备的 TCP 节点获得真实表拥有，失败参数在退出锁域后析构。
    pub fn insert_tcp(&self, endpoint: Arc<TcpEndpoint>) -> Result<(), FsError> {
        let id = endpoint.id();
        let mut state = self.state.lock();
        if state.closed || state.tcp.iter().any(|entry| entry.id() == id) {
            return Err(FsError::InvalidOperation);
        }
        if state.tcp.len() == TABLE_CAPACITY {
            return Err(FsError::NoMemory);
        }
        state.tcp.push(endpoint);
        Ok(())
    }

    /// 完整准备的 UDP 节点获得真实表拥有，端口索引另行发表。
    pub fn insert_udp(&self, endpoint: Arc<UdpEndpoint>) -> Result<(), FsError> {
        let id = endpoint.id();
        let mut state = self.state.lock();
        if state.closed || state.udp.iter().any(|entry| entry.id() == id) {
            return Err(FsError::InvalidOperation);
        }
        if state.udp.len() == TABLE_CAPACITY {
            return Err(FsError::NoMemory);
        }
        state.udp.push(endpoint);
        Ok(())
    }

    /// pair 和两个端点完整准备后才发表根拥有；失败拥有由调用者在表锁外释放。
    pub fn insert_unix(&self, pair: Arc<UnixPair>) -> Result<(), FsError> {
        let id = pair.id();
        let mut state = self.state.lock();
        if state.closed || state.unix.iter().any(|entry| entry.id() == id) {
            return Err(FsError::InvalidOperation);
        }
        if state.unix.len() == TABLE_CAPACITY {
            return Err(FsError::NoMemory);
        }
        state.unix.push(pair);
        Ok(())
    }

    /// 同锁核完整 tuple 唯一性后发表，重复 SYN 不能取得第二个连接键。
    pub fn publish_tcp_tuple(&self, id: u64, tuple: TcpTuple) -> Result<(), FsError> {
        if tuple.local.address.version() != tuple.peer.address.version()
            || tuple.local.is_unspecified()
            || tuple.peer.is_unspecified()
            || tuple.local.port == 0
            || tuple.peer.port == 0
        {
            return Err(FsError::InvalidOperation);
        }
        let mut state = self.state.lock();
        if state.closed || !state.tcp.iter().any(|entry| entry.id() == id) {
            return Err(FsError::BadFd);
        }
        if state.tuples.iter().any(|entry| entry.1 == tuple) {
            return Err(FsError::AddressInUse);
        }
        if state.tuples.iter().any(|entry| entry.0 == id) {
            return Err(FsError::InvalidOperation);
        }
        if state.tuples.len() == TABLE_CAPACITY {
            return Err(FsError::NoMemory);
        }
        state.tuples.push((id, tuple));
        Ok(())
    }

    /// 监听发表复核当前绑定策略；重叠监听者永远不能共享地址和端口。
    pub fn publish_listener(&self, id: u64, local: InetEndpoint) -> Result<(), FsError> {
        let mut state = self.state.lock();
        if state.closed || !state.tcp.iter().any(|entry| entry.id() == id) {
            return Err(FsError::BadFd);
        }
        let binding = state
            .bindings
            .iter()
            .find(|entry| entry.owner_id == id)
            .ok_or(FsError::BadFd)?;
        if binding.protocol != TransportProtocol::Tcp || binding.local != local {
            return Err(FsError::InvalidOperation);
        }
        let dual_stack = binding.dual_stack;
        if state
            .listeners
            .iter()
            .any(|entry| overlaps(entry.1, entry.2, local, dual_stack))
            || state.bindings.iter().any(|entry| {
                entry.owner_id != id
                    && entry.protocol == TransportProtocol::Tcp
                    && overlaps(entry.local, entry.dual_stack, local, dual_stack)
                    && !(binding.reuse && entry.reuse)
            })
        {
            return Err(FsError::AddressInUse);
        }
        publish_local(&mut state.listeners, id, local, dual_stack)
    }

    /// UDP 键只复制端点当前地址和 peer；合法复用不再被精确地址检查否定。
    pub fn publish_udp(
        &self,
        id: u64,
        local: InetEndpoint,
        peer: Option<InetEndpoint>,
    ) -> Result<(), FsError> {
        let mut state = self.state.lock();
        if state.closed || !state.udp.iter().any(|entry| entry.id() == id) {
            return Err(FsError::BadFd);
        }
        if local.port == 0 {
            return Err(FsError::InvalidOperation);
        }
        let dual_stack = state
            .bindings
            .iter()
            .find(|entry| {
                entry.owner_id == id
                    && entry.protocol == TransportProtocol::Udp
                    && matches_local(entry.local, entry.dual_stack, local)
            })
            .ok_or(FsError::BadFd)?
            .dual_stack;
        if let Some(index) = state.udp_keys.iter().position(|entry| entry.0 == id) {
            state.udp_keys.remove(index);
        } else if state.udp_keys.len() == TABLE_CAPACITY {
            return Err(FsError::NoMemory);
        }
        state.udp_keys.push((id, local, peer, dual_stack));
        Ok(())
    }

    /// 断开自动绑定只撤本身份的查询键，节点拥有和租约由各自拥有者结束。
    pub fn unpublish_udp(&self, id: u64) {
        self.state.lock().udp_keys.retain(|entry| entry.0 != id);
    }

    /// 查完整 tuple 并取得真实节点引用；协议调用由持引用者在解锁后执行。
    pub fn lookup_tcp(&self, tuple: TcpTuple) -> Option<Arc<TcpEndpoint>> {
        let state = self.state.lock();
        let id = state.tuples.iter().find(|entry| entry.1 == tuple)?.0;
        state.tcp.iter().find(|entry| entry.id() == id).cloned()
    }

    /// 按真实身份取得拥有表引用，容量恢复通知在解锁后只安排这个连接。
    pub fn tcp_by_id(&self, id: u64) -> Option<Arc<TcpEndpoint>> {
        let state = self.state.lock();
        state.tcp.iter().find(|entry| entry.id() == id).cloned()
    }

    /// 先查具体地址再查通配 listener，查找不改变监听者状态。
    pub fn lookup_listener(&self, local: InetEndpoint) -> Option<Arc<TcpEndpoint>> {
        let state = self.state.lock();
        let id = lookup_local(&state.listeners, local)?;
        state.tcp.iter().find(|entry| entry.id() == id).cloned()
    }

    /// 具体本地地址优先，其次 connected peer，同评分取最近发表的真实节点。
    pub fn lookup_udp(&self, local: InetEndpoint, peer: InetEndpoint) -> Option<Arc<UdpEndpoint>> {
        let state = self.state.lock();
        let id = state
            .udp_keys
            .iter()
            .enumerate()
            .filter(|(_, entry)| {
                matches_local(entry.1, entry.3, local)
                    && entry.2.map_or(true, |expected| expected == peer)
            })
            .max_by_key(|(index, entry)| {
                (
                    (
                        u8::from(!entry.1.is_unspecified()),
                        u8::from(entry.2.is_some()),
                    ),
                    *index,
                )
            })
            .map(|(_, entry)| entry.0)?;
        state.udp.iter().find(|entry| entry.id() == id).cloned()
    }

    /// 定向容量通知只取得这个 UDP 身份，解表锁后才访问接收事实或持续源。
    pub fn udp_by_id(&self, id: u64) -> Option<Arc<UdpEndpoint>> {
        let state = self.state.lock();
        state.udp.iter().find(|entry| entry.id() == id).cloned()
    }

    /// 一次撤全部该 TCP 身份的索引并移出真实拥有，Lease 寿命保持独立。
    pub fn take_tcp(&self, id: u64) -> Option<Arc<TcpEndpoint>> {
        let mut state = self.state.lock();
        state.tuples.retain(|entry| entry.0 != id);
        state.listeners.retain(|entry| entry.0 != id);
        let index = state.tcp.iter().position(|entry| entry.id() == id)?;
        Some(state.tcp.swap_remove(index))
    }

    /// 撤 UDP 本地键并移出真实拥有，最后节点析构由调用者在锁外承担。
    pub fn take_udp(&self, id: u64) -> Option<Arc<UdpEndpoint>> {
        let mut state = self.state.lock();
        state.udp_keys.retain(|entry| entry.0 != id);
        let index = state.udp.iter().position(|entry| entry.id() == id)?;
        Some(state.udp.swap_remove(index))
    }

    /// 两端均已关闭后移出根 pair 拥有，最后释放发生在调用者的锁外作用域。
    pub fn take_unix(&self, id: u64) -> Option<Arc<UnixPair>> {
        let mut state = self.state.lock();
        let index = state.unix.iter().position(|entry| entry.id() == id)?;
        Some(state.unix.swap_remove(index))
    }

    /// 唯一整域停止关闭发表并移动三类拥有；绑定只随最后 Lease 结束。
    pub fn take_all(
        &self,
    ) -> (
        Vec<Arc<TcpEndpoint>>,
        Vec<Arc<UdpEndpoint>>,
        Vec<Arc<UnixPair>>,
    ) {
        let mut state = self.state.lock();
        state.closed = true;
        state.tuples.clear();
        state.listeners.clear();
        state.udp_keys.clear();
        (
            mem::take(&mut state.tcp),
            mem::take(&mut state.udp),
            mem::take(&mut state.unix),
        )
    }
}

/// 所有有限索引使用同一构造期容量，不在持锁插入时申请存储。
fn reserved_entries<T>() -> Result<Vec<T>, FsError> {
    let mut entries = Vec::new();
    entries
        .try_reserve_exact(TABLE_CAPACITY)
        .map_err(|_| FsError::NoMemory)?;
    Ok(entries)
}

/// 同协议的重叠绑定仅在双方显式允许复用时不冲突。
fn binding_conflicts(
    bindings: &[Binding],
    protocol: TransportProtocol,
    local: InetEndpoint,
    reuse: bool,
    dual_stack: bool,
) -> bool {
    bindings.iter().any(|entry| {
        entry.protocol == protocol
            && overlaps(entry.local, entry.dual_stack, local, dual_stack)
            && !(reuse && entry.reuse)
    })
}

/// 两接收面只有同族通配、相同具体值或双面通配的 IPv4 别名才重叠。
fn overlaps(left: InetEndpoint, left_dual: bool, right: InetEndpoint, right_dual: bool) -> bool {
    matches_local(left, left_dual, right) || matches_local(right, right_dual, left)
}

/// 绑定冲突、端口选择和接收查询共用同一纯地址覆盖规则。
fn matches_local(binding: InetEndpoint, dual_stack: bool, local: InetEndpoint) -> bool {
    binding.port == local.port
        && (binding.address == local.address
            || (binding.is_unspecified()
                && (binding.address.version() == local.address.version()
                    || (dual_stack && matches!(local.address, IpAddress::Ipv4(_))))))
}

/// 游标仅覆盖动态端口区间，自动端口不使用复用策略；选中和发表保持同一锁。
fn choose_ephemeral(
    state: &mut TablesState,
    protocol: TransportProtocol,
    requested: InetEndpoint,
    dual_stack: bool,
) -> Result<InetEndpoint, FsError> {
    let start = state.next_port;
    loop {
        let local = InetEndpoint {
            port: state.next_port,
            ..requested
        };
        state.next_port = if state.next_port == u16::MAX {
            FIRST_EPHEMERAL_PORT
        } else {
            state.next_port + 1
        };
        if !binding_conflicts(&state.bindings, protocol, local, false, dual_stack)
            && !(protocol == TransportProtocol::Tcp
                && state
                    .listeners
                    .iter()
                    .any(|entry| overlaps(entry.1, entry.2, local, dual_stack)))
        {
            return Ok(local);
        }
        if state.next_port == start {
            return Err(FsError::AddressInUse);
        }
    }
}

/// 本地键同一身份只发表一次，完整相同键冲突真实返回 AddressInUse。
fn publish_local(
    entries: &mut Vec<(u64, InetEndpoint, bool)>,
    id: u64,
    local: InetEndpoint,
    dual_stack: bool,
) -> Result<(), FsError> {
    if local.port == 0 {
        return Err(FsError::InvalidOperation);
    }
    if entries.iter().any(|entry| entry.1 == local) {
        return Err(FsError::AddressInUse);
    }
    if entries.iter().any(|entry| entry.0 == id) {
        return Err(FsError::InvalidOperation);
    }
    if entries.len() == TABLE_CAPACITY {
        return Err(FsError::NoMemory);
    }
    entries.push((id, local, dual_stack));
    Ok(())
}

/// 地址精确键优先于同端口通配键，返回值不借用索引存储。
fn lookup_local(entries: &[(u64, InetEndpoint, bool)], local: InetEndpoint) -> Option<u64> {
    entries
        .iter()
        .filter(|entry| matches_local(entry.1, entry.2, local))
        .max_by_key(|entry| u8::from(!entry.1.is_unspecified()))
        .map(|entry| entry.0)
}
