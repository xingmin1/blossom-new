mod bootmem;
mod buddy;
mod frame_allocator;
mod page;
mod pcp;
#[cfg(feature = "boot-tests")]
pub(crate) mod tests;

pub use frame_allocator::{alloc_pages, OwnedPages};
pub use pcp::drain_all_pages;

use crate::boot::BootInfo;
use crate::mm::bootmem::BootMemory;
use crate::mm::page::{PageDesc, PageMap};
use core::mem::size_of;
use spin::Once;

static MEMORY: Once<PageMap<'static>> = Once::new();

/// 借用永久发布的物理页描述符表；初始化前调用直接报告时序错误。
pub(crate) fn pages() -> &'static PageMap<'static> {
    MEMORY
        .get()
        .expect("physical memory has not been initialized")
}

/// 从实际启动 RAM 发布描述符，将自由范围交给 buddy，随后建立每 CPU 空页缓存。
///
/// # Safety
/// 只能由 BSP 在 IRQ 关闭、AP 未释放时调用一次。info 必须是已验证并包含完整
/// 内核、启动栈、页表及固件保留区的实际 BootInfo；全部实际 RAM 的直接映射必须
/// 永久可写且一一对应。调用期间不得有其他使用者访问或占用尚未保留的 RAM；
/// 描述符及索引 backing 发布后不得通过其他别名修改、释放或重新分配。
pub unsafe fn initialize(info: &BootInfo) {
    assert!(
        MEMORY.get().is_none(),
        "physical memory is already initialized"
    );
    #[cfg(feature = "boot-tests")]
    {
        assert!(
            bootmem::test_input_boundaries(),
            "memory boot range checks failed"
        );
        crate::println!("PASS memory-boot-ranges");
    }

    let mut memory = BootMemory::new(info)
        .unwrap_or_else(|error| panic!("memory boot range initialization failed: {}", error));
    // SAFETY: 调用者保证 BSP 独占实际空闲 RAM、IRQ 关闭且 AP 未释放；BootMemory
    // 来自完整实际 BootInfo，backing 使用永久可写且一一对应的 RAM 直接映射，
    // 发布后不再通过其他别名修改或回收；初始化失败立即 panic 终止启动。
    let map = unsafe { page::initialize(&mut memory) }
        .unwrap_or_else(|error| panic!("physical page map initialization failed: {}", error));
    MEMORY.call_once(|| map);
    let map = pages();
    let stats = map.stats();
    crate::println!(
        "MEMORY present={} reserved={} metadata={} free={} sections={} present_sections={} descriptor_size={}",
        stats.present_pages,
        stats.reserved_pages,
        stats.metadata_pages,
        stats.free_pages,
        map.section_count(),
        map.present_sections(),
        size_of::<PageDesc>()
    );

    #[cfg(feature = "boot-tests")]
    {
        assert!(map.test_layout(info), "memory page map checks failed");
        crate::println!("PASS memory-page-map");
    }

    // SAFETY: 调用者保证 BSP 唯一初始化、IRQ 关闭且 AP 未释放；map 及全部
    // 描述符已永久发布，最终 free 范围尚未交给其他 owner，也没有并发分配或归还。
    unsafe { buddy::initialize(map) };
    let free = buddy::free_pages();
    assert_eq!(
        free, stats.free_pages,
        "buddy initial free pages differ from boot memory"
    );
    crate::println!("BUDDY free={} max_order={}", free, buddy::MAX_ORDER);

    // SAFETY: buddy 已唯一接管全部自由页；调用者保证 BSP 独占初始化、IRQ 关闭
    // 且 AP 未释放，当前尚无页申请或 PCP 使用者。
    unsafe { pcp::initialize() };

    #[cfg(feature = "boot-tests")]
    {
        assert!(
            buddy::test_orders_and_fragmentation(),
            "memory buddy checks failed"
        );
        crate::println!("PASS memory-buddy");
        assert!(
            frame_allocator::test_owned_pages(),
            "memory owned pages checks failed"
        );
        crate::println!("PASS memory-owned-pages");
    }
}
