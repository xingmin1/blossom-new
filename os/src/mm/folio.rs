//! 固定原页引用、范围初始化和内容修改；内容锁与等待源随活 folio 保持原地址。

use alloc::sync::Arc;
use core::cell::UnsafeCell;
use core::marker::{PhantomData, PhantomPinned};
use core::mem::MaybeUninit;
use core::pin::{pin, Pin};
use core::slice;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::arch;
use crate::block::{BlockError, IoResult};
use crate::cache::MetadataCache;
use crate::fs::InodeRef;
use crate::mm::page::{PageDesc, PageUseGuard};
use crate::mm::{alloc_pages, pages, OwnedPages, PAGE_SIZE};
use crate::sched;
use crate::wait::{WaitNode, WaitQueue, Waiter};

const SECTOR_SIZE: usize = 512;
const SECTORS_PER_PAGE: usize = PAGE_SIZE / SECTOR_SIZE;

/// 固定 4KiB 基本页内的 byte 窗口，验证后的起点和长度均可无损保存为 u16。
#[derive(Clone, Copy)]
struct InitializedRange {
    offset: u16,
    len: u16,
}

/// 文件页只初始化一次任意合法范围；元数据页逐 sector 保留有效位和首次具体错误。
enum FolioContents {
    File {
        initialized: Option<Result<InitializedRange, BlockError>>,
    },
    Metadata {
        valid: u8,
        errors: [Option<BlockError>; SECTORS_PER_PAGE],
    },
}

/// 共享头只经所属 PageUse 锁访问；Dirty 与 WB 各自描述实际 512 字节范围。
struct FolioShared {
    references: usize,
    contents: FolioContents,
    dirty: u8,
    writeback: u8,
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

/// 外部页句柄保活实际 canonical 内容域；内部索引和初始读入引用不附此拥有者。
#[derive(Clone)]
enum FolioOwner {
    File(InodeRef),
    Metadata(Arc<MetadataCache>),
}

/// 永久描述符上的一份原页引用；外部取得还保活实际 inode 或元数据缓存。
/// 索引内部的引用不附拥有者，避免 index 到自身内容域的强拥有环。
pub struct FolioRef {
    descriptor: Pin<&'static PageDesc>,
    owner: Option<FolioOwner>,
}

/// 唯一内容锁责任及一份保页引用；未完成时析构以 QueueStopped 结束填充。
/// DMA 层交设备后必须保护整个拥有者，直到实际收权才能完成或析构。
pub struct FolioRead {
    folio: FolioRef,
    offset: usize,
    len: usize,
    completed: bool,
}

/// 普通 Task 借用的内容锁；文件页仅可原始复制，元数据页才可借用初始化范围。
/// 锁不排斥用户共享 store；不得持本 guard 做可能 fault 的用户复制。
pub struct FolioContent<'a> {
    folio: &'a FolioRef,
    locked: bool,
    _task_local: PhantomData<*mut ()>,
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

    /// 只检查当前范围事实；调用者负责持内容锁或拒绝仍被锁定的状态。
    fn range_ready(shared: &FolioShared, offset: usize, len: usize) -> Result<(), BlockError> {
        match &shared.contents {
            FolioContents::File { initialized } => {
                let range = initialized.ok_or(BlockError::NotReady)??;
                let start = usize::from(range.offset);
                let end = start + usize::from(range.len);
                if offset < start || end < offset + len {
                    return Err(BlockError::InvalidRange);
                }
            }
            FolioContents::Metadata { valid, errors } => {
                let sectors = sector_mask(offset, len);
                for sector in offset / SECTOR_SIZE..=(offset + len - 1) / SECTOR_SIZE {
                    if let Some(error) = errors[sector] {
                        return Err(error);
                    }
                }
                if valid & sectors != sectors {
                    return Err(BlockError::NotReady);
                }
            }
        }
        Ok(())
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
            "removed folio retains a content owner"
        );
        let shared = self.shared.get_mut();
        assert_eq!(shared.dirty, 0, "removed folio retains dirty contents");
        assert_eq!(shared.writeback, 0, "removed folio retains writeback");
        self.backing.take().expect("folio backing already removed")
    }
}

impl FolioRef {
    /// 为外部文件页句柄附上实际 inode 引用，内部索引不得保存此句柄。
    /// 重复附同一对象保留原拥有者，不同对象或内容角色暴露为协议错误。
    pub(crate) fn with_file_owner(mut self, owner: InodeRef) -> Self {
        {
            let usage = self.descriptor.usage();
            assert!(
                matches!(
                    &FolioState::shared(&usage).contents,
                    FolioContents::File { .. }
                ),
                "file owner attached to metadata folio"
            );
        }
        match &self.owner {
            None => self.owner = Some(FolioOwner::File(owner)),
            Some(FolioOwner::File(existing)) => {
                assert!(existing.ptr_eq(&owner), "folio file owner changed");
            }
            Some(FolioOwner::Metadata(_)) => panic!("folio owner role changed"),
        }
        self
    }

    /// 为外部元数据页句柄附上真实 Arc 缓存，内部索引不得保存此句柄。
    /// 拥有凭证由 caller 直接移交，不从缓存 payload 地址恢复 Arc。
    pub(crate) fn with_metadata_owner(mut self, owner: Arc<MetadataCache>) -> Self {
        {
            let usage = self.descriptor.usage();
            assert!(
                matches!(
                    &FolioState::shared(&usage).contents,
                    FolioContents::Metadata { .. }
                ),
                "metadata owner attached to file folio"
            );
        }
        match &self.owner {
            None => self.owner = Some(FolioOwner::Metadata(owner)),
            Some(FolioOwner::Metadata(existing)) => {
                assert!(
                    Arc::ptr_eq(existing, &owner),
                    "folio metadata owner changed"
                );
            }
            Some(FolioOwner::File(_)) => panic!("folio owner role changed"),
        }
        self
    }

    /// 借用本外部句柄已有的元数据缓存拥有者，视图无需再保存一份等价 Arc。
    pub(crate) fn metadata_owner(&self) -> Option<&Arc<MetadataCache>> {
        match &self.owner {
            Some(FolioOwner::Metadata(owner)) => Some(owner),
            None | Some(FolioOwner::File(_)) => None,
        }
    }

    /// 消耗真实 order0 拥有者作为文件页；高阶完整返回，不拆分或改变其阶数。
    pub fn from_pages(backing: OwnedPages) -> Result<Self, OwnedPages> {
        Self::install(backing, FolioContents::File { initialized: None })
    }

    /// 消耗独占 order0 页作为元数据页；此角色终身禁止交给用户可写映射。
    pub fn from_metadata_pages(backing: OwnedPages) -> Result<Self, OwnedPages> {
        Self::install(
            backing,
            FolioContents::Metadata {
                valid: 0,
                errors: [None; SECTORS_PER_PAGE],
            },
        )
    }

    /// 在未发表的空用途槽安装原拥有者和终身内容角色。
    fn install(backing: OwnedPages, contents: FolioContents) -> Result<Self, OwnedPages> {
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
                    contents,
                    dirty: 0,
                    writeback: 0,
                }),
                locked: AtomicBool::new(false),
                waiters: WaitQueue::new(),
                _pinned: PhantomPinned,
            });
        }
        // SAFETY: 生产 PageMap 在独占元数据 RAM 中一次发布，描述符永久有效
        // 且不再移动；此 Pin 只借用描述符，活 folio 用途的固定由私有投影协议保证。
        let descriptor = unsafe { Pin::new_unchecked(descriptor) };
        Ok(Self {
            descriptor,
            owner: None,
        })
    }

    /// 从唯一生产页入口取得未清零 order0 页，不申请额外控制对象。
    pub fn allocate() -> Option<Self> {
        let backing = alloc_pages(0, false)?;
        Some(
            Self::from_pages(backing)
                .unwrap_or_else(|_| unreachable!("order0 allocation changed order")),
        )
    }

    /// 从唯一页入口准备未清零元数据页，不建立第二份缓存或额外控制对象。
    pub fn allocate_metadata() -> Option<Self> {
        let backing = alloc_pages(0, false)?;
        Some(
            Self::from_metadata_pages(backing)
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

    /// 文件的一次填充结束后为真（包括失败）；元数据须整页全部 sector 已终结。
    /// 检查特定视图的可读性必须使用 range_ready，不能据此忽略失败或部分有效范围。
    pub fn is_ready(&self) -> bool {
        let usage = self.descriptor.usage();
        if usage.folio().locked.load(Ordering::Acquire) {
            return false;
        }
        match &FolioState::shared(&usage).contents {
            FolioContents::File { initialized } => initialized.is_some(),
            FolioContents::Metadata { valid, errors } => (0..SECTORS_PER_PAGE)
                .all(|sector| valid & (1 << sector) != 0 || errors[sector].is_some()),
        }
    }

    /// 文件页只认领一次任意非空页内范围；已完成或已在途时返回 Busy。
    /// 在候选发表前调用，内容锁责任可整体移交 DMA 完成域，不携带 Task 睡眠锁。
    pub fn begin_read(&self, offset: usize, len: usize) -> Result<FolioRead, BlockError> {
        validate_range(offset, len)?;
        let state = self.state();
        {
            let mut usage = self.descriptor.usage();
            let shared = FolioState::shared_mut(&mut usage);
            match &shared.contents {
                FolioContents::File { initialized: None } => {}
                FolioContents::File { .. } => return Err(BlockError::Busy),
                FolioContents::Metadata { .. } => return Err(BlockError::InvalidRange),
            }
            if state.locked.load(Ordering::Relaxed) {
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
                owner: None,
            },
            offset,
            len,
            completed: false,
        })
    }

    /// 等待同页内容锁后重查请求，仅认领其中首段连续未初始化的 sector。
    /// 请求须按 512 字节对齐；已有效返回 None，失败保留所属 sector 的首次具体错误。
    /// owner 的 offset/len 是实际传输范围，不能用原请求覆盖已有效或 Dirty 字节。
    pub fn begin_metadata_read(
        &self,
        offset: usize,
        len: usize,
    ) -> Result<Option<FolioRead>, BlockError> {
        validate_range(offset, len)?;
        if offset % SECTOR_SIZE != 0 || len % SECTOR_SIZE != 0 {
            return Err(BlockError::Misaligned);
        }
        let mut content = self.lock_content();
        let range = {
            let mut usage = self.descriptor.usage();
            let shared = FolioState::shared_mut(&mut usage);
            let FolioContents::Metadata { valid, errors } = &shared.contents else {
                return Err(BlockError::InvalidRange);
            };
            let end = (offset + len) / SECTOR_SIZE;
            let mut first = offset / SECTOR_SIZE;
            while first < end {
                if let Some(error) = errors[first] {
                    return Err(error);
                }
                if valid & (1 << first) == 0 {
                    break;
                }
                first += 1;
            }
            if first == end {
                return Ok(None);
            }
            let mut last = first + 1;
            while last < end && valid & (1 << last) == 0 && errors[last].is_none() {
                last += 1;
            }
            shared.references = shared
                .references
                .checked_add(1)
                .expect("folio reference count overflow");
            (first * SECTOR_SIZE, (last - first) * SECTOR_SIZE)
        };
        // guard 的锁责任原样交给唯一 owner，不解锁、不携带非 Send 的 Task guard。
        content.locked = false;
        Ok(Some(FolioRead {
            folio: Self {
                descriptor: self.descriptor,
                owner: None,
            },
            offset: range.0,
            len: range.1,
            completed: false,
        }))
    }

    /// 等待当前内容锁结束；文件保留一次范围结果，元数据只认可整页真实有效范围。
    /// 元数据缺失但无 owner 时返回 NotReady，填充精确窗口须使用 begin_metadata_read。
    pub fn wait_ready(&self) -> Result<(), BlockError> {
        let _content = self.lock_content();
        let usage = self.descriptor.usage();
        let shared = FolioState::shared(&usage);
        match &shared.contents {
            FolioContents::File { initialized } => {
                initialized.ok_or(BlockError::NotReady)?.map(|_| ())
            }
            FolioContents::Metadata { .. } => FolioState::range_ready(shared, 0, PAGE_SIZE),
        }
    }

    /// 非阻塞检查本次 byte 窗口；持内容锁、缺失或失败均不授予读取权。
    pub fn range_ready(&self, offset: usize, len: usize) -> Result<(), BlockError> {
        validate_range(offset, len)?;
        let usage = self.descriptor.usage();
        if usage.folio().locked.load(Ordering::Acquire) {
            return Err(BlockError::NotReady);
        }
        FolioState::range_ready(FolioState::shared(&usage), offset, len)
    }

    /// 在普通 Task 中原始复制已初始化内容到独占内核片段，不形成文件页切片。
    /// 等待内容锁但不排斥用户共享 store，因此结果不承诺一致快照；空目标不访问页。
    pub fn copy_to(&self, offset: usize, dst: &mut [u8]) -> Result<(), BlockError> {
        validate_copy_range(offset, dst.len())?;
        if dst.is_empty() {
            return Ok(());
        }
        self.lock_content().copy_to(offset, dst)
    }

    /// 普通 Task 取得页内容锁；沿固定 WaitQueue 先登记再重查，不持用途锁 park。
    /// 内容锁串行内核修改及读入，不排斥用户共享字节访问或替代 DMA 收权证明。
    pub fn lock_content(&self) -> FolioContent<'_> {
        sched::assert_switchable();
        let state = self.state();
        loop {
            if let Some(content) = self.try_lock_content() {
                return content;
            }
            let waiter = Waiter::new();
            let mut node = pin!(WaitNode::new(waiter.waker()));
            // SAFETY: self 保活固定用途内的源，pin! 固定栈节点；
            // 所有退出均先撤 registration，再析构节点和 waiter，不遗忘登记。
            let _registration = unsafe { state.waiters.register(node.as_mut()) };
            if let Some(content) = self.try_lock_content() {
                return content;
            }
            waiter.park();
        }
    }

    /// 用途短锁下取得独立内容锁，不将用途锁带入字节访问或等待。
    fn try_lock_content(&self) -> Option<FolioContent<'_>> {
        let usage = self.descriptor.usage();
        let state = usage.folio();
        if state.locked.load(Ordering::Relaxed) {
            return None;
        }
        state.locked.store(true, Ordering::Release);
        Some(FolioContent {
            folio: self,
            locked: true,
            _task_local: PhantomData,
        })
    }

    /// 私有索引只能回收唯一引用且无内容锁、Dirty、WB 的静止页。
    pub fn can_prune(&self) -> bool {
        let usage = self.descriptor.usage();
        let shared = FolioState::shared(&usage);
        shared.references == 1
            && shared.dirty == 0
            && shared.writeback == 0
            && !usage.folio().locked.load(Ordering::Acquire)
    }

    /// 返回实际 512 字节脏范围位图；该快照不代表已经提交或持久化。
    pub fn dirty_sectors(&self) -> u8 {
        FolioState::shared(&self.descriptor.usage()).dirty
    }

    /// 返回真实回写范围位图；本接口不建立回写、清脏或完成成功的责任。
    pub fn writeback_sectors(&self) -> u8 {
        FolioState::shared(&self.descriptor.usage()).writeback
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
    /// 克隆真实内容域拥有者和原页计数，不复制内容；内部无 anchor 的引用仍保持无 anchor。
    fn clone(&self) -> Self {
        let owner = self.owner.clone();
        let mut usage = self.descriptor.usage();
        let shared = FolioState::shared_mut(&mut usage);
        shared.references = shared
            .references
            .checked_add(1)
            .expect("folio reference count overflow");
        Self {
            descriptor: self.descriptor,
            owner,
        }
    }
}

impl Drop for FolioRef {
    /// 先 put 原页并结束用途锁，再由字段析构释放内容域拥有者，避免持锁析构整个索引。
    /// 末引用仍拒绝有内容锁、Dirty 或 WB 的页，不以拥有者退出替代写入责任。
    fn drop(&mut self) {
        let discarded = {
            let mut usage = self.descriptor.usage();
            let shared = FolioState::shared_mut(&mut usage);
            shared.references = shared
                .references
                .checked_sub(1)
                .expect("folio reference count underflow");
            if shared.references == 0 {
                assert_eq!(
                    shared.dirty, 0,
                    "last folio reference retains dirty contents"
                );
                assert_eq!(
                    shared.writeback, 0,
                    "last folio reference retains writeback"
                );
                assert!(
                    !usage.folio().locked.load(Ordering::Acquire),
                    "last folio reference retains a content owner"
                );
                // SAFETY: 引用归零且无内容锁、Dirty、WB；读入 owner 自持引用，
                // 等待登记和 guard 均借用活 FolioRef，不能跨越其析构。
                // 所有固定投影已结束，只原地析构状态，退出用途锁后才归页。
                Some(unsafe { usage.take_folio() })
            } else {
                None
            }
        };
        // 等待者借用一份活引用，读入者另有保页引用；最后引用时二者均已结束。
        drop(discarded);
    }
}

impl FolioContent<'_> {
    /// 在持内容锁期间检查真实 byte 窗口，不把同页其他 sector 的成功借给本范围。
    pub fn range_ready(&self, offset: usize, len: usize) -> Result<(), BlockError> {
        validate_range(offset, len)?;
        let usage = self.folio.descriptor.usage();
        FolioState::range_ready(FolioState::shared(&usage), offset, len)
    }

    /// 持锁原始复制到独占片段，不建立共享文件内容的 Rust 引用或一致快照。
    pub fn copy_to(&self, offset: usize, dst: &mut [u8]) -> Result<(), BlockError> {
        validate_copy_range(offset, dst.len())?;
        if dst.is_empty() {
            return Ok(());
        }
        self.range_ready(offset, dst.len())?;
        let address = arch::phys_to_virt(self.folio.phys_addr()) + offset;
        // SAFETY: guard 保活原页且已验证完整初始化范围，原始源不创建共享 RAM 引用；
        // dst 是调用者独占的普通内核片段，不与本页有效内容重叠。
        // metadata 锁排斥写者，file 用户共享 store 可并发，汇编只作有界字节访问。
        unsafe { arch::copy_user_bytes(dst.as_mut_ptr(), address as *const u8, dst.len()) };
        Ok(())
    }

    /// 从普通内核片段原始修改已有效范围，修改前标记覆盖到的实际 sector 为 Dirty。
    /// 不清除已有 WB，不承诺用户共享 store 与此次复制之间的快照或顺序。
    pub fn copy_from(&mut self, offset: usize, src: &[u8]) -> Result<(), BlockError> {
        validate_copy_range(offset, src.len())?;
        if src.is_empty() {
            return Ok(());
        }
        self.mark_dirty(offset, src.len())?;
        let address = arch::phys_to_virt(self.folio.phys_addr()) + offset;
        // SAFETY: guard 保活原页并串行内核修改，mark_dirty 已验证已初始化范围；
        // src 是已初始化内核片段，其借用期间不能并发修改，不与目标原页重叠。
        // 目标没有 Rust 内容引用，用户共享 store 仅经原始访问；汇编不越界。
        unsafe { arch::copy_user_bytes(address as *mut u8, src.as_ptr(), src.len()) };
        Ok(())
    }

    /// 仅对终身元数据角色借用已初始化窗口；返回借用不能超出本内容 guard。
    /// 持该借用时不得做可能 fault 的用户访问，也不得通过物理地址交用户或设备写。
    pub fn metadata_bytes(&self, offset: usize, len: usize) -> Result<&[u8], BlockError> {
        self.metadata_range(offset, len)?;
        let address = arch::phys_to_virt(self.folio.phys_addr()) + offset;
        // SAFETY: 终身 Metadata 角色从未交用户共享写，guard 排斥 CPU 修改和初始化 DMA；
        // 完整窗口已经初始化，folio 引用保活固定原页，切片绑定 guard 且不越界。
        Ok(unsafe { slice::from_raw_parts(address as *const u8, len) })
    }

    /// 在同一元数据锁下借用可修改窗口，并在借用前标记对应 sector 为 Dirty。
    /// 邻记录和校验和应在此真实窗口内同步修改，不得拷回旧整块快照。
    pub fn metadata_bytes_mut(
        &mut self,
        offset: usize,
        len: usize,
    ) -> Result<&mut [u8], BlockError> {
        self.metadata_range(offset, len)?;
        self.mark_dirty(offset, len)?;
        let address = arch::phys_to_virt(self.folio.phys_addr()) + offset;
        // SAFETY: Metadata 角色无用户共享写或内容 DMA，guard 排斥其他内核访问；
        // &mut self 结束此前 guard 借用，窗口已初始化且保活，唯一可变切片绑定本 guard。
        Ok(unsafe { slice::from_raw_parts_mut(address as *mut u8, len) })
    }

    /// 在真实删除或失效协议下作废整页有效、失败和 Dirty 状态，不代表回写成功。
    ///
    /// # Safety
    /// 调用者须已从真实索引摘离本页并阻止新取得，结束全部 IO/WB 和用户映射访问，
    /// 完成必要失效及 TLB 同步；其他持页者不得再按旧内容使用或交设备。
    /// 内容须属于未发表候选或实际删除/失效协议，且确实无需落盘；
    /// 独占引用计数不能代替上述证明，已接受的文件系统修改不得仅为归页而丢弃。
    pub unsafe fn discard(&mut self) {
        assert!(self.locked, "discard without a content guard");
        let mut usage = self.folio.descriptor.usage();
        assert!(
            usage.folio().locked.load(Ordering::Relaxed),
            "discard lost content lock"
        );
        let shared = FolioState::shared_mut(&mut usage);
        assert_eq!(shared.writeback, 0, "discard retains writeback");
        // 本 guard 独占内容锁，初始化 owner 无法同时存在；设备尾部与外部
        // 映射失效仍由 caller 证明，状态清除不能取消任何已接受的访问责任。
        match &mut shared.contents {
            FolioContents::File { initialized } => *initialized = None,
            FolioContents::Metadata { valid, errors } => {
                *valid = 0;
                errors.fill(None);
            }
        }
        shared.dirty = 0;
    }

    /// 确认终身元数据角色和完整范围，普通文件页不能借用任何内容切片。
    fn metadata_range(&self, offset: usize, len: usize) -> Result<(), BlockError> {
        validate_range(offset, len)?;
        let usage = self.folio.descriptor.usage();
        let shared = FolioState::shared(&usage);
        if !matches!(&shared.contents, FolioContents::Metadata { .. }) {
            return Err(BlockError::InvalidRange);
        }
        FolioState::range_ready(shared, offset, len)
    }

    /// 只在有效 byte 窗口上增加 Dirty；WB 独立保留，失败不会污染其他范围。
    fn mark_dirty(&mut self, offset: usize, len: usize) -> Result<(), BlockError> {
        validate_range(offset, len)?;
        let mut usage = self.folio.descriptor.usage();
        let shared = FolioState::shared_mut(&mut usage);
        FolioState::range_ready(shared, offset, len)?;
        shared.dirty |= sector_mask(offset, len);
        Ok(())
    }
}

impl Drop for FolioContent<'_> {
    /// 归还实际内容锁并在用途锁外通知等待者；已交读入 owner 的责任不二次解锁。
    fn drop(&mut self) {
        if !self.locked {
            return;
        }
        let state = self.folio.state();
        {
            let usage = self.folio.descriptor.usage();
            assert!(
                usage.folio().locked.load(Ordering::Relaxed),
                "folio content lock lost"
            );
            state.locked.store(false, Ordering::Release);
        }
        state.waiters.wake_all();
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

    /// DMA 层拒绝将已经完成其初始化责任的拥有者再次交给设备。
    pub(crate) fn is_completed(&self) -> bool {
        self.completed
    }

    /// 完成前只借用本 owner 的缺失初始化范围；交设备后 DMA 层不得调用此入口。
    pub fn bytes_mut(&mut self) -> &mut [MaybeUninit<u8>] {
        assert!(
            !self.completed,
            "completed folio read cannot write contents"
        );
        let address = arch::phys_to_virt(self.phys_addr()) + self.offset;
        // SAFETY: 本对象唯一持有内容锁，范围尚未有效且不能交用户共享写；
        // metadata 的已有有效范围不在本 owner 内，guard 被内容锁排斥。
        // MaybeUninit 允许未初始化字节，借用绑定 &mut self；DMA 期间整体保护 owner。
        unsafe { slice::from_raw_parts_mut(address as *mut MaybeUninit<u8>, self.len) }
    }

    /// 在真实 DMA 全尾后发表唯一结果、解锁并锁外唤醒；重复完成不改变首次结果。
    ///
    /// # Safety
    /// 本 owner 范围的全部设备、CPU 初始化访问必须已经结束；成功要求其全部字节初始化。
    /// 部分成功与失败不授予本范围内容访问权，完成后原初始化地址不能再被写入。
    /// 元数据其他有效或失败范围保留；完成不约束后续合法 guard 修改或用户共享 store。
    pub unsafe fn complete(&mut self, result: IoResult) {
        if self.completed {
            return;
        }
        let initialized = match result.error {
            Some(error) => Err(error),
            None if result.completed == self.len => Ok(InitializedRange {
                offset: self.offset as u16,
                len: self.len as u16,
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
                state.locked.load(Ordering::Relaxed),
                "folio read lost its content lock"
            );
            match &mut shared.contents {
                FolioContents::File {
                    initialized: result,
                } => {
                    assert!(result.is_none(), "folio result already published");
                    *result = Some(initialized);
                }
                FolioContents::Metadata { valid, errors } => {
                    let sectors = sector_mask(self.offset, self.len);
                    assert_eq!(
                        *valid & sectors,
                        0,
                        "metadata read overwrites valid contents"
                    );
                    for sector in self.offset / SECTOR_SIZE..(self.offset + self.len) / SECTOR_SIZE
                    {
                        assert!(
                            errors[sector].is_none(),
                            "metadata read overwrites failed contents"
                        );
                        if let Err(error) = initialized {
                            errors[sector] = Some(error);
                        }
                    }
                    if initialized.is_ok() {
                        *valid |= sectors;
                    }
                }
            }
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

/// 原始复制允许页尾零长请求，非零请求仍须满足同一 byte 几何边界。
fn validate_copy_range(offset: usize, len: usize) -> Result<(), BlockError> {
    if len == 0 {
        return if offset <= PAGE_SIZE {
            Ok(())
        } else {
            Err(BlockError::InvalidRange)
        };
    }
    validate_range(offset, len)
}

/// 将已验证非空 byte 窗口投影到实际覆盖的 512 字节 sector，不扩大到整页。
fn sector_mask(offset: usize, len: usize) -> u8 {
    let first = offset / SECTOR_SIZE;
    let last = (offset + len - 1) / SECTOR_SIZE;
    (((1_u16 << (last - first + 1)) - 1) << first) as u8
}
