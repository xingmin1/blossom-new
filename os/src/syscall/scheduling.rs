//! 线程调度策略与实时优先级的 Linux 用户 ABI。
//!
//! 本模块解析 `sched_param` 的单个 32 位有符号优先级，复用 affinity 的稳定线程引用查找，
//! 再由原调度器在 `SCHED_LOCK` 下校验、更新和执行 OTHER/FIFO 策略。
//! 用户复制、运行期凭据读取均在调度锁外；未实现的调度策略返回 `EINVAL`。

use crate::sched::{self, SchedulingError};
use crate::syscall::affinity::target;
use crate::syscall::error::{copyin, copyout, Errno, SysResult};
use crate::task::{Policy, UserThread};

/// 普通时间片策略的用户 ABI 编号。
const SCHED_OTHER: i32 = 0;
/// 无同级时间片轮转的实时 FIFO 策略编号。
const SCHED_FIFO: i32 = 1;
/// 派生子线程时清除实时策略的用户 ABI 标志。
const SCHED_RESET_ON_FORK: i32 = 0x4000_0000;

/// 分发两种实际支持策略的参数修改、查询与优先级范围系统调用。
pub(super) fn dispatch(user: &UserThread, number: usize, raw: [usize; 6]) -> SysResult {
    match number {
        118 | 119 => set(user, number == 119, raw),
        120 => {
            let task = target(user, raw[0], false)?;
            let (policy, _, reset) = sched::parameters(&task);
            let policy = match policy {
                Policy::Other => SCHED_OTHER,
                Policy::Fifo => SCHED_FIFO,
            };
            Ok((policy | if reset { SCHED_RESET_ON_FORK } else { 0 }) as isize)
        }
        121 => get_param(user, raw),
        125 | 126 => match raw[0] as i32 {
            SCHED_OTHER => Ok(0),
            SCHED_FIFO => Ok(if number == 125 {
                sched::MAX_RT_PRIORITY as isize
            } else {
                1
            }),
            _ => Err(Errno::InvalidArgument),
        },
        _ => Err(Errno::NoSyscall),
    }
}

/// 读取参数后解析稳定目标，在调度锁外取得凭据，再由调度器原子校验并提交策略变化。
fn set(user: &UserThread, change_policy: bool, raw: [usize; 6]) -> SysResult {
    let policy = raw[1] as i32;
    if change_policy && policy < 0 {
        return Err(Errno::InvalidArgument);
    }
    let address = if change_policy { raw[2] } else { raw[1] };
    if (raw[0] as i32) < 0 || address == 0 {
        return Err(Errno::InvalidArgument);
    }
    let mm = user.mm().ok_or(Errno::NoProcess)?;
    let mut bytes = [0u8; core::mem::size_of::<i32>()];
    copyin(&mm, &mut bytes, address)?;
    let priority = i32::from_ne_bytes(bytes);
    let task = target(user, raw[0], false)?;
    let policy = if change_policy {
        let reset = policy & SCHED_RESET_ON_FORK != 0;
        let policy = match policy & !SCHED_RESET_ON_FORK {
            SCHED_OTHER => Policy::Other,
            SCHED_FIFO => Policy::Fifo,
            _ => return Err(Errno::InvalidArgument),
        };
        Some((policy, reset))
    } else {
        None
    };
    let caller = user.runtime().ok_or(Errno::NoProcess)?.credentials.uid;
    let owner = task
        .user_thread()
        .and_then(|thread| thread.runtime())
        .ok_or(Errno::NoProcess)?
        .credentials
        .uid;
    sched::set_parameters(&task, policy, priority, caller == owner, caller == 0).map_err(
        |error| match error {
            SchedulingError::Invalid => Errno::InvalidArgument,
            SchedulingError::Permission => Errno::NotPermitted,
        },
    )?;
    Ok(0)
}

/// 在调度锁内取得优先级快照，放锁后只向用户交付四字节的 `sched_param`。
fn get_param(user: &UserThread, raw: [usize; 6]) -> SysResult {
    if (raw[0] as i32) < 0 || raw[1] == 0 {
        return Err(Errno::InvalidArgument);
    }
    let task = target(user, raw[0], false)?;
    let (_, priority, _) = sched::parameters(&task);
    let mm = user.mm().ok_or(Errno::NoProcess)?;
    copyout(&mm, raw[1], &(priority as i32).to_ne_bytes())?;
    Ok(0)
}
