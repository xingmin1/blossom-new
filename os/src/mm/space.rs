//! 有序用户范围与固定 PTE 域分责；mmap 变更和文件 rmap 都在同一短锁核实际叶。

use alloc::boxed::Box;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::ops::Range;
use core::slice;
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::arch::{self, UserAccess};
use crate::cache::FileRangeData;
use crate::cpu;
use crate::fs::{ExecAccess, FileRef, FsError, InodeKind, InodeRef};
use crate::mm::anon::SharedAnon;
use crate::mm::user_table::{
    Mapping, PagePermissions, PageTableError, PreparedTables, RetiredTables, UserPageTable,
};
use crate::mm::vma::{self, MmapState, Vma, VmaBacking};
use crate::mm::{AnonPageRef, FolioRef, OwnedPages, PAGE_SIZE};
use crate::sync::{IrqSaveGuard, ReadGuard, SleepRwLock, SpinLock, SpinLockGuard};

/// 用户映射失败保留范围、权限、实际文件和内存不足各自的原因。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VmError {
    NoMemory,
    InvalidRange,
    Occupied,
    Unmapped,
    Permission,
    BeyondEof,
    File(FsError),
}

impl From<PageTableError> for VmError {
    /// 页表准备失败保持真实原因，缺准备表暴露为内部提交协议错误。
    fn from(error: PageTableError) -> Self {
        match error {
            PageTableError::NoMemory => Self::NoMemory,
            PageTableError::InvalidRange => Self::InvalidRange,
            PageTableError::Occupied => Self::Occupied,
            PageTableError::MissingTables => {
                panic!("user mapping committed without prepared tables")
            }
        }
    }
}

/// 用户选址将可移动 hint、固定覆盖和固定不覆盖明确区分。
pub(crate) enum MmapAddress {
    Hint(usize),
    Fixed(usize),
    FixedNoReplace(usize),
}

/// 映射源持真实打开实例；共享匿名对象在每次独立 mmap 时创建。
pub(crate) enum MmapSource {
    PrivateAnonymous,
    SharedAnonymous,
    File {
        file: FileRef,
        offset: u64,
        private: bool,
    },
}

/// 当前与最大权限由 ABI/OFD 边界决定，Mm 再守页范围和只读后备约束。
pub(crate) struct MmapRequest {
    pub(crate) address: MmapAddress,
    pub(crate) length: usize,
    pub(crate) permissions: PagePermissions,
    pub(crate) maximum: PagePermissions,
    pub(crate) source: MmapSource,
}

/// 已初始化的驻留原页；文件共享写须先完成原页脏准备，私有写复制匿名页。
#[derive(Clone)]
enum ResidentBacking {
    Anonymous(AnonPageRef),
    File(FolioRef),
}

impl ResidentBacking {
    /// 取得本引用保活的物理身份，不授予外部内容访问或再次归还权限。
    fn physical(&self) -> usize {
        match self {
            Self::Anonymous(page) => page.phys_addr(),
            Self::File(page) => page.phys_addr(),
        }
    }
}

struct ResidentPage {
    address: usize,
    backing: ResidentBacking,
    cow: bool,
    source: Weak<FileRangeData>,
    next: Option<Box<ResidentPage>>,
}

impl ResidentPage {
    /// 在 PTE 锁外准备具体驻留拥有节点，失败只释放本次未发表引用。
    fn try_new(address: usize, backing: ResidentBacking, cow: bool) -> Result<Box<Self>, VmError> {
        Box::try_new(Self {
            address,
            backing,
            cow,
            source: Weak::new(),
            next: None,
        })
        .map_err(|_| VmError::NoMemory)
    }

    /// 默认 COW 和文件叶保持只读，文件 W 只能由持内容锁的脏准备另行发表。
    fn permissions(&self, permissions: PagePermissions) -> PagePermissions {
        PagePermissions {
            write: permissions.write
                && !self.cow
                && !matches!(&self.backing, ResidentBacking::File(_)),
            ..permissions
        }
    }
}

/// 具体链只在短锁内移动已准备节点，回收采用迭代析构，避免用户页数转化为内核栈深度。
#[derive(Default)]
struct ResidentPages {
    head: Option<Box<ResidentPage>>,
}

impl ResidentPages {
    /// 查找实际 VA 的驻留责任；初版线性链不在短锁中建立额外分配索引。
    fn find(&self, address: usize) -> Option<&ResidentPage> {
        let mut cursor = self.head.as_deref();
        while let Some(page) = cursor {
            if page.address == address {
                return Some(page);
            }
            cursor = page.next.as_deref();
        }
        None
    }

    /// 在唯一 PTE 修改域定位可修改的 COW 标志，不借用或修改用户内容。
    fn find_mut(&mut self, address: usize) -> Option<&mut ResidentPage> {
        let mut cursor = self.head.as_deref_mut();
        while let Some(page) = cursor {
            if page.address == address {
                return Some(page);
            }
            cursor = page.next.as_deref_mut();
        }
        None
    }

    /// 移入已准备且未链接的具体节点，不分配、不释放旧页。
    fn push(&mut self, mut page: Box<ResidentPage>) {
        assert!(page.next.is_none(), "resident page is already linked");
        page.next = self.head.take();
        self.head = Some(page);
    }

    /// 摘取真实节点并断开后继，返回拥有只能在失效完成后锁外释放。
    fn remove(&mut self, address: usize) -> Option<Box<ResidentPage>> {
        let mut link = &mut self.head;
        while link.is_some() {
            if link.as_ref().unwrap().address == address {
                let mut page = link.take().unwrap();
                *link = page.next.take();
                return Some(page);
            }
            link = &mut link.as_mut().unwrap().next;
        }
        None
    }

    /// 只计算当前范围的实际驻留数量，供写侧在短锁外准备结果容器。
    fn count(&self, range: &Range<usize>) -> usize {
        let mut count = 0;
        let mut cursor = self.head.as_deref();
        while let Some(page) = cursor {
            count += usize::from(range.contains(&page.address));
            cursor = page.next.as_deref();
        }
        count
    }

    /// 向已足额预留的容器写入实际 VA，不允许在 PTE 锁中扩容。
    fn addresses(&self, range: &Range<usize>, addresses: &mut Vec<usize>) {
        let mut cursor = self.head.as_deref();
        while let Some(page) = cursor {
            if range.contains(&page.address) {
                assert!(addresses.len() < addresses.capacity());
                addresses.push(page.address);
            }
            cursor = page.next.as_deref();
        }
    }
}

impl Drop for ResidentPages {
    /// 硬件使用结束或同步失效完成后逐个释放原节点，不递归析构整条链。
    fn drop(&mut self) {
        while let Some(mut page) = self.head.take() {
            self.head = page.next.take();
            drop(page);
        }
    }
}

struct PteState {
    table: UserPageTable,
    pages: ResidentPages,
}

impl PteState {
    /// 当前驻留 PFN 与实际叶须同锁吻合，软件 COW 和硬件无许可均要求锁外 fault。
    fn allows(&self, address: usize, access: UserAccess) -> bool {
        self.pages.find(address).is_some_and(|page| {
            self.table.lookup(address).is_some_and(|leaf| {
                leaf.physical == page.backing.physical() && vma::allows(leaf.permissions, access)
            }) && !(page.cow && access == UserAccess::Write)
        })
    }
}

/// 固定地址的唯一 PTE 修改域；范围登记强持域，驻留页弱持范围，不形成强环。
pub(crate) struct PteDomain {
    state: SpinLock<PteState>,
}

impl PteDomain {
    /// 所有驻留拥有和硬件叶修改沿原短锁串行，锁内不分配或最后释放来源。
    fn lock(&self) -> SpinLockGuard<'_, PteState> {
        self.state.lock()
    }

    /// 内容锁持有者只从固定来源检查重叠叶，不取得 Mm 的 mmap 锁。
    pub(crate) fn mkclean_range(
        &self,
        source: &Arc<FileRangeData>,
        offset: u64,
        physical: usize,
    ) -> (bool, usize) {
        let Some(relative) = offset.checked_sub(source.offset) else {
            return (false, 0);
        };
        if (source.range.end - source.range.start) as u64 <= relative {
            return (false, 0);
        }
        let address = source.range.start + relative as usize;
        let mut state = self.lock();
        let Some(page) = state.pages.find(address) else {
            return (false, 0);
        };
        if page.source.as_ptr() != Arc::as_ptr(source)
            || !matches!(&page.backing, ResidentBacking::File(_))
            || page.backing.physical() != physical
        {
            return (false, 0);
        }
        // SAFETY: 同一 PTE 锁核实当前来源和 PFN，page 与内容 guard 保活原页；
        // 同 PFN 原子降权不释放数据或表，调用者在所有短锁外同步 CPU。
        unsafe { state.table.mkclean(address, physical) }
            .map_or((false, 0), |(dirty, changed)| (dirty, usize::from(changed)))
    }
}

struct FaultSource {
    vma: Arc<Vma>,
    backing: Option<ResidentBacking>,
    observed: Option<usize>,
    needed: usize,
}

/// 同一 Mm 的范围读写锁与 PTE 短锁分责；活动根由 CPU 强拥有到切回内核根后。
pub struct Mm {
    table: Arc<PteDomain>,
    mmap: SleepRwLock<MmapState>,
    users: AtomicUsize,
    /// 原执行文件与写排斥保留到此 Mm 最后实际拥有结束，普通私有映射不取得责任。
    executable: Option<(FileRef, ExecAccess)>,
    #[cfg(feature = "ktest")]
    mkwrite_hook: SpinLock<Option<Arc<dyn Fn() + Send + Sync>>>,
    #[cfg(feature = "ktest")]
    mkwrites: AtomicUsize,
}

type RetiredVmas = (Vec<Arc<Vma>>, Vec<Weak<FileRangeData>>);

/// 一份真实用户线程地址空间绑定；页表、CPU 和 futex key 的 Arc 不参与计数。
pub struct MmUser {
    mm: Arc<Mm>,
}

impl MmUser {
    /// 发表前取得一份绑定责任；失败路径和正常退出均由同一 Drop 归还。
    pub fn new(mm: Arc<Mm>) -> Self {
        mm.users
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                count.checked_add(1)
            })
            .expect("MM user count overflow");
        Self { mm }
    }

    /// 借用本绑定保活的真实地址空间，不额外增加用户绑定计数。
    pub fn mm(&self) -> &Arc<Mm> {
        &self.mm
    }
}

impl Drop for MmUser {
    /// 先归还真实绑定；Arc 的最后析构发生在原调用者持有的锁之外。
    fn drop(&mut self) {
        let previous = self.mm.users.fetch_sub(1, Ordering::AcqRel);
        assert!(previous != 0, "MM user count underflow");
    }
}

/// futex 保真实 Mm、共享匿名或 canonical inode 身份，不把可复用 PFN 当作长期 key。
#[derive(Clone)]
pub(crate) enum FutexKey {
    Mm {
        mm: Arc<Mm>,
        address: usize,
        private: bool,
    },
    Shared {
        owner: Arc<SharedAnon>,
        offset: u64,
    },
    File {
        inode: InodeRef,
        offset: u64,
    },
}

impl FutexKey {
    /// 按真实对象及字节位置比较，包括共享对象页内偏移。
    pub(crate) fn matches(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Self::Mm {
                    mm,
                    address,
                    private,
                },
                Self::Mm {
                    mm: other_mm,
                    address: other_address,
                    private: other_private,
                },
            ) => Arc::ptr_eq(mm, other_mm) && address == other_address && private == other_private,
            (
                Self::Shared { owner, offset },
                Self::Shared {
                    owner: other_owner,
                    offset: other_offset,
                },
            ) => Arc::ptr_eq(owner, other_owner) && offset == other_offset,
            (
                Self::File { inode, offset },
                Self::File {
                    inode: other_inode,
                    offset: other_offset,
                },
            ) => inode.ptr_eq(other_inode) && offset == other_offset,
            _ => false,
        }
    }

    /// 哈希只选择固定桶；相等性仍由完整的实际对象身份决定。
    pub(crate) fn hash(&self) -> usize {
        let (owner, position, kind) = match self {
            Self::Mm {
                mm,
                address,
                private,
            } => (Arc::as_ptr(mm) as usize, *address, usize::from(!*private)),
            Self::Shared { owner, offset } => {
                (Arc::as_ptr(owner) as usize, *offset as usize, 2usize)
            }
            Self::File { inode, offset } => {
                (&**inode as *const _ as usize, *offset as usize, 3usize)
            }
        };
        (owner >> 4).wrapping_mul(0x9e37_79b9) ^ (position >> 2) ^ kind
    }
}

/// 实际来源辨别需要缺页或私有 COW 时，调用者须放开 mmap 读侧再 fault。
pub(crate) enum FutexAccessError {
    Vm(VmError),
    NeedsFault(UserAccess),
}

impl From<VmError> for FutexAccessError {
    /// 保留实际范围、权限或缺页准备失败的 VM 原因。
    fn from(error: VmError) -> Self {
        Self::Vm(error)
    }
}

/// 先取得可睡眠 mmap 读侧，再建立唯一 Waiter；之后的字访问只取得 PTE 短锁。
pub(crate) struct FutexRead<'a> {
    mm: &'a Arc<Mm>,
    mmap: ReadGuard<'a, MmapState>,
}

impl FutexRead<'_> {
    /// 从同一稳定读侧解析当前 VMA 的具体 futex 身份。
    pub(crate) fn key(&self, address: usize, private: bool) -> Result<FutexKey, FutexAccessError> {
        if address & 3 != 0
            || address
                .checked_add(4)
                .is_none_or(|end| arch::USER_ADDRESS_END < end)
        {
            return Err(VmError::InvalidRange.into());
        }
        if private {
            return Ok(FutexKey::Mm {
                mm: Arc::clone(self.mm),
                address,
                private: true,
            });
        }
        let vma = self.mmap.find(address).ok_or(VmError::Unmapped)?;
        if !vma.allows(UserAccess::Read) {
            return Err(VmError::Permission.into());
        }
        match &vma.backing {
            VmaBacking::SharedAnonymous { owner, offset } => {
                return Ok(FutexKey::Shared {
                    owner: Arc::clone(owner),
                    offset: offset
                        .checked_add((address - vma.range.start) as u64)
                        .ok_or(VmError::InvalidRange)?,
                });
            }
            VmaBacking::File {
                offset, private, ..
            } => {
                let base = address & !(PAGE_SIZE - 1);
                let state = self.mm.table.lock();
                let Some(page) = state.pages.find(base) else {
                    let access = if *private && vma.allows(UserAccess::Write) {
                        UserAccess::Write
                    } else {
                        UserAccess::Read
                    };
                    return Err(FutexAccessError::NeedsFault(access));
                };
                if *private {
                    if vma.allows(UserAccess::Write) && page.cow {
                        return Err(FutexAccessError::NeedsFault(UserAccess::Write));
                    }
                    if matches!(&page.backing, ResidentBacking::Anonymous(_)) {
                        if !vma.allows(UserAccess::Write) {
                            return Err(VmError::Permission.into());
                        }
                        return Ok(FutexKey::Mm {
                            mm: Arc::clone(self.mm),
                            address,
                            private: false,
                        });
                    }
                }
                return Ok(FutexKey::File {
                    inode: vma
                        .file_range
                        .as_ref()
                        .expect("file VMA has no range")
                        .data()
                        .inode
                        .clone(),
                    offset: offset
                        .checked_add((address - vma.range.start) as u64)
                        .ok_or(VmError::InvalidRange)?,
                });
            }
            _ => {}
        }
        if !vma.allows(UserAccess::Write) {
            return Err(VmError::Permission.into());
        }
        Ok(FutexKey::Mm {
            mm: Arc::clone(self.mm),
            address,
            private: false,
        })
    }

    /// 在 mmap→PTE 保护下使用原始地址；缺页返回 None，准备必须放开此读侧后进行。
    /// visit 只允许原始字访问及固定桶操作，不得分配、睡眠或保留可访问地址。
    pub(crate) fn with_word<R>(&self, address: usize, visit: impl FnOnce(usize) -> R) -> Option<R> {
        if !self.mmap.find(address)?.allows(UserAccess::Read) {
            return None;
        }
        let base = address & !(PAGE_SIZE - 1);
        let state = self.mm.table.lock();
        let page = state.pages.find(base)?;
        let mapping = state.table.lookup(base)?;
        if mapping.physical != page.backing.physical()
            || !vma::allows(mapping.permissions, UserAccess::Read)
        {
            return None;
        }
        Some(visit(arch::phys_to_virt(mapping.physical) + address - base))
    }
}

impl Mm {
    /// 在普通任务上下文准备独立页表，尚未发表给执行者或硬件。
    pub fn try_new() -> Result<Self, PageTableError> {
        Ok(Self {
            table: Arc::try_new(PteDomain {
                state: SpinLock::new(PteState {
                    table: UserPageTable::try_new()?,
                    pages: ResidentPages::default(),
                }),
            })
            .map_err(|_| PageTableError::NoMemory)?,
            mmap: SleepRwLock::new(MmapState::default()),
            users: AtomicUsize::new(0),
            executable: None,
            #[cfg(feature = "ktest")]
            mkwrite_hook: SpinLock::new(None),
            #[cfg(feature = "ktest")]
            mkwrites: AtomicUsize::new(0),
        })
    }

    /// 未发表 Mm 接受原执行文件及其写排斥，后续错误由 Mm 正常析构归还。
    pub(crate) fn bind_executable(&mut self, file: FileRef, access: ExecAccess) {
        assert!(self.executable.is_none(), "MM executable already bound");
        self.executable = Some((file, access));
    }

    /// 返回真实用户线程绑定数，不包含 CPU、诊断或同步 key 的拥有引用。
    pub fn user_count(&self) -> usize {
        self.users.load(Ordering::Acquire)
    }

    /// 在唯一 futex Waiter 建立前取得可睡眠范围读侧，排除后续比较登记期间的重映射。
    pub(crate) fn futex_read(self: &Arc<Self>) -> FutexRead<'_> {
        FutexRead {
            mm: self,
            mmap: self.mmap.read(),
        }
    }

    /// 在当前共享匿名对象上安装真实额外别名，保留源 VMA；不接受文件或私有匿名。
    #[cfg(feature = "ktest")]
    pub fn test_shared_alias(
        &self,
        source: usize,
        destination: usize,
        length: usize,
    ) -> Result<(), VmError> {
        let source_range = vma::checked_range(source, length)?;
        let destination_range = vma::checked_range(destination, length)?;
        let mut mmap = self.mmap.write();
        let source_vma = mmap.find(source).ok_or(VmError::Unmapped)?;
        if source_vma.range.end < source_range.end {
            return Err(VmError::InvalidRange);
        }
        let VmaBacking::SharedAnonymous { owner, offset } = &source_vma.backing else {
            return Err(VmError::Permission);
        };
        let alias = Vma::try_new(
            destination_range,
            source_vma.permissions,
            source_vma.maximum,
            VmaBacking::SharedAnonymous {
                owner: Arc::clone(owner),
                offset: offset
                    .checked_add((source - source_vma.range.start) as u64)
                    .ok_or(VmError::InvalidRange)?,
            },
            None,
        )?;
        let retired = self.insert_vma(&mut mmap, alias)?;
        drop(mmap);
        drop(retired);
        Ok(())
    }

    /// 在尚未共享的地址空间安装完整初始化单页，保留当前与最大权限并转交匿名页归还责任。
    ///
    /// # Safety
    /// 调用者必须完整初始化 pages 的全部字节，结束全部排他 Rust 引用及未受控原始访问；
    /// 页面只能作为用户数据或代码，不能包含内核私有对象。成功后不得凭旧裸地址释放该页。
    pub unsafe fn map_owned(
        &mut self,
        address: usize,
        pages: OwnedPages,
        permissions: PagePermissions,
        maximum: PagePermissions,
    ) -> Result<(), PageTableError> {
        if pages.order() != 0 {
            return Err(PageTableError::InvalidRange);
        }
        let range = vma::checked_range(address, PAGE_SIZE).map_err(table_error)?;
        let vma = Vma::try_new(range, permissions, maximum, VmaBacking::Anonymous, None)
            .map_err(table_error)?;
        // SAFETY: 调用者保证完整初始化的独占 order0 页，无其他映射、设备或内容借用；
        // 此处消费原归还权，后续访问仅沿本 Mm 的原始访问和硬件权限协议。
        let backing = unsafe { AnonPageRef::from_owned(pages) };
        let page = ResidentPage::try_new(address, ResidentBacking::Anonymous(backing), false)
            .map_err(table_error)?;
        let needed = self.table.lock().table.needed_tables(address);
        let mut prepared = PreparedTables::try_new(needed)?;
        let mut mmap = self.mmap.write();
        let retired = self.insert_vma(&mut mmap, vma).map_err(table_error)?;
        {
            let mut state = self.table.lock();
            // SAFETY: self 的唯一借用排除活动共享，page 完整初始化并保有原页；
            // 中间表锁外准备，PTE 和驻留拥有在同一短锁内发表，离开全部 CPU 后才归还。
            let previous = unsafe {
                state
                    .table
                    .map(address, page.backing.physical(), permissions, &mut prepared)
            }?;
            assert!(previous.is_none(), "owned page replaced an existing leaf");
            state.pages.push(page);
        }
        drop(mmap);
        drop(retired);
        drop(prepared);
        arch::sync_user_icache();
        Ok(())
    }

    /// 只发表私有匿名范围，首次实际访问在锁外申请清零基本页。
    pub fn map_anonymous(
        &self,
        address: usize,
        length: usize,
        permissions: PagePermissions,
        maximum: PagePermissions,
    ) -> Result<(), VmError> {
        let range = vma::checked_range(address, length)?;
        let vma = Vma::try_new(range, permissions, maximum, VmaBacking::Anonymous, None)?;
        let mut mmap = self.mmap.write();
        let retired = self.insert_vma(&mut mmap, vma)?;
        drop(mmap);
        drop(retired);
        Ok(())
    }

    /// 映射持完整打开实例，属性检查与取页均不占 mmap/PTE 锁，关闭 fd 不撤销映射。
    pub fn map_file(
        &self,
        address: usize,
        length: usize,
        permissions: PagePermissions,
        maximum: PagePermissions,
        file: FileRef,
        offset: u64,
        private: bool,
    ) -> Result<(), VmError> {
        let range = vma::checked_range(address, length)?;
        check_file(&file, offset, length, private, permissions, maximum)?;
        let vma = Vma::try_new(
            range,
            permissions,
            maximum,
            VmaBacking::File {
                file,
                offset,
                private,
            },
            Some(&self.table),
        )?;
        let mut mmap = self.mmap.write();
        let retired = self.insert_vma(&mut mmap, vma)?;
        drop(mmap);
        drop(retired);
        Ok(())
    }

    /// 页上取整用户长度并在同一写侧选址与发表；固定覆盖先备齐候选再解除旧叶。
    pub(crate) fn mmap(&self, request: MmapRequest) -> Result<usize, VmError> {
        if request.length == 0 {
            return Err(VmError::InvalidRange);
        }
        let length = page_end(request.length)?;
        if let MmapSource::File {
            file,
            offset,
            private,
        } = &request.source
        {
            check_file(
                file,
                *offset,
                length,
                *private,
                request.permissions,
                request.maximum,
            )?;
        }
        let mut mmap = self.mmap.write();
        let (address, replace) = match request.address {
            MmapAddress::Hint(hint) => (mmap.choose_address(hint, length)?, false),
            MmapAddress::Fixed(address) => (address, true),
            MmapAddress::FixedNoReplace(address) => (address, false),
        };
        let range = vma::checked_range(address, length)?;
        if !replace && !mmap.vacant(&range) {
            return Err(VmError::Occupied);
        }
        let backing = match request.source {
            MmapSource::PrivateAnonymous => VmaBacking::Anonymous,
            MmapSource::SharedAnonymous => VmaBacking::SharedAnonymous {
                owner: SharedAnon::try_new()?,
                offset: 0,
            },
            MmapSource::File {
                file,
                offset,
                private,
            } => VmaBacking::File {
                file,
                offset,
                private,
            },
        };
        let vma = Vma::try_new(
            range.clone(),
            request.permissions,
            request.maximum,
            backing,
            Some(&self.table),
        )?;
        if replace {
            let retired = self.replace_range(&mut mmap, &range, Some(vma))?;
            drop(mmap);
            drop(retired);
        } else {
            let retired = self.insert_vma(&mut mmap, vma)?;
            drop(mmap);
            drop(retired);
        }
        Ok(address)
    }

    /// byte break 与页增删在单次 mmap 写侧完成；零请求及任何失败均返回原 break。
    pub(crate) fn brk(&self, request: usize) -> usize {
        let mut mmap = self.mmap.write();
        let old = mmap.heap_end;
        if request == 0 || request < mmap.heap_start || arch::USER_ADDRESS_END < request {
            return old;
        }
        let Ok(end) = page_end(request) else {
            return old;
        };
        let Ok(previous) = page_end(old) else {
            return old;
        };
        let mut retired = None;
        let mut inserted = None;
        if previous < end {
            let range = previous..end;
            if !mmap.vacant(&range) {
                return old;
            }
            let permissions = PagePermissions {
                read: true,
                write: true,
                execute: false,
            };
            let maximum = PagePermissions {
                read: true,
                write: true,
                execute: true,
            };
            let candidate = Vma::try_new(range, permissions, maximum, VmaBacking::Anonymous, None);
            match candidate.and_then(|vma| self.insert_vma(&mut mmap, vma)) {
                Ok(old_vmas) => inserted = Some(old_vmas),
                Err(_) => return old,
            }
        } else if end < previous {
            match self.replace_range(&mut mmap, &(end..previous), None) {
                Ok(discarded) => retired = Some(discarded),
                Err(_) => return old,
            }
        }
        mmap.heap_end = request;
        drop(mmap);
        drop(inserted);
        drop(retired);
        request
    }

    /// 按当前 VMA 与实际叶权限处理缺页、COW 或文件写准备，睡眠与复制均在映射锁外。
    pub fn fault(&self, address: usize, access: UserAccess) -> Result<(), VmError> {
        let base = address & !(PAGE_SIZE - 1);
        loop {
            let Some(source) = self.fault_source(address, access)? else {
                return Ok(());
            };
            if access == UserAccess::Write && source.vma.is_shared_file() {
                if self.mkwrite(&source, address)? {
                    return Ok(());
                }
                continue;
            }
            let preparation = prepare_fault(&source, base, access);
            let mmap = self.mmap.write();
            let current = mmap.find(address).ok_or(VmError::Unmapped)?;
            if !current.allows(access) {
                return Err(VmError::Permission);
            }
            if !Arc::ptr_eq(current, &source.vma) {
                drop(mmap);
                drop(preparation);
                continue;
            }
            let (page, mut prepared) = match preparation {
                Ok(prepared) => prepared,
                Err(error) => {
                    if self.table.lock().allows(base, access) {
                        return Ok(());
                    }
                    return Err(error);
                }
            };
            {
                let mut state = self.table.lock();
                let resident = state.pages.find(base);
                if state.allows(base, access) {
                    return Ok(());
                }
                if resident.map(|page| page.backing.physical()) != source.observed
                    || source.needed < state.table.needed_tables(base)
                {
                    continue;
                }
                if source.observed.is_some() {
                    // SAFETY: 当前短锁同时核实驻留和实际叶，source 及 resident 保活旧页；
                    // 只清叶而保留目录，mmap 写侧排除替换，短锁外同步完成后才发表新页。
                    let old = unsafe { state.table.clear_leaf(base) };
                    assert_eq!(old.map(|leaf| leaf.physical), source.observed);
                }
            }
            sync_all();
            let old = {
                let mut state = self.table.lock();
                let permissions = page.permissions(current.permissions);
                // SAFETY: 旧叶已解除且同步失效，mmap 写侧保身份和目录；新页已初始化，
                // 新旧引用保活到各自翻译停止，shared file 叶在普通 fault 中只读。
                let previous = unsafe {
                    state
                        .table
                        .map(base, page.backing.physical(), permissions, &mut prepared)
                }
                .expect("prepared user fault mapping became invalid");
                assert!(previous.is_none(), "fault publication replaced a live leaf");
                let old = state.pages.remove(base);
                state.pages.push(page);
                old
            };
            sync_all();
            drop(mmap);
            drop(old);
            drop(prepared);
            return Ok(());
        }
    }

    /// 文件写准备退出所有 FS guard 后重新检查范围；最终遵守 mmap→invalidate→folio→PTE。
    /// WB 等待必须放开全部保护，来源变化返回 false 让 fault 按当前身份重新处理。
    fn mkwrite(&self, source: &FaultSource, address: usize) -> Result<bool, VmError> {
        let base = address & !(PAGE_SIZE - 1);
        let data = source
            .vma
            .file_range
            .as_ref()
            .expect("shared file has no range")
            .data();
        let offset = data
            .offset
            .checked_add((base - data.range.start) as u64)
            .ok_or(VmError::InvalidRange)?;
        let index = offset / PAGE_SIZE as u64;
        let preparation = (|| {
            if data.inode.metadata_snapshot().map_err(VmError::File)?.size <= offset {
                return Err(VmError::BeyondEof);
            }
            let prepared = data.inode.prepare_mkwrite(index).map_err(VmError::File)?;
            #[cfg(feature = "ktest")]
            {
                let hook = self.mkwrite_hook.lock().clone();
                if let Some(hook) = hook {
                    hook();
                }
            }
            let page = if source.observed.is_none() {
                let mut page = ResidentPage::try_new(
                    base,
                    ResidentBacking::File(prepared.folio().clone()),
                    false,
                )?;
                page.source = Arc::downgrade(data);
                Some(page)
            } else {
                None
            };
            let tables = PreparedTables::try_new(source.needed)?;
            Ok((prepared, page, tables))
        })();
        let mmap = self.mmap.write();
        let current = mmap.find(address).ok_or(VmError::Unmapped)?;
        if !current.allows(UserAccess::Write) {
            return Err(VmError::Permission);
        }
        if !Arc::ptr_eq(current, &source.vma) {
            drop(mmap);
            drop(preparation);
            return Ok(false);
        }
        {
            let state = self.table.lock();
            let page = state.pages.find(base);
            if page.is_some_and(|page| {
                page.source.as_ptr() == Arc::as_ptr(data)
                    && state.table.lookup(base).is_some_and(|leaf| {
                        leaf.physical == page.backing.physical() && leaf.permissions.write
                    })
            }) {
                return Ok(true);
            }
            if page.map(|page| page.backing.physical()) != source.observed
                || source.needed < state.table.needed_tables(base)
            {
                return Ok(false);
            }
        }
        let (prepared, mut page, mut tables) = preparation?;
        let invalidate = data.inode.lock_invalidate().map_err(VmError::File)?;
        let mut content = prepared.folio().lock_content();
        if content.writeback_sectors() != 0 {
            drop(content);
            drop(invalidate);
            drop(mmap);
            prepared.folio().wait_writeback();
            return Ok(false);
        }
        if !data
            .inode
            .finish_mkwrite(&prepared, &mut content)
            .map_err(VmError::File)?
        {
            return Ok(false);
        }
        {
            let mut state = self.table.lock();
            let physical = prepared.folio().phys_addr();
            if let Some(resident) = state.pages.find(base) {
                if resident.source.as_ptr() != Arc::as_ptr(data)
                    || resident.backing.physical() != physical
                    || !matches!(&resident.backing, ResidentBacking::File(_))
                {
                    return Ok(false);
                }
                // SAFETY: 同一短锁核实当前来源/PFN/叶；invalidate 和内容 guard 保准备事实，
                // finish 已先置真实 Dirty，同 PFN 写授权不改变拥有，锁外再同步 CPU。
                if !unsafe {
                    state
                        .table
                        .protect_leaf(base, physical, current.permissions)
                }? {
                    return Ok(false);
                }
            } else {
                if state.table.lookup(base).is_some() || source.observed.is_some() {
                    return Ok(false);
                }
                // SAFETY: prepared 的 canonical 原页已完整初始化并先标 Dirty；真实源登记
                // 在叶可见前存在，表页锁外准备，同一短锁发表硬件叶和驻留拥有。
                let previous = unsafe {
                    state
                        .table
                        .map(base, physical, current.permissions, &mut tables)
                }?;
                assert!(previous.is_none());
                state
                    .pages
                    .push(page.take().expect("new mkwrite page lacks prepared owner"));
            }
            #[cfg(feature = "ktest")]
            self.mkwrites.fetch_add(1, Ordering::Relaxed);
        }
        drop(content);
        drop(invalidate);
        drop(mmap);
        sync_all();
        Ok(true)
    }

    /// 受控准备停点只保当前 Mm 的 callback，替换与最后析构发生在短锁外。
    #[cfg(feature = "ktest")]
    pub fn ktest_set_mkwrite_hook(&self, hook: Option<Arc<dyn Fn() + Send + Sync>>) {
        let previous = core::mem::replace(&mut *self.mkwrite_hook.lock(), hook);
        drop(previous);
    }

    /// 观察真实 Dirty 后的 W 发表次数，不把 VMA 最大权限或诊断读取计入授权。
    #[cfg(feature = "ktest")]
    pub fn ktest_mkwrite_count(&self) -> usize {
        self.mkwrites.load(Ordering::Relaxed)
    }

    /// 取得锁外准备所需的具体身份和原页引用，权限命中不分配或等待文件。
    fn fault_source(
        &self,
        address: usize,
        access: UserAccess,
    ) -> Result<Option<FaultSource>, VmError> {
        let mmap = self.mmap.read();
        let vma = mmap.find(address).ok_or(VmError::Unmapped)?;
        if !vma.allows(access) {
            return Err(VmError::Permission);
        }
        let base = address & !(PAGE_SIZE - 1);
        let state = self.table.lock();
        let resident = state.pages.find(base);
        if let Some(page) = resident {
            if state.table.lookup(base).is_some_and(|mapping| {
                mapping.physical == page.backing.physical()
                    && vma::allows(mapping.permissions, access)
                    && !(page.cow && access == UserAccess::Write)
            }) {
                return Ok(None);
            }
        } else {
            assert!(
                state.table.lookup(base).is_none(),
                "leaf has no resident owner"
            );
        }
        Ok(Some(FaultSource {
            vma: Arc::clone(vma),
            backing: resident.map(|page| page.backing.clone()),
            observed: resident.map(|page| page.backing.physical()),
            needed: state.table.needed_tables(base),
        }))
    }

    /// 短页命中保持读侧及 PTE 拥有稳定；缺页/COW 返回 None，调用者须放锁后 fault。
    /// visit 不得分配、睡眠、调度、构造用户内容引用或保留可继续访问的原始地址。
    pub(crate) fn with_page<R>(
        &self,
        address: usize,
        access: UserAccess,
        visit: impl FnOnce(usize) -> R,
    ) -> Result<Option<R>, VmError> {
        let mmap = self.mmap.read();
        let vma = mmap.find(address).ok_or(VmError::Unmapped)?;
        if !vma.allows(access) {
            return Err(VmError::Permission);
        }
        let base = address & !(PAGE_SIZE - 1);
        let state = self.table.lock();
        let Some(page) = state.pages.find(base) else {
            return Ok(None);
        };
        if page.cow && access == UserAccess::Write {
            return Ok(None);
        }
        let Some(mapping) = state.table.lookup(base) else {
            return Ok(None);
        };
        if mapping.physical != page.backing.physical() || !vma::allows(mapping.permissions, access)
        {
            return Ok(None);
        }
        let direct = arch::phys_to_virt(mapping.physical) + (address - base);
        let result = visit(direct);
        drop(state);
        drop(mmap);
        Ok(Some(result))
    }

    /// 子 Mm 独立登记文件范围并以 RO 文件叶继承原页，父 COW/TLB 与执行排斥完成后返回。
    pub fn fork(&self) -> Result<Self, VmError> {
        let mut child = Self::try_new()?;
        child.executable = self.executable.clone();
        let mmap = self.mmap.write();
        let snapshot = mmap.snapshot(&child.table)?;
        let addresses = self.resident_addresses(&(0..arch::USER_ADDRESS_END))?;
        *child.mmap.write() = snapshot;
        for &address in &addresses {
            let vma = mmap.find(address).expect("resident page has no VMA");
            let (backing, cow) = {
                let state = self.table.lock();
                let page = state
                    .pages
                    .find(address)
                    .expect("fork resident disappeared");
                (page.backing.clone(), page.cow || vma.is_private())
            };
            let mut page = ResidentPage::try_new(address, backing, cow)?;
            {
                let child_mmap = child.mmap.read();
                if let Some(range) = &child_mmap
                    .find(address)
                    .expect("child VMA missing")
                    .file_range
                {
                    page.source = Arc::downgrade(range.data());
                }
            }
            let needed = child.table.lock().table.needed_tables(address);
            let mut prepared = PreparedTables::try_new(needed)?;
            {
                let mut state = child.table.lock();
                // SAFETY: child 尚不可见，父 mmap 写侧保证共享责任稳定；引用已克隆且
                // 完整初始化，子私有叶只读，表页锁外准备，同一短锁移入驻留拥有。
                let old = unsafe {
                    state.table.map(
                        address,
                        page.backing.physical(),
                        page.permissions(vma.permissions),
                        &mut prepared,
                    )
                }?;
                assert!(old.is_none());
                state.pages.push(page);
            }
            drop(prepared);
        }
        for &address in &addresses {
            let vma = mmap.find(address).expect("fork resident has no VMA");
            if !vma.is_private() {
                continue;
            }
            let mut state = self.table.lock();
            let Some(page) = state.pages.find_mut(address) else {
                continue;
            };
            page.cow = true;
            let physical = page.backing.physical();
            let permissions = page.permissions(vma.permissions);
            // SAFETY: 此短锁核实当次驻留 PFN，父子引用保活原页；同 PFN 只撤 W，
            // 子尚未发表，全部 CPU 的旧父翻译在子可运行前同步失效。
            unsafe { state.table.protect_leaf(address, physical, permissions) }
                .expect("fork protection is not representable");
        }
        sync_all();
        drop(mmap);
        Ok(child)
    }

    /// 按最大权限预检并发表独立范围登记，同 PFN 改权保 COW，shared file 从不直接升 W。
    pub fn protect(
        &self,
        address: usize,
        length: usize,
        permissions: PagePermissions,
    ) -> Result<(), VmError> {
        let range = vma::checked_range(address, length)?;
        let mut mmap = self.mmap.write();
        mmap.check_protection(&range, permissions)?;
        let vmas = mmap.rewrite(&range, Some(permissions))?;
        let addresses = self.resident_addresses(&range)?;
        let old_vmas = self.publish_vmas(&mut mmap, vmas)?;
        for &address in &addresses {
            let vma = mmap.find(address).expect("protected resident has no VMA");
            let mut state = self.table.lock();
            let Some(page) = state.pages.find(address) else {
                continue;
            };
            let physical = page.backing.physical();
            let permissions = page.permissions(vma.permissions);
            // SAFETY: 当次驻留与实际叶在同一 PTE 锁核 PFN；旧页保持拥有，
            // shared file 许可仍 RO，改权在所有短锁外完成真实全 CPU 失效。
            unsafe { state.table.protect_leaf(address, physical, permissions) }
                .expect("protection is not representable");
        }
        if !addresses.is_empty() {
            sync_all();
        }
        drop(mmap);
        drop(old_vmas);
        Ok(())
    }

    /// 解除实际叶及空中间表，全部在线 CPU 的失效完成后才归还旧数据页和表页。
    pub fn unmap(&self, address: usize, length: usize) -> Result<(), VmError> {
        let range = vma::checked_range(address, length)?;
        let mut mmap = self.mmap.write();
        let retired = self.replace_range(&mut mmap, &range, None)?;
        drop(mmap);
        drop(retired);
        Ok(())
    }

    /// 新映射先准备完整索引，合并可能改变相邻文件来源，发表统一更新所有驻留来源。
    fn insert_vma(&self, mmap: &mut MmapState, vma: Arc<Vma>) -> Result<RetiredVmas, VmError> {
        if !mmap.vacant(&vma.range) {
            return Err(VmError::Occupied);
        }
        let range = vma.range.clone();
        let vmas = mmap.replacement(&range, Some(vma))?;
        self.publish_vmas(mmap, vmas)
    }

    /// mmap 写侧统一发表新范围身份；PTE 锁只移出 Weak，最后释放均交回锁外调用者。
    fn publish_vmas(
        &self,
        mmap: &mut MmapState,
        vmas: Vec<Arc<Vma>>,
    ) -> Result<RetiredVmas, VmError> {
        let count = {
            let state = self.table.lock();
            let mut cursor = state.pages.head.as_deref();
            let mut count = 0;
            while let Some(page) = cursor {
                let after = vmas.partition_point(|vma| vma.range.start <= page.address);
                let next = after
                    .checked_sub(1)
                    .and_then(|index| vmas.get(index))
                    .filter(|vma| page.address < vma.range.end)
                    .and_then(|vma| vma.file_range.as_ref());
                let pointer =
                    next.map_or_else(|| Weak::new().as_ptr(), |range| Arc::as_ptr(range.data()));
                count += usize::from(page.source.as_ptr() != pointer);
                cursor = page.next.as_deref();
            }
            count
        };
        let mut sources = Vec::new();
        sources
            .try_reserve_exact(count)
            .map_err(|_| VmError::NoMemory)?;
        let old = core::mem::replace(&mut mmap.vmas, vmas);
        let mut changed = false;
        {
            let mut state = self.table.lock();
            let PteState { table, pages } = &mut *state;
            let mut cursor = pages.head.as_deref_mut();
            while let Some(page) = cursor {
                let next = mmap
                    .find(page.address)
                    .and_then(|vma| vma.file_range.as_ref());
                let pointer =
                    next.map_or_else(|| Weak::new().as_ptr(), |range| Arc::as_ptr(range.data()));
                if page.source.as_ptr() != pointer {
                    if matches!(&page.backing, ResidentBacking::File(_)) {
                        // SAFETY: 此短锁核实驻留 PFN；新来源身份不能继承旧共享 W，
                        // 旧页一直保活，同 PFN 撤权后在短锁外同步全部 CPU。
                        changed |= unsafe { table.mkclean(page.address, page.backing.physical()) }
                            .is_some_and(|(_, changed)| changed);
                    }
                    let next = next.map_or_else(Weak::new, |range| Arc::downgrade(range.data()));
                    let previous = core::mem::replace(&mut page.source, next);
                    assert!(sources.len() < sources.capacity());
                    sources.push(previous);
                }
                cursor = page.next.as_deref_mut();
            }
        }
        if changed {
            sync_all();
        }
        Ok((old, sources))
    }
    /// 写侧先备齐候选索引及退休容器，再撤叶并同步所有 CPU；旧拥有交回调用者锁外释放。
    fn replace_range(
        &self,
        mmap: &mut MmapState,
        range: &Range<usize>,
        replacement: Option<Arc<Vma>>,
    ) -> Result<(RetiredVmas, ResidentPages, Vec<RetiredTables>), VmError> {
        let vmas = mmap.replacement(range, replacement)?;
        let addresses = self.resident_addresses(range)?;
        let mut tables = Vec::new();
        tables
            .try_reserve_exact(addresses.len())
            .map_err(|_| VmError::NoMemory)?;
        let mut discarded = ResidentPages::default();
        let old_vmas = self.publish_vmas(mmap, vmas)?;
        for address in addresses.iter().copied() {
            let retired = {
                let mut state = self.table.lock();
                let page = state
                    .pages
                    .remove(address)
                    .expect("unmap resident disappeared");
                // SAFETY: 写侧阻止叶与目录提交，具体退休链保活全部旧页；
                // 摘取表页仅移入已足额预留的容器，归还晚于全部 CPU 翻译与取指失效。
                let (old, retired) = unsafe { state.table.unmap(address) };
                assert_eq!(old.unwrap().physical, page.backing.physical());
                discarded.push(page);
                retired
            };
            tables.push(retired);
        }
        if !addresses.is_empty() {
            sync_all();
        }
        Ok((old_vmas, discarded, tables))
    }

    /// 写侧保证驻留数量稳定，在 PTE 锁外预留容器，短锁只复制实际 VA 标识。
    fn resident_addresses(&self, range: &Range<usize>) -> Result<Vec<usize>, VmError> {
        let count = self.table.lock().pages.count(range);
        let mut addresses = Vec::new();
        addresses
            .try_reserve_exact(count)
            .map_err(|_| VmError::NoMemory)?;
        self.table.lock().pages.addresses(range, &mut addresses);
        Ok(addresses)
    }

    /// 返回非拥有的硬件叶快照，仅用于诊断；不能凭物理整数继续访问或归页。
    pub fn mapping(&self, address: usize) -> Option<Mapping> {
        self.table.lock().table.lookup(address)
    }

    /// 记录执行映像确定的堆起点与当前 byte break，不替代实际 VMA 映射操作。
    pub(crate) fn set_heap(&self, start: usize, end: usize) -> Result<(), VmError> {
        if end < start || arch::USER_ADDRESS_END < end {
            return Err(VmError::InvalidRange);
        }
        let mut mmap = self.mmap.write();
        mmap.heap_start = start;
        mmap.heap_end = end;
        Ok(())
    }

    /// 返回 mmap 读侧保护的实际堆边界，地址单位为字节。
    pub(crate) fn heap(&self) -> (usize, usize) {
        let mmap = self.mmap.read();
        (mmap.heap_start, mmap.heap_end)
    }

    /// 返回由本对象保活的根物理地址，不移交页表或叶页的访问权。
    pub(crate) fn root_phys(&self) -> usize {
        self.table.lock().table.root_phys()
    }
}

/// 连同具体驻留节点和缺失目录完成锁外准备，准备失败也须先重查当前 VMA/PTE。
fn prepare_fault(
    source: &FaultSource,
    base: usize,
    access: UserAccess,
) -> Result<(Box<ResidentPage>, PreparedTables), VmError> {
    let (backing, cow) = prepare_backing(source, base, access)?;
    let mut page = ResidentPage::try_new(base, backing, cow)?;
    if let Some(range) = &source.vma.file_range {
        page.source = Arc::downgrade(range.data());
    }
    let tables = PreparedTables::try_new(source.needed)?;
    Ok((page, tables))
}

/// 锁外准备具体驻留拥有；共享文件尾页保 canonical folio，私有 COW 不按引用数猜独占性。
fn prepare_backing(
    source: &FaultSource,
    base: usize,
    access: UserAccess,
) -> Result<(ResidentBacking, bool), VmError> {
    if let Some(backing) = &source.backing {
        // ponytail: 私有 COW 总复制一页；有实际映射独占证明后才考虑原页复用。
        let page = AnonPageRef::try_new().ok_or(VmError::NoMemory)?;
        match backing {
            ResidentBacking::Anonymous(source) => {
                // SAFETY: source 克隆保活完整初始化原页，旧 COW 叶无硬件写权限；
                // 原始汇编不建立源引用。新匿名页尚未映射或共享，独占接收复制，
                // 原页仍被 source 持有，分配器返回的新页与其物理范围不重叠。
                unsafe {
                    arch::copy_user_bytes(
                        page.direct_address() as *mut u8,
                        source.direct_address() as *const u8,
                        PAGE_SIZE,
                    )
                };
            }
            ResidentBacking::File(folio) => {
                // SAFETY: 新匿名页已完整清零，只有本地 page 引用且尚未映射或交给设备；
                // folio 保活另一张真实原页，分配器不能返回仍被持有的源页。
                // 短切片只独占匿名目标，copy_to 从已初始化源原始读取，不形成源切片。
                let dst = unsafe {
                    slice::from_raw_parts_mut(page.direct_address() as *mut u8, PAGE_SIZE)
                };
                folio
                    .copy_to(0, dst)
                    .map_err(|error| VmError::File(error.into()))?;
            }
        }
        return Ok((ResidentBacking::Anonymous(page), false));
    }
    match &source.vma.backing {
        VmaBacking::Anonymous => Ok((
            ResidentBacking::Anonymous(AnonPageRef::try_new().ok_or(VmError::NoMemory)?),
            false,
        )),
        VmaBacking::SharedAnonymous { owner, offset } => {
            let offset = offset
                .checked_add((base - source.vma.range.start) as u64)
                .ok_or(VmError::InvalidRange)?;
            Ok((
                ResidentBacking::Anonymous(owner.page(offset / PAGE_SIZE as u64)?),
                false,
            ))
        }
        VmaBacking::File {
            file,
            offset,
            private,
        } => {
            let offset = offset
                .checked_add((base - source.vma.range.start) as u64)
                .ok_or(VmError::InvalidRange)?;
            let size = file.stat().map_err(VmError::File)?.size;
            if size <= offset {
                return Err(VmError::BeyondEof);
            }
            let valid = (size - offset).min(PAGE_SIZE as u64) as usize;
            let folio = file
                .file_page(offset / PAGE_SIZE as u64)
                .map_err(VmError::File)?;
            if (!*private || valid == PAGE_SIZE) && access != UserAccess::Write {
                folio
                    .lock_content()
                    .range_ready(0, PAGE_SIZE)
                    .map_err(|error| VmError::File(error.into()))?;
                return Ok((ResidentBacking::File(folio), *private));
            }
            let page = AnonPageRef::try_new().ok_or(VmError::NoMemory)?;
            // SAFETY: 新匿名页已完整清零，只有本地 page 引用，尚无映射、设备或其他访问；
            // valid 不超过 PAGE_SIZE，folio 保活源原页，分配器返回的目标与其不重叠。
            // 仅匿名目标形成短独占切片，copy_to 原始读取有效前缀，目标尾部保留零初始化。
            let dst = unsafe { slice::from_raw_parts_mut(page.direct_address() as *mut u8, valid) };
            folio
                .copy_to(0, dst)
                .map_err(|error| VmError::File(error.into()))?;
            Ok((ResidentBacking::Anonymous(page), false))
        }
    }
}

/// 文件映射先核真实 OFD 读取能力；共享最大 W 还要求 RW OFD 与完整路径写能力。
fn check_file(
    file: &FileRef,
    offset: u64,
    length: usize,
    private: bool,
    permissions: PagePermissions,
    maximum: PagePermissions,
) -> Result<(), VmError> {
    if file.flags() & 3 == 1 {
        return Err(VmError::File(FsError::BadFd));
    }
    if offset & (PAGE_SIZE as u64 - 1) != 0 || offset.checked_add(length as u64).is_none() {
        return Err(VmError::InvalidRange);
    }
    if !private && (permissions.write || maximum.write) {
        if file.flags() & 3 != 2 {
            return Err(VmError::Permission);
        }
        file.path().check_writable().map_err(VmError::File)?;
    }
    if file.stat().map_err(VmError::File)?.kind != InodeKind::Regular {
        return Err(VmError::File(FsError::IsDirectory));
    }
    Ok(())
}

/// 将字节边界 checked 上取整到基本页，用户长度和 raw break 共用此纯计算。
fn page_end(address: usize) -> Result<usize, VmError> {
    address
        .checked_add(PAGE_SIZE - 1)
        .map(|end| end & !(PAGE_SIZE - 1))
        .ok_or(VmError::InvalidRange)
}

/// 尚未共享的旧单页入口保留原页表错误类别，不吞掉已经发表范围的错误。
fn table_error(error: VmError) -> PageTableError {
    match error {
        VmError::NoMemory | VmError::File(FsError::NoMemory) => PageTableError::NoMemory,
        VmError::Occupied => PageTableError::Occupied,
        _ => PageTableError::InvalidRange,
    }
}

/// CSD 只执行本核架构失效，不反取 Mm、PTE 或文件锁。
///
/// # Safety
/// 只能由在线 CPU 的同步 call 分派；参数不被读取，调用者保活旧页与表页到返回。
unsafe fn invalidate_cpu(_: *mut ()) {
    arch::flush_user_translations();
    arch::sync_user_icache();
}

/// 叶修改在 PTE/范围短锁外同步所有在线 CPU，固定 ASID0 不需要活动核代次集合。
pub(crate) fn sync_all() {
    for target in 0..cpu::count() {
        // SAFETY: CPU 在线集合启动后固定，普通可切换任务能服务本地 IPI；
        // 未持 PTE 或其他 callback 锁，回调只做架构动作，不读取空参数或递归 call。
        unsafe { cpu::call(target, invalidate_cpu, core::ptr::null_mut()) };
    }
}

/// 在所属CPU的IRQ-off调度交接或exec提交中安装根，硬件使用期间保留CPU拥有。
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
    // SAFETY: 只有所属CPU在IRQ-off交接或exec中访问槽，原用户现场已完整保存。
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
