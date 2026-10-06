//! mmap 读写锁下的有序范围；不可变 VMA 身份用于锁外 fault 准备后的真实重查。

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ops::Range;

use crate::arch::{self, UserAccess};
use crate::fs::FileRef;
use crate::mm::anon::SharedAnon;
use crate::mm::space::VmError;
use crate::mm::user_table::PagePermissions;
use crate::mm::PAGE_SIZE;

/// 映射保留完整文件打开实例；私有匿名内容的原页拥有在实际驻留节点中。
#[derive(Clone)]
pub(crate) enum VmaBacking {
    Anonymous,
    SharedAnonymous {
        owner: Arc<SharedAnon>,
        offset: u64,
    },
    File {
        file: FileRef,
        offset: u64,
        private: bool,
    },
}

/// 已发表范围不可原地改身份；拆分、换权限和重新映射发布新的具体对象。
pub(crate) struct Vma {
    pub(crate) range: Range<usize>,
    pub(crate) permissions: PagePermissions,
    pub(crate) maximum: PagePermissions,
    pub(crate) backing: VmaBacking,
}

impl Vma {
    /// 在普通任务中准备尚未发表的完整范围，失败不改变现有映射。
    pub(crate) fn try_new(
        range: Range<usize>,
        permissions: PagePermissions,
        maximum: PagePermissions,
        backing: VmaBacking,
    ) -> Result<Arc<Self>, VmError> {
        validate_permissions(permissions)?;
        validate_permissions(maximum)?;
        if !permissions_within(permissions, maximum) {
            return Err(VmError::Permission);
        }
        Arc::try_new(Self {
            range,
            permissions,
            maximum,
            backing,
        })
        .map_err(|_| VmError::NoMemory)
    }

    /// 由当前 VMA 权限判断一次读、写或取指，不从硬件可写位推断最大权限。
    pub(crate) fn allows(&self, access: UserAccess) -> bool {
        allows(self.permissions, access)
    }

    /// 私有映射在独立 Mm 复制后必须维持 COW，包括当前只读但未来允许写的页。
    pub(crate) fn is_private(&self) -> bool {
        match &self.backing {
            VmaBacking::Anonymous => true,
            VmaBacking::SharedAnonymous { .. } => false,
            VmaBacking::File { private, .. } => *private,
        }
    }

    /// 从原范围拆出新身份，文件及共享匿名偏移随左边界移动，最大权限保持原义。
    fn slice(
        &self,
        range: Range<usize>,
        permissions: PagePermissions,
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
                offset,
                private,
            } => VmaBacking::File {
                file: Arc::clone(file),
                offset: offset
                    .checked_add((range.start - self.range.start) as u64)
                    .ok_or(VmError::InvalidRange)?,
                private: *private,
            },
        };
        Self::try_new(range, permissions, self.maximum, backing)
    }

    /// 只合并权限及真实后备连续的相邻范围，保留无访问范围的独立边界。
    fn mergeable(&self, next: &Self) -> bool {
        if self.range.end != next.range.start
            || self.permissions != next.permissions
            || self.maximum != next.maximum
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
                    offset,
                    private,
                },
                VmaBacking::File {
                    file: next_file,
                    offset: next_offset,
                    private: next_private,
                },
            ) => {
                private == next_private
                    && Arc::ptr_eq(file, next_file)
                    && offset.checked_add(length) == Some(*next_offset)
            }
            _ => false,
        }
    }
}

/// 睡眠读写锁负责范围、权限和真实堆边界；PTE 与数据页拥有另由短锁保护。
#[derive(Default)]
pub(crate) struct MmapState {
    pub(crate) vmas: Vec<Arc<Vma>>,
    pub(crate) heap_start: usize,
    pub(crate) heap_end: usize,
}

impl MmapState {
    /// 从有序不相交范围找到实际包含地址的身份，不分配或读取文件系统。
    pub(crate) fn find(&self, address: usize) -> Option<&Arc<Vma>> {
        let after = self.vmas.partition_point(|vma| vma.range.start <= address);
        after
            .checked_sub(1)
            .and_then(|index| self.vmas.get(index))
            .filter(|vma| address < vma.range.end)
    }

    /// 检查新范围与已发表 VMA 无重叠，允许紧邻边界。
    pub(crate) fn vacant(&self, range: &Range<usize>) -> bool {
        let index = self
            .vmas
            .partition_point(|vma| vma.range.end <= range.start);
        self.vmas
            .get(index)
            .is_none_or(|vma| range.end <= vma.range.start)
    }

    /// 在 mmap 写侧准备完整新索引及合并身份，成功后一次发表，失败保留原索引。
    pub(crate) fn insert(&mut self, vma: Arc<Vma>) -> Result<(), VmError> {
        if !self.vacant(&vma.range) {
            return Err(VmError::Occupied);
        }
        let range = vma.range.clone();
        let vmas = self.replacement(&range, Some(vma))?;
        self.vmas = vmas;
        Ok(())
    }

    /// 在独立 Mm 尚未发表时复制范围索引，共享不可变身份不合并驻留页责任。
    pub(crate) fn snapshot(&self) -> Result<Self, VmError> {
        let mut vmas = Vec::new();
        vmas.try_reserve_exact(self.vmas.len())
            .map_err(|_| VmError::NoMemory)?;
        vmas.extend(self.vmas.iter().cloned());
        Ok(Self {
            vmas,
            heap_start: self.heap_start,
            heap_end: self.heap_end,
        })
    }

    /// 检查改权限范围连续覆盖且每段最大权限允许请求，空洞不修改任何状态。
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
            if let VmaBacking::File { private: false, .. } = &vma.backing {
                if permissions.write {
                    return Err(VmError::File(crate::fs::FsError::ReadOnly));
                }
            }
            cursor = vma.range.end.min(range.end);
            if cursor == range.end {
                return Ok(());
            }
        }
        Err(VmError::Unmapped)
    }

    /// 锁外于 PTE 准备完整的新索引；None 删除相交范围，Some 只改变相交部分的权限。
    pub(crate) fn rewrite(
        &self,
        range: &Range<usize>,
        permissions: Option<PagePermissions>,
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
                vmas.push(vma.slice(vma.range.start..range.start, vma.permissions)?);
            }
            let start = vma.range.start.max(range.start);
            let end = vma.range.end.min(range.end);
            if let Some(permissions) = permissions {
                vmas.push(vma.slice(start..end, permissions)?);
            }
            if range.end < vma.range.end {
                vmas.push(vma.slice(range.end..vma.range.end, vma.permissions)?);
            }
        }
        coalesce(&mut vmas)?;
        Ok(vmas)
    }

    /// 准备删除或覆盖后的全部身份；所有分配与偏移检查发生在解除旧叶之前。
    pub(crate) fn replacement(
        &self,
        range: &Range<usize>,
        replacement: Option<Arc<Vma>>,
    ) -> Result<Vec<Arc<Vma>>, VmError> {
        let mut vmas = self.rewrite(range, None)?;
        if let Some(replacement) = replacement {
            vmas.try_reserve(1).map_err(|_| VmError::NoMemory)?;
            let index = vmas.partition_point(|vma| vma.range.start < range.start);
            vmas.insert(index, replacement);
            coalesce(&mut vmas)?;
        }
        Ok(vmas)
    }

    /// 先尝试已页下取整的合法 hint，冲突或越界时从普通 64KiB 起找首个完整空隙。
    pub(crate) fn choose_address(&self, hint: usize, length: usize) -> Result<usize, VmError> {
        const MINIMUM: usize = 64 * 1024;
        let hint = hint & !(PAGE_SIZE - 1);
        if MINIMUM <= hint {
            if let Ok(range) = checked_range(hint, length) {
                if self.vacant(&range) {
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
            if range.end <= vma.range.start {
                return Ok(cursor);
            }
            cursor = vma.range.end;
        }
        checked_range(cursor, length).map_err(|_| VmError::NoMemory)?;
        Ok(cursor)
    }
}

/// 在未发表候选索引中原地合并相邻身份，任何分配失败均不改当前 Mm。
fn coalesce(vmas: &mut Vec<Arc<Vma>>) -> Result<(), VmError> {
    let mut kept = 0;
    for index in 0..vmas.len() {
        if 0 < kept && vmas[kept - 1].mergeable(&vmas[index]) {
            let previous = &vmas[kept - 1];
            let merged = Vma::try_new(
                previous.range.start..vmas[index].range.end,
                previous.permissions,
                previous.maximum,
                previous.backing.clone(),
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

/// 一次 checked 解析用户页范围，不接受空长度、未对齐或越出架构用户区域。
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

/// 只比较逐项访问能力，权限降低不改变私有共享页的 COW 性质。
pub(crate) fn permissions_within(request: PagePermissions, maximum: PagePermissions) -> bool {
    (!request.read || maximum.read)
        && (!request.write || maximum.write)
        && (!request.execute || maximum.execute)
}

/// 返回当前权限是否允许指定的硬件访问方向。
pub(crate) fn allows(permissions: PagePermissions, access: UserAccess) -> bool {
    match access {
        UserAccess::Read => permissions.read,
        UserAccess::Write => permissions.write,
        UserAccess::Execute => permissions.execute,
    }
}

/// 拒绝本架构无法编码的权限，禁止把请求偷偷提升为另一种权限。
pub(crate) fn validate_permissions(permissions: PagePermissions) -> Result<(), VmError> {
    #[cfg(target_arch = "riscv64")]
    if permissions.write && !permissions.read {
        return Err(VmError::InvalidRange);
    }
    let _ = permissions;
    Ok(())
}
