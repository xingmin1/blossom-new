//! 一个显式网络域拥有 loopback 包队列与连接身份，协议处理不进入域的短锁。

pub mod address;
pub mod core;
pub mod packet;
pub mod socket;

use ::core::sync::atomic::{AtomicU64, Ordering};
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use smoltcp::wire::{Ipv4Address, Ipv4Cidr};

use crate::fs::FsError;
use crate::net::address::SocketAddress;
use crate::net::core::TcpCore;
use crate::net::packet::OwnedPacket;
use crate::net::socket::TcpEndpoint;
use crate::sync::SpinLock;
use crate::time;

pub const LOOPBACK_CAPACITY: usize = 32;
pub const LOOPBACK_MTU: usize = 1500;

/// Runtime、打开对象和系统 owner 持同一域引用，不按进程复制网络状态。
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

/// 网络域持真实配置和预留包队列，连接环境在构造时从本域取得独立编号与 seed。
pub struct NetStack {
    loopback: Ipv4Cidr,
    loopback_mtu: usize,
    packets: SpinLock<VecDeque<OwnedPacket>>,
    next_connection: AtomicU64,
    seed: u64,
}

impl NetStack {
    /// 在普通任务中一次准备固定三十二项队列；分配失败没有半发表服务或后备全局栈。
    pub fn try_new_lo() -> Result<NetStackRef, FsError> {
        let mut packets = VecDeque::new();
        packets
            .try_reserve_exact(LOOPBACK_CAPACITY)
            .map_err(|_| FsError::NoMemory)?;
        Arc::try_new(Self {
            loopback: Ipv4Cidr::new(Ipv4Address::new(127, 0, 0, 1), 8),
            loopback_mtu: LOOPBACK_MTU,
            packets: SpinLock::new(packets),
            next_connection: AtomicU64::new(1),
            seed: time::now().as_nanos(),
        })
        .map_err(|_| FsError::NoMemory)
    }

    /// 返回本域真实配置的 lo 地址和掩码，不从用户地址推测接口配置。
    pub fn loopback_cidr(&self) -> Ipv4Cidr {
        self.loopback
    }

    /// 新连接和包发表都使用同一 lo IP MTU。
    pub fn loopback_mtu(&self) -> usize {
        self.loopback_mtu
    }

    /// 只在短锁内移动已拥有的包；满队列返回真实 WouldBlock，raw emitter 据此保发送责任。
    pub fn enqueue_loopback(&self, packet: OwnedPacket) -> Result<(), FsError> {
        if self.loopback_mtu < packet.len() {
            return Err(FsError::MessageTooLarge);
        }
        if !self
            .loopback
            .contains_addr(&Ipv4Address::from(packet.destination()))
        {
            return Err(FsError::AddressNotAvailable);
        }
        let mut packets = self.packets.lock();
        if packets.len() == LOOPBACK_CAPACITY {
            return Err(FsError::WouldBlock);
        }
        packets.push_back(packet);
        Ok(())
    }

    /// 领取完整包后释放域锁，协议解析、用户复制及应答编码由连接自己的保护承担。
    pub fn take_loopback(&self) -> Option<OwnedPacket> {
        self.packets.lock().pop_front()
    }

    /// 返回真实已发表队列长度，读取不调用协议核心或制造虚假 poll。
    pub fn loopback_pending(&self) -> usize {
        self.packets.lock().len()
    }

    /// 在域短锁外构造稳定独立核心；编号不会回绕，失败准备不会发表协议对象。
    pub fn try_new_tcp(
        self: &NetStackRef,
        rx_len: usize,
        tx_len: usize,
    ) -> Result<Arc<TcpEndpoint>, FsError> {
        let id = self
            .next_connection
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
            .expect("network connection identity exhausted");
        let seed = self.seed.wrapping_add(id.wrapping_mul(0x9e3779b97f4a7c15));
        let core = TcpCore::try_new(rx_len, tx_len, self.loopback_mtu, seed)?;
        TcpEndpoint::try_new(self, id, core)
    }
}
