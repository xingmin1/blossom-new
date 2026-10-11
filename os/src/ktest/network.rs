//! 原生协议消费者分别使用本例同步 wire 与生产后台网络域，输入和复制故障使用实际内核对象。

use ::core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use ::ktest::{Fixture, TestContext, TestError, TestResult};
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec::Vec;
use smoltcp::socket::tcp::State;

use crate::fs::epoll::{EpollEvent, EpollFile, EPOLLET, EPOLL_CTL_ADD};
use crate::fs::{
    FileIoContext, FileLike, FileRef, FsError, PollMask, Pollable, ReadBuffer, WriteBuffer,
};
use crate::ktest::execution_services::{baseline, Baseline, SuiteBaseline};
use crate::mm::space::Mm;
use crate::mm::user_table::PagePermissions;
use crate::net::address::{InetEndpoint, SocketAddress};
use crate::net::core::{ProtocolDeadline, TcpCore};
use crate::net::packet::OwnedPacket;
use crate::net::socket::SocketFile;
use crate::net::table::TcpTuple;
use crate::net::tcp::TcpEndpoint;
use crate::net::udp::UdpEndpoint;
use crate::net::{
    IoProgress, NetStack, NetStackOwner, NetStackRef, LOOPBACK_CAPACITY, LOOPBACK_MTU,
};
use crate::sync::{SleepMutex, SpinLock};
use crate::{cpu, time, timer};

const CLIENT: InetEndpoint = InetEndpoint {
    address: [127, 0, 0, 1],
    port: 35701,
};
const SERVER: InetEndpoint = InetEndpoint {
    address: [127, 0, 0, 1],
    port: 44702,
};
const ADDRESS: usize = 0x4600_0000;
const RW: PagePermissions = PagePermissions {
    read: true,
    write: true,
    execute: false,
};

struct NetworkOwner {
    before: Baseline,
    wire: SpinLock<VecDeque<OwnedPacket>>,
    client: SleepMutex<TcpCore>,
    server: SleepMutex<TcpCore>,
}

impl Fixture<SuiteBaseline> for NetworkOwner {
    /// 两核心、队列和随机环境均在本例基线之后准备，设置失败由局部拥有释放。
    fn set_up(context: &mut TestContext<'_>, _: &SuiteBaseline) -> Result<Self, TestError> {
        let before = baseline();
        let mut wire = VecDeque::new();
        wire.try_reserve_exact(LOOPBACK_CAPACITY)
            .map_err(|_| TestError("network core wire allocation failed"))?;
        let client = SleepMutex::new(operation(
            context,
            TcpCore::try_new(256, 256, LOOPBACK_MTU, 1),
        )?);
        let server = SleepMutex::new(operation(
            context,
            TcpCore::try_new(256, 256, LOOPBACK_MTU, 2),
        )?);
        operation(context, server.lock().listen(SERVER))?;
        operation(context, client.lock().connect(CLIENT, SERVER, time::now()))?;
        Ok(Self {
            before,
            wire: SpinLock::new(wire),
            client,
            server,
        })
    }

    /// 无后台执行者的核心测试在原 owning 字段结束后核完整六域和任务元数据。
    fn tear_down(self, context: &mut TestContext<'_>) -> TestResult {
        let before = self.before;
        drop(self);
        let after = baseline();
        context.diagnostic(format_args!(
            "network-core resources-before={:?} resources-after={:?}",
            before, after
        ));
        context.expect_eq(after.0, before.0);
        context.expect_eq(after.1, before.1);
        if after != before {
            return Err(TestError("network core resources did not converge"));
        }
        Ok(())
    }
}

impl NetworkOwner {
    /// 本例 wire 只移动真实包拥有；满时向成熟 emitter 返回原容量错误。
    fn enqueue(&self, packet: OwnedPacket) -> Result<(), FsError> {
        let mut wire = self.wire.lock();
        if LOOPBACK_CAPACITY <= wire.len() {
            return Err(FsError::WouldBlock);
        }
        wire.push_back(packet);
        Ok(())
    }

    /// 锁外处理包与复制，wire 不持生产网络域或其它核心处理权。
    fn take_packet(&self) -> Option<OwnedPacket> {
        self.wire.lock().pop_front()
    }

    /// 两端编码真实包，领取后以实际输入构造重新核线性字节，再锁外选目的端处理。
    fn pump(&self, context: &TestContext<'_>) -> TestResult {
        let now = time::now();
        for endpoint in [&self.client, &self.server] {
            operation(
                context,
                endpoint.lock().dispatch(now, |packet| self.enqueue(packet)),
            )?;
        }
        while let Some(packet) = self.take_packet() {
            let packet = operation(context, OwnedPacket::try_from_ipv4(packet.bytes()))?;
            let (_, destination) = operation(context, packet.tcp_endpoints())?;
            let endpoint = if destination == SERVER {
                &self.server
            } else if destination == CLIENT {
                &self.client
            } else {
                return Err(TestError("unexpected native TCP destination"));
            };
            let reply = operation(context, endpoint.lock().process(&packet, now))?;
            if let Some(reply) = reply {
                operation(context, self.enqueue(reply))?;
            }
        }
        Ok(())
    }

    /// 在原 monotonic deadline 内同步驱动两端，轮次之间允许原 timer 服务和调度执行。
    fn until(&self, context: &TestContext<'_>, condition: impl Fn() -> bool) -> TestResult {
        let end = time::now()
            .checked_add(time::Duration::from_secs(5))
            .ok_or(TestError("network core observation deadline overflow"))?;
        while !condition() {
            self.pump(context)?;
            if end <= time::now() {
                return Err(TestError("native TCP core did not progress"));
            }
            timer::sleep(time::Duration::from_millis(1));
        }
        Ok(())
    }

    /// 成功要求两个成熟算法核心都完成握手，不以首个 SYN 或已排队包代替连接。
    fn connect(&self, context: &TestContext<'_>) -> TestResult {
        self.until(context, || {
            self.client.lock().state() == State::Established
                && self.server.lock().state() == State::Established
        })
    }
}

/// 保留具体失败原因供 guest 原诊断消费，不将错误改成零进度或通过。
fn operation<T>(context: &TestContext<'_>, result: Result<T, FsError>) -> Result<T, TestError> {
    result.map_err(|error| {
        context.diagnostic(format_args!("network operation-error={:?}", error));
        TestError("network operation failed")
    })
}

/// 输入按外部逻辑位置变化，环回绕或复制错误不能满足全文比较。
fn pattern(position: usize) -> u8 {
    (position as u8)
        .wrapping_mul(37)
        .wrapping_add((position / 251) as u8)
}

/// 保留两个具名套件的真实宏锚点，不增加入口或中央分派器。
pub(super) fn anchors() -> [&'static ::ktest::SuiteSlot; 2] {
    [core::__ktest_anchor(), stack::__ktest_anchor()]
}

#[ktest::suite(fixture = NetworkOwner, suite_resources = SuiteBaseline, context = "Thread", tags("network-core"), timeout_ms = 30000)]
pub(crate) mod core {
    use super::*;

    /// 超过 ring 容量的全文经过真正 TCP 包，PEEK 不消费，FIN 后先交付尾部再 EOF。
    #[test]
    fn owned_packets_stream_wrap_peek_and_fin(
        context: &mut TestContext<'_>,
        fixture: &mut NetworkOwner,
    ) -> TestResult {
        fixture.connect(context)?;
        let input: [u8; 1025] = ::core::array::from_fn(pattern);
        let mut output = [0u8; 1025];
        let mut sent = 0;
        let mut received = 0;
        let end = time::now()
            .checked_add(time::Duration::from_secs(5))
            .unwrap();
        while sent < input.len() || received < input.len() {
            if sent < input.len() {
                let mut source = WriteBuffer::kernel(&input[sent..]);
                if let IoProgress::Ready(count) =
                    operation(context, fixture.client.lock().send(&mut source))?
                {
                    sent += count;
                }
            }
            fixture.pump(context)?;
            if 0 < fixture.server.lock().receive_queue() {
                let mut peek = [0u8; 31];
                let observed = operation(
                    context,
                    fixture
                        .server
                        .lock()
                        .receive(&mut ReadBuffer::kernel(&mut peek), true),
                )?;
                let IoProgress::Ready(count) = observed else {
                    return Err(TestError("queued TCP bytes did not peek"));
                };
                context.expect_eq(&peek[..count], &input[received..received + count]);
                let before = fixture.server.lock().receive_queue();
                let mut target = ReadBuffer::kernel(&mut output[received..]);
                let IoProgress::Ready(count) =
                    operation(context, fixture.server.lock().receive(&mut target, false))?
                else {
                    return Err(TestError("queued TCP bytes did not read"));
                };
                received += count;
                context.expect_eq(fixture.server.lock().receive_queue(), before - count);
            }
            if end <= time::now() {
                return Err(TestError("native TCP full stream deadline"));
            }
            timer::sleep(time::Duration::from_millis(1));
        }
        context.expect_eq(output, input);
        let tail = b"final tcp tail";
        let mut source = WriteBuffer::kernel(tail);
        context.expect_eq(
            operation(context, fixture.client.lock().send(&mut source))?,
            IoProgress::Ready(tail.len()),
        );
        fixture.client.lock().close();
        fixture.until(context, || {
            fixture.server.lock().state() == State::CloseWait
        })?;
        let mut bytes = [0u8; 32];
        context.expect_eq(
            operation(
                context,
                fixture
                    .server
                    .lock()
                    .receive(&mut ReadBuffer::kernel(&mut bytes), false),
            )?,
            IoProgress::Ready(tail.len()),
        );
        context.expect_eq(&bytes[..tail.len()], tail);
        context.expect_eq(
            operation(
                context,
                fixture
                    .server
                    .lock()
                    .receive(&mut ReadBuffer::kernel(&mut bytes), false),
            )?,
            IoProgress::Eof,
        );
        Ok(())
    }

    /// 真队列满使 SYN 未提交，领取容量后同一核心仍发出可完成握手的原始 SYN。
    #[test]
    fn full_owned_queue_preserves_emit_commit(
        context: &mut TestContext<'_>,
        fixture: &mut NetworkOwner,
    ) -> TestResult {
        for index in 0..LOOPBACK_CAPACITY {
            let mut endpoint = operation(
                context,
                TcpCore::try_new(64, 64, LOOPBACK_MTU, 3 + index as u64),
            )?;
            let local = InetEndpoint {
                address: CLIENT.address,
                port: 36000 + index as u16,
            };
            operation(context, endpoint.connect(local, SERVER, time::now()))?;
            operation(
                context,
                endpoint.dispatch(time::now(), |packet| fixture.enqueue(packet)),
            )?;
        }
        let mut attempted = None;
        let failure = fixture.client.lock().dispatch(time::now(), |packet| {
            attempted = Some(packet.tcp()?.1.seq_number);
            fixture.enqueue(packet)
        });
        context.expect_eq(failure, Err(FsError::WouldBlock));
        context.expect_eq(
            fixture.client.lock().deadline(time::now()),
            ProtocolDeadline::Now,
        );
        for _ in 0..LOOPBACK_CAPACITY {
            fixture
                .take_packet()
                .ok_or(TestError("full queue lost an accepted packet"))?;
        }
        let mut submitted = None;
        operation(
            context,
            fixture.client.lock().dispatch(time::now(), |packet| {
                submitted = Some(packet.tcp()?.1.seq_number);
                fixture.enqueue(packet)
            }),
        )?;
        context.expect_eq(submitted, attempted);
        let now = time::now();
        match fixture.client.lock().deadline(now) {
            ProtocolDeadline::At(at) if now < at => {}
            _ => return Err(TestError("accepted SYN has no future protocol deadline")),
        }
        fixture.connect(context)?;
        context.expect_eq(fixture.client.lock().local_endpoint(), Some(CLIENT));
        context.expect_eq(fixture.server.lock().remote_endpoint(), Some(CLIENT));
        Ok(())
    }

    /// 用户 copyout 与 copyin 真实跨页故障只提交已交付前缀，PEEK 故障不消费 ring。
    #[test]
    fn user_copy_fault_keeps_exact_stream_prefix(
        context: &mut TestContext<'_>,
        fixture: &mut NetworkOwner,
    ) -> TestResult {
        fixture.connect(context)?;
        let memory = Mm::try_new().map_err(|_| TestError("network MM allocation failed"))?;
        memory
            .map_anonymous(ADDRESS, 4096, RW, RW)
            .map_err(|_| TestError("network user page mapping failed"))?;
        let input = b"real tcp fault boundary";
        let mut source = WriteBuffer::kernel(input);
        context.expect_eq(
            operation(context, fixture.client.lock().send(&mut source))?,
            IoProgress::Ready(input.len()),
        );
        fixture.until(context, || {
            fixture.server.lock().receive_queue() == input.len()
        })?;
        let mut target = operation(
            context,
            ReadBuffer::user(&memory, ADDRESS + 4096 - 7, input.len()),
        )?;
        context.expect_eq(
            operation(context, fixture.server.lock().receive(&mut target, true))?,
            IoProgress::Ready(7),
        );
        context.expect_eq(fixture.server.lock().receive_queue(), input.len());
        let mut target = operation(
            context,
            ReadBuffer::user(&memory, ADDRESS + 4096 - 7, input.len()),
        )?;
        context.expect_eq(
            operation(context, fixture.server.lock().receive(&mut target, false))?,
            IoProgress::Ready(7),
        );
        let mut output = [0u8; 64];
        context.expect_eq(
            operation(
                context,
                fixture
                    .server
                    .lock()
                    .receive(&mut ReadBuffer::kernel(&mut output), false),
            )?,
            IoProgress::Ready(input.len() - 7),
        );
        context.expect_eq(&output[..input.len() - 7], &input[7..]);
        let progress = memory.copyout(ADDRESS + 4096 - 9, b"nine real");
        context.expect_eq(progress.copied, 9);
        context.expect_eq(progress.error, None);
        let mut source = operation(context, WriteBuffer::user(&memory, ADDRESS + 4096 - 9, 20))?;
        context.expect_eq(
            operation(context, fixture.server.lock().send(&mut source))?,
            IoProgress::Ready(9),
        );
        fixture.until(context, || fixture.client.lock().receive_queue() == 9)?;
        context.expect_eq(
            operation(
                context,
                fixture
                    .client
                    .lock()
                    .receive(&mut ReadBuffer::kernel(&mut output), false),
            )?,
            IoProgress::Ready(9),
        );
        context.expect_eq(&output[..9], b"nine real");
        Ok(())
    }
}

struct StackOwner {
    before: Baseline,
    owner: Option<NetStackOwner>,
    net: NetStackRef,
    release: Arc<AtomicBool>,
}

impl Fixture<SuiteBaseline> for StackOwner {
    /// 每例基线之后创建独立服务 owner，设置失败仍由真实 owner 在普通任务停止。
    fn set_up(context: &mut TestContext<'_>, _: &SuiteBaseline) -> Result<Self, TestError> {
        let before = baseline();
        let owner = operation(context, NetStack::try_new_lo())?;
        let net = Arc::clone(owner.stack());
        let release = Arc::try_new(AtomicBool::new(false))
            .map_err(|_| TestError("network stack release allocation failed"))?;
        Ok(Self {
            before,
            owner: Some(owner),
            net,
            release,
        })
    }

    /// 先解除本例真实 CPU 停点，框架随后合作停止并 join 原子任务。
    fn quiesce(&mut self, _: &mut TestContext<'_>) -> TestResult {
        self.release.store(true, Ordering::Release);
        Ok(())
    }

    /// 原子任务已离栈后才停止域全尾，释放所有观察拥有，再比较完整 MM 和 metadata。
    fn tear_down(mut self, context: &mut TestContext<'_>) -> TestResult {
        let before = self.before;
        drop(self.owner.take());
        drop(self);
        let after = baseline();
        context.diagnostic(format_args!(
            "network-stack resources-before={:?} resources-after={:?}",
            before, after
        ));
        context.expect_eq(after.0, before.0);
        context.expect_eq(after.1, before.1);
        if after != before {
            return Err(TestError("network stack resources did not converge"));
        }
        Ok(())
    }
}

/// 只观察后台已产生的事实，等待本身不处理包、不安排协议 Work。
fn observe(mut ready: impl FnMut() -> bool, error: &'static str) -> TestResult {
    let end = time::now()
        .checked_add(time::Duration::from_secs(5))
        .ok_or(TestError("network stack observation deadline overflow"))?;
    while !ready() {
        if end <= time::now() {
            return Err(TestError(error));
        }
        timer::sleep(time::Duration::from_millis(1));
    }
    Ok(())
}

/// 子任务保留真实错误诊断，不能把绑定或队列错误转换成成功退出。
fn child_operation<T>(result: Result<T, FsError>) -> Result<T, TestError> {
    result.map_err(|error| {
        crate::ktest::report::diagnostic(format_args!("network-stack child-error={:?}", error));
        TestError("network stack child operation failed")
    })
}

/// listener 先构造并保持原对象，两个 client 的编号轮转不受动态 child 构造影响。
fn listener(
    context: &TestContext<'_>,
    net: &NetStackRef,
    port: u16,
) -> Result<Arc<TcpEndpoint>, TestError> {
    let endpoint = operation(context, net.try_new_tcp(256, 256))?;
    operation(context, endpoint.bind(InetEndpoint { port, ..SERVER }))?;
    operation(context, endpoint.listen(4))?;
    Ok(endpoint)
}

/// 完整握手由后台进行；accept 只消费真正 ready child，失败不会伪造连接。
fn accepted(
    context: &mut TestContext<'_>,
    listener: &Arc<TcpEndpoint>,
    client: &Arc<TcpEndpoint>,
) -> Result<Arc<TcpEndpoint>, TestError> {
    observe(
        || client.state() == State::Established,
        "background loopback handshake did not finish",
    )?;
    let mut child = None;
    let mut failure = None;
    observe(
        || {
            match listener.accept() {
                Ok(value) => child = value,
                Err(error) => failure = Some(error),
            }
            child.is_some() || failure.is_some()
        },
        "background listener child did not become ready",
    )?;
    if let Some(error) = failure {
        return operation(context, Err(error));
    }
    context.expect_eq(listener.state(), State::Listen);
    let child = child.ok_or(TestError("listener ready child missing"))?;
    context.expect_eq(child.state(), State::Established);
    Ok(child)
}

#[ktest::suite(fixture = StackOwner, suite_resources = SuiteBaseline, context = "Thread", tags("network-stack"), timeout_ms = 30000)]
pub(crate) mod stack {
    use super::*;

    /// 同域跨 Task 使用原通配绑定冲突；TCP 与 UDP 的端口拥有彼此独立。
    #[test]
    fn cross_task_bind_wildcard_and_protocol_spaces(
        context: &mut TestContext<'_>,
        fixture: &mut StackOwner,
    ) -> TestResult {
        let wildcard = InetEndpoint {
            address: [0; 4],
            port: 45120,
        };
        let first = operation(context, fixture.net.try_new_tcp(128, 128))?;
        context.expect_eq(operation(context, first.bind(wildcard))?, wildcard);
        operation(context, first.listen(4))?;
        let net = Arc::clone(&fixture.net);
        let child =
            crate::ktest::spawn_result_on((cpu::current().id + 1) % cpu::count(), move |_| {
                let tcp = child_operation(net.try_new_tcp(128, 128))?;
                if tcp.bind(InetEndpoint {
                    address: SERVER.address,
                    port: wildcard.port,
                }) != Err(FsError::AddressInUse)
                {
                    return Err(TestError("cross Task wildcard bind conflict missing"));
                }
                let udp = child_operation(net.try_new_udp())?;
                if child_operation(udp.bind(wildcard))? != wildcard || udp.local() != Some(wildcard)
                {
                    return Err(TestError("UDP wildcard or protocol space changed"));
                }
                Ok(0)
            })?;
        context.expect_eq(child.join()?, 0);
        context.expect_eq(first.lock().listen_endpoint(), wildcard);
        Ok(())
    }

    /// 两个固定 CPU 的原 Task 同时申请自动端口，每个协议的全部活绑定均不重复。
    #[test]
    #[ktest(min_cpus = 2)]
    fn concurrent_port_zero_is_unique(
        context: &mut TestContext<'_>,
        fixture: &mut StackOwner,
    ) -> TestResult {
        let mut ports = Vec::new();
        ports
            .try_reserve_exact(64)
            .map_err(|_| TestError("port observation allocation failed"))?;
        let ports = Arc::try_new(SpinLock::new(ports))
            .map_err(|_| TestError("port observation owner allocation failed"))?;
        let start = Arc::try_new(AtomicBool::new(false))
            .map_err(|_| TestError("port start allocation failed"))?;
        let ready = Arc::try_new(AtomicUsize::new(0))
            .map_err(|_| TestError("port readiness allocation failed"))?;
        let mut handles = Vec::new();
        handles
            .try_reserve_exact(2)
            .map_err(|_| TestError("port child handles allocation failed"))?;
        for id in 0..2 {
            let net = Arc::clone(&fixture.net);
            let ports = Arc::clone(&ports);
            let start = Arc::clone(&start);
            let ready = Arc::clone(&ready);
            handles.push(crate::ktest::spawn_result_on(id, move |token| {
                if cpu::current().id != id {
                    return Err(TestError(
                        "automatic port Task did not run on its fixed CPU",
                    ));
                }
                ready.fetch_add(1, Ordering::Release);
                while !start.load(Ordering::Acquire) {
                    if token.stop_requested() {
                        return Ok(0);
                    }
                    timer::sleep(time::Duration::from_millis(1));
                }
                for _ in 0..16 {
                    let tcp = child_operation(net.try_new_tcp(128, 128))?;
                    let local = child_operation(tcp.bind(InetEndpoint {
                        address: [0; 4],
                        port: 0,
                    }))?;
                    if local.address != [0; 4] || local.port < 49152 {
                        return Err(TestError(
                            "automatic TCP bind changed wildcard or port range",
                        ));
                    }
                    ports.lock().push((true, local.port));
                    let udp = child_operation(net.try_new_udp())?;
                    let local = child_operation(udp.bind(InetEndpoint {
                        address: [0; 4],
                        port: 0,
                    }))?;
                    if local.address != [0; 4] || local.port < 49152 {
                        return Err(TestError("automatic bind changed wildcard or port range"));
                    }
                    ports.lock().push((false, local.port));
                }
                Ok(0)
            })?);
        }
        observe(
            || ready.load(Ordering::Acquire) == 2,
            "automatic port participants did not reach start gate",
        )?;
        start.store(true, Ordering::Release);
        for handle in handles {
            context.expect_eq(handle.join()?, 0);
        }
        let ports = ports.lock();
        context.expect_eq(ports.len(), 64);
        for (index, port) in ports.iter().enumerate() {
            if ports[..index].contains(port) {
                return Err(TestError("concurrent automatic ports repeated"));
            }
        }
        Ok(())
    }

    /// 握手、全文、环回绕、PEEK 和 FIN 尾部仅由独立后台包处理推进。
    #[test]
    fn background_handshake_full_stream_peek_and_fin(
        context: &mut TestContext<'_>,
        fixture: &mut StackOwner,
    ) -> TestResult {
        let listener = listener(context, &fixture.net, SERVER.port)?;
        let client = operation(context, fixture.net.try_new_tcp(256, 256))?;
        operation(context, client.connect(SERVER))?;
        let server = accepted(context, &listener, &client)?;
        let input: [u8; 1025] = ::core::array::from_fn(pattern);
        let mut output = [0u8; 1025];
        let mut sent = 0;
        let mut received = 0;
        let end = time::now()
            .checked_add(time::Duration::from_secs(5))
            .unwrap();
        while sent < input.len() || received < input.len() {
            if sent < input.len() {
                if let IoProgress::Ready(count) = operation(
                    context,
                    client.lock().send(&mut WriteBuffer::kernel(&input[sent..])),
                )? {
                    sent += count;
                }
            }
            let mut core = server.lock();
            if 0 < core.receive_queue() {
                let before = core.receive_queue();
                let mut peek = [0u8; 31];
                let IoProgress::Ready(count) = operation(
                    context,
                    core.receive(&mut ReadBuffer::kernel(&mut peek), true),
                )?
                else {
                    return Err(TestError("background TCP peek lost queued data"));
                };
                context.expect_eq(&peek[..count], &input[received..received + count]);
                context.expect_eq(core.receive_queue(), before);
                let IoProgress::Ready(count) = operation(
                    context,
                    core.receive(&mut ReadBuffer::kernel(&mut output[received..]), false),
                )?
                else {
                    return Err(TestError("background TCP read lost queued data"));
                };
                received += count;
            }
            drop(core);
            if end <= time::now() {
                return Err(TestError("background TCP full stream deadline"));
            }
            timer::sleep(time::Duration::from_millis(1));
        }
        context.expect_eq(output, input);
        let tail = b"background final tail";
        context.expect_eq(
            operation(context, client.lock().send(&mut WriteBuffer::kernel(tail)))?,
            IoProgress::Ready(tail.len()),
        );
        client.close();
        observe(
            || server.state() == State::CloseWait,
            "background FIN not received",
        )?;
        let mut bytes = [0u8; 32];
        context.expect_eq(
            operation(
                context,
                server
                    .lock()
                    .receive(&mut ReadBuffer::kernel(&mut bytes), false),
            )?,
            IoProgress::Ready(tail.len()),
        );
        context.expect_eq(&bytes[..tail.len()], tail);
        context.expect_eq(
            operation(
                context,
                server
                    .lock()
                    .receive(&mut ReadBuffer::kernel(&mut bytes), false),
            )?,
            IoProgress::Eof,
        );
        Ok(())
    }

    /// A 真持操作 guard 时，另一个 CPU 的 B 原 Task 和后台连接仍完成全文传输。
    #[test]
    #[ktest(min_cpus = 2)]
    fn independent_connection_advances_while_other_is_owned(
        context: &mut TestContext<'_>,
        fixture: &mut StackOwner,
    ) -> TestResult {
        let listener_a = listener(context, &fixture.net, 45210)?;
        let listener_b = listener(context, &fixture.net, 45211)?;
        let client_a = operation(context, fixture.net.try_new_tcp(256, 256))?;
        let client_b = operation(context, fixture.net.try_new_tcp(256, 256))?;
        if client_a.work_cpu() == client_b.work_cpu() {
            return Err(TestError("independent clients share protocol CPU"));
        }
        operation(
            context,
            client_a.connect(InetEndpoint {
                port: 45210,
                ..SERVER
            }),
        )?;
        operation(
            context,
            client_b.connect(InetEndpoint {
                port: 45211,
                ..SERVER
            }),
        )?;
        let server_a = accepted(context, &listener_a, &client_a)?;
        let server_b = accepted(context, &listener_b, &client_b)?;
        let held = Arc::try_new(AtomicBool::new(false))
            .map_err(|_| TestError("operation ownership observation allocation failed"))?;
        let held_by_a = Arc::clone(&held);
        let release = Arc::clone(&fixture.release);
        let a = Arc::clone(&client_a);
        let a_cpu = a.work_cpu();
        let holder = crate::ktest::spawn_result_on(a_cpu, move |token| {
            if cpu::current().id != a_cpu {
                return Err(TestError("A operation owner ran on the wrong CPU"));
            }
            let owned = a.lock();
            held_by_a.store(true, Ordering::Release);
            let end = time::now()
                .checked_add(time::Duration::from_secs(5))
                .unwrap();
            while !release.load(Ordering::Acquire) && !token.stop_requested() {
                if end <= time::now() {
                    return Err(TestError("A ownership release deadline"));
                }
                timer::sleep(time::Duration::from_millis(1));
            }
            if owned.state() != State::Established {
                return Err(TestError("owned A protocol state changed"));
            }
            Ok(0)
        })?;
        observe(
            || held.load(Ordering::Acquire),
            "A did not acquire actual operation guard",
        )?;
        let b = Arc::clone(&client_b);
        let target_cpu = b.work_cpu();
        let child = crate::ktest::spawn_result_on(target_cpu, move |_| {
            if cpu::current().id != target_cpu {
                return Err(TestError(
                    "independent client Task did not run on its fixed CPU",
                ));
            }
            let input: [u8; 97] = ::core::array::from_fn(pattern);
            if child_operation(b.lock().send(&mut WriteBuffer::kernel(&input)))?
                != IoProgress::Ready(input.len())
            {
                return Err(TestError("independent B send prefix incomplete"));
            }
            Ok(0)
        })?;
        context.expect_eq(child.join()?, 0);
        observe(
            || server_b.lock().receive_queue() == 97,
            "B stalled while A operation was owned",
        )?;
        let mut output = [0u8; 97];
        context.expect_eq(
            operation(
                context,
                server_b
                    .lock()
                    .receive(&mut ReadBuffer::kernel(&mut output), false),
            )?,
            IoProgress::Ready(97),
        );
        context.expect_eq(output, ::core::array::from_fn(pattern));
        context.expect_eq(server_a.state(), State::Established);
        fixture.release.store(true, Ordering::Release);
        context.expect_eq(holder.join()?, 0);
        Ok(())
    }

    /// UDP 查找经真实 lo ingress 交付同记录的源、长度和数据，空消息仍可被领取。
    #[test]
    fn udp_index_delivers_owned_and_zero_length_records(
        context: &mut TestContext<'_>,
        fixture: &mut StackOwner,
    ) -> TestResult {
        let udp = operation(context, fixture.net.try_new_udp())?;
        let destination = operation(
            context,
            udp.bind(InetEndpoint {
                address: [0; 4],
                port: 45310,
            }),
        )?;
        context.expect_eq(destination.address, [0; 4]);
        for payload in [&b"real UDP record"[..], &b""[..]] {
            let packet = operation(
                context,
                OwnedPacket::try_from_udp(
                    CLIENT,
                    InetEndpoint {
                        address: SERVER.address,
                        port: destination.port,
                    },
                    payload,
                    LOOPBACK_MTU,
                ),
            )?;
            operation(context, fixture.net.send_ipv4(packet))?;
            let mut record = None;
            observe(
                || {
                    record = udp.take_datagram();
                    record.is_some()
                },
                "UDP ingress record missing",
            )?;
            let record = record.ok_or(TestError("UDP observed record disappeared"))?;
            context.expect_eq(operation(context, record.udp_endpoints())?.0, CLIENT);
            context.expect_eq(operation(context, record.udp_payload())?, payload);
            context.expect_eq(
                operation(context, record.udp_payload())?.len(),
                payload.len(),
            );
        }
        context.expect_eq(udp.take_datagram().is_none(), true);
        udp.close();
        context.expect_eq(
            fixture
                .net
                .tables()
                .lookup_udp(
                    InetEndpoint {
                        port: destination.port,
                        ..SERVER
                    },
                    CLIENT,
                )
                .is_none(),
            true,
        );
        Ok(())
    }

    /// 同一个 inbox 的 32 个完整 MTU 记录占满真实字节预算，领取后恢复；空记录同样满。
    #[test]
    fn udp_inbox_record_and_payload_capacity_recovers(
        context: &mut TestContext<'_>,
        fixture: &mut StackOwner,
    ) -> TestResult {
        let udp = operation(context, fixture.net.try_new_udp())?;
        let destination = operation(
            context,
            udp.bind(InetEndpoint {
                port: 45311,
                ..SERVER
            }),
        )?;
        let payload: [u8; LOOPBACK_MTU - 28] = ::core::array::from_fn(pattern);
        for input in [&payload[..], &b""[..]] {
            for _ in 0..32 {
                let packet = operation(
                    context,
                    OwnedPacket::try_from_udp(CLIENT, destination, input, LOOPBACK_MTU),
                )?;
                operation(context, udp.enqueue(packet))?;
            }
            let packet = operation(
                context,
                OwnedPacket::try_from_udp(CLIENT, destination, input, LOOPBACK_MTU),
            )?;
            context.expect_eq(udp.enqueue(packet), Err(FsError::WouldBlock));
            let record = udp
                .take_datagram()
                .ok_or(TestError("full UDP inbox lost record"))?;
            context.expect_eq(operation(context, record.udp_payload())?, input);
            context.expect_eq(operation(context, record.udp_endpoints())?.0, CLIENT);
            drop(record);
            let packet = operation(
                context,
                OwnedPacket::try_from_udp(CLIENT, destination, input, LOOPBACK_MTU),
            )?;
            operation(context, udp.enqueue(packet))?;
            for _ in 0..32 {
                let record = udp
                    .take_datagram()
                    .ok_or(TestError("UDP inbox recovery lost record"))?;
                context.expect_eq(operation(context, record.udp_payload())?, input);
            }
            context.expect_eq(udp.take_datagram().is_none(), true);
        }
        Ok(())
    }

    /// 真实 Closed 之后表与退休 worker 释放最后节点拥有，外部 Weak 不延长 Work/Timer 寿命。
    #[test]
    fn closed_connections_retire_work_timer_and_endpoint_slots(
        context: &mut TestContext<'_>,
        fixture: &mut StackOwner,
    ) -> TestResult {
        let listener = listener(context, &fixture.net, 45410)?;
        let client = operation(context, fixture.net.try_new_tcp(256, 256))?;
        operation(
            context,
            client.connect(InetEndpoint {
                port: 45410,
                ..SERVER
            }),
        )?;
        let server = accepted(context, &listener, &client)?;
        let client_weak = Arc::downgrade(&client);
        let server_weak = Arc::downgrade(&server);
        client.abandon();
        drop(client);
        drop(server);
        observe(
            || client_weak.upgrade().is_none() && server_weak.upgrade().is_none(),
            "closed protocol nodes retained after retirement",
        )?;
        listener.close();
        let listener_weak = Arc::downgrade(&listener);
        drop(listener);
        observe(
            || listener_weak.upgrade().is_none(),
            "closed listener retained after retirement",
        )?;
        let mut nodes: Vec<Arc<UdpEndpoint>> = Vec::new();
        nodes
            .try_reserve_exact(128)
            .map_err(|_| TestError("endpoint capacity observation allocation failed"))?;
        for _ in 0..128 {
            nodes.push(operation(context, fixture.net.try_new_udp())?);
        }
        match fixture.net.try_new_udp() {
            Err(FsError::NoMemory) => {}
            _ => return Err(TestError("live endpoint slot capacity not enforced")),
        }
        for endpoint in &nodes {
            endpoint.close();
        }
        drop(nodes);
        let recovered = operation(context, fixture.net.try_new_udp())?;
        recovered.close();
        Ok(())
    }

    /// 固定 CPU0 的短 IRQ 停点使真实 lo 满；另一核 SYN emitter 登记等待后由容量恢复唤醒。
    #[test]
    #[ktest(min_cpus = 2)]
    fn full_egress_waits_for_real_capacity_and_resumes(
        context: &mut TestContext<'_>,
        fixture: &mut StackOwner,
    ) -> TestResult {
        let listener = listener(context, &fixture.net, 45420)?;
        let client = operation(context, fixture.net.try_new_tcp(256, 256))?;
        context.expect_eq(client.work_cpu(), 1);
        let destination = InetEndpoint {
            port: 45421,
            ..SERVER
        };
        let mut packets = Vec::new();
        packets
            .try_reserve_exact(LOOPBACK_CAPACITY)
            .map_err(|_| TestError("egress prepared packets allocation failed"))?;
        for _ in 0..LOOPBACK_CAPACITY {
            packets.push(operation(
                context,
                OwnedPacket::try_from_udp(
                    CLIENT,
                    destination,
                    b"real egress capacity",
                    LOOPBACK_MTU,
                ),
            )?);
        }
        let extra = operation(
            context,
            OwnedPacket::try_from_udp(
                CLIENT,
                destination,
                b"unaccepted owned packet",
                LOOPBACK_MTU,
            ),
        )?;
        let full = Arc::try_new(AtomicBool::new(false))
            .map_err(|_| TestError("egress full observation allocation failed"))?;
        let full_by_child = Arc::clone(&full);
        let release = Arc::clone(&fixture.release);
        let net = Arc::clone(&fixture.net);
        let controlled_client = Arc::clone(&client);
        let controller = crate::ktest::spawn_result_on(1, move |_| {
            let client = controlled_client;
            let queued_net = Arc::clone(&net);
            let gate_release = Arc::clone(&release);
            let child = crate::ktest::spawn_result_on(0, move |token| {
                let mut unaccepted = None;
                let mut error = None;
                let mut packets = packets.into_iter();
                {
                    // 所有报文和控制拥有已在普通上下文准备；停点内只短锁移包和原子观察。
                    let irq = crate::sync::IrqSaveGuard::new();
                    for packet in packets.by_ref() {
                        if let Err((_, packet)) = queued_net.try_emit(packet) {
                            unaccepted = Some(packet);
                            error = Some("prepared egress queue did not accept 32 records");
                            break;
                        }
                    }
                    if error.is_none() {
                        let pointer = extra.bytes().as_ptr();
                        let length = extra.len();
                        match queued_net.try_emit(extra) {
                            Err((FsError::WouldBlock, packet))
                                if packet.bytes().as_ptr() == pointer && packet.len() == length =>
                            {
                                unaccepted = Some(packet);
                                full_by_child.store(true, Ordering::Release);
                            }
                            Err((_, packet)) => {
                                unaccepted = Some(packet);
                                error = Some(
                                    "full egress did not preserve unaccepted packet ownership",
                                );
                            }
                            Ok(()) => error = Some("egress queue exceeded actual packet capacity"),
                        }
                    }
                    let end = time::now()
                        .checked_add(time::Duration::from_secs(5))
                        .unwrap();
                    while error.is_none()
                        && !gate_release.load(Ordering::Acquire)
                        && !token.stop_requested()
                    {
                        if end <= time::now() {
                            error = Some("egress IRQ gate release deadline");
                            break;
                        }
                        ::core::hint::spin_loop();
                    }
                    drop(irq);
                }
                drop(packets);
                drop(unaccepted);
                error.map_or(Ok(0), |message| Err(TestError(message)))
            })?;
            observe(
                || full.load(Ordering::Acquire) || child.try_join().is_some(),
                "egress queue did not reach actual full capacity",
            )?;
            if !full.load(Ordering::Acquire) {
                return Err(TestError(
                    "egress capacity control failed before full observation",
                ));
            }
            child_operation(client.connect(InetEndpoint {
                port: 45420,
                ..SERVER
            }))?;
            observe(
                || net.egress_waiting(client.id()),
                "full SYN emitter did not register actual egress wait",
            )?;
            if client.state() != State::SynSent {
                return Err(TestError(
                    "full egress submitted SYN before capacity recovered",
                ));
            }
            timer::sleep(time::Duration::from_millis(20));
            if !net.egress_waiting(client.id()) {
                return Err(TestError(
                    "egress wait disappeared while ingress CPU was stopped",
                ));
            }
            release.store(true, Ordering::Release);
            if child.join()? != 0 {
                return Err(TestError("egress IRQ gate child failed"));
            }
            Ok(0)
        })?;
        context.expect_eq(controller.join()?, 0);
        let server = accepted(context, &listener, &client)?;
        context.expect_eq(fixture.net.egress_waiting(client.id()), false);
        let input = b"capacity recovered stream";
        context.expect_eq(
            operation(context, client.lock().send(&mut WriteBuffer::kernel(input)))?,
            IoProgress::Ready(input.len()),
        );
        observe(
            || server.lock().receive_queue() == input.len(),
            "capacity-restored stream did not advance independently",
        )?;
        let mut output = [0u8; 64];
        context.expect_eq(
            operation(
                context,
                server
                    .lock()
                    .receive(&mut ReadBuffer::kernel(&mut output), false),
            )?,
            IoProgress::Ready(input.len()),
        );
        context.expect_eq(&output[..input.len()], input);
        Ok(())
    }

    /// 真实 owner 停止仍被外部 Arc 保活的连接与 UDP 队列，关闭后的域不再接受节点或包。
    #[test]
    fn owner_stop_ends_live_protocol_and_rejects_new_ownership(
        context: &mut TestContext<'_>,
        fixture: &mut StackOwner,
    ) -> TestResult {
        let listener = listener(context, &fixture.net, 45430)?;
        let client = operation(context, fixture.net.try_new_tcp(256, 256))?;
        operation(
            context,
            client.connect(InetEndpoint {
                port: 45430,
                ..SERVER
            }),
        )?;
        let server = accepted(context, &listener, &client)?;
        let udp = operation(context, fixture.net.try_new_udp())?;
        let local = operation(
            context,
            udp.bind(InetEndpoint {
                port: 45431,
                ..SERVER
            }),
        )?;
        operation(
            context,
            udp.enqueue(operation(
                context,
                OwnedPacket::try_from_udp(CLIENT, local, b"owned until shutdown", LOOPBACK_MTU),
            )?),
        )?;
        operation(
            context,
            client
                .lock()
                .send(&mut WriteBuffer::kernel(b"pending TCP shutdown")),
        )?;
        drop(fixture.owner.take());
        context.expect_eq(fixture.net.is_stopping(), true);
        context.expect_eq(client.state(), State::Closed);
        context.expect_eq(server.state(), State::Closed);
        context.expect_eq(listener.state(), State::Closed);
        context.expect_eq(udp.take_datagram().is_none(), true);
        context.expect_eq(udp.local(), None);
        match fixture.net.try_new_udp() {
            Err(FsError::NotConnected) => {}
            _ => return Err(TestError("stopped network accepted new endpoint ownership")),
        }
        let packet = operation(
            context,
            OwnedPacket::try_from_udp(CLIENT, local, b"", LOOPBACK_MTU),
        )?;
        match fixture.net.try_emit(packet) {
            Err((FsError::NotConnected, packet)) => {
                context.expect_eq(operation(context, packet.udp_payload())?, &b""[..]);
            }
            _ => return Err(TestError("stopped network accepted new egress ownership")),
        }
        Ok(())
    }
}

/// 正式 SocketFile listener 使用同一域与用户文件数据面，不构造第二 raw wire。
fn socket_listener(
    context: &TestContext<'_>,
    net: &NetStackRef,
    port: u16,
) -> Result<Arc<SocketFile>, TestError> {
    let file = operation(context, SocketFile::try_new_tcp(net.clone(), 0, 0, 0))?;
    operation(
        context,
        file.bind(SocketAddress::Inet(InetEndpoint { port, ..SERVER })),
    )?;
    operation(context, file.listen(4))?;
    Ok(file)
}

/// 文件流按合法短读推进，零长度只在调用者明确验 EOF 时消费。
fn socket_read_exact(file: &SocketFile, output: &mut [u8]) -> TestResult {
    let mut done = 0;
    while done < output.len() {
        let count = child_operation(file.read(&mut output[done..]))?;
        if count == 0 {
            return Err(TestError("native socket ended before full byte delivery"));
        }
        done += count;
    }
    Ok(())
}

/// 文件流发送量来自真实接受前缀，不把输入准备量当成提交量。
fn socket_write_all(file: &SocketFile, input: &[u8]) -> TestResult {
    let mut done = 0;
    while done < input.len() {
        let count = child_operation(file.write(&input[done..]))?;
        if count == 0 {
            return Err(TestError("native socket send made no progress"));
        }
        done += count;
    }
    Ok(())
}

/// 只等待实际 EndpointSlot 归还，根服务继续存活并推进正常 FIN/TIME_WAIT。
fn socket_retired(net: &NetStackRef) -> TestResult {
    let end = time::now()
        .checked_add(time::Duration::from_secs(30))
        .ok_or(TestError("socket retirement deadline overflow"))?;
    while net.ktest_live_endpoints() != 0 {
        if end <= time::now() {
            return Err(TestError(
                "normal socket protocol retirement did not finish",
            ));
        }
        timer::sleep(time::Duration::from_millis(1));
    }
    Ok(())
}

/// 固定接收容量直接交给正式 scan，逐项保存真实事件字段供多轮 ET 消费核对。
fn socket_epoll_collect(file: &EpollFile, output: &mut [EpollEvent]) -> Result<usize, FsError> {
    let mut count = 0;
    file.scan(output.len(), &mut |event| {
        output[count] = event;
        count += 1;
        Ok(())
    })
}

#[ktest::suite(fixture = StackOwner, suite_resources = SuiteBaseline, context = "Thread", tags("network-socket"), timeout_ms = 45000)]
pub(crate) mod socket_native {
    use super::*;

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
            SocketFile::try_new_tcp(fixture.net.clone(), 0, 0, 0),
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
            SocketFile::try_new_tcp(fixture.net.clone(), 0, 0, 0),
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
            SocketFile::try_new_tcp(fixture.net.clone(), 0, 0, 0),
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
            SocketFile::try_new_tcp(fixture.net.clone(), 0, 0, 0),
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
            SocketFile::try_new_tcp(fixture.net.clone(), 0, 0, 0),
        )?;
        let client_b = operation(
            context,
            SocketFile::try_new_tcp(fixture.net.clone(), 0, 0, 0),
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
            SocketFile::try_new_tcp(fixture.net.clone(), 0, 0, 0),
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
            SocketFile::try_new_tcp(fixture.net.clone(), 0, 0, 0),
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
            SocketFile::try_new_tcp(fixture.net.clone(), 0, 1000, 2000),
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
            SocketFile::try_new_tcp(fixture.net.clone(), 0, 0, 0),
        )?;
        let endpoint = operation(context, listener.tcp_endpoint())?;
        context.expect_eq(endpoint.lock().nagle_enabled(), true);
        operation(context, listener.set_nodelay(true))?;
        context.expect_eq(endpoint.lock().nagle_enabled(), false);
        operation(context, listener.set_nodelay(false))?;
        context.expect_eq(endpoint.lock().nagle_enabled(), true);
        operation(context, listener.set_nodelay(true))?;
        operation(context, listener.set_reuse_address(true))?;
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
            SocketFile::try_new_tcp(fixture.net.clone(), 0, 0, 0),
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

    /// UNIX 末 OFD 撤真实 watch，别名保活到最后 put，两个 slot 在根未停止时归还。
    #[test]
    fn unix_last_ofd_watch_weak_and_slots_retire_normally(
        context: &mut TestContext<'_>,
        fixture: &mut StackOwner,
    ) -> TestResult {
        let [first, second] = operation(
            context,
            SocketFile::try_new_unix_pair(fixture.net.clone(), 0, 0, 0),
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
        let [first, second] = operation(context, fixture.net.try_new_unix_pair())?;
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
            occupied.push(operation(context, fixture.net.try_new_udp())?);
        }
        match fixture.net.try_new_unix_pair() {
            Err(FsError::NoMemory) => {}
            _ => return Err(TestError("UNIX pair exceeded two actual endpoint slots")),
        }
        context.expect_eq(fixture.net.ktest_live_endpoints(), 127);
        let last = operation(context, fixture.net.try_new_udp())?;
        context.expect_eq(fixture.net.ktest_live_endpoints(), 128);
        last.close();
        for endpoint in &occupied {
            endpoint.close();
        }
        drop((last, occupied));
        socket_retired(&fixture.net)?;
        let [first, second] = operation(context, fixture.net.try_new_unix_pair())?;
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

    /// AF_UNSPEC 后旧准备 Lease 保端口，旧末 put 不得撤同端点的后继绑定或阻止真实新包交付。
    #[test]
    fn udp_prepared_lease_survives_disconnect_without_unbinding_successor(
        context: &mut TestContext<'_>,
        fixture: &mut StackOwner,
    ) -> TestResult {
        let receiver = operation(context, fixture.net.try_new_udp())?;
        let destination = operation(
            context,
            receiver.bind(InetEndpoint {
                port: 45670,
                ..SERVER
            }),
        )?;
        let sender = operation(context, fixture.net.try_new_udp())?;
        operation(context, sender.connect(Some(destination)))?;
        let (old_packet, old_lease) =
            operation(context, sender.prepare_send(b"old prepared message", None))?;
        let old_local = old_lease.local();
        context.expect_eq(
            operation(context, old_packet.udp_endpoints())?.0.port,
            old_local.port,
        );
        operation(context, sender.connect(None))?;
        context.expect_eq(sender.peer_address(), Err(FsError::NotConnected));
        context.expect_eq(
            operation(context, sender.local_address())?,
            InetEndpoint {
                address: [0; 4],
                port: 0,
            },
        );
        let contender = operation(context, fixture.net.try_new_udp())?;
        context.expect_eq(contender.bind(old_local), Err(FsError::AddressInUse));
        let input = b"successor prepared datagram";
        let (new_packet, new_lease) =
            operation(context, sender.prepare_send(input, Some(destination)))?;
        let new_local = new_lease.local();
        context.expect_eq(new_local.port != old_local.port, true);
        context.expect_eq(sender.peer_address(), Err(FsError::NotConnected));
        let expected_source = InetEndpoint {
            address: SERVER.address,
            port: new_local.port,
        };
        context.expect_eq(
            operation(context, new_packet.udp_endpoints())?,
            (expected_source, destination),
        );
        drop((old_packet, old_lease));
        context.expect_eq(operation(context, contender.bind(old_local))?, old_local);
        let third = operation(context, fixture.net.try_new_udp())?;
        context.expect_eq(third.bind(new_local), Err(FsError::AddressInUse));
        operation(context, fixture.net.send_ipv4(new_packet))?;
        let mut delivered = None;
        observe(
            || {
                delivered = receiver.take_datagram();
                delivered.is_some()
            },
            "successor UDP packet did not reach actual receiver",
        )?;
        let delivered = delivered.ok_or(TestError("successor UDP record disappeared"))?;
        context.expect_eq(
            operation(context, delivered.udp_endpoints())?,
            (expected_source, destination),
        );
        context.expect_eq(operation(context, delivered.udp_payload())?, &input[..]);
        context.expect_eq(receiver.take_datagram().is_none(), true);
        context.diagnostic(format_args!(
            "network-socket udp-old-port={} udp-new-port={} successor-payload-bytes={} default-peer-none=1",
            old_local.port, new_local.port, input.len()
        ));
        sender.close();
        receiver.close();
        contender.close();
        third.close();
        drop((delivered, new_lease, sender, receiver, contender, third));
        socket_retired(&fixture.net)?;
        context.expect_eq(fixture.net.ktest_live_endpoints(), 0);
        context.expect_eq(fixture.net.is_stopping(), false);
        Ok(())
    }
}
