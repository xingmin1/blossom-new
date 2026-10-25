//! 2K1000LA 的 LIOINTC、Loongson PCI 与原生外设绑定。

use super::resources;
use super::{
    cells, compatible, device_region, enabled, identity_address, interrupt_parent, number, parent,
    property, regions, require_controller, single_region, word, Fdt, Node,
};
use crate::boot::{
    AhciResources, BootInfo, LioIntcResources, PciConfiguration, PciFunctionResource, PciIntxRoute,
    PciRange, Region, SysconAction,
};

/// 仅解释有来源的 UHI、Linux bootm 或精确 go kernel dtb 交接，不扫描物理内存。
///
/// # Safety
/// 固件须保证对应 ABI 的 a1/argv 及 argv[1] 指向有界可读存储，完整 DTB 的可读性由 discover 调用者承担。
pub(super) unsafe fn dtb_address(args: [usize; 4]) -> Result<usize, &'static str> {
    let candidate = if args[0] == usize::MAX - 1 {
        args[1]
    } else if (2..=16).contains(&args[0]) && args[1] != 0 {
        let argv = crate::arch::phys_to_virt(physical(args[1])?) as *const usize;
        // SAFETY: 固件 go ABI 保证 argc 个指针可读；范围已限制且只读必有的 argv[1]。
        let address = unsafe { argv.add(1).read_unaligned() };
        let pointer = crate::arch::phys_to_virt(physical(address)?) as *const u8;
        if address == 0 {
            return Err("U-Boot DTB address argument is null");
        }
        let mut value = 0usize;
        let mut digits = 0;
        for index in 0..19 {
            // SAFETY: 固件 ABI 保证 argv[1] 的十六进制字符串最多 18 字节及终止 NUL 可读。
            let byte = unsafe { pointer.add(index).read() };
            if byte == 0 {
                if digits == 0 {
                    return Err("U-Boot DTB address argument is empty");
                }
                return physical(value);
            }
            if index == 1 && byte == b'x' && value == 0 {
                digits = 0;
                continue;
            }
            let digit = match byte {
                b'0'..=b'9' => byte - b'0',
                b'a'..=b'f' => byte - b'a' + 10,
                b'A'..=b'F' => byte - b'A' + 10,
                _ => return Err("U-Boot DTB address argument is not hexadecimal"),
            };
            value = value
                .checked_mul(16)
                .and_then(|v| v.checked_add(digit as usize))
                .ok_or("U-Boot DTB address overflows")?;
            digits += 1;
        }
        return Err("U-Boot DTB address argument is too long");
    } else if args[3] != 0 {
        args[3]
    } else {
        return Err("LoongArch firmware did not supply an OS DTB");
    };
    physical(candidate)
}

/// 固件可传物理地址或 cached/uncached DMW 地址；其他地址类别拒绝。
fn physical(address: usize) -> Result<usize, &'static str> {
    if !matches!(address >> 60, 0 | 8 | 9) {
        return Err("firmware address is neither physical nor a supported DMW alias");
    }
    Ok(address & 0x0fff_ffff_ffff_ffff)
}

/// 由实际 LIOINTC 与 PCI 身份构造板快照，PCI reg 从不作为 BAR。
pub(super) fn discover<'a>(
    tree: &'a Fdt<'a>,
    uart: Node<'a>,
    info: &mut BootInfo,
) -> Result<(), &'static str> {
    let mut banks = [None; 2];
    for node in tree
        .all_nodes()
        .filter(|node| enabled(*node) && compatible(*node, "loongson,liointc-2.0"))
    {
        require_controller(node, "loongson,liointc-2.0", 2)?;
        identity_address(tree, node)?;
        let root = interrupt_parent(tree, node)?;
        require_controller(root, "loongson,cpu-interrupt-controller", 1)?;
        let cause = super::irq(node, root)?;
        if !(2..=3).contains(&cause) {
            return Err("FDT LIOINTC parent cause is unsupported");
        }
        let index = (cause - 2) as usize;
        if banks[index].is_some() {
            return Err("FDT LIOINTC parent cause is duplicated");
        }
        let mut registers = regions(node)?;
        let main = registers
            .next()
            .ok_or("FDT LIOINTC main registers are missing")??;
        let isr0 = registers
            .next()
            .ok_or("FDT LIOINTC CPU0 ISR is missing")??;
        let isr1 = registers
            .next()
            .ok_or("FDT LIOINTC CPU1 ISR is missing")??;
        if registers.next().is_some()
            || main.end - main.start != 0x40
            || isr0.end - isr0.start != 8
            || isr1.end - isr1.start != 8
        {
            return Err("FDT LIOINTC register geometry is unsupported");
        }
        let map = property(node, "loongson,parent_int_map")?;
        if map.len() != 16 {
            return Err("FDT LIOINTC parent masks are truncated");
        }
        let mut parent_map = [0; 4];
        for (slot, value) in parent_map.iter_mut().enumerate() {
            *value = word(map, slot * 4)?;
            if *value != if slot == index { u32::MAX } else { 0 } {
                return Err("FDT LIOINTC source routing is unsupported");
            }
        }
        info.board.liointc[index] = LioIntcResources {
            registers: main,
            isr: [isr0, isr1],
            parent_interrupt: cause,
            parent_map,
        };
        banks[index] = Some(node);
    }
    let banks = [
        banks[0].ok_or("FDT LIOINTC0 is missing")?,
        banks[1].ok_or("FDT LIOINTC1 is missing")?,
    ];
    info.interrupts.controller = info.board.liointc[0].registers;
    info.interrupts.parent_controller = info.board.liointc[1].registers;
    info.interrupts.root_interrupt = 2;
    info.interrupts.source_count = 64;
    info.interrupts.uart_source = lio_irq(tree, uart, banks)?;
    if info.hardware_ids[..info.cpu_count]
        .iter()
        .any(|id| 2 <= *id)
    {
        return Err("FDT LA264 hardware CPU ID exceeds the dual-core board");
    }
    let rtc = unique(tree, "loongson,ls2k1000-rtc")?;
    info.rtc = single_region(tree, rtc)?;
    let pmc = unique(tree, "loongson,ls2k1000-pmc")?;
    info.board.pmc = single_region(tree, pmc)?;
    for node in pmc.children() {
        if compatible(node, "syscon-reboot") {
            info.board.reboot = Some(power_action(node, info.board.pmc)?);
        }
        if compatible(node, "syscon-poweroff") {
            info.board.poweroff = Some(power_action(node, info.board.pmc)?);
        }
    }
    discover_pci(tree, banks, info)
}

/// 读取一个板级 syscon 动作并验证寄存器范围。
fn power_action(node: Node<'_>, registers: Region) -> Result<SysconAction, &'static str> {
    let offset = number(node, "offset")?;
    let mask = number(node, "mask")?;
    let value = resources::optional_number(node, "value")?.unwrap_or(mask);
    if offset % 4 != 0
        || registers.end - registers.start < offset as usize + 4
        || value & !mask != 0
    {
        return Err("FDT syscon power action is invalid");
    }
    Ok(SysconAction {
        registers,
        offset,
        mask,
        value,
    })
}

/// 取得唯一的实际控制器绑定。
fn unique<'a>(tree: &'a Fdt<'a>, binding: &str) -> Result<Node<'a>, &'static str> {
    let mut nodes = tree
        .all_nodes()
        .filter(|node| enabled(*node) && compatible(*node, binding));
    let node = nodes
        .next()
        .ok_or("FDT required LA controller is missing")?;
    if nodes.next().is_some() {
        return Err("FDT required LA controller is duplicated");
    }
    Ok(node)
}

/// 将真实 LIOINTC bank/source 规范为单一外部 IRQ 域编号。
fn lio_irq<'a>(
    tree: &'a Fdt<'a>,
    node: Node<'a>,
    banks: [Node<'a>; 2],
) -> Result<u32, &'static str> {
    let (controller, source, flags) = if let Some(extended) = node.property("interrupts-extended") {
        if extended.value.len() < 12 || extended.value.len() % 12 != 0 {
            return Err("FDT LIOINTC extended interrupt is truncated");
        }
        let controller = tree
            .find_phandle(word(extended.value, 0)?)
            .ok_or("FDT LIOINTC interrupt parent is missing")?;
        (
            controller,
            word(extended.value, 4)?,
            word(extended.value, 8)?,
        )
    } else {
        let controller = interrupt_parent(tree, node)?;
        let raw = property(node, "interrupts")?;
        if raw.len() != 8 {
            return Err("FDT LIOINTC interrupt is not two cells");
        }
        (controller, word(raw, 0)?, word(raw, 4)?)
    };
    let index = banks
        .iter()
        .position(|bank| bank.name.as_ptr() == controller.name.as_ptr())
        .ok_or("FDT device is not connected to selected LIOINTC")?;
    if 32 <= source || flags != 4 {
        return Err("FDT LIOINTC device source or trigger is unsupported");
    }
    Ok(index as u32 * 32 + source)
}

/// 收集 Loongson CFG0/CFG1、窗口与所选集成功能资源，保留 SATA 节点显式声明的端口图。
fn discover_pci<'a>(
    tree: &'a Fdt<'a>,
    banks: [Node<'a>; 2],
    info: &mut BootInfo,
) -> Result<(), &'static str> {
    let mut hosts = tree
        .all_nodes()
        .filter(|node| enabled(*node) && compatible(*node, "loongson,ls2k-pci"));
    let Some(host) = hosts.next() else {
        if tree.all_nodes().any(|node| {
            enabled(node) && node.property("device_type").and_then(|p| p.as_str()) == Some("pci")
        }) {
            return Err("FDT enabled LA PCI host has an unsupported binding");
        }
        return discover_mmio(tree, banks, info);
    };
    if hosts.next().is_some() {
        return Err("FDT Loongson PCI host is duplicated");
    }
    identity_address(tree, host)?;
    let mut registers = regions(host)?;
    info.pci.configuration = registers.next().ok_or("FDT Loongson CFG0 is missing")??;
    info.pci.configuration_extended = registers.next().ok_or("FDT Loongson CFG1 is missing")??;
    if registers.next().is_some() {
        return Err("FDT Loongson PCI has unsupported configuration windows");
    }
    info.pci.kind = PciConfiguration::Loongson;
    let buses = property(host, "bus-range")?;
    if buses.len() != 8 || word(buses, 0)? != 0 || 255 < word(buses, 4)? {
        return Err("FDT Loongson PCI bus range is invalid");
    }
    info.pci.bus_start = 0;
    info.pci.bus_end = word(buses, 4)?;
    let ranges = property(host, "ranges")?;
    if ranges.is_empty() || ranges.len() % 28 != 0 {
        return Err("FDT Loongson PCI ranges are truncated");
    }
    for range in ranges.chunks_exact(28) {
        if info.pci.range_count == info.pci.ranges.len() {
            return Err("FDT Loongson PCI range capacity exceeded");
        }
        let flags = word(range, 0)?;
        if !matches!(flags & 0x0300_0000, 0x0100_0000 | 0x0200_0000 | 0x0300_0000) {
            return Err("FDT Loongson PCI window kind is unsupported");
        }
        let cpu = Region::from_size(cells(&range[12..20])?, cells(&range[20..28])?)?;
        device_region(cpu, info)?;
        info.pci.ranges[info.pci.range_count] = PciRange {
            flags,
            bus_start: cells(&range[4..12])?,
            cpu,
        };
        info.pci.range_count += 1;
    }
    info.pci.intx_mask = [0x00ff_ff00, 0, 0, 0];
    let sata = host
        .children()
        .find(|node| node.name == "sata@8,0" && enabled(*node))
        .ok_or("FDT selected SATA function is missing or disabled")?;
    let ahci = pci_function(tree, sata, banks)?;
    add_route(info, ahci)?;
    info.board.ahci = Some(AhciResources {
        pci: Some(ahci),
        interrupt_source: ahci.interrupt_source,
        dma_coherent: ahci.dma_coherent,
        ports_implemented: resources::optional_number(sata, "ports-implemented")?,
        ..AhciResources::default()
    });
    let mac = host
        .children()
        .find(|node| node.name == "ethernet@3,0" && enabled(*node))
        .ok_or("FDT selected GMAC0 function is missing or disabled")?;
    let identity = pci_function(tree, mac, banks)?;
    add_route(info, identity)?;
    let mut gmac = resources::gmac(tree, mac)?;
    gmac.pci = Some(identity);
    gmac.interrupt_source = identity.interrupt_source;
    info.board.gmac = Some(gmac);
    Ok(())
}

/// PCI 五 cell reg 中只有首 cell 的 BDF 构成配置身份，其零 size 不建立 MMIO。
fn pci_function<'a>(
    tree: &'a Fdt<'a>,
    node: Node<'a>,
    banks: [Node<'a>; 2],
) -> Result<PciFunctionResource, &'static str> {
    let reg = property(node, "reg")?;
    if reg.len() != 20 || reg[4..].iter().any(|byte| *byte != 0) {
        return Err("FDT selected PCI function reg is not configuration identity");
    }
    let address = word(reg, 0)?;
    if address & 0xff00_00ff != 0 {
        return Err("FDT PCI configuration identity is invalid");
    }
    Ok(PciFunctionResource {
        bus: (address >> 16) as u8,
        device: ((address >> 11) & 31) as u8,
        function: ((address >> 8) & 7) as u8,
        interrupt_source: lio_irq(tree, node, banks)?,
        dma_coherent: resources::coherent(tree, node),
    })
}

/// 所选集成功能按完整 BDF 取得 DTS 中固定连线，不使用配置 interrupt-line。
fn add_route(info: &mut BootInfo, function: PciFunctionResource) -> Result<(), &'static str> {
    if info.pci.intx_route_count == info.pci.intx_routes.len() {
        return Err("FDT PCI function route capacity exceeded");
    }
    info.pci.intx_routes[info.pci.intx_route_count] = PciIntxRoute {
        child: [
            ((function.bus as u32) << 16)
                | ((function.device as u32) << 11)
                | ((function.function as u32) << 8),
            0,
            0,
            0,
        ],
        source: function.interrupt_source,
    };
    info.pci.intx_route_count += 1;
    Ok(())
}

/// 无 PCI host 的自定义树只接受真实 MMIO 绑定，不把 PCI 子节点的 reg 重新解释。
fn discover_mmio<'a>(
    tree: &'a Fdt<'a>,
    banks: [Node<'a>; 2],
    info: &mut BootInfo,
) -> Result<(), &'static str> {
    let mut sata_nodes = tree.all_nodes().filter(|node| {
        enabled(*node)
            && [
                "loongson,ls-ahci",
                "loongson,ls2k1000-ahci",
                "loongson,2k1000-ahci",
                "snps,spear-ahci",
            ]
            .iter()
            .any(|binding| compatible(*node, binding))
    });
    let sata = sata_nodes
        .next()
        .ok_or("FDT native AHCI MMIO binding is missing")?;
    if sata_nodes.next().is_some() {
        return Err("FDT native AHCI MMIO binding is duplicated");
    }
    let bus = parent(tree, sata).ok_or("FDT AHCI parent bus is missing")?;
    if bus.property("device_type").and_then(|p| p.as_str()) == Some("pci") {
        return Err("FDT AHCI PCI identity cannot be used as MMIO");
    }
    let registers = single_region(tree, sata)?;
    device_region(registers, info)?;
    if registers.end - registers.start < 0x1100 {
        return Err("FDT AHCI MMIO window is too short");
    }
    let dma_mask = sata
        .property("dma-mask")
        .map(|p| cells(p.value).map(|value| value as u64))
        .transpose()?;
    if dma_mask.is_some_and(|mask| mask < u32::MAX as u64) {
        return Err("FDT AHCI DMA mask is narrower than 32 bits");
    }
    info.board.ahci = Some(AhciResources {
        registers,
        pci: None,
        interrupt_source: lio_irq(tree, sata, banks)?,
        dma_coherent: resources::coherent(tree, sata),
        dma_mask,
        ports_implemented: resources::optional_number(sata, "ports-implemented")?,
    });
    let mac = resources::alias(tree, "ethernet0")?;
    if !(compatible(mac, "snps,dwmac-3.70a") || compatible(mac, "snps,arc-dwmac-3.70a")) {
        return Err("FDT ethernet0 is not a DWMAC1000 MMIO binding");
    }
    let mut gmac = resources::gmac(tree, mac)?;
    gmac.registers = single_region(tree, mac)?;
    device_region(gmac.registers, info)?;
    if gmac.registers.end - gmac.registers.start < 0x105c {
        return Err("FDT GMAC MMIO window is too short");
    }
    gmac.interrupt_source = lio_irq(tree, mac, banks)?;
    info.board.gmac = Some(gmac);
    Ok(())
}
