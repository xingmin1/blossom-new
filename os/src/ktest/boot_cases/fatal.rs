//! 真正硬件trap与AP上线失败各自独立VM；类型由实际错误控制者报告。

use crate::ktest::panic::Boundary;
use crate::{arch, boot, cpu, mm};
use ::ktest::TestContext;

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
    /// 独立冻结AP的终结性Boot动作独占建立MM和动态栈，再等待生产启核五秒超时；
    /// 建立MM后不得返回普通Boot分派，意外上线同样以未捕获panic终结本VM。
    #[test]
    #[ktest(expected_fatal = "panic", fatal_message = "AP online timed out")]
    fn frozen_ap(context: &mut TestContext<'_>) {
        assert_eq!(cpu::current().id, 0);
        let _uncaught = UncaughtBoot::enter();
        {
            let _irq = crate::sync::IrqSaveGuard::new();
            // SAFETY: 本例独占一个新VM，普通Boot尚未建立MM；CPU0唯一执行且IRQ已关闭，
            // 所有AP未释放。实际BootInfo完整保留内核、BSP栈、页表和固件区域，最终RAM
            // 直映永久可写且一一对应；空闲RAM无其他使用者，发布的metadata不再别名修改或归还。
            unsafe { mm::initialize(boot::info()) };
            // SAFETY: CPU表已建立，CPU0仍独占启动、IRQ关闭，MM的buddy/PCP/SLUB均已初始化；
            // 全部AP从未交给固件，无并发准备或栈访问；成功页随后只交给原生产启核路径。
            unsafe { cpu::prepare_secondary_stacks() }.expect("AP stack preparation failed");
        }
        for id in 1..cpu::count() {
            assert!(!cpu::get(id)
                .online
                .load(core::sync::atomic::Ordering::Acquire));
            context.diagnostic(format_args!(
                "frozen-ap before id={} hardware={} stack={:#x} local={:#x}",
                id,
                cpu::get(id).hardware_id,
                cpu::get(id).stack_top(),
                cpu::get(id) as *const _ as usize
            ));
        }
        // SAFETY: 实际BootInfo/CpuLocal已永久发布，MM和每个AP的独占动态启动栈均已准备；
        // 只有CPU0运行，所有AP尚未释放，未持有目标初始化所需锁；host保证目标在进入
        // Rust初始化前自跳，不会向BSP发同步CSD。生产超时保留目标栈，未捕获panic终结VM。
        unsafe { cpu::start_secondaries() };
        panic!("frozen AP unexpectedly came online");
    }
}
