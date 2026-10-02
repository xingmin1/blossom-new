//! QEMU virt 的一次性无堆资源发现；输入结构验证完成后才交给 fdt 解析。

use crate::boot::{BootInfo, Region, MAX_CPUS, MAX_MEMORY_REGIONS};
use fdt::{node::FdtNode, Fdt};

#[cfg(target_arch = "riscv64")]
pub const EARLY_UART_BASE: usize = 0x1000_0000;
#[cfg(target_arch = "loongarch64")]
pub const EARLY_UART_BASE: usize = 0x1fe0_01e0;

const MAX_DTB_SIZE: usize = 2 * 1024 * 1024;
type Node<'a> = FdtNode<'a, 'a>;

fn word(bytes: &[u8], at: usize) -> Result<u32, &'static str> {
    let end = at.checked_add(4).ok_or("FDT 字段偏移溢出")?;
    let value = bytes.get(at..end).ok_or("FDT 字段被截断")?;
    Ok(u32::from_be_bytes([value[0], value[1], value[2], value[3]]))
}

fn cells(bytes: &[u8]) -> Result<usize, &'static str> {
    match bytes.len() {
        0 => Ok(0),
        4 => Ok(word(bytes, 0)? as usize),
        8 => Ok(((word(bytes, 0)? as u64) << 32 | word(bytes, 4)? as u64) as usize),
        _ => Err("FDT 地址或长度不是支持的单/双 cell"),
    }
}

fn cstr(bytes: &[u8]) -> Result<&str, &'static str> {
    let end = bytes
        .iter()
        .position(|b| *b == 0)
        .ok_or("FDT 字符串缺少结束符")?;
    core::str::from_utf8(&bytes[..end]).map_err(|_| "FDT 字符串不是有效 UTF-8")
}

fn block(bytes: &[u8], offset: usize, size: usize) -> Result<&[u8], &'static str> {
    if offset < 40 {
        return Err("FDT 数据块覆盖头部");
    }
    let end = offset.checked_add(size).ok_or("FDT 数据块范围溢出")?;
    bytes.get(offset..end).ok_or("FDT 数据块超出总长度")
}

// fdt 0.1.5 只校验 magic 和总长；补齐其切片、字符串和固定深度栈的前提。
fn validate(bytes: &[u8]) -> Result<(), &'static str> {
    if word(bytes, 0)? != 0xd00d_feed {
        return Err("FDT magic 错误");
    }
    let total = word(bytes, 4)? as usize;
    if total < 40 || total > MAX_DTB_SIZE || total != bytes.len() {
        return Err("FDT 总长度无效或输入被截断");
    }
    if word(bytes, 20)? != 17 || word(bytes, 24)? > 17 {
        return Err("FDT 版本不是支持的 v17 格式");
    }
    let structure_start = word(bytes, 8)? as usize;
    let strings_start = word(bytes, 12)? as usize;
    let reserve_start = word(bytes, 16)? as usize;
    let structure = block(bytes, structure_start, word(bytes, 36)? as usize)?;
    let strings = block(bytes, strings_start, word(bytes, 32)? as usize)?;
    if structure_start % 4 != 0 || reserve_start % 8 != 0 {
        return Err("FDT 结构或保留表未对齐");
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
                return Err("FDT 数据块互相重叠");
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
                if depth >= 63 || (depth == 0 && root_seen) {
                    return Err("FDT 节点层级超限或存在多个根");
                }
                let name = cstr(structure.get(at..).ok_or("FDT 节点名被截断")?)?;
                if !name.is_ascii() || (depth == 0 && !name.is_empty()) {
                    return Err("FDT 节点名或根节点无效");
                }
                at = at
                    .checked_add((name.len() + 4) & !3)
                    .ok_or("FDT 节点名长度溢出")?;
                if at > structure.len() {
                    return Err("FDT 节点名填充被截断");
                }
                if depth > 0 {
                    children[depth - 1] = true;
                }
                children[depth] = false;
                aliases[depth] = depth == 1 && name == "aliases";
                depth += 1;
                root_seen = true;
            }
            2 => {
                depth = depth.checked_sub(1).ok_or("FDT 节点结束标记不匹配")?;
            }
            3 => {
                if depth == 0 || children[depth - 1] {
                    return Err("FDT 属性不在节点属性区");
                }
                let len = word(structure, at)? as usize;
                let name_at = word(structure, at + 4)? as usize;
                at += 8;
                let name = cstr(strings.get(name_at..).ok_or("FDT 属性名偏移越界")?)?;
                let end = at.checked_add(len).ok_or("FDT 属性长度溢出")?;
                let value = structure.get(at..end).ok_or("FDT 属性值被截断")?;
                if aliases[depth - 1] {
                    if name.contains('/') || !cstr(value)?.starts_with('/') {
                        return Err("FDT aliases 必须使用无斜杠名字并指向绝对节点路径");
                    }
                }
                if matches!(name, "#address-cells" | "#size-cells" | "#interrupt-cells") {
                    if value.len() != 4 || word(value, 0)? > 3 {
                        return Err("FDT cell 数无效或超过平台支持范围");
                    }
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
                at = end.checked_add(3).ok_or("FDT 属性填充溢出")? & !3;
                if at > structure.len() {
                    return Err("FDT 属性填充被截断");
                }
            }
            4 => {}
            9 => {
                if depth != 0 || !root_seen || structure[at..].iter().any(|b| *b != 0) {
                    return Err("FDT 结构结束标记无效");
                }
                return Ok(());
            }
            _ => return Err("FDT 含未知结构标记"),
        }
    }
}

fn property<'a>(node: Node<'a>, name: &str) -> Result<&'a [u8], &'static str> {
    node.property(name)
        .map(|p| p.value)
        .ok_or("FDT 缺少所需资源属性")
}

fn number(node: Node<'_>, name: &str) -> Result<u32, &'static str> {
    let value = property(node, name)?;
    if value.len() != 4 {
        return Err("FDT 整数属性长度不是一个 cell");
    }
    word(value, 0)
}

fn enabled(node: Node<'_>) -> bool {
    node.property("status")
        .map_or(true, |p| matches!(p.as_str(), Some("okay" | "ok")))
}

fn compatible(node: Node<'_>, expected: &str) -> bool {
    node.compatible()
        .is_some_and(|c| c.all().any(|s| s == expected))
}

fn parent<'a>(tree: &'a Fdt<'a>, node: Node<'a>) -> Option<Node<'a>> {
    tree.all_nodes().find(|candidate| {
        candidate
            .children()
            .any(|child| child.name.as_ptr() == node.name.as_ptr())
    })
}

fn interrupt_parent<'a>(tree: &'a Fdt<'a>, mut node: Node<'a>) -> Result<Node<'a>, &'static str> {
    loop {
        if node.property("interrupt-parent").is_some() {
            return tree
                .find_phandle(number(node, "interrupt-parent")?)
                .ok_or("FDT 中断父 phandle 没有对应控制器");
        }
        node = parent(tree, node).ok_or("FDT 缺少中断父控制器关系")?;
    }
}

fn require_controller(node: Node<'_>, expected: &str, count: u32) -> Result<(), &'static str> {
    if !enabled(node)
        || !compatible(node, expected)
        || node.property("interrupt-controller").is_none()
    {
        return Err("FDT 中断链不是所选平台的实际控制器");
    }
    if number(node, "#interrupt-cells")? != count {
        return Err("FDT 中断控制器 cell 格式与实际绑定不符");
    }
    Ok(())
}

fn regions<'a>(
    node: Node<'a>,
) -> Result<impl Iterator<Item = Result<Region, &'static str>> + 'a, &'static str> {
    let raw = node.raw_reg().ok_or("FDT 资源节点缺少 reg")?;
    let value = property(node, "reg")?;
    let first = node
        .raw_reg()
        .and_then(|mut r| r.next())
        .ok_or("FDT reg 为空或被截断")?;
    let stride = first.address.len() + first.size.len();
    if stride == 0
        || value.len() % stride != 0
        || first.address.is_empty()
        || first.address.len() > 8
        || first.size.is_empty()
        || first.size.len() > 8
    {
        return Err("FDT reg 地址长度格式无效");
    }
    Ok(raw.map(|reg| Region::from_size(cells(reg.address)?, cells(reg.size)?)))
}

fn identity_address<'a>(tree: &'a Fdt<'a>, mut node: Node<'a>) -> Result<(), &'static str> {
    while let Some(bus) = parent(tree, node) {
        if bus.name != "/" && !bus.property("ranges").is_some_and(|p| p.value.is_empty()) {
            return Err("FDT 当前物理资源的父总线不是支持的恒等映射");
        }
        node = bus;
    }
    Ok(())
}

fn single_region<'a>(tree: &'a Fdt<'a>, node: Node<'a>) -> Result<Region, &'static str> {
    identity_address(tree, node)?;
    let mut ranges = regions(node)?;
    let first = ranges.next().ok_or("FDT reg 无资源区间")??;
    if ranges.next().is_some() {
        return Err("FDT 当前设备含多个未支持的寄存器区间");
    }
    Ok(first)
}

fn irq(node: Node<'_>, controller: Node<'_>) -> Result<u32, &'static str> {
    let value = property(node, "interrupts")?;
    if value.len() != number(controller, "#interrupt-cells")? as usize * 4 {
        return Err("FDT UART/root 中断描述长度不符");
    }
    word(value, 0)
}

fn discover_tree(
    bytes: &[u8],
    raw_args: [usize; 3],
    bsp: usize,
    dtb_address: usize,
) -> Result<BootInfo, &'static str> {
    validate(bytes)?;
    let tree = Fdt::new(bytes).map_err(|_| "FDT 解析入口拒绝输入")?;
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
            return Err("FDT reserved-memory 不是物理恒等地址空间");
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
                return Err("FDT RAM 区间超过支持容量");
            }
            let region = region?;
            if info.memory[..info.memory_count]
                .iter()
                .any(|old| old.overlaps(region))
            {
                return Err("FDT RAM 区间重叠");
            }
            info.memory[info.memory_count] = region;
            info.memory_count += 1;
        }
    }
    if info.memory_count == 0 {
        return Err("FDT 缺少实际 RAM");
    }
    if !info.memory[..info.memory_count]
        .iter()
        .any(|r| r.start <= info.dtb.start && r.end >= info.dtb.end)
    {
        return Err("运行 DTB 不在已描述的 RAM 区间内");
    }
    let cpus = tree.find_node("/cpus").ok_or("FDT 缺少 cpus 节点")?;
    if !matches!(number(cpus, "#address-cells")?, 1 | 2) || number(cpus, "#size-cells")? != 0 {
        return Err("FDT CPU reg cell 格式无效");
    }
    for node in cpus.children().filter(|n| enabled(*n)) {
        if node.property("device_type").and_then(|p| p.as_str()) != Some("cpu") {
            continue;
        }
        let reg = property(node, "reg")?;
        if reg.len() != number(cpus, "#address-cells")? as usize * 4 {
            return Err("FDT CPU reg 为空、被截断或包含多个 ID");
        }
        let id = cells(reg)?;
        if info.hardware_ids[..info.cpu_count].contains(&id) {
            return Err("FDT CPU 硬件 ID 重复");
        }
        if info.cpu_count == MAX_CPUS {
            return Err("FDT 可用 CPU 超过四核容量");
        }
        info.hardware_ids[info.cpu_count] = id;
        info.cpu_count += 1;
    }
    let bsp_index = info.hardware_ids[..info.cpu_count]
        .iter()
        .position(|id| *id == bsp)
        .ok_or("FDT 可用 CPU 不包含实际 BSP")?;
    info.hardware_ids.swap(0, bsp_index);
    #[cfg(target_arch = "riscv64")]
    {
        info.counter_frequency = number(cpus, "timebase-frequency")
            .map_err(|_| "FDT cpus 缺少有效 timebase-frequency")?
            as u64;
    }
    #[cfg(target_arch = "loongarch64")]
    {
        info.counter_frequency = crate::arch::counter_frequency();
    }
    if info.counter_frequency == 0 {
        return Err("实际硬件 counter frequency 为零");
    }
    let chosen = tree.find_node("/chosen").ok_or("FDT 缺少 chosen 节点")?;
    let stdout = cstr(property(chosen, "stdout-path").map_err(|_| "FDT chosen 缺少 stdout-path")?)?;
    let path = stdout.split(':').next().ok_or("FDT stdout-path 无效")?;
    let uart = tree
        .find_node(path)
        .ok_or("FDT stdout-path 没有对应 UART")?;
    if !enabled(uart) || !compatible(uart, "ns16550a") {
        return Err("FDT chosen UART 不是可用 ns16550a");
    }
    info.interrupts.uart = single_region(&tree, uart)?;
    if info.interrupts.uart.start != EARLY_UART_BASE {
        return Err("FDT chosen UART 与所选早期 UART 不一致");
    }
    let controller = interrupt_parent(&tree, uart)?;
    info.interrupts.uart_source = irq(uart, controller)?;
    info.interrupts.controller = single_region(&tree, controller)?;
    #[cfg(target_arch = "riscv64")]
    discover_plic(&tree, cpus, controller, &mut info)?;
    #[cfg(target_arch = "loongarch64")]
    {
        require_controller(controller, "loongson,pch-pic-1.0", 2)?;
        if word(property(uart, "interrupts")?, 4)? != 4 || info.interrupts.uart_source >= 64 {
            return Err("FDT UART 不是支持的 PCH-PIC 高电平源");
        }
        let extioi = interrupt_parent(&tree, controller)?;
        require_controller(extioi, "loongson,ls2k2000-eiointc", 1)?;
        // EXTIOI 的 reg 是 IOCSR 地址，不能加入普通 MMIO 映射。
        info.interrupts.parent_controller = single_region(&tree, extioi)?;
        let root = interrupt_parent(&tree, extioi)?;
        require_controller(root, "loongson,cpu-interrupt-controller", 1)?;
        info.interrupts.root_interrupt = irq(extioi, root)?;
        if info.interrupts.root_interrupt >= 8 {
            return Err("FDT EXTIOI 的 CPU 根中断超出支持范围");
        }
    }
    discover_pci(&tree, &mut info)?;
    Ok(info)
}

fn discover_pci(tree: &Fdt<'_>, info: &mut BootInfo) -> Result<(), &'static str> {
    let mut hosts = tree
        .all_nodes()
        .filter(|n| enabled(*n) && compatible(*n, "pci-host-ecam-generic"));
    let host = hosts.next().ok_or("FDT 缺少所选 virt 的 PCI ECAM host")?;
    if hosts.next().is_some() {
        return Err("FDT 含多个未支持的 PCI host");
    }
    info.pci.configuration = single_region(tree, host)?;
    if number(host, "#address-cells")? != 3 || number(host, "#size-cells")? != 2 {
        return Err("FDT PCI ranges cell 格式不符");
    }
    let parent = parent(tree, host).ok_or("FDT PCI host 缺少父总线")?;
    if parent.cell_sizes().address_cells != 2 {
        return Err("FDT PCI host 的物理地址不是双 cell");
    }
    let buses = property(host, "bus-range")?;
    if buses.len() != 8 {
        return Err("FDT PCI bus-range 被截断");
    }
    info.pci.bus_start = word(buses, 0)?;
    info.pci.bus_end = word(buses, 4)?;
    if info.pci.bus_start > info.pci.bus_end || info.pci.bus_end > 255 {
        return Err("FDT PCI bus-range 无效");
    }
    let ranges = property(host, "ranges")?;
    if ranges.is_empty() || ranges.len() % 28 != 0 {
        return Err("FDT PCI ranges 被截断");
    }
    for range in ranges.chunks_exact(28) {
        if info.pci.range_count == info.pci.ranges.len() {
            return Err("FDT PCI 窗口超过支持容量");
        }
        let flags = word(range, 0)?;
        if !matches!(flags & 0x0300_0000, 0x0100_0000 | 0x0200_0000 | 0x0300_0000) {
            return Err("FDT PCI 窗口不是 IO 或内存空间");
        }
        let bus_start = cells(&range[4..12])?;
        let cpu = Region::from_size(cells(&range[12..20])?, cells(&range[20..28])?)?;
        Region::from_size(bus_start, cpu.end - cpu.start)?;
        info.pci.ranges[info.pci.range_count] = crate::boot::PciRange {
            flags,
            bus_start,
            cpu,
        };
        info.pci.range_count += 1;
    }
    Ok(())
}

#[cfg(target_arch = "riscv64")]
fn discover_plic(
    tree: &Fdt<'_>,
    cpus: Node<'_>,
    plic: Node<'_>,
    info: &mut BootInfo,
) -> Result<(), &'static str> {
    require_controller(plic, "riscv,plic0", 1)?;
    let sources = number(plic, "riscv,ndev")?;
    if info.interrupts.uart_source == 0 || info.interrupts.uart_source > sources {
        return Err("FDT UART 中断源超出实际 PLIC 容量");
    }
    let mut handles = [0; MAX_CPUS];
    for cpu in cpus.children().filter(|n| enabled(*n)) {
        if cpu.property("device_type").and_then(|p| p.as_str()) != Some("cpu") {
            continue;
        }
        let id = cells(property(cpu, "reg")?)?;
        let logical = info.hardware_ids[..info.cpu_count]
            .iter()
            .position(|old| *old == id)
            .ok_or("PLIC CPU 未在可用 CPU 表中")?;
        let intc = cpu
            .children()
            .find(|n| compatible(*n, "riscv,cpu-intc"))
            .ok_or("FDT CPU 缺少实际本地中断控制器")?;
        require_controller(intc, "riscv,cpu-intc", 1)?;
        handles[logical] = number(intc, "phandle")?;
        if handles[logical] == 0
            || handles
                .iter()
                .enumerate()
                .any(|(i, value)| i != logical && *value == handles[logical])
        {
            return Err("FDT CPU 中断控制器 phandle 无效或重复");
        }
    }
    let extended =
        property(plic, "interrupts-extended").map_err(|_| "FDT PLIC 缺少 interrupts-extended")?;
    if extended.is_empty() || extended.len() % 8 != 0 {
        return Err("FDT PLIC context 描述被截断");
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
            .ok_or("PLIC context 引用了不存在的 CPU 控制器")?;
        require_controller(intc, "riscv,cpu-intc", 1)?;
        if cause != 9 {
            continue;
        }
        if let Some(logical) = handles[..info.cpu_count]
            .iter()
            .position(|old| *old == handle)
        {
            if found[logical] {
                return Err("FDT CPU 含重复 S-mode PLIC context");
            }
            info.interrupts.plic_contexts[logical] = context;
            found[logical] = true;
        }
    }
    if found[..info.cpu_count].iter().any(|present| !present) {
        return Err("FDT PLIC 缺少某个可用 CPU 的 S-mode context");
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
        return Err("固件 DTB 物理地址为空或未对齐");
    }
    let pointer = crate::arch::phys_to_virt(physical) as *const u8;
    let header = core::slice::from_raw_parts(pointer, 40);
    if word(header, 0)? != 0xd00d_feed {
        return Err("固件 DTB magic 错误");
    }
    let total = word(header, 4)? as usize;
    if total < 40
        || total > MAX_DTB_SIZE
        || physical.checked_add(total).is_none()
        || (pointer as usize).checked_add(total).is_none()
    {
        return Err("固件 DTB 长度无效或地址溢出");
    }
    discover_tree(
        core::slice::from_raw_parts(pointer, total),
        raw_args,
        bsp_hardware_id,
        physical,
    )
}

/// 在测试构建中验证真实 DTB 的原始边界，并拒绝截断头部和数据块。
#[cfg(feature = "boot-tests")]
pub fn test_input_boundaries() -> bool {
    static mut COPY: [u8; MAX_DTB_SIZE] = [0; MAX_DTB_SIZE];
    let live = crate::boot::info();
    let physical = live.dtb.start;
    let size = live.dtb.end - physical;
    // 只由 BSP 的有限自检调用；LA 固件把 DTB 填充到 1 MiB，不能放进启动栈。
    let bytes = unsafe {
        let copy = core::ptr::addr_of_mut!(COPY).cast::<u8>();
        core::ptr::copy_nonoverlapping(
            crate::arch::phys_to_virt(physical) as *const u8,
            copy,
            size,
        );
        core::slice::from_raw_parts_mut(copy, size)
    };
    if validate(&bytes).is_err() || !(0..bytes.len()).all(|end| validate(&bytes[..end]).is_err()) {
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
        let rejected = validate(&bytes).is_err();
        bytes[field..field + 4].copy_from_slice(&saved);
        if !rejected {
            return false;
        }
    }
    true
}
