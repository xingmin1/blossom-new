//! 当前 QEMU coherent 直连 DMA：页拥有权、设备地址与访问期分别保存。

use crate::arch;
use crate::mm::bootmem::PAGE_SIZE;
use crate::mm::buddy::MAX_ORDER;
use crate::mm::{alloc_pages, pages as page_map, OwnedPages};
use core::convert::TryFrom;
use core::mem::{ManuallyDrop, MaybeUninit};
use core::ptr::NonNull;
use core::slice;
use virtio_drivers::{BufferDirection, DmaAddr, DmaRegion, Hal, MappedSegment};

/// DMA 预备失败；地址域错误不会自动使用另一分配器或 bounce 缓冲区。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DmaError {
    NoMemory,
    InvalidRange,
    AddressLimit,
}

/// 队列共享的设备屏障后端，不承担页分配或隐式映射职责。
pub struct KernelHal;

// SAFETY: 两个支持的 ISA 后端均执行真实设备排序指令；内联汇编保留编译器
// memory 副作用，既不使用 nomem，也不使用 readonly，普通原子排序不能替代这些屏障。
unsafe impl Hal for KernelHal {
    /// 在发表设备可见索引前，排序先前的 coherent 内存写入。
    fn dma_write_barrier() {
        arch::dma_write_barrier();
    }

    /// 在观察设备完成后，排序随后对 coherent 数据的读取。
    fn dma_read_barrier() {
        arch::dma_read_barrier();
    }

    /// 排序 coherent 内存与设备通知、状态之间的双向访问。
    fn dma_barrier() {
        arch::dma_barrier();
    }
}

/// 控制区保存原连续页块及原阶；原始视图不复制页归还权。
/// 消费视图的对象结束全部 CPU 访问且设备已停止访问前，unsafe 调用者须保活本拥有者与映射。
pub struct CoherentDma {
    pages: OwnedPages,
    device: DmaAddr,
}

impl CoherentDma {
    /// 非阻塞申请容纳 bytes 的最小整阶页块，退出 MM 锁后清零完整分配。
    /// 零长度不构造设备内存；真实连续块不足或超过原页阶上限返回 NoMemory。
    pub fn allocate(bytes: usize) -> Result<Self, DmaError> {
        if bytes == 0 {
            return Err(DmaError::InvalidRange);
        }
        let count = (bytes - 1) / PAGE_SIZE + 1;
        let count = count
            .checked_next_power_of_two()
            .ok_or(DmaError::NoMemory)?;
        let order = count.trailing_zeros() as usize;
        if MAX_ORDER < order {
            return Err(DmaError::NoMemory);
        }
        let pages = alloc_pages(order, true).ok_or(DmaError::NoMemory)?;
        let device = direct_device_address(&pages, 0, pages.len())?;
        Ok(Self { pages, device })
    }

    /// 返回完整控制页块的原始 CPU/device 视图，不授予安全解引用或长期 slice。
    /// 使用者须证明实际页及映射覆盖全部消费对象的 CPU 访问期，并另行确认设备停止。
    pub fn region(&self) -> DmaRegion {
        DmaRegion {
            cpu: NonNull::new(arch::phys_to_virt(self.pages.phys_addr()) as *mut u8)
                .expect("DMA direct mapping is null"),
            device: self.device,
            len: self.pages.len(),
        }
    }

    /// 返回已清零并实际持有的整块字节数，包含最小整阶分配的尾部空间。
    pub fn byte_len(&self) -> usize {
        self.pages.len()
    }

    /// 返回页拥有者保存的原阶，归还时仍由同一 OwnedPages 消费。
    pub fn order(&self) -> usize {
        self.pages.order()
    }
}

/// 在同一原页块内映射一个非空 payload 范围，不复制数据或拆散原阶。
/// 设备拥有期间 CPU 数据入口和归还入口均拒绝访问。
pub struct MappedPages {
    // 活动析构即使因断言进入 unwind，也不能让字段自动释放设备仍访问的页。
    pages: ManuallyDrop<OwnedPages>,
    offset: usize,
    segment: MappedSegment,
    device_owned: bool,
}

impl MappedPages {
    /// 消耗原页拥有者并验证范围和 descriptor 长度，失败按原阶归还页。
    /// 新映射处于 CPU 拥有状态；未清零来源仍保留其初始化义务。
    pub fn new(
        pages: OwnedPages,
        offset: usize,
        len: usize,
        direction: BufferDirection,
    ) -> Result<Self, DmaError> {
        let length = u32::try_from(len).map_err(|_| DmaError::InvalidRange)?;
        let address = direct_device_address(&pages, offset, len)?;
        Ok(Self {
            pages: ManuallyDrop::new(pages),
            offset,
            segment: MappedSegment {
                address,
                length,
                direction,
            },
            device_owned: false,
        })
    }

    /// 取得已验证的设备范围；复制此值不复制拥有权，也不允许独立结束访问期。
    pub fn segment(&self) -> MappedSegment {
        self.segment
    }

    /// 返回实际 payload 字节数，不包含原页块中未映射的前后部分。
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> usize {
        self.segment.length as usize
    }

    /// 返回建立映射时固定的设备访问方向。
    pub fn direction(&self) -> BufferDirection {
        self.segment.direction
    }

    /// 返回原页块内的 payload 起始偏移，供父请求检查子范围。
    pub fn offset(&self) -> usize {
        self.offset
    }

    /// 查询设备是否仍拥有该范围；此状态不代替真实 used 或 reset 证明。
    pub fn is_device_owned(&self) -> bool {
        self.device_owned
    }

    /// 在 CPU 拥有期间独占 payload 字节存储；MaybeUninit 不读取未初始化页。
    pub fn bytes_mut(&mut self) -> &mut [MaybeUninit<u8>] {
        assert!(!self.device_owned, "CPU access during DMA device ownership");
        let len = self.len();
        // SAFETY: new 已核对完整原块与 offset/len；本对象保有唯一页拥有权，
        // 当前处于 CPU 状态且可变借用禁止同时交权，MaybeUninit 不要求字节已初始化。
        unsafe { slice::from_raw_parts_mut(self.pages.as_mut_ptr().add(self.offset), len) }
    }

    /// 在 CPU 拥有期间读取已初始化的 payload，不根据完成状态猜测实际写入量。
    ///
    /// # Safety
    /// 调用者须证明整个 payload 已初始化；短读或失败完成不能证明未写尾部有效。
    /// 所有先前导出的裸别名必须遵守本对象的独占设备访问期，不能并发写此借用。
    pub unsafe fn bytes(&self) -> &[u8] {
        assert!(!self.device_owned, "CPU access during DMA device ownership");
        let pointer = (arch::phys_to_virt(self.pages.phys_addr()) + self.offset) as *const u8;
        // SAFETY: new 已核对原块和 CPU 直映范围，本对象保活且处于 CPU 状态；
        // 调用者保证全部字节初始化及没有并发写者，借用不能超出本对象寿命。
        unsafe { slice::from_raw_parts(pointer, self.len()) }
    }

    /// 在 descriptor 发表前将整个 payload 交给唯一设备访问期。
    ///
    /// # Safety
    /// 调用者必须结束所有 CPU 裸别名访问，并使本对象存活到对应全部子请求完成
    /// 或真实 reset 已停止 DMA；DriverToDevice 的全部传输字节必须已初始化。
    /// 子请求只可引用本映射的有界范围，不能自行收权、并发 CPU 访问或重复交权。
    pub unsafe fn begin_device_access(&mut self) {
        assert!(!self.device_owned, "DMA payload is already device owned");
        arch::dma_write_barrier();
        self.device_owned = true;
    }

    /// 在设备已结束全部范围访问后执行消费屏障并恢复 CPU 拥有状态。
    ///
    /// # Safety
    /// 调用者必须已验证全部引用该映射的真实 used，或已确认设备 reset 完成；
    /// 不得仅根据已写 status=0、错误返回、某一个子请求完成或超时收权。
    /// 此后设备不得继续访问该范围，其他执行者也不得并发改变拥有状态。
    pub unsafe fn finish_device_access(&mut self) {
        assert!(self.device_owned, "DMA payload is not device owned");
        arch::dma_read_barrier();
        self.device_owned = false;
    }

    /// 在非设备拥有状态恢复同一个原页拥有者，不按 payload 长度重算归还阶。
    pub fn into_pages(self) -> OwnedPages {
        let mut mapped = ManuallyDrop::new(self);
        assert!(!mapped.device_owned, "reclaiming device-owned DMA pages");
        // SAFETY: mapped 不再析构，且上面拒绝活动 DMA；take 只移动仍 live 的
        // 唯一原 OwnedPages，一次交还原阶归还权，之后不再访问 mapped 的页字段。
        unsafe { ManuallyDrop::take(&mut mapped.pages) }
    }
}

impl Drop for MappedPages {
    /// 只有结束设备访问期的 payload 才按原阶归还；活动状态立即报告协议错误。
    fn drop(&mut self) {
        assert!(!self.device_owned, "dropping device-owned DMA pages");
        // SAFETY: 原拥有者只由 new 移入且未取出；状态检查确认设备访问已停止，
        // 安全 CPU 借用随本对象结束，unsafe 裸别名使用者承担访问期义务。
        unsafe { ManuallyDrop::drop(&mut self.pages) };
    }
}

/// 当前 coherent 直连后端只接受原 allocator 管理的真实 RAM，设备地址使用独立值类型。
/// 校验完整原块避免范围越过 RAM 孔洞或保留区，再验证 payload 与 CPU/device 末端。
fn direct_device_address(
    pages: &OwnedPages,
    offset: usize,
    len: usize,
) -> Result<DmaAddr, DmaError> {
    let end = offset.checked_add(len).ok_or(DmaError::InvalidRange)?;
    if len == 0 || pages.len() < end {
        return Err(DmaError::InvalidRange);
    }
    let physical = pages.phys_addr();
    let block_end = physical
        .checked_add(pages.len())
        .ok_or(DmaError::AddressLimit)?;
    let head_pfn = physical / PAGE_SIZE;
    let end_pfn = block_end / PAGE_SIZE;
    if !page_map()
        .free_ranges()
        .iter()
        .any(|range| range.start <= head_pfn && end_pfn <= range.end)
    {
        return Err(DmaError::AddressLimit);
    }
    let cpu = arch::phys_to_virt(physical);
    if cpu == 0 || cpu.checked_add(pages.len()).is_none() {
        return Err(DmaError::AddressLimit);
    }
    let address = DmaAddr::new(u64::try_from(physical).map_err(|_| DmaError::AddressLimit)?)
        .checked_add(offset)
        .ok_or(DmaError::AddressLimit)?;
    address.checked_add(len).ok_or(DmaError::AddressLimit)?;
    Ok(address)
}
