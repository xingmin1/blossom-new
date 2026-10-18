// 完整 inline suite 保留原 module_path 与静态注册身份。
#[ktest::suite(fixture = StackOwner, suite_resources = SuiteBaseline, context = "Thread", tags("network-socket"), timeout_ms = 45000)]
pub(crate) mod socket_native {
    use ::core::sync::atomic::{AtomicBool, Ordering};
    use ::ktest::{TestContext, TestError, TestResult};
    use alloc::sync::Arc;
    use alloc::vec::Vec;
    use smoltcp::socket::tcp::State;

    use super::owner::StackOwner;
    use super::socket_helpers::{
        prepare_udp_oom_gate, socket_epoll_collect, socket_listener, socket_read_exact,
        socket_write_all,
    };
    use super::support::{
        child_operation, observe, operation, pattern, socket_retired, ADDRESS, RW, SERVER,
    };
    use crate::fs::epoll::{EpollEvent, EpollFile, EPOLLET, EPOLL_CTL_ADD};
    use crate::fs::{
        FileIoContext, FileLike, FileRef, FsError, PollMask, Pollable, ReadBuffer, WriteBuffer,
    };
    use crate::ktest::execution_services::SuiteBaseline;
    use crate::mm::space::Mm;
    use crate::net::address::{AddressFamily, InetEndpoint, SocketAddress};
    use crate::net::core::ProtocolDeadline;
    use crate::net::socket::SocketFile;
    use crate::net::table::TcpTuple;
    use crate::net::{IoProgress, UnixSocketType};
    use crate::{cpu, time, timer};

    /// 四页槽满后读一字节不归槽，跨槽 PEEK 部分 fault 保留全文；整页释放才写就绪。
    #[test]
    fn unix_shared_fifo_slots_and_cross_page_peek_fault(
        context: &mut TestContext<'_>,
        fixture: &mut StackOwner,
    ) -> TestResult {
        let [sender, receiver] = operation(
            context,
            fixture.net.try_new_unix_pair(UnixSocketType::Stream),
        )?;
        let mut fifo = crate::fs::pipe::PipeBuffer::<4>::new();
        let page: [u8; 4096] = ::core::array::from_fn(pattern);
        for _ in 0..4 {
            context.expect_eq(
                operation(context, sender.send(&page))?,
                IoProgress::Ready(page.len()),
            );
            context.expect_eq(
                operation(context, fifo.append(&mut WriteBuffer::kernel(&page)))?,
                page.len(),
            );
        }
        let mut bytes = [0u8; 4096];
        for count in [1, 4088] {
            context.expect_eq(
                operation(
                    context,
                    receiver.receive(&mut ReadBuffer::kernel(&mut bytes[..count]), false),
                )?,
                IoProgress::Ready(count),
            );
            context.expect_eq(
                operation(
                    context,
                    fifo.receive(&mut ReadBuffer::kernel(&mut bytes[..count]), false),
                )?,
                count,
            );
            context.expect_eq(
                operation(context, sender.poll(None))?.contains(PollMask::POLLOUT),
                false,
            );
            context.expect_eq(fifo.empty_slot_capacity(), 0);
        }
        let memory = Mm::try_new().map_err(|_| TestError("UNIX MM allocation failed"))?;
        memory
            .map_anonymous(ADDRESS, 4096, RW, RW)
            .map_err(|_| TestError("UNIX fault page mapping failed"))?;
        for unix in [true, false] {
            let mut target = operation(context, ReadBuffer::user(&memory, ADDRESS + 4096 - 9, 32))?;
            if unix {
                context.expect_eq(
                    operation(context, receiver.receive(&mut target, true))?,
                    IoProgress::Ready(9),
                );
            } else {
                context.expect_eq(operation(context, fifo.receive(&mut target, true))?, 9);
            }
            context.expect_eq(target.copied(), 9);
            let mut peeked = [0u8; 9];
            context.expect_eq(memory.copyin(&mut peeked, ADDRESS + 4096 - 9).copied, 9);
            context.expect_eq(&peeked[..7], &page[4089..]);
            context.expect_eq(&peeked[7..], &page[..2]);
        }
        context.expect_eq(
            operation(context, sender.poll(None))?.contains(PollMask::POLLOUT),
            false,
        );
        context.expect_eq(
            operation(
                context,
                receiver.receive(&mut ReadBuffer::kernel(&mut bytes[..7]), false),
            )?,
            IoProgress::Ready(7),
        );
        context.expect_eq(&bytes[..7], &page[4089..]);
        context.expect_eq(
            operation(
                context,
                fifo.receive(&mut ReadBuffer::kernel(&mut bytes[..7]), false),
            )?,
            7,
        );
        context.expect_eq(&bytes[..7], &page[4089..]);
        context.expect_eq(
            operation(context, sender.poll(None))?.contains(PollMask::POLLOUT),
            true,
        );
        context.expect_eq(fifo.empty_slot_capacity(), page.len());
        for _ in 0..3 {
            context.expect_eq(
                operation(
                    context,
                    receiver.receive(&mut ReadBuffer::kernel(&mut bytes), false),
                )?,
                IoProgress::Ready(page.len()),
            );
            context.expect_eq(bytes, page);
            context.expect_eq(
                operation(
                    context,
                    fifo.receive(&mut ReadBuffer::kernel(&mut bytes), false),
                )?,
                page.len(),
            );
            context.expect_eq(bytes, page);
        }
        sender.close();
        receiver.close();
        drop((sender, receiver, fifo, memory));
        socket_retired(&fixture.net)
    }

    /// 正常原页压力使新槽零进度 ENOMEM，旧页保图样，放压后同方向再次发送。
    #[test]
    fn unix_shared_fifo_real_page_oom_recovers(
        context: &mut TestContext<'_>,
        fixture: &mut StackOwner,
    ) -> TestResult {
        let [sender, receiver] = operation(
            context,
            fixture.net.try_new_unix_pair(UnixSocketType::Stream),
        )?;
        let mut fifo = crate::fs::pipe::PipeBuffer::<4>::new();
        let page = [0x79; 4096];
        context.expect_eq(
            operation(context, sender.send(&page))?,
            IoProgress::Ready(page.len()),
        );
        context.expect_eq(
            operation(context, fifo.append(&mut WriteBuffer::kernel(&page)))?,
            page.len(),
        );
        let mut pressure = Vec::new();
        pressure
            .try_reserve_exact(crate::mm::stats().present_pages)
            .map_err(|_| TestError("UNIX page pressure slots OOM"))?;
        crate::mm::shrink_all();
        crate::mm::drain_all_pages();
        for order in [10, 0] {
            while let Some(page) = crate::mm::alloc_pages(order, false) {
                pressure.push(page);
            }
        }
        crate::mm::drain_all_pages();
        context.expect_eq(crate::mm::alloc_pages(0, false).is_none(), true);
        context.expect_eq(sender.send(b"new page"), Err(FsError::NoMemory));
        let mut source = WriteBuffer::kernel(b"new page");
        context.expect_eq(fifo.append(&mut source), Err(FsError::NoMemory));
        context.expect_eq(source.copied(), 0);
        let mut bytes = [0u8; 4096];
        context.expect_eq(
            receiver.receive(&mut ReadBuffer::kernel(&mut bytes), false),
            Ok(IoProgress::Ready(page.len())),
        );
        context.expect_eq(bytes, page);
        context.expect_eq(
            fifo.receive(&mut ReadBuffer::kernel(&mut bytes), false),
            Ok(page.len()),
        );
        context.expect_eq(bytes, page);
        drop(pressure);
        context.expect_eq(
            operation(context, sender.send(b"after OOM"))?,
            IoProgress::Ready(9),
        );
        context.expect_eq(
            operation(
                context,
                receiver.receive(&mut ReadBuffer::kernel(&mut bytes), false),
            )?,
            IoProgress::Ready(9),
        );
        context.expect_eq(&bytes[..9], b"after OOM");
        context.expect_eq(
            operation(context, fifo.append(&mut WriteBuffer::kernel(b"after OOM")))?,
            9,
        );
        context.expect_eq(
            operation(
                context,
                fifo.receive(&mut ReadBuffer::kernel(&mut bytes), false),
            )?,
            9,
        );
        context.expect_eq(&bytes[..9], b"after OOM");
        sender.close();
        receiver.close();
        drop((sender, receiver, fifo));
        socket_retired(&fixture.net)
    }

    /// 已领取 child 正常退休不得重发旧 ready 的 ET；后继握手仍须产生新事件并正常归还全部端点。
    #[test]
    fn accepted_child_retirement_does_not_rearm_listener_edge(
        context: &mut TestContext<'_>,
        fixture: &mut StackOwner,
    ) -> TestResult {
        let local = InetEndpoint {
            port: 45650,
            ..SERVER
        };
        let listener = socket_listener(context, &fixture.net, local.port)?;
        let listener_weak = Arc::downgrade(listener.tcp_endpoint().unwrap());
        let listener_file_weak = Arc::downgrade(&listener);
        let listener_watch_weak = Arc::downgrade(
            listener
                .watch_head()
                .ok_or(TestError("native listener has no real watch head"))?,
        );
        let client_a = operation(
            context,
            SocketFile::try_new_tcp(fixture.net.clone(), AddressFamily::Ipv4, 0, 0, 0),
        )?;
        operation(
            context,
            client_a.connect(SocketAddress::Inet(local), FileIoContext::Kernel),
        )?;
        let server_a = operation(context, listener.accept(0, 0, 0, FileIoContext::Kernel))?;
        context.expect_eq(Arc::ptr_eq(&listener, &server_a), false);
        context.expect_eq(
            Arc::ptr_eq(
                listener.tcp_endpoint().unwrap(),
                server_a.tcp_endpoint().unwrap(),
            ),
            false,
        );
        context.expect_eq(
            Arc::ptr_eq(
                client_a.tcp_endpoint().unwrap(),
                server_a.tcp_endpoint().unwrap(),
            ),
            false,
        );
        context.expect_eq(listener.tcp_endpoint().unwrap().state(), State::Listen);
        let server_a_weak = Arc::downgrade(server_a.tcp_endpoint().unwrap());
        let server_a_file_weak = Arc::downgrade(&server_a);
        let client_a_weak = Arc::downgrade(client_a.tcp_endpoint().unwrap());

        let epoll = operation(context, EpollFile::try_new(0))?;
        let epoll_weak = Arc::downgrade(&epoll);
        let expected = EpollEvent {
            events: PollMask::POLLIN.0,
            data: 0xa1b2_c3d4_e5f6_0758,
        };
        {
            let owner: FileRef = epoll.clone();
            let target: FileRef = listener.clone();
            operation(
                context,
                epoll.control(
                    &owner,
                    EPOLL_CTL_ADD,
                    &target,
                    0,
                    Some(EpollEvent {
                        events: expected.events | EPOLLET,
                        data: expected.data,
                    }),
                ),
            )?;
        }
        let client_b = operation(
            context,
            SocketFile::try_new_tcp(fixture.net.clone(), AddressFamily::Ipv4, 0, 0, 0),
        )?;
        operation(
            context,
            client_b.connect(SocketAddress::Inet(local), FileIoContext::Kernel),
        )?;
        let readable = PollMask::POLLIN | PollMask::POLLRDNORM;
        observe(
            || listener.poll(None) == Ok(readable) && epoll.poll(None) == Ok(readable),
            "second handshake did not make listener and real epoll readable",
        )?;
        let peer_b = operation(context, client_b.tcp_endpoint().unwrap().local_address())?;
        let server_b_weak = Arc::downgrade(
            &fixture
                .net
                .tables()
                .lookup_tcp(TcpTuple {
                    local,
                    peer: peer_b,
                })
                .ok_or(TestError("ready B is missing from actual TCP tuple table"))?,
        );
        let client_b_weak = Arc::downgrade(client_b.tcp_endpoint().unwrap());
        let mut events = [EpollEvent::default(); 1];
        let first = operation(context, socket_epoll_collect(&epoll, &mut events))?;
        context.expect_eq(first, 1);
        context.expect_eq(events[0], expected);
        if first != 1 || events[0] != expected {
            return Err(TestError(
                "ready B did not deliver exactly one listener IN/data event",
            ));
        }
        let second = operation(context, socket_epoll_collect(&epoll, &mut events))?;
        context.expect_eq(second, 0);
        if second != 0 {
            return Err(TestError(
                "listener edge was not consumed before accepted A retirement",
            ));
        }

        operation(context, client_a.shutdown(1))?;
        context.expect_eq(operation(context, server_a.read(&mut [0u8; 1]))?, 0);
        operation(context, server_a.shutdown(1))?;
        context.expect_eq(operation(context, client_a.read(&mut [0u8; 1]))?, 0);
        drop(server_a);
        context.expect_eq(server_a_file_weak.upgrade().is_none(), true);
        // 被动端最后 Weak 失效覆盖退休者等待 Timer/Work、撤 parent child 和释放表拥有的全尾。
        observe(
            || server_a_weak.upgrade().is_none(),
            "accepted A did not finish normal FIN/EOF endpoint retirement",
        )?;
        context.expect_eq(listener.tcp_endpoint().unwrap().state(), State::Listen);
        context.expect_eq(operation(context, listener.poll(None))?, readable);
        context.expect_eq(
            server_b_weak.upgrade().map(|endpoint| endpoint.state()),
            Some(State::Established),
        );
        let third = operation(context, socket_epoll_collect(&epoll, &mut events))?;
        context.diagnostic(format_args!(
            "network-socket accepted_a_retired={} listener_state={:?} listener_mask={:?} retirement_scan_count={} retirement_scan_event={:?}",
            server_a_weak.upgrade().is_none(), listener.tcp_endpoint().unwrap().state(),
            operation(context, listener.poll(None))?, third,
            if third == 0 { None } else { Some(events[0]) }
        ));
        context.expect_eq(third, 0);
        if third != 0 {
            return Err(TestError(
                "accepted child retirement rearmed listener ET without a new ready child",
            ));
        }

        let client_c = operation(
            context,
            SocketFile::try_new_tcp(fixture.net.clone(), AddressFamily::Ipv4, 0, 0, 0),
        )?;
        operation(
            context,
            client_c.connect(SocketAddress::Inet(local), FileIoContext::Kernel),
        )?;
        observe(
            || epoll.poll(None) == Ok(readable),
            "new ready C did not notify the consumed listener edge",
        )?;
        let peer_c = operation(context, client_c.tcp_endpoint().unwrap().local_address())?;
        let server_c_weak = Arc::downgrade(
            &fixture
                .net
                .tables()
                .lookup_tcp(TcpTuple {
                    local,
                    peer: peer_c,
                })
                .ok_or(TestError("ready C is missing from actual TCP tuple table"))?,
        );
        let client_c_weak = Arc::downgrade(client_c.tcp_endpoint().unwrap());
        let fourth = operation(context, socket_epoll_collect(&epoll, &mut events))?;
        context.expect_eq(fourth, 1);
        context.expect_eq(events[0], expected);
        if fourth != 1 || events[0] != expected {
            return Err(TestError(
                "new ready C did not deliver exactly one listener IN/data event",
            ));
        }
        context.expect_eq(
            operation(context, socket_epoll_collect(&epoll, &mut events))?,
            0,
        );

        let server_b = operation(context, listener.accept(0, 0, 0, FileIoContext::Kernel))?;
        let server_c = operation(context, listener.accept(0, 0, 0, FileIoContext::Kernel))?;
        context.expect_eq(
            server_b_weak
                .upgrade()
                .is_some_and(|endpoint| Arc::ptr_eq(&endpoint, server_b.tcp_endpoint().unwrap())),
            true,
        );
        context.expect_eq(
            server_c_weak
                .upgrade()
                .is_some_and(|endpoint| Arc::ptr_eq(&endpoint, server_c.tcp_endpoint().unwrap())),
            true,
        );
        context.expect_eq(Arc::ptr_eq(&listener, &server_b), false);
        context.expect_eq(Arc::ptr_eq(&listener, &server_c), false);
        context.expect_eq(Arc::ptr_eq(&server_b, &server_c), false);
        context.expect_eq(
            Arc::ptr_eq(
                server_b.tcp_endpoint().unwrap(),
                server_c.tcp_endpoint().unwrap(),
            ),
            false,
        );
        context.expect_eq(
            Arc::ptr_eq(
                server_b.tcp_endpoint().unwrap(),
                client_b.tcp_endpoint().unwrap(),
            ),
            false,
        );
        context.expect_eq(
            Arc::ptr_eq(
                server_c.tcp_endpoint().unwrap(),
                client_c.tcp_endpoint().unwrap(),
            ),
            false,
        );
        context.expect_eq(listener.tcp_endpoint().unwrap().state(), State::Listen);
        context.expect_eq(
            operation(context, listener.poll(None))?,
            PollMask::default(),
        );
        for (client, server) in [(&client_b, &server_b), (&client_c, &server_c)] {
            operation(context, client.shutdown(1))?;
            context.expect_eq(operation(context, server.read(&mut [0u8; 1]))?, 0);
            operation(context, server.shutdown(1))?;
            context.expect_eq(operation(context, client.read(&mut [0u8; 1]))?, 0);
        }
        drop((
            epoll, listener, client_a, client_b, client_c, server_b, server_c,
        ));
        context.expect_eq(epoll_weak.upgrade().is_none(), true);
        context.expect_eq(listener_file_weak.upgrade().is_none(), true);
        context.expect_eq(listener_watch_weak.upgrade().is_none(), true);
        socket_retired(&fixture.net)?;
        for endpoint in [
            &listener_weak,
            &server_a_weak,
            &client_a_weak,
            &server_b_weak,
            &client_b_weak,
            &server_c_weak,
            &client_c_weak,
        ] {
            context.expect_eq(endpoint.upgrade().is_none(), true);
        }
        context.expect_eq(fixture.net.ktest_live_endpoints(), 0);
        context.expect_eq(fixture.net.is_stopping(), false);
        drop((
            epoll_weak,
            listener_file_weak,
            listener_watch_weak,
            server_a_file_weak,
            listener_weak,
            server_a_weak,
            client_a_weak,
            server_b_weak,
            client_b_weak,
            server_c_weak,
            client_c_weak,
        ));
        Ok(())
    }

    /// 首笔握手 ACK 丢失后仅用 FIN 完成被动握手，accept 必须交付同一 child 并保留反向流。
    #[test]
    fn passive_fin_ack_matures_before_accept(
        context: &mut TestContext<'_>,
        fixture: &mut StackOwner,
    ) -> TestResult {
        let local = InetEndpoint {
            port: 45640,
            ..SERVER
        };
        let peer = InetEndpoint {
            port: 45641,
            ..SERVER
        };
        let listener = socket_listener(context, &fixture.net, local.port)?;
        operation(context, listener.set_status_flags(0o4000))?;
        let client = operation(
            context,
            SocketFile::try_new_tcp(fixture.net.clone(), AddressFamily::Ipv4, 0, 0, 0),
        )?;
        operation(context, client.bind(SocketAddress::Inet(peer)))?;
        operation(
            context,
            fixture.net.ktest_drop_handshake_ack(peer.port, local.port),
        )?;
        operation(
            context,
            client.connect(SocketAddress::Inet(local), FileIoContext::Kernel),
        )?;
        observe(
            || fixture.net.ktest_handshake_ack_dropped(),
            "selected handshake ACK was not dropped",
        )?;
        let child = fixture
            .net
            .tables()
            .lookup_tcp(TcpTuple { local, peer })
            .ok_or(TestError(
                "passive handshake child is missing from actual TCP tuple table",
            ))?;
        let initial_state = child.state();
        context.diagnostic(format_args!(
            "network-socket handshake_ack_dropped={} child_id={} initial_state={:?} listener_state={:?}",
            fixture.net.ktest_handshake_ack_dropped(), child.id(), initial_state, listener.tcp_endpoint().unwrap().state()
        ));
        if initial_state != State::SynReceived {
            return Err(TestError(
                "dropped handshake ACK did not leave actual child in SYN-RECEIVED",
            ));
        }
        context.expect_eq(client.tcp_endpoint().unwrap().state(), State::Established);
        context.expect_eq(listener.tcp_endpoint().unwrap().state(), State::Listen);

        // 首笔应用发送就是 FIN；任何先行 payload 都会另带 ACK，使这个被动握手窗口消失。
        operation(context, client.shutdown(1))?;
        observe(
            || child.state() == State::CloseWait,
            "passive child did not process actual FIN into CLOSE-WAIT",
        )?;
        let fin_state = child.state();
        let accepted = listener.accept(0, 0, 0, FileIoContext::Kernel);
        let server = match accepted {
            Ok(server) => {
                context.diagnostic(format_args!(
                    "network-socket handshake_ack_dropped={} child_id={} fin_state={:?} accept_result=Ok accepted_child_id={}",
                    fixture.net.ktest_handshake_ack_dropped(), child.id(), fin_state, server.tcp_endpoint().unwrap().id()
                ));
                server
            }
            Err(error) => {
                context.diagnostic(format_args!(
                    "network-socket handshake_ack_dropped={} child_id={} fin_state={:?} accept_result=Err({:?}) listener_state={:?}",
                    fixture.net.ktest_handshake_ack_dropped(), child.id(), fin_state, error, listener.tcp_endpoint().unwrap().state()
                ));
                return Err(TestError(match error {
                    FsError::WouldBlock => {
                        "passive FIN completed handshake but nonblocking accept returned WouldBlock"
                    }
                    _ => "passive FIN nonblocking accept failed",
                }));
            }
        };
        context.expect_eq(Arc::ptr_eq(server.tcp_endpoint().unwrap(), &child), true);
        context.expect_eq(Arc::ptr_eq(&listener, &server), false);
        context.expect_eq(listener.tcp_endpoint().unwrap().state(), State::Listen);
        context.expect_eq(operation(context, server.read(&mut [0u8; 1]))?, 0);
        context.expect_eq(server.tcp_endpoint().unwrap().state(), State::CloseWait);
        socket_write_all(&server, b"accepted after FIN")?;
        let mut output = [0u8; 18];
        socket_read_exact(&client, &mut output)?;
        context.expect_eq(&output, b"accepted after FIN");
        operation(context, server.shutdown(1))?;
        context.expect_eq(operation(context, client.read(&mut [0u8; 1]))?, 0);
        context.expect_eq(listener.tcp_endpoint().unwrap().state(), State::Listen);
        let child_weak = Arc::downgrade(&child);
        let client_weak = Arc::downgrade(client.tcp_endpoint().unwrap());
        let server_file_weak = Arc::downgrade(&server);
        let client_file_weak = Arc::downgrade(&client);
        operation(context, fixture.net.ktest_clear_packet_control())?;
        drop((child, client, server, listener));
        context.expect_eq(client_file_weak.upgrade().is_none(), true);
        context.expect_eq(server_file_weak.upgrade().is_none(), true);
        socket_retired(&fixture.net)?;
        context.expect_eq(child_weak.upgrade().is_none(), true);
        context.expect_eq(client_weak.upgrade().is_none(), true);
        context.expect_eq(fixture.net.ktest_live_endpoints(), 0);
        context.expect_eq(fixture.net.is_stopping(), false);
        drop((child_weak, client_weak, server_file_weak, client_file_weak));
        Ok(())
    }

    /// A 的真实处理拥有持续期间，另一 CPU 的 B 普通文件收发和后台 ACK 独立完成。
    #[test]
    #[ktest(min_cpus = 2)]
    fn socket_other_cpu_completes_while_connection_is_owned(
        context: &mut TestContext<'_>,
        fixture: &mut StackOwner,
    ) -> TestResult {
        let listener_a = socket_listener(context, &fixture.net, 45610)?;
        let listener_b = socket_listener(context, &fixture.net, 45611)?;
        let client_a = operation(
            context,
            SocketFile::try_new_tcp(fixture.net.clone(), AddressFamily::Ipv4, 0, 0, 0),
        )?;
        let client_b = operation(
            context,
            SocketFile::try_new_tcp(fixture.net.clone(), AddressFamily::Ipv4, 0, 0, 0),
        )?;
        let cpu_a = client_a.tcp_endpoint().unwrap().work_cpu();
        let cpu_b = client_b.tcp_endpoint().unwrap().work_cpu();
        if cpu_a == cpu_b {
            return Err(TestError(
                "native socket clients do not have distinct protocol CPUs",
            ));
        }
        operation(
            context,
            client_a.connect(
                SocketAddress::Inet(InetEndpoint {
                    port: 45610,
                    ..SERVER
                }),
                FileIoContext::Kernel,
            ),
        )?;
        operation(
            context,
            client_b.connect(
                SocketAddress::Inet(InetEndpoint {
                    port: 45611,
                    ..SERVER
                }),
                FileIoContext::Kernel,
            ),
        )?;
        let server_a = operation(context, listener_a.accept(0, 0, 0, FileIoContext::Kernel))?;
        let server_b = operation(context, listener_b.accept(0, 0, 0, FileIoContext::Kernel))?;
        let held = Arc::try_new(AtomicBool::new(false))
            .map_err(|_| TestError("native socket ownership observation allocation failed"))?;
        let held_by_a = held.clone();
        let release = fixture.release.clone();
        let a = client_a.clone();
        let holder = crate::ktest::spawn_result_on(cpu_a, move |token| {
            let guard = a.tcp_endpoint().unwrap().lock();
            if cpu::current().id != cpu_a || guard.state() != State::Established {
                return Err(TestError("native socket owner has wrong CPU or state"));
            }
            held_by_a.store(true, Ordering::Release);
            while !release.load(Ordering::Acquire) && !token.stop_requested() {
                timer::sleep(time::Duration::from_millis(1));
            }
            drop(guard);
            Ok(0)
        })?;
        observe(
            || held.load(Ordering::Acquire),
            "native A did not take processing ownership",
        )?;
        let b = client_b.clone();
        let b_complete = Arc::try_new(AtomicBool::new(false))
            .map_err(|_| TestError("native B completion observation allocation failed"))?;
        let completed = b_complete.clone();
        let child = crate::ktest::spawn_result_on(cpu_b, move |_| {
            if cpu::current().id != cpu_b {
                return Err(TestError("native B ran on wrong CPU"));
            }
            let input: [u8; 97] = ::core::array::from_fn(pattern);
            socket_write_all(&b, &input)?;
            let mut reply = [0u8; 97];
            socket_read_exact(&b, &mut reply)?;
            if reply != input {
                return Err(TestError("native B response bytes differ"));
            }
            completed.store(true, Ordering::Release);
            Ok(0)
        })?;
        let mut output = [0u8; 97];
        socket_read_exact(&server_b, &mut output)?;
        context.expect_eq(output, ::core::array::from_fn(pattern));
        socket_write_all(&server_b, &output)?;
        observe(
            || b_complete.load(Ordering::Acquire),
            "native B stalled under A ownership",
        )?;
        context.expect_eq(fixture.release.load(Ordering::Acquire), false);
        context.expect_eq(child.join()?, 0);
        fixture.release.store(true, Ordering::Release);
        context.expect_eq(holder.join()?, 0);
        drop((
            client_a, client_b, server_a, server_b, listener_a, listener_b,
        ));
        socket_retired(&fixture.net)
    }

    /// 只丢一笔普通 data 段，真实 raw At/RTO 后同 seq 重到达并交付全部字节。
    #[test]
    fn one_data_segment_retransmits_at_real_raw_deadline(
        context: &mut TestContext<'_>,
        fixture: &mut StackOwner,
    ) -> TestResult {
        let listener = socket_listener(context, &fixture.net, 45620)?;
        let client = operation(
            context,
            SocketFile::try_new_tcp(fixture.net.clone(), AddressFamily::Ipv4, 0, 0, 0),
        )?;
        operation(
            context,
            client.bind(SocketAddress::Inet(InetEndpoint {
                port: 45621,
                ..SERVER
            })),
        )?;
        operation(
            context,
            client.connect(
                SocketAddress::Inet(InetEndpoint {
                    port: 45620,
                    ..SERVER
                }),
                FileIoContext::Kernel,
            ),
        )?;
        let server = operation(context, listener.accept(0, 0, 0, FileIoContext::Kernel))?;
        let healthy_listener = socket_listener(context, &fixture.net, 45622)?;
        let healthy_client = operation(
            context,
            SocketFile::try_new_tcp(fixture.net.clone(), AddressFamily::Ipv4, 0, 0, 0),
        )?;
        operation(
            context,
            healthy_client.connect(
                SocketAddress::Inet(InetEndpoint {
                    port: 45622,
                    ..SERVER
                }),
                FileIoContext::Kernel,
            ),
        )?;
        let healthy_server = operation(
            context,
            healthy_listener.accept(0, 0, 0, FileIoContext::Kernel),
        )?;
        operation(context, fixture.net.ktest_drop_next_data(45621, 45620))?;
        let input: [u8; 97] = ::core::array::from_fn(pattern);
        context.expect_eq(operation(context, client.write(&input))?, input.len());
        observe(
            || fixture.net.ktest_loss_observation().dropped == 1,
            "selected data segment was not dropped",
        )?;
        let first = fixture.net.ktest_loss_observation();
        let deadline = match client.tcp_endpoint().unwrap().lock().deadline(time::now()) {
            ProtocolDeadline::At(at) => at,
            _ => {
                return Err(TestError(
                    "dropped data did not retain actual raw retransmission deadline",
                ))
            }
        };
        if deadline.as_nanos() <= first.first_ns
            || first.retransmissions != 0
            || first.sequence.is_none()
        {
            return Err(TestError("data drop observation did not precede raw RTO"));
        }
        let mut output = [0u8; 97];
        socket_read_exact(&server, &mut output)?;
        context.expect_eq(output, input);
        let loss = fixture.net.ktest_loss_observation();
        context.expect_eq(loss.dropped, 1);
        context.expect_eq(loss.sequence, first.sequence);
        if loss.retransmissions == 0 || loss.repeated_ns < deadline.as_nanos() {
            return Err(TestError(
                "data arrived without actual At deadline retransmission",
            ));
        }
        context.expect_eq(client.tcp_endpoint().unwrap().state(), State::Established);
        context.expect_eq(server.tcp_endpoint().unwrap().state(), State::Established);
        context.expect_eq(
            healthy_client.tcp_endpoint().unwrap().state(),
            State::Established,
        );
        context.expect_eq(
            healthy_server.tcp_endpoint().unwrap().state(),
            State::Established,
        );
        socket_write_all(&healthy_client, &input)?;
        socket_read_exact(&healthy_server, &mut output)?;
        context.expect_eq(output, input);
        context.diagnostic(format_args!(
            "network-socket controlled_drop={} sequence={:?} first_ns={} raw_deadline_ns={} repeated_ns={} retransmissions={}",
            loss.dropped, loss.sequence, loss.first_ns, deadline.as_nanos(), loss.repeated_ns, loss.retransmissions
        ));
        operation(context, fixture.net.ktest_clear_packet_control())?;
        drop((
            client,
            server,
            listener,
            healthy_client,
            healthy_server,
            healthy_listener,
        ));
        socket_retired(&fixture.net)
    }

    /// 末 OFD 先释放 watch，协议仍送 FIN 并进入真实 TIME_WAIT，最终 Weak 和 slot 同时归还。
    #[test]
    fn last_ofd_fin_time_wait_and_endpoint_slot_retire_normally(
        context: &mut TestContext<'_>,
        fixture: &mut StackOwner,
    ) -> TestResult {
        let listener = socket_listener(context, &fixture.net, 45630)?;
        let client = operation(
            context,
            SocketFile::try_new_tcp(fixture.net.clone(), AddressFamily::Ipv4, 0, 1000, 2000),
        )?;
        operation(
            context,
            client.connect(
                SocketAddress::Inet(InetEndpoint {
                    port: 45630,
                    ..SERVER
                }),
                FileIoContext::Kernel,
            ),
        )?;
        let server = operation(context, listener.accept(0, 0, 0, FileIoContext::Kernel))?;
        let file_weak = Arc::downgrade(&client);
        let client_weak = Arc::downgrade(client.tcp_endpoint().unwrap());
        let server_weak = Arc::downgrade(server.tcp_endpoint().unwrap());
        socket_write_all(&client, b"tail before FIN")?;
        drop(client);
        context.expect_eq(file_weak.upgrade().is_none(), true);
        context.expect_eq(client_weak.upgrade().is_some(), true);
        let mut output = [0u8; 15];
        socket_read_exact(&server, &mut output)?;
        context.expect_eq(&output, b"tail before FIN");
        context.expect_eq(operation(context, server.read(&mut [0u8; 1]))?, 0);
        context.expect_eq(server.tcp_endpoint().unwrap().state(), State::CloseWait);
        drop(server);
        observe(
            || {
                client_weak
                    .upgrade()
                    .is_some_and(|endpoint| endpoint.state() == State::TimeWait)
            },
            "last OFD did not progress through real TIME_WAIT",
        )?;
        context.expect_eq(fixture.net.is_stopping(), false);
        context.expect_eq(0 < fixture.net.ktest_live_endpoints(), true);
        drop(listener);
        socket_retired(&fixture.net)?;
        context.expect_eq(client_weak.upgrade().is_none(), true);
        context.expect_eq(server_weak.upgrade().is_none(), true);
        context.expect_eq(fixture.net.ktest_live_endpoints(), 0);
        drop((file_weak, client_weak, server_weak));
        Ok(())
    }

    /// TCP_NODELAY 修改真正 raw Nagle，child 继承后仍可独立修改而不改变 listener。
    #[test]
    fn socket_options_modify_raw_nagle_and_child_inherits(
        context: &mut TestContext<'_>,
        fixture: &mut StackOwner,
    ) -> TestResult {
        let listener = operation(
            context,
            SocketFile::try_new_tcp(fixture.net.clone(), AddressFamily::Ipv4, 0, 0, 0),
        )?;
        let endpoint = operation(context, listener.tcp_endpoint())?;
        context.expect_eq(endpoint.lock().nagle_enabled(), true);
        operation(context, listener.set_nodelay(true))?;
        context.expect_eq(endpoint.lock().nagle_enabled(), false);
        operation(context, listener.set_nodelay(false))?;
        context.expect_eq(endpoint.lock().nagle_enabled(), true);
        operation(context, listener.set_nodelay(true))?;
        operation(context, listener.set_reuse_address(true))?;
        operation(context, endpoint.set_receive_budget(1024))?;
        operation(context, endpoint.set_send_budget(2048))?;
        operation(context, endpoint.set_keepalive(true))?;
        operation(
            context,
            listener.bind(SocketAddress::Inet(InetEndpoint {
                port: 45660,
                ..SERVER
            })),
        )?;
        operation(context, listener.listen(4))?;
        let client = operation(
            context,
            SocketFile::try_new_tcp(fixture.net.clone(), AddressFamily::Ipv4, 0, 0, 0),
        )?;
        operation(
            context,
            client.connect(
                SocketAddress::Inet(InetEndpoint {
                    port: 45660,
                    ..SERVER
                }),
                FileIoContext::Kernel,
            ),
        )?;
        let child = operation(context, listener.accept(0, 0, 0, FileIoContext::Kernel))?;
        let child_endpoint = operation(context, child.tcp_endpoint())?;
        context.expect_eq(child_endpoint.receive_budget(), 1024);
        context.expect_eq(child_endpoint.send_budget(), 2048);
        context.expect_eq(
            child_endpoint.receive_capacity(),
            endpoint.receive_capacity(),
        );
        context.expect_eq(child_endpoint.send_capacity(), endpoint.send_capacity());
        context.expect_eq(child_endpoint.keepalive(), true);
        context.expect_eq(
            child_endpoint.congestion_control(),
            smoltcp::socket::tcp::CongestionControl::Reno,
        );
        operation(context, child_endpoint.set_keepalive(false))?;
        context.expect_eq(child_endpoint.keepalive(), false);
        context.expect_eq(endpoint.keepalive(), true);
        context.expect_eq(
            operation(context, child.tcp_endpoint())?
                .lock()
                .nagle_enabled(),
            false,
        );
        context.expect_eq(child.reuse_address(), true);
        operation(context, child.set_nodelay(false))?;
        operation(context, child.set_reuse_address(false))?;
        context.expect_eq(
            operation(context, child.tcp_endpoint())?
                .lock()
                .nagle_enabled(),
            true,
        );
        context.expect_eq(endpoint.lock().nagle_enabled(), false);
        context.expect_eq(listener.reuse_address(), true);
        socket_write_all(&client, b"raw policy child")?;
        let mut output = [0u8; 16];
        socket_read_exact(&child, &mut output)?;
        context.expect_eq(&output, b"raw policy child");
        operation(context, client.shutdown(1))?;
        context.expect_eq(operation(context, child.read(&mut [0u8; 1]))?, 0);
        operation(context, child.shutdown(1))?;
        context.expect_eq(operation(context, client.read(&mut [0u8; 1]))?, 0);
        drop((child, client, listener));
        socket_retired(&fixture.net)?;
        context.expect_eq(fixture.net.is_stopping(), false);
        Ok(())
    }

    /// 有序记录不合并；PEEK、短读和零容量 recv 保原长，空消息与关闭 EOF 分别消费。
    #[test]
    fn unix_seqpacket_record_peek_truncation_empty_and_shutdown(
        context: &mut TestContext<'_>,
        fixture: &mut StackOwner,
    ) -> TestResult {
        let [sender, receiver] = operation(
            context,
            SocketFile::try_new_unix_pair(
                fixture.net.clone(),
                UnixSocketType::SeqPacket,
                0o4000,
                0,
                0,
            ),
        )?;
        context.expect_eq(sender.socket_type(), 5);
        context.expect_eq(receiver.socket_type(), 5);
        for bytes in [b"first".as_slice(), b"second".as_slice()] {
            context.expect_eq(
                operation(
                    context,
                    sender.send(
                        &mut WriteBuffer::kernel(bytes),
                        None,
                        0,
                        FileIoContext::Kernel,
                    ),
                )?,
                bytes.len(),
            );
        }
        let mut prefix = [0u8; 2];
        let peek = operation(
            context,
            receiver.receive(
                &mut ReadBuffer::kernel(&mut prefix),
                0x2,
                FileIoContext::Kernel,
            ),
        )?;
        context.expect_eq((peek.copied, peek.original_len, peek.flags), (2, 5, 0x20));
        context.expect_eq(&prefix, b"fi");
        context.expect_eq(peek.source, None);
        let short = operation(
            context,
            receiver.receive(
                &mut ReadBuffer::kernel(&mut prefix),
                0x20,
                FileIoContext::Kernel,
            ),
        )?;
        context.expect_eq(
            (short.copied, short.original_len, short.flags),
            (2, 5, 0x20),
        );
        context.expect_eq(&prefix, b"fi");
        let mut output = [0u8; 32];
        let next = operation(
            context,
            receiver.receive(
                &mut ReadBuffer::kernel(&mut output),
                0,
                FileIoContext::Kernel,
            ),
        )?;
        context.expect_eq((next.copied, next.original_len, next.flags), (6, 6, 0));
        context.expect_eq(&output[..next.copied], b"second");
        context.expect_eq(
            receiver.receive(
                &mut ReadBuffer::kernel(&mut output),
                0,
                FileIoContext::Kernel,
            ),
            Err(FsError::WouldBlock),
        );

        operation(
            context,
            sender.send(
                &mut WriteBuffer::kernel(b"discard"),
                None,
                0,
                FileIoContext::Kernel,
            ),
        )?;
        context.expect_eq(operation(context, receiver.read(&mut []))?, 0);
        context.expect_eq(
            operation(context, receiver.poll(None))?.contains(PollMask::POLLIN),
            true,
        );
        let zero_capacity = operation(
            context,
            receiver.receive(&mut ReadBuffer::kernel(&mut []), 0, FileIoContext::Kernel),
        )?;
        context.expect_eq(
            (
                zero_capacity.copied,
                zero_capacity.original_len,
                zero_capacity.flags,
            ),
            (0, 7, 0x20),
        );
        context.expect_eq(
            receiver.receive(
                &mut ReadBuffer::kernel(&mut output),
                0,
                FileIoContext::Kernel,
            ),
            Err(FsError::WouldBlock),
        );

        context.expect_eq(
            operation(
                context,
                sender.send(
                    &mut WriteBuffer::kernel(&[]),
                    None,
                    0,
                    FileIoContext::Kernel,
                ),
            )?,
            0,
        );
        context.expect_eq(operation(context, receiver.read(&mut []))?, 0);
        context.expect_eq(
            operation(context, receiver.poll(None))?.contains(PollMask::POLLIN),
            true,
        );
        let empty_peek = operation(
            context,
            receiver.receive(
                &mut ReadBuffer::kernel(&mut output),
                0x2,
                FileIoContext::Kernel,
            ),
        )?;
        context.expect_eq(
            (empty_peek.copied, empty_peek.original_len, empty_peek.flags),
            (0, 0, 0),
        );
        context.expect_eq(
            operation(context, receiver.poll(None))?.contains(PollMask::POLLIN),
            true,
        );
        let empty = operation(
            context,
            receiver.receive(&mut ReadBuffer::kernel(&mut []), 0, FileIoContext::Kernel),
        )?;
        context.expect_eq((empty.copied, empty.original_len, empty.flags), (0, 0, 0));
        context.expect_eq(
            operation(context, receiver.poll(None))?.contains(PollMask::POLLIN),
            false,
        );
        context.expect_eq(
            receiver.receive(
                &mut ReadBuffer::kernel(&mut output),
                0,
                FileIoContext::Kernel,
            ),
            Err(FsError::WouldBlock),
        );
        for bytes in [b"".as_slice(), b"tail".as_slice()] {
            context.expect_eq(
                operation(
                    context,
                    sender.send(
                        &mut WriteBuffer::kernel(bytes),
                        None,
                        0,
                        FileIoContext::Kernel,
                    ),
                )?,
                bytes.len(),
            );
        }
        operation(context, sender.shutdown(1))?;
        let empty = operation(
            context,
            receiver.receive(
                &mut ReadBuffer::kernel(&mut output),
                0,
                FileIoContext::Kernel,
            ),
        )?;
        context.expect_eq((empty.copied, empty.original_len, empty.flags), (0, 0, 0));
        context.expect_eq(operation(context, receiver.read(&mut output))?, 4);
        context.expect_eq(&output[..4], b"tail");
        context.expect_eq(operation(context, receiver.read(&mut output))?, 0);
        context.expect_eq(
            operation(context, receiver.poll(None))?.contains(PollMask::POLLRDHUP),
            true,
        );
        context.expect_eq(
            operation(
                context,
                receiver.send(
                    &mut WriteBuffer::kernel(b"reply"),
                    None,
                    0,
                    FileIoContext::Kernel,
                ),
            )?,
            5,
        );
        operation(context, receiver.shutdown(1))?;
        context.expect_eq(operation(context, sender.read(&mut output))?, 5);
        context.expect_eq(&output[..5], b"reply");
        context.expect_eq(operation(context, sender.read(&mut output))?, 0);
        for endpoint in [&sender, &receiver] {
            context.expect_eq(
                endpoint.send(
                    &mut WriteBuffer::kernel(&[]),
                    None,
                    0x4000,
                    FileIoContext::Kernel,
                ),
                Err(FsError::BrokenPipe),
            );
        }
        drop((sender, receiver));
        socket_retired(&fixture.net)?;
        context.expect_eq(fixture.net.ktest_live_endpoints(), 0);
        context.expect_eq(fixture.net.is_stopping(), false);
        Ok(())
    }

    /// 字节预算和32记录槽分别施加非阻塞背压，读取释放额度，末别名结束才归还节点。
    #[test]
    fn unix_seqpacket_capacity_and_last_ofd_return_slots(
        context: &mut TestContext<'_>,
        fixture: &mut StackOwner,
    ) -> TestResult {
        let [sender, receiver] = operation(
            context,
            SocketFile::try_new_unix_pair(
                fixture.net.clone(),
                UnixSocketType::SeqPacket,
                0o4000,
                0,
                0,
            ),
        )?;
        let alias = sender.clone();
        let weak = Arc::downgrade(&sender);
        drop(sender);
        context.expect_eq(weak.upgrade().is_some(), true);
        context.expect_eq(fixture.net.ktest_live_endpoints(), 2);
        let mut payload = operation(context, crate::net::packet::zeroed_bytes(16385))?;
        for (index, byte) in payload.iter_mut().enumerate() {
            *byte = pattern(index);
        }
        context.expect_eq(
            alias.send(
                &mut WriteBuffer::kernel(&payload),
                None,
                0,
                FileIoContext::Kernel,
            ),
            Err(FsError::MessageTooLarge),
        );
        let mut output = operation(context, crate::net::packet::zeroed_bytes(16384))?;
        context.expect_eq(
            operation(
                context,
                alias.send(
                    &mut WriteBuffer::kernel(&payload[..16384]),
                    None,
                    0,
                    FileIoContext::Kernel,
                ),
            )?,
            16384,
        );
        let maximum = operation(
            context,
            receiver.receive(
                &mut ReadBuffer::kernel(&mut output),
                0,
                FileIoContext::Kernel,
            ),
        )?;
        context.expect_eq(
            (maximum.copied, maximum.original_len, maximum.flags),
            (16384, 16384, 0),
        );
        context.expect_eq(output.as_slice(), &payload[..16384]);
        for bytes in [&payload[..8193], &payload[8193..16384]] {
            context.expect_eq(
                operation(
                    context,
                    alias.send(
                        &mut WriteBuffer::kernel(bytes),
                        None,
                        0,
                        FileIoContext::Kernel,
                    ),
                )?,
                bytes.len(),
            );
        }
        context.expect_eq(
            operation(context, alias.poll(None))?.contains(PollMask::POLLOUT),
            false,
        );
        context.expect_eq(
            alias.send(
                &mut WriteBuffer::kernel(&[]),
                None,
                0,
                FileIoContext::Kernel,
            ),
            Err(FsError::WouldBlock),
        );
        let mut first = [0u8; 1];
        let peek = operation(
            context,
            receiver.receive(
                &mut ReadBuffer::kernel(&mut first),
                0x2,
                FileIoContext::Kernel,
            ),
        )?;
        context.expect_eq(
            (peek.copied, peek.original_len, peek.flags),
            (1, 8193, 0x20),
        );
        context.expect_eq(first[0], payload[0]);
        context.expect_eq(
            operation(context, alias.poll(None))?.contains(PollMask::POLLOUT),
            false,
        );
        let short = operation(
            context,
            receiver.receive(
                &mut ReadBuffer::kernel(&mut first),
                0,
                FileIoContext::Kernel,
            ),
        )?;
        context.expect_eq(
            (short.copied, short.original_len, short.flags),
            (1, 8193, 0x20),
        );
        context.expect_eq(
            operation(context, alias.poll(None))?.contains(PollMask::POLLOUT),
            true,
        );
        for _ in 0..31 {
            context.expect_eq(
                operation(
                    context,
                    alias.send(
                        &mut WriteBuffer::kernel(&[]),
                        None,
                        0,
                        FileIoContext::Kernel,
                    ),
                )?,
                0,
            );
        }
        context.expect_eq(
            operation(context, alias.poll(None))?.contains(PollMask::POLLOUT),
            false,
        );
        context.expect_eq(
            alias.send(
                &mut WriteBuffer::kernel(b"z"),
                None,
                0,
                FileIoContext::Kernel,
            ),
            Err(FsError::WouldBlock),
        );
        let second = operation(
            context,
            receiver.receive(
                &mut ReadBuffer::kernel(&mut output),
                0,
                FileIoContext::Kernel,
            ),
        )?;
        context.expect_eq(
            (second.copied, second.original_len, second.flags),
            (8191, 8191, 0),
        );
        context.expect_eq(&output[..second.copied], &payload[8193..16384]);
        context.expect_eq(
            operation(
                context,
                alias.send(
                    &mut WriteBuffer::kernel(b"z"),
                    None,
                    0,
                    FileIoContext::Kernel,
                ),
            )?,
            1,
        );
        for _ in 0..31 {
            let empty = operation(
                context,
                receiver.receive(
                    &mut ReadBuffer::kernel(&mut first),
                    0,
                    FileIoContext::Kernel,
                ),
            )?;
            context.expect_eq((empty.copied, empty.original_len, empty.flags), (0, 0, 0));
        }
        context.expect_eq(operation(context, receiver.read(&mut first))?, 1);
        context.expect_eq(&first, b"z");
        context.expect_eq(
            receiver.receive(
                &mut ReadBuffer::kernel(&mut first),
                0,
                FileIoContext::Kernel,
            ),
            Err(FsError::WouldBlock),
        );
        drop(alias);
        context.expect_eq(weak.upgrade().is_none(), true);
        context.expect_eq(fixture.net.ktest_live_endpoints(), 1);
        context.expect_eq(operation(context, receiver.read(&mut first))?, 0);
        drop(receiver);
        socket_retired(&fixture.net)?;
        context.expect_eq(fixture.net.ktest_live_endpoints(), 0);
        context.expect_eq(fixture.net.is_stopping(), false);
        drop((weak, payload, output));
        Ok(())
    }

    /// UNIX 末 OFD 撤真实 watch，别名保活到最后 put，两个 slot 在根未停止时归还。
    #[test]
    fn unix_last_ofd_watch_weak_and_slots_retire_normally(
        context: &mut TestContext<'_>,
        fixture: &mut StackOwner,
    ) -> TestResult {
        let [first, second] = operation(
            context,
            SocketFile::try_new_unix_pair(fixture.net.clone(), UnixSocketType::Stream, 0, 0, 0),
        )?;
        context.expect_eq(fixture.net.ktest_live_endpoints(), 2);
        let alias = first.clone();
        let file_weak = Arc::downgrade(&first);
        let watch_weak = Arc::downgrade(
            first
                .watch_head()
                .ok_or(TestError("UNIX file has no watch"))?,
        );
        let epoll = operation(context, EpollFile::try_new(0))?;
        {
            let owner: FileRef = epoll.clone();
            let target: FileRef = first.clone();
            operation(
                context,
                epoll.control(
                    &owner,
                    EPOLL_CTL_ADD,
                    &target,
                    0,
                    Some(EpollEvent {
                        events: PollMask::POLLIN.0 | EPOLLET,
                        data: first.id(),
                    }),
                ),
            )?;
        }
        drop(first);
        context.expect_eq(file_weak.upgrade().is_some(), true);
        context.expect_eq(
            operation(
                context,
                second.send(
                    &mut WriteBuffer::kernel(b"last alias bytes"),
                    None,
                    0,
                    FileIoContext::Kernel,
                ),
            )?,
            16,
        );
        let mut events = [EpollEvent::default(); 1];
        context.expect_eq(
            operation(context, socket_epoll_collect(&epoll, &mut events))?,
            1,
        );
        context.expect_eq(events[0].data, alias.id());
        let mut output = [0u8; 16];
        socket_read_exact(&alias, &mut output)?;
        context.expect_eq(&output, b"last alias bytes");
        operation(context, alias.shutdown(1))?;
        context.expect_eq(operation(context, second.read(&mut [0u8; 1]))?, 0);
        operation(context, second.shutdown(1))?;
        context.expect_eq(operation(context, alias.read(&mut [0u8; 1]))?, 0);
        drop(alias);
        context.expect_eq(file_weak.upgrade().is_none(), true);
        context.expect_eq(watch_weak.upgrade().is_none(), true);
        context.expect_eq(
            operation(context, socket_epoll_collect(&epoll, &mut events))?,
            0,
        );
        drop((second, epoll));
        socket_retired(&fixture.net)?;
        let [first, second] = operation(
            context,
            fixture.net.try_new_unix_pair(UnixSocketType::Stream),
        )?;
        let first_weak = Arc::downgrade(&first);
        let second_weak = Arc::downgrade(&second);
        context.expect_eq(first.id() != second.id(), true);
        context.expect_eq(fixture.net.ktest_live_endpoints(), 2);
        context.expect_eq(
            operation(context, first.send(b"endpoint bytes"))?,
            IoProgress::Ready(14),
        );
        context.expect_eq(
            operation(
                context,
                second.receive(&mut ReadBuffer::kernel(&mut output), false),
            )?,
            IoProgress::Ready(14),
        );
        context.expect_eq(&output[..14], b"endpoint bytes");
        first.close();
        second.close();
        drop((first, second));
        context.expect_eq(first_weak.upgrade().is_none(), true);
        context.expect_eq(second_weak.upgrade().is_none(), true);
        socket_retired(&fixture.net)?;
        // 只剩一份真实端点额度时，pair 的第二份准备失败必须归还第一份未发表额度。
        let mut occupied = Vec::new();
        occupied
            .try_reserve_exact(127)
            .map_err(|_| TestError("UNIX slot observation allocation failed"))?;
        for _ in 0..127 {
            occupied.push(operation(
                context,
                fixture.net.try_new_udp(AddressFamily::Ipv4),
            )?);
        }
        match fixture.net.try_new_unix_pair(UnixSocketType::Stream) {
            Err(FsError::NoMemory) => {}
            _ => return Err(TestError("UNIX pair exceeded two actual endpoint slots")),
        }
        context.expect_eq(fixture.net.ktest_live_endpoints(), 127);
        let last = operation(context, fixture.net.try_new_udp(AddressFamily::Ipv4))?;
        context.expect_eq(fixture.net.ktest_live_endpoints(), 128);
        last.close();
        for endpoint in &occupied {
            endpoint.close();
        }
        drop((last, occupied));
        socket_retired(&fixture.net)?;
        let [first, second] = operation(
            context,
            fixture.net.try_new_unix_pair(UnixSocketType::Stream),
        )?;
        context.expect_eq(fixture.net.ktest_live_endpoints(), 2);
        first.close();
        second.close();
        drop((first, second));
        socket_retired(&fixture.net)?;
        context.expect_eq(fixture.net.ktest_live_endpoints(), 0);
        context.expect_eq(fixture.net.is_stopping(), false);
        drop((file_weak, watch_weak, first_weak, second_weak));
        Ok(())
    }

    /// AF_UNSPEC 保已接纳消息的旧租约，旧消息交付后归端口且后继绑定仍保持唯一。
    #[test]
    #[ktest(min_cpus = 2)]
    fn udp_prepared_lease_survives_disconnect_without_unbinding_successor(
        context: &mut TestContext<'_>,
        fixture: &mut StackOwner,
    ) -> TestResult {
        let net = Arc::clone(&fixture.net);
        let release = Arc::clone(&fixture.release);
        let controller = crate::ktest::spawn_result_on(1, move |_| {
            let receiver = child_operation(net.try_new_udp(AddressFamily::Ipv4))?;
            let destination = child_operation(receiver.bind(InetEndpoint {
                port: 45670,
                ..SERVER
            }))?;
            let sender = child_operation(net.try_new_udp(AddressFamily::Ipv4))?;
            child_operation(sender.connect(Some(destination)))?;
            let entered = Arc::try_new(AtomicBool::new(false))
                .map_err(|_| TestError("UDP gate observation OOM"))?;
            let held = Arc::clone(&entered);
            let gate_release = Arc::clone(&release);
            let gate = crate::ktest::spawn_result_on(0, move |token| {
                let irq = crate::sync::IrqSaveGuard::new();
                held.store(true, Ordering::Release);
                let end = time::now()
                    .checked_add(time::Duration::from_secs(5))
                    .unwrap();
                while !gate_release.load(Ordering::Acquire) && !token.stop_requested() {
                    if end <= time::now() {
                        return Err(TestError("UDP lease IRQ gate release deadline"));
                    }
                    ::core::hint::spin_loop();
                }
                drop(irq);
                Ok(0)
            })?;
            observe(
                || entered.load(Ordering::Acquire),
                "UDP Work CPU did not enter actual IRQ gate",
            )?;
            let old = child_operation(sender.local_address())?;
            let old_input = b"old prepared message";
            child_operation(sender.set_send_budget(old_input.len()))?;
            if child_operation(sender.send(old_input, None))? != IoProgress::Ready(old_input.len())
            {
                return Err(TestError("UDP old record not accepted"));
            }
            if child_operation(sender.poll(None))?.contains(PollMask::POLLOUT) {
                return Err(TestError(
                    "UDP accepted bytes did not exhaust real TX budget",
                ));
            }
            child_operation(sender.set_send_budget(64))?;
            if !child_operation(sender.poll(None))?.contains(PollMask::POLLOUT) {
                return Err(TestError("UDP raised TX budget did not restore OUT"));
            }
            child_operation(sender.connect(None))?;
            if child_operation(sender.local_address())?
                != (InetEndpoint {
                    address: AddressFamily::Ipv4.unspecified(),
                    port: 0,
                })
            {
                return Err(TestError(
                    "UDP automatic disconnect retained current binding",
                ));
            }
            let contender = child_operation(net.try_new_udp(AddressFamily::Ipv4))?;
            if contender.bind(old) != Err(FsError::AddressInUse) {
                return Err(TestError(
                    "accepted old UDP record lost binding before dispatch",
                ));
            }
            let input = b"successor prepared datagram";
            if child_operation(sender.send(input, Some(destination)))?
                != IoProgress::Ready(input.len())
            {
                return Err(TestError("UDP successor record not accepted"));
            }
            let new = child_operation(sender.local_address())?;
            if old.port == new.port || sender.peer_address() != Err(FsError::NotConnected) {
                return Err(TestError(
                    "UDP successor changed disconnect or reused live old port",
                ));
            }
            let third = child_operation(net.try_new_udp(AddressFamily::Ipv4))?;
            if third.bind(new) != Err(FsError::AddressInUse) {
                return Err(TestError("UDP successor binding not retained"));
            }
            release.store(true, Ordering::Release);
            if gate.join()? != 0 {
                return Err(TestError("UDP lease IRQ gate child failed"));
            }
            for (expected, source) in [(&old_input[..], old), (&input[..], new)] {
                observe(
                    || {
                        receiver
                            .poll(None)
                            .is_ok_and(|mask| mask.contains(PollMask::POLLIN))
                    },
                    "accepted UDP record not received",
                )?;
                let mut bytes = [0u8; 64];
                let record =
                    child_operation(receiver.receive(&mut ReadBuffer::kernel(&mut bytes), false))?
                        .ok_or(TestError("accepted UDP record disappeared"))?;
                let expected_source = SocketAddress::Inet(InetEndpoint {
                    address: SERVER.address,
                    port: source.port,
                });
                if record.source != Some(expected_source)
                    || record.original_len != expected.len()
                    || &bytes[..record.copied] != expected
                {
                    return Err(TestError("UDP old or successor source and bytes differ"));
                }
            }
            let mut bound = false;
            let mut failure = None;
            observe(
                || {
                    match contender.bind(old) {
                        Ok(local) => bound = local == old,
                        Err(FsError::AddressInUse) => {}
                        Err(error) => failure = Some(error),
                    }
                    bound || failure.is_some()
                },
                "old UDP lease did not retire after actual delivery",
            )?;
            if let Some(error) = failure {
                child_operation::<()>(Err(error))?;
            }
            if third.bind(new) != Err(FsError::AddressInUse) {
                return Err(TestError("old UDP lease final put unbound successor"));
            }
            release.store(false, Ordering::Release);
            entered.store(false, Ordering::Release);
            let held = Arc::clone(&entered);
            let gate_release = Arc::clone(&release);
            let gate = crate::ktest::spawn_result_on(0, move |token| {
                let irq = crate::sync::IrqSaveGuard::new();
                held.store(true, Ordering::Release);
                let end = time::now()
                    .checked_add(time::Duration::from_secs(5))
                    .unwrap();
                while !gate_release.load(Ordering::Acquire) && !token.stop_requested() {
                    if end <= time::now() {
                        return Err(TestError("UDP close IRQ gate release deadline"));
                    }
                    ::core::hint::spin_loop();
                }
                drop(irq);
                Ok(0)
            })?;
            observe(
                || entered.load(Ordering::Acquire),
                "UDP close Work CPU did not enter actual IRQ gate",
            )?;
            let tail = b"accepted before close";
            if child_operation(sender.send(tail, Some(destination)))?
                != IoProgress::Ready(tail.len())
            {
                return Err(TestError("UDP pre-close record not accepted"));
            }
            sender.close();
            release.store(true, Ordering::Release);
            if gate.join()? != 0 {
                return Err(TestError("UDP close IRQ gate child failed"));
            }
            observe(
                || {
                    receiver
                        .poll(None)
                        .is_ok_and(|mask| mask.contains(PollMask::POLLIN))
                },
                "UDP close discarded accepted raw TX record",
            )?;
            let mut bytes = [0u8; 64];
            let record =
                child_operation(receiver.receive(&mut ReadBuffer::kernel(&mut bytes), false))?
                    .ok_or(TestError("UDP close delivery disappeared"))?;
            if record.source
                != Some(SocketAddress::Inet(InetEndpoint {
                    address: SERVER.address,
                    port: new.port,
                }))
                || record.original_len != tail.len()
                || &bytes[..record.copied] != tail
            {
                return Err(TestError("UDP close delivery source or full bytes differ"));
            }
            receiver.close();
            contender.close();
            third.close();
            drop((sender, receiver, contender, third));
            socket_retired(&net)?;
            Ok(0)
        })?;
        context.expect_eq(controller.join()?, 0);
        context.expect_eq(fixture.net.ktest_live_endpoints(), 0);
        context.expect_eq(fixture.net.is_stopping(), false);
        Ok(())
    }

    /// 真实发送准备 OOM 后结束末 SocketFile，放压不再提交工作，原 reaper 须归还节点和绑定。
    #[test]
    #[ktest(min_cpus = 2)]
    fn udp_real_no_memory_last_ofd_retires_without_new_work(
        context: &mut TestContext<'_>,
        fixture: &mut StackOwner,
    ) -> TestResult {
        let net = Arc::clone(&fixture.net);
        let release = Arc::clone(&fixture.release);

        let controller = crate::ktest::spawn_result_on(1, move |_| {
            let sender = child_operation(SocketFile::try_new_udp(
                net.clone(),
                AddressFamily::Ipv4,
                0,
                0,
                0,
            ))?;
            child_operation(sender.connect(SocketAddress::Inet(SERVER), FileIoContext::Kernel))?;
            let endpoint = net
                .tables()
                .udp_by_id(sender.id())
                .ok_or(TestError("UDP OOM file endpoint is not published"))?;
            let local = child_operation(endpoint.local_address())?;
            let id = sender.id();
            let file_weak = Arc::downgrade(&sender);
            let endpoint_weak = Arc::downgrade(&endpoint);
            let payload = [0x97u8; 8193];
            child_operation(endpoint.set_send_budget(payload.len()))?;

            let mut pressure = crate::cache::ktest::HeapPressure::prepare()?;
            let entered = Arc::try_new(AtomicBool::new(false))
                .map_err(|_| TestError("UDP OOM gate observation allocation failed"))?;
            let finished = Arc::try_new(AtomicBool::new(false))
                .map_err(|_| TestError("UDP OOM gate lifetime allocation failed"))?;

            let prepared_gate = prepare_udp_oom_gate(
                Arc::clone(&entered),
                Arc::clone(&release),
                Arc::clone(&finished),
            )?;

            net.work_queue().flush();

            // exhaust 包含同步 SLUB IPI；停点尚未发表，预备任务栈已占有，CPU0 仍可服务 IPI。

            let evidence = pressure.exhaust();

            crate::ktest::report::diagnostic(format_args!(
                "UDP OOM case=last-ofd evidence={:?}",
                evidence
            ));
            if evidence != (true, true) {
                return Err(TestError("UDP OOM did not exhaust real pages and heap"));
            }

            let gate = prepared_gate.publish()?;

            let result = (|| -> TestResult {
                observe(
                    || entered.load(Ordering::Acquire),
                    "UDP OOM Work CPU did not enter IRQ gate",
                )?;

                // OFD 保有真实端点；直接提交预备字节，避免在压力中另做文件层 copyin 分配。

                if child_operation(endpoint.send(&payload, None))?
                    != IoProgress::Ready(payload.len())
                {
                    return Err(TestError("UDP OOM message was not actually accepted"));
                }

                if child_operation(endpoint.poll(None))?.contains(PollMask::POLLOUT) {
                    return Err(TestError(
                        "UDP OOM accepted message did not consume TX budget",
                    ));
                }
                release.store(true, Ordering::Release);

                net.work_queue().flush();

                if !child_operation(endpoint.poll(None))?.contains(PollMask::POLLERR) {
                    return Err(TestError("UDP OOM dispatch did not report POLLERR"));
                }
                let error = sender.take_error();
                crate::ktest::report::diagnostic(format_args!(
                    "UDP OOM case=last-ofd dispatch-error={:?}",
                    error
                ));
                if error != Some(FsError::NoMemory) {
                    return Err(TestError("UDP dispatch did not fail with real NoMemory"));
                }
                // take_error 的 Modify guard 也安排原 Work；这份票必须仍在压力下结束。

                net.work_queue().flush();

                drop(endpoint);
                drop(sender);

                net.work_queue().flush();

                // close 回调中提交的 reaper 属于后继颜色，再等一次覆盖其真实停止尾部。

                net.work_queue().flush();

                if file_weak.upgrade().is_some() || gate.try_join().is_some() {
                    return Err(TestError(
                        "UDP OOM OFD or gate lifetime did not end as required",
                    ));
                }
                Ok(())
            })();

            drop(pressure);

            release.store(true, Ordering::Release);
            finished.store(true, Ordering::Release);

            let joined = gate.join();

            result?;
            if joined? != 0 {
                return Err(TestError("UDP OOM IRQ gate child failed"));
            }
            // 放压后只观察末端点，不升级它调用 Modify，也不发送或手动安排工作。

            observe(
                || endpoint_weak.upgrade().is_none() && net.ktest_live_endpoints() == 0,
                "UDP terminal NoMemory stranded the last OFD endpoint or slot",
            )?;

            let binding = child_operation(net.tables().reserve_binding(
                &net,
                id,
                crate::net::table::TransportProtocol::Udp,
                local,
                false,
                false,
            ))?;
            if binding.local() != local || net.is_stopping() {
                return Err(TestError(
                    "UDP terminal NoMemory did not return the live-root binding",
                ));
            }
            drop((binding, file_weak, endpoint_weak));
            Ok(0)
        })?;

        context.expect_eq(controller.join()?, 0);

        context.expect_eq(fixture.net.ktest_live_endpoints(), 0);
        context.expect_eq(fixture.net.is_stopping(), false);
        Ok(())
    }

    /// OOM 结束队首大消息，超过原批预算的零长后继保同条 source，放压后正常全文不夹带失败消息。
    #[test]
    #[ktest(min_cpus = 2)]
    fn udp_real_no_memory_preserves_successor_records_sources_and_leases(
        context: &mut TestContext<'_>,
        fixture: &mut StackOwner,
    ) -> TestResult {
        let net = Arc::clone(&fixture.net);
        let release = Arc::clone(&fixture.release);

        let controller = crate::ktest::spawn_result_on(1, move |_| {
            let receiver = child_operation(SocketFile::try_new_udp(
                net.clone(),
                AddressFamily::Ipv4,
                0,
                0,
                0,
            ))?;
            let destination = InetEndpoint {
                port: 45682,
                ..SERVER
            };
            child_operation(receiver.bind(SocketAddress::Inet(destination)))?;
            let sender = child_operation(SocketFile::try_new_udp(
                net.clone(),
                AddressFamily::Ipv4,
                0,
                0,
                0,
            ))?;
            child_operation(
                sender.connect(SocketAddress::Inet(destination), FileIoContext::Kernel),
            )?;
            let endpoint = net
                .tables()
                .udp_by_id(sender.id())
                .ok_or(TestError("UDP successor file endpoint is not published"))?;
            let old = child_operation(endpoint.local_address())?;
            let payload = [0x69u8; 8193];

            let mut pressure = crate::cache::ktest::HeapPressure::prepare()?;
            let entered = Arc::try_new(AtomicBool::new(false))
                .map_err(|_| TestError("UDP successor gate observation allocation failed"))?;
            let finished = Arc::try_new(AtomicBool::new(false))
                .map_err(|_| TestError("UDP successor gate lifetime allocation failed"))?;

            let prepared_gate = prepare_udp_oom_gate(
                Arc::clone(&entered),
                Arc::clone(&release),
                Arc::clone(&finished),
            )?;

            net.work_queue().flush();

            let evidence = pressure.exhaust();

            crate::ktest::report::diagnostic(format_args!(
                "UDP OOM case=successor evidence={:?}",
                evidence
            ));
            if evidence != (true, true) {
                return Err(TestError(
                    "UDP successor OOM did not exhaust real pages and heap",
                ));
            }

            let gate = prepared_gate.publish()?;

            let result = (|| -> TestResult {
                observe(
                    || entered.load(Ordering::Acquire),
                    "UDP successor Work CPU did not enter IRQ gate",
                )?;

                if child_operation(endpoint.send(&payload, None))?
                    != IoProgress::Ready(payload.len())
                {
                    return Err(TestError("UDP successor OOM message was not accepted"));
                }

                // 原 metadata 容量为32；大消息之后31条零长记录跨过16条批预算，不需新 payload 后备。
                for _ in 0..31 {
                    if child_operation(endpoint.send(&[], None))? != IoProgress::Ready(0) {
                        return Err(TestError("UDP zero successor was not actually accepted"));
                    }
                }

                if child_operation(endpoint.poll(None))?.contains(PollMask::POLLOUT) {
                    return Err(TestError(
                        "UDP successor records did not consume metadata capacity",
                    ));
                }
                release.store(true, Ordering::Release);

                net.work_queue().flush();

                if !child_operation(endpoint.poll(None))?.contains(PollMask::POLLERR) {
                    return Err(TestError(
                        "UDP successor OOM dispatch did not report POLLERR",
                    ));
                }
                let error = sender.take_error();
                crate::ktest::report::diagnostic(format_args!(
                    "UDP OOM case=successor dispatch-error={:?}",
                    error
                ));
                if error != Some(FsError::NoMemory) {
                    return Err(TestError(
                        "UDP successor dispatch did not fail with real NoMemory",
                    ));
                }
                // 等查询票及原批预算后的后继票；接收逐条等待真正 IN，不把 raw 消费冒充交付。

                net.work_queue().flush();

                for _ in 0..31 {
                    observe(
                        || {
                            receiver
                                .poll(None)
                                .is_ok_and(|mask| mask.contains(PollMask::POLLIN))
                        },
                        "UDP zero successor did not reach the actual receiver",
                    )?;
                    let record = child_operation(receiver.receive(
                        &mut ReadBuffer::kernel(&mut []),
                        0x40,
                        FileIoContext::Kernel,
                    ))?;
                    if record.source != Some(SocketAddress::Inet(old))
                        || record.original_len != 0
                        || record.copied != 0
                        || record.flags != 0
                    {
                        return Err(TestError(
                            "UDP OOM successor record and source were mispaired",
                        ));
                    }
                }

                net.work_queue().flush();

                if !child_operation(endpoint.poll(None))?.contains(PollMask::POLLOUT)
                    || gate.try_join().is_some()
                {
                    return Err(TestError(
                        "UDP OOM successor budget or gate lifetime did not recover",
                    ));
                }
                Ok(())
            })();

            drop(pressure);

            release.store(true, Ordering::Release);
            finished.store(true, Ordering::Release);

            let joined = gate.join();

            result?;
            if joined? != 0 {
                return Err(TestError("UDP successor IRQ gate child failed"));
            }
            let mut bytes = [0u8; 64];
            if receiver.receive(
                &mut ReadBuffer::kernel(&mut bytes),
                0x40,
                FileIoContext::Kernel,
            ) != Err(FsError::WouldBlock)
            {
                return Err(TestError(
                    "UDP failed OOM message was delivered after pressure release",
                ));
            }
            {
                let input = b"normal after OOM";
                let sent = child_operation(sender.send(
                    &mut WriteBuffer::kernel(input),
                    None,
                    0x40,
                    FileIoContext::Kernel,
                ))?;
                if sent != input.len() {
                    return Err(TestError("UDP post-OOM full message was not accepted"));
                }
                observe(
                    || {
                        receiver
                            .poll(None)
                            .is_ok_and(|mask| mask.contains(PollMask::POLLIN))
                    },
                    "UDP post-OOM full message was not actually delivered",
                )?;
                let record = child_operation(receiver.receive(
                    &mut ReadBuffer::kernel(&mut bytes),
                    0x40,
                    FileIoContext::Kernel,
                ))?;
                if record.source != Some(SocketAddress::Inet(old))
                    || record.original_len != input.len()
                    || record.copied != input.len()
                    || &bytes[..record.copied] != input
                {
                    return Err(TestError(
                        "UDP post-OOM full bytes or source were mispaired",
                    ));
                }
            }
            child_operation(sender.connect(SocketAddress::Unspecified, FileIoContext::Kernel))?;
            let input = b"new lease after OOM";
            if child_operation(sender.send(
                &mut WriteBuffer::kernel(input),
                Some(SocketAddress::Inet(destination)),
                0x40,
                FileIoContext::Kernel,
            ))? != input.len()
            {
                return Err(TestError("UDP post-OOM new lease message was not accepted"));
            }
            let new = child_operation(endpoint.local_address())?;
            if old.port == new.port {
                return Err(TestError(
                    "UDP post-OOM automatic binding reused the old source",
                ));
            }
            observe(
                || {
                    receiver
                        .poll(None)
                        .is_ok_and(|mask| mask.contains(PollMask::POLLIN))
                },
                "UDP post-OOM new lease message was not actually delivered",
            )?;
            let record = child_operation(receiver.receive(
                &mut ReadBuffer::kernel(&mut bytes),
                0x40,
                FileIoContext::Kernel,
            ))?;
            // 未连接 sendto 保通配绑定名；报文源来自本次 lo 路由，端口仍属于同一新租约。
            let expected_source = SocketAddress::Inet(InetEndpoint {
                address: SERVER.address,
                port: new.port,
            });
            if record.source != Some(expected_source)
                || record.original_len != input.len()
                || record.copied != input.len()
                || &bytes[..record.copied] != input
            {
                crate::ktest::report::diagnostic(format_args!(
                    "UDP OOM case=successor new-lease record={:?} expected-source={:?} bytes={:?} expected-bytes={:?}",
                    record, expected_source, &bytes[..record.copied], input,
                ));
                return Err(TestError(
                    "UDP post-OOM new lease bytes or source were mispaired",
                ));
            }
            net.work_queue().flush();
            let binding = child_operation(net.tables().reserve_binding(
                &net,
                endpoint.id(),
                crate::net::table::TransportProtocol::Udp,
                old,
                false,
                false,
            ))?;
            match net.tables().reserve_binding(
                &net,
                endpoint.id(),
                crate::net::table::TransportProtocol::Udp,
                new,
                false,
                false,
            ) {
                Err(FsError::AddressInUse) => {}
                _ => {
                    return Err(TestError(
                        "UDP failed lease final put unbound the successor",
                    ))
                }
            }
            drop((binding, endpoint, sender, receiver));
            socket_retired(&net)?;
            if net.is_stopping() {
                return Err(TestError(
                    "UDP OOM successor required stopping the network root",
                ));
            }
            Ok(0)
        })?;

        context.expect_eq(controller.join()?, 0);

        context.expect_eq(fixture.net.ktest_live_endpoints(), 0);
        context.expect_eq(fixture.net.is_stopping(), false);
        Ok(())
    }
}
