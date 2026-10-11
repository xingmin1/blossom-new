//! 用户文件ABI沿正式表、OFD和逐组件VFS交付，不提前消费游标或借用户内存为Rust对象。

use alloc::borrow::Cow;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::convert::TryFrom;

use crate::arch;
use crate::fs::{
    self, DescriptorFlags, DirectoryEntry, FileIoContext, FileStat, FsContext, InodeKind, Path,
    ReadBuffer, ResolveMode, SeekFrom, UserIoVector, Vfs, WriteBuffer,
};
use crate::mm::space::Mm;
use crate::syscall::error::{copyout, Errno, SysResult};
use crate::task::{UserRuntime, UserThread};

const AT_FDCWD: i32 = -100;
const TRUNC: u32 = 0x200;
const CREATE: u32 = 0o100;
const EXCLUSIVE: u32 = 0o200;
const CLOEXEC: u32 = 0x8_0000;
const DIRECTORY: u32 = 0x1_0000;
const NOFOLLOW: u32 = 0x2_0000;
const AT_SYMLINK_NOFOLLOW: usize = 0x100;
const AT_EMPTY_PATH: usize = 0x1000;
const AT_NO_AUTOMOUNT: u32 = 0x800;
const AT_STATX_SYNC_TYPE: u32 = 0x6000;
const STATX_BASIC_STATS: u32 = 0x7ff;
const STATX_RESERVED: u32 = 0x8000_0000;
const AT_REMOVEDIR: u32 = 0x200;
const AT_SYMLINK_FOLLOW: u32 = 0x400;
const RENAME_NOREPLACE: u32 = 1;
const OPENAT_SUPPORTED: u32 = 0o3
    | CREATE
    | EXCLUSIVE
    | TRUNC
    | 0o2000
    | 0o4000
    | 0x101000
    | 0o100000
    | DIRECTORY
    | NOFOLLOW
    | 0o400
    | CLOEXEC;
const MAX_PATH_BYTES: usize = 4096;
const MAX_RW_COUNT: usize = 0x7fff_f000;
const MAX_IOV_COUNT: usize = 1024;

/// 只接受两个管道打开位；两个原预留槽在完整 copyout 成功后才分别发表。
pub(super) fn pipe2(user: &UserThread, raw: [usize; 6]) -> SysResult {
    let flags = raw[1] as u32;
    if flags & !(CLOEXEC | 0o4000) != 0 {
        return Err(Errno::InvalidArgument);
    }
    let runtime = user.runtime().expect("pipe2 has no runtime");
    let reader_slot = runtime.files.reserve()?;
    let writer_slot = runtime.files.reserve()?;
    let (reader, writer) =
        fs::pipe::try_new(flags, runtime.credentials.uid, runtime.credentials.gid)?;
    let mut bytes = [0u8; 8];
    bytes[..4].copy_from_slice(&(reader_slot.fd() as u32).to_le_bytes());
    bytes[4..].copy_from_slice(&(writer_slot.fd() as u32).to_le_bytes());
    let mm = user.mm().expect("pipe2 has no MM");
    copyout(&mm, raw[0], &bytes)?;
    let descriptor_flags = DescriptorFlags {
        close_on_exec: flags & CLOEXEC != 0,
    };
    reader_slot.publish(reader, descriptor_flags);
    writer_slot.publish(writer, descriptor_flags);
    Ok(0)
}

/// 一次读取目标 16 字节 iovec 数组，小请求借原栈快照，大请求才分配有限拥有。
fn import_vectors<'a>(
    mm: &Mm,
    address: usize,
    count: usize,
    fast: &'a mut [UserIoVector; 8],
) -> Result<Cow<'a, [UserIoVector]>, Errno> {
    // raw vlen 保留目标 unsigned long 宽度，所有负编码都超出有限上界。
    if MAX_IOV_COUNT < count {
        return Err(Errno::InvalidArgument);
    }
    if count == 0 {
        return Ok(Cow::Borrowed(&fast[..0]));
    }
    let mut heap = Vec::new();
    let vectors = if count <= fast.len() {
        &mut fast[..count]
    } else {
        heap.try_reserve_exact(count).map_err(|_| Errno::NoMemory)?;
        heap.resize(
            count,
            UserIoVector {
                address: 0,
                length: 0,
            },
        );
        heap.as_mut_slice()
    };
    assert_eq!(core::mem::size_of::<UserIoVector>(), 16);
    // SAFETY: repr(C) 两个64位usize与目标iovec布局相同，所有位模式均有效；
    // vectors独占且已初始化count项，字节切片长度恰覆盖这些项，copyin不保留借用。
    let bytes =
        unsafe { core::slice::from_raw_parts_mut(vectors.as_mut_ptr().cast::<u8>(), count * 16) };
    let progress = mm.copyin(bytes, address);
    if progress.error.is_some() || progress.copied != bytes.len() {
        return Err(Errno::BadAddress);
    }
    if vectors.iter().any(|vector| (vector.length as isize) < 0) {
        return Err(Errno::InvalidArgument);
    }
    let mut total = 0usize;
    for vector in vectors.iter_mut() {
        // 单段在裁剪后检查范围，多段检查原始范围；空段不解引用base。
        let checked_length = if count == 1 {
            vector.length.min(MAX_RW_COUNT)
        } else {
            vector.length
        };
        if checked_length != 0 {
            let end = vector
                .address
                .checked_add(checked_length)
                .ok_or(Errno::BadAddress)?;
            if arch::USER_ADDRESS_END < end {
                return Err(Errno::BadAddress);
            }
        }
        vector.length = vector.length.min(MAX_RW_COUNT - total);
        total += vector.length;
    }
    if count <= fast.len() {
        Ok(Cow::Borrowed(&fast[..count]))
    } else {
        Ok(Cow::Owned(heap))
    }
}

/// 沿原 fd 和 fmode 错误顺序进入一次 OFD 读取，向量故障保真实交付前缀。
pub(super) fn readv(user: &UserThread, raw: [usize; 6]) -> SysResult {
    let runtime = user.runtime().expect("readv has no runtime");
    let file = runtime.files.lookup(descriptor(raw[0])?)?;
    if file.flags() & 3 == 1 {
        return Err(Errno::BadFd);
    }
    let mm = user.mm().expect("readv has no MM");
    let mut fast = [UserIoVector {
        address: 0,
        length: 0,
    }; 8];
    let vectors = import_vectors(&mm, raw[1], raw[2], &mut fast)?;
    let mut target = ReadBuffer::user_vector(&mm, &vectors)?;
    if target.len() == 0 {
        return Ok(0);
    }
    Ok(file.read_into(&mut target, FileIoContext::User(user))? as isize)
}

/// 总向量是一笔写请求，只进入一次 OFD 和 inode 写事务，不逐段提交位置。
pub(super) fn writev(user: &UserThread, raw: [usize; 6]) -> SysResult {
    let runtime = user.runtime().expect("writev has no runtime");
    let file = runtime.files.lookup(descriptor(raw[0])?)?;
    if file.flags() & 3 == 0 {
        return Err(Errno::BadFd);
    }
    let mm = user.mm().expect("writev has no MM");
    let mut fast = [UserIoVector {
        address: 0,
        length: 0,
    }; 8];
    let vectors = import_vectors(&mm, raw[1], raw[2], &mut fast)?;
    let mut source = WriteBuffer::user_vector(&mm, &vectors)?;
    if source.len() == 0 {
        return Ok(0);
    }
    Ok(file.write_from(&mut source, FileIoContext::User(user))? as isize)
}

/// 先预留描述符，再让 VFS 决定最终名字；创建分支在名字发表前构造完整 OFD。
pub(super) fn openat(user: &UserThread, raw: [usize; 6]) -> SysResult {
    let runtime = user.runtime().expect("openat has no runtime");
    let mm = user.mm().expect("openat has no MM");
    let flags = raw[2] as u32;
    let reservation = runtime.files.reserve()?;
    if flags & 3 == 3 {
        return Err(Errno::InvalidArgument);
    }
    if flags & !OPENAT_SUPPORTED != 0 {
        return Err(Errno::Unsupported);
    }
    if flags & EXCLUSIVE != 0 && flags & CREATE == 0 {
        return Err(Errno::InvalidArgument);
    }
    let pathname = mm.read_c_string(raw[1], MAX_PATH_BYTES)?;
    let (vfs, context) = fs_snapshot(&runtime);
    let start = resolve_start(&runtime, &context, raw[0], &pathname)?;
    let credentials = (runtime.credentials.uid, runtime.credentials.gid);
    let file = if flags & CREATE != 0 {
        vfs.open_with(
            &context,
            &start,
            &pathname,
            credentials,
            Some((raw[3] as u16, context.umask())),
            flags & EXCLUSIVE != 0,
            flags & NOFOLLOW != 0,
            |path, created| {
                prepare_open(
                    &vfs,
                    path,
                    flags,
                    credentials,
                    created,
                    FileIoContext::User(user),
                )
            },
        )?
    } else {
        let mode = if flags & NOFOLLOW != 0 {
            ResolveMode::NoFollow
        } else {
            ResolveMode::Follow
        };
        let path = vfs.resolve_from(&context, &start, &pathname, mode, Some(credentials))?;
        prepare_open(
            &vfs,
            path,
            flags,
            credentials,
            false,
            FileIoContext::User(user),
        )?
    };
    Ok(reservation.publish(
        file,
        DescriptorFlags {
            close_on_exec: flags & CLOEXEC != 0,
        },
    ) as isize)
}
/// 按最终 inode 决定 DAC 与 OFD；刚创建的空对象不重新核自身 mode，也不执行 O_TRUNC。
fn prepare_open(
    vfs: &Vfs,
    path: Path,
    flags: u32,
    credentials: (u32, u32),
    created: bool,
    caller: FileIoContext<'_>,
) -> Result<fs::FileRef, fs::FsError> {
    let access = flags & 3;
    if access == 3 {
        return Err(fs::FsError::InvalidOperation);
    }
    let kind = path.inode()?.metadata_snapshot()?.kind;
    if flags & CREATE != 0 && !created && kind == InodeKind::Directory {
        return Err(fs::FsError::IsDirectory);
    }
    if flags & DIRECTORY != 0 && kind != InodeKind::Directory {
        return Err(fs::FsError::NotDirectory);
    }
    if !created {
        let access = match access {
            0 => 4,
            1 => 2,
            2 => 6,
            _ => unreachable!(),
        };
        let access = if flags & TRUNC != 0 {
            access | 2
        } else {
            access
        };
        path.check_access(credentials.0, credentials.1, access)?;
    }
    let flags = flags & !(CREATE | EXCLUSIVE | if created { TRUNC } else { 0 });
    fs::open_path_with_flags(vfs, path, flags, caller)
}
/// mkdirat 按本次 FsContext umask 建目录，路径逐组件从当前目录或真实 dirfd 开始。
pub(super) fn mkdirat(user: &UserThread, raw: [usize; 6]) -> SysResult {
    let runtime = user.runtime().expect("mkdirat has no runtime");
    let mm = user.mm().expect("mkdirat has no MM");
    let pathname = mm.read_c_string(raw[1], MAX_PATH_BYTES)?;
    let (vfs, context) = fs_snapshot(&runtime);
    let start = resolve_start(&runtime, &context, raw[0], &pathname)?;
    let credentials = (runtime.credentials.uid, runtime.credentials.gid);
    vfs.mkdir_from(
        &context,
        &start,
        &pathname,
        raw[2] as u16,
        credentials,
        context.umask(),
    )?;
    Ok(0)
}

/// unlinkat 仅接受 AT_REMOVEDIR；具体 unlink 或 rmdir 由真实目录后端执行。
pub(super) fn unlinkat(user: &UserThread, raw: [usize; 6]) -> SysResult {
    let flags = raw[2] as u32;
    if flags & !AT_REMOVEDIR != 0 {
        return Err(Errno::InvalidArgument);
    }
    let runtime = user.runtime().expect("unlinkat has no runtime");
    let mm = user.mm().expect("unlinkat has no MM");
    let pathname = mm.read_c_string(raw[1], MAX_PATH_BYTES)?;
    let (vfs, context) = fs_snapshot(&runtime);
    let start = resolve_start(&runtime, &context, raw[0], &pathname)?;
    let credentials = (runtime.credentials.uid, runtime.credentials.gid);
    if flags & AT_REMOVEDIR != 0 {
        vfs.rmdir_from(&context, &start, &pathname, credentials)?;
    } else {
        vfs.unlink_from(&context, &start, &pathname, credentials)?;
    }
    Ok(0)
}

/// symlinkat 保留目标和链接名的原始字节；只有链接名按当前 FsContext 解析。
pub(super) fn symlinkat(user: &UserThread, raw: [usize; 6]) -> SysResult {
    let runtime = user.runtime().expect("symlinkat has no runtime");
    let mm = user.mm().expect("symlinkat has no MM");
    let target = mm.read_c_string(raw[0], MAX_PATH_BYTES)?;
    let pathname = mm.read_c_string(raw[2], MAX_PATH_BYTES)?;
    let (vfs, context) = fs_snapshot(&runtime);
    let start = resolve_start(&runtime, &context, raw[1], &pathname)?;
    let credentials = (runtime.credentials.uid, runtime.credentials.gid);
    vfs.symlink_from(&context, &start, &pathname, &target, credentials)?;
    Ok(0)
}

/// linkat 默认链接原末 symlink；AT_SYMLINK_FOLLOW 只改变源路径的最终跟随。
pub(super) fn linkat(user: &UserThread, raw: [usize; 6]) -> SysResult {
    let flags = raw[4] as u32;
    if flags & !AT_SYMLINK_FOLLOW != 0 {
        return Err(Errno::InvalidArgument);
    }
    let runtime = user.runtime().expect("linkat has no runtime");
    let mm = user.mm().expect("linkat has no MM");
    let old_pathname = mm.read_c_string(raw[1], MAX_PATH_BYTES)?;
    let new_pathname = mm.read_c_string(raw[3], MAX_PATH_BYTES)?;
    let (vfs, context) = fs_snapshot(&runtime);
    let old_start = resolve_start(&runtime, &context, raw[0], &old_pathname)?;
    let new_start = resolve_start(&runtime, &context, raw[2], &new_pathname)?;
    let mode = if flags & AT_SYMLINK_FOLLOW != 0 {
        ResolveMode::Follow
    } else {
        ResolveMode::NoFollow
    };
    let credentials = (runtime.credentials.uid, runtime.credentials.gid);
    let source = vfs.resolve_from(&context, &old_start, &old_pathname, mode, Some(credentials))?;
    vfs.link_from(&source, &context, &new_start, &new_pathname, credentials)?;
    Ok(0)
}

/// renameat2 仅公开普通替换和 RENAME_NOREPLACE，不降级其他 rename flags。
pub(super) fn renameat2(user: &UserThread, raw: [usize; 6]) -> SysResult {
    let flags = raw[4] as u32;
    if flags & !RENAME_NOREPLACE != 0 {
        return Err(Errno::InvalidArgument);
    }
    let runtime = user.runtime().expect("renameat2 has no runtime");
    let mm = user.mm().expect("renameat2 has no MM");
    let old_pathname = mm.read_c_string(raw[1], MAX_PATH_BYTES)?;
    let new_pathname = mm.read_c_string(raw[3], MAX_PATH_BYTES)?;
    let (vfs, context) = fs_snapshot(&runtime);
    let old_start = resolve_start(&runtime, &context, raw[0], &old_pathname)?;
    let new_start = resolve_start(&runtime, &context, raw[2], &new_pathname)?;
    let credentials = (runtime.credentials.uid, runtime.credentials.gid);
    vfs.rename_from(
        &context,
        &old_start,
        &old_pathname,
        &new_start,
        &new_pathname,
        flags & RENAME_NOREPLACE != 0,
        credentials,
    )?;
    Ok(0)
}

/// 在共享 UserFs 锁内交换 umask 并返回旧值，CLONE_FS 使用同一组属性。
pub(super) fn umask(user: &UserThread, raw: usize) -> SysResult {
    let runtime = user.runtime().expect("umask has no runtime");
    let previous = runtime.fs.lock().context.exchange_umask(raw as u16);
    Ok(previous as isize)
}

/// 负长度先于用户路径访问失败；最终链接按 Follow 解析，临时写拥有只覆盖这次截断。
pub(super) fn truncate(user: &UserThread, raw: [usize; 6]) -> SysResult {
    let length = raw[1] as i64;
    if length < 0 {
        return Err(Errno::InvalidArgument);
    }
    let runtime = user.runtime().expect("truncate has no runtime");
    let mm = user.mm().expect("truncate has no MM");
    let path = resolve(
        &runtime,
        &mm,
        AT_FDCWD as usize,
        raw[0],
        ResolveMode::Follow,
    )?;
    let inode = path.inode()?;
    match inode.metadata_snapshot()?.kind {
        InodeKind::Regular => {}
        InodeKind::Directory => return Err(Errno::IsDirectory),
        _ => return Err(Errno::InvalidArgument),
    }
    path.check_access(runtime.credentials.uid, runtime.credentials.gid, 2)?;
    let access = inode.acquire_write()?;
    access.truncate(length as u64)?;
    Ok(0)
}

/// 负长度先于 fd 查找失败；持原强 OFD 在表锁外截断，不读取或修改共享位置。
pub(super) fn ftruncate(user: &UserThread, raw: [usize; 6]) -> SysResult {
    let length = raw[1] as i64;
    if length < 0 {
        return Err(Errno::InvalidArgument);
    }
    let runtime = user.runtime().expect("ftruncate has no runtime");
    let file = runtime.files.lookup(descriptor(raw[0])?)?;
    file.truncate(length as u64)?;
    Ok(0)
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
    Ok(file.read_into(&mut target, FileIoContext::User(user))? as isize)
}

/// 负offset先拒绝，取得OFD后先核位置能力，再核读模式及用户地址，且不改共享游标。
pub(super) fn pread(user: &UserThread, raw: [usize; 6]) -> SysResult {
    if (raw[3] as i64) < 0 {
        return Err(Errno::InvalidArgument);
    }
    let runtime = user.runtime().expect("pread has no runtime");
    let file = runtime.files.lookup(descriptor(raw[0])?)?;
    if !file.positioned_io() {
        return Err(Errno::IllegalSeek);
    }
    if file.flags() & 3 == 1 {
        return Err(Errno::BadFd);
    }
    let mm = user.mm().expect("pread has no MM");
    let mut target = ReadBuffer::user(&mm, raw[1], raw[2].min(MAX_RW_COUNT))?;
    Ok(file.read_at_into(raw[3] as u64, &mut target, FileIoContext::User(user))? as isize)
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
    Ok(file.write_from(&mut source, FileIoContext::User(user))? as isize)
}

/// 负偏移先拒绝，OFD位置能力先于写模式和用户地址，追加仍由同一inode仲裁。
pub(super) fn pwrite(user: &UserThread, raw: [usize; 6]) -> SysResult {
    if (raw[3] as i64) < 0 {
        return Err(Errno::InvalidArgument);
    }
    let runtime = user.runtime().expect("pwrite has no runtime");
    let file = runtime.files.lookup(descriptor(raw[0])?)?;
    if !file.positioned_io() {
        return Err(Errno::IllegalSeek);
    }
    if file.flags() & 3 == 0 {
        return Err(Errno::BadFd);
    }
    let mm = user.mm().expect("pwrite has no MM");
    let mut source = WriteBuffer::user(&mm, raw[1], raw[2].min(MAX_RW_COUNT))?;
    Ok(file.write_at_from(raw[3] as u64, &mut source, FileIoContext::User(user))? as isize)
}

/// 在表锁外向原 OFD 传递真实调用线程，设备自行解释命令与用户参数。
pub(super) fn ioctl(user: &UserThread, raw: [usize; 6]) -> SysResult {
    let runtime = user.runtime().expect("ioctl has no runtime");
    let file = runtime.files.lookup(descriptor(raw[0])?)?;
    Ok(file.ioctl(raw[1] as u32, raw[2], FileIoContext::User(user))? as isize)
}

/// lookup 取得原强 OFD 后在表锁外同步全文件，独立打开与 dup 保持各自观察归属。
pub(super) fn fsync(user: &UserThread, raw: usize) -> SysResult {
    sync_file(user, raw, false)
}

/// 数据同步仍包含使已写字节可寻址的元数据及真实设备 FLUSH。
pub(super) fn fdatasync(user: &UserThread, raw: usize) -> SysResult {
    sync_file(user, raw, true)
}

/// 本次操作始终使用 lookup 返回的原文件，不重查可能已经关闭复用的整数编号。
fn sync_file(user: &UserThread, raw: usize, data_only: bool) -> SysResult {
    let runtime = user.runtime().expect("file sync has no runtime");
    let file = runtime.files.lookup(descriptor(raw)?)?;
    file.sync_range(0..u64::MAX, data_only)?;
    Ok(0)
}

/// 共享位置的位移在原OFD锁中提交，不能把失败后的游标在表层回滚。
pub(super) fn seek(user: &UserThread, raw: [usize; 6]) -> SysResult {
    let delta = raw[1] as i64;
    let from = match raw[2] as u32 {
        0 => SeekFrom::Start(raw[1] as u64),
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
    let stat = query_stat(&runtime, &mm, raw[0], raw[1], raw[3], false)?;
    copyout(&mm, raw[2], &encode_stat(stat)?)?;
    Ok(0)
}

/// 从唯一 FileStat 查询编码 statx；本地当前属性不需要远程同步或磁盘 FLUSH。
pub(super) fn statx(user: &UserThread, raw: [usize; 6]) -> SysResult {
    let flags = raw[2] as u32;
    let mask = raw[3] as u32;
    let supported =
        AT_SYMLINK_NOFOLLOW as u32 | AT_EMPTY_PATH as u32 | AT_NO_AUTOMOUNT | AT_STATX_SYNC_TYPE;
    if flags & !supported != 0
        || flags & AT_STATX_SYNC_TYPE == AT_STATX_SYNC_TYPE
        || mask & STATX_RESERVED != 0
    {
        return Err(Errno::InvalidArgument);
    }
    let runtime = user.runtime().expect("statx has no runtime");
    let mm = user.mm().expect("statx has no MM");
    let path_flags = (flags & !(AT_NO_AUTOMOUNT | AT_STATX_SYNC_TYPE)) as usize;
    let allow_null = path_flags == AT_EMPTY_PATH;
    if raw[1] == 0 && !allow_null {
        return Err(Errno::BadAddress);
    }
    let stat = query_stat(&runtime, &mm, raw[0], raw[1], path_flags, allow_null)?;
    copyout(&mm, raw[4], &encode_statx(stat))?;
    Ok(0)
}

/// 共用 dirfd、空路径和最后链接属性查询；只有 statx 明确授权的 NULL 才视作空串。
fn query_stat(
    runtime: &UserRuntime,
    mm: &Mm,
    dirfd: usize,
    address: usize,
    flags: usize,
    allow_null: bool,
) -> Result<FileStat, Errno> {
    let pathname = if address == 0 && allow_null {
        Vec::new()
    } else {
        mm.read_c_string(address, MAX_PATH_BYTES)?
    };
    if pathname.is_empty() && flags & AT_EMPTY_PATH != 0 {
        if dirfd as i32 == AT_FDCWD {
            let (_, context) = fs_snapshot(runtime);
            Ok(fs::stat_path(context.cwd())?)
        } else {
            Ok(runtime.files.stat(descriptor(dirfd)?)?)
        }
    } else {
        let mode = if flags & AT_SYMLINK_NOFOLLOW != 0 {
            ResolveMode::NoFollow
        } else {
            ResolveMode::Follow
        };
        Ok(fs::stat_path(&resolve_bytes(
            runtime, dirfd, &pathname, mode,
        )?)?)
    }
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
        bytes[8..16].copy_from_slice(&entry.next_cookie.to_le_bytes());
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

/// 锁外解析并验证真实目录；提交时只替换原共享 FsContext 的 cwd。
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
    if path.inode()?.metadata_snapshot()?.kind != InodeKind::Directory {
        return Err(Errno::NotDirectory);
    }
    path.check_access(runtime.credentials.uid, runtime.credentials.gid, 1)?;
    let retired = runtime.fs.lock().context.exchange_cwd(path);
    drop(retired);
    Ok(0)
}

/// 返回真实 cwd 原始字节及 NUL 的总长度，容量不足和用户写故障不改 FS 上下文。
pub(super) fn getcwd(user: &UserThread, raw: [usize; 6]) -> SysResult {
    let runtime = user.runtime().expect("getcwd has no runtime");
    let mm = user.mm().expect("getcwd has no MM");
    let (vfs, context) = fs_snapshot(&runtime);
    let mut bytes = vfs.cwd_bytes(&context)?;
    if raw[1] < bytes.len() + 1 {
        return Err(Errno::Range);
    }
    // cwd_bytes 已准备包含 NUL 的容量，此处不增加分配或读取第二次路径。
    bytes.push(0);
    copyout(&mm, raw[0], &bytes)?;
    Ok(bytes.len() as isize)
}

/// 从一次 fd 查找所得 OFD 的真实目录 Path 核搜索权限，锁内只交换 cwd 拥有。
pub(super) fn fchdir(user: &UserThread, raw: [usize; 6]) -> SysResult {
    let runtime = user.runtime().expect("fchdir has no runtime");
    let file = runtime.files.lookup(descriptor(raw[0])?)?;
    let path = file.path().ok_or(Errno::NotDirectory)?.clone();
    if path.inode()?.metadata_snapshot()?.kind != InodeKind::Directory {
        return Err(Errno::NotDirectory);
    }
    path.check_access(runtime.credentials.uid, runtime.credentials.gid, 1)?;
    let retired = runtime.fs.lock().context.exchange_cwd(path);
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
/// 绝对路径从本次 root 开始，其他路径复用 cwd 或已打开目录的真实 Path。
fn resolve_start(
    runtime: &UserRuntime,
    context: &FsContext,
    dirfd: usize,
    pathname: &[u8],
) -> Result<Path, Errno> {
    if pathname.is_empty() {
        return Err(Errno::NotFound);
    }
    if pathname[0] == b'/' {
        return Ok(context.root().clone());
    }
    if dirfd as i32 == AT_FDCWD {
        return Ok(context.cwd().clone());
    }
    let file = runtime.files.lookup(descriptor(dirfd)?)?;
    if file.stat()?.kind != InodeKind::Directory {
        return Err(Errno::NotDirectory);
    }
    Ok(file.path().ok_or(Errno::NotDirectory)?.clone())
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

/// 绝对路径忽略 dirfd，相对路径从已取得的真实起点行走并逐组件核 search 权限。
pub(super) fn resolve_bytes(
    runtime: &UserRuntime,
    dirfd: usize,
    pathname: &[u8],
    mode: ResolveMode,
) -> Result<Path, Errno> {
    let (vfs, context) = fs_snapshot(runtime);
    let start = resolve_start(runtime, &context, dirfd, pathname)?;
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
    put_u64(&mut bytes, 32, stat.rdev);
    put_u64(&mut bytes, 48, size as u64);
    put_u32(&mut bytes, 56, stat.block_size);
    put_u64(&mut bytes, 64, blocks as u64);
    for (offset, time) in [(72, stat.atime), (88, stat.mtime), (104, stat.ctime)] {
        put_u64(&mut bytes, offset, time.seconds as u64);
        put_u64(&mut bytes, offset + 8, u64::from(time.nanoseconds));
    }
    Ok(bytes)
}

/// 以零初始化的 256 字节快照交付真实基本属性，未支持能力、时间和保留域保持零。
fn encode_statx(stat: FileStat) -> [u8; 256] {
    let mut bytes = [0u8; 256];
    put_u32(&mut bytes, 0, STATX_BASIC_STATS);
    put_u32(&mut bytes, 4, stat.block_size);
    put_u32(&mut bytes, 16, stat.links);
    put_u32(&mut bytes, 20, stat.uid);
    put_u32(&mut bytes, 24, stat.gid);
    bytes[28..30].copy_from_slice(&stat.mode.to_le_bytes());
    put_u64(&mut bytes, 32, stat.inode);
    put_u64(&mut bytes, 40, stat.size);
    put_u64(&mut bytes, 48, stat.blocks);
    for (offset, time) in [(64, stat.atime), (96, stat.ctime), (112, stat.mtime)] {
        put_u64(&mut bytes, offset, time.seconds as u64);
        put_u32(&mut bytes, offset + 8, time.nanoseconds);
    }
    for (offset, device) in [(128, stat.rdev), (136, stat.device)] {
        let (major, minor) = device_numbers(device);
        put_u32(&mut bytes, offset, major);
        put_u32(&mut bytes, offset + 4, minor);
    }
    bytes
}

/// 将已有 Linux 编码 dev_t64 拆为实际主次号，高位设备身份不得截断为单个 minor。
fn device_numbers(device: u64) -> (u32, u32) {
    let major = ((device >> 8) & 0xfff) | ((device >> 32) & 0xffff_f000);
    let minor = (device & 0xff) | ((device >> 12) & 0xffff_ff00);
    (major as u32, minor as u32)
}

/// 完整初始化快照中固定八字节字段编码，偏移只由此模块的ABI常量提供。
fn put_u64(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

/// 完整初始化快照中固定四字节字段编码，不从用户字节构造Rust布局对象。
fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

/// 未知后端类型保DT_UNKNOWN，目录记录不能凭名字猜类型。
fn dirent_type(entry: &DirectoryEntry) -> u8 {
    match entry.kind {
        Some(InodeKind::Regular) => 8,
        Some(InodeKind::Directory) => 4,
        Some(InodeKind::Symlink) => 10,
        Some(InodeKind::Character) => 2,
        Some(InodeKind::Block) => 6,
        Some(InodeKind::Fifo) => 1,
        _ => 0,
    }
}
