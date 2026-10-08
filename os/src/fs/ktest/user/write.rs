//! 可写私有工作卷上的实际字节、分配树、普通用户 ABI 与映像排斥消费。

use crate::arch::UserAccess;
use crate::fs::ext4::{codec, BlockMapping, InodeData};
use crate::fs::file::{open_path_with_flags, WriteBuffer};
use crate::fs::ktest::user::*;
use crate::fs::SeekFrom;
use crate::mm::space::{MmapAddress, MmapRequest, MmapSource};
use crate::mm::user_table::PagePermissions;
use crate::mm::PAGE_SIZE;
use crate::sync::PreemptGuard;

const RW: PagePermissions = PagePermissions {
    read: true,
    write: true,
    execute: false,
};
const RO: PagePermissions = PagePermissions {
    read: true,
    write: false,
    execute: false,
};
const SOURCE: usize = 0x4800_0000;
const PROGRAM_WRITE: &[u8] = b"/test/bin/user-write";
const SHARED_CONTROL: usize = 0x4900_0000;
const SHARED_FILE_OFFSET: usize = 24;
const SHARED_ALIAS: usize = 0x4a00_0000;

struct WriteOwner {
    user: UserOwner,
    inodes: Vec<InodeRef>,
    append_gate: Option<Arc<Completion>>,
    shared_mm: Option<Arc<Mm>>,
    prepare_gate: Option<Arc<Completion>>,
    consumers: [Option<JoinHandle>; 2],
}

impl WriteOwner {
    /// 正文显式准备 RW 卷及同一根挂载，普通用户运行仍借原身份拥有者。
    fn prepare(&mut self, context: &TestContext<'_>) -> TestResult {
        self.inodes
            .try_reserve_exact(16)
            .map_err(|_| TestError("write inode owner OOM"))?;
        self.user.prepare_mode(context, AccessMode::ReadWrite)
    }

    /// 保留实际 inode 到同步排空之后；失败路径也不能丢弃已接受修改。
    fn open(
        &mut self,
        context: &TestContext<'_>,
        pathname: &[u8],
        flags: u32,
    ) -> Result<FileRef, TestError> {
        let path = checked(
            context,
            self.user
                .path
                .vfs()
                .resolve(self.user.path.context(), pathname, ResolveMode::Follow),
        )?;
        let inode = checked(context, path.inode())?.clone();
        if !self.inodes.iter().any(|old| old.ptr_eq(&inode)) {
            if self.inodes.len() == self.inodes.capacity() {
                return Err(TestError("write inode owner capacity"));
            }
            self.inodes.push(inode);
        }
        checked(context, open_path_with_flags(path, flags))
    }

    /// 从同一原根取得卷，根引用仅在正文和同步期间短暂保活。
    fn volume(&self) -> Arc<Ext4Volume> {
        self.user.path.vfs().root().filesystem().clone()
    }

    /// 用户普通 syscall 自己断言返回值，内核仍消费 wait 状态与真实离栈 join。
    fn run(&mut self, context: &mut TestContext<'_>, mode: &[u8]) -> TestResult {
        for pathname in [
            b"/test/write-user" as &[u8],
            b"/test/write-append",
            b"/test/write-map",
        ] {
            drop(self.open(context, pathname, 0)?);
        }
        let image = self.user.load_named(context, PROGRAM_WRITE, mode)?;
        self.user
            .start_program(context, image, false, false, Credentials::ROOT)?;
        let event = self.user.wait(context)?;
        if event.status != 0 {
            self.user.graph().request_root_exit(9);
        }
        self.user.reap_children(context, event.status != 0)?;
        context.expect_eq(event.status, 0);
        Ok(())
    }

    /// 普通 ELF 自己 mmap 文件；共享匿名控制页只承担检查点，不改变生产访问路径。
    fn shared_image(
        &mut self,
        context: &TestContext<'_>,
        mode: &[u8],
    ) -> Result<LoadedImage, TestError> {
        let image = self.user.load_named(context, PROGRAM_WRITE, mode)?;
        vm(
            context,
            image.mm.mmap(MmapRequest {
                address: MmapAddress::FixedNoReplace(SHARED_CONTROL),
                length: 4096,
                permissions: RW,
                maximum: RW,
                source: MmapSource::SharedAnonymous,
            }),
        )?;
        if image.mm.copyout(SHARED_CONTROL, &[0; 32]).copied != 32 {
            return Err(TestError("shared control initialization failed"));
        }
        self.shared_mm = Some(image.mm.clone());
        Ok(image)
    }

    /// 用户执行固定在另一 CPU，控制者不持文件锁等待其普通指令检查点。
    fn start_shared(&mut self, context: &TestContext<'_>, image: LoadedImage) -> TestResult {
        self.user
            .start_program(context, image, true, false, Credentials::ROOT)
    }

    /// 实际退出离栈必须在原十秒内可观察，随后消费原 wait 事件和全部子进程。
    fn finish_shared(&mut self, context: &mut TestContext<'_>) -> TestResult {
        until("shared user did not leave original stack", || {
            self.user
                .running
                .as_ref()
                .unwrap()
                .join
                .try_join()
                .is_some()
        })?;
        let event = self.user.wait(context)?;
        if event.status != 0 {
            self.user.graph().request_root_exit(9);
        }
        self.user.reap_children(context, event.status != 0)?;
        context.expect_eq(event.status, 0);
        self.shared_mm.take();
        Ok(())
    }

    /// 检查点同时核原用户绑定仍活，避免把用户提前死亡当作阶段完成。
    fn phase(&self, mm: &Mm, offset: usize, expected: u32) -> TestResult {
        until("shared user checkpoint missing", || {
            mm.load_user_u32(SHARED_CONTROL + offset) == Ok(expected)
                || self
                    .user
                    .running
                    .as_ref()
                    .unwrap()
                    .join
                    .try_join()
                    .is_some()
        })?;
        if mm.load_user_u32(SHARED_CONTROL + offset) != Ok(expected) {
            return Err(TestError("shared user exited before checkpoint"));
        }
        Ok(())
    }

    /// 原 copyout 发布普通控制字，不能借文件原页或 PTE 裸写绕过 mkwrite。
    fn release(mm: &Mm, value: u32) -> TestResult {
        mm.store_user_u32(SHARED_CONTROL + 4, value)
            .map_err(|_| TestError("shared user gate publication failed"))
    }

    /// 由普通 fork 已发表的真实 TID 取 child 原绑定，诊断引用不承担执行寿命。
    fn child(&self, mm: &Mm) -> Result<Arc<task::UserThread>, TestError> {
        let mut tid = 0;
        until("shared fork child identity missing", || {
            tid = mm.read_usize(SHARED_CONTROL + 16).unwrap_or(0);
            tid != 0
        })?;
        self.user
            .graph()
            .lookup_thread(tid)
            .ok_or(TestError("shared fork child binding missing"))
    }

    /// 未离栈句柄留在 fixture，错误返回仍先恢复设备完成再进行有限 join。
    fn join_consumers(&mut self) -> TestResult {
        for join in self.consumers.iter().flatten() {
            until("shared kernel consumer did not leave stack", || {
                join.try_join().is_some()
            })?;
        }
        for join in &mut self.consumers {
            if let Some(join) = join.take() {
                if join.join() != 0 {
                    return Err(TestError("shared kernel consumer failed"));
                }
            }
        }
        Ok(())
    }
}

impl Fixture<DeviceInventory> for WriteOwner {
    /// 设备与压力存储均复用既有拥有者，正文失败也保留停止责任。
    fn set_up(
        context: &mut TestContext<'_>,
        inventory: &DeviceInventory,
    ) -> Result<Self, TestError> {
        Ok(Self {
            user: <UserOwner as Fixture<DeviceInventory>>::set_up(context, inventory)?,
            inodes: Vec::new(),
            append_gate: None,
            shared_mm: None,
            prepare_gate: None,
            consumers: core::array::from_fn(|_| None),
        })
    }

    /// 所有生产者先退出和离栈，再真实写出文件页、元数据及 FLUSH，最后释放根并停止设备。
    fn quiesce(&mut self, context: &mut TestContext<'_>) -> TestResult {
        if let Some(mm) = self.shared_mm.take() {
            mm.ktest_set_mkwrite_hook(None);
        }
        if let Some(gate) = self.prepare_gate.take() {
            gate.complete();
        }
        if let Some(gate) = self.append_gate.take() {
            gate.complete();
            gate.complete();
        }
        self.user
            .path
            .owner
            .observer()
            .cancelled
            .store(true, Ordering::Release);
        self.user.path.owner.observer().gate.complete();
        for disk in &self.user.path.owner.disks {
            disk.test_resume_completions();
        }
        if let Some(graph) = &self.user.graph {
            graph.request_root_exit(9);
        }
        self.join_consumers()?;
        self.user.path.owner.join_tasks()?;
        if self.user.running.is_some() {
            until("shared user cleanup did not leave original stack", || {
                self.user
                    .running
                    .as_ref()
                    .unwrap()
                    .join
                    .try_join()
                    .is_some()
            })?;
            self.user.wait(context)?;
        }
        if self.user.graph.is_some() {
            self.user.reap_children(context, true)?;
        }
        self.user.release_pressure();
        if self.user.graph.is_some() {
            for inode in &self.inodes {
                checked(context, crate::fs::writeback::sync_inode(inode))?;
            }
            checked(context, crate::fs::writeback::sync_volume(&self.volume()))?;
        }
        self.inodes.clear();
        drop(core::mem::take(&mut self.inodes));
        <UserOwner as Fixture<DeviceInventory>>::quiesce(&mut self.user, context)
    }

    /// 两层六域和 Task/worker/domain 均由原拥有者比较，不重置统计。
    fn tear_down(self, context: &mut TestContext<'_>) -> TestResult {
        <UserOwner as Fixture<DeviceInventory>>::tear_down(self.user, context)
    }
}

/// 每个期望字节来自独立输入，短读取或读到旧缓存均失败。
fn bytes_at(
    context: &mut TestContext<'_>,
    file: &FileRef,
    offset: u64,
    expected: &[u8],
) -> TestResult {
    let mut bytes = [0xa7; 4096];
    assert!(expected.len() <= bytes.len());
    context.expect_eq(
        checked(context, file.read_at(offset, &mut bytes[..expected.len()]))?,
        expected.len(),
    );
    context.expect_eq(&bytes[..expected.len()], expected);
    Ok(())
}

/// stat 和当前记录须交付本次已接受写入的真实时间，未覆盖扩展字段的格式仅比较整秒。
fn modified_between(
    context: &mut TestContext<'_>,
    file: &FileRef,
    inode: &InodeRef,
    earlier: u64,
    later: u64,
) -> TestResult {
    let stat = checked(context, file.stat())?;
    let record = checked(context, inode.filesystem().load_inode(inode.number()))?;
    for (modified, recorded, extra) in [
        (stat.mtime, record.mtime, 136),
        (stat.ctime, record.ctime, 132),
    ] {
        context.expect_eq(modified, recorded);
        let quantum = if extra + 4 <= 128 + record.extra_isize {
            1
        } else {
            1_000_000_000
        };
        let first = earlier / quantum * quantum;
        let last = later / quantum * quantum;
        let lower = (
            (first / 1_000_000_000) as i64,
            (first % 1_000_000_000) as u32,
        );
        let upper = ((last / 1_000_000_000) as i64, (last % 1_000_000_000) as u32);
        let actual = (modified.seconds, modified.nanoseconds);
        context.expect_eq(lower <= actual && actual <= upper, true);
    }
    Ok(())
}

/// VM 错误保原原因，用户输入不经测试专用页或旁路复制。
fn vm<T>(context: &TestContext<'_>, result: Result<T, VmError>) -> Result<T, TestError> {
    result.map_err(|error| {
        context.diagnostic(format_args!("write-vm error={:?}", error));
        TestError("write VM operation failed")
    })
}

/// 每份别名有独立 Mm 和 VA，文件页偏移由本例实际准备的范围决定。
fn shared_alias(
    context: &TestContext<'_>,
    file: &FileRef,
    offset: u64,
) -> Result<Arc<Mm>, TestError> {
    let mm = Arc::try_new(vm(context, Mm::try_new().map_err(VmError::from))?)
        .map_err(|_| TestError("shared alias MM owner OOM"))?;
    vm(
        context,
        mm.map_file(SHARED_ALIAS, 8192, RW, RW, file.clone(), offset, false),
    )?;
    vm(context, mm.fault(SHARED_ALIAS, UserAccess::Read))?;
    Ok(mm)
}

/// 完整复制的字节与独立输入比较，PFN 诊断不能替代实际内容。
fn mapped_bytes(context: &mut TestContext<'_>, mm: &Mm, address: usize, expected: &[u8]) {
    let mut bytes = [0xa7; 4096];
    context.expect_eq(
        mm.copyin(&mut bytes[..expected.len()], address).copied,
        expected.len(),
    );
    context.expect_eq(&bytes[..expected.len()], expected);
}

/// 位图只在 writer 已停稳后读取；未初始化组仍使用正式描述符合法计数。
fn allocation_counts(
    context: &mut TestContext<'_>,
    volume: &Ext4Volume,
) -> Result<(u64, u64), TestError> {
    let g = volume.geometry();
    let cache = volume.metadata_cache();
    let superblock = checked(
        context,
        cache
            .get(1024 / g.block_size as u64, g.block_size)
            .map_err(FsError::Io),
    )?;
    let decoded = checked(
        context,
        superblock
            .with_read(|bytes| {
                let at = 1024 % g.block_size;
                let b = &bytes[at..at + 1024];
                Ok((
                    u64::from(codec::le32(b, 12)?)
                        | if g.features_incompat & 0x80 != 0 {
                            u64::from(codec::le32(b, 344)?) << 32
                        } else {
                            0
                        },
                    u64::from(codec::le32(b, 16)?),
                ))
            })
            .map_err(FsError::Io),
    )?;
    let super_free = checked(context, decoded)?;
    drop(superblock);
    let mut sum = (0, 0);
    for group in 0..g.groups_count {
        let byte = group * g.descriptor_size as u64;
        let descriptor = checked(
            context,
            cache
                .get(
                    g.first_data_block + 1 + byte / g.block_size as u64,
                    g.block_size,
                )
                .map_err(FsError::Io),
        )?;
        let mut record = [0u8; 64];
        checked(
            context,
            descriptor
                .with_read(|bytes| {
                    let at = (byte % g.block_size as u64) as usize;
                    record[..g.descriptor_size].copy_from_slice(&bytes[at..at + g.descriptor_size]);
                })
                .map_err(FsError::Io),
        )?;
        drop(descriptor);
        let layout = checked(
            context,
            codec::group_layout(g, group, &record[..g.descriptor_size]),
        )?;
        let state = checked(context, codec::group_state(g, &record[..g.descriptor_size]))?;
        sum.0 += u64::from(state.free_blocks);
        sum.1 += u64::from(state.free_inodes);
        for (inode, physical, uninitialized, expected, total) in [
            (
                false,
                layout.block_bitmap,
                state.flags & 2 != 0,
                u64::from(state.free_blocks),
                core::cmp::min(
                    g.blocks_per_group,
                    g.blocks_count - (g.first_data_block + group * g.blocks_per_group),
                ),
            ),
            (
                true,
                layout.inode_bitmap,
                state.flags & 1 != 0,
                u64::from(state.free_inodes),
                core::cmp::min(
                    g.inodes_per_group,
                    g.inodes_count - group * g.inodes_per_group,
                ),
            ),
        ] {
            if uninitialized {
                continue;
            }
            let bitmap = checked(
                context,
                cache.get(physical, g.block_size).map_err(FsError::Io),
            )?;
            let decoded = checked(
                context,
                bitmap
                    .with_read(|bytes| {
                        codec::verify_bitmap(g, &record[..g.descriptor_size], bytes, inode)?;
                        Ok((0..total)
                            .filter(|bit| bytes[*bit as usize / 8] & (1 << (*bit % 8)) == 0)
                            .count() as u64)
                    })
                    .map_err(FsError::Io),
            )?;
            let free = checked(context, decoded)?;
            context.expect_eq(free, expected);
        }
    }
    context.expect_eq(sum, super_free);
    context.diagnostic(format_args!(
        "write-allocation free-blocks={} free-inodes={} bitmap-count-crc=1",
        sum.0, sum.1
    ));
    Ok(sum)
}

#[derive(Default, Debug)]
struct Tree {
    data: u64,
    nodes: u64,
    extents: usize,
    depth: u16,
    blocks: Vec<u64>,
}

/// 每个连接到树的物理块必须有自己的真实位图位，不能只靠总计数相等。
fn account_block(volume: &Ext4Volume, physical: u64, tree: &mut Tree) -> Result<(), FsError> {
    let g = volume.geometry();
    g.validate_blocks(physical, 1)?;
    let group = (physical - g.first_data_block) / g.blocks_per_group;
    let bit = (physical - g.first_data_block) % g.blocks_per_group;
    let byte = group * g.descriptor_size as u64;
    let descriptor = volume.metadata_cache().get(
        g.first_data_block + 1 + byte / g.block_size as u64,
        g.block_size,
    )?;
    let mut record = [0u8; 64];
    descriptor.with_read(|bytes| {
        let at = (byte % g.block_size as u64) as usize;
        record[..g.descriptor_size].copy_from_slice(&bytes[at..at + g.descriptor_size]);
    })?;
    drop(descriptor);
    let layout = codec::group_layout(g, group, &record[..g.descriptor_size])?;
    let bitmap = volume
        .metadata_cache()
        .get(layout.block_bitmap, g.block_size)?;
    let used = bitmap.with_read(|bytes| bytes[bit as usize / 8] & (1 << (bit % 8)) != 0)?;
    if !used || tree.blocks.capacity() == tree.blocks.len() {
        return Err(FsError::Corrupt);
    }
    tree.blocks.push(physical);
    Ok(())
}

/// 实际读取每个 extent 节点并消费正式 CRC，不以逻辑块扫描代替 grow/split 的树证据。
fn extent_tree(
    volume: &Ext4Volume,
    inode: &InodeData,
    node: &[u8],
    header: codec::ExtentHeader,
    tree: &mut Tree,
) -> Result<(), FsError> {
    for index in 0..header.entries as usize {
        let at = 12 + index * 12;
        if header.depth == 0 {
            let count = codec::extent_length(codec::le16(node, at + 4)?)?.0;
            let physical = u64::from(codec::le32(node, at + 8)?)
                | (u64::from(codec::le16(node, at + 6)?) << 32);
            for block in physical..physical + count {
                account_block(volume, block, tree)?;
            }
            tree.data += count;
            tree.extents += 1;
        } else {
            let physical = u64::from(codec::le32(node, at + 4)?)
                | (u64::from(codec::le16(node, at + 8)?) << 32);
            account_block(volume, physical, tree)?;
            let buffer = volume.metadata_block(physical)?;
            let mut bytes = [0u8; 4096];
            buffer
                .with_read(|data| bytes[..data.len()].copy_from_slice(data))
                .map_err(FsError::Io)?;
            drop(buffer);
            let child = codec::extent_header(
                &bytes[..volume.geometry().block_size],
                Some(header.depth - 1),
            )?;
            codec::verify_extent_checksum(
                volume.geometry(),
                inode,
                &bytes[..volume.geometry().block_size],
                child,
            )?;
            tree.nodes += 1;
            extent_tree(
                volume,
                inode,
                &bytes[..volume.geometry().block_size],
                child,
                tree,
            )?;
        }
    }
    Ok(())
}

/// 传统指针树逐个访问实际节点，空指针不计块，所有层的节点都计入 i_blocks。
fn indirect_tree(
    volume: &Ext4Volume,
    physical: u64,
    depth: u16,
    tree: &mut Tree,
) -> Result<(), FsError> {
    if physical == 0 {
        return Ok(());
    }
    account_block(volume, physical, tree)?;
    if depth == 0 {
        tree.data += 1;
        return Ok(());
    }
    let buffer = volume.metadata_block(physical)?;
    let mut bytes = [0u8; 4096];
    buffer
        .with_read(|data| bytes[..data.len()].copy_from_slice(data))
        .map_err(FsError::Io)?;
    drop(buffer);
    tree.nodes += 1;
    for at in (0..volume.geometry().block_size).step_by(4) {
        indirect_tree(volume, u64::from(codec::le32(&bytes, at)?), depth - 1, tree)?;
    }
    Ok(())
}

/// inode 本身的 CRC、实际根格式和全部外部节点共同决定数据/节点计数。
fn tree(context: &mut TestContext<'_>, inode: &InodeRef) -> Result<Tree, TestError> {
    let volume = inode.filesystem();
    let data = checked(context, volume.load_inode(inode.number()))?;
    let mut tree = Tree::default();
    tree.blocks
        .try_reserve_exact((data.blocks * 512 / volume.geometry().block_size as u64) as usize)
        .map_err(|_| TestError("write tree observations OOM"))?;
    if data.flags & 0x80000 != 0 {
        let header = checked(context, codec::extent_header(&data.block, None))?;
        tree.depth = header.depth;
        checked(
            context,
            extent_tree(volume, &data, &data.block, header, &mut tree),
        )?;
    } else {
        for index in 0..15 {
            checked(
                context,
                indirect_tree(
                    volume,
                    u64::from(checked(context, codec::le32(&data.block, index * 4))?),
                    if index < 12 { 0 } else { (index - 11) as u16 },
                    &mut tree,
                ),
            )?;
        }
    }
    tree.blocks.sort_unstable();
    context.expect_eq(tree.blocks.windows(2).all(|pair| pair[0] != pair[1]), true);
    context.expect_eq(
        data.blocks,
        (tree.data + tree.nodes) * volume.geometry().block_size as u64 / 512,
    );
    context.expect_eq(
        checked(context, inode.metadata_snapshot())?.blocks,
        data.blocks,
    );
    context.diagnostic(format_args!(
        "write-tree inode={} data={} nodes={} extents={} depth={} i-blocks={}",
        inode.number(),
        tree.data,
        tree.nodes,
        tree.extents,
        tree.depth,
        data.blocks
    ));
    Ok(tree)
}

/// 真实 PagePressure 只占物理页，堆类别不耗尽，控制槽在施压之前完成预留。
struct PagePressure {
    pages: Vec<mm::OwnedPages>,
}
impl PagePressure {
    /// 固定容量记录原页凭证，压力期间不分配控制对象。
    fn prepare() -> Result<Self, TestError> {
        let mut pages = Vec::new();
        pages
            .try_reserve_exact(mm::stats().present_pages)
            .map_err(|_| TestError("write page pressure slots OOM"))?;
        Ok(Self { pages })
    }
    /// 原 allocator 的真实 None 是页耗尽证据；放压由原凭证 Drop 归还。
    fn exhaust(&mut self) -> bool {
        mm::shrink_all();
        mm::drain_all_pages();
        for order in [10, 0] {
            while let Some(page) = mm::alloc_pages(order, false) {
                assert!(self.pages.len() < self.pages.capacity());
                self.pages.push(page);
            }
        }
        mm::drain_all_pages();
        mm::alloc_pages(0, false).is_none()
    }
}

#[ktest::suite(fixture = WriteOwner, suite_resources = DeviceInventory, context = "Thread", tags("fs-write", "fs-path", "fs-oom"), resources("root"), timeout_ms = 180000)]
pub(crate) mod writes {
    use super::*;

    /// 源故障只接受真实前缀；写提交更新时间，共享只读尾页继续观察覆盖与同页增长。
    #[test]
    #[ktest(tags("W04"))]
    fn kernel_user_prefix_and_same_file_source(
        context: &mut TestContext<'_>,
        fixture: &mut WriteOwner,
    ) -> TestResult {
        fixture.prepare(context)?;
        let file = fixture.open(context, b"/test/write-kernel", 2)?;
        let inode = fixture.inodes.last().unwrap().clone();
        let first_write = time::now().as_nanos();
        let input = [0x39; 1027];
        context.expect_eq(checked(context, file.write_at(4093, &input))?, input.len());
        modified_between(context, &file, &inode, first_write, time::now().as_nanos())?;
        context.expect_eq(checked(context, file.stat())?.size, 5120);
        bytes_at(context, &file, 0, &[0; 4093])?;
        bytes_at(context, &file, 4093, &input)?;
        context.expect_eq(checked(context, file.seek(SeekFrom::Current(0)))?, 0);
        let address_space = vm(context, Mm::try_new().map_err(VmError::from))?;
        vm(context, address_space.map_anonymous(SOURCE, 4096, RW, RW))?;
        context.expect_eq(address_space.copyout(SOURCE + 4079, &[0x72; 17]).copied, 17);
        let mut source = checked(
            context,
            WriteBuffer::user(&address_space, SOURCE + 4079, 64),
        )?;
        let prefix_write = time::now().as_nanos();
        context.expect_eq(checked(context, file.write_from(&mut source))?, 17);
        modified_between(context, &file, &inode, prefix_write, time::now().as_nanos())?;
        context.expect_eq(checked(context, file.seek(SeekFrom::Current(0)))?, 17);
        context.expect_eq(checked(context, file.stat())?.size, 5120);
        bytes_at(context, &file, 0, &[0x72; 17])?;
        let mut source = checked(
            context,
            WriteBuffer::user(&address_space, SOURCE + 4096, 64),
        )?;
        let before_fault = checked(context, file.stat())?;
        context.expect_eq(file.write_from(&mut source), Err(FsError::BadAddress));
        context.expect_eq(checked(context, file.seek(SeekFrom::Current(0)))?, 17);
        let after_fault = checked(context, file.stat())?;
        context.expect_eq(
            (after_fault.mtime, after_fault.ctime),
            (before_fault.mtime, before_fault.ctime),
        );
        let shared = vm(context, Mm::try_new().map_err(VmError::from))?;
        vm(
            context,
            shared.map_file(SOURCE, 8192, RO, RO, file.clone(), 0, false),
        )?;
        let mut visible = [0u8; 1];
        context.expect_eq(shared.copyin(&mut visible, SOURCE + 4096).copied, 1);
        context.expect_eq(visible, [0x39]);
        context.expect_eq(checked(context, file.write_at(4096, &[0x64]))?, 1);
        bytes_at(context, &file, 4096, &[0x64])?;
        context.expect_eq(shared.copyin(&mut visible, SOURCE + 4096).copied, 1);
        context.expect_eq(visible, [0x64]);
        context.expect_eq(shared.copyin(&mut visible, SOURCE + 6000).copied, 1);
        context.expect_eq(visible, [0]);
        context.expect_eq(checked(context, file.write_at(6000, &[0x42]))?, 1);
        context.expect_eq(shared.copyin(&mut visible, SOURCE + 6000).copied, 1);
        context.expect_eq(visible, [0x42]);
        drop(shared);
        let mapped = fixture.open(context, b"/test/write-map", 2)?;
        let mm = vm(context, Mm::try_new().map_err(VmError::from))?;
        vm(
            context,
            mm.map_file(SOURCE, 8192, RO, RO, mapped.clone(), 0, true),
        )?;
        // 输入页未预触；write_from 内部 pull 必须允许同 inode 的真实 filefault。
        let mut source = checked(context, WriteBuffer::user(&mm, SOURCE + 4093, 1027))?;
        context.expect_eq(
            checked(context, mapped.write_at_from(8192, &mut source))?,
            1027,
        );
        let mut expected = [0u8; 1027];
        for (index, byte) in expected.iter_mut().enumerate() {
            *byte = ((4093 + index) & 255) as u8;
        }
        bytes_at(context, &mapped, 8192, &expected)?;
        context.expect_eq(checked(context, mapped.stat())?.size, 9219);
        drop(source);
        drop(mm);
        drop(mapped);
        fixture.run(context, b"w")
    }

    /// 两独立打开在同一因果 gate 后竞争 EOF，记录不能覆盖；dup 与 APPEND pwrite 保各自位置协议。
    #[test]
    #[ktest(tags("W05"), min_cpus = 2)]
    fn append_race_dup_and_pwrite(
        context: &mut TestContext<'_>,
        fixture: &mut WriteOwner,
    ) -> TestResult {
        fixture.prepare(context)?;
        let first = fixture.open(context, b"/test/write-append-kernel", 2 | 0x400)?;
        let second = fixture.open(context, b"/test/write-append-kernel", 2 | 0x400)?;
        context.expect_eq(Arc::ptr_eq(&first, &second), false);
        let gate = Arc::try_new(Completion::new()).map_err(|_| TestError("append gate OOM"))?;
        fixture.append_gate = Some(gate.clone());
        fixture.user.path.owner.publish(
            0,
            Job::Append {
                file: first.clone(),
                gate: gate.clone(),
                marker: 0x51,
                records: 32,
            },
        );
        fixture.user.path.owner.publish(
            1,
            Job::Append {
                file: second.clone(),
                gate: gate.clone(),
                marker: 0x92,
                records: 32,
            },
        );
        until("append writers did not block at causal gate", || {
            fixture.user.path.owner.observer().blocked(0)
                && fixture.user.path.owner.observer().blocked(1)
        })?;
        gate.complete();
        gate.complete();
        fixture.user.path.owner.join_tasks()?;
        for index in 0..2 {
            let result = fixture.user.path.owner.observer().results.lock()[index]
                .take()
                .ok_or(TestError("append result missing"))?;
            let Returned::Written(count) = checked(context, result)? else {
                return Err(TestError("append result type"));
            };
            context.expect_eq(count, 16384);
        }
        context.expect_eq(checked(context, first.stat())?.size, 32768);
        let mut seen = [0u32; 2];
        let mut bytes = [0u8; 512];
        for record in 0..64 {
            context.expect_eq(
                checked(context, first.read_at(record * 512, &mut bytes))?,
                512,
            );
            let writer = match bytes[0] {
                0x51 => 0,
                0x92 => 1,
                _ => return Err(TestError("append invalid record")),
            };
            let index = bytes[1] as usize;
            if 32 <= index
                || seen[writer] & (1 << index) != 0
                || !bytes[2..].iter().all(|byte| *byte == bytes[0])
            {
                return Err(TestError("append overlap or partial record"));
            }
            seen[writer] |= 1 << index;
        }
        context.expect_eq(seen, [u32::MAX; 2]);
        let table = checked(context, FileTable::try_new(4))?;
        let fd =
            checked(context, table.reserve())?.publish(first.clone(), DescriptorFlags::default());
        let dup = checked(context, table.dup(fd))?;
        context.expect_eq(
            Arc::ptr_eq(&checked(context, table.lookup(dup))?, &first),
            true,
        );
        let cursor = checked(context, first.seek(SeekFrom::Current(0)))?;
        context.expect_eq(checked(context, table.write_at(dup, 0, b"positioned"))?, 10);
        context.expect_eq(checked(context, first.seek(SeekFrom::Current(0)))?, cursor);
        bytes_at(context, &second, 32768, b"positioned")?;
        context.expect_eq(checked(context, table.write(dup, b"shared"))?, 6);
        context.expect_eq(checked(context, first.seek(SeekFrom::Current(0)))?, 32784);
        fixture.run(context, b"a")
    }

    /// 真实页和全部堆类别不足保 NoMemory，暖成功前缀保留，冷失败不得泄漏盘块。
    #[test]
    #[ktest(tags("W06"))]
    fn real_heap_page_oom_and_unwritten_zero(
        context: &mut TestContext<'_>,
        fixture: &mut WriteOwner,
    ) -> TestResult {
        fixture.prepare(context)?;
        let file = fixture.open(context, b"/test/write-oom", 2)?;
        context.expect_eq(checked(context, file.write(&[0x41; 4096]))?, 4096);
        let volume = fixture.volume();
        let before = allocation_counts(context, &volume)?;
        let unpublished = checked(context, volume.allocate_block(0))?;
        let allocated = allocation_counts(context, &volume);
        volume.release_unpublished_block(unpublished);
        let allocated = allocated?;
        context.expect_eq(allocated, (before.0 - 1, before.1));
        let restored = allocation_counts(context, &volume)?;
        context.expect_eq(restored, before);
        let unpublished = checked(context, volume.allocate_inode(0, 0o100600, 123, 234))?;
        let record = volume.load_inode(unpublished.number());
        let allocated = allocation_counts(context, &volume);
        volume.release_unpublished_inode(unpublished);
        let record = checked(context, record)?;
        context.expect_eq(
            (
                record.mode,
                record.uid,
                record.gid,
                record.size,
                record.blocks,
            ),
            (0o100600, 123, 234, 0, 0),
        );
        context.expect_eq(allocated?, (before.0, before.1 - 1));
        let restored = allocation_counts(context, &volume)?;
        context.expect_eq(restored, before);
        let mut page_pressure = PagePressure::prepare()?;
        let (evidence, page_error) = {
            let evidence = {
                let _pin = PreemptGuard::new();
                page_pressure.exhaust()
            };
            let error = file.write_at(4096, &[0x9b; 17]);
            drop(page_pressure);
            (evidence, error)
        };
        context.expect_eq(evidence, true);
        context.expect_eq(page_error, Err(FsError::NoMemory));
        let after_page_failure = allocation_counts(context, &volume)?;
        context.expect_eq(after_page_failure, before);
        let pressure = fixture
            .user
            .path
            .owner
            .pressure
            .take()
            .ok_or(TestError("write heap pressure missing"))?;
        fixture.user.path.owner.publish(
            0,
            Job::WriteOom {
                file: file.clone(),
                pressure,
            },
        );
        fixture.user.path.owner.join_tasks()?;
        let result = fixture.user.path.owner.observer().results.lock()[0]
            .take()
            .ok_or(TestError("write OOM result missing"))?;
        let Returned::WriteOom {
            evidence,
            warm,
            cold,
        } = checked(context, result)?
        else {
            return Err(TestError("write OOM result type"));
        };
        context.expect_eq(evidence, (true, true));
        context.expect_eq(warm, Ok(17));
        context.expect_eq(cold, Err(FsError::NoMemory));
        let after_heap_failure = allocation_counts(context, &volume)?;
        context.expect_eq(after_heap_failure, before);
        context.expect_eq(checked(context, file.stat())?.size, 4096);
        bytes_at(context, &file, 0, &[0x6d; 17])?;
        context.expect_eq(checked(context, file.write_at(4096, &[0x9b; 17]))?, 17);
        let unwritten = fixture.open(context, b"/test/write-unwritten", 2)?;
        context.expect_eq(checked(context, unwritten.write_at(513, &[0x63; 17]))?, 17);
        let mut expected = [0u8; 4096];
        expected[513..530].fill(0x63);
        bytes_at(context, &unwritten, 0, &expected)?;
        bytes_at(context, &unwritten, 4096, &[0; 4096])?;
        context.expect_eq(checked(context, unwritten.stat())?.size, 16384);
        Ok(())
    }

    /// 正常近满盘由真实分配耗尽，跨剩余块请求返回正前缀，下一次保 ENOSPC 和当前位置。
    #[test]
    #[ktest(tags("W06", "fs-write-enospc"))]
    fn real_enospc_keeps_accepted_prefix(
        context: &mut TestContext<'_>,
        fixture: &mut WriteOwner,
    ) -> TestResult {
        fixture.prepare(context)?;
        let volume = fixture.volume();
        let initial = allocation_counts(context, &volume)?;
        if initial.0 < 8 || 256 < initial.0 {
            return Err(TestError(
                "ENOSPC fixture must have 8..256 genuinely free blocks",
            ));
        }
        let rollback = fixture.open(context, b"/test/write-rollback", 2)?;
        let rollback_inode = fixture.inodes.last().unwrap().clone();
        let block_size = volume.geometry().block_size as u64;
        for logical in [0, 2, 4, 6] {
            context.expect_eq(
                checked(context, rollback.write_at(logical * block_size, &[0x5d]))?,
                1,
            );
        }
        let original_tree = tree(context, &rollback_inode)?;
        context.expect_eq(original_tree.extents, 4);
        let metadata = checked(context, rollback_inode.metadata_snapshot())?;
        let root = checked(context, rollback_inode.with_map_read(|_, root| Ok(*root)))?;
        let available = allocation_counts(context, &volume)?;
        let mut held = Vec::new();
        held.try_reserve_exact(available.0 as usize)
            .map_err(|_| TestError("ENOSPC reservation slots OOM"))?;
        for _ in 1..available.0 {
            match volume.allocate_block(0) {
                Ok(block) => held.push(block),
                Err(error) => {
                    for block in held.drain(..) {
                        volume.release_unpublished_block(block);
                    }
                    context.diagnostic(format_args!(
                        "ENOSPC reservation preparation error={:?}",
                        error
                    ));
                    return Err(TestError("ENOSPC reservation preparation failed"));
                }
            }
        }
        // 四项根的第五个稀疏数据块会先申请唯一余块，再因外部节点无空间而归还。
        let single_before = allocation_counts(context, &volume);
        let failed = rollback.write_at(8 * block_size, &[0x28]);
        let single_after = allocation_counts(context, &volume);
        let metadata_after = rollback_inode.metadata_snapshot();
        let root_after = rollback_inode.with_map_read(|_, root| Ok(*root));
        for block in held.drain(..) {
            volume.release_unpublished_block(block);
        }
        context.expect_eq(single_before?, (1, available.1));
        context.expect_eq(single_after?, (1, available.1));
        context.expect_eq(failed, Err(FsError::NoSpace));
        context.expect_eq(checked(context, metadata_after)?, metadata);
        context.expect_eq(checked(context, root_after)?, root);
        for logical in [0, 2, 4, 6] {
            bytes_at(context, &rollback, logical * block_size, &[0x5d])?;
        }
        context.expect_eq(checked(context, rollback.seek(SeekFrom::Current(0)))?, 0);
        let mut untouched = [0xa7; 17];
        context.expect_eq(
            checked(context, rollback.read_at(8 * block_size, &mut untouched))?,
            0,
        );
        context.expect_eq(untouched, [0xa7; 17]);
        let restored = allocation_counts(context, &volume)?;
        context.expect_eq(restored, available);
        let file = fixture.open(context, b"/test/write-full", 2)?;
        let full_inode = fixture.inodes.last().unwrap().clone();
        let before = allocation_counts(context, &volume)?;
        let mut request = Vec::new();
        let length = (before.0 as usize + 1) * volume.geometry().block_size;
        request
            .try_reserve_exact(length)
            .map_err(|_| TestError("ENOSPC input OOM"))?;
        request.resize(length, 0x87);
        let count = checked(context, file.write(&request))?;
        context.expect_eq(0 < count && count < request.len(), true);
        context.expect_eq(
            checked(context, file.seek(SeekFrom::Current(0)))?,
            count as u64,
        );
        context.expect_eq(checked(context, file.stat())?.size, count as u64);
        context.expect_eq(file.write(&[0x18; 512]), Err(FsError::NoSpace));
        context.expect_eq(
            checked(context, file.seek(SeekFrom::Current(0)))?,
            count as u64,
        );
        for offset in (0..count).step_by(4096) {
            bytes_at(
                context,
                &file,
                offset as u64,
                &request[offset..core::cmp::min(count, offset + 4096)],
            )?;
        }
        let after = allocation_counts(context, &volume)?;
        context.expect_eq(after.0, 0);
        let tree = tree(context, &full_inode)?;
        context.expect_eq(before.0 - after.0, tree.data + tree.nodes);
        context.expect_eq(before.1, after.1);
        Ok(())
    }

    /// 稀疏插入使实际树增长和分裂；合并归还外部节点后，新文件重用原块仍保完整盘面字节。
    #[test]
    #[ktest(tags("W07", "fs-write-extents"))]
    fn extent_grow_split_merge_and_allocation(
        context: &mut TestContext<'_>,
        fixture: &mut WriteOwner,
    ) -> TestResult {
        fixture.prepare(context)?;
        let file = fixture.open(context, b"/test/write-tree", 2)?;
        let inode = fixture.inodes.last().unwrap().clone();
        let volume = fixture.volume();
        let block_size = volume.geometry().block_size;
        let before = allocation_counts(context, &volume)?;
        let entries = ((block_size - 16) / 12) * 4 + 1;
        let data = [0x4e; 4096];
        for index in 0..entries {
            context.expect_eq(
                checked(
                    context,
                    file.write_at((index * 2 * block_size) as u64, &data[..block_size]),
                )?,
                block_size,
            );
            if index == 4 {
                let grown = tree(context, &inode)?;
                context.expect_eq(grown.depth, 1);
            }
        }
        let grown = tree(context, &inode)?;
        context.expect_eq(grown.depth, 2);
        context.expect_eq(grown.extents, entries);
        context.expect_eq(grown.data, entries as u64);
        let after = allocation_counts(context, &volume)?;
        context.expect_eq(before.0 - after.0, grown.data + grown.nodes);
        context.expect_eq(before.1, after.1);
        for index in 0..entries {
            bytes_at(
                context,
                &file,
                (index * 2 * block_size) as u64,
                &data[..block_size],
            )?;
            if index + 1 < entries {
                bytes_at(
                    context,
                    &file,
                    ((index * 2 + 1) * block_size) as u64,
                    &[0; 4096][..block_size],
                )?;
            }
            context.expect_eq(
                matches!(
                    checked(context, inode.map_block((index * 2) as u64))?,
                    BlockMapping::Mapped { .. }
                ),
                true,
            );
        }
        let merge = fixture.open(context, b"/test/write-merge", 2)?;
        let merge_inode = fixture.inodes.last().unwrap().clone();
        let original = tree(context, &merge_inode)?;
        context.expect_eq(
            checked(
                context,
                merge.write_at(block_size as u64, &data[..block_size]),
            )?,
            block_size,
        );
        let split = tree(context, &merge_inode)?;
        context.expect_eq(split.extents, original.extents + 2);
        context.expect_eq(
            checked(context, merge.write_at(0, &data[..block_size]))?,
            block_size,
        );
        context.expect_eq(
            checked(
                context,
                merge.write_at((2 * block_size) as u64, &data[..block_size]),
            )?,
            block_size,
        );
        let merged = tree(context, &merge_inode)?;
        context.expect_eq(merged.extents, original.extents + 1);
        context.expect_eq((merged.data, merged.nodes), (original.data, original.nodes));
        let merge_before = allocation_counts(context, &volume)?;
        context.expect_eq(original.data, 64);
        for logical in (3..8).step_by(2) {
            context.expect_eq(
                checked(
                    context,
                    merge.write_at((logical * block_size) as u64, &data[..block_size]),
                )?,
                block_size,
            );
        }
        let first_split = tree(context, &merge_inode)?;
        context.expect_eq((first_split.depth, first_split.nodes), (1, 2));
        for logical in (9..64).step_by(2) {
            context.expect_eq(
                checked(
                    context,
                    merge.write_at((logical * block_size) as u64, &data[..block_size]),
                )?,
                block_size,
            );
        }
        let expanded = tree(context, &merge_inode)?;
        context.expect_eq(expanded.depth, 1);
        context.expect_eq(0 < expanded.nodes, true);
        context.expect_eq(expanded.data, original.data);
        let split_counts = allocation_counts(context, &volume)?;
        context.expect_eq(merge_before.0 - split_counts.0, expanded.nodes);
        for logical in (4..64).step_by(2) {
            context.expect_eq(
                checked(
                    context,
                    merge.write_at((logical * block_size) as u64, &data[..block_size]),
                )?,
                block_size,
            );
        }
        let collapsed = tree(context, &merge_inode)?;
        context.expect_eq(
            (collapsed.depth, collapsed.nodes, collapsed.extents),
            (0, 0, 1),
        );
        context.expect_eq(collapsed.data, original.data);
        let collapsed_counts = allocation_counts(context, &volume)?;
        context.expect_eq(collapsed_counts, merge_before);
        let reused = fixture.open(context, b"/test/write-rollback", 2)?;
        let reused_inode = fixture.inodes.last().unwrap().clone();
        let replacement = [0xb9; 4096];
        for (logical, physical) in first_split
            .blocks
            .iter()
            .filter(|physical| !collapsed.blocks.contains(physical))
            .enumerate()
        {
            context.expect_eq(
                checked(
                    context,
                    reused.write_at((logical * block_size) as u64, &replacement[..block_size]),
                )?,
                block_size,
            );
            let BlockMapping::Mapped {
                physical: actual, ..
            } = checked(context, reused_inode.map_block(logical as u64))?
            else {
                return Err(TestError("retired extent node was not reused as file data"));
            };
            context.expect_eq(actual, *physical);
            context.diagnostic(format_args!(
                "write-retired-node-reuse physical={} logical={}",
                physical, logical
            ));
        }
        for inode in &fixture.inodes {
            checked(context, crate::fs::writeback::sync_inode(inode))?;
        }
        checked(context, crate::fs::writeback::sync_volume(&volume))?;
        let mut device_bytes = [0u8; 4096];
        for physical in first_split
            .blocks
            .iter()
            .filter(|physical| !collapsed.blocks.contains(physical))
        {
            checked(
                context,
                volume
                    .claim()
                    .disk()
                    .read_exact_at(
                        *physical * block_size as u64,
                        &mut device_bytes[..block_size],
                    )
                    .map_err(FsError::Io),
            )?;
            context.expect_eq(&device_bytes[..block_size], &replacement[..block_size]);
        }
        for logical in 0..64 {
            bytes_at(
                context,
                &merge,
                (logical * block_size) as u64,
                &data[..block_size],
            )?;
        }
        Ok(())
    }

    /// 传统缺链由普通写建立，数据与节点计数准确，128B inode 的修改时间按整秒编码。
    #[test]
    #[ktest(tags("W07", "fs-write-traditional"))]
    fn traditional_direct_single_double_triple(
        context: &mut TestContext<'_>,
        fixture: &mut WriteOwner,
    ) -> TestResult {
        fixture.prepare(context)?;
        let file = fixture.open(context, b"/test/write-tree", 2)?;
        let inode = fixture.inodes.last().unwrap().clone();
        let volume = fixture.volume();
        context.expect_eq(volume.geometry().block_size, 1024);
        context.expect_eq(
            checked(context, inode.metadata_snapshot())?.flags & 0x80000,
            0,
        );
        let before = allocation_counts(context, &volume)?;
        let first_write = time::now().as_nanos();
        for (index, logical) in [0u64, 12, 268, 65804].iter().copied().enumerate() {
            let bytes = [0x31 + index as u8; 17];
            context.expect_eq(
                checked(context, file.write_at(logical * 1024 + 513, &bytes))?,
                17,
            );
            let mut expected = [0u8; 1024];
            expected[513..530].copy_from_slice(&bytes);
            // 末块的 EOF 是字节数，先写块尾再核完整零初始化。
            context.expect_eq(
                checked(context, file.write_at(logical * 1024 + 1023, &[0]))?,
                1,
            );
            bytes_at(context, &file, logical * 1024, &expected)?;
        }
        modified_between(context, &file, &inode, first_write, time::now().as_nanos())?;
        let mapped = tree(context, &inode)?;
        context.expect_eq((mapped.data, mapped.nodes), (4, 6));
        let after = allocation_counts(context, &volume)?;
        context.expect_eq(before.0 - after.0, 10);
        context.expect_eq(before.1, after.1);
        context.expect_eq(checked(context, file.stat())?.size, (65804 + 1) * 1024);
        Ok(())
    }

    /// 真实 loader、fork 和未发表 Task 控制 deny 寿命，普通 private 文件映射不取得 deny。
    #[test]
    #[ktest(tags("W08"))]
    fn executable_deny_prepare_fork_and_private_map(
        context: &mut TestContext<'_>,
        fixture: &mut WriteOwner,
    ) -> TestResult {
        fixture.prepare(context)?;
        let program = fixture.open(context, PROGRAM_WRITE, 0)?;
        let path = checked(
            context,
            fixture.user.path.vfs().resolve(
                fixture.user.path.context(),
                PROGRAM_WRITE,
                ResolveMode::Follow,
            ),
        )?;
        let writer = checked(context, open_path_with_flags(path.clone(), 1))?;
        let args = Arguments::from_kernel(PROGRAM_WRITE, &[PROGRAM_WRITE, b"e"], &[])
            .map_err(|_| TestError("deny arguments OOM"))?;
        context.expect_eq(
            exec::load(program.clone(), &args, 0, 0).err(),
            Some(ExecError::File(FsError::TextBusy)),
        );
        drop(writer);
        let image = exec::load(program.clone(), &args, 0, 0)
            .map_err(|_| TestError("deny real ELF load failed"))?;
        context.expect_eq(
            open_path_with_flags(path.clone(), 1).err(),
            Some(FsError::TextBusy),
        );
        context.expect_eq(
            open_path_with_flags(path.clone(), 1 | 0x200).err(),
            Some(FsError::TextBusy),
        );
        let child = vm(context, image.mm.fork())?;
        let old_mm = image.mm.clone();
        drop(image);
        context.expect_eq(
            open_path_with_flags(path.clone(), 1).err(),
            Some(FsError::TextBusy),
        );
        drop(old_mm);
        context.expect_eq(
            open_path_with_flags(path.clone(), 1).err(),
            Some(FsError::TextBusy),
        );
        drop(child);
        drop(checked(context, open_path_with_flags(path.clone(), 1))?);
        let mm = vm(context, Mm::try_new().map_err(VmError::from))?;
        vm(
            context,
            mm.map_file(SOURCE, 4096, RO, RO, program.clone(), 0, true),
        )?;
        drop(checked(context, open_path_with_flags(path.clone(), 1))?);
        let bad = fixture.open(context, b"/test/write-map", 0)?;
        context.expect_eq(
            exec::load(bad.clone(), &args, 0, 0).err(),
            Some(ExecError::Format),
        );
        drop(fixture.open(context, b"/test/write-map", 1)?);
        let image = exec::load(program.clone(), &args, 0, 0)
            .map_err(|_| TestError("prepare deny real load failed"))?;
        let runtime = checked(
            context,
            UserRuntime::try_new(
                fixture.user.path.vfs().clone(),
                fixture.user.path.context().clone(),
                Credentials::ROOT,
            ),
        )?;
        let prepared = task::prepare_program(
            fixture.user.graph(),
            None,
            image,
            runtime,
            Placement::Cpu(cpu::current().id),
        )
        .map_err(|_| TestError("unpublished executable prepare failed"))?;
        context.expect_eq(
            open_path_with_flags(path.clone(), 1).err(),
            Some(FsError::TextBusy),
        );
        drop(prepared);
        drop(checked(context, open_path_with_flags(path.clone(), 1))?);
        let image = exec::load(program.clone(), &args, 0, 0)
            .map_err(|_| TestError("OOM deny real load failed"))?;
        let runtime = checked(
            context,
            UserRuntime::try_new(
                fixture.user.path.vfs().clone(),
                fixture.user.path.context().clone(),
                Credentials::ROOT,
            ),
        )?;
        let graph = fixture.user.graph().clone();
        let pressure = fixture
            .user
            .path
            .owner
            .pressure
            .as_mut()
            .ok_or(TestError("exec pressure missing"))?;
        let (evidence, fork_error, prepare_error) = {
            let evidence = {
                let _pin = PreemptGuard::new();
                pressure.exhaust()
            };
            let fork_error = image.mm.fork().err();
            let prepare_error = task::prepare_program(
                &graph,
                None,
                image,
                runtime,
                Placement::Cpu(cpu::current().id),
            )
            .err();
            fixture.user.release_pressure();
            (evidence, fork_error, prepare_error)
        };
        context.expect_eq(evidence, (true, true));
        context.expect_eq(fork_error, Some(VmError::NoMemory));
        context.expect_eq(
            prepare_error,
            Some(crate::task::ProgramError::Identity(
                crate::task::process::ProcessError::NoMemory,
            )),
        );
        drop(checked(context, open_path_with_flags(path.clone(), 1))?);
        // loader 已取得 deny 后格式失败与真实 Mm 分配失败都必须归还原责任。
        let mut pages = PagePressure::prepare()?;
        let (evidence, load_error) = {
            let evidence = {
                let _pin = PreemptGuard::new();
                pages.exhaust()
            };
            let error = exec::load(program.clone(), &args, 0, 0).err();
            drop(pages);
            (evidence, error)
        };
        context.expect_eq(evidence, true);
        context.expect_eq(load_error, Some(ExecError::NoMemory));
        drop(checked(context, open_path_with_flags(path, 1))?);
        fixture.run(context, b"e")
    }

    /// 两个实际 Mm 的同源尾页保留双向内容；EOF 外 store 不分配，普通增长清新有效 gap。
    #[test]
    #[ktest(tags("W15", "fs-shared"), min_cpus = 2)]
    fn shared_canonical_tail_growth(
        context: &mut TestContext<'_>,
        fixture: &mut WriteOwner,
    ) -> TestResult {
        fixture.prepare(context)?;
        let file = fixture.open(context, b"/test/write-reserve", 2)?;
        let page_offset = checked(context, file.stat())?
            .size
            .div_ceil(PAGE_SIZE as u64)
            .checked_mul(PAGE_SIZE as u64)
            .ok_or(TestError("shared tail file offset overflow"))?;
        context.expect_eq(
            checked(context, file.write_at(page_offset, &[0x39; 17]))?,
            17,
        );
        let inode = fixture.inodes.last().unwrap().clone();
        let before = checked(context, file.stat())?;
        let alias = shared_alias(context, &file, page_offset)?;
        let image = fixture.shared_image(context, b"s")?;
        let mm = image.mm.clone();
        if mm
            .copyout(
                SHARED_CONTROL + SHARED_FILE_OFFSET,
                &page_offset.to_le_bytes(),
            )
            .copied
            != core::mem::size_of::<u64>()
        {
            return Err(TestError("shared file offset publication failed"));
        }
        fixture.start_shared(context, image)?;
        fixture.phase(&mm, 0, 1)?;
        let folio = checked(context, file.file_page(page_offset / PAGE_SIZE as u64))?;
        context.expect_eq(mm.mapping(SOURCE).unwrap().physical, folio.phys_addr());
        context.expect_eq(
            alias.mapping(SHARED_ALIAS).unwrap().physical,
            folio.phys_addr(),
        );
        mapped_bytes(context, &alias, SHARED_ALIAS, &[0x61]);
        mapped_bytes(context, &alias, SHARED_ALIAS + 128, &[0xde]);
        context.expect_eq(alias.copyout(SHARED_ALIAS + 1, &[0x62]).copied, 1);
        mapped_bytes(context, &mm, SOURCE + 1, &[0x62]);
        let polluted = checked(context, file.stat())?;
        context.expect_eq(
            (polluted.size, polluted.blocks),
            (before.size, before.blocks),
        );
        context.expect_eq(
            mm.fault(SOURCE + 4096, UserAccess::Read),
            Err(VmError::BeyondEof),
        );
        context.expect_eq(
            checked(context, file.write_at(page_offset + 512, &[0x73]))?,
            1,
        );
        bytes_at(context, &file, page_offset, &[0x61, 0x62])?;
        bytes_at(context, &file, page_offset + 2, &[0x39; 15])?;
        bytes_at(context, &file, page_offset + 17, &[0; 495])?;
        bytes_at(context, &file, page_offset + 512, &[0x73])?;
        context.expect_eq(mm.mapping(SOURCE).unwrap().permissions.write, false);
        context.expect_eq(
            alias.mapping(SHARED_ALIAS).unwrap().permissions.write,
            false,
        );
        let grants = mm.ktest_mkwrite_count();
        WriteOwner::release(&mm, 1)?;
        fixture.phase(&mm, 0, 2)?;
        context.expect_eq(grants < mm.ktest_mkwrite_count(), true);
        mapped_bytes(context, &alias, SHARED_ALIAS + 128, &[0x74]);
        bytes_at(context, &file, page_offset + 128, &[0x74])?;
        context.expect_eq(checked(context, file.stat())?.size, page_offset + 513);
        context.expect_eq(folio.dirty_sectors() != 0, true);
        WriteOwner::release(&mm, 2)?;
        fixture.finish_shared(context)?;
        drop((mm, alias, folio));
        checked(context, crate::fs::writeback::sync_inode(&inode))?;
        context.diagnostic(format_args!(
            "W15 canonical-cross-mm=1 tail-store-no-growth=1 growth-gap-zero=1 renewed-mkwrite=1"
        ));
        Ok(())
    }

    /// 同一远核用户先填真实 W TLB，另一 CPU 同步撤权后由原执行者再次 store。
    #[test]
    #[ktest(tags("W16", "fs-shared"), min_cpus = 2)]
    fn remote_write_tlb_mkclean_refault(
        context: &mut TestContext<'_>,
        fixture: &mut WriteOwner,
    ) -> TestResult {
        fixture.prepare(context)?;
        let file = fixture.open(context, b"/test/write-map", 2)?;
        let inode = fixture.inodes.last().unwrap().clone();
        let alias = shared_alias(context, &file, 0)?;
        context.expect_eq(alias.copyout(SHARED_ALIAS + 3, &[0x84]).copied, 1);
        let image = fixture.shared_image(context, b"r")?;
        let mm = image.mm.clone();
        fixture.start_shared(context, image)?;
        fixture.phase(&mm, 0, 1)?;
        let remote = fixture.user.running.as_ref().unwrap().target.unwrap();
        mapped_bytes(context, &alias, SHARED_ALIAS, &[0x81]);
        context.expect_eq(mm.mapping(SOURCE).unwrap().permissions.write, true);
        let grants = mm.ktest_mkwrite_count();
        let folio = checked(context, file.file_page(0))?;
        let cleaner = cpu::current().id;
        context.expect_eq(remote != cleaner, true);
        checked(context, crate::fs::writeback::sync_inode(&inode))?;
        context.expect_eq(mm.mapping(SOURCE).unwrap().permissions.write, false);
        context.expect_eq(
            alias.mapping(SHARED_ALIAS).unwrap().permissions.write,
            false,
        );
        context.expect_eq((folio.dirty_sectors(), folio.writeback_sectors()), (0, 0));
        WriteOwner::release(&mm, 1)?;
        fixture.phase(&mm, 0, 2)?;
        context.expect_eq(grants < mm.ktest_mkwrite_count(), true);
        context.expect_eq(folio.dirty_sectors() != 0, true);
        mapped_bytes(context, &alias, SHARED_ALIAS + 1, &[0x82]);
        bytes_at(context, &file, 1, &[0x82])?;
        context.diagnostic(format_args!(
            "W16 remote-cpu={} cleaner-cpu={} real-user-w-store=1 post-csd-mkwrite=1 dirty=1",
            remote, cleaner
        ));
        WriteOwner::release(&mm, 2)?;
        fixture.finish_shared(context)?;
        drop((mm, alias, folio));
        Ok(())
    }

    /// 真实 prepare 已结束且锁全部退出时 remap 同 VA，旧准备不得向新 source 发布。
    #[test]
    #[ktest(tags("W17", "fs-shared", "fs-oom"), min_cpus = 2)]
    fn prepared_mkwrite_same_address_remap(
        context: &mut TestContext<'_>,
        fixture: &mut WriteOwner,
    ) -> TestResult {
        fixture.prepare(context)?;
        let old = fixture.open(context, b"/test/write-map", 2)?;
        context.expect_eq(checked(context, old.write_at(0, &[0, 1]))?, 2);
        let old_inode = fixture.inodes.last().unwrap().clone();
        let replacement = fixture.open(context, b"/test/write-reserve", 2)?;
        context.expect_eq(checked(context, replacement.write(&[0x5a; 4096]))?, 4096);
        let replacement_folio = checked(context, replacement.file_page(0))?;
        let old_folio = checked(context, old.file_page(0))?;
        let image = fixture.shared_image(context, b"m")?;
        let mm = image.mm.clone();
        let ready =
            Arc::try_new(AtomicBool::new(false)).map_err(|_| TestError("prepare ready OOM"))?;
        let gate = Arc::try_new(Completion::new()).map_err(|_| TestError("prepare gate OOM"))?;
        fixture.prepare_gate = Some(gate.clone());
        let once = ready.clone();
        let pause = gate.clone();
        let hook: Arc<dyn Fn() + Send + Sync> = Arc::try_new(move || {
            if !once.swap(true, Ordering::AcqRel) {
                pause.wait();
            }
        })
        .map_err(|_| TestError("prepare hook OOM"))?;
        mm.ktest_set_mkwrite_hook(Some(hook));
        fixture.start_shared(context, image)?;
        until("real mkwrite prepare did not pause", || {
            ready.load(Ordering::Acquire)
        })?;
        let pid = fixture.user.running.as_ref().unwrap().pid;
        let preparer = fixture
            .user
            .graph()
            .lookup_thread(pid)
            .ok_or(TestError("mkwrite preparer binding missing"))?;
        let executor = preparer
            .executor()
            .ok_or(TestError("mkwrite preparer executor missing"))?;
        until("mkwrite preparer did not block at lock-free hook", || {
            sched::test_state(&executor) == State::Blocked
        })?;
        context.expect_eq(
            mm.mapping(SOURCE)
                .is_none_or(|leaf| !leaf.permissions.write),
            true,
        );
        vm(
            context,
            mm.mmap(MmapRequest {
                address: MmapAddress::Fixed(SOURCE),
                length: 4096,
                permissions: RW,
                maximum: RW,
                source: MmapSource::File {
                    file: replacement.clone(),
                    offset: 0,
                    private: false,
                },
            }),
        )?;
        mm.ktest_set_mkwrite_hook(None);
        gate.complete();
        fixture.prepare_gate.take();
        fixture.phase(&mm, 0, 1)?;
        context.expect_eq(
            mm.mapping(SOURCE).unwrap().physical,
            replacement_folio.phys_addr(),
        );
        bytes_at(context, &old, 0, &[0])?;
        bytes_at(context, &replacement, 0, &[0x81])?;
        context.expect_eq(old_folio.phys_addr() != replacement_folio.phys_addr(), true);
        WriteOwner::release(&mm, 1)?;
        fixture.phase(&mm, 0, 2)?;
        bytes_at(context, &replacement, 1, &[0x82])?;
        bytes_at(context, &old, 1, &[1])?;
        WriteOwner::release(&mm, 2)?;
        fixture.finish_shared(context)?;
        drop((mm, old_folio, replacement_folio));
        checked(context, crate::fs::writeback::sync_inode(&old_inode))?;

        // 暖 hole 页和页表后耗尽真实堆；prepare 失败不能把可写零页发表为成功。
        let sparse = fixture.open(context, b"/test/write-reserve", 2)?;
        let hole_offset = checked(context, sparse.stat())?
            .size
            .div_ceil(PAGE_SIZE as u64)
            .checked_mul(PAGE_SIZE as u64)
            .ok_or(TestError("mkwrite hole offset overflow"))?;
        let data_offset = hole_offset
            .checked_add(2 * PAGE_SIZE as u64)
            .ok_or(TestError("mkwrite sparse data offset overflow"))?;
        context.expect_eq(checked(context, sparse.write_at(data_offset, &[0x71]))?, 1);
        let probe = shared_alias(context, &sparse, hole_offset)?;
        let original = probe.mapping(SHARED_ALIAS).unwrap().physical;
        let pressure = fixture
            .user
            .path
            .owner
            .pressure
            .as_mut()
            .ok_or(TestError("mkwrite heap pressure missing"))?;
        let evidence = {
            let _pin = PreemptGuard::new();
            pressure.exhaust()
        };
        let error = probe.fault(SHARED_ALIAS, UserAccess::Write);
        fixture.user.release_pressure();
        context.expect_eq(evidence, (true, true));
        context.expect_eq(error, Err(VmError::File(FsError::NoMemory)));
        context.expect_eq(probe.mapping(SHARED_ALIAS).unwrap().physical, original);
        context.expect_eq(
            probe.mapping(SHARED_ALIAS).unwrap().permissions.write,
            false,
        );
        vm(context, probe.fault(SHARED_ALIAS, UserAccess::Write))?;
        context.diagnostic(format_args!("W17 real-prepare-pause=1 same-va-remap=1 old-not-committed=1 real-prepare-oom=1 truncate-tested=0"));
        Ok(())
    }

    /// 真实 fork 子叶先 RO，mprotect 只改许可，WB 完成后 copyout 才重新授权并标 Dirty。
    #[test]
    #[ktest(tags("W18", "fs-shared"), min_cpus = 2)]
    fn fork_protect_copyout_writeback_private_cow(
        context: &mut TestContext<'_>,
        fixture: &mut WriteOwner,
    ) -> TestResult {
        fixture.prepare(context)?;
        let file = fixture.open(context, b"/test/write-map", 2)?;
        let inode = fixture.inodes.last().unwrap().clone();
        let folio = checked(context, file.file_page(0))?;
        let image = fixture.shared_image(context, b"c")?;
        let mm = image.mm.clone();
        fixture.start_shared(context, image)?;
        fixture.phase(&mm, 0, 1)?;
        WriteOwner::release(&mm, 1)?;
        fixture.phase(&mm, 0, 2)?;
        fixture.phase(&mm, 8, 3)?;
        let child = fixture.child(&mm)?;
        let child_mm = child.mm().ok_or(TestError("fork child MM missing"))?;
        context.expect_eq(
            child_mm.mapping(SOURCE).unwrap().physical,
            folio.phys_addr(),
        );
        context.expect_eq(child_mm.mapping(SOURCE).unwrap().permissions.write, false);
        vm(context, child_mm.protect(SOURCE, 4096, RO))?;
        vm(context, child_mm.protect(SOURCE, 4096, RW))?;
        context.expect_eq(child_mm.mapping(SOURCE).unwrap().permissions.write, false);
        let disk = fixture.user.path.owner.disk()?.clone();
        disk.test_suspend_completions();
        let sync_inode = inode.clone();
        fixture.consumers[0] = Some(
            task::prepare_kernel_on(cpu::current().id, move || {
                if crate::fs::writeback::sync_inode(&sync_inode).is_ok() {
                    0
                } else {
                    1
                }
            })
            .map_err(|_| TestError("WB consumer prepare failed"))?
            .publish(),
        );
        until("file page did not enter real WB", || {
            folio.writeback_sectors() != 0 && disk.test_snapshot().inflight != 0
        })?;
        context.expect_eq(mm.mapping(SOURCE).unwrap().permissions.write, false);
        context.expect_eq(child_mm.mapping(SOURCE).unwrap().permissions.write, false);
        let copying = child_mm.clone();
        let observer = Arc::try_new(SpinLock::new(None::<alloc::sync::Weak<Task>>))
            .map_err(|_| TestError("copyout task observer OOM"))?;
        let record = observer.clone();
        let copy = task::prepare_kernel_on(cpu::current().id, move || {
            *record.lock() = Some(Arc::downgrade(&task::current_task()));
            let progress = copying.copyout(SOURCE + 4, &[0x85]);
            if progress.copied == 1 && progress.error.is_none() {
                0
            } else {
                1
            }
        })
        .map_err(|_| TestError("copyout consumer prepare failed"))?;
        let grants = child_mm.ktest_mkwrite_count();
        fixture.consumers[1] = Some(copy.publish());
        until("copyout did not block on real WB", || {
            let executor = observer
                .lock()
                .as_ref()
                .and_then(alloc::sync::Weak::upgrade);
            executor.is_some_and(|executor| sched::test_state(&executor) == State::Blocked)
        })?;
        context.expect_eq(child_mm.ktest_mkwrite_count(), grants);
        context.expect_eq(child_mm.mapping(SOURCE).unwrap().permissions.write, false);
        disk.test_resume_completions();
        fixture.join_consumers()?;
        context.expect_eq(folio.writeback_sectors(), 0);
        context.expect_eq(folio.dirty_sectors() != 0, true);
        context.expect_eq(grants < child_mm.ktest_mkwrite_count(), true);
        mapped_bytes(context, &mm, SOURCE + 4, &[0x85]);
        bytes_at(context, &file, 4, &[0x85])?;
        checked(context, crate::fs::writeback::sync_inode(&inode))?;
        let private = vm(context, Mm::try_new().map_err(VmError::from))?;
        vm(
            context,
            private.map_file(SHARED_ALIAS, 4096, RW, RW, file.clone(), 0, true),
        )?;
        vm(context, private.fault(SHARED_ALIAS, UserAccess::Read))?;
        context.expect_eq(
            private.mapping(SHARED_ALIAS).unwrap().physical,
            folio.phys_addr(),
        );
        context.expect_eq(private.copyout(SHARED_ALIAS + 5, &[0xef]).copied, 1);
        context.expect_eq(
            private.mapping(SHARED_ALIAS).unwrap().physical != folio.phys_addr(),
            true,
        );
        context.expect_eq(folio.dirty_sectors(), 0);
        bytes_at(context, &file, 5, &[5])?;
        mapped_bytes(context, &private, SHARED_ALIAS + 5, &[0xef]);
        WriteOwner::release(&mm, 3)?;
        until("shared fork child did not exit", || child.mm().is_none())?;
        bytes_at(context, &file, 2, &[0x83])?;
        WriteOwner::release(&mm, 4)?;
        fixture.finish_shared(context)?;
        drop((private, child_mm, child, mm, folio, disk, observer));
        context.diagnostic(format_args!("W18 fork-child-ro=1 protect-no-direct-w=1 copyout-wb-blocked=1 post-wb-dirty=1 private-cow-isolated=1"));
        Ok(())
    }

    /// 用户 WAIT/WAKE 在跨 Mm/VA 文件字节上匹配；PRIVATE、inode 与实际复用 PFN 分别隔离。
    #[test]
    #[ktest(tags("W19", "fs-shared", "user-futex"), min_cpus = 2)]
    fn file_futex_identity_private_cancel_and_pfn_reuse(
        context: &mut TestContext<'_>,
        fixture: &mut WriteOwner,
    ) -> TestResult {
        fixture.prepare(context)?;
        let file = fixture.open(context, b"/test/write-map", 2)?;
        let old_inode = fixture.inodes.last().unwrap().clone();
        context.expect_eq(checked(context, file.write_at(64, &[0; 8]))?, 8);
        let other = fixture.open(context, b"/test/write-reserve", 2)?;
        let new_inode = fixture.inodes.last().unwrap().clone();
        context.expect_eq(checked(context, other.write_at(0, &[0; 4096]))?, 4096);
        for mode in [b"f" as &[u8], b"p", b"q"] {
            let image = fixture.shared_image(context, mode)?;
            let mm = image.mm.clone();
            fixture.start_shared(context, image)?;
            fixture.phase(&mm, 0, 1)?;
            let child = fixture.child(&mm)?;
            let child_mm = child.mm().ok_or(TestError("file futex child MM missing"))?;
            let private = mode[0] == b'p';
            let snapshot =
                UserOwner::futex_waiting(&child_mm, &child, SHARED_ALIAS + 64, private, u32::MAX)?;
            context.expect_eq(snapshot.first_wait_id.is_some(), true);
            context.expect_eq(snapshot.first_deadline.is_some(), true);
            let parent = futex::test_snapshot(&mm, SOURCE + 64, private, u32::MAX)
                .map_err(|_| TestError("file futex parent snapshot failed"))?;
            context.expect_eq(parent.count, usize::from(mode[0] == b'f'));
            if mode[0] == b'f' {
                context.expect_eq(parent.first_wait_id, snapshot.first_wait_id);
                let neighbor = futex::test_snapshot(&mm, SOURCE + 68, false, u32::MAX)
                    .map_err(|_| TestError("file futex neighbor snapshot failed"))?;
                context.expect_eq(neighbor.count, 0);
            }
            WriteOwner::release(&mm, 1)?;
            fixture.finish_shared(context)?;
            let cancelled = futex::test_snapshot(&child_mm, SHARED_ALIAS + 64, private, u32::MAX)
                .map_err(|_| TestError("file futex cancelled snapshot failed"))?;
            context.expect_eq(cancelled.count, 0);
            context.expect_eq(child.mm().is_none(), true);
            drop((child, child_mm, mm));
        }
        checked(context, crate::fs::writeback::sync_inode(&old_inode))?;
        checked(context, crate::fs::writeback::sync_inode(&new_inode))?;
        let image = fixture.shared_image(context, b"u")?;
        let mm = image.mm.clone();
        fixture.start_shared(context, image)?;
        fixture.phase(&mm, 0, 1)?;
        let child = fixture.child(&mm)?;
        let child_mm = child
            .mm()
            .ok_or(TestError("reused PFN futex child MM missing"))?;
        let waiting =
            UserOwner::futex_waiting(&child_mm, &child, SHARED_ALIAS + 64, false, u32::MAX)?;
        let old_physical = child_mm.mapping(SHARED_ALIAS).unwrap().physical;
        let old_key = child_mm
            .futex_read()
            .key(SHARED_ALIAS + 64, false)
            .map_err(|_| TestError("old file futex key failed"))?;
        // 只建无驻留的身份观察范围；旧 node 与 key 均保 inode，不保文件原页。
        let identity = Arc::try_new(vm(context, Mm::try_new().map_err(VmError::from))?)
            .map_err(|_| TestError("old futex identity MM OOM"))?;
        vm(
            context,
            identity.map_file(SOURCE, 4096, RO, RO, file.clone(), 0, false),
        )?;
        vm(context, mm.unmap(SOURCE, 4096))?;
        vm(context, child_mm.unmap(SHARED_ALIAS, 4096))?;
        drop(checked(context, other.file_page(0))?);
        new_inode.mapping().prune_unused();
        let mut pressure = PagePressure::prepare()?;
        context.expect_eq(old_inode.mapping().prune_unused(), 1);
        mm::drain_all_pages();
        let mut recycled = None;
        {
            let _pin = PreemptGuard::new();
            for _ in 0..pressure.pages.capacity() {
                let Some(page) = mm::alloc_pages(0, false) else {
                    break;
                };
                if page.phys_addr() == old_physical {
                    recycled = Some(page);
                    break;
                }
                pressure.pages.push(page);
            }
        }
        let recycled =
            recycled.ok_or(TestError("released file PFN was not actually reacquired"))?;
        // 先归还其他候选并排空 PCP，目标仍保活；最后归还目标后立即走生产 canonical miss。
        drop(pressure);
        mm::drain_all_pages();
        drop(recycled);
        let reused = other.file_page(0);
        let reused = checked(context, reused)?;
        context.expect_eq(reused.phys_addr(), old_physical);
        vm(
            context,
            mm.mmap(MmapRequest {
                address: MmapAddress::Fixed(SOURCE),
                length: 4096,
                permissions: RW,
                maximum: RW,
                source: MmapSource::File {
                    file: other.clone(),
                    offset: 0,
                    private: false,
                },
            }),
        )?;
        vm(context, mm.fault(SOURCE, UserAccess::Read))?;
        context.expect_eq(mm.mapping(SOURCE).unwrap().physical, old_physical);
        let new_key = mm
            .futex_read()
            .key(SOURCE + 64, false)
            .map_err(|_| TestError("new file futex key failed"))?;
        context.expect_eq(old_key.matches(&new_key), false);
        context.expect_eq(old_inode.ptr_eq(&new_inode), false);
        vm(context, identity.fault(SOURCE, UserAccess::Read))?;
        let still_waiting = futex::test_snapshot(&identity, SOURCE + 64, false, u32::MAX)
            .map_err(|_| TestError("old live futex identity snapshot failed"))?;
        context.expect_eq(still_waiting.count, 1);
        context.expect_eq(still_waiting.first_wait_id, waiting.first_wait_id);
        let isolated = futex::test_snapshot(&mm, SOURCE + 64, false, u32::MAX)
            .map_err(|_| TestError("reused PFN futex identity snapshot failed"))?;
        context.expect_eq(isolated.count, 0);
        WriteOwner::release(&mm, 1)?;
        fixture.finish_shared(context)?;
        let cancelled = futex::test_snapshot(&identity, SOURCE + 64, false, u32::MAX)
            .map_err(|_| TestError("reused PFN cancelled snapshot failed"))?;
        context.expect_eq(cancelled.count, 0);
        context.diagnostic(format_args!("W19 shared-file-byte-wake=1 private-domain=1 different-inode=1 timed-cancel-join=1 reused-pfn={} live-old-node-isolated=1", reused.phys_addr()));
        drop((old_key, new_key, identity, mm, child_mm, child, reused));
        Ok(())
    }
}
