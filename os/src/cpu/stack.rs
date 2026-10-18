//! 每个辅核独占连续页栈；准备完成后才发布可供启动记录使用的栈顶。

use alloc::alloc::AllocError;
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::cpu::STACK_SIZE;
use crate::mm::{alloc_pages, OwnedPages, PAGE_SIZE};
use crate::sync::SpinLock;

/// 保存已发布栈顶和动态页的唯一归还权，静态主核栈不持有动态页。
pub(crate) struct StackSlot {
    top: AtomicUsize,
    pages: SpinLock<Option<OwnedPages>>,
}

impl StackSlot {
    /// 建立静态栈顶或等待动态准备的空槽；非零栈顶须由外部永久保活。
    pub(crate) const fn new(top: usize) -> Self {
        Self {
            top: AtomicUsize::new(top),
            pages: SpinLock::new(None),
        }
    }

    /// 读取准备成功后发布的栈顶，零表示尚无可交给辅核的栈。
    pub(crate) fn top(&self) -> usize {
        self.top.load(Ordering::Acquire)
    }

    /// 借真实页拥有取得直映半开范围的起点和字节数，不转移访问或归还权。
    #[cfg(feature = "ktest")]
    pub(crate) fn allocation(&self) -> Option<(usize, usize)> {
        self.pages
            .lock()
            .as_ref()
            .map(|pages| (crate::arch::phys_to_virt(pages.phys_addr()), pages.len()))
    }

    /// 撤下从未交给辅核的动态栈，短锁外归还整块页；空槽可重复释放。
    ///
    /// # Safety
    /// 调用者必须独占本槽的准备和释放操作；本槽不得表示静态主核栈。
    /// 页及其栈顶从未交给任何 CPU 或启动固件，全部地址访问已结束；
    /// 如启动记录已引用此栈，须先清空该记录，之后也不得再使用旧栈顶。
    pub(crate) unsafe fn release_unstarted(&self) {
        self.top.store(0, Ordering::Release);
        let pages = self.pages.lock().take();
        drop(pages);
    }
}

/// 为候选槽逐个取得页拥有，全部成功后才发布栈顶；分配失败归还全部候选页。
///
/// # Safety
/// MM 页分配器和永久可写直映已建立，当前上下文允许申请及归还页。
/// 迭代器及其克隆必须给出同一有限、无重复的槽集合；调用者独占集合内的
/// 准备和释放操作，全部槽初始为空且从未交给 CPU 或启动固件。
/// 成功后槽必须保活到使用栈的 CPU 永久结束全部访问，期间不得移动或销毁槽。
pub(crate) unsafe fn prepare_slots<'a>(
    slots: impl Iterator<Item = &'a StackSlot> + Clone,
) -> Result<(), AllocError> {
    for slot in slots.clone() {
        assert_eq!(slot.top(), 0, "CPU stack is already published");
        assert!(slot.pages.lock().is_none(), "CPU stack already owns pages");
    }

    let order = (STACK_SIZE / PAGE_SIZE).trailing_zeros() as usize;
    for slot in slots.clone() {
        let Some(pages) = alloc_pages(order, false) else {
            for candidate in slots.clone() {
                // SAFETY: 调用者独占尚未交给 CPU 的集合；全部栈顶仍为零，
                // 当前仅持有候选槽的页拥有，没有启动记录或裸地址访问。
                unsafe { candidate.release_unstarted() };
            }
            return Err(AllocError);
        };
        *slot.pages.lock() = Some(pages);
    }

    for slot in slots {
        let top = {
            let pages = slot.pages.lock();
            let pages = pages.as_ref().expect("prepared CPU stack lost its pages");
            crate::arch::phys_to_virt(pages.phys_addr()) + pages.len()
        };
        slot.top.store(top, Ordering::Release);
    }
    Ok(())
}
