//! 软件队列只转移完整 IPv4 包的拥有权，wire 借用止于当前包或同步编码调用。

use alloc::vec::Vec;
use smoltcp::phy::ChecksumCapabilities;
use smoltcp::socket::tcp::Socket as RawTcpSocket;
use smoltcp::wire::{
    IpAddress, IpProtocol, IpRepr, Ipv4Address, Ipv4Packet, Ipv4Repr, TcpControl, TcpPacket,
    TcpRepr, UdpPacket, UdpRepr,
};

use crate::fs::FsError;
use crate::net::address::InetEndpoint;

/// 完整且已验证 IPv4 头的线性存储，不保存用户地址、raw ring 或 DMA 借用。
#[derive(Debug)]
pub struct OwnedPacket {
    bytes: Vec<u8>,
}

impl OwnedPacket {
    /// 验证输入后复制真正的 IPv4 长度；介质尾部 padding 不进入软件包。
    pub fn try_from_ipv4(bytes: &[u8]) -> Result<Self, FsError> {
        let packet = checked_ipv4(bytes)?;
        match packet.next_header() {
            IpProtocol::Tcp => {
                parse_tcp(&packet)?;
            }
            IpProtocol::Udp => {
                parse_udp(&packet)?;
            }
            _ => {}
        }
        let length = packet.total_len() as usize;
        let mut owned = Vec::new();
        owned
            .try_reserve_exact(length)
            .map_err(|_| FsError::NoMemory)?;
        owned.extend_from_slice(&bytes[..length]);
        Ok(Self { bytes: owned })
    }

    /// 在当前协议保护内编码 TCP 与 IPv4 校验和，分配或 MTU 错误交还同步 emitter。
    pub fn try_from_tcp(ip: &IpRepr, tcp: &TcpRepr<'_>, ip_mtu: usize) -> Result<Self, FsError> {
        let IpRepr::Ipv4(ip) = *ip;
        if ip.next_header != IpProtocol::Tcp
            || ip.payload_len != tcp.buffer_len()
            || tcp.src_port == 0
            || tcp.dst_port == 0
        {
            return Err(FsError::InvalidOperation);
        }
        let length = ip
            .buffer_len()
            .checked_add(tcp.buffer_len())
            .ok_or(FsError::MessageTooLarge)?;
        if ip_mtu < length || (u16::MAX as usize) < length {
            return Err(FsError::MessageTooLarge);
        }
        let mut bytes = zeroed_bytes(length)?;
        let checksums = ChecksumCapabilities::default();
        let mut packet = Ipv4Packet::new_unchecked(bytes.as_mut_slice());
        ip.emit(&mut packet, &checksums);
        let mut payload = TcpPacket::new_unchecked(packet.payload_mut());
        tcp.emit(
            &mut payload,
            &IpAddress::Ipv4(ip.src_addr),
            &IpAddress::Ipv4(ip.dst_addr),
            &checksums,
        );
        Ok(Self { bytes })
    }

    /// 一次编码完整 IPv4/UDP 报文，超过路径 MTU 时不发送应用数据前缀。
    pub fn try_from_udp(
        source: InetEndpoint,
        destination: InetEndpoint,
        payload: &[u8],
        mtu: usize,
    ) -> Result<Self, FsError> {
        if destination.port == 0 {
            return Err(FsError::InvalidOperation);
        }
        let udp = UdpRepr {
            src_port: source.port,
            dst_port: destination.port,
        };
        let payload_len = udp
            .header_len()
            .checked_add(payload.len())
            .ok_or(FsError::MessageTooLarge)?;
        let ip = Ipv4Repr {
            src_addr: Ipv4Address::from(source.address),
            dst_addr: Ipv4Address::from(destination.address),
            next_header: IpProtocol::Udp,
            payload_len,
            hop_limit: 64,
        };
        let length = ip
            .buffer_len()
            .checked_add(payload_len)
            .ok_or(FsError::MessageTooLarge)?;
        if mtu < length || (u16::MAX as usize) < length {
            return Err(FsError::MessageTooLarge);
        }
        let mut bytes = zeroed_bytes(length)?;
        let checksums = ChecksumCapabilities::default();
        let mut packet = Ipv4Packet::new_unchecked(bytes.as_mut_slice());
        ip.emit(&mut packet, &checksums);
        udp.emit(
            &mut UdpPacket::new_unchecked(packet.payload_mut()),
            &IpAddress::Ipv4(ip.src_addr),
            &IpAddress::Ipv4(ip.dst_addr),
            payload.len(),
            |target| target.copy_from_slice(payload),
            &checksums,
        );
        Ok(Self { bytes })
    }

    /// 使用成熟协议的正常拒绝应答；输入 RST 不再生成 RST。
    pub fn try_tcp_reset(&self, mtu: usize) -> Result<Option<Self>, FsError> {
        let (ip, tcp) = self.tcp()?;
        if tcp.control == TcpControl::Rst {
            return Ok(None);
        }
        let (reply_ip, reply_tcp) = RawTcpSocket::rst_reply(&ip, &tcp);
        Self::try_from_tcp(&reply_ip, &reply_tcp, mtu).map(Some)
    }

    /// 查询构造时验证的 IPv4 协议号，不重新解析传输校验和。
    pub fn protocol(&self) -> IpProtocol {
        Ipv4Packet::new_unchecked(self.bytes.as_slice()).next_header()
    }

    /// UDP 地址与端口均来自当前不可变报文，不保存另一个 source 快照。
    pub fn udp_endpoints(&self) -> Result<(InetEndpoint, InetEndpoint), FsError> {
        let ip = Ipv4Packet::new_unchecked(self.bytes.as_slice());
        if ip.next_header() != IpProtocol::Udp {
            return Err(FsError::Unsupported);
        }
        let udp = UdpPacket::new_unchecked(ip.payload());
        Ok((
            InetEndpoint {
                address: ip.src_addr().octets(),
                port: udp.src_port(),
            },
            InetEndpoint {
                address: ip.dst_addr().octets(),
                port: udp.dst_port(),
            },
        ))
    }

    /// 借用同一已验证 UDP 记录的实际 payload，零长报文返回空切片。
    pub fn udp_payload(&self) -> Result<&[u8], FsError> {
        let ip = Ipv4Packet::new_unchecked(self.bytes.as_slice());
        if ip.next_header() != IpProtocol::Udp {
            return Err(FsError::Unsupported);
        }
        Ok(UdpPacket::new_unchecked(ip.payload()).payload())
    }

    /// 借用本包的完整 IPv4 字节，借用期内拥有者不能进入另一队列或释放。
    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// 返回真正的 IP 长度，不计介质头、尾部 padding 或设备头。
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// 已验证的 IPv4 头至少二十字节，地址快照不依赖协议借用。
    pub fn destination(&self) -> [u8; 4] {
        [
            self.bytes[16],
            self.bytes[17],
            self.bytes[18],
            self.bytes[19],
        ]
    }

    /// 私有不可变存储已在构造时验证，四元组查询只读同一包的 IPv4/TCP 头字段。
    pub fn tcp_endpoints(&self) -> Result<(InetEndpoint, InetEndpoint), FsError> {
        let ip = Ipv4Packet::new_unchecked(self.bytes.as_slice());
        if ip.next_header() != IpProtocol::Tcp {
            return Err(FsError::Unsupported);
        }
        let tcp = TcpPacket::new_unchecked(ip.payload());
        Ok((
            InetEndpoint {
                address: ip.src_addr().octets(),
                port: tcp.src_port(),
            },
            InetEndpoint {
                address: ip.dst_addr().octets(),
                port: tcp.dst_port(),
            },
        ))
    }

    /// 私有不可变包已验证长度和校验和，repr 解析只借当前存储，不重复计算全文校验和。
    pub(crate) fn tcp(&self) -> Result<(IpRepr, TcpRepr<'_>), FsError> {
        let packet = Ipv4Packet::new_unchecked(self.bytes.as_slice());
        if packet.next_header() != IpProtocol::Tcp {
            return Err(FsError::Unsupported);
        }
        let checksums = ChecksumCapabilities::ignored();
        let ip = Ipv4Repr::parse(&packet, &checksums).map_err(|_| FsError::Corrupt)?;
        let tcp_packet = TcpPacket::new_unchecked(packet.payload());
        let tcp = TcpRepr::parse(
            &tcp_packet,
            &IpAddress::Ipv4(ip.src_addr),
            &IpAddress::Ipv4(ip.dst_addr),
            &checksums,
        )
        .map_err(|_| FsError::Corrupt)?;
        Ok((IpRepr::Ipv4(ip), tcp))
    }
}

/// 所有可失败存储在发表前准备，resize 只初始化已预留容量。
pub(crate) fn zeroed_bytes(length: usize) -> Result<Vec<u8>, FsError> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(length)
        .map_err(|_| FsError::NoMemory)?;
    bytes.resize(length, 0);
    Ok(bytes)
}

/// wire 长度检查先于字段访问；分片没有重组责任时明确拒绝。
fn checked_ipv4(bytes: &[u8]) -> Result<Ipv4Packet<&[u8]>, FsError> {
    let packet = Ipv4Packet::new_checked(bytes).map_err(|_| FsError::Corrupt)?;
    if packet.version() != 4 || packet.header_len() < 20 {
        return Err(FsError::Corrupt);
    }
    if !packet.verify_checksum() {
        return Err(FsError::Checksum);
    }
    if packet.more_frags() || packet.frag_offset() != 0 {
        return Err(FsError::Unsupported);
    }
    Ok(packet)
}

/// TCP 先检查头长再验证伪头校验和，返回的 payload 仍借原 IPv4 包。
fn parse_tcp<'a>(packet: &Ipv4Packet<&'a [u8]>) -> Result<TcpRepr<'a>, FsError> {
    if packet.next_header() != IpProtocol::Tcp {
        return Err(FsError::Unsupported);
    }
    let tcp = TcpPacket::new_checked(packet.payload()).map_err(|_| FsError::Corrupt)?;
    let source = IpAddress::Ipv4(packet.src_addr());
    let destination = IpAddress::Ipv4(packet.dst_addr());
    if !tcp.verify_checksum(&source, &destination) {
        return Err(FsError::Checksum);
    }
    TcpRepr::parse(
        &tcp,
        &source,
        &destination,
        &ChecksumCapabilities::ignored(),
    )
    .map_err(|_| FsError::Corrupt)
}

/// UDP 长度先校验；IPv4 的零 checksum 表示发送者合法省略校验和。
fn parse_udp(packet: &Ipv4Packet<&[u8]>) -> Result<UdpRepr, FsError> {
    let udp = UdpPacket::new_checked(packet.payload()).map_err(|_| FsError::Corrupt)?;
    let source = IpAddress::Ipv4(packet.src_addr());
    let destination = IpAddress::Ipv4(packet.dst_addr());
    if !udp.verify_checksum(&source, &destination) {
        return Err(FsError::Checksum);
    }
    UdpRepr::parse(
        &udp,
        &source,
        &destination,
        &ChecksumCapabilities::ignored(),
    )
    .map_err(|_| FsError::Corrupt)
}
