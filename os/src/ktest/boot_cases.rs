//! 启动自检的独立实例；早期固件与映射检查不使用 MM 或 Task。

mod calls;
mod fatal;
mod firmware;
mod hardware;
mod uart;

/// 保留真实注册槽锚点，具体目录仍由链接节发现。
pub(crate) fn anchors() -> [&'static ::ktest::SuiteSlot; 8] {
    [
        firmware::cases::__ktest_anchor(),
        hardware::mapping::__ktest_anchor(),
        hardware::cpus::__ktest_anchor(),
        #[cfg(target_arch = "riscv64")]
        hardware::rv_fp::__ktest_anchor(),
        #[cfg(target_arch = "loongarch64")]
        hardware::fcc::cases::__ktest_anchor(),
        calls::cases::__ktest_anchor(),
        uart::cases::__ktest_anchor(),
        fatal::traps::__ktest_anchor(),
        fatal::startup::__ktest_anchor(),
    ]
}
