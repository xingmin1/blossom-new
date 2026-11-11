use byteorder::{ByteOrder, NetworkEndian};
use core::{cmp, fmt};

use super::{Error, Result};
use crate::phy::ChecksumCapabilities;
use crate::wire::ip::checksum;
use crate::wire::{Ipv4Packet, Ipv4Repr};

enum_with_unknown! {
    /// Internet protocol control message type.
    pub enum Message(u8) {
        /// Echo reply
        EchoReply      =  0,
        /// Destination unreachable
        DstUnreachable =  3,
        /// Message redirect
        Redirect       =  5,
        /// Echo request
        EchoRequest    =  8,
        /// Router advertisement
        RouterAdvert   =  9,
        /// Router solicitation
        RouterSolicit  = 10,
        /// Time exceeded
        TimeExceeded   = 11,
        /// Parameter problem
        ParamProblem   = 12,
        /// Timestamp
        Timestamp      = 13,
        /// Timestamp reply
        TimestampReply = 14
    }
}

impl fmt::Display for Message {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            Message::EchoReply => write!(f, "echo reply"),
            Message::DstUnreachable => write!(f, "destination unreachable"),
            Message::Redirect => write!(f, "message redirect"),
            Message::EchoRequest => write!(f, "echo request"),
            Message::RouterAdvert => write!(f, "router advertisement"),
            Message::RouterSolicit => write!(f, "router solicitation"),
            Message::TimeExceeded => write!(f, "time exceeded"),
            Message::ParamProblem => write!(f, "parameter problem"),
            Message::Timestamp => write!(f, "timestamp"),
            Message::TimestampReply => write!(f, "timestamp reply"),
            Message::Unknown(id) => write!(f, "{id}"),
        }
    }
}

enum_with_unknown! {
    /// Internet protocol control message subtype for type "Destination Unreachable".
    pub enum DstUnreachable(u8) {
        /// Destination network unreachable
        NetUnreachable   =  0,
        /// Destination host unreachable
        HostUnreachable  =  1,
        /// Destination protocol unreachable
        ProtoUnreachable =  2,
        /// Destination port unreachable
        PortUnreachable  =  3,
        /// Fragmentation required, and DF flag set
        FragRequired     =  4,
        /// Source route failed
        SrcRouteFailed   =  5,
        /// Destination network unknown
        DstNetUnknown    =  6,
        /// Destination host unknown
        DstHostUnknown   =  7,
        /// Source host isolated
        SrcHostIsolated  =  8,
        /// Network administratively prohibited
        NetProhibited    =  9,
        /// Host administratively prohibited
        HostProhibited   = 10,
        /// Network unreachable for ToS
        NetUnreachToS    = 11,
        /// Host unreachable for ToS
        HostUnreachToS   = 12,
        /// Communication administratively prohibited
        CommProhibited   = 13,
        /// Host precedence violation
        HostPrecedViol   = 14,
        /// Precedence cutoff in effect
        PrecedCutoff     = 15
    }
}

impl fmt::Display for DstUnreachable {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            DstUnreachable::NetUnreachable => write!(f, "destination network unreachable"),
            DstUnreachable::HostUnreachable => write!(f, "destination host unreachable"),
            DstUnreachable::ProtoUnreachable => write!(f, "destination protocol unreachable"),
            DstUnreachable::PortUnreachable => write!(f, "destination port unreachable"),
            DstUnreachable::FragRequired => write!(f, "fragmentation required, and DF flag set"),
            DstUnreachable::SrcRouteFailed => write!(f, "source route failed"),
            DstUnreachable::DstNetUnknown => write!(f, "destination network unknown"),
            DstUnreachable::DstHostUnknown => write!(f, "destination host unknown"),
            DstUnreachable::SrcHostIsolated => write!(f, "source host isolated"),
            DstUnreachable::NetProhibited => write!(f, "network administratively prohibited"),
            DstUnreachable::HostProhibited => write!(f, "host administratively prohibited"),
            DstUnreachable::NetUnreachToS => write!(f, "network unreachable for ToS"),
            DstUnreachable::HostUnreachToS => write!(f, "host unreachable for ToS"),
            DstUnreachable::CommProhibited => {
                write!(f, "communication administratively prohibited")
            }
            DstUnreachable::HostPrecedViol => write!(f, "host precedence violation"),
            DstUnreachable::PrecedCutoff => write!(f, "precedence cutoff in effect"),
            DstUnreachable::Unknown(id) => write!(f, "{id}"),
        }
    }
}

enum_with_unknown! {
    /// Internet protocol control message subtype for type "Redirect Message".
    pub enum Redirect(u8) {
        /// Redirect Datagram for the Network
        Net     = 0,
        /// Redirect Datagram for the Host
        Host    = 1,
        /// Redirect Datagram for the ToS & network
        NetToS  = 2,
        /// Redirect Datagram for the ToS & host
        HostToS = 3
    }
}

enum_with_unknown! {
    /// Internet protocol control message subtype for type "Time Exceeded".
    pub enum TimeExceeded(u8) {
        /// TTL expired in transit
        TtlExpired  = 0,
        /// Fragment reassembly time exceeded
        FragExpired = 1
    }
}

impl fmt::Display for TimeExceeded {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            TimeExceeded::TtlExpired => write!(f, "time-to-live exceeded in transit"),
            TimeExceeded::FragExpired => write!(f, "fragment reassembly time exceeded"),
            TimeExceeded::Unknown(id) => write!(f, "{id}"),
        }
    }
}

enum_with_unknown! {
    /// Internet protocol control message subtype for type "Parameter Problem".
    pub enum ParamProblem(u8) {
        /// Pointer indicates the error
        AtPointer     = 0,
        /// Missing a required option
        MissingOption = 1,
        /// Bad length
        BadLength     = 2
    }
}

/// A read/write wrapper around an Internet Control Message Protocol version 4 packet buffer.
#[derive(Debug, PartialEq, Eq, Clone)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
pub struct Packet<T: AsRef<[u8]>> {
    buffer: T,
}

mod field {
    use crate::wire::field::*;

    pub const TYPE: usize = 0;
    pub const CODE: usize = 1;
    pub const CHECKSUM: Field = 2..4;

    pub const UNUSED: Field = 4..8;
    pub const DST_UNUSED: Field = 4..6;
    pub const NEXT_HOP_MTU: Field = 6..8;
    pub const POINTER: usize = 4;
    pub const PARAM_UNUSED: Field = 5..8;

    pub const ECHO_IDENT: Field = 4..6;
    pub const ECHO_SEQNO: Field = 6..8;

    pub const HEADER_END: usize = 8;
}

impl<T: AsRef<[u8]>> Packet<T> {
    /// Imbue a raw octet buffer with ICMPv4 packet structure.
    pub const fn new_unchecked(buffer: T) -> Packet<T> {
        Packet { buffer }
    }

    /// Shorthand for a combination of [new_unchecked] and [check_len].
    ///
    /// [new_unchecked]: #method.new_unchecked
    /// [check_len]: #method.check_len
    pub fn new_checked(buffer: T) -> Result<Packet<T>> {
        let packet = Self::new_unchecked(buffer);
        packet.check_len()?;
        Ok(packet)
    }

    /// Ensure that no accessor method will panic if called.
    /// Returns `Err(Error)` if the buffer is too short.
    ///
    /// The result of this check is invalidated by calling [set_header_len].
    ///
    /// [set_header_len]: #method.set_header_len
    pub fn check_len(&self) -> Result<()> {
        let len = self.buffer.as_ref().len();
        if len < field::HEADER_END {
            Err(Error)
        } else {
            Ok(())
        }
    }

    /// Consume the packet, returning the underlying buffer.
    pub fn into_inner(self) -> T {
        self.buffer
    }

    /// Return the message type field.
    #[inline]
    pub fn msg_type(&self) -> Message {
        let data = self.buffer.as_ref();
        Message::from(data[field::TYPE])
    }

    /// Return the message code field.
    #[inline]
    pub fn msg_code(&self) -> u8 {
        let data = self.buffer.as_ref();
        data[field::CODE]
    }

    /// Return the checksum field.
    #[inline]
    pub fn checksum(&self) -> u16 {
        let data = self.buffer.as_ref();
        NetworkEndian::read_u16(&data[field::CHECKSUM])
    }

    /// 返回需要分片错误携带的下一跳 MTU，单位为字节；零表示发送方未提供 MTU。
    #[inline]
    pub fn next_hop_mtu(&self) -> u16 {
        let data = self.buffer.as_ref();
        NetworkEndian::read_u16(&data[field::NEXT_HOP_MTU])
    }

    /// 返回参数问题报文指出的原 IPv4 首部字节偏移。
    #[inline]
    pub fn param_problem_ptr(&self) -> u8 {
        let data = self.buffer.as_ref();
        data[field::POINTER]
    }

    /// Return the identifier field (for echo request and reply packets).
    ///
    /// # Panics
    /// This function may panic if this packet is not an echo request or reply packet.
    #[inline]
    pub fn echo_ident(&self) -> u16 {
        let data = self.buffer.as_ref();
        NetworkEndian::read_u16(&data[field::ECHO_IDENT])
    }

    /// Return the sequence number field (for echo request and reply packets).
    ///
    /// # Panics
    /// This function may panic if this packet is not an echo request or reply packet.
    #[inline]
    pub fn echo_seq_no(&self) -> u16 {
        let data = self.buffer.as_ref();
        NetworkEndian::read_u16(&data[field::ECHO_SEQNO])
    }

    /// Return the header length.
    /// The result depends on the value of the message type field.
    pub fn header_len(&self) -> usize {
        match self.msg_type() {
            Message::EchoRequest => field::ECHO_SEQNO.end,
            Message::EchoReply => field::ECHO_SEQNO.end,
            Message::DstUnreachable => field::UNUSED.end,
            _ => field::UNUSED.end, // make a conservative assumption
        }
    }

    /// Validate the header checksum.
    ///
    /// # Fuzzing
    /// This function always returns `true` when fuzzing.
    pub fn verify_checksum(&self) -> bool {
        if cfg!(fuzzing) {
            return true;
        }

        let data = self.buffer.as_ref();
        checksum::data(data) == !0
    }
}

impl<'a, T: AsRef<[u8]> + ?Sized> Packet<&'a T> {
    /// Return a pointer to the type-specific data.
    #[inline]
    pub fn data(&self) -> &'a [u8] {
        let data = self.buffer.as_ref();
        &data[self.header_len()..]
    }
}

impl<T: AsRef<[u8]> + AsMut<[u8]>> Packet<T> {
    /// Set the message type field.
    #[inline]
    pub fn set_msg_type(&mut self, value: Message) {
        let data = self.buffer.as_mut();
        data[field::TYPE] = value.into()
    }

    /// Set the message code field.
    #[inline]
    pub fn set_msg_code(&mut self, value: u8) {
        let data = self.buffer.as_mut();
        data[field::CODE] = value
    }

    /// Set the checksum field.
    #[inline]
    pub fn set_checksum(&mut self, value: u16) {
        let data = self.buffer.as_mut();
        NetworkEndian::write_u16(&mut data[field::CHECKSUM], value)
    }

    /// 写入需要分片错误携带的下一跳 MTU，保留零值表示未知。
    #[inline]
    pub fn set_next_hop_mtu(&mut self, value: u16) {
        let data = self.buffer.as_mut();
        NetworkEndian::write_u16(&mut data[field::NEXT_HOP_MTU], value)
    }

    /// 写入参数问题报文指出的原 IPv4 首部字节偏移。
    #[inline]
    pub fn set_param_problem_ptr(&mut self, value: u8) {
        let data = self.buffer.as_mut();
        data[field::POINTER] = value;
    }

    /// Set the identifier field (for echo request and reply packets).
    ///
    /// # Panics
    /// This function may panic if this packet is not an echo request or reply packet.
    #[inline]
    pub fn set_echo_ident(&mut self, value: u16) {
        let data = self.buffer.as_mut();
        NetworkEndian::write_u16(&mut data[field::ECHO_IDENT], value)
    }

    /// Set the sequence number field (for echo request and reply packets).
    ///
    /// # Panics
    /// This function may panic if this packet is not an echo request or reply packet.
    #[inline]
    pub fn set_echo_seq_no(&mut self, value: u16) {
        let data = self.buffer.as_mut();
        NetworkEndian::write_u16(&mut data[field::ECHO_SEQNO], value)
    }

    /// Compute and fill in the header checksum.
    pub fn fill_checksum(&mut self) {
        self.set_checksum(0);
        let checksum = {
            let data = self.buffer.as_ref();
            !checksum::data(data)
        };
        self.set_checksum(checksum)
    }
}

impl<'a, T: AsRef<[u8]> + AsMut<[u8]> + ?Sized> Packet<&'a mut T> {
    /// Return a mutable pointer to the type-specific data.
    #[inline]
    pub fn data_mut(&mut self) -> &mut [u8] {
        let range = self.header_len()..;
        let data = self.buffer.as_mut();
        &mut data[range]
    }
}

impl<T: AsRef<[u8]>> AsRef<[u8]> for Packet<T> {
    fn as_ref(&self) -> &[u8] {
        self.buffer.as_ref()
    }
}

/// ICMPv4 首部的高层表示。
///
/// 错误引用的 `header.payload_len` 保存原总长减真实 IHL，`data` 只借用实际携带的传输字节。
/// 编码仍遵循 `Ipv4Repr` 的二十字节首部约定，不保存或逐字重发原 IPv4 选项。
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
#[non_exhaustive]
pub enum Repr<'a> {
    EchoRequest {
        ident: u16,
        seq_no: u16,
        data: &'a [u8],
    },
    EchoReply {
        ident: u16,
        seq_no: u16,
        data: &'a [u8],
    },
    DstUnreachable {
        reason: DstUnreachable,
        /// 下一跳 MTU，单位为字节；仅 FragRequired 使用，零表示未知。
        next_hop_mtu: u16,
        header: Ipv4Repr,
        data: &'a [u8],
    },
    TimeExceeded {
        reason: TimeExceeded,
        header: Ipv4Repr,
        data: &'a [u8],
    },
    ParamProblem {
        reason: ParamProblem,
        pointer: u8,
        header: Ipv4Repr,
        data: &'a [u8],
    },
}

impl<'a> Repr<'a> {
    /// 解析 ICMPv4 报文并校验外层校验和，错误引用仅要求完整 IPv4 首部和八字节传输首部。
    pub fn parse<T>(
        packet: &Packet<&'a T>,
        checksum_caps: &ChecksumCapabilities,
    ) -> Result<Repr<'a>>
    where
        T: AsRef<[u8]> + ?Sized,
    {
        packet.check_len()?;
        if checksum_caps.icmpv4.rx() && !packet.verify_checksum() {
            return Err(Error);
        }

        match (packet.msg_type(), packet.msg_code()) {
            (Message::EchoRequest, 0) => Ok(Repr::EchoRequest {
                ident: packet.echo_ident(),
                seq_no: packet.echo_seq_no(),
                data: packet.data(),
            }),

            (Message::EchoReply, 0) => Ok(Repr::EchoReply {
                ident: packet.echo_ident(),
                seq_no: packet.echo_seq_no(),
                data: packet.data(),
            }),

            (Message::DstUnreachable, code) => {
                let (header, data) = Self::parse_quoted_packet(packet)?;
                Ok(Repr::DstUnreachable {
                    reason: DstUnreachable::from(code),
                    next_hop_mtu: packet.next_hop_mtu(),
                    header,
                    data,
                })
            }

            (Message::TimeExceeded, code) => {
                let (header, data) = Self::parse_quoted_packet(packet)?;
                Ok(Repr::TimeExceeded {
                    reason: TimeExceeded::from(code),
                    header,
                    data,
                })
            }

            (Message::ParamProblem, code) => {
                let (header, data) = Self::parse_quoted_packet(packet)?;
                Ok(Repr::ParamProblem {
                    reason: ParamProblem::from(code),
                    pointer: packet.param_problem_ptr(),
                    header,
                    data,
                })
            }

            _ => Err(Error),
        }
    }

    /// 借用错误报文实际携带的引用，保留原 IPv4 声明的正文长度。
    /// 被引用报文可能因首部错误而触发 ICMP，因此不要求其校验和有效。
    fn parse_quoted_packet<T>(packet: &Packet<&'a T>) -> Result<(Ipv4Repr, &'a [u8])>
    where
        T: AsRef<[u8]> + ?Sized,
    {
        let data = packet.data();
        let ip_packet = Ipv4Packet::new_unchecked(data);
        ip_packet.check_header_len()?;
        if ip_packet.version() != 4 {
            return Err(Error);
        }

        let header_len = ip_packet.header_len() as usize;
        let total_len = ip_packet.total_len() as usize;
        let payload = &data[header_len..cmp::min(data.len(), total_len)];
        if payload.len() < 8 {
            return Err(Error);
        }

        let header = Ipv4Repr {
            src_addr: ip_packet.src_addr(),
            dst_addr: ip_packet.dst_addr(),
            next_header: ip_packet.next_header(),
            payload_len: total_len - header_len,
            hop_limit: ip_packet.hop_limit(),
        };
        Ok((header, payload))
    }

    /// 返回实际编码长度；错误引用的正文可以短于原 IPv4 声明长度。
    pub const fn buffer_len(&self) -> usize {
        match self {
            &Repr::EchoRequest { data, .. } | &Repr::EchoReply { data, .. } => {
                field::ECHO_SEQNO.end + data.len()
            }
            &Repr::DstUnreachable { header, data, .. }
            | &Repr::TimeExceeded { header, data, .. }
            | &Repr::ParamProblem { header, data, .. } => {
                field::UNUSED.end + header.buffer_len() + data.len()
            }
        }
    }

    /// 编码 ICMPv4 报文，保留错误原因、MTU 和参数问题偏移。
    /// 被引 IPv4 首部按既有 `Ipv4Repr` 约定规范化，带选项输入不保证原总长逐字往返。
    pub fn emit<T>(&self, packet: &mut Packet<&mut T>, checksum_caps: &ChecksumCapabilities)
    where
        T: AsRef<[u8]> + AsMut<[u8]> + ?Sized,
    {
        /// 编码被引用的 IPv4 首部，并仅复制实际提供的传输数据。
        fn emit_contained_packet<T>(
            packet: &mut Packet<&mut T>,
            header: Ipv4Repr,
            data: &[u8],
            checksum_caps: &ChecksumCapabilities,
        ) where
            T: AsRef<[u8]> + AsMut<[u8]> + ?Sized,
        {
            let mut ip_packet = Ipv4Packet::new_unchecked(packet.data_mut());
            header.emit(&mut ip_packet, checksum_caps);
            let payload = &mut ip_packet.into_inner()[header.buffer_len()..];
            payload.copy_from_slice(data);
        }

        match *self {
            Repr::EchoRequest {
                ident,
                seq_no,
                data,
            } => {
                packet.set_msg_type(Message::EchoRequest);
                packet.set_msg_code(0);
                packet.set_echo_ident(ident);
                packet.set_echo_seq_no(seq_no);
                let data_len = cmp::min(packet.data_mut().len(), data.len());
                packet.data_mut()[..data_len].copy_from_slice(&data[..data_len])
            }

            Repr::EchoReply {
                ident,
                seq_no,
                data,
            } => {
                packet.set_msg_type(Message::EchoReply);
                packet.set_msg_code(0);
                packet.set_echo_ident(ident);
                packet.set_echo_seq_no(seq_no);
                let data_len = cmp::min(packet.data_mut().len(), data.len());
                packet.data_mut()[..data_len].copy_from_slice(&data[..data_len])
            }

            Repr::DstUnreachable {
                reason,
                next_hop_mtu,
                header,
                data,
            } => {
                packet.set_msg_type(Message::DstUnreachable);
                packet.set_msg_code(reason.into());

                packet.buffer.as_mut()[field::DST_UNUSED].fill(0);
                packet.set_next_hop_mtu(next_hop_mtu);
                emit_contained_packet(packet, header, data, checksum_caps);
            }

            Repr::TimeExceeded {
                reason,
                header,
                data,
            } => {
                packet.set_msg_type(Message::TimeExceeded);
                packet.set_msg_code(reason.into());

                packet.buffer.as_mut()[field::UNUSED].fill(0);
                emit_contained_packet(packet, header, data, checksum_caps);
            }

            Repr::ParamProblem {
                reason,
                pointer,
                header,
                data,
            } => {
                packet.set_msg_type(Message::ParamProblem);
                packet.set_msg_code(reason.into());
                packet.set_param_problem_ptr(pointer);
                packet.buffer.as_mut()[field::PARAM_UNUSED].fill(0);
                emit_contained_packet(packet, header, data, checksum_caps);
            }
        }

        if checksum_caps.icmpv4.tx() {
            packet.fill_checksum()
        } else {
            // make sure we get a consistently zeroed checksum,
            // since implementations might rely on it
            packet.set_checksum(0);
        }
    }
}

impl<'a, T: AsRef<[u8]> + ?Sized> fmt::Display for Packet<&'a T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match Repr::parse(self, &ChecksumCapabilities::default()) {
            Ok(repr) => write!(f, "{repr}"),
            Err(err) => {
                write!(f, "ICMPv4 ({err})")?;
                write!(f, " type={:?}", self.msg_type())?;
                match self.msg_type() {
                    Message::DstUnreachable => {
                        write!(f, " code={:?}", DstUnreachable::from(self.msg_code()))
                    }
                    Message::TimeExceeded => {
                        write!(f, " code={:?}", TimeExceeded::from(self.msg_code()))
                    }
                    _ => write!(f, " code={}", self.msg_code()),
                }
            }
        }
    }
}

impl<'a> fmt::Display for Repr<'a> {
    /// 格式化 ICMPv4 语义，保留错误类型与对应的原始原因。
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match *self {
            Repr::EchoRequest {
                ident,
                seq_no,
                data,
            } => write!(
                f,
                "ICMPv4 echo request id={} seq={} len={}",
                ident,
                seq_no,
                data.len()
            ),
            Repr::EchoReply {
                ident,
                seq_no,
                data,
            } => write!(
                f,
                "ICMPv4 echo reply id={} seq={} len={}",
                ident,
                seq_no,
                data.len()
            ),
            Repr::DstUnreachable { reason, .. } => {
                write!(f, "ICMPv4 destination unreachable ({reason})")
            }
            Repr::TimeExceeded { reason, .. } => {
                write!(f, "ICMPv4 time exceeded ({reason})")
            }
            Repr::ParamProblem { reason, .. } => {
                write!(f, "ICMPv4 parameter problem ({reason:?})")
            }
        }
    }
}

use crate::wire::pretty_print::{PrettyIndent, PrettyPrint};

impl<T: AsRef<[u8]>> PrettyPrint for Packet<T> {
    /// 展示 ICMPv4 报文，并继续展示支持的错误引用中的 IPv4 首部。
    fn pretty_print(
        buffer: &dyn AsRef<[u8]>,
        f: &mut fmt::Formatter,
        indent: &mut PrettyIndent,
    ) -> fmt::Result {
        let packet = match Packet::new_checked(buffer) {
            Err(err) => return write!(f, "{indent}({err})"),
            Ok(packet) => packet,
        };
        write!(f, "{indent}{packet}")?;

        match packet.msg_type() {
            Message::DstUnreachable | Message::TimeExceeded | Message::ParamProblem => {
                indent.increase(f)?;
                super::Ipv4Packet::<&[u8]>::pretty_print(&packet.data(), f, indent)
            }
            _ => Ok(()),
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    static ECHO_PACKET_BYTES: [u8; 12] = [
        0x08, 0x00, 0x8e, 0xfe, 0x12, 0x34, 0xab, 0xcd, 0xaa, 0x00, 0x00, 0xff,
    ];

    static ECHO_DATA_BYTES: [u8; 4] = [0xaa, 0x00, 0x00, 0xff];

    #[test]
    fn test_echo_deconstruct() {
        let packet = Packet::new_unchecked(&ECHO_PACKET_BYTES[..]);
        assert_eq!(packet.msg_type(), Message::EchoRequest);
        assert_eq!(packet.msg_code(), 0);
        assert_eq!(packet.checksum(), 0x8efe);
        assert_eq!(packet.echo_ident(), 0x1234);
        assert_eq!(packet.echo_seq_no(), 0xabcd);
        assert_eq!(packet.data(), &ECHO_DATA_BYTES[..]);
        assert!(packet.verify_checksum());
    }

    #[test]
    fn test_echo_construct() {
        let mut bytes = vec![0xa5; 12];
        let mut packet = Packet::new_unchecked(&mut bytes);
        packet.set_msg_type(Message::EchoRequest);
        packet.set_msg_code(0);
        packet.set_echo_ident(0x1234);
        packet.set_echo_seq_no(0xabcd);
        packet.data_mut().copy_from_slice(&ECHO_DATA_BYTES[..]);
        packet.fill_checksum();
        assert_eq!(&packet.into_inner()[..], &ECHO_PACKET_BYTES[..]);
    }

    fn echo_packet_repr() -> Repr<'static> {
        Repr::EchoRequest {
            ident: 0x1234,
            seq_no: 0xabcd,
            data: &ECHO_DATA_BYTES,
        }
    }

    #[test]
    fn test_echo_parse() {
        let packet = Packet::new_unchecked(&ECHO_PACKET_BYTES[..]);
        let repr = Repr::parse(&packet, &ChecksumCapabilities::default()).unwrap();
        assert_eq!(repr, echo_packet_repr());
    }

    #[test]
    fn test_echo_emit() {
        let repr = echo_packet_repr();
        let mut bytes = vec![0xa5; repr.buffer_len()];
        let mut packet = Packet::new_unchecked(&mut bytes);
        repr.emit(&mut packet, &ChecksumCapabilities::default());
        assert_eq!(&packet.into_inner()[..], &ECHO_PACKET_BYTES[..]);
    }

    #[test]
    fn test_check_len() {
        let bytes = [0x0b, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00];
        assert_eq!(Packet::new_checked(&[]), Err(Error));
        assert_eq!(Packet::new_checked(&bytes[..4]), Err(Error));
        assert!(Packet::new_checked(&bytes[..]).is_ok());
    }

    /// 构造原长为五百三十二字节、仅引用 UDP 首部及四字节正文的 ICMP 错误。
    fn quoted_udp_error(msg_type: Message, code: u8) -> [u8; 40] {
        let mut bytes = [
            0x03, 0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x45, 0x00, 0x02, 0x14, 0x12, 0x34,
            0x40, 0x00, 0x40, 0x11, 0x00, 0x00, 192, 0, 2, 1, 198, 51, 100, 2, 0xc0, 0x00, 0x00,
            0x35, 0x02, 0x00, 0x12, 0x34, 0xde, 0xad, 0xbe, 0xef,
        ];
        Ipv4Packet::new_unchecked(&mut bytes[8..]).fill_checksum();
        let mut packet = Packet::new_unchecked(&mut bytes[..]);
        packet.set_msg_type(msg_type);
        packet.set_msg_code(code);
        packet.fill_checksum();
        bytes
    }

    /// 合法截短引用保留原 IPv4 长度及地址，并无分配借用实际 UDP 正文。
    #[test]
    fn test_error_quote_truncated_udp() {
        let bytes = quoted_udp_error(Message::DstUnreachable, 3);
        let packet = Packet::new_checked(&bytes[..]).unwrap();
        assert_eq!(Ipv4Packet::new_checked(packet.data()), Err(Error));

        let repr = Repr::parse(&packet, &ChecksumCapabilities::default()).unwrap();
        let (header, data) = match repr {
            Repr::DstUnreachable {
                reason: DstUnreachable::PortUnreachable,
                next_hop_mtu: 0,
                header,
                data,
            } => (header, data),
            _ => panic!("unexpected ICMP representation"),
        };
        assert_eq!(header.src_addr, crate::wire::Ipv4Address::new(192, 0, 2, 1));
        assert_eq!(
            header.dst_addr,
            crate::wire::Ipv4Address::new(198, 51, 100, 2)
        );
        assert_eq!(header.next_header, crate::wire::IpProtocol::Udp);
        assert_eq!(header.payload_len, 512);
        assert_eq!(header.hop_limit, 64);
        assert_eq!(data.len(), 12);
        assert_eq!(data.as_ptr(), bytes[28..].as_ptr());

        let (udp, payload) = crate::wire::UdpRepr::parse_quote(data).unwrap();
        assert_eq!(udp.src_port, 49152);
        assert_eq!(udp.dst_port, 53);
        assert_eq!(payload, &[0xde, 0xad, 0xbe, 0xef]);
        assert_eq!(payload.as_ptr(), bytes[36..].as_ptr());
    }

    /// 所有不足 IPv4 首部加八字节传输首部的引用均拒绝，最短合法引用的正文为空。
    #[test]
    fn test_error_quote_minimum_and_short_headers() {
        let mut bytes = quoted_udp_error(Message::DstUnreachable, 3);
        for quote_len in 0..28 {
            let mut packet = Packet::new_unchecked(&mut bytes[..8 + quote_len]);
            packet.fill_checksum();
            let packet = Packet::new_unchecked(&*packet.into_inner());
            assert_eq!(
                Repr::parse(&packet, &ChecksumCapabilities::default()),
                Err(Error)
            );
        }

        let mut packet = Packet::new_unchecked(&mut bytes[..36]);
        packet.fill_checksum();
        let packet = Packet::new_unchecked(&*packet.into_inner());
        match Repr::parse(&packet, &ChecksumCapabilities::default()).unwrap() {
            Repr::DstUnreachable { header, data, .. } => {
                assert_eq!(header.payload_len, 512);
                assert_eq!(data.len(), 8);
                assert!(crate::wire::UdpRepr::parse_quote(data)
                    .unwrap()
                    .1
                    .is_empty());
            }
            _ => panic!("unexpected ICMP representation"),
        }
    }

    /// 原 IHL 包含选项时，引用正文从完整首部之后开始，原声明长度仍独立保存。
    #[test]
    fn test_error_quote_ipv4_options_and_original_length() {
        let base = quoted_udp_error(Message::DstUnreachable, 3);
        let mut bytes = [0u8; 44];
        bytes[..28].copy_from_slice(&base[..28]);
        bytes[28..32].copy_from_slice(&[1, 1, 1, 0]);
        bytes[32..].copy_from_slice(&base[28..]);
        let mut ip_packet = Ipv4Packet::new_unchecked(&mut bytes[8..]);
        ip_packet.set_header_len(24);
        ip_packet.set_total_len(536);
        ip_packet.fill_checksum();
        Packet::new_unchecked(&mut bytes[..]).fill_checksum();

        let packet = Packet::new_unchecked(&bytes[..]);
        match Repr::parse(&packet, &ChecksumCapabilities::default()).unwrap() {
            Repr::DstUnreachable { header, data, .. } => {
                assert_eq!(header.payload_len, 512);
                assert_eq!(data, &base[28..]);
                assert_eq!(data.as_ptr(), bytes[32..].as_ptr());
            }
            _ => panic!("unexpected ICMP representation"),
        }
        let quoted = Ipv4Packet::new_unchecked(packet.data());
        assert_eq!(quoted.header_len(), 24);
        assert_eq!(quoted.total_len(), 536);
    }

    /// 原 IPv4 总长限制有效引用范围，额外字节不能被当作原报文正文。
    #[test]
    fn test_error_quote_bounds_original_total_len() {
        let mut bytes = quoted_udp_error(Message::DstUnreachable, 3);
        let mut ip_packet = Ipv4Packet::new_unchecked(&mut bytes[8..]);
        ip_packet.set_total_len(30);
        ip_packet.fill_checksum();
        Packet::new_unchecked(&mut bytes[..]).fill_checksum();
        let packet = Packet::new_unchecked(&bytes[..]);
        match Repr::parse(&packet, &ChecksumCapabilities::default()).unwrap() {
            Repr::DstUnreachable { header, data, .. } => {
                assert_eq!(header.payload_len, 10);
                assert_eq!(data, &bytes[28..38]);
            }
            _ => panic!("unexpected ICMP representation"),
        }

        Ipv4Packet::new_unchecked(&mut bytes[8..]).set_total_len(27);
        Packet::new_unchecked(&mut bytes[..]).fill_checksum();
        let packet = Packet::new_unchecked(&bytes[..]);
        assert_eq!(
            Repr::parse(&packet, &ChecksumCapabilities::default()),
            Err(Error)
        );
    }

    /// 截短引用不放宽外层校验和与内层 IPv4 版本检查。
    #[test]
    fn test_error_quote_checks_outer_checksum_and_version() {
        let mut bytes = quoted_udp_error(Message::DstUnreachable, 3);
        bytes[39] ^= 1;
        let packet = Packet::new_unchecked(&bytes[..]);
        assert_eq!(
            Repr::parse(&packet, &ChecksumCapabilities::default()),
            Err(Error)
        );

        Packet::new_unchecked(&mut bytes[..]).fill_checksum();
        let packet = Packet::new_unchecked(&bytes[..]);
        assert!(Repr::parse(&packet, &ChecksumCapabilities::default()).is_ok());
        Ipv4Packet::new_unchecked(&mut bytes[8..]).set_version(6);
        Packet::new_unchecked(&mut bytes[..]).fill_checksum();
        let packet = Packet::new_unchecked(&bytes[..]);
        assert_eq!(
            Repr::parse(&packet, &ChecksumCapabilities::default()),
            Err(Error)
        );
    }

    /// 需要分片错误的 MTU 按真实网络字节序解析并编码，未知零值同样保留。
    #[test]
    fn test_frag_required_mtu_roundtrip() {
        for mtu in [0u16, 1400] {
            let mut bytes = quoted_udp_error(Message::DstUnreachable, 4);
            bytes[6..8].copy_from_slice(&mtu.to_be_bytes());
            Packet::new_unchecked(&mut bytes[..]).fill_checksum();
            let packet = Packet::new_unchecked(&bytes[..]);
            let repr = Repr::parse(&packet, &ChecksumCapabilities::default()).unwrap();
            match repr {
                Repr::DstUnreachable {
                    reason: DstUnreachable::FragRequired,
                    next_hop_mtu,
                    ..
                } => assert_eq!(next_hop_mtu, mtu),
                _ => panic!("unexpected ICMP representation"),
            }

            let mut emitted = [0xa5u8; 40];
            let mut packet = Packet::new_unchecked(&mut emitted[..]);
            repr.emit(&mut packet, &ChecksumCapabilities::default());
            assert_eq!(packet.next_hop_mtu(), mtu);
            assert_eq!(&packet.as_ref()[4..6], &[0, 0]);
            assert_eq!(&packet.as_ref()[6..8], &mtu.to_be_bytes());
            assert!(packet.verify_checksum());
            let packet = Packet::new_unchecked(&*packet.into_inner());
            assert_eq!(
                Repr::parse(&packet, &ChecksumCapabilities::default()),
                Ok(repr)
            );
        }
    }

    /// 时间超限与参数问题使用同一截短引用规则，并保留原始类型、原因和偏移。
    #[test]
    fn test_time_exceeded_and_param_problem_roundtrip() {
        for (msg_type, code, pointer) in
            [(Message::TimeExceeded, 0, 0), (Message::ParamProblem, 2, 9)]
        {
            let mut bytes = quoted_udp_error(msg_type, code);
            bytes[4] = pointer;
            Packet::new_unchecked(&mut bytes[..]).fill_checksum();
            let packet = Packet::new_unchecked(&bytes[..]);
            let repr = Repr::parse(&packet, &ChecksumCapabilities::default()).unwrap();
            match repr {
                Repr::TimeExceeded {
                    reason: TimeExceeded::TtlExpired,
                    header,
                    data,
                } => {
                    assert_eq!(header.payload_len, 512);
                    assert_eq!(data.len(), 12);
                }
                Repr::ParamProblem {
                    reason: ParamProblem::BadLength,
                    pointer: 9,
                    header,
                    data,
                } => {
                    assert_eq!(header.payload_len, 512);
                    assert_eq!(data.len(), 12);
                }
                _ => panic!("unexpected ICMP representation"),
            }

            let mut emitted = [0xa5u8; 40];
            let mut packet = Packet::new_unchecked(&mut emitted[..]);
            repr.emit(&mut packet, &ChecksumCapabilities::default());
            assert_eq!(packet.msg_type(), msg_type);
            assert_eq!(packet.msg_code(), code);
            assert_eq!(packet.param_problem_ptr(), pointer);
            assert!(packet.verify_checksum());
            let packet = Packet::new_unchecked(&*packet.into_inner());
            assert_eq!(
                Repr::parse(&packet, &ChecksumCapabilities::default()),
                Ok(repr)
            );
        }
    }

    /// Source Quench 和 Redirect 不进入可交付的 ICMP 错误表示。
    #[test]
    fn test_source_quench_and_redirect_not_errors() {
        for msg_type in [Message::Unknown(4), Message::Redirect] {
            let bytes = quoted_udp_error(msg_type, 0);
            let packet = Packet::new_unchecked(&bytes[..]);
            assert_eq!(
                Repr::parse(&packet, &ChecksumCapabilities::default()),
                Err(Error)
            );
        }
    }
}
