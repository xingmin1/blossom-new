//! 每次调用只读取一次用户寄存器；返回值与退出是互斥控制结果，阻塞时不借用户现场。

mod error;
mod file;
mod memory;
mod task;

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
}

pub(crate) enum Outcome {
    Return(isize),
    Exit(i32),
    GroupExit(i32),
}

/// 分派已支持的真实调用，未知编号保持ENOSYS；控制结果不落入普通写寄存器尾部。
pub(crate) fn dispatch(user: &UserThread, request: Request) -> Outcome {
    let raw = request.arguments;
    let result: SysResult = match request.number {
        93 => return Outcome::Exit(((raw[0] & 255) << 8) as i32),
        94 => return Outcome::GroupExit(((raw[0] & 255) << 8) as i32),
        23 => file::dup(user, raw[0]),
        24 => file::dup3(user, raw),
        25 => file::fcntl(user, raw),
        49 => file::chdir(user, raw[0]),
        56 => file::openat(user, raw),
        57 => file::close(user, raw[0]),
        61 => file::getdents(user, raw),
        62 => file::seek(user, raw),
        63 => file::read(user, raw),
        64 | 68 => file::write(user, raw),
        67 => file::pread(user, raw),
        78 => file::readlink(user, raw),
        79 => file::statat(user, raw),
        80 => file::fstat(user, raw),
        124 => {
            sched::yield_now();
            Ok(0)
        }
        134 => task::sigaction(user, raw),
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
        222 => memory::mmap(user, raw),
        226 => memory::mprotect(user, raw),
        _ => Err(Errno::NoSyscall),
    };
    Outcome::Return(result.unwrap_or_else(Errno::result))
}
