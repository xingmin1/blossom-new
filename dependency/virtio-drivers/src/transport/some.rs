//! 两种实际设备协议的唯一分派类型。

use super::{
    mmio::MmioTransport, pci::PciTransport, DeviceStatus, DeviceType, InterruptStatus,
    Notification, Transport,
};
use crate::{DmaAddr, Result};
use zerocopy::{FromBytes, Immutable, IntoBytes};

/// 已选设备的 MMIO 或现代 PCI 访问者。
#[derive(Debug)]
pub enum SomeTransport<'a> {
    /// 真实 version1/2 MMIO 协议。
    Mmio(MmioTransport<'a>),
    /// 真实现代 PCI capability 协议。
    Pci(PciTransport),
}

impl<'a> From<MmioTransport<'a>> for SomeTransport<'a> {
    /// 移动唯一 MMIO 访问者，不复制寄存器拥有关系。
    fn from(mmio: MmioTransport<'a>) -> Self {
        Self::Mmio(mmio)
    }
}
impl From<PciTransport> for SomeTransport<'_> {
    /// 移动唯一 PCI 访问者，不保存额外配置 owner。
    fn from(pci: PciTransport) -> Self {
        Self::Pci(pci)
    }
}

impl Transport for SomeTransport<'_> {
    /// 返回选定设备类型。
    fn device_type(&self) -> DeviceType {
        match self {
            Self::Mmio(t) => t.device_type(),
            Self::Pci(t) => t.device_type(),
        }
    }
    /// 读取完整功能位。
    fn read_device_features(&mut self) -> u64 {
        match self {
            Self::Mmio(t) => t.read_device_features(),
            Self::Pci(t) => t.read_device_features(),
        }
    }
    /// 写入协商功能位。
    fn write_driver_features(&mut self, features: u64) {
        match self {
            Self::Mmio(t) => t.write_driver_features(features),
            Self::Pci(t) => t.write_driver_features(features),
        }
    }
    /// 读取选定队列容量。
    fn max_queue_size(&mut self, queue: u16) -> u32 {
        match self {
            Self::Mmio(t) => t.max_queue_size(queue),
            Self::Pci(t) => t.max_queue_size(queue),
        }
    }
    /// 准备可锁外使用的独立 doorbell。
    fn notification(&mut self, queue: u16) -> Result<Notification> {
        match self {
            Self::Mmio(t) => t.notification(queue),
            Self::Pci(t) => t.notification(queue),
        }
    }
    /// 读取真实硬件状态。
    fn get_status(&self) -> DeviceStatus {
        match self {
            Self::Mmio(t) => t.get_status(),
            Self::Pci(t) => t.get_status(),
        }
    }
    /// 写入真实硬件状态。
    fn set_status(&mut self, status: DeviceStatus) {
        match self {
            Self::Mmio(t) => t.set_status(status),
            Self::Pci(t) => t.set_status(status),
        }
    }
    /// 仅适用 legacy MMIO 的页大小协议由具体后端处理。
    fn set_guest_page_size(&mut self, size: u32) {
        match self {
            Self::Mmio(t) => t.set_guest_page_size(size),
            Self::Pci(t) => t.set_guest_page_size(size),
        }
    }
    /// 返回真实队列布局要求。
    fn requires_legacy_layout(&self) -> bool {
        match self {
            Self::Mmio(t) => t.requires_legacy_layout(),
            Self::Pci(t) => t.requires_legacy_layout(),
        }
    }
    /// 发表真实地址并保留后端失败原因。
    fn queue_set(
        &mut self,
        queue: u16,
        size: u32,
        descriptors: DmaAddr,
        driver_area: DmaAddr,
        device_area: DmaAddr,
    ) -> Result<()> {
        match self {
            Self::Mmio(t) => t.queue_set(queue, size, descriptors, driver_area, device_area),
            Self::Pci(t) => t.queue_set(queue, size, descriptors, driver_area, device_area),
        }
    }
    /// 查询硬件队列是否启用。
    fn queue_used(&mut self, queue: u16) -> bool {
        match self {
            Self::Mmio(t) => t.queue_used(queue),
            Self::Pci(t) => t.queue_used(queue),
        }
    }
    /// 读取并确认本设备中断原因，不吞掉共享线的其他设备。
    fn ack_interrupt(&mut self) -> InterruptStatus {
        match self {
            Self::Mmio(t) => t.ack_interrupt(),
            Self::Pci(t) => t.ack_interrupt(),
        }
    }
    /// 读取真实配置 generation。
    fn read_config_generation(&self) -> u32 {
        match self {
            Self::Mmio(t) => t.read_config_generation(),
            Self::Pci(t) => t.read_config_generation(),
        }
    }
    /// 有界读取选定设备配置。
    fn read_config_space<T: FromBytes + IntoBytes>(&self, offset: usize) -> Result<T> {
        match self {
            Self::Mmio(t) => t.read_config_space(offset),
            Self::Pci(t) => t.read_config_space(offset),
        }
    }
    /// 有界写入选定设备配置。
    fn write_config_space<T: IntoBytes + Immutable>(
        &mut self,
        offset: usize,
        value: T,
    ) -> Result<()> {
        match self {
            Self::Mmio(t) => t.write_config_space(offset, value),
            Self::Pci(t) => t.write_config_space(offset, value),
        }
    }
}
