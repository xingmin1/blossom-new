//! 本域绑定、协议拥有与查找键分别保存；短锁不执行协议或最后对象析构。

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::mem;

use crate::fs::FsError;
use crate::net::address::InetEndpoint;
use crate::net::tcp::TcpEndpoint;
use crate::net::udp::UdpEndpoint;
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
}

impl Drop for BindLease {
    /// 只撤已经发表的预留，弱域升级及最终域引用析构均在表锁外。
    fn drop(&mut self) {
        if self.published {
            if let Some(net) = self.net.upgrade() {
                net.tables().release_binding(self.owner_id);
            }
        }
    }
}

#[derive(Clone, Copy)]
struct Binding {
    protocol: TransportProtocol,
    local: InetEndpoint,
    owner_id: u64,
}

struct TablesState {
    bindings: Vec<Binding>,
    tcp: Vec<Arc<TcpEndpoint>>,
    tuples: Vec<(u64, TcpTuple)>,
    listeners: Vec<(u64, InetEndpoint)>,
    udp: Vec<Arc<UdpEndpoint>>,
    udp_keys: Vec<(u64, InetEndpoint)>,
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
                next_port: FIRST_EPHEMERAL_PORT,
                closed: false,
            }),
        })
    }

    /// 锁外准备 Lease；选端口、核冲突与发表记录在同一短锁内完成。
    pub fn reserve_binding(
        &self,
        net: &NetStackRef,
        owner_id: u64,
        protocol: TransportProtocol,
        requested: InetEndpoint,
    ) -> Result<Arc<BindLease>, FsError> {
        if !requested.is_unspecified() && !net.routes().is_local(requested.address) {
            return Err(FsError::AddressNotAvailable);
        }
        let mut lease = Arc::try_new(BindLease {
            protocol,
            local: requested,
            owner_id,
            net: Arc::downgrade(net),
            published: false,
        })
        .map_err(|_| FsError::NoMemory)?;
        {
            let mut state = self.state.lock();
            if state.closed {
                return Err(FsError::InvalidOperation);
            }
            if state
                .bindings
                .iter()
                .any(|entry| entry.owner_id == owner_id)
            {
                return Err(FsError::InvalidOperation);
            }
            if state.bindings.len() == TABLE_CAPACITY {
                return Err(FsError::NoMemory);
            }
            let local = if requested.port == 0 {
                choose_ephemeral(&mut state, protocol, requested)?
            } else {
                if binding_conflicts(&state.bindings, protocol, requested) {
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
            });
            prepared.published = true;
        }
        Ok(lease)
    }

    /// 只移除小型绑定记录；协议索引的撤除不提前归还端口。
    pub fn release_binding(&self, owner_id: u64) {
        let mut state = self.state.lock();
        if let Some(index) = state
            .bindings
            .iter()
            .position(|entry| entry.owner_id == owner_id)
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

    /// 同锁核完整 tuple 唯一性后发表，重复 SYN 不能取得第二个连接键。
    pub fn publish_tcp_tuple(&self, id: u64, tuple: TcpTuple) -> Result<(), FsError> {
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

    /// 稳定 listener 的本地键单独发表，不替换连接 tuple 或对象拥有。
    pub fn publish_listener(&self, id: u64, local: InetEndpoint) -> Result<(), FsError> {
        let mut state = self.state.lock();
        if state.closed || !state.tcp.iter().any(|entry| entry.id() == id) {
            return Err(FsError::BadFd);
        }
        publish_local(&mut state.listeners, id, local)
    }

    /// UDP 的本地键只关联 UDP 拥有表，与 TCP 监听空间分开。
    pub fn publish_udp(&self, id: u64, local: InetEndpoint) -> Result<(), FsError> {
        let mut state = self.state.lock();
        if state.closed || !state.udp.iter().any(|entry| entry.id() == id) {
            return Err(FsError::BadFd);
        }
        publish_local(&mut state.udp_keys, id, local)
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

    /// 先查具体地址再查通配 UDP 键，数据和 source 仍由包拥有。
    pub fn lookup_udp(&self, local: InetEndpoint) -> Option<Arc<UdpEndpoint>> {
        let state = self.state.lock();
        let id = lookup_local(&state.udp_keys, local)?;
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

    /// 唯一整域停止关闭发表并移动两张拥有表；绑定只随最后 Lease 结束。
    pub fn take_all(&self) -> (Vec<Arc<TcpEndpoint>>, Vec<Arc<UdpEndpoint>>) {
        let mut state = self.state.lock();
        state.closed = true;
        state.tuples.clear();
        state.listeners.clear();
        state.udp_keys.clear();
        (mem::take(&mut state.tcp), mem::take(&mut state.udp))
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

/// 同协议同端口中，通配任一方或相同具体地址构成冲突。
fn binding_conflicts(
    bindings: &[Binding],
    protocol: TransportProtocol,
    local: InetEndpoint,
) -> bool {
    bindings.iter().any(|entry| {
        entry.protocol == protocol
            && entry.local.port == local.port
            && (entry.local.is_unspecified()
                || local.is_unspecified()
                || entry.local.address == local.address)
    })
}

/// 游标仅覆盖动态端口区间，选中和发表由调用者保持同一绑定锁。
fn choose_ephemeral(
    state: &mut TablesState,
    protocol: TransportProtocol,
    requested: InetEndpoint,
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
        if !binding_conflicts(&state.bindings, protocol, local) {
            return Ok(local);
        }
        if state.next_port == start {
            return Err(FsError::AddressInUse);
        }
    }
}

/// 本地键同一身份只发表一次，完整相同键冲突真实返回 AddressInUse。
fn publish_local(
    entries: &mut Vec<(u64, InetEndpoint)>,
    id: u64,
    local: InetEndpoint,
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
    entries.push((id, local));
    Ok(())
}

/// 地址精确键优先于同端口通配键，返回值不借用索引存储。
fn lookup_local(entries: &[(u64, InetEndpoint)], local: InetEndpoint) -> Option<u64> {
    entries
        .iter()
        .find(|entry| entry.1 == local)
        .or_else(|| {
            entries
                .iter()
                .find(|entry| entry.1.port == local.port && entry.1.is_unspecified())
        })
        .map(|entry| entry.0)
}
