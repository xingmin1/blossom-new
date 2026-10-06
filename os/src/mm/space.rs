//! 有序用户映射、真实驻留页及硬件页表拥有；改变翻译的提交在 mmap 写侧串行。

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ops::Range;

use crate::arch::{self, UserAccess};
use crate::cpu;
use crate::fs::{FileRef, FsError, InodeKind};
use crate::mm::user_table::{
    Mapping, PagePermissions, PageTableError, PreparedTables, UserPageTable,
};
use crate::mm::vma::{self, MmapState, Vma, VmaBacking};
use crate::mm::{AnonPageRef, FolioRef, OwnedPages, PAGE_SIZE};
use crate::sync::{IrqSaveGuard, SleepRwLock, SpinLock};

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

/// 所有内容已初始化的驻留原页；文件页只允许硬件只读或执行，私有写先复制匿名页。
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
    next: Option<Box<ResidentPage>>,
}

impl ResidentPage {
    /// 在 PTE 锁外准备具体驻留拥有节点，失败只释放本次未发表引用。
    fn try_new(address: usize, backing: ResidentBacking, cow: bool) -> Result<Box<Self>, VmError> {
        Box::try_new(Self {
            address,
            backing,
            cow,
            next: None,
        })
        .map_err(|_| VmError::NoMemory)
    }

    /// COW 与文件只读页的有效硬件权限不能绕过软件写准备。
    fn permissions(&self, permissions: PagePermissions) -> PagePermissions {
        PagePermissions {
            write: permissions.write && !self.cow,
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

struct FaultSource {
    vma: Arc<Vma>,
    backing: Option<ResidentBacking>,
    observed: Option<usize>,
    needed: usize,
}

/// 同一 Mm 的范围读写锁与 PTE 短锁分责；活动根由 CPU 强拥有到切回内核根后。
pub struct Mm {
    table: SpinLock<PteState>,
    mmap: SleepRwLock<MmapState>,
}

impl Mm {
    /// 在普通任务上下文准备独立页表，尚未发表给执行者或硬件。
    pub fn try_new() -> Result<Self, PageTableError> {
        Ok(Self {
            table: SpinLock::new(PteState {
                table: UserPageTable::try_new()?,
                pages: ResidentPages::default(),
            }),
            mmap: SleepRwLock::new(MmapState::default()),
        })
    }

    /// 在尚未共享的地址空间安装完整初始化单页，并转交匿名页的真实共享归还责任。
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
        let range = vma::checked_range(address, PAGE_SIZE).map_err(table_error)?;
        let vma = Vma::try_new(range, permissions, permissions, VmaBacking::Anonymous)
            .map_err(table_error)?;
        // SAFETY: 调用者保证完整初始化的独占 order0 页，无其他映射、设备或内容借用；
        // 此处消费原归还权，后续访问仅沿本 Mm 的原始访问和硬件权限协议。
        let backing = unsafe { AnonPageRef::from_owned(pages) };
        let page = ResidentPage::try_new(address, ResidentBacking::Anonymous(backing), false)
            .map_err(table_error)?;
        let needed = self.table.lock().table.needed_tables(address);
        let mut prepared = PreparedTables::try_new(needed)?;
        let mut mmap = self.mmap.write();
        mmap.insert(vma).map_err(table_error)?;
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
        let vma = Vma::try_new(range, permissions, maximum, VmaBacking::Anonymous)?;
        self.mmap.write().insert(vma)
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
        if offset & (PAGE_SIZE as u64 - 1) != 0 || offset.checked_add(length as u64).is_none() {
            return Err(VmError::InvalidRange);
        }
        if !private && (permissions.write || maximum.write) {
            return Err(VmError::File(FsError::ReadOnly));
        }
        if file.stat().map_err(VmError::File)?.kind != InodeKind::Regular {
            return Err(VmError::File(FsError::IsDirectory));
        }
        let vma = Vma::try_new(
            range,
            permissions,
            maximum,
            VmaBacking::File {
                file,
                offset,
                private,
            },
        )?;
        self.mmap.write().insert(vma)
    }

    /// 按当前实际 VMA 与方向解决缺页或 COW；分配、读盘及原始复制均在映射锁外。
    pub fn fault(&self, address: usize, access: UserAccess) -> Result<(), VmError> {
        let base = address & !(PAGE_SIZE - 1);
        loop {
            let Some(source) = self.fault_source(address, access)? else {
                return Ok(());
            };
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
            {
                let state = self.table.lock();
                let resident = state.pages.find(base);
                if resident.is_some_and(|resident| !(resident.cow && access == UserAccess::Write)) {
                    drop(state);
                    drop(mmap);
                    drop(preparation);
                    return Ok(());
                }
                if resident.map(|page| page.backing.physical()) != source.observed
                    || source.needed < state.table.needed_tables(base)
                {
                    drop(state);
                    drop(mmap);
                    drop(preparation);
                    continue;
                }
            }
            let (page, mut prepared) = preparation?;
            let replace = source.observed.is_some();
            if replace {
                let mut state = self.table.lock();
                // SAFETY: mmap 写侧串行最终提交，驻留节点及 source 克隆保活旧页；
                // 中间表保留，清空旧叶后先在短锁外完成全部 CPU 的失效。
                let old =
                    unsafe { state.table.clear_leaf(base) }.expect("COW page has no hardware leaf");
                assert_eq!(Some(old.physical), source.observed);
            }
            sync_all();
            let old = {
                let mut state = self.table.lock();
                let permissions = page.permissions(current.permissions);
                // SAFETY: 新页已完整初始化，旧叶已解除且同步失效完成；mmap 写侧阻止
                // 其他提交插入，中间表锁外准备，新旧驻留引用均保活至后续同步完成。
                let previous = unsafe {
                    state
                        .table
                        .map(base, page.backing.physical(), permissions, &mut prepared)
                }
                .expect("prepared user fault mapping became invalid");
                assert!(previous.is_none(), "fault publication replaced a live leaf");
                let old = if replace {
                    state.pages.remove(base)
                } else {
                    None
                };
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
            let mapping = state.table.lookup(base).expect("resident page has no leaf");
            assert_eq!(mapping.physical, page.backing.physical());
            if !(page.cow && access == UserAccess::Write) {
                assert!(vma::allows(mapping.permissions, access));
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
        let mapping = state.table.lookup(base).expect("resident page has no leaf");
        assert_eq!(mapping.physical, page.backing.physical());
        assert!(vma::allows(mapping.permissions, access));
        let direct = arch::phys_to_virt(mapping.physical) + (address - base);
        let result = visit(direct);
        drop(state);
        drop(mmap);
        Ok(Some(result))
    }

    /// 新 Mm 未发表前完成全部 fallible 准备，父私有写保护与全在线失效结束后才返回。
    pub fn fork(&self) -> Result<Self, VmError> {
        let child = Self::try_new()?;
        let mmap = self.mmap.write();
        let snapshot = mmap.snapshot()?;
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
            let page = ResidentPage::try_new(address, backing, cow)?;
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
        let mut changed = false;
        for &address in &addresses {
            if !mmap.find(address).unwrap().is_private() {
                continue;
            }
            let mut state = self.table.lock();
            state.pages.find_mut(address).unwrap().cow = true;
            // SAFETY: 父写侧串行提交，父子驻留引用都保活原页；中间表保留，
            // 子尚未发表，全部旧父翻译须在子可运行前失效。
            let old = unsafe { state.table.clear_leaf(address) };
            assert!(old.is_some());
            changed = true;
        }
        if changed {
            sync_all();
            for &address in &addresses {
                let vma = mmap.find(address).unwrap();
                if !vma.is_private() {
                    continue;
                }
                let mut state = self.table.lock();
                let page = state.pages.find(address).unwrap();
                let physical = page.backing.physical();
                let permissions = page.permissions(vma.permissions);
                // SAFETY: 已完成旧叶失效，中间表未退休，父子引用保活数据；
                // 写侧仍排除其他提交，零准备对象只用于已有完整目录。
                let old = unsafe {
                    state.table.map(
                        address,
                        physical,
                        permissions,
                        &mut PreparedTables::default(),
                    )
                }
                .expect("fork write protection changed its table path");
                assert!(old.is_none());
            }
        }
        sync_all();
        drop(mmap);
        Ok(child)
    }

    /// 按最大权限预检完整区间，拆分后保留 COW 性质，再以先撤叶后失效的顺序改权限。
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
        let old_vmas = core::mem::replace(&mut mmap.vmas, vmas);
        for &address in &addresses {
            let mut state = self.table.lock();
            // SAFETY: 写侧串行全部最终修改，驻留拥有不移出，中间表保留；
            // 改权限前先清旧叶，失效在短锁外执行。
            assert!(unsafe { state.table.clear_leaf(address) }.is_some());
        }
        if !addresses.is_empty() {
            sync_all();
            for &address in &addresses {
                let vma = mmap.find(address).unwrap();
                let mut state = self.table.lock();
                let page = state.pages.find(address).unwrap();
                let physical = page.backing.physical();
                let permissions = page.permissions(vma.permissions);
                // SAFETY: 旧翻译已同步失效，目录保持 live，当前身份及 COW 权限在
                // 同一写侧稳定；完整驻留页在解除并再次失效前始终保活。
                let old = unsafe {
                    state.table.map(
                        address,
                        physical,
                        permissions,
                        &mut PreparedTables::default(),
                    )
                }
                .expect("protection changed its prepared table path");
                assert!(old.is_none());
            }
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
        let vmas = mmap.rewrite(&range, None)?;
        let addresses = self.resident_addresses(&range)?;
        let mut tables = Vec::new();
        tables
            .try_reserve_exact(addresses.len())
            .map_err(|_| VmError::NoMemory)?;
        let mut discarded = ResidentPages::default();
        let old_vmas = core::mem::replace(&mut mmap.vmas, vmas);
        for address in addresses.iter().copied() {
            let retired = {
                let mut state = self.table.lock();
                let page = state
                    .pages
                    .remove(address)
                    .expect("unmap resident disappeared");
                // SAFETY: 写侧阻止新的叶或目录提交，page 移入本次具体退休链保活；
                // 返回表页只在全部 CPU 失效后短锁外析构。
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
        drop(mmap);
        drop(discarded);
        drop(tables);
        drop(old_vmas);
        Ok(())
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
    let page = ResidentPage::try_new(base, backing, cow)?;
    let tables = PreparedTables::try_new(source.needed)?;
    Ok((page, tables))
}

/// 锁外按具体源准备新页；COW 总复制，不用真实引用数快照推断映射独占性。
fn prepare_backing(
    source: &FaultSource,
    base: usize,
    access: UserAccess,
) -> Result<(ResidentBacking, bool), VmError> {
    if let Some(backing) = &source.backing {
        // ponytail: 私有 COW 总复制一页；有实际映射独占证明后才考虑原页复用。
        let page = AnonPageRef::try_new().ok_or(VmError::NoMemory)?;
        // SAFETY: source 克隆保活完整初始化原页，旧 COW 叶无硬件写权限；原始汇编允许
        // 用户并发原始访问，不建立引用。新页尚未映射，独占初始化，两页物理范围不重叠。
        unsafe {
            arch::copy_user_bytes(
                page.direct_address() as *mut u8,
                arch::phys_to_virt(backing.physical()) as *const u8,
                PAGE_SIZE,
            )
        };
        return Ok((ResidentBacking::Anonymous(page), false));
    }
    match &source.vma.backing {
        VmaBacking::Anonymous => Ok((
            ResidentBacking::Anonymous(AnonPageRef::try_new().ok_or(VmError::NoMemory)?),
            false,
        )),
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
            if valid == PAGE_SIZE && access != UserAccess::Write {
                folio
                    .with_bytes(0, PAGE_SIZE, |_| ())
                    .map_err(|error| VmError::File(error.into()))?;
                return Ok((ResidentBacking::File(folio), *private));
            }
            let page = AnonPageRef::try_new().ok_or(VmError::NoMemory)?;
            folio
                .with_bytes(0, valid, |bytes| {
                    // SAFETY: folio 完成后只读且本引用保活，bytes 已检查完整成功初始化范围；
                    // 新匿名页清零且尚未映射，独占初始化，两个真实原页不重叠。
                    unsafe {
                        arch::copy_user_bytes(
                            page.direct_address() as *mut u8,
                            bytes.as_ptr(),
                            bytes.len(),
                        )
                    };
                })
                .map_err(|error| VmError::File(error.into()))?;
            Ok((ResidentBacking::Anonymous(page), false))
        }
    }
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

/// mmap 写侧在 PTE 锁外同步所有在线 CPU，固定 ASID0 不需要活动核代次集合。
fn sync_all() {
    for target in 0..cpu::count() {
        // SAFETY: CPU 在线集合启动后固定，普通可切换任务能服务本地 IPI；
        // 未持 PTE 或其他 callback 锁，回调只做架构动作，不读取空参数或递归 call。
        unsafe { cpu::call(target, invalidate_cpu, core::ptr::null_mut()) };
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
