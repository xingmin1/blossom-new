//! 每次打开拥有独立位置，描述符复制只共享同一个具体文件对象。

use alloc::sync::Arc;

use crate::fs::{DirectoryEntry, FsError, InodeKind, Path};
use crate::sync::SleepMutex;

/// 共享一个打开实例及其位置；最后引用释放该实例持有的路径。
pub type FileRef = Arc<dyn FileLike>;

/// 文件中的非负字节位置；相对位移计算失败时保留原位置。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SeekFrom {
    Start(u64),
    Current(i64),
    End(i64),
}

/// 已加载 inode 的真实属性，不包含文件页或尚未实现的属性。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FileStat {
    pub inode: u64,
    pub kind: InodeKind,
    pub mode: u16,
    pub size: u64,
    pub uid: u32,
    pub gid: u32,
    pub links: u32,
    /// 已分配空间按 512 字节计数。
    pub blocks: u64,
}

/// 普通可睡眠任务使用的文件操作；调用者须在描述符表锁外持有实例引用。
pub trait FileLike: Send + Sync {
    /// 从共享位置读取，只有实际交付的字节推进位置。
    fn read(&self, dst: &mut [u8]) -> Result<usize, FsError>;

    /// 按指定位置读取，不获取或修改共享位置。
    fn read_at(&self, offset: u64, dst: &mut [u8]) -> Result<usize, FsError>;

    /// 更新共享字节位置，负值或溢出不修改原位置。
    fn seek(&self, from: SeekFrom) -> Result<u64, FsError>;

    /// 借用真实 inode 元数据生成属性快照，不获取位置锁。
    fn stat(&self) -> Result<FileStat, FsError>;

    /// 返回下一项及其原始名字，成功取得项后才推进目录位置。
    fn readdir(&self) -> Result<Option<DirectoryEntry>, FsError>;

    /// 当前只读文件拒绝写入，包括空输入。
    fn write(&self, src: &[u8]) -> Result<usize, FsError>;
}

/// 普通文件的打开实例，路径保活挂载和唯一 inode 的文件页索引。
pub struct InodeFile {
    path: Path,
    cursor: SleepMutex<u64>,
}

/// 目录的打开实例，枚举位置使用后端已有的字节记录边界。
pub struct DirectoryFile {
    path: Path,
    cursor: SleepMutex<u64>,
}

/// 按真实 inode 类型创建位置为零的新实例，元数据错误原样返回，分配失败返回 NoMemory。
/// 最终未跟随的符号链接不能成为可读实例，路径的挂载拥有权随新实例保存。
pub fn open_path(path: Path) -> Result<FileRef, FsError> {
    match path.inode()?.metadata()?.kind {
        InodeKind::Regular => Ok(Arc::try_new(InodeFile {
            path,
            cursor: SleepMutex::new(0),
        })
        .map_err(|_| FsError::NoMemory)?),
        InodeKind::Directory => Ok(Arc::try_new(DirectoryFile {
            path,
            cursor: SleepMutex::new(0),
        })
        .map_err(|_| FsError::NoMemory)?),
        InodeKind::Symlink => Err(FsError::TooManySymlinks),
        InodeKind::Other => Err(FsError::Unsupported),
    }
}

impl FileLike for InodeFile {
    /// 位置锁跨真实读取保持独占，后页错误产生的正前缀仍按实际长度推进。
    fn read(&self, dst: &mut [u8]) -> Result<usize, FsError> {
        let mut cursor = self.cursor.lock();
        let bytes = self.path.inode()?.read_at(*cursor, dst)?;
        // 原读取限制 bytes 不超过 size-cursor；EOF 或零长返回零，因此位置相加不会溢出。
        *cursor += bytes as u64;
        Ok(bytes)
    }

    /// 直接读取唯一 inode 的文件页，不参与共享位置的串行化。
    fn read_at(&self, offset: u64, dst: &mut [u8]) -> Result<usize, FsError> {
        self.path.inode()?.read_at(offset, dst)
    }

    /// 与顺序读取共用位置锁，允许定位到文件末尾之后。
    fn seek(&self, from: SeekFrom) -> Result<u64, FsError> {
        seek_path(&self.path, &self.cursor, from)
    }

    /// 只复制属性字段，文件页与 inode 的内部块数据保持原有拥有权。
    fn stat(&self) -> Result<FileStat, FsError> {
        stat_path(&self.path)
    }

    /// 普通文件不能作为目录枚举。
    fn readdir(&self) -> Result<Option<DirectoryEntry>, FsError> {
        Err(FsError::NotDirectory)
    }

    /// 只读实例不接受写入，也不推进位置。
    fn write(&self, _src: &[u8]) -> Result<usize, FsError> {
        Err(FsError::ReadOnly)
    }
}

impl FileLike for DirectoryFile {
    /// 目录字节由目录项操作消费，不能按普通文件读取。
    fn read(&self, _dst: &mut [u8]) -> Result<usize, FsError> {
        Err(FsError::IsDirectory)
    }

    /// 指定位置的普通字节读取同样拒绝目录。
    fn read_at(&self, _offset: u64, _dst: &mut [u8]) -> Result<usize, FsError> {
        Err(FsError::IsDirectory)
    }

    /// 设置目录字节位置，后续枚举由后端检查该位置的记录边界。
    fn seek(&self, from: SeekFrom) -> Result<u64, FsError> {
        seek_path(&self.path, &self.cursor, from)
    }

    /// 目录属性直接来自真实 inode，不获取枚举位置锁。
    fn stat(&self) -> Result<FileStat, FsError> {
        stat_path(&self.path)
    }

    /// 每次只取得下一项；分配、格式或 I/O 错误均保留原枚举位置。
    fn readdir(&self) -> Result<Option<DirectoryEntry>, FsError> {
        let mut cursor = self.cursor.lock();
        let entry = self.path.inode()?.dir_next(*cursor)?;
        if let Some(entry) = &entry {
            *cursor = entry.next_offset;
        }
        Ok(entry)
    }

    /// 只读目录拒绝写入，空输入也不能伪报成功。
    fn write(&self, _src: &[u8]) -> Result<usize, FsError> {
        Err(FsError::ReadOnly)
    }
}

/// 在同一次位置锁占有中计算并提交位置，只有 End 借用真实文件大小。
fn seek_path(path: &Path, cursor: &SleepMutex<u64>, from: SeekFrom) -> Result<u64, FsError> {
    let mut cursor = cursor.lock();
    let position = match from {
        SeekFrom::Start(position) => Some(position),
        SeekFrom::Current(delta) => cursor.checked_add_signed(delta),
        SeekFrom::End(delta) => path.inode()?.metadata()?.size.checked_add_signed(delta),
    }
    .ok_or(FsError::InvalidOperation)?;
    *cursor = position;
    Ok(position)
}

/// 从共享 inode 借用字段形成小快照，不复制完整元数据或文件内容。
fn stat_path(path: &Path) -> Result<FileStat, FsError> {
    let inode = path.inode()?;
    let data = inode.metadata()?;
    Ok(FileStat {
        inode: inode.number(),
        kind: data.kind,
        mode: data.mode,
        size: data.size,
        uid: data.uid,
        gid: data.gid,
        links: data.links,
        blocks: data.blocks,
    })
}
