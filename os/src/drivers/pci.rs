//! 单次 root-bus 资源发现，配置访问与 BAR 地址域由同一个 owner 保存。

use alloc::vec::Vec;
use core::convert::TryFrom;
use core::sync::atomic::{AtomicBool, Ordering};
use virtio_drivers::transport::pci::bus::{
    BarInfo, Cam, Command, ConfigurationAccess, DeviceFunction, DeviceFunctionInfo, HeaderType,
    MemoryBarType, MmioCam, PciRoot,
};

use crate::{
    boot::{
        self, AhciResources, BootInfo, GmacResources, PciConfiguration, PciFunctionResource,
        PciRange, PciResources, Region,
    },
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
    /// 首次枚举读取的 class/subclass/progif，不保存未使用的 revision。
    class_code: u32,
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
    root: Option<PciRoot<HostCam>>,
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

/// 唯一 host 配置 owner；所选 LS2K 使用 CFG1，ECAM 保持独立地址公式。
enum HostCam {
    Ecam(MmioCam<'static>),
    Loongson {
        base: usize,
        length: usize,
        bus_end: u8,
    },
}

/// 计算 Loongson CFG0/CFG1 的字偏移，包含真实硬件禁止访问的根功能。
fn loongson_offset(bdf: DeviceFunction, offset: u16, extended: bool) -> Option<usize> {
    if 32 <= bdf.device
        || 8 <= bdf.function
        || offset & 3 != 0
        || (!extended && 256 <= offset)
        || 4096 <= offset
        || (bdf.bus == 0 && (9..=20).contains(&bdf.device) && bdf.function != 0)
    {
        return None;
    }
    let devfn = ((bdf.device as usize) << 3) | bdf.function as usize;
    let mut address = (devfn << 8) | (offset as usize & 0xff);
    if extended {
        address |= (offset as usize & 0xf00) << 16;
    }
    if bdf.bus != 0 {
        address |= (bdf.bus as usize) << 16;
        address |= if extended { 1 << 28 } else { 1 << 24 };
    }
    Some(address)
}

impl ConfigurationAccess for HostCam {
    /// 配置只读字段由 PciRoot 借用访问，越过 host 窗口按 PCI 无设备语义返回。
    fn read_word(&self, bdf: DeviceFunction, offset: u8) -> u32 {
        match self {
            Self::Ecam(cam) => cam.read_word(bdf, offset),
            Self::Loongson {
                base,
                length,
                bus_end,
            } => {
                if *bus_end < bdf.bus {
                    return u32::MAX;
                }
                let Some(at) = loongson_offset(bdf, offset as u16, true) else {
                    return u32::MAX;
                };
                if *length < at + 4 {
                    return u32::MAX;
                }
                // SAFETY: 唯一库存持有永久强序 CFG1 映射；对齐偏移和完整 u32 已验证在窗口内。
                unsafe { ((*base + at) as *const u32).read_volatile() }
            }
        }
    }

    /// 独占配置写保持唯一库存所有权，不产生可复制的可写 CAM。
    fn write_word(&mut self, bdf: DeviceFunction, offset: u8, data: u32) {
        match self {
            Self::Ecam(cam) => cam.write_word(bdf, offset, data),
            Self::Loongson {
                base,
                length,
                bus_end,
            } => {
                assert!(bdf.bus <= *bus_end, "PCI bus exceeds Loongson host");
                let at = loongson_offset(bdf, offset as u16, true)
                    .expect("invalid Loongson configuration write");
                assert!(
                    at + 4 <= *length,
                    "Loongson configuration write exceeds CFG1"
                );
                // SAFETY: &mut CAM 保证配置写独占；对齐完整 u32 位于永久强序 CFG1 窗口内。
                unsafe { ((*base + at) as *mut u32).write_volatile(data) };
            }
        }
    }

    /// 为 PciRoot 的只读枚举创建同一映射的借用副本。
    ///
    /// # Safety
    /// 调用者只能通过副本读取无副作用配置字段，且不得与原 owner 的配置写并发。
    unsafe fn unsafe_clone(&self) -> Self {
        match self {
            // SAFETY: 调用者继承 ConfigurationAccess 的只读与不并发写约束，原映射永久有效。
            Self::Ecam(cam) => Self::Ecam(unsafe { cam.unsafe_clone() }),
            Self::Loongson {
                base,
                length,
                bus_end,
            } => Self::Loongson {
                base: *base,
                length: *length,
                bus_end: *bus_end,
            },
        }
    }
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
        if info.pci.kind == PciConfiguration::Absent {
            return Ok(Self {
                root: None,
                devices: Vec::new(),
            });
        }
        if info.pci.bus_start != 0 {
            return Err(ProbeError::Resource("PCI root bus must start at zero"));
        }
        let configuration = match info.pci.kind {
            PciConfiguration::Ecam => info.pci.configuration,
            PciConfiguration::Loongson => info.pci.configuration_extended,
            PciConfiguration::Absent => unreachable!(),
        };
        let virtual_address = mmio_region_address(configuration)?;
        let cam = match info.pci.kind {
            PciConfiguration::Ecam => {
                // SAFETY: 已发布的真实 ECAM 在 AP 前永久映射，唯一库存持配置写拥有到停止尾部。
                HostCam::Ecam(unsafe {
                    MmioCam::new(
                        virtual_address as *mut u8,
                        configuration.end - configuration.start,
                        Cam::Ecam,
                    )?
                })
            }
            PciConfiguration::Loongson => HostCam::Loongson {
                base: virtual_address,
                length: configuration.end - configuration.start,
                bus_end: info.pci.bus_end as u8,
            },
            PciConfiguration::Absent => unreachable!(),
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
        if let Err(error) = configure_resources(&mut root, &mut pending, &mut occupied, info) {
            restore_configuration(&mut root, &pending);
            return Err(error);
        }
        for entry in pending {
            devices.push(entry.device);
        }
        Ok(Self {
            root: Some(root),
            devices,
        })
    }

    /// 借用稳定 BDF 顺序的所有 root-bus 功能，不重新枚举 host。
    pub fn devices(&self) -> &[PciDevice] {
        &self.devices
    }

    /// 从唯一已配置库存解析所选 AHCI；只读快照，不重新探测或修改 command。
    pub fn resolve_ahci(&self, resources: &AhciResources) -> Result<AhciResources, ProbeError> {
        let mut resolved = *resources;
        resolved.registers = resolve_registers(
            self.devices.iter(),
            resources.pci,
            resources.registers,
            resources.interrupt_source,
            resources.dma_coherent,
            ahci_bar,
        )?;
        Ok(resolved)
    }

    /// 从唯一已配置库存解析所选 GMAC，保留直接 MMIO 的身份与 PHY 配置。
    pub fn resolve_gmac(&self, resources: &GmacResources) -> Result<GmacResources, ProbeError> {
        let mut resolved = *resources;
        resolved.registers = resolve_registers(
            self.devices.iter(),
            resources.pci,
            resources.registers,
            resources.interrupt_source,
            resources.dma_coherent,
            gmac_bar,
        )?;
        Ok(resolved)
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
        Ok(self
            .root
            .as_ref()
            .ok_or(ProbeError::Resource("PCI host is absent"))?
            .read_config_word(device.bdf(), offset))
    }
}

/// Loongson AHCI 使用 BAR0；其他已明确指定 BDF 的标准 AHCI 功能使用 BAR5。
fn ahci_bar(vendor: u16, device: u16, class_code: u32) -> Result<usize, ProbeError> {
    if vendor == 0x0014 && device == 0x7a08 {
        return Ok(0);
    }
    if class_code == 0x010601 {
        return Ok(5);
    }
    Err(ProbeError::Resource(
        "selected PCI function is not a supported AHCI device",
    ))
}

/// 两种单通道 Loongson GMAC 均使用 BAR0，不接受具有不同 DMA 格式的 GNET。
fn gmac_bar(vendor: u16, device: u16, _class_code: u32) -> Result<usize, ProbeError> {
    if vendor == 0x0014 && matches!(device, 0x7a03 | 0x7a23) {
        return Ok(0);
    }
    Err(ProbeError::Resource(
        "selected PCI function is not a supported GMAC device",
    ))
}

/// 直接窗口必须从寄存器零偏移开始且完全位于 BAR 内；结果总是完整 CPU 物理 BAR。
fn normalize_window(bar: Region, described: Region, pci: bool) -> Result<Region, ProbeError> {
    if bar.end <= bar.start
        || (!(pci && described == Region::default())
            && (described.start != bar.start
                || described.end <= described.start
                || bar.end < described.end))
    {
        return Err(ProbeError::Resource(
            "native register window differs from selected PCI BAR",
        ));
    }
    Ok(bar)
}

/// 按完整 BDF 或直接物理窗口找唯一功能，核对 IRQ 与整个映射；没有 PCI 的 MMIO 保持独立。
fn resolve_registers<'a>(
    devices: impl Iterator<Item = &'a PciDevice>,
    selected: Option<PciFunctionResource>,
    registers: Region,
    source: u32,
    coherent: bool,
    select_bar: fn(u16, u16, u32) -> Result<usize, ProbeError>,
) -> Result<Region, ProbeError> {
    let interrupt = irq::map_source(source)?;
    if let Some(identity) = selected {
        if identity.interrupt_source != source || identity.dma_coherent != coherent {
            return Err(ProbeError::Resource(
                "native PCI identity and device resources differ",
            ));
        }
    } else if registers.end <= registers.start {
        return Err(ProbeError::Resource(
            "native MMIO register window is missing",
        ));
    }
    let mut found = None;
    for device in devices {
        let index = if let Some(identity) = selected {
            if device.bus != identity.bus
                || device.device != identity.device
                || device.function != identity.function
            {
                continue;
            }
            select_bar(device.vendor_id, device.device_id, device.class_code)?
        } else {
            let Ok(index) = select_bar(device.vendor_id, device.device_id, device.class_code)
            else {
                continue;
            };
            if !device.bars[index].is_some_and(|bar| bar.physical.overlaps(registers)) {
                continue;
            }
            index
        };
        if found.is_some() {
            return Err(ProbeError::Resource("native PCI resource is not unique"));
        }
        let bar = device.bars[index].ok_or(ProbeError::Resource(
            "selected native PCI memory BAR is absent",
        ))?;
        let physical = normalize_window(bar.physical, registers, selected.is_some())?;
        if device.interrupt != Some(interrupt) {
            return Err(ProbeError::Resource(
                "native PCI interrupt differs from device resources",
            ));
        }
        found = Some(physical);
    }
    let physical = match found {
        Some(physical) => physical,
        None if selected.is_some() => {
            return Err(ProbeError::Resource(
                "selected native PCI function is absent",
            ));
        }
        None => registers,
    };
    mmio_region_address(physical)?;
    Ok(physical)
}

/// 在修改配置之前完成身份、cap 与 IRQ 映射，增长只发生于普通 Task。
fn collect_devices(
    root: &PciRoot<HostCam>,
    info: &BootInfo,
) -> Result<Vec<PendingDevice>, ProbeError> {
    let mut pending = Vec::new();
    for (bdf, identity) in root.enumerate_bus(0) {
        let native_source = if info.pci.kind == PciConfiguration::Loongson {
            let source = selected_native(root, info, bdf, &identity)?;
            if source.is_none() {
                continue;
            }
            source
        } else {
            None
        };
        if identity.header_type != HeaderType::Standard {
            return Err(ProbeError::Resource(
                "PCI bridge or unsupported header is outside root-bus discovery",
            ));
        }
        pending.try_reserve(1).map_err(|_| ProbeError::NoMemory)?;
        let header_type = (root.read_config_word(bdf, 0x0c) >> 16) as u8;
        let pin = (root.read_config_word(bdf, 0x3c) >> 8) as u8;
        let interrupt = if let Some(source) = native_source {
            Some(irq::map_source(source)?)
        } else if pin == 0 {
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
                class_code: (u32::from(identity.class) << 16)
                    | (u32::from(identity.subclass) << 8)
                    | u32::from(identity.prog_if),
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
    if info.pci.kind == PciConfiguration::Loongson {
        for selected in info
            .board
            .ahci
            .and_then(|ahci| ahci.pci)
            .into_iter()
            .chain(info.board.gmac.and_then(|gmac| gmac.pci))
        {
            if !pending.iter().any(|entry| {
                entry.device.bus == selected.bus
                    && entry.device.device == selected.device
                    && entry.device.function == selected.function
            }) {
                return Err(ProbeError::Resource(
                    "selected native PCI function is absent",
                ));
            }
        }
    }
    // 枚举器天然按 BDF 前进，保持此顺序避免发现完成时间改变设备编号。
    Ok(pending)
}

/// Loongson 只接管显式 BDF 或已知功能 BAR 与直接窗口精确同址的设备，保留其他集成功能。
fn selected_native(
    root: &PciRoot<HostCam>,
    info: &BootInfo,
    bdf: DeviceFunction,
    identity: &DeviceFunctionInfo,
) -> Result<Option<u32>, ProbeError> {
    let class_code = (u32::from(identity.class) << 16)
        | (u32::from(identity.subclass) << 8)
        | u32::from(identity.prog_if);
    let ahci = info.board.ahci.map(|resources| {
        (
            resources.pci,
            resources.registers,
            resources.interrupt_source,
            ahci_bar(identity.vendor_id, identity.device_id, class_code),
        )
    });
    let gmac = info.board.gmac.map(|resources| {
        (
            resources.pci,
            resources.registers,
            resources.interrupt_source,
            gmac_bar(identity.vendor_id, identity.device_id, class_code),
        )
    });
    let mut source = None;
    for (selected, registers, interrupt, bar) in IntoIterator::into_iter([ahci, gmac]).flatten() {
        if let Some(selected) = selected {
            if bdf.bus != selected.bus
                || bdf.device != selected.device
                || bdf.function != selected.function
            {
                continue;
            }
            bar?;
            // 仅已知集成 AHCI/GMAC 的错误 pin 按功能号修正，其他功能使用配置声明。
            let pin =
                if identity.vendor_id == 0x0014 && matches!(identity.device_id, 0x7a08 | 0x7a03) {
                    1 + (bdf.function & 3)
                } else {
                    (root.read_config_word(bdf, 0x3c) >> 8) as u8
                };
            let routed = info
                .pci
                .interrupt_source(bdf.bus, bdf.device, bdf.function, pin)
                .map_err(ProbeError::Resource)?;
            if routed != interrupt || selected.interrupt_source != interrupt {
                return Err(ProbeError::Resource(
                    "selected native PCI interrupt route differs",
                ));
            }
        } else {
            let Ok(index) = bar else { continue };
            let low = root.read_config_word(bdf, 0x10 + index as u8 * 4);
            if low & 1 != 0 || !matches!((low >> 1) & 3, 0 | 2) {
                continue;
            }
            let high = if low & 6 == 4 {
                root.read_config_word(bdf, 0x14 + index as u8 * 4)
            } else {
                0
            };
            let address = u64::from(low & !15) | (u64::from(high) << 32);
            if address == 0 {
                continue;
            }
            let physical = translate(&info.pci, 2, address, 1)?;
            if physical.start != registers.start {
                continue;
            }
        }
        if source.replace(interrupt).is_some() {
            return Err(ProbeError::Resource(
                "native PCI function has multiple selected resources",
            ));
        }
    }
    Ok(source)
}

/// 一次探测与分配全部 BAR，原生窗口和 IRQ 验证成功后才写入地址并启用 DMA。
fn configure_resources(
    root: &mut PciRoot<HostCam>,
    pending: &mut [PendingDevice],
    occupied: &mut Vec<Occupied>,
    info: &BootInfo,
) -> Result<(), ProbeError> {
    let host = &info.pci;
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
    if let Some(resources) = info.board.ahci {
        resolve_registers(
            pending.iter().map(|entry| &entry.device),
            resources.pci,
            resources.registers,
            resources.interrupt_source,
            resources.dma_coherent,
            ahci_bar,
        )?;
    }
    if let Some(resources) = info.board.gmac {
        resolve_registers(
            pending.iter().map(|entry| &entry.device),
            resources.pci,
            resources.registers,
            resources.interrupt_source,
            resources.dma_coherent,
            gmac_bar,
        )?;
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
fn restore_configuration(root: &mut PciRoot<HostCam>, pending: &[PendingDevice]) {
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

#[cfg(feature = "ktest")]
#[::ktest::suite(context = "Boot", tags("boot", "pci"), timeout_ms = 1000)]
mod configuration_checks {
    use super::*;

    /// 两种 Loongson 配置空间与隐藏功能边界保持硬件绑定的独立地址公式。
    #[test]
    fn loongson_configuration_offsets() {
        let sata = DeviceFunction {
            bus: 0,
            device: 8,
            function: 0,
        };
        assert_eq!(loongson_offset(sata, 0x10, false), Some(0x4010));
        assert_eq!(loongson_offset(sata, 0x310, true), Some(0x0300_4010));
        assert_eq!(loongson_offset(sata, 0x310, false), None);
        let secondary = DeviceFunction {
            bus: 2,
            device: 0,
            function: 0,
        };
        assert_eq!(loongson_offset(secondary, 0x10, false), Some(0x0102_0010));
        assert_eq!(loongson_offset(secondary, 0x310, true), Some(0x1302_0010));
        let hidden = DeviceFunction {
            bus: 0,
            device: 9,
            function: 1,
        };
        assert_eq!(loongson_offset(hidden, 0, false), None);
    }

    /// BAR 规则由实际设备身份或显式 AHCI 类别确定，不随其他非空 BAR 改变。
    #[test]
    fn native_device_bar_rules() {
        assert_eq!(ahci_bar(0x0014, 0x7a08, 0).unwrap(), 0);
        assert_eq!(ahci_bar(0x8086, 0x2922, 0x010601).unwrap(), 5);
        assert!(ahci_bar(0x0014, 0x7a03, 0x020000).is_err());
        assert!(ahci_bar(0x8086, 0x2922, 0x010600).is_err());
        assert_eq!(gmac_bar(0x0014, 0x7a03, 0x020000).unwrap(), 0);
        assert_eq!(gmac_bar(0x0014, 0x7a23, 0x020000).unwrap(), 0);
        assert!(gmac_bar(0x0014, 0x7a13, 0x020000).is_err());
        assert!(gmac_bar(0x1234, 0x7a03, 0x020000).is_err());
    }

    /// 配置身份不充当 MMIO；直接窗口只允许同起点的有界描述并返回完整 BAR。
    #[test]
    fn native_complete_register_windows() {
        let bar = Region::from_size(0x400e_0000, 0x10000).unwrap();
        assert_eq!(normalize_window(bar, Region::default(), true).unwrap(), bar);
        assert_eq!(normalize_window(bar, bar, true).unwrap(), bar);
        let direct = Region::from_size(bar.start, 0x1100).unwrap();
        assert_eq!(normalize_window(bar, direct, false).unwrap(), bar);
        assert!(normalize_window(bar, Region::default(), false).is_err());
        let configuration_identity = Region::from_size(0x4000, 0x1100).unwrap();
        assert!(normalize_window(bar, configuration_identity, true).is_err());
        let offset = Region::from_size(bar.start + 0x100, 0x1000).unwrap();
        assert!(normalize_window(bar, offset, false).is_err());
        let oversized = Region::from_size(bar.start, 0x20000).unwrap();
        assert!(normalize_window(bar, oversized, false).is_err());
        assert!(normalize_window(Region::default(), direct, false).is_err());
    }
}
