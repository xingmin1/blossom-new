//! 可写私有工作卷上的实际字节、分配树、普通用户 ABI 与映像排斥消费。

use crate::fs::ext4::{codec, BlockMapping, InodeData};
use crate::fs::file::{open_path_with_flags, WriteBuffer};
use crate::fs::ktest::user::*;
use crate::fs::SeekFrom;
use crate::mm::user_table::PagePermissions;
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

struct WriteOwner {
    user: UserOwner,
    inodes: Vec<InodeRef>,
    append_gate: Option<Arc<Completion>>,
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
        })
    }

    /// 所有生产者先退出和离栈，再真实写出文件页、元数据及 FLUSH，最后释放根并停止设备。
    fn quiesce(&mut self, context: &mut TestContext<'_>) -> TestResult {
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
        self.user.path.owner.join_tasks()?;
        if let Some(graph) = &self.user.graph {
            graph.request_root_exit(9);
        }
        if self.user.running.is_some() {
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
}
