//! 从实际 RAM 直映建立永久页描述符；空洞仅占 section 索引。

use core::cell::UnsafeCell;
use core::mem::size_of;
#[cfg(feature = "boot-tests")]
use core::mem::size_of_val;
use core::slice;

#[cfg(feature = "boot-tests")]
use crate::boot::BootInfo;
use crate::boot::MAX_MEMORY_REGIONS;
use crate::mm::bootmem::{BootMemory, BootStats, PageRange, PAGE_SIZE, RANGE_CAPACITY};

const PAGES_PER_SECTION: usize = (128 * 1024 * 1024) / PAGE_SIZE;
type Section<'a> = Option<&'a [PageDesc]>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum PageKind {
    Absent,
    Reserved,
    Metadata,
    Available,
}

pub(crate) struct PageDesc {
    kind: PageKind,
    /// 仅由唯一所属 zone 的锁域访问，不以启动分类代替自由链成员身份。
    pub(crate) buddy: UnsafeCell<BuddyLink>,
}

#[derive(Clone, Copy)]
pub(crate) struct BuddyLink {
    pub(crate) order: usize,
    pub(crate) previous: usize,
    pub(crate) next: usize,
}

// SAFETY: kind 发布后不可变，buddy 的全部读写由唯一所属 zone 锁互斥。
unsafe impl Sync for PageDesc {}

impl PageDesc {
    /// 写入不可变启动类别和未加入自由链的初始状态。
    pub(crate) const fn new(kind: PageKind) -> Self {
        Self {
            kind,
            buddy: UnsafeCell::new(BuddyLink {
                order: usize::MAX,
                previous: usize::MAX,
                next: usize::MAX,
            }),
        }
    }

    /// 取得页的不可变启动来源类别；Available 不表示当前在自由链中。
    pub(crate) fn kind(&self) -> PageKind {
        self.kind
    }
}

pub(crate) struct PageMap<'a> {
    sections: &'a [Section<'a>],
    free: &'a [PageRange],
    stats: BootStats,
    present_sections: usize,
    #[cfg(feature = "boot-tests")]
    metadata: &'a [PageRange],
}

#[derive(Clone, Copy)]
struct SectionGroup {
    start: usize,
    end: usize,
    backing: PageRange,
}

impl<'a> PageMap<'a> {
    /// 按 section 和槽内偏移定位 PFN，不读取启动区间表。
    pub(crate) fn get(&self, pfn: usize) -> Option<&PageDesc> {
        self.sections
            .get(pfn / PAGES_PER_SECTION)?
            .as_ref()?
            .get(pfn % PAGES_PER_SECTION)
    }

    /// 返回元数据全部扣除后的 RAM 分类计数。
    pub(crate) fn stats(&self) -> BootStats {
        self.stats
    }

    /// 返回按实际 RAM 最大端点建立的根索引长度。
    pub(crate) fn section_count(&self) -> usize {
        self.sections.len()
    }

    /// 返回实际拥有描述符 backing 的 section 数量。
    pub(crate) fn present_sections(&self) -> usize {
        self.present_sections
    }

    /// 返回下一次页分配器交接所需的最终自由范围，不授予释放权限。
    pub(crate) fn free_ranges(&self) -> &[PageRange] {
        self.free
    }

    /// 借用独立测试描述符与范围；不发布，也不授予物理页访问权限。
    #[cfg(feature = "boot-tests")]
    pub(crate) fn test_map(sections: &'a [Section<'a>], free: &'a [PageRange]) -> Self {
        Self {
            sections,
            free,
            stats: BootStats {
                present_pages: 0,
                reserved_pages: 0,
                metadata_pages: 0,
                free_pages: free.iter().map(|range| range.end - range.start).sum(),
            },
            present_sections: sections.iter().filter(|section| section.is_some()).count(),
            metadata: &[],
        }
    }

    /// 用实际启动资源逐 PFN 核对分类、空洞、保留资源和计数。
    #[cfg(feature = "boot-tests")]
    pub(crate) fn test_layout(&self, info: &BootInfo) -> bool {
        let mut present = 0;
        let mut reserved = 0;
        let mut metadata = 0;
        let mut free = 0;
        let mut present_sections = 0;
        for (section_index, section) in self.sections.iter().enumerate() {
            let first = section_index * PAGES_PER_SECTION;
            let mut section_has_ram = false;
            for offset in 0..PAGES_PER_SECTION {
                let pfn = first + offset;
                let ram = page_is_ram(info, pfn);
                section_has_ram |= ram;
                let is_reserved = ram
                    && info.reserved[..info.reserved_count].iter().any(|region| {
                        region.start / PAGE_SIZE <= pfn && pfn < region.end.div_ceil(PAGE_SIZE)
                    });
                let is_metadata = contains(self.metadata, pfn);
                let is_free = contains(self.free, pfn);
                if ((is_reserved || !ram) && (is_metadata || is_free)) || (is_metadata && is_free) {
                    return false;
                }
                let expected = if !ram {
                    PageKind::Absent
                } else if is_reserved {
                    reserved += 1;
                    PageKind::Reserved
                } else if is_metadata {
                    metadata += 1;
                    PageKind::Metadata
                } else {
                    if !is_free {
                        return false;
                    }
                    free += 1;
                    PageKind::Available
                };
                if ram {
                    present += 1;
                }
                match self.get(pfn) {
                    Some(desc) if desc.kind() == expected => {}
                    None if !ram && section.is_none() => {}
                    _ => return false,
                }
            }
            if section_has_ram != section.is_some() {
                return false;
            }
            if let Some(descriptors) = section {
                present_sections += 1;
                if descriptors.len() != PAGES_PER_SECTION
                    || !self
                        .metadata_covers(descriptors.as_ptr() as usize, size_of_val(*descriptors))
                {
                    return false;
                }
            }
        }
        let mut previous_end = 0;
        let mut listed_free = 0;
        for range in self.free {
            if range.end <= range.start || range.start < previous_end {
                return false;
            }
            previous_end = range.end;
            listed_free += range.end - range.start;
        }
        let maximum = info.memory[..info.memory_count]
            .iter()
            .filter_map(|region| {
                let end = region.end / PAGE_SIZE;
                end.checked_sub(1)
                    .filter(|&pfn| page_is_ram(info, pfn))
                    .map(|_| end)
            })
            .max()
            .unwrap_or(0);
        self.sections.len() == maximum.div_ceil(PAGES_PER_SECTION)
            && self.get(self.sections.len() * PAGES_PER_SECTION).is_none()
            && self.get(usize::MAX).is_none()
            && present_sections == self.present_sections
            && present == self.stats.present_pages
            && reserved == self.stats.reserved_pages
            && metadata == self.stats.metadata_pages
            && free == self.stats.free_pages
            && listed_free == free
            && present == reserved + metadata + free
            && self.resource_is_reserved(crate::boot::kernel_region())
            && self.resource_is_reserved(info.dtb)
            && self.metadata_covers(self.sections.as_ptr() as usize, size_of_val(self.sections))
            && self.metadata_covers(self.free.as_ptr() as usize, size_of_val(self.free))
            && self.metadata_covers(self.metadata.as_ptr() as usize, size_of_val(self.metadata))
    }

    /// 核对静态内核资源与 DTB 的 RAM 交集不出现在自由范围中。
    #[cfg(feature = "boot-tests")]
    fn resource_is_reserved(&self, region: crate::boot::Region) -> bool {
        if region.start == region.end {
            return true;
        }
        (region.start / PAGE_SIZE..region.end.div_ceil(PAGE_SIZE)).all(|pfn| {
            !contains(self.free, pfn)
                && self
                    .get(pfn)
                    .is_none_or(|desc| matches!(desc.kind(), PageKind::Absent | PageKind::Reserved))
        })
    }

    /// 核对永久控制对象的每个 backing 页均由元数据分类覆盖。
    #[cfg(feature = "boot-tests")]
    fn metadata_covers(&self, address: usize, bytes: usize) -> bool {
        if bytes == 0 {
            return true;
        }
        let physical = crate::arch::virt_to_phys(address);
        (physical / PAGE_SIZE..(physical + bytes).div_ceil(PAGE_SIZE)).all(|pfn| {
            self.get(pfn)
                .is_some_and(|desc| desc.kind() == PageKind::Metadata)
                && contains(self.metadata, pfn)
                && !contains(self.free, pfn)
        })
    }
}

/// 将字节数向上转换成 backing 页数，拒绝指针跨度不可表达的布局。
fn backing_pages(bytes: usize) -> Result<usize, &'static str> {
    if bytes == 0 || (isize::MAX as usize) < bytes {
        return Err("page metadata layout is invalid");
    }
    Ok(bytes.div_ceil(PAGE_SIZE))
}

/// 顺序推进规范化区间游标；初始化按递增 PFN 访问每个列表一次。
fn cursor_contains(ranges: &[PageRange], cursor: &mut usize, pfn: usize) -> bool {
    while *cursor < ranges.len() && ranges[*cursor].end <= pfn {
        *cursor += 1;
    }
    *cursor < ranges.len() && ranges[*cursor].start <= pfn
}

/// 检查测试快照中是否存在包含该 PFN 的范围。
#[cfg(feature = "boot-tests")]
fn contains(ranges: &[PageRange], pfn: usize) -> bool {
    ranges
        .iter()
        .any(|range| range.start <= pfn && pfn < range.end)
}

/// 从原始字节资源的覆盖关系判断整页 RAM，允许相邻未对齐资源共同覆盖一页。
#[cfg(feature = "boot-tests")]
fn page_is_ram(info: &BootInfo, pfn: usize) -> bool {
    let mut covered = pfn * PAGE_SIZE;
    let end = covered + PAGE_SIZE;
    while covered < end {
        let next = info.memory[..info.memory_count]
            .iter()
            .filter(|region| region.start <= covered && covered < region.end)
            .map(|region| region.end)
            .max()
            .unwrap_or(covered);
        if next == covered {
            return false;
        }
        covered = next;
    }
    true
}

/// 在独占 backing 中逐项写入范围后，创建永久只读快照。
///
/// # Safety
/// address 必须非空且按 PageRange 对齐，即使 ranges 为空也必须如此；目标必须
/// 位于单个分配内，拥有至少 ranges.len() 个槽的独占、永久可写映射 RAM，
/// 字节跨度不超过 isize::MAX，地址加跨度不得回绕。目标不得与 ranges 或任何
/// 已建立引用的对象重叠；写入后不得复用或修改这些槽。
unsafe fn write_ranges(address: usize, ranges: &[PageRange]) -> &'static [PageRange] {
    let destination = address as *mut PageRange;
    for (index, range) in ranges.iter().enumerate() {
        // SAFETY: 调用者提供独占且足够大的对齐 backing；index 位于其槽范围内。
        unsafe { destination.add(index).write(*range) };
    }
    // SAFETY: 所有目标槽已写入有效 PageRange；调用者保证映射永久且不再修改。
    unsafe { slice::from_raw_parts(destination, ranges.len()) }
}

/// 先扣除全部元数据页，再按递增 PFN 填写 present section 的描述符和根索引。
///
/// # Safety
/// 只能由 BSP 在 IRQ 关闭、AP 尚未释放时调用；memory 必须来自当前机器真实
/// BootInfo，包含内核、栈、页表、DTB 和固件保留资源。其可用 RAM 的直映必须
/// 永久可写且一一对应；每次分配的连续页必须在直映中构成单个连续分配，
/// 起址非空且按 PAGE_SIZE 对齐，整个分配的虚拟地址范围不得回绕。
/// 本函数取得的页必须独占，不得与 memory 自身或其他存活引用重叠，之后
/// backing 不得释放或重新分配，kind/索引/区间快照不得修改；buddy 字段仅允许
/// 由唯一所属 zone 锁域改变。失败时已扣除的 backing 仍保留，调用者必须终止启动。
pub(crate) unsafe fn initialize(memory: &mut BootMemory) -> Result<PageMap<'static>, &'static str> {
    let mut groups = [SectionGroup {
        start: 0,
        end: 0,
        backing: PageRange { start: 0, end: 0 },
    }; MAX_MEMORY_REGIONS];
    let mut group_count = 0;
    for range in memory.ram() {
        let start = range.start / PAGES_PER_SECTION;
        let end = range.end.div_ceil(PAGES_PER_SECTION);
        if group_count != 0 && start <= groups[group_count - 1].end {
            groups[group_count - 1].end = groups[group_count - 1].end.max(end);
        } else {
            groups[group_count].start = start;
            groups[group_count].end = end;
            group_count += 1;
        }
    }
    if group_count == 0 {
        return Err("page map has no RAM sections");
    }
    let section_count = groups[group_count - 1].end;
    let section_bytes = section_count
        .checked_mul(size_of::<Section<'static>>())
        .ok_or("page root size overflows")?;
    // Section 与 PageRange 均按 usize 对齐，索引后可直接放范围快照。
    let range_bytes = RANGE_CAPACITY
        .checked_mul(size_of::<PageRange>())
        .ok_or("page range storage overflows")?;
    let root_bytes = section_bytes
        .checked_add(range_bytes)
        .ok_or("page root size overflows")?;
    #[cfg(feature = "boot-tests")]
    let root_bytes = root_bytes
        .checked_add(range_bytes)
        .ok_or("page root size overflows")?;
    let root = memory.allocate(backing_pages(root_bytes)?, 1)?;
    let section_descriptor_bytes = PAGES_PER_SECTION
        .checked_mul(size_of::<PageDesc>())
        .ok_or("section descriptor size overflows")?;
    let mut present_sections = 0;
    for group in &mut groups[..group_count] {
        let count = group.end - group.start;
        let bytes = count
            .checked_mul(section_descriptor_bytes)
            .ok_or("page descriptor size overflows")?;
        group.backing = memory.allocate(backing_pages(bytes)?, 1)?;
        present_sections += count;
    }

    let root_address = crate::arch::phys_to_virt(root.start * PAGE_SIZE);
    let root_pointer = root_address as *mut Section<'static>;
    let mut group_index = 0;
    let mut ram_cursor = 0;
    let mut reserved_cursor = 0;
    let mut metadata_cursor = 0;
    for section_index in 0..section_count {
        while group_index < group_count && groups[group_index].end <= section_index {
            group_index += 1;
        }
        let descriptors = if group_index < group_count && groups[group_index].start <= section_index
        {
            let group = groups[group_index];
            let offset = (section_index - group.start) * section_descriptor_bytes;
            let address = crate::arch::phys_to_virt(group.backing.start * PAGE_SIZE) + offset;
            let destination = address as *mut PageDesc;
            for index in 0..PAGES_PER_SECTION {
                let pfn = section_index * PAGES_PER_SECTION + index;
                let ram = cursor_contains(memory.ram(), &mut ram_cursor, pfn);
                let reserved = cursor_contains(memory.reserved(), &mut reserved_cursor, pfn);
                let metadata = cursor_contains(memory.metadata(), &mut metadata_cursor, pfn);
                let kind = if !ram {
                    PageKind::Absent
                } else if reserved {
                    PageKind::Reserved
                } else if metadata {
                    PageKind::Metadata
                } else {
                    PageKind::Available
                };
                // SAFETY: group backing 独占且永久，offset 将本 section 定位到
                // 不重叠的 section_descriptor_bytes 子范围，PageDesc 对齐由页
                // 对齐起址与类型大小保证；add 仅在此 section 内，每槽首次写入。
                unsafe { destination.add(index).write(PageDesc::new(kind)) };
            }
            // SAFETY: 本 section 全部槽已写入有效值，跨度由 backing_pages 限制；
            // 该子范围属于一个永久分配，后续 section 写入不与本 slice 重叠。
            Some(unsafe { slice::from_raw_parts(destination, PAGES_PER_SECTION) })
        } else {
            None
        };
        // SAFETY: root 已独占预留且按 Section 对齐；每个索引槽仅写入一次合法值。
        unsafe { root_pointer.add(section_index).write(descriptors) };
    }
    // SAFETY: 根数组全部槽已初始化，跨度不超过 root_bytes，永久且不再修改；
    // 后续快照仅写 root 的未被 sections 引用的尾部。
    let sections = unsafe { slice::from_raw_parts(root_pointer, section_count) };
    // SAFETY: root 后部留有 RANGE_CAPACITY 个对齐槽，不与 sections 重叠；
    // BootMemory 的范围容量不超过该上界，写完后仅发布只读引用。
    let free = unsafe { write_ranges(root_address + section_bytes, memory.free()) };
    #[cfg(feature = "boot-tests")]
    // SAFETY: 测试布局额外预留另一组 RANGE_CAPACITY 槽，与根和 free 快照不重叠。
    let metadata = unsafe {
        write_ranges(
            root_address + section_bytes + range_bytes,
            memory.metadata(),
        )
    };
    Ok(PageMap {
        sections,
        free,
        stats: memory.stats(),
        present_sections,
        #[cfg(feature = "boot-tests")]
        metadata,
    })
}
