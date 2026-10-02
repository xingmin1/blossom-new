//! 启动输入与阶段二接管的物理资源；这里只记录实际区间，不提供运行期分配器。

pub const MAX_CPUS: usize = 4;
pub const MAX_MEMORY_REGIONS: usize = 8;
pub const MAX_RESERVED_REGIONS: usize = 24;

/// 半开物理地址区间。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Region {
    pub start: usize,
    pub end: usize,
}

impl Region {
    /// 构造外部资源区间，在解析边界拒绝地址溢出和空区间。
    pub fn from_size(start: usize, size: usize) -> Result<Self, &'static str> {
        let end = start.checked_add(size).ok_or("物理资源区间溢出")?;
        if end <= start {
            return Err("物理资源区间为空");
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

/// 记录真实 PCI 配置与窗口，不在启动阶段执行枚举。
#[derive(Clone, Copy, Debug, Default)]
pub struct PciResources {
    pub configuration: Region,
    pub ranges: [PciRange; 4],
    pub range_count: usize,
    pub bus_start: u32,
    pub bus_end: u32,
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
    pub pci: PciResources,
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
            pci: PciResources::default(),
            counter_frequency: 0,
        }
    }

    /// 在输入边界登记一段实际保留资源。
    pub fn reserve(&mut self, region: Region) -> Result<(), &'static str> {
        if self.reserved_count == MAX_RESERVED_REGIONS {
            return Err("启动保留区数量超过支持容量");
        }
        self.reserved[self.reserved_count] = region;
        self.reserved_count += 1;
        Ok(())
    }
}

struct BootStorage(core::cell::UnsafeCell<core::mem::MaybeUninit<BootInfo>>);
// BSP 在释放 AP 前写入一次；发布以后快照只读，AP 上线握手包含其可见性。
unsafe impl Sync for BootStorage {}

static BOOT_INFO: BootStorage = BootStorage(core::cell::UnsafeCell::new(core::mem::MaybeUninit::uninit()));

/// 从附近高地址链接符号取得完整内核物理保留区，包含栈和启动表。
pub fn kernel_region() -> Region {
    extern "C" {
        static __kernel_virt_start: u8;
        static __kernel_virt_end: u8;
    }
    unsafe {
        Region {
            start: crate::arch::virt_to_phys(core::ptr::addr_of!(__kernel_virt_start) as usize),
            end: crate::arch::virt_to_phys(core::ptr::addr_of!(__kernel_virt_end) as usize),
        }
    }
}

/// 发布已解析、已登记内核拥有区间的唯一启动快照。
///
/// # 安全
/// 只能由 BSP 在 AP 尚未发布时写入一次，之后不得修改。
pub unsafe fn publish(info: BootInfo) {
    (*BOOT_INFO.0.get()).write(info);
}

/// 返回初始化顺序已保证存在的真实启动快照，不在每层重复检查 ready。
pub fn info() -> &'static BootInfo {
    unsafe { (*BOOT_INFO.0.get()).assume_init_ref() }
}
