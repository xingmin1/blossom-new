//! 明确选择启动盘、根权限与用户系统根路径，复用正式挂载和文件表。

use alloc::sync::Arc;

use crate::block::{AccessMode, DiskRef};
use crate::fs::ext4::crc::crc32c;
use crate::fs::{
    Ext4Volume, FileLike, FileTable, Filesystem, FsContext, FsError, InodeCache, OpenOptions,
    ResolveMode, Vfs,
};
use crate::mm::PAGE_SIZE;

/// 启动参数选中的固定磁盘槽；是否存在由本次真实设备表检查。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct BootDevices<'a> {
    pub root: usize,
    pub extra: Option<usize>,
    pub root_mode: AccessMode,
    pub init: &'a [u8],
}

/// 一次按 ASCII 空白消费 root/extra/init 与完整 ro/rw token 的最后值，根默认只读。
/// 仅支持完整的 /dev/vd[a-z] 块选择名；init 仅在缺键时默认 /sbin/init，显式错误留给 exec。
pub(crate) fn boot_devices(arguments: &[u8]) -> Result<BootDevices<'_>, FsError> {
    let mut root = None;
    let mut extra = None;
    let mut root_mode = AccessMode::ReadOnly;
    let mut init = None;
    for argument in arguments.split(|byte| byte.is_ascii_whitespace() || *byte == 0x0b) {
        if let Some(value) = argument.strip_prefix(b"root=") {
            root = Some(value);
        } else if let Some(value) = argument.strip_prefix(b"extra=") {
            extra = Some(value);
        } else if let Some(value) = argument.strip_prefix(b"init=") {
            init = Some(value);
        } else if argument == b"ro" {
            root_mode = AccessMode::ReadOnly;
        } else if argument == b"rw" {
            root_mode = AccessMode::ReadWrite;
        }
    }
    Ok(BootDevices {
        root: device_index(root.ok_or(FsError::MissingRoot)?)?,
        extra: extra.map(device_index).transpose()?,
        root_mode,
        init: init.unwrap_or(b"/sbin/init"),
    })
}

/// 从完整原始字节名字取得一个槽号，不接受分区、LABEL、UUID 或多字母后缀。
fn device_index(name: &[u8]) -> Result<usize, FsError> {
    match name.strip_prefix(b"/dev/vd") {
        Some([letter @ b'a'..=b'z']) => Ok(usize::from(*letter - b'a')),
        _ => Err(FsError::MissingRoot),
    }
}

/// 普通可睡眠 Task 以同一权限构造根卷和根挂载，仅显式附盘才以只读挂到 /mnt/extra。
/// 卷与路径只保活原设备及缓存，不停止设备，也不建立全局文件系统拥有者。
pub(crate) fn mount_disks(
    root: DiskRef,
    extra: Option<DiskRef>,
    inodes: Arc<InodeCache>,
    root_mode: AccessMode,
) -> Result<Vfs, FsError> {
    let volume = Ext4Volume::open(root, root_mode, 64)?;
    let vfs = Vfs::try_new(Filesystem::Ext4(volume), inodes, 128, 8, root_mode)?;
    if let Some(extra) = extra {
        let root = vfs.root();
        let context = FsContext::new(root.clone(), root)?;
        let covered = vfs.resolve(&context, b"/mnt/extra", ResolveMode::Follow)?;
        let volume = Ext4Volume::open(extra, AccessMode::ReadOnly, 64)?;
        vfs.mount(&covered, Filesystem::Ext4(volume), AccessMode::ReadOnly)?;
    }
    Ok(vfs)
}

/// 实际交付的完整文件长度与标准最终取反 CRC32C，空文件摘要为零。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct FileSummary {
    pub bytes: u64,
    pub crc32c: u32,
}

/// 在普通可睡眠 Task 中只读打开并完整读取文件，所有返回分支都关闭本次描述符。
/// 调用者通过自有表独占本次新开编号直至关闭，期间不得由其他任务关闭或替换它。
/// 本次取得的文件引用在表锁外使用和释放；读取错误优先于关闭错误。
pub(crate) fn read_summary(
    files: &FileTable,
    vfs: &Vfs,
    context: &FsContext,
    pathname: &[u8],
) -> Result<FileSummary, FsError> {
    let fd = files.open(vfs, context, pathname, OpenOptions::default())?;
    let result = files
        .lookup(fd)
        .and_then(|file| summarize_file(file.as_ref()));
    let closed = files.close(fd);
    let summary = result?;
    closed?;
    Ok(summary)
}

/// 用一页任务栈缓冲顺序读取同一打开实例，读到 EOF 后核对真实 inode 大小。
/// 正前缀仍继续读取，后次错误原样返回，不把部分交付冒充完整文件。
fn summarize_file(file: &dyn FileLike) -> Result<FileSummary, FsError> {
    let size = file.stat()?.size;
    let mut buffer = [0; PAGE_SIZE];
    let mut bytes = 0_u64;
    let mut crc = !0_u32;
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        bytes = bytes.checked_add(count as u64).ok_or(FsError::Corrupt)?;
        crc = crc32c(crc, &buffer[..count]);
    }
    if bytes != size {
        return Err(FsError::Corrupt);
    }
    Ok(FileSummary {
        bytes,
        crc32c: !crc,
    })
}
