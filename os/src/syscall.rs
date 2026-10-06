//! 每次调用只读取一次用户寄存器；返回值与退出是互斥控制结果，阻塞时不借用户现场。

use crate::arch::UserContext;
use crate::sched;
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
}

/// 分派真实 Linux 编号；让出后仍回到原 Task，未知请求返回 ENOSYS。
pub(crate) fn dispatch(user: &UserThread, request: Request) -> Outcome {
    let result = match request.number {
        93 => return Outcome::Exit(((request.arguments[0] & 255) << 8) as i32),
        124 => {
            sched::yield_now();
            0
        }
        172 | 178 => {
            let identity = user.identity().expect("syscall has no user identity");
            if request.number == 172 {
                identity.tgid() as isize
            } else {
                identity.tid() as isize
            }
        }
        _ => -38,
    };
    Outcome::Return(result)
}
