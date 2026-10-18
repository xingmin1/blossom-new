//! SocketFile 的完整流、真实 epoll 扫描与 UDP 压力停点准备。

use ::core::sync::atomic::{AtomicBool, Ordering};
use ::ktest::{TestContext, TestError, TestResult};
use alloc::sync::Arc;

use super::support::{child_operation, operation, SERVER};
use crate::fs::epoll::{EpollEvent, EpollFile};
use crate::fs::{FileLike, FsError};
use crate::net::address::{AddressFamily, InetEndpoint, SocketAddress};
use crate::net::socket::SocketFile;
use crate::net::NetStackRef;
use crate::time;

/// 正式 SocketFile listener 使用同一域与用户文件数据面，不构造第二 raw wire。
pub(super) fn socket_listener(
    context: &TestContext<'_>,
    net: &NetStackRef,
    port: u16,
) -> Result<Arc<SocketFile>, TestError> {
    let file = operation(
        context,
        SocketFile::try_new_tcp(net.clone(), AddressFamily::Ipv4, 0, 0, 0),
    )?;
    operation(
        context,
        file.bind(SocketAddress::Inet(InetEndpoint { port, ..SERVER })),
    )?;
    operation(context, file.listen(4))?;
    Ok(file)
}

/// 文件流按合法短读推进，零长度只在调用者明确验 EOF 时消费。
pub(super) fn socket_read_exact(file: &SocketFile, output: &mut [u8]) -> TestResult {
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
pub(super) fn socket_write_all(file: &SocketFile, input: &[u8]) -> TestResult {
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

/// 固定接收容量直接交给正式 scan，逐项保存真实事件字段供多轮 ET 消费核对。
pub(super) fn socket_epoll_collect(
    file: &EpollFile,
    output: &mut [EpollEvent],
) -> Result<usize, FsError> {
    let mut count = 0;
    file.scan(output.len(), &mut |event| {
        output[count] = event;
        count += 1;
        Ok(())
    })
}

/// 先预备而不发表停点任务，使压力回收仍能向 CPU0 同步发送 SLUB IPI。
/// 恢复 IRQ 后通过原调度器让出执行权，同时保留栈；提前离栈会给大消息归还可分配页。
pub(super) fn prepare_udp_oom_gate(
    entered: Arc<AtomicBool>,
    release: Arc<AtomicBool>,
    finished: Arc<AtomicBool>,
) -> Result<crate::ktest::PreparedChild, TestError> {
    crate::ktest::prepare_result_on(0, move |token| {
        let irq = crate::sync::IrqSaveGuard::new();
        entered.store(true, Ordering::Release);
        let end = time::now()
            .checked_add(time::Duration::from_secs(5))
            .ok_or(TestError("UDP OOM IRQ gate deadline overflow"))?;
        let mut error = None;
        while !release.load(Ordering::Acquire) && !token.stop_requested() {
            if end <= time::now() {
                error = Some(TestError("UDP OOM IRQ gate release deadline"));
                break;
            }
            ::core::hint::spin_loop();
        }
        drop(irq);

        // 内核 timer 只置调度提示；普通 Task 必须显式让出，纯原子自旋会饿死同核 Work。
        while !finished.load(Ordering::Acquire) && !token.stop_requested() {
            crate::sched::yield_now();
        }

        error.map_or(Ok(0), Err)
    })
}
