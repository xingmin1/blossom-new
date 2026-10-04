//! Module for dealing with a PCI bus in general, without anything specific to VirtIO.

use bitflags::bitflags;
use core::mem::size_of;
use core::{
    array,
    convert::TryFrom,
    fmt::{self, Display, Formatter},
    ops::Deref,
    ptr::NonNull,
};
use safe_mmio::{fields::ReadPureWrite, UniqueMmioPointer};
use thiserror::Error;

const INVALID_READ: u32 = 0xffffffff;

/// The maximum number of devices on a bus.
const MAX_DEVICES: u8 = 32;
/// The maximum number of functions on a device.
const MAX_FUNCTIONS: u8 = 8;

/// The offset in bytes to the status and command fields within PCI configuration space.
const STATUS_COMMAND_OFFSET: u8 = 0x04;
/// The offset in bytes to BAR0 within PCI configuration space.
const BAR0_OFFSET: u8 = 0x10;

/// ID for vendor-specific PCI capabilities.
pub const PCI_CAP_ID_VNDR: u8 = 0x09;

bitflags! {
    /// The status register in PCI configuration space.
    #[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
    pub struct Status: u16 {
        // Bits 0-2 are reserved.
        /// The state of the device's INTx# signal.
        const INTERRUPT_STATUS = 1 << 3;
        /// The device has a linked list of capabilities.
        const CAPABILITIES_LIST = 1 << 4;
        /// The device is capabile of running at 66 MHz rather than 33 MHz.
        const MHZ_66_CAPABLE = 1 << 5;
        // Bit 6 is reserved.
        /// The device can accept fast back-to-back transactions not from the same agent.
        const FAST_BACK_TO_BACK_CAPABLE = 1 << 7;
        /// The bus agent observed a parity error (if parity error handling is enabled).
        const MASTER_DATA_PARITY_ERROR = 1 << 8;
        // Bits 9-10 are DEVSEL timing.
        /// A target device terminated a transaction with target-abort.
        const SIGNALED_TARGET_ABORT = 1 << 11;
        /// A master device transaction was terminated with target-abort.
        const RECEIVED_TARGET_ABORT = 1 << 12;
        /// A master device transaction was terminated with master-abort.
        const RECEIVED_MASTER_ABORT = 1 << 13;
        /// A device asserts SERR#.
        const SIGNALED_SYSTEM_ERROR = 1 << 14;
        /// The device detects a parity error, even if parity error handling is disabled.
        const DETECTED_PARITY_ERROR = 1 << 15;
    }
}

bitflags! {
    /// The command register in PCI configuration space.
    #[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
    pub struct Command: u16 {
        /// The device can respond to I/O Space accesses.
        const IO_SPACE = 1 << 0;
        /// The device can respond to Memory Space accesses.
        const MEMORY_SPACE = 1 << 1;
        /// The device can behave as a bus master.
        const BUS_MASTER = 1 << 2;
        /// The device can monitor Special Cycle operations.
        const SPECIAL_CYCLES = 1 << 3;
        /// The device can generate the Memory Write and Invalidate command.
        const MEMORY_WRITE_AND_INVALIDATE_ENABLE = 1 << 4;
        /// The device will snoop palette register data.
        const VGA_PALETTE_SNOOP = 1 << 5;
        /// The device should take its normal action when a parity error is detected.
        const PARITY_ERROR_RESPONSE = 1 << 6;
        // Bit 7 is reserved.
        /// The SERR# driver is enabled.
        const SERR_ENABLE = 1 << 8;
        /// The device is allowed to generate fast back-to-back transactions.
        const FAST_BACK_TO_BACK_ENABLE = 1 << 9;
        /// Assertion of the device's INTx# signal is disabled.
        const INTERRUPT_DISABLE = 1 << 10;
    }
}

/// Errors accessing a PCI device.
#[derive(Copy, Clone, Debug, Eq, Error, PartialEq)]
pub enum PciError {
    /// The device reported an invalid BAR type.
    #[error("Invalid PCI BAR type")]
    InvalidBarType,
    /// BAR 索引越过当前配置头的资源表。
    #[error("Invalid PCI BAR index")]
    InvalidBarIndex,
    /// BAR 的大小掩码不能表示合法的二次幂窗口。
    #[error("Invalid PCI BAR size")]
    InvalidBarSize,
    /// CAM 地址、长度或总线边界无效。
    #[error("Invalid PCI configuration window")]
    InvalidConfigurationWindow,
    /// capability 指针未对齐或落在配置头内。
    #[error("Invalid PCI capability offset")]
    InvalidCapabilityOffset,
    /// capability 链重复访问同一位置。
    #[error("PCI capability cycle")]
    CapabilityCycle,
    /// capability 声明的长度越界或短于所需字段。
    #[error("Invalid PCI capability length")]
    InvalidCapabilityLength,
    /// 设备未接受 BAR 探测所需的 decode 关闭。
    #[error("PCI command change was rejected")]
    CommandRejected,
}

/// The root complex of a PCI bus.
#[derive(Debug)]
pub struct PciRoot<C: ConfigurationAccess> {
    pub(crate) configuration_access: C,
}

/// A PCI Configuration Access Mechanism.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum Cam {
    /// The PCI memory-mapped Configuration Access Mechanism.
    ///
    /// This provides access to 256 bytes of configuration space per device function.
    MmioCam,
    /// The PCIe memory-mapped Enhanced Configuration Access Mechanism.
    ///
    /// This provides access to 4 KiB of configuration space per device function.
    Ecam,
}

impl Cam {
    /// Returns the total size in bytes of the memory-mapped region.
    pub const fn size(self) -> u32 {
        match self {
            Self::MmioCam => 0x1000000,
            Self::Ecam => 0x10000000,
        }
    }

    /// Returns the offset in bytes within the CAM region for the given device, function and
    /// register.
    pub fn cam_offset(self, device_function: DeviceFunction, register_offset: u8) -> u32 {
        assert!(device_function.valid());

        let bdf = ((device_function.bus as u32) << 8)
            | ((device_function.device as u32) << 3)
            | (device_function.function as u32);
        let address =
            (bdf << match self {
                Cam::MmioCam => 8,
                Cam::Ecam => 12,
            }) | (register_offset as u32);
        // Ensure that address is within range.
        assert!(address < self.size());
        // Ensure that address is word-aligned.
        assert!(address & 0x3 == 0);
        address
    }
}

impl<C: ConfigurationAccess> PciRoot<C> {
    /// Creates a new `PciRoot` to access a PCI root complex through the given configuration access
    /// implementation.
    pub fn new(configuration_access: C) -> Self {
        Self {
            configuration_access,
        }
    }

    /// Enumerates PCI devices on the given bus.
    pub fn enumerate_bus(&self, bus: u8) -> BusDeviceIterator<C> {
        // SAFETY: The `BusDeviceIterator` only reads read-only fields.
        let configuration_access = unsafe { self.configuration_access.unsafe_clone() };
        BusDeviceIterator {
            configuration_access,
            next: DeviceFunction {
                bus,
                device: 0,
                function: 0,
            },
        }
    }

    /// Reads the status and command registers of the given device function.
    pub fn get_status_command(&self, device_function: DeviceFunction) -> (Status, Command) {
        let status_command = self
            .configuration_access
            .read_word(device_function, STATUS_COMMAND_OFFSET);
        let status = Status::from_bits_truncate((status_command >> 16) as u16);
        let command = Command::from_bits_truncate(status_command as u16);
        (status, command)
    }

    /// Sets the command register of the given device function.
    pub fn set_command(&mut self, device_function: DeviceFunction, command: Command) {
        self.configuration_access.write_word(
            device_function,
            STATUS_COMMAND_OFFSET,
            command.bits().into(),
        );
    }

    /// 遍历选定功能的 capability，非法 offset、长度或循环通过迭代项返回错误。
    pub fn capabilities(&self, device_function: DeviceFunction) -> CapabilityIterator<C> {
        CapabilityIterator {
            configuration_access: &self.configuration_access,
            device_function,
            next_capability_offset: self.capabilities_offset(device_function),
            visited: 0,
        }
    }

    /// Returns information about all the given device function's BARs.
    pub fn bars(
        &mut self,
        device_function: DeviceFunction,
    ) -> Result<[Option<BarInfo>; 6], PciError> {
        let mut bars = array::from_fn(|_| None);
        let mut bar_index = 0;
        while bar_index < 6 {
            let info = self.bar_info(device_function, bar_index)?;
            let bar_entries = if info.as_ref().is_some_and(BarInfo::takes_two_entries) {
                2
            } else {
                1
            };
            bars[usize::from(bar_index)] = info;
            bar_index += bar_entries;
        }
        Ok(bars)
    }

    /// 在关闭 decode 时探测单个 BAR；所有正常错误都保留原 BAR 与 command。
    pub fn bar_info(
        &mut self,
        device_function: DeviceFunction,
        bar_index: u8,
    ) -> Result<Option<BarInfo>, PciError> {
        let header =
            (self.configuration_access.read_word(device_function, 0x0c) >> 16) as u8 & 0x7f;
        let count = match header {
            0 => 6,
            1 => 2,
            _ => 0,
        };
        if count <= bar_index {
            return Err(PciError::InvalidBarIndex);
        }
        let offset = BAR0_OFFSET + 4 * bar_index;
        let original = self.configuration_access.read_word(device_function, offset);
        let io_space = original & 1 != 0;
        let address_type = if io_space {
            MemoryBarType::Width32
        } else {
            MemoryBarType::try_from(((original >> 1) & 3) as u8)?
        };
        let wide = address_type == MemoryBarType::Width64;
        if wide && count <= bar_index + 1 {
            return Err(PciError::InvalidBarIndex);
        }
        let top = if wide {
            self.configuration_access
                .read_word(device_function, offset + 4)
        } else {
            0
        };
        let (_, command) = self.get_status_command(device_function);
        self.set_command(
            device_function,
            command & !(Command::IO_SPACE | Command::MEMORY_SPACE),
        );
        if self
            .get_status_command(device_function)
            .1
            .intersects(Command::IO_SPACE | Command::MEMORY_SPACE)
        {
            self.set_command(device_function, command);
            return Err(PciError::CommandRejected);
        }
        self.configuration_access
            .write_word(device_function, offset, u32::MAX);
        if wide {
            self.configuration_access
                .write_word(device_function, offset + 4, u32::MAX);
        }
        let low_mask = self.configuration_access.read_word(device_function, offset);
        let high_mask = if wide {
            self.configuration_access
                .read_word(device_function, offset + 4)
        } else {
            0
        };
        self.configuration_access
            .write_word(device_function, offset, original);
        if wide {
            self.configuration_access
                .write_word(device_function, offset + 4, top);
        }
        self.set_command(device_function, command);

        let flags = if io_space { 3u64 } else { 15u64 };
        let width_mask = if wide { u64::MAX } else { u64::from(u32::MAX) };
        let mask = (u64::from(low_mask) | (u64::from(high_mask) << 32)) & !flags;
        if mask == 0 {
            return Ok(None);
        }
        // 二次幂大小的地址掩码必须连续；零大小与全 1 错误读都不能发表成资源。
        let size = ((!mask & width_mask).wrapping_add(1)) & width_mask;
        if !size.is_power_of_two()
            || mask != (width_mask & !(size - 1) & !flags)
            || low_mask == INVALID_READ
        {
            return Err(PciError::InvalidBarSize);
        }
        if io_space {
            Ok(Some(BarInfo::IO {
                address: original & !3,
                size: size as u32,
            }))
        } else {
            Ok(Some(BarInfo::Memory {
                address_type,
                prefetchable: original & 8 != 0,
                address: u64::from(original & !15) | (u64::from(top) << 32),
                size,
            }))
        }
    }

    /// 配置 32 位 BAR 的地址，保留设备报告的类型与属性位。
    pub fn set_bar_32(&mut self, device_function: DeviceFunction, bar_index: u8, address: u32) {
        assert!(bar_index < 6);
        let offset = BAR0_OFFSET + 4 * bar_index;
        let original = self.configuration_access.read_word(device_function, offset);
        let flags = if original & 1 != 0 { 3 } else { 15 };
        self.configuration_access.write_word(
            device_function,
            offset,
            (address & !flags) | (original & flags),
        );
    }

    /// 配置配对的 64 位 BAR 地址，保留低寄存器中的属性位。
    pub fn set_bar_64(&mut self, device_function: DeviceFunction, bar_index: u8, address: u64) {
        assert!(bar_index < 5);
        self.set_bar_32(device_function, bar_index, address as u32);
        self.configuration_access.write_word(
            device_function,
            BAR0_OFFSET + 4 * (bar_index + 1),
            (address >> 32) as u32,
        );
    }

    /// 读取选定功能的对齐配置字，不改变设备状态。
    pub fn read_config_word(&self, device_function: DeviceFunction, offset: u8) -> u32 {
        self.configuration_access.read_word(device_function, offset)
    }

    /// 保存原始 capability 指针，由迭代边界拒绝未对齐和越界值。
    fn capabilities_offset(&self, device_function: DeviceFunction) -> Option<u8> {
        let (status, _) = self.get_status_command(device_function);
        if status.contains(Status::CAPABILITIES_LIST) {
            let offset = self.configuration_access.read_word(device_function, 0x34) as u8;
            if offset == 0 {
                None
            } else {
                Some(offset)
            }
        } else {
            None
        }
    }
}

/// A method to access PCI configuration space for a particular PCI bus.
pub trait ConfigurationAccess {
    /// Reads 4 bytes from the configuration space.
    fn read_word(&self, device_function: DeviceFunction, register_offset: u8) -> u32;

    /// Writes 4 bytes to the configuration space.
    fn write_word(&mut self, device_function: DeviceFunction, register_offset: u8, data: u32);

    /// Makes a clone of the `ConfigurationAccess`, accessing the same PCI bus.
    ///
    /// # Safety
    ///
    /// This function allows concurrent mutable access to the PCI CAM. To avoid this causing
    /// problems, the returned `ConfigurationAccess` instance must only be used to read read-only
    /// fields.
    unsafe fn unsafe_clone(&self) -> Self;
}

/// 按真实映射长度访问 CAM；未映射总线的读取返回无设备，不构造越界引用。
pub struct MmioCam<'a> {
    mmio: UniqueMmioPointer<'a, [ReadPureWrite<u32>]>,
    cam: Cam,
}

impl MmioCam<'_> {
    /// 使用真实字节长度构造配置窗口；窗口须由完整的总线页组成。
    ///
    /// # Safety
    /// mmio_base 必须指向在整个借用寿命内有效、正确映射的 byte_length 字节
    /// MMIO 区间。该区间不能存在 Rust 引用或另一份可写 CAM owner。
    pub unsafe fn new(mmio_base: *mut u8, byte_length: usize, cam: Cam) -> Result<Self, PciError> {
        let bus_size = cam.size() as usize / 256;
        if mmio_base.is_null()
            || mmio_base as usize & 3 != 0
            || byte_length == 0
            || (cam.size() as usize) < byte_length
            || byte_length % bus_size != 0
            || (mmio_base as usize).checked_add(byte_length).is_none()
        {
            return Err(PciError::InvalidConfigurationWindow);
        }
        // SAFETY: 调用者保证完整有效 MMIO 区间与独占访问；上面的检查保证对齐和真实长度。
        let mmio = unsafe {
            UniqueMmioPointer::new(NonNull::slice_from_raw_parts(
                NonNull::new(mmio_base.cast::<ReadPureWrite<u32>>()).unwrap(),
                byte_length / size_of::<u32>(),
            ))
        };
        Ok(Self { mmio, cam })
    }
}

impl ConfigurationAccess for MmioCam<'_> {
    /// 越过真实窗口的总线读取按 PCI 无设备返回，不访问窗口外地址。
    fn read_word(&self, device_function: DeviceFunction, register_offset: u8) -> u32 {
        let address = self.cam.cam_offset(device_function, register_offset);
        let value = match self.mmio.deref().get((address >> 2) as usize) {
            Some(word) => word.read(),
            None => INVALID_READ,
        };
        configuration_barrier();
        value
    }

    /// 写入已映射窗口内的配置字；错误目标在解引用以前失败。
    fn write_word(&mut self, device_function: DeviceFunction, register_offset: u8, data: u32) {
        let address = self.cam.cam_offset(device_function, register_offset);
        self.mmio
            .get((address >> 2) as usize)
            .expect("PCI write outside CAM window")
            .write(data);
        configuration_barrier();
    }

    /// 枚举器只借此副本读取设备的不可变身份字段。
    ///
    /// # Safety
    /// 返回副本只能读取只读配置字段，不能与原 owner 并发写配置。
    unsafe fn unsafe_clone(&self) -> Self {
        // SAFETY: 调用者承担副本只读约束；原窗口长度和生命周期原样保留。
        let mmio =
            unsafe { UniqueMmioPointer::new(NonNull::new(self.mmio.ptr().cast_mut()).unwrap()) };
        Self {
            mmio,
            cam: self.cam,
        }
    }
}

/// 配置寄存器操作在弱序架构上必须按序，尤其 decode 关闭必须先于 BAR 探测。
fn configuration_barrier() {
    #[cfg(target_arch = "riscv64")]
    // SAFETY: fence 只约束本 CPU 内存和 IO 操作顺序，不读取指针或修改执行权限。
    unsafe {
        core::arch::asm!("fence iorw, iorw", options(nostack))
    };
    #[cfg(target_arch = "loongarch64")]
    // SAFETY: dbar 只约束本 CPU 已发出的设备访问，不要求额外寄存器或对象寿命。
    unsafe {
        core::arch::asm!("dbar 0", options(nostack))
    };
}

// SAFETY: `&MmioCam` only allows MMIO reads, which are fine to happen concurrently on different CPU
// cores.
unsafe impl Sync for MmioCam<'_> {}

/// Information about a PCI Base Address Register.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum BarInfo {
    /// The BAR is for a memory region.
    Memory {
        /// The size of the BAR address and where it can be located.
        address_type: MemoryBarType,
        /// If true, then reading from the region doesn't have side effects. The CPU may cache reads
        /// and merge repeated stores.
        prefetchable: bool,
        /// The memory address, always 16-byte aligned.
        address: u64,
        /// The size of the BAR in bytes.
        size: u64,
    },
    /// The BAR is for an I/O region.
    IO {
        /// The I/O address, always 4-byte aligned.
        address: u32,
        /// The size of the BAR in bytes.
        size: u32,
    },
}

impl BarInfo {
    /// Returns whether this BAR is a 64-bit memory region, and so takes two entries in the table in
    /// configuration space.
    pub fn takes_two_entries(&self) -> bool {
        matches!(
            self,
            BarInfo::Memory {
                address_type: MemoryBarType::Width64,
                ..
            }
        )
    }

    /// Returns the address and size of this BAR if it is a memory bar, or `None` if it is an IO
    /// BAR.
    pub fn memory_address_size(&self) -> Option<(u64, u64)> {
        if let Self::Memory { address, size, .. } = self {
            Some((*address, *size))
        } else {
            None
        }
    }
}

impl Display for BarInfo {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Memory {
                address_type,
                prefetchable,
                address,
                size,
            } => write!(
                f,
                "Memory space at {:#010x}, size {}, type {:?}, prefetchable {}",
                address, size, address_type, prefetchable
            ),
            Self::IO { address, size } => {
                write!(f, "I/O space at {:#010x}, size {}", address, size)
            }
        }
    }
}

/// The location allowed for a memory BAR.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum MemoryBarType {
    /// The BAR has a 32-bit address and can be mapped anywhere in 32-bit address space.
    Width32,
    /// The BAR must be mapped below 1MiB.
    Below1MiB,
    /// The BAR has a 64-bit address and can be mapped anywhere in 64-bit address space.
    Width64,
}

impl From<MemoryBarType> for u8 {
    fn from(bar_type: MemoryBarType) -> Self {
        match bar_type {
            MemoryBarType::Width32 => 0,
            MemoryBarType::Below1MiB => 1,
            MemoryBarType::Width64 => 2,
        }
    }
}

impl TryFrom<u8> for MemoryBarType {
    type Error = PciError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(Self::Width32),
            1 => Ok(Self::Below1MiB),
            2 => Ok(Self::Width64),
            _ => Err(PciError::InvalidBarType),
        }
    }
}

/// Iterator over capabilities for a device.
#[derive(Debug)]
pub struct CapabilityIterator<'a, C: ConfigurationAccess> {
    configuration_access: &'a C,
    device_function: DeviceFunction,
    next_capability_offset: Option<u8>,
    visited: u64,
}

impl<C: ConfigurationAccess> Iterator for CapabilityIterator<'_, C> {
    type Item = Result<CapabilityInfo, PciError>;

    /// 有界遍历配置头后的 capability；非法链只返回一次错误并结束。
    fn next(&mut self) -> Option<Self::Item> {
        let offset = self.next_capability_offset.take()?;
        if offset < 0x40 || offset & 3 != 0 {
            return Some(Err(PciError::InvalidCapabilityOffset));
        }
        let bit = 1u64 << (offset / 4);
        if self.visited & bit != 0 {
            return Some(Err(PciError::CapabilityCycle));
        }
        self.visited |= bit;
        let header = self
            .configuration_access
            .read_word(self.device_function, offset);
        let id = header as u8;
        let next = (header >> 8) as u8;
        let private_header = (header >> 16) as u16;
        if id == 0xff {
            return Some(Err(PciError::InvalidCapabilityOffset));
        }
        if id == PCI_CAP_ID_VNDR {
            let length = private_header as u8;
            let vendor = self.configuration_access.read_word(self.device_function, 0) as u16;
            let kind = (private_header >> 8) as u8;
            let minimum = if vendor == 0x1af4 && (1..=5).contains(&kind) {
                if kind == 2 || kind == 5 {
                    20
                } else {
                    16
                }
            } else {
                3
            };
            if length < minimum || 256 < u16::from(offset) + u16::from(length) {
                return Some(Err(PciError::InvalidCapabilityLength));
            }
        }
        if next != 0 {
            if next < 0x40 || next & 3 != 0 {
                return Some(Err(PciError::InvalidCapabilityOffset));
            }
            if self.visited & (1u64 << (next / 4)) != 0 {
                return Some(Err(PciError::CapabilityCycle));
            }
            self.next_capability_offset = Some(next);
        }
        Some(Ok(CapabilityInfo {
            offset,
            id,
            private_header,
        }))
    }
}

/// Information about a PCI device capability.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub struct CapabilityInfo {
    /// The offset of the capability in the PCI configuration space of the device function.
    pub offset: u8,
    /// The ID of the capability.
    pub id: u8,
    /// The third and fourth bytes of the capability, to save reading them again.
    pub private_header: u16,
}

/// An iterator which enumerates PCI devices and functions on a given bus.
#[derive(Debug)]
pub struct BusDeviceIterator<C: ConfigurationAccess> {
    /// This must only be used to read read-only fields, and must not be exposed outside this
    /// module, because it uses the same CAM as the main `PciRoot` instance.
    configuration_access: C,
    next: DeviceFunction,
}

impl<C: ConfigurationAccess> Iterator for BusDeviceIterator<C> {
    type Item = (DeviceFunction, DeviceFunctionInfo);

    /// 按设备号与真实 multifunction 位枚举，不读取单功能设备的虚假兄弟功能。
    fn next(&mut self) -> Option<Self::Item> {
        while self.next.device < MAX_DEVICES {
            let current = self.next;
            let device_vendor = self.configuration_access.read_word(current, 0);
            let present = device_vendor as u16 != 0xffff;
            let raw_header = if present {
                self.configuration_access.read_word(current, 12)
            } else {
                0
            };
            self.next.function += 1;
            if MAX_FUNCTIONS <= self.next.function
                || (current.function == 0 && (!present || raw_header & (1 << 23) == 0))
            {
                self.next.function = 0;
                self.next.device += 1;
            }
            if present {
                let class_revision = self.configuration_access.read_word(current, 8);
                return Some((
                    current,
                    DeviceFunctionInfo {
                        vendor_id: device_vendor as u16,
                        device_id: (device_vendor >> 16) as u16,
                        class: (class_revision >> 24) as u8,
                        subclass: (class_revision >> 16) as u8,
                        prog_if: (class_revision >> 8) as u8,
                        revision: class_revision as u8,
                        header_type: HeaderType::from((raw_header >> 16) as u8 & 0x7f),
                    },
                ));
            }
        }
        None
    }
}

/// An identifier for a PCI bus, device and function.
#[derive(Copy, Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct DeviceFunction {
    /// The PCI bus number, between 0 and 255.
    pub bus: u8,
    /// The device number on the bus, between 0 and 31.
    pub device: u8,
    /// The function number of the device, between 0 and 7.
    pub function: u8,
}

impl DeviceFunction {
    /// Returns whether the device and function numbers are valid, i.e. the device is between 0 and
    /// 31, and the function is between 0 and 7.
    pub fn valid(&self) -> bool {
        self.device < 32 && self.function < 8
    }
}

impl Display for DeviceFunction {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        write!(f, "{:02x}:{:02x}.{}", self.bus, self.device, self.function)
    }
}

/// Information about a PCI device function.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DeviceFunctionInfo {
    /// The PCI vendor ID.
    pub vendor_id: u16,
    /// The PCI device ID.
    pub device_id: u16,
    /// The PCI class.
    pub class: u8,
    /// The PCI subclass.
    pub subclass: u8,
    /// The PCI programming interface byte.
    pub prog_if: u8,
    /// The PCI revision ID.
    pub revision: u8,
    /// The type of PCI device.
    pub header_type: HeaderType,
}

impl Display for DeviceFunctionInfo {
    fn fmt(&self, f: &mut Formatter) -> fmt::Result {
        write!(
            f,
            "{:04x}:{:04x} (class {:02x}.{:02x}, rev {:02x}) {:?}",
            self.vendor_id,
            self.device_id,
            self.class,
            self.subclass,
            self.revision,
            self.header_type,
        )
    }
}

/// The type of a PCI device function header.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum HeaderType {
    /// A normal PCI device.
    Standard,
    /// A PCI to PCI bridge.
    PciPciBridge,
    /// A PCI to CardBus bridge.
    PciCardbusBridge,
    /// Unrecognised header type.
    Unrecognised(u8),
}

impl From<u8> for HeaderType {
    fn from(value: u8) -> Self {
        match value {
            0x00 => Self::Standard,
            0x01 => Self::PciPciBridge,
            0x02 => Self::PciCardbusBridge,
            _ => Self::Unrecognised(value),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_status_command() {
        let status_command = 0x0020_0003;
        let device_function = DeviceFunction {
            bus: 0,
            device: 1,
            function: 2,
        };
        let fake_cam = FakeCam {
            device_function,
            bar_values: [0, 1, 4, 0, 0, 0],
            bar_masks: [
                0xffffffff, 0xffffffff, 0xffffffff, 0xffffffff, 0xffffffff, 0xffffffff,
            ],
            status_command,
        };
        let root = PciRoot::new(fake_cam);

        assert_eq!(
            root.get_status_command(device_function),
            (
                Status::MHZ_66_CAPABLE,
                Command::IO_SPACE | Command::MEMORY_SPACE
            )
        );
    }

    /// BAR5 声称 64 位时必须报错，且不破坏原 decode 或资源寄存器。
    #[test]
    fn bar_info_invalid_pair_preserves_configuration() {
        let device_function = DeviceFunction {
            bus: 0,
            device: 1,
            function: 2,
        };
        let fake_cam = FakeCam {
            device_function,
            bar_values: [0, 0, 0, 0, 0, 4],
            bar_masks: [0; 6],
            status_command: 0x0020_0007,
        };
        let original = fake_cam.clone();
        let mut root = PciRoot::new(fake_cam);
        assert_eq!(
            root.bar_info(device_function, 5),
            Err(PciError::InvalidBarIndex)
        );
        assert_eq!(root.configuration_access, original);
    }

    /// 不连续的大小掩码在探测后失败，原 BAR 与 command 仍必须恢复。
    #[test]
    fn bar_info_invalid_size_preserves_configuration() {
        let device_function = DeviceFunction {
            bus: 0,
            device: 1,
            function: 2,
        };
        let fake_cam = FakeCam {
            device_function,
            bar_values: [0x4000_0000, 0, 0, 0, 0, 0],
            bar_masks: [0x50, 0, 0, 0, 0, 0],
            status_command: 0x0020_0007,
        };
        let original = fake_cam.clone();
        let mut root = PciRoot::new(fake_cam);
        assert_eq!(
            root.bar_info(device_function, 0),
            Err(PciError::InvalidBarSize)
        );
        assert_eq!(root.configuration_access, original);
    }

    #[test]
    fn bar_info_32() {
        let status_command = 0x0020_0003;
        let device_function = DeviceFunction {
            bus: 0,
            device: 1,
            function: 2,
        };
        let fake_cam = FakeCam {
            device_function,
            bar_values: [0b0000, 0b0010, 0b1000, 0b01, 0b0000, 0b0000],
            bar_masks: [63, 31, 127, 7, 1023, 255],
            status_command,
        };
        let fake_cam_orig = fake_cam.clone();
        let mut root = PciRoot::new(fake_cam);

        assert_eq!(
            root.bars(device_function).unwrap(),
            [
                Some(BarInfo::Memory {
                    address_type: MemoryBarType::Width32,
                    prefetchable: false,
                    address: 0,
                    size: 64,
                }),
                Some(BarInfo::Memory {
                    address_type: MemoryBarType::Below1MiB,
                    prefetchable: false,
                    address: 0,
                    size: 32,
                }),
                Some(BarInfo::Memory {
                    address_type: MemoryBarType::Width32,
                    prefetchable: true,
                    address: 0,
                    size: 128,
                }),
                Some(BarInfo::IO {
                    address: 0,
                    size: 8,
                }),
                Some(BarInfo::Memory {
                    address_type: MemoryBarType::Width32,
                    prefetchable: false,
                    address: 0,
                    size: 1024,
                }),
                Some(BarInfo::Memory {
                    address_type: MemoryBarType::Width32,
                    prefetchable: false,
                    address: 0,
                    size: 256,
                }),
            ]
        );

        // Status and command should be restored to their initial values, as should BAR values.
        assert_eq!(root.configuration_access, fake_cam_orig);
    }

    #[test]
    fn bar_info_64() {
        let status_command = 0x0020_0003;
        let device_function = DeviceFunction {
            bus: 0,
            device: 1,
            function: 2,
        };
        let fake_cam = FakeCam {
            device_function,
            bar_values: [0b0100, 0, 0b0100, 0, 0b1100, 0],
            bar_masks: [127, 0, 0xffffffff, 3, 255, 0],
            status_command,
        };
        let fake_cam_orig = fake_cam.clone();
        let mut root = PciRoot::new(fake_cam);

        assert_eq!(
            root.bars(device_function).unwrap(),
            [
                Some(BarInfo::Memory {
                    address_type: MemoryBarType::Width64,
                    prefetchable: false,
                    address: 0,
                    size: 128,
                }),
                None,
                Some(BarInfo::Memory {
                    address_type: MemoryBarType::Width64,
                    prefetchable: false,
                    address: 0,
                    size: 0x400000000,
                }),
                None,
                Some(BarInfo::Memory {
                    address_type: MemoryBarType::Width64,
                    prefetchable: true,
                    address: 0,
                    size: 256,
                }),
                None,
            ]
        );

        // Status and command should be restored to their initial values, as should BAR values.
        assert_eq!(root.configuration_access, fake_cam_orig);
    }

    /// 循环链必须终止为明确错误，不能反复返回同一 capability。
    #[test]
    fn capability_cycle_is_rejected() {
        let bdf = DeviceFunction {
            bus: 0,
            device: 0,
            function: 0,
        };
        let mut words = [0u32; 64];
        words[1] = u32::from(Status::CAPABILITIES_LIST.bits()) << 16;
        words[0x34 / 4] = 0x40;
        words[0x40 / 4] = 0x4401;
        words[0x44 / 4] = 0x4001;
        let root = PciRoot::new(CapabilityCam(words));
        let mut capabilities = root.capabilities(bdf);
        assert_eq!(
            capabilities.next(),
            Some(Ok(CapabilityInfo {
                offset: 0x40,
                id: 1,
                private_header: 0
            }))
        );
        assert_eq!(capabilities.next(), Some(Err(PciError::CapabilityCycle)));
        assert_eq!(capabilities.next(), None);
    }

    /// 原始低指针、未对齐指针与越过配置页尾的 vendor 长度都不能自动修正。
    #[test]
    fn capability_boundaries_are_rejected() {
        let bdf = DeviceFunction {
            bus: 0,
            device: 0,
            function: 0,
        };
        for pointer in [0x3c, 0x41, 0xfc] {
            let mut words = [0u32; 64];
            words[0] = 0x1af4;
            words[1] = u32::from(Status::CAPABILITIES_LIST.bits()) << 16;
            words[0x34 / 4] = pointer;
            words[0xfc / 4] = 0x0110_0009;
            let root = PciRoot::new(CapabilityCam(words));
            let mut capabilities = root.capabilities(bdf);
            let expected = if pointer == 0xfc {
                PciError::InvalidCapabilityLength
            } else {
                PciError::InvalidCapabilityOffset
            };
            assert_eq!(capabilities.next(), Some(Err(expected)));
            assert_eq!(capabilities.next(), None);
        }
    }

    #[derive(Clone)]
    struct CapabilityCam([u32; 64]);

    impl ConfigurationAccess for CapabilityCam {
        /// 测试配置页保持原始 byte offset，暴露链的真实边界错误。
        fn read_word(&self, _bdf: DeviceFunction, offset: u8) -> u32 {
            self.0[usize::from(offset) / 4]
        }

        /// 配置页允许普通测试写入，不把非法链修正为有效输入。
        fn write_word(&mut self, _bdf: DeviceFunction, offset: u8, data: u32) {
            self.0[usize::from(offset) / 4] = data;
        }

        /// 枚举只读测试页时保留相同数据。
        ///
        /// # Safety
        /// 调用者必须遵守 ConfigurationAccess 的只读副本约束。
        unsafe fn unsafe_clone(&self) -> Self {
            self.clone()
        }
    }

    #[derive(Clone, Debug, Eq, PartialEq)]
    struct FakeCam {
        device_function: DeviceFunction,
        bar_values: [u32; 6],
        // Bits which can't be changed.
        bar_masks: [u32; 6],
        status_command: u32,
    }

    impl ConfigurationAccess for FakeCam {
        /// 模拟标准配置头与可写 BAR，用于验证探测后的完整恢复。
        fn read_word(&self, device_function: DeviceFunction, register_offset: u8) -> u32 {
            assert_eq!(device_function, self.device_function);
            assert_eq!(register_offset & 0b11, 0);
            if register_offset == STATUS_COMMAND_OFFSET {
                self.status_command
            } else if register_offset == 0x0c {
                0
            } else if register_offset >= BAR0_OFFSET && register_offset < 0x28 {
                let bar_index = usize::from((register_offset - BAR0_OFFSET) / 4);
                self.bar_values[bar_index]
            } else {
                println!("Reading unsupported register offset {}", register_offset);
                0xffffffff
            }
        }

        fn write_word(&mut self, device_function: DeviceFunction, register_offset: u8, data: u32) {
            assert_eq!(device_function, self.device_function);
            assert_eq!(register_offset & 0b11, 0);
            if register_offset == STATUS_COMMAND_OFFSET {
                // Ignore write to status, only write to command.
                self.status_command = (self.status_command & 0xffff_0000) | (data & 0x0000_ffff);
            } else if register_offset >= BAR0_OFFSET && register_offset < 0x28 {
                let bar_index = usize::from((register_offset - BAR0_OFFSET) / 4);
                let bar_mask = self.bar_masks[bar_index];
                self.bar_values[bar_index] =
                    (bar_mask & self.bar_values[bar_index]) | (!bar_mask & data);
            } else {
                println!("Ignoring write of {:#010x} to {}", data, register_offset);
                return;
            }
        }

        unsafe fn unsafe_clone(&self) -> Self {
            self.clone()
        }
    }
}
