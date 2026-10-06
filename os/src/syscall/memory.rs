//! 用户映射请求按目标ABI解释一次，再交同一Mm的实际范围与页拥有提交。

use crate::mm::space::{MmapAddress, MmapRequest, MmapSource};
use crate::mm::user_table::PagePermissions;
use crate::mm::PAGE_SIZE;
use crate::syscall::error::{Errno, SysResult};
use crate::task::UserThread;

const SHARED: usize = 1;
const PRIVATE: usize = 2;
const FIXED: usize = 0x10;
const ANONYMOUS: usize = 0x20;
const FIXED_NOREPLACE: usize = 0x10_0000;
const SUPPORTED: usize = SHARED | PRIVATE | FIXED | ANONYMOUS | FIXED_NOREPLACE;
const ALL: PagePermissions = PagePermissions {
    read: true,
    write: true,
    execute: true,
};

/// 仅支持基本页匿名与正式文件映射，私有文件写权限不授权向只读卷写回。
pub(super) fn mmap(user: &UserThread, raw: [usize; 6]) -> SysResult {
    let [address, length, prot, flags, fd, offset] = raw;
    if flags & !SUPPORTED != 0
        || !matches!(flags & 3, SHARED | PRIVATE)
        || offset & (PAGE_SIZE - 1) != 0
    {
        return Err(Errno::InvalidArgument);
    }
    let permissions = permissions(prot)?;
    let private = flags & 3 == PRIVATE;
    let mut maximum = ALL;
    let source = if flags & ANONYMOUS != 0 {
        if private {
            MmapSource::PrivateAnonymous
        } else {
            MmapSource::SharedAnonymous
        }
    } else {
        let runtime = user.runtime().expect("mmap has no runtime");
        let fd = fd as i32;
        if fd < 0 {
            return Err(Errno::BadFd);
        }
        let file = runtime.files.lookup(fd as usize)?;
        let access = file.flags() & 3;
        if access == 1 || (!private && permissions.write && access == 0) {
            return Err(Errno::Permission);
        }
        if !private && access == 0 {
            maximum.write = false;
        }
        MmapSource::File {
            file,
            offset: offset as u64,
            private,
        }
    };
    let address = if flags & FIXED_NOREPLACE != 0 {
        MmapAddress::FixedNoReplace(address)
    } else if flags & FIXED != 0 {
        MmapAddress::Fixed(address)
    } else {
        MmapAddress::Hint(address)
    };
    let mm = user.mm().expect("mmap has no MM");
    mm.mmap(MmapRequest {
        address,
        length,
        permissions,
        maximum,
        source,
    })
    .map(|address| address as isize)
    .map_err(Errno::from)
}

/// 用户长度向上取基本页，起址和有效范围仍由同一Mm检查，空洞解除正常成功。
pub(super) fn munmap(user: &UserThread, raw: [usize; 6]) -> SysResult {
    if raw[1] == 0 {
        return Err(Errno::InvalidArgument);
    }
    let length = rounded_length(raw[1])?;
    user.mm().expect("munmap has no MM").unmap(raw[0], length)?;
    Ok(0)
}

/// 零长度仍先检查起址页齐；非空请求保当前COW与VMA最大权限。
pub(super) fn mprotect(user: &UserThread, raw: [usize; 6]) -> SysResult {
    if raw[0] & (PAGE_SIZE - 1) != 0 {
        return Err(Errno::InvalidArgument);
    }
    if raw[1] == 0 {
        return Ok(0);
    }
    let permissions = permissions(raw[2])?;
    let length = rounded_length(raw[1])?;
    user.mm()
        .expect("mprotect has no MM")
        .protect(raw[0], length, permissions)?;
    Ok(0)
}

/// raw brk的失败仍是原正break，不经过普通负errno转换。
pub(super) fn brk(user: &UserThread, address: usize) -> isize {
    user.mm().expect("brk has no MM").brk(address) as isize
}

/// 写权限在RV硬件上必然包含读，LA保持实际NR/NX能力；未知PROT位明确失败。
fn permissions(raw: usize) -> Result<PagePermissions, Errno> {
    if raw & !7 != 0 {
        return Err(Errno::InvalidArgument);
    }
    let write = raw & 2 != 0;
    #[cfg(target_arch = "riscv64")]
    let read = raw & 1 != 0 || write;
    #[cfg(target_arch = "loongarch64")]
    let read = raw & 1 != 0;
    Ok(PagePermissions {
        read,
        write,
        execute: raw & 4 != 0,
    })
}

/// 长度进位只在用户粒度转换时计算，随后严格MM接口不再解释raw长度。
fn rounded_length(length: usize) -> Result<usize, Errno> {
    length
        .checked_add(PAGE_SIZE - 1)
        .map(|length| length & !(PAGE_SIZE - 1))
        .ok_or(Errno::NoMemory)
}
