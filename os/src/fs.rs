//! 只读文件后端与唯一 inode 身份共用原页缓存和块 I/O。

pub(crate) mod ext4;
mod inode;
#[cfg(feature = "ktest")]
pub(crate) mod ktest;

pub use ext4::{BlockMapping, DirectoryEntry, Ext4Volume, Geometry, InodeData, InodeKind};
pub use inode::{InodeCache, InodeRef};

use crate::block::BlockError;

/// 格式、名字与块 I/O 错误保留各自原因，不以零内容代替失败。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FsError {
    NoMemory,
    Corrupt,
    Checksum,
    Unsupported,
    NotFound,
    NotDirectory,
    InvalidName,
    InvalidOperation,
    LoadAborted,
    Io(BlockError),
}

impl FsError {
    /// 将页填充的实际格式或 I/O 错误写入共享 folio，等待者读取同一原因。
    /// 名字查找和 inode 加载取消不属于页填充结果。
    pub(crate) fn folio_error(self) -> BlockError {
        match self {
            Self::NoMemory => BlockError::NoMemory,
            Self::Corrupt => BlockError::Corrupt,
            Self::Checksum => BlockError::Checksum,
            Self::Unsupported => BlockError::Unsupported,
            Self::Io(error) => error,
            Self::NotFound
            | Self::NotDirectory
            | Self::InvalidName
            | Self::InvalidOperation
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
