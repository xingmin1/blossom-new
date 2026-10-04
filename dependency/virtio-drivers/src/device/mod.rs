//! 当前编译的 VirtIO 设备协议实现。

/// 已映射 SG 与稳定 PDU 上的块设备协议；需要构造期 fallible shadow。
#[cfg(feature = "alloc")]
pub mod blk;
