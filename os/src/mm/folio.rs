//! 原 order0 页共享引用与一次读入责任；内容锁和等待源随活 folio 固定在永久描述符内。

use core::cell::UnsafeCell;
use core::marker::PhantomPinned;
use core::mem::MaybeUninit;
use core::pin::{pin, Pin};
use core::slice;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::arch;
use crate::block::{BlockError, IoResult};
use crate::mm::page::{PageDesc, PageUseGuard};
use crate::mm::{alloc_pages, pages, OwnedPages, PAGE_SIZE};
use crate::sched;
use crate::wait::{WaitNode, WaitQueue, Waiter};

#[derive(Clone, Copy)]
struct InitializedRange {
    offset: usize,
    len: usize,
}

/// 共享头只由所属 PageUse 锁读写，UnsafeCell 允许固定用途的共享 Pin 保持有效。
struct FolioShared {
    references: usize,
    initialized: Option<Result<InitializedRange, BlockError>>,
}

/// 页用途内保存原拥有者、独立内容锁和等待源；活引用期间禁止移动整个用途。
pub(crate) struct FolioState {
    backing: Option<OwnedPages>,
    shared: UnsafeCell<FolioShared>,
    locked: AtomicBool,
    waiters: WaitQueue,
    _pinned: PhantomPinned,
}

// SAFETY: backing 在活引用期间不可变；shared 仅通过所属 PageUse guard 访问，
// 内容锁用原子协议转交，等待源内部短锁保护节点。活用途只做内部可变访问，
// refs 归零且读入者、等待者和内容借用均结束后才取走原页，固定状态原地析构。
unsafe impl Sync for FolioState {}

/// 永久描述符上的一份原页引用；克隆不分配，也不复制页内容。
pub struct FolioRef {
    descriptor: Pin<&'static PageDesc>,
}

/// 唯一内容锁责任及一份保页引用；未完成时析构以 QueueStopped 结束填充。
/// DMA 层交设备后必须保护整个拥有者，直到实际收权才能完成或析构。
pub struct FolioRead {
    folio: FolioRef,
    offset: usize,
    len: usize,
    completed: bool,
}

impl FolioState {
    /// 借用所属用途锁保护的共享头，借用不能超出 guard 的存活期。
    fn shared<'a>(usage: &'a PageUseGuard<'_>) -> &'a FolioShared {
        let state = usage.folio();
        // SAFETY: usage 是本状态唯一所属用途锁的活 guard；所有 shared 写者
        // 都持同锁，返回借用绑定 guard，期间没有可变 shared 借用。
        unsafe { &*state.shared.get() }
    }

    /// 仅修改固定用途内部的共享头，不创建会排斥其他 Pin 借用的 &mut FolioState。
    fn shared_mut<'a>(usage: &'a mut PageUseGuard<'_>) -> &'a mut FolioShared {
        let state = usage.folio();
        // SAFETY: 可变借用 guard 串行本状态全部 shared 访问；只从 UnsafeCell
        // 创建内部可变借用，不改变固定状态、原拥有者、内容锁或等待源的地址。
        unsafe { &mut *state.shared.get() }
    }

    /// 最后引用结束后取走原页归还权，固定等待源仍由用途槽原地析构。
    pub(crate) fn take_backing(&mut self) -> OwnedPages {
        assert_eq!(
            self.shared.get_mut().references,
            0,
            "removed folio retains references"
        );
        assert!(
            !self.locked.load(Ordering::Acquire),
            "removed folio retains a read owner"
        );
        self.backing.take().expect("folio backing already removed")
    }
}

impl FolioRef {
    /// 消耗真实 order0 拥有者；高阶完整返回原块，不拆分或改变其阶数。
    pub fn from_pages(backing: OwnedPages) -> Result<Self, OwnedPages> {
        if backing.order() != 0 {
            return Err(backing);
        }
        let descriptor = pages()
            .get(backing.phys_addr() / PAGE_SIZE)
            .expect("folio PFN has no descriptor");
        {
            let mut usage = descriptor.usage();
            usage.install_folio(FolioState {
                backing: Some(backing),
                shared: UnsafeCell::new(FolioShared {
                    references: 1,
                    initialized: None,
                }),
                locked: AtomicBool::new(false),
                waiters: WaitQueue::new(),
                _pinned: PhantomPinned,
            });
        }
        // SAFETY: 生产 PageMap 在独占元数据 RAM 中一次发布，描述符永久有效
        // 且不再移动；此 Pin 只借用描述符，活 folio 用途的固定由私有投影协议保证。
        let descriptor = unsafe { Pin::new_unchecked(descriptor) };
        Ok(Self { descriptor })
    }

    /// 从唯一生产页入口取得未清零 order0 页，不申请额外控制对象。
    pub fn allocate() -> Option<Self> {
        let backing = alloc_pages(0, false)?;
        Some(
            Self::from_pages(backing)
                .unwrap_or_else(|_| unreachable!("order0 allocation changed order")),
        )
    }

    /// 返回同一原页地址，仅用于身份与 DMA 映射，不授予内容访问权。
    pub fn phys_addr(&self) -> usize {
        self.state()
            .backing
            .as_ref()
            .expect("live folio lost backing")
            .phys_addr()
    }

    /// 读取用途锁内的真实引用数；并发快照不授予独占访问权。
    pub fn ref_count(&self) -> usize {
        FolioState::shared(&self.descriptor.usage()).references
    }

    /// 完成本次填充并解除内容锁后为真，失败结果也算已完成。
    pub fn is_ready(&self) -> bool {
        let usage = self.descriptor.usage();
        FolioState::shared(&usage).initialized.is_some()
            && !usage.folio().locked.load(Ordering::Acquire)
    }

    /// 验证非空页内范围并取得唯一内容锁责任；已完成或已在途页返回 Busy。
    /// 在候选发表前调用，不持用途锁运行 IO，也不把任务局部睡眠锁转交给完成上下文。
    pub fn begin_read(&self, offset: usize, len: usize) -> Result<FolioRead, BlockError> {
        validate_range(offset, len)?;
        let state = self.state();
        {
            let mut usage = self.descriptor.usage();
            let shared = FolioState::shared_mut(&mut usage);
            if shared.initialized.is_some() || state.locked.load(Ordering::Relaxed) {
                return Err(BlockError::Busy);
            }
            shared.references = shared
                .references
                .checked_add(1)
                .expect("folio reference count overflow");
            state.locked.store(true, Ordering::Release);
        }
        Ok(FolioRead {
            folio: Self {
                descriptor: self.descriptor,
            },
            offset,
            len,
            completed: false,
        })
    }

    /// 普通可切换 Task 沿永久源登记、复查再 park，不持页用途锁等待。
    /// 尚未取得读入责任的页返回 NotReady；失败页返回实际首次错误。
    pub fn wait_ready(&self) -> Result<(), BlockError> {
        sched::assert_switchable();
        let state = self.state();
        loop {
            if let Some(result) = self.read_result() {
                return result.map(|_| ());
            }
            let waiter = Waiter::new();
            let mut node = pin!(WaitNode::new(waiter.waker()));
            // SAFETY: self 保有原页引用，state 的私有 Pin 投影固定用途内等待源；
            // pin! 固定当前栈节点，registration 不被遗忘且所有退出均先撤链，
            // 随后才析构节点和 waiter；state 借用结束前不能释放最后引用或移动用途。
            let _registration = unsafe { state.waiters.register(node.as_mut()) };
            if let Some(result) = self.read_result() {
                return result.map(|_| ());
            }
            waiter.park();
        }
    }

    /// 只借用本次成功初始化范围，消费者在全部 MM 短锁外运行，返回值不得借用该切片。
    /// 成功后页为只读且禁止再次填充；本引用保活整个短访问，不会与设备或写者别名。
    pub fn with_bytes<T>(
        &self,
        offset: usize,
        len: usize,
        consume: impl FnOnce(&[u8]) -> T,
    ) -> Result<T, BlockError> {
        validate_range(offset, len)?;
        let initialized = self.read_result().ok_or(BlockError::NotReady)??;
        if offset < initialized.offset || initialized.offset + initialized.len < offset + len {
            return Err(BlockError::InvalidRange);
        }
        let address = arch::phys_to_virt(self.phys_addr()) + offset;
        // SAFETY: 唯一完成者已保证完整成功范围初始化且结束设备和可变访问；
        // 完成后的内容不可再填充，self 保页，范围验证排除越界，切片仅供本次闭包借用。
        let bytes = unsafe { slice::from_raw_parts(address as *const u8, len) };
        Ok(consume(bytes))
    }

    /// 仅供私有索引唯一引用语境判断 clean 回收；不修改用途、IO 或引用计数。
    pub fn can_prune(&self) -> bool {
        let usage = self.descriptor.usage();
        FolioState::shared(&usage).references == 1 && !usage.folio().locked.load(Ordering::Acquire)
    }

    /// 在短用途锁内读取一次结果；None 仅表示内容锁仍由读入者持有。
    fn read_result(&self) -> Option<Result<InitializedRange, BlockError>> {
        let usage = self.descriptor.usage();
        if usage.folio().locked.load(Ordering::Acquire) {
            return None;
        }
        Some(
            FolioState::shared(&usage)
                .initialized
                .unwrap_or(Err(BlockError::NotReady)),
        )
    }

    /// 在用途短锁内定位状态，再投影绑定本活引用的共享 Pin，不将锁带入 IO 或等待。
    fn state(&self) -> Pin<&FolioState> {
        let pointer = {
            let usage = self.descriptor.usage();
            usage.folio() as *const FolioState
        };
        // SAFETY: from_pages 先完成用途写入再发表首引用；此后活引用只通过
        // UnsafeCell 修改共享头，从不创建 &mut FolioState 或替换用途。
        // self 保证 references 至少为一，本借用终止前无法执行最后引用移出；
        // 永久 descriptor 固定该用途槽地址，内容锁、等待源和 backing 均不移动。
        unsafe { Pin::new_unchecked(&*pointer) }
    }
}

impl Clone for FolioRef {
    /// 在原引用保活期间增加真实头计数，不分配对象或改变 PFN。
    fn clone(&self) -> Self {
        let mut usage = self.descriptor.usage();
        let shared = FolioState::shared_mut(&mut usage);
        shared.references = shared
            .references
            .checked_add(1)
            .expect("folio reference count overflow");
        Self {
            descriptor: self.descriptor,
        }
    }
}

impl Drop for FolioRef {
    /// 最后引用仅在用途锁内移出原拥有者，退出锁和 IRQ guard 后才按原阶归页。
    fn drop(&mut self) {
        let discarded = {
            let mut usage = self.descriptor.usage();
            let shared = FolioState::shared_mut(&mut usage);
            shared.references = shared
                .references
                .checked_sub(1)
                .expect("folio reference count underflow");
            if shared.references == 0 {
                assert!(
                    !usage.folio().locked.load(Ordering::Acquire),
                    "last folio reference retains a read owner"
                );
                // SAFETY: 引用数已归零，内容锁无读入者；等待登记与内容访问均借用
                // 活 FolioRef，不能跨越其析构，锁内 shared 和状态借用也已结束。
                // 本析构不保留固定投影，原地析构固定状态后退出用途锁才归还原页。
                Some(unsafe { usage.take_folio() })
            } else {
                None
            }
        };
        // 等待者借用一份活引用，读入者另有保页引用；最后引用时二者均已结束。
        drop(discarded);
    }
}

impl FolioRead {
    /// 返回仍由本读入者保活的原页地址，DMA 层须维护其自身交权与收权协议。
    pub fn phys_addr(&self) -> usize {
        self.folio.phys_addr()
    }

    /// 返回本次页内读入起点。
    pub fn offset(&self) -> usize {
        self.offset
    }

    /// 返回本次非空读入长度。
    #[allow(clippy::len_without_is_empty)]
    pub fn len(&self) -> usize {
        self.len
    }

    /// DMA 层拒绝将已经发布只读内容的拥有者再次交给设备。
    pub(crate) fn is_completed(&self) -> bool {
        self.completed
    }

    /// 完成前在 CPU 独占阶段借用未初始化内容；交设备后 DMA 层不得再调用此入口。
    pub fn bytes_mut(&mut self) -> &mut [MaybeUninit<u8>] {
        assert!(
            !self.completed,
            "completed folio read cannot write contents"
        );
        let address = arch::phys_to_virt(self.phys_addr()) + self.offset;
        // SAFETY: 本对象独占内容锁责任，尚未完成，没有只读内容借用；
        // CPU 阶段范围已验证，MaybeUninit 允许未初始化字节，借用不超过 &mut self。
        // 真实 DMA 层须将本对象整体保护到设备收权后，交权期间不得访问 CPU 内容。
        unsafe { slice::from_raw_parts_mut(address as *mut MaybeUninit<u8>, self.len) }
    }

    /// 在真实 DMA 全尾后发表唯一结果、解锁并锁外唤醒；重复完成不改变首次结果。
    ///
    /// # Safety
    /// 所有设备访问与可变访问必须已经结束；完整成功必须保证本次全部范围已初始化。
    /// 部分成功与失败不授予内容访问权，完成后不得再通过原始地址写页。
    pub unsafe fn complete(&mut self, result: IoResult) {
        if self.completed {
            return;
        }
        let initialized = match result.error {
            Some(error) => Err(error),
            None if result.completed == self.len => Ok(InitializedRange {
                offset: self.offset,
                len: self.len,
            }),
            None => Err(BlockError::ShortIo),
        };
        self.publish(initialized);
    }

    /// 持短用途锁发表终态并结束独立内容锁责任，释放锁后通知实际等待者。
    fn publish(&mut self, initialized: Result<InitializedRange, BlockError>) {
        let state = self.folio.state();
        {
            let mut usage = self.folio.descriptor.usage();
            let shared = FolioState::shared_mut(&mut usage);
            assert!(
                shared.initialized.is_none(),
                "folio result already published"
            );
            assert!(
                state.locked.load(Ordering::Relaxed),
                "folio read lost its content lock"
            );
            shared.initialized = Some(initialized);
            self.completed = true;
            state.locked.store(false, Ordering::Release);
        }
        state.waiters.wake_all();
    }
}

impl Drop for FolioRead {
    /// 未提交读入者结束自身填充责任并唤醒，已完成者只释放所保原页引用。
    fn drop(&mut self) {
        if !self.completed {
            self.publish(Err(BlockError::QueueStopped));
        }
    }
}

/// 拒绝空范围、溢出和跨页访问，所有内容入口共用同一几何边界。
fn validate_range(offset: usize, len: usize) -> Result<(), BlockError> {
    if len == 0 || offset.checked_add(len).is_none_or(|end| PAGE_SIZE < end) {
        return Err(BlockError::InvalidRange);
    }
    Ok(())
}
