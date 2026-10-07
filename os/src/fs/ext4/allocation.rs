//! 卷内分配序列保护共同位图与计数；本次未发表资源由显式释放归还。

use crate::cache::BufferRef;
use crate::fs::ext4::codec::{self, GroupState};
use crate::fs::ext4::{Ext4Volume, Geometry};
use crate::fs::FsError;

#[derive(Clone, Copy)]
pub(crate) struct GroupLayout {
    pub block_bitmap: u64,
    pub inode_bitmap: u64,
    pub inode_table: u64,
}

/// 保留本次位图拥有权及已经准备好的释放窗口；析构不得执行睡眠释放。
struct Reservation {
    volume: u32,
    group: u64,
    bit: usize,
    inode: bool,
    directory: bool,
    bitmap: BufferRef,
    descriptor: BufferRef,
    offset: usize,
    superblock: BufferRef,
    active: bool,
}

/// 尚未连接到文件块树的单块责任，不允许 Clone 或从任意块号构造。
pub struct UnpublishedBlock {
    physical: u64,
    reservation: Reservation,
}

/// 已有节点的预备释放窗口；准备时尚未摘树，取消计划只释放窗口引用。
pub(crate) struct DetachedBlock {
    reservation: Reservation,
    node: BufferRef,
}

/// 尚未连接到名字的 inode 责任，位图、记录与释放窗口均由本次申请保留。
pub struct UnpublishedInode {
    number: u64,
    reservation: Reservation,
}

impl UnpublishedBlock {
    /// 仅借用本次申请的物理地址，借用不会转移归还责任。
    pub fn physical(&self) -> u64 {
        self.physical
    }

    /// 映射指针正式发表后结束本次临时责任，不能再沿未发表接口归还。
    pub(crate) fn publish(mut self) {
        self.reservation.active = false;
    }
}

impl UnpublishedInode {
    /// 返回本次尚未发表的 inode 号。
    pub fn number(&self) -> u64 {
        self.number
    }

    /// 名字与 inode 连接成功后消费临时拥有权。
    pub(crate) fn publish(mut self) -> u64 {
        self.reservation.active = false;
        self.number
    }
}

impl Drop for Reservation {
    /// 未发表资源必须经过显式释放，析构只暴露丢失拥有权的协议错误。
    fn drop(&mut self) {
        assert!(!self.active, "unpublished ext4 reservation lost");
    }
}

impl Ext4Volume {
    /// 普通任务按组提示扫描真实位图；零空间与缓存、页、设备失败分别返回。
    pub fn allocate_block(&self, hint: u64) -> Result<UnpublishedBlock, FsError> {
        let reservation = self.reserve(hint, false, false)?;
        let physical = self.geometry.first_data_block
            + reservation.group * self.geometry.blocks_per_group
            + reservation.bit as u64;
        Ok(UnpublishedBlock {
            physical,
            reservation,
        })
    }

    /// 当前操作尚未发表块指针时显式归还，不执行任何已接受前缀的回滚。
    pub fn release_unpublished_block(&self, mut block: UnpublishedBlock) {
        self.release_reservation(&mut block.reservation);
    }

    /// 准备普通或目录 inode 的真实盘面记录，失败只返还本次尚未连接的 inode。
    pub fn allocate_inode(
        &self,
        hint: u64,
        mode: u16,
        uid: u32,
        gid: u32,
    ) -> Result<UnpublishedInode, FsError> {
        let directory = mode & 0xf000 == 0x4000;
        if !directory && mode & 0xf000 != 0x8000 {
            return Err(FsError::InvalidOperation);
        }
        let mut reservation = self.reserve(hint, true, directory)?;
        let number =
            reservation.group * self.geometry.inodes_per_group + reservation.bit as u64 + 1;
        let result = self.inode_record(number);
        let (buffer, offset) = match result {
            Ok(record) => record,
            Err(error) => {
                self.release_reservation(&mut reservation);
                return Err(error);
            }
        };
        let g = &self.geometry;
        buffer
            .update(&self.claim, |bytes| {
                let record = &mut bytes[offset..offset + g.inode_size];
                let generation = codec::le32(record, 100)
                    .expect("prepared inode record")
                    .wrapping_add(1);
                record.fill(0);
                codec::put16(record, 0, mode);
                codec::put16(record, 2, uid as u16);
                codec::put16(record, 120, (uid >> 16) as u16);
                codec::put16(record, 24, gid as u16);
                codec::put16(record, 122, (gid >> 16) as u16);
                codec::put32(record, 100, generation);
                let extra = if g.inode_size == 256 { 32 } else { 0 };
                if extra != 0 {
                    codec::put16(record, 128, extra as u16);
                }
                if g.features_incompat & 0x40 != 0 {
                    codec::put32(record, 32, codec::EXTENTS);
                    codec::put16(record, 40, 0xf30a);
                    codec::put16(record, 44, 4);
                }
                let seed = codec::crc32c(
                    codec::crc32c(g.checksum_seed, &(number as u32).to_le_bytes()),
                    &generation.to_le_bytes(),
                );
                codec::inode_checksum(g, record, seed, extra);
            })
            .expect("prepared writable inode record");
        Ok(UnpublishedInode {
            number,
            reservation,
        })
    }

    /// 本次 inode 尚未连接名字时归还位图与目录计数，不对已有对象作清理。
    pub fn release_unpublished_inode(&self, mut inode: UnpublishedInode) {
        self.release_reservation(&mut inode.reservation);
    }

    /// 节点合并在发表之前保留释放所需窗口，尚不改变位图或空闲计数。
    pub(crate) fn prepare_detached_block(&self, physical: u64) -> Result<DetachedBlock, FsError> {
        self.geometry.validate_blocks(physical, 1)?;
        let group = (physical - self.geometry.first_data_block) / self.geometry.blocks_per_group;
        let bit =
            ((physical - self.geometry.first_data_block) % self.geometry.blocks_per_group) as usize;
        let (descriptor, offset) = self.group_buffer(group)?;
        let bitmap = self.metadata_block(self.groups[group as usize].block_bitmap)?;
        let superblock = self.metadata.get(0, 4096)?;
        let node = self.metadata_block(physical)?;
        Ok(DetachedBlock {
            reservation: Reservation {
                volume: self.device,
                group,
                bit,
                inode: false,
                directory: false,
                bitmap,
                descriptor,
                offset,
                superblock,
                active: false,
            },
            node,
        })
    }

    /// 块根和 inode 记录均已去掉节点引用后，归还实际摘离节点的物理空间。
    ///
    /// # Safety
    /// caller 必须保持 inode root 写侧并已摘离节点；旧树读者和 IO 已结束，
    /// 新读者不能消费旧窗口，且失效前后的引用不能再提交旧内容。
    pub(crate) unsafe fn release_detached_block(&self, mut block: DetachedBlock) {
        // SAFETY: caller 证明真实摘树和旧使用者终结；node 保准确物理窗口，
        // forget 在共同内容域等待真实 WB 收权，只失效本窗口，随后才归还位图。
        unsafe { block.node.forget(&self.claim) }.expect("detached metadata node invalidation");
        block.reservation.active = true;
        self.release_reservation(&mut block.reservation);
    }

    /// 先取得所有可能睡眠的窗口；内容借用内只扫描、置位与计算 CRC。
    fn reserve(&self, hint: u64, inode: bool, directory: bool) -> Result<Reservation, FsError> {
        self.require_write(&self.claim)?;
        // ponytail: 卷分配睡眠锁串行化计数，真实分配竞争成为瓶颈时拆每组锁。
        let _allocation = self.allocation.lock();
        let g = &self.geometry;
        let superblock = self.metadata.get(0, 4096)?;
        let available = superblock.with_read(|bytes| -> Result<u64, FsError> {
            let record = &bytes[1024..2048];
            if inode {
                return Ok(u64::from(codec::le32(record, 16)?));
            }
            Ok(u64::from(codec::le32(record, 12)?)
                | if g.features_incompat & 0x80 != 0 {
                    u64::from(codec::le32(record, 344)?) << 32
                } else {
                    0
                })
        })??;
        if available == 0 {
            return Err(FsError::NoSpace);
        }
        for step in 0..g.groups_count {
            let group = (hint % g.groups_count + step) % g.groups_count;
            let (descriptor, offset) = self.group_buffer(group)?;
            let mut state = descriptor.with_read(|bytes| {
                let record = codec::bytes(bytes, offset, g.descriptor_size)?;
                codec::decode_group(g, group, record)?;
                codec::group_state(g, record)
            })??;
            if if inode {
                state.free_inodes
            } else {
                state.free_blocks
            } == 0
            {
                continue;
            }
            let layout = self.groups[group as usize];
            let bitmap = self.metadata_block(if inode {
                layout.inode_bitmap
            } else {
                layout.block_bitmap
            })?;
            if inode && state.flags & 2 != 0 {
                let blocks = self.metadata_block(layout.block_bitmap)?;
                self.initialize_bitmap(group, false, &blocks, &descriptor, offset, &mut state)?;
            }
            self.initialize_bitmap(group, inode, &bitmap, &descriptor, offset, &mut state)?;
            if g.features_ro_compat & codec::METADATA_CSUM != 0 {
                let sum = bitmap.with_read(|bits| codec::bitmap_checksum(g, bits, inode))?;
                let stored = if inode {
                    state.inode_checksum
                } else {
                    state.block_checksum
                };
                if stored != sum {
                    return Err(FsError::Checksum);
                }
            }
            let valid = valid_bits(g, group, inode);
            let first = if inode && group == 0 {
                g.first_inode.saturating_sub(1) as usize
            } else {
                0
            };
            let bit =
                bitmap.with_read(|bits| (first..valid).find(|&bit| !bit_is_set(bits, bit)))?;
            let Some(bit) = bit else {
                return Err(FsError::Corrupt);
            };
            let checksum = bitmap
                .update(&self.claim, |bits| {
                    assert!(!bit_is_set(bits, bit), "ext4 double allocation");
                    set_bit(bits, bit, true);
                    codec::bitmap_checksum(g, bits, inode)
                })
                .expect("prepared writable bitmap");
            if inode {
                state.free_inodes -= 1;
                state.inode_checksum = checksum;
                if directory {
                    state.used_dirs = state
                        .used_dirs
                        .checked_add(1)
                        .expect("directory count overflow");
                }
                state.unused_inodes = state
                    .unused_inodes
                    .min((g.inodes_per_group - bit as u64 - 1) as u32);
            } else {
                state.free_blocks -= 1;
                state.block_checksum = checksum;
            }
            self.commit_group(group, &descriptor, offset, state);
            superblock
                .update(&self.claim, |bytes| {
                    codec::patch_super_counts(
                        g,
                        &mut bytes[1024..2048],
                        if inode { 0 } else { -1 },
                        if inode { -1 } else { 0 },
                    )
                })
                .expect("prepared writable superblock");
            return Ok(Reservation {
                volume: self.device,
                group,
                bit,
                inode,
                directory,
                bitmap,
                descriptor,
                offset,
                superblock,
                active: true,
            });
        }
        Err(FsError::NoSpace)
    }

    /// 合法延迟初始化按物理布局保留全部 flex 元数据和末组尾位。
    fn initialize_bitmap(
        &self,
        group: u64,
        inode: bool,
        bitmap: &BufferRef,
        descriptor: &BufferRef,
        offset: usize,
        state: &mut GroupState,
    ) -> Result<(), FsError> {
        let flag = if inode { 1 } else { 2 };
        if state.flags & flag == 0 {
            return Ok(());
        }
        let g = &self.geometry;
        if group == 0 || g.features_ro_compat & (0x10 | codec::METADATA_CSUM) == 0 {
            return Err(FsError::Corrupt);
        }
        let valid = valid_bits(g, group, inode);
        let mut initialized = [0u8; 4096];
        let bits = &mut initialized[..g.block_size];
        if !inode {
            mark_metadata(g, &self.groups, group, bits);
        }
        for bit in valid..bits.len() * 8 {
            set_bit(bits, bit, true);
        }
        let free = (0..valid).filter(|&bit| !bit_is_set(bits, bit)).count() as u32;
        if free
            != if inode {
                state.free_inodes
            } else {
                state.free_blocks
            }
        {
            return Err(FsError::Corrupt);
        }
        let checksum = codec::bitmap_checksum(g, bits, inode);
        bitmap
            .update(&self.claim, |current| current.copy_from_slice(bits))
            .expect("prepared uninitialized bitmap");
        state.flags &= !flag;
        if inode {
            state.inode_checksum = checksum;
        } else {
            state.block_checksum = checksum;
        }
        self.commit_group(group, descriptor, offset, *state);
        Ok(())
    }

    /// 只更新当前描述符字段，不把此前读取的整块副本覆盖共同内容。
    fn commit_group(&self, group: u64, descriptor: &BufferRef, offset: usize, state: GroupState) {
        descriptor
            .update(&self.claim, |bytes| {
                codec::patch_group(
                    &self.geometry,
                    group,
                    &mut bytes[offset..offset + self.geometry.descriptor_size],
                    state,
                )
            })
            .expect("prepared writable group descriptor");
    }

    /// 释放窗口随 token 保留，释放不申请内存、不读盘，内容锁等待在短锁之外。
    fn release_reservation(&self, reservation: &mut Reservation) {
        assert_eq!(
            reservation.volume, self.device,
            "reservation belongs to another volume"
        );
        assert!(
            reservation.active,
            "reservation already published or released"
        );
        let _allocation = self.allocation.lock();
        let g = &self.geometry;
        let mut state = reservation
            .descriptor
            .with_read(|bytes| {
                codec::group_state(
                    g,
                    &bytes[reservation.offset..reservation.offset + g.descriptor_size],
                )
            })
            .expect("retained group buffer")
            .expect("retained group descriptor");
        let sum = reservation
            .bitmap
            .update(&self.claim, |bits| {
                assert!(bit_is_set(bits, reservation.bit), "ext4 double release");
                set_bit(bits, reservation.bit, false);
                codec::bitmap_checksum(g, bits, reservation.inode)
            })
            .expect("retained writable bitmap");
        if reservation.inode {
            state.free_inodes += 1;
            state.inode_checksum = sum;
            if reservation.directory {
                state.used_dirs -= 1;
            }
        } else {
            state.free_blocks += 1;
            state.block_checksum = sum;
        }
        self.commit_group(
            reservation.group,
            &reservation.descriptor,
            reservation.offset,
            state,
        );
        reservation
            .superblock
            .update(&self.claim, |bytes| {
                codec::patch_super_counts(
                    g,
                    &mut bytes[1024..2048],
                    if reservation.inode { 0 } else { 1 },
                    if reservation.inode { 1 } else { 0 },
                )
            })
            .expect("retained writable superblock");
        reservation.active = false;
    }
}

/// 位图有效位按实际组尾计算；inode 编号和块号具有各自独立的末组边界。
fn valid_bits(g: &Geometry, group: u64, inode: bool) -> usize {
    if inode {
        (g.inodes_count - group * g.inodes_per_group).min(g.inodes_per_group) as usize
    } else {
        (g.blocks_count - g.first_data_block - group * g.blocks_per_group).min(g.blocks_per_group)
            as usize
    }
}

/// 小端位序与盘面 ext4 位图一致，不将字节尾 padding 当作可分配空间。
fn bit_is_set(bits: &[u8], bit: usize) -> bool {
    bits[bit / 8] & (1 << (bit % 8)) != 0
}

/// 在已验证的位图范围内修改一位。
fn set_bit(bits: &mut [u8], bit: usize, used: bool) {
    if used {
        bits[bit / 8] |= 1 << (bit % 8);
    } else {
        bits[bit / 8] &= !(1 << (bit % 8));
    }
}

/// 元数据按物理交集置位，多个逻辑组的 flex 表可以共同占据当前物理组。
fn mark_metadata(g: &Geometry, layouts: &[GroupLayout], group: u64, bits: &mut [u8]) {
    let first = g.first_data_block + group * g.blocks_per_group;
    let end = (first + g.blocks_per_group).min(g.blocks_count);
    if g.has_super(group) {
        mark_range(
            bits,
            first,
            end,
            first,
            1 + g.descriptor_blocks + g.reserved_gdt_blocks,
        );
    }
    let table_blocks = g.inodes_per_group * g.inode_size as u64 / g.block_size as u64;
    for layout in layouts {
        mark_range(bits, first, end, layout.block_bitmap, 1);
        mark_range(bits, first, end, layout.inode_bitmap, 1);
        mark_range(bits, first, end, layout.inode_table, table_blocks);
    }
}

/// 范围求交后置位；重复占用只保同一个 used 位，不重复扣计数。
fn mark_range(bits: &mut [u8], first: u64, end: u64, start: u64, len: u64) {
    for physical in start.max(first)..(start + len).min(end) {
        set_bit(bits, (physical - first) as usize, true);
    }
}
