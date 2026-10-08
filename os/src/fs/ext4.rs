//! ext4 卷保存实际 FS 权限和共同元数据页，块映射由 canonical inode 的 data-sem 保护。

mod allocation;
pub use allocation::{UnpublishedBlock, UnpublishedInode};
pub(crate) mod codec;
pub(crate) mod crc;
mod directory;
mod mapping;

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::cmp::min;
use core::convert::TryFrom;
use core::sync::atomic::{AtomicU32, Ordering};

use crate::block::{AccessMode, Disk, DiskRef, FsClaim, IoResult, SECTOR_BYTES};
use crate::cache::{BufferRef, MetadataCache};
use crate::fs::inode::{InodeDomain, InodeMetadata, InodeRef};
use crate::fs::FsError;
use crate::mm::{FolioRead, PAGE_SIZE};
use crate::sync::{SleepMutex, SpinLock};

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
    pub first_inode: u64,
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
    metadata: Arc<MetadataCache>,
    geometry: Geometry,
    claim: FsClaim,
    inode_domain: SpinLock<Weak<InodeDomain>>,
    allocation: SleepMutex<()>,
    groups: Vec<allocation::GroupLayout>,
    #[cfg(feature = "ktest")]
    segment_hook: SpinLock<Option<SegmentHook>>,
}

/// 虚拟卷编号只表示本内核的独立卷实例，耗尽后拒绝构造而不回绕复用。
static NEXT_DEVICE: AtomicU32 = AtomicU32::new(1);

impl Ext4Volume {
    /// 普通任务在 Disk 共同域中取得唯一卷；活卷的不同权限请求明确拒绝。
    /// RO Mount 可直接使用同一个 RW 卷，卷模式不替代挂载视图权限。
    /// 同 Disk 活缓存的容量须完全相同；不同活 inode 缓存域由真实 token 明确拒绝。
    /// 卷反向只存 Weak，外部文件页保 inode，元数据页保 cache；两者保持原占用。
    /// 加载失败不发表卷弱引用，局部 claim 与候选按 RAII 退出，不取消外部旧拥有。
    pub fn open(
        disk: DiskRef,
        mode: AccessMode,
        metadata_capacity: usize,
    ) -> Result<Arc<Self>, FsError> {
        let claim = Disk::claim_fs(&disk, mode)?;
        let metadata = MetadataCache::shared(&claim, metadata_capacity)?;
        let mut published = metadata.filesystem.lock();
        if let Some(volume) = published.upgrade() {
            if volume.mode() != mode {
                return Err(FsError::Busy);
            }
            return Ok(volume);
        }
        let capacity = disk
            .limits()
            .capacity_sectors
            .checked_mul(SECTOR_BYTES)
            .ok_or(FsError::Corrupt)?;
        // 主超级块在 1024 字节处；读设备对齐的整页后仅解码其固定范围。
        let superblock = metadata.get(0, PAGE_SIZE)?;
        let geometry = superblock
            .with_read(|data| codec::decode_super(codec::bytes(data, 1024, 1024)?, capacity))??;
        drop(superblock);
        let logical = disk.limits().logical_block_size as usize;
        if geometry.block_size % logical != 0 {
            return Err(FsError::Unsupported);
        }
        // 未识别的 RO-compatible 格式可以读取，不能授予当前后端写能力。
        let supported_ro = 0x1 | 0x2 | 0x8 | 0x10 | 0x20 | 0x40 | codec::METADATA_CSUM;
        if mode == AccessMode::ReadWrite && geometry.features_ro_compat & !supported_ro != 0 {
            return Err(FsError::Unsupported);
        }
        let mut volume = Self {
            device: NEXT_DEVICE
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |next| {
                    next.checked_add(1)
                })
                .map_err(|_| FsError::NoMemory)?,
            disk,
            metadata: metadata.clone(),
            geometry,
            claim,
            inode_domain: SpinLock::new(Weak::new()),
            allocation: SleepMutex::new(()),
            groups: Vec::new(),
            #[cfg(feature = "ktest")]
            segment_hook: SpinLock::new(None),
        };
        volume
            .groups
            .try_reserve_exact(volume.geometry.groups_count as usize)
            .map_err(|_| FsError::NoMemory)?;
        for group in 0..volume.geometry.groups_count {
            let (buffer, offset) = volume.group_buffer(group)?;
            let layout = buffer.with_read(|data| {
                codec::group_layout(
                    &volume.geometry,
                    group,
                    codec::bytes(data, offset, volume.geometry.descriptor_size)?,
                )
            })??;
            volume.groups.push(layout);
        }
        let volume = Arc::try_new(volume).map_err(|_| FsError::NoMemory)?;
        *published = Arc::downgrade(&volume);
        Ok(volume)
    }

    /// 返回卷自己的 FS 能力，不推导任何 Path/Mount 的写许可。
    pub fn mode(&self) -> AccessMode {
        self.claim.mode()
    }

    /// 借用真实 FS 占用；授权的 Bio 仍通过唯一 Disk 请求核心提交。
    pub fn claim(&self) -> &FsClaim {
        &self.claim
    }

    /// 正式回写借用同 Disk 的唯一元数据索引，不建立第二个设备内容域。
    pub fn metadata_cache(&self) -> &Arc<MetadataCache> {
        &self.metadata
    }

    /// 同一活卷只允许一个 inode 缓存域，旧 inode/外部文件页保留域身份时拒绝冲突。
    /// 反向仅存 Weak，cache 和 inode 的真实拥有退出后可由新域取得。
    pub(crate) fn bind_inode_domain(&self, candidate: &Arc<InodeDomain>) -> Result<(), FsError> {
        let retired = {
            let mut domain = self.inode_domain.lock();
            if let Some(previous) = domain.upgrade() {
                let same = Arc::ptr_eq(&previous, candidate);
                drop(domain);
                drop(previous);
                return if same { Ok(()) } else { Err(FsError::Busy) };
            }
            core::mem::replace(&mut *domain, Arc::downgrade(candidate))
        };
        drop(retired);
        Ok(())
    }

    /// 返回准备时固定的弱登记预算，扫描不能因新 Dirty 无限延长。
    pub(crate) fn dirty_inode_limit(&self) -> usize {
        let domain = self.inode_domain.lock().upgrade();
        domain.map_or(0, |domain| domain.dirty_inode_limit())
    }

    /// 只取得本卷同一 canonical inode 的真实脏义务，弱域和最后 put 均在短锁外。
    pub(crate) fn dirty_inode_at(&self, position: usize) -> Option<InodeRef> {
        let domain = self.inode_domain.lock().upgrade();
        domain
            .and_then(|domain| domain.dirty_inode_at(position))
            .filter(|inode| core::ptr::eq(inode.filesystem().as_ref(), self))
    }

    /// 借用一次解码并验证的有效几何。
    pub fn geometry(&self) -> &Geometry {
        &self.geometry
    }

    /// 同一卷的所有 Arc 引用共享稳定非零编号，不泄漏地址或宿主设备号。
    pub fn device(&self) -> u64 {
        u64::from(self.device)
    }

    /// 借用保活且被实际 FS 占用保护的磁盘。
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

    /// 定位主描述符的共同窗口，取得内容之前不持任何元数据短锁。
    fn group_buffer(&self, group: u64) -> Result<(BufferRef, usize), FsError> {
        let g = &self.geometry;
        if g.groups_count <= group {
            return Err(FsError::Corrupt);
        }
        let byte = group * g.descriptor_size as u64;
        let block = g.first_data_block + 1 + byte / g.block_size as u64;
        Ok((
            self.metadata.get(block, g.block_size)?,
            (byte % g.block_size as u64) as usize,
        ))
    }

    /// 布局地址在卷打开时验证一次；可变计数和位图仍消费共同 Buffer 当前字段。
    fn inode_table(&self, group: u64) -> Result<u64, FsError> {
        self.groups
            .get(group as usize)
            .map(|layout| layout.inode_table)
            .ok_or(FsError::Corrupt)
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
        let inode = buffer.with_read(|data| {
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

    /// caller 保持 data-sem 读侧到真实查询结束，合法空洞与 EOF 不掩盖格式错误。
    pub fn map_block(
        &self,
        metadata: &InodeMetadata,
        root: &[u8; 60],
        logical: u64,
    ) -> Result<BlockMapping, FsError> {
        let inode = metadata.with_block(root);
        if inode.fast_symlink || inode.kind == InodeKind::Other {
            return Err(FsError::Unsupported);
        }
        let end = codec::ceil_div(inode.size, self.geometry.block_size as u64)?;
        if end <= logical {
            return Ok(BlockMapping::Eof);
        }
        mapping::map(self, &inode, logical, end)
    }

    /// caller 保持 data-sem 读侧到原页所有分段 IO 结束，全部初始化后才唯一发表。
    pub fn fill_page(
        &self,
        inode: &InodeMetadata,
        root: &[u8; 60],
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
            let mapping = match self.map_block(inode, root, byte / size as u64) {
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

    /// caller 持块映射读保护；单组件名字查找只在元数据 guard 内消费字节。
    pub fn lookup(
        &self,
        inode: &InodeMetadata,
        root: &[u8; 60],
        name: &[u8],
    ) -> Result<Option<u64>, FsError> {
        directory::lookup(self, inode, root, name)
    }

    /// caller 持块映射读保护，返回下一真实目录项及后端字节游标。
    pub fn dir_next(
        &self,
        inode: &InodeMetadata,
        root: &[u8; 60],
        offset: u64,
    ) -> Result<Option<DirectoryEntry>, FsError> {
        directory::next(self, inode, root, offset)
    }

    /// caller 持块映射读保护，按当前属性与块根读取符号链接原始字节。
    pub fn read_link(&self, inode: &InodeMetadata, root: &[u8; 60]) -> Result<Vec<u8>, FsError> {
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
            result.extend_from_slice(codec::bytes(root, 0, len)?);
        } else {
            let mut offset = 0usize;
            while offset < len {
                let logical = offset / self.geometry.block_size;
                let physical = match self.map_block(inode, root, logical as u64)? {
                    BlockMapping::Mapped { physical, .. } => physical,
                    _ => return Err(FsError::Corrupt),
                };
                let buffer = self.metadata_block(physical)?;
                let count = min(len - offset, self.geometry.block_size);
                buffer.with_read(|data| result.extend_from_slice(&data[..count]))?;
                offset += count;
            }
        }
        if result.contains(&0) {
            return Err(FsError::Corrupt);
        }
        Ok(result)
    }

    /// 核实当次挂载能力属于同一真实 Disk，并且卷和挂载都授予写许可。
    pub(crate) fn require_write(&self, claim: &FsClaim) -> Result<(), FsError> {
        if claim.mode() != AccessMode::ReadWrite || self.mode() != AccessMode::ReadWrite {
            return Err(FsError::ReadOnly);
        }
        if !core::ptr::eq(
            claim.disk().as_ref().get_ref(),
            self.disk.as_ref().get_ref(),
        ) {
            return Err(FsError::InvalidOperation);
        }
        Ok(())
    }

    /// 在块树或目的内容发表前保留本 inode 的真实记录窗口。
    pub(crate) fn inode_record(&self, number: u64) -> Result<(BufferRef, usize), FsError> {
        let g = &self.geometry;
        if number == 0 || g.inodes_count < number {
            return Err(FsError::Corrupt);
        }
        let table = self.inode_table((number - 1) / g.inodes_per_group)?;
        let byte = ((number - 1) % g.inodes_per_group) * g.inode_size as u64;
        Ok((
            self.metadata_block(table + byte / g.block_size as u64)?,
            (byte % g.block_size as u64) as usize,
        ))
    }

    /// caller 保持 inode 写序列化；准备不会连接新资源，失败有显式本次释放。
    pub(crate) fn prepare_write(
        &self,
        inode: &InodeMetadata,
        root: &[u8; 60],
        logical: u64,
    ) -> Result<mapping::PreparedMapping, FsError> {
        mapping::prepare(self, &inode.with_block(root), logical)
    }

    /// 先准备 inode-table 真实窗口，再在同一页锁内仅修改当前 inode 的字段和校验和。
    pub(crate) fn update_inode_metadata(
        &self,
        claim: &FsClaim,
        old: &InodeMetadata,
        next: &InodeMetadata,
    ) -> Result<(), FsError> {
        codec::validate_inode_metadata_update(old, next)?;
        let g = &self.geometry;
        let group = (old.number - 1) / g.inodes_per_group;
        let table = self.inode_table(group)?;
        let byte = ((old.number - 1) % g.inodes_per_group) * g.inode_size as u64;
        let buffer = self.metadata_block(table + byte / g.block_size as u64)?;
        let offset = (byte % g.block_size as u64) as usize;
        buffer.update(claim, |bytes| {
            codec::patch_inode_metadata(g, &mut bytes[offset..offset + g.inode_size], old, next)
        })??;
        Ok(())
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
