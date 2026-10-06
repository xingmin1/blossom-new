//! ext4 只读卷拥有实际设备声明；元数据与文件内容使用共享原页。

pub(crate) mod codec;
pub(crate) mod crc;
mod directory;
mod mapping;

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cmp::min;
use core::convert::TryFrom;
use core::sync::atomic::{AtomicU32, Ordering};

use crate::block::{Disk, DiskRef, IoResult, ReadOnlyClaim, SECTOR_BYTES};
use crate::cache::{BufferRef, MetadataCache};
use crate::fs::FsError;
use crate::mm::{FolioRead, PAGE_SIZE};
#[cfg(feature = "ktest")]
use crate::sync::SpinLock;

#[cfg(feature = "ktest")]
type SegmentHook = Arc<dyn Fn(u64, usize) + Send + Sync>;

#[derive(Debug)]
pub struct Geometry {
    /// 文件系统块的字节数，与设备 sector 大小独立。
    pub block_size: usize,
    pub inode_size: usize,
    pub descriptor_size: usize,
    pub blocks_count: u64,
    pub inodes_count: u64,
    pub features_compat: u32,
    pub features_incompat: u32,
    pub features_ro_compat: u32,
    pub first_data_block: u64,
    pub blocks_per_group: u64,
    pub inodes_per_group: u64,
    pub groups_count: u64,
    pub descriptor_blocks: u64,
    pub checksum_seed: u32,
    pub uuid: [u8; 16],
    pub reserved_gdt_blocks: u64,
    pub backup_groups: [u32; 2],
    pub log_groups_per_flex: u8,
}

impl Geometry {
    /// 判断块组是否保留超级块副本，主描述符始终位于第一块组。
    fn has_super(&self, group: u64) -> bool {
        if group == 0 {
            return true;
        }
        if self.features_compat & 0x200 != 0 {
            return self
                .backup_groups
                .iter()
                .any(|&backup| u64::from(backup) == group);
        }
        if self.features_ro_compat & 1 == 0 || group == 1 {
            return true;
        }
        [3u64, 5, 7].into_iter().any(|base| {
            let mut value = group;
            while value % base == 0 {
                value /= base;
            }
            value == 1
        })
    }

    /// 拒绝越界、零地址和覆盖超级块或保留描述符的物理范围。
    pub(crate) fn validate_blocks(&self, physical: u64, len: u64) -> Result<(), FsError> {
        let end = physical.checked_add(len).ok_or(FsError::Corrupt)?;
        if len == 0 || physical <= self.first_data_block || self.blocks_count < end {
            return Err(FsError::Corrupt);
        }
        let first_group = (physical - self.first_data_block) / self.blocks_per_group;
        let last_group = (end - 1 - self.first_data_block) / self.blocks_per_group;
        for group in first_group..=last_group {
            if self.has_super(group) {
                let start = self.first_data_block + group * self.blocks_per_group;
                let reserved_end = start
                    .checked_add(1)
                    .and_then(|v| v.checked_add(self.descriptor_blocks))
                    .and_then(|v| v.checked_add(self.reserved_gdt_blocks))
                    .ok_or(FsError::Corrupt)?;
                if physical < reserved_end && start < end {
                    return Err(FsError::Corrupt);
                }
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum InodeKind {
    Regular,
    Directory,
    Symlink,
    Other,
}

/// inode 时间按盘面有符号秒与扩展纳秒保存，不在只读读取时修改时钟。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InodeTime {
    pub seconds: i64,
    pub nanoseconds: u32,
}

#[derive(Debug)]
pub struct InodeData {
    pub number: u64,
    pub kind: InodeKind,
    pub mode: u16,
    pub size: u64,
    pub uid: u32,
    pub gid: u32,
    pub links: u32,
    /// 已分配空间按 512 字节计数，包括独立 EA 块。
    pub blocks: u64,
    pub atime: InodeTime,
    pub mtime: InodeTime,
    pub ctime: InodeTime,
    pub generation: u32,
    pub flags: u32,
    pub block: [u8; 60],
    pub checksum_seed: u32,
    pub fast_symlink: bool,
    pub file_acl: u64,
    pub extra_isize: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BlockMapping {
    Mapped { physical: u64, len: u64 },
    Hole { len: u64 },
    Unwritten { physical: u64, len: u64 },
    Eof,
}

#[derive(Debug, Eq, PartialEq)]
pub struct DirectoryEntry {
    pub number: u64,
    pub name: Vec<u8>,
    pub kind: Option<InodeKind>,
    pub next_offset: u64,
}

pub struct Ext4Volume {
    device: u32,
    disk: DiskRef,
    metadata: MetadataCache,
    geometry: Geometry,
    _claim: ReadOnlyClaim,
    #[cfg(feature = "ktest")]
    segment_hook: SpinLock<Option<SegmentHook>>,
}

/// 虚拟卷编号只表示本内核的独立卷实例，耗尽后拒绝构造而不回绕复用。
static NEXT_DEVICE: AtomicU32 = AtomicU32::new(1);

impl Ext4Volume {
    /// 普通任务取得只读声明后读取主超级块；不执行 journal replay 或写盘。
    pub fn open(disk: DiskRef, metadata_capacity: usize) -> Result<Arc<Self>, FsError> {
        let claim = Disk::claim_readonly(&disk)?;
        let metadata = MetadataCache::try_new(disk.clone(), metadata_capacity)?;
        let capacity = disk
            .limits()
            .capacity_sectors
            .checked_mul(SECTOR_BYTES)
            .ok_or(FsError::Corrupt)?;
        // 主超级块在 1024 字节处；读设备对齐的整页后仅解码其固定范围。
        let superblock = metadata.get(0, PAGE_SIZE)?;
        let geometry = superblock
            .with_bytes(|data| codec::decode_super(codec::bytes(data, 1024, 1024)?, capacity))??;
        drop(superblock);
        let logical = disk.limits().logical_block_size as usize;
        if geometry.block_size % logical != 0 {
            return Err(FsError::Unsupported);
        }
        let volume = Self {
            device: NEXT_DEVICE
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |next| {
                    next.checked_add(1)
                })
                .map_err(|_| FsError::NoMemory)?,
            disk,
            metadata,
            geometry,
            _claim: claim,
            #[cfg(feature = "ktest")]
            segment_hook: SpinLock::new(None),
        };
        // 挂载时检查每个实际描述符，后续 inode 定位仍校验所消费的原字节。
        for group in 0..volume.geometry.groups_count {
            volume.inode_table(group)?;
        }
        Arc::try_new(volume).map_err(|_| FsError::NoMemory)
    }

    /// 借用一次解码并验证的有效几何。
    pub fn geometry(&self) -> &Geometry {
        &self.geometry
    }

    /// 同一卷的所有 Arc 引用共享稳定非零编号，不泄漏地址或宿主设备号。
    pub fn device(&self) -> u64 {
        u64::from(self.device)
    }

    /// 借用保活且被只读声明保护的实际磁盘。
    pub fn disk(&self) -> &DiskRef {
        &self.disk
    }

    /// 设置仅验收镜像使用的真实分段收权观察点；回调在卷锁之外执行。
    #[cfg(feature = "ktest")]
    pub(crate) fn ktest_set_segment_hook(&self, hook: Option<SegmentHook>) {
        let previous = core::mem::replace(&mut *self.segment_hook.lock(), hook);
        drop(previous);
    }

    /// 仅释放无人使用的静止元数据缓存引用。
    pub fn prune_unused(&self) -> usize {
        self.metadata.prune_unused()
    }

    /// 普通任务读取一个已验证物理地址的元数据块，等待不跨内容借用。
    pub(crate) fn metadata_block(&self, physical: u64) -> Result<BufferRef, FsError> {
        self.geometry.validate_blocks(physical, 1)?;
        Ok(self.metadata.get(physical, self.geometry.block_size)?)
    }

    /// 读取主描述符数组中的指定组，允许 flex 组跨组安置 inode 表。
    fn inode_table(&self, group: u64) -> Result<u64, FsError> {
        let g = &self.geometry;
        if g.groups_count <= group {
            return Err(FsError::Corrupt);
        }
        let byte = group
            .checked_mul(g.descriptor_size as u64)
            .ok_or(FsError::Corrupt)?;
        let block = g.first_data_block + 1 + byte / g.block_size as u64;
        let offset = (byte % g.block_size as u64) as usize;
        let buffer = self.metadata.get(block, g.block_size)?;
        let table = buffer.with_bytes(|data| {
            codec::decode_group(g, group, codec::bytes(data, offset, g.descriptor_size)?)
        })??;
        g.validate_blocks(
            table,
            codec::ceil_div(
                g.inodes_per_group * g.inode_size as u64,
                g.block_size as u64,
            )?,
        )?;
        Ok(table)
    }

    /// 定位并校验一个完整 inode，外部编号不能穿过组或设备边界。
    pub fn load_inode(&self, number: u64) -> Result<InodeData, FsError> {
        let g = &self.geometry;
        if number == 0 || g.inodes_count < number {
            return Err(FsError::Corrupt);
        }
        let group = (number - 1) / g.inodes_per_group;
        let table = self.inode_table(group)?;
        let byte = ((number - 1) % g.inodes_per_group) * g.inode_size as u64;
        let physical = table
            .checked_add(byte / g.block_size as u64)
            .ok_or(FsError::Corrupt)?;
        let buffer = self.metadata_block(physical)?;
        let inode = buffer.with_bytes(|data| {
            codec::decode_inode(
                g,
                number,
                codec::bytes(data, (byte % g.block_size as u64) as usize, g.inode_size)?,
            )
        })??;
        if !inode.fast_symlink && inode.kind != InodeKind::Other {
            mapping::validate_root(g, &inode)?;
        }
        Ok(inode)
    }

    /// 返回从指定逻辑块开始的真实映射；合法空洞与 EOF 不掩盖格式错误。
    pub fn map_block(&self, inode: &InodeData, logical: u64) -> Result<BlockMapping, FsError> {
        if inode.fast_symlink || inode.kind == InodeKind::Other {
            return Err(FsError::Unsupported);
        }
        let end = codec::ceil_div(inode.size, self.geometry.block_size as u64)?;
        if end <= logical {
            return Ok(BlockMapping::Eof);
        }
        mapping::map(self, inode, logical, end)
    }

    /// 顺序填充原页小块；每段收权后才初始化合法零范围，全部完成后唯一发表。
    pub fn fill_page(
        &self,
        inode: &InodeData,
        index: u64,
        mut read: FolioRead,
    ) -> Result<(), FsError> {
        let start = match index.checked_mul(PAGE_SIZE as u64) {
            Some(start) if read.offset() == 0 && read.len() == PAGE_SIZE => start,
            _ => return finish_error(read, FsError::Corrupt),
        };
        let size = self.geometry.block_size;
        let mut offset = 0usize;
        while offset < PAGE_SIZE {
            let byte = match start.checked_add(offset as u64) {
                Some(byte) => byte,
                None => return finish_error(read, FsError::Corrupt),
            };
            let mapping = match self.map_block(inode, byte / size as u64) {
                Ok(mapping) => mapping,
                Err(error) => return finish_error(read, error),
            };
            match mapping {
                BlockMapping::Mapped { physical, .. } => {
                    let disk_byte = match physical.checked_mul(size as u64) {
                        Some(byte) => byte,
                        None => return finish_error(read, FsError::Corrupt),
                    };
                    read = self.disk.fill_folio_range(disk_byte, read, offset, size)?;
                    #[cfg(feature = "ktest")]
                    {
                        let hook = self.segment_hook.lock().clone();
                        if let Some(hook) = hook {
                            hook(index, offset + size);
                        }
                    }
                }
                BlockMapping::Hole { .. } | BlockMapping::Unwritten { .. } | BlockMapping::Eof => {
                    zero_range(&mut read, offset, size);
                }
            }
            if inode.size < byte.saturating_add(size as u64) {
                let valid = min(inode.size.saturating_sub(byte), size as u64) as usize;
                zero_range(&mut read, offset + valid, size - valid);
            }
            offset += size;
        }
        // SAFETY: 每个小块已由真实 DMA 全尾收权或 CPU 零填充，EOF 尾也已初始化；
        // read 仍独占整页内容锁，设备及所有可变借用均已结束。
        unsafe {
            read.complete(IoResult {
                completed: PAGE_SIZE,
                error: None,
            });
        }
        Ok(())
    }

    /// 校验单组件的原始名字字节，线性查找不分配目录名称副本。
    pub fn lookup(&self, inode: &InodeData, name: &[u8]) -> Result<Option<u64>, FsError> {
        directory::lookup(self, inode, name)
    }

    /// 返回下一真实目录项及可继续枚举的字节偏移。
    pub fn dir_next(
        &self,
        inode: &InodeData,
        offset: u64,
    ) -> Result<Option<DirectoryEntry>, FsError> {
        directory::next(self, inode, offset)
    }

    /// 按 inode 类型及已验证的 fast-link 判定读取原始链接字节。
    pub fn read_link(&self, inode: &InodeData) -> Result<Vec<u8>, FsError> {
        if inode.kind != InodeKind::Symlink {
            return Err(FsError::InvalidOperation);
        }
        let len = usize::try_from(inode.size).map_err(|_| FsError::Corrupt)?;
        if len == 0 || PAGE_SIZE < len {
            return Err(FsError::Corrupt);
        }
        let mut result = Vec::new();
        result
            .try_reserve_exact(len)
            .map_err(|_| FsError::NoMemory)?;
        if inode.fast_symlink {
            result.extend_from_slice(codec::bytes(&inode.block, 0, len)?);
        } else {
            let mut offset = 0usize;
            while offset < len {
                let logical = offset / self.geometry.block_size;
                let physical = match self.map_block(inode, logical as u64)? {
                    BlockMapping::Mapped { physical, .. } => physical,
                    _ => return Err(FsError::Corrupt),
                };
                let buffer = self.metadata_block(physical)?;
                let count = min(len - offset, self.geometry.block_size);
                buffer.with_bytes(|data| result.extend_from_slice(&data[..count]))?;
                offset += count;
            }
        }
        if result.contains(&0) {
            return Err(FsError::Corrupt);
        }
        Ok(result)
    }
}

/// 将映射准备失败的原始原因发表给同页等待者，不依赖析构取消语义。
fn finish_error(mut read: FolioRead, error: FsError) -> Result<(), FsError> {
    // SAFETY: 映射准备时页仍由 CPU 独占，此前分段 DMA 已结束；失败不授予字节读取权。
    unsafe {
        read.complete(IoResult {
            completed: 0,
            error: Some(error.folio_error()),
        });
    }
    Err(error)
}

/// CPU 收权阶段初始化原页的一段合法零内容，不发表中间可读状态。
fn zero_range(read: &mut FolioRead, offset: usize, len: usize) {
    for byte in &mut read.bytes_mut()[offset..offset + len] {
        byte.write(0);
    }
}
