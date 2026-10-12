//! 固件资源与内核保留区的不可变快照，由 BSP 在释放 AP 前发布。

use spin::Once;

pub const MAX_CPUS: usize = 4;
pub const MAX_MEMORY_REGIONS: usize = 8;
pub const MAX_RESERVED_REGIONS: usize = 24;
pub const MAX_VIRTIO_MMIO: usize = 8;
/// 所选控制器的 source 表容量，PLIC 包含保留的零号源。
pub const MAX_INTERRUPT_SOURCES: usize = if cfg!(target_arch = "riscv64") {
    96
} else {
    64
};

/// 半开物理地址区间。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Region {
    pub start: usize,
    pub end: usize,
}

impl Region {
    /// 构造外部资源区间，在解析边界拒绝地址溢出和空区间。
    pub fn from_size(start: usize, size: usize) -> Result<Self, &'static str> {
        let end = start.checked_add(size).ok_or("physical region overflows")?;
        if end <= start {
            return Err("physical region is empty");
        }
        Ok(Self { start, end })
    }

    /// 判断两个半开区间是否相交。
    pub fn overlaps(self, other: Self) -> bool {
        self.start < other.end && other.start < self.end
    }
}

/// QEMU 实际提供的 UART 和中断控制器资源。
#[derive(Clone, Copy, Debug, Default)]
pub struct InterruptResources {
    pub uart: Region,
    pub uart_source: u32,
    /// 合法 source 索引的开区间上界；PLIC 的零号不用于设备。
    pub source_count: u32,
    pub controller: Region,
    pub parent_controller: Region,
    pub root_interrupt: u32,
    pub plic_contexts: [usize; MAX_CPUS],
}

/// 已选 PCI host 提供的总线地址到物理窗口映射。
#[derive(Clone, Copy, Debug, Default)]
pub struct PciRange {
    pub flags: u32,
    pub bus_start: usize,
    pub cpu: Region,
}

/// 启用的 VirtIO-MMIO 槽资源，设备类型由普通初始化读取。
#[derive(Clone, Copy, Debug, Default)]
pub struct VirtioMmioResource {
    pub registers: Region,
    pub interrupt_source: u32,
}

/// PCI 子地址与 INTx pin 经 host 掩码匹配到的控制器源。
#[derive(Clone, Copy, Debug, Default)]
pub struct PciIntxRoute {
    pub child: [u32; 4],
    pub source: u32,
}

/// 记录真实 PCI 配置与窗口，不在启动阶段执行枚举。
#[derive(Clone, Copy, Debug, Default)]
pub struct PciResources {
    pub configuration: Region,
    pub ranges: [PciRange; 4],
    pub range_count: usize,
    pub bus_start: u32,
    pub bus_end: u32,
    pub intx_mask: [u32; 4],
    pub intx_routes: [PciIntxRoute; 16],
    pub intx_route_count: usize,
}

impl PciResources {
    /// 以真实 BDF 高 cell 和 pin 匹配 host INTx 表，不使用 interrupt-line 字节。
    pub fn interrupt_source(
        &self,
        bus: u8,
        device: u8,
        function: u8,
        pin: u8,
    ) -> Result<u32, &'static str> {
        if (bus as u32) < self.bus_start
            || self.bus_end < bus as u32
            || 32 <= device
            || 8 <= function
            || !(1..=4).contains(&pin)
        {
            return Err("PCI INTx BDF or pin is out of range");
        }
        let child = [
            ((bus as u32) << 16) | ((device as u32) << 11) | ((function as u32) << 8),
            0,
            0,
            pin as u32,
        ];
        self.intx_routes[..self.intx_route_count]
            .iter()
            .find(|route| (0..4).all(|i| ((route.child[i] ^ child[i]) & self.intx_mask[i]) == 0))
            .map(|route| route.source)
            .ok_or("PCI INTx has no host interrupt-map route")
    }
}

/// 一次解析完成、由 BSP 发布的启动资源快照。
#[derive(Clone, Copy, Debug)]
pub struct BootInfo {
    pub raw_args: [usize; 3],
    pub bsp_hardware_id: usize,
    pub cpu_count: usize,
    pub hardware_ids: [usize; MAX_CPUS],
    pub memory_count: usize,
    pub memory: [Region; MAX_MEMORY_REGIONS],
    pub reserved_count: usize,
    pub reserved: [Region; MAX_RESERVED_REGIONS],
    pub dtb: Region,
    pub interrupts: InterruptResources,
    /// 唯一启用 RTC 的物理寄存器区间，运行期只用于 BSP 初始日历采样。
    pub rtc: Region,
    pub pci: PciResources,
    pub virtio_mmio: [VirtioMmioResource; MAX_VIRTIO_MMIO],
    pub virtio_mmio_count: usize,
    pub bootargs: [u8; 256],
    pub bootargs_len: usize,
    pub counter_frequency: u64,
}

impl BootInfo {
    /// 创建仅供边界解析填充的空快照；计数决定哪些数组槽位有效。
    pub fn empty(raw_args: [usize; 3], bsp_hardware_id: usize) -> Self {
        Self {
            raw_args,
            bsp_hardware_id,
            cpu_count: 0,
            hardware_ids: [0; MAX_CPUS],
            memory_count: 0,
            memory: [Region::default(); MAX_MEMORY_REGIONS],
            reserved_count: 0,
            reserved: [Region::default(); MAX_RESERVED_REGIONS],
            dtb: Region::default(),
            interrupts: InterruptResources::default(),
            rtc: Region::default(),
            pci: PciResources::default(),
            virtio_mmio: [VirtioMmioResource::default(); MAX_VIRTIO_MMIO],
            virtio_mmio_count: 0,
            bootargs: [0; 256],
            bootargs_len: 0,
            counter_frequency: 0,
        }
    }

    /// 借用固件命令行的实际字节前缀，不包含终止 NUL 或补齐空间。
    pub fn bootargs(&self) -> &[u8] {
        &self.bootargs[..self.bootargs_len]
    }

    /// 在输入边界登记一段实际保留资源。
    pub fn reserve(&mut self, region: Region) -> Result<(), &'static str> {
        if self.reserved_count == MAX_RESERVED_REGIONS {
            return Err("boot reservation capacity exceeded");
        }
        self.reserved[self.reserved_count] = region;
        self.reserved_count += 1;
        Ok(())
    }
}

static BOOT_INFO: Once<BootInfo> = Once::new();

/// 从附近高地址链接符号取得完整内核物理保留区，包含栈和启动表。
pub fn kernel_region() -> Region {
    extern "C" {
        static __kernel_virt_start: u8;
        static __kernel_virt_end: u8;
    }
    Region {
        start: crate::arch::virt_to_phys(core::ptr::addr_of!(__kernel_virt_start) as usize),
        end: crate::arch::virt_to_phys(core::ptr::addr_of!(__kernel_virt_end) as usize),
    }
}

/// 发布实际固件资源和当前内核拥有的物理区间；AP 读取以前完成初始化。
///
/// # Safety
/// 只能由 BSP 在 AP 尚未释放、IRQ 关闭时执行一次。info 必须来自选定机器的
/// 实际固件，已验证数组计数、物理区间和设备寄存器，并包含内核保留区。
pub unsafe fn publish(info: BootInfo) {
    assert!(BOOT_INFO.get().is_none(), "BootInfo is already published");
    BOOT_INFO.call_once(|| info);
}

/// 借用已发布的真实启动资源；发布前调用会在这个边界失败。
pub fn info() -> &'static BootInfo {
    BOOT_INFO.get().expect("BootInfo has not been published")
}
