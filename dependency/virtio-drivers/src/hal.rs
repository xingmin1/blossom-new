use core::ptr::NonNull;

/// 设备地址域中的 DMA 地址；不表示 CPU 指针或 CPU 物理地址。
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct DmaAddr(u64);

impl DmaAddr {
    /// 从已经由 DMA 后端验证的设备地址建立地址值。
    pub const fn new(value: u64) -> Self {
        Self(value)
    }

    /// 返回协议寄存器和 descriptor 使用的完整设备地址。
    pub const fn value(self) -> u64 {
        self.0
    }

    /// 检查字节偏移在设备地址域中的加法。
    pub fn checked_add(self, offset: usize) -> Option<Self> {
        self.0.checked_add(u64::try_from(offset).ok()?).map(Self)
    }
}

/// 一次设备访问期间的缓冲区方向。
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum BufferDirection {
    /// 设备只读；调用者在设备访问期间不得修改内容。
    DriverToDevice,
    /// 设备可写；调用者在设备访问期间不得访问内容。
    DeviceToDriver,
}

/// 已经映射并由调用者持有其资源的一个设备可见片段。
#[derive(Copy, Clone, Debug)]
pub struct MappedSegment {
    /// 片段首字节的设备地址。
    pub address: DmaAddr,
    /// 片段字节数，受 descriptor 的 32 位长度限制。
    pub length: u32,
    /// 设备访问方向。
    pub direction: BufferDirection,
}

/// 连贯 DMA 内存的原始视图，不拥有内存，也不授予解引用权限。
///
/// 使用该视图的 unsafe 接口须使原页和 CPU 映射覆盖所有消费对象的 CPU 访问期，
/// 并在归还前另外确认设备访问已结束；设备停止不终止仍存活对象的 CPU 访问义务。
#[derive(Copy, Clone, Debug)]
pub struct DmaRegion {
    /// 实际连贯内存的 CPU 映射起点。
    pub cpu: NonNull<u8>,
    /// 同一内存的设备地址起点。
    pub device: DmaAddr,
    /// 实际映射的字节数。
    pub len: usize,
}

/// 平台提供的设备 DMA 屏障，不承担分配、映射或释放职责。
///
/// # Safety
///
/// 实现必须同时约束编译器重排及平台的 CPU/设备访问顺序；普通 Rust 原子屏障
/// 不能代替平台设备屏障。三个方法均不得分配或阻塞。
pub unsafe trait Hal {
    /// 使此前 descriptor、avail 和数据写入先于随后向设备发布的写入。
    fn dma_write_barrier();

    /// 使此前观察到的设备完成先于随后读取 used 元素及设备写入的数据。
    fn dma_read_barrier();

    /// 完整排序 CPU 与设备读写，用于发布后的通知判断及中断重装复查。
    fn dma_barrier();
}
