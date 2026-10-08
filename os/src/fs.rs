//! 文件内容、唯一 inode 身份与元数据视图共用原页缓存和块 I/O。

mod dentry;
pub(crate) mod ext4;
mod file;
mod file_table;
mod inode;
#[cfg(feature = "ktest")]
pub(crate) mod ktest;
mod path;
pub(crate) mod system;
pub(crate) mod writeback;

pub use dentry::{DentryCache, DentryRef};
pub use ext4::{
    BlockMapping, DirectoryEntry, Ext4Volume, Geometry, InodeData, InodeKind, InodeTime,
};
pub use file::{
    open_path, open_path_with_flags, stat_path, DirectoryFile, FileLike, FileRef, FileStat,
    InodeFile, SeekFrom,
};
pub use file::{ReadBuffer, WriteBuffer};
pub use file_table::{DescriptorFlags, FileReservation, FileTable, OpenOptions};
pub(crate) use inode::PreparedMkwrite;
pub use inode::{ExecAccess, InodeCache, InodeMetadata, InodeRef, WriteAccess};
pub use path::{FsContext, Path, ResolveMode, Vfs};

use crate::block::BlockError;

/// 文件能力、格式、名字与块 I/O 错误保留各自原因，不以零内容代替失败。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FsError {
    NoMemory,
    NoSpace,
    TextBusy,
    FileTooLarge,
    Corrupt,
    Checksum,
    Unsupported,
    NotFound,
    MissingRoot,
    NotDirectory,
    InvalidName,
    InvalidOperation,
    Busy,
    TooManySymlinks,
    BadFd,
    TooManyFiles,
    ReadOnly,
    IsDirectory,
    BadAddress,
    Permission,
    LoadAborted,
    Io(BlockError),
}

impl FsError {
    /// 将页填充的实际格式或 I/O 错误写入共享 folio，等待者读取同一原因。
    /// 根选择、名字查找、文件能力和 inode 加载取消不属于页填充结果。
    pub(crate) fn folio_error(self) -> BlockError {
        match self {
            Self::NoMemory => BlockError::NoMemory,
            Self::Corrupt => BlockError::Corrupt,
            Self::Checksum => BlockError::Checksum,
            Self::Unsupported => BlockError::Unsupported,
            Self::Io(error) => error,
            Self::BadFd
            | Self::TooManyFiles
            | Self::NoSpace
            | Self::TextBusy
            | Self::FileTooLarge
            | Self::ReadOnly
            | Self::IsDirectory
            | Self::BadAddress
            | Self::Permission
            | Self::NotFound
            | Self::MissingRoot
            | Self::NotDirectory
            | Self::InvalidName
            | Self::InvalidOperation
            | Self::Busy
            | Self::TooManySymlinks
            | Self::LoadAborted => panic!("non-fill error published to a folio: {:?}", self),
        }
    }
}

impl From<BlockError> for FsError {
    /// 页填充保留的格式错误恢复到文件接口，其余错误保持真实块层原因。
    fn from(error: BlockError) -> Self {
        match error {
            BlockError::NoMemory => Self::NoMemory,
            BlockError::Corrupt => Self::Corrupt,
            BlockError::Checksum => Self::Checksum,
            BlockError::Unsupported => Self::Unsupported,
            other => Self::Io(other),
        }
    }
}
