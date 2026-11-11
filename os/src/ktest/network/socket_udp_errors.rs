/// UDP 错误队列、共同接收预算与发送准备对象的生命周期测试。
///
/// 本套件通过真实回环 ICMP 保留未读错误，并验证已消费 TCP 帧被拒收后仍能推进已接纳的 UDP 后继。
/// 端点和文件的关闭、根停止及两层资源核对均由既有 `StackOwner` 负责。
#[ktest::suite(fixture = StackOwner, suite_resources = SuiteBaseline, context = "Thread", tags("network-socket", "network-udp-errors"), timeout_ms = 45000)]
pub(crate) mod socket_udp_errors {
    use ::core::sync::atomic::{AtomicBool, Ordering};
    use ::ktest::{TestContext, TestError, TestResult};
    use alloc::sync::Arc;
    use smoltcp::wire::{IpProtocol, IpRepr, Ipv4Address, TcpControl, TcpRepr, TcpSeqNumber};

    use super::owner::StackOwner;
    use super::socket_helpers::prepare_udp_oom_gate;
    use super::socket_udp_lease::receive_exact;
    use super::stack_helpers::{accepted, listener};
    use super::support::{child_operation, observe, operation, socket_retired, CLIENT, SERVER};
    use crate::fs::{FsError, PollMask, ReadBuffer};
    use crate::ktest::execution_services::SuiteBaseline;
    use crate::net::address::{AddressFamily, InetEndpoint, SocketAddress};
    use crate::net::packet::OwnedPacket;
    use crate::net::route::InterfaceId;
    use crate::net::socket::SocketFile;
    use crate::net::udp::UdpEndpoint;
    use crate::net::{IoProgress, LOOPBACK_CAPACITY, LOOPBACK_MTU};

    /// 向本域未绑定的回环端口实际发送报文，只消费返回的标量错误，保留 IP_RECVERR 记录。
    fn retain_icmp_error(
        context: &mut TestContext<'_>,
        endpoint: &UdpEndpoint,
        destination: InetEndpoint,
        payload: &[u8],
    ) -> TestResult {
        operation(context, endpoint.set_recv_error(true))?;
        let prepared = operation(
            context,
            endpoint.prepare_send(payload.len(), Some(destination)),
        )?;
        if operation(context, endpoint.send(&prepared, payload))?
            != IoProgress::Ready(payload.len())
        {
            return Err(TestError("UDP ICMP request was not accepted"));
        }
        drop(prepared);

        let mut reported = None;
        observe(
            || {
                reported = endpoint.take_error();
                reported.is_some()
            },
            "loopback UDP did not report an actual ICMP error",
        )?;
        context.expect_eq(reported, Some(FsError::ConnectionRefused));
        context.expect_eq(
            operation(context, endpoint.poll(None))?.contains(PollMask::POLLERR),
            true,
        );
        Ok(())
    }

    /// 测试目标：验证排队和已领取错误的正文占用均限制共同接收预算，同时保留旧正常 RX 和错误。
    ///
    /// 场景步骤（Given-When-Then）：先保留 17 字节回环 RX 和引用 64 字节正文的真实 ICMP 错误，
    /// 再把预算缩至 1，读出旧 RX；分别在错误仍排队和领取 guard 存活、FIFO 已空时，
    /// 经端点输入 Work 完成票据提交空 UDP。
    /// 预期结果：两次提交均没有新 POLLIN 或普通 RX；旧错误正文仍完整，释放 guard 并恢复预算后空报文正常交付。
    #[test]
    fn udp_error_charge_over_budget_rejects_zero_length_rx(
        context: &mut TestContext<'_>,
        fixture: &mut StackOwner,
    ) -> TestResult {
        // Step 1: 使用同一显式绑定端点向自身发送正常报文，再向未绑定端口发送错误请求。
        let endpoint = operation(context, fixture.net.try_new_udp(AddressFamily::Ipv4))?;
        let local = operation(context, endpoint.bind(SERVER))?;
        let normal = [0x51u8; 17];
        context.expect_eq(
            operation(
                context,
                endpoint
                    .prepare_send(normal.len(), Some(local))
                    .and_then(|prepared| endpoint.send(&prepared, &normal)),
            )?,
            IoProgress::Ready(normal.len()),
        );
        observe(
            || {
                endpoint
                    .poll(None)
                    .is_ok_and(|mask| mask.contains(PollMask::POLLIN))
            },
            "UDP normal RX did not arrive before the budget reduction",
        )?;
        let destination = InetEndpoint {
            port: local.port + 1,
            ..local
        };
        let error_payload = [0xd3u8; 64];
        retain_icmp_error(context, &endpoint, destination, &error_payload)?;
        let budget = endpoint.receive_budget();

        // Step 2: 错误正文实际容量至少为 64 字节，预算 1 必然超额；旧 RX 仍须完整可读。
        operation(context, endpoint.set_receive_budget(1))?;
        context.expect_eq(endpoint.receive_budget(), 1);
        receive_exact(&endpoint, &normal, local)?;
        let empty = operation(
            context,
            OwnedPacket::try_from_udp(local, local, &[], LOOPBACK_MTU),
        )?;
        operation(context, endpoint.enqueue(empty))?;
        fixture.net.work_queue().flush();

        context.expect_eq(
            operation(context, endpoint.poll(None))?.contains(PollMask::POLLIN),
            false,
        );
        let mut bytes = [0u8; 1];
        context.expect_eq(
            operation(
                context,
                endpoint.receive(&mut ReadBuffer::kernel(&mut bytes), false),
            )?,
            None,
        );
        context.expect_eq(
            operation(context, endpoint.poll(None))?.contains(PollMask::POLLERR),
            true,
        );

        // Step 3: 领取原错误并确认 FIFO 已空；guard 仍拥有的 64 字节正文继续占用共同预算。
        let error = operation(context, endpoint.receive_error())?;
        context.expect_eq(error.error, FsError::ConnectionRefused);
        context.expect_eq(error.origin, 2);
        context.expect_eq(error.kind, 3);
        context.expect_eq(error.code, 3);
        context.expect_eq(error.info, 0);
        context.expect_eq(error.destination, destination);
        context.expect_eq(error.offender, Some(Ipv4Address::new(127, 0, 0, 1)));
        context.expect_eq(error.payload(), &error_payload[..]);
        context.expect_eq(endpoint.receive_error().err(), Some(FsError::WouldBlock));
        let empty = operation(
            context,
            OwnedPacket::try_from_udp(local, local, &[], LOOPBACK_MTU),
        )?;
        operation(context, endpoint.enqueue(empty))?;
        fixture.net.work_queue().flush();
        context.expect_eq(
            operation(context, endpoint.poll(None))?.contains(PollMask::POLLIN),
            false,
        );
        context.expect_eq(
            operation(
                context,
                endpoint.receive(&mut ReadBuffer::kernel(&mut bytes), false),
            )?,
            None,
        );

        // Step 4: 释放领取 guard 后再恢复原预算，随后提交的空报文须真实交付。
        drop(error);
        operation(context, endpoint.set_receive_budget(budget))?;
        let empty = operation(
            context,
            OwnedPacket::try_from_udp(local, local, &[], LOOPBACK_MTU),
        )?;
        operation(context, endpoint.enqueue(empty))?;
        fixture.net.work_queue().flush();
        receive_exact(&endpoint, &[], local)?;

        endpoint.close();
        drop(endpoint);
        socket_retired(&fixture.net)?;
        context.expect_eq(fixture.net.is_stopping(), false);
        Ok(())
    }

    /// 测试目标：验证未消费的真实错误和正常 RX 随最后文件关闭或根停止释放，根停止不等待已领取错误。
    ///
    /// 场景步骤（Given-When-Then）：同一 SocketFile 保留正常回环 RX 和 IP_RECVERR；
    /// 最后文件分支保持根运行，停根分支保留两条真实错误，领取一条并让另一条继续排队，
    /// 在领取 guard、外部文件和端点仍存活时同步停止根，随后依次释放 guard 和这些引用。
    /// 预期结果：停止不等待 guard，领取正文在停止后仍完整；Weak、槽和索引归还，活根原端口可重绑，两层资源账照常核对。
    #[test]
    #[case("last_ofd", false)]
    #[case("root_stop", true)]
    fn udp_unread_error_and_rx_release_on_close_or_stop(
        context: &mut TestContext<'_>,
        fixture: &mut StackOwner,
        stop_root: bool,
    ) -> TestResult {
        // Step 1: 只创建一个文件端点，使未读正常 RX、错误和最后外部引用归属于同一对象。
        let file = operation(
            context,
            SocketFile::try_new_udp(Arc::clone(&fixture.net), AddressFamily::Ipv4, 0, 0, 0),
        )?;
        operation(context, file.bind(SocketAddress::Inet(SERVER)))?;
        let endpoint = fixture
            .net
            .tables()
            .udp_by_id(file.id())
            .ok_or(TestError("UDP error file endpoint is not published"))?;
        let local = operation(context, endpoint.local_address())?;
        let normal = [0x6au8; 17];
        context.expect_eq(
            operation(
                context,
                endpoint
                    .prepare_send(normal.len(), Some(local))
                    .and_then(|prepared| endpoint.send(&prepared, &normal)),
            )?,
            IoProgress::Ready(normal.len()),
        );
        observe(
            || {
                endpoint
                    .poll(None)
                    .is_ok_and(|mask| mask.contains(PollMask::POLLIN))
            },
            "UDP unread normal RX did not arrive",
        )?;
        let error_payload = [0x72u8; 64];
        retain_icmp_error(
            context,
            &endpoint,
            InetEndpoint {
                port: local.port + 1,
                ..local
            },
            &error_payload,
        )?;
        context.expect_eq(
            operation(context, endpoint.poll(None))?.contains(PollMask::POLLIN | PollMask::POLLERR),
            true,
        );
        let claimed = if stop_root {
            retain_icmp_error(
                context,
                &endpoint,
                InetEndpoint {
                    port: local.port + 2,
                    ..local
                },
                &[0x73u8; 64],
            )?;
            let error = operation(context, endpoint.receive_error())?;
            context.expect_eq(error.payload(), &error_payload[..]);
            context.expect_eq(endpoint.take_error(), Some(FsError::ConnectionRefused));
            context.expect_eq(
                operation(context, endpoint.poll(None))?
                    .contains(PollMask::POLLIN | PollMask::POLLERR),
                true,
            );
            Some(error)
        } else {
            None
        };
        let id = file.id();
        let file_weak = Arc::downgrade(&file);
        let endpoint_weak = Arc::downgrade(&endpoint);
        context.expect_eq(fixture.net.ktest_live_endpoints(), 1);

        // Step 2: 领取 guard 和另一条排队错误同时存活，根的同步停止不能等待 guard 释放。
        if stop_root {
            drop(fixture.owner.take());
            context.expect_eq(fixture.net.is_stopping(), true);
            context.expect_eq(file_weak.upgrade().is_some(), true);
            context.expect_eq(endpoint_weak.upgrade().is_some(), true);
            context.expect_eq(fixture.net.ktest_live_endpoints(), 1);
            context.expect_eq(fixture.net.tables().udp_by_id(id).is_none(), true);
            context.expect_eq(
                fixture.net.tables().lookup_udp(local, local).is_none(),
                true,
            );
        }
        if let Some(error) = claimed.as_ref() {
            context.expect_eq(error.payload(), &error_payload[..]);
        }
        drop(claimed);
        drop((endpoint, file));
        observe(
            || file_weak.upgrade().is_none() && endpoint_weak.upgrade().is_none(),
            "UDP unread error retained a closed file or endpoint",
        )?;
        socket_retired(&fixture.net)?;
        context.expect_eq(fixture.net.tables().udp_by_id(id).is_none(), true);
        context.expect_eq(
            fixture.net.tables().lookup_udp(local, local).is_none(),
            true,
        );

        // Step 3: 最后文件关闭不得借根停止清理，原绑定须能在同一个仍运行的根中重新取得。
        if !stop_root {
            context.expect_eq(fixture.net.is_stopping(), false);
            let rebound = operation(context, fixture.net.try_new_udp(AddressFamily::Ipv4))?;
            context.expect_eq(operation(context, rebound.bind(local))?, local);
            rebound.close();
            drop(rebound);
            socket_retired(&fixture.net)?;
        }
        context.expect_eq(fixture.net.ktest_live_endpoints(), 0);
        context.expect_eq(fixture.net.is_stopping(), stop_root);
        Ok(())
    }

    /// 测试目标：验证尚未提交发送的准备对象独立持有旧租约，断连和后继绑定不改变其源地址。
    ///
    /// 场景步骤（Given-When-Then）：连接后先完成旧 prepare，再断连并准备另一端口的后继发送；
    /// 逐个实际发送、接收两份原字节，随后释放旧准备对象并重新绑定旧端口。
    /// 预期结果：线上源地址分别为 127.0.0.1 的旧、新端口；旧对象释放前端口冲突，释放后可用，后继端口仍受保护。
    #[test]
    fn udp_unsubmitted_prepared_lease_survives_disconnect(
        context: &mut TestContext<'_>,
        fixture: &mut StackOwner,
    ) -> TestResult {
        // Step 1: 旧准备对象尚未提交 TX，断连必须清除当前绑定，但不能释放它持有的旧端口。
        let receiver = operation(context, fixture.net.try_new_udp(AddressFamily::Ipv4))?;
        let destination = operation(context, receiver.bind(SERVER))?;
        let sender = operation(context, fixture.net.try_new_udp(AddressFamily::Ipv4))?;
        operation(context, sender.connect(Some(destination)))?;
        let old_source = operation(context, sender.local_address())?;
        let old_payload = b"prepared before disconnect";
        let old_prepared = operation(context, sender.prepare_send(old_payload.len(), None))?;
        operation(context, sender.connect(None))?;
        context.expect_eq(
            operation(context, sender.local_address())?,
            InetEndpoint {
                address: AddressFamily::Ipv4.unspecified(),
                port: 0,
            },
        );
        let old_contender = operation(context, fixture.net.try_new_udp(AddressFamily::Ipv4))?;
        context.expect_eq(old_contender.bind(old_source), Err(FsError::AddressInUse));

        // Step 2: 后继 prepare 自动取得另一端口；线上源地址采用回环路由地址，不能采用未连接的通配地址。
        let new_payload = b"successor after disconnect";
        let new_prepared = operation(
            context,
            sender.prepare_send(new_payload.len(), Some(destination)),
        )?;
        let new_local = operation(context, sender.local_address())?;
        let new_source = InetEndpoint {
            address: destination.address,
            port: new_local.port,
        };
        context.expect_eq(old_source.address, destination.address);
        context.expect_eq(new_source.port == old_source.port, false);
        context.expect_eq(sender.peer_address(), Err(FsError::NotConnected));
        let new_contender = operation(context, fixture.net.try_new_udp(AddressFamily::Ipv4))?;
        context.expect_eq(new_contender.bind(new_source), Err(FsError::AddressInUse));

        // Step 3: 分别提交和消费两条数据报，接收 helper 同时核对完整正文、消息边界和具体源地址。
        context.expect_eq(
            operation(context, sender.send(&old_prepared, old_payload))?,
            IoProgress::Ready(old_payload.len()),
        );
        receive_exact(&receiver, old_payload, old_source)?;
        context.expect_eq(
            operation(context, sender.send(&new_prepared, new_payload))?,
            IoProgress::Ready(new_payload.len()),
        );
        receive_exact(&receiver, new_payload, new_source)?;
        fixture.net.work_queue().flush();
        drop(new_prepared);

        // Step 4: 旧报文已经交付，旧准备对象仍保护旧端口；释放它不影响当前后继绑定。
        context.expect_eq(old_contender.bind(old_source), Err(FsError::AddressInUse));
        drop(old_prepared);
        context.expect_eq(
            operation(context, old_contender.bind(old_source))?,
            old_source,
        );
        context.expect_eq(new_contender.bind(new_source), Err(FsError::AddressInUse));
        sender.close();
        receiver.close();
        old_contender.close();
        new_contender.close();
        drop((sender, receiver, old_contender, new_contender));
        socket_retired(&fixture.net)?;
        context.expect_eq(fixture.net.is_stopping(), false);
        Ok(())
    }

    /// 验证已消费的 TCP 帧因真实输入队列满而丢弃后，原工作票仍能交付已接纳的 UDP 后继。
    ///
    /// 真实握手后由 CPU1 持有 TCP core 并填满 32 条输入；CPU0 关中断期间依次接纳 TCP 和 UDP，
    /// 放行后在 core 仍被持有、没有追加发送或根唤醒时观察丢包计数增加 1，并核对 UDP 完整正文与源地址。
    /// 业务结果确定后才释放 core、结束门控并 join，随后关闭端点并由原夹具核对两层资源账。
    #[test]
    #[ktest(min_cpus = 2, tags("network-loopback-progress"))]
    fn tcp_full_input_drop_preserves_accepted_udp_successor(
        context: &mut TestContext<'_>,
        fixture: &mut StackOwner,
    ) -> TestResult {
        let listener_endpoint = listener(context, &fixture.net, 45730)?;
        let client = operation(
            context,
            fixture.net.try_new_tcp(AddressFamily::Ipv4, 256, 256),
        )?;
        operation(
            context,
            client.connect(InetEndpoint {
                port: 45730,
                ..SERVER
            }),
        )?;
        let server = accepted(context, &listener_endpoint, &client)?;
        let peer = operation(context, server.peer_address())?;
        let local = operation(context, server.local_address())?;
        let receiver = operation(context, fixture.net.try_new_udp(AddressFamily::Ipv4))?;
        let destination = operation(
            context,
            receiver.bind(InetEndpoint {
                port: 45731,
                ..SERVER
            }),
        )?;
        let net = Arc::clone(&fixture.net);
        let release = Arc::clone(&fixture.release);
        let controller = crate::ktest::spawn_result_on(1, move |_| {
            let tcp = TcpRepr {
                src_port: peer.port,
                dst_port: local.port,
                control: TcpControl::None,
                seq_number: TcpSeqNumber(0),
                ack_number: Some(TcpSeqNumber(0)),
                window_len: 256,
                window_scale: None,
                max_seg_size: None,
                sack_permitted: false,
                sack_ranges: [None; 3],
                timestamp: None,
                payload: &[],
            };
            let ip = IpRepr::new(
                peer.address,
                local.address,
                IpProtocol::Tcp,
                tcp.buffer_len(),
                64,
            );
            let first = child_operation(OwnedPacket::try_from_tcp(&ip, &tcp, LOOPBACK_MTU))?;
            let payload = b"accepted UDP after full TCP input";
            let second = child_operation(OwnedPacket::try_from_udp(
                CLIENT,
                destination,
                payload,
                LOOPBACK_MTU,
            ))?;
            let entered = Arc::try_new(AtomicBool::new(false))
                .map_err(|_| TestError("TCP input gate observation allocation failed"))?;
            let finished = Arc::try_new(AtomicBool::new(false))
                .map_err(|_| TestError("TCP input gate lifetime allocation failed"))?;
            let prepared_gate = prepare_udp_oom_gate(
                Arc::clone(&entered),
                Arc::clone(&release),
                Arc::clone(&finished),
            )?;
            net.work_queue().flush();
            let before = child_operation(net.statistics(InterfaceId::Loopback))?;

            // core 的真实拥有使协议 Work 只能退让，验证结束前不能消费这 32 条输入。
            let owned = server.lock();
            for _ in 0..LOOPBACK_CAPACITY {
                let packet = child_operation(OwnedPacket::try_from_tcp(&ip, &tcp, LOOPBACK_MTU))?;
                child_operation(server.enqueue(packet))?;
            }
            let gate = prepared_gate.publish()?;
            let result = (|| -> TestResult {
                observe(
                    || entered.load(Ordering::Acquire),
                    "TCP input ingress CPU did not enter IRQ gate",
                )?;
                // CPU0 关中断期间两笔提交合并为同一根 Work 票，后继没有独立发送票可补救停滞。
                child_operation(net.try_emit(first).map_err(|(error, _)| error))?;
                child_operation(net.try_emit(second).map_err(|(error, _)| error))?;
                release.store(true, Ordering::Release);
                observe(
                    || {
                        net.statistics(InterfaceId::Loopback).is_ok_and(|stats| {
                            stats.rx_dropped.wrapping_sub(before.rx_dropped) == 1
                        })
                    },
                    "TCP full input did not count the consumed loopback frame as dropped",
                )?;
                let delivered = receive_exact(&receiver, payload, CLIENT);
                let after = child_operation(net.statistics(InterfaceId::Loopback))?;
                crate::ktest::report::diagnostic(format_args!(
                    "loopback TCP-full-input stats-before={:?} stats-after={:?}",
                    before, after
                ));
                delivered?;
                if after.rx_dropped.wrapping_sub(before.rx_dropped) != 1
                    || after.rx_packets.wrapping_sub(before.rx_packets) != 2
                {
                    return Err(TestError("TCP drop or UDP successor frame count differs"));
                }
                Ok(())
            })();

            // 先保存正文结果再释放 core；清理产生的 ACK/FIN 和工作票不能补成正文通过。
            drop(owned);
            release.store(true, Ordering::Release);
            finished.store(true, Ordering::Release);
            let joined = gate.join();
            server.close();
            client.close();
            listener_endpoint.close();
            receiver.close();
            drop((server, client, listener_endpoint, receiver));
            let retired = socket_retired(&net);
            result?;
            if joined? != 0 {
                return Err(TestError("TCP input IRQ gate child failed"));
            }
            retired?;
            Ok(0)
        })?;
        context.expect_eq(controller.join()?, 0);
        context.expect_eq(fixture.net.ktest_live_endpoints(), 0);
        context.expect_eq(fixture.net.is_stopping(), false);
        Ok(())
    }
}
