//! 匿名基本页的共享归还权；用户字节只经原始访问，不建立 Rust 内容引用。

use core::pin::Pin;

use crate::arch;
use crate::mm::page::PageDesc;
use crate::mm::{alloc_pages, pages, OwnedPages, PAGE_SIZE};

/// 用途锁下保存原 order0 拥有者和真实引用数，不含用户内容视图。
pub(crate) struct AnonState {
    pub(crate) backing: OwnedPages,
    pub(crate) references: usize,
}

/// 永久描述符上的一份匿名页引用；只保活原页，不授予内容独占或访问权限。
/// 映射持有者须在最后引用释放前撤离硬件访问并完成所需的 TLB 同步。
pub struct AnonPageRef {
    descriptor: Pin<&'static PageDesc>,
}

impl AnonPageRef {
    /// 非阻塞地申请并清零一张基本页，不为引用头额外分配对象。
    pub fn try_new() -> Option<Self> {
        let backing = alloc_pages(0, true)?;
        // SAFETY: 生产入口返回独占 order0 原页并清零完整页，清零访问已结束；
        // 尚无映射、设备或内容借用，原拥有者唯一持有归还权。
        Some(unsafe { Self::from_owned(backing) })
    }

    /// 将完整初始化的独占基本页交给描述符用途，发布首份共享引用。
    ///
    /// # Safety
    /// backing 必须是 order0 原拥有者，整页字节已初始化；所有排他引用、
    /// 裸指针访问和设备访问均已结束，尚无硬件映射或其他内容访问责任。
    /// 后续内容访问必须使用原始访问原语并由上层维护方向、范围和并发协议，
    /// 不得从共享用户字节构造普通 Rust 引用；归还前必须结束全部硬件访问。
    pub unsafe fn from_owned(backing: OwnedPages) -> Self {
        assert_eq!(backing.order(), 0, "anonymous backing is not order0");
        let descriptor = pages()
            .get(backing.phys_addr() / PAGE_SIZE)
            .expect("anonymous PFN has no descriptor");
        {
            let mut usage = descriptor.usage();
            usage.install_anon(AnonState {
                backing,
                references: 1,
            });
        }
        // SAFETY: 生产 PageMap 一次发布永久描述符，随后不移动；首引用发布前
        // 已安装唯一匿名用途，所有用途变化都由同一用途锁与真实计数串行。
        let descriptor = unsafe { Pin::new_unchecked(descriptor) };
        Self { descriptor }
    }

    /// 返回保活原页的物理身份；整数地址不授予内容访问或再次归还权限。
    pub fn phys_addr(&self) -> usize {
        self.descriptor.usage().anon().backing.phys_addr()
    }

    /// 返回原页永久直映首址；解引用须另行满足用户访问方向、范围和并发协议。
    pub fn direct_address(&self) -> usize {
        arch::phys_to_virt(self.phys_addr())
    }

    /// 比较是否保活同一真实原页，不推导写权限或 COW 独占性。
    pub fn ptr_eq(&self, other: &Self) -> bool {
        core::ptr::eq(self.descriptor.get_ref(), other.descriptor.get_ref())
    }
}

impl Clone for AnonPageRef {
    /// 在原引用保活期间增加用途锁下真实计数，不分配或复制用户字节。
    fn clone(&self) -> Self {
        let mut usage = self.descriptor.usage();
        let state = usage.anon_mut();
        state.references = state
            .references
            .checked_add(1)
            .expect("anonymous reference count overflow");
        Self {
            descriptor: self.descriptor,
        }
    }
}

impl Drop for AnonPageRef {
    /// 最后引用在用途锁内移出原拥有者，退出锁和 IRQ guard 后按原阶归页。
    fn drop(&mut self) {
        let discarded = {
            let mut usage = self.descriptor.usage();
            let state = usage.anon_mut();
            state.references = state
                .references
                .checked_sub(1)
                .expect("anonymous reference count underflow");
            if state.references == 0 {
                Some(usage.take_anon())
            } else {
                None
            }
        };
        drop(discarded);
    }
}
