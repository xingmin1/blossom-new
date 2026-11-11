//! 用户虚拟内存区域（VMA, Virtual Memory Area）与地址区间索引管理。
//!
//! # 模块职责与架构定位
//! 本模块定义用户进程地址空间中的连续虚拟内存区域（[`Vma`]）及其有序集合（[`MmapState`]）。
//! 上游由 [`crate::mm::space::Mm`] 在持有地址空间睡眠读写锁（`mmap: SleepRwLock<MmapState>`）时调用；
//! 下游关联共享匿名页对象（[`SharedAnon`]）、虚拟文件系统打开实例与索引节点（[`FileRef`] / [`InodeRef`]），
//! 以及页缓存的文件映射反向映射登记（[`FileRangeRegistration`]）。
//!
//! # 核心设计与不可变 VMA 身份
//! 已发布到 [`MmapState`] 中的 [`Vma`] 采用**不可变身份（Immutable Identity）**设计，通过 `Arc<Vma>` 共享：
//! 1. **锁外缺页准备与重查**：缺页处理（Page Fault）先在读锁下克隆 `Arc<Vma>`，释放锁后执行磁盘读取或物理页分配，
//!    最后重新获取写锁并用 `Arc::ptr_eq` 比对当前地址处的 VMA 是否仍是同一个 `Arc<Vma>` 实例。
//! 2. **写时复制式的区间变更**：拆分（`slice`）、合并（`coalesce`）、修改权限（`rewrite`）或覆盖映射（`replacement`）
//!    不原地修改已发布的区间属性；仅最后一个真实用户退出时，无分配地清除锁页标志。
//!    每段候选构造失败时保留该段旧状态，跨段锁页操作由调用者逐段提交。
//!
//! ```text
//! 低地址                                                                    高地址
//! +------------------+------------------+       +------------------+-------------+
//! | 普通 VMA (代码/堆) |   未映射空闲区   | ...   | 栈防护间隙 (256页) | 栈 VMA (↓) |
//! +------------------+------------------+       +------------------+-------------+
//!                                               ^                  ^
//!                                        vma.start_gap()    vma.range.start
//! ```
//!
//! # 锁获取顺序与并发契约
//! - [`MmapState`] 本身不包含锁，由所属 `Mm` 的 `mmap` 睡眠读写锁保护。
//! - 构造文件后备 `Vma` 时，[`FileRangeRegistration::try_new`] 会在内部获取该文件 `AddressSpace` 的范围自旋锁；
//!   因此调用本模块构造或销毁 `Vma` 时，**严禁持有页表自旋锁（`PteDomain`）**，必须遵守 `mmap 睡眠锁 -> 文件范围自旋锁 -> PTE 自旋锁` 的全局层级。

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ops::Range;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::arch::{self, UserAccess};
use crate::cache::FileRangeRegistration;
use crate::fs::FileRef;
use crate::fs::InodeRef;
use crate::mm::anon::SharedAnon;
use crate::mm::shm::ShmAttachment;
use crate::mm::space::{PteDomain, VmError};
use crate::mm::user_table::PagePermissions;
use crate::mm::PAGE_SIZE;

/// 向下增长的用户栈 VMA 与其低地址方向相邻可访问 VMA 之间保留的防护间隙大小（256 个物理页，4 KiB 页下为 1 MiB）。
///
/// 该间隙用于防止自动选址（`mmap` 传 `Hint`）、堆扩展（`brk`）或栈自动向下增长直接撞上相邻映射区。
const STACK_GUARD_GAP: usize = 256 * PAGE_SIZE;

/// 虚拟内存区域的后备存储类型（Backing Store）。
///
/// 文件映射直接持有完整的文件打开实例（[`FileRef`]），因此用户态关闭文件描述符（`close(fd)`）不会使已建立的映射失效；
/// 私有映射（含私有匿名与私有文件 COW 副本）实际分配的匿名物理页所有权保存在页表层的驻留页链表中。
#[derive(Clone)]
pub(crate) enum VmaBacking {
    /// 私有匿名映射（`MAP_PRIVATE | MAP_ANONYMOUS`），缺页时按需分配零初始化物理页。
    Anonymous,
    /// 共享匿名映射（`MAP_SHARED | MAP_ANONYMOUS`），由跨进程共享的匿名页容器提供物理页。
    SharedAnonymous {
        /// 共享匿名页集合对象，按页索引保管物理页。
        owner: Arc<SharedAnon>,
        /// 当前 `Vma` 起始虚拟地址（`range.start`）在 `owner` 中的起始字节偏移（页大小对齐）。
        offset: u64,
    },
    /// 文件后备映射（`MAP_SHARED` 或 `MAP_PRIVATE` 文件映射）。
    File {
        /// 建立映射时的文件打开实例，用于保持底层文件系统引用及后续 `msync` 回写。
        file: FileRef,
        /// 被映射文件的索引节点引用，用于缺页时读取页缓存（Page Cache）及校验文件大小。
        inode: InodeRef,
        /// 当前 `Vma` 起始虚拟地址（`range.start`）对应的文件内起始字节偏移（页大小对齐）。
        offset: u64,
        /// 是否为私有文件映射（`MAP_PRIVATE`）：为 `true` 时写入触发写时复制（COW），不回写底层文件；为 `false` 时为共享文件映射。
        private: bool,
    },
}

/// 一段连续用户虚拟地址空间的内存区域描述符（Virtual Memory Area）。
///
/// 区间拆分、合并、权限或锁页修改均创建新的实例；仅最后一个真实用户退出时原地清锁。
/// 文件锁页标志保存在文件范围登记中，供回收器在当前来源和 PFN 验证后读取。
pub(crate) struct Vma {
    /// 虚拟地址半开区间 `[start, end)`，单位为字节，两端均按 [`PAGE_SIZE`] 对齐。
    pub(crate) range: Range<usize>,
    /// 当前生效的页面访问权限（读、写、执行）。
    pub(crate) permissions: PagePermissions,
    /// 允许通过 `mprotect` 提升到的最大页面访问权限上限（例如只读打开的文件映射禁止将最大写权限设为 `true`）。
    pub(crate) maximum: PagePermissions,
    /// 是否为允许随缺页自动向低地址方向扩展的用户栈区域（`MAP_GROWSDOWN`）。
    pub(crate) growdown: bool,
    /// 该虚拟内存区域的后备数据源。
    pub(crate) backing: VmaBacking,
    /// 所属 Mm 中独立 shmat 连接的编号；VMA 快照和区间切片不承担连接计数。
    pub(crate) shm_attachment: Option<usize>,
    /// 当后备源为文件时，在文件 `AddressSpace` 中注册的反向映射范围句柄；销毁时自动从文件反向映射表中注销。
    pub(crate) file_range: Option<FileRangeRegistration>,
    /// 非文件映射的锁页标志；文件映射使用 `file_range` 内的同类标志。
    anonymous_locked: AtomicBool,
}

impl Vma {
    /// 在普通可睡眠任务上下文中构造一个非向下增长的普通 [`Vma`] 实例。
    ///
    /// 若权限不合法、超出 `maximum` 上限或内存分配失败，直接返回错误且不改变现有地址空间状态。
    pub(crate) fn try_new(
        range: Range<usize>,
        permissions: PagePermissions,
        maximum: PagePermissions,
        backing: VmaBacking,
        domain: Option<&Arc<PteDomain>>,
    ) -> Result<Arc<Self>, VmError> {
        Self::try_new_with_growth(
            range,
            permissions,
            maximum,
            backing,
            domain,
            false,
            None,
            false,
        )
    }

    /// 构造一个允许向低地址方向自动扩展的用户栈 [`Vma`]（后备类型为私有匿名映射）。
    ///
    /// 初始仅登记已准备好的实际栈范围；运行期触发缺页向下扩展时，再结合当前线程的 `RLIMIT_STACK` 软限额动态裁决。
    pub(crate) fn try_stack(
        range: Range<usize>,
        permissions: PagePermissions,
        maximum: PagePermissions,
    ) -> Result<Arc<Self>, VmError> {
        Self::try_new_with_growth(
            range,
            permissions,
            maximum,
            VmaBacking::Anonymous,
            None,
            true,
            None,
            false,
        )
    }

    /// 为已经预留的独立共享段连接创建 VMA，物理页继续由段内的 SharedAnon 提供。
    pub(crate) fn try_shm(
        range: Range<usize>,
        permissions: PagePermissions,
        attachment: &ShmAttachment,
    ) -> Result<Arc<Self>, VmError> {
        Self::try_new_with_growth(
            range,
            permissions,
            PagePermissions {
                read: true,
                write: permissions.write,
                execute: true,
            },
            VmaBacking::SharedAnonymous {
                owner: Arc::clone(&attachment.segment.owner),
                offset: 0,
            },
            None,
            false,
            Some(attachment.identity),
            false,
        )
    }

    /// 内部通用构造函数：校验权限合法性与上限约束，为文件映射注册反向映射条目，并分配 `Arc<Vma>`。
    #[expect(
        clippy::too_many_arguments,
        reason = "VMA 构造显式保留权限、来源、增长、共享段归属和锁页状态，避免另造参数对象"
    )]
    fn try_new_with_growth(
        range: Range<usize>,
        permissions: PagePermissions,
        maximum: PagePermissions,
        backing: VmaBacking,
        domain: Option<&Arc<PteDomain>>,
        growdown: bool,
        shm_attachment: Option<usize>,
        locked: bool,
    ) -> Result<Arc<Self>, VmError> {
        validate_permissions(permissions)?;
        validate_permissions(maximum)?;
        if !permissions_within(permissions, maximum) {
            return Err(VmError::Permission);
        }

        let file_range = match &backing {
            VmaBacking::File {
                inode,
                offset,
                private,
                ..
            } => Some(
                FileRangeRegistration::try_new(
                    inode.clone(),
                    Arc::clone(domain.expect("file VMA requires its PTE domain")),
                    range.clone(),
                    *offset,
                    *private,
                    locked,
                )
                .map_err(|error| VmError::File(error.into()))?,
            ),
            _ => None,
        };

        Arc::try_new(Self {
            range,
            permissions,
            maximum,
            growdown,
            backing,
            shm_attachment,
            file_range,
            anonymous_locked: AtomicBool::new(locked),
        })
        .map_err(|_| VmError::NoMemory)
    }

    /// 返回当前映射唯一生效的锁页标志，文件来源持有其自己的原子状态。
    fn lock_flag(&self) -> &AtomicBool {
        self.file_range
            .as_ref()
            .map_or(&self.anonymous_locked, |range| &range.data().locked)
    }

    /// 在 mmap 保护下读取该虚拟区间是否计入锁页限额。
    pub(crate) fn is_locked(&self) -> bool {
        self.lock_flag().load(Ordering::Acquire)
    }

    /// 最后一个真实用户退出时原地清锁，不分配新 VMA，也不改变驻留页寿命。
    pub(crate) fn clear_lock(&self) {
        self.lock_flag().store(false, Ordering::Release);
    }

    /// 检查当前 `Vma` 的软件权限是否允许指定的用户态访问类型（读、写或取指）。
    pub(crate) fn allows(&self, access: UserAccess) -> bool {
        allows(self.permissions, access)
    }

    /// 判断该区域是否为私有映射（包含私有匿名映射与 `MAP_PRIVATE` 文件映射）。
    ///
    /// 私有映射在 `fork` 复制地址空间后，父子进程对应的物理页必须启用写时复制（COW）保护（即使当前权限暂时只读但最大权限允许后续通过 `mprotect` 开启写权限）。
    pub(crate) fn is_private(&self) -> bool {
        match &self.backing {
            VmaBacking::Anonymous => true,
            VmaBacking::SharedAnonymous { .. } => false,
            VmaBacking::File { private, .. } => *private,
        }
    }

    /// 判断该区域是否为共享文件映射（`MAP_SHARED` 文件映射）。
    ///
    /// 共享文件映射的首次写入必须经过页缓存的 `mkwrite` 脏页准备流程；而共享匿名映射与私有写时复制映射不需要走文件脏页授权。
    pub(crate) fn is_shared_file(&self) -> bool {
        matches!(&self.backing, VmaBacking::File { private: false, .. })
    }

    /// 返回自动选址和堆扩展时需要避开的低地址起始边界（字节地址）。
    ///
    /// 若当前 `Vma` 为向下增长的用户栈，则在 `range.start` 下方额外预留 [`STACK_GUARD_GAP`] 字节的防护间隙；
    /// 显式指定固定地址的映射（`MAP_FIXED` / `MAP_FIXED_NOREPLACE`）仍只检查实际 `range`。
    fn start_gap(&self) -> usize {
        if self.growdown {
            self.range.start.saturating_sub(STACK_GUARD_GAP)
        } else {
            self.range.start
        }
    }

    /// 从当前 `Vma` 中切分出子区间 `range` 并赋予新权限 `permissions`，构造全新的 `Arc<Vma>`。
    ///
    /// 子区间保留权限上限、增长属性和共享段归属，按参数设置锁页状态；后备偏移随左边界变化。
    fn slice(
        &self,
        range: Range<usize>,
        permissions: PagePermissions,
        locked: bool,
    ) -> Result<Arc<Self>, VmError> {
        let backing = match &self.backing {
            VmaBacking::Anonymous => VmaBacking::Anonymous,
            VmaBacking::SharedAnonymous { owner, offset } => VmaBacking::SharedAnonymous {
                owner: Arc::clone(owner),
                offset: offset
                    .checked_add((range.start - self.range.start) as u64)
                    .ok_or(VmError::InvalidRange)?,
            },
            VmaBacking::File {
                file,
                inode,
                offset,
                private,
            } => VmaBacking::File {
                file: Arc::clone(file),
                inode: inode.clone(),
                offset: offset
                    .checked_add((range.start - self.range.start) as u64)
                    .ok_or(VmError::InvalidRange)?,
                private: *private,
            },
        };
        Self::try_new_with_growth(
            range,
            permissions,
            self.maximum,
            backing,
            self.file_range.as_ref().map(|range| &range.data().domain),
            self.growdown,
            self.shm_attachment,
            locked,
        )
    }

    /// 判断当前 `Vma` 与紧邻的高地址 `next` 区域是否可以无损合并为单个 `Vma`。
    ///
    /// 仅当两者首尾相接、权限、锁页状态与增长属性一致、至少具备一项访问权限，
    /// 且底层后备对象相同、文件或共享匿名偏移严格连续时，才允许合并。
    fn mergeable(&self, next: &Self) -> bool {
        if self.range.end != next.range.start
            || self.permissions != next.permissions
            || self.maximum != next.maximum
            || self.growdown != next.growdown
            || self.is_locked() != next.is_locked()
            || self.shm_attachment != next.shm_attachment
            || !(self.permissions.read || self.permissions.write || self.permissions.execute)
        {
            return false;
        }

        let length = (self.range.end - self.range.start) as u64;
        match (&self.backing, &next.backing) {
            (VmaBacking::Anonymous, VmaBacking::Anonymous) => true,
            (
                VmaBacking::SharedAnonymous { owner, offset },
                VmaBacking::SharedAnonymous {
                    owner: next_owner,
                    offset: next_offset,
                },
            ) => Arc::ptr_eq(owner, next_owner) && offset.checked_add(length) == Some(*next_offset),
            (
                VmaBacking::File {
                    file,
                    inode,
                    offset,
                    private,
                },
                VmaBacking::File {
                    file: next_file,
                    inode: next_inode,
                    offset: next_offset,
                    private: next_private,
                },
            ) => {
                private == next_private
                    && Arc::ptr_eq(file, next_file)
                    && inode.ptr_eq(next_inode)
                    && offset.checked_add(length) == Some(*next_offset)
            }
            _ => false,
        }
    }
}

/// 地址空间的虚拟内存区域集合与堆边界状态，由所属 [`crate::mm::space::Mm`] 的睡眠读写锁保护。
///
/// 硬件页表项（PTE）与驻留物理页所有权由 `PteDomain` 自旋锁独立保护，不在此结构体中直接存放。
#[derive(Default)]
pub(crate) struct MmapState {
    /// 按 `range.start` 严格升序排列且互不重叠的已发布 `Vma` 列表。
    pub(crate) vmas: Vec<Arc<Vma>>,
    /// 本 Mm 拥有的独立连接；末片 VMA 卸载后在 TLB 同步完成前移入退休值。
    pub(crate) shm_attachments: Vec<ShmAttachment>,
    /// 下一笔连接的编号前值，递增后使用，避免与尚存片段的旧连接混淆。
    pub(crate) next_shm_attachment: usize,
    /// 用户堆区的起始虚拟地址（由加载器在 `execve` 装载完 ELF 段后设定，单位：字节）。
    pub(crate) heap_start: usize,
    /// 当前用户堆顶（Program Break）的精确字节地址。
    pub(crate) heap_end: usize,
}

impl MmapState {
    /// 在有序且互不重叠的 `vmas` 列表中二分查找包含虚拟地址 `address` 的 [`Vma`]。
    pub(crate) fn find(&self, address: usize) -> Option<&Arc<Vma>> {
        let after = self.vmas.partition_point(|vma| vma.range.start <= address);
        after
            .checked_sub(1)
            .and_then(|index| self.vmas.get(index))
            .filter(|vma| address < vma.range.end)
    }

    /// 检查候选区间 `range` 是否与所有已发布的 `Vma` 均无交集（允许区间端点紧邻）。
    pub(crate) fn vacant(&self, range: &Range<usize>) -> bool {
        let index = self
            .vmas
            .partition_point(|vma| vma.range.end <= range.start);
        self.vmas
            .get(index)
            .is_none_or(|vma| range.end <= vma.range.start)
    }

    /// 检查候选区间 `range` 是否与所有已发布的 `Vma` 及其栈下方防护间隙（[`STACK_GUARD_GAP`]）均无交集。
    ///
    /// 用于非固定地址分配（`choose_address`）和 `brk` 堆扩展，防止新映射或堆增长侵占相邻用户栈下方的防护区。
    pub(crate) fn vacant_with_guard(&self, range: &Range<usize>) -> bool {
        let index = self
            .vmas
            .partition_point(|vma| vma.range.end <= range.start);
        self.vmas
            .get(index)
            .is_none_or(|vma| range.end <= vma.start_gap())
    }

    /// 验证位于未映射空洞中的缺页地址 `address` 是否允许通过紧邻高地址栈 VMA 向下扩展来满足，并构造扩展后的新栈 [`Vma`]。
    ///
    /// 在持有 `mmap` 写锁时调用，依次校验：
    /// 1. `address` 上方紧邻的 VMA 必须带有 `growdown` 标志，且其权限允许本次访问 `access`；
    /// 2. 扩展后的栈底 `base` 不得越过前一个 VMA 的末尾，且若前一个 VMA 是可访问的非栈映射，两者之间必须保留至少 [`STACK_GUARD_GAP`] 字节的防护间隙；
    /// 3. 扩展后的栈总字节数不得超过栈软限额；锁定栈还须满足扣除旧栈后的地址空间锁页总限额。
    ///
    /// 前邻冲突或超出软限额仍返回 [`VmError::Unmapped`]（触发段错误），仅当候选 `Vma` 内存分配失败时返回 [`VmError::NoMemory`]。
    pub(crate) fn stack_growth(
        &self,
        address: usize,
        access: UserAccess,
        limit: u64,
        lock_limit: u64,
    ) -> Result<Arc<Vma>, VmError> {
        // Step 1: 将缺页地址向下对齐到页边界，并定位其上方紧邻的候选栈 VMA。
        let base = address & !(PAGE_SIZE - 1);
        checked_range(base, PAGE_SIZE)?;
        let index = self.vmas.partition_point(|vma| vma.range.start <= address);
        let stack = self.vmas.get(index).ok_or(VmError::Unmapped)?;
        if !stack.growdown {
            return Err(VmError::Unmapped);
        }
        if !stack.allows(access) {
            return Err(VmError::Permission);
        }
        // Step 2: 校验与低地址前邻 VMA 的边界及栈防护间隙（STACK_GUARD_GAP）约束。
        if let Some(previous) = index.checked_sub(1).map(|index| &self.vmas[index]) {
            if base < previous.range.end {
                return Err(VmError::Unmapped);
            }
            if !previous.growdown
                && (previous.permissions.read
                    || previous.permissions.write
                    || previous.permissions.execute)
                && base - previous.range.end < STACK_GUARD_GAP
            {
                return Err(VmError::Unmapped);
            }
        }
        // Step 3: 校验扩展后的栈总长度是否超出当前线程的 RLIMIT_STACK 软限额，并构造新栈 VMA。
        if limit < (stack.range.end - base) as u64 {
            return Err(VmError::Unmapped);
        }

        let range = base..stack.range.end;
        if stack.is_locked()
            && lock_limit / (PAGE_SIZE as u64)
                < ((range.end - range.start) / PAGE_SIZE + self.locked_pages_outside(&range)) as u64
        {
            return Err(VmError::Unmapped);
        }
        Vma::try_new_with_growth(
            range,
            stack.permissions,
            stack.maximum,
            VmaBacking::Anonymous,
            None,
            true,
            stack.shm_attachment,
            stack.is_locked(),
        )
    }

    /// 为 fork 子 Mm 复制区间、堆边界和独立共享段连接。
    ///
    /// 文件映射独立登记到子页表域；锁定的匿名 VMA 也必须新建为未锁定实例，不能跨 Mm 共享可清除状态。
    pub(crate) fn snapshot(&self, domain: &Arc<PteDomain>) -> Result<Self, VmError> {
        let mut vmas = Vec::new();
        vmas.try_reserve_exact(self.vmas.len())
            .map_err(|_| VmError::NoMemory)?;
        let mut shm_attachments = Vec::new();
        shm_attachments
            .try_reserve_exact(self.shm_attachments.len())
            .map_err(|_| VmError::NoMemory)?;
        for attachment in &self.shm_attachments {
            shm_attachments.push(attachment.try_fork()?);
        }
        for vma in &self.vmas {
            if vma.file_range.is_some() || vma.is_locked() {
                vmas.push(Vma::try_new_with_growth(
                    vma.range.clone(),
                    vma.permissions,
                    vma.maximum,
                    vma.backing.clone(),
                    Some(domain),
                    vma.growdown,
                    vma.shm_attachment,
                    false,
                )?);
            } else {
                vmas.push(Arc::clone(vma));
            }
        }
        Ok(Self {
            vmas,
            shm_attachments,
            next_shm_attachment: self.next_shm_attachment,
            heap_start: self.heap_start,
            heap_end: self.heap_end,
        })
    }

    /// 单次遍历计算目标区间以外已经锁定的虚拟页数；同一物理页的不同虚拟别名分别计费。
    pub(crate) fn locked_pages_outside(&self, range: &Range<usize>) -> usize {
        self.vmas
            .iter()
            .filter(|vma| vma.is_locked())
            .map(|vma| {
                let overlap = vma
                    .range
                    .end
                    .min(range.end)
                    .saturating_sub(vma.range.start.max(range.start));
                (vma.range.end - vma.range.start - overlap) / PAGE_SIZE
            })
            .sum()
    }

    /// 预检 `mprotect` 请求区间 `range` 是否被已映射 VMA 连续完整覆盖，且每段 VMA 的最大权限 `maximum` 均允许目标权限 `permissions`。
    ///
    /// 若区间内存在未映射空洞则返回 [`VmError::Unmapped`]，若权限超出上限则返回 [`VmError::Permission`]，不修改任何现有状态。
    pub(crate) fn check_protection(
        &self,
        range: &Range<usize>,
        permissions: PagePermissions,
    ) -> Result<(), VmError> {
        validate_permissions(permissions)?;
        let mut cursor = range.start;
        for vma in &self.vmas {
            if vma.range.end <= cursor {
                continue;
            }
            if cursor < vma.range.start {
                return Err(VmError::Unmapped);
            }
            if !permissions_within(permissions, vma.maximum) {
                return Err(VmError::Permission);
            }
            cursor = vma.range.end.min(range.end);
            if cursor == range.end {
                return Ok(());
            }
        }
        Err(VmError::Unmapped)
    }

    /// 针对目标区间 `range` 构造重写后的完整新 VMA 列表（在获取页表锁前于锁外完成全部分配）。
    ///
    /// 两个变更参数均为空时移除相交区间；指定权限或锁页状态时只替换对应属性。
    /// 相交区间两侧保留旧属性和偏移，最后合并兼容的相邻节点。
    pub(crate) fn rewrite(
        &self,
        range: &Range<usize>,
        permissions: Option<PagePermissions>,
        locked: Option<bool>,
    ) -> Result<Vec<Arc<Vma>>, VmError> {
        let mut vmas = Vec::new();
        vmas.try_reserve_exact(self.vmas.len().checked_add(2).ok_or(VmError::NoMemory)?)
            .map_err(|_| VmError::NoMemory)?;
        for vma in &self.vmas {
            if vma.range.end <= range.start || range.end <= vma.range.start {
                vmas.push(Arc::clone(vma));
                continue;
            }
            if vma.range.start < range.start {
                vmas.push(vma.slice(
                    vma.range.start..range.start,
                    vma.permissions,
                    vma.is_locked(),
                )?);
            }
            let start = vma.range.start.max(range.start);
            let end = vma.range.end.min(range.end);
            if permissions.is_some() || locked.is_some() {
                vmas.push(vma.slice(
                    start..end,
                    permissions.unwrap_or(vma.permissions),
                    locked.unwrap_or_else(|| vma.is_locked()),
                )?);
            }
            if range.end < vma.range.end {
                vmas.push(vma.slice(range.end..vma.range.end, vma.permissions, vma.is_locked())?);
            }
        }
        coalesce(&mut vmas)?;
        Ok(vmas)
    }

    /// 构造将区间 `range` 内的旧映射移除、并可选插入新映射 `replacement` 后的完整候选 VMA 列表。
    ///
    /// 所有的内存分配、区间拆分、相邻合并与文件偏移溢出检查均在解除旧硬件页表项之前完成。
    pub(crate) fn replacement(
        &self,
        range: &Range<usize>,
        replacement: Option<Arc<Vma>>,
    ) -> Result<Vec<Arc<Vma>>, VmError> {
        let mut vmas = self.rewrite(range, None, None)?;
        if let Some(replacement) = replacement {
            vmas.try_reserve(1).map_err(|_| VmError::NoMemory)?;
            let index = vmas.partition_point(|vma| vma.range.start < range.start);
            vmas.insert(index, replacement);
            coalesce(&mut vmas)?;
        }
        Ok(vmas)
    }

    /// 为未指定固定地址的 `mmap` 请求寻找一段长度为 `length` 字节的空闲虚拟地址区间起点。
    ///
    /// 优先尝试向下页对齐且不低于 `64 KiB` 的 `hint` 地址；若不可用，则从 `64 KiB` 起按低地址向高地址线性扫描第一个满足长度要求且避开栈防护间隙（[`STACK_GUARD_GAP`]）的空隙。
    pub(crate) fn choose_address(&self, hint: usize, length: usize) -> Result<usize, VmError> {
        const MINIMUM: usize = 64 * 1024;
        let hint = hint & !(PAGE_SIZE - 1);
        if MINIMUM <= hint {
            if let Ok(range) = checked_range(hint, length) {
                if self.vacant_with_guard(&range) {
                    return Ok(hint);
                }
            }
        }

        let mut cursor = MINIMUM;
        for vma in &self.vmas {
            if vma.range.end <= cursor {
                continue;
            }
            let range = checked_range(cursor, length).map_err(|_| VmError::NoMemory)?;
            if range.end <= vma.start_gap() {
                return Ok(cursor);
            }
            cursor = vma.range.end;
        }
        checked_range(cursor, length).map_err(|_| VmError::NoMemory)?;
        Ok(cursor)
    }
}

/// 在尚未发布的候选 VMA 列表中原地合并所有相邻且满足 [`Vma::mergeable`] 条件的区域。
///
/// 若合并过程中的新节点分配失败，直接返回错误，不会污染当前已生效的 `MmapState`。
fn coalesce(vmas: &mut Vec<Arc<Vma>>) -> Result<(), VmError> {
    let mut kept = 0;
    for index in 0..vmas.len() {
        if 0 < kept && vmas[kept - 1].mergeable(&vmas[index]) {
            let previous = &vmas[kept - 1];
            let merged = Vma::try_new_with_growth(
                previous.range.start..vmas[index].range.end,
                previous.permissions,
                previous.maximum,
                previous.backing.clone(),
                previous
                    .file_range
                    .as_ref()
                    .map(|range| &range.data().domain),
                previous.growdown,
                previous.shm_attachment,
                previous.is_locked(),
            )?;
            vmas[kept - 1] = merged;
        } else {
            if kept != index {
                vmas[kept] = Arc::clone(&vmas[index]);
            }
            kept += 1;
        }
    }
    vmas.truncate(kept);
    Ok(())
}

/// 校验并构造用户虚拟地址区间 `[address, address + length)`。
///
/// 拒绝长度为 0、起始地址或长度未按 [`PAGE_SIZE`] 对齐、加法溢出或超出架构用户地址空间上限（[`arch::USER_ADDRESS_END`]）的非法范围。
pub(crate) fn checked_range(address: usize, length: usize) -> Result<Range<usize>, VmError> {
    let end = address.checked_add(length).ok_or(VmError::InvalidRange)?;
    if length == 0
        || address & (PAGE_SIZE - 1) != 0
        || length & (PAGE_SIZE - 1) != 0
        || arch::USER_ADDRESS_END < end
    {
        return Err(VmError::InvalidRange);
    }
    Ok(address..end)
}

/// 判断请求的页面权限 `request` 的每一项能力是否均不超过上限权限 `maximum`。
pub(crate) fn permissions_within(request: PagePermissions, maximum: PagePermissions) -> bool {
    (!request.read || maximum.read)
        && (!request.write || maximum.write)
        && (!request.execute || maximum.execute)
}

/// 判断页面权限 `permissions` 是否允许硬件访问类型 `access`（读、写或执行）。
pub(crate) fn allows(permissions: PagePermissions, access: UserAccess) -> bool {
    match access {
        UserAccess::Read => permissions.read,
        UserAccess::Write => permissions.write,
        UserAccess::Execute => permissions.execute,
    }
}

/// 校验目标权限组合是否能够被当前硬件架构原生表示，拒绝将不支持的权限组合静默提升为更宽的权限。
///
/// 例如在 RISC-V Sv39 架构下，硬件页表不支持“仅可写不可读（W=1, R=0）”编码，此类请求直接返回 [`VmError::InvalidRange`]。
pub(crate) fn validate_permissions(permissions: PagePermissions) -> Result<(), VmError> {
    #[cfg(target_arch = "riscv64")]
    if permissions.write && !permissions.read {
        return Err(VmError::InvalidRange);
    }
    let _ = permissions;
    Ok(())
}
