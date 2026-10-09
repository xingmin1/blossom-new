//! 当前 QEMU coherent 直连 DMA：页拥有权、设备地址与访问期分别保存。

use crate::arch;
use crate::block::{BlockError, IoResult};
use crate::mm::bootmem::PAGE_SIZE;
use crate::mm::buddy::MAX_ORDER;
use crate::mm::{alloc_pages, pages as page_map, FolioRead, FolioWriteback, OwnedPages};
use core::convert::TryFrom;
use core::mem::{self, ManuallyDrop, MaybeUninit};
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
    Folio {
        read: FolioRead,
        publish_on_success: bool,
    },
    Writeback(FolioWriteback),
}

impl PayloadBacking {
    /// 每种来源都保留 allocator 管理的原页，不从共享页复制归还权。
    fn phys_addr(&self) -> usize {
        match self {
            Self::Pages(pages) => pages.phys_addr(),
            Self::Folio { read, .. } => read.phys_addr(),
            Self::Writeback(writeback) => writeback.phys_addr(),
        }
    }
}

/// 在同一原页块内映射一个非空 payload 范围，不复制数据或拆散原阶。
/// 设备拥有期间不提供 CPU 数据切片；文件回写可按原 folio 协议产生新的 Dirty。
pub struct MappedPages {
    // 活动析构即使因断言进入 unwind，也不能让字段自动释放设备仍访问的页。
    backing: ManuallyDrop<PayloadBacking>,
    offset: usize,
    segment: MappedSegment,
    device_owned: bool,
    accepted: usize,
    wb_error: Option<(usize, BlockError)>,
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
            accepted: 0,
            wb_error: None,
        })
    }

    /// 原 folio 直接映射其已验证页内读入范围；准备失败完整返回唯一读入责任。
    pub(crate) fn from_folio(read: FolioRead) -> Result<Self, (DmaError, FolioRead)> {
        let offset = read.offset();
        let len = read.len();
        Self::map_folio(read, offset, len, true)
    }

    /// 消耗整份读入责任，只将指定页内子范围交设备；成功后仍由来源结束整页填充。
    pub(crate) fn from_folio_range(
        read: FolioRead,
        offset: usize,
        len: usize,
    ) -> Result<Self, (DmaError, FolioRead)> {
        Self::map_folio(read, offset, len, false)
    }

    /// 验证当前可写范围并保存真实完成责任，不拆分读入者或复制原页。
    fn map_folio(
        read: FolioRead,
        offset: usize,
        len: usize,
        publish_on_success: bool,
    ) -> Result<Self, (DmaError, FolioRead)> {
        if read.is_completed()
            || offset < read.offset()
            || offset
                .checked_add(len)
                .is_none_or(|end| read.offset() + read.len() < end)
        {
            return Err((DmaError::InvalidRange, read));
        }
        let address = match direct_device_address(read.phys_addr(), PAGE_SIZE, offset, len) {
            Ok(address) => address,
            Err(error) => return Err((error, read)),
        };
        Ok(Self {
            backing: ManuallyDrop::new(PayloadBacking::Folio {
                read,
                publish_on_success,
            }),
            offset,
            segment: MappedSegment {
                address,
                length: len as u32,
                direction: BufferDirection::DeviceToDriver,
            },
            device_owned: false,
            accepted: 0,
            wb_error: None,
        })
    }

    /// 消耗本轮 WB 凭证，把已初始化范围直接用于 ToDevice；失败返回原义务。
    pub(crate) fn from_writeback(
        writeback: FolioWriteback,
    ) -> Result<Self, (DmaError, FolioWriteback)> {
        if writeback.is_completed() {
            return Err((DmaError::InvalidRange, writeback));
        }
        let offset = writeback.offset();
        let len = writeback.len();
        let address = match direct_device_address(writeback.phys_addr(), PAGE_SIZE, offset, len) {
            Ok(address) => address,
            Err(error) => return Err((error, writeback)),
        };
        Ok(Self {
            backing: ManuallyDrop::new(PayloadBacking::Writeback(writeback)),
            offset,
            segment: MappedSegment {
                address,
                length: len as u32,
                direction: BufferDirection::DriverToDevice,
            },
            device_owned: false,
            accepted: 0,
            wb_error: None,
        })
    }

    /// 全部设备责任结束后取回分段读入者，内容锁继续由该拥有者保存。
    pub(crate) fn into_folio(mut self) -> FolioRead {
        assert!(!self.device_owned, "taking a device-owned folio");
        assert!(
            matches!(
                &*self.backing,
                PayloadBacking::Folio {
                    publish_on_success: false,
                    ..
                }
            ),
            "taking a payload without continuing folio ownership"
        );
        // SAFETY: 本映射独占原来源且已真实收权；取出后立即跳过外壳析构，
        // 不再读取已移出字段，读入者仍持原页及尚未发表的内容锁责任。
        let backing = unsafe { ManuallyDrop::take(&mut self.backing) };
        mem::forget(self);
        match backing {
            PayloadBacking::Folio { read, .. } => read,
            PayloadBacking::Pages(_) | PayloadBacking::Writeback(_) => unreachable!(),
        }
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

    /// 已结束的初始化或 WB 凭证不能再次作为设备访问来源。
    pub(crate) fn is_folio_complete(&self) -> bool {
        match &*self.backing {
            PayloadBacking::Folio { read, .. } => read.is_completed(),
            PayloadBacking::Writeback(writeback) => writeback.is_completed(),
            PayloadBacking::Pages(_) => false,
        }
    }

    /// 只观察 folio 原页地址，不借用设备仍可写的内容。
    #[cfg(feature = "ktest")]
    pub(crate) fn ktest_folio_phys_addr(&self) -> Option<usize> {
        match &*self.backing {
            PayloadBacking::Folio { read, .. } => Some(read.phys_addr()),
            PayloadBacking::Writeback(writeback) => Some(writeback.phys_addr()),
            PayloadBacking::Pages(_) => None,
        }
    }

    /// 返回原 WB 页的终身角色，普通独占页和读入来源不属于写回。
    #[cfg(feature = "ktest")]
    pub(crate) fn ktest_writeback_metadata(&self) -> Option<bool> {
        match &*self.backing {
            PayloadBacking::Writeback(writeback) => Some(writeback.is_metadata()),
            _ => None,
        }
    }

    /// 只识别文件原页的读入凭证，raw 页和 WB 来源均不属于文件读入。
    #[cfg(feature = "ktest")]
    pub(crate) fn ktest_file_read(&self) -> bool {
        matches!(&*self.backing, PayloadBacking::Folio { read, .. } if read.ktest_is_file())
    }

    /// 原父在真实 submit 成功后登记本映射的连续接受窗口，不改变设备拥有期。
    pub(crate) fn accept_range(&mut self, offset: usize, len: usize) {
        assert_eq!(
            offset, self.accepted,
            "payload acceptance is not contiguous"
        );
        assert!(len <= self.len() - offset);
        self.accepted += len;
    }

    /// 同一父账记录本映射实际失败位置，后部反序完成不覆盖前部错误。
    pub(crate) fn record_writeback_result(
        &mut self,
        offset: usize,
        len: usize,
        completed: usize,
        error: Option<BlockError>,
    ) {
        if !matches!(&*self.backing, PayloadBacking::Writeback(_)) {
            return;
        }
        let end = offset + len;
        // register 失败的 child 不属于已接受设备范围，不能给 WB 发布设备错误。
        if self.accepted <= offset {
            return;
        }
        assert!(end <= self.accepted && completed <= len);
        if let Some(error) = error {
            let failure = offset + completed;
            if self.wb_error.is_none_or(|(first, _)| failure < first) {
                self.wb_error = Some((failure, error));
            }
        }
    }

    /// CPU-owned 独占来源可写初始化存储；共享 WB 来源始终拒绝内容切片。
    pub fn bytes_mut(&mut self) -> &mut [MaybeUninit<u8>] {
        assert!(!self.device_owned, "CPU access during DMA device ownership");
        let len = self.len();
        match &mut *self.backing {
            PayloadBacking::Pages(pages) => {
                // SAFETY: new 核对完整原块和 offset/len，当前 CPU-owned；
                // 可变借用排除并发交权，MaybeUninit 不要求字节已经初始化。
                unsafe { slice::from_raw_parts_mut(pages.as_mut_ptr().add(self.offset), len) }
            }
            PayloadBacking::Folio { read, .. } => {
                let start = self.offset - read.offset();
                &mut read.bytes_mut()[start..start + len]
            }
            PayloadBacking::Writeback(_) => panic!("mutable slice of a writeback folio"),
        }
    }

    /// 在 CPU 拥有期间读取已初始化的 payload，不根据完成状态猜测实际写入量。
    ///
    /// # Safety
    /// 调用者须证明整个 payload 已初始化；短读或失败完成不能证明未写尾部有效。
    /// 所有先前导出的裸别名必须遵守独占设备访问期，不能并发写此借用。
    /// 共享 WB 来源不得通过本入口取得切片，须沿 folio 原始字节访问。
    pub unsafe fn bytes(&self) -> &[u8] {
        assert!(!self.device_owned, "CPU access during DMA device ownership");
        assert!(
            !matches!(&*self.backing, PayloadBacking::Writeback(_)),
            "slice of a writeback folio"
        );
        let pointer = (arch::phys_to_virt(self.backing.phys_addr()) + self.offset) as *const u8;
        // SAFETY: new 已核对原块和 CPU 直映范围，本对象保活且处于 CPU 状态；
        // 调用者保证全部字节初始化及没有并发写者，借用不能超出本对象寿命。
        unsafe { slice::from_raw_parts(pointer, self.len()) }
    }

    /// 在 descriptor 发表前将整个 payload 交给唯一设备访问期。
    ///
    /// # Safety
    /// 独占页和读入者的全部 CPU 裸别名须先结束；ToDevice 字节必须已初始化。
    /// WB 凭证已完成对应映射撤权，后续文件原始写按同一 folio 记录新 Dirty；
    /// 不得借出 Rust 内容切片。子请求只引用本范围，来源保活到全部真实 used
    /// 或确认 reset 结束，不能独立收权、归还原页或重复交权。
    pub unsafe fn begin_device_access(&mut self) {
        assert!(!self.device_owned, "DMA payload is already device owned");
        assert!(
            !self.is_folio_complete(),
            "completed folio cannot enter DMA"
        );
        // 已收权的独占 Pages 可再次提交，每个设备访问期只保本轮父账。
        self.accepted = 0;
        self.wb_error = None;
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

    /// Bio 全尾收权后结束来源自己的初始化或 WB，保留每个原映射的接受范围。
    /// 完整读入已覆盖本页时，父后继范围的错误不撤销该页的成功前缀；分段读保原错误。
    ///
    /// # Safety
    /// 本映射全部设备访问已经结束；独占读入者的 CPU 可变别名也须结束。
    /// result 属于读入的整体连续前缀；WB 使用原父登记的真实接受与本映射错误。
    pub(crate) unsafe fn complete_folio(&mut self, result: IoResult) {
        assert!(!self.device_owned, "completing device-owned folio");
        match &mut *self.backing {
            PayloadBacking::Folio {
                read,
                publish_on_success,
            } => {
                if *publish_on_success || result.error.is_some() {
                    let result = if *publish_on_success && result.completed == read.len() {
                        IoResult {
                            completed: result.completed,
                            error: None,
                        }
                    } else {
                        result
                    };
                    // SAFETY: Bio 已真实收权，completed 是父连续成功前缀在本映射的交集；
                    // 完整覆盖才授予整页访问，分段或不完整结果仍按原错误结束读入。
                    unsafe { read.complete(result) };
                }
            }
            PayloadBacking::Writeback(writeback) => {
                let result = match self.wb_error {
                    Some((completed, error)) => IoResult {
                        completed,
                        error: Some(error),
                    },
                    None => IoResult {
                        completed: self.accepted,
                        error: None,
                    },
                };
                // SAFETY: Bio 已结束全部 child 与生成者且真实收权；accepted 只在
                // submit 成功后登记，错误只由本映射已接受的实际 child 完成记录。
                unsafe { writeback.complete(self.accepted, result) };
            }
            PayloadBacking::Pages(_) => {}
        }
    }
}

impl Drop for MappedPages {
    /// 只有结束设备访问期的 payload 才释放原来源；活动状态报告错误并保留整个拥有者。
    fn drop(&mut self) {
        assert!(!self.device_owned, "dropping device-owned DMA pages");
        // SAFETY: 原来源只由构造器移入且未取出；设备访问已停止，CPU 借用已结束。
        // 初始化与 WB 凭证各结束自己的责任；未提交 WB 恢复 Dirty，不转换原页归还权。
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
