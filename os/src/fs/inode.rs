//! canonical inode 的加载结果、运行属性和块映射分别受实际拥有与同步协议保护。

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::marker::PhantomPinned;
use core::ops::Deref;
use core::pin::{pin, Pin};
#[cfg(feature = "ktest")]
use core::sync::atomic::{AtomicUsize, Ordering};
use spin::Once;

use crate::block::{AccessMode, FsClaim};
use crate::cache::FileMapping;
use crate::fs::ext4::{
    codec, BlockMapping, DirectoryEntry, Ext4Volume, InodeData, InodeKind, InodeTime,
};
use crate::fs::{FsError, ReadBuffer, WriteBuffer};
use crate::mm::{FolioRef, PAGE_SIZE};
use crate::sched;
use crate::sync::{SleepMutex, SleepRwLock, SpinLock};
use crate::time;
use crate::wait::{WaitNode, WaitQueue, Waiter};

#[cfg(feature = "ktest")]
type LoadHook = Arc<dyn Fn(&InodeRef) + Send + Sync>;

/// 独立缓存域强持 inode；后端只由 inode 保活，不反向拥有缓存域。
/// ponytail: 固定预算内线性查找，预算显著增大且查找成为瓶颈时改为有限散列表。
pub struct InodeCache {
    entries: SpinLock<Vec<InodeRef>>,
    capacity: usize,
    domain: Arc<()>,
    #[cfg(feature = "ktest")]
    load_hook: SpinLock<Option<LoadHook>>,
    #[cfg(feature = "ktest")]
    loads: AtomicUsize,
}

/// 私有 Arc 保证等待源从发表到最后引用结束始终留在原分配中。
/// 仅提供共享借用，不允许取回 Arc、取得可变借用或移出 inode。
#[derive(Clone)]
pub struct InodeRef(Arc<Inode>);

/// 同一 FS 实例中的唯一运行对象，未发表加载结果时处于 Loading。
pub struct Inode {
    filesystem: Arc<Ext4Volume>,
    _domain: Arc<()>,
    number: u64,
    data: Once<Result<InodeState, FsError>>,
    mapping: FileMapping,
    waiters: WaitQueue,
    _pinned: PhantomPinned,
}

/// 有界属性快照不含块根；size 只在 canonical 属性锁内保存一份运行值。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InodeMetadata {
    pub number: u64,
    pub kind: InodeKind,
    pub mode: u16,
    pub size: u64,
    pub uid: u32,
    pub gid: u32,
    pub links: u32,
    pub blocks: u64,
    pub atime: InodeTime,
    pub mtime: InodeTime,
    pub ctime: InodeTime,
    pub generation: u32,
    pub flags: u32,
    pub checksum_seed: u32,
    pub fast_symlink: bool,
    pub file_acl: u64,
    pub extra_isize: usize,
}

struct InodeState {
    write: SleepMutex<()>,
    metadata: SpinLock<InodeMetadata>,
    root: SleepRwLock<[u8; 60]>,
    writecount: SpinLock<isize>,
}

impl InodeMetadata {
    /// 将盘面加载值拆为运行属性，块根单独进入睡眠读写保护。
    pub(crate) fn from_data(data: &InodeData) -> Self {
        Self {
            number: data.number,
            kind: data.kind,
            mode: data.mode,
            size: data.size,
            uid: data.uid,
            gid: data.gid,
            links: data.links,
            blocks: data.blocks,
            atime: data.atime,
            mtime: data.mtime,
            ctime: data.ctime,
            generation: data.generation,
            flags: data.flags,
            checksum_seed: data.checksum_seed,
            fast_symlink: data.fast_symlink,
            file_acl: data.file_acl,
            extra_isize: data.extra_isize,
        }
    }

    /// 仅为当次真实块映射查询组装 codec 输入，不保存第二份可变状态。
    pub(crate) fn with_block(&self, block: &[u8; 60]) -> InodeData {
        InodeData {
            number: self.number,
            kind: self.kind,
            mode: self.mode,
            size: self.size,
            uid: self.uid,
            gid: self.gid,
            links: self.links,
            blocks: self.blocks,
            atime: self.atime,
            mtime: self.mtime,
            ctime: self.ctime,
            generation: self.generation,
            flags: self.flags,
            checksum_seed: self.checksum_seed,
            fast_symlink: self.fast_symlink,
            file_acl: self.file_acl,
            extra_isize: self.extra_isize,
            block: *block,
        }
    }
}

impl InodeState {
    /// 完成真实加载后一次安装同步容器，初始化不执行额外 IO。
    fn new(data: InodeData) -> Self {
        Self {
            write: SleepMutex::new(()),
            metadata: SpinLock::new(InodeMetadata::from_data(&data)),
            root: SleepRwLock::new(data.block),
            writecount: SpinLock::new(0),
        }
    }
}

/// 真正写打开持有的 inode 责任；dup 共享 OFD 不复制本 token。
pub struct WriteAccess {
    inode: InodeRef,
}

/// 每个活执行地址空间保有独立 deny 责任，最后实际映像使用结束才释放。
pub struct ExecAccess {
    inode: InodeRef,
}

impl WriteAccess {
    /// 写能力始终绑定取得时的 canonical inode，不能借给其他文件。
    pub fn write_from(
        &self,
        claim: &FsClaim,
        offset: u64,
        append: bool,
        source: &mut WriteBuffer<'_>,
    ) -> Result<(u64, usize), FsError> {
        self.inode.write_from(claim, offset, append, source)
    }
}

impl Drop for WriteAccess {
    /// 只归还原 inode 的一次写拥有，计数短锁内不分配或等待。
    fn drop(&mut self) {
        let mut count = self.inode.loaded_state().writecount.lock();
        assert!(0 < *count, "write access count lost");
        *count -= 1;
    }
}

impl Clone for ExecAccess {
    /// fork 增加同 inode 的独立映像拥有，活 deny 期间不可能出现正写计数。
    fn clone(&self) -> Self {
        let mut count = self.inode.loaded_state().writecount.lock();
        assert!(*count < 0, "exec access count lost");
        *count = count.checked_sub(1).expect("exec access count overflow");
        Self {
            inode: self.inode.clone(),
        }
    }
}

impl Drop for ExecAccess {
    /// 地址空间最后退出时归还本次 deny，不能替其他映像释放。
    fn drop(&mut self) {
        let mut count = self.inode.loaded_state().writecount.lock();
        assert!(*count < 0, "exec access count lost");
        *count += 1;
    }
}

/// 只有胜出的候选取得此责任；提前退出发布取消结果并解除自己的占位。
struct LoadGuard<'a> {
    cache: &'a InodeCache,
    inode: &'a InodeRef,
    active: bool,
}

impl InodeCache {
    /// 锁外预留有限 inode 槽；文件页索引不预留每文件固定页槽。
    pub fn try_new(inode_capacity: usize) -> Result<Self, FsError> {
        if inode_capacity == 0 {
            return Err(FsError::InvalidOperation);
        }
        let mut entries = Vec::new();
        entries
            .try_reserve_exact(inode_capacity)
            .map_err(|_| FsError::NoMemory)?;
        Ok(Self {
            entries: SpinLock::new(entries),
            capacity: inode_capacity,
            domain: Arc::try_new(()).map_err(|_| FsError::NoMemory)?,
            #[cfg(feature = "ktest")]
            load_hook: SpinLock::new(None),
            #[cfg(feature = "ktest")]
            loads: AtomicUsize::new(0),
        })
    }

    /// 普通可切换 Task 获取同身份对象；加载与等待均在索引锁和 once 初始化之外。
    pub fn iget(&self, filesystem: &Arc<Ext4Volume>, number: u64) -> Result<InodeRef, FsError> {
        sched::assert_switchable();
        filesystem.bind_inode_domain(&self.domain)?;
        let (existing, full) = {
            let entries = self.entries.lock();
            (
                find(&entries, filesystem, number),
                self.capacity <= entries.len(),
            )
        };
        if let Some(inode) = existing {
            inode.wait_loaded()?;
            return Ok(inode);
        }
        if full {
            self.prune_unused();
            // 回收期间可能发表同 key，须先查身份再判断预算，避免命中也报 OOM。
            let entries = self.entries.lock();
            let existing = find(&entries, filesystem, number);
            let full = self.capacity <= entries.len();
            drop(entries);
            if let Some(inode) = existing {
                inode.wait_loaded()?;
                return Ok(inode);
            }
            if full {
                return Err(FsError::NoMemory);
            }
        }
        let candidate = InodeRef::try_new(filesystem, number, &self.domain)?;
        let mut loader = LoadGuard {
            cache: self,
            inode: &candidate,
            active: false,
        };
        let existing = match self.publish(&candidate, &mut loader) {
            Err(FsError::NoMemory) => {
                // 候选准备期间预算可能被占满；只做一次锁外回收，再查身份并发表。
                self.prune_unused();
                self.publish(&candidate, &mut loader)?
            }
            result => result?,
        };
        if let Some(inode) = existing {
            // 竞争失败者没有加载责任，不改变赢家的 once 或等待队列。
            drop(loader);
            drop(candidate);
            inode.wait_loaded()?;
            return Ok(inode);
        }
        #[cfg(feature = "ktest")]
        {
            self.loads.fetch_add(1, Ordering::Relaxed);
            let hook = self.load_hook.lock().clone();
            if let Some(hook) = hook {
                hook(&candidate);
            }
        }
        let result = filesystem.load_inode(number);
        loader.complete(result);
        drop(loader);
        candidate.wait_loaded()?;
        Ok(candidate)
    }

    /// 逐个移出 cache-only 且已静止的 inode，文件页和后端引用均在索引锁外释放。
    pub fn prune_unused(&self) -> usize {
        let mut removed = 0;
        for position in (0..self.len()).rev() {
            let candidate = {
                let entries = self.entries.lock();
                entries
                    .get(position)
                    .filter(|inode| Arc::strong_count(&inode.0) == 1 && inode.data.get().is_some())
                    .cloned()
            };
            let Some(candidate) = candidate else {
                continue;
            };
            // mapping 可把 folio 或 IO owner 交给外部；有活页时不得重造另一个 inode。
            candidate.mapping.prune_unused();
            let victim = {
                let mut entries = self.entries.lock();
                let same = entries
                    .get(position)
                    .is_some_and(|inode| inode.ptr_eq(&candidate));
                // 只有缓存和本次扫描引用时才查 mapping；索引到页索引的短锁顺序
                // 不跨页释放。页填充和页回收从不反向取得 inode 索引锁。
                if same && Arc::strong_count(&candidate.0) == 2 && candidate.mapping.is_empty() {
                    Some(entries.swap_remove(position))
                } else {
                    None
                }
            };
            if victim.is_some() {
                removed += 1;
            }
            drop(victim);
            drop(candidate);
        }
        removed
    }

    /// 返回占用槽数，Loading 也占用构造时固定的预算。
    pub fn len(&self) -> usize {
        self.entries.lock().len()
    }

    /// 观察本缓存域是否没有已发表的 inode。
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// 观察包含加载中对象的固定槽预算，名字索引可先释放静止绑定持有的 inode 引用。
    pub(crate) fn is_full(&self) -> bool {
        self.capacity <= self.len()
    }

    /// 双查同身份后发表 Loading；锁内只增加引用和占槽，不分配、加载或释放页。
    fn publish(
        &self,
        candidate: &InodeRef,
        loader: &mut LoadGuard<'_>,
    ) -> Result<Option<InodeRef>, FsError> {
        let mut entries = self.entries.lock();
        if let Some(inode) = find(&entries, &candidate.filesystem, candidate.number) {
            return Ok(Some(inode));
        }
        if self.capacity <= entries.len() {
            return Err(FsError::NoMemory);
        }
        // 构造时预留全部槽位，push 不分配；guard 先于 Loading 对外发表承担清理责任。
        loader.active = true;
        entries.push(candidate.clone());
        Ok(None)
    }

    /// 只摘除加载失败的原对象，后续同 key 的新加载者不受旧 guard 影响。
    fn remove_failed(&self, inode: &InodeRef) {
        let victim = {
            let mut entries = self.entries.lock();
            entries
                .iter()
                .position(|entry| entry.ptr_eq(inode))
                .map(|position| entries.swap_remove(position))
        };
        drop(victim);
    }

    /// 在真实 Loading 发表之后、读取 inode 之前运行锁外停点；旧回调锁外析构。
    #[cfg(feature = "ktest")]
    pub(crate) fn ktest_set_load_hook(&self, hook: Option<LoadHook>) {
        let old = core::mem::replace(&mut *self.load_hook.lock(), hook);
        drop(old);
    }

    /// 返回真正取得加载责任的次数，不计命中与私有竞争失败候选。
    #[cfg(feature = "ktest")]
    pub(crate) fn ktest_load_count(&self) -> usize {
        self.loads.load(Ordering::Relaxed)
    }

    /// 借用原对象读取等待源成员数，观测不制造另一份等待状态。
    #[cfg(feature = "ktest")]
    pub(crate) fn ktest_waiting(&self, filesystem: &Arc<Ext4Volume>, number: u64) -> usize {
        let inode = find(&self.entries.lock(), filesystem, number);
        inode.map_or(0, |inode| inode.waiters.test_members())
    }
}

impl InodeRef {
    /// 锁外准备尚未发表的稳定对象与唯一 mapping，任何分配失败均无索引占位。
    fn try_new(
        filesystem: &Arc<Ext4Volume>,
        number: u64,
        domain: &Arc<()>,
    ) -> Result<Self, FsError> {
        let mapping = FileMapping::new();
        let inode = Arc::try_new(Inode {
            filesystem: Arc::clone(filesystem),
            _domain: domain.clone(),
            number,
            data: Once::new(),
            mapping,
            waiters: WaitQueue::new(),
            _pinned: PhantomPinned,
        })
        .map_err(|_| FsError::NoMemory)?;
        Ok(Self(inode))
    }

    /// 比较原对象身份，不以相同卷号、元数据内容或盘上 inode 号代替实例身份。
    pub fn ptr_eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

    /// 同一短计数锁线性化写打开与 exec deny；失败不改变原拥有。
    pub fn acquire_write(&self) -> Result<WriteAccess, FsError> {
        let state = self.wait_loaded()?;
        let mut count = state.writecount.lock();
        if *count < 0 {
            return Err(FsError::TextBusy);
        }
        *count = count.checked_add(1).expect("write access count overflow");
        Ok(WriteAccess {
            inode: self.clone(),
        })
    }

    /// 第一次映像读取前取得 deny，任何真正写 owner 活时明确 TextBusy。
    pub fn deny_writes(&self) -> Result<ExecAccess, FsError> {
        let state = self.wait_loaded()?;
        let mut count = state.writecount.lock();
        if 0 < *count {
            return Err(FsError::TextBusy);
        }
        *count = count.checked_sub(1).expect("exec access count overflow");
        Ok(ExecAccess {
            inode: self.clone(),
        })
    }

    /// 唯一普通写循环只由绑定本 inode 的 WriteAccess 调用；输入 fault 不持块根或内容锁。
    fn write_from(
        &self,
        claim: &FsClaim,
        offset: u64,
        append: bool,
        source: &mut WriteBuffer<'_>,
    ) -> Result<(u64, usize), FsError> {
        self.filesystem.require_write(claim)?;
        let state = self.wait_loaded()?;
        let _write = state.write.lock();
        let initial = *state.metadata.lock();
        if initial.kind == InodeKind::Directory {
            return Err(FsError::IsDirectory);
        }
        if initial.kind != InodeKind::Regular {
            return Err(FsError::InvalidOperation);
        }
        let start = if append { initial.size } else { offset };
        let limit = write_limit(self.filesystem.geometry(), initial.flags);
        let remaining = source.len() - source.copied();
        let mut accepted = 0usize;
        let mut input = [0u8; 512];
        while accepted < remaining {
            let position = match start.checked_add(accepted as u64) {
                Some(position) if position < limit => position,
                _ => return write_result(start, accepted, FsError::FileTooLarge),
            };
            let size = self.filesystem.geometry().block_size;
            let count = (remaining - accepted)
                .min(input.len())
                .min(size - (position % size as u64) as usize)
                .min((limit - position) as usize);
            let pulled = match source.pull(&mut input[..count]) {
                Ok(0) => break,
                Ok(count) => count,
                Err(error) => return write_result(start, accepted, error),
            };
            let old = *state.metadata.lock();
            if old.size < position {
                if let Err(error) = self.zero_eof_tail(&old, position) {
                    return write_result(start, accepted, error);
                }
            }
            let (record, record_offset) = match self.filesystem.inode_record(old.number) {
                Ok(record) => record,
                Err(error) => return write_result(start, accepted, error),
            };
            let prepared = {
                let root = state.root.write();
                let current = record.with_read(|bytes| {
                    crate::fs::ext4::codec::decode_inode(
                        self.filesystem.geometry(),
                        old.number,
                        &bytes
                            [record_offset..record_offset + self.filesystem.geometry().inode_size],
                    )
                });
                let valid = match current {
                    Ok(Ok(current))
                        if InodeMetadata::from_data(&current) == old && current.block == *root =>
                    {
                        Ok(())
                    }
                    Ok(Ok(_)) => Err(FsError::Corrupt),
                    Ok(Err(error)) => Err(error),
                    Err(error) => Err(error.into()),
                };
                if let Err(error) = valid {
                    return write_result(start, accepted, error);
                }
                self.filesystem
                    .prepare_write(&old, &root, position / size as u64)
            };
            let prepared = match prepared {
                Ok(prepared) => prepared,
                Err(error) => return write_result(start, accepted, error),
            };
            let next = match write_metadata(
                self.filesystem.geometry(),
                &old,
                position,
                pulled,
                prepared.added_blocks,
                prepared.removed_blocks,
                time::now().as_nanos(),
            ) {
                Ok(next) => next,
                Err(error) => {
                    prepared.cancel(&self.filesystem);
                    return write_result(start, accepted, error);
                }
            };
            // 尚未发表的映射保持旧根；filefault 可取得读侧完成目的页初始化。
            let folio = match self.read_page(position / PAGE_SIZE as u64) {
                Ok(folio) => folio,
                Err(error) => {
                    prepared.cancel(&self.filesystem);
                    return write_result(start, accepted, error);
                }
            };
            let mut root = state.root.write();
            let mut content = folio.lock_content();
            let block_offset = ((position / size as u64 * size as u64) % PAGE_SIZE as u64) as usize;
            if prepared.zero {
                let zeros = [0u8; 512];
                for part in (block_offset..block_offset + size).step_by(zeros.len()) {
                    content
                        .copy_from(part, &zeros)
                        .expect("prepared writable file block");
                }
            }
            content
                .copy_from((position % PAGE_SIZE as u64) as usize, &input[..pulled])
                .expect("prepared writable file bytes");
            content
                .mark_dirty(block_offset, size)
                .expect("prepared file block dirty range");
            let next_root = prepared.root;
            let retired = prepared.publish(&self.filesystem, old.checksum_seed);
            record
                .update(claim, |bytes| {
                    crate::fs::ext4::codec::patch_inode_mapping(
                        self.filesystem.geometry(),
                        &mut bytes
                            [record_offset..record_offset + self.filesystem.geometry().inode_size],
                        &next,
                        &next_root,
                    )
                })
                .expect("prepared writable inode record");
            *root = next_root;
            *state.metadata.lock() = next;
            drop(content);
            for block in retired {
                // SAFETY: root 写侧已排空旧块树读者，发表后的根和父节点均已去掉旧节点；
                // 写侧仍阻止新读者，释放窗口保原页，forget 等真实元数据 IO/WB 收权后失效，
                // 此后旧窗口不再被映射查询使用，位图最后才允许物理块复用。
                unsafe {
                    self.filesystem.release_detached_block(block);
                }
            }
            drop(root);
            accepted += pulled;
        }
        Ok((start, accepted))
    }
    /// 内核按位置读取与用户接收端使用同一页交付循环。
    pub fn read_at(&self, offset: u64, dst: &mut [u8]) -> Result<usize, FsError> {
        self.read_into(offset, &mut ReadBuffer::kernel(dst))
    }

    /// 读取普通文件有效字节，页或接收端失败均优先返回本次已交付前缀。
    pub(crate) fn read_into(
        &self,
        offset: u64,
        target: &mut ReadBuffer<'_>,
    ) -> Result<usize, FsError> {
        let initial = target.copied();
        let remaining = target.len() - initial;
        if remaining == 0 {
            return Ok(0);
        }
        let data = self.metadata_snapshot()?;
        if data.kind == InodeKind::Directory {
            return Err(FsError::IsDirectory);
        }
        if data.kind != InodeKind::Regular {
            return Err(FsError::InvalidOperation);
        }
        if data.size <= offset {
            return Ok(0);
        }
        let len = (data.size - offset).min(remaining as u64) as usize;
        let mut copied = 0;
        let mut staging = [0u8; 512];
        while copied < len {
            // 已交付量不超过 size-offset，位置和页内有效范围不会溢出。
            let position = offset + copied as u64;
            let index = position / PAGE_SIZE as u64;
            let in_page = (position % PAGE_SIZE as u64) as usize;
            let page_end = copied + (len - copied).min(PAGE_SIZE - in_page);
            let result = self.read_page(index).and_then(|folio| {
                while copied < page_end {
                    let count = (page_end - copied).min(staging.len());
                    let source = ((offset + copied as u64) % PAGE_SIZE as u64) as usize;
                    folio.copy_to(source, &mut staging[..count])?;
                    // 原页内容访问已经结束，目标 fault 可再次取得同一 inode/page。
                    target.push(&staging[..count])?;
                    copied = target.copied() - initial;
                }
                Ok::<_, FsError>(())
            });
            copied = target.copied() - initial;
            if let Err(error) = result {
                return if copied == 0 { Err(error) } else { Ok(copied) };
            }
        }
        Ok(copied)
    }

    /// 普通任务取得正式元数据后等待同一 mapping 的页；映射调用者另行检查 EOF 和私有尾页。
    pub(crate) fn file_page(&self, index: u64) -> Result<FolioRef, FsError> {
        let data = self.metadata_snapshot()?;
        if data.kind != InodeKind::Regular {
            return Err(FsError::InvalidOperation);
        }
        self.read_page(index)
    }

    /// 稀疏增长先清旧 EOF 所在已映射块的尾部，避免新 size 暴露原盘尾字节。
    /// 此时尚未持目的 folio 或块根写侧，filefault 可以沿正常读侧完成旧页。
    fn zero_eof_tail(&self, old: &InodeMetadata, position: u64) -> Result<(), FsError> {
        let size = self.filesystem.geometry().block_size as u64;
        if old.size % size == 0 || old.size / size == position / size {
            return Ok(());
        }
        if !matches!(
            self.map_block(old.size / size)?,
            BlockMapping::Mapped { .. }
        ) {
            return Ok(());
        }
        let folio = self.read_page(old.size / PAGE_SIZE as u64)?;
        let mut content = folio.lock_content();
        let start = (old.size % PAGE_SIZE as u64) as usize;
        let block_start = start / size as usize * size as usize;
        let end = block_start + size as usize;
        let zeros = [0u8; 512];
        for offset in (start..end).step_by(zeros.len()) {
            content.copy_from(offset, &zeros[..(end - offset).min(zeros.len())])?;
        }
        content.mark_dirty(block_start, size as usize)?;
        Ok(())
    }

    /// 通过共同 mapping 协调同页唯一填充；不同页没有 inode 级读入锁。
    fn read_page(&self, index: u64) -> Result<FolioRef, FsError> {
        let (folio, read) = self.mapping.get_or_prepare(index)?;
        if let Some(read) = read {
            self.with_map_read(|metadata, root| {
                self.filesystem.fill_page(metadata, root, index, read)
            })?;
        }
        folio.wait_ready()?;
        Ok(folio.with_file_owner(self.clone()))
    }
}

impl Deref for InodeRef {
    type Target = Inode;

    /// 只借用原固定对象；没有 DerefMut 或公开 Arc 提供移动固定等待源的入口。
    fn deref(&self) -> &Inode {
        &self.0
    }
}

impl Inode {
    /// 返回该对象的盘上 inode 号；跨卷身份还须包括 filesystem 实例。
    pub fn number(&self) -> u64 {
        self.number
    }

    /// 借用强持的实际后端实例，不建立新的文件系统身份层。
    pub fn filesystem(&self) -> &Arc<Ext4Volume> {
        &self.filesystem
    }

    /// 等待唯一加载结果；属性与块根的访问必须继续取得各自规定的锁。
    fn wait_loaded(&self) -> Result<&InodeState, FsError> {
        sched::assert_switchable();
        let inode = self.pinned().get_ref();
        loop {
            if let Some(result) = inode.data.get() {
                return result.as_ref().map_err(|error| *error);
            }
            let waiter = Waiter::new();
            let mut node = pin!(WaitNode::new(waiter.waker()));
            // SAFETY: 私有 Arc 只给共享借用，inode 和内嵌等待源固定且由调用者引用保活；
            // pin! 固定原栈节点，registration 不被遗忘，退出先撤链再释放节点和 waiter。
            let _registration = unsafe { inode.waiters.register(node.as_mut()) };
            if let Some(result) = inode.data.get() {
                return result.as_ref().map_err(|error| *error);
            }
            waiter.park();
        }
    }

    /// token 只能在加载成功后构造，析构路径不再等待加载或创建新的责任。
    fn loaded_state(&self) -> &InodeState {
        self.data
            .get()
            .expect("access token precedes inode load")
            .as_ref()
            .expect("access token retains a failed inode")
    }

    /// 短属性锁内复制当前运行值，不将 Spin guard 带入 IO、等待或用户复制。
    pub fn metadata_snapshot(&self) -> Result<InodeMetadata, FsError> {
        Ok(*self.wait_loaded()?.metadata.lock())
    }

    /// 睡眠 data-sem 读侧保持块映射有效；属性快照的短锁在调用闭包前已经释放。
    pub fn with_map_read<T>(
        &self,
        consume: impl FnOnce(&InodeMetadata, &[u8; 60]) -> Result<T, FsError>,
    ) -> Result<T, FsError> {
        let state = self.wait_loaded()?;
        let root = state.root.read();
        let metadata = *state.metadata.lock();
        consume(&metadata, &root)
    }

    /// 在块映射读保护下执行真实后端查询，不导出永久块根借用。
    pub fn map_block(&self, logical: u64) -> Result<BlockMapping, FsError> {
        self.with_map_read(|metadata, root| self.filesystem.map_block(metadata, root, logical))
    }

    /// inode 写序列化与 data-sem 写侧保护受控属性修改，并字段级更新共同盘面记录。
    /// 此入口不分配块、不截断文件；大小和块布局变更须由对应内容协议实现。
    pub fn update_metadata<T>(
        &self,
        claim: &FsClaim,
        update: impl FnOnce(&mut InodeMetadata) -> T,
    ) -> Result<T, FsError> {
        if claim.mode() != AccessMode::ReadWrite || self.filesystem.mode() != AccessMode::ReadWrite
        {
            return Err(FsError::ReadOnly);
        }
        if !core::ptr::eq(
            claim.disk().as_ref().get_ref(),
            self.filesystem.disk().as_ref().get_ref(),
        ) {
            return Err(FsError::InvalidOperation);
        }
        let state = self.wait_loaded()?;
        let _write = state.write.lock();
        let _root = state.root.write();
        let old = *state.metadata.lock();
        let mut next = old;
        let result = update(&mut next);
        self.filesystem.update_inode_metadata(claim, &old, &next)?;
        *state.metadata.lock() = next;
        Ok(result)
    }

    /// 内核加载或范围扫描借用唯一页索引，须保对应 InodeRef 到裸页使用与 IO 全尾结束。
    /// 普通文件页消费使用 InodeRef 的出口，交给外部的页引用不能缺少原 inode anchor。
    pub(crate) fn mapping(&self) -> &FileMapping {
        &self.mapping
    }

    /// 原始单组件名字经真实目录后端查找，再在同缓存域获取硬链接的唯一对象。
    pub fn lookup(&self, cache: &InodeCache, name: &[u8]) -> Result<InodeRef, FsError> {
        let number = self
            .with_map_read(|metadata, root| self.filesystem.lookup(metadata, root, name))?
            .ok_or(FsError::NotFound)?;
        cache.iget(&self.filesystem, number)
    }

    /// 沿盘上字节位置枚举下一个有效目录项，不构造整目录副本。
    pub fn dir_next(&self, offset: u64) -> Result<Option<DirectoryEntry>, FsError> {
        self.with_map_read(|metadata, root| self.filesystem.dir_next(metadata, root, offset))
    }

    /// 由后端按真实 inode 存储规则返回符号链接的原始字节。
    pub fn read_link(&self) -> Result<Vec<u8>, FsError> {
        self.with_map_read(|metadata, root| self.filesystem.read_link(metadata, root))
    }

    /// 私有拥有类型禁止移动内嵌等待源，借用期间最后一个 Arc 不能析构。
    fn pinned(&self) -> Pin<&Self> {
        // SAFETY: Inode 只能由私有构造放入 Arc 后发表，InodeRef 不暴露 Arc 或可变借用；
        // 本模块从不 unwrap、移出或替换对象，Arc 的原分配保持到本共享借用终止之后。
        unsafe { Pin::new_unchecked(self) }
    }
}

impl LoadGuard<'_> {
    /// I/O 已结束后只将结果移入 once，失败解除原占位，再锁外唤醒全部等待者。
    fn complete(&mut self, result: Result<InodeData, FsError>) {
        self.publish(result);
        self.active = false;
    }

    /// 复用首次结果，展开清理不能覆盖已经发布的真实加载原因。
    fn publish(&self, result: Result<InodeData, FsError>) {
        let result = self.inode.data.call_once(|| result.map(InodeState::new));
        if result.is_err() {
            self.cache.remove_failed(self.inode);
        }
        self.inode.waiters.wake_all();
    }
}

impl Drop for LoadGuard<'_> {
    /// 尚未完成的唯一加载者退出时发布取消，等待者仍持原对象直到消费错误。
    fn drop(&mut self) {
        if self.active {
            self.publish(Err(FsError::LoadAborted));
        }
    }
}

/// 索引锁内只增加现有对象引用，卷身份使用实际 Arc 实例地址。
fn find(entries: &[InodeRef], filesystem: &Arc<Ext4Volume>, number: u64) -> Option<InodeRef> {
    entries
        .iter()
        .find(|inode| inode.number == number && Arc::ptr_eq(&inode.filesystem, filesystem))
        .cloned()
}

/// 正前缀已经被文件接受后保留它；零前缀保留具体的首次失败原因。
fn write_result(start: u64, accepted: usize, error: FsError) -> Result<(u64, usize), FsError> {
    if accepted == 0 {
        Err(error)
    } else {
        Ok((start, accepted))
    }
}

/// 有限最大文件位置由实际布局决定，传统树容量与 extent 的 32 位逻辑块号分别计算。
fn write_limit(g: &crate::fs::ext4::Geometry, flags: u32) -> u64 {
    let blocks = if flags & crate::fs::ext4::codec::EXTENTS != 0 {
        1u64 << 32
    } else {
        let n = (g.block_size / 4) as u64;
        12 + n + n * n + n * n * n
    };
    (blocks * g.block_size as u64).min(i64::MAX as u64)
}

/// 根据真实接受范围、块净增量与当前时间计算下一属性，未发表失败不修改原状态。
fn write_metadata(
    g: &crate::fs::ext4::Geometry,
    old: &InodeMetadata,
    position: u64,
    count: usize,
    added: u64,
    removed: u64,
    now: u64,
) -> Result<InodeMetadata, FsError> {
    let sectors = (g.block_size / 512) as u64;
    let blocks = old
        .blocks
        .checked_add(added * sectors)
        .ok_or(FsError::FileTooLarge)?;
    let blocks = blocks
        .checked_sub(removed * sectors)
        .ok_or(FsError::Corrupt)?;
    let (_, huge) = codec::blocks_encoding(g, blocks)?;
    let (mtime, ctime) = codec::prepare_inode_write_times(now, old.extra_isize)?;
    let mut next = *old;
    next.size = old.size.max(
        position
            .checked_add(count as u64)
            .ok_or(FsError::FileTooLarge)?,
    );
    next.blocks = blocks;
    next.flags = (old.flags & !0x40000) | if huge { 0x40000 } else { 0 };
    next.mtime = mtime;
    next.ctime = ctime;
    Ok(next)
}
