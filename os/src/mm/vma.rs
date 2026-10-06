//! mmap 读写锁下的有序范围；不可变 VMA 身份用于锁外 fault 准备后的真实重查。

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ops::Range;

use crate::arch::{self, UserAccess};
use crate::fs::FileRef;
use crate::mm::space::VmError;
use crate::mm::user_table::PagePermissions;
use crate::mm::PAGE_SIZE;

/// 映射保留完整文件打开实例；私有匿名内容的原页拥有在实际驻留节点中。
#[derive(Clone)]
pub(crate) enum VmaBacking {
    Anonymous,
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
            VmaBacking::File { private, .. } => *private,
        }
    }

    /// 从原范围拆出新身份，文件偏移随左边界移动，backing 与最大权限保持原义。
    fn slice(
        &self,
        range: Range<usize>,
        permissions: PagePermissions,
    ) -> Result<Arc<Self>, VmError> {
        let backing = match &self.backing {
            VmaBacking::Anonymous => VmaBacking::Anonymous,
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

    /// 在 mmap 写侧且 PTE 短锁外准备索引容量，然后发布已准备的身份。
    pub(crate) fn insert(&mut self, vma: Arc<Vma>) -> Result<(), VmError> {
        if !self.vacant(&vma.range) {
            return Err(VmError::Occupied);
        }
        self.vmas.try_reserve(1).map_err(|_| VmError::NoMemory)?;
        let index = self
            .vmas
            .partition_point(|current| current.range.start < vma.range.start);
        self.vmas.insert(index, vma);
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
        Ok(vmas)
    }
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
