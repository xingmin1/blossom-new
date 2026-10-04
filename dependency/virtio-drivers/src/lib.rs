//! 连贯 DMA 与已映射 SG 上的 VirtIO split-ring 块设备后端。
//!
//! 内核拥有控制页及 payload 映射，并在普通任务、队列锁外准备所有资源。
//! [`Hal`] 只提供平台设备屏障；[`queue::VirtQueue`] 的 unsafe 提交入口
//! 接受 [`MappedSegment`]，不隐式分配、映射或共享 CPU slice。
//! 队列构造不发表硬件地址，设备停止与 DMA 页归还由实际资源拥有者负责。
//! 无 `alloc` 时仅提供平台屏障、设备地址和原始 DMA 视图，完整后端显式要求 `alloc`。

#![cfg_attr(not(test), no_std)]
#![deny(unused_must_use, missing_docs, clippy::undocumented_unsafe_blocks)]

#[cfg(any(feature = "alloc", test))]
extern crate alloc;

pub mod device;
mod hal;
#[cfg(feature = "alloc")]
pub mod queue;
#[cfg(feature = "alloc")]
pub mod transport;

use thiserror::Error;

pub use crate::hal::{BufferDirection, DmaAddr, DmaRegion, Hal, MappedSegment};
pub use safe_mmio::UniqueMmioPointer;

/// 协议 legacy ring 使用的页对齐字节数。
pub const PAGE_SIZE: usize = 0x1000;

/// 库协议操作的错误返回。
pub type Result<T = ()> = core::result::Result<T, Error>;

/// 实际块设备及 transport 操作可能遇到的错误。
#[derive(Copy, Clone, Debug, Eq, Error, PartialEq)]
pub enum Error {
    /// 当前 descriptor 额度不足，调用者保留未提交请求。
    #[error("Virtqueue is full")]
    QueueFull,
    /// 设备或预备资源尚未就绪。
    #[error("Device not ready")]
    NotReady,
    /// used id 越界、不是已提交的链头，或重复完成。
    #[error("Device returned an invalid descriptor head")]
    WrongToken,
    /// 队列已被设备使用，不能再次绑定。
    #[error("Virtqueue is already in use")]
    AlreadyUsed,
    /// 参数范围、地址或协议布局不合法。
    #[error("Invalid parameter")]
    InvalidParam,
    /// 普通任务预备资源时分配失败。
    #[error("Memory allocation failed")]
    NoMemory,
    /// 设备报告 I/O 失败或非法完成状态。
    #[error("I/O error")]
    IoError,
    /// 设备不支持该请求或协议能力。
    #[error("Request not supported by device")]
    Unsupported,
    /// 实际配置窗口不足以读取声明的字段。
    #[error("Config space advertised by the device is smaller than expected")]
    ConfigSpaceTooSmall,
    /// 设备缺少所需的配置窗口。
    #[error("The device does not have config space")]
    ConfigSpaceMissing,
    /// 设备拒绝 FEATURES_OK 或缺少现代协议必需能力。
    #[error("Device rejected negotiated features")]
    FeaturesRejected,
    /// 设备尚未确认 reset，不能归还 DMA 或开始初始化。
    #[error("Device reset is pending")]
    ResetPending,
    /// 设备故障或协议状态不可信，拥有者须先停止 DMA。
    #[error("Device needs reset")]
    DeviceNeedsReset,
}
