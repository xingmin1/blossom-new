//! 用户 Sv39 根借用永久内核高半映射，低半页表由 MM 独占拥有。

use crate::mm::user_table::PagePermissions;
use core::{arch::asm, ptr};

pub const USER_ADDRESS_END: usize = 1 << 38;
pub const USER_STACK_TOP: usize = USER_ADDRESS_END - 4096;
pub const SIGNAL_RETURN_PAGE: usize = USER_ADDRESS_END - 4096;
pub const USER_LEVELS: usize = 3;
const PPN_MASK: usize = ((1usize << 44) - 1) << 10;

/// 未映射的 Sv39 项在所有层级均为零。
pub const fn user_empty_entry(_level: usize) -> usize {
    0
}

/// 编码仅含 V 位的非叶目录，物理地址合法性由调用者作往返检查。
pub const fn user_table_entry(physical: usize) -> usize {
    ((physical >> 2) & PPN_MASK) | 1
}

/// 仅在非叶层识别有效且没有 R/W/X 的目录项。
pub const fn user_entry_is_table(entry: usize, level: usize) -> bool {
    0 < level && entry & 0xf == 1
}

/// 从硬件 PPN 字段取得页对齐物理地址。
pub const fn user_entry_physical(entry: usize, _level: usize) -> usize {
    (entry & PPN_MASK) << 2
}

/// 编码用户叶项；无权限页保留 PPN 并关闭 V，Sv39 纯写权限无效。
pub const fn user_leaf_entry(physical: usize, permissions: PagePermissions) -> usize {
    if !permissions.read && permissions.write {
        return 0;
    }
    // V=0 时 U/A 由软件保留，用于区分无权限叶与没有 backing 的空槽。
    if !permissions.read && !permissions.execute {
        return ((physical >> 2) & PPN_MASK) | 0x50;
    }
    ((physical >> 2) & PPN_MASK)
        | 0x51
        | ((permissions.read as usize) << 1)
        | ((permissions.write as usize) << 2)
        | ((permissions.execute as usize) << 3)
        | ((permissions.write as usize) << 7)
}

/// 解码有效用户叶或保留 backing 的无权限叶，目录与非法 R/W 组合不属于用户叶。
pub const fn user_leaf_permissions(entry: usize) -> Option<PagePermissions> {
    if entry & 0x3ff == 0x50 {
        return Some(PagePermissions {
            read: false,
            write: false,
            execute: false,
        });
    }
    if entry & 0x11 != 0x11 || entry & 0xe == 0 || entry & 6 == 4 {
        return None;
    }
    Some(PagePermissions {
        read: entry & 2 != 0,
        write: entry & 4 != 0,
        execute: entry & 8 != 0,
    })
}

/// 清空用户低半，并借用已经发表且不再修改的最终内核根高半。
///
/// # Safety
/// root 必须指向独占、可写、4KiB 对齐的完整页；最终内核表已安装并永久存活，
/// 此根尚未发表给硬件。调用者不得取得共享高半目录的拥有权或修改其内容。
pub unsafe fn initialize_user_root(root: *mut usize) {
    // SAFETY: 调用者保证新根的独占与整页容量；FINAL_TABLES 在引导完成后只读，
    // 第零张是实际最终根，高半的子表由永久静态表池保活。
    unsafe {
        assert_ne!(ptr::read(ptr::addr_of!(super::RV_FINAL_SATP)), 0);
        let kernel = ptr::addr_of!(super::FINAL_TABLES).cast::<usize>();
        ptr::write_bytes(root, 0, 256);
        ptr::copy_nonoverlapping(kernel.add(256), root.add(256), 256);
    }
}

/// 在 IRQ 关闭的调度交接段安装 ASID0 用户根并清除本核旧翻译。
///
/// # Safety
/// root_phys 必须为已初始化、4KiB 对齐的 Sv39 根；完整表和所有映射的页仍被保活。
/// 内核高半必须包含当前代码、栈与 CpuLocal；本核 IRQ 已关闭，MM 活动归属由调用者管理。
pub unsafe fn activate_user_root(root_phys: usize) {
    assert_eq!(root_phys & 4095, 0);
    assert!(!super::irq_enabled());
    let satp = (8usize << 60) | (root_phys >> 12);
    // SAFETY: 调用者保证根及当前内核映射存活；ASID 固定零，完整 sfence 同时排序新 PTE。
    unsafe {
        asm!("fence rw, rw", "csrw satp, {satp}", "sfence.vma", satp = in(reg) satp, options(nostack));
    }
}

/// 在 IRQ 关闭时返回永久内核根，清除旧用户翻译后才允许释放活动 MM。
pub fn activate_kernel_root() {
    assert!(!super::irq_enabled());
    // SAFETY: 最终根在 AP 运行前已发表且永久存活；其代码、栈和直映覆盖当前执行资源。
    unsafe {
        let satp = ptr::read(ptr::addr_of!(super::RV_FINAL_SATP));
        assert_ne!(satp, 0);
        asm!("fence rw, rw", "csrw satp, {satp}", "sfence.vma", satp = in(reg) satp, options(nostack));
    }
}

/// 清除本核所有旧翻译并排序此前 PTE 修改，供同步 CSD 与切表使用。
pub fn flush_user_translations() {
    // SAFETY: 仅失效本核缓存且排序内存，不读取或改变映射页的拥有权。
    unsafe {
        asm!("sfence.vma", options(nostack));
    }
}

/// 同步本核指令缓存；远端执行者须通过既有同步 CSD 在各自核调用。
pub fn sync_user_icache() {
    // SAFETY: fence.i 仅同步本核取指与已完成的数据写，不访问未映射内存。
    unsafe {
        asm!("fence.i", options(nostack));
    }
}
