use crate::arch;
use crate::mm::bootmem::PAGE_SIZE;
use crate::mm::buddy::{self, Block};
use core::mem::MaybeUninit;

/// 持有一个真实连续页块的唯一归还权；移动或复制原始地址不会复制拥有权。
pub struct OwnedPages {
    block: Block,
}

impl OwnedPages {
    /// 返回块头物理地址，不授予该整数地址的解引用或归还权限。
    pub fn phys_addr(&self) -> usize {
        self.block.head_pfn() * PAGE_SIZE
    }

    /// 返回整块字节长度；成功申请的块始终至少包含一页。
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> usize {
        PAGE_SIZE << self.order()
    }

    /// 返回唯一归还所需的原始阶数。
    pub fn order(&self) -> usize {
        self.block.order()
    }

    /// 返回整块直接映射的原始写指针；读取前须完成初始化，所有访问须在归还前结束。
    pub fn as_mut_ptr(&mut self) -> *mut MaybeUninit<u8> {
        arch::phys_to_virt(self.phys_addr()) as *mut MaybeUninit<u8>
    }
}

impl Drop for OwnedPages {
    /// 在调用者结束全部原始访问后，按原块头与阶数一次归还整个块。
    fn drop(&mut self) {
        // SAFETY: block 仅来自 buddy 的成功摘块且不能复制；本对象仍持有整块唯一
        // 归还权，safe 接口没有产生页内容引用，原始指针使用者须在析构前结束访问。
        unsafe { buddy::release(self.block.head_pfn(), self.block.order()) };
    }
}

/// 非阻塞地申请一个连续页块；不足或阶数超限返回 None，不睡眠或自动回收。
/// 清零在 buddy 已释放 zone 锁及其 IRQ guard 后执行，返回前写满全部请求字节。
pub fn alloc_pages(order: usize, zeroed: bool) -> Option<OwnedPages> {
    let block = buddy::allocate(order)?;
    let mut pages = OwnedPages { block };
    if zeroed {
        let len = pages.len();
        // SAFETY: buddy 返回已摘链的真实连续 RAM 块，当前拥有者独占全部 len 字节，
        // BSP 初始化已保证永久可写直映；此时 zone 锁和 IRQ guard 均已释放。
        unsafe { pages.as_mut_ptr().cast::<u8>().write_bytes(0, len) };
    }
    Some(pages)
}

/// 在 BSP 独占分配期间，用生产入口检查各阶拥有、真实分类、锁外清零与完整归还。
#[cfg(feature = "boot-tests")]
pub(crate) fn test_owned_pages() -> bool {
    use crate::mm::page::PageKind;
    use crate::mm::{alloc_pages, OwnedPages};

    let initial = buddy::free_pages();
    if alloc_pages(buddy::MAX_ORDER + 1, false).is_some()
        || alloc_pages(usize::MAX, true).is_some()
        || buddy::free_pages() != initial
    {
        return false;
    }
    let mut held: [Option<OwnedPages>; buddy::MAX_ORDER + 1] =
        [const { None }; buddy::MAX_ORDER + 1];
    let mut allocated = 0;
    for order in 0..=buddy::MAX_ORDER {
        let Some(mut pages) = alloc_pages(order, false) else {
            return false;
        };
        let len = PAGE_SIZE << order;
        let start = pages.phys_addr();
        if pages.order() != order || pages.len() != len || start % len != 0 {
            return false;
        }
        for pfn in start / PAGE_SIZE..start / PAGE_SIZE + (1 << order) {
            if crate::mm::pages().get(pfn).map(|page| page.kind()) != Some(PageKind::Available) {
                return false;
            }
        }
        for other in held.iter().flatten() {
            if start < other.phys_addr() + other.len() && other.phys_addr() < start + len {
                return false;
            }
        }
        // SAFETY: pages 独占真实连续块且 len 是完整长度；只写入原始字节，
        // 不从尚未初始化的页构造正常引用；写入结束后才允许归还。
        unsafe { pages.as_mut_ptr().cast::<u8>().write_bytes(0xa5, len) };
        allocated += 1 << order;
        if buddy::free_pages() != initial - allocated {
            return false;
        }
        held[order] = Some(pages);
    }
    for slot in held.iter_mut().rev() {
        drop(slot.take());
    }
    if buddy::free_pages() != initial {
        return false;
    }
    for order in 0..=buddy::MAX_ORDER {
        let Some(mut pages) = alloc_pages(order, true) else {
            return false;
        };
        let len = pages.len();
        // SAFETY: 此调用点明确请求 zeroed，生产入口已写满 len 字节；pages 在
        // 读取期间持有唯一拥有权且没有并发写者，切片借用在归还前结束。
        let bytes = unsafe { core::slice::from_raw_parts(pages.as_mut_ptr().cast::<u8>(), len) };
        if bytes.iter().any(|byte| *byte != 0) || buddy::free_pages() != initial - (1 << order) {
            return false;
        }
        drop(pages);
        if buddy::free_pages() != initial {
            return false;
        }
    }
    buddy::free_pages() == initial
}
