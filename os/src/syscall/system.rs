//! 挂载、同步与关机准入只消费当前用户的真实 VFS、凭据和身份图。

use crate::block::AccessMode;
use crate::fs::memfs::{Filesystem, MemoryFs, MemoryFsKind};
use crate::fs::{devfs, procfs, ResolveMode};
use crate::syscall::error::{Errno, SysResult};
use crate::syscall::Outcome;
use crate::task::process::{RootRole, ShutdownOperation};
use crate::task::UserThread;

const PATH_LIMIT: usize = 4096;
const MS_RDONLY: usize = 1;
const MS_SILENT: usize = 0x8000;
const UMOUNT_NOFOLLOW: usize = 8;

/// uid0 才可建立所选真实内存后端；输入各读取一次，完整准备后沿原 Mount 发表。
pub(super) fn mount(user: &UserThread, raw: &[usize; 6]) -> SysResult {
    let runtime = user.runtime().expect("mount has no runtime");
    if runtime.credentials.uid != 0 {
        return Err(Errno::NotPermitted);
    }
    if raw[3] & !(MS_RDONLY | MS_SILENT) != 0 || raw[2] == 0 {
        return Err(Errno::InvalidArgument);
    }
    let mm = user.mm().expect("mount has no MM");
    if raw[0] != 0 {
        // 所选后端没有设备来源；字符串仍按普通 ABI 校验一次，不构造 Disk。
        mm.read_c_string(raw[0], PATH_LIMIT)?;
    }
    let target = mm.read_c_string(raw[1], PATH_LIMIT)?;
    let kind = mm.read_c_string(raw[2], PATH_LIMIT)?;
    let options = if raw[4] == 0 {
        None
    } else {
        Some(mm.read_c_string(raw[4], PATH_LIMIT)?)
    };
    let options = options.as_deref().unwrap_or(b"");
    let (filesystem, mode) = match kind.as_slice() {
        b"tmpfs" => {
            let root_mode = tmpfs_mode(options)?;
            let mode = if raw[3] & MS_RDONLY != 0 {
                AccessMode::ReadOnly
            } else {
                AccessMode::ReadWrite
            };
            (MemoryFs::try_new(MemoryFsKind::Tmpfs, root_mode)?, mode)
        }
        b"proc" | b"devfs" => {
            if !options.is_empty() {
                return Err(Errno::InvalidArgument);
            }
            let filesystem = if kind.as_slice() == b"proc" {
                procfs::try_new(0o555)?
            } else {
                devfs::try_new(0o755)?
            };
            (filesystem, AccessMode::ReadOnly)
        }
        _ => return Err(Errno::NoDevice),
    };
    let (vfs, context) = {
        let fs = runtime.fs.lock();
        (fs.vfs.clone(), fs.context.clone())
    };
    let path = vfs.resolve_from(
        &context,
        context.cwd(),
        &target,
        ResolveMode::Follow,
        Some((runtime.credentials.uid, runtime.credentials.gid)),
    )?;
    vfs.mount(&path, Filesystem::Memory(filesystem), mode)?;
    Ok(0)
}

/// 普通卸载仅接受零或 NOFOLLOW，消费本次查找所得真实子挂载根及其 owning 引用。
pub(super) fn umount(user: &UserThread, raw: &[usize; 6]) -> SysResult {
    let runtime = user.runtime().expect("umount has no runtime");
    if runtime.credentials.uid != 0 {
        return Err(Errno::NotPermitted);
    }
    if raw[1] & !UMOUNT_NOFOLLOW != 0 {
        return Err(Errno::InvalidArgument);
    }
    let mm = user.mm().expect("umount has no MM");
    let target = mm.read_c_string(raw[0], PATH_LIMIT)?;
    let (vfs, context) = {
        let fs = runtime.fs.lock();
        (fs.vfs.clone(), fs.context.clone())
    };
    let mode = if raw[1] & UMOUNT_NOFOLLOW != 0 {
        ResolveMode::NoFollow
    } else {
        ResolveMode::Follow
    };
    let path = vfs.resolve_from(
        &context,
        context.cwd(),
        &target,
        mode,
        Some((runtime.credentials.uid, runtime.credentials.gid)),
    )?;
    vfs.unmount_path(path)?;
    Ok(0)
}

/// 对当前 VFS 的全部真实挂载执行锁外同步，后端失败按原 errno 返回。
pub(super) fn sync(user: &UserThread) -> SysResult {
    let runtime = user.runtime().expect("sync has no runtime");
    let vfs = runtime.fs.lock().vfs.clone();
    vfs.sync()?;
    Ok(0)
}

/// 先核真实 uid 与 32 位 reboot 参数，停机意图交给系统图，当前用户只沿组退出离栈。
pub(super) fn reboot(user: &UserThread, raw: &[usize; 6]) -> Outcome {
    let runtime = user.runtime().expect("reboot has no runtime");
    if runtime.credentials.uid != 0 {
        return Outcome::Return(Errno::NotPermitted.result());
    }
    let magic1 = raw[0] as u32;
    let magic2 = raw[1] as u32;
    let command = raw[2] as u32;
    if magic1 != 0xfee1dead || !matches!(magic2, 672274793 | 85072278 | 369367448 | 537993216) {
        return Outcome::Return(Errno::InvalidArgument.result());
    }
    let operation = match command {
        // 串口平台没有 CAD 键事件源，其禁用状态固定；重复禁用不另造无人读取的开关。
        0 => return Outcome::Return(0),
        0x01234567 => ShutdownOperation::Restart,
        0xcdef0123 => ShutdownOperation::Halt,
        0x4321fedc => ShutdownOperation::PowerOff,
        _ => return Outcome::Return(Errno::InvalidArgument.result()),
    };
    let identity = user.identity().expect("reboot has no identity");
    let graph = identity.graph();
    if graph.root_role() != RootRole::System {
        return Outcome::Return(Errno::InvalidArgument.result());
    }
    match graph.request_shutdown(operation) {
        Ok(()) => Outcome::GroupExit(0),
        Err(error) => Outcome::Return(Errno::from(error).result()),
    }
}

/// 只解析唯一完整八进制 mode，空选项使用 1777；逗号、重复键及超出权限位均拒绝。
fn tmpfs_mode(options: &[u8]) -> Result<u16, Errno> {
    if options.is_empty() {
        return Ok(0o1777);
    }
    let digits = options
        .strip_prefix(b"mode=")
        .ok_or(Errno::InvalidArgument)?;
    if digits.is_empty() {
        return Err(Errno::InvalidArgument);
    }
    let mut mode = 0u16;
    for &digit in digits {
        if !(b'0'..=b'7').contains(&digit) {
            return Err(Errno::InvalidArgument);
        }
        mode = mode
            .checked_mul(8)
            .and_then(|value| value.checked_add(u16::from(digit - b'0')))
            .ok_or(Errno::InvalidArgument)?;
        if 0o7777 < mode {
            return Err(Errno::InvalidArgument);
        }
    }
    Ok(mode)
}
