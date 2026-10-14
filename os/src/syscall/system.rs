//! 系统身份、挂载、同步与关机准入消费当前用户的真实 MM、VFS、凭据和身份图。

use core::convert::TryFrom;

use crate::block::AccessMode;
use crate::fs::memfs::{FilesystemStat, MemoryFs, MemoryFsKind};
use crate::fs::system::device_index;
use crate::fs::{devfs, procfs, Ext4Volume, Filesystem, Path, ResolveMode};
use crate::storage::Storage;
use crate::syscall::error::{copyout, Errno, SysResult};
use crate::syscall::Outcome;
use crate::task::process::{RootRole, ShutdownOperation};
use crate::task::UserThread;
use crate::{mm, sched, time};

const PATH_LIMIT: usize = 4096;
const MS_RDONLY: usize = 1;
const MS_SILENT: usize = 0x8000;
const UMOUNT_NOFOLLOW: usize = 8;
const STATFS_SIZE: usize = 120;
const ST_RDONLY: u64 = 1;
const ST_VALID: u64 = 0x20;

const UTS_FIELD_LEN: usize = 65;
#[cfg(target_arch = "riscv64")]
const UTS_MACHINE: &[u8] = b"riscv64";
#[cfg(target_arch = "loongarch64")]
const UTS_MACHINE: &[u8] = b"loongarch64";
static UTS_NAME: [u8; UTS_FIELD_LEN * 6] = uname_bytes();

/// 向本次 MM 交付固定系统身份；用户访问可处理缺页并睡眠，部分故障保留已写前缀并返回 EFAULT。
pub(super) fn uname(user: &UserThread, raw: &[usize; 6]) -> SysResult {
    let mm = user.mm().expect("uname has no MM");
    copyout(&mm, raw[0], &UTS_NAME)?;
    Ok(0)
}

/// 交付 LP64 sysinfo 的实际 RAM、运行时间及活执行者快照，复制期间不持调度或 MM 锁。
pub(super) fn sysinfo(user: &UserThread, raw: &[usize; 6]) -> SysResult {
    let memory = mm::stats();
    let elapsed = time::now().as_nanos();
    let uptime =
        u64::try_from(elapsed.div_ceil(1_000_000_000)).expect("sysinfo uptime exceeds Linux long");
    let fields = [
        uptime,
        0,
        0,
        0,
        (memory.present_pages - memory.reserved_pages - memory.metadata_pages) as u64,
        (memory.buddy_free_pages + memory.pcp_cached_pages) as u64,
        0,
        0,
        0,
        0,
    ];
    let mut bytes = [0u8; 112];
    for (index, value) in fields.iter().enumerate() {
        bytes[index * 8..index * 8 + 8].copy_from_slice(&value.to_le_bytes());
    }
    // ABI: procs 位于80，两个high字段位于88/96，mem_unit位于104；显式填零全部padding。
    // 没有 swap 或 highmem；共享页、buffer 和 load average 尚无独立统计来源。
    bytes[80..82].copy_from_slice(&(sched::live_task_count() as u16).to_le_bytes());
    bytes[104..108].copy_from_slice(&(mm::PAGE_SIZE as u32).to_le_bytes());
    let mm = user.mm().expect("sysinfo has no MM");
    copyout(&mm, raw[0], &bytes)?;
    Ok(0)
}

/// 在编译期构造六个 NUL 结尾、余部为零的固定身份字段，不依赖 Rust 结构体布局。
const fn uname_bytes() -> [u8; UTS_FIELD_LEN * 6] {
    let fields: [&[u8]; 6] = [
        b"Linux",
        b"blossom",
        b"6.12.0-blossom",
        env!("CARGO_PKG_VERSION").as_bytes(),
        UTS_MACHINE,
        b"(none)",
    ];
    let mut bytes = [0; UTS_FIELD_LEN * 6];
    let mut field = 0;
    while field < fields.len() {
        assert!(fields[field].len() < UTS_FIELD_LEN);
        let mut index = 0;
        while index < fields[field].len() {
            bytes[field * UTS_FIELD_LEN + index] = fields[field][index];
            index += 1;
        }
        field += 1;
    }
    bytes
}

/// 一次导入路径并沿当前 root/cwd 跟随链接，采样所选实际挂载后在锁外交付容量。
pub(super) fn statfs(user: &UserThread, raw: &[usize; 6]) -> SysResult {
    let mm = user.mm().expect("statfs has no MM");
    let pathname = mm.read_c_string(raw[0], PATH_LIMIT)?;
    let runtime = user.runtime().expect("statfs has no runtime");
    let (vfs, context) = {
        let fs = runtime.fs.lock();
        (fs.vfs.clone(), fs.context.clone())
    };
    let path = vfs.resolve_from(
        &context,
        context.cwd(),
        &pathname,
        ResolveMode::Follow,
        Some((runtime.credentials.uid, runtime.credentials.gid)),
    )?;
    write_statfs(user, &path, raw[1])
}

/// 一次取得 fd 的原 OFD并借其 Path；无效 fd 返回 EBADF，未具文件系统来源的对象保 ENOSYS。
pub(super) fn fstatfs(user: &UserThread, raw: &[usize; 6]) -> SysResult {
    let fd = raw[0] as i32;
    if fd < 0 {
        return Err(Errno::BadFd);
    }
    let runtime = user.runtime().expect("fstatfs has no runtime");
    let file = runtime.files.lookup(fd as usize)?;
    let path = file.path().ok_or(Errno::NoSyscall)?;
    write_statfs(user, path, raw[1])
}

/// 后端采样结束后一次交付完整字节，用户缺页和部分写入故障沿原 copyout 返回 EFAULT。
fn write_statfs(user: &UserThread, path: &Path, address: usize) -> SysResult {
    let stat = path.filesystem().statfs()?;
    let bytes = encode_statfs(stat, path.mount_mode());
    let mm = user.mm().expect("statfs copyout has no MM");
    copyout(&mm, address, &bytes)?;
    Ok(0)
}

/// 编码 asm-generic LP64 的 120 字节布局，所有保留字节为零，不创建用户结构体引用。
fn encode_statfs(stat: FilesystemStat, mode: AccessMode) -> [u8; STATFS_SIZE] {
    let flags = ST_VALID
        | if mode == AccessMode::ReadOnly {
            ST_RDONLY
        } else {
            0
        };
    // ABI: 前七项各占八字节；fsid 是低、高两个四字节字，之后依次为 namelen、frsize 和 flags。
    let fields = [
        stat.fs_type,
        stat.block_size,
        stat.blocks,
        stat.free,
        stat.available,
        stat.files,
        stat.freefiles,
        stat.device,
        255,
        stat.block_size,
        flags,
    ];
    let mut bytes = [0u8; STATFS_SIZE];
    for (index, value) in fields.iter().enumerate() {
        bytes[index * 8..index * 8 + 8].copy_from_slice(&value.to_le_bytes());
    }
    bytes
}

/// uid0 才可挂载原设备的 ext4 或独立内存后端；输入各读取一次，锁外准备后沿原 Mount 发表。
pub(super) fn mount(user: &UserThread, raw: &[usize; 6]) -> SysResult {
    let runtime = user.runtime().expect("mount has no runtime");
    if runtime.credentials.uid != 0 {
        return Err(Errno::NotPermitted);
    }
    if raw[3] & !(MS_RDONLY | MS_SILENT) != 0 || raw[2] == 0 {
        return Err(Errno::InvalidArgument);
    }
    let mm = user.mm().expect("mount has no MM");
    let source = if raw[0] == 0 {
        None
    } else {
        Some(mm.read_c_string(raw[0], PATH_LIMIT)?)
    };
    let target = mm.read_c_string(raw[1], PATH_LIMIT)?;
    let kind = mm.read_c_string(raw[2], PATH_LIMIT)?;
    let options = if raw[4] == 0 {
        None
    } else {
        Some(mm.read_c_string(raw[4], PATH_LIMIT)?)
    };
    let options = options.as_deref().unwrap_or(b"");
    let requested_mode = if raw[3] & MS_RDONLY != 0 {
        AccessMode::ReadOnly
    } else {
        AccessMode::ReadWrite
    };
    let (filesystem, mode) = match kind.as_slice() {
        b"ext4" => {
            if !options.is_empty() {
                return Err(Errno::InvalidArgument);
            }
            let source = source.as_deref().ok_or(Errno::NoDevice)?;
            let index = device_index(source).map_err(|_| Errno::NoDevice)?;
            let storage = Storage::get().ok_or(Errno::NoDevice)?;
            let disk = storage.disk(index).ok_or(Errno::NoDevice)?;
            let volume = Ext4Volume::open(disk, requested_mode, 64)?;
            (Filesystem::Ext4(volume), requested_mode)
        }
        b"tmpfs" => {
            let root_mode = tmpfs_mode(options)?;
            let filesystem = MemoryFs::try_new(MemoryFsKind::Tmpfs, root_mode)?;
            (Filesystem::Memory(filesystem), requested_mode)
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
            (Filesystem::Memory(filesystem), AccessMode::ReadOnly)
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
    vfs.mount(&path, filesystem, mode)?;
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
