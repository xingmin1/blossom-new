//! 用户页表与其数据页的拥有；活动根在切回内核映射前保持强引用。

use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::arch;
use crate::cpu;
use crate::mm::user_table::{PagePermissions, PageTableError, PreparedTables, UserPageTable};
use crate::mm::OwnedPages;
use crate::sync::{IrqSaveGuard, SpinLock};

pub struct Mm {
    table: SpinLock<UserPageTable>,
    pages: Vec<OwnedPages>,
}

impl Mm {
    /// 在普通任务上下文准备独立页表，尚未发表给执行者或硬件。
    pub fn try_new() -> Result<Self, PageTableError> {
        Ok(Self {
            table: SpinLock::new(UserPageTable::try_new()?),
            pages: Vec::new(),
        })
    }

    /// 在尚未共享的地址空间安装真实单页并接管其归还权，失败时原页正常归还。
    ///
    /// # Safety
    /// 调用者必须完整初始化 pages 的全部字节，结束全部排他 Rust 引用及未受控原始访问；
    /// 页面只能作为用户数据或代码，不能包含内核私有对象。成功后不得凭旧裸地址释放该页。
    pub unsafe fn map_owned(
        &mut self,
        address: usize,
        pages: OwnedPages,
        permissions: PagePermissions,
    ) -> Result<(), PageTableError> {
        if pages.order() != 0 {
            return Err(PageTableError::InvalidRange);
        }
        let needed = {
            let table = self.table.lock();
            if table.lookup(address).is_some() {
                return Err(PageTableError::Occupied);
            }
            table.needed_tables(address)
        };
        self.pages
            .try_reserve(1)
            .map_err(|_| PageTableError::NoMemory)?;
        let mut prepared = PreparedTables::try_new(needed)?;
        let previous = {
            let mut table = self.table.lock();
            // SAFETY: 调用者保证完整初始化且无排他存储引用，self 的唯一借用排除共享执行；
            // 原页成功后移入同一 Mm，全部活动根撤离后才归还，准备页已在短锁外初始化。
            unsafe { table.map(address, pages.phys_addr(), permissions, &mut prepared) }?
        };
        assert!(
            previous.is_none(),
            "new owned page replaced an existing mapping"
        );
        self.pages.push(pages);
        drop(prepared);
        arch::sync_user_icache();
        Ok(())
    }

    /// 返回由本对象保活的根物理地址，不移交页表或叶页的访问权。
    pub(crate) fn root_phys(&self) -> usize {
        self.table.lock().root_phys()
    }
}

/// 在所属 CPU 的 IRQ-off 调度交接中安装根，硬件使用期间由 CPU 保留一份拥有。
pub(crate) fn enter_current(mm: Option<Arc<Mm>>) {
    assert!(!arch::irq_enabled(), "MM switch requires disabled IRQ");
    let local = cpu::current();
    // SAFETY: 仅所属 CPU 在 IRQ-off 段访问活动槽；上次任务已切回内核根并移出拥有。
    let active = unsafe { &mut *local.active_mm.get() };
    assert!(active.is_none(), "previous MM is still active");
    if let Some(mm) = mm {
        // SAFETY: mm 拥有完整根和全部已安装页，内核静态映射由架构共享；
        // IRQ 关闭且当前调度栈有效，随后把强拥有放入本 CPU 的活动槽。
        unsafe { arch::activate_user_root(mm.root_phys()) };
        arch::sync_user_icache();
        *active = Some(mm);
    }
}

/// 先安装内核根并完成本地失效，再移出旧拥有供调度器锁外释放。
pub(crate) fn leave_current() -> Option<Arc<Mm>> {
    assert!(!arch::irq_enabled(), "MM detach requires disabled IRQ");
    let local = cpu::current();
    // SAFETY: 只有所属 CPU 的 IRQ-off 调度交接访问活动槽，旧 Task 已保存并离栈。
    let active = unsafe { &mut *local.active_mm.get() };
    if active.is_some() {
        arch::activate_kernel_root();
    }
    active.take()
}

/// 在本 CPU 不可迁移的短段取得活动 Mm 拥有，不借用会在交接中变化的槽。
pub(crate) fn current_mm() -> Option<Arc<Mm>> {
    let _irq = IrqSaveGuard::new();
    let local = cpu::current();
    // SAFETY: IRQ guard 固定本 CPU，只有本 CPU 修改槽；克隆在保护段内完成。
    unsafe { (*local.active_mm.get()).clone() }
}
