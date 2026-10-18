//! 本例同步 wire 拥有两个 TcpCore，没有生产后台服务需要停止。

use ::ktest::{Fixture, TestContext, TestError, TestResult};
use alloc::collections::VecDeque;
use smoltcp::socket::tcp::State;

use super::support::{operation, CLIENT, SERVER};
use crate::fs::FsError;
use crate::ktest::execution_services::{baseline, Baseline, SuiteBaseline};
use crate::net::core::TcpCore;
use crate::net::packet::OwnedPacket;
use crate::net::{LOOPBACK_CAPACITY, LOOPBACK_MTU};
use crate::sync::{SleepMutex, SpinLock};
use crate::{time, timer};

pub(super) struct NetworkOwner {
    before: Baseline,
    wire: SpinLock<VecDeque<OwnedPacket>>,
    pub(super) client: SleepMutex<TcpCore>,
    pub(super) server: SleepMutex<TcpCore>,
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
    pub(super) fn enqueue(&self, packet: OwnedPacket) -> Result<(), FsError> {
        let mut wire = self.wire.lock();
        if LOOPBACK_CAPACITY <= wire.len() {
            return Err(FsError::WouldBlock);
        }
        wire.push_back(packet);
        Ok(())
    }

    /// 锁外处理包与复制，wire 不持生产网络域或其它核心处理权。
    pub(super) fn take_packet(&self) -> Option<OwnedPacket> {
        self.wire.lock().pop_front()
    }

    /// 本例 wire 移动库 repr 与 payload 的真实拥有，锁外选择目的端处理。
    pub(super) fn pump(&self, context: &TestContext<'_>) -> TestResult {
        let now = time::now();
        for endpoint in [&self.client, &self.server] {
            operation(
                context,
                endpoint.lock().dispatch(now, |packet| self.enqueue(packet)),
            )?;
        }
        while let Some(packet) = self.take_packet() {
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
    pub(super) fn until(
        &self,
        context: &TestContext<'_>,
        condition: impl Fn() -> bool,
    ) -> TestResult {
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
    pub(super) fn connect(&self, context: &TestContext<'_>) -> TestResult {
        self.until(context, || {
            self.client.lock().state() == State::Established
                && self.server.lock().state() == State::Established
        })
    }
}
