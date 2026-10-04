//! QEMU virt 的一次性无堆资源发现；输入结构验证完成后才交给 fdt 解析。

use crate::boot::{
    BootInfo, PciIntxRoute, PciRange, Region, VirtioMmioResource, MAX_CPUS, MAX_INTERRUPT_SOURCES,
    MAX_MEMORY_REGIONS, MAX_VIRTIO_MMIO,
};
use fdt::{node::FdtNode, Fdt};

#[cfg(target_arch = "riscv64")]
pub const EARLY_UART_BASE: usize = 0x1000_0000;
#[cfg(target_arch = "loongarch64")]
pub const EARLY_UART_BASE: usize = 0x1fe0_01e0;

const MAX_DTB_SIZE: usize = 2 * 1024 * 1024;
type Node<'a> = FdtNode<'a, 'a>;

/// 从已取得的字节切片读取一个大端 cell，拒绝截断和偏移溢出。
fn word(bytes: &[u8], at: usize) -> Result<u32, &'static str> {
    let end = at.checked_add(4).ok_or("FDT field offset overflows")?;
    let value = bytes.get(at..end).ok_or("FDT field is truncated")?;
    Ok(u32::from_be_bytes([value[0], value[1], value[2], value[3]]))
}

/// 将绑定允许的单/双 cell 转为原生地址；空 size-cells 表示零。
fn cells(bytes: &[u8]) -> Result<usize, &'static str> {
    match bytes.len() {
        0 => Ok(0),
        4 => Ok(word(bytes, 0)? as usize),
        8 => Ok((((word(bytes, 0)? as u64) << 32) | word(bytes, 4)? as u64) as usize),
        _ => Err("FDT address or size is not one or two cells"),
    }
}

/// 借用 NUL 结尾的属性字符串，解析时验证 UTF-8。
fn cstr(bytes: &[u8]) -> Result<&str, &'static str> {
    let end = bytes
        .iter()
        .position(|b| *b == 0)
        .ok_or("FDT string is not NUL terminated")?;
    core::str::from_utf8(&bytes[..end]).map_err(|_| "FDT string is not valid UTF-8")
}

/// 借用头部以外的数据块，所有范围检查在传入解析库前完成。
fn block(bytes: &[u8], offset: usize, size: usize) -> Result<&[u8], &'static str> {
    if offset < 40 {
        return Err("FDT block overlaps the header");
    }
    let end = offset
        .checked_add(size)
        .ok_or("FDT block range overflows")?;
    bytes
        .get(offset..end)
        .ok_or("FDT block exceeds the total size")
}

/// 验证库会使用的块边界、字符串和父栈深度，成功后才允许资源遍历。
fn validate(bytes: &[u8]) -> Result<(), &'static str> {
    if word(bytes, 0)? != 0xd00d_feed {
        return Err("FDT magic is invalid");
    }
    let total = word(bytes, 4)? as usize;
    if !(40..=MAX_DTB_SIZE).contains(&total) || total != bytes.len() {
        return Err("FDT total size is invalid or truncated");
    }
    if word(bytes, 20)? != 17 || 17 < word(bytes, 24)? {
        return Err("FDT version is not v17");
    }
    let structure_start = word(bytes, 8)? as usize;
    let strings_start = word(bytes, 12)? as usize;
    let reserve_start = word(bytes, 16)? as usize;
    let structure = block(bytes, structure_start, word(bytes, 36)? as usize)?;
    let strings = block(bytes, strings_start, word(bytes, 32)? as usize)?;
    if structure_start % 4 != 0 || reserve_start % 8 != 0 {
        return Err("FDT structure or reservation block is unaligned");
    }
    let mut reserve_end = reserve_start;
    loop {
        let entry = block(bytes, reserve_end, 16)?;
        reserve_end += 16;
        if entry.iter().all(|b| *b == 0) {
            break;
        }
    }
    let spans = [
        (structure_start, structure_start + structure.len()),
        (strings_start, strings_start + strings.len()),
        (reserve_start, reserve_end),
    ];
    for i in 0..spans.len() {
        for j in i + 1..spans.len() {
            if spans[i].0 < spans[j].1 && spans[j].0 < spans[i].1 {
                return Err("FDT blocks overlap");
            }
        }
    }
    let mut at = 0;
    let mut depth = 0usize;
    let mut children = [false; 64];
    let mut aliases = [false; 64];
    let mut root_seen = false;
    loop {
        let token = word(structure, at)?;
        at += 4;
        match token {
            1 => {
                if 63 <= depth || (depth == 0 && root_seen) {
                    return Err("FDT node depth exceeds capacity or has multiple roots");
                }
                let name = cstr(structure.get(at..).ok_or("FDT node name is truncated")?)?;
                if !name.is_ascii() || (depth == 0 && !name.is_empty()) {
                    return Err("FDT node name or root is invalid");
                }
                at = at
                    .checked_add((name.len() + 4) & !3)
                    .ok_or("FDT node name size overflows")?;
                if structure.len() < at {
                    return Err("FDT node name padding is truncated");
                }
                if 0 < depth {
                    children[depth - 1] = true;
                }
                children[depth] = false;
                aliases[depth] = depth == 1 && name == "aliases";
                depth += 1;
                root_seen = true;
            }
            2 => {
                depth = depth
                    .checked_sub(1)
                    .ok_or("FDT end-node token is unmatched")?;
            }
            3 => {
                if depth == 0 || children[depth - 1] {
                    return Err("FDT property is outside the property section");
                }
                let len = word(structure, at)? as usize;
                let name_at = word(structure, at + 4)? as usize;
                at += 8;
                let name = cstr(
                    strings
                        .get(name_at..)
                        .ok_or("FDT property name offset exceeds the string block")?,
                )?;
                let end = at.checked_add(len).ok_or("FDT property size overflows")?;
                let value = structure
                    .get(at..end)
                    .ok_or("FDT property value is truncated")?;
                if aliases[depth - 1] && (name.contains('/') || !cstr(value)?.starts_with('/')) {
                    return Err("FDT aliases require slash-free names and absolute paths");
                }
                if matches!(name, "#address-cells" | "#size-cells" | "#interrupt-cells")
                    && (value.len() != 4 || 3 < word(value, 0)?)
                {
                    return Err("FDT cell count exceeds the supported range");
                }
                if matches!(
                    name,
                    "compatible" | "status" | "device_type" | "stdout-path"
                ) {
                    let mut list = value;
                    while !list.is_empty() {
                        let text = cstr(list)?;
                        list = &list[text.len() + 1..];
                    }
                }
                at = end.checked_add(3).ok_or("FDT property padding overflows")? & !3;
                if structure.len() < at {
                    return Err("FDT property padding is truncated");
                }
            }
            4 => {}
            9 => {
                if depth != 0 || !root_seen || structure[at..].iter().any(|b| *b != 0) {
                    return Err("FDT structure end token is invalid");
                }
                return Ok(());
            }
            _ => return Err("FDT structure contains an unknown token"),
        }
    }
}

/// 借用必须存在的资源属性，不为缺失设备填入默认配置。
fn property<'a>(node: Node<'a>, name: &str) -> Result<&'a [u8], &'static str> {
    node.property(name)
        .map(|p| p.value)
        .ok_or("FDT required resource property is missing")
}

/// 读取绑定要求的一个整数 cell。
fn number(node: Node<'_>, name: &str) -> Result<u32, &'static str> {
    let value = property(node, name)?;
    if value.len() != 4 {
        return Err("FDT integer property is not one cell");
    }
    word(value, 0)
}

/// 按设备树规范判断节点状态，未写 status 的节点处于启用状态。
fn enabled(node: Node<'_>) -> bool {
    node.property("status")
        .is_none_or(|p| matches!(p.as_str(), Some("okay" | "ok")))
}

/// 判断节点声明是否含所需控制器绑定。
fn compatible(node: Node<'_>, expected: &str) -> bool {
    node.compatible()
        .is_some_and(|c| c.all().any(|s| s == expected))
}

/// 在已验证的树中定位直接父节点，保持原始节点身份。
fn parent<'a>(tree: &'a Fdt<'a>, node: Node<'a>) -> Option<Node<'a>> {
    tree.all_nodes().find(|candidate| {
        candidate
            .children()
            .any(|child| child.name.as_ptr() == node.name.as_ptr())
    })
}

/// 沿真实中断父链解析 phandle，缺少关系时返回错误。
fn interrupt_parent<'a>(tree: &'a Fdt<'a>, mut node: Node<'a>) -> Result<Node<'a>, &'static str> {
    loop {
        if node.property("interrupt-parent").is_some() {
            return tree
                .find_phandle(number(node, "interrupt-parent")?)
                .ok_or("FDT interrupt-parent phandle has no controller");
        }
        node = parent(tree, node).ok_or("FDT interrupt-parent relationship is missing")?;
    }
}

/// 验证选定控制器的启用状态、compatible 和中断 cell 格式。
fn require_controller(node: Node<'_>, expected: &str, count: u32) -> Result<(), &'static str> {
    if !enabled(node)
        || !compatible(node, expected)
        || node.property("interrupt-controller").is_none()
    {
        return Err("FDT interrupt chain is not the selected platform controller");
    }
    if number(node, "#interrupt-cells")? != count {
        return Err("FDT interrupt cell count does not match the controller binding");
    }
    Ok(())
}

/// 按父节点 cell 约定遍历 reg，逐段返回不溢出的物理区间。
fn regions(
    node: Node<'_>,
) -> Result<impl Iterator<Item = Result<Region, &'static str>> + '_, &'static str> {
    let raw = node
        .raw_reg()
        .ok_or("FDT resource node has no reg property")?;
    let value = property(node, "reg")?;
    let first = node
        .raw_reg()
        .and_then(|mut r| r.next())
        .ok_or("FDT reg is empty or truncated")?;
    let stride = first.address.len() + first.size.len();
    if stride == 0
        || value.len() % stride != 0
        || first.address.is_empty()
        || 8 < first.address.len()
        || first.size.is_empty()
        || 8 < first.size.len()
    {
        return Err("FDT reg address or size format is invalid");
    }
    Ok(raw.map(|reg| Region::from_size(cells(reg.address)?, cells(reg.size)?)))
}

/// 验证当前资源所在总线是物理恒等映射，不隐式猜测地址转换。
fn identity_address<'a>(tree: &'a Fdt<'a>, mut node: Node<'a>) -> Result<(), &'static str> {
    while let Some(bus) = parent(tree, node) {
        if bus.name != "/" && !bus.property("ranges").is_some_and(|p| p.value.is_empty()) {
            return Err("FDT resource parent bus is not identity mapped");
        }
        node = bus;
    }
    Ok(())
}

/// 取得绑定要求的唯一寄存器区间。
fn single_region<'a>(tree: &'a Fdt<'a>, node: Node<'a>) -> Result<Region, &'static str> {
    identity_address(tree, node)?;
    let mut ranges = regions(node)?;
    let first = ranges.next().ok_or("FDT reg has no resource range")??;
    if ranges.next().is_some() {
        return Err("FDT device has multiple unsupported register ranges");
    }
    Ok(first)
}

/// 读取与父控制器绑定一致的真实中断源号。
fn irq(node: Node<'_>, controller: Node<'_>) -> Result<u32, &'static str> {
    let value = property(node, "interrupts")?;
    if value.len() != number(controller, "#interrupt-cells")? as usize * 4 {
        return Err("FDT UART or root interrupt size does not match its binding");
    }
    word(value, 0)
}

/// 将完整 DTB 转为一份资源快照，保留 BSP 身份及全部实际 RAM/保留区。
fn discover_tree(
    bytes: &[u8],
    raw_args: [usize; 3],
    bsp: usize,
    dtb_address: usize,
) -> Result<BootInfo, &'static str> {
    validate(bytes)?;
    let tree = Fdt::new(bytes).map_err(|_| "FDT parser rejected the input")?;
    let mut info = BootInfo::empty(raw_args, bsp);
    info.dtb = Region::from_size(dtb_address, tree.total_size())?;
    info.reserve(info.dtb)?;
    #[cfg(target_arch = "loongarch64")]
    info.reserve(Region::from_size(0, 0x10_0000)?)?;
    for reservation in tree.memory_reservations() {
        info.reserve(Region::from_size(
            reservation.address() as usize,
            reservation.size(),
        )?)?;
    }
    if let Some(reserved) = tree.find_node("/reserved-memory") {
        if reserved
            .property("ranges")
            .is_some_and(|p| !p.value.is_empty())
        {
            return Err("FDT reserved-memory is not an identity address space");
        }
        for child in reserved.children().filter(|n| enabled(*n)) {
            identity_address(&tree, child)?;
            for region in regions(child)? {
                info.reserve(region?)?;
            }
        }
    }
    for node in tree.all_nodes().filter(|n| enabled(*n)) {
        if node.property("device_type").and_then(|p| p.as_str()) != Some("memory") {
            continue;
        }
        identity_address(&tree, node)?;
        for region in regions(node)? {
            if info.memory_count == MAX_MEMORY_REGIONS {
                return Err("FDT RAM range capacity exceeded");
            }
            let region = region?;
            if info.memory[..info.memory_count]
                .iter()
                .any(|old| old.overlaps(region))
            {
                return Err("FDT RAM ranges overlap");
            }
            info.memory[info.memory_count] = region;
            info.memory_count += 1;
        }
    }
    if info.memory_count == 0 {
        return Err("FDT has no RAM resources");
    }
    if !info.memory[..info.memory_count]
        .iter()
        .any(|r| r.start <= info.dtb.start && info.dtb.end <= r.end)
    {
        return Err("runtime DTB is outside the described RAM");
    }
    let cpus = tree.find_node("/cpus").ok_or("FDT cpus node is missing")?;
    if !matches!(number(cpus, "#address-cells")?, 1 | 2) || number(cpus, "#size-cells")? != 0 {
        return Err("FDT CPU reg cell format is invalid");
    }
    for node in cpus.children().filter(|n| enabled(*n)) {
        if node.property("device_type").and_then(|p| p.as_str()) != Some("cpu") {
            continue;
        }
        let reg = property(node, "reg")?;
        if reg.len() != number(cpus, "#address-cells")? as usize * 4 {
            return Err("FDT CPU reg is empty, truncated, or contains multiple IDs");
        }
        let id = cells(reg)?;
        if info.hardware_ids[..info.cpu_count].contains(&id) {
            return Err("FDT CPU hardware IDs are duplicated");
        }
        if info.cpu_count == MAX_CPUS {
            return Err("FDT CPU count exceeds the four-CPU capacity");
        }
        info.hardware_ids[info.cpu_count] = id;
        info.cpu_count += 1;
    }
    let bsp_index = info.hardware_ids[..info.cpu_count]
        .iter()
        .position(|id| *id == bsp)
        .ok_or("FDT enabled CPUs do not include the BSP")?;
    info.hardware_ids.swap(0, bsp_index);
    #[cfg(target_arch = "riscv64")]
    {
        info.counter_frequency = number(cpus, "timebase-frequency")
            .map_err(|_| "FDT cpus timebase-frequency is missing or invalid")?
            as u64;
    }
    #[cfg(target_arch = "loongarch64")]
    {
        info.counter_frequency = crate::arch::boot_counter_frequency();
    }
    if info.counter_frequency == 0 {
        return Err("hardware counter frequency is zero");
    }
    let chosen = tree
        .find_node("/chosen")
        .ok_or("FDT chosen node is missing")?;
    discover_bootargs(chosen, &mut info)?;
    let stdout =
        cstr(property(chosen, "stdout-path").map_err(|_| "FDT chosen stdout-path is missing")?)?;
    let path = stdout
        .split(':')
        .next()
        .ok_or("FDT stdout-path is invalid")?;
    let uart = tree.find_node(path).ok_or("FDT stdout-path has no UART")?;
    if !enabled(uart) || !compatible(uart, "ns16550a") {
        return Err("FDT chosen UART is not an enabled ns16550a");
    }
    info.interrupts.uart = single_region(&tree, uart)?;
    if info.interrupts.uart.start != EARLY_UART_BASE {
        return Err("FDT chosen UART differs from the early UART");
    }
    let controller = interrupt_parent(&tree, uart)?;
    info.interrupts.uart_source = irq(uart, controller)?;
    info.interrupts.controller = single_region(&tree, controller)?;
    #[cfg(target_arch = "riscv64")]
    discover_plic(&tree, cpus, controller, &mut info)?;
    #[cfg(target_arch = "loongarch64")]
    {
        require_controller(controller, "loongson,pch-pic-1.0", 2)?;
        info.interrupts.source_count = MAX_INTERRUPT_SOURCES as u32;
        if word(property(uart, "interrupts")?, 4)? != 4 || 64 <= info.interrupts.uart_source {
            return Err("FDT UART is not a supported PCH-PIC level-high source");
        }
        let extioi = interrupt_parent(&tree, controller)?;
        require_controller(extioi, "loongson,ls2k2000-eiointc", 1)?;
        // EXTIOI 的 reg 是 IOCSR 地址，不能加入普通 MMIO 映射。
        info.interrupts.parent_controller = single_region(&tree, extioi)?;
        let root = interrupt_parent(&tree, extioi)?;
        require_controller(root, "loongson,cpu-interrupt-controller", 1)?;
        info.interrupts.root_interrupt = irq(extioi, root)?;
        if 8 <= info.interrupts.root_interrupt {
            return Err("FDT EXTIOI CPU root interrupt exceeds the supported range");
        }
    }
    discover_pci(&tree, controller, &mut info)?;
    discover_virtio_mmio(&tree, controller, &mut info)?;
    Ok(info)
}

/// 对测试拥有的 DTB 字节运行唯一正式结构校验，不访问固件指针或修改启动状态。
#[cfg(feature = "ktest")]
pub(crate) fn ktest_validate(bytes: &[u8]) -> Result<(), &'static str> {
    validate(bytes)
}

/// 对测试拥有的 DTB 字节运行正式资源发现，返回独立快照而不发布到全局启动信息。
#[cfg(feature = "ktest")]
pub(crate) fn ktest_discover_tree(
    bytes: &[u8],
    raw_args: [usize; 3],
    bsp: usize,
    dtb_address: usize,
) -> Result<crate::boot::BootInfo, &'static str> {
    discover_tree(bytes, raw_args, bsp, dtb_address)
}

/// 保存完整 NUL 结尾的单一命令行，拒绝截断、内嵌 NUL 和容量外输入。
fn discover_bootargs(chosen: Node<'_>, info: &mut BootInfo) -> Result<(), &'static str> {
    let Some(property) = chosen.property("bootargs") else {
        return Ok(());
    };
    let value = property.value;
    if value.is_empty()
        || info.bootargs.len() < value.len()
        || value.last() != Some(&0)
        || value[..value.len() - 1].contains(&0)
    {
        return Err("FDT bootargs is not a bounded NUL-terminated command line");
    }
    info.bootargs[..value.len()].copy_from_slice(value);
    info.bootargs_len = value.len() - 1;
    Ok(())
}

/// 验证新增设备物理窗口不覆盖 RAM、已有控制器或先前登记的设备窗口。
fn device_region(region: Region, info: &BootInfo) -> Result<(), &'static str> {
    if info.memory[..info.memory_count]
        .iter()
        .any(|old| old.overlaps(region))
        || info.interrupts.uart.overlaps(region)
        || info.interrupts.controller.overlaps(region)
        || info.pci.configuration.overlaps(region)
        || info.pci.ranges[..info.pci.range_count]
            .iter()
            .any(|old| old.cpu.overlaps(region))
        || info.virtio_mmio[..info.virtio_mmio_count]
            .iter()
            .any(|old| old.registers.overlaps(region))
    {
        return Err("FDT device resource overlaps RAM or another device window");
    }
    Ok(())
}

/// 校验当前外部控制器的设备源范围，PLIC 零号保持保留。
fn device_source(source: u32, info: &BootInfo) -> Result<(), &'static str> {
    #[cfg(target_arch = "riscv64")]
    if source == 0 {
        return Err("FDT device interrupt uses reserved PLIC source zero");
    }
    if info.interrupts.source_count <= source {
        return Err("FDT device interrupt source exceeds the controller capacity");
    }
    Ok(())
}

/// 一次记录启用的 MMIO 槽，物理顺序独立于 FDT 节点遍历顺序。
fn discover_virtio_mmio(
    tree: &Fdt<'_>,
    controller: Node<'_>,
    info: &mut BootInfo,
) -> Result<(), &'static str> {
    for node in tree
        .all_nodes()
        .filter(|n| enabled(*n) && compatible(*n, "virtio,mmio"))
    {
        if info.virtio_mmio_count == MAX_VIRTIO_MMIO {
            return Err("FDT VirtIO-MMIO slot capacity exceeded");
        }
        require_controller(controller, "riscv,plic0", 1)?;
        let parent = interrupt_parent(tree, node)?;
        if parent.name.as_ptr() != controller.name.as_ptr() {
            return Err("FDT VirtIO-MMIO interrupt parent is not the selected PLIC");
        }
        let registers = single_region(tree, node)?;
        if registers.start % 0x1000 != 0 || registers.end - registers.start != 0x1000 {
            return Err("FDT VirtIO-MMIO register window is not one aligned page");
        }
        device_region(registers, info)?;
        let interrupt_source = irq(node, parent)?;
        device_source(interrupt_source, info)?;
        info.virtio_mmio[info.virtio_mmio_count] = VirtioMmioResource {
            registers,
            interrupt_source,
        };
        info.virtio_mmio_count += 1;
    }
    info.virtio_mmio[..info.virtio_mmio_count]
        .sort_unstable_by_key(|resource| resource.registers.start);
    Ok(())
}

/// 记录 PCI ECAM、总线编号和窗口，不枚举设备或访问配置空间。
fn discover_pci(
    tree: &Fdt<'_>,
    controller: Node<'_>,
    info: &mut BootInfo,
) -> Result<(), &'static str> {
    let mut hosts = tree
        .all_nodes()
        .filter(|n| enabled(*n) && compatible(*n, "pci-host-ecam-generic"));
    let host = hosts.next().ok_or("FDT PCI ECAM host is missing")?;
    if hosts.next().is_some() {
        return Err("FDT has multiple unsupported PCI hosts");
    }
    let configuration = single_region(tree, host)?;
    device_region(configuration, info)?;
    info.pci.configuration = configuration;
    if number(host, "#address-cells")? != 3 || number(host, "#size-cells")? != 2 {
        return Err("FDT PCI ranges cell format is invalid");
    }
    let parent = parent(tree, host).ok_or("FDT PCI host has no parent bus")?;
    if parent.cell_sizes().address_cells != 2 {
        return Err("FDT PCI physical addresses are not two cells");
    }
    let buses = property(host, "bus-range")?;
    if buses.len() != 8 {
        return Err("FDT PCI bus-range is truncated");
    }
    info.pci.bus_start = word(buses, 0)?;
    info.pci.bus_end = word(buses, 4)?;
    if info.pci.bus_end < info.pci.bus_start || 255 < info.pci.bus_end {
        return Err("FDT PCI bus-range is invalid");
    }
    let configuration_size = ((info.pci.bus_end - info.pci.bus_start + 1) as usize) << 20;
    if configuration.start % (1 << 20) != 0
        || configuration.end - configuration.start != configuration_size
    {
        return Err("FDT PCI ECAM window does not match the declared bus range");
    }
    let ranges = property(host, "ranges")?;
    if ranges.is_empty() || ranges.len() % 28 != 0 {
        return Err("FDT PCI ranges is truncated");
    }
    for range in ranges.chunks_exact(28) {
        if info.pci.range_count == info.pci.ranges.len() {
            return Err("FDT PCI window capacity exceeded");
        }
        let flags = word(range, 0)?;
        if !matches!(flags & 0x0300_0000, 0x0100_0000 | 0x0200_0000 | 0x0300_0000) {
            return Err("FDT PCI window is neither IO nor memory");
        }
        let bus_start = cells(&range[4..12])?;
        let cpu = Region::from_size(cells(&range[12..20])?, cells(&range[20..28])?)?;
        let bus = Region::from_size(bus_start, cpu.end - cpu.start)?;
        device_region(cpu, info)?;
        for old in &info.pci.ranges[..info.pci.range_count] {
            let old_io = old.flags & 0x0300_0000 == 0x0100_0000;
            let new_io = flags & 0x0300_0000 == 0x0100_0000;
            let old_bus = Region::from_size(old.bus_start, old.cpu.end - old.cpu.start)?;
            if old_io == new_io && old_bus.overlaps(bus) {
                return Err("FDT PCI bus windows overlap in the same address space");
            }
        }
        info.pci.ranges[info.pci.range_count] = PciRange {
            flags,
            bus_start,
            cpu,
        };
        info.pci.range_count += 1;
    }
    discover_pci_intx(tree, host, controller, info)?;
    Ok(())
}

/// 按所选 QEMU 9.2.1 host 的六 cell 条目解析 INTx，不猜测其他表格式。
fn discover_pci_intx(
    tree: &Fdt<'_>,
    host: Node<'_>,
    controller: Node<'_>,
    info: &mut BootInfo,
) -> Result<(), &'static str> {
    // 两机器均以三个 PCI 地址 cell 和一个 pin 构成 key。LA host 未声明
    // #interrupt-cells，PCH-PIC 虽声明两个 cell，host 条目实际只有 source，
    // 没有 flag；所选机器的 PCI INTx 固定为 level-high。
    if host.property("#interrupt-cells").is_some() && number(host, "#interrupt-cells")? != 1 {
        return Err("FDT PCI host interrupt cells do not describe one INTx pin");
    }
    let mask = property(host, "interrupt-map-mask")?;
    if mask.len() != 16 {
        return Err("FDT PCI interrupt-map-mask is not four cells");
    }
    for i in 0..4 {
        info.pci.intx_mask[i] = word(mask, i * 4)?;
    }
    if info.pci.intx_mask != [0x1800, 0, 0, 7] {
        return Err("FDT PCI interrupt-map-mask differs from the selected host profile");
    }
    let map = property(host, "interrupt-map")?;
    if map.is_empty() || map.len() % 24 != 0 {
        return Err("FDT PCI interrupt-map is not a complete six-cell table");
    }
    for entry in map.chunks_exact(24) {
        if info.pci.intx_route_count == info.pci.intx_routes.len() {
            return Err("FDT PCI INTx route capacity exceeded");
        }
        let child = [
            word(entry, 0)?,
            word(entry, 4)?,
            word(entry, 8)?,
            word(entry, 12)?,
        ];
        if !(1..=4).contains(&child[3]) {
            return Err("FDT PCI interrupt-map contains an invalid INTx pin");
        }
        let parent = tree
            .find_phandle(word(entry, 16)?)
            .ok_or("FDT PCI interrupt-map parent phandle has no controller")?;
        if parent.name.as_ptr() != controller.name.as_ptr() {
            return Err("FDT PCI interrupt-map parent is not the selected controller");
        }
        let source = word(entry, 20)?;
        device_source(source, info)?;
        if info.pci.intx_routes[..info.pci.intx_route_count]
            .iter()
            .any(|old| (0..4).all(|i| ((old.child[i] ^ child[i]) & info.pci.intx_mask[i]) == 0))
        {
            return Err("FDT PCI interrupt-map contains duplicate masked keys");
        }
        info.pci.intx_routes[info.pci.intx_route_count] = PciIntxRoute { child, source };
        info.pci.intx_route_count += 1;
    }
    Ok(())
}

/// 按 CPU phandle 定位 S-mode context，并在输入边界验证寄存器窗口。
#[cfg(target_arch = "riscv64")]
fn discover_plic(
    tree: &Fdt<'_>,
    cpus: Node<'_>,
    plic: Node<'_>,
    info: &mut BootInfo,
) -> Result<(), &'static str> {
    require_controller(plic, "riscv,plic0", 1)?;
    let sources = number(plic, "riscv,ndev")?;
    if info.interrupts.uart_source == 0 || sources < info.interrupts.uart_source {
        return Err("FDT UART source exceeds the PLIC capacity");
    }
    if MAX_INTERRUPT_SOURCES as u32 <= sources || info.interrupts.controller.start % 4 != 0 {
        return Err("FDT PLIC register layout is unsupported");
    }
    info.interrupts.source_count = sources + 1;
    let mut handles = [0; MAX_CPUS];
    for cpu in cpus.children().filter(|n| enabled(*n)) {
        if cpu.property("device_type").and_then(|p| p.as_str()) != Some("cpu") {
            continue;
        }
        let id = cells(property(cpu, "reg")?)?;
        let logical = info.hardware_ids[..info.cpu_count]
            .iter()
            .position(|old| *old == id)
            .ok_or("PLIC CPU is not in the enabled CPU table")?;
        let intc = cpu
            .children()
            .find(|n| compatible(*n, "riscv,cpu-intc"))
            .ok_or("FDT CPU local interrupt controller is missing")?;
        require_controller(intc, "riscv,cpu-intc", 1)?;
        handles[logical] = number(intc, "phandle")?;
        if handles[logical] == 0
            || handles
                .iter()
                .enumerate()
                .any(|(i, value)| i != logical && *value == handles[logical])
        {
            return Err("FDT CPU interrupt phandle is zero or duplicated");
        }
    }
    let extended = property(plic, "interrupts-extended")
        .map_err(|_| "FDT PLIC interrupts-extended is missing")?;
    if extended.is_empty() || extended.len() % 8 != 0 {
        return Err("FDT PLIC context description is truncated");
    }
    let mut found = [false; MAX_CPUS];
    for (context, entry) in extended.chunks_exact(8).enumerate() {
        let handle = word(entry, 0)?;
        let cause = word(entry, 4)?;
        // OpenSBI 会把 M-mode context 的 cause 标为全一；它仍占实际 context 槽位。
        if cause == u32::MAX {
            continue;
        }
        let intc = tree
            .find_phandle(handle)
            .ok_or("PLIC context references a missing CPU controller")?;
        require_controller(intc, "riscv,cpu-intc", 1)?;
        if cause != 9 {
            continue;
        }
        if let Some(logical) = handles[..info.cpu_count]
            .iter()
            .position(|old| *old == handle)
        {
            if found[logical] {
                return Err("FDT CPU has duplicate supervisor PLIC contexts");
            }
            info.interrupts.plic_contexts[logical] = context;
            found[logical] = true;
        }
    }
    if found[..info.cpu_count].iter().any(|present| !present) {
        return Err("FDT PLIC has no supervisor context for an enabled CPU");
    }
    let size = info.interrupts.controller.end - info.interrupts.controller.start;
    for context in &info.interrupts.plic_contexts[..info.cpu_count] {
        let claim_end = 0x20_0000 + context * 0x1000 + 8;
        let enable_end = 0x2000 + context * 0x80 + (sources as usize / 32 + 1) * 4;
        if size < claim_end.max(enable_end) {
            return Err("FDT PLIC context exceeds its register window");
        }
    }
    info.interrupts.root_interrupt = 9;
    Ok(())
}

/// 从所选 QEMU 引导协议读取唯一 DTB，返回完整的无堆启动资源快照。
///
/// # Safety
/// 固件必须提供至少可读的 FDT 头部及头部所声明的完整存储范围；本函数检查内容边界，
/// 不能验证任意物理指针是否实际可读。调用时 BSP 必须已建立对应 RAM 访问映射。
pub unsafe fn discover(
    raw_args: [usize; 3],
    bsp_hardware_id: usize,
) -> Result<BootInfo, &'static str> {
    #[cfg(target_arch = "riscv64")]
    let physical = raw_args[1];
    #[cfg(target_arch = "loongarch64")]
    let physical: usize = 0x10_0000;
    if physical == 0 || physical % 8 != 0 || physical.checked_add(40).is_none() {
        return Err("firmware DTB physical address is zero or unaligned");
    }
    let pointer = crate::arch::phys_to_virt(physical) as *const u8;
    // SAFETY: caller 保证固件头部可读，物理地址已转换为当前 RAM 映射。
    let header = unsafe { core::slice::from_raw_parts(pointer, 40) };
    if word(header, 0)? != 0xd00d_feed {
        return Err("firmware DTB magic is invalid");
    }
    let total = word(header, 4)? as usize;
    if !(40..=MAX_DTB_SIZE).contains(&total)
        || physical.checked_add(total).is_none()
        || (pointer as usize).checked_add(total).is_none()
    {
        return Err("firmware DTB size is invalid or address overflows");
    }
    // SAFETY: caller 保证固件声明的完整存储可读；长度、物理和虚拟地址范围已验证。
    let bytes = unsafe { core::slice::from_raw_parts(pointer, total) };
    discover_tree(bytes, raw_args, bsp_hardware_id, physical)
}

/// 在独占副本上检验实际 DTB 的截断、块边界和 NOP 属性处理。
///
/// # Safety
/// 仅由 BSP 在所有 CPU 上线后的普通执行流调用一次，不得重入或被其他核并发调用。
/// 原始 DTB 映射仍有效，且内核自检副本不与固件存储重叠。
#[cfg(feature = "boot-tests")]
pub unsafe fn test_input_boundaries() -> bool {
    static mut COPY: [u8; MAX_DTB_SIZE] = [0; MAX_DTB_SIZE];
    let live = crate::boot::info();
    let physical = live.dtb.start;
    let size = live.dtb.end - physical;
    // LA 固件把 DTB 填充到 1 MiB，副本使用独占静态存储而非启动栈。
    // SAFETY: caller 保证 BSP 唯一访问 COPY；size 来自已验证 DTB，未超过其容量，
    // 源映射与 COPY 不重叠，可变借用仅持续到本次自检返回。
    let bytes = unsafe {
        let copy = core::ptr::addr_of_mut!(COPY).cast::<u8>();
        core::ptr::copy_nonoverlapping(
            crate::arch::phys_to_virt(physical) as *const u8,
            copy,
            size,
        );
        core::slice::from_raw_parts_mut(copy, size)
    };
    if validate(bytes).is_err() || !(0..bytes.len()).all(|end| validate(&bytes[..end]).is_err()) {
        return false;
    }
    let hardware_id = live.hardware_ids[live.cpu_count - 1];
    let original = match discover_tree(bytes, live.raw_args, hardware_id, physical) {
        Ok(info) => info,
        Err(_) => return false,
    };
    if original.cpu_count != live.cpu_count || original.hardware_ids[0] != hardware_id {
        return false;
    }
    // 删除不参与资源发现的根 compatible 属性，覆盖固件留下 NOP 属性洞的情况。
    let (start, end) = {
        let tree = match Fdt::new(bytes) {
            Ok(tree) => tree,
            Err(_) => return false,
        };
        let metadata = match tree.find_node("/").and_then(|n| n.property("compatible")) {
            Some(metadata) => metadata,
            None => return false,
        };
        let value_start = metadata.value.as_ptr() as usize - bytes.as_ptr() as usize;
        (
            value_start - 12,
            (value_start + metadata.value.len() + 3) & !3,
        )
    };
    for token in bytes[start..end].chunks_exact_mut(4) {
        token.copy_from_slice(&4u32.to_be_bytes());
    }
    let nop_ok = match discover_tree(bytes, live.raw_args, hardware_id, physical) {
        Ok(info) => {
            info.hardware_ids == original.hardware_ids
                && info.memory == original.memory
                && info.interrupts.uart == original.interrupts.uart
                && info.interrupts.plic_contexts == original.interrupts.plic_contexts
                && info.pci.range_count == original.pci.range_count
        }
        Err(_) => false,
    };
    if !nop_ok {
        return false;
    }
    // 保持真实总长不变，仅破坏各数据块边界；不能靠总长检查替代块校验。
    for field in [8, 12, 16, 32, 36] {
        let saved = [
            bytes[field],
            bytes[field + 1],
            bytes[field + 2],
            bytes[field + 3],
        ];
        bytes[field..field + 4].copy_from_slice(&u32::MAX.to_be_bytes());
        let rejected = validate(bytes).is_err();
        bytes[field..field + 4].copy_from_slice(&saved);
        if !rejected {
            return false;
        }
    }
    true
}
