//! 每次调用只读取一次用户寄存器；返回值与退出是互斥控制结果，阻塞时不借用户现场。

mod error;
mod file;
mod futex;
mod memory;
mod poll;
pub(crate) mod signal;
mod system;
mod task;
pub(crate) mod time;

use crate::arch::UserContext;
use crate::sched;
use crate::syscall::error::{Errno, SysResult};
use crate::task::UserThread;

#[cfg(target_arch = "riscv64")]
const ARGUMENT_REGISTER: usize = 10;
#[cfg(target_arch = "loongarch64")]
const ARGUMENT_REGISTER: usize = 4;
#[cfg(target_arch = "riscv64")]
const NUMBER_REGISTER: usize = 17;
#[cfg(target_arch = "loongarch64")]
const NUMBER_REGISTER: usize = 11;

#[derive(Clone, Copy, Debug)]
pub(crate) struct Request {
    pub(crate) pc: usize,
    pub(crate) number: usize,
    pub(crate) arguments: [usize; 6],
}

impl Request {
    /// 在进入可阻塞分派之前保存原 PC、编号及六个 raw 参数，不再读取硬件 CSR。
    pub(crate) fn snapshot(context: &UserContext) -> Self {
        Self {
            pc: context.pc,
            number: context.gpr[NUMBER_REGISTER],
            arguments: core::array::from_fn(|index| context.gpr[ARGUMENT_REGISTER + index]),
        }
    }

    /// 只在普通返回时写 a0 并越过原 syscall；fault、中断和其他控制转移不调用此尾部。
    pub(crate) fn return_to(self, context: &mut UserContext, result: isize) {
        context.gpr[ARGUMENT_REGISTER] = result as usize;
        context.pc = self.pc.checked_add(4).expect("syscall PC overflow");
    }

    /// 重放仅由内部restart结果授权的原始调用，恢复参数与原PC而不推断用户指令。
    pub(crate) fn restart_to(self, context: &mut UserContext, restart_block: bool) {
        context.gpr[ARGUMENT_REGISTER..ARGUMENT_REGISTER + 6].copy_from_slice(&self.arguments);
        context.gpr[NUMBER_REGISTER] = if restart_block { 128 } else { self.number };
        context.pc = self.pc;
    }
}

#[derive(Clone, Copy)]
pub(crate) enum Restart {
    Syscall {
        request: Request,
    },
    NoHandler {
        request: Request,
    },
    Sleep {
        request: Request,
        deadline: crate::time::MonotonicInstant,
        remaining: Option<usize>,
    },
    Futex {
        request: Request,
        deadline: crate::time::MonotonicInstant,
    },
}

pub(crate) enum Outcome {
    Return(isize),
    Exit(i32),
    GroupExit(i32),
    Restart(Restart),
    Restored,
    NewImage,
}

/// 信号选择后依据已选handler策略决定EINTR或重启，剩余时间交付先于现场借用。
pub(crate) fn resolve_restart(user: &UserThread, state: Restart, handler_flags: Option<usize>) {
    let (request, restart) = match state {
        Restart::Syscall { request } => (
            request,
            handler_flags.is_none_or(|flags| flags & crate::task::signal::SA_RESTART != 0),
        ),
        Restart::NoHandler { request }
        | Restart::Sleep { request, .. }
        | Restart::Futex { request, .. } => (request, handler_flags.is_none()),
    };
    if restart {
        if matches!(state, Restart::NoHandler { .. }) {
            user.restore_saved_mask();
        }
        let block = matches!(state, Restart::Sleep { .. } | Restart::Futex { .. });
        if block {
            user.save_restart(state);
        }
        request.restart_to(&mut user.context(), block);
    } else {
        let result = match state {
            Restart::Sleep {
                deadline,
                remaining,
                ..
            } => time::interrupt_sleep(user, deadline, remaining).unwrap_or_else(Errno::result),
            _ => Errno::Interrupted.result(),
        };
        request.return_to(&mut user.context(), result);
    }
}

/// 分派真实调用；无进度I/O和ioctl中断交由用户返回裁决重启，未知编号保持ENOSYS。
pub(crate) fn dispatch(user: &UserThread, request: Request) -> Outcome {
    let raw = request.arguments;
    if matches!(request.number, 20..=22 | 72 | 73) {
        return poll::dispatch(user, request);
    }
    if request.number == 98 {
        return futex::dispatch(user, request);
    }
    if matches!(request.number, 101..=103 | 113..=115 | 153 | 165 | 169) {
        return time::dispatch(user, request);
    }
    if request.number == 128 {
        return match user.take_restart() {
            Some(Restart::Sleep {
                request,
                deadline,
                remaining,
            }) => time::resume_sleep(user, request, deadline, remaining),
            Some(Restart::Futex { request, deadline }) => {
                futex::resume_wait(user, request, deadline)
            }
            Some(Restart::Syscall { .. } | Restart::NoHandler { .. }) | None => {
                Outcome::Return(Errno::Interrupted.result())
            }
        };
    }
    if matches!(request.number, 129..=139 | 240) {
        return signal::dispatch(user, request);
    }
    if matches!(request.number, 29 | 63..=66) {
        let result = match request.number {
            29 => file::ioctl(user, raw),
            63 => file::read(user, raw),
            64 => file::write(user, raw),
            65 => file::readv(user, raw),
            66 => file::writev(user, raw),
            _ => unreachable!(),
        };
        return match result {
            Err(Errno::Interrupted) => Outcome::Restart(Restart::Syscall { request }),
            other => Outcome::Return(other.unwrap_or_else(Errno::result)),
        };
    }
    let result: SysResult = match request.number {
        93 => return Outcome::Exit(((raw[0] & 255) << 8) as i32),
        94 => return Outcome::GroupExit(((raw[0] & 255) << 8) as i32),
        17 => file::getcwd(user, raw),
        23 => file::dup(user, raw[0]),
        24 => file::dup3(user, raw),
        25 => file::fcntl(user, raw),
        34 => file::mkdirat(user, raw),
        35 => file::unlinkat(user, raw),
        36 => file::symlinkat(user, raw),
        37 => file::linkat(user, raw),
        39 => system::umount(user, &raw),
        40 => system::mount(user, &raw),
        45 => file::truncate(user, raw),
        46 => file::ftruncate(user, raw),
        49 => file::chdir(user, raw[0]),
        50 => file::fchdir(user, raw),
        56 => file::openat(user, raw),
        57 => file::close(user, raw[0]),
        59 => file::pipe2(user, raw),
        61 => file::getdents(user, raw),
        62 => file::seek(user, raw),
        67 => file::pread(user, raw),
        68 => file::pwrite(user, raw),
        78 => file::readlink(user, raw),
        79 => file::statat(user, raw),
        80 => file::fstat(user, raw),
        81 => system::sync(user),
        82 => file::fsync(user, raw[0]),
        83 => file::fdatasync(user, raw[0]),
        166 => file::umask(user, raw[0]),
        95 => return task::waitid(user, request),
        96 => Ok(user.set_tid_address(raw[0]) as isize),
        154..=157 | 173..=177 => task::identity(user, request.number, raw),
        124 => {
            sched::yield_now();
            Ok(0)
        }
        172 | 178 => {
            let identity = user.identity().expect("syscall has no user identity");
            Ok(if request.number == 172 {
                identity.tgid()
            } else {
                identity.tid()
            } as isize)
        }
        214 => Ok(memory::brk(user, raw[0])),
        215 => memory::munmap(user, raw),
        220 => task::clone(user, request),
        221 => return task::execve(user, request),
        222 => memory::mmap(user, raw),
        226 => memory::mprotect(user, raw),
        227 => memory::msync(user, raw),
        260 => return task::wait4(user, request),
        276 => file::renameat2(user, raw),
        291 => file::statx(user, raw),
        _ => Err(Errno::NoSyscall),
    };
    Outcome::Return(result.unwrap_or_else(Errno::result))
}
