//! 当前 QEMU coherent 直连 DMA：页拥有权、设备地址与访问期分别保存。

use crate::arch;
use crate::block::IoResult;
use crate::mm::bootmem::PAGE_SIZE;
use crate::mm::buddy::MAX_ORDER;
use crate::mm::{alloc_pages, pages as page_map, FolioRead, OwnedPages};
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
        let device = direct_device_address(pages.phys_addr(), pages.len(), 0, pages.len())?;
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

/// 映射始终保留原来源的完整拥有者，不将共享 folio 还原为独占原页。
enum PayloadBacking {
    Pages(OwnedPages),
    Folio(FolioRead),
}

impl PayloadBacking {
    /// 两种来源都返回 allocator 管理的原页地址，不授予 CPU 内容访问权。
    fn phys_addr(&self) -> usize {
        match self {
            Self::Pages(pages) => pages.phys_addr(),
            Self::Folio(read) => read.phys_addr(),
        }
    }
}

/// 在同一原页块内映射一个非空 payload 范围，不复制数据或拆散原阶。
/// 设备拥有期间 CPU 数据入口和归还入口均拒绝访问。
pub struct MappedPages {
    // 活动析构即使因断言进入 unwind，也不能让字段自动释放设备仍访问的页。
    backing: ManuallyDrop<PayloadBacking>,
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
        let address = direct_device_address(pages.phys_addr(), pages.len(), offset, len)?;
        Ok(Self {
            backing: ManuallyDrop::new(PayloadBacking::Pages(pages)),
            offset,
            segment: MappedSegment {
                address,
                length,
                direction,
            },
            device_owned: false,
        })
    }

    /// 原 folio 直接映射其已验证页内读入范围；准备失败完整返回唯一读入责任。
    pub(crate) fn from_folio(read: FolioRead) -> Result<Self, (DmaError, FolioRead)> {
        if read.is_completed() {
            return Err((DmaError::InvalidRange, read));
        }
        let offset = read.offset();
        let len = read.len();
        let address = match direct_device_address(read.phys_addr(), PAGE_SIZE, offset, len) {
            Ok(address) => address,
            Err(error) => return Err((error, read)),
        };
        Ok(Self {
            backing: ManuallyDrop::new(PayloadBacking::Folio(read)),
            offset,
            segment: MappedSegment {
                address,
                length: len as u32,
                direction: BufferDirection::DeviceToDriver,
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

    /// 已发布结果的 folio 只允许读取，不能作为新 Bio 的可写 DMA 来源。
    pub(crate) fn is_folio_complete(&self) -> bool {
        matches!(&*self.backing, PayloadBacking::Folio(read) if read.is_completed())
    }

    /// 在 CPU 拥有期间独占 payload 字节存储；folio 完成后禁止再次可变访问。
    pub fn bytes_mut(&mut self) -> &mut [MaybeUninit<u8>] {
        assert!(!self.device_owned, "CPU access during DMA device ownership");
        let len = self.len();
        match &mut *self.backing {
            PayloadBacking::Pages(pages) => {
                // SAFETY: new 核对完整原块和 offset/len，当前 CPU-owned；
                // 可变借用排除并发交权，MaybeUninit 不要求字节已经初始化。
                unsafe { slice::from_raw_parts_mut(pages.as_mut_ptr().add(self.offset), len) }
            }
            PayloadBacking::Folio(read) => read.bytes_mut(),
        }
    }

    /// 在 CPU 拥有期间读取已初始化的 payload，不根据完成状态猜测实际写入量。
    ///
    /// # Safety
    /// 调用者须证明整个 payload 已初始化；短读或失败完成不能证明未写尾部有效。
    /// 所有先前导出的裸别名必须遵守本对象的独占设备访问期，不能并发写此借用。
    pub unsafe fn bytes(&self) -> &[u8] {
        assert!(!self.device_owned, "CPU access during DMA device ownership");
        let pointer = (arch::phys_to_virt(self.backing.phys_addr()) + self.offset) as *const u8;
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
        assert!(
            !self.is_folio_complete(),
            "completed folio cannot enter DMA"
        );
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

    /// Bio 全部义务结束且 CPU 收权后发布 folio 结果；独占原页来源不含填充责任。
    ///
    /// # Safety
    /// 本映射的全部设备访问和可变 CPU 别名必须已经结束；完整成功必须保证
    /// payload 全范围已经初始化，completed 必须属于本映射的连续成功前缀。
    pub(crate) unsafe fn complete_folio(&mut self, result: IoResult) {
        assert!(!self.device_owned, "completing device-owned folio");
        if let PayloadBacking::Folio(read) = &mut *self.backing {
            // SAFETY: 调用者保证真实收权和初始化，本映射固定为 read 的完整原范围。
            unsafe { read.complete(result) };
        }
    }
}

impl Drop for MappedPages {
    /// 只有结束设备访问期的 payload 才释放原来源；活动状态报告错误并保留整个拥有者。
    fn drop(&mut self) {
        assert!(!self.device_owned, "dropping device-owned DMA pages");
        // SAFETY: 原来源只由构造器移入且未取出；设备访问已停止，CPU 借用已结束。
        // folio 的读入责任由 Bio 发布，未提交者由其原 Drop 结束；不转换原页归还权。
        unsafe { ManuallyDrop::drop(&mut self.backing) };
    }
}

/// 当前 coherent 直连后端只接受原 allocator 管理的真实 RAM，设备地址使用独立值类型。
/// 校验完整原块避免范围越过 RAM 孔洞或保留区，再验证 payload 与 CPU/device 末端。
fn direct_device_address(
    physical: usize,
    allocation_len: usize,
    offset: usize,
    len: usize,
) -> Result<DmaAddr, DmaError> {
    let end = offset.checked_add(len).ok_or(DmaError::InvalidRange)?;
    if len == 0 || allocation_len < end {
        return Err(DmaError::InvalidRange);
    }
    let block_end = physical
        .checked_add(allocation_len)
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
    if cpu == 0 || cpu.checked_add(allocation_len).is_none() {
        return Err(DmaError::AddressLimit);
    }
    let address = DmaAddr::new(u64::try_from(physical).map_err(|_| DmaError::AddressLimit)?)
        .checked_add(offset)
        .ok_or(DmaError::AddressLimit)?;
    address.checked_add(len).ok_or(DmaError::AddressLimit)?;
    Ok(address)
}
