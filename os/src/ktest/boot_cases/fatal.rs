//! 真正硬件trap与AP上线失败各自独立VM；类型由实际错误控制者报告。

use crate::ktest::panic::Boundary;
use crate::{arch, cpu};
use ::ktest::{TestContext, TestError, TestResult};

struct UncaughtBoot {
    previous: *mut Boundary,
    irq: usize,
}
impl UncaughtBoot {
    /// 借用外层仍Pin保活的Boot边界，临时禁用可恢复catch，不改变case的armed身份。
    fn enter() -> Self {
        let irq = arch::irq_save();
        // SAFETY: 当前为BSP Boot独占栈，外层框架Boundary处于Pin且一直保活到dispatch返回；
        // 只改同一Boot槽，保留原指针，在任何正常返回路径由本guard恢复。
        let previous = unsafe { crate::ktest::replace_boundary(core::ptr::null_mut()) };
        arch::enable_interrupts();
        Self { previous, irq }
    }
}
impl Drop for UncaughtBoot {
    /// 若真实失败未发生，先关IRQ恢复同一稳定边界，再恢复原始IRQ状态。
    fn drop(&mut self) {
        arch::irq_save();
        // SAFETY: 原外层Boot边界仍Pin保活，guard未迁核；恢复槽先于外层catch继续，
        // AP即使意外上线也不能访问BSP的Boot boundary，保存的IRQ值属于同核。
        unsafe {
            crate::ktest::replace_boundary(self.previous);
            arch::irq_restore(self.irq);
        }
    }
}

#[cfg(target_arch = "riscv64")]
#[::ktest::suite(
    context = "Boot",
    isolation = "Case",
    tags("boot", "fatal", "trap"),
    timeout_ms = 1000
)]
pub(crate) mod traps {
    use super::*;
    /// 非法指令直接进入真实trap，host必须核PC/SP非零和scause=2。
    #[test]
    #[ktest(expected_fatal = "illegal")]
    fn illegal_instruction() {
        arch::trigger_fault();
    }
}

#[cfg(target_arch = "loongarch64")]
#[::ktest::suite(
    context = "Boot",
    isolation = "Case",
    tags("boot", "fatal", "trap"),
    timeout_ms = 1000
)]
pub(crate) mod traps {
    use super::*;
    /// 非法指令直接进入真实trap，host必须核PC/SP非零和ESTAT.Ecode=13。
    #[test]
    #[ktest(expected_fatal = "illegal")]
    fn illegal_instruction() {
        arch::trigger_fault();
    }

    /// DMW之外0x1000真实load进入refill专用trap，host核IsTLBR=1/TLBRBADV=0x1000。
    #[test]
    #[ktest(expected_fatal = "refill")]
    fn unmapped_refill() {
        arch::trigger_unmapped_fault();
    }
}

#[::ktest::suite(
    context = "Boot",
    isolation = "Case",
    tags("boot", "fatal", "ap-startup"),
    resources("ap-freeze"),
    min_cpus = 2,
    timeout_ms = 10000
)]
pub(crate) mod startup {
    use super::*;
    /// host仅修改本VM的AP首条指令为自跳，真实启动协议必须五秒超时且不释放启动记录。
    #[test]
    #[ktest(expected_fatal = "panic", fatal_message = "AP online timed out")]
    fn frozen_ap(context: &mut TestContext<'_>) -> TestResult {
        assert_eq!(cpu::current().id, 0);
        for id in 1..cpu::count() {
            assert!(!cpu::get(id)
                .online
                .load(core::sync::atomic::Ordering::Acquire));
            context.diagnostic(format_args!(
                "frozen-ap before id={} hardware={} stack={:#x} local={:#x}",
                id,
                cpu::get(id).hardware_id,
                cpu::get(id).stack_top,
                cpu::get(id) as *const _ as usize
            ));
        }
        let _uncaught = UncaughtBoot::enter();
        // SAFETY: BootInfo/CpuLocal/永久独立启动栈已完成，只有BSP启动；所有AP尚未释放，
        // 当前BSP可服务IPI且不持有call需要的锁。host控制只冻结本VM的真实AP入口，
        // 超时路径不归还记录/栈、不降核，最终panic在无catch边界的真实控制者终结整VM。
        unsafe { cpu::start_secondaries() };
        Err(TestError("frozen AP unexpectedly came online"))
    }
}
