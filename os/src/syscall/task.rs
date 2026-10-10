//! 用户任务ABI只复制已初始化字节，状态消费与用户交付分开完成。

use crate::fs::{self, InodeKind, ResolveMode};
use crate::mm::space::VmError;
use crate::syscall::error::{copyout, Errno, SysResult};
use crate::syscall::{file, Outcome, Request, Restart};
use crate::task::clone::{self, CloneArguments, CloneError};
use crate::task::exec::{self, Arguments, ExecError, LoadedImage};
use crate::task::process::{ChildEvent, WaitSelector};
use crate::task::signal::{SignalInfo, SignalPayload};
use crate::task::{UserRuntime, UserThread};
use crate::wait::{WaitNode, WaitResult, Waiter};

const NOHANG: u32 = 1;
const STOPPED: u32 = 2;
const EXITED: u32 = 4;
const CONTINUED: u32 = 8;
const NOWAIT: u32 = 0x0100_0000;
const WALL: u32 = 0x4000_0000;
const WCLONE: u32 = 0x8000_0000;

/// 按架构次序解码本次六参数并发表真实child，返回用户TID而非TaskId。
pub(super) fn clone(user: &UserThread, request: Request) -> SysResult {
    let arguments = CloneArguments::decode(request.arguments).map_err(clone_error)?;
    clone::create(user, request, arguments)
        .map(|tid| tid as isize)
        .map_err(clone_error)
}

/// 准备失败保留旧程序，跨不可回返点后的任何失败只沿真实组退出清理。
pub(super) fn execve(user: &UserThread, request: Request) -> Outcome {
    let (image, runtime) = match prepare_exec(user, request.arguments) {
        Ok(prepared) => prepared,
        Err(error) => return Outcome::Return(error.result()),
    };
    let identity = user.identity().expect("exec has no identity");
    if identity.graph().de_thread(&identity).is_err() {
        return abort_exec(user);
    }
    let runtime = match runtime.try_exec() {
        Ok(runtime) => runtime,
        Err(_) => return abort_exec(user),
    };
    user.install_image(image, runtime);
    if identity.graph().complete_exec(&identity).is_err() {
        return abort_exec(user);
    }
    Outcome::NewImage
}

/// 路径与参数只读取一次；正式OFD校验后在私有Mm完成ELF与初始栈准备。
fn prepare_exec(user: &UserThread, raw: [usize; 6]) -> Result<(LoadedImage, UserRuntime), Errno> {
    let runtime = user.runtime().expect("exec has no runtime");
    let mm = user.mm().expect("exec has no MM");
    let pathname = mm.read_c_string(raw[0], 4096)?;
    let path = file::resolve_bytes(&runtime, (-100i32) as usize, &pathname, ResolveMode::Follow)?;
    let credentials = runtime.credentials;
    if path.inode()?.metadata_snapshot()?.kind != InodeKind::Regular {
        return Err(Errno::Permission);
    }
    path.check_access(credentials.uid, credentials.gid, 1)?;
    let vfs = runtime.fs.lock().vfs.clone();
    let file = fs::open_path(&vfs, path)?;
    let arguments = Arguments::from_user(&mm, pathname, raw[1], raw[2]).map_err(exec_error)?;
    let image =
        exec::load(file, &arguments, credentials.uid, credentials.gid).map_err(exec_error)?;
    Ok((image, runtime))
}

/// 不可回返点后不伪造errno返回旧现场，已存在的致命组原因保持优先。
fn abort_exec(user: &UserThread) -> Outcome {
    let identity = user.identity().expect("failed exec has no identity");
    let status = identity.graph().exit_request(&identity).unwrap_or(11);
    identity.graph().request_exit(&identity, status);
    Outcome::GroupExit(status)
}

/// 有限进程组、会话和亲缘查询共用原身份图，不从调度队列按PID查执行者。
pub(super) fn identity(user: &UserThread, number: usize, raw: [usize; 6]) -> SysResult {
    let identity = user.identity().expect("identity syscall has no identity");
    let graph = identity.graph();
    match number {
        154 => {
            let pid = raw[0] as i32;
            let pgid = raw[1] as i32;
            if pgid < 0 {
                return Err(Errno::InvalidArgument);
            }
            if pid < 0 {
                return Err(Errno::NoProcess);
            }
            graph.set_pgid(&identity, pid as usize, pgid as usize)?;
            Ok(0)
        }
        155 => Ok(graph.get_pgid(&identity, raw[0] as i32 as usize)? as isize),
        156 => Ok(graph.get_sid(&identity, raw[0] as i32 as usize)? as isize),
        157 => Ok(graph.create_session(&identity)? as isize),
        173 => Ok(graph.parent_pid(&identity) as isize),
        _ => unreachable!("unknown identity dispatch"),
    }
}

/// waitid按原身份对象筛选；领取后rusage或siginfo交付失败不恢复事件。
pub(super) fn waitid(user: &UserThread, request: Request) -> Outcome {
    let raw = request.arguments;
    let options = raw[3] as u32;
    if options & !(NOHANG | STOPPED | EXITED | CONTINUED | NOWAIT | WALL | WCLONE) != 0
        || options & (STOPPED | EXITED | CONTINUED) == 0
    {
        return Outcome::Return(Errno::InvalidArgument.result());
    }
    let identity = user.identity().expect("waitid has no identity");
    let selector = match raw[0] as i32 {
        0 => Ok(WaitSelector::Any),
        1 if raw[1] as i32 > 0 => identity
            .graph()
            .pid_object(raw[1] as i32 as usize)
            .map(WaitSelector::Pid)
            .ok_or(Errno::NoChild),
        2 if raw[1] as i32 == 0 => Ok(WaitSelector::Group(identity.group().pgid_object())),
        2 if raw[1] as i32 > 0 => identity
            .graph()
            .pid_object(raw[1] as i32 as usize)
            .map(WaitSelector::Group)
            .ok_or(Errno::NoChild),
        _ => Err(Errno::InvalidArgument),
    };
    let selector = match selector {
        Ok(selector) => selector,
        Err(error) => return Outcome::Return(error.result()),
    };
    let event = match wait_event(user, request, &selector, options) {
        Ok(event) => event,
        Err(outcome) => return outcome,
    };
    let result: SysResult = (|| {
        let mm = user.mm().expect("waitid has no MM");
        let mut bytes = [0u8; 128];
        if let Some(event) = event {
            if raw[4] != 0 {
                copyout(&mm, raw[4], &usage(event))?;
            }
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
        if raw[2] != 0 {
            copyout(&mm, raw[2], &bytes)?;
        }
        Ok(0)
    })();
    Outcome::Return(result.unwrap_or_else(Errno::result))
}

/// wait4的负PID选择进程组，零选择本组；无事件不改status和rusage。
pub(super) fn wait4(user: &UserThread, request: Request) -> Outcome {
    let raw = request.arguments;
    let options = raw[2] as u32;
    if options & !(NOHANG | STOPPED | CONTINUED | WALL | WCLONE) != 0 {
        return Outcome::Return(Errno::InvalidArgument.result());
    }
    let identity = user.identity().expect("wait4 has no identity");
    let pid = raw[0] as i32;
    let selector = match pid {
        i32::MIN => Err(Errno::NoProcess),
        -1 => Ok(WaitSelector::Any),
        1.. => identity
            .graph()
            .pid_object(pid as usize)
            .map(WaitSelector::Pid)
            .ok_or(Errno::NoChild),
        0 => Ok(WaitSelector::Group(identity.group().pgid_object())),
        _ => identity
            .graph()
            .pid_object(pid.unsigned_abs() as usize)
            .map(WaitSelector::Group)
            .ok_or(Errno::NoChild),
    };
    let selector = match selector {
        Ok(selector) => selector,
        Err(error) => return Outcome::Return(error.result()),
    };
    let event = match wait_event(user, request, &selector, options | EXITED) {
        Ok(Some(event)) => event,
        Ok(None) => return Outcome::Return(0),
        Err(outcome) => return outcome,
    };
    let result: SysResult = (|| {
        let mm = user.mm().expect("wait4 has no MM");
        let status = match event.code {
            1 => event.status << 8,
            2 => event.status,
            3 => event.status | 0x80,
            5 => (event.status << 8) | 0x7f,
            6 => 0xffff,
            _ => unreachable!("unknown child event"),
        };
        if raw[1] != 0 {
            copyout(&mm, raw[1], &status.to_le_bytes())?;
        }
        if raw[3] != 0 {
            copyout(&mm, raw[3], &usage(event))?;
        }
        Ok(event.pid as isize)
    })();
    Outcome::Return(result.unwrap_or_else(Errno::result))
}

/// 同一Waiter登记后重查真实事件，中断只授权原PC/参数的内部重启。
fn wait_event(
    user: &UserThread,
    request: Request,
    selector: &WaitSelector,
    options: u32,
) -> Result<Option<ChildEvent>, Outcome> {
    let identity = user.identity().expect("wait has no identity");
    let group = identity.group();
    let graph = identity.graph();
    loop {
        match graph.try_wait_event(&group, selector, options) {
            Ok(Some(event)) => return Ok(Some(event)),
            Err(error) => return Err(Outcome::Return(Errno::from(error).result())),
            Ok(None) if options & NOHANG != 0 => return Ok(None),
            Ok(None) => {}
        }
        let waiter = Waiter::new_interruptible();
        let mut node = core::pin::pin!(WaitNode::new(waiter.waker()));
        // SAFETY: identity保活通知源；node固定本栈，registration先于node/Waiter撤除。
        let _registration = unsafe { graph.waiters().register(node.as_mut()) };
        match graph.try_wait_event(&group, selector, options) {
            Ok(Some(event)) => return Ok(Some(event)),
            Err(error) => return Err(Outcome::Return(Errno::from(error).result())),
            Ok(None) => {}
        }
        if waiter.park_signal() == WaitResult::Interrupted {
            return Err(Outcome::Restart(Restart::Syscall { request }));
        }
    }
}

/// timeval使用真实组及已reaped后代CPU账；尚无生产计数的其他rusage字段明确为零。
fn usage(event: ChildEvent) -> [u8; 144] {
    let mut bytes = [0u8; 144];
    for (offset, ns) in [(0, event.user_ns), (16, event.kernel_ns)] {
        bytes[offset..offset + 8].copy_from_slice(&(ns / 1_000_000_000).to_le_bytes());
        bytes[offset + 8..offset + 16]
            .copy_from_slice(&((ns % 1_000_000_000) / 1000).to_le_bytes());
    }
    bytes
}

/// ELF准备保格式和参数预算，用户参数访问错误独立编码为EFAULT。
fn exec_error(error: ExecError) -> Errno {
    match error {
        ExecError::Format => Errno::ExecFormat,
        ExecError::ArgumentsTooBig => Errno::ArgumentsTooBig,
        ExecError::NoMemory | ExecError::Vm(VmError::NoMemory) => Errno::NoMemory,
        ExecError::File(error) | ExecError::Vm(VmError::File(error)) => error.into(),
        ExecError::Vm(_) => Errno::BadAddress,
    }
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
