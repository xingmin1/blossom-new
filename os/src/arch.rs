//! 编译目标是唯一的架构选择来源，公共代码不再选择板型 feature。

#[cfg(target_arch = "riscv64")]
mod riscv64;
#[cfg(target_arch = "riscv64")]
pub use riscv64::*;

#[cfg(target_arch = "loongarch64")]
mod loongarch64;
#[cfg(target_arch = "loongarch64")]
pub use loongarch64::*;

#[cfg(target_arch = "loongarch64")]
use loongarch64 as dma_backend;
#[cfg(target_arch = "riscv64")]
use riscv64 as dma_backend;

/// 用户缺页所请求的硬件访问方向，各后端独立解释自己的异常编号。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UserAccess {
    Read,
    Write,
    Execute,
}

/// 用户现场已完整保存并返回原 Task 栈后的事件快照。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UserTrap {
    Syscall,
    Interrupt,
    PageFault { address: usize, access: UserAccess },
    Exception { code: usize, address: usize },
}

/// 在 coherent DMA 索引发表前排序描述符和 payload 写入，保留编译器内存副作用。
pub fn dma_write_barrier() {
    dma_backend::dma_write_barrier();
}

/// 在观察真实设备完成后排序 coherent DMA 数据读取。
pub fn dma_read_barrier() {
    dma_backend::dma_read_barrier();
}

/// 排序 coherent DMA 内存与 MMIO 通知、设备状态之间的双向访问。
pub fn dma_barrier() {
    dma_backend::dma_barrier();
}

#[cfg(not(any(target_arch = "riscv64", target_arch = "loongarch64")))]
compile_error!("kernel supports only riscv64 and loongarch64 bare-metal targets");
