//! 编译目标是唯一的架构选择来源，公共代码不再选择板型 feature。

#[cfg(target_arch = "riscv64")]
mod riscv64;
#[cfg(target_arch = "riscv64")]
pub use riscv64::*;

#[cfg(target_arch = "loongarch64")]
mod loongarch64;
#[cfg(target_arch = "loongarch64")]
pub use loongarch64::*;

#[cfg(not(any(target_arch = "riscv64", target_arch = "loongarch64")))]
compile_error!("内核只支持 riscv64 和 loongarch64 裸机目标");
