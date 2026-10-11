//! 原生 TCP 核心经 owned IPv4 包驱动协议，输入和复制故障使用实际内核对象。

use ::ktest::{Fixture, TestContext, TestError, TestResult};
use alloc::sync::Arc;
use smoltcp::socket::tcp::State;

use crate::fs::{FsError, ReadBuffer, WriteBuffer};
use crate::ktest::execution_services::{baseline, Baseline, SuiteBaseline};
use crate::mm::space::Mm;
use crate::mm::user_table::PagePermissions;
use crate::net::address::InetEndpoint;
use crate::net::core::ProtocolDeadline;
use crate::net::packet::OwnedPacket;
use crate::net::socket::TcpEndpoint;
use crate::net::{IoProgress, NetStack, NetStackRef, LOOPBACK_CAPACITY};
use crate::{time, timer};

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
    net: NetStackRef,
    client: Arc<TcpEndpoint>,
    server: Arc<TcpEndpoint>,
}

impl Fixture<SuiteBaseline> for NetworkOwner {
    /// 两核心、队列和随机环境均在本例基线之后准备，设置失败由局部拥有释放。
    fn set_up(context: &mut TestContext<'_>, _: &SuiteBaseline) -> Result<Self, TestError> {
        let before = baseline();
        let net = operation(context, NetStack::try_new_lo())?;
        let client = operation(context, net.try_new_tcp(256, 256))?;
        let server = operation(context, net.try_new_tcp(256, 256))?;
        operation(context, server.lock().listen(SERVER))?;
        operation(context, client.lock().connect(CLIENT, SERVER, time::now()))?;
        Ok(Self {
            before,
            net,
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
    /// 两端编码真实包，领取后以实际输入构造重新核线性字节，再锁外选目的端处理。
    fn pump(&self, context: &TestContext<'_>) -> TestResult {
        let now = time::now();
        for endpoint in [&self.client, &self.server] {
            operation(
                context,
                endpoint
                    .lock()
                    .dispatch(now, |packet| self.net.enqueue_loopback(packet)),
            )?;
        }
        while let Some(packet) = self.net.take_loopback() {
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
                operation(context, self.net.enqueue_loopback(reply))?;
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
        context.diagnostic(format_args!("network-core operation-error={:?}", error));
        TestError("network core operation failed")
    })
}

/// 输入按外部逻辑位置变化，环回绕或复制错误不能满足全文比较。
fn pattern(position: usize) -> u8 {
    (position as u8)
        .wrapping_mul(37)
        .wrapping_add((position / 251) as u8)
}

/// 保留原生宏实际锚点，不增加新的测试入口、feature或分派器。
pub(super) fn anchor() -> &'static ::ktest::SuiteSlot {
    core::__ktest_anchor()
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
            let endpoint = operation(context, fixture.net.try_new_tcp(64, 64))?;
            let local = InetEndpoint {
                address: CLIENT.address,
                port: 36000 + index as u16,
            };
            operation(context, endpoint.lock().connect(local, SERVER, time::now()))?;
            operation(
                context,
                endpoint
                    .lock()
                    .dispatch(time::now(), |packet| fixture.net.enqueue_loopback(packet)),
            )?;
        }
        let mut attempted = None;
        let failure = fixture.client.lock().dispatch(time::now(), |packet| {
            attempted = Some(packet.tcp()?.1.seq_number);
            fixture.net.enqueue_loopback(packet)
        });
        context.expect_eq(failure, Err(FsError::WouldBlock));
        context.expect_eq(
            fixture.client.lock().deadline(time::now()),
            ProtocolDeadline::Now,
        );
        for _ in 0..LOOPBACK_CAPACITY {
            fixture
                .net
                .take_loopback()
                .ok_or(TestError("full queue lost an accepted packet"))?;
        }
        let mut submitted = None;
        operation(
            context,
            fixture.client.lock().dispatch(time::now(), |packet| {
                submitted = Some(packet.tcp()?.1.seq_number);
                fixture.net.enqueue_loopback(packet)
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
