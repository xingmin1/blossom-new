//! VirtIO 设备协议与已验证的锁外通知视图。

pub mod mmio;
pub mod pci;
mod some;

use crate::{DmaAddr, Error, Hal, Result, PAGE_SIZE};
use bitflags::bitflags;
use core::{
    fmt::{self, Debug, Formatter},
    ptr::{self, NonNull},
};
pub use some::SomeTransport;
use zerocopy::{FromBytes, Immutable, IntoBytes};

/// 设备初始化、队列发表和中断确认的唯一传输协议。
pub trait Transport {
    /// 返回已验证的设备类型。
    fn device_type(&self) -> DeviceType;
    /// 读取设备的完整功能位；选择寄存器由调用方串行化。
    fn read_device_features(&mut self) -> u64;
    /// 写入已协商的完整功能位。
    fn write_driver_features(&mut self, driver_features: u64);
    /// 返回选定队列尚未启用时的最大深度。
    fn max_queue_size(&mut self, queue: u16) -> u32;
    /// 准备一次有界检查后的通知地址，后续通知不再选择队列。
    fn notification(&mut self, queue: u16) -> Result<Notification>;
    /// 读取真实硬件状态。
    fn get_status(&self) -> DeviceStatus;
    /// 写入协议状态；写零只请求 reset，不证明 reset 已完成。
    fn set_status(&mut self, status: DeviceStatus);
    /// 为 legacy MMIO 写入实际页大小，现代设备不使用此寄存器。
    fn set_guest_page_size(&mut self, guest_page_size: u32);
    /// 返回设备是否采用连续且 used 按页对齐的旧布局。
    fn requires_legacy_layout(&self) -> bool;
    /// 验证地址、深度和未启用状态后写入队列，并读回启用结果。
    fn queue_set(
        &mut self,
        queue: u16,
        size: u32,
        descriptors: DmaAddr,
        driver_area: DmaAddr,
        device_area: DmaAddr,
    ) -> Result<()>;
    /// 查询真实 PFN 或 ready/enable，不缓存设备状态。
    fn queue_used(&mut self, queue: u16) -> bool;
    /// 读取并确认本设备中断，保留 queue/config 两种原因。
    fn ack_interrupt(&mut self) -> InterruptStatus;
    /// 请求设备停止 DMA，完成确认由拥有者在普通 Task 中安排。
    fn request_reset(&mut self) {
        self.set_status(DeviceStatus::empty());
    }
    /// 一次读取 reset 是否完成，不忙等。
    fn reset_complete(&self) -> bool {
        self.get_status().is_empty()
    }
    /// 在已确认 reset 后协商功能；现代设备必须接受 VERSION_1。
    fn begin_init(&mut self, supported_features: u64) -> Result<u64> {
        if !self.reset_complete() {
            return Err(Error::ResetPending);
        }
        let initial = DeviceStatus::ACKNOWLEDGE | DeviceStatus::DRIVER;
        self.set_status(DeviceStatus::ACKNOWLEDGE);
        self.set_status(initial);
        let offered = self.read_device_features();
        let modern = !self.requires_legacy_layout();
        let version_one = 1u64 << 32;
        if modern && offered & version_one == 0 {
            return Err(Error::FeaturesRejected);
        }
        let negotiated = if modern {
            (offered & supported_features) | version_one
        } else {
            offered & supported_features & !version_one
        };
        self.write_driver_features(negotiated);
        if modern {
            self.set_status(initial | DeviceStatus::FEATURES_OK);
        } else {
            self.set_guest_page_size(PAGE_SIZE as u32);
        }
        let status = self.get_status();
        check_device_status(status)?;
        if !status.contains(initial) || (modern && !status.contains(DeviceStatus::FEATURES_OK)) {
            return Err(Error::FeaturesRejected);
        }
        Ok(negotiated)
    }
    /// 发表 DRIVER_OK 并核对设备仍接受全部初始化状态。
    fn finish_init(&mut self) -> Result<()> {
        let mut required = DeviceStatus::ACKNOWLEDGE | DeviceStatus::DRIVER;
        if !self.requires_legacy_layout() {
            required |= DeviceStatus::FEATURES_OK;
        }
        let status = self.get_status();
        check_device_status(status)?;
        if !status.contains(required) {
            return Err(Error::FeaturesRejected);
        }
        required |= DeviceStatus::DRIVER_OK;
        self.set_status(required);
        let status = self.get_status();
        check_device_status(status)?;
        if !status.contains(required) {
            return Err(Error::NotReady);
        }
        Ok(())
    }
    /// 读取现代 config generation；legacy 返回零。
    fn read_config_generation(&self) -> u32;
    /// 有界读取配置字段，偏移及类型对齐不合法时返回错误。
    fn read_config_space<T: FromBytes + IntoBytes>(&self, offset: usize) -> Result<T>;
    /// 有界写入配置字段。
    fn write_config_space<T: IntoBytes + Immutable>(
        &mut self,
        offset: usize,
        value: T,
    ) -> Result<()>;
    /// 至多重读八次配置快照，持续变化或设备故障交给拥有者停止设备。
    fn read_consistent<T>(&self, f: impl Fn() -> Result<T>) -> Result<T> {
        for _ in 0..8 {
            check_device_status(self.get_status())?;
            let before = self.read_config_generation();
            let result = f();
            let after = self.read_config_generation();
            check_device_status(self.get_status())?;
            if before == after {
                return result;
            }
        }
        Err(Error::DeviceNeedsReset)
    }
}

/// 设备故障状态不能当成功初始化或稳定配置。
fn check_device_status(status: DeviceStatus) -> Result<()> {
    if status.intersects(DeviceStatus::DEVICE_NEEDS_RESET | DeviceStatus::FAILED) {
        Err(Error::DeviceNeedsReset)
    } else {
        Ok(())
    }
}

bitflags! {
    /// 本设备中断原因；共享 IRQ 必须先核对此值。
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub struct InterruptStatus: u8 {
        /// 本设备 used-ring 有待处理完成。
        const QUEUE = 1;
        /// 本设备配置发生变化。
        const CONFIG = 2;
    }
}

/// 已准备的 doorbell 地址；裸视图不拥有设备或映射。
#[derive(Clone, Copy, Debug)]
pub struct Notification {
    pointer: NonNull<u8>,
    width: NotificationWidth,
    queue: u16,
}

#[derive(Clone, Copy, Debug)]
enum NotificationWidth {
    U16,
    U32,
}

impl Notification {
    /// 保存已核对窗口及对齐的 PCI doorbell。
    fn pci(pointer: NonNull<u8>, queue: u16) -> Self {
        Self {
            pointer,
            width: NotificationWidth::U16,
            queue,
        }
    }
    /// 保存已核对 MMIO header 内的 queue_notify 寄存器。
    fn mmio(pointer: NonNull<u8>, queue: u16) -> Self {
        Self {
            pointer,
            width: NotificationWidth::U32,
            queue,
        }
    }
    /// 在完整设备屏障后写 doorbell；可在队列锁外调用。
    ///
    /// # Safety
    /// 调用者必须保持设备和强序 MMIO 映射有效，且与 reset/停机串行化；
    /// 该队列的已发表 DMA 缓冲区仍须由请求拥有者保活。
    pub unsafe fn notify<H: Hal>(&self) {
        H::dma_barrier();
        // SAFETY: 构造已检查寄存器宽度和对齐；调用者保证映射及设备仍存活。
        unsafe {
            match self.width {
                NotificationWidth::U16 => {
                    ptr::write_volatile(self.pointer.as_ptr().cast::<u16>(), self.queue)
                }
                NotificationWidth::U32 => {
                    ptr::write_volatile(self.pointer.as_ptr().cast::<u32>(), u32::from(self.queue))
                }
            }
        }
    }
}

// SAFETY: 视图不提供安全解引用；跨 CPU 通知的设备寿命和停机串行化由 unsafe notify 调用者保证。
unsafe impl Send for Notification {}
// SAFETY: 视图无可变 CPU 数据；每次 unsafe notify 都独立承担设备存活及同步义务。
unsafe impl Sync for Notification {}

/// 核对本实现的连续 split-ring 布局及完整设备地址范围。
fn validate_queue_addresses(
    size: u32,
    legacy: bool,
    descriptors: DmaAddr,
    driver_area: DmaAddr,
    device_area: DmaAddr,
) -> Result<()> {
    let depth = u16::try_from(size).map_err(|_| Error::InvalidParam)?;
    let layout = crate::queue::RingLayout::new(depth, legacy)?;
    let alignment = if legacy { PAGE_SIZE as u64 } else { 16 };
    if descriptors.value() % alignment != 0
        || descriptors.checked_add(layout.avail_offset()) != Some(driver_area)
        || descriptors.checked_add(layout.used_offset()) != Some(device_area)
        || descriptors.checked_add(layout.byte_len() - 1).is_none()
    {
        return Err(Error::InvalidParam);
    }
    Ok(())
}

/// 检查配置访问的范围及协议宽度；legacy 按字节访问，现代字段最多两个字。
fn config_access_width<T>(
    base: NonNull<u8>,
    len: usize,
    offset: usize,
    legacy: bool,
) -> Result<usize> {
    let size = core::mem::size_of::<T>();
    let end = offset.checked_add(size).ok_or(Error::ConfigSpaceTooSmall)?;
    if len < end {
        return Err(Error::ConfigSpaceTooSmall);
    }
    let width = if legacy {
        1
    } else {
        match size {
            1 | 2 | 4 => size,
            8 => 4,
            _ => return Err(Error::InvalidParam),
        }
    };
    if size == 0
        || (base.as_ptr() as usize)
            .checked_add(offset)
            .ok_or(Error::InvalidParam)?
            % width
            != 0
    {
        return Err(Error::InvalidParam);
    }
    Ok(width)
}

/// 按配置 ABI 宽度读取完整字段，不要求设备支持任意 Rust 类型的单次 MMIO。
///
/// # Safety
/// base 的 len 字节必须是真实且存活的配置映射；读取不得与配置写入并发。
unsafe fn read_config<T: FromBytes + IntoBytes>(
    base: NonNull<u8>,
    len: usize,
    offset: usize,
    legacy: bool,
) -> Result<T> {
    let width = config_access_width::<T>(base, len, offset, legacy)?;
    let mut result = core::mem::MaybeUninit::<T>::uninit();
    let target = result.as_mut_ptr().cast::<u8>();
    for index in (0..core::mem::size_of::<T>()).step_by(width) {
        // SAFETY: 完整字段范围和设备访问宽度已检查；局部目标容纳 T 的全部字节。
        unsafe {
            let source = base.as_ptr().add(offset + index);
            match width {
                1 => target.add(index).write(ptr::read_volatile(source)),
                2 => target
                    .add(index)
                    .cast::<u16>()
                    .write_unaligned(ptr::read_volatile(source.cast::<u16>())),
                4 => target
                    .add(index)
                    .cast::<u32>()
                    .write_unaligned(ptr::read_volatile(source.cast::<u32>())),
                _ => unreachable!(),
            }
        }
    }
    // SAFETY: 循环完整写入 T 的每个字节，FromBytes 保证所有字节组合都是有效值。
    Ok(unsafe { result.assume_init() })
}

/// 按配置 ABI 宽度写入字段，legacy 保持逐字节协议。
///
/// # Safety
/// base 的 len 字节必须是真实且存活的配置映射；调用者必须独占配置写入。
unsafe fn write_config<T: IntoBytes + Immutable>(
    base: NonNull<u8>,
    len: usize,
    offset: usize,
    value: T,
    legacy: bool,
) -> Result<()> {
    let width = config_access_width::<T>(base, len, offset, legacy)?;
    let source = value.as_bytes().as_ptr();
    for index in (0..core::mem::size_of::<T>()).step_by(width) {
        // SAFETY: 完整字段范围和设备访问宽度已检查；IntoBytes/Immutable 提供已初始化且无并发变动的源。
        unsafe {
            let target = base.as_ptr().add(offset + index);
            match width {
                1 => ptr::write_volatile(target, source.add(index).read()),
                2 => ptr::write_volatile(
                    target.cast::<u16>(),
                    source.add(index).cast::<u16>().read_unaligned(),
                ),
                4 => ptr::write_volatile(
                    target.cast::<u32>(),
                    source.add(index).cast::<u32>().read_unaligned(),
                ),
                _ => unreachable!(),
            }
        }
    }
    Ok(())
}

/// The device status field. Writing 0 into this field resets the device.
#[derive(Copy, Clone, Default, Eq, FromBytes, Immutable, IntoBytes, PartialEq)]
pub struct DeviceStatus(u32);

impl Debug for DeviceStatus {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        write!(f, "DeviceStatus(")?;
        bitflags::parser::to_writer(self, &mut *f)?;
        write!(f, ")")?;
        Ok(())
    }
}

bitflags! {
    impl DeviceStatus: u32 {
        /// Indicates that the guest OS has found the device and recognized it
        /// as a valid virtio device.
        const ACKNOWLEDGE = 1;

        /// Indicates that the guest OS knows how to drive the device.
        const DRIVER = 2;

        /// Indicates that something went wrong in the guest, and it has given
        /// up on the device. This could be an internal error, or the driver
        /// didn’t like the device for some reason, or even a fatal error
        /// during device operation.
        const FAILED = 128;

        /// Indicates that the driver has acknowledged all the features it
        /// understands, and feature negotiation is complete.
        const FEATURES_OK = 8;

        /// Indicates that the driver is set up and ready to drive the device.
        const DRIVER_OK = 4;

        /// Indicates that the device has experienced an error from which it
        /// can’t recover.
        const DEVICE_NEEDS_RESET = 64;
    }
}

/// Types of virtio devices.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(missing_docs)]
pub enum DeviceType {
    Network = 1,
    Block = 2,
    Console = 3,
    EntropySource = 4,
    MemoryBallooning = 5,
    IoMemory = 6,
    Rpmsg = 7,
    ScsiHost = 8,
    _9P = 9,
    Mac80211 = 10,
    RprocSerial = 11,
    VirtioCAIF = 12,
    MemoryBalloon = 13,
    GPU = 16,
    Timer = 17,
    Input = 18,
    Socket = 19,
    Crypto = 20,
    SignalDistributionModule = 21,
    Pstore = 22,
    IOMMU = 23,
    Memory = 24,
    Sound = 25,
}

/// Errors converting a number to a virtio device type.
#[derive(Copy, Clone, Debug, Eq, thiserror::Error, PartialEq)]
pub enum DeviceTypeError {
    /// Invalid or unknown virtio device type.
    #[error("Invalid or unknown virtio device type {0}")]
    InvalidDeviceType(u32),
}

impl TryFrom<u32> for DeviceType {
    type Error = DeviceTypeError;

    fn try_from(virtio_device_id: u32) -> core::result::Result<Self, Self::Error> {
        match virtio_device_id {
            1 => Ok(DeviceType::Network),
            2 => Ok(DeviceType::Block),
            3 => Ok(DeviceType::Console),
            4 => Ok(DeviceType::EntropySource),
            5 => Ok(DeviceType::MemoryBalloon),
            6 => Ok(DeviceType::IoMemory),
            7 => Ok(DeviceType::Rpmsg),
            8 => Ok(DeviceType::ScsiHost),
            9 => Ok(DeviceType::_9P),
            10 => Ok(DeviceType::Mac80211),
            11 => Ok(DeviceType::RprocSerial),
            12 => Ok(DeviceType::VirtioCAIF),
            13 => Ok(DeviceType::MemoryBalloon),
            16 => Ok(DeviceType::GPU),
            17 => Ok(DeviceType::Timer),
            18 => Ok(DeviceType::Input),
            19 => Ok(DeviceType::Socket),
            20 => Ok(DeviceType::Crypto),
            21 => Ok(DeviceType::SignalDistributionModule),
            22 => Ok(DeviceType::Pstore),
            23 => Ok(DeviceType::IOMMU),
            24 => Ok(DeviceType::Memory),
            25 => Ok(DeviceType::Sound),
            _ => Err(DeviceTypeError::InvalidDeviceType(virtio_device_id)),
        }
    }
}

impl TryFrom<u16> for DeviceType {
    type Error = DeviceTypeError;

    fn try_from(virtio_device_id: u16) -> core::result::Result<Self, Self::Error> {
        u32::from(virtio_device_id).try_into()
    }
}

impl TryFrom<u8> for DeviceType {
    type Error = DeviceTypeError;

    fn try_from(virtio_device_id: u8) -> core::result::Result<Self, Self::Error> {
        u32::from(virtio_device_id).try_into()
    }
}

#[cfg(test)]
mod tests {
    use super::config_access_width;
    use crate::Error;
    use core::ptr::NonNull;

    /// 字段边界不能溢出，现代八字节字段只要求设备支持两个对齐字访问。
    #[test]
    fn configuration_field_boundaries() {
        let base = NonNull::<u32>::dangling().cast::<u8>();
        assert_eq!(config_access_width::<u64>(base, 8, 0, false), Ok(4));
        assert_eq!(
            config_access_width::<u32>(base, 6, 2, false),
            Err(Error::InvalidParam)
        );
        assert_eq!(config_access_width::<u32>(base, 6, 2, true), Ok(1));
        assert_eq!(
            config_access_width::<u64>(base, 7, 0, false),
            Err(Error::ConfigSpaceTooSmall)
        );
        assert_eq!(
            config_access_width::<u32>(base, usize::MAX, usize::MAX - 1, false),
            Err(Error::ConfigSpaceTooSmall)
        );
    }
}
