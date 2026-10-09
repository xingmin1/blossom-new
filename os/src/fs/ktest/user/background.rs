//! 原后台回写与普通 writer 共用真实文件页、设备完成和有界同步责任。

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::block::{Bio, BioAllocator, BioCompletion, BioLane, BioLease, BioReserve, BlkOperation};
use crate::cache::FolioMark;
use crate::fs::background::{global_snapshot, WritebackDomain, WritebackSnapshot};
use crate::fs::file::WriteBuffer;
use crate::fs::ktest::user::write::{
    allocation_counts, bytes_at, shared_alias, SyncObservation, WriteOwner, SHARED_ALIAS,
};
use crate::fs::ktest::*;
use crate::fs::{FileRef, SeekFrom};
use crate::mm::dma::MappedPages;
use crate::mm::reclaim::ReclaimAttempt;
use crate::mm::space::Mm;
use crate::mm::PAGE_SIZE;
use crate::workqueue::testing::{metadata_stats, pool_stats};

const ROUND_PAGES: usize = 96;
const PRESSURE_PAGES: usize = 4097;
const INITIAL_BYTE: u8 = 0x31;
const PRESSURE_BYTE: u8 = 0x86;

/// 门只暂停已结束的原回写轮；生产者的停止位与原 Task 句柄分别拥有取消和离栈责任。
struct WaveControl {
    cancelled: AtomicBool,
    producer_stop: AtomicBool,
    produced: AtomicUsize,
    accepted_bytes: AtomicUsize,
    returned_sequence: AtomicUsize,
    rounds: AtomicUsize,
    last: SpinLock<Option<WritebackSnapshot>>,
    gate: Completion,
}

impl WaveControl {
    /// 正文准备可失败的观察存储，设置期的原资源基线不包含本例控制对象。
    fn try_new() -> Result<Arc<Self>, TestError> {
        Arc::try_new(Self {
            cancelled: AtomicBool::new(false),
            producer_stop: AtomicBool::new(false),
            produced: AtomicUsize::new(0),
            accepted_bytes: AtomicUsize::new(0),
            returned_sequence: AtomicUsize::new(0),
            rounds: AtomicUsize::new(0),
            last: SpinLock::new(None),
            gate: Completion::new(),
        })
        .map_err(|_| TestError("writeback wave observation OOM"))
    }

    /// 快照来自原回调已经完成的数据和元数据；不取得文件锁或更改任何页状态。
    fn completed_round(&self, snapshot: WritebackSnapshot) {
        if self.cancelled.load(Ordering::Acquire) {
            return;
        }
        *self.last.lock() = Some(snapshot);
        self.rounds.fetch_add(1, Ordering::Release);
        self.gate.wait();
    }

    /// 同时观察原轮序号和真实完成等待成员，避免把轮尾短暂状态当作停点。
    fn round(&self, number: usize) -> Result<WritebackSnapshot, TestError> {
        until(
            "background round did not reach its completed boundary",
            || number <= self.rounds.load(Ordering::Acquire) && self.gate.test_waiting() == 1,
        )?;
        if self.rounds.load(Ordering::Acquire) != number {
            return Err(TestError("background round escaped its owned gate"));
        }
        self.last
            .lock()
            .as_ref()
            .copied()
            .ok_or(TestError("completed background round has no snapshot"))
    }

    /// 先禁止后续等待，再唤醒已经登记或即将登记的本轮回调。
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        self.gate.complete();
    }
}

/// 只增加本例门控的清理顺序，卷、设备、消费者句柄与两层资源账仍归原 WriteOwner。
struct BackgroundOwner {
    write: WriteOwner,
    control: Option<Arc<WaveControl>>,
    domain: Option<Arc<WritebackDomain>>,
    retired: Option<InodeCacheWatch>,
    source: Option<Arc<BioReserve>>,
    lane: Option<BioLane>,
    observers: [Option<BioCompletion>; 8],
}

impl BackgroundOwner {
    /// 正常写入分配真实盘块并同步；同 VM 前例内容通过普通 truncate 退出，不假设空文件。
    fn prepare_file(
        &mut self,
        context: &mut TestContext<'_>,
        pages: usize,
    ) -> Result<FileRef, TestError> {
        self.write.prepare_namespace(context)?;
        let file = self.write.open(context, b"/test/write-reserve", 2)?;
        let volume = self.write.volume();
        if volume.geometry().block_size != PAGE_SIZE {
            return Err(TestError(
                "background wave requires the ordinary 4KiB root image",
            ));
        }
        checked(context, file.truncate(0))?;
        let (free_blocks, free_inodes) = allocation_counts(context, &volume)?;
        context.diagnostic(format_args!(
            "background-input block-size={} free-blocks={} free-inodes={} requested-file-pages={} dirty-limit={}",
            volume.geometry().block_size,
            free_blocks,
            free_inodes,
            pages,
            global_snapshot().dirty_limit,
        ));
        if free_blocks < (pages + 64) as u64 {
            return Err(TestError(
                "background wave requires actual free data and mapping blocks",
            ));
        }
        let input = [INITIAL_BYTE; PAGE_SIZE];
        for index in 0..pages {
            if checked(context, file.write_at((index * PAGE_SIZE) as u64, &input))? != PAGE_SIZE {
                return Err(TestError(
                    "background file preparation accepted a short page",
                ));
            }
        }
        checked(context, file.sync_range(0..u64::MAX, false))?;
        drained(&volume)?;
        context.expect_eq(
            checked(context, file.stat())?.size,
            (pages * PAGE_SIZE) as u64,
        );
        Ok(file)
    }

    /// 门只观察武装后选中指定页类别的真实轮，准备期迟到的空轮不占本例门票。
    fn observe_rounds(&mut self, metadata: bool) -> Result<Arc<WaveControl>, TestError> {
        assert!(self.control.is_none() && self.domain.is_none());
        let control = WaveControl::try_new()?;
        let observed = control.clone();
        let domain = self.write.volume().writeback_domain().clone();
        let previous_round = domain.snapshot().rounds;
        let hook: Arc<dyn Fn(WritebackSnapshot) + Send + Sync> =
            Arc::try_new(move |snapshot: WritebackSnapshot| {
                let selected = if metadata {
                    snapshot.round_selected_metadata
                } else {
                    snapshot.round_selected_data
                };
                if previous_round < snapshot.rounds && 0 < selected {
                    observed.completed_round(snapshot);
                }
            })
            .map_err(|_| TestError("writeback round hook OOM"))?;
        domain.ktest_set_round_hook(Some(hook));
        self.domain = Some(domain);
        self.control = Some(control.clone());
        Ok(control)
    }

    /// 撤钩只结束轮尾观察；停止位仍保在夹具，已接受的生产者必须由原 WriteOwner join。
    fn release_rounds(&mut self) {
        if let Some(control) = &self.control {
            control.cancel();
        }
        if let Some(domain) = self.domain.take() {
            domain.ktest_set_round_hook(None);
        }
    }

    /// 普通覆盖与真实共享映射持续产生后继 Dirty，控制者不能直接改页标记。
    fn redirty(
        &mut self,
        file: FileRef,
        mapping: Arc<Mm>,
        control: Arc<WaveControl>,
        outside: u64,
    ) -> Result<Arc<SyncObservation>, TestError> {
        let deadline = time::now()
            .checked_add(time::Duration::from_secs(60))
            .ok_or(TestError("redirty producer deadline overflow"))?;
        self.write.consume_sync(1, move || {
            while !control.producer_stop.load(Ordering::Acquire) {
                if deadline <= time::now() {
                    return Err(FsError::InvalidOperation);
                }
                if file.write_at(1, &[0x52])? != 1 {
                    return Err(FsError::InvalidOperation);
                }
                let copied = mapping.copyout(SHARED_ALIAS + 2, &[0x73]);
                if copied.copied != 1 || copied.error.is_some() {
                    return Err(FsError::BadAddress);
                }
                if file.write_at(outside, &[0x64])? != 1 {
                    return Err(FsError::InvalidOperation);
                }
                control.produced.fetch_add(1, Ordering::Release);
                sched::cond_resched();
            }
            Ok(())
        })
    }
}

impl Fixture<DeviceInventory> for BackgroundOwner {
    /// 原设备库存只由 suite 发现一次，每例继续使用原 WriteOwner 的真实基线。
    fn set_up(
        context: &mut TestContext<'_>,
        inventory: &DeviceInventory,
    ) -> Result<Self, TestError> {
        Ok(Self {
            write: <WriteOwner as Fixture<DeviceInventory>>::set_up(context, inventory)?,
            control: None,
            domain: None,
            retired: None,
            source: None,
            lane: None,
            observers: [const { None }; 8],
        })
    }

    /// 先放行完成、轮尾门和 lane，再 join 原消费者并等 canonical 退休；停止只由无槽卷 owner 执行。
    fn quiesce(&mut self, context: &mut TestContext<'_>) -> TestResult {
        if let Some(control) = &self.control {
            control.producer_stop.store(true, Ordering::Release);
        }
        for disk in &self.write.user.path.owner.disks {
            disk.test_resume_completions();
        }
        self.release_rounds();
        if let Some(source) = &self.source {
            source.notify_available();
        }
        for observer in &mut self.observers {
            if let Some(completion) = observer.as_ref() {
                until("reserved observer retains accepted DMA", || {
                    completion.is_complete()
                })?;
                drop(completion.take_memory());
            }
            drop(observer.take());
        }
        drop(self.lane.take());
        self.write.join_consumers()?;
        if let Some(watch) = self.retired.as_ref() {
            checked(context, watch.wait_retired())?;
        }
        drop(self.retired.take());
        self.control.take();
        drop(self.source.take());
        <WriteOwner as Fixture<DeviceInventory>>::quiesce(&mut self.write, context)
    }

    /// 控制存储已经释放，原 MM 六字段和用户三项元数据逐项比较各自同 VM 基线。
    fn tear_down(self, context: &mut TestContext<'_>) -> TestResult {
        assert!(
            self.control.is_none()
                && self.domain.is_none()
                && self.source.is_none()
                && self.lane.is_none()
        );
        assert!(self.observers.iter().all(Option::is_none));
        assert!(self.retired.is_none());
        <WriteOwner as Fixture<DeviceInventory>>::tear_down(self.write, context)
    }
}

/// 原域不再声明 pending/running 后等待设备工作票；完整回调尾部仍由卷退休同步结束。
fn drained(volume: &Ext4Volume) -> TestResult {
    until(
        "background work retained a pending or running obligation",
        || {
            let state = volume.writeback_domain().snapshot();
            !state.pending && !state.running
        },
    )?;
    volume.claim().disk().test_flush_completions();
    Ok(())
}

/// 本轮入口槽数和两个页预算各自核对，真实接受和完成字节不能由选页数代替。
fn finite_round(context: &mut TestContext<'_>, snapshot: WritebackSnapshot) {
    context.expect_eq(0 < snapshot.round_limit, true);
    context.expect_eq(0 < snapshot.round_scanned, true);
    context.expect_eq(snapshot.round_scanned <= snapshot.round_limit, true);
    context.expect_eq(snapshot.round_selected_data <= 48, true);
    context.expect_eq(snapshot.round_selected_metadata <= 16, true);
    context.expect_eq(
        snapshot.round_selected_data + snapshot.round_selected_metadata <= 64,
        true,
    );
    context.expect_eq(0 < snapshot.round_accepted_bytes, true);
    context.expect_eq(
        snapshot.round_accepted_bytes <= (64 * PAGE_SIZE) as u64,
        true,
    );
    context.expect_eq(
        snapshot.round_completed_bytes <= snapshot.round_accepted_bytes,
        true,
    );
    context.expect_eq(snapshot.running, false);
}

/// 每 CPU 原池累计创建数解释共享 worker 的实际分配，不用 live 差掩盖瞬时 Task。
fn worker_creations() -> usize {
    (0..cpu::count()).map(|id| pool_stats(id).created).sum()
}

/// 同一窗口没有其他测试 actor；manager 真正阻塞后读取原 ID 和原池累计创建事实。
fn creation_snapshot() -> Result<(usize, usize), TestError> {
    until(
        "worker manager did not finish actual Task preparation",
        || (0..cpu::count()).all(crate::workqueue::testing::manager_blocked),
    )?;
    Ok((task::test_created_tasks(), worker_creations()))
}

/// 同一根的正式脏 inode 槽继续拥有后继义务，不建立测试专用 inode 名单。
fn dirty_inode_present(volume: &Ext4Volume, inode: &InodeRef) -> bool {
    (0..volume.dirty_inode_limit())
        .filter_map(|slot| volume.dirty_inode_at(slot))
        .any(|dirty| dirty.ptr_eq(inode))
}

/// 至少两次实际覆盖与映射写结束后观察 Dirty/WB 同页共存，未运行的 actor 不能充当生产者。
fn redirty_observed(
    control: &WaveControl,
    producer: &SyncObservation,
    folio: &FolioRef,
) -> TestResult {
    until(
        "ordinary and mapped producer did not redirty the held original page",
        || {
            producer.result().is_some()
                || (2 <= control.produced.load(Ordering::Acquire)
                    && folio.dirty_sectors() != 0
                    && folio.writeback_sectors() != 0)
        },
    )?;
    if producer.result().is_some() {
        return Err(TestError(
            "redirty producer exited before the original completion",
        ));
    }
    Ok(())
}

/// 控制 Task 保持就绪并让出 CPU，观察短于普通采样间隔的真实限速阻塞；期限不随写进度延长。
fn throttled_writer(
    domain: &WritebackDomain,
    writer: &SyncObservation,
    earlier_waits: u64,
) -> Result<WritebackSnapshot, TestError> {
    let deadline = time::now()
        .checked_add(time::Duration::from_secs(60))
        .ok_or(TestError("dirty writer observation deadline overflow"))?;
    loop {
        let snapshot = domain.snapshot();
        let (limit, _) = domain.ktest_limits();
        if limit < snapshot.dirty_pages + snapshot.writeback_pages
            && earlier_waits < snapshot.writer_waits
            && 0 < snapshot.waiting_writers
            && writer.blocked()
        {
            return Ok(snapshot);
        }
        if writer.result().is_some() {
            return Err(TestError(
                "pressure writer exited before a real throttled wait",
            ));
        }
        if deadline <= time::now() {
            return Err(TestError(
                "ordinary writer never blocked at real domain pressure",
            ));
        }
        sched::yield_now();
    }
}

/// 同字节更新真实盘块窗口并取得原 metadata WB 凭证，不更改盘面内容或复制请求来源。
fn unchanged_metadata_writeback(
    volume: &Ext4Volume,
    block: u64,
) -> Result<crate::mm::FolioWriteback, FsError> {
    let size = volume.geometry().block_size;
    let buffer = volume.metadata_cache().get(block, size, None)?;
    buffer.update(volume.claim(), |bytes| {
        let first = bytes[0];
        bytes[..1].copy_from_slice(&[first]);
    })?;
    let mut content = buffer.folio().lock_content();
    // SAFETY: 真实 metadata 窗口已读入且同字节更新建立 Dirty；内容锁排斥修改，
    // 此终身 metadata 页没有用户映射，凭证沿原 FsClaim/Bio 全尾收权。
    let writeback =
        unsafe { content.begin_writeback(0, size)? }.ok_or(FsError::InvalidOperation)?;
    Ok(writeback.with_writeback_domain(volume.writeback_domain(), false))
}

/// 一个实际完整 lease 只构造一份原页父；构造结束后 lease 尾独立归还。
fn reserved_metadata_bio(
    volume: &Ext4Volume,
    block: u64,
    writeback: crate::mm::FolioWriteback,
    lease: BioLease,
) -> Result<Bio, FsError> {
    let mut memory = lease.memory()?;
    let mapped = MappedPages::from_writeback(writeback).map_err(|(_, writeback)| {
        drop(writeback);
        FsError::InvalidOperation
    })?;
    memory.push(mapped);
    // SAFETY: 映射消耗已初始化原 metadata WB 凭证，未导出 CPU 裸别名，
    // 原 FsClaim 持页及设备责任到全部 child/生成者真实收权，None 不创建完成 Task。
    let bio = unsafe {
        Bio::try_new(
            volume.claim().disk(),
            BlkOperation::Write,
            block * volume.geometry().block_size as u64 / 512,
            memory,
            0,
            None,
        )
    }?;
    drop(lease);
    Ok(bio)
}

/// CSD 同步参数只移动真实末拥有；回调不等待、分配或执行文件系统操作。
unsafe fn put_observer<T: Send>(argument: *mut ()) {
    assert!(!crate::arch::irq_enabled());
    // SAFETY: caller 将独占 Option<T> 栈槽保持到 cpu::call 返回；同步回调是唯一访问者，
    // T 为真实 observer 且其 Drop 只归还 allocator/原页，不进入睡眠或 join。
    let observer = unsafe { &mut *argument.cast::<Option<T>>() };
    drop(observer.take());
}

/// 调用者保持独占槽到同步 CSD 完成，只向真实在线异核交末次 observer。
///
/// # Safety
/// T 的末次 Drop 必须只归还 allocator/原页，不得睡眠、等待 IO 或 join；target 必须在线。
unsafe fn remote_observer_put<T: Send>(observer: &mut Option<T>, target: usize) {
    // SAFETY: target 由在线 CPU 表选择，栈槽在同步返回前存活且只由回调访问；
    // 本核无回调所需锁并服务 IPI，observer 末 put 仅运行非睡眠 allocator 析构。
    unsafe {
        cpu::call(
            target,
            put_observer::<T>,
            core::ptr::from_mut(observer).cast(),
        )
    };
}

/// 向既有文件系统测试目录返回唯一 suite 锚点，不增加运行入口或设备发现。
pub(crate) fn anchor() -> &'static ::ktest::SuiteSlot {
    waves::__ktest_anchor()
}

#[ktest::suite(fixture = BackgroundOwner, suite_resources = DeviceInventory, context = "Thread", tags("fs-background", "fs-sync", "fs-path"), resources("root"), timeout_ms = 240000)]
pub(crate) mod waves {
    use super::*;

    /// 正常卸载与最后 canonical 使用先退休卷服务；唯一真 Node Weak 的 allocator 才延长同源 C 页。
    #[test]
    #[ktest(tags("W34", "fs-reserve-retire"), resources("extra"), min_cpus = 2)]
    fn unmounted_volume_retires_before_remote_last_node_weak_returns_source_pages(
        context: &mut TestContext<'_>,
        fixture: &mut BackgroundOwner,
    ) -> TestResult {
        let root_disk = fixture.write.user.path.owner.disk()?.clone();
        let extra_disk = fixture
            .write
            .user
            .path
            .owner
            .disks
            .get(1)
            .ok_or(TestError("reserve retirement requires ordinary extra disk"))?
            .clone();
        let host = checked(
            context,
            Ext4Volume::open(root_disk, AccessMode::ReadOnly, 64),
        )?;
        let volume = checked(
            context,
            Ext4Volume::open(extra_disk.clone(), AccessMode::ReadWrite, 64),
        )?;
        fixture.write.cold_volume = Some(volume.clone());
        if volume.geometry().block_size != PAGE_SIZE {
            return Err(TestError(
                "source retirement requires ordinary 4KiB extra image",
            ));
        }
        let cache = Arc::try_new(checked(context, InodeCache::try_new(16))?)
            .map_err(|_| TestError("mounted retirement inode cache OOM"))?;
        fixture.retired = Some(cache.watch());
        let watch = cache.watch();
        let vfs = checked(
            context,
            Vfs::try_new(host.clone(), cache.clone(), 128, 4, AccessMode::ReadOnly),
        )?;
        let fs_context = checked(context, FsContext::new(vfs.root(), vfs.root()))?;
        let covered = checked(
            context,
            vfs.resolve(&fs_context, b"/mnt/extra", ResolveMode::Follow),
        )?;
        checked(
            context,
            vfs.mount(&covered, volume.clone(), AccessMode::ReadWrite),
        )?;
        let mounted = checked(
            context,
            vfs.resolve(&fs_context, b"/mnt/extra", ResolveMode::Follow),
        )?;
        let inode = checked(context, mounted.inode())?.clone();
        checked(
            context,
            inode.update_metadata(volume.claim(), |attributes| attributes.mode ^= 0o100),
        )?;
        checked(context, volume.sync())?;
        drained(&volume)?;
        let metadata = volume.metadata_cache().clone();
        let source = metadata
            .reserve()
            .ok_or(TestError("mounted RW source absent"))?;
        fixture.source = Some(source.clone());
        if source.capacity() != 64 {
            return Err(TestError("source retirement requires actual C=64 reserve"));
        }
        let block = volume.geometry().blocks_count - 1;
        let buffer = checked(
            context,
            metadata.get(block, PAGE_SIZE, None).map_err(FsError::from),
        )?;
        let physical = buffer.folio().phys_addr();
        let slot = source
            .metadata_slot_for(buffer.folio())
            .ok_or(TestError("cold metadata is not an original source page"))?;
        let mut observer = Some(
            metadata
                .ktest_node(block * PAGE_SIZE as u64)
                .ok_or(TestError("cold metadata has no genuine Node Weak"))?,
        );
        let held = source.snapshot();
        drop(buffer);
        metadata.prune_unused();
        context.expect_eq(observer.as_ref().unwrap().upgrade().is_none(), true);
        context.expect_eq(source.snapshot().metadata_control_live, 1);
        context.expect_eq(source.metadata_folio(slot).phys_addr(), physical);
        context.expect_eq(vfs.unmount(&covered).err(), Some(FsError::Busy));
        drop(inode);
        drop(mounted);
        checked(context, vfs.unmount(&covered))?;
        let after_unmount = checked(
            context,
            vfs.resolve(&fs_context, b"/mnt/extra", ResolveMode::Follow),
        )?;
        context.expect_eq(after_unmount.ptr_eq(&covered), true);
        drop(after_unmount);
        drop(covered);
        drop(fs_context);
        drop(vfs);
        drop(host);
        drop(cache);
        checked(context, watch.wait_retired())?;
        drop(watch);
        drop(fixture.retired.take());
        extra_disk.test_flush_completions();
        let stopped = volume.writeback_domain().snapshot();
        context.expect_eq(
            (stopped.active, stopped.pending, stopped.running),
            (false, false, false),
        );
        let weak_volume = Arc::downgrade(&volume);
        let weak_metadata = Arc::downgrade(&metadata);
        let weak_domain = Arc::downgrade(volume.writeback_domain());
        let weak_source = Arc::downgrade(&source);
        let mut original_pages = [0; 64];
        for (index, address) in original_pages.iter_mut().enumerate() {
            let folio = source.metadata_folio(index);
            *address = folio.phys_addr();
            context.expect_eq(folio.ref_count(), 2);
        }
        context.expect_eq(
            original_pages
                .iter()
                .enumerate()
                .all(|(index, address)| !original_pages[..index].contains(address)),
            true,
        );
        context.diagnostic(format_args!(
            "W34 retired-before-last-weak source={:p} C={} observed-slot={} observed-pfn={} original-pages={:?} layout-held={:?} source-tail={:?} stopped={:?} driver={:?} work-metadata={:?}",
            Arc::as_ptr(&source), source.capacity(), slot, physical / PAGE_SIZE, original_pages,
            held, source.snapshot(), stopped, extra_disk.test_snapshot(), metadata_stats(),
        ));
        drop(metadata);
        drop(volume);
        drop(fixture.write.cold_volume.take());
        drop(fixture.source.take());
        drop(source);
        context.expect_eq(weak_volume.upgrade().is_none(), true);
        context.expect_eq(weak_metadata.upgrade().is_none(), true);
        context.expect_eq(weak_domain.upgrade().is_none(), true);
        context.expect_eq(weak_source.strong_count(), 1);
        let before_put = mm::stats();
        let target = (cpu::current().id + 1) % cpu::count();
        // SAFETY: 真 Node Weak 仅结束 IndexAllocator 控制尾及同源无 owner 基准页，
        // cache/canonical/Work 已退休；目标在线，独占栈槽保持到同步返回，无 FS/源锁。
        unsafe { remote_observer_put(&mut observer, target) };
        let after_put = mm::stats();
        context.expect_eq(weak_source.upgrade().is_none(), true);
        context.expect_eq(weak_source.strong_count(), 0);
        let free_before = before_put.buddy_free_pages + before_put.pcp_cached_pages;
        let free_after = after_put.buddy_free_pages + after_put.pcp_cached_pages;
        context.expect_eq(free_before + 64 <= free_after, true);
        context.diagnostic(format_args!(
            "W34 final-node-weak remote-put-cpu={} callback-irq-save=1 sleeping-callback=0 source-expired={} C-original-pages=64 actual-free-before={} actual-free-after={} before={:?} after={:?} volume-metadata-work-expired-before-put=1 normal-unmount=1",
            target, weak_source.upgrade().is_none(), free_before, free_after, before_put, after_put,
        ));
        Ok(())
    }

    /// 先在活卷回收旧 canonical，再由 cache owner 退休唤醒真实源与 scratch 等待者；新绑定不复活旧边界。
    #[test]
    #[case("zero_submit", false)]
    #[case("real_heap_oom", true)]
    #[ktest(tags("W34", "fs-reserve-stop", "fs-oom"), min_cpus = 2)]
    fn stopped_source_wakes_original_borrower_and_rejects_late_metadata_context(
        context: &mut TestContext<'_>,
        fixture: &mut BackgroundOwner,
        oom: bool,
    ) -> TestResult {
        let disk = fixture.write.user.path.owner.disk()?.clone();
        let volume = checked(
            context,
            Ext4Volume::open(disk.clone(), AccessMode::ReadWrite, 64),
        )?;
        fixture.write.cold_volume = Some(volume.clone());
        let cache = Arc::try_new(checked(context, InodeCache::try_new(4))?)
            .map_err(|_| TestError("source stop canonical cache OOM"))?;
        let watch = cache.watch();
        fixture.retired = Some(cache.watch());
        drop(checked(context, cache.iget(&volume, 2))?);
        let domain = volume.writeback_domain().clone();
        let source = volume
            .metadata_cache()
            .reserve()
            .ok_or(TestError("source stop has no reserve"))?;
        fixture.source = Some(source.clone());
        let old_context = source.metadata_io_context();
        let cold_block = volume.geometry().blocks_count - 1;
        context.expect_eq(
            checked(
                context,
                volume
                    .metadata_cache()
                    .cached_buffer(cold_block, volume.geometry().block_size)
                    .map_err(FsError::from),
            )?
            .is_none(),
            true,
        );
        let before = domain.snapshot();
        // cache_open 时末槽回收仍保卷服务，旧 inode 的 evict 必须在零 lane 压力与 stop 之前完成。
        context.diagnostic(format_args!(
            "W34 stop-stage=old-canonical-evict source={:p} oom={} slots={} active={}",
            Arc::as_ptr(&source),
            oom,
            cache.len(),
            before.active,
        ));
        context.expect_eq(cache.prune_unused(), 1);
        checked(context, watch.wait())?;
        context.expect_eq(watch.contains(2), false);
        context.expect_eq(cache.is_empty(), true);
        context.expect_eq(domain.snapshot().active, true);
        if !cache.is_empty() {
            return Err(TestError(
                "old canonical slot survived active-domain eviction",
            ));
        }
        fixture.lane = Some(checked(
            context,
            source.acquire_wb_lane(|| false).map_err(FsError::from),
        )?);
        context.expect_eq(source.snapshot().free_wb, 0);
        let control = WaveControl::try_new()?;
        fixture.control = Some(control.clone());
        let borrowing = domain.clone();
        let borrower = fixture
            .write
            .consume_sync(0, move || borrowing.borrow_storage().map(drop))?;
        let stopping = cache.watch();
        let stop_control = control.clone();
        let stopper = fixture.write.consume_sync(1, move || {
            stop_control.gate.wait();
            drop(cache);
            stopping.wait_retired()?;
            Ok(())
        })?;
        until("original storage borrower did not block on zero WB", || {
            borrower.blocked() && 0 < source.ktest_waiting()
        })?;
        let boundary = domain.request_progress(&volume);
        until(
            "original callback did not block behind borrowed scratch",
            || domain.snapshot().running,
        )?;
        let blocked = domain.snapshot();
        context.diagnostic(format_args!(
            "W34 stop-stage=old-owner-retire source={:p} zero-wb={} borrower-blocked={} source-waiting={} callback-running={} old-canonical-absent={}",
            Arc::as_ptr(&source), source.snapshot().free_wb == 0, borrower.blocked(),
            source.ktest_waiting(), blocked.running, !watch.contains(2),
        ));
        control.gate.complete();
        until(
            "external stop did not wake and finish original borrower and callback",
            || borrower.result().is_some() && stopper.result().is_some(),
        )?;
        fixture.write.join_consumers()?;
        context.expect_eq(
            borrower.result(),
            Some(Err(FsError::Io(BlockError::QueueStopped))),
        );
        context.expect_eq(stopper.result(), Some(Ok(())));
        let stopped = domain.snapshot();
        let old_progress = domain.wait_progress(boundary);
        context.expect_eq(old_progress.stopped, true);
        context.expect_eq(before.stop_sequence < stopped.stop_sequence, true);
        context.expect_eq(before.finished_rounds < stopped.finished_rounds, true);
        context.expect_eq(stopped.running, false);
        context.expect_eq(stopped.pending, false);
        context.expect_eq(source.ktest_waiting(), 0);
        let reads = disk.test_snapshot().submitted;
        context.expect_eq(
            volume
                .metadata_cache()
                .get(cold_block, volume.geometry().block_size, Some(old_context))
                .err(),
            Some(BlockError::NotReady),
        );
        context.expect_eq(disk.test_snapshot().submitted, reads);
        drop(fixture.lane.take());
        checked(context, watch.wait_retired())?;
        drop(watch);
        drop(fixture.retired.take());
        context.diagnostic(format_args!(
            "W34 stop-stage=new-canonical-bind source={:p} old-stop-sequence={} active={}",
            Arc::as_ptr(&source),
            stopped.stop_sequence,
            domain.snapshot().active,
        ));
        let next = Arc::try_new(checked(context, InodeCache::try_new(4))?)
            .map_err(|_| TestError("new source canonical cache OOM"))?;
        let next_watch = next.watch();
        fixture.retired = Some(next.watch());
        let root = checked(context, next.iget(&volume, 2))?;
        context.expect_eq(domain.snapshot().active, true);
        context.expect_eq(domain.wait_progress(boundary).stopped, true);
        context.expect_eq(
            borrower.result(),
            Some(Err(FsError::Io(BlockError::QueueStopped))),
        );
        context.expect_eq(
            volume
                .metadata_cache()
                .get(cold_block, volume.geometry().block_size, Some(old_context))
                .err(),
            Some(BlockError::NotReady),
        );
        drop(checked(
            context,
            volume
                .metadata_cache()
                .get(cold_block, volume.geometry().block_size, None)
                .map_err(FsError::from),
        )?);
        let pressure = if oom {
            Some(
                fixture
                    .write
                    .user
                    .path
                    .owner
                    .pressure
                    .take()
                    .ok_or(TestError("finite round real heap pressure missing"))?,
            )
        } else {
            None
        };
        let evidence = Arc::try_new(SpinLock::new(None))
            .map_err(|_| TestError("finite round evidence OOM"))?;
        let actor_evidence = evidence.clone();
        let round_domain = domain.clone();
        let round_volume = volume.clone();
        let finite = fixture.write.consume_sync(0, move || {
            let mut pressure = pressure;
            let allocation = pressure.as_mut().map(|pressure| pressure.exhaust());
            let start = round_domain.snapshot();
            let boundary = round_domain.request_progress(&round_volume);
            let progress = round_domain.wait_progress(boundary);
            let finish = round_domain.snapshot();
            drop(pressure);
            *actor_evidence.lock() = Some((allocation, start, finish, progress));
            Ok(())
        })?;
        until(
            "finite zero-submit/OOM round did not publish its actual finished boundary",
            || finite.result().is_some(),
        )?;
        fixture.write.join_consumers()?;
        context.expect_eq(finite.result(), Some(Ok(())));
        let (allocation, start, finish, progress) = evidence
            .lock()
            .take()
            .ok_or(TestError("finite round has no real boundary evidence"))?;
        context.expect_eq(allocation, oom.then_some((true, true)));
        context.expect_eq(start.finished_rounds < finish.finished_rounds, true);
        context.expect_eq(
            (
                progress.submitted_bytes,
                progress.completed_bytes,
                progress.stopped,
                progress.error,
            ),
            (0, 0, false, false),
        );
        context.expect_eq(
            (finish.round_selected_data, finish.round_selected_metadata),
            (0, 0),
        );
        // 第二次 owner 退休也先结束 canonical；同字节 Buffer 更新保原 metadata Dirty 与真实接受责任。
        drop(root);
        context.diagnostic(format_args!(
            "W34 stop-stage=new-canonical-evict source={:p} slots={} active={} finite-finished-rounds={}",
            Arc::as_ptr(&source), next.len(), domain.snapshot().active, finish.finished_rounds,
        ));
        context.expect_eq(next.prune_unused(), 1);
        checked(context, next_watch.wait())?;
        context.expect_eq(next_watch.contains(2), false);
        context.expect_eq(next.is_empty(), true);
        context.expect_eq(domain.snapshot().active, true);
        if !next.is_empty() {
            return Err(TestError(
                "new canonical slot survived active-domain eviction",
            ));
        }
        let buffer = checked(
            context,
            volume
                .metadata_cache()
                .get(cold_block, volume.geometry().block_size, None)
                .map_err(FsError::from),
        )?;
        disk.test_suspend_completions();
        checked(
            context,
            buffer
                .update(volume.claim(), |bytes| {
                    let first = bytes[0];
                    bytes[..1].copy_from_slice(&[first]);
                })
                .map_err(FsError::from),
        )?;
        drop(buffer);
        let accepted_boundary = domain.request_progress(&volume);
        until(
            "background callback did not accept original metadata before stop",
            || {
                domain.snapshot().running
                    && 0 < disk.test_snapshot().inflight
                    && 0 < source.snapshot().control_live
            },
        )?;
        let accepted = disk.test_snapshot();
        context.diagnostic(format_args!(
            "W34 stop-stage=accepted-owner-retire source={:p} old-canonical-absent={} accepted-driver={:?} source-layout={:?}",
            Arc::as_ptr(&source), !next_watch.contains(2), accepted, source.snapshot(),
        ));
        let io_stopping = next.watch();
        let io_stopper = fixture.write.consume_sync(1, move || {
            drop(next);
            io_stopping.wait_retired()?;
            Ok(())
        })?;
        until(
            "external stop did not wait for accepted IO/Work tail",
            || finish.stop_sequence < domain.snapshot().stop_sequence && io_stopper.blocked(),
        )?;
        context.expect_eq(io_stopper.result(), None);
        context.expect_eq(domain.wait_progress(accepted_boundary).stopped, true);
        disk.test_resume_completions();
        fixture.write.join_consumers()?;
        disk.test_flush_completions();
        context.expect_eq(io_stopper.result(), Some(Ok(())));
        context.expect_eq(domain.snapshot().active, false);
        context.expect_eq(domain.snapshot().pending, false);
        context.expect_eq(domain.snapshot().running, false);
        context.expect_eq(disk.test_snapshot().inflight, 0);
        context.expect_eq(source.snapshot().control_live, 0);
        context.expect_eq(source.snapshot().payload_live, 0);
        checked(context, next_watch.wait_retired())?;
        drop(next_watch);
        drop(fixture.retired.take());
        context.diagnostic(format_args!(
            "W34 stop source={:p} oom={} allocation-null={:?} borrower-blocked=1 callback-running-with-scratch-owned=1 before={:?} blocked={:?} stopped={:?} finite-start={:?} finite-finished={:?} progress={:?} accepted-driver={:?} finished-driver={:?} source-layout={:?} old-context-after-stop-and-rebind=NotReady old-waiter-stopped=1 canonical-evicted-before-owner-stop=1 canonical-retired-before-rebind=1 accepted-stop-by-owner=1",
            Arc::as_ptr(&source), oom, allocation, before, blocked, stopped, start, finish, progress,
            accepted, disk.test_snapshot(), source.snapshot(),
        ));
        fixture.control.take();
        fixture.source.take();
        Ok(())
    }

    /// 八个已完成的原父仍占真实 allocator 尾，第九个预建消费者只能从同源一单元前进。
    #[test]
    #[ktest(tags("W33", "W34", "fs-reserve-observer"), min_cpus = 2)]
    fn eight_completed_reserved_parents_block_ninth_until_real_weak_tail(
        context: &mut TestContext<'_>,
        fixture: &mut BackgroundOwner,
    ) -> TestResult {
        let disk = fixture.write.user.path.owner.disk()?.clone();
        let volume = checked(
            context,
            Ext4Volume::open(disk.clone(), AccessMode::ReadWrite, 64),
        )?;
        fixture.write.cold_volume = Some(volume.clone());
        if volume.geometry().block_size != PAGE_SIZE {
            return Err(TestError(
                "reserve observer requires ordinary 4KiB root image",
            ));
        }
        let source = volume
            .metadata_cache()
            .reserve()
            .ok_or(TestError("RW metadata has no real reserve"))?;
        context.expect_eq(volume.writeback_domain().snapshot().active, false);
        context.expect_eq(volume.writeback_domain().snapshot().rounds, 0);
        fixture.source = Some(source.clone());
        let lane = checked(
            context,
            source.acquire_wb_lane(|| false).map_err(FsError::from),
        )?;
        context.expect_eq(lane.len(), 8);
        fixture.lane = Some(lane);
        let control = WaveControl::try_new()?;
        fixture.control = Some(control.clone());
        let ninth = checked(context, unchanged_metadata_writeback(&volume, 9))?;
        let record = Arc::try_new(SpinLock::new(None))
            .map_err(|_| TestError("ninth acceptance observation OOM"))?;
        let actor_record = record.clone();
        let actor_source = source.clone();
        let actor_volume = volume.clone();
        let actor_control = control.clone();
        let actor = fixture.write.consume_sync(0, move || {
            actor_control.gate.wait();
            let lane =
                actor_source.acquire_wb_lane(|| actor_control.cancelled.load(Ordering::Acquire))?;
            let count = lane.len();
            let (_, lease) =
                lane.lease_available(|| actor_control.cancelled.load(Ordering::Acquire))?;
            let bio = reserved_metadata_bio(&actor_volume, 9, ninth, lease)?;
            let completion = bio.completion();
            actor_volume.claim().submit_bio(bio);
            let result = completion.wait();
            let memory = completion.take_memory()?;
            let reserved = matches!(memory.allocator(), BioAllocator::Reserved { .. });
            *actor_record.lock() = Some((
                count,
                completion.test_acceptance(),
                result,
                reserved,
                memory.as_ptr() as usize,
            ));
            drop(memory);
            drop(completion);
            drop(lane);
            result.error.map_or(Ok(()), |error| Err(FsError::Io(error)))
        })?;
        let before = disk.test_snapshot();
        for index in 0..8 {
            let block = index as u64 + 1;
            let writeback = checked(context, unchanged_metadata_writeback(&volume, block))?;
            let lease = checked(
                context,
                fixture
                    .lane
                    .as_ref()
                    .unwrap()
                    .try_lease(index, || false)
                    .map_err(FsError::from),
            )?
            .ok_or(TestError("prepared WB book lost a complete unit"))?;
            let bio = checked(
                context,
                reserved_metadata_bio(&volume, block, writeback, lease),
            )?;
            let completion = bio.completion();
            fixture.observers[index] = Some(completion.clone());
            volume.claim().submit_bio(bio);
            let result = completion.wait();
            context.expect_eq((result.completed, result.error), (PAGE_SIZE, None));
            context.expect_eq(completion.test_acceptance().2, PAGE_SIZE);
        }
        disk.test_flush_completions();
        let completed = disk.test_snapshot();
        let held = source.snapshot();
        context.expect_eq(completed.inflight, 0);
        context.expect_eq(held.control_live, 8);
        context.expect_eq(held.payload_live, 8);
        let mut returned = [const { None }; 8];
        let mut payload_addresses = [0; 8];
        for (index, (memory, completion)) in returned.iter_mut().zip(&fixture.observers).enumerate()
        {
            let owned = checked(
                context,
                completion
                    .as_ref()
                    .unwrap()
                    .take_memory()
                    .map_err(FsError::from),
            )?;
            context.expect_eq(
                matches!(owned.allocator(), BioAllocator::Reserved { .. }),
                true,
            );
            payload_addresses[index] = owned.as_ptr() as usize;
            *memory = Some(owned);
        }
        context.expect_eq(source.snapshot().payload_live, 8);
        drop(returned);
        context.expect_eq(source.snapshot().payload_live, 0);
        context.expect_eq(source.snapshot().control_live, 8);
        let mut weak = Some(fixture.observers[0].as_ref().unwrap().ktest_weak());
        drop(fixture.lane.take());
        control.gate.complete();
        until(
            "ninth consumer never blocked on the original empty source",
            || actor.blocked() && 0 < source.ktest_waiting(),
        )?;
        let blocked = disk.test_snapshot();
        context.expect_eq(actor.result(), None);
        context.expect_eq(source.snapshot().free_wb, 0);
        context.expect_eq(blocked.metadata_submitted, completed.metadata_submitted);
        drop(fixture.observers[0].take());
        context.expect_eq(source.snapshot().control_live, 8);
        context.expect_eq(source.snapshot().free_wb, 0);
        context.expect_eq(actor.blocked(), true);
        let target = (cpu::current().id + 1) % cpu::count();
        // SAFETY: 真 Bio Weak 保有实际 BioAllocator 控制尾；返回 Vec 已 Drop，
        // driver/生成者尾已 flush，目标在线，末 put 只归控制 allocator，不睡眠或 join。
        unsafe { remote_observer_put(&mut weak, target) };
        until(
            "one real allocator tail did not admit and finish the ninth parent",
            || actor.result().is_some(),
        )?;
        fixture.write.join_consumers()?;
        context.expect_eq(actor.result(), Some(Ok(())));
        let (count, acceptance, result, reserved, ninth_payload) = record
            .lock()
            .take()
            .ok_or(TestError("ninth parent has no actual acceptance evidence"))?;
        context.expect_eq(count, 1);
        context.expect_eq(
            (acceptance.2, result.completed, result.error, reserved),
            (PAGE_SIZE, PAGE_SIZE, None, true),
        );
        context.expect_eq(ninth_payload, payload_addresses[0]);
        disk.test_flush_completions();
        let after = source.snapshot();
        context.expect_eq(after.control_live, 7);
        context.expect_eq(after.payload_live, 0);
        context.expect_eq(after.free_wb, 1);
        context.expect_eq(
            completed.metadata_submitted < disk.test_snapshot().metadata_submitted,
            true,
        );
        context.diagnostic(format_args!(
            "W33-W34 observer source={:p} domain-inactive=1 background-rounds={} real-wb-parents=8 ninth-blocked=1 returned-Vec-before-control=1 last-strong-before-last-weak=1 ninth-book={} ninth-reserved={} payload-addresses={:?} ninth-payload={} driver-before={:?} driver-completed={:?} driver-blocked={:?} driver-after={:?} layout-held={:?} layout-after={:?} weak-put-cpu={}",
            Arc::as_ptr(&source), volume.writeback_domain().snapshot().rounds, count, reserved, payload_addresses, ninth_payload,
            before, completed, blocked, disk.test_snapshot(), held, after, target,
        ));
        for observer in &mut fixture.observers {
            drop(observer.take());
        }
        fixture.control.take();
        fixture.source.take();
        Ok(())
    }

    /// 每轮重新覆盖前半脏集合，后半原页仍须由后续有限后台轮真实写完。
    #[test]
    #[case("file_pages", false)]
    #[case("inode_table_pages", true)]
    #[ktest(tags("W30", "fs-background-progress"), min_cpus = 2)]
    fn hot_prefix_does_not_starve_later_original_pages(
        context: &mut TestContext<'_>,
        fixture: &mut BackgroundOwner,
        metadata: bool,
    ) -> TestResult {
        let file = fixture.prepare_file(context, if metadata { 1 } else { ROUND_PAGES })?;
        let volume = fixture.write.volume();
        let inode = checked(context, file.path().inode())?.clone();
        let mut records = Vec::new();
        let mut record_keys = Vec::new();
        let mut pages = Vec::new();
        pages
            .try_reserve_exact(ROUND_PAGES)
            .map_err(|_| TestError("hot prefix original page observation OOM"))?;
        if metadata {
            records
                .try_reserve_exact(32)
                .map_err(|_| TestError("hot metadata inode observation OOM"))?;
            record_keys
                .try_reserve_exact(32)
                .map_err(|_| TestError("hot metadata original key observation OOM"))?;
            let cache = fixture.write.user.path.vfs().inodes().clone();
            let root = checked(context, cache.iget(&volume, 2))?;
            let directory = checked(context, root.lookup(&cache, b"indexed"))?;
            let mut cursor = checked(context, directory.directory_cursor(0))?;
            let geometry = volume.geometry();
            let per_page = (PAGE_SIZE / geometry.inode_size) as u64;
            for _ in 0..DIRECTORY_ENTRIES + 2 {
                let Some(entry) = checked(context, directory.dir_next(&cursor))? else {
                    break;
                };
                cursor = entry.next_cursor;
                if entry.name == b"." || entry.name == b".." {
                    continue;
                }
                let group = (entry.number - 1) / geometry.inodes_per_group;
                let page = ((entry.number - 1) % geometry.inodes_per_group) / per_page;
                if records.iter().any(|(key, _)| *key == (group, page)) {
                    continue;
                }
                records.push((
                    (group, page),
                    checked(context, cache.iget(&volume, entry.number))?,
                ));
                if records.len() == 32 {
                    break;
                }
            }
            if records.len() != 32 {
                return Err(TestError(
                    "actual indexed directory has fewer than 32 inode table pages",
                ));
            }
            checked(context, volume.sync())?;
            drained(&volume)?;
        } else {
            let mut attempt = ReclaimAttempt::new();
            for index in 0..ROUND_PAGES {
                pages.push((
                    index as u64,
                    checked(context, file.file_page(index as u64, &mut attempt))?,
                ));
            }
        }
        let control = fixture.observe_rounds(metadata)?;
        {
            let _data = inode.mapping().lock_writeback();
            let _metadata = volume.metadata_cache().lock_writeback();
            if metadata {
                for (_, record) in &records {
                    checked(
                        context,
                        record.update_metadata(volume.claim(), |attributes| {
                            attributes.mode ^= 0o100;
                        }),
                    )?;
                    let previous = pages.len();
                    let mut cursor = 0;
                    for _ in 0..volume.metadata_cache().capacity() {
                        let mut selected = [None];
                        let next = volume.metadata_cache().select_range(
                            cursor,
                            u64::MAX,
                            FolioMark::Dirty,
                            &mut selected,
                        );
                        if let Some((key, folio)) = selected[0].take() {
                            if !pages.iter().any(|(old, _)| *old == key) {
                                pages.push((key, folio));
                            }
                        }
                        let Some(next) = next else { break };
                        cursor = next;
                    }
                    if pages.len() != previous + 1 {
                        return Err(TestError(
                            "inode update did not dirty its unique original table page",
                        ));
                    }
                    record_keys.push(pages.last().unwrap().0);
                }
            }
            if checked(context, file.write_at(0, &[0x45]))? != 1 {
                return Err(TestError(
                    "hot prefix ordinary producer accepted a short write",
                ));
            }
            if !metadata {
                for index in 1..ROUND_PAGES {
                    if checked(context, file.write_at((index * PAGE_SIZE) as u64, &[0x45]))? != 1 {
                        return Err(TestError(
                            "hot prefix ordinary producer accepted a short write",
                        ));
                    }
                }
            }
        }
        let first = control.round(1)?;
        finite_round(context, first);
        let prefix = if metadata { 16 } else { 48 };
        pages.sort_unstable_by_key(|(key, page)| (page.dirty_sectors() != 0, *key));
        context.expect_eq(
            pages[..prefix]
                .iter()
                .all(|(_, page)| page.dirty_sectors() == 0),
            true,
        );
        context.expect_eq(
            pages[prefix..]
                .iter()
                .all(|(_, page)| page.dirty_sectors() != 0),
            true,
        );
        for round in 2..=3 {
            if metadata {
                for (position, (_, record)) in records.iter().enumerate() {
                    if pages[..prefix]
                        .iter()
                        .any(|(key, _)| *key == record_keys[position])
                    {
                        checked(
                            context,
                            record.update_metadata(volume.claim(), |attributes| {
                                attributes.mode ^= 0o100;
                            }),
                        )?;
                    }
                }
            } else {
                for (index, _) in &pages[..prefix] {
                    if checked(
                        context,
                        file.write_at(*index * PAGE_SIZE as u64, &[round as u8]),
                    )? != 1
                    {
                        return Err(TestError("hot prefix redirty accepted a short write"));
                    }
                }
            }
            control.gate.complete();
            let completed = control.round(round)?;
            finite_round(context, completed);
            context.diagnostic(format_args!(
                "background-hot-prefix metadata={} round={} prefix={} tail-dirty={} snapshot={:?}",
                metadata,
                round,
                prefix,
                pages[prefix..]
                    .iter()
                    .filter(|(_, page)| page.dirty_sectors() != 0)
                    .count(),
                completed,
            ));
            if pages[prefix..]
                .iter()
                .all(|(_, page)| page.dirty_sectors() == 0)
            {
                break;
            }
        }
        context.expect_eq(
            pages[prefix..]
                .iter()
                .all(|(_, page)| page.dirty_sectors() == 0),
            true,
        );
        fixture.release_rounds();
        checked(context, volume.sync())?;
        drained(&volume)?;
        Ok(())
    }

    /// 卷已静止后只修改 inode 属性，不借普通文件写或显式 sync 启动后台义务。
    #[test]
    #[ktest(tags("W30", "fs-background-metadata"))]
    fn idle_metadata_update_starts_real_background_writeback(
        context: &mut TestContext<'_>,
        fixture: &mut BackgroundOwner,
    ) -> TestResult {
        let file = fixture.prepare_file(context, 1)?;
        let volume = fixture.write.volume();
        let inode = checked(context, file.path().inode())?.clone();
        let before = volume.writeback_domain().snapshot();
        let old_mode = checked(context, inode.metadata_snapshot())?.mode;
        let control = fixture.observe_rounds(true)?;
        checked(
            context,
            inode.update_metadata(volume.claim(), |attributes| {
                attributes.mode ^= 0o100;
            }),
        )?;
        context.expect_eq(checked(context, file.stat())?.mode, old_mode ^ 0o100);
        let after = control.round(1)?;
        context.expect_eq(volume.metadata_cache().has_dirty_or_writeback(), false);
        context.expect_eq(before.completed_bytes < after.completed_bytes, true);
        context.expect_eq(after.round_selected_metadata, 1);
        context.diagnostic(format_args!(
            "background-idle-metadata before={:?} after={:?}",
            before, after
        ));
        fixture.release_rounds();
        drained(&volume)?;
        Ok(())
    }

    /// 空文件创建只产生名字和 inode 元数据，静止卷仍须自行完成到期回写。
    #[test]
    #[ktest(tags("W30", "fs-background-metadata"))]
    fn idle_empty_create_starts_real_background_writeback(
        context: &mut TestContext<'_>,
        fixture: &mut BackgroundOwner,
    ) -> TestResult {
        drop(fixture.prepare_file(context, 1)?);
        let volume = fixture.write.volume();
        let vfs = fixture.write.user.path.vfs().clone();
        let fs_context = fixture.write.user.path.context().clone();
        let root = vfs.root();
        let pathname = b"/test/background-empty";
        let before = volume.writeback_domain().snapshot();
        let control = fixture.observe_rounds(true)?;
        let created = checked(
            context,
            vfs.open_with(
                &fs_context,
                &root,
                pathname,
                (0, 0),
                Some((0o644, 0)),
                true,
                false,
                |path, _| crate::fs::open_path(path),
            ),
        )?;
        let result = (|| {
            context.expect_eq(checked(context, created.stat())?.size, 0);
            let after = control.round(1)?;
            context.expect_eq(volume.metadata_cache().has_dirty_or_writeback(), false);
            context.expect_eq(after.round_selected_data, 0);
            context.expect_eq(before.completed_bytes < after.completed_bytes, true);
            context.diagnostic(format_args!(
                "background-idle-empty-create before={:?} after={:?}",
                before, after,
            ));
            Ok(())
        })();
        fixture.release_rounds();
        let removed = checked(
            context,
            vfs.unlink_from(&fs_context, &root, pathname, (0, 0)),
        );
        drop(created);
        let synced = checked(context, volume.sync());
        let quiescent = drained(&volume);
        result.and(removed).and(synced).and(quiescent)
    }

    /// cache 空退场、首次加载 OOM 和带真实外部使用的退场均结束旧卷服务后才允许新域取得。
    #[test]
    #[case("pruned_empty", 0)]
    #[case("first_load_oom", 1)]
    #[case("live_inode", 2)]
    #[ktest(tags("W30", "fs-background-retire", "fs-oom"), min_cpus = 2)]
    fn retired_cache_allows_same_volume_new_canonical_domain(
        context: &mut TestContext<'_>,
        fixture: &mut BackgroundOwner,
        path: usize,
    ) -> TestResult {
        let volume = checked(
            context,
            Ext4Volume::open(
                fixture.write.user.path.owner.disk()?.clone(),
                AccessMode::ReadWrite,
                64,
            ),
        )?;
        fixture.write.cold_volume = Some(volume.clone());
        let cache = Arc::try_new(checked(context, InodeCache::try_new(4))?)
            .map_err(|_| TestError("retirement cache owner OOM"))?;
        let watch = cache.watch();
        let held = if path == 1 {
            let mut pressure = fixture
                .write
                .user
                .path
                .owner
                .pressure
                .take()
                .ok_or(TestError("retirement allocator pressure was not prepared"))?;
            let observed = Arc::try_new(SpinLock::new(None))
                .map_err(|_| TestError("retirement OOM observation allocation failed"))?;
            let record = observed.clone();
            let original = cache.clone();
            let filesystem = volume.clone();
            let consumer = fixture.write.consume_sync(0, move || {
                let evidence = pressure.exhaust();
                let result = original.iget(&filesystem, 2).err();
                drop(pressure);
                *record.lock() = Some((evidence, result));
                Ok(())
            })?;
            fixture.write.join_consumers()?;
            context.expect_eq(consumer.result(), Some(Ok(())));
            let (evidence, result) = observed.lock().take().ok_or(TestError(
                "retirement OOM producer did not publish its real result",
            ))?;
            context.expect_eq(evidence, (true, true));
            context.expect_eq(result, Some(FsError::NoMemory));
            None
        } else {
            let inode = checked(context, cache.iget(&volume, 2))?;
            checked(
                context,
                inode.update_metadata(volume.claim(), |attributes| {
                    attributes.mode ^= 0o100;
                }),
            )?;
            checked(context, volume.sync())?;
            drained(&volume)?;
            context.expect_eq(
                0 < volume.writeback_domain().snapshot().completed_bytes,
                true,
            );
            if path == 0 {
                drop(inode);
                context.expect_eq(cache.prune_unused(), 1);
                context.expect_eq(cache.len(), 0);
                None
            } else {
                Some(inode)
            }
        };
        drop(cache);
        if let Some(inode) = held {
            context.expect_eq(watch.contains(inode.number()), true);
            let competing = checked(context, InodeCache::try_new(4))?;
            context.expect_eq(competing.iget(&volume, 2).err(), Some(FsError::Busy));
            drop(competing);
            drop(inode);
        }
        checked(context, watch.wait_retired())?;
        drop(watch);
        let next = checked(context, InodeCache::try_new(4))?;
        let next_watch = next.watch();
        let inode = checked(context, next.iget(&volume, 2))?;
        context.expect_eq(
            checked(context, inode.metadata_snapshot())?.kind,
            InodeKind::Directory,
        );
        drop(inode);
        drop(next);
        checked(context, next_watch.wait_retired())?;
        context.diagnostic(format_args!(
            "background-retirement path={} same-volume={} real-domain-rebind=1 snapshot={:?}",
            path,
            volume.device(),
            volume.writeback_domain().snapshot(),
        ));
        Ok(())
    }

    /// 大于单轮上界的真实脏集合与持续再脏共存，两个后台轮都必须有限结束并保留下轮义务。
    #[test]
    #[ktest(tags("W30", "fs-shared"), min_cpus = 2)]
    fn finite_background_round_keeps_successor_dirty(
        context: &mut TestContext<'_>,
        fixture: &mut BackgroundOwner,
    ) -> TestResult {
        let file = fixture.prepare_file(context, ROUND_PAGES)?;
        let inode = checked(context, file.path().inode())?.clone();
        let volume = fixture.write.volume();
        let disk = fixture.write.user.path.owner.disk()?.clone();
        let mut attempt = ReclaimAttempt::new();
        let folio = checked(context, file.file_page(0, &mut attempt))?;
        let mapping = shared_alias(context, &file, 0)?;
        let control = fixture.observe_rounds(false)?;
        let before = disk.test_snapshot();
        let entry_slots = volume.dirty_inode_limit();
        disk.test_hold_file_writeback(&folio);
        // 原扫描互斥仅稳定入口集合，普通写仍通过正式 inode/invalidate/content 路径。
        {
            let _writeback = inode.mapping().lock_writeback();
            for index in 0..ROUND_PAGES {
                if checked(context, file.write_at((index * PAGE_SIZE) as u64, &[0x41]))? != 1 {
                    return Err(TestError(
                        "background initial Dirty producer accepted a short write",
                    ));
                }
            }
        }
        until(
            "background data did not reach real used with its software tail held",
            || {
                disk.test_file_writeback_is_held(&folio)
                    && folio.writeback_sectors() != 0
                    && volume.writeback_domain().snapshot().round_selected_data == 48
            },
        )?;
        let producer = fixture.redirty(
            file.clone(),
            mapping.clone(),
            control.clone(),
            ((ROUND_PAGES - 1) * PAGE_SIZE) as u64,
        )?;
        redirty_observed(&control, &producer, &folio)?;
        context.expect_eq(
            mapping.mapping(SHARED_ALIAS).unwrap().physical,
            folio.phys_addr(),
        );
        bytes_at(context, &file, 0, &[0x41, 0x52, 0x73])?;
        disk.test_resume_completions();
        let first = control.round(1)?;
        finite_round(context, first);
        context.expect_eq(first.round_limit, entry_slots);
        context.expect_eq(first.round_selected_data, 48);
        context.expect_eq(first.round_completed_bytes, first.round_accepted_bytes);
        context.expect_eq(first.later_dirty, true);
        context.expect_eq(ROUND_PAGES - 48 <= first.dirty_pages, true);
        context.expect_eq(
            (folio.dirty_sectors() != 0, folio.writeback_sectors()),
            (true, 0),
        );
        context.expect_eq(dirty_inode_present(&volume, &inode), true);
        context.expect_eq(producer.result(), None);
        let produced = control.produced.load(Ordering::Acquire);
        until(
            "producer stopped while the completed round remained gated",
            || produced < control.produced.load(Ordering::Acquire),
        )?;
        control.gate.complete();
        let second = control.round(2)?;
        finite_round(context, second);
        context.expect_eq(second.rounds, first.rounds + 1);
        context.expect_eq(first.completed_bytes < second.completed_bytes, true);
        context.expect_eq(second.round_selected_data, 48);
        context.expect_eq(second.later_dirty, true);
        context.expect_eq(producer.result(), None);
        context.expect_eq(
            before.data_submitted < disk.test_snapshot().data_submitted,
            true,
        );
        control.producer_stop.store(true, Ordering::Release);
        fixture.write.join_consumers()?;
        context.expect_eq(producer.result(), Some(Ok(())));
        fixture.release_rounds();
        checked(context, file.sync_range(0..u64::MAX, false))?;
        drained(&volume)?;
        context.expect_eq(inode.mapping().has_dirty_or_writeback(), false);
        context.diagnostic(format_args!(
            "W30 background first={:?} second={:?} producer-writes={} original-folio={} real-used-software-tail=1 physical-DMA-held=0 finite-entry-and-64-page-bound=1 next-round-progress=1",
            first, second, control.produced.load(Ordering::Acquire), folio.phys_addr(),
        ));
        Ok(())
    }

    /// 显式范围同步返回时原页再脏和范围外义务仍在，持续写入不能延长入口 ToWrite 集合。
    #[test]
    #[ktest(tags("W30", "fs-shared"), min_cpus = 2)]
    fn explicit_range_returns_while_future_dirty_is_produced(
        context: &mut TestContext<'_>,
        fixture: &mut BackgroundOwner,
    ) -> TestResult {
        let file = fixture.prepare_file(context, ROUND_PAGES)?;
        let inode = checked(context, file.path().inode())?.clone();
        let volume = fixture.write.volume();
        let disk = fixture.write.user.path.owner.disk()?.clone();
        let mut attempt = ReclaimAttempt::new();
        let folio = checked(context, file.file_page(0, &mut attempt))?;
        let outside = checked(context, file.file_page(64, &mut attempt))?;
        let mapping = shared_alias(context, &file, 0)?;
        let control = fixture.observe_rounds(false)?;
        context.expect_eq(file.write_at(95 * PAGE_SIZE as u64, &[0x45]), Ok(1));
        let background = control.round(1)?;
        finite_round(context, background);
        for index in 0..8 {
            context.expect_eq(file.write_at((index * PAGE_SIZE) as u64, &[0x41]), Ok(1));
        }
        context.expect_eq(file.write_at(64 * PAGE_SIZE as u64, &[0x64]), Ok(1));
        let before = volume.writeback_domain().snapshot();
        disk.test_hold_file_writeback(&folio);
        let syncing = file.clone();
        let sync = fixture.write.consume_sync(0, move || {
            syncing.sync_range(0..(8 * PAGE_SIZE) as u64, false)
        })?;
        until(
            "explicit range did not wait for its accepted original data",
            || disk.test_file_writeback_is_held(&folio) && sync.blocked(),
        )?;
        let producer = fixture.redirty(
            file.clone(),
            mapping,
            control.clone(),
            64 * PAGE_SIZE as u64,
        )?;
        redirty_observed(&control, &producer, &folio)?;
        disk.test_resume_completions();
        until(
            "explicit range chased future Dirty instead of ending its entry selection",
            || sync.result().is_some(),
        )?;
        context.expect_eq(sync.result(), Some(Ok(())));
        context.expect_eq(producer.result(), None);
        context.expect_eq(control.producer_stop.load(Ordering::Acquire), false);
        context.expect_eq(
            (folio.dirty_sectors() != 0, folio.writeback_sectors()),
            (true, 0),
        );
        context.expect_eq(
            (outside.dirty_sectors() != 0, outside.writeback_sectors()),
            (true, 0),
        );
        context.expect_eq(dirty_inode_present(&volume, &inode), true);
        let after = volume.writeback_domain().snapshot();
        let accepted = after.accepted_bytes - before.accepted_bytes;
        // 文件不增长且已有全部块；本轮只有八个数据页和同一 inode 记录所在的一个元数据页。
        context.expect_eq((8 * PAGE_SIZE) as u64 <= accepted, true);
        context.expect_eq(accepted <= (9 * PAGE_SIZE) as u64, true);
        context.expect_eq(after.rounds, background.rounds);
        let produced = control.produced.load(Ordering::Acquire);
        until(
            "range producer no longer advances after sync returned",
            || produced < control.produced.load(Ordering::Acquire),
        )?;
        control.producer_stop.store(true, Ordering::Release);
        fixture.write.join_consumers()?;
        context.expect_eq(producer.result(), Some(Ok(())));
        fixture.release_rounds();
        checked(context, file.sync_range(0..u64::MAX, false))?;
        drained(&volume)?;
        context.diagnostic(format_args!(
            "W30 explicit-range-pages=8 accepted-bytes={} later-original-Dirty=1 outside-page=64 outside-Dirty=1 producer-active-at-return=1 background-round={}",
            accepted, background.rounds,
        ));
        Ok(())
    }

    /// 在真实域压力下观察普通写阻塞、锁外并行 I/O、真实完成带宽和独立 OFD 错误游标。
    #[test]
    #[case("completed_write", false)]
    #[case("accepted_eio_pwrite", true)]
    #[ktest(tags("W31", "controlled-ledger"), min_cpus = 2)]
    fn slow_accepted_wave_balances_real_writer(
        context: &mut TestContext<'_>,
        fixture: &mut BackgroundOwner,
        fail: bool,
    ) -> TestResult {
        let file = fixture.prepare_file(context, PRESSURE_PAGES)?;
        let independent = fixture.write.open(context, b"/test/write-reserve", 2)?;
        let duplicate = file.clone();
        let inode = checked(context, file.path().inode())?.clone();
        let volume = fixture.write.volume();
        let disk = fixture.write.user.path.owner.disk()?.clone();
        if !disk.limits().flush || global_snapshot().dirty_limit != 4096 {
            return Err(TestError(
                "pressure case requires real FLUSH and the unchanged 4096-page ceiling",
            ));
        }
        let mut attempt = ReclaimAttempt::new();
        let folio = checked(context, file.file_page(0, &mut attempt))?;
        let control = fixture.observe_rounds(false)?;
        let creations_before = creation_snapshot()?;
        let metadata_before = metadata_stats();
        let before = volume.writeback_domain().snapshot();
        let global_before = global_snapshot();
        let disk_before = disk.test_snapshot();
        disk.test_hold_file_writeback(&folio);
        if fail {
            disk.test_fail_next_data(BlockError::Io);
        }
        {
            let _writeback = inode.mapping().lock_writeback();
            context.expect_eq(file.write_at(0, &[0x42; PAGE_SIZE]), Ok(PAGE_SIZE));
        }
        until(
            "pressure anchor never reached a real used completion",
            || {
                disk.test_file_writeback_is_held(&folio)
                    && folio.writeback_sectors() != 0
                    && before.rounds < volume.writeback_domain().snapshot().rounds
            },
        )?;
        let held_at = time::now();
        let held = volume.writeback_domain().snapshot();
        context.expect_eq(held.round_selected_data, 1);
        context.expect_eq(
            disk_before.data_submitted < disk.test_snapshot().data_submitted,
            true,
        );
        let writing = file.clone();
        let writer_control = control.clone();
        let writer_domain = volume.writeback_domain().clone();
        let producer_deadline = time::now()
            .checked_add(time::Duration::from_secs(120))
            .ok_or(TestError("pressure producer deadline overflow"))?;
        let writer = fixture.write.consume_sync(0, move || {
            let input = [PRESSURE_BYTE; PAGE_SIZE];
            let mut page = 1;
            while !writer_control.producer_stop.load(Ordering::Acquire) {
                if producer_deadline <= time::now() {
                    return Err(FsError::InvalidOperation);
                }
                let offset = (page * PAGE_SIZE) as u64;
                let accepted = if fail {
                    writing.write_at(offset, &input)?
                } else {
                    writing.seek(SeekFrom::Start(offset))?;
                    writing.write(&input)?
                };
                if accepted != input.len() {
                    return Err(FsError::InvalidOperation);
                }
                writer_control
                    .accepted_bytes
                    .fetch_add(accepted, Ordering::Release);
                writer_control.produced.fetch_add(1, Ordering::Release);
                writer_control.returned_sequence.store(
                    writer_domain.snapshot().completion_seq as usize,
                    Ordering::Release,
                );
                page = (page + 1).min(PRESSURE_PAGES - 1);
                sched::cond_resched();
            }
            Ok(())
        })?;
        let pressured = throttled_writer(volume.writeback_domain(), &writer, held.writer_waits)?;
        let global_pressured = global_snapshot();
        let pressure_limits = volume.writeback_domain().ktest_limits();
        context.expect_eq(pressure_limits.1 <= pressured.dirty_pages, true);
        context.expect_eq(
            pressure_limits.0 < pressured.dirty_pages + pressured.writeback_pages,
            true,
        );
        context.expect_eq(
            0 < pressure_limits.1 && pressure_limits.1 <= pressure_limits.0,
            true,
        );
        context.expect_eq(pressure_limits.0 <= global_pressured.dirty_limit, true);
        context.expect_eq(1 <= pressured.writeback_pages, true);
        context.expect_eq(global_pressured.dirty_limit, 4096);
        context.expect_eq(global_pressured.background_limit, 2048);
        context.expect_eq(pressured.completed_bytes, held.completed_bytes);
        context.expect_eq(pressured.error_seq, held.error_seq);
        context.expect_eq(pressured.sample_end_ns, held.sample_end_ns);
        context.expect_eq(disk.test_file_writeback_is_held(&folio), true);
        let observed_cursor = Arc::try_new(AtomicUsize::new(usize::MAX))
            .map_err(|_| TestError("pressure cursor observation OOM"))?;
        let cursor = observed_cursor.clone();
        let probing = file.clone();
        let probe_inode = inode.clone();
        let probe_volume = volume.clone();
        let io_before = disk.test_snapshot();
        let independent_io = fixture.write.consume_sync(1, move || {
            cursor.store(
                probing.seek(SeekFrom::Current(0))? as usize,
                Ordering::Release,
            );
            // 仅用原写能力探测 inode/invalidate 独占已释放；压力主体始终经过 FileLike 节流。
            let access = probe_inode.acquire_write()?;
            let mut input = WriteBuffer::kernel(&[PRESSURE_BYTE]);
            if access.write_from(probe_volume.claim(), PAGE_SIZE as u64, false, &mut input)?
                != (PAGE_SIZE as u64, 1)
            {
                return Err(FsError::InvalidOperation);
            }
            probe_volume
                .claim()
                .flush(Vec::new_in(BioAllocator::Heap))
                .map_err(FsError::from)
        })?;
        until(
            "throttled writer retained cursor or inode locks or blocked unrelated real I/O",
            || independent_io.result().is_some(),
        )?;
        context.expect_eq(independent_io.result(), Some(Ok(())));
        context.expect_eq(observed_cursor.load(Ordering::Acquire) != usize::MAX, true);
        if fail {
            context.expect_eq(observed_cursor.load(Ordering::Acquire), 0);
        } else {
            let cursor = observed_cursor.load(Ordering::Acquire);
            context.expect_eq(
                2 * PAGE_SIZE <= cursor && cursor <= PRESSURE_PAGES * PAGE_SIZE,
                true,
            );
            context.expect_eq(cursor % PAGE_SIZE, 0);
        }
        let io_after = disk.test_snapshot();
        context.expect_eq(io_after.flush_submitted, io_before.flush_submitted + 1);
        context.expect_eq(io_before.completed < io_after.completed, true);
        context.expect_eq(disk.test_file_writeback_is_held(&folio), true);
        context.expect_eq(folio.writeback_sectors() != 0, true);
        let waits_before_release =
            throttled_writer(volume.writeback_domain(), &writer, held.writer_waits)?;
        let release_at = time::now();
        disk.test_resume_completions();
        control.producer_stop.store(true, Ordering::Release);
        let completed = control.round(1)?;
        until(
            "real completion did not wake the already throttled writer",
            || {
                held.completion_seq < control.returned_sequence.load(Ordering::Acquire) as u64
                    && waits_before_release.writer_wakes
                        < volume.writeback_domain().snapshot().writer_wakes
            },
        )?;
        let returned = volume.writeback_domain().snapshot();
        let global_after = global_snapshot();
        context.expect_eq(independent_io.result(), Some(Ok(())));
        finite_round(context, completed);
        context.expect_eq(completed.round_selected_data, 1);
        context.expect_eq(completed.later_dirty, true);
        context.expect_eq(held.completion_seq < returned.completion_seq, true);
        context.expect_eq(held.accepted_bytes < returned.accepted_bytes, true);
        context.expect_eq(
            waits_before_release.writer_wakes < returned.writer_wakes,
            true,
        );
        context.expect_eq(0 < returned.bandwidth_bytes_per_sec, true);
        context.expect_eq(0 < global_after.bandwidth_bytes_per_sec, true);
        context.expect_eq(held_at < release_at, true);
        context.expect_eq(
            held.completion_seq < control.returned_sequence.load(Ordering::Acquire) as u64,
            true,
        );
        context.expect_eq((folio.dirty_sectors(), folio.writeback_sectors()), (0, 0));
        if fail {
            context.expect_eq(held.error_seq < returned.error_seq, true);
            context.expect_eq(disk.test_completion_error_pending(), false);
            context.expect_eq(
                PAGE_SIZE as u64
                    <= completed.round_accepted_bytes - completed.round_completed_bytes,
                true,
            );
            let data_after_error = disk.test_snapshot().data_submitted;
            let error = Err(FsError::Io(BlockError::Io));
            context.expect_eq(file.sync_range(0..PAGE_SIZE as u64, false), error);
            context.expect_eq(independent.sync_range(0..PAGE_SIZE as u64, false), error);
            context.expect_eq(duplicate.sync_range(0..PAGE_SIZE as u64, false), Ok(()));
            context.expect_eq(file.sync_range(0..PAGE_SIZE as u64, false), Ok(()));
            context.expect_eq(disk.test_snapshot().data_submitted, data_after_error);
        } else {
            context.expect_eq(returned.error_seq, held.error_seq);
            context.expect_eq(
                completed.round_completed_bytes,
                completed.round_accepted_bytes,
            );
            context.expect_eq(
                held.completed_bytes + PAGE_SIZE as u64 <= returned.completed_bytes,
                true,
            );
            context.expect_eq(
                global_before.completed_bytes < global_after.completed_bytes,
                true,
            );
        }
        fixture.release_rounds();
        fixture.write.join_consumers()?;
        context.expect_eq(writer.result(), Some(Ok(())));
        context.expect_eq(independent_io.result(), Some(Ok(())));
        let accepted = control.accepted_bytes.load(Ordering::Acquire);
        context.expect_eq((PRESSURE_PAGES - 1) * PAGE_SIZE <= accepted, true);
        context.expect_eq(
            accepted,
            control.produced.load(Ordering::Acquire) * PAGE_SIZE,
        );
        bytes_at(context, &file, PAGE_SIZE as u64, &[PRESSURE_BYTE; 32])?;
        bytes_at(
            context,
            &file,
            ((PRESSURE_PAGES - 1) * PAGE_SIZE) as u64,
            &[PRESSURE_BYTE; 32],
        )?;
        checked(context, file.sync_range(0..u64::MAX, false))?;
        drained(&volume)?;
        let final_state = volume.writeback_domain().snapshot();
        context.expect_eq(inode.mapping().has_dirty_or_writeback(), false);
        context.expect_eq(final_state.waiting_writers, 0);
        context.expect_eq(returned.completed_bytes < final_state.completed_bytes, true);
        if final_state.sample_bytes == 0 || final_state.sample_elapsed_ns == 0 {
            return Err(TestError(
                "completed pressure wave has no measured bandwidth sample",
            ));
        }
        let measured_rate = (u128::from(final_state.sample_bytes) * 1_000_000_000
            / u128::from(final_state.sample_elapsed_ns)) as u64;
        context.expect_eq(final_state.bandwidth_bytes_per_sec, measured_rate);
        context.expect_eq(held.sample_end_ns < final_state.sample_end_ns, true);
        context.expect_eq(release_at.as_nanos() <= final_state.sample_end_ns, true);
        context.expect_eq(final_state.sample_end_ns <= time::now().as_nanos(), true);
        context.expect_eq(
            final_state.sample_elapsed_ns <= final_state.sample_end_ns,
            true,
        );
        context.expect_eq(
            final_state.sample_bytes <= final_state.completed_bytes,
            true,
        );
        let creations_after = creation_snapshot()?;
        let metadata_after = metadata_stats();
        context.expect_eq(
            creations_after.0 - creations_before.0,
            creations_after.1 - creations_before.1 + 2,
        );
        context.expect_eq(metadata_after.domains, metadata_before.domains);
        context.diagnostic(format_args!(
            "W31 accepted-error={} global-before={:?} global-pressure={:?} domain-limit={} domain-background={} hold-ns={} writer-accepted={}",
            fail, global_before, global_pressured, pressure_limits.0, pressure_limits.1,
            release_at.as_nanos() - held_at.as_nanos(), accepted,
        ));
        context.diagnostic(format_args!(
            "W31 held={:?} pressured={:?}",
            held, pressured
        ));
        context.diagnostic(format_args!(
            "W31 completed={:?} returned={:?}",
            completed, returned
        ));
        context.diagnostic(format_args!(
            "W31 final={:?} first-wave-sample-fresh={} final-sample-fresh={} measured-rate={}",
            final_state,
            held.sample_end_ns < completed.sample_end_ns,
            held.sample_end_ns < final_state.sample_end_ns,
            measured_rate,
        ));
        context.diagnostic(format_args!(
            "W31 allocated-task-ids={} shared-worker-creations={} owned-consumers=2 unattributed-task-ids={} metadata-before={:?} metadata-after={:?} real-used-software-tail=1 physical-DMA-held=0 hardware-fault=0",
            creations_after.0 - creations_before.0, creations_after.1 - creations_before.1,
            (creations_after.0 - creations_before.0) as i128
                - (creations_after.1 - creations_before.1) as i128 - 2,
            metadata_before, metadata_after,
        ));
        Ok(())
    }
}
