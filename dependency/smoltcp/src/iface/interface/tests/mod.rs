#[cfg(feature = "proto-ipv4")]
mod ipv4;
#[cfg(feature = "proto-ipv6")]
mod ipv6;
#[cfg(feature = "proto-sixlowpan")]
mod sixlowpan;

#[allow(unused)]
use std::vec::Vec;

use crate::tests::setup;

use rstest::*;

use super::*;

use crate::iface::Interface;
use crate::phy::ChecksumCapabilities;
#[cfg(feature = "alloc")]
use crate::phy::Loopback;
use crate::time::Instant;

#[allow(unused)]
fn fill_slice(s: &mut [u8], val: u8) {
    for x in s.iter_mut() {
        *x = val
    }
}

#[allow(unused)]
fn recv_all(device: &mut crate::tests::TestingDevice, timestamp: Instant) -> Vec<Vec<u8>> {
    let mut pkts = Vec::new();
    while let Some(pkt) = device.tx_queue.pop_front() {
        pkts.push(pkt)
    }
    pkts
}

#[derive(Debug, PartialEq)]
#[cfg_attr(feature = "defmt", derive(defmt::Format))]
struct MockTxToken;

impl TxToken for MockTxToken {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut junk = [0; 1536];
        f(&mut junk[..len])
    }
}

#[test]
#[should_panic(expected = "The hardware address does not match the medium of the interface.")]
#[cfg(all(feature = "medium-ip", feature = "medium-ethernet", feature = "alloc"))]
/// 预备后备不改变原介质不匹配的拒绝。
fn test_new_panic() {
    let mut device = Loopback::new(Medium::Ethernet);
    let config = Config::new(HardwareAddress::Ip);
    let buffers = InterfaceBuffers::try_new(
        crate::config::FRAGMENTATION_BUFFER_SIZE,
        crate::config::REASSEMBLY_BUFFER_SIZE,
    )
    .unwrap();
    Interface::new(config, &mut device, Instant::ZERO, buffers);
}

#[cfg(feature = "socket-udp")]
#[rstest]
#[case::ip(Medium::Ip)]
#[cfg(feature = "medium-ip")]
#[case::ethernet(Medium::Ethernet)]
#[cfg(feature = "medium-ethernet")]
#[case::ieee802154(Medium::Ieee802154)]
#[cfg(feature = "medium-ieee802154")]
/// 原 UDP 广播接纳保持实际源地址和完整消息。
fn test_handle_udp_broadcast(#[case] medium: Medium) {
    use crate::socket::udp;
    use crate::wire::IpEndpoint;

    static UDP_PAYLOAD: [u8; 5] = [0x48, 0x65, 0x6c, 0x6c, 0x6f];

    let (mut iface, mut sockets, _device) = setup(medium);

    let rx_buffer = udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY], vec![0; 15]);
    let tx_buffer = udp::PacketBuffer::new(vec![udp::PacketMetadata::EMPTY], vec![0; 15]);

    let udp_socket = udp::Socket::new(rx_buffer, tx_buffer);

    let mut udp_bytes = vec![0u8; 13];
    let mut packet = UdpPacket::new_unchecked(&mut udp_bytes);

    let socket_handle = sockets.add(udp_socket);

    #[cfg(feature = "proto-ipv6")]
    let src_ip = Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 1);
    #[cfg(all(not(feature = "proto-ipv6"), feature = "proto-ipv4"))]
    let src_ip = Ipv4Address::new(0x7f, 0x00, 0x00, 0x02);

    let udp_repr = UdpRepr {
        src_port: 67,
        dst_port: 68,
    };

    #[cfg(feature = "proto-ipv6")]
    let ip_repr = IpRepr::Ipv6(Ipv6Repr {
        src_addr: src_ip,
        dst_addr: IPV6_LINK_LOCAL_ALL_NODES,
        next_header: IpProtocol::Udp,
        payload_len: udp_repr.header_len() + UDP_PAYLOAD.len(),
        hop_limit: 0x40,
    });
    #[cfg(all(not(feature = "proto-ipv6"), feature = "proto-ipv4"))]
    let ip_repr = IpRepr::Ipv4(Ipv4Repr {
        src_addr: src_ip,
        dst_addr: Ipv4Address::BROADCAST,
        next_header: IpProtocol::Udp,
        payload_len: udp_repr.header_len() + UDP_PAYLOAD.len(),
        hop_limit: 0x40,
    });
    let dst_addr = ip_repr.dst_addr();

    // Bind the socket to port 68
    let socket = sockets.get_mut::<udp::Socket>(socket_handle);
    assert_eq!(socket.bind(68), Ok(()));
    assert!(!socket.can_recv());
    assert!(socket.can_send());

    udp_repr.emit(
        &mut packet,
        &ip_repr.src_addr(),
        &ip_repr.dst_addr(),
        UDP_PAYLOAD.len(),
        |buf| buf.copy_from_slice(&UDP_PAYLOAD),
        &ChecksumCapabilities::default(),
    );

    // Packet should be handled by bound UDP socket
    assert_eq!(
        iface.inner.process_udp(
            &mut sockets,
            PacketMeta::default(),
            false,
            ip_repr,
            packet.into_inner(),
        ),
        None
    );

    // Make sure the payload to the UDP packet processed by process_udp is
    // appended to the bound sockets rx_buffer
    let socket = sockets.get_mut::<udp::Socket>(socket_handle);
    assert!(socket.can_recv());
    assert_eq!(
        socket.recv(),
        Ok((
            &UDP_PAYLOAD[..],
            udp::UdpMetadata {
                local_address: Some(dst_addr),
                ..IpEndpoint::new(src_ip.into(), 67).into()
            }
        ))
    );
}

#[test]
#[cfg(all(feature = "medium-ip", feature = "socket-tcp", feature = "proto-ipv6"))]
/// 原未匹配 TCP 沿正常 RST 拒绝并保传输输入 metadata。
pub fn tcp_not_accepted() {
    let (mut iface, mut sockets, _) = setup(Medium::Ip);
    let tcp = TcpRepr {
        src_port: 4242,
        dst_port: 4243,
        control: TcpControl::Syn,
        seq_number: TcpSeqNumber(-10001),
        ack_number: None,
        window_len: 256,
        window_scale: None,
        max_seg_size: None,
        sack_permitted: false,
        sack_ranges: [None, None, None],
        timestamp: None,
        payload: &[],
    };

    let mut tcp_bytes = vec![0u8; tcp.buffer_len()];

    tcp.emit(
        &mut TcpPacket::new_unchecked(&mut tcp_bytes),
        &Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 2).into(),
        &Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 1).into(),
        &ChecksumCapabilities::default(),
    );

    assert_eq!(
        iface.inner.process_tcp(
            &mut sockets,
            PacketMeta::default(),
            IpRepr::Ipv6(Ipv6Repr {
                src_addr: Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 2),
                dst_addr: Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 1),
                next_header: IpProtocol::Tcp,
                payload_len: tcp.buffer_len(),
                hop_limit: 64,
            }),
            &tcp_bytes,
        ),
        Some(Packet::new_ipv6(
            Ipv6Repr {
                src_addr: Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 1),
                dst_addr: Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 2),
                next_header: IpProtocol::Tcp,
                payload_len: tcp.buffer_len(),
                hop_limit: 64,
            },
            IpPayload::Tcp(TcpRepr {
                src_port: 4243,
                dst_port: 4242,
                control: TcpControl::Rst,
                seq_number: TcpSeqNumber(0),
                ack_number: Some(TcpSeqNumber(-10000)),
                window_len: 0,
                window_scale: None,
                max_seg_size: None,
                sack_permitted: false,
                sack_ranges: [None, None, None],
                timestamp: None,
                payload: &[],
            })
        ))
    );
    // Unspecified destination address.
    tcp.emit(
        &mut TcpPacket::new_unchecked(&mut tcp_bytes),
        &Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 2).into(),
        &Ipv6Address::UNSPECIFIED.into(),
        &ChecksumCapabilities::default(),
    );

    assert_eq!(
        iface.inner.process_tcp(
            &mut sockets,
            PacketMeta::default(),
            IpRepr::Ipv6(Ipv6Repr {
                src_addr: Ipv6Address::new(0xfe80, 0, 0, 0, 0, 0, 0, 2),
                dst_addr: Ipv6Address::UNSPECIFIED,
                next_header: IpProtocol::Tcp,
                payload_len: tcp.buffer_len(),
                hop_limit: 64,
            }),
            &tcp_bytes,
        ),
        None,
    );
}

#[cfg(all(
    feature = "alloc",
    feature = "medium-ip",
    feature = "proto-ipv4",
    feature = "proto-ipv4-fragmentation",
    feature = "socket-udp"
))]
mod prepared_ingress {
    use crate::iface::{
        Config, DispatchError, Interface, InterfaceBuffers, InterfaceInner, IpPayload, Packet,
        SocketSet, TransportIngress, TransportResponse,
    };
    use crate::phy::{ChecksumCapabilities, Device, DeviceCapabilities, Medium, PacketMeta};
    use crate::tests::TestingDevice;
    use crate::time::Instant;
    use crate::wire::*;
    use alloc::vec::Vec;

    struct OneFrame(TestingDevice);

    impl Device for OneFrame {
        type RxToken<'a> = <TestingDevice as Device>::RxToken<'a>;
        type TxToken<'a> = <TestingDevice as Device>::TxToken<'a>;

        /// 单 frame 软件槽如实报告一次只能接纳一个输出。
        fn capabilities(&self) -> DeviceCapabilities {
            let mut caps = self.0.capabilities();
            caps.max_burst_size = Some(1);
            caps
        }

        /// 控制应答无可保活槽位时不接纳输入。
        fn receive(
            &mut self,
            timestamp: Instant,
        ) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
            if !self.0.tx_queue.is_empty() {
                return None;
            }
            self.0.receive(timestamp)
        }

        /// 没有实际槽位时不给 token，分片状态不得推进。
        fn transmit(&mut self, timestamp: Instant) -> Option<Self::TxToken<'_>> {
            if !self.0.tx_queue.is_empty() {
                return None;
            }
            self.0.transmit(timestamp)
        }
    }

    struct Capture {
        bytes: Vec<u8>,
        ip_len: usize,
        local: Option<IpAddress>,
    }

    impl<'s> TransportIngress<'s> for Capture {
        #[cfg(feature = "socket-tcp")]
        /// 本测试未注册 TCP，拒绝应答仍由库产生。
        fn tcp<'p>(
            &mut self,
            _: &mut InterfaceInner,
            _: PacketMeta,
            _: &IpRepr,
            _: &TcpRepr<'p>,
        ) -> TransportResponse<'p> {
            TransportResponse::Unmatched
        }

        /// 在已预备容量中同步捕获，不保存 assembler 借用。
        fn udp(
            &mut self,
            _: &mut InterfaceInner,
            _: PacketMeta,
            ip: &IpRepr,
            _: &UdpRepr,
            payload: &[u8],
        ) -> bool {
            assert!(payload.len() <= self.bytes.capacity());
            self.bytes.extend_from_slice(payload);
            self.ip_len = ip.buffer_len();
            true
        }

        /// 无用户 socket 视图仍保留接口控制算法。
        fn socket_set(&mut self) -> Option<&mut SocketSet<'s>> {
            None
        }

        /// 本轮目的快照不进入接口地址配置。
        fn local_destination(&self) -> Option<IpAddress> {
            self.local
        }
    }

    /// 构造有限后备，loopback 子网只存在于该接口地址表。
    fn interface(device: &mut OneFrame, capacity: usize) -> Interface {
        let buffers = InterfaceBuffers::try_new(capacity, capacity).unwrap();
        let mut iface = Interface::new(
            Config::new(HardwareAddress::Ip),
            device,
            Instant::ZERO,
            buffers,
        );
        iface.update_ip_addrs(|addresses| {
            addresses
                .push(IpCidr::new(IpAddress::v4(127, 0, 0, 1), 8))
                .unwrap();
        });
        iface
    }

    /// 构造合法 UDP 表示，线格式始终由库编码。
    fn datagram<'a>(payload: &'a [u8], destination: Ipv4Address) -> Packet<'a> {
        Packet::new(
            IpRepr::Ipv4(Ipv4Repr {
                src_addr: Ipv4Address::new(127, 0, 0, 2),
                dst_addr: destination,
                next_header: IpProtocol::Udp,
                payload_len: payload.len() + 8,
                hop_limit: 64,
            }),
            IpPayload::Udp(
                UdpRepr {
                    src_port: 1200,
                    dst_port: 1300,
                },
                payload,
            ),
        )
    }

    #[test]
    /// 满 TX 保完整尾片，全部原片重组后交出一个完整 UDP payload。
    fn fragmentation_full_tx_and_owned_ingress() {
        let now = Instant::from_millis(7);
        let mut sender = OneFrame(TestingDevice::new(Medium::Ip));
        let mut iface = interface(&mut sender, 65536);
        let payload: Vec<u8> = (0..8192).map(|index| (index % 251) as u8).collect();
        let destination = Ipv4Address::new(127, 0, 0, 1);
        let token = sender.transmit(now).unwrap();
        iface
            .dispatch_ip(
                now,
                token,
                PacketMeta::default(),
                datagram(&payload, destination),
            )
            .unwrap();
        assert_eq!(iface.last_transmit_destination(), Some(destination.into()));
        let sent = iface.fragmenter.sent_bytes;
        iface.poll_fragmentation(now, &mut sender);
        assert_eq!(iface.fragmenter.sent_bytes, sent);
        assert!(iface.fragmentation_pending());
        let first = sender.0.tx_queue.pop_front().unwrap();
        let token = sender.transmit(now).unwrap();
        assert_eq!(
            iface.dispatch_ip(
                now,
                token,
                PacketMeta::default(),
                datagram(&payload, destination)
            ),
            Err(DispatchError::Exhausted)
        );
        assert!(sender.0.tx_queue.is_empty());
        assert_eq!(iface.fragmenter.sent_bytes, sent);
        sender.0.tx_queue.push_back(first);

        let mut frames = Vec::new();
        while iface.fragmentation_pending() {
            frames.push(sender.0.tx_queue.pop_front().unwrap());
            iface.poll_fragmentation(now, &mut sender);
            assert_eq!(iface.last_transmit_destination(), Some(destination.into()));
        }
        frames.push(sender.0.tx_queue.pop_front().unwrap());
        let mut receiver = OneFrame(TestingDevice::new(Medium::Ip));
        let mut received = interface(&mut receiver, 65536);
        let mut capture = Capture {
            bytes: Vec::with_capacity(payload.len()),
            ip_len: 0,
            local: None,
        };
        for frame in frames {
            receiver.0.rx_queue.push_back(frame);
            received.poll_ingress_single(now, &mut receiver, &mut capture);
        }
        assert_eq!(capture.bytes, payload);
        assert_eq!(capture.ip_len, payload.len() + 28);
        assert_eq!(received.maintenance_deadline(), None);
        assert!(receiver.0.tx_queue.is_empty());
    }

    #[test]
    /// 超工作区明确拒绝，非本地址只按本次已核目的快照接纳。
    fn workspace_error_and_transient_destination() {
        let mut device = OneFrame(TestingDevice::new(Medium::Ip));
        let mut iface = interface(&mut device, 1500);
        let payload = vec![3; 4096];
        let token = device.transmit(Instant::ZERO).unwrap();
        assert_eq!(
            iface.dispatch_ip(
                Instant::ZERO,
                token,
                PacketMeta::default(),
                datagram(&payload, Ipv4Address::LOCALHOST)
            ),
            Err(DispatchError::PacketTooLarge)
        );
        assert!(device.0.tx_queue.is_empty());
        assert!(!iface.fragmentation_pending());
        assert_eq!(iface.last_transmit_destination(), None);
        let destination = Ipv4Address::new(192, 168, 1, 5);
        let token = device.transmit(Instant::ZERO).unwrap();
        iface
            .dispatch_ip(
                Instant::ZERO,
                token,
                PacketMeta::default(),
                datagram(b"local", destination),
            )
            .unwrap();
        let frame = device.0.tx_queue.pop_front().unwrap();
        device.0.rx_queue.push_back(frame);
        let mut capture = Capture {
            bytes: Vec::with_capacity(5),
            ip_len: 0,
            local: Some(destination.into()),
        };
        iface.poll_ingress_single(Instant::ZERO, &mut device, &mut capture);
        assert_eq!(capture.bytes, b"local");
        assert!(!iface.is_local(destination.into()));
        assert!(iface.is_local(IpAddress::v4(127, 5, 6, 7)));
        assert_eq!(
            iface.resolve_route(destination.into(), IpAddress::v4(0, 0, 0, 0)),
            Err(DispatchError::NoRoute)
        );
    }

    #[test]
    /// 不存在用户 ICMP socket 时 echo 控制仍由原接口回应。
    fn icmp_control_without_socket_set() {
        let mut device = OneFrame(TestingDevice::new(Medium::Ip));
        let mut iface = interface(&mut device, 1500);
        let echo = Icmpv4Repr::EchoRequest {
            ident: 17,
            seq_no: 23,
            data: b"control",
        };
        let request = Packet::new(
            IpRepr::Ipv4(Ipv4Repr {
                src_addr: Ipv4Address::new(127, 0, 0, 2),
                dst_addr: Ipv4Address::LOCALHOST,
                next_header: IpProtocol::Icmp,
                payload_len: echo.buffer_len(),
                hop_limit: 64,
            }),
            IpPayload::Icmpv4(echo),
        );
        let token = device.transmit(Instant::ZERO).unwrap();
        iface
            .dispatch_ip(Instant::ZERO, token, PacketMeta::default(), request)
            .unwrap();
        let frame = device.0.tx_queue.pop_front().unwrap();
        device.0.rx_queue.push_back(frame);
        let mut capture = Capture {
            bytes: Vec::new(),
            ip_len: 0,
            local: None,
        };
        iface.poll_ingress_single(Instant::ZERO, &mut device, &mut capture);
        assert_eq!(
            iface.last_transmit_destination(),
            Some(IpAddress::v4(127, 0, 0, 2))
        );
        let reply = device.0.tx_queue.pop_front().unwrap();
        let ip = Ipv4Packet::new_checked(&reply[..]).unwrap();
        let icmp = Icmpv4Packet::new_checked(ip.payload()).unwrap();
        assert_eq!(
            Icmpv4Repr::parse(&icmp, &ChecksumCapabilities::default()).unwrap(),
            Icmpv4Repr::EchoReply {
                ident: 17,
                seq_no: 23,
                data: b"control"
            }
        );
    }

    #[test]
    /// 只有首片时最早定时器仍到期清理，不依赖后继输入。
    fn reassembly_deadline_without_next_packet() {
        let now = Instant::from_millis(7);
        let mut sender = OneFrame(TestingDevice::new(Medium::Ip));
        let mut sending = interface(&mut sender, 65536);
        let payload = vec![11; 2048];
        let token = sender.transmit(now).unwrap();
        sending
            .dispatch_ip(
                now,
                token,
                PacketMeta::default(),
                datagram(&payload, Ipv4Address::LOCALHOST),
            )
            .unwrap();
        let first = sender.0.tx_queue.pop_front().unwrap();
        let mut device = OneFrame(TestingDevice::new(Medium::Ip));
        let mut iface = interface(&mut device, 65536);
        device.0.rx_queue.push_back(first);
        let mut capture = Capture {
            bytes: Vec::with_capacity(payload.len()),
            ip_len: 0,
            local: None,
        };
        iface.poll_ingress_single(now, &mut device, &mut capture);
        assert!(capture.bytes.is_empty());
        let deadline = iface.maintenance_deadline().unwrap();
        let sockets = SocketSet::new(vec![]);
        assert_eq!(iface.poll_at(now, &sockets), Some(deadline));
        iface.poll_maintenance(deadline);
        assert_eq!(iface.maintenance_deadline(), None);
        assert_eq!(iface.poll_at(deadline, &sockets), None);
    }

    #[cfg(all(feature = "socket-tcp", feature = "medium-ethernet"))]
    mod window_credit {
        use super::OneFrame;
        use crate::iface::{
            Config, DispatchError, Interface, InterfaceBuffers, InterfaceInner, IpPayload, Packet,
            SocketSet, TransportIngress, TransportResponse,
        };
        use crate::phy::{ChecksumCapabilities, Device, Medium, PacketMeta};
        use crate::socket::tcp::{Socket, SocketBuffer, State, TcpContextState};
        use crate::tests::TestingDevice;
        use crate::time::Instant;
        use crate::wire::*;
        use alloc::vec::Vec;

        const LOCAL: Ipv4Address = Ipv4Address::new(192, 0, 2, 1);
        const PEER: Ipv4Address = Ipv4Address::new(192, 0, 2, 2);
        const LOCAL_MAC: EthernetAddress = EthernetAddress([2, 0, 0, 0, 0, 1]);
        const PEER_MAC: EthernetAddress = EthernetAddress([2, 0, 0, 0, 0, 2]);
        const LOCAL_PORT: u16 = 1234;
        const PEER_PORT: u16 = 4321;

        struct Receiver<'a> {
            socket: &'a mut Socket<'static>,
            context: &'a mut TcpContextState,
        }

        impl<'s> TransportIngress<'s> for Receiver<'_> {
            /// 原接口校验后的 TCP 输入交给同一真实 raw 连接。
            fn tcp<'p>(
                &mut self,
                _: &mut InterfaceInner,
                _: PacketMeta,
                ip: &IpRepr,
                repr: &TcpRepr<'p>,
            ) -> TransportResponse<'p> {
                if !self.socket.accepts(ip, repr) {
                    return TransportResponse::Unmatched;
                }
                TransportResponse::Matched(
                    self.socket
                        .process(self.context, ip, repr, repr.payload)
                        .map(|(ip, tcp)| Packet::new(ip, IpPayload::Tcp(tcp))),
                )
            }

            /// 本消费者没有 UDP 目标。
            fn udp(
                &mut self,
                _: &mut InterfaceInner,
                _: PacketMeta,
                _: &IpRepr,
                _: &UdpRepr,
                _: &[u8],
            ) -> bool {
                false
            }

            /// 独立连接不创建空 SocketSet。
            fn socket_set(&mut self) -> Option<&mut SocketSet<'s>> {
                None
            }
        }

        struct WireTcp {
            repr: TcpRepr<'static>,
            payload: Vec<u8>,
        }

        /// 两端只准备实际单播地址和成熟邻居缓存，不在此消费 ARP。
        fn ethernet_interface(
            device: &mut OneFrame,
            address: Ipv4Address,
            hardware: EthernetAddress,
            peer: Ipv4Address,
            peer_hardware: EthernetAddress,
        ) -> Interface {
            let buffers = InterfaceBuffers::try_new(65536, 65536).unwrap();
            let mut iface =
                Interface::new(Config::new(hardware.into()), device, Instant::ZERO, buffers);
            iface.update_ip_addrs(|addresses| {
                addresses.push(IpCidr::new(address.into(), 24)).unwrap()
            });
            iface
                .inner
                .neighbor_cache
                .fill(peer.into(), peer_hardware.into(), Instant::ZERO);
            iface
        }

        /// peer 的序号、确认和窗口来自实际握手或上一份真实 frame。
        fn peer_segment<'p>(
            seq: TcpSeqNumber,
            ack: Option<TcpSeqNumber>,
            payload: &'p [u8],
        ) -> TcpRepr<'p> {
            TcpRepr {
                src_port: PEER_PORT,
                dst_port: LOCAL_PORT,
                control: TcpControl::None,
                seq_number: seq,
                ack_number: ack,
                window_len: u16::MAX,
                window_scale: None,
                max_seg_size: None,
                sack_permitted: false,
                sack_ranges: [None, None, None],
                timestamp: None,
                payload,
            }
        }

        /// 真实库编码 peer 输入后转移整 frame，再沿成熟介质 ingress 接纳。
        fn ingress(
            now: Instant,
            peer_iface: &mut Interface,
            peer_device: &mut OneFrame,
            iface: &mut Interface,
            device: &mut OneFrame,
            socket: &mut Socket<'static>,
            context: &mut TcpContextState,
            repr: TcpRepr<'_>,
        ) {
            assert!(device.0.tx_queue.is_empty());
            assert!(peer_device.0.tx_queue.is_empty());
            let ip = IpRepr::Ipv4(Ipv4Repr {
                src_addr: PEER,
                dst_addr: LOCAL,
                next_header: IpProtocol::Tcp,
                payload_len: repr.buffer_len(),
                hop_limit: 64,
            });
            let token = peer_device.transmit(now).unwrap();
            peer_iface
                .dispatch_ip(
                    now,
                    token,
                    PacketMeta::default(),
                    Packet::new(ip, IpPayload::Tcp(repr)),
                )
                .unwrap();
            let frame = peer_device.0.tx_queue.pop_front().unwrap();
            assert!(frame.len() <= device.capabilities().max_transmission_unit);
            device.0.rx_queue.push_back(frame);
            context.set_now(now);
            let mut receiver = Receiver { socket, context };
            iface.poll_ingress_single(now, device, &mut receiver);
            assert!(device.0.rx_queue.is_empty());
        }

        /// 仅真实 token 接纳后 raw 才提交发送信用；不回显未编码的 repr。
        fn dispatch(
            now: Instant,
            iface: &mut Interface,
            device: &mut OneFrame,
            socket: &mut Socket<'static>,
            context: &mut TcpContextState,
        ) {
            context.set_now(now);
            socket
                .dispatch(context, |_, (ip, tcp)| {
                    let token = device.transmit(now).ok_or(DispatchError::Exhausted)?;
                    iface.dispatch_ip(
                        now,
                        token,
                        PacketMeta::default(),
                        Packet::new(ip, IpPayload::Tcp(tcp)),
                    )
                })
                .unwrap();
        }

        /// 消费实际接纳的 frame，使用原 parser 校验头部和传输校验和。
        fn take_frame(device: &mut OneFrame) -> WireTcp {
            let frame = device
                .0
                .tx_queue
                .pop_front()
                .expect("expected accepted TCP frame");
            assert!(device.0.tx_queue.is_empty());
            assert!(frame.len() <= device.capabilities().max_transmission_unit);
            let ethernet = EthernetFrame::new_checked(&frame[..]).unwrap();
            assert_eq!(ethernet.ethertype(), EthernetProtocol::Ipv4);
            assert_eq!(ethernet.src_addr(), LOCAL_MAC);
            assert_eq!(ethernet.dst_addr(), PEER_MAC);
            let ipv4 = Ipv4Packet::new_checked(ethernet.payload()).unwrap();
            let ip = Ipv4Repr::parse(&ipv4, &ChecksumCapabilities::default()).unwrap();
            assert_eq!(ip.src_addr, LOCAL);
            assert_eq!(ip.dst_addr, PEER);
            let tcp = TcpPacket::new_checked(ipv4.payload()).unwrap();
            let repr = TcpRepr::parse(
                &tcp,
                &LOCAL.into(),
                &PEER.into(),
                &ChecksumCapabilities::default(),
            )
            .unwrap();
            WireTcp {
                payload: repr.payload.to_vec(),
                repr: TcpRepr {
                    payload: &[],
                    ..repr
                },
            }
        }

        /// 即时处理返回的 ACK 与 raw 待发 ACK 使用同一个实际编码路径。
        fn output(
            now: Instant,
            iface: &mut Interface,
            device: &mut OneFrame,
            socket: &mut Socket<'static>,
            context: &mut TcpContextState,
        ) -> WireTcp {
            if device.0.tx_queue.is_empty() {
                dispatch(now, iface, device, socket, context);
            }
            take_frame(device)
        }

        #[test]
        /// 完整握手后仅消费实际线上信用，缩预算耗尽时归零并通过全文读取恢复。
        fn tcp_wire_window_credit_with_finite_ethernet_burst() {
            let mut device = OneFrame(TestingDevice::new(Medium::Ethernet));
            let mut peer_device = OneFrame(TestingDevice::new(Medium::Ethernet));
            let mut iface = ethernet_interface(&mut device, LOCAL, LOCAL_MAC, PEER, PEER_MAC);
            let mut peer_iface =
                ethernet_interface(&mut peer_device, PEER, PEER_MAC, LOCAL, LOCAL_MAC);
            let mtu = device.capabilities().ip_mtu();
            let mut context = TcpContextState::new(Instant::ZERO, mtu, 17);
            let mut socket = Socket::new(
                SocketBuffer::new(vec![0; 131072]),
                SocketBuffer::new(vec![0; 4096]),
            );
            socket
                .listen(IpEndpoint::new(LOCAL.into(), LOCAL_PORT))
                .unwrap();
            socket.set_ack_delay(None);
            let peer_initial = TcpSeqNumber(10000);
            let mut syn = peer_segment(peer_initial, None, &[]);
            syn.control = TcpControl::Syn;
            syn.window_scale = Some(0);
            syn.max_seg_size = Some((mtu - IPV4_HEADER_LEN - TCP_HEADER_LEN) as u16);
            ingress(
                Instant::ZERO,
                &mut peer_iface,
                &mut peer_device,
                &mut iface,
                &mut device,
                &mut socket,
                &mut context,
                syn,
            );
            let syn_ack = output(
                Instant::ZERO,
                &mut iface,
                &mut device,
                &mut socket,
                &mut context,
            );
            assert_eq!(syn_ack.repr.control, TcpControl::Syn);
            assert_eq!(syn_ack.repr.ack_number, Some(peer_initial + 1));
            assert!(syn_ack.payload.is_empty());
            assert!(0 < syn_ack.repr.window_len);
            let scale = syn_ack
                .repr
                .window_scale
                .expect("receiver negotiated window scaling");
            let peer_seq = peer_initial + 1;
            let local_seq = syn_ack.repr.seq_number + 1;
            ingress(
                Instant::from_millis(1),
                &mut peer_iface,
                &mut peer_device,
                &mut iface,
                &mut device,
                &mut socket,
                &mut context,
                peer_segment(peer_seq, Some(local_seq), &[]),
            );
            assert_eq!(socket.state(), State::Established);
            assert!(device.0.tx_queue.is_empty());

            assert_eq!(socket.send_slice(b"!"), Ok(1));
            let data = output(
                Instant::from_millis(2),
                &mut iface,
                &mut device,
                &mut socket,
                &mut context,
            );
            assert_eq!(data.payload, b"!");
            assert_eq!(data.repr.ack_number, Some(peer_seq));
            let credit = (data.repr.window_len as usize) << scale;
            assert!(1024 < credit);
            let peer_ack = data.repr.seq_number + data.payload.len();
            let pattern: Vec<u8> = (0..credit).map(|index| (index % 251) as u8).collect();
            socket.set_recv_budget(1024);
            let segment_len = mtu - IPV4_HEADER_LEN - TCP_HEADER_LEN;
            let mut consumed = 0;
            let mut last_window = None;
            for (index, bytes) in pattern.chunks(segment_len).enumerate() {
                let now = Instant::from_millis(3 + index as i64);
                ingress(
                    now,
                    &mut peer_iface,
                    &mut peer_device,
                    &mut iface,
                    &mut device,
                    &mut socket,
                    &mut context,
                    peer_segment(peer_seq + consumed, Some(peer_ack), bytes),
                );
                consumed += bytes.len();
                let ack = output(now, &mut iface, &mut device, &mut socket, &mut context);
                assert_eq!(ack.repr.ack_number, Some(peer_seq + consumed));
                assert!(ack.payload.is_empty());
                last_window = Some(ack.repr.window_len);
            }
            assert_eq!(consumed, credit);
            assert_eq!(
                last_window,
                Some(0),
                "wire credit must be exhausted after the budget shrink"
            );
            let mut received = vec![0; pattern.len()];
            assert_eq!(socket.recv_slice(&mut received), Ok(pattern.len()));
            assert_eq!(received, pattern);

            socket.set_recv_budget(4096);
            let now = Instant::from_millis(1000);
            let renewed = output(now, &mut iface, &mut device, &mut socket, &mut context);
            let renewed_credit = (renewed.repr.window_len as usize) << scale;
            let next = b"after-budget";
            assert!(next.len() <= renewed_credit);
            ingress(
                now,
                &mut peer_iface,
                &mut peer_device,
                &mut iface,
                &mut device,
                &mut socket,
                &mut context,
                peer_segment(peer_seq + consumed, Some(peer_ack), next),
            );
            let ack = output(now, &mut iface, &mut device, &mut socket, &mut context);
            assert_eq!(ack.repr.ack_number, Some(peer_seq + consumed + next.len()));
            let mut recovered = [0; 12];
            assert_eq!(socket.recv_slice(&mut recovered), Ok(next.len()));
            assert_eq!(&recovered[..], next);
        }
    }
}
