//! 用户文件ABI沿正式表、OFD和逐组件VFS交付，不提前消费游标或借用户内存为Rust对象。

use alloc::sync::Arc;
use core::convert::TryFrom;

use crate::fs::{
    self, DescriptorFlags, DirectoryEntry, FileStat, FsContext, InodeKind, Path, ReadBuffer,
    ResolveMode, SeekFrom, Vfs, WriteBuffer,
};
use crate::mm::space::Mm;
use crate::syscall::error::{copyout, Errno, SysResult};
use crate::task::{UserRuntime, UserThread};

const AT_FDCWD: i32 = -100;
const CLOEXEC: u32 = 0x8_0000;
const DIRECTORY: u32 = 0x1_0000;
const NOFOLLOW: u32 = 0x2_0000;
const AT_SYMLINK_NOFOLLOW: usize = 0x100;
const AT_EMPTY_PATH: usize = 0x1000;
const MAX_PATH_BYTES: usize = 4096;
const MAX_RW_COUNT: usize = 0x7fff_f000;

/// 预留归属原文件表，锁外按真实访问模式检查路径权限后发表完整 OFD。
pub(super) fn openat(user: &UserThread, raw: [usize; 6]) -> SysResult {
    let runtime = user.runtime().expect("openat has no runtime");
    let mm = user.mm().expect("openat has no MM");
    let flags = raw[2] as u32;
    let reservation = runtime.files.reserve()?;
    let mode = if flags & NOFOLLOW != 0 {
        ResolveMode::NoFollow
    } else {
        ResolveMode::Follow
    };
    let path = resolve(&runtime, &mm, raw[0], raw[1], mode)?;
    if flags & DIRECTORY != 0 && path.inode()?.metadata_snapshot()?.kind != InodeKind::Directory {
        return Err(Errno::NotDirectory);
    }
    let access = match flags & 3 {
        0 => 4,
        1 => 2,
        2 => 6,
        _ => return Err(Errno::InvalidArgument),
    };
    let access = if flags & 0x200 != 0 {
        access | 2
    } else {
        access
    };
    path.check_access(runtime.credentials.uid, runtime.credentials.gid, access)?;
    let file = fs::open_path_with_flags(path, flags)?;
    Ok(reservation.publish(
        file,
        DescriptorFlags {
            close_on_exec: flags & CLOEXEC != 0,
        },
    ) as isize)
}

/// 查表先取得稳定OFD，用户接收端只按真实交付前缀推进共享位置。
pub(super) fn read(user: &UserThread, raw: [usize; 6]) -> SysResult {
    let runtime = user.runtime().expect("read has no runtime");
    let file = runtime.files.lookup(descriptor(raw[0])?)?;
    if file.flags() & 3 == 1 {
        return Err(Errno::BadFd);
    }
    let mm = user.mm().expect("read has no MM");
    let mut target = ReadBuffer::user(&mm, raw[1], raw[2].min(MAX_RW_COUNT))?;
    Ok(file.read_into(&mut target)? as isize)
}

/// 有符号负offset明确失败，按位置读不获取OFD位置锁，也不改dup共享游标。
pub(super) fn pread(user: &UserThread, raw: [usize; 6]) -> SysResult {
    if (raw[3] as i64) < 0 {
        return Err(Errno::InvalidArgument);
    }
    let runtime = user.runtime().expect("pread has no runtime");
    let file = runtime.files.lookup(descriptor(raw[0])?)?;
    if file.flags() & 3 == 1 {
        return Err(Errno::BadFd);
    }
    let mm = user.mm().expect("pread has no MM");
    let mut target = ReadBuffer::user(&mm, raw[1], raw[2].min(MAX_RW_COUNT))?;
    Ok(file.read_at_into(raw[3] as u64, &mut target)? as isize)
}

/// 用户输入绑定本次真实 Mm，文件只按已接受前缀更新共享位置。
pub(super) fn write(user: &UserThread, raw: [usize; 6]) -> SysResult {
    let runtime = user.runtime().expect("write has no runtime");
    let file = runtime.files.lookup(descriptor(raw[0])?)?;
    if file.flags() & 3 == 0 {
        return Err(Errno::BadFd);
    }
    let mm = user.mm().expect("write has no MM");
    let mut source = WriteBuffer::user(&mm, raw[1], raw[2].min(MAX_RW_COUNT))?;
    Ok(file.write_from(&mut source)? as isize)
}

/// 负偏移先拒绝，按位置写不改变 OFD 游标，追加状态由同一 inode 判断。
pub(super) fn pwrite(user: &UserThread, raw: [usize; 6]) -> SysResult {
    if (raw[3] as i64) < 0 {
        return Err(Errno::InvalidArgument);
    }
    let runtime = user.runtime().expect("pwrite has no runtime");
    let file = runtime.files.lookup(descriptor(raw[0])?)?;
    if file.flags() & 3 == 0 {
        return Err(Errno::BadFd);
    }
    let mm = user.mm().expect("pwrite has no MM");
    let mut source = WriteBuffer::user(&mm, raw[1], raw[2].min(MAX_RW_COUNT))?;
    Ok(file.write_at_from(raw[3] as u64, &mut source)? as isize)
}

/// 共享位置的位移在原OFD锁中提交，不能把失败后的游标在表层回滚。
pub(super) fn seek(user: &UserThread, raw: [usize; 6]) -> SysResult {
    let delta = raw[1] as i64;
    let from = match raw[2] as u32 {
        0 if 0 <= delta => SeekFrom::Start(delta as u64),
        1 => SeekFrom::Current(delta),
        2 => SeekFrom::End(delta),
        _ => return Err(Errno::InvalidArgument),
    };
    let runtime = user.runtime().expect("lseek has no runtime");
    let position = runtime.files.seek(descriptor(raw[0])?, from)?;
    isize::try_from(position).map_err(|_| Errno::Overflow)
}

/// slot关闭后锁外释放OFD，已查出的并发I/O拥有仍指原文件。
pub(super) fn close(user: &UserThread, raw: usize) -> SysResult {
    user.runtime()
        .expect("close has no runtime")
        .files
        .close(descriptor(raw)?)?;
    Ok(0)
}

/// 新descriptor共享原OFD，自己的CLOEXEC清除。
pub(super) fn dup(user: &UserThread, raw: usize) -> SysResult {
    Ok(user
        .runtime()
        .expect("dup has no runtime")
        .files
        .dup(descriptor(raw)?)? as isize)
}

/// dup3只接受CLOEXEC且同号EINVAL，替换释放责任由原表在锁外承担。
pub(super) fn dup3(user: &UserThread, raw: [usize; 6]) -> SysResult {
    if raw[2] as u32 & !CLOEXEC != 0 {
        return Err(Errno::InvalidArgument);
    }
    let runtime = user.runtime().expect("dup3 has no runtime");
    Ok(runtime.files.dup_exact_flags(
        descriptor(raw[0])?,
        descriptor(raw[1])?,
        DescriptorFlags {
            close_on_exec: raw[2] as u32 & CLOEXEC != 0,
        },
    )? as isize)
}

/// fd属性和OFD状态保持各自归属，未知命令不默认成功。
pub(super) fn fcntl(user: &UserThread, raw: [usize; 6]) -> SysResult {
    let runtime = user.runtime().expect("fcntl has no runtime");
    let fd = descriptor(raw[0])?;
    match raw[1] as u32 {
        0 | 1030 => {
            let minimum = raw[2] as i32;
            if minimum < 0 || runtime.files.capacity() <= minimum as usize {
                return Err(Errno::InvalidArgument);
            }
            Ok(runtime.files.dup_min(
                fd,
                minimum as usize,
                DescriptorFlags {
                    close_on_exec: raw[1] as u32 == 1030,
                },
            )? as isize)
        }
        1 => Ok(runtime.files.flags(fd)?.close_on_exec as isize),
        2 => {
            runtime.files.set_flags(
                fd,
                DescriptorFlags {
                    close_on_exec: raw[2] & 1 != 0,
                },
            )?;
            Ok(0)
        }
        3 => Ok(runtime.files.lookup(fd)?.flags() as isize),
        4 => {
            runtime.files.lookup(fd)?.set_status_flags(raw[2] as u32)?;
            Ok(0)
        }
        _ => Err(Errno::InvalidArgument),
    }
}

/// fd的真实属性先完整编码为128字节快照，再在表锁外一次交付。
pub(super) fn fstat(user: &UserThread, raw: [usize; 6]) -> SysResult {
    let runtime = user.runtime().expect("fstat has no runtime");
    let stat = runtime.files.stat(descriptor(raw[0])?)?;
    let bytes = encode_stat(stat)?;
    copyout(&user.mm().expect("fstat has no MM"), raw[1], &bytes)?;
    Ok(0)
}

/// statat按真实dirfd和最后链接选项查询，空路径只在显式EMPTY_PATH时查原对象。
pub(super) fn statat(user: &UserThread, raw: [usize; 6]) -> SysResult {
    if raw[3] & !(AT_SYMLINK_NOFOLLOW | AT_EMPTY_PATH) != 0 {
        return Err(Errno::InvalidArgument);
    }
    let runtime = user.runtime().expect("statat has no runtime");
    let mm = user.mm().expect("statat has no MM");
    let pathname = mm.read_c_string(raw[1], MAX_PATH_BYTES)?;
    let stat = if pathname.is_empty() && raw[3] & AT_EMPTY_PATH != 0 {
        if raw[0] as i32 == AT_FDCWD {
            let (_, context) = fs_snapshot(&runtime);
            fs::stat_path(context.cwd())?
        } else {
            runtime.files.stat(descriptor(raw[0])?)?
        }
    } else {
        let mode = if raw[3] & AT_SYMLINK_NOFOLLOW != 0 {
            ResolveMode::NoFollow
        } else {
            ResolveMode::Follow
        };
        fs::stat_path(&resolve_bytes(&runtime, raw[0], &pathname, mode)?)?
    };
    copyout(&mm, raw[2], &encode_stat(stat)?)?;
    Ok(0)
}

/// 只在一条dirent完整写入后授权OFD提交cookie，半条记录不算返回前缀。
pub(super) fn getdents(user: &UserThread, raw: [usize; 6]) -> SysResult {
    let runtime = user.runtime().expect("getdents has no runtime");
    let file = runtime.files.lookup(descriptor(raw[0])?)?;
    let mm = user.mm().expect("getdents has no MM");
    let mut target = ReadBuffer::user(&mm, raw[1], raw[2].min(u32::MAX as usize))?;
    let mut complete = 0usize;
    let result = file.for_each_dirent(&mut |entry| {
        let length = (19 + entry.name.len() + 1 + 7) & !7;
        if target.len() - complete < length {
            return if complete == 0 {
                Err(fs::FsError::InvalidOperation)
            } else {
                Ok(false)
            };
        }
        let mut bytes = [0u8; 280];
        bytes[0..8].copy_from_slice(&entry.number.to_le_bytes());
        bytes[8..16].copy_from_slice(&entry.next_offset.to_le_bytes());
        bytes[16..18].copy_from_slice(&(length as u16).to_le_bytes());
        bytes[18] = dirent_type(entry);
        bytes[19..19 + entry.name.len()].copy_from_slice(&entry.name);
        if target.push(&bytes[..length])? != length {
            return Err(fs::FsError::BadAddress);
        }
        complete += length;
        Ok(true)
    });
    if 0 < complete {
        Ok(complete as isize)
    } else {
        result.map(|()| 0).map_err(Errno::from)
    }
}

/// 链接只交付原始目标字节且不补NUL，故障保持已写内存但返回EFAULT。
pub(super) fn readlink(user: &UserThread, raw: [usize; 6]) -> SysResult {
    let count = raw[3] as i32;
    if count <= 0 {
        return Err(Errno::InvalidArgument);
    }
    let runtime = user.runtime().expect("readlink has no runtime");
    let mm = user.mm().expect("readlink has no MM");
    let path = resolve(&runtime, &mm, raw[0], raw[1], ResolveMode::NoFollow)?;
    if path.inode()?.metadata_snapshot()?.kind != InodeKind::Symlink {
        return Err(Errno::InvalidArgument);
    }
    let target = path.inode()?.read_link()?;
    let count = target.len().min(count as usize);
    copyout(&mm, raw[2], &target[..count])?;
    Ok(count as isize)
}

/// 查找和目录/root验证全部锁外完成，共享FS短提交仅移动新快照。
pub(super) fn chdir(user: &UserThread, address: usize) -> SysResult {
    let runtime = user.runtime().expect("chdir has no runtime");
    let mm = user.mm().expect("chdir has no MM");
    let path = resolve(
        &runtime,
        &mm,
        AT_FDCWD as usize,
        address,
        ResolveMode::Follow,
    )?;
    let (_, context) = fs_snapshot(&runtime);
    let replacement = FsContext::new(context.root().clone(), path)?;
    replacement
        .cwd()
        .check_access(runtime.credentials.uid, runtime.credentials.gid, 1)?;
    let retired = {
        let mut current = runtime.fs.lock();
        core::mem::replace(&mut current.context, replacement)
    };
    drop(retired);
    Ok(0)
}

/// 文件号码以目标int宽度解释，负值不能转为巨大的slot下标。
fn descriptor(raw: usize) -> Result<usize, Errno> {
    let fd = raw as i32;
    if fd < 0 {
        Err(Errno::BadFd)
    } else {
        Ok(fd as usize)
    }
}

/// 取得稳定VFS/root/cwd引用后释放共享FS锁，不在整条路径/I/O上持它。
fn fs_snapshot(runtime: &UserRuntime) -> (Arc<Vfs>, FsContext) {
    let current = runtime.fs.lock();
    (current.vfs.clone(), current.context.clone())
}

/// 路径只从当前Mm读取一次原始NUL字节串，不要求UTF8。
fn resolve(
    runtime: &UserRuntime,
    mm: &Mm,
    dirfd: usize,
    address: usize,
    mode: ResolveMode,
) -> Result<Path, Errno> {
    let pathname = mm.read_c_string(address, MAX_PATH_BYTES)?;
    resolve_bytes(runtime, dirfd, &pathname, mode)
}

/// 绝对路径忽略dirfd，相对路径使用已取得的真实目录Path，每次search权限由VFS执行。
pub(super) fn resolve_bytes(
    runtime: &UserRuntime,
    dirfd: usize,
    pathname: &[u8],
    mode: ResolveMode,
) -> Result<Path, Errno> {
    if pathname.is_empty() {
        return Err(Errno::NotFound);
    }
    let (vfs, context) = fs_snapshot(runtime);
    let start = if pathname[0] == b'/' {
        context.root().clone()
    } else if dirfd as i32 == AT_FDCWD {
        context.cwd().clone()
    } else {
        let file = runtime.files.lookup(descriptor(dirfd)?)?;
        if file.stat()?.kind != InodeKind::Directory {
            return Err(Errno::NotDirectory);
        }
        file.path().clone()
    };
    vfs.resolve_from(
        &context,
        &start,
        pathname,
        mode,
        Some((runtime.credentials.uid, runtime.credentials.gid)),
    )
    .map_err(Errno::from)
}

/// 两目标使用同一asm-generic stat布局，所有padding明确初始化，不伪造盘面属性。
fn encode_stat(stat: FileStat) -> Result<[u8; 128], Errno> {
    let size = i64::try_from(stat.size).map_err(|_| Errno::Overflow)?;
    let blocks = i64::try_from(stat.blocks).map_err(|_| Errno::Overflow)?;
    let mut bytes = [0u8; 128];
    put_u64(&mut bytes, 0, stat.device);
    put_u64(&mut bytes, 8, stat.inode);
    put_u32(&mut bytes, 16, u32::from(stat.mode));
    put_u32(&mut bytes, 20, stat.links);
    put_u32(&mut bytes, 24, stat.uid);
    put_u32(&mut bytes, 28, stat.gid);
    put_u64(&mut bytes, 48, size as u64);
    put_u32(&mut bytes, 56, stat.block_size);
    put_u64(&mut bytes, 64, blocks as u64);
    for (offset, time) in [(72, stat.atime), (88, stat.mtime), (104, stat.ctime)] {
        put_u64(&mut bytes, offset, time.seconds as u64);
        put_u64(&mut bytes, offset + 8, u64::from(time.nanoseconds));
    }
    Ok(bytes)
}

/// 完整初始化快照中固定八字节字段编码，偏移只由此模块的ABI常量提供。
fn put_u64(bytes: &mut [u8; 128], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

/// 完整初始化快照中固定四字节字段编码，不从用户字节构造Rust布局对象。
fn put_u32(bytes: &mut [u8; 128], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

/// 未知后端类型保DT_UNKNOWN，目录记录不能凭名字猜类型。
fn dirent_type(entry: &DirectoryEntry) -> u8 {
    match entry.kind {
        Some(InodeKind::Regular) => 8,
        Some(InodeKind::Directory) => 4,
        Some(InodeKind::Symlink) => 10,
        _ => 0,
    }
}
