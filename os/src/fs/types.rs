//! 文件系统后端共享的 inode 属性、目录项与枚举位置类型。

use alloc::vec::Vec;

/// inode 类型同时保留普通文件、目录、链接和可识别的设备节点语义。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InodeKind {
    Regular,
    Directory,
    Symlink,
    Character,
    Block,
    Other,
}

/// inode 时间按盘面有符号秒与扩展纳秒保存，不在只读读取时修改时钟。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InodeTime {
    pub seconds: i64,
    pub nanoseconds: u32,
}

/// 目录枚举返回的 inode 号、原始名字及下一次枚举所需的后端位置。
#[derive(Debug, Eq, PartialEq)]
pub struct DirectoryEntry {
    pub number: u64,
    pub name: Vec<u8>,
    pub kind: Option<InodeKind>,
    pub next_cookie: u64,
    pub(crate) next_cursor: DirectoryCursor,
}

/// 目录游标保留后端可见 cookie；内存目录不应被磁盘后端解释为字节位置。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum DirectoryCursor {
    /// 普通目录的盘面字节偏移。
    Byte(u64),
    /// indexed 目录的盘面哈希和碰撞续读位置。
    Indexed {
        major: u32,
        minor: u32,
        collision: Option<DirectoryPosition>,
    },
    /// 内存目录拥有的真实游标值。
    Memory(u64),
}

/// 精确定位已交付记录，区分 major/minor 完全相同的多个目录项。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct DirectoryPosition {
    /// inode mapping 中的目录逻辑块号。
    pub(crate) logical: u64,
    /// 目录块内最后一次交付的记录字节偏移。
    pub(crate) offset: u32,
}

impl DirectoryCursor {
    /// 返回给用户的稳定目录 cookie；碰撞位置只保存在 OFD 游标内。
    pub(crate) fn cookie(&self) -> u64 {
        match self {
            Self::Byte(offset) | Self::Memory(offset) => *offset,
            Self::Indexed { major, minor, .. } => {
                (u64::from(*major >> 1) << 32) | u64::from(*minor)
            }
        }
    }
}
