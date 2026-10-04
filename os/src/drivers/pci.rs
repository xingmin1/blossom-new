//! 单次 root-bus 资源发现，配置访问与 BAR 地址域由同一个 owner 保存。

use alloc::vec::Vec;
use core::convert::TryFrom;
use core::sync::atomic::{AtomicBool, Ordering};
use virtio_drivers::transport::pci::bus::{
    BarInfo, Cam, Command, DeviceFunction, HeaderType, MemoryBarType, MmioCam, PciRoot,
};

use crate::{
    boot::{self, BootInfo, PciRange, PciResources, Region},
    drivers::{mmio_region_address, ProbeError},
    irq::{self, IrqNumber},
    task,
};

static DISCOVERED: AtomicBool = AtomicBool::new(false);

/// 内存 BAR 的总线地址和已验证 CPU 物理窗口；64 位配对的高槽保留为空。
#[derive(Clone, Copy, Debug)]
pub struct PciBar {
    pub bus_address: u64,
    pub physical: Region,
    pub prefetchable: bool,
}

/// 有界 capability 链中的原始头，后续 transport 只读取此选定功能。
#[derive(Clone, Copy, Debug)]
pub struct PciCapability {
    pub offset: u8,
    pub id: u8,
    pub private_header: u16,
}

/// root bus 上一个实际功能的不可变资源，不以全局当前设备传递身份。
#[derive(Debug)]
pub struct PciDevice {
    pub bus: u8,
    pub device: u8,
    pub function: u8,
    pub vendor_id: u16,
    pub device_id: u16,
    pub header_type: u8,
    pub bars: [Option<PciBar>; 6],
    pub capabilities: [Option<PciCapability>; 48],
    pub capability_count: usize,
    pub interrupt: Option<IrqNumber>,
}

impl PciDevice {
    /// 返回此功能的配置地址，资源身份始终包含总线、设备与功能。
    fn bdf(&self) -> DeviceFunction {
        DeviceFunction {
            bus: self.bus,
            device: self.device,
            function: self.function,
        }
    }
}

/// 唯一配置窗口 owner 与按 BDF 排序的资源快照。
pub struct PciInventory {
    root: PciRoot<MmioCam<'static>>,
    devices: Vec<PciDevice>,
}

struct PendingDevice {
    device: PciDevice,
    command: Command,
    original_bars: [u32; 6],
    bar_info: [Option<BarInfo>; 6],
}

#[derive(Clone, Copy)]
struct Occupied {
    domain: u8,
    start: u64,
    end: u64,
}

impl PciInventory {
    /// 普通 Task 一次枚举已发布 host，锁外 fallible 准备全部资源后才配置设备。
    pub fn discover(info: &'static BootInfo) -> Result<Self, ProbeError> {
        drop(task::current_task());
        if !core::ptr::eq(info, boot::info()) {
            return Err(ProbeError::Resource(
                "PCI requires the published boot snapshot",
            ));
        }
        if DISCOVERED.swap(true, Ordering::AcqRel) {
            return Err(ProbeError::AlreadyDiscovered);
        }
        if info.pci.bus_start != 0 {
            return Err(ProbeError::Resource("PCI root bus must start at zero"));
        }
        let configuration = info.pci.configuration;
        let virtual_address = mmio_region_address(configuration)?;
        // SAFETY: published BootInfo 已在 AP 前安装真实 ECAM 映射；唯一 discover
        // 准入保存独占配置 owner，不把区间借为普通 Rust 引用，寿命覆盖整个内核。
        let cam = unsafe {
            MmioCam::new(
                virtual_address as *mut u8,
                configuration.end - configuration.start,
                Cam::Ecam,
            )?
        };
        let mut root = PciRoot::new(cam);
        let mut pending = collect_devices(&root, info)?;
        let mut devices = Vec::new();
        devices
            .try_reserve_exact(pending.len())
            .map_err(|_| ProbeError::NoMemory)?;
        let mut occupied = Vec::new();
        let capacity = pending.len().checked_mul(6).ok_or(ProbeError::NoMemory)?;
        occupied
            .try_reserve_exact(capacity)
            .map_err(|_| ProbeError::NoMemory)?;
        if let Err(error) = configure_resources(&mut root, &mut pending, &mut occupied, &info.pci) {
            restore_configuration(&mut root, &pending);
            return Err(error);
        }
        for entry in pending {
            devices.push(entry.device);
        }
        Ok(Self { root, devices })
    }

    /// 借用稳定 BDF 顺序的所有 root-bus 功能，不重新枚举 host。
    pub fn devices(&self) -> &[PciDevice] {
        &self.devices
    }

    /// 读取本 inventory 中选定功能的只读配置字，拒绝越界、未对齐与外来身份。
    pub fn read_config_word(&self, device: &PciDevice, offset: u8) -> Result<u32, ProbeError> {
        if offset & 3 != 0
            || !self
                .devices
                .iter()
                .any(|entry| core::ptr::eq(entry, device))
        {
            return Err(ProbeError::Resource(
                "invalid selected PCI configuration access",
            ));
        }
        // 不暴露可能 read-to-clear 或可写字段的可复制 CAM；此入口仅服务身份与 cap。
        if !matches!(offset, 0x00 | 0x08 | 0x0c | 0x2c | 0x34 | 0x3c) && offset < 0x40 {
            return Err(ProbeError::Resource(
                "PCI configuration field is not read-only inventory data",
            ));
        }
        Ok(self.root.read_config_word(device.bdf(), offset))
    }
}

/// 在修改配置之前完成身份、cap 与 IRQ 映射，增长只发生于普通 Task。
fn collect_devices(
    root: &PciRoot<MmioCam<'static>>,
    info: &BootInfo,
) -> Result<Vec<PendingDevice>, ProbeError> {
    let mut pending = Vec::new();
    for (bdf, identity) in root.enumerate_bus(0) {
        if identity.header_type != HeaderType::Standard {
            return Err(ProbeError::Resource(
                "PCI bridge or unsupported header is outside root-bus discovery",
            ));
        }
        pending.try_reserve(1).map_err(|_| ProbeError::NoMemory)?;
        let header_type = (root.read_config_word(bdf, 0x0c) >> 16) as u8;
        let pin = (root.read_config_word(bdf, 0x3c) >> 8) as u8;
        let interrupt = if pin == 0 {
            None
        } else {
            let source = info
                .pci
                .interrupt_source(bdf.bus, bdf.device, bdf.function, pin)
                .map_err(ProbeError::Resource)?;
            Some(irq::map_source(source)?)
        };
        let mut capabilities = [None; 48];
        let mut capability_count = 0;
        for capability in root.capabilities(bdf) {
            let capability = capability?;
            if capability_count == capabilities.len() {
                return Err(ProbeError::Resource("PCI capability capacity exceeded"));
            }
            capabilities[capability_count] = Some(PciCapability {
                offset: capability.offset,
                id: capability.id,
                private_header: capability.private_header,
            });
            capability_count += 1;
        }
        let original_bars =
            core::array::from_fn(|index| root.read_config_word(bdf, 0x10 + index as u8 * 4));
        pending.push(PendingDevice {
            device: PciDevice {
                bus: bdf.bus,
                device: bdf.device,
                function: bdf.function,
                vendor_id: identity.vendor_id,
                device_id: identity.device_id,
                header_type,
                bars: [None; 6],
                capabilities,
                capability_count,
                interrupt,
            },
            command: root.get_status_command(bdf).1,
            original_bars,
            bar_info: core::array::from_fn(|_| None),
        });
    }
    // 枚举器天然按 BDF 前进，保持此顺序避免发现完成时间改变设备编号。
    Ok(pending)
}

/// 先探测全部 BAR 并保留全部既存占用，再对未配置内存 BAR 做统一分配。
fn configure_resources(
    root: &mut PciRoot<MmioCam<'static>>,
    pending: &mut [PendingDevice],
    occupied: &mut Vec<Occupied>,
    host: &PciResources,
) -> Result<(), ProbeError> {
    let decode = Command::IO_SPACE | Command::MEMORY_SPACE | Command::BUS_MASTER;
    // 保存的原始配置覆盖全部函数；确认关闭以后才允许尺寸探测。
    for entry in pending.iter() {
        root.set_command(entry.device.bdf(), entry.command & !decode);
        if root
            .get_status_command(entry.device.bdf())
            .1
            .intersects(decode)
        {
            return Err(ProbeError::Resource(
                "PCI device did not disable address decoding",
            ));
        }
    }
    for entry in pending.iter_mut() {
        entry.bar_info = root.bars(entry.device.bdf())?;
    }
    for entry in pending.iter() {
        for bar in entry.bar_info.iter().flatten() {
            let (domain, address, size) = bar_address(bar);
            if address != 0 {
                validate_bar_address(bar, address)?;
                translate(host, domain, address, size)?;
                reserve(occupied, domain, address, size)?;
            }
        }
    }
    for entry in pending.iter_mut() {
        for (index, bar) in entry.bar_info.iter().enumerate() {
            let Some(bar) = bar else { continue };
            let BarInfo::Memory {
                address_type,
                prefetchable,
                address,
                size,
            } = bar
            else {
                continue;
            };
            let address = if *address == 0 {
                allocate(host, occupied, *address_type, *size)?
            } else {
                *address
            };
            let physical = translate(host, 2, address, *size)?;
            mmio_region_address(physical)?;
            entry.device.bars[index] = Some(PciBar {
                bus_address: address,
                physical,
                prefetchable: *prefetchable,
            });
        }
    }
    // 全部地址和占用已成功验证，之后的配置写入不分配。
    for entry in pending.iter() {
        for (index, selected) in entry.device.bars.iter().enumerate() {
            let Some(selected) = selected else { continue };
            let bdf = entry.device.bdf();
            let wide = matches!(
                entry.bar_info[index],
                Some(BarInfo::Memory {
                    address_type: MemoryBarType::Width64,
                    ..
                })
            );
            if wide {
                root.set_bar_64(bdf, index as u8, selected.bus_address);
            } else {
                root.set_bar_32(bdf, index as u8, selected.bus_address as u32);
            }
            let low = root.read_config_word(bdf, 0x10 + index as u8 * 4) & !15;
            let high = if wide {
                root.read_config_word(bdf, 0x14 + index as u8 * 4)
            } else {
                0
            };
            if (u64::from(low) | (u64::from(high) << 32)) != selected.bus_address {
                return Err(ProbeError::Resource(
                    "PCI BAR configuration readback differs",
                ));
            }
        }
    }
    for entry in pending.iter() {
        let mut command = entry.command & !decode;
        if entry.device.bars.iter().any(Option::is_some) {
            command |= Command::MEMORY_SPACE | Command::BUS_MASTER;
        }
        root.set_command(entry.device.bdf(), command);
        if root.get_status_command(entry.device.bdf()).1 & decode != command & decode {
            return Err(ProbeError::Resource(
                "PCI command configuration readback differs",
            ));
        }
    }
    Ok(())
}

/// 配置失败恢复原地址后再恢复 command，不在部分配置状态启用 decode。
fn restore_configuration(root: &mut PciRoot<MmioCam<'static>>, pending: &[PendingDevice]) {
    for entry in pending {
        root.set_command(
            entry.device.bdf(),
            entry.command & !(Command::IO_SPACE | Command::MEMORY_SPACE | Command::BUS_MASTER),
        );
    }
    for entry in pending {
        let mut index = 0;
        while index < 6 {
            let original = entry.original_bars[index];
            if original & 7 == 4 && index < 5 {
                let address =
                    u64::from(original) | (u64::from(entry.original_bars[index + 1]) << 32);
                root.set_bar_64(entry.device.bdf(), index as u8, address);
                index += 2;
            } else {
                root.set_bar_32(entry.device.bdf(), index as u8, original);
                index += 1;
            }
        }
    }
    for entry in pending {
        root.set_command(entry.device.bdf(), entry.command);
    }
}

/// 把 BAR 类型转换为占用地址域；32/64 位内存 BAR 共享同一内存地址域。
fn bar_address(bar: &BarInfo) -> (u8, u64, u64) {
    match bar {
        BarInfo::IO { address, size } => (1, u64::from(*address), u64::from(*size)),
        BarInfo::Memory { address, size, .. } => (2, *address, *size),
    }
}

/// 检查对齐、大小和 BAR 编码的可寻址上限，不截断真实总线地址。
fn validate_bar_address(bar: &BarInfo, address: u64) -> Result<(), ProbeError> {
    let (_, _, size) = bar_address(bar);
    let end = address
        .checked_add(size)
        .ok_or(ProbeError::Resource("PCI BAR address overflows"))?;
    let limit = match bar {
        BarInfo::Memory {
            address_type: MemoryBarType::Width64,
            ..
        } => u64::MAX,
        BarInfo::Memory {
            address_type: MemoryBarType::Below1MiB,
            ..
        } => 1 << 20,
        _ => 1 << 32,
    };
    if !size.is_power_of_two() || address & (size - 1) != 0 || limit < end {
        return Err(ProbeError::Resource(
            "PCI BAR size, alignment or address width is invalid",
        ));
    }
    Ok(())
}

/// 返回 PCI range 所属 IO 或内存地址域，属性位不改变地址域身份。
fn range_domain(range: &PciRange) -> u8 {
    match (range.flags >> 24) & 3 {
        1 => 1,
        2 | 3 => 2,
        _ => 0,
    }
}

/// checked bus→CPU PA 换算，只接受 host 实际提供的完整窗口。
fn translate(
    host: &PciResources,
    domain: u8,
    address: u64,
    size: u64,
) -> Result<Region, ProbeError> {
    for range in &host.ranges[..host.range_count] {
        if range_domain(range) != domain {
            continue;
        }
        let start = range.bus_start as u64;
        let Some(offset) = address.checked_sub(start) else {
            continue;
        };
        let length = (range.cpu.end - range.cpu.start) as u64;
        let end = offset
            .checked_add(size)
            .ok_or(ProbeError::Resource("PCI range offset overflows"))?;
        if length < end {
            continue;
        }
        let offset = usize::try_from(offset)
            .map_err(|_| ProbeError::Resource("PCI offset exceeds CPU address width"))?;
        let size = usize::try_from(size)
            .map_err(|_| ProbeError::Resource("PCI size exceeds CPU address width"))?;
        let physical = range
            .cpu
            .start
            .checked_add(offset)
            .ok_or(ProbeError::Resource("PCI CPU address overflows"))?;
        return Region::from_size(physical, size).map_err(ProbeError::Resource);
    }
    Err(ProbeError::Resource(
        "PCI BAR is outside allowed host ranges",
    ))
}

/// 保存既存或新分配 BAR 的占用，拒绝同域重叠；容量在探测前已经准备。
fn reserve(
    occupied: &mut Vec<Occupied>,
    domain: u8,
    start: u64,
    size: u64,
) -> Result<(), ProbeError> {
    let end = start
        .checked_add(size)
        .ok_or(ProbeError::Resource("PCI BAR occupation overflows"))?;
    if occupied
        .iter()
        .any(|old| old.domain == domain && start < old.end && old.start < end)
    {
        return Err(ProbeError::Resource("PCI BAR occupations overlap"));
    }
    occupied.push(Occupied { domain, start, end });
    Ok(())
}

/// 所有未配置内存 BAR 固定在 MEM32 窗口找空洞；64 位 BAR 也合法使用低地址。
fn allocate(
    host: &PciResources,
    occupied: &mut Vec<Occupied>,
    width: MemoryBarType,
    size: u64,
) -> Result<u64, ProbeError> {
    if !size.is_power_of_two() {
        return Err(ProbeError::Resource("PCI allocation size is invalid"));
    }
    let width_limit = match width {
        MemoryBarType::Width64 => u64::MAX,
        MemoryBarType::Width32 => 1 << 32,
        MemoryBarType::Below1MiB => 1 << 20,
    };
    for range in &host.ranges[..host.range_count] {
        if (range.flags >> 24) & 3 != 2 {
            continue;
        }
        let limit = (range.bus_start as u64)
            .checked_add((range.cpu.end - range.cpu.start) as u64)
            .ok_or(ProbeError::Resource("PCI bus range overflows"))?
            .min(width_limit);
        let mut candidate = (range.bus_start as u64).max(size);
        loop {
            candidate = candidate
                .checked_add(size - 1)
                .ok_or(ProbeError::Resource("PCI BAR alignment overflows"))?
                & !(size - 1);
            let end = candidate
                .checked_add(size)
                .ok_or(ProbeError::Resource("PCI allocation overflows"))?;
            if limit < end {
                break;
            }
            let collision = occupied
                .iter()
                .filter(|old| old.domain == 2 && candidate < old.end && old.start < end)
                .map(|old| old.end)
                .max();
            if let Some(next) = collision {
                candidate = next;
                continue;
            }
            reserve(occupied, 2, candidate, size)?;
            return Ok(candidate);
        }
    }
    Err(ProbeError::Resource(
        "PCI memory windows have no free aligned BAR extent",
    ))
}
