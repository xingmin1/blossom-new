#![no_main]
use libfuzzer_sys::fuzz_target;
use smoltcp::iface::{Config, Interface, SocketSet};
use smoltcp::phy::{Loopback, Medium};
use smoltcp::socket::tcp;
use smoltcp::time::{Duration, Instant};
use smoltcp::wire::{EthernetAddress, EthernetFrame, EthernetProtocol};
use smoltcp::wire::{IpAddress, IpCidr, Ipv4Packet, Ipv6Packet, TcpPacket};
use std::cmp;

#[path = "../utils.rs"]
mod utils;

mod mock {
    use smoltcp::time::{Duration, Instant};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    // should be AtomicU64 but that's unstable
    #[derive(Debug, Clone)]
    #[cfg_attr(feature = "defmt", derive(defmt::Format))]
    pub struct Clock(Arc<AtomicUsize>);

    impl Clock {
        pub fn new() -> Clock {
            Clock(Arc::new(AtomicUsize::new(0)))
        }

        pub fn advance(&self, duration: Duration) {
            self.0
                .fetch_add(duration.total_millis() as usize, Ordering::SeqCst);
        }

        pub fn elapsed(&self) -> Instant {
            Instant::from_millis(self.0.load(Ordering::SeqCst) as i64)
        }
    }
}

struct TcpHeaderFuzzer([u8; 56], usize);

impl TcpHeaderFuzzer {
    // The fuzzer won't fuzz any packets with the SYN flag set in order to make sure the connection
    // is established before the fuzzed headers arrive.
    //
    // It will also not fuzz the source and dest port so it reaches the open socket.
    //
    // Otherwise, it replaces the entire rest of the TCP header with the fuzzer's output.
    pub fn new(data: &[u8]) -> TcpHeaderFuzzer {
        let copy_len = cmp::min(
            data.len(),
            56, /* max TCP header length without port numbers*/
        );

        let mut fuzzer = TcpHeaderFuzzer([0; 56], copy_len);
        fuzzer.0[..copy_len].copy_from_slice(&data[..copy_len]);
        fuzzer
    }
}

impl smoltcp::phy::Fuzzer for TcpHeaderFuzzer {
    fn fuzz_packet(&self, frame_data: &mut [u8]) {
        if self.1 == 0 {
            return;
        }

        let tcp_packet_offset = {
            let eth_frame = EthernetFrame::new_unchecked(&frame_data);
            EthernetFrame::<&mut [u8]>::header_len()
                + match eth_frame.ethertype() {
                    EthernetProtocol::Ipv4 => {
                        Ipv4Packet::new_unchecked(eth_frame.payload()).header_len() as usize
                    }
                    EthernetProtocol::Ipv6 => {
                        Ipv6Packet::new_unchecked(eth_frame.payload()).header_len() as usize
                    }
                    _ => return,
                }
        };

        let tcp_is_syn = {
            let tcp_packet = TcpPacket::new_checked(&frame_data[tcp_packet_offset..]).unwrap();
            tcp_packet.syn()
        };

        if tcp_is_syn {
            return;
        }

        if !frame_data.ends_with(b"abcdef") {
            return;
        }

        let tcp_header_len = {
            let tcp_packet = &frame_data[tcp_packet_offset..];
            (tcp_packet[12] as usize >> 4) * 4
        };

        let tcp_packet = &mut frame_data[tcp_packet_offset + 4..];

        let replacement_data = &self.0[..self.1];
        let copy_len = cmp::min(replacement_data.len(), tcp_header_len);
        assert!(copy_len < tcp_packet.len());
        tcp_packet[..copy_len].copy_from_slice(&replacement_data[..copy_len]);
    }
}

struct EmptyFuzzer();

impl smoltcp::phy::Fuzzer for EmptyFuzzer {
    fn fuzz_packet(&self, _: &mut [u8]) {}
}

fuzz_target!(|data: &[u8]| exercise_tcp_headers(data));

/// 在真实 loopback 连接上保留握手后 TCP 首部扰动。
fn exercise_tcp_headers(data: &[u8]) {
    let clock = mock::Clock::new();

    let mut device = {
        let (mut opts, mut free) = utils::create_options();
        utils::add_middleware_options(&mut opts, &mut free);

        let mut matches = utils::parse_options(&opts, free);
        let device = utils::parse_middleware_options(
            &mut matches,
            Loopback::new(Medium::Ethernet),
            /*loopback=*/ true,
        );

        smoltcp::phy::FuzzInjector::new(device, EmptyFuzzer(), TcpHeaderFuzzer::new(data))
    };

    let config = Config::new(EthernetAddress([0x02, 0, 0, 0, 0, 1]).into());
    let buffers = smoltcp::iface::InterfaceBuffers::try_new(
        smoltcp::config::FRAGMENTATION_BUFFER_SIZE,
        smoltcp::config::REASSEMBLY_BUFFER_SIZE,
    )
    .unwrap();
    let mut iface = Interface::new(config, &mut device, clock.elapsed(), buffers);
    iface.update_ip_addrs(|addrs| {
        addrs
            .push(IpCidr::new(IpAddress::v4(127, 0, 0, 1), 8))
            .unwrap();
    });

    let mut server_rx = [0; 1024];
    let mut server_tx = [0; 1024];
    let mut client_rx = [0; 1024];
    let mut client_tx = [0; 1024];

    let server_socket = {
        let tcp_rx_buffer = tcp::SocketBuffer::new(&mut server_rx[..]);
        let tcp_tx_buffer = tcp::SocketBuffer::new(&mut server_tx[..]);
        tcp::Socket::new(tcp_rx_buffer, tcp_tx_buffer)
    };

    let client_socket = {
        let tcp_rx_buffer = tcp::SocketBuffer::new(&mut client_rx[..]);
        let tcp_tx_buffer = tcp::SocketBuffer::new(&mut client_tx[..]);
        tcp::Socket::new(tcp_rx_buffer, tcp_tx_buffer)
    };

    let mut socket_set_entries: [_; 2] = Default::default();
    let mut socket_set = SocketSet::new(&mut socket_set_entries[..]);
    let server_handle = socket_set.add(server_socket);
    let client_handle = socket_set.add(client_socket);

    let mut did_listen = false;
    let mut did_connect = false;
    let mut done = false;
    while !done && clock.elapsed() < Instant::from_millis(4_000) {
        iface.poll(clock.elapsed(), &mut device, &mut socket_set);

        {
            let socket = socket_set.get_mut::<tcp::Socket>(server_handle);
            if !socket.is_active() && !socket.is_listening() {
                if !did_listen {
                    socket.listen(1234).unwrap();
                    did_listen = true;
                }
            }

            if socket.can_recv() {
                socket.close();
                done = true;
            }
        }

        {
            let socket = socket_set.get_mut::<tcp::Socket>(client_handle);
            if !socket.is_open() {
                if !did_connect {
                    socket
                        .connect(
                            iface.context(),
                            (IpAddress::v4(127, 0, 0, 1), 1234),
                            (IpAddress::v4(127, 0, 0, 1), 65000),
                        )
                        .unwrap();
                    did_connect = true;
                }
            }

            if socket.can_send() {
                socket
                    .send_slice(b"0123456789abcdef0123456789abcdef0123456789abcdef")
                    .unwrap();
                socket.close();
            }
        }

        match iface.poll_delay(clock.elapsed(), &socket_set) {
            Some(Duration::ZERO) => {}
            Some(delay) => clock.advance(delay),
            None => clock.advance(Duration::from_millis(1)),
        }
    }
}
