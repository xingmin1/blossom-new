//! 用户任务ABI只复制已初始化字节，状态提交在用户交付之外明确完成。

use core::convert::TryInto;

use crate::syscall::error::{copyin, copyout, Errno, SysResult};
use crate::syscall::Request;
use crate::task::clone::{self, CloneArguments, CloneError};
use crate::task::signal::{SignalAction, SignalError};
use crate::task::UserThread;

/// 按架构次序解码本次六参数并发表真实child，返回的是用户TID而非TaskId。
pub(super) fn clone(user: &UserThread, request: Request) -> SysResult {
    let arguments = CloneArguments::decode(request.arguments).map_err(clone_error)?;
    clone::create(user, request, arguments)
        .map(|tid| tid as isize)
        .map_err(clone_error)
}

/// 完整快照新动作后在同siglock取旧值并替换；旧动作copyout失败不撤已提交的新处置。
pub(super) fn sigaction(user: &UserThread, raw: [usize; 6]) -> SysResult {
    if raw[3] != 8 {
        return Err(Errno::InvalidArgument);
    }
    let mm = user.mm().expect("sigaction has no MM");
    let runtime = user.runtime().expect("sigaction has no runtime");
    let action = if raw[1] == 0 {
        None
    } else {
        let mut bytes = [0u8; 24];
        copyin(&mm, &mut bytes, raw[1])?;
        Some(SignalAction {
            handler: u64::from_le_bytes(bytes[0..8].try_into().expect("action handler width"))
                as usize,
            flags: u64::from_le_bytes(bytes[8..16].try_into().expect("action flags width"))
                as usize,
            mask: u64::from_le_bytes(bytes[16..24].try_into().expect("action mask width")),
        })
    };
    let previous = runtime
        .sighand
        .update(raw[0] as i32 as usize, action)
        .map_err(|error| match error {
            SignalError::InvalidSignal => Errno::InvalidArgument,
        })?;
    if raw[2] != 0 {
        let mut bytes = [0u8; 24];
        bytes[0..8].copy_from_slice(&(previous.handler as u64).to_le_bytes());
        bytes[8..16].copy_from_slice(&(previous.flags as u64).to_le_bytes());
        bytes[16..24].copy_from_slice(&previous.mask.to_le_bytes());
        copyout(&mm, raw[2], &bytes)?;
    }
    Ok(0)
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
