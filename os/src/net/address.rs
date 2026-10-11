//! 地址只保存内核值；用户 sockaddr 的长度和字节序在复制边界编解码。

use smoltcp::wire::{IpAddress, IpEndpoint, IpListenEndpoint, Ipv4Address};

use crate::fs::FsError;

pub const AF_UNSPEC: u16 = 0;
pub const AF_UNIX: u16 = 1;
pub const AF_INET: u16 = 2;
pub const INET_ADDRESS_LEN: usize = 16;

/// IPv4 地址保持网络顺序，端口在内核中使用主机顺序。
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct InetEndpoint {
    pub address: [u8; 4],
    pub port: u16,
}

impl InetEndpoint {
    /// 将明确的本地或远端地址交给协议核心，不在转换时选择地址或端口。
    pub fn raw(self) -> IpEndpoint {
        IpEndpoint::new(IpAddress::Ipv4(Ipv4Address::from(self.address)), self.port)
    }

    /// 监听允许通配地址，端口必须由网络域的绑定责任选定。
    pub fn raw_listen(self) -> IpListenEndpoint {
        IpListenEndpoint {
            addr: if self.address == [0; 4] {
                None
            } else {
                Some(IpAddress::Ipv4(Ipv4Address::from(self.address)))
            },
            port: self.port,
        }
    }

    /// 将 IPv4 raw 端点恢复为不借用协议存储的内核地址值。
    pub fn from_raw(endpoint: IpEndpoint) -> Self {
        let IpAddress::Ipv4(address) = endpoint.addr;
        Self {
            address: address.octets(),
            port: endpoint.port,
        }
    }
}

/// 无地址族用于解除默认 peer；UNIX 本版只表示匿名端点。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SocketAddress {
    Unspecified,
    Inet(InetEndpoint),
    UnixUnnamed,
}

impl SocketAddress {
    /// 从已复制的 sockaddr 解码；IPv4 使用完整十六字节 ABI，UNIX 路径不被接受。
    pub fn decode(bytes: &[u8]) -> Result<Self, FsError> {
        if bytes.len() < 2 {
            return Err(FsError::InvalidOperation);
        }
        match u16::from_ne_bytes([bytes[0], bytes[1]]) {
            AF_UNSPEC => Ok(Self::Unspecified),
            AF_INET => {
                if bytes.len() < INET_ADDRESS_LEN {
                    return Err(FsError::InvalidOperation);
                }
                Ok(Self::Inet(InetEndpoint {
                    address: [bytes[4], bytes[5], bytes[6], bytes[7]],
                    port: u16::from_be_bytes([bytes[2], bytes[3]]),
                }))
            }
            AF_UNIX if bytes.len() == 2 => Ok(Self::UnixUnnamed),
            AF_UNIX => Err(FsError::Unsupported),
            _ => Err(FsError::AddressFamilyNotSupported),
        }
    }

    /// 将可容纳的地址前缀写入内核切片，并返回完整地址长度供 socklen 消费。
    pub fn encode(self, target: &mut [u8]) -> usize {
        let mut bytes = [0u8; INET_ADDRESS_LEN];
        let (family, length) = match self {
            Self::Unspecified => (AF_UNSPEC, 2),
            Self::UnixUnnamed => (AF_UNIX, 2),
            Self::Inet(endpoint) => {
                bytes[2..4].copy_from_slice(&endpoint.port.to_be_bytes());
                bytes[4..8].copy_from_slice(&endpoint.address);
                (AF_INET, INET_ADDRESS_LEN)
            }
        };
        bytes[..2].copy_from_slice(&family.to_ne_bytes());
        let copied = target.len().min(length);
        target[..copied].copy_from_slice(&bytes[..copied]);
        length
    }
}
