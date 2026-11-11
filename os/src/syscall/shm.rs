//! System V 共享内存系统调用的参数校验和错误转换。
//!
//! shmget/shmctl 管理共享段身份；shmat/shmdt 通过当前 Mm 管理独立连接及其 VMA 片段。
//! 当前 shmctl 只支持 IPC_RMID；其余命令返回 EINVAL，不解析或伪造用户 shmid_ds。
//! 用户运行时锁和段表锁均在取得 Mm 的 mmap 锁前释放。

use crate::mm::shm::{self, ShmAttachment, ShmError, IPC_CREAT, IPC_EXCL};
use crate::mm::space::MmapAddress;
use crate::mm::user_table::PagePermissions;
use crate::mm::PAGE_SIZE;
use crate::syscall::error::{Errno, SysResult};
use crate::task::UserThread;

/// 两种目标架构共同使用的 shmget 系统调用号。
const SHMGET: usize = 194;
/// 两种目标架构共同使用的 shmctl 系统调用号。
const SHMCTL: usize = 195;
/// 两种目标架构共同使用的 shmat 系统调用号。
const SHMAT: usize = 196;
/// 两种目标架构共同使用的 shmdt 系统调用号。
const SHMDT: usize = 197;
/// 删除段名字并请求在最后一个连接退役时销毁。
const IPC_RMID: i32 = 0;
/// 共享段不预留全部物理页；现有 SharedAnon 本身按缺页分配。
const SHM_NORESERVE: u32 = 0o10000;
/// 建立只读连接。
const SHM_RDONLY: u32 = 0o10000;
/// 将显式连接地址按 SHMLBA 向下取整；两目标的 SHMLBA 均为基本页大小。
const SHM_RND: u32 = 0o20000;
/// 允许覆盖显式地址范围中的旧映射。
const SHM_REMAP: u32 = 0o40000;
/// 为连接增加执行权限。
const SHM_EXEC: u32 = 0o100000;

/// 在普通用户任务上下文分派四个共享内存调用，未知编号保持 ENOSYS。
pub(super) fn dispatch(user: &UserThread, number: usize, raw: [usize; 6]) -> SysResult {
    match number {
        SHMGET => {
            let flags = raw[2] as u32;
            if flags & !(0o777 | IPC_CREAT | IPC_EXCL | SHM_NORESERVE) != 0 {
                return Err(Errno::InvalidArgument);
            }
            let credentials = user.runtime().expect("shmget has no runtime").credentials;
            shm::get(raw[0] as i32, raw[1], flags, credentials)
                .map(|id| id as isize)
                .map_err(errno)
        }
        SHMCTL => {
            if raw[1] as i32 != IPC_RMID {
                return Err(Errno::InvalidArgument);
            }
            let credentials = user.runtime().expect("shmctl has no runtime").credentials;
            shm::remove(raw[0] as i32, credentials).map_err(errno)?;
            Ok(0)
        }
        SHMAT => attach(user, raw),
        SHMDT => {
            user.mm().expect("shmdt has no MM").detach_shm(raw[0])?;
            Ok(0)
        }
        _ => Err(Errno::NoSyscall),
    }
}

/// 校验连接标志并保留原始 NULL 与显式取整到零的选址差异，再预留段引用并建立映射。
fn attach(user: &UserThread, raw: [usize; 6]) -> SysResult {
    let flags = raw[2] as u32;
    if flags & !(SHM_RDONLY | SHM_RND | SHM_REMAP | SHM_EXEC) != 0 {
        return Err(Errno::InvalidArgument);
    }
    let mut address = raw[1];
    if address & (PAGE_SIZE - 1) != 0 {
        if flags & SHM_RND == 0 {
            return Err(Errno::InvalidArgument);
        }
        address &= !(PAGE_SIZE - 1);
    }
    let remap = flags & SHM_REMAP != 0;
    if remap && address == 0 {
        return Err(Errno::InvalidArgument);
    }
    // 显式地址经 SHM_RND 取整为零后仍采用 MAP_FIXED；原始 NULL 才允许自动选址。
    let address = if raw[1] == 0 {
        MmapAddress::Hint(0)
    } else if remap || address == 0 {
        MmapAddress::Fixed(address)
    } else {
        MmapAddress::FixedNoReplace(address)
    };
    let permissions = PagePermissions {
        read: true,
        write: flags & SHM_RDONLY == 0,
        execute: flags & SHM_EXEC != 0,
    };
    let requested = 0o4
        | (if permissions.write { 0o2 } else { 0 })
        | (if permissions.execute { 0o1 } else { 0 });
    let credentials = user.runtime().expect("shmat has no runtime").credentials;
    let attachment =
        ShmAttachment::acquire(raw[0] as i32, credentials, requested).map_err(errno)?;
    user.mm()
        .expect("shmat has no MM")
        .attach_shm(attachment, address, permissions)
        .map(|address| address as isize)
        .map_err(Errno::from)
}

/// 将共享段的领域错误逐项转换成 Linux errno。
fn errno(error: ShmError) -> Errno {
    match error {
        ShmError::Invalid => Errno::InvalidArgument,
        ShmError::NotFound => Errno::NotFound,
        ShmError::Exists => Errno::Exists,
        ShmError::NoMemory => Errno::NoMemory,
        ShmError::NoSpace => Errno::NoSpace,
        ShmError::Permission => Errno::Permission,
        ShmError::NotPermitted => Errno::NotPermitted,
    }
}
