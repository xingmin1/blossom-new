//! 可写私有工作卷上的实际字节、分配树、普通用户 ABI 与映像排斥消费。

use crate::arch::UserAccess;
use crate::block::{Bio, BioAllocator, BlkOperation, IoResult};
use crate::cache::FileUnmapPhase;
use crate::fs::ext4::{codec, BlockMapping, InodeData};
use crate::fs::file::{open_path_with_flags, FileIoContext, WriteBuffer};
use crate::fs::inode::{InodeCacheWatch, InodeRef, TruncatePhase};
use crate::fs::ktest::user::*;
use crate::fs::{InodeKind, SeekFrom};
use crate::mm::dma::MappedPages;
use crate::mm::reclaim::ReclaimAttempt;
use crate::mm::space::{MmapAddress, MmapRequest, MmapSource};
use crate::mm::user_table::PagePermissions;
use crate::mm::PAGE_SIZE;
use crate::sync::PreemptGuard;
use crate::task::{ReclaimMode, ReclaimScope};
use virtio_drivers::BufferDirection;

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
pub(super) const SHARED_ALIAS: usize = 0x4a00_0000;

/// 普通测试消费者的真实 Task 状态与同步结果观察，不承担 join 或文件拥有。

pub(super) struct SyncObservation {
    executor: SpinLock<Option<alloc::sync::Weak<Task>>>,
    result: SpinLock<Option<Result<(), FsError>>>,
}

impl SyncObservation {
    /// 只查询实际执行者的调度状态，entered 标记不能替代 Blocked。
    pub(super) fn blocked(&self) -> bool {
        self.executor
            .lock()
            .as_ref()
            .and_then(alloc::sync::Weak::upgrade)
            .is_some_and(|task| sched::test_state(&task) == State::Blocked)
    }
    /// 返回真实同步调用已经写下的结果，不用观察任务退出代替底层结果。
    pub(super) fn result(&self) -> Option<Result<(), FsError>> {
        *self.result.lock()
    }
}

struct TruncateControls {
    phase: AtomicUsize,
    cancelled: AtomicBool,
    gates: [Completion; 3],
    unmap_phase: AtomicUsize,
    before_sync: SpinLock<Option<(usize, usize)>>,
    after_sync: SpinLock<Option<(usize, usize)>>,
}

impl TruncateControls {
    /// 固定门仅暂停真实截断或 CSD 前后的拥有交接，错误清理放行同一批原参与者。
    fn new() -> Result<Arc<Self>, TestError> {
        Arc::try_new(Self {
            phase: AtomicUsize::new(0),
            cancelled: AtomicBool::new(false),
            gates: core::array::from_fn(|_| Completion::new()),
            unmap_phase: AtomicUsize::new(0),
            before_sync: SpinLock::new(None),
            after_sync: SpinLock::new(None),
        })
        .map_err(|_| TestError("truncate controls OOM"))
    }
}

/// 复用原用户、任务、卷及异步消费者拥有边界的可写文件夹具。

pub(super) struct WriteOwner {
    pub(super) user: UserOwner,
    pub(super) inodes: Vec<InodeRef>,
    append_gate: Option<Arc<Completion>>,
    shared_mm: Option<Arc<Mm>>,
    prepare_gate: Option<Arc<Completion>>,
    truncate_controls: Option<Arc<TruncateControls>>,
    consumers: [Option<JoinHandle>; 2],
    pub(super) cold_volume: Option<Arc<Ext4Volume>>,
    cold_cache: Option<Arc<InodeCache>>,
    eviction: Option<InodeCacheWatch>,
}

impl WriteOwner {
    /// 正文显式准备 RW 卷及同一根挂载，普通用户运行仍借原身份拥有者。
    fn prepare(&mut self, context: &TestContext<'_>) -> TestResult {
        self.inodes
            .try_reserve_exact(16)
            .map_err(|_| TestError("write inode owner OOM"))?;
        self.user.prepare_mode(context, AccessMode::ReadWrite, 4)
    }
    /// 原生 namespace 消费者沿同一 RW PathOwner 准备，并让原清理无论 graph 状态都同步此卷。
    pub(super) fn prepare_namespace(&mut self, context: &TestContext<'_>) -> TestResult {
        self.prepare(context)?;
        assert!(self.cold_volume.is_none() && self.cold_cache.is_none());
        self.cold_volume = Some(self.volume());
        Ok(())
    }
    /// 准备可由测试组写入的 setgid 目录与只允许 root 搜索的负例子目录。
    fn prepare_namespace_parent(&mut self, context: &TestContext<'_>) -> TestResult {
        let vfs = self.user.path.vfs().clone();
        let fs_context = self.user.path.context().clone();
        let root = vfs.root();
        checked(
            context,
            vfs.mkdir_from(&fs_context, &root, b"/test/ns-work", 0o777, (0, 0), 0),
        )?;
        let workspace = self.open(context, b"/test/ns-work", 0x10000)?;
        let inode = checked(context, workspace.path().unwrap().inode())?.clone();
        // mkdir 不接受 SGID 模式位；父目录的继承属性通过受保护元数据更新设置。
        checked(
            context,
            inode.update_metadata(checked(context, inode.disk_volume())?.claim(), |metadata| {
                metadata.mode |= 0o2000;
                metadata.gid = 1001;
            }),
        )?;
        drop(workspace);
        checked(
            context,
            vfs.mkdir_from(
                &fs_context,
                &root,
                b"/test/ns-work/ns-permission",
                0o700,
                (0, 0),
                0,
            ),
        )?;
        drop(self.open(context, b"/test/ns-work/ns-permission", 0x10000)?);
        Ok(())
    }
    /// 删除普通用户无权移除的预置权限目录与最后一层测试目录。
    fn remove_namespace_parent(&self, context: &TestContext<'_>) -> TestResult {
        let vfs = self.user.path.vfs();
        let fs_context = self.user.path.context();
        let root = vfs.root();
        checked(
            context,
            vfs.rmdir_from(fs_context, &root, b"/test/ns-work/ns-permission", (0, 0)),
        )?;
        checked(
            context,
            vfs.rmdir_from(fs_context, &root, b"/test/ns-work", (0, 0)),
        )?;
        Ok(())
    }

    /// 保留实际 inode 到同步排空之后；失败路径也不能丢弃已接受修改。
    pub(super) fn open(
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
        checked(
            context,
            open_path_with_flags(self.user.path.vfs(), path, flags, FileIoContext::Kernel),
        )
    }
    /// 保留末组件本身的 canonical inode，symlink 不跟随到其目标。
    fn hold_nofollow(
        &mut self,
        context: &TestContext<'_>,
        pathname: &[u8],
    ) -> Result<InodeRef, TestError> {
        let path = checked(
            context,
            self.user
                .path
                .vfs()
                .resolve(self.user.path.context(), pathname, ResolveMode::NoFollow),
        )?;
        let inode = checked(context, path.inode())?.clone();
        if !self.inodes.iter().any(|old| old.ptr_eq(&inode)) {
            if self.inodes.len() == self.inodes.capacity() {
                return Err(TestError("write inode owner capacity"));
            }
            self.inodes.push(inode.clone());
        }
        Ok(inode)
    }

    /// 从同一原根取得卷，根引用仅在正文和同步期间短暂保活。
    pub(super) fn volume(&self) -> Arc<Ext4Volume> {
        self.user
            .path
            .vfs()
            .root()
            .inode()
            .unwrap()
            .disk_volume()
            .unwrap()
            .clone()
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
    /// 按指定的真实用户身份启动同一普通 ELF，并保留原 UserOwner 的 wait/join。
    fn start_shared_as(
        &mut self,
        context: &TestContext<'_>,
        image: LoadedImage,
        credentials: Credentials,
    ) -> TestResult {
        self.user
            .start_program(context, image, true, false, credentials)
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

    /// 原 Task 运行真实同步调用，结果另存以消费预期错误，句柄保在 fixture 的清理边界外。
    pub(super) fn consume_sync(
        &mut self,
        index: usize,
        operation: impl FnOnce() -> Result<(), FsError> + Send + 'static,
    ) -> Result<Arc<SyncObservation>, TestError> {
        assert!(self.consumers[index].is_none());
        let observation = Arc::try_new(SyncObservation {
            executor: SpinLock::new(None),
            result: SpinLock::new(None),
        })
        .map_err(|_| TestError("sync observation OOM"))?;
        let record = observation.clone();
        let prepared = task::prepare_kernel_on(cpu::current().id, move || {
            *record.executor.lock() = Some(Arc::downgrade(&task::current_task()));
            *record.result.lock() = Some(operation());
            0
        })
        .map_err(|_| TestError("sync consumer prepare failed"))?;
        self.consumers[index] = Some(prepared.publish());
        Ok(observation)
    }

    /// 冷消费者独立拥有完整 FS 层，设备仍借本例原 owner；旧 volume 的弱引用到期才允许下一次调用。
    fn cold_open(&mut self, context: &TestContext<'_>) -> Result<FileRef, TestError> {
        assert!(self.cold_volume.is_none() && self.cold_cache.is_none());
        self.inodes
            .try_reserve_exact(16)
            .map_err(|_| TestError("cold inode slots OOM"))?;
        let volume = checked(
            context,
            Ext4Volume::open(
                self.user.path.owner.disk()?.clone(),
                AccessMode::ReadWrite,
                64,
            ),
        )?;
        self.cold_volume = Some(volume.clone());
        let cache = Arc::try_new(checked(context, InodeCache::try_new(64))?)
            .map_err(|_| TestError("cold cache owner OOM"))?;
        self.cold_cache = Some(cache.clone());
        let vfs = checked(
            context,
            Vfs::try_new(
                crate::fs::Filesystem::Ext4(volume),
                cache,
                128,
                4,
                AccessMode::ReadWrite,
            ),
        )?;
        let fs_context = checked(context, FsContext::new(vfs.root(), vfs.root()))?;
        let path = checked(
            context,
            vfs.resolve(&fs_context, b"/test/write-reserve", ResolveMode::Follow),
        )?;
        self.inodes.push(checked(context, path.inode())?.clone());
        checked(
            context,
            open_path_with_flags(&vfs, path, 2, FileIoContext::Kernel),
        )
    }

    /// 正式同步后的全部旧 FS 拥有先退出，真实完成 Work 尾和弱引用到期后才重建同设备的卷及缓存。
    fn cold_reopen(
        &mut self,
        context: &mut TestContext<'_>,
        mm: Arc<Mm>,
        file: FileRef,
        inode: InodeRef,
        volume: Arc<Ext4Volume>,
    ) -> Result<(FileRef, usize), TestError> {
        let disk = self.user.path.owner.disk()?.clone();
        let weak_mm = Arc::downgrade(&mm);
        let weak_file = Arc::downgrade(&file);
        let weak_volume = Arc::downgrade(&volume);
        let weak_metadata = Arc::downgrade(volume.metadata_cache());
        let weak_cache = Arc::downgrade(self.cold_cache.as_ref().unwrap());
        assert!(self.eviction.is_none());
        self.eviction = Some(self.cold_cache.as_ref().unwrap().watch());
        drop((mm, file, inode, volume));
        self.inodes.clear();
        drop(self.cold_cache.take());
        checked(context, self.eviction.as_ref().unwrap().wait_retired())?;
        drop(self.eviction.take());
        drop(self.cold_volume.take());
        disk.test_flush_completions();
        context.expect_eq(disk.test_snapshot().inflight, 0);
        context.expect_eq(weak_mm.upgrade().is_none(), true);
        context.expect_eq(weak_file.upgrade().is_none(), true);
        context.expect_eq(weak_cache.upgrade().is_none(), true);
        context.expect_eq(weak_metadata.upgrade().is_none(), true);
        context.expect_eq(weak_volume.upgrade().is_none(), true);
        if weak_mm.strong_count() != 0
            || weak_file.strong_count() != 0
            || weak_cache.strong_count() != 0
            || weak_volume.strong_count() != 0
            || weak_metadata.strong_count() != 0
        {
            return Err(TestError("cold reload still has old FS owners"));
        }
        let reads_before = disk.test_snapshot().submitted;
        Ok((self.cold_open(context)?, reads_before))
    }
    /// 未离栈句柄留在 fixture，错误返回仍先恢复设备完成再进行有限 join。
    pub(super) fn join_consumers(&mut self) -> TestResult {
        for join in self.consumers.iter().flatten() {
            until("shared kernel consumer did not leave stack", || {
                join.try_join().is_some()
            })?;
        }
        let mut first = None;
        for join in &mut self.consumers {
            if let Some(join) = join.take() {
                if join.join() != 0 && first.is_none() {
                    first = Some(TestError("shared kernel consumer failed"));
                }
            }
        }
        first.map_or(Ok(()), Err)
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
            truncate_controls: None,
            consumers: core::array::from_fn(|_| None),
            cold_volume: None,
            cold_cache: None,
            eviction: None,
        })
    }

    /// 所有生产者先退出和离栈，再真实写出文件页、元数据及 FLUSH，最后释放根并停止设备。
    fn quiesce(&mut self, context: &mut TestContext<'_>) -> TestResult {
        if let Some(mm) = self.shared_mm.take() {
            mm.ktest_set_mkwrite_hook(None);
            mm.ktest_set_fault_hook(None);
            mm.ktest_set_fork_hook(None);
            mm.ktest_set_vma_unmap_hook(None);
        }
        for inode in &self.inodes {
            inode.ktest_set_truncate_hook(None);
            inode.mapping().ktest_set_unmap_hook(None);
        }
        if let Some(controls) = self.truncate_controls.take() {
            controls.cancelled.store(true, Ordering::Release);
            for gate in &controls.gates {
                gate.complete();
            }
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
        let mut first = self.join_consumers().err();
        if self.consumers.iter().any(Option::is_some) {
            return Err(first.unwrap_or(TestError("consumer still owns original stack")));
        }
        if let Err(error) = self.user.path.owner.join_tasks() {
            if first.is_none() {
                first = Some(error);
            }
        }
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
        for inode in &self.inodes {
            let result = crate::fs::writeback::sync_inode(inode);
            if result == Err(FsError::Io(BlockError::StopUnconfirmed)) {
                return Err(TestError(
                    "writeback stop unconfirmed; retaining original owners",
                ));
            }
            if let Err(error) = checked(context, result) {
                if first.is_none() {
                    first = Some(error);
                }
            }
        }
        if self
            .inodes
            .iter()
            .any(|inode| inode.mapping().has_dirty_or_writeback())
        {
            return Err(first.unwrap_or(TestError("write cleanup retains actual Dirty or WB")));
        }
        if self.eviction.is_none() {
            self.eviction = self.cold_cache.as_ref().map(|cache| cache.watch());
        }
        if let Some(eviction) = self.eviction.as_ref() {
            self.inodes.clear();
            drop(self.cold_cache.take());
            let result = eviction.wait_retired();
            if result == Err(FsError::Io(BlockError::StopUnconfirmed)) {
                return Err(TestError(
                    "inode eviction stop unconfirmed; retaining original owners",
                ));
            }
            if let Err(error) = checked(context, result) {
                return Err(first.unwrap_or(error));
            }
            drop(self.eviction.take());
        }
        let volume = self
            .cold_volume
            .clone()
            .or_else(|| self.user.graph.as_ref().map(|_| self.volume()));
        if let Some(volume) = volume {
            let result = crate::fs::writeback::sync_metadata(&volume);
            if result == Err(FsError::Io(BlockError::StopUnconfirmed)) {
                return Err(TestError(
                    "metadata stop unconfirmed; retaining original owners",
                ));
            }
            if let Err(error) = checked(context, result) {
                if first.is_none() {
                    first = Some(error);
                }
            }
        }
        if let Some(volume) = self.cold_volume.as_ref() {
            if metadata_pending(volume) {
                return Err(first.unwrap_or(TestError("cold cleanup retains metadata Dirty or WB")));
            }
        } else if self.user.graph.is_some() && metadata_pending(&self.volume()) {
            return Err(first.unwrap_or(TestError("write cleanup retains metadata Dirty or WB")));
        }
        self.inodes.clear();
        drop(core::mem::take(&mut self.inodes));
        drop(self.cold_cache.take());
        drop(self.cold_volume.take());
        let cleanup = <UserOwner as Fixture<DeviceInventory>>::quiesce(&mut self.user, context);
        context.expect_eq(first, None);
        cleanup
    }

    /// 两层六域和 Task/worker/domain 均由原拥有者比较，不重置统计。
    fn tear_down(self, context: &mut TestContext<'_>) -> TestResult {
        <UserOwner as Fixture<DeviceInventory>>::tear_down(self.user, context)
    }
}

/// 每个期望字节来自独立输入，短读取或读到旧缓存均失败。
pub(super) fn bytes_at(
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
    let record = checked(
        context,
        checked(context, inode.disk_volume())?.load_inode(inode.number(), None),
    )?;
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
pub(super) fn shared_alias(
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
pub(super) fn allocation_counts(
    context: &mut TestContext<'_>,
    volume: &Ext4Volume,
) -> Result<(u64, u64), TestError> {
    let g = volume.geometry();
    let cache = volume.metadata_cache();
    let superblock = checked(
        context,
        cache
            .get(1024 / g.block_size as u64, g.block_size, None)
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
                    None,
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
                cache.get(physical, g.block_size, None).map_err(FsError::Io),
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

/// 正常分配和末使用释放都从当前共同位图取真实 inode 位，并验证同一记录的 checksum。
pub(super) fn inode_allocation_bit(volume: &Ext4Volume, number: u64) -> Result<bool, FsError> {
    let g = volume.geometry();
    let group = (number - 1) / g.inodes_per_group;
    let bit = (number - 1) % g.inodes_per_group;
    let byte = group * g.descriptor_size as u64;
    let descriptor = volume.metadata_cache().get(
        g.first_data_block + 1 + byte / g.block_size as u64,
        g.block_size,
        None,
    )?;
    let mut record = [0u8; 64];
    descriptor.with_read(|bytes| {
        let offset = (byte % g.block_size as u64) as usize;
        record[..g.descriptor_size].copy_from_slice(&bytes[offset..offset + g.descriptor_size]);
    })?;
    drop(descriptor);
    let layout = codec::group_layout(g, group, &record[..g.descriptor_size])?;
    let state = codec::group_state(g, &record[..g.descriptor_size])?;
    if state.flags & 1 != 0 {
        return Err(FsError::Corrupt);
    }
    let bitmap = volume
        .metadata_cache()
        .get(layout.inode_bitmap, g.block_size, None)?;
    bitmap.with_read(|bytes| {
        codec::verify_bitmap(g, &record[..g.descriptor_size], bytes, true)?;
        Ok(bytes[bit as usize / 8] & (1 << (bit % 8)) != 0)
    })?
}

#[derive(Default, Debug)]
struct Tree {
    data: u64,
    nodes: u64,
    extents: usize,
    depth: u16,
    blocks: Vec<u64>,
}

/// 停稳其他分配与释放者后核物理位；几何排除或真实 used 返回 Some(true)，未初始化且仍有空闲返回 None。
pub(super) fn block_allocation_bit(
    volume: &Ext4Volume,
    physical: u64,
) -> Result<Option<bool>, FsError> {
    let g = volume.geometry();
    // validate_blocks 只核卷边界和固定 super/GDT 保留范围，这些地址不能成为新 data。
    if g.validate_blocks(physical, 1).is_err() {
        return Ok(Some(true));
    }
    let group = (physical - g.first_data_block) / g.blocks_per_group;
    let bit = (physical - g.first_data_block) % g.blocks_per_group;
    let byte = group * g.descriptor_size as u64;
    let descriptor = volume.metadata_cache().get(
        g.first_data_block + 1 + byte / g.block_size as u64,
        g.block_size,
        None,
    )?;
    let mut record = [0u8; 64];
    descriptor.with_read(|bytes| {
        let at = (byte % g.block_size as u64) as usize;
        record[..g.descriptor_size].copy_from_slice(&bytes[at..at + g.descriptor_size]);
    })?;
    drop(descriptor);
    let layout = codec::group_layout(g, group, &record[..g.descriptor_size])?;
    let state = codec::group_state(g, &record[..g.descriptor_size])?;
    if state.flags & 2 != 0 {
        if group == 0 || g.features_ro_compat & (0x10 | codec::METADATA_CSUM) == 0 {
            return Err(FsError::Corrupt);
        }
        // 未初始化 bitmap 不含权威位；正式 free_blocks=0 时 allocator 会跳过整组。
        return Ok((state.free_blocks == 0).then_some(true));
    }
    let bitmap = volume
        .metadata_cache()
        .get(layout.block_bitmap, g.block_size, None)?;
    bitmap.with_read(|bytes| {
        codec::verify_bitmap(g, &record[..g.descriptor_size], bytes, false)?;
        Ok(Some(bytes[bit as usize / 8] & (1 << (bit % 8)) != 0))
    })?
}

/// 每个连接到树的物理块必须有自己的真实位图位，不能只靠总计数相等。
fn account_block(volume: &Ext4Volume, physical: u64, tree: &mut Tree) -> Result<(), FsError> {
    volume.geometry().validate_blocks(physical, 1)?;
    if block_allocation_bit(volume, physical)? != Some(true)
        || tree.blocks.capacity() == tree.blocks.len()
    {
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
    let volume = checked(context, inode.disk_volume())?;
    let data = checked(context, volume.load_inode(inode.number(), None))?;
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

/// 普通小对象沿原堆预留已存在 slab 的空位，物理页施压时不耗尽堆类别。
struct SmallHeapRoom {
    objects: [[Option<Vec<u8>>; 16]; 11],
}

impl SmallHeapRoom {
    /// 保留每个小对象类别的真实分配，避免页压力准备时收缩其 backing。
    fn prepare() -> Result<Self, TestError> {
        let mut room = Self {
            objects: core::array::from_fn(|_| core::array::from_fn(|_| None)),
        };
        for (objects, size) in room.objects.iter_mut().zip(mm::SLUB_SIZES) {
            for object in objects {
                let mut bytes = Vec::new();
                bytes
                    .try_reserve_exact(size)
                    .map_err(|_| TestError("slow consumer small heap preparation OOM"))?;
                *object = Some(bytes);
            }
        }
        Ok(room)
    }

    /// 交错释放同 slab 内小对象，仍保其余真实分配；随后必须重新观察原页 None。
    fn release_half(&mut self) {
        for objects in &mut self.objects {
            for object in objects.iter_mut().step_by(2) {
                drop(object.take());
            }
        }
    }
}

/// 生产者停稳后逐个有界检查原 metadata 索引，残留真实义务时不得销毁 cache 平账。
fn metadata_pending(volume: &Ext4Volume) -> bool {
    let cache = volume.metadata_cache();
    for mark in [
        crate::cache::FolioMark::Dirty,
        crate::cache::FolioMark::Writeback,
    ] {
        let mut cursor = 0;
        for _ in 0..cache.len() {
            let mut selected = [None];
            let next = cache.select_range(cursor, u64::MAX, mark, &mut selected);
            if selected[0].is_some() {
                return true;
            }
            let Some(next) = next else {
                break;
            };
            cursor = next;
        }
    }
    false
}

/// 有效前缀和新尾由独立图样决定；普通读取、原共享 VMA 与冷 FS 读取采用相同逐页字节断言。
fn truncate_contents(
    context: &mut TestContext<'_>,
    file: &FileRef,
    mapping: Option<&Mm>,
    prefix: usize,
    size: usize,
) -> TestResult {
    for offset in (0..size).step_by(PAGE_SIZE) {
        let length = PAGE_SIZE.min(size - offset);
        let mut expected = [0u8; PAGE_SIZE];
        let valid = prefix.saturating_sub(offset).min(length);
        expected[..valid].fill(0x65);
        bytes_at(context, file, offset as u64, &expected[..length])?;
        if let Some(mm) = mapping {
            mapped_bytes(context, mm, SOURCE + offset, &expected[..length]);
        }
    }
    Ok(())
}

#[ktest::suite(fixture = WriteOwner, suite_resources = DeviceInventory, context = "Thread", tags("fs-write", "fs-path", "fs-oom"), resources("root"), timeout_ms = 180000)]
pub(crate) mod writes {
    use super::*;

    /// 冷文件 miss 和文件、匿名 fault 在原页 None 后消费可收文件页；无进展返回真实 NoMemory。
    #[test]
    #[case("file_like_miss", 0u8)]
    #[case("file_fault", 1u8)]
    #[case("anonymous_zero_fault", 2u8)]
    #[ktest(tags("W32", "W33", "fs-reclaim", "fs-slow-consumer"))]
    fn normal_consumers_retry_original_allocator_after_page_none(
        context: &mut TestContext<'_>,
        fixture: &mut WriteOwner,
        consumer: u8,
    ) -> TestResult {
        const FILE: usize = 0x4b00_0000;
        const ANON: usize = 0x4d00_0000;
        let file = fixture.cold_open(context)?;
        let inode = fixture.inodes.last().unwrap().clone();
        let volume = fixture.cold_volume.as_ref().unwrap().clone();
        checked(context, file.truncate(0))?;
        for index in 0..19 {
            context.expect_eq(
                checked(
                    context,
                    file.write_at((index * PAGE_SIZE) as u64, &[0x60 + index as u8; PAGE_SIZE]),
                )?,
                PAGE_SIZE,
            );
        }
        checked(context, file.sync_range(0..u64::MAX, false))?;
        let old_mm = Arc::try_new(vm(context, Mm::try_new().map_err(VmError::from))?)
            .map_err(|_| TestError("slow consumer old MM owner OOM"))?;
        let (file, _) = fixture.cold_reopen(context, old_mm, file, inode, volume)?;
        let inode = fixture.inodes.last().unwrap().clone();
        let disk = checked(context, inode.disk_volume())?.disk().clone();
        context.expect_eq(checked(context, file.stat())?.size, (19 * PAGE_SIZE) as u64);
        let mm = vm(context, Mm::try_new().map_err(VmError::from))?;
        vm(
            context,
            mm.map_file(
                FILE,
                4 * PAGE_SIZE,
                RO,
                RO,
                file.clone(),
                (16 * PAGE_SIZE) as u64,
                false,
            ),
        )?;
        vm(
            context,
            mm.map_file(SHARED_ALIAS, PAGE_SIZE, RW, RW, file.clone(), 0, true),
        )?;
        vm(context, mm.map_anonymous(ANON, 2 * PAGE_SIZE, RW, RW))?;
        // 同页表区域的匿名叶保住目录；有限无进展检查不能把撤 File 叶后的表页归还算成无进展。
        vm(
            context,
            mm.map_anonymous(FILE + 4 * PAGE_SIZE, PAGE_SIZE, RW, RW),
        )?;
        vm(context, mm.fault(FILE + 4 * PAGE_SIZE, UserAccess::Read))?;
        context.expect_eq(mm.copyout(SHARED_ALIAS, &[0x93]).copied, 1);
        let private = mm
            .mapping(SHARED_ALIAS)
            .ok_or(TestError("slow consumer private COW leaf absent"))?
            .physical;
        let mut donors = [0usize; 16];
        let mut preparing = ReclaimAttempt::new();
        for (index, physical) in donors.iter_mut().enumerate() {
            let folio = checked(
                context,
                file.mapping_inode()
                    .unwrap()
                    .file_page(index as u64, &mut preparing),
            )?;
            context.expect_eq(folio.dirty_sectors(), 0);
            context.expect_eq(folio.writeback_sectors(), 0);
            *physical = folio.phys_addr();
        }
        context.expect_eq(inode.mapping().cached_folio(16).is_none(), true);
        context.expect_eq(mm.mapping(FILE).is_none(), true);
        context.expect_eq(mm.mapping(ANON).is_none(), true);
        let mut eof = [0xa7];
        context.expect_eq(file.read_at((19 * PAGE_SIZE) as u64, &mut eof), Ok(0));
        context.expect_eq(eof, [0xa7]);
        context.expect_eq(
            mm.fault(FILE + 3 * PAGE_SIZE, UserAccess::Read),
            Err(VmError::BeyondEof),
        );
        sched::test_migrate_to(0);
        let mut room = SmallHeapRoom::prepare()?;
        let mut pressure = PagePressure::prepare()?;
        disk.test_flush_completions();
        let before_io = disk.test_snapshot();
        let result = (|| -> TestResult {
            let page_none = {
                let _pin = PreemptGuard::new();
                pressure.exhaust()
            };
            room.release_half();
            let small_heap_page_none = mm::alloc_pages(0, false).is_none();
            context.expect_eq(page_none && small_heap_page_none, true);
            if !page_none || !small_heap_page_none {
                return Err(TestError("slow consumer did not start at real page None"));
            }
            let fastpath_preserved = donors.iter().enumerate().all(|(index, physical)| {
                inode
                    .mapping()
                    .cached_folio(index as u64)
                    .map(|folio| folio.phys_addr())
                    == Some(*physical)
            });
            context.expect_eq(fastpath_preserved, true);
            if !fastpath_preserved {
                return Err(TestError(
                    "raw allocator reclaimed file cache before the explicit consumer",
                ));
            }
            context.expect_eq(ReclaimMode::current(), ReclaimMode::Normal);
            let mut attempt = ReclaimAttempt::new();
            let mut target = None;
            match consumer {
                0 => {
                    target = Some(checked(
                        context,
                        file.mapping_inode().unwrap().file_page(16, &mut attempt),
                    )?)
                }
                1 => vm(context, mm.fault(FILE, UserAccess::Read))?,
                2 => vm(context, mm.fault(ANON, UserAccess::Read))?,
                _ => unreachable!(),
            }
            let retired = (0..16)
                .filter(|index| inode.mapping().cached_folio(*index).is_none())
                .count();
            context.expect_eq(0 < retired, true);
            // 原 donor 没有诊断 folio pin；缓存退休后由正式入口同源申请目标页，核验其内容与 PFN。
            let physical = if consumer == 2 {
                let physical = mm
                    .mapping(ANON)
                    .ok_or(TestError("slow anonymous fault leaf absent"))?
                    .physical;
                mapped_bytes(context, &mm, ANON, &[0; PAGE_SIZE]);
                physical
            } else {
                if target.is_none() {
                    target = inode.mapping().cached_folio(16);
                }
                let folio = target
                    .as_ref()
                    .ok_or(TestError("slow file consumer canonical absent"))?;
                let mut bytes = [0xa7; PAGE_SIZE];
                checked(context, folio.copy_to(0, &mut bytes).map_err(FsError::from))?;
                context.expect_eq(bytes, [0x70; PAGE_SIZE]);
                if consumer == 1 {
                    context.expect_eq(
                        mm.mapping(FILE).map(|leaf| leaf.physical),
                        Some(folio.phys_addr()),
                    );
                    mapped_bytes(context, &mm, FILE, &[0x70; PAGE_SIZE]);
                }
                folio.phys_addr()
            };
            context.expect_eq(
                mm.mapping(SHARED_ALIAS).map(|leaf| leaf.physical),
                Some(private),
            );
            mapped_bytes(context, &mm, SHARED_ALIAS, &[0x93]);
            let stats = attempt.stats();
            context.expect_eq(
                stats.scanned <= 64 && stats.isolated <= 16 && stats.rmap_checks <= 64,
                true,
            );
            if consumer == 0 {
                context.expect_eq(0 < stats.freed_file, true);
            }
            let after_io = disk.test_snapshot();
            context.expect_eq(after_io.data_submitted, before_io.data_submitted);
            if consumer != 2 {
                context.expect_eq(before_io.submitted < after_io.submitted, true);
                context.expect_eq(before_io.completed < after_io.completed, true);
            }
            context.diagnostic(format_args!("W32 slow consumer={} page-none={} small-heap-page-none={} donors={:?} retired-cache={} consumed-physical={:#x} private-physical={:#x} file-api-budget={:?} MM-budget-observer=none IO-submitted={} IO-completed={}",
                consumer, page_none, small_heap_page_none, donors, retired, physical, private,
                if consumer == 0 { Some(stats) } else { None }, after_io.submitted - before_io.submitted, after_io.completed - before_io.completed));
            let pins: [Option<FolioRef>; 17] =
                core::array::from_fn(|index| inode.mapping().cached_folio(index as u64));
            let finite_none = {
                let _pin = PreemptGuard::new();
                pressure.exhaust()
            };
            context.expect_eq(finite_none, true);
            if !finite_none {
                return Err(TestError(
                    "slow consumer finite failure did not start at page None",
                ));
            }
            let mut finite = ReclaimAttempt::new();
            let io = disk.test_snapshot();
            if consumer == 0 {
                context.expect_eq(
                    file.mapping_inode()
                        .unwrap()
                        .file_page(17, &mut finite)
                        .err(),
                    Some(FsError::NoMemory),
                );
            } else {
                let address = if consumer == 1 {
                    FILE + PAGE_SIZE
                } else {
                    ANON + PAGE_SIZE
                };
                let error = if consumer == 1 {
                    VmError::File(FsError::NoMemory)
                } else {
                    VmError::NoMemory
                };
                context.expect_eq(mm.fault(address, UserAccess::Read), Err(error));
                context.expect_eq(mm.mapping(address).is_none(), true);
            }
            let finite_stats = finite.stats();
            context.expect_eq(finite_stats.freed_file + finite_stats.freed_tables, 0);
            context.expect_eq(
                finite_stats.scanned <= 64
                    && finite_stats.isolated <= 16
                    && finite_stats.rmap_checks <= 64,
                true,
            );
            context.expect_eq(disk.test_snapshot().submitted, io.submitted);
            context.expect_eq(
                mm.mapping(SHARED_ALIAS).map(|leaf| leaf.physical),
                Some(private),
            );
            mapped_bytes(context, &mm, SHARED_ALIAS, &[0x93]);
            context.diagnostic(format_args!("W33 slow consumer={} no-progress-page-none={} real-NoMemory=1 file-api-budget={:?} completed-is-not-freed=1 retained-canonical-pins={}", consumer, finite_none,
                if consumer == 0 { Some(finite_stats) } else { None }, pins.iter().flatten().count()));
            drop((pins, target));
            Ok(())
        })();
        drop(pressure);
        result?;
        context.expect_eq(checked(context, file.stat())?.size, (19 * PAGE_SIZE) as u64);
        bytes_at(context, &file, (16 * PAGE_SIZE) as u64, &[0x70; PAGE_SIZE])?;
        mapped_bytes(context, &mm, SHARED_ALIAS, &[0x93]);
        Ok(())
    }

    /// 原 Dirty 页有外部 pin 时只获得一次 WB 值进度，实际全堆 None 下由原 Work 提交且申请仍失败。
    #[test]
    #[ktest(
        tags("W33", "fs-reclaim", "fs-reserve", "fs-slow-consumer"),
        min_cpus = 2
    )]
    fn dirty_reclaim_requests_original_work_under_full_heap_none(
        context: &mut TestContext<'_>,
        fixture: &mut WriteOwner,
    ) -> TestResult {
        let file = fixture.cold_open(context)?;
        let inode = fixture.inodes.last().unwrap().clone();
        let volume = fixture.cold_volume.as_ref().unwrap().clone();
        let disk = volume.disk().clone();
        let domain = volume.writeback_domain().clone();
        checked(context, file.truncate(0))?;
        context.expect_eq(
            checked(context, file.write_at(0, &[0x81; 2 * PAGE_SIZE]))?,
            2 * PAGE_SIZE,
        );
        checked(context, file.truncate((3 * PAGE_SIZE) as u64))?;
        checked(context, file.sync_range(0..u64::MAX, false))?;
        let mut preparing = ReclaimAttempt::new();
        let dirty = checked(
            context,
            file.mapping_inode().unwrap().file_page(0, &mut preparing),
        )?;
        let clean = checked(
            context,
            file.mapping_inode().unwrap().file_page(1, &mut preparing),
        )?;
        let original = dirty.phys_addr();
        let block = volume.geometry().block_size;
        let mut targets = [0u64; 4];
        for (part, target) in targets.iter_mut().enumerate().take(PAGE_SIZE / block) {
            let BlockMapping::Mapped { physical, .. } =
                checked(context, inode.map_block(part as u64))?
            else {
                return Err(TestError("dirty slow consumer original disk block absent"));
            };
            *target = physical * block as u64;
        }
        let pressure = crate::cache::ktest::HeapPressure::prepare()?;
        let gate = Arc::try_new(Completion::new())
            .map_err(|_| TestError("dirty slow consumer gate OOM"))?;
        fixture.prepare_gate = Some(gate.clone());
        let entered = Arc::try_new(AtomicBool::new(false))
            .map_err(|_| TestError("dirty slow consumer observation OOM"))?;
        let evidence = Arc::try_new(SpinLock::new(None))
            .map_err(|_| TestError("dirty slow consumer result OOM"))?;
        let actor_entered = entered.clone();
        let actor_evidence = evidence.clone();
        let actor_file = file.clone();
        let actor_gate = gate.clone();
        sched::test_migrate_to(0);
        // WB 互斥只挡住原 Work 的提交；施压和 Normal 回收由另一份预建 Task 执行。
        let writeback = inode.mapping().lock_writeback();
        let actor = fixture.consume_sync(0, move || {
            actor_gate.wait();
            let mut pressure = pressure;
            let allocation = {
                let _pin = PreemptGuard::new();
                pressure.exhaust()
            };
            actor_entered.store(true, Ordering::Release);
            let mut attempt = ReclaimAttempt::new();
            let error = actor_file
                .mapping_inode()
                .unwrap()
                .file_page(2, &mut attempt)
                .err();
            let stats = attempt.stats();
            drop(pressure);
            *actor_evidence.lock() = Some((allocation, stats, error));
            error.map_or(Ok(()), Err)
        })?;
        context.expect_eq(
            checked(context, file.write_at(0, &[0x82; PAGE_SIZE]))?,
            PAGE_SIZE,
        );
        let dirty_mask = dirty.dirty_sectors();
        context.expect_eq(dirty_mask != 0, true);
        disk.test_flush_completions();
        let before_io = disk.test_snapshot();
        let before_domain = domain.snapshot();
        gate.complete();
        let waiting = until(
            "dirty slow consumer did not wait for original WB Work",
            || entered.load(Ordering::Acquire) && actor.blocked() && domain.snapshot().running,
        );
        drop(writeback);
        fixture.join_consumers()?;
        waiting?;
        let (allocation, stats, error) = evidence
            .lock()
            .take()
            .ok_or(TestError("dirty slow consumer result absent"))?;
        context.expect_eq(allocation, (true, true));
        context.expect_eq(error, Some(FsError::NoMemory));
        context.expect_eq(actor.result(), Some(Err(FsError::NoMemory)));
        context.expect_eq(
            stats.scanned <= 64 && stats.isolated <= 16 && stats.rmap_checks <= 64,
            true,
        );
        context.expect_eq(stats.freed_file, 0);
        context.expect_eq(stats.freed_tables, 0);
        context.expect_eq(0 < stats.submitted_bytes, true);
        context.expect_eq(0 < stats.completed_bytes, true);
        context.expect_eq(stats.completed_bytes <= stats.submitted_bytes, true);
        context.expect_eq(stats.io_error, false);
        until(
            "dirty slow consumer original Work did not finish its finite round",
            || {
                let state = domain.snapshot();
                !state.running && !state.pending
            },
        )?;
        disk.test_flush_completions();
        let after_domain = domain.snapshot();
        let after_io = disk.test_snapshot();
        context.expect_eq(
            before_domain.finished_rounds < after_domain.finished_rounds,
            true,
        );
        context.expect_eq(before_io.data_submitted < after_io.data_submitted, true);
        context.expect_eq(before_io.completed < after_io.completed, true);
        context.expect_eq(dirty.dirty_sectors(), 0);
        context.expect_eq(dirty.writeback_sectors(), 0);
        context.expect_eq(
            inode
                .mapping()
                .cached_folio(0)
                .map(|folio| folio.phys_addr()),
            Some(original),
        );
        context.expect_eq(
            inode
                .mapping()
                .cached_folio(1)
                .map(|folio| folio.phys_addr()),
            Some(clean.phys_addr()),
        );
        context.expect_eq(inode.mapping().cached_folio(2).is_none(), true);
        bytes_at(context, &file, 0, &[0x82; PAGE_SIZE])?;
        for target in targets.iter().copied().take(PAGE_SIZE / block) {
            disk_block_bytes(context, &disk, target, &[0x82; PAGE_SIZE][..block])?;
        }
        context.diagnostic(format_args!("W33 dirty-original-Work page-none={} heap-none={} original-physical={:#x} dirty-before={} stats={:?} domain-before={:?} domain-after={:?} IO-data-submitted={} IO-completed={} retained-original-pin=1 actual-freed=0 real-NoMemory={:?} independent-disk-bytes={}",
            allocation.0, allocation.1, original, dirty_mask, stats, before_domain, after_domain,
            after_io.data_submitted - before_io.data_submitted, after_io.completed - before_io.completed, error, PAGE_SIZE));
        Ok(())
    }

    /// 同步先等共同 metadata WB，持该互斥的正常冷 inode 读取仍能取得原 aux 并完成真实 IO。
    #[test]
    #[ktest(tags("W33", "fs-reserve", "fs-reserve-lock-order"), min_cpus = 2)]
    fn metadata_writeback_wait_leaves_aux_for_cold_inode_read(
        context: &mut TestContext<'_>,
        fixture: &mut WriteOwner,
    ) -> TestResult {
        let file = fixture.cold_open(context)?;
        let inode = fixture.inodes.last().unwrap().clone();
        let volume = fixture.cold_volume.as_ref().unwrap().clone();
        let disk = volume.disk().clone();
        let cache = volume.metadata_cache().clone();
        let source = cache
            .reserve()
            .ok_or(TestError("metadata lock regression RW source absent"))?;
        if !disk.limits().flush {
            return Err(TestError("metadata lock regression requires real FLUSH"));
        }
        checked(context, file.truncate(0))?;
        context.expect_eq(
            checked(context, file.write_at(0, &[0x76; PAGE_SIZE]))?,
            PAGE_SIZE,
        );
        checked(context, file.sync_range(0..u64::MAX, false))?;
        until(
            "metadata lock regression initial Work did not finish",
            || {
                let state = volume.writeback_domain().snapshot();
                !state.pending && !state.running
            },
        )?;
        let (expected, record_size) = inode_record_snapshot(context, &volume, inode.number())?;
        let geometry = volume.geometry();
        let descriptor_byte =
            (inode.number() - 1) / geometry.inodes_per_group * geometry.descriptor_size as u64;
        let descriptor = checked(
            context,
            cache
                .get(
                    geometry.first_data_block + 1 + descriptor_byte / geometry.block_size as u64,
                    geometry.block_size,
                    None,
                )
                .map_err(FsError::from),
        )?;
        let mut record = [0u8; 64];
        checked(
            context,
            descriptor
                .with_read(|bytes| {
                    let at = (descriptor_byte % geometry.block_size as u64) as usize;
                    record[..geometry.descriptor_size]
                        .copy_from_slice(&bytes[at..at + geometry.descriptor_size]);
                })
                .map_err(FsError::from),
        )?;
        drop(descriptor);
        let layout = checked(
            context,
            codec::group_layout(
                geometry,
                (inode.number() - 1) / geometry.inodes_per_group,
                &record[..geometry.descriptor_size],
            ),
        )?;
        let inode_byte = (inode.number() - 1) % geometry.inodes_per_group * record_size as u64;
        let table_block = layout.inode_table + inode_byte / geometry.block_size as u64;
        let gate = Arc::try_new(Completion::new())
            .map_err(|_| TestError("metadata lock regression gate OOM"))?;
        fixture.prepare_gate = Some(gate.clone());
        let entered = Arc::try_new(AtomicBool::new(false))
            .map_err(|_| TestError("metadata lock regression observer OOM"))?;
        let actor_entered = entered.clone();
        let syncing = file.clone();
        let actor_gate = gate.clone();
        let actor = fixture.consume_sync(0, move || {
            actor_gate.wait();
            actor_entered.store(true, Ordering::Release);
            syncing.sync_range(0..u64::MAX, false)
        })?;
        cache.prune_unused();
        let cold = checked(
            context,
            cache
                .cached_buffer(table_block, geometry.block_size)
                .map_err(FsError::from),
        )?
        .is_none();
        context.expect_eq(cold, true);
        if !cold {
            return Err(TestError(
                "metadata lock regression inode table did not become cold",
            ));
        }
        disk.test_flush_completions();
        context.expect_eq(disk.test_snapshot().inflight, 0);
        let before = disk.test_snapshot();
        let writeback = cache.lock_writeback();
        gate.complete();
        let result = (|| -> TestResult {
            until("metadata sync did not block on the held WB mutex", || {
                entered.load(Ordering::Acquire) && actor.blocked()
            })?;
            context.expect_eq(actor.result(), None);
            context.expect_eq(disk.test_snapshot().submitted, before.submitted);
            let free_aux = source.snapshot().aux_free;
            context.expect_eq(free_aux, true);
            context.diagnostic(format_args!("W33 metadata-lock-order inode={} cold-block={} sync-blocked=1 held-metadata-WB=1 aux-free-before-cold-read={}", inode.number(), table_block, free_aux));
            // 旧错误顺序已经占住 aux；先退出观察段并放 WB，不能在测试控制者中形成同一死锁。
            if !free_aux {
                return Err(TestError(
                    "metadata sync acquired aux before waiting for WB",
                ));
            }
            let (buffer, offset) = checked(context, volume.inode_record(inode.number(), None))?;
            let mut actual = [0u8; 256];
            checked(
                context,
                buffer
                    .with_read(|bytes| {
                        actual[..record_size].copy_from_slice(&bytes[offset..offset + record_size]);
                    })
                    .map_err(FsError::from),
            )?;
            context.expect_eq(&actual[..record_size], &expected[..record_size]);
            let decoded = checked(
                context,
                codec::decode_inode(geometry, inode.number(), &actual[..record_size]),
            )?;
            context.expect_eq(decoded.number, inode.number());
            context.expect_eq(decoded.size, PAGE_SIZE as u64);
            drop(buffer);
            disk.test_flush_completions();
            let cold_io = disk.test_snapshot();
            context.expect_eq(before.submitted < cold_io.submitted, true);
            context.expect_eq(before.completed < cold_io.completed, true);
            context.expect_eq(cold_io.inflight, 0);
            context.expect_eq(cold_io.flush_submitted, before.flush_submitted);
            context.expect_eq(source.snapshot().aux_free, true);
            context.expect_eq(actor.result(), None);
            context.diagnostic(format_args!("W33 metadata-lock-order full-record-bytes={} CRC-decoded=1 cold-read-submitted={} cold-read-completed={} aux-returned=1 sync-still-blocked=1", record_size, cold_io.submitted - before.submitted, cold_io.completed - before.completed));
            Ok(())
        })();
        drop(writeback);
        fixture.join_consumers()?;
        result?;
        context.expect_eq(actor.result(), Some(Ok(())));
        disk.test_flush_completions();
        let after = disk.test_snapshot();
        context.expect_eq(after.flush_submitted - before.flush_submitted, 1);
        context.expect_eq(after.inflight, 0);
        context.expect_eq(source.snapshot().aux_free, true);
        bytes_at(context, &file, 0, &[0x76; PAGE_SIZE])?;
        context.diagnostic(format_args!("W33 metadata-lock-order original-sync-FLUSH={} IO-submitted={} IO-completed={} joined=1 fulltail-aux-free={}", after.flush_submitted - before.flush_submitted, after.submitted - before.submitted, after.completed - before.completed, source.snapshot().aux_free));
        Ok(())
    }

    /// 只检查本请求的真实有限预算，受限作用域不得扫描 rmap 或产生回写进度等待。
    fn cache_only_attempt(context: &mut TestContext<'_>) -> usize {
        let mut attempt = ReclaimAttempt::new();
        let progress = attempt.reclaim();
        let stats = attempt.stats();
        context.expect_eq(stats.scanned <= 64, true);
        context.expect_eq(stats.isolated <= 16, true);
        context.expect_eq(stats.rmap_checks, 0);
        context.expect_eq(stats.unmapped, 0);
        context.expect_eq(stats.freed_tables, 0);
        context.expect_eq(stats.submitted_bytes, 0);
        context.expect_eq(stats.completed_bytes, 0);
        context.expect_eq(stats.io_error, false);
        context.expect_eq(progress, stats.freed_file != 0);
        context.diagnostic(format_args!(
            "W33 cache-only mode={:?} stats={:?}",
            ReclaimMode::current(),
            stats
        ));
        stats.freed_file
    }

    /// 普通 Read Bio 从设备读取准确 FS 块，构造错误保原原因，全部 DMA 收权后逐字节核验独立输入。
    fn disk_block_bytes(
        context: &mut TestContext<'_>,
        disk: &DiskRef,
        byte: u64,
        expected: &[u8],
    ) -> TestResult {
        let pages = mm::alloc_pages(0, true).ok_or(TestError("W33 disk readback page OOM"))?;
        let mapped = MappedPages::new(pages, 0, expected.len(), BufferDirection::DeviceToDriver)
            .map_err(|_| TestError("W33 disk readback mapping failed"))?;
        let mut memory = Vec::new_in(BioAllocator::Heap);
        memory
            .try_reserve_exact(1)
            .map_err(|_| TestError("W33 disk readback vector OOM"))?;
        memory.push(mapped);
        let bio = checked(
            context,
            // SAFETY: 唯一清零原页没有外部 CPU 别名，Read 的整个映射移交给同一真实 Bio；
            // 无回调，只有完整完成并领取原 memory 后才借用设备写入的字节。
            unsafe { Bio::try_new(disk, BlkOperation::Read, byte / 512, memory, 0, None) }
                .map_err(FsError::from),
        )?;
        let completion = bio.completion();
        disk.submit_bio(bio);
        let result = completion.wait();
        let memory = checked(context, completion.take_memory().map_err(FsError::from))?;
        if result
            != (IoResult {
                completed: expected.len(),
                error: None,
            })
        {
            return Err(TestError("W33 direct disk readback incomplete"));
        }
        context.expect_eq(memory.len(), 1);
        // SAFETY: 本次完整 Read 成功，completion 已结束设备访问；独占原 memory 保活，
        // 无回调、并发写者或裸别名，准确映射的全部字节已由实际设备初始化。
        context.expect_eq(unsafe { memory[0].bytes() }, expected);
        drop(memory);
        drop(completion);
        Ok(())
    }

    /// 从当前真实根挑选外部节点；不把 inline 根或普通数据块冒充冷 metadata。
    fn external_root_nodes(inode: &InodeRef, traditional: bool) -> Result<[u64; 4], FsError> {
        inode.with_map_read(|metadata, root| {
            if (metadata.flags & 0x80000 == 0) != traditional {
                return Err(FsError::InvalidOperation);
            }
            let mut nodes = [0; 4];
            if traditional {
                for (slot, index) in (12..15).enumerate() {
                    nodes[slot] = u64::from(codec::le32(root, index * 4)?);
                }
            } else {
                let header = codec::extent_header(root, None)?;
                if header.depth == 0 || nodes.len() < header.entries as usize {
                    return Err(FsError::InvalidOperation);
                }
                for (slot, node) in nodes.iter_mut().enumerate().take(header.entries as usize) {
                    let at = 12 + slot * 12;
                    *node = u64::from(codec::le32(root, at + 4)?)
                        | (u64::from(codec::le16(root, at + 8)?) << 32);
                }
            }
            if nodes.iter().all(|node| *node == 0) {
                return Err(FsError::InvalidOperation);
            }
            Ok(nodes)
        })
    }

    /// 记录真实常驻与新建 Task 成本，预备救援者已包含在同一 VM 的 live 基线内。
    fn reclaim_cost(context: &TestContext<'_>, phase: &str) {
        context.diagnostic(format_args!(
            "W33 cost phase={} live={:?} task-created={} task-stack-bytes={} cpus={} prebuilt-rescuers-in-live-baseline=1",
            phase, metadata_stats(), task::test_created_tasks(), 16 * PAGE_SIZE, cpu::count()
        ));
        for id in 0..cpu::count() {
            let pool = crate::workqueue::testing::pool_stats(id);
            context.diagnostic(format_args!(
                "W33 worker-cost phase={} cpu={} workers={} created={} retired={} create-failures={} rescued-executions={}",
                phase, id, pool.workers, pool.created, pool.retired, pool.create_failures, pool.rescued
            ));
        }
    }

    /// 两种真实外部块树在同源页与全堆耗尽时完成两个窗口原页 WB、冷节点读入及 FLUSH。
    /// 每次整窗原页准备和同步后的身份检查各共用一份有限回收预算。
    #[test]
    #[case("external_extent", false)]
    #[case("traditional_indirect", true)]
    #[ktest(tags("W33", "fs-reserve", "fs-reclaim"))]
    fn reserved_writeback_cold_tree_and_flush_under_full_pressure(
        context: &mut TestContext<'_>,
        fixture: &mut WriteOwner,
        traditional: bool,
    ) -> TestResult {
        fixture.prepare(context)?;
        let file = fixture.open(context, b"/test/write-tree", 2)?;
        let inode = fixture.inodes.last().unwrap().clone();
        let volume = fixture.volume();
        let disk = volume.disk().clone();
        let block = volume.geometry().block_size;
        if !disk.limits().flush || (traditional && block != 1024) {
            return Err(TestError(
                "W33 requires real FLUSH and the selected tree disk geometry",
            ));
        }
        checked(context, file.truncate(0))?;
        let source = volume
            .metadata_cache()
            .reserve()
            .ok_or(TestError("W33 RW reserve absent"))?;
        let domain = volume.writeback_domain().clone();
        // 唯一预备窗口在任何 inode invalidate/root/content/WB 依赖之前借入。
        // 后台可等待此 scratch，但本 Task 不等待后台轮，也不依赖其创建新的存储。
        let mut storage = checked(context, domain.borrow_storage())?;
        context.expect_eq(
            storage
                .metadata_io_context(&domain)
                .is_some_and(|io| io.belongs_to(&source)),
            true,
        );
        let mut folios: [Option<FolioRef>; 16] = core::array::from_fn(|_| None);
        let mut targets = [[0u64; 4]; 16];
        let mut offsets = [0u64; 16];
        let mut prepare_attempt = ReclaimAttempt::new();
        for index in 0..16 {
            offsets[index] = ((4 + index * 2) * PAGE_SIZE) as u64;
            let input = [0x40 + index as u8; PAGE_SIZE];
            context.expect_eq(
                checked(context, file.write_at(offsets[index], &input))?,
                PAGE_SIZE,
            );
            folios[index] = Some(checked(
                context,
                file.mapping_inode()
                    .unwrap()
                    .file_page(offsets[index] / PAGE_SIZE as u64, &mut prepare_attempt),
            )?);
            for part in 0..PAGE_SIZE / block {
                let BlockMapping::Mapped { physical, .. } = checked(
                    context,
                    inode.map_block((offsets[index] + (part * block) as u64) / block as u64),
                )?
                else {
                    return Err(TestError("W33 prepared data block is not mapped"));
                };
                targets[index][part] = physical * block as u64;
            }
        }
        let nodes = checked(context, external_root_nodes(&inode, traditional))?;
        let (record, _) = checked(context, volume.inode_record(inode.number(), None))?;
        // 冷节点必须先完成正式写盘；独立 Buffer 归还之后仅由原 prune 摘离静止 Node。
        checked(
            context,
            crate::fs::writeback::sync_inode_with_storage(&inode, 0..u64::MAX, &mut storage),
        )?;
        checked(
            context,
            crate::fs::writeback::sync_metadata_with_storage(&volume, &mut storage),
        )?;
        disk.test_flush_completions();
        for node in nodes.iter().copied().filter(|node| *node != 0) {
            let buffer = checked(context, volume.metadata_block(node))?;
            context.expect_eq(buffer.folio().dirty_sectors(), 0);
            context.expect_eq(buffer.folio().writeback_sectors(), 0);
            drop(buffer);
        }
        for index in 0..16 {
            let input = [0x70 + index as u8; PAGE_SIZE];
            context.expect_eq(
                checked(context, file.write_at(offsets[index], &input))?,
                PAGE_SIZE,
            );
            context.expect_eq(folios[index].as_ref().unwrap().dirty_sectors(), u8::MAX);
        }
        volume.metadata_cache().prune_unused();
        for node in nodes.iter().copied().filter(|node| *node != 0) {
            if checked(
                context,
                volume
                    .metadata_cache()
                    .cached_buffer(node, block)
                    .map_err(FsError::from),
            )?
            .is_some()
            {
                return Err(TestError(
                    "W33 external tree node did not become a real cold miss",
                ));
            }
        }
        sched::test_migrate_to(0);
        reclaim_cost(context, "pressure-ready");
        let layouts = source.snapshot();
        context.expect_eq(layouts.ready, true);
        context.expect_eq(layouts.metadata_capacity, 64);
        context.diagnostic(format_args!(
            "W33 source-before={:?} scratch-bytes={} node-payload-bytes={}",
            layouts,
            core::mem::size_of::<crate::fs::writeback::WbStorage>(),
            crate::cache::ktest_node_bytes()
        ));
        let before = disk.test_snapshot();
        let domain_before = domain.snapshot();
        let evidence = {
            let _pin = PreemptGuard::new();
            fixture.user.pressure().exhaust()
        };
        let pressured = (|| -> Result<_, FsError> {
            if evidence != (true, true) {
                return Err(FsError::NoMemory);
            }
            let mode = ReclaimMode::current();
            crate::fs::writeback::sync_inode_with_storage(&inode, 0..u64::MAX, &mut storage)?;
            disk.test_flush_completions();
            let data = disk.test_snapshot();
            let data_domain = domain.snapshot();
            let dma = disk.ktest_last_folio_dma();
            let after_data_source = source.snapshot();
            crate::fs::writeback::sync_metadata_with_storage(&volume, &mut storage)?;
            disk.test_flush_completions();
            Ok((
                mode,
                data,
                data_domain,
                dma,
                after_data_source,
                disk.test_snapshot(),
                domain.snapshot(),
                source.snapshot(),
            ))
        })();
        // 同步接口先消费全部已接受 IO 尾；解除真实压力后才归还 scratch/book 或建立读回存储。
        fixture.user.release_pressure();
        let (mode, data, data_domain, dma, data_source, after, domain_after, after_source) =
            checked(context, pressured)?;
        context.expect_eq(evidence, (true, true));
        context.expect_eq(mode, ReclaimMode::NoIo);
        context.expect_eq(data.metadata_submitted, before.metadata_submitted);
        context.expect_eq(data.flush_submitted, before.flush_submitted);
        context.expect_eq(
            0 < data.submitted - before.submitted - (data.data_submitted - before.data_submitted),
            true,
        );
        context.expect_eq(2 <= data.data_submitted - before.data_submitted, true);
        context.expect_eq(
            data.submitted - before.submitted,
            data.completed - before.completed,
        );
        context.expect_eq(data.inflight, 0);
        context.expect_eq(
            data_domain.submitted_bytes - domain_before.submitted_bytes,
            (16 * PAGE_SIZE) as u64,
        );
        context.expect_eq(
            data_domain.completed_bytes - domain_before.completed_bytes,
            (16 * PAGE_SIZE) as u64,
        );
        let (original, address, offset, length) =
            dma.ok_or(TestError("W33 real data SG sample absent"))?;
        context.expect_eq(
            folios
                .iter()
                .flatten()
                .any(|folio| folio.phys_addr() == original),
            true,
        );
        context.expect_eq(address, (original + offset) as u64);
        context.expect_eq(0 < length && offset + length <= PAGE_SIZE, true);
        context.expect_eq(0 < after.metadata_submitted - data.metadata_submitted, true);
        context.expect_eq(after.data_submitted, data.data_submitted);
        context.expect_eq(after.flush_submitted - data.flush_submitted, 1);
        context.expect_eq(
            after.submitted - data.submitted,
            after.completed - data.completed,
        );
        context.expect_eq(after.inflight, 0);
        context.expect_eq(
            domain_after.submitted_bytes - domain_before.submitted_bytes,
            domain_after.completed_bytes - domain_before.completed_bytes,
        );
        context.expect_eq(
            domain_after.accepted_bytes - domain_before.accepted_bytes,
            domain_after.completed_bytes - domain_before.completed_bytes,
        );
        for snapshot in [data_source, after_source] {
            context.expect_eq(snapshot.control_live, 0);
            context.expect_eq(snapshot.payload_live, 0);
            context.expect_eq(snapshot.leased, 0);
            context.expect_eq(snapshot.aux_free, true);
        }
        let mut verify_attempt = ReclaimAttempt::new();
        for (index, folio) in folios.iter().flatten().enumerate() {
            context.expect_eq(folio.dirty_sectors(), 0);
            context.expect_eq(folio.writeback_sectors(), 0);
            context.expect_eq(
                checked(
                    context,
                    file.mapping_inode()
                        .unwrap()
                        .file_page(offsets[index] / PAGE_SIZE as u64, &mut verify_attempt),
                )?
                .phys_addr(),
                folio.phys_addr(),
            );
            context.diagnostic(format_args!(
                "W33 original-page index={} file-offset={} physical={:#x}",
                index,
                offsets[index],
                folio.phys_addr()
            ));
        }
        context.expect_eq(record.folio().dirty_sectors(), 0);
        context.expect_eq(record.folio().writeback_sectors(), 0);
        drop(storage);
        let returned = source.snapshot();
        context.expect_eq(returned.free_wb, 8);
        context.expect_eq(returned.booked, 0);
        context.diagnostic(format_args!("W33 format={} page-null={} heap-null={} data-submitted={} cold-read-submitted={} data-stage-completed={} metadata-submitted={} flush-submitted={} metadata-flush-stage-completed={} domain-before={:?} domain-after={:?} recent-real-SG={:?} source-after={:?}",
            if traditional { "traditional" } else { "external-extent" }, evidence.0, evidence.1,
            data.data_submitted - before.data_submitted,
            data.submitted - before.submitted - (data.data_submitted - before.data_submitted),
            data.completed - before.completed, after.metadata_submitted - data.metadata_submitted,
            after.flush_submitted - data.flush_submitted, after.completed - data.completed,
            domain_before, domain_after, dma, returned));
        let readback = disk.test_snapshot();
        for index in 0..16 {
            let expected = [0x70 + index as u8; PAGE_SIZE];
            for part in 0..PAGE_SIZE / block {
                disk_block_bytes(
                    context,
                    &disk,
                    targets[index][part],
                    &expected[part * block..(part + 1) * block],
                )?;
            }
        }
        disk.test_flush_completions();
        let readback_after = disk.test_snapshot();
        context.expect_eq(
            readback_after.submitted - readback.submitted,
            16 * PAGE_SIZE / block,
        );
        context.expect_eq(
            readback_after.completed - readback.completed,
            16 * PAGE_SIZE / block,
        );
        context.diagnostic(format_args!(
            "W33 independent-device-readback-bytes={} expected-pages=16 cache-readback=0",
            16 * PAGE_SIZE
        ));
        reclaim_cost(context, "pressure-ended");
        Ok(())
    }

    /// 同 Task 嵌套及迁核保持回收限制；真实 FS 依赖下只收缓存，匿名页和原 Dirty/WB 不变。
    /// 原文件的两页准备共用请求预算，另一个 IO 文件保留自己的准备预算。
    #[test]
    #[ktest(tags("W33", "fs-reclaim", "fs-reserve"), min_cpus = 2)]
    fn task_scopes_migrate_cache_only_and_finite_no_memory(
        context: &mut TestContext<'_>,
        fixture: &mut WriteOwner,
    ) -> TestResult {
        fixture.prepare(context)?;
        let file = fixture.open(context, b"/test/write-reserve", 2)?;
        let inode = fixture.inodes.last().unwrap().clone();
        let volume = fixture.volume();
        let io_file = fixture.open(context, b"/test/write-user", 2)?;
        let io_inode = fixture.inodes.last().unwrap().clone();
        context.expect_eq(
            checked(context, file.write_at(0, &[0x51; 2 * PAGE_SIZE]))?,
            2 * PAGE_SIZE,
        );
        context.expect_eq(
            checked(context, io_file.write_at(0, &[0x52; PAGE_SIZE]))?,
            PAGE_SIZE,
        );
        checked(context, crate::fs::writeback::sync_inode(&inode))?;
        checked(context, crate::fs::writeback::sync_inode(&io_inode))?;
        checked(context, crate::fs::writeback::sync_metadata(&volume))?;
        let mut file_attempt = ReclaimAttempt::new();
        let clean = checked(
            context,
            file.mapping_inode()
                .unwrap()
                .file_page(0, &mut file_attempt),
        )?;
        let dirty = checked(
            context,
            file.mapping_inode()
                .unwrap()
                .file_page(1, &mut file_attempt),
        )?;
        let mut io_attempt = ReclaimAttempt::new();
        let wb = checked(
            context,
            io_file
                .mapping_inode()
                .unwrap()
                .file_page(0, &mut io_attempt),
        )?;
        let mm = vm(context, Mm::try_new().map_err(VmError::from))?;
        vm(
            context,
            mm.map_file(SOURCE, PAGE_SIZE, RO, RO, file.clone(), 0, false),
        )?;
        vm(context, mm.map_anonymous(SHARED_ALIAS, PAGE_SIZE, RW, RW))?;
        mapped_bytes(context, &mm, SOURCE, &[0x51]);
        context.expect_eq(mm.copyout(SHARED_ALIAS, &[0x93]).copied, 1);
        let anonymous = mm
            .mapping(SHARED_ALIAS)
            .ok_or(TestError("W33 anonymous page absent"))?
            .physical;
        let physical = clean.phys_addr();
        let (record, _) = checked(context, volume.inode_record(inode.number(), None))?;
        let metadata_physical = record.folio().phys_addr();
        let disk = volume.disk().clone();
        disk.test_flush_completions();
        let id = task::current_task().id();
        context.expect_eq(ReclaimMode::current(), ReclaimMode::Normal);
        let outer = ReclaimScope::nofs();
        context.expect_eq(ReclaimMode::current(), ReclaimMode::NoFs);
        {
            let inner = ReclaimScope::nofs();
            context.expect_eq(ReclaimMode::current(), ReclaimMode::NoFs);
            drop(inner);
            context.expect_eq(ReclaimMode::current(), ReclaimMode::NoFs);
            let noio = ReclaimScope::noio();
            context.expect_eq(ReclaimMode::current(), ReclaimMode::NoIo);
            for target in 0..cpu::count() {
                sched::test_migrate_to(target);
                context.expect_eq(cpu::current().id, target);
                context.expect_eq(task::current_task().id(), id);
                context.expect_eq(ReclaimMode::current(), ReclaimMode::NoIo);
            }
            drop(noio);
            context.expect_eq(ReclaimMode::current(), ReclaimMode::NoFs);
        }
        let fresh = fixture.consume_sync(1, || {
            if ReclaimMode::current() == ReclaimMode::Normal {
                Ok(())
            } else {
                Err(FsError::InvalidOperation)
            }
        })?;
        until("W33 new Task did not leave its original stack", || {
            fixture.consumers[1].as_ref().unwrap().try_join().is_some()
        })?;
        context.expect_eq(fixture.consumers[1].take().unwrap().join(), 0);
        context.expect_eq(fresh.result(), Some(Ok(())));
        context.expect_eq(ReclaimMode::current(), ReclaimMode::NoFs);
        let writeback_lock = inode.mapping().lock_writeback();
        context.expect_eq(
            checked(context, file.write_at(PAGE_SIZE as u64, &[0x61; PAGE_SIZE]))?,
            PAGE_SIZE,
        );
        context.expect_eq(
            checked(context, io_file.write_at(0, &[0x62; PAGE_SIZE]))?,
            PAGE_SIZE,
        );
        let ranges = checked(context, prepare_unlinked_original_writeback(&io_inode, &wb))?;
        let syncing_volume = volume.clone();
        disk.test_suspend_completions();
        let io = fixture.consume_sync(0, move || {
            let mut ranges = ranges;
            let mut bios = [const { None }; 8];
            let mut completions = [const { None }; 8];
            syncing_volume
                .claim()
                .writeback_batch(
                    &mut ranges,
                    None,
                    &mut bios,
                    &mut completions,
                    syncing_volume.writeback_domain(),
                    || false,
                )
                .map_err(FsError::from)
        })?;
        until("W33 original WB did not enter actual blocked IO", || {
            wb.writeback_sectors() != 0 && disk.test_snapshot().inflight != 0 && io.blocked()
        })?;
        let dirty_mask = dirty.dirty_sectors();
        let wb_mask = wb.writeback_sectors();
        context.expect_eq(dirty_mask != 0, true);
        context.expect_eq(wb_mask != 0, true);
        let before = disk.test_snapshot();
        {
            let invalidate = checked(context, inode.lock_invalidate())?;
            let limited = inode.with_map_read(|_, _| {
                context.expect_eq(ReclaimMode::current(), ReclaimMode::NoFs);
                cache_only_attempt(context);
                let noio = ReclaimScope::noio();
                context.expect_eq(ReclaimMode::current(), ReclaimMode::NoIo);
                cache_only_attempt(context);
                drop(noio);
                Ok(())
            });
            checked(context, limited)?;
            drop(invalidate);
        }
        context.expect_eq(dirty.dirty_sectors(), dirty_mask);
        context.expect_eq(wb.writeback_sectors(), wb_mask);
        context.expect_eq(
            mm.mapping(SOURCE).map(|mapping| mapping.physical),
            Some(physical),
        );
        context.expect_eq(
            mm.mapping(SHARED_ALIAS).map(|mapping| mapping.physical),
            Some(anonymous),
        );
        context.expect_eq(record.folio().phys_addr(), metadata_physical);
        context.expect_eq(disk.test_snapshot().submitted, before.submitted);
        context.expect_eq(disk.test_snapshot().completed, before.completed);
        // 一个共享请求预算先消费所有可收缓存；再用同一 Task 的真实 allocator 填满释放的存储。
        let mut attempt = ReclaimAttempt::new();
        for _ in 0..64 {
            let old = attempt.stats().scanned;
            attempt.reclaim();
            if attempt.stats().scanned == old {
                break;
            }
        }
        sched::test_migrate_to(0);
        reclaim_cost(context, "scope-pressure-ready");
        let evidence = {
            let _pin = PreemptGuard::new();
            fixture.user.pressure().exhaust()
        };
        let no_memory = (|| -> Result<_, FsError> {
            let _invalidate = inode.lock_invalidate()?;
            inode.with_map_read(|_, _| {
                let failed = inode
                    .mapping()
                    .prepare_file(&inode, 4096, &mut attempt)
                    .err();
                Ok((failed, attempt.stats(), ReclaimMode::current()))
            })
        })();
        fixture.user.release_pressure();
        let (error, stats, mode) = checked(context, no_memory)?;
        context.expect_eq(evidence, (true, true));
        context.expect_eq(error, Some(FsError::NoMemory));
        context.expect_eq(mode, ReclaimMode::NoFs);
        context.expect_eq(stats.scanned <= 64 && stats.isolated <= 16, true);
        context.expect_eq(stats.rmap_checks, 0);
        context.expect_eq(stats.submitted_bytes, 0);
        context.expect_eq(stats.completed_bytes, 0);
        context.expect_eq(dirty.dirty_sectors(), dirty_mask);
        context.expect_eq(wb.writeback_sectors(), wb_mask);
        drop(writeback_lock);
        drop(outer);
        context.expect_eq(ReclaimMode::current(), ReclaimMode::Normal);
        disk.test_resume_completions();
        fixture.join_consumers()?;
        context.expect_eq(io.result(), Some(Ok(())));
        drop(clean);
        let mut freed = 0;
        for _ in 0..4 {
            let mut normal = ReclaimAttempt::new();
            normal.reclaim();
            let result = normal.stats();
            freed += result.freed_file;
            context.expect_eq(
                result.scanned <= 64 && result.isolated <= 16 && result.rmap_checks <= 64,
                true,
            );
            context.diagnostic(format_args!("W33 normal stats={:?}", result));
            if inode.mapping().cached_folio(0).is_none() {
                break;
            }
        }
        context.expect_eq(inode.mapping().cached_folio(0).is_none(), true);
        context.expect_eq(mm.mapping(SOURCE).is_none(), true);
        context.expect_eq(0 < freed, true);
        context.expect_eq(
            mm.mapping(SHARED_ALIAS).map(|mapping| mapping.physical),
            Some(anonymous),
        );
        mapped_bytes(context, &mm, SHARED_ALIAS, &[0x93]);
        context.expect_eq(record.folio().phys_addr(), metadata_physical);
        context.diagnostic(format_args!("W33 scope task={} online-cpus={} nested-NOFS-NOFS-NOIO-restored=1 new-Task-normal=1 held-invalidate-root-cache-only=1 dirty-mask={} WB-mask={} active-metadata-physical={:#x} anonymous-physical={:#x} finite-NoMemory={:?} page-null={} heap-null={} normal-freed-file={}",
            id.as_usize(), cpu::count(), dirty_mask, wb_mask, metadata_physical, anonymous, error, evidence.0, evidence.1, freed));
        reclaim_cost(context, "scope-ended");
        Ok(())
    }

    /// 普通 fork 的两个文件叶经历真实撤叶和 CSD，refault 重读原盘，私有匿名修改保留。
    #[test]
    #[ktest(tags("W32", "fs-reclaim", "fs-shared", "user-fs"), min_cpus = 2)]
    fn clean_shared_leaves_refault_and_private_cow_survives(
        context: &mut TestContext<'_>,
        fixture: &mut WriteOwner,
    ) -> TestResult {
        const PARENT_FILE: usize = 0x4b00_0000;
        const PRIVATE: usize = 0x4c00_0000;
        const CHILD_FILE: usize = 0x4d00_0000;
        fixture.prepare(context)?;
        let file = fixture.open(context, b"/test/write-reserve", 2)?;
        let inode = fixture.inodes.last().unwrap().clone();
        let input = [0x62; PAGE_SIZE];
        for page in 0..8 {
            context.expect_eq(
                checked(context, file.write_at((page * PAGE_SIZE) as u64, &input))?,
                PAGE_SIZE,
            );
        }
        checked(context, file.sync_range(0..u64::MAX, false))?;
        let image = fixture.shared_image(context, b"R")?;
        let mm = image.mm.clone();
        fixture.start_shared(context, image)?;
        fixture.phase(&mm, 0, 1)?;
        let child = fixture.child(&mm)?;
        let child_mm = child
            .mm()
            .ok_or(TestError("reclaim fork child has no original MM"))?;
        let parent_file = mm
            .mapping(PARENT_FILE)
            .ok_or(TestError("reclaim parent did not fault its real file leaf"))?;
        let child_file = child_mm.mapping(CHILD_FILE).ok_or(TestError(
            "reclaim child did not fault its relocated file leaf",
        ))?;
        let parent_private = mm.mapping(PRIVATE).ok_or(TestError(
            "reclaim parent did not create private anonymous COW",
        ))?;
        let child_private = child_mm.mapping(PRIVATE).ok_or(TestError(
            "reclaim fork did not retain private anonymous COW",
        ))?;
        context.expect_eq(parent_file.physical, child_file.physical);
        context.expect_eq(parent_private.physical, child_private.physical);
        context.expect_eq(parent_file.physical != parent_private.physical, true);
        let controls = TruncateControls::new()?;
        fixture.truncate_controls = Some(controls.clone());
        let observing = controls.clone();
        inode.mapping().ktest_set_unmap_hook(Some(
            Arc::try_new(move |phase, pages, tables| {
                if pages == 0 {
                    return;
                }
                assert_eq!(pages, 2, "file reclaim retired a private anonymous leaf");
                let slot = match phase {
                    FileUnmapPhase::BeforeSync => &observing.before_sync,
                    FileUnmapPhase::AfterSync => &observing.after_sync,
                };
                let mut slot = slot.lock();
                assert!(
                    slot.is_none(),
                    "file reclaim repeated the same resident retirement"
                );
                *slot = Some((pages, tables));
            })
            .map_err(|_| TestError("reclaim CSD observation preparation OOM"))?,
        ));
        let gone = || {
            inode.mapping().cached_folio(0).is_none()
                && mm.mapping(PARENT_FILE).is_none()
                && child_mm.mapping(CHILD_FILE).is_none()
        };
        let mut freed_file = 0;
        let mut freed_tables = 0;
        let mut unmapped = 0;
        for round in 0..4 {
            let mut attempt = ReclaimAttempt::new();
            let _ = attempt.reclaim();
            let result = attempt.stats();
            context.expect_eq(result.scanned <= 64, true);
            context.expect_eq(result.isolated <= 16, true);
            context.expect_eq(result.rmap_checks <= 64, true);
            context.expect_eq(result.io_error, false);
            freed_file += result.freed_file;
            freed_tables += result.freed_tables;
            unmapped += result.unmapped;
            context.diagnostic(format_args!(
                "W32 reclaim-round={} result={:?}",
                round, result
            ));
            if gone() {
                break;
            }
        }
        inode.mapping().ktest_set_unmap_hook(None);
        context.expect_eq(gone(), true);
        context.expect_eq(0 < freed_file, true);
        context.expect_eq(2 <= unmapped, true);
        let before_sync = *controls.before_sync.lock();
        let after_sync = *controls.after_sync.lock();
        context.expect_eq(before_sync.map(|counts| counts.0), Some(2));
        context.expect_eq(after_sync, before_sync);
        context.expect_eq(
            mm.mapping(PRIVATE).map(|mapping| mapping.physical),
            Some(parent_private.physical),
        );
        context.expect_eq(
            child_mm.mapping(PRIVATE).map(|mapping| mapping.physical),
            Some(child_private.physical),
        );
        WriteOwner::release(&mm, 1)?;
        fixture.phase(&mm, 0, 2)?;
        fixture.phase(&mm, 8, 2)?;
        let refaulted = inode.mapping().cached_folio(0).ok_or(TestError(
            "ordinary refault did not load the canonical file page",
        ))?;
        context.expect_eq(
            mm.mapping(PARENT_FILE).map(|mapping| mapping.physical),
            Some(refaulted.phys_addr()),
        );
        context.expect_eq(
            child_mm.mapping(CHILD_FILE).map(|mapping| mapping.physical),
            Some(refaulted.phys_addr()),
        );
        context.expect_eq(
            mm.mapping(PRIVATE).map(|mapping| mapping.physical),
            Some(parent_private.physical),
        );
        context.expect_eq(
            child_mm.mapping(PRIVATE).map(|mapping| mapping.physical),
            Some(child_private.physical),
        );
        WriteOwner::release(&mm, 2)?;
        fixture.finish_shared(context)?;
        context.diagnostic(format_args!(
            "W32 original-file-physical={:#x} refault-file-physical={:#x} private-physical={:#x} retired-before={:?} retired-after={:?} freed-file={} freed-tables={} unmapped={} ordinary-fork-wait=0",
            parent_file.physical,
            refaulted.phys_addr(),
            parent_private.physical,
            before_sync,
            after_sync,
            freed_file,
            freed_tables,
            unmapped,
        ));
        Ok(())
    }

    /// 目标页确认为空洞后占住首原页的真实物理后继，多个父请求先发表并 kick，随后消费全部原尾。
    /// 十六页准备共用一份回收预算，完成后的整窗身份检查另用一份预算。
    #[test]
    #[ktest(tags("W10", "fs-sync"), min_cpus = 2)]
    fn original_folio_batch_publishes_prefix_before_wait(
        context: &mut TestContext<'_>,
        fixture: &mut WriteOwner,
    ) -> TestResult {
        fixture.prepare(context)?;
        let file = fixture.open(context, b"/test/write-reserve", 2)?;
        let inode = fixture.inodes.last().unwrap().clone();
        let volume = checked(context, inode.disk_volume())?.clone();
        let allocation_hint = (inode.number() - 1) / volume.geometry().inodes_per_group;
        let block = volume.geometry().block_size as u64;
        let base = checked(context, file.stat())?
            .size
            .div_ceil(PAGE_SIZE as u64)
            * PAGE_SIZE as u64;
        let end = base + 16 * PAGE_SIZE as u64;
        checked(
            context,
            inode.with_map_read(|metadata, root| {
                // 本地查询终点只放开 EOF 裁剪，不修改 canonical size 或块树。
                let mut lookup = *metadata;
                lookup.size = end;
                for logical in base / block..end / block {
                    let mapping = volume.map_block(&lookup, root, logical, None)?;
                    if !matches!(mapping, BlockMapping::Hole { .. }) {
                        context.diagnostic(format_args!(
                            "W10 preparation requires actual hole logical={} mapping={:?}",
                            logical, mapping
                        ));
                        return Err(FsError::InvalidOperation);
                    }
                }
                Ok(())
            }),
        )?;
        let mut folios: [Option<mm::FolioRef>; 16] = core::array::from_fn(|_| None);
        let mut held = Vec::new();
        let prepared = (|| -> Result<(u64, u64, u64, usize), TestError> {
            let mut prepare_attempt = ReclaimAttempt::new();
            context.expect_eq(checked(context, file.write_at(base, &[0xc4; 4096]))?, 4096);
            folios[0] = Some(checked(
                context,
                file.mapping_inode()
                    .unwrap()
                    .file_page(base / PAGE_SIZE as u64, &mut prepare_attempt),
            )?);
            let mapping = checked(context, inode.map_block(base / block))?;
            context.diagnostic(format_args!("W10 first-page mapping={:?}", mapping));
            let BlockMapping::Mapped {
                physical: first, ..
            } = mapping
            else {
                return Err(TestError("batch first file page not mapped"));
            };
            let target = first + PAGE_SIZE as u64 / block;
            let budget = volume.geometry().blocks_count - volume.geometry().first_data_block;
            // 本例没有并发分配者；second 的首 data 申请先于同次节点摘离，已占用目标此时不可申请。
            let mut target_state = checked(context, block_allocation_bit(&volume, target))?;
            context.diagnostic(format_args!(
                "W10 fragmentation first-physical={} target-successor={} target-allocation-state={:?} reservation-budget={} second-unprepared=1",
                first, target, target_state, budget
            ));
            let mut occupied = target_state == Some(true);
            let mut reserved_bounds: Option<(u64, u64)> = None;
            for _ in 0..budget {
                if occupied {
                    break;
                }
                // 容量准备先于新责任；同组较低空洞的 token 一并保留，直到真实目标位被占住。
                held.try_reserve(1)
                    .map_err(|_| TestError("W10 physical separator owner OOM"))?;
                let separator = checked(context, volume.allocate_block(allocation_hint))?;
                let physical = separator.physical();
                held.push(separator);
                reserved_bounds =
                    Some(reserved_bounds.map_or((physical, physical), |(low, high)| {
                        (low.min(physical), high.max(physical))
                    }));
                context.diagnostic(format_args!(
                    "W10 fragmentation first-physical={} target-successor={} separator-physical={} reserved-physical-bounds={:?} held-reservations={} second-unprepared=1",
                    first, target, physical, reserved_bounds, held.len()
                ));
                if physical == target {
                    occupied = true;
                    break;
                }
                if target_state.is_none() {
                    // 只在本次真实分配之后重核未知组；不把 BLOCK_UNINIT 的随机位当已拥有。
                    target_state = checked(context, block_allocation_bit(&volume, target))?;
                    occupied = target_state == Some(true);
                    context.diagnostic(format_args!(
                        "W10 target-after-real-allocation first-physical={} target-successor={} target-allocation-state={:?} held-reservations={}",
                        first, target, target_state, held.len()
                    ));
                }
            }
            if !occupied {
                return Err(TestError("W10 physical successor neither unavailable nor reserved within geometry block budget"));
            }
            context.diagnostic(format_args!(
                "W10 target-exclusion first-physical={} target-successor={} target-allocation-state={:?} exclusion-source={} reserved-physical-bounds={:?} held-reservations={}",
                first, target, target_state,
                if target_state == Some(true) { "existing-unavailable" } else { "owned-token" },
                reserved_bounds, held.len()
            ));
            let separators = held.len();
            let mut second = 0;
            for (index, slot) in folios.iter_mut().enumerate().skip(1) {
                context.expect_eq(
                    checked(
                        context,
                        file.write_at(base + (index * PAGE_SIZE) as u64, &[0xc4; 4096]),
                    )?,
                    4096,
                );
                *slot = Some(checked(
                    context,
                    file.mapping_inode()
                        .unwrap()
                        .file_page(base / PAGE_SIZE as u64 + index as u64, &mut prepare_attempt),
                )?);
                if index == 1 {
                    let mapping =
                        checked(context, inode.map_block((base + PAGE_SIZE as u64) / block))?;
                    context.diagnostic(format_args!(
                        "W10 second-page first-physical={} target-successor={} reserved-physical-bounds={:?} held-reservations={} mapping={:?}",
                        first, target, reserved_bounds, held.len(), mapping
                    ));
                    let BlockMapping::Mapped { physical, .. } = mapping else {
                        return Err(TestError("batch second file page not mapped"));
                    };
                    second = physical;
                    context.diagnostic(format_args!(
                        "W10 fragmentation first-physical={} target-successor={} second-physical={} reserved-physical-bounds={:?} held-reservations={}",
                        first, target, second, reserved_bounds, held.len()
                    ));
                    for separator in held.drain(..) {
                        volume.release_unpublished_block(separator);
                    }
                }
            }
            Ok((first, target, second, separators))
        })();
        // 任一步失败均先显式归还全部未发表 token；已接受文件内容仍由原 fixture 真实同步。
        for separator in held.drain(..) {
            volume.release_unpublished_block(separator);
        }
        let (first, target, second, separators) = prepared?;
        context.expect_eq(first + PAGE_SIZE as u64 / block != second, true);
        let disk = fixture.user.path.owner.disk()?.clone();
        let before = disk.test_snapshot();
        disk.test_suspend_completions();
        let syncing = inode.clone();
        let observed = fixture.consume_sync(0, move || {
            crate::fs::writeback::sync_inode_range(&syncing, base..base + 16 * PAGE_SIZE as u64)
        })?;
        until(
            "W10 accepted batch did not make progress before wait",
            || {
                1 < disk.test_snapshot().inflight
                    && before.data_submitted + 1 < disk.test_snapshot().data_submitted
                    && observed.blocked()
            },
        )?;
        let accepted = disk.test_snapshot();
        context.expect_eq(before.notifications < accepted.notifications, true);
        context.expect_eq(
            2 <= folios
                .iter()
                .flatten()
                .filter(|folio| folio.writeback_sectors() != 0)
                .count(),
            true,
        );
        disk.test_resume_completions();
        fixture.join_consumers()?;
        context.expect_eq(*observed.result.lock(), Some(Ok(())));
        context.expect_eq(
            folios
                .iter()
                .flatten()
                .all(|folio| folio.dirty_sectors() == 0 && folio.writeback_sectors() == 0),
            true,
        );
        disk.test_flush_completions();
        context.expect_eq(disk.test_snapshot().inflight, 0);
        let mut verify_attempt = ReclaimAttempt::new();
        for index in 0..16 {
            bytes_at(
                context,
                &file,
                base + (index * PAGE_SIZE) as u64,
                &[0xc4; 4096],
            )?;
            context.expect_eq(
                folios[index].as_ref().unwrap().phys_addr(),
                checked(
                    context,
                    file.mapping_inode()
                        .unwrap()
                        .file_page(base / PAGE_SIZE as u64 + index as u64, &mut verify_attempt),
                )?
                .phys_addr(),
            );
        }
        context.diagnostic(format_args!("W10 real-accepted-prefix-before-wait=1 original-folios=16 real-parent-and-queue-tails=1 first-physical={} target-successor={} second-physical={} all-unpublished-separators-released={} accepted-inflight={} notifications={} hardware-reverse-order-tested=0", first, target, second, separators, accepted.inflight, accepted.notifications - before.notifications));
        Ok(())
    }

    /// 正式数据、元数据与 FLUSH 后结束全部 FS 和 VMA 拥有，弱引用到期后重新读盘验证新根、CRC 和内容。
    #[test]
    #[ktest(tags("W07", "fs-write-cold", "fs-sync"))]
    fn formal_sync_drop_all_owners_and_cold_reload(
        context: &mut TestContext<'_>,
        fixture: &mut WriteOwner,
    ) -> TestResult {
        assert!(
            fixture.user.graph.is_none()
                && fixture.user.running.is_none()
                && fixture.shared_mm.is_none()
        );
        assert!(
            fixture.inodes.is_empty()
                && fixture.cold_volume.is_none()
                && fixture.cold_cache.is_none()
        );
        let disk = fixture.user.path.owner.disk()?.clone();
        let file = fixture.cold_open(context)?;
        let inode = fixture.inodes.last().unwrap().clone();
        let volume = fixture.cold_volume.as_ref().unwrap().clone();
        if !disk.limits().flush {
            return Err(TestError("W07 cold reload requires real FLUSH"));
        }
        let block = volume.geometry().block_size as u64;
        let base = checked(context, file.stat())?
            .size
            .div_ceil(PAGE_SIZE as u64)
            * PAGE_SIZE as u64;
        let traditional = checked(context, inode.metadata_snapshot())?.flags & 0x80000 == 0;
        let logical = if traditional {
            [0, 12, 268, 65804, 65806]
        } else {
            [0, 2, 4, 6, 8]
        };
        let mut physical = [0u64; 5];
        for (index, logical) in logical.iter().copied().enumerate() {
            let offset = base + logical * block;
            let input = [0x41 + index as u8; 4096];
            context.expect_eq(
                file.write_at(offset, &input[..block as usize]),
                Ok(block as usize),
            );
            let BlockMapping::Mapped {
                physical: mapped, ..
            } = checked(context, inode.map_block(offset / block))?
            else {
                return Err(TestError("cold producer block not mapped"));
            };
            physical[index] = mapped;
        }
        let mapped = tree(context, &inode)?;
        if !traditional {
            context.expect_eq(0 < mapped.depth, true);
        }
        let before = disk.test_snapshot();
        checked(context, file.sync_range(0..u64::MAX, false))?;
        let synced = disk.test_snapshot();
        context.expect_eq(before.data_submitted < synced.data_submitted, true);
        context.expect_eq(before.metadata_submitted < synced.metadata_submitted, true);
        context.expect_eq(synced.flush_submitted - before.flush_submitted, 1);
        let stat = checked(context, file.stat())?;
        let metadata = checked(context, inode.metadata_snapshot())?;
        let root = checked(context, inode.with_map_read(|_, root| Ok(*root)))?;
        let mm = Arc::try_new(vm(context, Mm::try_new().map_err(VmError::from))?)
            .map_err(|_| TestError("cold VMA owner OOM"))?;
        vm(
            context,
            mm.map_file(SOURCE, 4096, RO, RO, file.clone(), base, false),
        )?;
        mapped_bytes(context, &mm, SOURCE, &[0x41]);
        let (reopened, reads_before) = fixture.cold_reopen(context, mm, file, inode, volume)?;
        let reloaded_inode = fixture.inodes.last().unwrap().clone();
        let mut reloaded_stat = checked(context, reopened.stat())?;
        context.expect_eq(reloaded_stat.device != stat.device, true);
        // 新卷实例分配新的内核设备号；盘面属性仍须逐项相同。
        reloaded_stat.device = stat.device;
        context.expect_eq(reloaded_stat, stat);
        context.expect_eq(
            checked(context, reloaded_inode.metadata_snapshot())?,
            metadata,
        );
        context.expect_eq(
            checked(context, reloaded_inode.with_map_read(|_, root| Ok(*root)))?,
            root,
        );
        let reloaded = tree(context, &reloaded_inode)?;
        context.expect_eq(
            (
                reloaded.depth,
                reloaded.extents,
                reloaded.data,
                reloaded.nodes,
                &reloaded.blocks,
            ),
            (
                mapped.depth,
                mapped.extents,
                mapped.data,
                mapped.nodes,
                &mapped.blocks,
            ),
        );
        for (index, logical) in logical.iter().copied().enumerate() {
            let offset = base + logical * block;
            let expected = [0x41 + index as u8; 4096];
            bytes_at(context, &reopened, offset, &expected[..block as usize])?;
            let BlockMapping::Mapped {
                physical: mapped, ..
            } = checked(context, reloaded_inode.map_block(offset / block))?
            else {
                return Err(TestError("cold reloaded block not mapped"));
            };
            context.expect_eq(mapped, physical[index]);
        }
        context.expect_eq(reads_before < disk.test_snapshot().submitted, true);
        context.diagnostic(format_args!("W07 cold-reload original-MM-OFD-inode-cache-metadata-volume-ended=1 fresh-volume-and-cache=1 real-device-reads=1 root-and-all-blocks=1 production-inode-and-tree-codec=1 format={} base={}", if traditional { "traditional" } else { "extent" }, base));
        Ok(())
    }

    /// 真正在途的原页允许普通覆盖和映射再脏，旧父完成只撤自己的 WB，下一轮写出新义务。
    /// 原页由显式有限请求准备，随后持有同一凭证观察再脏与完成。
    #[test]
    #[ktest(tags("W09", "fs-sync", "fs-shared"), min_cpus = 2)]
    fn coherent_redirty_during_original_writeback(
        context: &mut TestContext<'_>,
        fixture: &mut WriteOwner,
    ) -> TestResult {
        fixture.prepare(context)?;
        let file = fixture.open(context, b"/test/write-reserve", 2)?;
        let offset = checked(context, file.stat())?
            .size
            .div_ceil(PAGE_SIZE as u64)
            * PAGE_SIZE as u64;
        context.expect_eq(file.write_at(offset, &[0x31; 4096]), Ok(4096));
        let inode = fixture.inodes.last().unwrap().clone();
        // 普通增长可能给旧 EOF 尾页留下合法义务，先真实同步它，再只覆盖本例新页。
        checked(context, crate::fs::writeback::sync_inode(&inode))?;
        context.expect_eq(file.write_at(offset, &[0x31; 4096]), Ok(4096));
        let volume = fixture.volume();
        context.expect_eq(
            (0..volume.dirty_inode_limit())
                .filter_map(|slot| volume.dirty_inode_at(slot))
                .any(|dirty| dirty.ptr_eq(&inode)),
            true,
        );
        let alias = shared_alias(context, &file, offset)?;
        let mut attempt = ReclaimAttempt::new();
        let folio = checked(
            context,
            file.mapping_inode()
                .unwrap()
                .file_page(offset / PAGE_SIZE as u64, &mut attempt),
        )?;
        let disk = fixture.user.path.owner.disk()?.clone();
        let before = disk.test_snapshot();
        disk.test_suspend_completions();
        let syncing = inode.clone();
        let observed = fixture.consume_sync(0, move || {
            crate::fs::writeback::sync_inode_range(&syncing, offset..offset + PAGE_SIZE as u64)
        })?;
        until("W09 original WB did not block with accepted IO", || {
            folio.writeback_sectors() != 0
                && disk.test_snapshot().inflight != 0
                && observed.blocked()
        })?;
        context.expect_eq(folio.dirty_sectors(), 0);
        let writing = file.clone();
        let mapping = alias.clone();
        let producer = fixture.consume_sync(1, move || {
            if writing.write_at(offset + 1, &[0x52])? != 1 {
                return Err(FsError::InvalidOperation);
            }
            let progress = mapping.copyout(SHARED_ALIAS + 2, &[0x73]);
            if progress.copied == 1 && progress.error.is_none() {
                Ok(())
            } else {
                Err(FsError::BadAddress)
            }
        })?;
        until(
            "W09 redirty producer did not reach a determinate state",
            || producer.result.lock().is_some() || producer.blocked(),
        )?;
        context.expect_eq(*producer.result.lock(), Some(Ok(())));
        context.expect_eq(
            alias.mapping(SHARED_ALIAS).unwrap().physical,
            folio.phys_addr(),
        );
        context.expect_eq(
            (folio.dirty_sectors() != 0, folio.writeback_sectors() != 0),
            (true, true),
        );
        bytes_at(context, &file, offset, &[0x31, 0x52, 0x73])?;
        disk.test_resume_completions();
        fixture.join_consumers()?;
        context.expect_eq(*observed.result.lock(), Some(Ok(())));
        context.expect_eq(folio.writeback_sectors(), 0);
        context.expect_eq(folio.dirty_sectors() != 0, true);
        context.expect_eq(
            (0..volume.dirty_inode_limit())
                .filter_map(|slot| volume.dirty_inode_at(slot))
                .any(|dirty| dirty.ptr_eq(&inode)),
            true,
        );
        let first = disk.test_snapshot();
        checked(
            context,
            crate::fs::writeback::sync_inode_range(&inode, offset..offset + PAGE_SIZE as u64),
        )?;
        context.expect_eq((folio.dirty_sectors(), folio.writeback_sectors()), (0, 0));
        context.expect_eq(before.data_submitted < first.data_submitted, true);
        context.expect_eq(
            first.data_submitted < disk.test_snapshot().data_submitted,
            true,
        );
        context.expect_eq(
            (0..volume.dirty_inode_limit())
                .filter_map(|slot| volume.dirty_inode_at(slot))
                .any(|dirty| dirty.ptr_eq(&inode)),
            false,
        );
        context.diagnostic(format_args!("W09 original-folio={} original-parent-blocked=1 ordinary-and-mapped-redirty=1 old-completion-preserves-dirty=1 next-round-progress=1", folio.phys_addr()));
        Ok(())
    }

    /// 两独立 OFD 各观察一次错误，dup 共游标；没有任何观察者的新错误仍由后来 open 报告。
    /// 错误观察结束后用独立有限请求取得原页检查剩余 Dirty/WB。
    #[test]
    #[ktest(tags("W11", "fs-sync", "controlled-ledger"))]
    fn independent_open_dup_and_unseen_old_errseq(
        context: &mut TestContext<'_>,
        fixture: &mut WriteOwner,
    ) -> TestResult {
        fixture.prepare(context)?;
        context.expect_eq(crate::cache::ktest_wb_error_protocol(), true);
        let first = fixture.open(context, b"/test/write-reserve", 2)?;
        let second = fixture.open(context, b"/test/write-reserve", 2)?;
        context.expect_eq(Arc::ptr_eq(&first, &second), false);
        let inode = fixture.inodes.last().unwrap().clone();
        let disk = fixture.user.path.owner.disk()?.clone();
        let table = checked(context, FileTable::try_new(4))?;
        let fd =
            checked(context, table.reserve())?.publish(first.clone(), DescriptorFlags::default());
        let duplicate = checked(context, table.dup(fd))?;
        let duplicated = checked(context, table.lookup(duplicate))?;
        context.expect_eq(Arc::ptr_eq(&first, &duplicated), true);
        context.expect_eq(first.write_at(0, &[0x91; 4096]), Ok(4096));
        disk.test_fail_next_data(BlockError::Io);
        let error = Err(FsError::Io(BlockError::Io));
        context.expect_eq(first.sync_range(0..u64::MAX, false), error);
        context.expect_eq(disk.test_completion_error_pending(), false);
        let failed = disk.test_snapshot();
        context.expect_eq(second.sync_range(0..u64::MAX, false), error);
        context.expect_eq(duplicated.sync_range(0..u64::MAX, false), Ok(()));
        context.expect_eq(first.sync_range(0..u64::MAX, false), Ok(()));
        let seen = fixture.open(context, b"/test/write-reserve", 2)?;
        context.expect_eq(seen.sync_range(0..u64::MAX, false), Ok(()));
        context.expect_eq(disk.test_snapshot().data_submitted, failed.data_submitted);
        context.expect_eq(first.write_at(0, &[0xa2; 4096]), Ok(4096));
        disk.test_fail_next_data(BlockError::Io);
        context.expect_eq(crate::fs::writeback::sync_inode(&inode), error);
        context.expect_eq(disk.test_completion_error_pending(), false);
        let unseen = fixture.open(context, b"/test/write-reserve", 2)?;
        context.expect_eq(unseen.sync_range(0..u64::MAX, false), error);
        context.expect_eq(unseen.sync_range(0..u64::MAX, false), Ok(()));
        context.expect_eq(first.sync_range(0..u64::MAX, false), error);
        context.expect_eq(duplicated.sync_range(0..u64::MAX, false), Ok(()));
        context.expect_eq(second.sync_range(0..u64::MAX, false), error);
        let mut attempt = ReclaimAttempt::new();
        let folio = checked(
            context,
            first.mapping_inode().unwrap().file_page(0, &mut attempt),
        )?;
        context.expect_eq((folio.dirty_sectors(), folio.writeback_sectors()), (0, 0));
        context.diagnostic(format_args!("W11 real-used-controlled-EIO=1 two-independent-observers=1 dup-shared-cursor=1 seen-new-open-skips=1 unseen-new-open-reports=1 accepted-error-no-replay=1"));
        Ok(())
    }

    /// 页已清脏但原 WB 未完成时，普通 sys82 必须真实 Blocked，放行真实完成后才返回。
    /// 在暂停完成前以显式有限请求取得待观察的原页。
    #[test]
    #[ktest(tags("W12", "fs-sync", "user-fs"), min_cpus = 2)]
    fn fsync_waits_for_existing_clean_writeback(
        context: &mut TestContext<'_>,
        fixture: &mut WriteOwner,
    ) -> TestResult {
        fixture.prepare(context)?;
        let file = fixture.open(context, b"/test/write-reserve", 2)?;
        context.expect_eq(file.write_at(0, &[0x45; 4096]), Ok(4096));
        let inode = fixture.inodes.last().unwrap().clone();
        let mut attempt = ReclaimAttempt::new();
        let folio = checked(
            context,
            file.mapping_inode().unwrap().file_page(0, &mut attempt),
        )?;
        let image = fixture.shared_image(context, b"b")?;
        let mm = image.mm.clone();
        fixture.start_shared(context, image)?;
        fixture.phase(&mm, 0, 1)?;
        let executor = fixture
            .user
            .graph()
            .lookup_thread(fixture.user.running.as_ref().unwrap().pid)
            .and_then(|user| user.executor())
            .ok_or(TestError("W12 user executor missing"))?;
        let disk = fixture.user.path.owner.disk()?.clone();
        disk.test_suspend_completions();
        let syncing = inode.clone();
        let first = fixture.consume_sync(0, move || crate::fs::writeback::sync_inode(&syncing))?;
        until("W12 clean original page not in WB", || {
            folio.dirty_sectors() == 0
                && folio.writeback_sectors() != 0
                && disk.test_snapshot().inflight != 0
                && first.blocked()
        })?;
        WriteOwner::release(&mm, 1)?;
        until("W12 sys82 did not really block on existing WB", || {
            sched::test_state(&executor) == State::Blocked
        })?;
        context.expect_eq(mm.load_user_u32(SHARED_CONTROL), Ok(1));
        context.expect_eq(folio.dirty_sectors(), 0);
        context.expect_eq(folio.writeback_sectors() != 0, true);
        context.expect_eq(
            fixture
                .user
                .running
                .as_ref()
                .unwrap()
                .join
                .try_join()
                .is_none(),
            true,
        );
        disk.test_resume_completions();
        fixture.join_consumers()?;
        context.expect_eq(*first.result.lock(), Some(Ok(())));
        fixture.phase(&mm, 0, 2)?;
        WriteOwner::release(&mm, 2)?;
        fixture.finish_shared(context)?;
        context.expect_eq((folio.dirty_sectors(), folio.writeback_sectors()), (0, 0));
        context.diagnostic(format_args!("W12 Dirty0-WB1=1 original-data-parent-blocked=1 ordinary-sys82-blocked=1 completion-before-return=1"));
        Ok(())
    }

    /// 数据真实完成后才消费一次元数据或 FLUSH 错误，显式后次同步和静止窗口都不得自动 replay。
    /// 错误后原页检查使用独立有限请求，不改变完成错误或重放数据。
    #[test]
    #[case("metadata", false)]
    #[case("flush", true)]
    #[ktest(tags("W13", "fs-sync", "controlled-ledger"))]
    fn data_then_metadata_flush_error_without_replay(
        context: &mut TestContext<'_>,
        fixture: &mut WriteOwner,
        flush: bool,
    ) -> TestResult {
        fixture.prepare(context)?;
        let file = fixture.open(context, b"/test/write-reserve", 2)?;
        let disk = fixture.user.path.owner.disk()?.clone();
        if !disk.limits().flush {
            return Err(TestError("W13 requires negotiated real FLUSH"));
        }
        context.expect_eq(file.write_at(0, &[0xb3; 4096]), Ok(4096));
        let before = disk.test_snapshot();
        if flush {
            disk.test_fail_next_flush(BlockError::Io);
        } else {
            disk.test_fail_next_metadata(BlockError::Io);
        }
        context.expect_eq(
            file.sync_range(0..u64::MAX, false),
            Err(FsError::Io(BlockError::Io)),
        );
        context.expect_eq(disk.test_completion_error_pending(), false);
        let failed = disk.test_snapshot();
        context.expect_eq(before.data_submitted < failed.data_submitted, true);
        context.expect_eq(before.metadata_submitted < failed.metadata_submitted, true);
        if flush {
            context.expect_eq(failed.flush_submitted - before.flush_submitted, 1);
        }
        let mut attempt = ReclaimAttempt::new();
        let folio = checked(
            context,
            file.mapping_inode().unwrap().file_page(0, &mut attempt),
        )?;
        context.expect_eq((folio.dirty_sectors(), folio.writeback_sectors()), (0, 0));
        disk.test_flush_completions();
        timer::sleep(time::Duration::from_millis(10));
        context.expect_eq(disk.test_snapshot().submitted, failed.submitted);
        context.expect_eq(file.sync_range(0..u64::MAX, false), Ok(()));
        let next = disk.test_snapshot();
        context.expect_eq(next.data_submitted, failed.data_submitted);
        context.expect_eq(next.metadata_submitted, failed.metadata_submitted);
        context.expect_eq(next.flush_submitted - failed.flush_submitted, 1);
        bytes_at(context, &file, 0, &[0xb3; 4096])?;
        context.diagnostic(format_args!("W13 kind={} actual-used-controlled-EIO=1 data-before-error=1 automatic-replay=0 explicit-next-sync-data-replay=0 explicit-next-sync-metadata-replay=0", if flush { "FLUSH" } else { "metadata" }));
        Ok(())
    }

    /// 小块几何核同原页的精确 Dirty 交集；原 OFD 与 VMA 偏移决定写出范围，private COW 和异步不写源页。
    /// 两页初始准备共享预算，各次同步后的身份检查分别使用独立有限请求。
    #[test]
    #[ktest(tags("W14", "fs-sync", "fs-shared", "user-fs"))]
    fn msync_ranges_private_and_ordinary_abi(
        context: &mut TestContext<'_>,
        fixture: &mut WriteOwner,
    ) -> TestResult {
        fixture.prepare(context)?;
        let file = fixture.open(context, b"/test/write-reserve", 2)?;
        context.expect_eq(file.write_at(0, &[0x66; 8192]), Ok(8192));
        let inode = fixture.inodes.last().unwrap().clone();
        checked(context, crate::fs::writeback::sync_inode(&inode))?;
        let mut prepare_attempt = ReclaimAttempt::new();
        let first = checked(
            context,
            file.mapping_inode()
                .unwrap()
                .file_page(0, &mut prepare_attempt),
        )?;
        let second = checked(
            context,
            file.mapping_inode()
                .unwrap()
                .file_page(1, &mut prepare_attempt),
        )?;
        let block = inode.block_size() as usize;
        let disk = fixture.user.path.owner.disk()?.clone();
        if block < PAGE_SIZE {
            let (first_mask, neighbor_mask) = match block {
                1024 => (0x03u8, 0x0cu8),
                2048 => (0x0fu8, 0xf0u8),
                _ => return Err(TestError("W14 unsupported small FS block geometry")),
            };
            let original = first.phys_addr();
            context.expect_eq((first.dirty_sectors(), first.writeback_sectors()), (0, 0));
            context.expect_eq(checked(context, file.write_at(64, &[0xa6; 3]))?, 3);
            context.expect_eq(
                checked(context, file.write_at(block as u64 + 64, &[0xb7; 3]))?,
                3,
            );
            context.expect_eq(
                (first.dirty_sectors(), first.writeback_sectors()),
                (first_mask | neighbor_mask, 0),
            );
            let before_range = disk.test_snapshot();
            checked(context, file.sync_range(64..67, true))?;
            context.expect_eq(
                (first.dirty_sectors(), first.writeback_sectors()),
                (neighbor_mask, 0),
            );
            let mut range_attempt = ReclaimAttempt::new();
            context.expect_eq(
                checked(
                    context,
                    file.mapping_inode()
                        .unwrap()
                        .file_page(0, &mut range_attempt),
                )?
                .phys_addr(),
                original,
            );
            context.expect_eq(
                before_range.data_submitted < disk.test_snapshot().data_submitted,
                true,
            );
            bytes_at(context, &file, 64, &[0xa6; 3])?;
            bytes_at(context, &file, block as u64 + 64, &[0xb7; 3])?;
            let before_neighbor = disk.test_snapshot();
            checked(
                context,
                file.sync_range(block as u64 + 64..block as u64 + 67, true),
            )?;
            context.expect_eq((first.dirty_sectors(), first.writeback_sectors()), (0, 0));
            let mut neighbor_attempt = ReclaimAttempt::new();
            context.expect_eq(
                checked(
                    context,
                    file.mapping_inode()
                        .unwrap()
                        .file_page(0, &mut neighbor_attempt),
                )?
                .phys_addr(),
                original,
            );
            context.expect_eq(
                before_neighbor.data_submitted < disk.test_snapshot().data_submitted,
                true,
            );
            bytes_at(context, &file, 64, &[0xa6; 3])?;
            bytes_at(context, &file, block as u64 + 64, &[0xb7; 3])?;
            bytes_at(context, &file, 29, &[0x66])?;
            context.diagnostic(format_args!("W14 small-FS-block-range block-size={} original-folio={} initial-dirty-mask={:#04x} neighbor-dirty-mask={:#04x} first-intersection-only=1 neighbor-explicit-sync=1 WB0-after-each-range=1", block, original, first_mask | neighbor_mask, neighbor_mask));
        } else {
            context.diagnostic(format_args!("W14 small-FS-block-range block-size={} branch-exercised=0 same-folio-neighbor-unavailable=1", block));
        }
        let mm = vm(context, Mm::try_new().map_err(VmError::from))?;
        vm(
            context,
            mm.map_file(SOURCE, 4096, RW, RW, file.clone(), 4096, false),
        )?;
        vm(
            context,
            mm.map_file(SHARED_ALIAS, 4096, RW, RW, file.clone(), 0, true),
        )?;
        context.expect_eq(mm.copyout(SOURCE + 29, &[0x77]).copied, 1);
        context.expect_eq(mm.copyout(SHARED_ALIAS + 29, &[0x88]).copied, 1);
        let before = disk.test_snapshot();
        vm(
            context,
            mm.sync_mapping_range(SOURCE..SOURCE + 4096, false, true),
        )?;
        context.expect_eq(disk.test_snapshot().submitted, before.submitted);
        context.expect_eq(mm.mapping(SOURCE).unwrap().permissions.write, true);
        vm(
            context,
            mm.sync_mapping_range(SHARED_ALIAS..SHARED_ALIAS + 4096, true, false),
        )?;
        context.expect_eq(disk.test_snapshot().submitted, before.submitted);
        context.expect_eq(second.dirty_sectors() != 0, true);
        context.expect_eq(first.dirty_sectors(), 0);
        context.expect_eq(
            mm.sync_mapping_range(SOURCE..SOURCE + 8192, true, false),
            Err(VmError::Unmapped),
        );
        context.expect_eq((second.dirty_sectors(), second.writeback_sectors()), (0, 0));
        context.expect_eq(
            before.data_submitted < disk.test_snapshot().data_submitted,
            true,
        );
        bytes_at(context, &file, 4096 + 29, &[0x77])?;
        bytes_at(context, &file, 29, &[0x66])?;
        mapped_bytes(context, &mm, SHARED_ALIAS + 29, &[0x88]);
        drop(mm);
        let image = fixture.shared_image(context, b"y")?;
        let user_mm = image.mm.clone();
        fixture.start_shared(context, image)?;
        fixture.phase(&user_mm, 0, 1)?;
        let no_io = disk.test_snapshot();
        WriteOwner::release(&user_mm, 1)?;
        fixture.phase(&user_mm, 0, 2)?;
        context.expect_eq(disk.test_snapshot().submitted, no_io.submitted);
        context.expect_eq(second.dirty_sectors() != 0, true);
        WriteOwner::release(&user_mm, 2)?;
        fixture.finish_shared(context)?;
        context.diagnostic(format_args!("W14 nonzero-file-offset=1 range-only-page=1 private-and-async-no-IO=1 invalidate-no-purge=1 hole-ENOMEM-after-progress=1 ordinary-sys82-83-227-flags-and-range=1"));
        Ok(())
    }

    /// 同步 write 交数据和 cursor 后放写锁，独立覆盖可前进；用户完成 phase2 后才观察自己的错误完成。
    /// 暂停完成前的原页观察由一份显式有限请求准备。
    #[test]
    #[case("O_SYNC", b"d" as &[u8])]
    #[case("O_DSYNC", b"z" as &[u8])]
    #[ktest(tags("W14", "fs-sync", "user-fs"), min_cpus = 2)]
    fn synchronous_write_releases_inode_and_cursor_locks(
        context: &mut TestContext<'_>,
        fixture: &mut WriteOwner,
        mode: &[u8],
    ) -> TestResult {
        fixture.prepare(context)?;
        let file = fixture.open(context, b"/test/write-reserve", 2)?;
        context.expect_eq(file.write_at(0, &[0x22; 4096]), Ok(4096));
        let inode = fixture.inodes.last().unwrap().clone();
        checked(context, file.sync_range(0..u64::MAX, false))?;
        let image = fixture.shared_image(context, mode)?;
        let mm = image.mm.clone();
        fixture.start_shared(context, image)?;
        fixture.phase(&mm, 0, 1)?;
        let user = fixture
            .user
            .graph()
            .lookup_thread(fixture.user.running.as_ref().unwrap().pid)
            .ok_or(TestError("sync writer user missing"))?;
        let executor = user
            .executor()
            .ok_or(TestError("sync writer executor missing"))?;
        let user_fd = mm
            .read_usize(SHARED_CONTROL + 16)
            .map_err(|_| TestError("sync writer fd missing"))?;
        let original = checked(
            context,
            user.runtime()
                .ok_or(TestError("sync writer runtime missing"))?
                .files
                .lookup(user_fd),
        )?;
        let mut attempt = ReclaimAttempt::new();
        let folio = checked(
            context,
            file.mapping_inode().unwrap().file_page(0, &mut attempt),
        )?;
        let disk = fixture.user.path.owner.disk()?.clone();
        disk.test_suspend_completions();
        disk.test_fail_next_data(BlockError::Io);
        WriteOwner::release(&mm, 1)?;
        until("O_SYNC write did not block in actual accepted WB", || {
            folio.writeback_sectors() != 0
                && disk.test_snapshot().inflight != 0
                && sched::test_state(&executor) == State::Blocked
        })?;
        let cursor = original.clone();
        let writing = file.clone();
        let unlocked = fixture.consume_sync(0, move || {
            if cursor.seek(SeekFrom::Current(0))? != 1 {
                return Err(FsError::InvalidOperation);
            }
            if writing.write_at(1, &[0x33])? != 1 {
                return Err(FsError::InvalidOperation);
            }
            Ok(())
        })?;
        until(
            "sync write lock probe did not reach a determinate state",
            || unlocked.result.lock().is_some() || unlocked.blocked(),
        )?;
        context.expect_eq(*unlocked.result.lock(), Some(Ok(())));
        context.expect_eq(folio.dirty_sectors() != 0, true);
        disk.test_resume_completions();
        fixture.join_consumers()?;
        fixture.phase(&mm, 0, 2)?;
        context.expect_eq(disk.test_completion_error_pending(), false);
        bytes_at(context, &file, 0, &[0x39, 0x33])?;
        context.expect_eq(original.seek(SeekFrom::Current(0)), Ok(1));
        WriteOwner::release(&mm, 2)?;
        fixture.finish_shared(context)?;
        checked(context, crate::fs::writeback::sync_inode(&inode))?;
        context.diagnostic(format_args!("W14 mode={} actual-sync-write-blocked=1 inode-lock-released=1 OFD-cursor-lock-released=1 accepted-data-and-cursor-not-rolled-back-on-EIO=1", if mode == b"d" { "O_SYNC" } else { "O_DSYNC" }));
        Ok(())
    }

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
        context.expect_eq(
            checked(context, file.write_from(&mut source, FileIoContext::Kernel))?,
            17,
        );
        modified_between(context, &file, &inode, prefix_write, time::now().as_nanos())?;
        context.expect_eq(checked(context, file.seek(SeekFrom::Current(0)))?, 17);
        context.expect_eq(checked(context, file.stat())?.size, 5120);
        bytes_at(context, &file, 0, &[0x72; 17])?;
        let mut source = checked(
            context,
            WriteBuffer::user(&address_space, SOURCE + 4096, 64),
        )?;
        let before_fault = checked(context, file.stat())?;
        context.expect_eq(
            file.write_from(&mut source, FileIoContext::Kernel),
            Err(FsError::BadAddress),
        );
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
            checked(
                context,
                mapped.write_at_from(8192, &mut source, FileIoContext::Kernel),
            )?,
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
        let record = volume.load_inode(unpublished.number(), None);
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
        checked(context, crate::fs::writeback::sync_metadata(&volume))?;
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
        let writer = checked(
            context,
            open_path_with_flags(
                fixture.user.path.vfs(),
                path.clone(),
                1,
                FileIoContext::Kernel,
            ),
        )?;
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
            open_path_with_flags(
                fixture.user.path.vfs(),
                path.clone(),
                1,
                FileIoContext::Kernel,
            )
            .err(),
            Some(FsError::TextBusy),
        );
        context.expect_eq(
            open_path_with_flags(
                fixture.user.path.vfs(),
                path.clone(),
                1 | 0x200,
                FileIoContext::Kernel,
            )
            .err(),
            Some(FsError::TextBusy),
        );
        let child = vm(context, image.mm.fork())?;
        let old_mm = image.mm.clone();
        drop(image);
        context.expect_eq(
            open_path_with_flags(
                fixture.user.path.vfs(),
                path.clone(),
                1,
                FileIoContext::Kernel,
            )
            .err(),
            Some(FsError::TextBusy),
        );
        drop(old_mm);
        context.expect_eq(
            open_path_with_flags(
                fixture.user.path.vfs(),
                path.clone(),
                1,
                FileIoContext::Kernel,
            )
            .err(),
            Some(FsError::TextBusy),
        );
        drop(child);
        drop(checked(
            context,
            open_path_with_flags(
                fixture.user.path.vfs(),
                path.clone(),
                1,
                FileIoContext::Kernel,
            ),
        )?);
        let mm = vm(context, Mm::try_new().map_err(VmError::from))?;
        vm(
            context,
            mm.map_file(SOURCE, 4096, RO, RO, program.clone(), 0, true),
        )?;
        drop(checked(
            context,
            open_path_with_flags(
                fixture.user.path.vfs(),
                path.clone(),
                1,
                FileIoContext::Kernel,
            ),
        )?);
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
            open_path_with_flags(
                fixture.user.path.vfs(),
                path.clone(),
                1,
                FileIoContext::Kernel,
            )
            .err(),
            Some(FsError::TextBusy),
        );
        drop(prepared);
        drop(checked(
            context,
            open_path_with_flags(
                fixture.user.path.vfs(),
                path.clone(),
                1,
                FileIoContext::Kernel,
            ),
        )?);
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
        drop(checked(
            context,
            open_path_with_flags(
                fixture.user.path.vfs(),
                path.clone(),
                1,
                FileIoContext::Kernel,
            ),
        )?);
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
        drop(checked(
            context,
            open_path_with_flags(fixture.user.path.vfs(), path, 1, FileIoContext::Kernel),
        )?);
        fixture.run(context, b"e")
    }

    /// 两个实际 Mm 的同源尾页保留双向内容；EOF 外 store 不分配，普通增长清新有效 gap。
    /// 已映射尾页仍经正式有限准备请求取得 canonical 原页来核对物理身份。
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
        let mut attempt = ReclaimAttempt::new();
        let folio = checked(
            context,
            file.mapping_inode()
                .unwrap()
                .file_page(page_offset / PAGE_SIZE as u64, &mut attempt),
        )?;
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
    /// 同步前使用一份有限请求取得 canonical 原页，保留撤权与 refault 的身份断言。
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
        let mut attempt = ReclaimAttempt::new();
        let folio = checked(
            context,
            file.mapping_inode().unwrap().file_page(0, &mut attempt),
        )?;
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
    /// 新旧文件的原页分别经独立有限请求准备，再保活到同址 remap 检查结束。
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
        let mut replacement_attempt = ReclaimAttempt::new();
        let replacement_folio = checked(
            context,
            replacement
                .mapping_inode()
                .unwrap()
                .file_page(0, &mut replacement_attempt),
        )?;
        let mut old_attempt = ReclaimAttempt::new();
        let old_folio = checked(
            context,
            old.mapping_inode().unwrap().file_page(0, &mut old_attempt),
        )?;
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
                    inode: replacement
                        .mapping_inode()
                        .expect("regular replacement has no mapping inode")
                        .clone(),
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

    /// 真实 fork 子叶先 RO，mprotect 只改许可，coherent WB 中 copyout 重新授权并留下新 Dirty。
    /// fork 前的 canonical 原页由显式有限请求准备，观察期间保持其拥有。
    #[test]
    #[ktest(tags("W18", "fs-shared"), min_cpus = 2)]
    fn fork_protect_copyout_writeback_private_cow(
        context: &mut TestContext<'_>,
        fixture: &mut WriteOwner,
    ) -> TestResult {
        fixture.prepare(context)?;
        let file = fixture.open(context, b"/test/write-map", 2)?;
        let inode = fixture.inodes.last().unwrap().clone();
        let mut attempt = ReclaimAttempt::new();
        let folio = checked(
            context,
            file.mapping_inode().unwrap().file_page(0, &mut attempt),
        )?;
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
        let copy = task::prepare_kernel_on(cpu::current().id, move || {
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
        until("coherent copyout did not progress during real WB", || {
            fixture.consumers[1].as_ref().unwrap().try_join().is_some()
        })?;
        context.expect_eq(folio.writeback_sectors() != 0, true);
        context.expect_eq(disk.test_snapshot().inflight != 0, true);
        context.expect_eq(grants < child_mm.ktest_mkwrite_count(), true);
        context.expect_eq(child_mm.mapping(SOURCE).unwrap().permissions.write, true);
        context.expect_eq(
            child_mm.mapping(SOURCE).unwrap().physical,
            folio.phys_addr(),
        );
        context.expect_eq(folio.dirty_sectors() != 0, true);
        disk.test_resume_completions();
        fixture.join_consumers()?;
        context.expect_eq(folio.writeback_sectors(), 0);
        context.expect_eq(folio.dirty_sectors() != 0, true);
        context.expect_eq(grants < child_mm.ktest_mkwrite_count(), true);
        mapped_bytes(context, &mm, SOURCE + 4, &[0x85]);
        bytes_at(context, &file, 4, &[0x85])?;
        checked(context, crate::fs::writeback::sync_inode(&inode))?;
        let mut original = [0; 1];
        context.expect_eq(file.read_at(5, &mut original), Ok(1));
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
        bytes_at(context, &file, 5, &original)?;
        mapped_bytes(context, &private, SHARED_ALIAS + 5, &[0xef]);
        WriteOwner::release(&mm, 3)?;
        until("shared fork child did not exit", || child.mm().is_none())?;
        bytes_at(context, &file, 2, &[0x83])?;
        WriteOwner::release(&mm, 4)?;
        fixture.finish_shared(context)?;
        drop((private, child_mm, child, mm, folio, disk));
        context.diagnostic(format_args!("W18 fork-child-ro=1 protect-no-direct-w=1 coherent-copyout-during-wb=1 original-dma-pfn=1 old-completion-preserves-dirty=1 private-cow-isolated=1"));
        Ok(())
    }

    /// 用户 WAIT/WAKE 在跨 Mm/VA 文件字节上匹配；PRIVATE、inode 与实际复用 PFN 分别隔离。
    /// 摘离前准备与归还目标 PFN 后的真实 miss 各使用独立有限请求。
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
        let mut prepare_attempt = ReclaimAttempt::new();
        drop(checked(
            context,
            other
                .mapping_inode()
                .unwrap()
                .file_page(0, &mut prepare_attempt),
        )?);
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
        let mut reuse_attempt = ReclaimAttempt::new();
        let reused = other
            .mapping_inode()
            .unwrap()
            .file_page(0, &mut reuse_attempt);
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
                    inode: new_inode.clone(),
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

    /// 同块、跨块和精确边界缩小后增长，原共享叶重新受 mkwrite 约束；正式同步并结束旧 FS 后重新读设备。
    #[test]
    #[case("same_block_9_3_9", 0u8)]
    #[case("cross_block_partial", 1u8)]
    #[case("block_boundary", 2u8)]
    #[case("page_boundary", 3u8)]
    #[ktest(tags("W22", "fs-truncate", "fs-write-cold", "fs-shared", "fs-sync"))]
    fn shrink_grow_zero_tail_live_mapping_and_cold_reload(
        context: &mut TestContext<'_>,
        fixture: &mut WriteOwner,
        boundary: u8,
    ) -> TestResult {
        let file = fixture.cold_open(context)?;
        let inode = fixture.inodes.last().unwrap().clone();
        let volume = fixture.cold_volume.as_ref().unwrap().clone();
        let disk = fixture.user.path.owner.disk()?.clone();
        if !disk.limits().flush {
            return Err(TestError("W22 cold reload requires real FLUSH"));
        }
        let block = volume.geometry().block_size;
        let (size, prefix) = match boundary {
            0 => (9, 3),
            1 => (block + 9, block - 3),
            2 => (2 * block, block),
            3 => (2 * PAGE_SIZE + 9, PAGE_SIZE),
            _ => unreachable!(),
        };
        checked(context, file.truncate(0))?;
        for offset in (0..size).step_by(PAGE_SIZE) {
            let length = PAGE_SIZE.min(size - offset);
            context.expect_eq(
                checked(
                    context,
                    file.write_at(offset as u64, &[0x65; PAGE_SIZE][..length]),
                )?,
                length,
            );
        }
        let mm = Arc::try_new(vm(context, Mm::try_new().map_err(VmError::from))?)
            .map_err(|_| TestError("truncate shared MM owner OOM"))?;
        vm(
            context,
            mm.map_file(
                SOURCE,
                size.div_ceil(PAGE_SIZE) * PAGE_SIZE,
                RW,
                RW,
                file.clone(),
                0,
                false,
            ),
        )?;
        context.expect_eq(mm.copyout(SOURCE + size - 1, &[0xe7]).copied, 1);
        let private = vm(context, Mm::try_new().map_err(VmError::from))?;
        vm(
            context,
            private.map_file(SHARED_ALIAS, PAGE_SIZE, RW, RW, file.clone(), 0, true),
        )?;
        context.expect_eq(private.copyout(SHARED_ALIAS, &[0x83]).copied, 1);
        let private_pfn = private.mapping(SHARED_ALIAS).unwrap().physical;
        checked(context, file.seek(SeekFrom::Start((size + 17) as u64)))?;
        checked(context, file.truncate(prefix as u64))?;
        context.expect_eq(checked(context, file.stat())?.size, prefix as u64);
        context.expect_eq(
            checked(context, file.seek(SeekFrom::Current(0)))?,
            (size + 17) as u64,
        );
        truncate_contents(context, &file, Some(&mm), prefix, prefix)?;
        mapped_bytes(context, &private, SHARED_ALIAS, &[0x83]);
        context.expect_eq(private.mapping(SHARED_ALIAS).unwrap().physical, private_pfn);
        let mut eof = [0xa7];
        context.expect_eq(file.read_at(prefix as u64, &mut eof), Ok(0));
        context.expect_eq(eof, [0xa7]);
        // 页内 EOF 的有效页再次暖 W；精确页边界之前的整页写许可无需因增长撤销。
        context.expect_eq(mm.copyout(SOURCE + prefix - 1, &[0x65]).copied, 1);
        let old_tail = SOURCE + (prefix - 1) / PAGE_SIZE * PAGE_SIZE;
        context.expect_eq(mm.mapping(old_tail).unwrap().permissions.write, true);
        let grants = mm.ktest_mkwrite_count();
        checked(context, file.truncate(size as u64))?;
        if prefix % PAGE_SIZE != 0 {
            context.expect_eq(
                mm.mapping(old_tail)
                    .is_none_or(|leaf| !leaf.permissions.write),
                true,
            );
        }
        context.expect_eq(checked(context, file.stat())?.size, size as u64);
        context.expect_eq(
            checked(context, file.seek(SeekFrom::Current(0)))?,
            (size + 17) as u64,
        );
        truncate_contents(context, &file, Some(&mm), prefix, size)?;
        context.expect_eq(mm.copyout(SOURCE + size - 1, &[0x7e]).copied, 1);
        context.expect_eq(grants < mm.ktest_mkwrite_count(), true);
        context.expect_eq(
            matches!(
                checked(context, inode.map_block((size - 1) as u64 / block as u64))?,
                BlockMapping::Mapped { .. }
            ),
            true,
        );
        bytes_at(context, &file, (size - 1) as u64, &[0x7e])?;
        context.expect_eq(mm.copyout(SOURCE + size - 1, &[0]).copied, 1);
        truncate_contents(context, &file, Some(&mm), prefix, size)?;
        drop(private);
        let before_sync = disk.test_snapshot();
        checked(context, file.sync_range(0..u64::MAX, false))?;
        context.expect_eq(
            before_sync.data_submitted < disk.test_snapshot().data_submitted,
            true,
        );
        context.expect_eq(
            disk.test_snapshot().flush_submitted - before_sync.flush_submitted,
            1,
        );
        let stat = checked(context, file.stat())?;
        let root = checked(context, inode.with_map_read(|_, root| Ok(*root)))?;
        let mapped = tree(context, &inode)?;
        let (reopened, reads_before) = fixture.cold_reopen(context, mm, file, inode, volume)?;
        let reloaded_inode = fixture.inodes.last().unwrap().clone();
        let mut reloaded_stat = checked(context, reopened.stat())?;
        context.expect_eq(reloaded_stat.device != stat.device, true);
        reloaded_stat.device = stat.device;
        context.expect_eq(reloaded_stat, stat);
        context.expect_eq(
            checked(context, reloaded_inode.with_map_read(|_, root| Ok(*root)))?,
            root,
        );
        let reloaded = tree(context, &reloaded_inode)?;
        context.expect_eq(
            (reloaded.data, reloaded.nodes, &reloaded.blocks),
            (mapped.data, mapped.nodes, &mapped.blocks),
        );
        truncate_contents(context, &reopened, None, prefix, size)?;
        context.expect_eq(reads_before < disk.test_snapshot().submitted, true);
        context.diagnostic(format_args!("W22 boundary={} block-size={} size={}->{}->{} normal-read=1 original-shared-mmap=1 private-valid-prefix-preserved=1 partial-EOF-W-revoked={} new-range-mkwrite-and-block-prepared=1 production-sync-FLUSH=1 old-FS-owners-ended=1 fresh-FS-real-device-read=1 tail-zero=1", boundary, block, size, prefix, size, prefix % PAGE_SIZE != 0));
        Ok(())
    }

    /// 普通用户直接消费截断 ABI、准入 errno 和 OFD 位置；权限修改仅作用于本例正常工作文件。
    #[test]
    #[case("read_write", 0u8)]
    #[case("read_only_mount", 1u8)]
    #[case("write_permission", 2u8)]
    #[ktest(tags("W22", "fs-truncate", "user-fs"))]
    fn ordinary_truncate_open_errors_size_and_cursor(
        context: &mut TestContext<'_>,
        fixture: &mut WriteOwner,
        admission: u8,
    ) -> TestResult {
        if admission == 1 {
            fixture
                .inodes
                .try_reserve_exact(16)
                .map_err(|_| TestError("readonly truncate inode slots OOM"))?;
            fixture.user.prepare(context)?;
            let file = fixture.open(context, b"/test/write-map", 0)?;
            let before = checked(context, file.stat())?;
            let mut prefix = [0; 32];
            context.expect_eq(file.read_at(0, &mut prefix), Ok(prefix.len()));
            fixture.run(context, b"v")?;
            context.expect_eq(checked(context, file.stat())?, before);
            bytes_at(context, &file, 0, &prefix)?;
        } else {
            fixture.prepare(context)?;
            let file = fixture.open(context, b"/test/write-reserve", 2)?;
            if admission == 0 {
                context.expect_eq(file.truncate(u64::MAX), Err(FsError::FileTooLarge));
                fixture.run(context, b"t")?;
                context.expect_eq(checked(context, file.stat())?.size, 0);
            } else {
                assert_eq!(admission, 2);
                checked(context, file.truncate(0))?;
                context.expect_eq(file.write_at(0, b"123456789"), Ok(9));
                let inode = fixture.inodes.last().unwrap().clone();
                let original = checked(context, inode.metadata_snapshot())?.mode;
                checked(
                    context,
                    inode.update_metadata(
                        checked(context, inode.disk_volume())?.claim(),
                        |metadata| {
                            metadata.mode = (metadata.mode & !0o777) | 0o444;
                        },
                    ),
                )?;
                let event = fixture.user.run_named_as(
                    context,
                    PROGRAM_WRITE,
                    b"h",
                    false,
                    Credentials {
                        uid: 1001,
                        gid: 1001,
                    },
                );
                checked(
                    context,
                    inode.update_metadata(
                        checked(context, inode.disk_volume())?.claim(),
                        |metadata| {
                            metadata.mode = original;
                        },
                    ),
                )?;
                context.expect_eq(event?.status, 0);
                bytes_at(context, &file, 0, b"123456789")?;
                context.expect_eq(checked(context, file.stat())?.size, 9);
            }
        }
        context.diagnostic(format_args!(
            "truncate-user-ABI admission={} real-user-syscalls=1 errno-and-size-checked-in-ELF=1",
            admission
        ));
        Ok(())
    }

    /// 真实 private 候选在首失效后完成 store 与 fork；另一个参数在真实 fork 快照后失效，禁止提交过期子叶。
    /// 原文件页在失效前由显式有限请求准备，并保活到两次失效完成。
    #[test]
    #[case("late_private_cow", false)]
    #[case("fork_snapshot", true)]
    #[ktest(tags("W20", "fs-truncate", "fs-shared", "user-fs"), min_cpus = 2)]
    fn first_unmap_private_cow_fork_and_second_invalidation(
        context: &mut TestContext<'_>,
        fixture: &mut WriteOwner,
        fork_snapshot: bool,
    ) -> TestResult {
        fixture.prepare(context)?;
        let file = fixture.open(context, b"/test/write-reserve", 2)?;
        checked(context, file.truncate(0))?;
        context.expect_eq(file.write_at(0, &[0x65; 8192]), Ok(8192));
        checked(context, file.sync_range(0..u64::MAX, false))?;
        let inode = fixture.inodes.last().unwrap().clone();
        let mut attempt = ReclaimAttempt::new();
        let original = checked(
            context,
            file.mapping_inode().unwrap().file_page(1, &mut attempt),
        )?;
        let image = fixture.shared_image(context, b"k")?;
        let mm = image.mm.clone();
        let file_offset = (PAGE_SIZE as u64).to_le_bytes();
        if mm
            .copyout(SHARED_CONTROL + SHARED_FILE_OFFSET, &file_offset)
            .copied
            != file_offset.len()
        {
            return Err(TestError("truncate file offset initialization failed"));
        }
        let controls = TruncateControls::new()?;
        fixture.truncate_controls = Some(controls.clone());
        let stages = controls.clone();
        let hook: Arc<dyn Fn(&InodeRef, TruncatePhase) + Send + Sync> =
            Arc::try_new(move |_: &InodeRef, phase| {
                let (value, gate) = match phase {
                    TruncatePhase::AfterFirstUnmap => (1, Some(0)),
                    TruncatePhase::BeforeSecondUnmap => (2, Some(1)),
                    TruncatePhase::BeforeBlockRelease => (3, None),
                    TruncatePhase::AfterStorageRelease => return,
                };
                stages.phase.store(value, Ordering::Release);
                if let Some(gate) = gate {
                    if !stages.cancelled.load(Ordering::Acquire) {
                        stages.gates[gate].wait();
                    }
                }
            })
            .map_err(|_| TestError("truncate phase hook OOM"))?;
        inode.ktest_set_truncate_hook(Some(hook));
        if !fork_snapshot {
            let retirement = controls.clone();
            let hook: Arc<dyn Fn(FileUnmapPhase, usize, usize) + Send + Sync> =
                Arc::try_new(move |phase, pages, tables| {
                    if pages == 0 {
                        return;
                    }
                    match phase {
                        FileUnmapPhase::BeforeSync => {
                            *retirement.before_sync.lock() = Some((pages, tables));
                            retirement.unmap_phase.store(1, Ordering::Release);
                            if !retirement.cancelled.load(Ordering::Acquire) {
                                retirement.gates[2].wait();
                            }
                        }
                        FileUnmapPhase::AfterSync => {
                            *retirement.after_sync.lock() = Some((pages, tables));
                            retirement.unmap_phase.store(2, Ordering::Release);
                        }
                    }
                })
                .map_err(|_| TestError("unmap retirement hook OOM"))?;
            inode.mapping().ktest_set_unmap_hook(Some(hook));
        }
        let ready = Arc::try_new(AtomicBool::new(false))
            .map_err(|_| TestError("truncate fault readiness OOM"))?;
        let gate =
            Arc::try_new(Completion::new()).map_err(|_| TestError("truncate fault gate OOM"))?;
        fixture.prepare_gate = Some(gate.clone());
        let entered = ready.clone();
        let pause = gate.clone();
        let hook: Arc<dyn Fn(usize) + Send + Sync> = Arc::try_new(move |address| {
            if address == SOURCE && !entered.swap(true, Ordering::AcqRel) {
                pause.wait();
            }
        })
        .map_err(|_| TestError("truncate fault or fork hook OOM"))?;
        if fork_snapshot {
            mm.ktest_set_fork_hook(Some(hook));
        } else {
            mm.ktest_set_fault_hook(Some(hook));
        }
        fixture.start_shared(context, image)?;
        fixture.phase(&mm, 0, 1)?;
        WriteOwner::release(&mm, 1)?;
        if fork_snapshot {
            fixture.phase(&mm, 0, 2)?;
            context.expect_eq(
                mm.mapping(SOURCE).unwrap().physical != original.phys_addr(),
                true,
            );
            WriteOwner::release(&mm, 2)?;
        }
        until("W20 original private fault or fork did not pause", || {
            ready.load(Ordering::Acquire)
        })?;
        let user = fixture
            .user
            .graph()
            .lookup_thread(fixture.user.running.as_ref().unwrap().pid)
            .ok_or(TestError("W20 private user binding missing"))?;
        let executor = user
            .executor()
            .ok_or(TestError("W20 private executor missing"))?;
        until("W20 private preparer did not really block", || {
            sched::test_state(&executor) == State::Blocked
        })?;
        let truncating = file.clone();
        let observed = fixture.consume_sync(0, move || truncating.truncate(PAGE_SIZE as u64))?;
        until("W20 truncate did not finish first unmap", || {
            controls.phase.load(Ordering::Acquire) == 1
        })?;
        context.expect_eq(checked(context, file.stat())?.size, PAGE_SIZE as u64);
        context.expect_eq(mm.mapping(SOURCE), None);
        gate.complete();
        mm.ktest_set_fault_hook(None);
        mm.ktest_set_fork_hook(None);
        fixture.prepare_gate.take();
        if !fork_snapshot {
            fixture.phase(&mm, 0, 2)?;
            let late = mm
                .mapping(SOURCE)
                .ok_or(TestError("W20 real late COW leaf missing"))?;
            context.expect_eq(late.physical != original.phys_addr(), true);
            context.expect_eq(late.permissions.write, true);
            mapped_bytes(context, &mm, SOURCE, &[0xb6]);
            WriteOwner::release(&mm, 2)?;
        }
        fixture.phase(&mm, 0, 3)?;
        fixture.phase(&mm, 8, 3)?;
        let child = fixture.child(&mm)?;
        let child_mm = child.mm().ok_or(TestError("W20 fork child MM missing"))?;
        if fork_snapshot {
            context.expect_eq(mm.mapping(SOURCE), None);
            context.expect_eq(child_mm.mapping(SOURCE), None);
        } else {
            let parent = mm
                .mapping(SOURCE)
                .ok_or(TestError("W20 parent late COW missing"))?;
            let inherited = child_mm
                .mapping(SOURCE)
                .ok_or(TestError("W20 child late COW missing"))?;
            context.expect_eq(parent.physical, inherited.physical);
            context.expect_eq(parent.physical != original.phys_addr(), true);
            context.expect_eq(
                (parent.permissions.write, inherited.permissions.write),
                (false, false),
            );
        }
        controls.gates[0].complete();
        until("W20 cache phase did not reach second unmap", || {
            controls.phase.load(Ordering::Acquire) == 2
        })?;
        context.expect_eq(inode.mapping().is_canonical(1, &original), false);
        if !fork_snapshot {
            context.expect_eq(mm.mapping(SOURCE).is_some(), true);
            context.expect_eq(child_mm.mapping(SOURCE).is_some(), true);
        }
        controls.gates[1].complete();
        if !fork_snapshot {
            until(
                "W20 second sweep did not retain real owners before CSD",
                || controls.unmap_phase.load(Ordering::Acquire) == 1,
            )?;
            context.expect_eq(mm.mapping(SOURCE), None);
            context.expect_eq(child_mm.mapping(SOURCE), None);
            let before = controls
                .before_sync
                .lock()
                .ok_or(TestError("W20 real retirement observation missing"))?;
            context.expect_eq(before.0, 2);
            context.expect_eq(2 <= before.1, true);
            controls.gates[2].complete();
            until(
                "W20 CSD did not finish with original retirement owners",
                || controls.unmap_phase.load(Ordering::Acquire) == 2,
            )?;
            context.expect_eq(*controls.after_sync.lock(), Some(before));
        }
        fixture.join_consumers()?;
        context.expect_eq(*observed.result.lock(), Some(Ok(())));
        context.expect_eq(controls.phase.load(Ordering::Acquire), 3);
        context.expect_eq(mm.mapping(SOURCE), None);
        context.expect_eq(child_mm.mapping(SOURCE), None);
        context.expect_eq(mm.fault(SOURCE, UserAccess::Read), Err(VmError::BeyondEof));
        context.expect_eq(
            child_mm.fault(SOURCE, UserAccess::Read),
            Err(VmError::BeyondEof),
        );
        bytes_at(context, &file, 0, &[0x65; PAGE_SIZE])?;
        inode.ktest_set_truncate_hook(None);
        inode.mapping().ktest_set_unmap_hook(None);
        fixture.truncate_controls.take();
        WriteOwner::release(&mm, 3)?;
        fixture.finish_shared(context)?;
        context.diagnostic(format_args!("W20 fork-snapshot={} original-private-fault-or-fork-paused=1 first-unmap-before-resume=1 real-store-and-clone=1 second-range-invalidation=1 stale-child-snapshot-rejected={} real-retired-pages-and-tables-held-through-CSD={} no-synthetic-leaf=1", fork_snapshot, fork_snapshot, !fork_snapshot));
        drop((child_mm, child, user, mm, original));
        Ok(())
    }

    /// 普通 ftruncate 等原数据或真实树节点 WB；旧位图仍 used，真实全尾结束后旧 data 和 metadata 块才复用为新数据。
    /// 旧树全部原页共享准备预算，块复用后的新页检查使用独立有限请求。
    #[test]
    #[case("data", false)]
    #[case("metadata_node", true)]
    #[ktest(tags("W21", "fs-truncate", "fs-sync", "user-fs"), min_cpus = 2)]
    fn old_writeback_waits_before_bitmap_release_and_real_reuse(
        context: &mut TestContext<'_>,
        fixture: &mut WriteOwner,
        metadata: bool,
    ) -> TestResult {
        fixture.prepare(context)?;
        let file = fixture.open(context, b"/test/write-reserve", 2)?;
        let inode = fixture.inodes.last().unwrap().clone();
        let volume = fixture.volume();
        let disk = fixture.user.path.owner.disk()?.clone();
        if !disk.limits().flush {
            return Err(TestError("W21 requires production FLUSH"));
        }
        checked(context, file.truncate(0))?;
        let block = volume.geometry().block_size;
        let traditional = checked(context, inode.metadata_snapshot())?.flags & 0x80000 == 0;
        let logical = if traditional {
            [0, 12, 268, 65804, 65806]
        } else {
            [0, 2, 4, 6, 8]
        };
        let mut data_blocks = [0; 5];
        let mut old_folios = Vec::new();
        old_folios
            .try_reserve_exact(5)
            .map_err(|_| TestError("old WB folio slots OOM"))?;
        let mut prepare_attempt = ReclaimAttempt::new();
        for (index, logical) in logical.iter().copied().enumerate() {
            let offset = logical * block as u64;
            context.expect_eq(
                file.write_at(offset, &[0x3a; PAGE_SIZE][..block]),
                Ok(block),
            );
            let BlockMapping::Mapped { physical, .. } = checked(context, inode.map_block(logical))?
            else {
                return Err(TestError("W21 producer data not mapped"));
            };
            data_blocks[index] = physical;
            old_folios.push(checked(
                context,
                file.mapping_inode()
                    .unwrap()
                    .file_page(offset / PAGE_SIZE as u64, &mut prepare_attempt),
            )?);
        }
        let old = tree(context, &inode)?;
        context.expect_eq(old.data, 5);
        context.expect_eq(0 < old.nodes, true);
        let node = old
            .blocks
            .iter()
            .copied()
            .find(|physical| !data_blocks.contains(physical))
            .ok_or(TestError("W21 actual old tree node missing"))?;
        let node_buffer = checked(context, volume.metadata_block(node))?;
        let node_folio = node_buffer.folio().clone();
        checked(context, file.sync_range(0..u64::MAX, false))?;
        let image = fixture.shared_image(context, b"j")?;
        let mm = image.mm.clone();
        fixture.start_shared(context, image)?;
        fixture.phase(&mm, 0, 1)?;
        let user = fixture
            .user
            .graph()
            .lookup_thread(fixture.user.running.as_ref().unwrap().pid)
            .ok_or(TestError("W21 truncating user binding missing"))?;
        let executor = user
            .executor()
            .ok_or(TestError("W21 truncating executor missing"))?;
        let first_unmap = Arc::try_new(AtomicBool::new(false))
            .map_err(|_| TestError("W21 first unmap observation OOM"))?;
        let reached = first_unmap.clone();
        let hook: Arc<dyn Fn(&InodeRef, TruncatePhase) + Send + Sync> =
            Arc::try_new(move |_: &InodeRef, phase| {
                if phase == TruncatePhase::AfterFirstUnmap {
                    reached.store(true, Ordering::Release);
                }
            })
            .map_err(|_| TestError("W21 truncate observer OOM"))?;
        inode.ktest_set_truncate_hook(Some(hook));
        if metadata {
            // 原节点条目和 CRC 保持合法；update 重新记其真实 Dirty，正式 metadata WB 写出旧树字节。
            checked(
                context,
                node_buffer
                    .update(volume.claim(), |_| ())
                    .map_err(FsError::Io),
            )?;
        } else {
            for logical in logical.iter().copied() {
                context.expect_eq(
                    file.write_at(logical * block as u64, &[0x4b; PAGE_SIZE][..block]),
                    Ok(block),
                );
            }
        }
        let before = disk.test_snapshot();
        disk.test_suspend_completions();
        let syncing_inode = inode.clone();
        let syncing_volume = volume.clone();
        let observed = fixture.consume_sync(0, move || {
            if metadata {
                crate::fs::writeback::sync_metadata(&syncing_volume)
            } else {
                crate::fs::writeback::sync_inode(&syncing_inode)
            }
        })?;
        until("W21 real original WB did not accept IO and block", || {
            let snapshot = disk.test_snapshot();
            let submitted = if metadata {
                before.metadata_submitted < snapshot.metadata_submitted
                    && node_folio.writeback_sectors() != 0
            } else {
                before.data_submitted < snapshot.data_submitted
                    && old_folios
                        .iter()
                        .any(|folio| folio.writeback_sectors() != 0)
            };
            submitted && snapshot.inflight != 0 && observed.blocked()
        })?;
        WriteOwner::release(&mm, 1)?;
        until(
            "W21 ordinary ftruncate did not block on original IO",
            || {
                sched::test_state(&executor) == State::Blocked
                    || mm.load_user_u32(SHARED_CONTROL) == Ok(2)
            },
        )?;
        context.expect_eq(sched::test_state(&executor), State::Blocked);
        context.expect_eq(mm.load_user_u32(SHARED_CONTROL), Ok(1));
        if !metadata {
            until(
                "W21 data wait began before new size and first unmap",
                || first_unmap.load(Ordering::Acquire),
            )?;
            context.expect_eq(checked(context, file.stat())?.size, 0);
        }
        for physical in &old.blocks {
            context.expect_eq(
                checked(context, block_allocation_bit(&volume, *physical))?,
                Some(true),
            );
        }
        let hint = (inode.number() - 1) / volume.geometry().inodes_per_group;
        let probe = checked(context, volume.allocate_block(hint))?;
        let allocated = probe.physical();
        let excluded = !old.blocks.contains(&allocated);
        volume.release_unpublished_block(probe);
        context.expect_eq(excluded, true);
        context.expect_eq(disk.test_snapshot().inflight != 0, true);
        context.expect_eq(observed.result.lock().is_none(), true);
        context.diagnostic(format_args!("W21 kind={} completion-consumption-paused=1 ordinary-sys46-blocked=1 first-unmap-observed={} old-data-and-node-bitmap-used=1 real-allocation-excludes-old-blocks=1 allocated-probe={} hardware-DMA-inflight-claim=0", if metadata { "metadata-node" } else { "data" }, first_unmap.load(Ordering::Acquire), allocated));
        disk.test_resume_completions();
        fixture.join_consumers()?;
        context.expect_eq(*observed.result.lock(), Some(Ok(())));
        fixture.phase(&mm, 0, 2)?;
        disk.test_flush_completions();
        context.expect_eq(disk.test_snapshot().inflight, 0);
        context.expect_eq(checked(context, file.stat())?.size, 0);
        context.expect_eq(checked(context, inode.metadata_snapshot())?.blocks, 0);
        context.expect_eq(inode.mapping().is_empty(), true);
        for physical in &old.blocks {
            context.expect_eq(
                checked(context, block_allocation_bit(&volume, *physical))?,
                Some(false),
            );
        }
        drop((node_buffer, node_folio));
        let replacement = [0xd7; PAGE_SIZE];
        for (logical, physical) in old.blocks.iter().copied().enumerate() {
            context.expect_eq(
                file.write_at((logical * block) as u64, &replacement[..block]),
                Ok(block),
            );
            let BlockMapping::Mapped {
                physical: reused, ..
            } = checked(context, inode.map_block(logical as u64))?
            else {
                return Err(TestError("W21 released block not reused as real file data"));
            };
            context.expect_eq(reused, physical);
            bytes_at(
                context,
                &file,
                (logical * block) as u64,
                &replacement[..block],
            )?;
        }
        let mut reuse_attempt = ReclaimAttempt::new();
        let fresh = checked(
            context,
            file.mapping_inode()
                .unwrap()
                .file_page(0, &mut reuse_attempt),
        )?;
        context.expect_eq(
            old_folios
                .iter()
                .all(|old| old.phys_addr() != fresh.phys_addr()),
            true,
        );
        checked(context, file.sync_range(0..u64::MAX, false))?;
        disk.test_flush_completions();
        context.expect_eq(disk.test_snapshot().inflight, 0);
        let mut device_bytes = [0; PAGE_SIZE];
        for physical in &old.blocks {
            checked(
                context,
                volume
                    .claim()
                    .disk()
                    .read_exact_at(*physical * block as u64, &mut device_bytes[..block])
                    .map_err(FsError::Io),
            )?;
            context.expect_eq(&device_bytes[..block], &replacement[..block]);
        }
        inode.ktest_set_truncate_hook(None);
        WriteOwner::release(&mm, 2)?;
        fixture.finish_shared(context)?;
        context.diagnostic(format_args!("W21 kind={} real-old-IO-and-Work-tails-ended=1 released-data={} released-nodes={} actual-all-old-physical-block-reuse=1 detached-folio-not-reattached=1 production-sync-and-device-readback=1 old-write-does-not-overwrite-new-data=1", if metadata { "metadata-node" } else { "data" }, old.data, old.nodes));
        Ok(())
    }

    /// 用真实文件块地址和原页凭证准备数据回写，并将完成字节归入所属卷。
    fn prepare_unlinked_original_writeback(
        inode: &InodeRef,
        folio: &crate::mm::FolioRef,
    ) -> Result<Vec<(u64, crate::mm::FolioWriteback)>, FsError> {
        let volume = inode.disk_volume()?;
        let block_size = volume.geometry().block_size;
        let folio = folio.clone().with_file_owner(inode.clone());
        folio.wait_writeback();
        let (_, targets) =
            crate::fs::writeback::prepare_file_targets(inode, 0, &(0..PAGE_SIZE as u64), None)?;
        let mut ranges = Vec::new();
        ranges
            .try_reserve_exact(PAGE_SIZE / block_size)
            .map_err(|_| FsError::NoMemory)?;
        let mut content = folio.lock_content();
        inode.mapping().mkclean(0, &folio, &mut content);
        let dirty = folio.dirty_sectors();
        let block_mask = ((1u16 << (block_size / 512)) - 1) as u8;
        for offset in (0..PAGE_SIZE).step_by(block_size) {
            if dirty & (block_mask << (offset / 512)) == 0 {
                continue;
            }
            let target = targets[offset / block_size].ok_or(FsError::Corrupt)?;
            // SAFETY: 与生产准备路径一样，内容 guard 排斥许可提交；mkclean 已撤销
            // 同源共享 W 并完成全 CPU CSD，返回凭证随后由原 FsClaim/Bio 全尾收权。
            if let Some(writeback) = unsafe { content.begin_writeback(offset, block_size)? } {
                ranges.push((
                    target,
                    writeback.with_writeback_domain(volume.writeback_domain(), false),
                ));
            }
        }
        drop(content);
        drop(folio);
        if ranges.is_empty() {
            return Err(FsError::Corrupt);
        }
        Ok(ranges)
    }

    /// 合法未链接 inode 的缓存先退出，OFD、VMA、外部原页和真实 IO 分别保活；最后使用交原 Task 回收并返块和 inode 位。
    /// 外部原页由一份显式有限请求取得，其最后释放仍承担原 inode 寿命。
    #[test]
    #[ktest(tags("fs-truncate", "fs-inode-lifetime", "fs-sync"), min_cpus = 2)]
    fn unlinked_last_ofd_vma_folio_and_io_release_canonical_inode(
        context: &mut TestContext<'_>,
        fixture: &mut WriteOwner,
    ) -> TestResult {
        let linked = fixture.cold_open(context)?;
        let volume = fixture.cold_volume.as_ref().unwrap().clone();
        let cache = fixture.cold_cache.as_ref().unwrap().clone();
        let linked_inode = fixture.inodes.last().unwrap().clone();
        context.expect_eq(
            linked
                .path()
                .unwrap()
                .unhashed_inode(linked_inode.clone())
                .err(),
            Some(FsError::InvalidOperation),
        );
        let before = allocation_counts(context, &volume)?;
        let token = checked(context, volume.allocate_inode(0, 0o100600, 0, 0))?;
        let number = token.number();
        let inode = match cache.iget(&volume, number) {
            Ok(inode) => {
                context.expect_eq(token.publish(), number);
                inode
            }
            Err(error) => {
                volume.release_unpublished_inode(token);
                return checked(context, Err(error));
            }
        };
        fixture.inodes.push(inode.clone());
        fixture.eviction = Some(cache.watch());
        let path = checked(
            context,
            linked.path().unwrap().unhashed_inode(inode.clone()),
        )?;
        let file = checked(
            context,
            open_path_with_flags(fixture.user.path.vfs(), path, 2, FileIoContext::Kernel),
        )?;
        context.expect_eq(checked(context, file.stat())?.links, 0);
        context.expect_eq(file.write_at(0, &[0x91; PAGE_SIZE]), Ok(PAGE_SIZE));
        let mut attempt = ReclaimAttempt::new();
        let folio = checked(
            context,
            file.mapping_inode().unwrap().file_page(0, &mut attempt),
        )?;
        let mm = Arc::try_new(vm(context, Mm::try_new().map_err(VmError::from))?)
            .map_err(|_| TestError("unlinked live VMA MM owner OOM"))?;
        vm(
            context,
            mm.map_file(SOURCE, PAGE_SIZE, RW, RW, file.clone(), 0, false),
        )?;
        context.expect_eq(mm.copyout(SOURCE + 1, &[0x72]).copied, 1);
        let mapped = tree(context, &inode)?;
        context.expect_eq(checked(context, file.stat())?.size, PAGE_SIZE as u64);
        context.expect_eq(
            checked(context, inode_allocation_bit(&volume, number))?,
            true,
        );
        let disk = fixture.user.path.owner.disk()?.clone();
        let ranges = checked(context, prepare_unlinked_original_writeback(&inode, &folio))?;
        disk.test_suspend_completions();
        let io_volume = volume.clone();
        let observed = fixture.consume_sync(0, move || {
            let mut ranges = ranges;
            let mut bios = [const { None }; 8];
            let mut completions = [const { None }; 8];
            io_volume
                .claim()
                .writeback_batch(
                    &mut ranges,
                    None,
                    &mut bios,
                    &mut completions,
                    io_volume.writeback_domain(),
                    || false,
                )
                .map_err(FsError::from)
        })?;
        until(
            "unlinked inode original data WB did not really block",
            || {
                folio.writeback_sectors() != 0
                    && disk.test_snapshot().inflight != 0
                    && observed.blocked()
            },
        )?;
        fixture.inodes.clear();
        drop(inode);
        drop(fixture.cold_cache.take());
        drop(cache);
        checked(context, fixture.eviction.as_ref().unwrap().wait())?;
        context.expect_eq(fixture.eviction.as_ref().unwrap().contains(number), true);
        context.expect_eq(
            checked(context, inode_allocation_bit(&volume, number))?,
            true,
        );
        context.expect_eq(checked(context, file.stat())?.size, PAGE_SIZE as u64);
        drop(file);
        mapped_bytes(context, &mm, SOURCE, &[0x91, 0x72]);
        checked(context, fixture.eviction.as_ref().unwrap().wait())?;
        context.expect_eq(fixture.eviction.as_ref().unwrap().contains(number), true);
        context.expect_eq(
            checked(context, inode_allocation_bit(&volume, number))?,
            true,
        );
        drop(mm);
        checked(context, fixture.eviction.as_ref().unwrap().wait())?;
        context.expect_eq(fixture.eviction.as_ref().unwrap().contains(number), true);
        context.expect_eq(
            checked(context, inode_allocation_bit(&volume, number))?,
            true,
        );
        context.expect_eq(folio.writeback_sectors() != 0, true);
        drop(folio);
        checked(context, fixture.eviction.as_ref().unwrap().wait())?;
        context.expect_eq(fixture.eviction.as_ref().unwrap().contains(number), true);
        context.expect_eq(
            checked(context, inode_allocation_bit(&volume, number))?,
            true,
        );
        for physical in &mapped.blocks {
            context.expect_eq(
                checked(context, block_allocation_bit(&volume, *physical))?,
                Some(true),
            );
        }
        context.diagnostic(format_args!("inode-last-use number={} legitimate-published-nlink0=1 unhashed-real-OFD-and-VMA=1 cache-owner-ended=1 OFD-then-VMA-then-folio-ended=1 original-IO-credential-only-owns-inode=1 bitmap-and-size-preserved=1", number));
        disk.test_resume_completions();
        fixture.join_consumers()?;
        context.expect_eq(*observed.result.lock(), Some(Ok(())));
        disk.test_flush_completions();
        checked(context, fixture.eviction.as_ref().unwrap().wait())?;
        context.expect_eq(fixture.eviction.as_ref().unwrap().contains(number), false);
        context.expect_eq(
            checked(context, inode_allocation_bit(&volume, number))?,
            false,
        );
        for physical in &mapped.blocks {
            context.expect_eq(
                checked(context, block_allocation_bit(&volume, *physical))?,
                Some(false),
            );
        }
        drop((linked, linked_inode));
        checked(context, fixture.eviction.as_ref().unwrap().wait_retired())?;
        let retired_counts = allocation_counts(context, &volume)?;
        context.expect_eq(retired_counts, before);
        let reused = checked(context, volume.allocate_inode(0, 0o100600, 123, 234))?;
        let reused_number = reused.number();
        let reloaded = volume.load_inode(reused_number, None);
        volume.release_unpublished_inode(reused);
        context.expect_eq(reused_number, number);
        let reloaded = checked(context, reloaded)?;
        context.expect_eq(
            (
                reloaded.size,
                reloaded.blocks,
                reloaded.links,
                reloaded.uid,
                reloaded.gid,
            ),
            (0, 0, 0, 123, 234),
        );
        checked(context, crate::fs::writeback::sync_metadata(&volume))?;
        disk.test_flush_completions();
        context.expect_eq(disk.test_snapshot().inflight, 0);
        let final_counts = allocation_counts(context, &volume)?;
        context.expect_eq(final_counts, before);
        drop(fixture.eviction.take());
        context.diagnostic(format_args!("inode-last-use number={} original-IO-and-Work-tails-ended=1 canonical-slot-removed=1 task-context-eviction=1 file-blocks-and-inode-bitmap-returned=1 actual-inode-number-reuse=1 production-metadata-sync=1", number));
        Ok(())
    }
    struct InodeReuseControls {
        phase: AtomicUsize,
        cancelled: AtomicBool,
        gate: Completion,
    }

    struct ReusedCanonicalInode {
        number: u64,
        slot_removed_before_iget: bool,
        inode: InodeRef,
    }

    /// 分配成功后先由同一缓存取得 canonical inode，再发表并结束临时 reservation。
    fn allocate_published_inode(
        volume: &Arc<Ext4Volume>,
        cache: &Arc<InodeCache>,
    ) -> Result<ReusedCanonicalInode, FsError> {
        let watch = cache.watch();
        let token = volume.allocate_inode(0, 0o100600, 123, 234)?;
        let number = token.number();
        let slot_removed_before_iget = !watch.contains(number);
        let inode = match cache.iget(volume, number) {
            Ok(inode) => inode,
            Err(error) => {
                volume.release_unpublished_inode(token);
                return Err(error);
            }
        };
        Ok(ReusedCanonicalInode {
            number: token.publish(),
            slot_removed_before_iget,
            inode,
        })
    }

    /// 在短内容锁内复制完整的真实 inode 记录，支持本例两种 ext4 记录尺寸。
    fn inode_record_snapshot(
        context: &mut TestContext<'_>,
        volume: &Ext4Volume,
        number: u64,
    ) -> Result<([u8; 256], usize), TestError> {
        let size = volume.geometry().inode_size;
        if size != 128 && size != 256 {
            return Err(TestError("inode reuse record size is unsupported"));
        }
        let (buffer, offset) = checked(context, volume.inode_record(number, None))?;
        let mut record = [0; 256];
        checked(
            context,
            buffer
                .with_read(|bytes| record[..size].copy_from_slice(&bytes[offset..offset + size]))
                .map_err(FsError::Io),
        )?;
        Ok((record, size))
    }

    /// 真实未链接 inode 释放存储后，分配者须等 post-delete metadata WB 和原 canonical 槽退场。
    #[test]
    #[case("running", false)]
    #[case("failed", true)]
    #[ktest(tags("fs-truncate", "fs-inode-lifetime", "fs-sync"), min_cpus = 2)]
    fn unlinked_inode_reuse_waits_for_post_delete_writeback(
        context: &mut TestContext<'_>,
        fixture: &mut WriteOwner,
        fail_metadata: bool,
    ) -> TestResult {
        let disk = fixture.user.path.owner.disk()?.clone();
        let controls = Arc::try_new(InodeReuseControls {
            phase: AtomicUsize::new(0),
            cancelled: AtomicBool::new(false),
            gate: Completion::new(),
        })
        .map_err(|_| TestError("inode reuse controls OOM"))?;
        let allocation = Arc::try_new(SpinLock::new(None::<Result<ReusedCanonicalInode, FsError>>))
            .map_err(|_| TestError("inode reuse result OOM"))?;
        let body = (|| -> TestResult {
            let linked = fixture.cold_open(context)?;
            let volume = fixture.cold_volume.as_ref().unwrap().clone();
            let cache = fixture.cold_cache.as_ref().unwrap().clone();
            let linked_inode = fixture.inodes.last().unwrap().clone();
            let before = allocation_counts(context, &volume)?;
            let original = checked(context, allocate_published_inode(&volume, &cache))?;
            context.expect_eq(original.slot_removed_before_iget, true);
            let number = original.number;
            let original_metadata = checked(context, original.inode.metadata_snapshot())?;
            context.expect_eq(
                checked(context, inode_allocation_bit(&volume, number))?,
                true,
            );
            let allocated_counts = allocation_counts(context, &volume)?;
            context.expect_eq(allocated_counts, (before.0, before.1 - 1));
            let path = checked(
                context,
                linked
                    .path()
                    .unwrap()
                    .unhashed_inode(original.inode.clone()),
            )?;
            let file = checked(
                context,
                open_path_with_flags(fixture.user.path.vfs(), path, 2, FileIoContext::Kernel),
            )?;
            context.expect_eq(checked(context, file.stat())?.links, 0);
            let paused = controls.clone();
            let hook: Arc<dyn Fn(&InodeRef, TruncatePhase) + Send + Sync> =
                Arc::try_new(move |_: &InodeRef, phase| {
                    if phase != TruncatePhase::AfterStorageRelease {
                        return;
                    }
                    paused.phase.store(1, Ordering::Release);
                    if !paused.cancelled.load(Ordering::Acquire) {
                        paused.gate.wait();
                    }
                })
                .map_err(|_| TestError("inode reuse truncate hook OOM"))?;
            original.inode.ktest_set_truncate_hook(Some(hook));
            fixture.eviction = Some(cache.watch());
            drop((file, linked, linked_inode));
            fixture.inodes.clear();
            drop(original.inode);
            until("inode reuse did not reach real storage release", || {
                controls.phase.load(Ordering::Acquire) == 1
            })?;
            disk.test_flush_completions();
            let released_counts = allocation_counts(context, &volume)?;
            context.expect_eq(released_counts, before);
            let released_bit = checked(context, inode_allocation_bit(&volume, number))?;
            context.expect_eq(released_bit, false);
            context.expect_eq(fixture.eviction.as_ref().unwrap().contains(number), true);
            context.expect_eq(disk.test_snapshot().inflight, 0);
            context.expect_eq(disk.test_completion_error_pending(), false);
            let (old_record, record_size) = inode_record_snapshot(context, &volume, number)?;
            let old_generation = checked(context, codec::le32(&old_record[..record_size], 100))?;
            context.expect_eq(old_generation, original_metadata.generation);

            let result = allocation.clone();
            let actor_volume = volume.clone();
            let actor_cache = cache.clone();
            let observed = fixture.consume_sync(0, move || {
                *result.lock() = Some(allocate_published_inode(&actor_volume, &actor_cache));
                Ok(())
            })?;
            until("inode reuse allocator neither blocked nor returned", || {
                observed.blocked() || allocation.lock().is_some()
            })?;
            if !observed.blocked() || allocation.lock().is_some() {
                return Err(TestError("inode reuse allocator missed canonical wait"));
            }
            context.expect_eq(*observed.result.lock(), None);
            context.expect_eq(fixture.eviction.as_ref().unwrap().contains(number), true);
            let reserved_bit = checked(context, inode_allocation_bit(&volume, number))?;
            context.expect_eq(reserved_bit, true);
            let reserved_counts = allocation_counts(context, &volume)?;
            context.expect_eq(reserved_counts.0, released_counts.0);
            context.expect_eq(reserved_counts.1 + 1, released_counts.1);
            let (waiting_record, waiting_size) = inode_record_snapshot(context, &volume, number)?;
            context.expect_eq(waiting_size, record_size);
            context.expect_eq(&waiting_record[..record_size], &old_record[..record_size]);
            context.expect_eq(
                codec::le32(&waiting_record[..record_size], 100),
                Ok(old_generation),
            );

            disk.test_suspend_completions();
            let before_writeback = disk.test_snapshot();
            context.expect_eq(before_writeback.inflight, 0);
            if fail_metadata {
                disk.test_fail_next_metadata(BlockError::Io);
            }
            controls.gate.complete();
            until(
                "post-delete metadata WB did not submit a real in-flight request",
                || {
                    let current = disk.test_snapshot();
                    before_writeback.metadata_submitted < current.metadata_submitted
                        && before_writeback.inflight < current.inflight
                },
            )?;
            context.expect_eq(observed.blocked(), true);
            context.expect_eq(allocation.lock().is_none(), true);
            context.expect_eq(disk.test_completion_error_pending(), fail_metadata);
            disk.test_resume_completions();
            let eviction = fixture.eviction.as_ref().unwrap().wait();
            until("inode reuse metadata slot tail did not end", || {
                let current = disk.test_snapshot();
                current.inflight == 0 && before_writeback.completed < current.completed
            })?;
            disk.test_flush_completions();
            let completed = disk.test_snapshot();
            context.expect_eq(completed.inflight, 0);
            context.expect_eq(before_writeback.completed < completed.completed, true);
            context.expect_eq(disk.test_completion_error_pending(), false);

            let reused = if fail_metadata {
                until(
                    "inode reuse allocator did not return the real metadata error",
                    || allocation.lock().is_some(),
                )?;
                let attempt = allocation
                    .lock()
                    .take()
                    .ok_or(TestError("inode reuse allocation result missing"))?;
                let expected = FsError::Io(BlockError::Io);
                let error = match attempt {
                    Err(error) => error,
                    Ok(reused) => {
                        drop(reused);
                        return Err(TestError("failed inode reuse unexpectedly succeeded"));
                    }
                };
                if eviction != Err(expected) || error != expected {
                    context.expect_eq(eviction, Err(expected));
                    context.expect_eq(error, expected);
                    return Err(TestError(
                        "inode reuse failure did not preserve the original error",
                    ));
                }
                context.expect_eq(fixture.eviction.as_ref().unwrap().contains(number), true);
                let cancelled_bit = checked(context, inode_allocation_bit(&volume, number))?;
                let cancelled_counts = allocation_counts(context, &volume)?;
                context.expect_eq(cancelled_bit, false);
                context.expect_eq(cancelled_counts, released_counts);
                let (failed_record, failed_size) = inode_record_snapshot(context, &volume, number)?;
                context.expect_eq(failed_size, record_size);
                context.expect_eq(&failed_record[..record_size], &old_record[..record_size]);
                context.expect_eq(
                    codec::le32(&failed_record[..record_size], 100),
                    Ok(old_generation),
                );

                let before_retry = disk.test_snapshot();
                let pruned = cache.prune_unused();
                context.expect_eq(pruned > 0, true);
                disk.test_flush_completions();
                let recovered = disk.test_snapshot();
                context.expect_eq(
                    before_retry.metadata_submitted < recovered.metadata_submitted,
                    true,
                );
                context.expect_eq(before_retry.completed < recovered.completed, true);
                context.expect_eq(recovered.inflight, 0);
                context.expect_eq(fixture.eviction.as_ref().unwrap().contains(number), false);
                let recovered_bit = checked(context, inode_allocation_bit(&volume, number))?;
                let recovered_counts = allocation_counts(context, &volume)?;
                context.expect_eq(recovered_bit, false);
                context.expect_eq(recovered_counts, released_counts);
                let (retried_record, retried_size) =
                    inode_record_snapshot(context, &volume, number)?;
                context.expect_eq(retried_size, record_size);
                context.expect_eq(&retried_record[..record_size], &old_record[..record_size]);
                checked(context, allocate_published_inode(&volume, &cache))?
            } else {
                checked(context, eviction)?;
                until(
                    "inode reuse allocator did not resume after slot removal",
                    || allocation.lock().is_some(),
                )?;
                context.expect_eq(*observed.result.lock(), Some(Ok(())));
                let attempt = allocation
                    .lock()
                    .take()
                    .ok_or(TestError("inode reuse allocation result missing"))?;
                checked(context, attempt)?
            };
            context.expect_eq(reused.number, number);
            context.expect_eq(reused.slot_removed_before_iget, true);
            let metadata = checked(context, reused.inode.metadata_snapshot())?;
            context.expect_eq(metadata.number, number);
            context.expect_eq(metadata.generation, old_generation.wrapping_add(1));
            context.expect_eq(
                (metadata.mode, metadata.uid, metadata.gid),
                (0o100600, 123, 234),
            );
            context.expect_eq((metadata.links, metadata.size, metadata.blocks), (0, 0, 0));
            drop(reused.inode);
            checked(context, fixture.eviction.as_ref().unwrap().wait())?;
            disk.test_flush_completions();
            context.expect_eq(disk.test_snapshot().inflight, 0);
            context.expect_eq(fixture.eviction.as_ref().unwrap().contains(number), false);
            let final_bit = checked(context, inode_allocation_bit(&volume, number))?;
            let final_counts = allocation_counts(context, &volume)?;
            context.expect_eq(final_bit, false);
            context.expect_eq(final_counts, before);
            drop(fixture.eviction.take());
            context.diagnostic(format_args!(
                "inode-reuse phase={} reserved-before-canonical-wait=1 real-post-delete-metadata-WB=1 failed-slot-retried-explicitly={} generation-and-credentials=1 final-resources-restored=1",
                if fail_metadata { "failed" } else { "running" },
                fail_metadata
            ));
            Ok(())
        })();
        controls.cancelled.store(true, Ordering::Release);
        controls.gate.complete();
        disk.test_resume_completions();
        disk.test_flush_completions();
        let joined = fixture.join_consumers();
        drop(allocation.lock().take());
        if let Err(error) = body {
            joined?;
            return Err(error);
        }
        joined?;
        Ok(())
    }

    /// 新 VMA 发表而必撤叶尚在时，旧文件来源与登记仍须供真实截断找到；CSD 结束后才能丢原页表拥有。
    /// 旧文件与可选新文件分别用独立有限请求准备原页，保留各自的来源身份。
    #[test]
    #[case("file_to_file", 0u8)]
    #[case("file_to_anonymous", 1u8)]
    #[case("munmap", 2u8)]
    #[ktest(tags("fs-truncate", "vm-file-retirement", "fs-shared"), min_cpus = 2)]
    fn mandatory_vma_leaf_retirement_preserves_old_file_source(
        context: &mut TestContext<'_>,
        fixture: &mut WriteOwner,
        replacement: u8,
    ) -> TestResult {
        fixture.prepare(context)?;
        let old = fixture.open(context, b"/test/write-reserve", 2)?;
        checked(context, old.truncate(0))?;
        context.expect_eq(old.write_at(0, &[0x4f; PAGE_SIZE]), Ok(PAGE_SIZE));
        checked(context, old.sync_range(0..u64::MAX, false))?;
        let old_inode = fixture.inodes.last().unwrap().clone();
        let mut old_attempt = ReclaimAttempt::new();
        let old_folio = checked(
            context,
            old.mapping_inode().unwrap().file_page(0, &mut old_attempt),
        )?;
        let mut expected = [0; 32];
        let (new, new_folio) = if replacement == 0 {
            let file = fixture.open(context, b"/test/write-map", 0)?;
            context.expect_eq(file.read_at(0, &mut expected), Ok(expected.len()));
            let mut new_attempt = ReclaimAttempt::new();
            let folio = checked(
                context,
                file.mapping_inode().unwrap().file_page(0, &mut new_attempt),
            )?;
            (Some(file), Some(folio))
        } else {
            (None, None)
        };
        let mm = Arc::try_new(vm(context, Mm::try_new().map_err(VmError::from))?)
            .map_err(|_| TestError("mandatory retirement MM owner OOM"))?;
        vm(
            context,
            mm.map_file(SOURCE, PAGE_SIZE, RO, RO, old.clone(), 0, false),
        )?;
        mapped_bytes(context, &mm, SOURCE, &[0x4f; 32]);
        context.expect_eq(mm.mapping(SOURCE).unwrap().physical, old_folio.phys_addr());
        fixture.shared_mm = Some(mm.clone());
        let ready = Arc::try_new(AtomicBool::new(false))
            .map_err(|_| TestError("mandatory retirement ready OOM"))?;
        let gate = Arc::try_new(Completion::new())
            .map_err(|_| TestError("mandatory retirement gate OOM"))?;
        fixture.prepare_gate = Some(gate.clone());
        let entered = ready.clone();
        let pause = gate.clone();
        let hook: Arc<dyn Fn(usize, usize) + Send + Sync> = Arc::try_new(move |start, end| {
            if start == SOURCE && end == SOURCE + PAGE_SIZE && !entered.swap(true, Ordering::AcqRel)
            {
                pause.wait();
            }
        })
        .map_err(|_| TestError("mandatory retirement VMA hook OOM"))?;
        mm.ktest_set_vma_unmap_hook(Some(hook));
        let controls = TruncateControls::new()?;
        fixture.truncate_controls = Some(controls.clone());
        let retirement = controls.clone();
        let hook: Arc<dyn Fn(FileUnmapPhase, usize, usize) + Send + Sync> =
            Arc::try_new(move |phase, pages, tables| {
                if pages == 0 {
                    return;
                }
                match phase {
                    FileUnmapPhase::BeforeSync => {
                        *retirement.before_sync.lock() = Some((pages, tables));
                        retirement.unmap_phase.store(1, Ordering::Release);
                        if !retirement.cancelled.load(Ordering::Acquire) {
                            retirement.gates[2].wait();
                        }
                    }
                    FileUnmapPhase::AfterSync => {
                        *retirement.after_sync.lock() = Some((pages, tables));
                        retirement.unmap_phase.store(2, Ordering::Release);
                    }
                }
            })
            .map_err(|_| TestError("mandatory retirement ownership hook OOM"))?;
        old_inode.mapping().ktest_set_unmap_hook(Some(hook));
        let result = Arc::try_new(SpinLock::new(None::<Result<(), VmError>>))
            .map_err(|_| TestError("mandatory retirement result OOM"))?;
        let recorded = result.clone();
        let replacing = mm.clone();
        let file = new;
        fixture.consumers[0] = Some(
            task::prepare_kernel_on(cpu::current().id, move || {
                let operation = if replacement == 2 {
                    replacing.unmap(SOURCE, PAGE_SIZE)
                } else {
                    let source = if replacement == 0 {
                        let file = file.expect("file replacement has no original OFD");
                        let inode = file
                            .mapping_inode()
                            .expect("regular replacement has no mapping inode")
                            .clone();
                        MmapSource::File {
                            file,
                            inode,
                            offset: 0,
                            private: false,
                        }
                    } else {
                        assert_eq!(replacement, 1);
                        MmapSource::PrivateAnonymous
                    };
                    replacing
                        .mmap(MmapRequest {
                            address: MmapAddress::Fixed(SOURCE),
                            length: PAGE_SIZE,
                            permissions: RO,
                            maximum: RO,
                            source,
                        })
                        .map(|_| ())
                };
                *recorded.lock() = Some(operation);
                0
            })
            .map_err(|_| TestError("mandatory retirement producer prepare failed"))?
            .publish(),
        );
        until(
            "mandatory replacement did not pause with original leaf",
            || ready.load(Ordering::Acquire),
        )?;
        context.expect_eq(mm.mapping(SOURCE).unwrap().physical, old_folio.phys_addr());
        let truncating = old.clone();
        let observed = fixture.consume_sync(1, move || truncating.truncate(0))?;
        until("old file rmap lost mandatory retirement source", || {
            controls.unmap_phase.load(Ordering::Acquire) == 1 || observed.result.lock().is_some()
        })?;
        context.expect_eq(mm.mapping(SOURCE), None);
        let before = controls.before_sync.lock().ok_or(TestError(
            "mandatory old file retirement did not retain original owners",
        ))?;
        context.expect_eq(before.0, 1);
        context.expect_eq(2 <= before.1, true);
        context.expect_eq(old_inode.mapping().is_canonical(0, &old_folio), true);
        controls.gates[2].complete();
        until("mandatory old file retirement CSD did not finish", || {
            controls.unmap_phase.load(Ordering::Acquire) == 2
        })?;
        context.expect_eq(*controls.after_sync.lock(), Some(before));
        until("mandatory old file truncate did not finish", || {
            observed.result.lock().is_some()
        })?;
        context.expect_eq(*observed.result.lock(), Some(Ok(())));
        context.expect_eq(checked(context, old.stat())?.size, 0);
        context.expect_eq(mm.mapping(SOURCE), None);
        gate.complete();
        mm.ktest_set_vma_unmap_hook(None);
        fixture.prepare_gate.take();
        fixture.join_consumers()?;
        context.expect_eq(*result.lock(), Some(Ok(())));
        old_inode.mapping().ktest_set_unmap_hook(None);
        fixture.truncate_controls.take();
        if replacement == 2 {
            context.expect_eq(mm.fault(SOURCE, UserAccess::Read), Err(VmError::Unmapped));
        } else {
            vm(context, mm.fault(SOURCE, UserAccess::Read))?;
            if replacement == 0 {
                context.expect_eq(
                    mm.mapping(SOURCE).unwrap().physical,
                    new_folio.as_ref().unwrap().phys_addr(),
                );
                mapped_bytes(context, &mm, SOURCE, &expected);
            } else {
                mapped_bytes(context, &mm, SOURCE, &[0; 32]);
                context.expect_eq(
                    mm.mapping(SOURCE).unwrap().physical != old_folio.phys_addr(),
                    true,
                );
            }
        }
        fixture.shared_mm.take();
        context.diagnostic(format_args!("mandatory-file-retirement replacement={} new-VMA-published-old-leaf-still-live=1 real-old-file-truncate-finds-source=1 old-leaf-cleared=1 original-page-and-table-owners-held-through-CSD=1 actual-new-fault-or-unmapped=1 native-MM=1 active-user-root-claim=0 fail-before-runtime-claim=0", replacement));
        Ok(())
    }
    /// 普通用户通过真实 ABI 创建、竞态排他、umask、短长链接与原始字节名字。
    #[test]
    #[ktest(tags("W23", "user-fs", "fs-path"), min_cpus = 2)]
    fn user_create_exclusive_umask_links_and_raw_names(
        context: &mut TestContext<'_>,
        fixture: &mut WriteOwner,
    ) -> TestResult {
        fixture.prepare_namespace(context)?;
        fixture.prepare_namespace_parent(context)?;
        let vfs = fixture.user.path.vfs().clone();
        let fs_context = fixture.user.path.context().clone();
        let image = fixture.shared_image(context, b"N3")?;
        let mm = image.mm.clone();
        fixture.start_shared_as(
            context,
            image,
            Credentials {
                uid: 1001,
                gid: 1001,
            },
        )?;
        fixture.phase(&mm, 0, 1)?;

        let race = fixture.open(context, b"/test/ns-work/ns-race", 2)?;
        let race_again = fixture.open(context, b"/test/ns-work/ns-race", 2)?;
        context.expect_eq(
            checked(context, race.path().unwrap().inode())?
                .ptr_eq(checked(context, race_again.path().unwrap().inode())?),
            true,
        );
        let race_stat = checked(context, race.stat())?;
        context.expect_eq((race_stat.links, race_stat.size), (1, 4));
        let mode = fixture.open(context, b"/test/ns-work/ns-mode", 2)?;
        let mode_stat = checked(context, mode.stat())?;
        context.expect_eq(mode_stat.kind, InodeKind::Regular);
        context.expect_eq((mode_stat.links, mode_stat.size), (1, 0));
        context.expect_eq(mode_stat.mode & 0o7777, 0o7750);
        let data = fixture.open(context, b"/test/ns-work/ns-data", 2)?;
        let data_stat = checked(context, data.stat())?;
        context.expect_eq((data_stat.links, data_stat.size), (1, 4));
        context.expect_eq(data_stat.mode & 0o777, 0o640);
        bytes_at(context, &data, 0, b"MODE")?;
        let readonly = fixture.open(context, b"/test/ns-work/ns-ro", 0)?;
        let readonly_stat = checked(context, readonly.stat())?;
        context.expect_eq((readonly_stat.size, readonly_stat.mode & 0o777), (0, 0o640));
        let shared = fixture.open(context, b"/test/ns-work/ns-shared", 0)?;
        let copied = fixture.open(context, b"/test/ns-work/ns-copy", 0)?;
        context.expect_eq(checked(context, shared.stat())?.mode & 0o777, 0o660);
        context.expect_eq(checked(context, copied.stat())?.mode & 0o777, 0o600);
        let directory = fixture.open(context, b"/test/ns-work/ns-dir", 0x10000)?;
        let directory_stat = checked(context, directory.stat())?;
        context.expect_eq(directory_stat.links, 2);
        context.expect_eq(directory_stat.mode & 0o7777, 0o3750);
        let raw = fixture.open(context, b"/test/ns-work/ns-raw-\xff", 2)?;
        let mut raw_bytes = [0; 4];
        context.expect_eq(checked(context, raw.read_at(0, &mut raw_bytes))?, 4);
        context.expect_eq(&raw_bytes, b"RAW!");
        let short = fixture.hold_nofollow(context, b"/test/ns-work/ns-short")?;
        context.expect_eq(
            checked(context, short.metadata_snapshot())?.kind,
            InodeKind::Symlink,
        );
        context.expect_eq(
            checked(context, short.read_link())?.as_slice(),
            b"/test/ns-work/ns-data",
        );
        let long = fixture.hold_nofollow(context, b"/test/ns-work/ns-long")?;
        context.expect_eq(60 < checked(context, long.read_link())?.len(), true);
        let dangling = fixture.hold_nofollow(context, b"/test/ns-work/ns-dangling")?;
        context.expect_eq(
            checked(context, dangling.read_link())?.as_slice(),
            b"/test/ns-work/ns-missing",
        );
        let workspace = fixture.open(context, b"/test/ns-work", 0x10000)?;
        context.expect_eq(checked(context, workspace.stat())?.gid, 1001);
        context.expect_eq(
            vfs.resolve(&fs_context, b"/test/ns-work/ns-lex", ResolveMode::Follow)
                .err(),
            Some(FsError::NotFound),
        );
        for inode in [&short, &long, &dangling] {
            checked(context, crate::fs::writeback::sync_inode(inode))?;
        }
        let volume = fixture.volume();
        checked(context, crate::fs::writeback::sync_metadata(&volume))?;
        WriteOwner::release(&mm, 1)?;

        fixture.phase(&mm, 0, 2)?;
        let missing = fixture.open(context, b"/test/ns-work/ns-missing", 2)?;
        let missing_stat = checked(context, missing.stat())?;
        context.expect_eq((missing_stat.uid, missing_stat.gid), (1001, 1001));
        let inherited = fixture.open(context, b"/test/ns-work/ns-sgid-child", 2)?;
        let inherited_stat = checked(context, inherited.stat())?;
        context.expect_eq((inherited_stat.uid, inherited_stat.gid), (1001, 1001));
        context.expect_eq(inherited_stat.mode & 0o7777, 0o7750);
        WriteOwner::release(&mm, 2)?;
        fixture.finish_shared(context)?;

        context.expect_eq(
            vfs.resolve(
                &fs_context,
                b"/test/ns-work/ns-permission/denied",
                ResolveMode::Follow,
            )
            .err(),
            Some(FsError::NotFound),
        );
        fixture.remove_namespace_parent(context)?;
        for pathname in [
            b"/test/ns-work/ns-race".as_slice(),
            b"/test/ns-work/ns-mode",
            b"/test/ns-work/ns-data",
            b"/test/ns-work/ns-ro",
            b"/test/ns-work/ns-shared",
            b"/test/ns-work/ns-copy",
            b"/test/ns-work/ns-dir",
            b"/test/ns-work/ns-raw-\xff",
            b"/test/ns-work/ns-short",
            b"/test/ns-work/ns-long",
            b"/test/ns-work/ns-dangling",
            b"/test/ns-work/ns-missing",
            b"/test/ns-work/ns-sgid-child",
        ] {
            context.expect_eq(
                vfs.resolve(&fs_context, pathname, ResolveMode::Follow)
                    .err(),
                Some(FsError::NotFound),
            );
        }
        context.diagnostic(format_args!(
            "W23 ordinary-openat=1 exclusive-race=2 one-winner=1 shared-canonical=1 umask-old-and-inheritance=1 file-mode-special-bits=1 directory-dot-nlink=1 short-long-dangling-symlink=1 nofollow-and-exclusive-symlink=1 raw-byte-name=1 sgid-gid-inheritance=1 fd-reservation-EMFILE=1 permission-tail-slash-root-boundary=1"
        ));
        Ok(())
    }

    /// 硬链接unlink后旧OFD与两类mmap仍读原内容；旧页真实used后的WB软件责任暂留，末使用等待其尾部。
    /// 旧 OFD 的原页由显式有限请求取得，之后只观察真实写入与 WB 尾部。
    #[test]
    #[ktest(tags("W24", "user-fs", "fs-sync", "fs-inode-lifetime"), min_cpus = 2)]
    fn user_unlink_keeps_old_descriptors_until_real_writeback_retires(
        context: &mut TestContext<'_>,
        fixture: &mut WriteOwner,
    ) -> TestResult {
        fixture.prepare_namespace(context)?;
        fixture.prepare_namespace_parent(context)?;
        let image = fixture.shared_image(context, b"N4")?;
        let mm = image.mm.clone();
        fixture.start_shared_as(
            context,
            image,
            Credentials {
                uid: 1001,
                gid: 1001,
            },
        )?;
        fixture.phase(&mm, 0, 1)?;

        let old = fixture.open(context, b"/test/ns-work/ns-old", 2)?;
        let old_inode = fixture.inodes.last().unwrap().clone();
        let number = old_inode.number();
        let volume = fixture.volume();
        let cache = fixture.user.path.owner.cache().clone();
        let watch = cache.watch();
        let disk = fixture.user.path.owner.disk()?.clone();
        let old_stat = checked(context, old.stat())?;
        context.expect_eq((old_stat.links, old_stat.size), (2, 4));
        let mut attempt = ReclaimAttempt::new();
        let folio = checked(
            context,
            old.mapping_inode().unwrap().file_page(0, &mut attempt),
        )?;
        context.expect_eq(old.write_at(0, b"OLD!"), Ok(4));
        let old_tree = tree(context, &old_inode)?;
        context.expect_eq(old_tree.data, 1);
        let ranges = checked(
            context,
            prepare_unlinked_original_writeback(&old_inode, &folio),
        )?;
        let before = disk.test_snapshot();
        disk.test_hold_file_writeback(&folio);
        let syncing_volume = volume.clone();
        let observed = fixture.consume_sync(0, move || {
            let mut ranges = ranges;
            let mut bios = [const { None }; 8];
            let mut completions = [const { None }; 8];
            syncing_volume
                .claim()
                .writeback_batch(
                    &mut ranges,
                    None,
                    &mut bios,
                    &mut completions,
                    syncing_volume.writeback_domain(),
                    || false,
                )
                .map_err(FsError::from)
        })?;
        until(
            "W24 old-file WB did not remain held after real used",
            || {
                let snapshot = disk.test_snapshot();
                before.data_submitted < snapshot.data_submitted
                    && folio.writeback_sectors() != 0
                    && snapshot.inflight != 0
                    && observed.blocked()
                    && disk.test_file_writeback_is_held(&folio)
            },
        )?;

        WriteOwner::release(&mm, 1)?;
        fixture.phase(&mm, 0, 2)?;
        let replacement = fixture.open(context, b"/test/ns-work/ns-old", 2)?;
        let replacement_stat = checked(context, replacement.stat())?;
        context.expect_eq((replacement_stat.links, replacement_stat.size), (1, 4));
        context.expect_eq(
            checked(context, replacement.path().unwrap().inode())?.ptr_eq(&old_inode),
            false,
        );
        context.expect_eq(checked(context, old.stat())?.links, 0);
        bytes_at(context, &old, 0, b"OLD!")?;
        bytes_at(context, &replacement, 0, b"NEW!")?;
        context.expect_eq(disk.test_snapshot().inflight != 0, true);
        WriteOwner::release(&mm, 2)?;
        fixture.phase(&mm, 0, 3)?;
        context.expect_eq(disk.test_file_writeback_is_held(&folio), true);
        context.expect_eq(folio.writeback_sectors() != 0, true);

        fixture.inodes.retain(|inode| !inode.ptr_eq(&old_inode));
        drop((old, old_inode, folio));
        context.expect_eq(disk.test_snapshot().inflight != 0, true);
        context.expect_eq(watch.contains(number), true);
        context.expect_eq(
            checked(context, inode_allocation_bit(&volume, number))?,
            true,
        );
        for physical in &old_tree.blocks {
            context.expect_eq(
                checked(context, block_allocation_bit(&volume, *physical))?,
                Some(true),
            );
        }

        disk.test_resume_completions();
        fixture.join_consumers()?;
        if observed.result() != Some(Ok(())) {
            if let Ok(inode) = cache.iget(&volume, number) {
                fixture.inodes.push(inode);
            }
            return Err(TestError("W24 old-file WB failed after user unlink"));
        }
        disk.test_flush_completions();
        checked(context, watch.wait())?;
        checked(context, volume.wait_inode_reuse(number))?;
        context.expect_eq(watch.contains(number), false);
        context.expect_eq(
            checked(context, inode_allocation_bit(&volume, number))?,
            false,
        );
        for physical in &old_tree.blocks {
            context.expect_eq(
                checked(context, block_allocation_bit(&volume, *physical))?,
                Some(false),
            );
        }

        WriteOwner::release(&mm, 3)?;
        fixture.finish_shared(context)?;
        fixture.remove_namespace_parent(context)?;
        context.expect_eq(
            fixture
                .user
                .path
                .vfs()
                .resolve(
                    fixture.user.path.context(),
                    b"/test/ns-work/ns-old",
                    ResolveMode::Follow,
                )
                .err(),
            Some(FsError::NotFound),
        );
        context.diagnostic(format_args!("W24 real-user-hardlink-unlink=1 old-fd-shared-map-private-map=1 replacement-distinct-canonical=1 accepted-data-WB-used-held-during-unlink=1 old-user-owners-closed-before-WB-release=1 old-slot-and-blocks-held-by-original-WB=1 real-join-and-per-inode-reuse-gate=1 old-slot-and-blocks-returned-after-WB=1"));
        Ok(())
    }

    /// 覆盖rename之后新路径指向source，旧target fd与映射仍锚定原canonical inode。
    #[test]
    #[ktest(tags("W25", "user-fs", "fs-path"), min_cpus = 2)]
    fn user_rename_replaces_name_without_replacing_open_target(
        context: &mut TestContext<'_>,
        fixture: &mut WriteOwner,
    ) -> TestResult {
        fixture.prepare_namespace(context)?;
        fixture.prepare_namespace_parent(context)?;
        let image = fixture.shared_image(context, b"N5")?;
        let mm = image.mm.clone();
        fixture.start_shared_as(
            context,
            image,
            Credentials {
                uid: 1001,
                gid: 1001,
            },
        )?;
        fixture.phase(&mm, 0, 1)?;

        let source = fixture.open(context, b"/test/ns-work/ns-source", 2)?;
        let old_target = fixture.open(context, b"/test/ns-work/ns-target", 2)?;
        let source_stat = checked(context, source.stat())?;
        let target_stat = checked(context, old_target.stat())?;
        context.expect_eq((source_stat.links, source_stat.size), (1, 4));
        context.expect_eq((target_stat.links, target_stat.size), (1, 4));
        context.expect_eq(
            checked(context, source.path().unwrap().inode())?
                .ptr_eq(checked(context, old_target.path().unwrap().inode())?),
            false,
        );
        WriteOwner::release(&mm, 1)?;
        fixture.phase(&mm, 0, 2)?;

        let new_target = fixture.open(context, b"/test/ns-work/ns-target", 2)?;
        context.expect_eq(
            checked(context, new_target.path().unwrap().inode())?
                .ptr_eq(checked(context, source.path().unwrap().inode())?),
            true,
        );
        context.expect_eq(
            checked(context, new_target.path().unwrap().inode())?
                .ptr_eq(checked(context, old_target.path().unwrap().inode())?),
            false,
        );
        context.expect_eq(checked(context, old_target.stat())?.links, 0);
        context.expect_eq(checked(context, source.stat())?.links, 1);
        bytes_at(context, &new_target, 0, b"SRC!")?;
        bytes_at(context, &old_target, 0, b"TGT!")?;
        let plain_target = fixture.open(context, b"/test/ns-work/ns-plain-target", 0)?;
        bytes_at(context, &plain_target, 0, b"PLAIN")?;
        for pathname in [
            b"/test/ns-work/ns-source".as_slice(),
            b"/test/ns-work/ns-plain-source",
            b"/test/ns-work/ns-noreplace-source",
        ] {
            context.expect_eq(
                fixture
                    .user
                    .path
                    .vfs()
                    .resolve(fixture.user.path.context(), pathname, ResolveMode::Follow)
                    .err(),
                Some(FsError::NotFound),
            );
        }
        WriteOwner::release(&mm, 2)?;
        fixture.finish_shared(context)?;
        fixture.remove_namespace_parent(context)?;
        for pathname in [
            b"/test/ns-work/ns-target".as_slice(),
            b"/test/ns-work/ns-plain-target",
            b"/test/ns-work/ns-noreplace-target",
        ] {
            context.expect_eq(
                fixture
                    .user
                    .path
                    .vfs()
                    .resolve(fixture.user.path.context(), pathname, ResolveMode::Follow)
                    .err(),
                Some(FsError::NotFound),
            );
        }
        context.diagnostic(format_args!("W25 real-renameat2-overwrite=1 new-lookup-source-canonical=1 old-target-fd-and-maps-old-content=1 target-nlink-zero=1 source-nlink-preserved=1 ordinary-rename=1 noreplace-EEXIST-and-success=1 metadata-flush-cleanup=1"));
        Ok(())
    }

    /// 真实用户目录改名更新点点与父 nlink，并以共享匿名门同时启动反向父锁顺序。
    #[test]
    #[ktest(tags("W26", "user-fs", "fs-path"), min_cpus = 2)]
    fn user_directory_rename_topology_and_opposite_race(
        context: &mut TestContext<'_>,
        fixture: &mut WriteOwner,
    ) -> TestResult {
        fixture.prepare_namespace(context)?;
        fixture.prepare_namespace_parent(context)?;
        let vfs = fixture.user.path.vfs().clone();
        let fs_context = fixture.user.path.context().clone();
        let image = fixture.shared_image(context, b"N6")?;
        let mm = image.mm.clone();
        fixture.start_shared_as(
            context,
            image,
            Credentials {
                uid: 1001,
                gid: 1001,
            },
        )?;
        fixture.phase(&mm, 0, 1)?;

        let alias_source = fixture.open(context, b"/test/ns-work/ns-alias-source", 2)?;
        let alias_target = fixture.open(context, b"/test/ns-work/ns-alias-target", 2)?;
        context.expect_eq(
            checked(context, alias_source.path().unwrap().inode())?
                .ptr_eq(checked(context, alias_target.path().unwrap().inode())?),
            true,
        );
        context.expect_eq(checked(context, alias_source.stat())?.links, 2);
        let cross_a = fixture.open(context, b"/test/ns-work/ns-cross-a", 2)?;
        let cross_b = fixture.open(context, b"/test/ns-work/ns-cross-b", 2)?;
        context.expect_eq(
            (
                checked(context, cross_a.stat())?.links,
                checked(context, cross_b.stat())?.links,
            ),
            (3, 3),
        );
        let d1 = fixture.open(context, b"/test/ns-work/ns-d1", 0x10000)?;
        let d2 = fixture.open(context, b"/test/ns-work/ns-d2", 0x10000)?;
        context.expect_eq(
            (
                checked(context, d1.stat())?.links,
                checked(context, d2.stat())?.links,
            ),
            (2, 2),
        );
        let d1_a = fixture.open(context, b"/test/ns-work/ns-d1/a", 0)?;
        let d1_b = fixture.open(context, b"/test/ns-work/ns-d1/b", 0)?;
        let d2_a = fixture.open(context, b"/test/ns-work/ns-d2/a", 0)?;
        let d2_b = fixture.open(context, b"/test/ns-work/ns-d2/b", 0)?;
        context.expect_eq(
            checked(context, d1_a.path().unwrap().inode())?
                .ptr_eq(checked(context, cross_a.path().unwrap().inode())?),
            true,
        );
        context.expect_eq(
            checked(context, d2_a.path().unwrap().inode())?
                .ptr_eq(checked(context, cross_a.path().unwrap().inode())?),
            true,
        );
        context.expect_eq(
            checked(context, d1_b.path().unwrap().inode())?
                .ptr_eq(checked(context, cross_b.path().unwrap().inode())?),
            true,
        );
        context.expect_eq(
            checked(context, d2_b.path().unwrap().inode())?
                .ptr_eq(checked(context, cross_b.path().unwrap().inode())?),
            true,
        );
        drop((d1_a, d1_b, d2_a, d2_b));
        context.expect_eq(
            (
                checked(context, d1.stat())?.links,
                checked(context, d2.stat())?.links,
            ),
            (2, 2),
        );
        let a = fixture.open(context, b"/test/ns-work/ns-a", 0x10000)?;
        let b = fixture.open(context, b"/test/ns-work/ns-b", 0x10000)?;
        let a_stat = checked(context, a.stat())?;
        let b_stat = checked(context, b.stat())?;
        context.expect_eq((a_stat.links, b_stat.links), (4, 3));
        for pathname in [
            b"/test/ns-work/ns-a/x".as_slice(),
            b"/test/ns-work/ns-a/a2",
            b"/test/ns-work/ns-b/y",
            b"/test/ns-work/ns-cwd",
        ] {
            drop(fixture.open(context, pathname, 0x10000)?);
        }
        WriteOwner::release(&mm, 1)?;
        fixture.phase(&mm, 0, 2)?;

        let a_stat = checked(context, a.stat())?;
        let b_stat = checked(context, b.stat())?;
        context.expect_eq((a_stat.links, b_stat.links), (3, 4));
        let a_path = checked(
            context,
            vfs.resolve(&fs_context, b"/test/ns-work/ns-a", ResolveMode::Follow),
        )?;
        let b_path = checked(
            context,
            vfs.resolve(&fs_context, b"/test/ns-work/ns-b", ResolveMode::Follow),
        )?;
        context.expect_eq(
            checked(
                context,
                vfs.resolve(&fs_context, b"/test/ns-work/ns-b/x/..", ResolveMode::Follow),
            )?
            .ptr_eq(&b_path),
            true,
        );
        context.expect_eq(
            checked(
                context,
                vfs.resolve(&fs_context, b"/test/ns-work/ns-a/y/..", ResolveMode::Follow),
            )?
            .ptr_eq(&a_path),
            true,
        );
        context.expect_eq(
            vfs.resolve(&fs_context, b"/test/ns-work/ns-a/x", ResolveMode::Follow)
                .err(),
            Some(FsError::NotFound),
        );
        context.expect_eq(
            vfs.resolve(&fs_context, b"/test/ns-work/ns-b/a2", ResolveMode::Follow)
                .is_ok(),
            true,
        );
        context.expect_eq(
            vfs.resolve(&fs_context, b"/test/ns-work/ns-a/y", ResolveMode::Follow)
                .is_ok(),
            true,
        );
        context.expect_eq(checked(context, cross_a.stat())?.links, 1);
        context.expect_eq(checked(context, cross_b.stat())?.links, 1);
        bytes_at(context, &cross_a, 0, b"AAAA")?;
        bytes_at(context, &cross_b, 0, b"BBBB")?;
        let d1_b = fixture.open(context, b"/test/ns-work/ns-d1/b", 0)?;
        let d2_a = fixture.open(context, b"/test/ns-work/ns-d2/a", 0)?;
        context.expect_eq(
            checked(context, d1_b.path().unwrap().inode())?
                .ptr_eq(checked(context, cross_a.path().unwrap().inode())?),
            true,
        );
        context.expect_eq(
            checked(context, d2_a.path().unwrap().inode())?
                .ptr_eq(checked(context, cross_b.path().unwrap().inode())?),
            true,
        );
        bytes_at(context, &d1_b, 0, b"AAAA")?;
        bytes_at(context, &d2_a, 0, b"BBBB")?;
        drop((d1_b, d2_a));
        for pathname in [
            b"/test/ns-work/ns-d1/a".as_slice(),
            b"/test/ns-work/ns-d2/b",
        ] {
            context.expect_eq(
                vfs.resolve(&fs_context, pathname, ResolveMode::Follow)
                    .err(),
                Some(FsError::NotFound),
            );
        }
        context.expect_eq(checked(context, alias_source.stat())?.links, 2);
        context.expect_eq(
            checked(context, alias_source.path().unwrap().inode())?
                .ptr_eq(checked(context, alias_target.path().unwrap().inode())?),
            true,
        );
        context.expect_eq(
            vfs.resolve(&fs_context, b"/test/ns-work/ns-cwd", ResolveMode::Follow)
                .err(),
            Some(FsError::NotFound),
        );
        WriteOwner::release(&mm, 2)?;
        fixture.finish_shared(context)?;
        fixture.remove_namespace_parent(context)?;
        for pathname in [
            b"/test/ns-work/ns-alias-source".as_slice(),
            b"/test/ns-work/ns-alias-target",
            b"/test/ns-work/ns-a",
            b"/test/ns-work/ns-b",
            b"/test/ns-work/ns-cwd",
            b"/test/ns-work/ns-d1",
            b"/test/ns-work/ns-d2",
            b"/test/ns-work/ns-d1/a",
            b"/test/ns-work/ns-d1/b",
            b"/test/ns-work/ns-d2/a",
            b"/test/ns-work/ns-d2/b",
            b"/test/ns-work/ns-cross-a",
            b"/test/ns-work/ns-cross-b",
        ] {
            context.expect_eq(
                vfs.resolve(&fs_context, pathname, ResolveMode::Follow)
                    .err(),
                Some(FsError::NotFound),
            );
        }
        context.diagnostic(format_args!("W26 same-inode-same-parent-noop=1 missing-missing-ENOENT=1 descendant-move-EINVAL=1 sequential-cross-parent-dotdot-nlink=1 old-cwd-rmdir-and-create-rejected=1 opposite-parent-clones-shared-gate-reaped=1 reverse-hardlink-inode-lock-order-rename-complete=1 old-fd-content-and-nlink=1 new-parent-lookups-canonical=1 exchange-whiteout-flags-rejected=1"));
        Ok(())
    }
}
