//! 已映射窗口上的 legacy/modern VirtIO MMIO 协议。

use super::{
    read_config, validate_queue_addresses, write_config, DeviceStatus, DeviceType, DeviceTypeError,
    InterruptStatus, Notification, Transport,
};
use crate::{DmaAddr, Error, PAGE_SIZE};
use core::{
    convert::{TryFrom, TryInto},
    mem::align_of,
    ptr::NonNull,
};
use safe_mmio::{
    field, field_shared,
    fields::{ReadPure, ReadPureWrite, WriteOnly},
    UniqueMmioPointer,
};
use zerocopy::{FromBytes, Immutable, IntoBytes};

const MAGIC_VALUE: u32 = 0x7472_6976;
pub(crate) const LEGACY_VERSION: u32 = 1;
pub(crate) const MODERN_VERSION: u32 = 2;
const CONFIG_SPACE_OFFSET: usize = 0x100;

/// The version of the VirtIO MMIO transport supported by a device.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
#[repr(u32)]
pub enum MmioVersion {
    /// Legacy MMIO transport with page-based addressing.
    Legacy = LEGACY_VERSION,
    /// Modern MMIO transport.
    Modern = MODERN_VERSION,
}

impl TryFrom<u32> for MmioVersion {
    type Error = MmioError;

    fn try_from(version: u32) -> Result<Self, Self::Error> {
        match version {
            LEGACY_VERSION => Ok(Self::Legacy),
            MODERN_VERSION => Ok(Self::Modern),
            _ => Err(MmioError::UnsupportedVersion(version)),
        }
    }
}

impl From<MmioVersion> for u32 {
    fn from(version: MmioVersion) -> Self {
        match version {
            MmioVersion::Legacy => LEGACY_VERSION,
            MmioVersion::Modern => MODERN_VERSION,
        }
    }
}

/// An error encountered initialising a VirtIO MMIO transport.
#[derive(Clone, Debug, Eq, thiserror::Error, PartialEq)]
pub enum MmioError {
    /// The header doesn't start with the expected magic value 0x74726976.
    #[error("Invalid magic value {0:#010x} (expected 0x74726976)")]
    BadMagic(u32),
    /// The header reports a version number that is neither 1 (legacy) nor 2 (modern).
    #[error("Unsupported Virtio MMIO version {0}")]
    UnsupportedVersion(u32),
    /// The header reports a device ID of 0.
    #[error("Invalid or unknown device ID: {0}")]
    InvalidDeviceID(DeviceTypeError),
    /// The MMIO region size was smaller than the header size we expect.
    #[error("MMIO region too small")]
    MmioRegionTooSmall,
}

/// MMIO Device Register Interface, both legacy and modern.
///
/// Ref: 4.2.2 MMIO Device Register Layout and 4.2.4 Legacy interface
#[derive(Debug)]
#[repr(C)]
pub struct VirtIOHeader {
    /// Magic value
    magic: ReadPure<u32>,

    /// Device version number
    ///
    /// Legacy device returns value 0x1.
    version: ReadPure<u32>,

    /// Virtio Subsystem Device ID
    device_id: ReadPure<u32>,

    /// Virtio Subsystem Vendor ID
    vendor_id: ReadPure<u32>,

    /// Flags representing features the device supports
    device_features: ReadPure<u32>,

    /// Device (host) features word selection
    device_features_sel: WriteOnly<u32>,

    /// Reserved
    __r1: [u32; 2],

    /// Flags representing device features understood and activated by the driver
    driver_features: WriteOnly<u32>,

    /// Activated (guest) features word selection
    driver_features_sel: WriteOnly<u32>,

    /// Guest page size
    ///
    /// The driver writes the guest page size in bytes to the register during
    /// initialization, before any queues are used. This value should be a
    /// power of 2 and is used by the device to calculate the Guest address
    /// of the first queue page (see QueuePFN).
    legacy_guest_page_size: WriteOnly<u32>,

    /// Reserved
    __r2: u32,

    /// Virtual queue index
    ///
    /// Writing to this register selects the virtual queue that the following
    /// operations on the QueueNumMax, QueueNum, QueueAlign and QueuePFN
    /// registers apply to. The index number of the first queue is zero (0x0).
    queue_sel: WriteOnly<u32>,

    /// Maximum virtual queue size
    ///
    /// Reading from the register returns the maximum size of the queue the
    /// device is ready to process or zero (0x0) if the queue is not available.
    /// This applies to the queue selected by writing to QueueSel and is
    /// allowed only when QueuePFN is set to zero (0x0), so when the queue is
    /// not actively used.
    queue_num_max: ReadPure<u32>,

    /// Virtual queue size
    ///
    /// Queue size is the number of elements in the queue. Writing to this
    /// register notifies the device what size of the queue the driver will use.
    /// This applies to the queue selected by writing to QueueSel.
    queue_num: WriteOnly<u32>,

    /// Used Ring alignment in the virtual queue
    ///
    /// Writing to this register notifies the device about alignment boundary
    /// of the Used Ring in bytes. This value should be a power of 2 and
    /// applies to the queue selected by writing to QueueSel.
    legacy_queue_align: WriteOnly<u32>,

    /// Guest physical page number of the virtual queue
    ///
    /// Writing to this register notifies the device about location of the
    /// virtual queue in the Guest’s physical address space. This value is
    /// the index number of a page starting with the queue Descriptor Table.
    /// Value zero (0x0) means physical address zero (0x00000000) and is illegal.
    /// When the driver stops using the queue it writes zero (0x0) to this
    /// register. Reading from this register returns the currently used page
    /// number of the queue, therefore a value other than zero (0x0) means that
    /// the queue is in use. Both read and write accesses apply to the queue
    /// selected by writing to QueueSel.
    legacy_queue_pfn: ReadPureWrite<u32>,

    /// new interface only
    queue_ready: ReadPureWrite<u32>,

    /// Reserved
    __r3: [u32; 2],

    /// Queue notifier
    queue_notify: WriteOnly<u32>,

    /// Reserved
    __r4: [u32; 3],

    /// Interrupt status
    interrupt_status: ReadPure<u32>,

    /// Interrupt acknowledge
    interrupt_ack: WriteOnly<u32>,

    /// Reserved
    __r5: [u32; 2],

    /// Device status
    ///
    /// Reading from this register returns the current device status flags.
    /// Writing non-zero values to this register sets the status flags,
    /// indicating the OS/driver progress. Writing zero (0x0) to this register
    /// triggers a device reset. The device sets QueuePFN to zero (0x0) for
    /// all queues in the device. Also see 3.1 Device Initialization.
    status: ReadPureWrite<DeviceStatus>,

    /// Reserved
    __r6: [u32; 3],

    // new interface only since here
    queue_desc_low: WriteOnly<u32>,
    queue_desc_high: WriteOnly<u32>,

    /// Reserved
    __r7: [u32; 2],

    queue_driver_low: WriteOnly<u32>,
    queue_driver_high: WriteOnly<u32>,

    /// Reserved
    __r8: [u32; 2],

    queue_device_low: WriteOnly<u32>,
    queue_device_high: WriteOnly<u32>,

    /// Reserved
    __r9: [u32; 21],

    config_generation: ReadPure<u32>,
}

/// MMIO Device Register Interface.
///
/// Ref: 4.2.2 MMIO Device Register Layout and 4.2.4 Legacy interface
#[derive(Debug)]
pub struct MmioTransport<'a> {
    header: UniqueMmioPointer<'a, VirtIOHeader>,
    config_space: UniqueMmioPointer<'a, [u8]>,
    version: MmioVersion,
    device_type: DeviceType,
}

impl<'a> MmioTransport<'a> {
    /// 从完整已映射窗口创建唯一寄存器访问者。
    ///
    /// # Safety
    /// header 必须指向真实 VirtIO MMIO 设备，完整 mmio_size 字节必须强序映射
    /// 并在 'a 内保持有效；调用者必须独占选择及写寄存器，不能另建 transport。
    pub unsafe fn new(header: NonNull<VirtIOHeader>, mmio_size: usize) -> Result<Self, MmioError> {
        let config_space_size = mmio_size
            .checked_sub(CONFIG_SPACE_OFFSET)
            .ok_or(MmioError::MmioRegionTooSmall)?;
        if header.as_ptr() as usize % align_of::<VirtIOHeader>() != 0
            || (header.as_ptr() as usize).checked_add(mmio_size).is_none()
        {
            return Err(MmioError::MmioRegionTooSmall);
        }
        // SAFETY: 调用者保证完整窗口有效，配置区偏移已由最小窗口长度核对。
        let config_space = unsafe {
            UniqueMmioPointer::new(NonNull::slice_from_raw_parts(
                header.cast::<u8>().byte_add(CONFIG_SPACE_OFFSET),
                config_space_size,
            ))
        };
        // SAFETY: 已检查 header 对齐；调用者承担真实映射及整个借用期独占写义务。
        let header = unsafe { UniqueMmioPointer::new(header) };
        Self::new_from_unique(header, config_space)
    }

    /// 从具有同一设备寿命的唯一 header/config MMIO 借用核对协议身份。
    pub fn new_from_unique(
        header: UniqueMmioPointer<'a, VirtIOHeader>,
        config_space: UniqueMmioPointer<'a, [u8]>,
    ) -> Result<Self, MmioError> {
        let magic = field_shared!(header, magic).read();
        if magic != MAGIC_VALUE {
            return Err(MmioError::BadMagic(magic));
        }
        let device_id = field_shared!(header, device_id).read();
        let device_type = DeviceType::try_from(device_id).map_err(MmioError::InvalidDeviceID)?;
        let version = field_shared!(header, version).read().try_into()?;
        Ok(Self {
            header,
            version,
            device_type,
            config_space,
        })
    }

    /// 返回构造时已验证的 MMIO 协议版本。
    pub fn version(&self) -> MmioVersion {
        self.version
    }

    /// 读取真实 vendor 字段。
    pub fn vendor_id(&self) -> u32 {
        field_shared!(self.header, vendor_id).read()
    }
}

// SAFETY: 共享引用仅读取无副作用身份、状态及配置；选择和中断确认要求独占引用。
unsafe impl Sync for MmioTransport<'_> {}

impl Transport for MmioTransport<'_> {
    /// 返回此已验证窗口的设备类型。
    fn device_type(&self) -> DeviceType {
        self.device_type
    }

    /// legacy 仅访问低功能字，modern 才选择高功能字。
    fn read_device_features(&mut self) -> u64 {
        field!(self.header, device_features_sel).write(0);
        let low = u64::from(field_shared!(self.header, device_features).read());
        if self.version == MmioVersion::Legacy {
            return low;
        }
        field!(self.header, device_features_sel).write(1);
        low | (u64::from(field_shared!(self.header, device_features).read()) << 32)
    }

    /// 按版本只写存在的功能字。
    fn write_driver_features(&mut self, driver_features: u64) {
        field!(self.header, driver_features_sel).write(0);
        field!(self.header, driver_features).write(driver_features as u32);
        if self.version == MmioVersion::Modern {
            field!(self.header, driver_features_sel).write(1);
            field!(self.header, driver_features).write((driver_features >> 32) as u32);
        }
    }

    /// 选择尚未启用的队列后读取容量。
    fn max_queue_size(&mut self, queue: u16) -> u32 {
        field!(self.header, queue_sel).write(u32::from(queue));
        field_shared!(self.header, queue_num_max).read()
    }

    /// 通知寄存器位于固定 header 偏移，后续不再写 queue_sel。
    fn notification(&mut self, queue: u16) -> crate::Result<Notification> {
        if self.max_queue_size(queue) == 0 {
            return Err(Error::InvalidParam);
        }
        // SAFETY: 0x50 是两版本 header 内已映射且四字节对齐的通知寄存器。
        let pointer =
            unsafe { NonNull::new_unchecked(self.header.ptr().cast::<u8>().cast_mut().add(0x50)) };
        Ok(Notification::mmio(pointer, queue))
    }

    /// 读取设备真实状态。
    fn get_status(&self) -> DeviceStatus {
        field_shared!(self.header, status).read()
    }

    /// 写零只请求停止，拥有者仍须读回确认。
    fn set_status(&mut self, status: DeviceStatus) {
        field!(self.header, status).write(status);
    }

    /// 仅 legacy MMIO 使用 guest page size。
    fn set_guest_page_size(&mut self, guest_page_size: u32) {
        if self.version == MmioVersion::Legacy {
            field!(self.header, legacy_guest_page_size).write(guest_page_size);
        }
    }

    /// legacy 使用连续 PFN 布局。
    fn requires_legacy_layout(&self) -> bool {
        self.version == MmioVersion::Legacy
    }

    /// 完整验证后一次发表队列；失败也可能已有设备访问，拥有者必须 reset。
    fn queue_set(
        &mut self,
        queue: u16,
        size: u32,
        descriptors: DmaAddr,
        driver_area: DmaAddr,
        device_area: DmaAddr,
    ) -> crate::Result<()> {
        let legacy = self.requires_legacy_layout();
        validate_queue_addresses(size, legacy, descriptors, driver_area, device_area)?;
        if self.queue_used(queue) {
            return Err(Error::AlreadyUsed);
        }
        if self.max_queue_size(queue) < size {
            return Err(Error::InvalidParam);
        }
        let pfn = if legacy {
            let page = descriptors.value() / PAGE_SIZE as u64;
            let pfn = u32::try_from(page).map_err(|_| Error::InvalidParam)?;
            if pfn == 0 {
                return Err(Error::InvalidParam);
            }
            Some(pfn)
        } else {
            None
        };
        field!(self.header, queue_num).write(size);
        if let Some(pfn) = pfn {
            field!(self.header, legacy_queue_align).write(PAGE_SIZE as u32);
            field!(self.header, legacy_queue_pfn).write(pfn);
            if field_shared!(self.header, legacy_queue_pfn).read() != pfn {
                return Err(Error::NotReady);
            }
        } else {
            field!(self.header, queue_desc_low).write(descriptors.value() as u32);
            field!(self.header, queue_desc_high).write((descriptors.value() >> 32) as u32);
            field!(self.header, queue_driver_low).write(driver_area.value() as u32);
            field!(self.header, queue_driver_high).write((driver_area.value() >> 32) as u32);
            field!(self.header, queue_device_low).write(device_area.value() as u32);
            field!(self.header, queue_device_high).write((device_area.value() >> 32) as u32);
            field!(self.header, queue_ready).write(1);
            if field_shared!(self.header, queue_ready).read() != 1 {
                return Err(Error::NotReady);
            }
        }
        Ok(())
    }

    /// 读取选定队列实际 PFN 或 ready。
    fn queue_used(&mut self, queue: u16) -> bool {
        field!(self.header, queue_sel).write(u32::from(queue));
        if self.version == MmioVersion::Legacy {
            field_shared!(self.header, legacy_queue_pfn).read() != 0
        } else {
            field_shared!(self.header, queue_ready).read() != 0
        }
    }

    /// 确认读取到的全部位，并保留 queue/config 原因供共享线处理。
    fn ack_interrupt(&mut self) -> InterruptStatus {
        let raw = field_shared!(self.header, interrupt_status).read();
        if raw != 0 {
            field!(self.header, interrupt_ack).write(raw);
        }
        InterruptStatus::from_bits_retain(raw as u8)
    }

    /// legacy 没有 generation 寄存器，不访问其保留字段。
    fn read_config_generation(&self) -> u32 {
        if self.version == MmioVersion::Legacy {
            0
        } else {
            field_shared!(self.header, config_generation).read()
        }
    }

    /// 按真实版本的配置访问宽度读取有界字段。
    fn read_config_space<T: FromBytes + IntoBytes>(&self, offset: usize) -> crate::Result<T> {
        // SAFETY: 构造保证完整配置映射和设备寿命；helper 检查完整字段范围及协议对齐。
        unsafe {
            read_config(
                NonNull::new_unchecked(self.config_space.ptr().cast::<u8>().cast_mut()),
                self.config_space.len(),
                offset,
                self.version == MmioVersion::Legacy,
            )
        }
    }

    /// 按真实版本的配置访问宽度写入有界字段。
    fn write_config_space<T: IntoBytes + Immutable>(
        &mut self,
        offset: usize,
        value: T,
    ) -> crate::Result<()> {
        // SAFETY: 构造保证配置映射有效，独占 transport 串行化写；helper 检查范围和协议对齐。
        unsafe {
            write_config(
                self.config_space.ptr_nonnull().cast::<u8>(),
                self.config_space.len(),
                offset,
                value,
                self.version == MmioVersion::Legacy,
            )
        }
    }
}
