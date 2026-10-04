//! 仅消费已选定、已映射 capability 的现代 PCI 传输。

pub mod bus;

use self::bus::{DeviceFunction, DeviceFunctionInfo, PciError};
use super::{
    read_config, validate_queue_addresses, write_config, DeviceStatus, DeviceType, InterruptStatus,
    Notification, Transport,
};
use crate::{DmaAddr, Error};
use core::ptr::{self, NonNull};
use zerocopy::{FromBytes, Immutable, IntoBytes};

/// VirtIO 协议分配的 PCI vendor 身份。
pub const VIRTIO_VENDOR_ID: u16 = 0x1af4;
/// 公共功能、状态和队列配置窗口。
pub const VIRTIO_PCI_CAP_COMMON_CFG: u8 = 1;
/// 队列 doorbell 窗口。
pub const VIRTIO_PCI_CAP_NOTIFY_CFG: u8 = 2;
/// 单字节 read-to-clear 中断窗口。
pub const VIRTIO_PCI_CAP_ISR_CFG: u8 = 3;
/// 设备特定配置窗口。
pub const VIRTIO_PCI_CAP_DEVICE_CFG: u8 = 4;

/// 从已识别的 PCI 设备 ID 得到协议设备类型。
fn device_type(id: u16) -> Option<DeviceType> {
    match id {
        0x1000 => Some(DeviceType::Network),
        0x1001 => Some(DeviceType::Block),
        0x1002 => Some(DeviceType::MemoryBalloon),
        0x1003 => Some(DeviceType::Console),
        0x1004 => Some(DeviceType::ScsiHost),
        0x1005 => Some(DeviceType::EntropySource),
        0x1009 => Some(DeviceType::_9P),
        id if 0x1040 <= id => DeviceType::try_from(id - 0x1040).ok(),
        _ => None,
    }
}

/// 识别库存中的 VirtIO 身份，不读取配置窗口。
pub fn virtio_device_type(info: &DeviceFunctionInfo) -> Option<DeviceType> {
    if info.vendor_id == VIRTIO_VENDOR_ID {
        device_type(info.device_id)
    } else {
        None
    }
}

/// 已映射 capability 的完整 CPU 窗口；不拥有映射。
#[derive(Clone, Copy, Debug)]
pub struct PciRegion {
    /// capability 已映射的强序 CPU 首地址。
    pub base: NonNull<u8>,
    /// capability 完整窗口的实际字节长度。
    pub len: usize,
}

/// 一次库存选择后的构造数据；不得另行扫描或探测 BAR。
#[derive(Debug)]
pub struct SelectedConstruct {
    /// 第一批库存保存的完整 BDF。
    pub device_function: DeviceFunction,
    /// 已选功能真实 vendor ID。
    pub vendor_id: u16,
    /// 已选功能真实 device ID。
    pub device_id: u16,
    /// 包含完整现代公共配置结构的窗口。
    pub common_cfg: PciRegion,
    /// 包含队列 doorbell 的窗口。
    pub notify_cfg: PciRegion,
    /// capability 原始 notify_off_multiplier，单位为字节。
    pub notify_off_multiplier: u32,
    /// 独立 ISR read-to-clear 字节窗口。
    pub isr_cfg: PciRegion,
    /// 可缺省的设备特定配置窗口，访问时仍核对字段范围。
    pub device_cfg: Option<PciRegion>,
}

/// 现代 PCI 寄存器访问者；设备/DMA 页归内核 owner。
#[derive(Debug)]
pub struct PciTransport {
    device_type: DeviceType,
    device_function: DeviceFunction,
    common_cfg: PciRegion,
    notify_cfg: PciRegion,
    notify_off_multiplier: u32,
    isr_cfg: PciRegion,
    config_space: Option<PciRegion>,
}

impl PciRegion {
    /// 核对完整窗口、最小长度、首地址对齐及末端算术。
    fn validate(self, minimum: usize, alignment: usize) -> Result<(), VirtioPciError> {
        let start = self.base.as_ptr() as usize;
        if self.len < minimum || start.checked_add(self.len).is_none() {
            return Err(VirtioPciError::BarOffsetOutOfRange);
        }
        if start % alignment != 0 {
            return Err(VirtioPciError::Misaligned {
                address: start,
                alignment,
            });
        }
        Ok(())
    }

    /// 核对两个已检查窗口是否存在字节重叠。
    fn overlaps(self, other: Self) -> bool {
        let start = self.base.as_ptr() as usize;
        let other_start = other.base.as_ptr() as usize;
        start < other_start + other.len && other_start < start + self.len
    }
}

impl PciTransport {
    /// 从库存已选 capability 创建现代 PCI 协议访问者。
    ///
    /// # Safety
    /// 每个窗口必须来自同一真实设备的已分配 memory BAR，完整字节须永久强序映射；
    /// 调用者必须独占此设备状态和队列选择，且保证设备在所有视图访问期间有效。
    pub unsafe fn new(selected: SelectedConstruct) -> Result<Self, VirtioPciError> {
        if selected.vendor_id != VIRTIO_VENDOR_ID {
            return Err(VirtioPciError::InvalidVendorId(selected.vendor_id));
        }
        let device_type = device_type(selected.device_id)
            .ok_or(VirtioPciError::InvalidDeviceId(selected.device_id))?;
        selected.common_cfg.validate(56, 4)?;
        selected.notify_cfg.validate(2, 2)?;
        selected.isr_cfg.validate(1, 1)?;
        if selected.notify_off_multiplier % 2 != 0 {
            return Err(VirtioPciError::InvalidNotifyOffMultiplier(
                selected.notify_off_multiplier,
            ));
        }
        if let Some(config) = selected.device_cfg {
            config.validate(1, 4)?;
        }
        let regions = [
            Some(selected.common_cfg),
            Some(selected.notify_cfg),
            Some(selected.isr_cfg),
            selected.device_cfg,
        ];
        for (index, region) in regions.iter().enumerate() {
            if let Some(region) = region {
                if regions[index + 1..]
                    .iter()
                    .flatten()
                    .any(|other| region.overlaps(*other))
                {
                    return Err(VirtioPciError::OverlappingCapabilities);
                }
            }
        }
        Ok(Self {
            device_type,
            device_function: selected.device_function,
            common_cfg: selected.common_cfg,
            notify_cfg: selected.notify_cfg,
            notify_off_multiplier: selected.notify_off_multiplier,
            isr_cfg: selected.isr_cfg,
            config_space: selected.device_cfg,
        })
    }

    /// 返回完整 BDF，身份不依赖可变全局当前设备。
    pub fn device_function(&self) -> DeviceFunction {
        self.device_function
    }

    /// 读取 common 字节状态；偏移固定在构造已检查的 56 字节内。
    fn read8(&self, offset: usize) -> u8 {
        // SAFETY: 所有调用点只传 common ABI 的有效字节偏移，映射寿命由构造者保证。
        unsafe { ptr::read_volatile(self.common_cfg.base.as_ptr().add(offset)) }
    }
    /// 读取对齐的 common 半字字段。
    fn read16(&self, offset: usize) -> u16 {
        // SAFETY: 所有调用点偏移为 common 内对齐半字，构造检查首地址四字节对齐。
        unsafe {
            u16::from_le(ptr::read_volatile(
                self.common_cfg.base.as_ptr().add(offset).cast::<u16>(),
            ))
        }
    }
    /// 读取对齐的 common 字段。
    fn read32(&self, offset: usize) -> u32 {
        // SAFETY: 所有调用点偏移为 common 内对齐字，完整寄存器位于映射范围。
        unsafe {
            u32::from_le(ptr::read_volatile(
                self.common_cfg.base.as_ptr().add(offset).cast::<u32>(),
            ))
        }
    }
    /// 写入 common 字节状态。
    fn write8(&mut self, offset: usize, value: u8) {
        // SAFETY: 调用点只写有效 status 字节，独占 transport 串行化状态转换。
        unsafe { ptr::write_volatile(self.common_cfg.base.as_ptr().add(offset), value) }
    }
    /// 写入对齐的 common 半字控制字段。
    fn write16(&mut self, offset: usize, value: u16) {
        // SAFETY: 所有调用点只写 common 内对齐控制字段；独占引用串行化队列选择。
        unsafe {
            ptr::write_volatile(
                self.common_cfg.base.as_ptr().add(offset).cast::<u16>(),
                value.to_le(),
            )
        }
    }
    /// 写入对齐的 common 字控制字段。
    fn write32(&mut self, offset: usize, value: u32) {
        // SAFETY: 所有调用点只写 common 内对齐字；不会创建普通内存引用。
        unsafe {
            ptr::write_volatile(
                self.common_cfg.base.as_ptr().add(offset).cast::<u32>(),
                value.to_le(),
            )
        }
    }
    /// 以两个协议支持的 32 位访问写 DMA 地址，避免依赖 64 位 MMIO 原子访问。
    fn write_address(&mut self, offset: usize, address: DmaAddr) {
        self.write32(offset, address.value() as u32);
        self.write32(offset + 4, (address.value() >> 32) as u32);
    }
    /// 读回低高地址字，队列尚未启用时设备不得修改这些字段。
    fn read_address(&self, offset: usize) -> u64 {
        u64::from(self.read32(offset)) | (u64::from(self.read32(offset + 4)) << 32)
    }
    /// 验证队列索引后选择，防止无效索引读取上一队列状态。
    fn select_queue(&mut self, queue: u16) -> crate::Result<()> {
        if self.read16(18) <= queue {
            return Err(Error::InvalidParam);
        }
        self.write16(22, queue);
        if self.read16(22) != queue {
            return Err(Error::NotReady);
        }
        Ok(())
    }
}

/// 核对设备提供的通知偏移乘法及完整半字窗口。
fn checked_notify_offset(len: usize, queue_offset: u16, multiplier: u32) -> crate::Result<usize> {
    let offset = usize::from(queue_offset)
        .checked_mul(multiplier as usize)
        .ok_or(Error::InvalidParam)?;
    let end = offset.checked_add(2).ok_or(Error::InvalidParam)?;
    if len < end || offset % 2 != 0 {
        return Err(Error::InvalidParam);
    }
    Ok(offset)
}

impl Transport for PciTransport {
    /// 返回构造验证的设备类型。
    fn device_type(&self) -> DeviceType {
        self.device_type
    }
    /// 选择并读取两个功能字。
    fn read_device_features(&mut self) -> u64 {
        self.write32(0, 0);
        let low = u64::from(self.read32(4));
        self.write32(0, 1);
        low | (u64::from(self.read32(4)) << 32)
    }
    /// 选择并写入两个驱动功能字。
    fn write_driver_features(&mut self, features: u64) {
        self.write32(8, 0);
        self.write32(12, features as u32);
        self.write32(8, 1);
        self.write32(12, (features >> 32) as u32);
    }
    /// 无效队列没有可用容量。
    fn max_queue_size(&mut self, queue: u16) -> u32 {
        if self.select_queue(queue).is_err() {
            0
        } else {
            u32::from(self.read16(24))
        }
    }
    /// 一次核对 notify_off 乘法、窗口末端与实际半字对齐。
    fn notification(&mut self, queue: u16) -> crate::Result<Notification> {
        self.select_queue(queue)?;
        let offset = checked_notify_offset(
            self.notify_cfg.len,
            self.read16(30),
            self.notify_off_multiplier,
        )?;
        // SAFETY: 构造验证首地址对齐和映射；本次完整核对偏移及宽度位于窗口内。
        let pointer = unsafe { NonNull::new_unchecked(self.notify_cfg.base.as_ptr().add(offset)) };
        Ok(Notification::pci(pointer, queue))
    }
    /// 读取真实 status 字节并保留所有状态位。
    fn get_status(&self) -> DeviceStatus {
        DeviceStatus::from_bits_retain(u32::from(self.read8(20)))
    }
    /// 写入真实 status，零值只请求 reset。
    fn set_status(&mut self, status: DeviceStatus) {
        self.write8(20, status.bits() as u8);
    }
    /// 现代 PCI 不存在 guest page size 字段。
    fn set_guest_page_size(&mut self, _guest_page_size: u32) {}
    /// 本后端只实现现代 capability 协议。
    fn requires_legacy_layout(&self) -> bool {
        false
    }
    /// 验证未启用队列及地址后写入，并真实读回地址、深度与 enable。
    fn queue_set(
        &mut self,
        queue: u16,
        size: u32,
        descriptors: DmaAddr,
        driver_area: DmaAddr,
        device_area: DmaAddr,
    ) -> crate::Result<()> {
        validate_queue_addresses(size, false, descriptors, driver_area, device_area)?;
        self.select_queue(queue)?;
        if self.read16(28) != 0 {
            return Err(Error::AlreadyUsed);
        }
        if u32::from(self.read16(24)) < size {
            return Err(Error::InvalidParam);
        }
        self.write16(24, size as u16);
        self.write_address(32, descriptors);
        self.write_address(40, driver_area);
        self.write_address(48, device_area);
        if u32::from(self.read16(24)) != size
            || self.read_address(32) != descriptors.value()
            || self.read_address(40) != driver_area.value()
            || self.read_address(48) != device_area.value()
        {
            return Err(Error::NotReady);
        }
        self.write16(28, 1);
        if self.read16(28) != 1 {
            return Err(Error::NotReady);
        }
        Ok(())
    }
    /// 查询选定队列真实 enable，非法队列不视为启用。
    fn queue_used(&mut self, queue: u16) -> bool {
        self.select_queue(queue).is_ok() && self.read16(28) != 0
    }
    /// ISR 是 u8 read-to-clear；共享中断保留本设备两种原因。
    fn ack_interrupt(&mut self) -> InterruptStatus {
        // SAFETY: 构造保证 ISR 的真实单字节映射，独占引用串行化 read-to-clear。
        let raw = unsafe { ptr::read_volatile(self.isr_cfg.base.as_ptr()) };
        InterruptStatus::from_bits_retain(raw)
    }
    /// 读取真实配置 generation 字节。
    fn read_config_generation(&self) -> u32 {
        u32::from(self.read8(21))
    }
    /// 按现代配置 ABI 的 1/2/4 字节访问读取字段，u64 拆为两个字。
    fn read_config_space<T: FromBytes + IntoBytes>(&self, offset: usize) -> crate::Result<T> {
        let region = self.config_space.ok_or(Error::ConfigSpaceMissing)?;
        // SAFETY: 构造保证真实配置映射保活；helper 核对整个字段范围及设备访问对齐。
        unsafe { read_config(region.base, region.len, offset, false) }
    }
    /// 按现代配置 ABI 的宽度有界写入字段。
    fn write_config_space<T: IntoBytes + Immutable>(
        &mut self,
        offset: usize,
        value: T,
    ) -> crate::Result<()> {
        let region = self.config_space.ok_or(Error::ConfigSpaceMissing)?;
        // SAFETY: 配置映射保活，独占引用串行化写；helper 检查字段范围和协议对齐。
        unsafe { write_config(region.base, region.len, offset, value, false) }
    }
}

// SAFETY: MMIO 映射永久有效，移动不复制独占状态或选择寄存器访问者。
unsafe impl Send for PciTransport {}
// SAFETY: 共享引用只读取无副作用 common/config 字段；ISR 和选择必须独占引用。
unsafe impl Sync for PciTransport {}

/// 选定 capability 构造的实际协议错误。
#[derive(Clone, Debug, Eq, thiserror::Error, PartialEq)]
pub enum VirtioPciError {
    /// 库存 device ID 不属于已识别 VirtIO 类型。
    #[error("Invalid VirtIO PCI device ID {0:#06x}")]
    InvalidDeviceId(u16),
    /// 库存 vendor ID 不是 VirtIO。
    #[error("Invalid VirtIO PCI vendor ID {0:#06x}")]
    InvalidVendorId(u16),
    /// 已选 capability 中没有公共配置。
    #[error("Missing common configuration capability")]
    MissingCommonConfig,
    /// 已选 capability 中没有通知窗口。
    #[error("Missing notification capability")]
    MissingNotifyConfig,
    /// notify multiplier 不能产生对齐半字地址。
    #[error("Invalid notification multiplier {0}")]
    InvalidNotifyOffMultiplier(u32),
    /// 已选 capability 中没有 ISR 字节。
    #[error("Missing ISR capability")]
    MissingIsrConfig,
    /// capability 完整范围不能位于 BAR 映射中。
    #[error("Capability is outside its assigned BAR")]
    BarOffsetOutOfRange,
    /// 两个被选协议窗口占用同一字节。
    #[error("Overlapping VirtIO capabilities")]
    OverlappingCapabilities,
    /// capability 首地址不满足协议访问对齐。
    #[error("Address {address:#x} is not aligned to {alignment} bytes")]
    Misaligned {
        /// 已映射窗口的 CPU 首地址。
        address: usize,
        /// 协议要求的字节对齐。
        alignment: usize,
    },
    /// 唯一 PCI 配置算法返回的原始错误。
    #[error(transparent)]
    Pci(PciError),
}

impl From<PciError> for VirtioPciError {
    /// 保留唯一 PCI 配置算法的原始错误。
    fn from(error: PciError) -> Self {
        Self::Pci(error)
    }
}

#[cfg(test)]
mod tests {
    use super::checked_notify_offset;
    use crate::Error;

    /// 设备通知偏移必须容纳完整半字，不能越界或截断奇数偏移。
    #[test]
    fn notification_aperture_bounds() {
        assert_eq!(checked_notify_offset(10, 2, 4), Ok(8));
        assert_eq!(checked_notify_offset(9, 2, 4), Err(Error::InvalidParam));
        assert_eq!(checked_notify_offset(10, 3, 1), Err(Error::InvalidParam));
        assert_eq!(
            checked_notify_offset(2, u16::MAX, u32::MAX),
            Err(Error::InvalidParam)
        );
        assert_eq!(checked_notify_offset(2, u16::MAX, 0), Ok(0));
    }
}
