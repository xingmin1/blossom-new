//! 用户任务ABI只复制已初始化字节，状态提交在用户交付之外明确完成。

use crate::syscall::error::{copyout, Errno, SysResult};
use crate::syscall::{Outcome, Request, Restart};
use crate::task::clone::{self, CloneArguments, CloneError};
use crate::task::signal::{SignalInfo, SignalPayload};
use crate::task::UserThread;
use crate::wait::{WaitNode, WaitResult, Waiter};

/// 按架构次序解码本次六参数并发表真实child，返回的是用户TID而非TaskId。
pub(super) fn clone(user: &UserThread, request: Request) -> SysResult {
    let arguments = CloneArguments::decode(request.arguments).map_err(clone_error)?;
    clone::create(user, request, arguments)
        .map(|tid| tid as isize)
        .map_err(clone_error)
}

/// 本批waitid仅选择具体子PID的真实退出/停止/继续事件，领取后EFAULT不恢复事件。
pub(super) fn waitid(user: &UserThread, request: Request) -> Outcome {
    let raw = request.arguments;
    let options = raw[3] as u32;
    const ALLOWED: u32 = 1 | 2 | 4 | 8 | 0x0100_0000;
    if raw[0] as i32 != 1
        || raw[1] as i32 <= 0
        || options & !ALLOWED != 0
        || options & (2 | 4 | 8) == 0
    {
        return Outcome::Return(Errno::InvalidArgument.result());
    }
    if raw[4] != 0 {
        return Outcome::Return(Errno::Unsupported.result());
    }
    let identity = user.identity().expect("waitid has no identity");
    let group = identity.group();
    let graph = identity.graph();
    let pid = raw[1] as i32 as usize;
    let result = loop {
        match graph.try_wait_event(&group, pid, options & !1) {
            Ok(Some(event)) => break Ok(Some(event)),
            Err(error) => break Err(Errno::from(error)),
            Ok(None) if options & 1 != 0 => break Ok(None),
            Ok(None) => {}
        }
        let waiter = Waiter::new_interruptible();
        let mut node = core::pin::pin!(WaitNode::new(waiter.waker()));
        // SAFETY: identity保活同一图和通知源；node固定当前栈且registration先撤链，
        // 领取、返回或被信号中断均在登记撤除后才允许copyout或退栈。
        let _registration = unsafe { graph.waiters().register(node.as_mut()) };
        match graph.try_wait_event(&group, pid, options & !1) {
            Ok(Some(event)) => break Ok(Some(event)),
            Err(error) => break Err(Errno::from(error)),
            Ok(None) => {}
        }
        if waiter.park_signal() == WaitResult::Interrupted {
            return Outcome::Restart(Restart::Syscall { request });
        }
    };
    let result: SysResult = result.and_then(|event| {
        let mm = user.mm().expect("waitid has no MM");
        let mut bytes = [0u8; 128];
        if let Some(event) = event {
            SignalInfo {
                signum: 17,
                errno: 0,
                code: event.code,
                payload: SignalPayload::Child {
                    pid: event.pid,
                    uid: event.uid,
                    status: event.status,
                    user_ticks: 0,
                    kernel_ticks: 0,
                },
            }
            .encode(&mut bytes);
        }
        copyout(&mm, raw[2], &bytes)?;
        Ok(0)
    });
    Outcome::Return(result.unwrap_or_else(Errno::result))
}

/// 创建错误只在syscall边界编码，不把未支持标志或失败构造伪装为child存在。
fn clone_error(error: CloneError) -> Errno {
    match error {
        CloneError::InvalidFlags => Errno::InvalidArgument,
        CloneError::Unsupported => Errno::NoSyscall,
        CloneError::NoMemory => Errno::NoMemory,
        CloneError::Vm(error) => error.into(),
        CloneError::File(error) => error.into(),
        CloneError::Identity(error) => error.into(),
    }
}
