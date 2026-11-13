//! LoongArch64 浮点条件标志寄存器（`FCC0`..`FCC7`）全位图跨时钟中断保存恢复测试。
//!
//! # 模块职责与架构定位
//! 本模块隶属于启动与硬件自检子系统（`boot_cases::hardware`），仅在 `loongarch64` 架构下编译：
//! - **上游调用者**：由内核测试框架（`ktest`）在 `Thread` 阶段运行单个用例，该用例使用一个 [`CpuFixture`]。
//! - **下游依赖**：调用父模块的 [`every_cpu`] 与 [`registers`]，最终在每个在线 CPU 上执行 `arch::ktest_register_pattern(true, fcc)` 触发硬件时钟中断并校验寄存器现场。
//!
//! # 核心设计与寄存器位图布局
//! LoongArch64 浮点比较指令的结果存放在 8 个独立的 1 位条件标志寄存器 `FCC0`..`FCC7`（合称 `CF`）中。
//! 内核陷阱入口（`la_kernel_trap`）在开启浮点单元（`CSR_EUEN.FPE == 1`）时，必须将全部 8 个条件位连同通用浮点寄存器（`$f0`..`$f31`）与控制状态寄存器（`FCSR0`）一并保存并在中断返回前完整恢复。
//!
//! 测试图样 `fcc: u8` 的 8 个比特与硬件 `FCC` 寄存器的对应关系如下：
//! ```text
//!   bit:    7      6      5      4      3      2      1      0
//!        +------+------+------+------+------+------+------+------+
//!   fcc: | FCC7 | FCC6 | FCC5 | FCC4 | FCC3 | FCC2 | FCC1 | FCC0 |
//!        +------+------+------+------+------+------+------+------+
//!        |<------------ 0x00 ..= 0xff 共 256 种位图组合 --------->|
//! ```
//! 穷举全部 $2^8 = 256$ 种组合，可发现保存/恢复汇编中任何单比特漏存、移位错位、掩码截断或相邻标志位串扰缺陷。
//!
//! # 锁获取顺序与并发契约
//! - 本模块自身不定义也不获取自旋锁或睡眠锁。
//! - 该用例通过 [`CpuFixture`] 在所有在线逻辑 CPU 上各派生一个绑定核的探测子任务，
//!   每个子任务依次遍历 `0x00..=0xff`。每个图样都通过 `PreemptGuard` 禁止跨核迁移，并通过 `TimerRestore` 守卫恢复系统时钟事件。
use super::*;

#[::ktest::suite(context = "Thread", fixture = CpuFixture, tags("boot", "cpu", "fcc"), timeout_ms = 30000)]
pub(crate) mod cases {
    use super::*;
    /// 在一次 Thread 测试中，让每个在线 CPU 依次检查全部 256 种 FCC 位图。
    ///
    /// 每种位图均触发真实时钟中断，并沿原有断言检查中断计数及整数和浮点寄存器现场；
    /// 子任务收到停止信号时报告失败，避免将未完成的位图检查视为成功。
    #[test]
    fn full_fcc(context: &mut TestContext<'_>, fixture: &mut CpuFixture) -> TestResult {
        every_cpu(
            context,
            fixture,
            |target, fp, _, stop| {
                for fcc in u8::MIN..=u8::MAX {
                    if stop.load(Ordering::Acquire) {
                        return Err(TestError("FCC probe stopped"));
                    }
                    registers(target, fp, fcc, stop)?;
                }
                Ok(())
            },
            true,
            0,
        )
    }
}
