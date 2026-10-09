//! 原生 VFS namespace 的 HTree、目录加载与文件页竞争消费者。

use alloc::sync::Arc;
use alloc::vec::Vec;
use virtio_drivers::BufferDirection;

use crate::block::{
    AccessMode, Bio, BioAllocator, BlkOperation, BlockError, DiskRef, SECTOR_BYTES,
};
use crate::cache::ktest::HeapPressure;
use crate::cache::{BufferRef, MetadataCache, MetadataLoadHook};
use crate::fs::ext4::{codec, BlockMapping, DirectoryEntry, Ext4Volume, InodeKind};
use crate::fs::ktest::user::write::{
    allocation_counts, block_allocation_bit, inode_allocation_bit, WriteOwner,
};
use crate::fs::ktest::*;
use crate::fs::writeback::sync_metadata;
use crate::fs::{
    open_path, DentryRef, DescriptorFlags, FileLike, FileRef, FileTable, FsContext, FsError,
    InodeCache, InodeMetadata, InodeRef, OpenOptions, Path, ResolveMode, SeekFrom, Vfs,
};
use crate::mm::dma::MappedPages;

const INDEX: u32 = 0x1000;
const LONG_NAME_PREFIX: &[u8] = b"w27-\xff-";
const PROMOTION_PREFIX_SIGNED: &[u8] = b"signed-";
const PROMOTION_PREFIX_UNSIGNED: &[u8] = b"unsigned-";
const HASH_CANDIDATE_LIMIT: u32 = 1_048_576;
const DIRECTORY_WRITE_BATCH: usize = 8;

#[derive(Clone, Copy)]
struct DxNode {
    root_position: usize,
    logical: u64,
    count: usize,
    limit: usize,
}

#[derive(Clone)]
struct HTreeShape {
    metadata: InodeMetadata,
    map_root: [u8; 60],
    root: [u8; mm::PAGE_SIZE],
    depth: u8,
    root_count: usize,
    root_limit: usize,
    leaves: usize,
    nodes: Vec<DxNode>,
}

#[derive(Clone, Copy)]
struct RawName {
    nonce: u32,
    bytes: [u8; 255],
}

struct LegacyDirectory {
    pathname: &'static [u8],
    path: Path,
    inode: InodeRef,
    names: Vec<Vec<u8>>,
    collision: [&'static [u8]; 2],
}

struct NamespaceView {
    vfs: Arc<Vfs>,
    context: FsContext,
    root: Path,
    source: Path,
    volume: Arc<Ext4Volume>,
}

struct SymlinkLoadState {
    pressure: Option<HeapPressure>,
    captured: bool,
    failed: Option<BufferRef>,
    prefix: Option<BufferRef>,
    neighbor: Option<BufferRef>,
    neighbor_bytes: [u8; mm::PAGE_SIZE],
    evidence: Option<(bool, bool)>,
    preparation_error: Option<BlockError>,
}

/// 在目标槽已发表、范围读尚未认领时保留准确视图；邻窗口读完后才耗尽原页和堆。
fn observe_symlink_oom(
    cache: &Arc<MetadataCache>,
    disk: &DiskRef,
    state: &Arc<SpinLock<SymlinkLoadState>>,
    block: u64,
    block_size: usize,
    failed_block: u64,
    prefix_block: Option<u64>,
) -> Result<(), BlockError> {
    if block != failed_block {
        return Ok(());
    }
    let mut pressure = {
        let mut state = state.lock();
        if state.captured {
            return Ok(());
        }
        state.captured = true;
        state.pressure.take().expect("symlink load pressure absent")
    };
    let failed = cache
        .cached_buffer(block, block_size)?
        .ok_or(BlockError::NotReady)?;
    let prefix = prefix_block
        .map(|block| {
            cache
                .cached_buffer(block, block_size)?
                .ok_or(BlockError::NotReady)
        })
        .transpose()?;
    if let Some(prefix) = &prefix {
        prefix.with_read(|_| ())?;
    }
    let windows = mm::PAGE_SIZE / block_size;
    let page = block / windows as u64 * windows as u64;
    let neighbor_block = (page..page + windows as u64)
        .find(|&candidate| candidate != block && Some(candidate) != prefix_block);
    let neighbor = neighbor_block
        .map(|block| cache.get(block, block_size, None))
        .transpose()?;
    if let Some(neighbor) = &neighbor {
        neighbor.with_read(|bytes| {
            state.lock().neighbor_bytes[..block_size].copy_from_slice(bytes);
        })?;
    }
    disk.test_flush_completions();
    let evidence = pressure.exhaust();
    let mut state = state.lock();
    state.failed = Some(failed);
    state.prefix = prefix;
    state.neighbor = neighbor;
    state.evidence = Some(evidence);
    state.pressure = Some(pressure);
    Ok(())
}
/// 沿 E 提供的原 RW UserOwner/PathOwner 取得本例实际 Vfs、FsContext 与 Volume。
fn prepare_namespace_view(
    context: &TestContext<'_>,
    fixture: &mut WriteOwner,
) -> Result<NamespaceView, TestError> {
    fixture.prepare_namespace(context)?;
    let vfs = fixture.user.path.vfs().clone();
    let fs_context = fixture.user.path.context().clone();
    let root = vfs.root();
    let source = checked(
        context,
        vfs.resolve(&fs_context, b"/pattern", ResolveMode::Follow),
    )?;
    Ok(NamespaceView {
        vfs,
        context: fs_context,
        root,
        source,
        volume: fixture.volume(),
    })
}

/// 在原 DentryCache 的 InLookup 停点只阻塞指定原始组件。
fn install_lookup_gate(
    context: &TestContext<'_>,
    vfs: &Vfs,
    observer: Arc<Observation>,
    name: &'static [u8],
) -> TestResult {
    let hook: Arc<dyn Fn(&DentryRef) + Send + Sync> = Arc::try_new(move |dentry: &DentryRef| {
        if dentry.name().as_slice() == name && observer.hold_loader.load(Ordering::Acquire) {
            observer.loader_entered.fetch_add(1, Ordering::AcqRel);
            observer.gate.wait();
        }
    })
    .map_err(|_| TestError("namespace lookup observer OOM"))?;
    vfs.dcache().ktest_set_lookup_hook(Some(hook));
    Ok(())
}

/// 在原 root hash 区间内取得一个固定原始名字，后续目录修改仍走真实 HTree 路由。
fn name_for_root_range(
    volume: &Ext4Volume,
    shape: &HTreeShape,
    root_position: usize,
    next_nonce: &mut u32,
) -> Result<RawName, FsError> {
    for _ in 0..HASH_CANDIDATE_LIMIT {
        let nonce = *next_nonce;
        *next_nonce = next_nonce.checked_add(1).ok_or(FsError::NoSpace)?;
        let bytes = raw_long_name(LONG_NAME_PREFIX, nonce);
        let (major, _) = codec::directory_hash(volume.geometry(), shape.root[28], &bytes)?;
        if dx_select(&shape.root, 32, shape.root_count, major)? == root_position {
            return Ok(RawName { nonce, bytes });
        }
    }
    Err(FsError::NoSpace)
}

/// 在同一叶的有效 hash 区间连续取名；树分裂后重新选择最宽区间，不依赖输入装填率。
fn name_for_split_leaf(
    volume: &Ext4Volume,
    shape: &HTreeShape,
    node: DxNode,
    next_nonce: &mut u32,
) -> Result<RawName, FsError> {
    let node_bytes = directory_block(volume, &shape.metadata, &shape.map_root, node.logical)?;
    let mut selected = 0;
    let mut widest = 0;
    for position in 0..node.count {
        let lower = if position == 0 {
            if node.root_position == 0 {
                0
            } else {
                u64::from(codec::le32(&shape.root, 32 + node.root_position * 8)? & !1)
            }
        } else {
            u64::from(codec::le32(&node_bytes, 8 + position * 8)? & !1)
        };
        let upper = if position + 1 < node.count {
            u64::from(codec::le32(&node_bytes, 8 + (position + 1) * 8)? & !1)
        } else if node.root_position + 1 < shape.root_count {
            u64::from(codec::le32(&shape.root, 32 + (node.root_position + 1) * 8)? & !1)
        } else {
            1u64 << 32
        };
        let width = upper.checked_sub(lower).ok_or(FsError::Corrupt)?;
        if widest < width {
            selected = position;
            widest = width;
        }
    }
    for _ in 0..HASH_CANDIDATE_LIMIT {
        let nonce = *next_nonce;
        *next_nonce = next_nonce.checked_add(1).ok_or(FsError::NoSpace)?;
        let bytes = raw_long_name(LONG_NAME_PREFIX, nonce);
        let (major, _) = codec::directory_hash(volume.geometry(), shape.root[28], &bytes)?;
        if dx_select(&shape.root, 32, shape.root_count, major)? == node.root_position
            && dx_select(&node_bytes, 8, node.count, major)? == selected
        {
            return Ok(RawName { nonce, bytes });
        }
    }
    Err(FsError::NoSpace)
}

/// 从真实 OFD readdir 找到所有新名字；短 callback 拒绝的末项必须按原位置重交付。
fn verify_indexed_new_names(
    context: &mut TestContext<'_>,
    path: &Path,
    expected: &[RawName],
    inode_number: u64,
) -> TestResult {
    if expected.is_empty() {
        return Err(TestError("indexed directory name set is empty"));
    }
    let file = checked(context, open_path(path.clone()))?;
    let mut seen = Vec::new();
    seen.try_reserve_exact(expected.len())
        .map_err(|_| TestError("indexed name bitmap OOM"))?;
    seen.resize(expected.len(), false);
    let mut found = 0usize;
    let mut committed_cookie = checked(context, file.seek(SeekFrom::Current(0)))?;
    let mut declined_cookie = committed_cookie;
    let mut declined_number = 0u64;
    let mut declined_name = [0u8; 255];
    let mut declined_len = 0usize;
    file.for_each_dirent(&mut |entry| {
        if entry.name.starts_with(LONG_NAME_PREFIX) {
            let nonce = raw_name_nonce(&entry.name).ok_or(FsError::Corrupt)?;
            let Some(index) = expected.iter().position(|name| name.nonce == nonce) else {
                return Err(FsError::Corrupt);
            };
            if seen[index]
                || expected[index].bytes.as_slice() != entry.name.as_slice()
                || entry.number != inode_number
                || entry.kind != Some(InodeKind::Regular)
            {
                return Err(FsError::Corrupt);
            }
            seen[index] = true;
            found += 1;
            if found == expected.len() {
                if 255 < entry.name.len() {
                    return Err(FsError::Corrupt);
                }
                declined_cookie = entry.next_cookie;
                declined_number = entry.number;
                declined_len = entry.name.len();
                declined_name[..declined_len].copy_from_slice(&entry.name);
                return Ok(false);
            }
        }
        committed_cookie = entry.next_cookie;
        Ok(true)
    })
    .map_err(|error| {
        context.diagnostic(format_args!("W27 indexed readdir error={:?}", error));
        TestError("indexed directory readdir failed")
    })?;
    context.expect_eq(found, expected.len());
    context.expect_eq(seen.iter().all(|present| *present), true);
    context.expect_eq(
        checked(context, file.seek(SeekFrom::Current(0)))?,
        committed_cookie,
    );
    let declined = checked(context, file.readdir())?
        .ok_or(TestError("indexed short delivery lost its final entry"))?;
    context.expect_eq(declined.next_cookie, declined_cookie);
    context.expect_eq(declined.number, declined_number);
    context.expect_eq(declined.name.as_slice(), &declined_name[..declined_len]);
    Ok(())
}

/// 解码本测试固定十六进制 nonce，不把 raw 名称转成 UTF-8。
fn raw_name_nonce(name: &[u8]) -> Option<u32> {
    if name.len() != 255 || !name.starts_with(LONG_NAME_PREFIX) {
        return None;
    }
    let digits = name.get(LONG_NAME_PREFIX.len()..LONG_NAME_PREFIX.len() + 8)?;
    let mut value = 0u32;
    for byte in digits {
        let nibble = match *byte {
            b'0'..=b'9' => *byte - b'0',
            b'a'..=b'f' => *byte - b'a' + 10,
            _ => return None,
        };
        value = value.checked_mul(16)?.checked_add(u32::from(nibble))?;
    }
    Some(value)
}

/// 父入口只登记本模块的原生 namespace suite，不增中央测试分派。
pub(super) fn anchor() -> &'static ::ktest::SuiteSlot {
    native::__ktest_anchor()
}

/// 原 RootOwner 已持有本例唯一 Disk 与完成域；Superblock 默认版本在首次 FS open 前改为有效 legacy unsigned。
fn select_unsigned_legacy_default(context: &mut TestContext<'_>, disk: &DiskRef) -> TestResult {
    let limits = disk.limits();
    let logical = limits.logical_block_size as usize;
    if !matches!(logical, 512 | 1024 | 2048 | 4096) {
        return Err(TestError(
            "legacy hash preparation has unsupported Disk geometry",
        ));
    }
    let start = 1024 / logical * logical;
    let end = 2048_usize.div_ceil(logical) * logical;
    let length = end - start;
    let capacity = limits
        .capacity_sectors
        .checked_mul(SECTOR_BYTES)
        .ok_or(TestError("legacy hash preparation Disk capacity overflow"))?;
    let mut bytes = [0u8; mm::PAGE_SIZE];
    checked(
        context,
        disk.read_exact_at(start as u64, &mut bytes[..length])
            .map_err(FsError::from),
    )?;
    let super_offset = 1024 - start;
    let superblock = &mut bytes[super_offset..super_offset + 1024];
    let original = checked(context, codec::decode_super(superblock, capacity))?;
    superblock[252] = 3;
    if original.features_ro_compat & codec::METADATA_CSUM != 0 {
        let checksum = codec::crc32c(!0, &superblock[..1020]);
        superblock[1020..1024].copy_from_slice(&checksum.to_le_bytes());
    }
    let updated = checked(context, codec::decode_super(superblock, capacity))?;
    context.expect_eq(updated.default_hash_version, 3);
    context.expect_eq(updated.hash_unsigned, original.hash_unsigned);
    context.expect_eq(updated.hash_seed, original.hash_seed);
    context.expect_eq(updated.uuid, original.uuid);

    let pages = mm::alloc_pages(0, true).ok_or(TestError("legacy hash write page OOM"))?;
    let mut mapped = MappedPages::new(pages, 0, length, BufferDirection::DriverToDevice)
        .map_err(|_| TestError("legacy hash write mapping failed"))?;
    for (target, source) in mapped.bytes_mut().iter_mut().zip(&bytes[..length]) {
        target.write(*source);
    }
    let mut memory = Vec::new_in(BioAllocator::Heap);
    memory
        .try_reserve_exact(1)
        .map_err(|_| TestError("legacy hash Bio slot OOM"))?;
    memory.push(mapped);
    // SAFETY: bytes_mut 已初始化整个 Write 范围且可变借用已结束；
    // Bio 接管唯一映射拥有，原 Disk 保活至 wait 确认设备与完成 Work 全尾结束。
    // take_memory 收回原拥有之前没有 CPU 内容别名。
    let bio = unsafe {
        Bio::try_new(
            disk.as_ref().get_ref(),
            BlkOperation::Write,
            start as u64 / SECTOR_BYTES,
            memory,
            0,
            None,
        )
    }
    .map_err(|error| {
        context.diagnostic(format_args!(
            "legacy-hash-super-write prepare-error={:?}",
            error
        ));
        TestError("legacy hash superblock Bio preparation failed")
    })?;
    let completion = bio.completion();
    disk.submit_bio(bio);
    let result = completion.wait();
    context.expect_eq(result.completed, length);
    context.expect_eq(result.error, None);
    drop(
        completion
            .take_memory()
            .map_err(|_| TestError("legacy hash superblock memory reclaim failed"))?,
    );
    disk.test_flush_completions();
    context.expect_eq(disk.test_snapshot().inflight, 0);
    context.diagnostic(format_args!(
        "W27-superblock default-hash-version=3 unsigned-legacy-explicit=1 original-hash-flags-preserved=1 metadata-csum-recomputed={} raw-write-completion=1",
        original.features_ro_compat & codec::METADATA_CSUM != 0
    ));
    Ok(())
}

/// 以真实 inode 映射取得目录逻辑块；每个 BufferRef 借用结束后才读取下一块。
fn directory_block(
    volume: &Ext4Volume,
    metadata: &InodeMetadata,
    map_root: &[u8; 60],
    logical: u64,
) -> Result<[u8; mm::PAGE_SIZE], FsError> {
    let physical = match volume.map_block(metadata, map_root, logical, None)? {
        BlockMapping::Mapped { physical, .. } => physical,
        _ => return Err(FsError::Corrupt),
    };
    let buffer = volume.metadata_block(physical)?;
    let mut bytes = [0u8; mm::PAGE_SIZE];
    buffer
        .with_read(|data| bytes[..data.len()].copy_from_slice(data))
        .map_err(FsError::from)?;
    Ok(bytes)
}

/// 校验当前共享元数据页中的 HTree root、节点及可选全部叶 CRC。
fn htree_shape(
    volume: &Ext4Volume,
    inode: &InodeRef,
    verify_leaves: bool,
) -> Result<HTreeShape, FsError> {
    let (metadata, map_root) = inode.with_map_read(|metadata, root| Ok((*metadata, *root)))?;
    if metadata.kind != InodeKind::Directory || metadata.flags & INDEX == 0 {
        return Err(FsError::NotDirectory);
    }
    let geometry = volume.geometry();
    let root = directory_block(volume, &metadata, &map_root, 0)?;
    if codec::verify_directory(geometry, &metadata, 0, &root[..geometry.block_size], false)?
        != codec::DirectoryLayout::Root
    {
        return Err(FsError::Corrupt);
    }
    let depth = root[30];
    if depth > 1 {
        return Err(FsError::Unsupported);
    }
    let root_count = usize::from(codec::le16(&root, 34)?);
    let root_limit = usize::from(codec::le16(&root, 32)?);
    let mut nodes = Vec::new();
    nodes
        .try_reserve_exact(root_count)
        .map_err(|_| FsError::NoMemory)?;
    let mut leaves = 0;
    for root_position in 0..root_count {
        let root_entry = 32 + root_position * 8;
        let logical = u64::from(codec::le32(&root, root_entry + 4)? & 0x0fff_ffff);
        if depth == 0 {
            if verify_leaves {
                let leaf = directory_block(volume, &metadata, &map_root, logical)?;
                if !matches!(
                    codec::verify_directory(
                        geometry,
                        &metadata,
                        logical,
                        &leaf[..geometry.block_size],
                        false,
                    )?,
                    codec::DirectoryLayout::Leaf { .. }
                ) {
                    return Err(FsError::Corrupt);
                }
            }
            leaves += 1;
            continue;
        }
        let node_bytes = directory_block(volume, &metadata, &map_root, logical)?;
        if codec::verify_directory(
            geometry,
            &metadata,
            logical,
            &node_bytes[..geometry.block_size],
            true,
        )? != codec::DirectoryLayout::Index
        {
            return Err(FsError::Corrupt);
        }
        let count = usize::from(codec::le16(&node_bytes, 10)?);
        let limit = usize::from(codec::le16(&node_bytes, 8)?);
        nodes.push(DxNode {
            root_position,
            logical,
            count,
            limit,
        });
        leaves += count;
        if verify_leaves {
            for position in 0..count {
                let entry = 8 + position * 8;
                let leaf_logical = u64::from(codec::le32(&node_bytes, entry + 4)? & 0x0fff_ffff);
                let leaf = directory_block(volume, &metadata, &map_root, leaf_logical)?;
                if !matches!(
                    codec::verify_directory(
                        geometry,
                        &metadata,
                        leaf_logical,
                        &leaf[..geometry.block_size],
                        false,
                    )?,
                    codec::DirectoryLayout::Leaf { .. }
                ) {
                    return Err(FsError::Corrupt);
                }
            }
        }
    }
    Ok(HTreeShape {
        metadata,
        map_root,
        root,
        depth,
        root_count,
        root_limit,
        leaves,
        nodes,
    })
}

/// 按后端 dx 前驱选择规则返回 hash 所属的真实索引槽。
fn dx_select(data: &[u8], offset: usize, count: usize, hash: u32) -> Result<usize, FsError> {
    let mut low = 1usize;
    let mut high = count;
    while low < high {
        let middle = low + (high - low) / 2;
        if codec::le32(data, offset + middle * 8)? <= hash {
            low = middle + 1;
        } else {
            high = middle;
        }
    }
    Ok(low - 1)
}

/// 在名称预算内逐字节构造固定长度原始名称，是否含非 UTF-8 字节由前缀决定。
fn raw_long_name(prefix: &[u8], nonce: u32) -> [u8; 255] {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut name = [b'n'; 255];
    name[..prefix.len()].copy_from_slice(prefix);
    for index in 0..8 {
        let shift = (7 - index) * 4;
        name[prefix.len() + index] = HEX[((nonce >> shift) & 0xf) as usize];
    }
    name
}
/// 为已发表目录项准备独立字节拥有，不依赖 UTF-8 解释或意外分配。
fn copy_name(name: &[u8]) -> Result<Vec<u8>, TestError> {
    let mut copy = Vec::new();
    copy.try_reserve_exact(name.len())
        .map_err(|_| TestError("directory name copy OOM"))?;
    copy.extend_from_slice(name);
    Ok(copy)
}

/// 把已发表的有限名字落盘并等实际 metadata/FLUSH 完成，保持本例原分配预算。
fn sync_namespace_batch(
    context: &mut TestContext<'_>,
    disk: &DiskRef,
    source: &FileRef,
    directory: &FileRef,
) -> TestResult {
    checked(context, source.sync_range(0..u64::MAX, false))?;
    checked(context, directory.sync_range(0..u64::MAX, false))?;
    disk.test_flush_completions();
    Ok(())
}

/// 普通目录在首块满时转为 HTree，并用转换前打开且复制的 OFD 验证 cookie 续读。
fn promote_legacy_directory(
    context: &mut TestContext<'_>,
    fixture: &mut WriteOwner,
    view: &NamespaceView,
    source_file: &FileRef,
    pathname: &'static [u8],
    prefix: &[u8],
    hash_version: u8,
    collision: [&'static [u8]; 2],
) -> Result<LegacyDirectory, TestError> {
    checked(
        context,
        view.vfs.mkdir_from(
            &view.context,
            &view.root,
            pathname,
            0o777,
            (0, 0),
            view.context.umask(),
        ),
    )?;
    let directory_file = fixture.open(context, pathname, 0)?;
    let directory_table = checked(context, FileTable::try_new(2))?;
    let directory_fd = checked(context, directory_table.reserve())?
        .publish(directory_file.clone(), DescriptorFlags::default());
    let duplicate_fd = checked(context, directory_table.dup(directory_fd))?;
    let directory = directory_file.path().clone();
    let inode = checked(context, directory.inode())?.clone();
    context.expect_eq(
        checked(context, inode.metadata_snapshot())?.flags & INDEX,
        0,
    );
    let geometry = view.volume.geometry();
    let tail = if geometry.features_ro_compat & codec::METADATA_CSUM != 0 {
        12
    } else {
        0
    };
    let leaf_end = geometry
        .block_size
        .checked_sub(tail)
        .ok_or(TestError("promotion geometry has no leaf tail"))?;
    let long_record = (8 + 255 + 3) & !3;
    let long_count = leaf_end
        .checked_sub(24)
        .ok_or(TestError("promotion geometry cannot hold dot entries"))?
        / long_record;
    let leaf_free = leaf_end - long_count * long_record;
    let ordinary_free = leaf_free
        .checked_sub(24)
        .ok_or(TestError("promotion geometry cannot reserve dot records"))?;
    let trigger_record = (ordinary_free / 4 + 1) * 4;
    if leaf_free < trigger_record || trigger_record < 8 {
        return Err(TestError("promotion trigger does not fit the indexed leaf"));
    }
    let trigger_len = trigger_record - 8;
    if trigger_len < prefix.len() + 8 || 255 < trigger_len {
        return Err(TestError("promotion trigger component is invalid"));
    }
    let mut names = Vec::new();
    names
        .try_reserve_exact(long_count + 3)
        .map_err(|_| TestError("legacy directory name storage OOM"))?;
    let mut writes = 0usize;
    for nonce in 0..long_count {
        let name = raw_long_name(prefix, nonce as u32);
        checked(
            context,
            view.vfs
                .link_from(&view.source, &view.context, &directory, &name, (0, 0)),
        )?;
        names.push(copy_name(&name)?);
        writes += 1;
        if writes % DIRECTORY_WRITE_BATCH == 0 {
            sync_namespace_batch(context, view.volume.disk(), source_file, &directory_file)?;
        }
    }
    let trigger = raw_long_name(prefix, u32::MAX);
    checked(
        context,
        view.vfs.link_from(
            &view.source,
            &view.context,
            &directory,
            &trigger[..trigger_len],
            (0, 0),
        ),
    )?;
    names.push(copy_name(&trigger[..trigger_len])?);
    writes += 1;
    sync_namespace_batch(context, view.volume.disk(), source_file, &directory_file)?;
    context.expect_eq(
        checked(context, inode.metadata_snapshot())?.flags & INDEX,
        INDEX,
    );
    let first = checked(context, directory_table.readdir(directory_fd))?.ok_or(TestError(
        "old-open directory lost its first entry after promotion",
    ))?;
    context.expect_eq(first.name.as_slice(), b".".as_slice());
    let continued = checked(context, directory_table.readdir(duplicate_fd))?.ok_or(TestError(
        "duplicated old-open cursor stopped after promotion",
    ))?;
    context.expect_eq(continued.name.as_slice() != b".".as_slice(), true);
    let shape = checked(context, htree_shape(&view.volume, &inode, true))?;
    context.expect_eq(shape.root[28], 3);
    if hash_version != 3 {
        set_root_hash_version(context, &view.volume, &inode, hash_version, &names)?;
        let updated = checked(context, htree_shape(&view.volume, &inode, true))?;
        context.expect_eq(updated.root[28], hash_version);
    }
    let first_hash = checked(
        context,
        codec::directory_hash(geometry, hash_version, collision[0]),
    )?;
    let second_hash = checked(
        context,
        codec::directory_hash(geometry, hash_version, collision[1]),
    )?;
    context.expect_eq(first_hash, second_hash);
    context.expect_eq(first_hash.1, 0);
    for name in &names {
        let existing_hash = checked(context, codec::directory_hash(geometry, hash_version, name))?;
        if existing_hash == first_hash {
            return Err(TestError("fixed collision vector overlaps promotion names"));
        }
    }
    for name in collision {
        checked(
            context,
            view.vfs
                .link_from(&view.source, &view.context, &directory, name, (0, 0)),
        )?;
        names.push(copy_name(name)?);
        writes += 1;
        if writes % DIRECTORY_WRITE_BATCH == 0 {
            sync_namespace_batch(context, view.volume.disk(), source_file, &directory_file)?;
        }
    }
    context.expect_eq(
        checked(context, inode.metadata_snapshot())?.flags & INDEX,
        INDEX,
    );
    sync_namespace_batch(context, view.volume.disk(), source_file, &directory_file)?;
    let final_shape = checked(context, htree_shape(&view.volume, &inode, true))?;
    context.expect_eq(final_shape.root[28], hash_version);
    Ok(LegacyDirectory {
        pathname,
        path: directory,
        inode,
        names,
        collision,
    })
}

/// 碰撞名尚未插入且所有已有名字的 hash 保持相同时，更新真实 dx 根版本及 checksum。
fn set_root_hash_version(
    context: &TestContext<'_>,
    volume: &Ext4Volume,
    inode: &InodeRef,
    version: u8,
    names: &[Vec<u8>],
) -> TestResult {
    let (metadata, map_root) = checked(
        context,
        inode.with_map_read(|metadata, root| Ok((*metadata, *root))),
    )?;
    let geometry = volume.geometry();
    let mut bytes = checked(context, directory_block(volume, &metadata, &map_root, 0))?;
    if codec::verify_directory(geometry, &metadata, 0, &bytes[..geometry.block_size], false)
        .map_err(|error| {
            context.diagnostic(format_args!("W27 legacy root validation error={:?}", error));
            TestError("legacy directory root validation failed")
        })?
        != codec::DirectoryLayout::Root
    {
        return Err(TestError("legacy directory root layout changed"));
    }
    let old_version = bytes[28];
    for name in names {
        let old_hash = checked(context, codec::directory_hash(geometry, old_version, name))?;
        let new_hash = checked(context, codec::directory_hash(geometry, version, name))?;
        if old_hash != new_hash {
            return Err(TestError(
                "legacy hash version changes existing directory routing",
            ));
        }
    }
    bytes[28] = version;
    if geometry.features_ro_compat & codec::METADATA_CSUM != 0 {
        let count =
            usize::from(codec::le16(&bytes, 34).map_err(|_| TestError("dx count decode failed"))?);
        let limit =
            usize::from(codec::le16(&bytes, 32).map_err(|_| TestError("dx limit decode failed"))?);
        codec::update_dx_checksum(
            &metadata,
            &mut bytes[..geometry.block_size],
            32 + count * 8,
            32 + limit * 8,
        )
        .map_err(|_| TestError("dx root checksum preparation failed"))?;
    }
    if codec::verify_directory(geometry, &metadata, 0, &bytes[..geometry.block_size], false)
        .map_err(|_| TestError("dx root checksum verification failed"))?
        != codec::DirectoryLayout::Root
    {
        return Err(TestError("legacy hash version changed the root layout"));
    }
    let physical = match volume.map_block(&metadata, &map_root, 0, None) {
        Ok(BlockMapping::Mapped { physical, .. }) => physical,
        Ok(_) => return Err(TestError("legacy root block is not mapped")),
        Err(error) => {
            context.diagnostic(format_args!("W27 legacy root mapping error={:?}", error));
            return Err(TestError("legacy root mapping failed"));
        }
    };
    let buffer = checked(context, volume.metadata_block(physical))?;
    checked(
        context,
        buffer
            .update(volume.claim(), |target| {
                target.copy_from_slice(&bytes[..geometry.block_size])
            })
            .map_err(FsError::from),
    )?;
    Ok(())
}

/// 核对目录字节名、拒绝交付位置与 OFD cookie 推进，读完后保留末尾位置。
fn complete_directory_names(
    context: &mut TestContext<'_>,
    path: &Path,
    names: &[Vec<u8>],
    inode_number: u64,
) -> TestResult {
    let file = checked(context, open_path(path.clone()))?;
    let mut seen = Vec::new();
    seen.try_reserve_exact(names.len())
        .map_err(|_| TestError("directory name bitmap OOM"))?;
    seen.resize(names.len(), false);
    let mut dots = 0usize;
    let mut entries = 0usize;
    let mut accepted = 0usize;
    let mut accepted_cookie = checked(context, file.seek(SeekFrom::Current(0)))?;
    let mut declined_cookie = accepted_cookie;
    let mut declined_number = 0;
    let mut declined_name = [0u8; 255];
    let mut declined_len = 0usize;
    file.for_each_dirent(&mut |entry| {
        accepted += 1;
        if accepted <= 2 {
            observe_directory_name(
                entry,
                names,
                inode_number,
                &mut seen,
                &mut dots,
                &mut entries,
            )?;
            accepted_cookie = entry.next_cookie;
            return Ok(true);
        }
        if 255 < entry.name.len() {
            return Err(FsError::Corrupt);
        }
        declined_len = entry.name.len();
        declined_name[..declined_len].copy_from_slice(&entry.name);
        declined_number = entry.number;
        declined_cookie = entry.next_cookie;
        Ok(false)
    })
    .map_err(|error| {
        context.diagnostic(format_args!(
            "W27 short directory delivery error={:?}",
            error
        ));
        TestError("directory short delivery failed")
    })?;
    if accepted < 3 {
        return Err(TestError("legacy directory lacks short-delivery entries"));
    }
    context.expect_eq(
        checked(context, file.seek(SeekFrom::Current(0)))?,
        accepted_cookie,
    );
    let declined =
        checked(context, file.readdir())?.ok_or(TestError("short directory entry was lost"))?;
    context.expect_eq(declined.next_cookie, declined_cookie);
    context.expect_eq(declined.number, declined_number);
    context.expect_eq(declined.name.as_slice(), &declined_name[..declined_len]);
    observe_directory_name(
        &declined,
        names,
        inode_number,
        &mut seen,
        &mut dots,
        &mut entries,
    )
    .map_err(|_| TestError("short directory entry changed before retry"))?;
    file.for_each_dirent(&mut |entry| {
        observe_directory_name(
            entry,
            names,
            inode_number,
            &mut seen,
            &mut dots,
            &mut entries,
        )?;
        Ok(true)
    })
    .map_err(|error| {
        context.diagnostic(format_args!(
            "W27 full directory enumeration error={:?}",
            error
        ));
        TestError("directory enumeration failed")
    })?;
    context.expect_eq(dots, 2);
    context.expect_eq(entries, names.len() + 2);
    context.expect_eq(seen.iter().all(|present| *present), true);
    let mut after_eof = 0;
    file.for_each_dirent(&mut |_| {
        after_eof += 1;
        Ok(true)
    })
    .map_err(|_| TestError("directory EOF enumeration failed"))?;
    context.expect_eq(after_eof, 0);
    context.expect_eq(
        checked(context, file.seek(SeekFrom::Current(0)))?,
        checked(context, file.seek(SeekFrom::End(0)))?,
    );
    Ok(())
}

/// 只接受测试记录中的原始字节名并确认它们全部指向同一真实 hardlink inode。
fn observe_directory_name(
    entry: &DirectoryEntry,
    names: &[Vec<u8>],
    inode_number: u64,
    seen: &mut [bool],
    dots: &mut usize,
    entries: &mut usize,
) -> Result<(), FsError> {
    *entries += 1;
    if entry.name == b"." || entry.name == b".." {
        *dots += 1;
        return Ok(());
    }
    let Some(index) = names.iter().position(|name| name.as_slice() == entry.name) else {
        return Err(FsError::Corrupt);
    };
    if seen[index] || entry.number != inode_number || entry.kind != Some(InodeKind::Regular) {
        return Err(FsError::Corrupt);
    }
    seen[index] = true;
    Ok(())
}

/// 按目录 root 的实际版本计算碰撞，核验 dup、独立打开、显式 seek 和续读状态。
fn check_collision_cursors(
    context: &mut TestContext<'_>,
    view: &NamespaceView,
    legacy: &LegacyDirectory,
) -> TestResult {
    let source_inode = checked(context, view.source.inode())?;
    let shape = checked(context, htree_shape(&view.volume, &legacy.inode, true))?;
    let version = shape.root[28];
    let first_hash = checked(
        context,
        codec::directory_hash(view.volume.geometry(), version, legacy.collision[0]),
    )?;
    let second_hash = checked(
        context,
        codec::directory_hash(view.volume.geometry(), version, legacy.collision[1]),
    )?;
    context.expect_eq((first_hash.0, first_hash.1), (second_hash.0, second_hash.1));
    context.expect_eq(first_hash.1, 0);
    for name in legacy.collision {
        let path = checked(
            context,
            view.vfs.resolve_from(
                &view.context,
                &legacy.path,
                name,
                ResolveMode::Follow,
                Some((0, 0)),
            ),
        )?;
        context.expect_eq(checked(context, path.inode())?.ptr_eq(source_inode), true);
    }

    let table = checked(context, FileTable::try_new(4))?;
    let options = OpenOptions {
        directory: true,
        ..OpenOptions::default()
    };
    let fd = checked(
        context,
        table.open(&view.vfs, &view.context, legacy.pathname, options),
    )?;
    let independent = checked(
        context,
        table.open(&view.vfs, &view.context, legacy.pathname, options),
    )?;
    let duplicate = checked(context, table.dup(fd))?;
    let independent_dot = checked(context, table.readdir(independent))?
        .ok_or(TestError("independent indexed directory is empty"))?;
    context.expect_eq(independent_dot.name.as_slice(), b".".as_slice());
    let fd_dot = checked(context, table.readdir(fd))?
        .ok_or(TestError("original indexed directory is empty"))?;
    context.expect_eq(fd_dot.name.as_slice(), b".".as_slice());

    let mut first = None;
    while let Some(entry) = checked(context, table.readdir(fd))? {
        if legacy
            .collision
            .iter()
            .any(|name| *name == entry.name.as_slice())
        {
            first = Some(entry);
            break;
        }
    }
    let first = first.ok_or(TestError("real same-hash collision was not enumerated"))?;
    let first_name = first.name.clone();
    let first_cookie = first.next_cookie;
    let second = checked(context, table.readdir(duplicate))?
        .ok_or(TestError("dup lost the same-hash continuation"))?;
    context.expect_eq(
        second.name.as_slice() != first_name.as_slice()
            && legacy
                .collision
                .iter()
                .any(|name| *name == second.name.as_slice()),
        true,
    );
    checked(
        context,
        table.seek(independent, SeekFrom::Start(first_cookie)),
    )?;
    let reset = checked(context, table.readdir(independent))?
        .ok_or(TestError("explicit seek lost the first collision"))?;
    context.expect_eq(reset.name.as_slice(), first_name.as_slice());
    context.expect_eq(
        checked(context, table.seek(independent, SeekFrom::Current(0)))?,
        first_cookie,
    );
    let continued = checked(context, table.readdir(independent))?
        .ok_or(TestError("SEEK_CUR(0) discarded collision continuation"))?;
    context.expect_eq(continued.name.as_slice(), second.name.as_slice());
    for descriptor in [fd, independent, duplicate] {
        checked(context, table.close(descriptor))?;
    }
    Ok(())
}

/// 改写后端已连接 metadata 窗口前核主目录索引未清除INDEX或降低真实深度。
fn preserve_indexed_layout(
    context: &mut TestContext<'_>,
    volume: &Ext4Volume,
    inode: &InodeRef,
    before: &HTreeShape,
    after: &HTreeShape,
) -> TestResult {
    let metadata = checked(context, inode.metadata_snapshot())?;
    context.expect_eq(metadata.flags & INDEX, INDEX);
    context.expect_eq(before.depth <= after.depth, true);
    context.expect_eq(before.root_count < after.root_count, true);
    context.expect_eq(before.nodes.len() < after.nodes.len(), true);
    context.expect_eq(before.leaves < after.leaves, true);
    context.expect_eq(
        matches!(
            codec::verify_directory(
                volume.geometry(),
                &after.metadata,
                0,
                &after.root[..volume.geometry().block_size],
                false,
            ),
            Ok(codec::DirectoryLayout::Root)
        ),
        true,
    );
    Ok(())
}

/// 原 FS 回写与 Work 退役、旧 Weak 失效后同 Disk 建新 cache，读取仍须匹配本轮原内容。
fn cold_reopen_namespace(
    context: &mut TestContext<'_>,
    fixture: &mut WriteOwner,
    sample_path: &[u8],
    expected_inode: u64,
    expected_bytes: &[u8; 64],
) -> TestResult {
    let disk = fixture.user.path.owner.disk()?.clone();
    let old_volume = fixture
        .cold_volume
        .as_ref()
        .ok_or(TestError("namespace original Volume owner missing"))?
        .clone();
    for inode in &fixture.inodes {
        checked(context, crate::fs::writeback::sync_inode(inode))?;
    }
    checked(context, crate::fs::writeback::sync_metadata(&old_volume))?;
    disk.test_flush_completions();
    fixture.inodes.clear();
    fixture.user.path.release_view();

    let old_cache = fixture
        .user
        .path
        .owner
        .cache
        .take()
        .ok_or(TestError("namespace original InodeCache owner missing"))?;
    old_cache.ktest_set_load_hook(None);
    let weak_cache = Arc::downgrade(&old_cache);
    let weak_volume = Arc::downgrade(&old_volume);
    let weak_metadata = Arc::downgrade(old_volume.metadata_cache());
    assert!(fixture.user.path.owner.cache_watch.is_none());
    fixture.user.path.owner.cache_watch = Some(old_cache.watch());
    drop(old_cache);
    checked(
        context,
        fixture
            .user
            .path
            .owner
            .cache_watch
            .as_ref()
            .expect("retired namespace cache has no watch")
            .wait_retired(),
    )?;
    drop(fixture.user.path.owner.cache_watch.take());
    checked(context, crate::fs::writeback::sync_metadata(&old_volume))?;
    disk.test_flush_completions();
    context.expect_eq(disk.test_snapshot().inflight, 0);
    drop(fixture.cold_volume.take());
    drop(old_volume);
    context.expect_eq(weak_cache.upgrade().is_none(), true);
    context.expect_eq(weak_volume.upgrade().is_none(), true);
    context.expect_eq(weak_metadata.upgrade().is_none(), true);

    let volume = checked(context, Ext4Volume::open(disk, AccessMode::ReadWrite, 64))?;
    let cache = Arc::try_new(checked(context, InodeCache::try_new(64))?)
        .map_err(|_| TestError("cold namespace cache owner OOM"))?;
    fixture.user.path.owner.cache = Some(cache.clone());
    fixture.cold_volume = Some(volume.clone());
    let vfs = Arc::try_new(checked(
        context,
        Vfs::try_new(volume.clone(), cache, 128, 4, AccessMode::ReadWrite),
    )?)
    .map_err(|_| TestError("cold namespace Vfs owner OOM"))?;
    let fs_context = checked(context, FsContext::new(vfs.root(), vfs.root()))?;
    let path = checked(
        context,
        vfs.resolve(&fs_context, sample_path, ResolveMode::Follow),
    )?;
    let inode = checked(context, path.inode())?;
    context.expect_eq(inode.number(), expected_inode);
    context.expect_eq(
        checked(context, inode.metadata_snapshot())?.kind,
        InodeKind::Regular,
    );
    let mut bytes = [0xa7; 64];
    context.expect_eq(checked(context, inode.read_at(0, &mut bytes))?, bytes.len());
    context.expect_eq(&bytes, expected_bytes);
    let index_path = checked(
        context,
        vfs.resolve(&fs_context, b"/indexed", ResolveMode::Follow),
    )?;
    let index_inode = checked(context, index_path.inode())?;
    context.expect_eq(
        checked(context, index_inode.metadata_snapshot())?.flags & INDEX,
        INDEX,
    );
    let index_shape = checked(context, htree_shape(&volume, index_inode, true))?;
    context.expect_eq(index_shape.depth, 1);
    context.diagnostic(format_args!(
        "W27 cold-read old-volume-weak=0 old-metadata-weak=0 old-cache-weak=0 same-disk=1 new-canonical-cache=1 inode-number={} full-bytes=64 indexed-CRC-verified=1",
        expected_inode
    ));
    Ok(())
}
/// 原 namespace writer 与本例路径读取保持同一 canonical inode/Work 生命周期。
#[ktest::suite(fixture = WriteOwner, suite_resources = DeviceInventory, context = "Thread", tags("fs-path", "fs-namespace"), resources("root"), timeout_ms = 240000)]
pub(crate) mod native {
    use super::*;

    /// 已打开卷的整页超级块视图与短窗口保持同一原页；两个任务只改 boot 保留字节并恢复。
    #[test]
    #[ktest(min_cpus = 2, tags("W02", "fs-metadata-super"))]
    fn initial_super_page_and_short_views_keep_both_updates(
        context: &mut TestContext<'_>,
        fixture: &mut WriteOwner,
    ) -> TestResult {
        fixture.prepare_namespace(context)?;
        let volume = fixture.volume();
        let cache = volume.metadata_cache();
        let full = checked(
            context,
            cache.get(0, mm::PAGE_SIZE, None).map_err(FsError::from),
        )?;
        let short = checked(context, cache.get(0, 1024, None).map_err(FsError::from))?;
        let wide = checked(context, cache.get(0, 2048, None).map_err(FsError::from))?;
        let physical = full.folio().phys_addr();
        context.expect_eq(short.folio().phys_addr(), physical);
        context.expect_eq(wide.folio().phys_addr(), physical);
        let mut expected = checked(
            context,
            full.with_read(|bytes| {
                let mut snapshot = [0; mm::PAGE_SIZE];
                snapshot.copy_from_slice(bytes);
                snapshot
            })
            .map_err(FsError::from),
        )?;
        let original = [expected[17], expected[31]];
        expected[17] ^= 0x6e;
        expected[31] ^= 0xb1;
        let gate = Arc::try_new(crate::sync::Completion::new())
            .map_err(|_| TestError("metadata view start gate OOM"))?;
        let entered = Arc::try_new(AtomicUsize::new(0))
            .map_err(|_| TestError("metadata view participants OOM"))?;
        let mut observations = [None, None];
        let scenario = (|| -> TestResult {
            for (slot, buffer, at, value) in [
                (0, short.clone(), 17, expected[17]),
                (1, wide.clone(), 31, expected[31]),
            ] {
                let gate = gate.clone();
                let entered = entered.clone();
                let volume = volume.clone();
                observations[slot] = Some(fixture.consume_sync(slot, move || {
                    entered.fetch_add(1, Ordering::Release);
                    gate.wait();
                    buffer
                        .update(volume.claim(), |bytes| bytes[at] = value)
                        .map_err(FsError::from)
                })?);
            }
            until(
                "metadata view participants did not reach start gate",
                || entered.load(Ordering::Acquire) == 2,
            )
        })();
        // 每个真实等待者消费一个 token；准备失败也先放行已发表任务再 join。
        gate.complete();
        gate.complete();
        let joined = fixture.join_consumers();
        let current = full
            .with_read(|bytes| bytes == expected)
            .map_err(FsError::from);
        // 只恢复两个 boot 保留字节，不覆盖超级块或其它访问者的共同记录。
        let restored = short
            .update(volume.claim(), |bytes| {
                bytes[17] = original[0];
                bytes[31] = original[1];
            })
            .map_err(FsError::from);
        checked(context, restored)?;
        joined?;
        scenario?;
        context.expect_eq(checked(context, current)?, true);
        for observation in observations {
            context.expect_eq(
                observation
                    .ok_or(TestError("metadata view participant absent"))?
                    .result(),
                Some(Ok(())),
            );
        }
        checked(context, sync_metadata(&volume))?;
        context.diagnostic(format_args!(
            "W02 actual-ext4-block-bytes={} initial-super-page={} short-view-bytes={}/{} original-PFN={} participants=2 full-content-preserved=1 restored-only-boot-bytes=1",
            volume.geometry().block_size,
            full.len(),
            short.len(),
            wide.len(),
            physical,
        ));
        Ok(())
    }

    /// 长链接的真实加载 OOM 先取消全部数据窗口再返位；同页邻窗口保留，正常重用不继承旧错。
    #[test]
    #[ktest(tags("W23", "fs-oom"))]
    fn long_symlink_load_oom_cancels_exact_aliases(
        context: &mut TestContext<'_>,
        fixture: &mut WriteOwner,
    ) -> TestResult {
        let view = prepare_namespace_view(context, fixture)?;
        let volume = view.volume.clone();
        let cache = volume.metadata_cache().clone();
        let disk = fixture.user.path.owner.disk()?.clone();
        let block_size = volume.geometry().block_size;
        let target_len: usize = if block_size == 1024 { 1084 } else { 128 };
        let blocks = target_len.div_ceil(block_size);
        let parent = checked(context, view.root.inode())?;
        let metadata = checked(context, parent.metadata_snapshot())?;
        let allocation = checked(
            context,
            volume.allocate_inode_for_parent(&metadata, InodeKind::Symlink, 0o777, 0, 0, 0),
        )?;
        let number = allocation.number();
        let hint = (number - 1) / volume.geometry().inodes_per_group;
        let first = checked(context, volume.allocate_block(hint))?;
        let first_block = first.physical();
        let second = if blocks == 2 {
            Some(checked(context, volume.allocate_block(hint))?)
        } else {
            None
        };
        let failed_block = second
            .as_ref()
            .map_or(first_block, |block| block.physical());
        if let Some(second) = second {
            volume.release_unpublished_block(second);
        }
        volume.release_unpublished_block(first);
        volume.release_unpublished_inode(allocation);
        let before = allocation_counts(context, &volume)?;
        let pressure = fixture
            .user
            .path
            .owner
            .pressure
            .take()
            .ok_or(TestError("symlink real OOM pressure absent"))?;
        let state = Arc::try_new(SpinLock::new(SymlinkLoadState {
            pressure: Some(pressure),
            captured: false,
            failed: None,
            prefix: None,
            neighbor: None,
            neighbor_bytes: [0; mm::PAGE_SIZE],
            evidence: None,
            preparation_error: None,
        }))
        .map_err(|_| TestError("symlink load observation OOM"))?;
        let observer = Arc::downgrade(&state);
        let hook_disk = disk.clone();
        let prefix_block = (blocks == 2).then_some(first_block);
        let hook: MetadataLoadHook =
            Arc::try_new(move |cache: &Arc<MetadataCache>, block, size| {
                let Some(state) = observer.upgrade() else {
                    return;
                };
                if let Err(error) = observe_symlink_oom(
                    cache,
                    &hook_disk,
                    &state,
                    block,
                    size,
                    failed_block,
                    prefix_block,
                ) {
                    state.lock().preparation_error = Some(error);
                }
            })
            .map_err(|_| TestError("symlink load hook OOM"))?;
        let vfs = view.vfs.clone();
        let fs_context = view.context.clone();
        let root = view.root.clone();
        cache.ktest_set_load_hook(Some(hook));
        let consumer = fixture.consume_sync(0, move || {
            let target = [b'x'; mm::PAGE_SIZE];
            vfs.symlink_from(
                &fs_context,
                &root,
                b"/w23-symlink-oom",
                &target[..target_len],
                (0, 0),
            )
        });
        let joined = if consumer.is_ok() {
            fixture.join_consumers()
        } else {
            Ok(())
        };
        cache.ktest_set_load_hook(None);
        let pressure = state.lock().pressure.take();
        drop(pressure);
        joined?;
        let consumer = consumer?;
        context.expect_eq(state.lock().captured, true);
        context.expect_eq(state.lock().preparation_error, None);
        context.expect_eq(state.lock().evidence, Some((true, true)));
        context.expect_eq(consumer.result(), Some(Err(FsError::NoMemory)));
        let (failed, prefix, neighbor, neighbor_bytes) = {
            let mut state = state.lock();
            (
                state.failed.take(),
                state.prefix.take(),
                state.neighbor.take(),
                state.neighbor_bytes,
            )
        };
        let failed = failed.ok_or(TestError("symlink failed-range observer absent"))?;
        context.expect_eq(failed.with_read(|_| ()), Err(BlockError::NotReady));
        if let Some(prefix) = &prefix {
            context.expect_eq(prefix.with_read(|_| ()), Err(BlockError::NotReady));
        }
        if let Some(neighbor) = &neighbor {
            context.expect_eq(neighbor.folio().phys_addr(), failed.folio().phys_addr());
            let unchanged = checked(
                context,
                neighbor
                    .with_read(|bytes| bytes == &neighbor_bytes[..block_size])
                    .map_err(FsError::Io),
            )?;
            context.expect_eq(unchanged, true);
        }
        let after_failure = allocation_counts(context, &volume)?;
        context.expect_eq(after_failure, before);
        context.expect_eq(
            checked(context, inode_allocation_bit(&volume, number))?,
            false,
        );
        context.expect_eq(
            checked(context, block_allocation_bit(&volume, failed_block))?,
            Some(false),
        );
        if let Some(prefix_block) = prefix_block {
            context.expect_eq(
                checked(context, block_allocation_bit(&volume, prefix_block))?,
                Some(false),
            );
        }
        context.expect_eq(
            view.vfs
                .resolve(&view.context, b"/w23-symlink-oom", ResolveMode::NoFollow)
                .err(),
            Some(FsError::NotFound),
        );
        let target = [b'x'; mm::PAGE_SIZE];
        checked(
            context,
            view.vfs.symlink_from(
                &view.context,
                &view.root,
                b"/w23-symlink-oom",
                &target[..target_len],
                (0, 0),
            ),
        )?;
        let path = checked(
            context,
            view.vfs
                .resolve(&view.context, b"/w23-symlink-oom", ResolveMode::NoFollow),
        )?;
        let inode = checked(context, path.inode())?;
        context.expect_eq(inode.number(), number);
        context.expect_eq(
            checked(context, inode.read_link())?.as_slice(),
            &target[..target_len],
        );
        context.expect_eq(
            checked(context, inode.map_block((blocks - 1) as u64))?,
            BlockMapping::Mapped {
                physical: failed_block,
                len: 1,
            },
        );
        let restored = checked(
            context,
            failed
                .with_read(|bytes| {
                    let offset = (blocks - 1) * block_size;
                    bytes[..target_len - offset] == target[offset..target_len]
                })
                .map_err(FsError::Io),
        )?;
        context.expect_eq(restored, true);
        checked(
            context,
            view.vfs
                .unlink_from(&view.context, &view.root, b"/w23-symlink-oom", (0, 0)),
        )?;
        drop(path);
        checked(context, volume.wait_inode_reuse(number))?;
        disk.test_flush_completions();
        let after_reuse = allocation_counts(context, &volume)?;
        context.expect_eq(after_reuse, before);
        context.diagnostic(format_args!(
            "W23 long-symlink blocks={} block-size={} post-slot-real-page-and-heap-OOM=1 exact-failed-and-prefix-alias-invalidated=1 same-page-neighbor-preserved={} bits-returned=1 normal-reuse-and-readlink=1 controlled-completion-error=0",
            blocks,
            block_size,
            neighbor.is_some()
        ));
        Ok(())
    }

    /// 既有 indexed 目录真实扩展并拆叶/索引节点，rename/unlink 后保留完整 raw 名与冷盘读取。
    #[test]
    #[ktest(tags("W27"))]
    fn indexed_extension_splits_leaf_and_index_node(
        context: &mut TestContext<'_>,
        fixture: &mut WriteOwner,
    ) -> TestResult {
        let disk = fixture.user.path.owner.disk()?.clone();
        select_unsigned_legacy_default(context, &disk)?;
        let view = prepare_namespace_view(context, fixture)?;
        context.expect_eq(view.volume.geometry().default_hash_version, 3);
        context.expect_eq(view.volume.geometry().hash_unsigned, false);
        context.expect_eq(
            view.volume
                .geometry()
                .hash_seed
                .iter()
                .any(|seed| *seed != 0),
            true,
        );
        let source_inode = checked(context, view.source.inode())?.clone();
        let source_number = source_inode.number();
        let initial_links = checked(context, source_inode.metadata_snapshot())?.links;
        let mut source_bytes_before = [0xa7; 64];
        context.expect_eq(
            checked(context, source_inode.read_at(0, &mut source_bytes_before))?,
            source_bytes_before.len(),
        );
        let indexed = checked(
            context,
            view.vfs
                .resolve(&view.context, b"/indexed", ResolveMode::Follow),
        )?;
        let indexed_inode = checked(context, indexed.inode())?.clone();
        let initial = checked(context, htree_shape(&view.volume, &indexed_inode, true))?;
        context.expect_eq(initial.depth, 1);
        context.expect_eq(
            checked(context, indexed_inode.metadata_snapshot())?.flags & INDEX,
            INDEX,
        );
        let target = initial
            .nodes
            .iter()
            .max_by_key(|node| node.count)
            .copied()
            .ok_or(TestError("existing HTree has no index node"))?;
        if target.limit < target.count || initial.root_limit <= initial.root_count {
            return Err(TestError("existing HTree index capacity is inconsistent"));
        }
        let split_limit = target.limit - target.count + 1;
        // 255 字节新名占 264 字节；每个固定叶区间最多再写一个叶容量就必须分裂。
        let leaf_capacity = view.volume.geometry().block_size / ((8 + 255 + 3) & !3);
        let operation_limit = (split_limit * (leaf_capacity + 1)).max(3);
        let link_total = initial_links
            .checked_add(operation_limit as u32)
            .ok_or(TestError("hardlink count overflow"))?;
        if (u16::MAX as u32) < link_total {
            return Err(TestError(
                "bounded hardlink batch exceeds ext4 link-count capacity",
            ));
        }
        let mut names = Vec::new();
        names
            .try_reserve_exact(operation_limit)
            .map_err(|_| TestError("W27 hardlink name storage OOM"))?;
        let mut next_nonce = 0u32;
        let root_file = fixture.open(context, b"/", 0)?;
        let source_file = fixture.open(context, b"/pattern", 0)?;
        let directory_file = fixture.open(context, b"/indexed", 0)?;
        let mut current = initial.clone();
        let mut index_split = false;
        let mut leaf_splits = 0usize;
        let mut operations = 0usize;
        for _ in 0..operation_limit {
            let node = current
                .nodes
                .iter()
                .find(|node| node.logical == target.logical)
                .copied()
                .ok_or(TestError("target index node disappeared before split"))?;
            let name = name_for_split_leaf(&view.volume, &current, node, &mut next_nonce).map_err(
                |error| {
                    context.diagnostic(format_args!("W27 leaf-range candidate error={:?}", error));
                    TestError("bounded leaf-range hash candidate search failed")
                },
            )?;
            let old_leaves = current.leaves;
            checked(
                context,
                view.vfs
                    .link_from(&view.source, &view.context, &indexed, &name.bytes, (0, 0)),
            )?;
            names.push(name);
            operations += 1;
            let next = checked(context, htree_shape(&view.volume, &indexed_inode, false))?;
            if next.leaves != old_leaves {
                context.expect_eq(next.leaves, old_leaves + 1);
                leaf_splits += 1;
            }
            if current.root_count < next.root_count {
                context.expect_eq(next.root_count, current.root_count + 1);
                context.expect_eq(next.nodes.len(), current.nodes.len() + 1);
                context.expect_eq(next.depth, current.depth);
                index_split = true;
                current = next;
                break;
            }
            context.expect_eq(next.root_count, current.root_count);
            context.expect_eq(next.nodes.len(), current.nodes.len());
            context.expect_eq(next.depth, current.depth);
            let updated_node = next
                .nodes
                .iter()
                .find(|entry| entry.logical == node.logical)
                .ok_or(TestError("split leaf lost its parent index node"))?;
            context.expect_eq(
                updated_node.count,
                node.count + usize::from(old_leaves < next.leaves),
            );
            current = next;
            if operations % DIRECTORY_WRITE_BATCH == 0 {
                sync_namespace_batch(context, &disk, &source_file, &directory_file)?;
            }
        }
        if !index_split {
            return Err(TestError(
                "bounded HTree writes did not split a full index node",
            ));
        }
        context.expect_eq(leaf_splits, split_limit);
        while names.len() < 3 {
            let name = name_for_root_range(
                &view.volume,
                &initial,
                target.root_position,
                &mut next_nonce,
            )
            .map_err(|error| {
                context.diagnostic(format_args!("W27 root-range candidate error={:?}", error));
                TestError("bounded root-range hash candidate search failed")
            })?;
            checked(
                context,
                view.vfs
                    .link_from(&view.source, &view.context, &indexed, &name.bytes, (0, 0)),
            )?;
            names.push(name);
            operations += 1;
            if operations % DIRECTORY_WRITE_BATCH == 0 {
                sync_namespace_batch(context, &disk, &source_file, &directory_file)?;
            }
        }
        let rename_target = name_for_root_range(
            &view.volume,
            &initial,
            target.root_position,
            &mut next_nonce,
        )
        .map_err(|error| {
            context.diagnostic(format_args!("W27 rename candidate error={:?}", error));
            TestError("bounded rename hash candidate search failed")
        })?;
        let deleted = names[1];
        checked(
            context,
            view.vfs.rename_from(
                &view.context,
                &indexed,
                &names[0].bytes,
                &indexed,
                &rename_target.bytes,
                false,
                (0, 0),
            ),
        )?;
        checked(
            context,
            view.vfs
                .unlink_from(&view.context, &indexed, &deleted.bytes, (0, 0)),
        )?;
        names[0] = rename_target;
        names.remove(1);
        names.sort_unstable_by_key(|name| name.nonce);
        sync_namespace_batch(context, &disk, &source_file, &directory_file)?;
        checked(context, root_file.sync_range(0..u64::MAX, false))?;
        let after = checked(context, htree_shape(&view.volume, &indexed_inode, true))?;
        preserve_indexed_layout(context, &view.volume, &indexed_inode, &initial, &after)?;
        let final_links = checked(context, source_inode.metadata_snapshot())?.links;
        context.expect_eq(final_links, initial_links + operations as u32 - 1);
        for name in &names {
            let path = checked(
                context,
                view.vfs.resolve_from(
                    &view.context,
                    &indexed,
                    &name.bytes,
                    ResolveMode::Follow,
                    Some((0, 0)),
                ),
            )?;
            context.expect_eq(checked(context, path.inode())?.ptr_eq(&source_inode), true);
        }
        context.expect_eq(
            view.vfs
                .resolve_from(
                    &view.context,
                    &indexed,
                    &deleted.bytes,
                    ResolveMode::Follow,
                    Some((0, 0)),
                )
                .err(),
            Some(FsError::NotFound),
        );
        verify_indexed_new_names(context, &indexed, &names, source_number)?;
        let mut cold_path = Vec::new();
        cold_path
            .try_reserve_exact(9 + rename_target.bytes.len())
            .map_err(|_| TestError("cold namespace path OOM"))?;
        cold_path.extend_from_slice(b"/indexed/");
        cold_path.extend_from_slice(&rename_target.bytes);

        let collision_signed = promote_legacy_directory(
            context,
            fixture,
            &view,
            &source_file,
            b"/legacy-signed",
            PROMOTION_PREFIX_SIGNED,
            0,
            [b"w27-00000788-\xff", b"w27-0000078f-\xff"],
        )?;
        let collision_unsigned = promote_legacy_directory(
            context,
            fixture,
            &view,
            &source_file,
            b"/legacy-unsigned",
            PROMOTION_PREFIX_UNSIGNED,
            3,
            [b"w27-000050b2-\xff", b"w27-000050bb-\xff"],
        )?;
        let signed_version = checked(
            context,
            htree_shape(&view.volume, &collision_signed.inode, true),
        )?;
        let unsigned_version = checked(
            context,
            htree_shape(&view.volume, &collision_unsigned.inode, true),
        )?;
        context.expect_eq(signed_version.root[28], 0);
        context.expect_eq(unsigned_version.root[28], 3);
        complete_directory_names(
            context,
            &collision_signed.path,
            &collision_signed.names,
            source_number,
        )?;
        complete_directory_names(
            context,
            &collision_unsigned.path,
            &collision_unsigned.names,
            source_number,
        )?;
        check_collision_cursors(context, &view, &collision_signed)?;
        check_collision_cursors(context, &view, &collision_unsigned)?;
        sync_namespace_batch(context, &disk, &source_file, &directory_file)?;
        checked(context, root_file.sync_range(0..u64::MAX, false))?;
        for inode in &fixture.inodes {
            checked(context, crate::fs::writeback::sync_inode(inode))?;
        }
        checked(context, crate::fs::writeback::sync_metadata(&view.volume))?;
        disk.test_flush_completions();
        let depth_before = initial.depth;
        let roots_before = initial.root_count;
        let leaves_before = initial.leaves;
        let depth_after = after.depth;
        let roots_after = after.root_count;
        let leaves_after = after.leaves;
        drop((collision_signed, collision_unsigned));
        drop((directory_file, source_file, root_file));
        drop((
            source_inode,
            indexed_inode,
            indexed,
            view,
            initial,
            current,
            after,
        ));
        fixture.inodes.clear();
        cold_reopen_namespace(
            context,
            fixture,
            &cold_path,
            source_number,
            &source_bytes_before,
        )?;
        context.diagnostic(format_args!(
            "W27 existing-indexed depth-before={} root-nodes-before={} leaf-blocks-before={} depth-after={} root-nodes-after={} leaf-blocks-after={} index-node-split=1 leaf-split=1 INDEX-preserved=1 links-added={} renamed=1 unlinked=1 signed-and-unsigned-legacy-cookies=1 complete-new-directory-names=1 cold-same-disk-read=1",
            depth_before, roots_before, leaves_before, depth_after, roots_after, leaves_after, operations
        ));
        Ok(())
    }

    /// 原卷读写视图共享名字与 canonical inode，alias Mount 保留各自权限和目录边界。
    #[test]
    #[ktest(min_cpus = 2, tags("W26"))]
    fn shared_views_alias_mount_and_directory_boundaries(
        context: &mut TestContext<'_>,
        fixture: &mut WriteOwner,
    ) -> TestResult {
        let view = prepare_namespace_view(context, fixture)?;
        drop(fixture.open(context, b"/", 0)?);
        drop(fixture.open(context, b"/pattern", 0)?);

        let cache = fixture.user.path.owner.cache().clone();
        let read_only_vfs = Arc::try_new(checked(
            context,
            Vfs::try_new(view.volume.clone(), cache, 128, 4, AccessMode::ReadOnly),
        )?)
        .map_err(|_| TestError("read-only namespace view OOM"))?;
        let read_only_root = read_only_vfs.root();
        let read_only_context = checked(
            context,
            FsContext::new(read_only_root.clone(), read_only_root.clone()),
        )?;
        let old_pattern = checked(
            context,
            read_only_vfs.resolve(&read_only_context, b"/pattern", ResolveMode::Follow),
        )?;
        let old_pattern_inode = checked(context, old_pattern.inode())?.clone();
        let mut old_bytes_before = [0xa7; 64];
        context.expect_eq(
            checked(context, old_pattern_inode.read_at(0, &mut old_bytes_before))?,
            old_bytes_before.len(),
        );
        context.expect_eq(
            read_only_vfs
                .resolve(&read_only_context, b"/w26-created", ResolveMode::Follow)
                .err(),
            Some(FsError::NotFound),
        );

        checked(
            context,
            view.vfs
                .mkdir_from(&view.context, &view.root, b"/w26-created", 0o777, (0, 0), 0),
        )?;
        drop(fixture.open(context, b"/w26-created", 0)?);
        let old_created = checked(
            context,
            read_only_vfs.resolve(&read_only_context, b"/w26-created", ResolveMode::Follow),
        )?;
        let created_inode = checked(context, old_created.inode())?.clone();
        checked(
            context,
            view.vfs.rename_from(
                &view.context,
                &view.root,
                b"w26-created",
                &view.root,
                b"w26-renamed",
                false,
                (0, 0),
            ),
        )?;
        context.expect_eq(
            read_only_vfs
                .resolve(&read_only_context, b"/w26-created", ResolveMode::Follow)
                .err(),
            Some(FsError::NotFound),
        );
        let renamed = checked(
            context,
            read_only_vfs.resolve(&read_only_context, b"/w26-renamed", ResolveMode::Follow),
        )?;
        context.expect_eq(
            checked(context, renamed.inode())?.ptr_eq(&created_inode),
            true,
        );

        checked(
            context,
            view.vfs
                .unlink_from(&view.context, &view.root, b"pattern", (0, 0)),
        )?;
        let new_file = checked(
            context,
            view.vfs.open_with(
                &view.context,
                &view.root,
                b"pattern",
                (0, 0),
                Some((0o666, view.context.umask())),
                false,
                false,
                |path, _| open_path(path),
            ),
        )?;
        let new_pattern = new_file.path().clone();
        let new_pattern_inode = checked(context, new_pattern.inode())?.clone();
        fixture.inodes.push(new_pattern_inode.clone());
        let current_pattern = checked(
            context,
            read_only_vfs.resolve(&read_only_context, b"/pattern", ResolveMode::Follow),
        )?;
        context.expect_eq(
            checked(context, current_pattern.inode())?.ptr_eq(&new_pattern_inode),
            true,
        );
        context.expect_eq(
            checked(context, old_pattern.inode())?.ptr_eq(&old_pattern_inode),
            true,
        );
        context.expect_eq(
            old_pattern.dentry().name().as_slice(),
            b"pattern".as_slice(),
        );
        let mut old_bytes = [0xa7; 64];
        context.expect_eq(
            checked(context, old_pattern_inode.read_at(0, &mut old_bytes))?,
            old_bytes.len(),
        );
        context.expect_eq(old_bytes, old_bytes_before);

        let self_directory = b"/w26-nonempty";
        checked(
            context,
            view.vfs
                .mkdir_from(&view.context, &view.root, self_directory, 0o777, (0, 0), 0),
        )?;
        drop(fixture.open(context, self_directory, 0)?);
        let self_path = checked(
            context,
            view.vfs
                .resolve(&view.context, self_directory, ResolveMode::Follow),
        )?;
        let self_link = fixture.consume_sync(0, {
            let vfs = view.vfs.clone();
            let fs_context = view.context.clone();
            let start = self_path.clone();
            let source = self_path.clone();
            move || vfs.link_from(&source, &fs_context, &start, b"self", (0, 0))
        })?;
        until("directory self-hardlink did not finish", || {
            self_link.result().is_some()
        })?;
        fixture.join_consumers()?;
        context.expect_eq(self_link.result(), Some(Err(FsError::NotPermitted)));

        checked(
            context,
            view.vfs
                .mkdir_from(&view.context, &self_path, b"child", 0o777, (0, 0), 0),
        )?;
        drop(fixture.open(context, b"/w26-nonempty/child", 0)?);
        context.expect_eq(
            view.vfs
                .rmdir_from(&view.context, &view.root, self_directory, (0, 0))
                .err(),
            Some(FsError::NotEmpty),
        );
        checked(
            context,
            view.vfs.mkdir_from(
                &view.context,
                &view.root,
                b"/w26-empty-source",
                0o777,
                (0, 0),
                0,
            ),
        )?;
        drop(fixture.open(context, b"/w26-empty-source", 0)?);
        context.expect_eq(
            view.vfs
                .rename_from(
                    &view.context,
                    &view.root,
                    b"w26-empty-source",
                    &view.root,
                    b"w26-nonempty",
                    false,
                    (0, 0),
                )
                .err(),
            Some(FsError::NotEmpty),
        );

        checked(
            context,
            view.vfs.mkdir_from(
                &view.context,
                &view.root,
                b"/w26-held-cwd",
                0o777,
                (0, 0),
                0,
            ),
        )?;
        drop(fixture.open(context, b"/w26-held-cwd", 0)?);
        let held_cwd = checked(
            context,
            view.vfs
                .resolve(&view.context, b"/w26-held-cwd", ResolveMode::Follow),
        )?;
        let held_context = checked(context, FsContext::new(view.root.clone(), held_cwd.clone()))?;
        checked(
            context,
            view.vfs
                .rmdir_from(&held_context, &view.root, b"/w26-held-cwd", (0, 0)),
        )?;
        context.expect_eq(
            view.vfs
                .mkdir_from(&held_context, &held_cwd, b"child", 0o777, (0, 0), 0)
                .err(),
            Some(FsError::NotFound),
        );
        context.expect_eq(
            view.vfs
                .resolve(&view.context, b"/w26-held-cwd", ResolveMode::Follow)
                .err(),
            Some(FsError::NotFound),
        );

        checked(
            context,
            view.vfs
                .mkdir_from(&view.context, &view.root, b"/w26-alias", 0o777, (0, 0), 0),
        )?;
        drop(fixture.open(context, b"/w26-alias", 0)?);
        let alias_covered = checked(
            context,
            view.vfs
                .resolve(&view.context, b"/w26-alias", ResolveMode::Follow),
        )?;
        checked(
            context,
            view.vfs
                .mount(&alias_covered, view.volume.clone(), AccessMode::ReadWrite),
        )?;
        let alias_root = checked(
            context,
            view.vfs
                .resolve(&view.context, b"/w26-alias", ResolveMode::Follow),
        )?;
        context.expect_eq(alias_root.ptr_eq(&alias_covered), false);
        context.expect_eq(
            Arc::ptr_eq(alias_root.filesystem(), view.root.filesystem()),
            true,
        );
        context.expect_eq(
            view.vfs
                .link_from(
                    &view.source,
                    &view.context,
                    &alias_root,
                    b"w26-cross-link",
                    (0, 0),
                )
                .err(),
            Some(FsError::CrossDevice),
        );
        context.expect_eq(
            view.vfs
                .rename_from(
                    &view.context,
                    &view.root,
                    b"pattern",
                    &alias_root,
                    b"w26-cross-rename",
                    false,
                    (0, 0),
                )
                .err(),
            Some(FsError::CrossDevice),
        );
        context.expect_eq(
            view.vfs
                .rmdir_from(&view.context, &view.root, b"/", (0, 0))
                .err(),
            Some(FsError::Busy),
        );
        context.expect_eq(
            view.vfs
                .rename_from(
                    &view.context,
                    &view.root,
                    b"/",
                    &view.root,
                    b"w26-root-move",
                    false,
                    (0, 0),
                )
                .err(),
            Some(FsError::Busy),
        );
        context.expect_eq(
            view.vfs
                .rmdir_from(&view.context, &view.root, b"/w26-alias", (0, 0))
                .err(),
            Some(FsError::Busy),
        );
        context.expect_eq(
            view.vfs
                .rename_from(
                    &view.context,
                    &view.root,
                    b"/w26-alias",
                    &view.root,
                    b"/w26-alias-moved",
                    false,
                    (0, 0),
                )
                .err(),
            Some(FsError::Busy),
        );
        drop(alias_root);
        checked(context, view.vfs.unmount(&alias_covered))?;

        checked(
            context,
            view.vfs.mkdir_from(
                &view.context,
                &view.root,
                b"/w26-source-readonly",
                0o555,
                (0, 0),
                0,
            ),
        )?;
        let source_parent_file = fixture.open(context, b"/w26-source-readonly", 0)?;
        let source_parent = source_parent_file.path().clone();
        checked(
            context,
            view.vfs.mkdir_from(
                &view.context,
                &source_parent,
                b"restricted",
                0o555,
                (0, 0),
                0,
            ),
        )?;
        drop(fixture.open(context, b"/w26-source-readonly/restricted", 0)?);
        checked(
            context,
            view.vfs.mkdir_from(
                &view.context,
                &view.root,
                b"/w26-target-parent",
                0o777,
                (0, 0),
                0,
            ),
        )?;
        let target_parent_file = fixture.open(context, b"/w26-target-parent", 0)?;
        let target_parent = target_parent_file.path().clone();
        context.expect_eq(
            view.vfs
                .rename_from(
                    &view.context,
                    &source_parent,
                    b"restricted",
                    &target_parent,
                    b"restricted",
                    false,
                    (1001, 1001),
                )
                .err(),
            Some(FsError::Permission),
        );

        context.diagnostic(format_args!(
            "W26 shared-volume-ro-rw-positive-negative-and-current-names=1 old-path-inode-retained=1 alias-mount-rename-link=EXDEV self-directory-link=NotPermitted nonempty-rmdir-rename=ENOTEMPTY held-cwd-removal=1 root-mountpoint-removal-move=EBUSY restricted-cross-parent=EACCES"
        ));
        Ok(())
    }

    /// 原同名加载先结束并唤醒等待者，写者随后发表硬链接；新查找不得复用旧负绑定。
    #[test]
    #[ktest(min_cpus = 2, tags("W28"))]
    fn directory_loading_serializes_link_publication(
        context: &mut TestContext<'_>,
        fixture: &mut WriteOwner,
    ) -> TestResult {
        fixture.prepare_namespace(context)?;
        let vfs = fixture.user.path.vfs().clone();
        let fs_context = fixture.user.path.context().clone();
        let root = vfs.root();
        let source = checked(
            context,
            vfs.resolve(&fs_context, b"/pattern", ResolveMode::Follow),
        )?;
        let source_inode = checked(context, source.inode())?.clone();
        drop(fixture.open(context, b"/", 0)?);
        drop(fixture.open(context, b"/pattern", 0)?);
        let observer = fixture.user.path.owner.observer().clone();
        install_lookup_gate(context, &vfs, observer.clone(), b"w28-publish")?;
        observer.hold_loader.store(true, Ordering::Release);
        let before_loads = vfs.dcache().ktest_load_count();
        let parent = root.dentry().clone();
        fixture.user.path.owner.publish(
            0,
            Job::Lookup {
                vfs: vfs.clone(),
                parent: parent.clone(),
                name: b"w28-publish",
            },
        );
        let first_ready = until("metadata loader did not reach actual InLookup", || {
            observer.loader_entered.load(Ordering::Acquire) == 1
                && fixture.user.path.owner.observer().blocked(0)
        });
        let mut observation = None;
        let scenario = (|| -> TestResult {
            first_ready?;
            fixture.user.path.owner.publish(
                1,
                Job::Lookup {
                    vfs: vfs.clone(),
                    parent: parent.clone(),
                    name: b"w28-publish",
                },
            );
            until("metadata lookup waiter did not Block", || {
                vfs.dcache().ktest_waiting(root.dentry(), b"w28-publish") == 1
                    && fixture.user.path.owner.observer().blocked(1)
            })?;
            observation = Some(fixture.consume_sync(0, {
                let vfs = vfs.clone();
                let context = fs_context.clone();
                let start = root.clone();
                let source = source.clone();
                move || vfs.link_from(&source, &context, &start, b"w28-publish", (0, 0))
            })?);
            until(
                "namespace writer did not Block on directory read side",
                || observation.as_ref().is_some_and(|state| state.blocked()),
            )?;
            context.expect_eq(vfs.dcache().ktest_load_count(), before_loads + 1);
            Ok(())
        })();
        observer.hold_loader.store(false, Ordering::Release);
        observer.gate.complete();
        let reader_join = fixture.user.path.owner.join_tasks();
        let writer_join = fixture.join_consumers();
        vfs.dcache().ktest_set_lookup_hook(None);
        scenario?;
        reader_join?;
        writer_join?;
        context.expect_eq(
            observation
                .as_ref()
                .ok_or(TestError("namespace writer observation absent"))?
                .result(),
            Some(Ok(())),
        );
        let results = core::mem::replace(&mut *observer.results.lock(), [const { None }; 3]);
        for result in IntoIterator::into_iter(results).take(2) {
            context.expect_eq(
                result
                    .ok_or(TestError("metadata lookup waiter result absent"))?
                    .err(),
                Some(FsError::NotFound),
            );
        }
        context.expect_eq(vfs.dcache().ktest_waiting(root.dentry(), b"w28-publish"), 0);
        let published = checked(
            context,
            vfs.resolve(&fs_context, b"/w28-publish", ResolveMode::Follow),
        )?;
        context.expect_eq(
            checked(context, published.inode())?.ptr_eq(&source_inode),
            true,
        );
        context.expect_eq(vfs.dcache().ktest_load_count(), before_loads + 1);
        context.diagnostic(format_args!(
            "W28 metadata-loading unique-InLookup=1 real-waiters=1 writer-Blocked=1 old-result=NotFound post-write-current-inode=1 negative-snapshot-not-republished=1 hardware-EIO-covered=0"
        ));
        Ok(())
    }

    /// 真实目录校验失败结束唯一 Loading；恢复校验字节后同名新链接必须重新加载成功。
    #[test]
    #[ktest(min_cpus = 2, tags("W28"))]
    fn directory_loading_preserves_metadata_error_and_reloads(
        context: &mut TestContext<'_>,
        fixture: &mut WriteOwner,
    ) -> TestResult {
        fixture.prepare_namespace(context)?;
        let vfs = fixture.user.path.vfs().clone();
        let fs_context = fixture.user.path.context().clone();
        let root = vfs.root();
        let volume = fixture.volume();
        let root_inode = checked(context, root.inode())?.clone();
        let source = checked(
            context,
            vfs.resolve(&fs_context, b"/pattern", ResolveMode::Follow),
        )?;
        let source_inode = checked(context, source.inode())?.clone();
        drop(fixture.open(context, b"/", 0)?);
        drop(fixture.open(context, b"/pattern", 0)?);
        let (metadata, map_root) = checked(
            context,
            root_inode.with_map_read(|metadata, root| Ok((*metadata, *root))),
        )?;
        let physical = match volume.map_block(&metadata, &map_root, 0, None) {
            Ok(BlockMapping::Mapped { physical, .. }) => physical,
            Ok(_) => return Err(TestError("root directory block is not mapped")),
            Err(error) => {
                context.diagnostic(format_args!("W28 root block lookup error={:?}", error));
                return Err(TestError("root directory mapping failed"));
            }
        };
        let buffer = checked(context, volume.metadata_block(physical))?;
        let geometry = volume.geometry();
        let original = directory_block(&volume, &metadata, &map_root, 0).map_err(|error| {
            context.diagnostic(format_args!("W28 root metadata read error={:?}", error));
            TestError("root metadata read failed")
        })?;
        let mut corrupted = original;
        corrupted[4..6].copy_from_slice(&0u16.to_le_bytes());
        let expected = codec::verify_directory(
            geometry,
            &metadata,
            0,
            &corrupted[..geometry.block_size],
            false,
        )
        .err()
        .ok_or(TestError(
            "controlled metadata corruption did not fail layout or CRC",
        ))?;
        let observer = fixture.user.path.owner.observer().clone();
        install_lookup_gate(context, &vfs, observer.clone(), b"w28-checksum")?;
        observer.hold_loader.store(true, Ordering::Release);
        let before_len = vfs.dcache().len();
        let before_loads = vfs.dcache().ktest_load_count();
        let parent = root.dentry().clone();
        fixture.user.path.owner.publish(
            0,
            Job::Lookup {
                vfs: vfs.clone(),
                parent: parent.clone(),
                name: b"w28-checksum",
            },
        );
        let first_ready = until("checksum loader did not reach published Loading", || {
            observer.loader_entered.load(Ordering::Acquire) == 1
                && fixture.user.path.owner.observer().blocked(0)
        });
        let scenario = (|| -> TestResult {
            first_ready?;
            checked(
                context,
                buffer
                    .update(volume.claim(), |bytes| {
                        bytes.copy_from_slice(&corrupted[..geometry.block_size]);
                    })
                    .map_err(FsError::from),
            )?;
            fixture.user.path.owner.publish(
                1,
                Job::Lookup {
                    vfs: vfs.clone(),
                    parent: parent.clone(),
                    name: b"w28-checksum",
                },
            );
            until("checksum metadata waiter did not Block", || {
                vfs.dcache().ktest_waiting(root.dentry(), b"w28-checksum") == 1
                    && fixture.user.path.owner.observer().blocked(1)
            })?;
            observer.hold_loader.store(false, Ordering::Release);
            observer.gate.complete();
            Ok(())
        })();
        observer.hold_loader.store(false, Ordering::Release);
        observer.gate.complete();
        let joined = fixture.user.path.owner.join_tasks();
        let restored = buffer
            .update(volume.claim(), |bytes| {
                bytes.copy_from_slice(&original[..geometry.block_size])
            })
            .map_err(FsError::from);
        vfs.dcache().ktest_set_lookup_hook(None);
        joined?;
        checked(context, restored)?;
        scenario?;
        let results = core::mem::replace(&mut *observer.results.lock(), [const { None }; 3]);
        for result in IntoIterator::into_iter(results).take(2) {
            context.expect_eq(
                result
                    .ok_or(TestError("metadata waiter result absent"))?
                    .err(),
                Some(expected),
            );
        }
        context.expect_eq(vfs.dcache().len(), before_len);
        context.expect_eq(
            vfs.dcache().ktest_waiting(root.dentry(), b"w28-checksum"),
            0,
        );
        context.expect_eq(vfs.dcache().ktest_load_count(), before_loads + 1);
        context.expect_eq(
            vfs.dcache()
                .lookup(vfs.inodes(), root.dentry(), b"w28-checksum")
                .err(),
            Some(FsError::NotFound),
        );
        checked(
            context,
            vfs.link_from(&source, &fs_context, &root, b"w28-checksum", (0, 0)),
        )?;
        let current = checked(
            context,
            vfs.resolve(&fs_context, b"/w28-checksum", ResolveMode::Follow),
        )?;
        context.expect_eq(
            checked(context, current.inode())?.ptr_eq(&source_inode),
            true,
        );
        context.expect_eq(vfs.dcache().ktest_load_count(), before_loads + 2);
        context.diagnostic(format_args!(
            "W28 metadata-error first-error={:?} shared-waiters=1 failed-placeholder-removed=1 restored-current-value=1 hardware-EIO-covered=0",
            expected
        ));
        Ok(())
    }

    /// 实际文件页读入 completion 停点下同页 writer 等待原填充，再只留下新当前字节。
    #[test]
    #[ktest(min_cpus = 2, tags("W28"))]
    fn file_page_loading_serializes_write_before_current_value(
        context: &mut TestContext<'_>,
        fixture: &mut WriteOwner,
    ) -> TestResult {
        fixture.prepare_namespace(context)?;
        let file = fixture.open(context, b"/pattern", 2)?;
        let path = file.path().clone();
        let inode = checked(context, path.inode())?.clone();
        let volume = fixture.volume();
        let block_size = volume.geometry().block_size as u64;
        for logical in 0..(mm::PAGE_SIZE as u64 / block_size) {
            checked(context, inode.map_block(logical))?;
        }
        let disk = fixture.user.path.owner.disk()?.clone();
        let before = disk.test_snapshot();
        disk.test_suspend_completions();
        fixture.user.path.owner.publish(
            0,
            Job::Read {
                inode: inode.clone(),
                page: 0,
            },
        );
        let loader_ready = until("file page loader did not enter real data I/O", || {
            let snapshot = disk.test_snapshot();
            before.file_read_submitted < snapshot.file_read_submitted
                && snapshot.inflight != 0
                && fixture.user.path.owner.observer().blocked(0)
        });
        let mut writer = None;
        let scenario = (|| -> TestResult {
            loader_ready?;
            fixture.user.path.owner.publish(
                1,
                Job::Read {
                    inode: inode.clone(),
                    page: 0,
                },
            );
            until(
                "same-page reader did not Block on the published folio",
                || fixture.user.path.owner.observer().blocked(1),
            )?;
            let folio = inode
                .mapping()
                .cached_folio(0)
                .ok_or(TestError("loading folio owner disappeared"))?;
            context.expect_eq(folio.is_ready(), false);
            let file_read_requests = disk.test_snapshot().file_read_submitted;
            writer = Some(fixture.consume_sync(0, {
                let file = file.clone();
                move || {
                    let replacement = [0x5a; 32];
                    if file.write_at(0, &replacement)? != replacement.len() {
                        return Err(FsError::Corrupt);
                    }
                    Ok(())
                }
            })?);
            until(
                "same-page writer did not Block behind original page load",
                || writer.as_ref().is_some_and(|state| state.blocked()),
            )?;
            context.expect_eq(disk.test_snapshot().file_read_submitted, file_read_requests);
            context.expect_eq(folio.is_ready(), false);
            drop(folio);
            Ok(())
        })();
        disk.test_resume_completions();
        let readers = fixture.user.path.owner.join_tasks();
        let writers = fixture.join_consumers();
        disk.test_flush_completions();
        context.expect_eq(disk.test_snapshot().inflight, 0);
        scenario?;
        readers?;
        writers?;
        context.expect_eq(
            writer
                .as_ref()
                .ok_or(TestError("file writer observation absent"))?
                .result(),
            Some(Ok(())),
        );
        let results = core::mem::replace(
            &mut *fixture.user.path.owner.observer().results.lock(),
            [const { None }; 3],
        );
        let mut address = None;
        for result in IntoIterator::into_iter(results).take(2) {
            let returned = checked(context, result.ok_or(TestError("file read result absent"))?)?;
            let Returned::Page {
                folio,
                bytes,
                valid,
            } = returned
            else {
                return Err(TestError("file read returned wrong result"));
            };
            context.expect_eq(
                (bytes, valid, folio.is_ready()),
                (mm::PAGE_SIZE, true, true),
            );
            if let Some(address) = address {
                context.expect_eq(folio.phys_addr(), address);
            } else {
                address = Some(folio.phys_addr());
            }
        }
        let mut data = [0xa7; mm::PAGE_SIZE];
        context.expect_eq(checked(context, inode.read_at(0, &mut data))?, data.len());
        context.expect_eq(&data[..32], &[0x5a; 32]);
        context.expect_eq(patterned(&data[32..], 32), true);
        let cached = inode
            .mapping()
            .cached_folio(0)
            .ok_or(TestError("current file folio disappeared"))?;
        context.expect_eq(inode.mapping().is_canonical(0, &cached), true);
        context.expect_eq(Some(cached.phys_addr()), address);
        context.diagnostic(format_args!(
            "W28 file-page real-data-request-accepted=1 completion-paused=1 same-page-reader-Blocked=1 writer-Blocked=1 one-canonical-folio=1 old-read-valid=1 final-current-bytes=1 pre-IO-hook-used=0 hardware-DMA-state-claimed=0"
        ));
        Ok(())
    }

    /// 真实文件页读入的受控完成错误由原等待者共享，失败 folio 回收后新读取重新提交。
    #[test]
    #[ktest(min_cpus = 2, tags("W28"))]
    fn failed_file_page_loading_retries_only_after_real_retirement(
        context: &mut TestContext<'_>,
        fixture: &mut WriteOwner,
    ) -> TestResult {
        fixture.prepare_namespace(context)?;
        let file = fixture.open(context, b"/pattern", 0)?;
        let inode = checked(context, file.path().inode())?.clone();
        let volume = fixture.volume();
        let block_size = volume.geometry().block_size as u64;
        for logical in 0..(mm::PAGE_SIZE as u64 / block_size) {
            checked(context, inode.map_block(logical))?;
        }
        let disk = fixture.user.path.owner.disk()?.clone();
        let before = disk.test_snapshot();
        disk.test_suspend_completions();
        fixture.user.path.owner.publish(
            0,
            Job::Read {
                inode: inode.clone(),
                page: 0,
            },
        );
        let loader_ready = until("failing page did not submit original data request", || {
            let snapshot = disk.test_snapshot();
            before.file_read_submitted < snapshot.file_read_submitted
                && snapshot.inflight != 0
                && fixture.user.path.owner.observer().blocked(0)
        });
        let scenario = (|| -> TestResult {
            loader_ready?;
            disk.test_fail_next_file_read(BlockError::Io);
            fixture.user.path.owner.publish(
                1,
                Job::Read {
                    inode: inode.clone(),
                    page: 0,
                },
            );
            until(
                "failed page waiter did not Block on unique loading folio",
                || fixture.user.path.owner.observer().blocked(1),
            )?;
            let folio = inode
                .mapping()
                .cached_folio(0)
                .ok_or(TestError("failing page disappeared before completion"))?;
            context.expect_eq(folio.is_ready(), false);
            Ok(())
        })();
        disk.test_resume_completions();
        let readers = fixture.user.path.owner.join_tasks();
        disk.test_flush_completions();
        context.expect_eq(disk.test_snapshot().inflight, 0);
        scenario?;
        readers?;
        context.expect_eq(disk.test_completion_error_pending(), false);
        let results = core::mem::replace(
            &mut *fixture.user.path.owner.observer().results.lock(),
            [const { None }; 3],
        );
        for result in IntoIterator::into_iter(results).take(2) {
            context.expect_eq(
                result
                    .ok_or(TestError("failed page waiter result absent"))?
                    .err(),
                Some(FsError::Io(BlockError::Io)),
            );
        }
        let failed = inode
            .mapping()
            .cached_folio(0)
            .ok_or(TestError("failed page cache owner disappeared"))?;
        context.expect_eq(failed.wait_ready(), Err(BlockError::Io));
        drop(failed);
        context.expect_eq(0 < inode.mapping().prune_unused(), true);
        context.expect_eq(inode.mapping().len(), 0);
        let before_retry = disk.test_snapshot();
        let mut bytes = [0xa7; mm::PAGE_SIZE];
        context.expect_eq(checked(context, inode.read_at(0, &mut bytes))?, bytes.len());
        context.expect_eq(patterned(&bytes, 0), true);
        context.expect_eq(
            before_retry.file_read_submitted < disk.test_snapshot().file_read_submitted,
            true,
        );
        context.diagnostic(format_args!(
            "W28 controlled-data-completion-error=Io original-waiters=2 failed-folio-pruned=1 subsequent-normal-load=1 hardware-EIO-claimed=0"
        ));
        Ok(())
    }
}
