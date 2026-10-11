use super::*;

impl InterfaceInner {
    /// 在接口解析边界校验报文，再交给匹配连接或构造拒绝应答。
    pub fn process_tcp<'frame, 's>(
        &mut self,
        sockets: &mut impl TransportIngress<'s>,
        meta: PacketMeta,
        ip_repr: IpRepr,
        ip_payload: &'frame [u8],
    ) -> Option<Packet<'frame>> {
        let (src_addr, dst_addr) = (ip_repr.src_addr(), ip_repr.dst_addr());
        let tcp_packet = check!(TcpPacket::new_checked(ip_payload));
        let tcp_repr = check!(TcpRepr::parse(
            &tcp_packet,
            &src_addr,
            &dst_addr,
            &self.caps.checksum
        ));

        match sockets.tcp(self, meta, &ip_repr, &tcp_repr) {
            TransportResponse::Matched(reply) => return reply,
            TransportResponse::Unmatched => {}
        }

        if tcp_repr.control == TcpControl::Rst
            || ip_repr.dst_addr().is_unspecified()
            || ip_repr.src_addr().is_unspecified()
        {
            // Never reply to a TCP RST packet with another TCP RST packet. We also never want to
            // send a TCP RST packet with unspecified addresses.
            None
        } else {
            // The packet wasn't handled by a socket, send a TCP RST packet.
            let (ip, tcp) = crate::socket::tcp::Socket::rst_reply(&ip_repr, &tcp_repr);
            Some(Packet::new(ip, IpPayload::Tcp(tcp)))
        }
    }
}
