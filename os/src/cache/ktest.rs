//! 原 folio、真实块设备读入与有界只读缓存的具名消费回归。

use ::ktest::{self, Fixture, TestContext, TestError, TestResult};
use alloc::alloc::{alloc, dealloc};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::alloc::Layout;
use core::hint::black_box;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::block::{self, BlockError, Disk, DiskRef};
use crate::cache::{BufferRef, FileMapping, MetadataCache};
use crate::drivers::virtio::{VirtioDevice, VirtioIdentity};
use crate::mm::folio::{FolioRead, FolioRef};
use crate::sync::{Completion, SpinLock};
use crate::task::{self, JoinHandle, PreparedTask, State, Task};
use crate::workqueue::{WorkQueue, WorkQueueConfig};
use crate::{cpu, drivers, mm, sched, time, timer};

const OFFSET: u64 = 64 * mm::PAGE_SIZE as u64;
const PRESSURE_CAPACITY: usize = 65536;

/// 父入口只显式引用宏产生的实际注册槽，不维护第二份用例表。
pub(crate) fn anchors() -> [&'static ::ktest::SuiteSlot; 10] {
    [
        folio::__ktest_anchor(),
        metadata_views::__ktest_anchor(),
        metadata_singleflight::__ktest_anchor(),
        file_singleflight::__ktest_anchor(),
        metadata_parallel::__ktest_anchor(),
        file_parallel::__ktest_anchor(),
        stopped_failure::__ktest_anchor(),
        bounded::__ktest_anchor(),
        oom::__ktest_anchor(),
        prepare_oom::__ktest_anchor(),
    ]
}

/// 有界轮询只观察真实状态，等待正确性由实际 Blocked 与生产唤醒证明。
fn until(label: &'static str, mut ready: impl FnMut() -> bool) -> TestResult {
    let deadline = time::now()
        .checked_add(time::Duration::from_secs(5))
        .ok_or(TestError("cache deadline overflow"))?;
    while !ready() {
        if deadline <= time::now() {
            return Err(TestError(label));
        }
        timer::sleep(time::Duration::from_millis(1));
    }
    Ok(())
}

/// 参与者离栈后整理真实 SLUB/PCP，六域包括当前框架与服务的常驻页。
fn settled() -> Result<mm::MemoryStats, TestError> {
    until("cache resource CPUs did not settle", || {
        (0..cpu::count())
            .all(|id| id == cpu::current().id || cpu::get(id).idle.load(Ordering::Acquire))
    })?;
    mm::flush_all();
    mm::shrink_all();
    mm::drain_all_pages();
    Ok(mm::stats())
}

/// 完整消费拥有者后核六域，不重置计数或回滚物理内存状态。
fn recovered(context: &mut TestContext<'_>, baseline: mm::MemoryStats) -> TestResult {
    let mut after = settled()?;
    until("cache six-domain resources did not return", || {
        mm::shrink_all();
        mm::drain_all_pages();
        after = mm::stats();
        after == baseline
    })?;
    context.expect_eq(after, baseline);
    context.diagnostic(format_args!(
        "cache-resources six-domains-before={:?} six-domains-after={:?} real-stop-join-cleanup=1",
        baseline, after
    ));
    Ok(())
}

/// 逐字节验证宿主原 raw 输入盘的非零图样，零内容或短范围不能充当成功。
fn patterned(bytes: &[u8], tag: u8, offset: u64, len: usize) -> bool {
    bytes.len() == len
        && bytes.iter().any(|byte| *byte != 0)
        && bytes.iter().enumerate().all(|(index, byte)| {
            let at = offset + index as u64;
            *byte
                == tag
                    .wrapping_mul(29)
                    .wrapping_add(((at / 512) as u8).wrapping_mul(17))
                    .wrapping_add((at % 512) as u8)
        })
}

/// 只从本例声明的 root/extra 工作副本库存准备设备，不写原输入盘。
fn devices(count: usize) -> Result<Vec<VirtioDevice>, TestError> {
    let inventory = drivers::discover().map_err(|_| TestError("cache discovery failed"))?;
    let pci =
        inventory.pci.devices().iter().filter(|device| {
            device.vendor_id == 0x1af4 && matches!(device.device_id, 0x1001 | 0x1042)
        });
    if inventory.mmio_blocks.len() + pci.clone().count() != count {
        return Err(TestError(
            "cache attached device count differs from declared resources",
        ));
    }
    let mut prepared = Vec::new();
    prepared
        .try_reserve_exact(count)
        .map_err(|_| TestError("cache device owner allocation failed"))?;
    for candidate in &inventory.mmio_blocks {
        // SAFETY: 本例独占宿主声明的工作副本；候选来自真实库存，永久映射保活至真实 stop。
        prepared.push(
            unsafe { VirtioDevice::prepare_mmio(candidate) }
                .map_err(|_| TestError("cache MMIO preparation failed"))?,
        );
    }
    for device in pci {
        // SAFETY: BDF 属于同一真实库存，fixture 唯一拥有至 reset、IRQ 与 Work 同步结束。
        prepared.push(
            unsafe { VirtioDevice::prepare_pci(&inventory.pci, device) }
                .map_err(|_| TestError("cache PCI preparation failed"))?,
        );
    }
    Ok(prepared)
}

struct Pages {
    baseline: mm::MemoryStats,
    folios: [Option<FolioRef>; 2],
    higher: Option<mm::OwnedPages>,
}

impl Fixture for Pages {
    /// 在捕获外保留两种原页的完整拥有，正文失败也由具体 fixture 归还。
    fn set_up(_: &mut TestContext<'_>, _: &()) -> Result<Self, TestError> {
        let baseline = settled()?;
        let pages = mm::alloc_pages(0, false).ok_or(TestError("folio order0 preparation OOM"))?;
        let folio = FolioRef::from_pages(pages).map_err(|_| TestError("folio rejected order0"))?;
        let higher = mm::alloc_pages(1, false).ok_or(TestError("folio order1 preparation OOM"))?;
        Ok(Self {
            baseline,
            folios: [Some(folio), None],
            higher: Some(higher),
        })
    }

    /// 只有同步原页责任；析构全部拥有后核六域原基线。
    fn tear_down(self, context: &mut TestContext<'_>) -> TestResult {
        let baseline = self.baseline;
        drop(self);
        recovered(context, baseline)
    }
}

impl Pages {
    /// Clone 保同 PFN，非末引用不归页，末引用沿原分配器归还；高阶 Err 保原块与内容。
    fn identity(&mut self, context: &mut TestContext<'_>) -> TestResult {
        let before = settled()?;
        let first = self.folios[0].as_ref().unwrap();
        let address = first.phys_addr();
        self.folios[1] = Some(first.clone());
        context.expect_eq(self.folios[1].as_ref().unwrap().phys_addr(), address);
        context.expect_eq(self.folios[0].as_ref().unwrap().ref_count(), 2);
        drop(self.folios[0].take());
        context.expect_eq(self.folios[1].as_ref().unwrap().ref_count(), 1);
        context.expect_eq(settled()?, before);
        drop(self.folios[1].take());
        let mut expected = before;
        expected.buddy_free_pages += 1;
        context.expect_eq(settled()?, expected);

        let mut higher = self.higher.take().unwrap();
        let address = higher.phys_addr();
        let len = higher.len();
        // SAFETY: higher 唯一拥有整块且尚无设备/folio 别名，全部字节写定后才读取。
        unsafe { higher.as_mut_ptr().cast::<u8>().write_bytes(0xa7, len) };
        match FolioRef::from_pages(higher) {
            Err(mut original) => {
                context.expect_eq(original.phys_addr(), address);
                context.expect_eq(original.order(), 1);
                context.expect_eq(original.len(), len);
                // SAFETY: 上方整块已初始化，返回 Err 仍持原块唯一拥有，短借用结束后才移动。
                let unchanged =
                    unsafe { core::slice::from_raw_parts(original.as_mut_ptr().cast::<u8>(), len) }
                        .iter()
                        .all(|byte| *byte == 0xa7);
                context.expect_eq(unchanged, true);
                self.higher = Some(original);
            }
            Ok(folio) => {
                self.folios[0] = Some(folio);
                return Err(TestError("folio accepted higher-order pages"));
            }
        }
        drop(self.higher.take());
        expected.buddy_free_pages += 2;
        context.expect_eq(settled()?, expected);
        Ok(())
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Scenario {
    Views,
    MetadataFlight,
    FileFlight,
    MetadataIndependent,
    Independent,
    Failure,
    Bounded,
    PrepareOom,
    Oom,
}

enum Returned {
    Buffer(BufferRef),
    Folio(FolioRef),
}

impl Returned {
    /// 只借本次真实页引用身份，观察不复制内容。
    fn folio(&self) -> &FolioRef {
        match self {
            Self::Buffer(buffer) => buffer.folio(),
            Self::Folio(folio) => folio,
        }
    }

    /// 真实成功初始化范围内核验整个页图样，不越过 buffer 的短视图。
    fn valid(&self, offset: u64) -> Result<bool, BlockError> {
        match self {
            Self::Buffer(buffer) => {
                buffer.with_bytes(|bytes| patterned(bytes, 1, offset, mm::PAGE_SIZE))
            }
            Self::Folio(folio) => folio.with_bytes(0, mm::PAGE_SIZE, |bytes| {
                patterned(bytes, 1, offset, mm::PAGE_SIZE)
            }),
        }
    }
}

struct HeapPressure {
    pages: Vec<mm::OwnedPages>,
    // 每个独占原堆对象的首字保存本类别下一节点，压力期间不申请链表节点。
    heap: [usize; 12],
}

impl HeapPressure {
    /// 控制 Task 的真实页凭证容量先于设备 start 预留，堆类别链头常驻本 owner。
    fn prepare() -> Result<Self, TestError> {
        let mut pages = Vec::new();
        pages
            .try_reserve_exact(mm::stats().present_pages)
            .map_err(|_| TestError("cache prepare-OOM pressure slots unavailable"))?;
        Ok(Self {
            pages,
            heap: [0; 12],
        })
    }

    /// 固定 CPU 控制者耗尽真实页及全部 SLUB 类，再用原 allocator 观察真实 NULL。
    fn exhaust(&mut self) -> (bool, bool) {
        mm::shrink_all();
        mm::drain_all_pages();
        for order in [10, 0] {
            while let Some(pages) = mm::alloc_pages(order, false) {
                assert!(
                    self.pages.len() < self.pages.capacity(),
                    "cache pressure capacity"
                );
                self.pages.push(pages);
            }
        }
        for (index, size) in mm::SLUB_SIZES.iter().copied().enumerate() {
            let layout = Layout::from_size_align(size, 8).expect("cache heap pressure layout");
            loop {
                // SAFETY: 固定有效非零布局，真实全局分配器返回的成功对象由本 owner 独占。
                let pointer = black_box(unsafe { alloc(layout) });
                if pointer.is_null() {
                    break;
                }
                // SAFETY: 所有类别至少八字节且八对齐，首字仅保存本对象的独占链链接。
                unsafe { pointer.cast::<usize>().write(self.heap[index]) };
                self.heap[index] = pointer as usize;
            }
        }
        mm::drain_all_pages();
        let pages_failed = black_box(mm::alloc_pages(0, false)).is_none();
        let mut heap_failed = true;
        for size in mm::SLUB_SIZES
            .iter()
            .copied()
            .chain(core::iter::once(mm::PAGE_SIZE * 2))
        {
            let layout = Layout::from_size_align(size, 8).expect("cache heap probe layout");
            // SAFETY: 有效原布局探测同一真实 allocator，black_box 保留实际 NULL 观察。
            let pointer = black_box(unsafe { alloc(layout) });
            if !pointer.is_null() {
                heap_failed = false;
                // SAFETY: 意外成功的原对象没有外部引用，立即按同一布局归还一次。
                unsafe { dealloc(pointer, layout) };
            }
        }
        (pages_failed, heap_failed)
    }
}

impl Drop for HeapPressure {
    /// 正文返回或展开都按原布局释放每个节点，最后锁外归还全部物理页。
    fn drop(&mut self) {
        for (index, size) in mm::SLUB_SIZES.iter().copied().enumerate() {
            let layout = Layout::from_size_align(size, 8).expect("cache heap release layout");
            while self.heap[index] != 0 {
                let pointer = self.heap[index] as *mut u8;
                // SAFETY: 节点是本链尚未归还的原 alloc，首字已初始化，布局与分配相同。
                unsafe {
                    self.heap[index] = pointer.cast::<usize>().read();
                    dealloc(pointer, layout);
                }
            }
        }
    }
}

/// 预建固定 CPU0 Task 独占压力与唯一 read，正式准备返回后才放压；不制造完成结果。
fn prepare_oom_read(observation: &Observation, disk: &Disk) -> Result<(), BlockError> {
    let mut pressure = observation
        .pressure
        .lock()
        .take()
        .expect("cache prepared heap pressure missing");
    let evidence = pressure.exhaust();
    *observation.oom_evidence.lock() = Some(evidence);
    let read = observation
        .read
        .lock()
        .take()
        .expect("cache prepare-OOM original read missing");
    let result = if evidence == (true, true) {
        disk.load_folio(OFFSET, read)
    } else {
        // 探测未证明耗尽时取消未发表 owner；正文将拒绝该证据，不冒称准备 OOM。
        drop(read);
        Err(BlockError::QueueStopped)
    };
    drop(pressure);
    result
}

struct Observation {
    cancelled: AtomicBool,
    gate: Completion,
    entered: [AtomicBool; 3],
    tasks: SpinLock<[Option<Arc<Task>>; 3]>,
    results: SpinLock<[Option<Result<Returned, BlockError>>; 3]>,
    read: SpinLock<Option<FolioRead>>,
    pressure: SpinLock<Option<HeapPressure>>,
    oom_evidence: SpinLock<Option<(bool, bool)>>,
}

impl Observation {
    /// 所有 gate、结果槽与唯一未提交 read owner 在任何设备发表之前准备。
    fn prepare(
        read: Option<FolioRead>,
        pressure: Option<HeapPressure>,
    ) -> Result<Arc<Self>, TestError> {
        Arc::try_new(Self {
            cancelled: AtomicBool::new(false),
            gate: Completion::new(),
            entered: [const { AtomicBool::new(false) }; 3],
            tasks: SpinLock::new([const { None }; 3]),
            results: SpinLock::new([const { None }; 3]),
            read: SpinLock::new(read),
            pressure: SpinLock::new(pressure),
            oom_evidence: SpinLock::new(None),
        })
        .map_err(|_| TestError("cache observer preparation OOM"))
    }

    /// 登记真实调用后的 Task 才统计 Blocked，gate 自身等待不计入缓存等待证据。
    fn blocked(&self, index: usize) -> bool {
        if !self.entered[index].load(Ordering::Acquire) {
            return false;
        }
        let task = self.tasks.lock()[index].clone();
        task.is_some_and(|task| sched::test_state(&task) == State::Blocked)
    }
}

struct Owner {
    baseline: mm::MemoryStats,
    disk: DiskRef,
    extra: Option<DiskRef>,
    queue: WorkQueue,
    metadata: Arc<MetadataCache>,
    extra_metadata: Option<MetadataCache>,
    mapping: Arc<FileMapping>,
    observation: Arc<Observation>,
    held: [Option<Returned>; 3],
    prepared: Vec<PreparedTask>,
    joins: Vec<JoinHandle>,
    pressure: Vec<mm::OwnedPages>,
    legacy: bool,
    activation_error: Option<BlockError>,
    avail_address: usize,
}

impl Owner {
    /// 完整 fallible 控制存储与原生 Task 先于 start，失败只释放未发表对象。
    fn prepare(scenario: Scenario) -> Result<Self, TestError> {
        let baseline = settled()?;
        let mut inventory = devices(if scenario == Scenario::Views { 2 } else { 1 })?;
        let device = inventory.remove(0);
        let info = device.info();
        let unit = info.logical_block_size as usize;
        if !matches!(unit, 512 | 1024 | 2048 | 4096)
            || (scenario == Scenario::Views
                && (1024 < unit
                    || inventory
                        .iter()
                        .any(|device| 1024 < device.info().logical_block_size)))
        {
            return Err(TestError(
                "cache views require logical block <= 1KiB; page cases require <= 4KiB",
            ));
        }
        let legacy = matches!(device.identity(), VirtioIdentity::Mmio { version: 1, .. });
        let ring = device.control_regions().0;
        let layout = virtio_drivers::queue::RingLayout::new(info.queue_depth, legacy)
            .map_err(|_| TestError("cache real ring layout invalid"))?;
        let avail_address = ring.cpu.as_ptr() as usize + layout.avail_offset() + 2;
        let queue = WorkQueue::try_new(WorkQueueConfig {
            max_active: usize::from(info.tag_count) + 1,
            ordered: false,
            reclaim: true,
        })
        .map_err(|_| TestError("cache completion domain preparation OOM"))?;
        let disk = match Disk::prepare(device, &queue) {
            Ok(disk) => disk,
            Err(_) => {
                queue.close_sync();
                return Err(TestError("cache Disk preparation failed"));
            }
        };
        let mut owner = Self::prepare_resources(
            scenario,
            baseline,
            disk.clone(),
            queue,
            legacy,
            inventory,
            avail_address,
        )?;
        owner.activation_error = disk.start().err();
        if let Some(extra) = &owner.extra {
            if let Err(error) = extra.start() {
                owner.activation_error.get_or_insert(error);
            }
        }
        Ok(owner)
    }

    /// 构造缓存、观察者与任务；尚未 start 的设备不具有已发表 DMA 请求。
    fn prepare_resources(
        scenario: Scenario,
        baseline: mm::MemoryStats,
        disk: DiskRef,
        queue: WorkQueue,
        legacy: bool,
        mut inventory: Vec<VirtioDevice>,
        avail_address: usize,
    ) -> Result<Self, TestError> {
        let extra = match inventory
            .pop()
            .map(|device| Disk::prepare(device, &queue))
            .transpose()
        {
            Ok(extra) => extra,
            Err(_) => {
                disk.stop()
                    .map_err(|_| TestError("unpublished cache Disk stop failed"))?;
                drop(disk);
                queue.close_sync();
                return Err(TestError("cache extra Disk preparation failed"));
            }
        };
        let resources = (|| {
            let capacity = if scenario == Scenario::Bounded { 2 } else { 8 };
            let metadata = Arc::try_new(
                MetadataCache::try_new(disk.clone(), capacity)
                    .map_err(|_| TestError("metadata preparation failed"))?,
            )
            .map_err(|_| TestError("metadata owner preparation OOM"))?;
            let mapping = Arc::try_new(
                FileMapping::try_new(capacity)
                    .map_err(|_| TestError("mapping preparation failed"))?,
            )
            .map_err(|_| TestError("mapping owner preparation OOM"))?;
            let extra_metadata = extra
                .as_ref()
                .map(|disk| MetadataCache::try_new(disk.clone(), 8))
                .transpose()
                .map_err(|_| TestError("extra metadata preparation failed"))?;
            let (failure_folio, read) =
                if matches!(scenario, Scenario::Failure | Scenario::PrepareOom) {
                    let (folio, read) = mapping
                        .get_or_prepare(64)
                        .map_err(|_| TestError("failure placeholder preparation failed"))?;
                    (Some(folio), read)
                } else {
                    (None, None)
                };
            let pressure = if scenario == Scenario::PrepareOom {
                Some(HeapPressure::prepare()?)
            } else {
                None
            };
            let observation = Observation::prepare(read, pressure)?;
            let count = match scenario {
                Scenario::MetadataFlight
                | Scenario::FileFlight
                | Scenario::MetadataIndependent
                | Scenario::Independent => 2,
                Scenario::Failure | Scenario::PrepareOom => 3,
                _ => 0,
            };
            let mut prepared = Vec::new();
            let mut joins = Vec::new();
            prepared
                .try_reserve_exact(count)
                .map_err(|_| TestError("cache prepared Task slots OOM"))?;
            joins
                .try_reserve_exact(count)
                .map_err(|_| TestError("cache join slots OOM"))?;
            let mut pressure = Vec::new();
            if scenario == Scenario::Oom {
                pressure
                    .try_reserve_exact(PRESSURE_CAPACITY)
                    .map_err(|_| TestError("cache pressure handles OOM"))?;
            }
            for index in 0..count {
                let observer = observation.clone();
                let task_disk = disk.clone();
                let cache = metadata.clone();
                let file = mapping.clone();
                let failure_folio = failure_folio.clone();
                prepared.push(
                    task::prepare_kernel_on(index % cpu::count(), move || {
                        observer.tasks.lock()[index] = Some(task::current_task());
                        if matches!(scenario, Scenario::Failure | Scenario::PrepareOom)
                            && index == 0
                        {
                            observer.gate.wait();
                        }
                        if observer.cancelled.load(Ordering::Acquire) {
                            return 0;
                        }
                        observer.entered[index].store(true, Ordering::Release);
                        let key = if matches!(
                            scenario,
                            Scenario::Independent | Scenario::MetadataIndependent
                        ) {
                            64 + index as u64
                        } else {
                            64
                        };
                        let result = if matches!(
                            scenario,
                            Scenario::MetadataFlight | Scenario::MetadataIndependent
                        ) {
                            cache.get(key, mm::PAGE_SIZE).map(Returned::Buffer)
                        } else if scenario == Scenario::PrepareOom && index == 0 {
                            prepare_oom_read(&observer, &task_disk)
                                .map(|()| Returned::Folio(failure_folio.unwrap()))
                        } else if scenario == Scenario::Failure && index == 0 {
                            let read = observer
                                .read
                                .lock()
                                .take()
                                .expect("failure original read owner missing");
                            task_disk
                                .load_folio(OFFSET, read)
                                .map(|()| Returned::Folio(failure_folio.unwrap()))
                        } else {
                            match file.get_or_prepare(key) {
                                Ok((folio, Some(read))) => task_disk
                                    .load_folio(key * mm::PAGE_SIZE as u64, read)
                                    .map(|()| Returned::Folio(folio)),
                                Ok((folio, None)) => {
                                    folio.wait_ready().map(|()| Returned::Folio(folio))
                                }
                                Err(error) => Err(error),
                            }
                        };
                        observer.results.lock()[index] = Some(result);
                        0
                    })
                    .map_err(|_| TestError("cache native Task preparation failed"))?,
                );
            }
            let mut held = [const { None }; 3];
            held[0] = failure_folio.map(Returned::Folio);
            Ok((
                metadata,
                mapping,
                extra_metadata,
                observation,
                prepared,
                joins,
                pressure,
                held,
            ))
        })();
        match resources {
            Ok((
                metadata,
                mapping,
                extra_metadata,
                observation,
                prepared,
                joins,
                pressure,
                held,
            )) => Ok(Self {
                baseline,
                disk,
                extra,
                queue,
                metadata,
                extra_metadata,
                mapping,
                observation,
                prepared,
                joins,
                pressure,
                held,
                legacy,
                activation_error: None,
                avail_address,
            }),
            Err(error) => {
                if let Some(extra) = &extra {
                    extra
                        .stop()
                        .map_err(|_| TestError("unpublished extra cache Disk stop failed"))?;
                }
                drop(extra);
                disk.stop()
                    .map_err(|_| TestError("unpublished cache Disk stop failed"))?;
                drop(disk);
                queue.close_sync();
                Err(error)
            }
        }
    }

    /// 激活失败仍保完整 fixture，交具体 quiesce 处理真实 stop。
    fn ready(&self) -> TestResult {
        if self.activation_error.is_some() {
            Err(TestError("cache Disk activation failed"))
        } else {
            Ok(())
        }
    }

    /// 消费一个预建原生 Task；固定 join 容量在发表前预留。
    fn publish_one(&mut self) {
        self.joins.push(self.prepared.remove(0).publish());
    }

    /// 全部 Task 真正离栈后才消费 JoinHandle，完成 flag 不能代替 join。
    fn join_tasks(&mut self) -> TestResult {
        for join in &self.joins {
            until("cache producer did not leave stack", || {
                join.try_join().is_some()
            })?;
        }
        let mut failed = false;
        for join in self.joins.drain(..) {
            failed |= join.join() != 0;
        }
        if failed {
            Err(TestError("cache native producer failed"))
        } else {
            Ok(())
        }
    }

    /// 真实硬件 used.idx 写到目标才继续，不修改 ring 或制造完成。
    fn used(&self, target: u16) -> TestResult {
        until("cache real used did not progress", || {
            block::tests::used_index(&self.disk, self.legacy) == target
        })
    }

    /// 结果先从观察锁移到 fixture 原拥有槽，再调用可能取得内容锁的真实消费者。
    fn take_results(&mut self) -> TestResult {
        for index in 0..2 {
            let result = self.observation.results.lock()[index].take();
            self.held[index] = Some(
                result
                    .ok_or(TestError("cache producer result absent"))?
                    .map_err(|_| TestError("cache real read failed"))?,
            );
        }
        Ok(())
    }

    /// 同 key 等待者实际 Blocked，恢复原 used 后全部消费同 PFN 与完整数据。
    fn singleflight(&mut self, context: &mut TestContext<'_>) -> TestResult {
        self.ready()?;
        self.disk.test_suspend_completions();
        let before = self.disk.test_snapshot();
        let used = block::tests::used_index(&self.disk, self.legacy);
        self.publish_one();
        self.used(used.wrapping_add(1))?;
        until("cache read owner did not block", || {
            self.observation.blocked(0)
        })?;
        self.publish_one();
        until("cache same-key waiter did not block", || {
            self.observation.blocked(1)
        })?;
        context.expect_eq(self.disk.test_snapshot().submitted - before.submitted, 1);
        context.expect_eq(self.disk.test_snapshot().inflight, 1);
        self.disk.test_resume_completions();
        self.join_tasks()?;
        self.disk.test_flush_completions();
        self.take_results()?;
        let mut address = None;
        for returned in self.held[..2].iter().flatten() {
            if let Some(address) = address {
                context.expect_eq(returned.folio().phys_addr(), address);
            }
            address = Some(returned.folio().phys_addr());
            context.expect_eq(returned.valid(OFFSET), Ok(true));
        }
        context.expect_eq(self.disk.test_snapshot().submitted - before.submitted, 1);
        Ok(())
    }

    /// 不同文件页在真实 used 暂停窗口同时拥有两个设备请求。
    fn independent(&mut self, context: &mut TestContext<'_>) -> TestResult {
        self.ready()?;
        self.disk.test_suspend_completions();
        let before = self.disk.test_snapshot();
        let used = block::tests::used_index(&self.disk, self.legacy);
        self.publish_one();
        self.publish_one();
        self.used(used.wrapping_add(2))?;
        until("independent file reads did not block", || {
            self.observation.blocked(0) && self.observation.blocked(1)
        })?;
        context.expect_eq(self.disk.test_snapshot().submitted - before.submitted, 2);
        context.expect_eq(self.disk.test_snapshot().inflight, 2);
        self.disk.test_resume_completions();
        self.join_tasks()?;
        self.disk.test_flush_completions();
        self.take_results()?;
        let mut first = None;
        for (index, returned) in self.held[..2].iter().flatten().enumerate() {
            if let Some(address) = first {
                context.expect_eq(returned.folio().phys_addr() != address, true);
            }
            first = Some(returned.folio().phys_addr());
            context.expect_eq(
                returned.valid(OFFSET + index as u64 * mm::PAGE_SIZE as u64),
                Ok(true),
            );
        }
        Ok(())
    }

    /// QueueStopped 只证明真实停止后的提交拒绝与 folio 失败唤醒，不外推硬件 EIO。
    fn failure(&mut self, context: &mut TestContext<'_>) -> TestResult {
        self.ready()?;
        self.publish_one();
        self.publish_one();
        self.publish_one();
        until("failed file waiters did not block", || {
            self.observation.blocked(1) && self.observation.blocked(2)
        })?;
        self.disk
            .stop()
            .map_err(|_| TestError("cache failure real stop failed"))?;
        let before = self.disk.test_snapshot().submitted;
        self.observation.gate.complete();
        self.join_tasks()?;
        context.expect_eq(self.disk.test_snapshot().submitted, before);
        for result in &*self.observation.results.lock() {
            context.expect_eq(matches!(result, Some(Err(BlockError::QueueStopped))), true);
        }
        let folio = self.held[0].as_ref().unwrap().folio();
        context.expect_eq(folio.is_ready(), true);
        context.expect_eq(folio.wait_ready(), Err(BlockError::QueueStopped));
        context.expect_eq(
            folio.with_bytes(0, mm::PAGE_SIZE, |_| ()),
            Err(BlockError::QueueStopped),
        );
        let (hit, owner) = self
            .mapping
            .get_or_prepare(64)
            .map_err(|_| TestError("failure hit disappeared"))?;
        context.expect_eq(hit.phys_addr(), folio.phys_addr());
        context.expect_eq(owner.is_none(), true);
        context.expect_eq(hit.wait_ready(), Err(BlockError::QueueStopped));
        context.diagnostic(format_args!(
            "cache-failure real-QueueStopped=1 hardware-EIO-covered=0"
        ));
        Ok(())
    }

    /// root 与 extra 各自验证 1/2/4KiB 短视图，身份不跨 Disk 合并。
    fn views(&mut self, context: &mut TestContext<'_>) -> TestResult {
        self.ready()?;
        let extra = self
            .extra_metadata
            .as_ref()
            .ok_or(TestError("extra metadata absent"))?;
        for size in [1024, 2048, 4096] {
            let offset = 33 * size as u64;
            let root = self
                .metadata
                .get(33, size)
                .map_err(|_| TestError("root metadata view read failed"))?;
            let second = extra
                .get(33, size)
                .map_err(|_| TestError("extra metadata view read failed"))?;
            context.expect_eq(root.len(), size);
            context.expect_eq(root.offset(), offset as usize % mm::PAGE_SIZE);
            context.expect_eq(
                root.with_bytes(|bytes| patterned(bytes, 1, offset, size)),
                Ok(true),
            );
            context.expect_eq(
                second.with_bytes(|bytes| patterned(bytes, 2, offset, size)),
                Ok(true),
            );
            context.expect_eq(root.folio().phys_addr() != second.folio().phys_addr(), true);
            context.expect_eq(
                root.folio()
                    .with_bytes(root.offset(), size + 1, |_| ())
                    .is_err(),
                true,
            );
        }
        Ok(())
    }

    /// FileMapping 的 Some 唯一 owner 由实际 Disk 填充，再沿原 folio 等待成功。
    fn file_page(&self, index: u64) -> Result<FolioRef, BlockError> {
        let (folio, read) = self.mapping.get_or_prepare(index)?;
        if let Some(read) = read {
            self.disk.load_folio(index * mm::PAGE_SIZE as u64, read)?;
        }
        folio.wait_ready()?;
        Ok(folio)
    }

    /// 两种有界索引全活时拒 miss，clean victim 自动回收与显式 prune 均保活引用。
    fn bounded(&mut self, context: &mut TestContext<'_>) -> TestResult {
        self.ready()?;
        context.expect_eq(self.metadata.capacity(), 2);
        context.expect_eq(self.mapping.capacity(), 2);
        self.held[0] = Some(Returned::Buffer(
            self.metadata
                .get(64, mm::PAGE_SIZE)
                .map_err(|_| TestError("bounded first metadata read failed"))?,
        ));
        self.held[1] = Some(Returned::Buffer(
            self.metadata
                .get(65, mm::PAGE_SIZE)
                .map_err(|_| TestError("bounded second metadata read failed"))?,
        ));
        self.disk.test_flush_completions();
        context.expect_eq(
            matches!(
                self.metadata.get(66, mm::PAGE_SIZE),
                Err(BlockError::NoMemory)
            ),
            true,
        );
        context.expect_eq(self.metadata.prune_unused(), 0);
        drop(self.held[0].take());
        self.held[0] = Some(Returned::Buffer(
            self.metadata
                .get(66, mm::PAGE_SIZE)
                .map_err(|_| TestError("bounded clean metadata victim did not retire"))?,
        ));
        context.expect_eq(self.metadata.len(), 2);
        context.expect_eq(
            self.held[0]
                .as_ref()
                .unwrap()
                .valid(66 * mm::PAGE_SIZE as u64),
            Ok(true),
        );
        drop(self.held[0].take());
        drop(self.held[1].take());
        self.disk.test_flush_completions();
        context.expect_eq(self.metadata.prune_unused(), 2);
        context.expect_eq(self.metadata.len(), 0);

        self.held[0] = Some(Returned::Folio(
            self.file_page(64)
                .map_err(|_| TestError("bounded first file read failed"))?,
        ));
        self.held[1] = Some(Returned::Folio(
            self.file_page(65)
                .map_err(|_| TestError("bounded second file read failed"))?,
        ));
        self.disk.test_flush_completions();
        context.expect_eq(
            matches!(self.mapping.get_or_prepare(66), Err(BlockError::NoMemory)),
            true,
        );
        context.expect_eq(self.mapping.prune_unused(), 0);
        drop(self.held[0].take());
        self.held[0] =
            Some(Returned::Folio(self.file_page(66).map_err(|_| {
                TestError("bounded clean file victim did not retire")
            })?));
        context.expect_eq(self.mapping.len(), 2);
        context.expect_eq(
            self.held[0]
                .as_ref()
                .unwrap()
                .valid(66 * mm::PAGE_SIZE as u64),
            Ok(true),
        );
        drop(self.held[0].take());
        drop(self.held[1].take());
        self.disk.test_flush_completions();
        context.expect_eq(self.mapping.prune_unused(), 2);
        context.expect_eq(self.mapping.len(), 0);
        Ok(())
    }

    /// 预留控制槽后耗尽真实物理页；不在压力期间申请额外测试拥有存储。
    fn exhaust_pages(&mut self, context: &mut TestContext<'_>) -> TestResult {
        mm::drain_all_pages();
        for order in [10, 0] {
            loop {
                if self.pressure.len() == self.pressure.capacity() {
                    return Err(TestError("cache pressure handle capacity exhausted"));
                }
                let Some(pages) = mm::alloc_pages(order, false) else {
                    break;
                };
                self.pressure.push(pages);
            }
        }
        let stats = mm::stats();
        context.expect_eq(stats.buddy_free_pages + stats.pcp_cached_pages, 0);
        context.expect_eq(mm::alloc_pages(0, false).is_none(), true);
        Ok(())
    }

    /// 满缓存先锁外归还 clean 页，真实零空闲页时仍能发表新占位；放压后实际读盘验证。
    fn reclaim_under_oom(&mut self, context: &mut TestContext<'_>) -> TestResult {
        self.ready()?;
        for index in 64..64 + self.mapping.capacity() as u64 {
            drop(
                self.file_page(index)
                    .map_err(|_| TestError("OOM reclaim warm read failed"))?,
            );
        }
        self.disk.test_flush_completions();
        context.expect_eq(self.mapping.len(), self.mapping.capacity());
        self.exhaust_pages(context)?;
        let index = 80;
        let (folio, read) = self.mapping.get_or_prepare(index).map_err(|_| {
            TestError("full clean cache did not reclaim before candidate allocation")
        })?;
        let read = read.ok_or(TestError("OOM replacement did not own the new fill"))?;
        let (hit, duplicate) = self
            .mapping
            .get_or_prepare(index)
            .map_err(|_| TestError("OOM replacement placeholder disappeared"))?;
        context.expect_eq(hit.phys_addr(), folio.phys_addr());
        context.expect_eq(duplicate.is_none(), true);
        context.expect_eq(folio.is_ready(), false);
        context.expect_eq(self.mapping.len(), self.mapping.capacity());
        self.pressure.clear();
        self.disk
            .load_folio(index * mm::PAGE_SIZE as u64, read)
            .map_err(|_| TestError("reclaimed folio real read failed"))?;
        context.expect_eq(folio.wait_ready(), Ok(()));
        context.expect_eq(
            folio.with_bytes(0, mm::PAGE_SIZE, |bytes| {
                patterned(bytes, 1, index * mm::PAGE_SIZE as u64, mm::PAGE_SIZE)
            }),
            Ok(true),
        );
        Ok(())
    }

    /// 真实页 OOM 中已有 metadata/file hit 仍可读且保原 PFN，不依赖新的申请。
    fn oom(&mut self, context: &mut TestContext<'_>) -> TestResult {
        self.ready()?;
        self.held[0] = Some(Returned::Buffer(
            self.metadata
                .get(64, mm::PAGE_SIZE)
                .map_err(|_| TestError("OOM warm metadata read failed"))?,
        ));
        self.held[1] = Some(Returned::Folio(
            self.file_page(64)
                .map_err(|_| TestError("OOM warm file read failed"))?,
        ));
        self.disk.test_flush_completions();
        self.exhaust_pages(context)?;
        let buffer = self
            .metadata
            .get(64, mm::PAGE_SIZE)
            .map_err(|_| TestError("metadata hit allocated under OOM"))?;
        context.expect_eq(
            buffer.folio().phys_addr(),
            self.held[0].as_ref().unwrap().folio().phys_addr(),
        );
        context.expect_eq(
            buffer.with_bytes(|bytes| patterned(bytes, 1, OFFSET, mm::PAGE_SIZE)),
            Ok(true),
        );
        let (folio, read) = self
            .mapping
            .get_or_prepare(64)
            .map_err(|_| TestError("file hit allocated under OOM"))?;
        context.expect_eq(read.is_none(), true);
        context.expect_eq(
            folio.phys_addr(),
            self.held[1].as_ref().unwrap().folio().phys_addr(),
        );
        context.expect_eq(
            folio.with_bytes(0, mm::PAGE_SIZE, |bytes| {
                patterned(bytes, 1, OFFSET, mm::PAGE_SIZE)
            }),
            Ok(true),
        );
        context.expect_eq(
            matches!(
                self.metadata.get(65, mm::PAGE_SIZE),
                Err(BlockError::NoMemory)
            ),
            true,
        );
        context.expect_eq(
            matches!(self.mapping.get_or_prepare(65), Err(BlockError::NoMemory)),
            true,
        );
        self.pressure.clear();
        Ok(())
    }

    /// 本 fixture 独占 producer，Disk 保活原控制区；只观察真实 avail.idx，不写 ring。
    fn avail_index(&self) -> u16 {
        // SAFETY: 地址来自此 Disk 接管前的原控制页及真实 RingLayout，u16 对齐；
        // 调用仅在 ready 成功且尚未 stop 的正文，fixture 无并发 stop，仅读 volatile 标量。
        u16::from_le(unsafe { core::ptr::read_volatile(self.avail_address as *const u16) })
    }

    /// 两个真实 file waiter 先 Blocked，CPU0 原控制 Task 再令正式准备失败且不发表请求。
    fn prepare_oom(&mut self, context: &mut TestContext<'_>) -> TestResult {
        self.ready()?;
        self.publish_one();
        self.publish_one();
        self.publish_one();
        until("prepare-OOM file waiters did not block", || {
            self.observation.blocked(1) && self.observation.blocked(2)
        })?;
        let before = self.disk.test_snapshot();
        let avail = self.avail_index();
        self.observation.gate.complete();
        self.join_tasks()?;
        let evidence = self
            .observation
            .oom_evidence
            .lock()
            .as_ref()
            .copied()
            .ok_or(TestError("prepare-OOM allocator evidence absent"))?;
        context.expect_eq(evidence, (true, true));
        for result in &*self.observation.results.lock() {
            context.expect_eq(matches!(result, Some(Err(BlockError::NoMemory))), true);
        }
        let folio = self.held[0].as_ref().unwrap().folio();
        context.expect_eq(folio.is_ready(), true);
        context.expect_eq(folio.wait_ready(), Err(BlockError::NoMemory));
        context.expect_eq(
            folio.with_bytes(0, mm::PAGE_SIZE, |_| ()),
            Err(BlockError::NoMemory),
        );
        let (hit, read) = self
            .mapping
            .get_or_prepare(64)
            .map_err(|_| TestError("prepare-OOM live placeholder disappeared"))?;
        context.expect_eq(hit.phys_addr(), folio.phys_addr());
        context.expect_eq(read.is_none(), true);
        context.expect_eq(hit.wait_ready(), Err(BlockError::NoMemory));
        let after = self.disk.test_snapshot();
        context.expect_eq(after.submitted, before.submitted);
        context.expect_eq(after.notifications, before.notifications);
        context.expect_eq(after.inflight, before.inflight);
        context.expect_eq(self.avail_index(), avail);
        context.diagnostic(format_args!(
            "cache-prepare-OOM real-page-NULL={} real-heap-NULL={} native-Blocked-waiters=2 original-PFN={} submitted-before={} submitted-after={} avail-before={} avail-after={} hardware-EIO-covered=0",
            evidence.0, evidence.1, folio.phys_addr(), before.submitted, after.submitted, avail, self.avail_index(),
        ));
        Ok(())
    }

    /// 正常和主体失败共用真实静止化：停未发表 producer、放 gate/read、恢复 used、stop/join/Work 尾。
    fn quiesce(&mut self, context: &mut TestContext<'_>) -> TestResult {
        self.observation.cancelled.store(true, Ordering::Release);
        self.observation.gate.complete();
        drop(core::mem::take(&mut self.prepared));
        self.pressure.clear();
        let pressure = self.observation.pressure.lock().take();
        drop(pressure);
        self.disk.test_resume_completions();
        if let Some(extra) = &self.extra {
            extra.test_resume_completions();
        }
        self.disk
            .stop()
            .map_err(|_| TestError("cache stop cannot converge; retaining original DMA owner"))?;
        if let Some(extra) = &self.extra {
            extra.stop().map_err(|_| {
                TestError("extra cache stop cannot converge; retaining original owner")
            })?;
        }
        if let Some(join) = self.joins.first() {
            until("cache read owner did not leave stack", || {
                join.try_join().is_some()
            })?;
        }
        let read = self.observation.read.lock().take();
        drop(read);
        self.join_tasks()?;
        self.queue.close_sync();
        context.expect_eq(self.disk.test_snapshot().inflight, 0);
        if let Some(extra) = &self.extra {
            context.expect_eq(extra.test_snapshot().inflight, 0);
        }
        Ok(())
    }

    /// stop、IRQ/Work 同步与真实 join 已成功后锁外销毁原引用，核完整六域基线。
    fn tear_down(self, context: &mut TestContext<'_>) -> TestResult {
        let baseline = self.baseline;
        drop(self);
        recovered(context, baseline)
    }
}

macro_rules! fixture {
    ($name:ident, $scenario:ident) => {
        struct $name(Owner);
        impl Fixture for $name {
            /// 每例完整准备自己的真实设备、控制存储与 Task，再发表硬件。
            fn set_up(_: &mut TestContext<'_>, _: &()) -> Result<Self, TestError> {
                Owner::prepare(Scenario::$scenario).map(Self)
            }
            /// 具体 owner 无法收敛时返回错误，typed case-abort 保留原 fixture。
            fn quiesce(&mut self, context: &mut TestContext<'_>) -> TestResult {
                self.0.quiesce(context)
            }
            /// 仅在所有实际异步责任结束后消费 fixture 并结六域账。
            fn tear_down(self, context: &mut TestContext<'_>) -> TestResult {
                self.0.tear_down(context)
            }
        }
    };
}
fixture!(Views, Views);
fixture!(MetadataFlight, MetadataFlight);
fixture!(FileFlight, FileFlight);
fixture!(Independent, Independent);
fixture!(MetadataIndependent, MetadataIndependent);
fixture!(Failure, Failure);
fixture!(Bounded, Bounded);
fixture!(Oom, Oom);
fixture!(PrepareOom, PrepareOom);

#[ktest::suite(fixture = Pages, context = "Thread", tags("cache", "folio"), timeout_ms = 15000)]
mod folio {
    use super::*;
    /// 同 PFN 原引用、末引用归页与高阶拒绝保原块。
    #[test]
    fn original_page_lifetime(context: &mut TestContext<'_>, fixture: &mut Pages) -> TestResult {
        fixture.identity(context)
    }
}

#[ktest::suite(fixture = Views, context = "Thread", tags("cache", "metadata"), resources("root", "extra"), timeout_ms = 15000)]
mod metadata_views {
    use super::*;
    /// 两个真实设备各自提供完整 1/2/4KiB 非零短视图。
    #[test]
    fn complete_pattern_1_2_4k(context: &mut TestContext<'_>, fixture: &mut Views) -> TestResult {
        fixture.0.views(context)
    }
}

#[ktest::suite(fixture = MetadataFlight, context = "Thread", min_cpus = 2, tags("cache", "metadata", "B14"), resources("root"), timeout_ms = 15000)]
mod metadata_singleflight {
    use super::*;
    /// 同 key 唯一真实提交，原生等待者 Blocked 后读同 PFN。
    #[test]
    fn blocked_waiter_one_submit(
        context: &mut TestContext<'_>,
        fixture: &mut MetadataFlight,
    ) -> TestResult {
        fixture.0.singleflight(context)
    }
}

#[ktest::suite(fixture = FileFlight, context = "Thread", min_cpus = 2, tags("cache", "file", "B14"), resources("root"), timeout_ms = 15000)]
mod file_singleflight {
    use super::*;
    /// 唯一 file read owner 由实际 Disk 填充，同 key waiter 复用原页。
    #[test]
    fn blocked_waiter_one_submit(
        context: &mut TestContext<'_>,
        fixture: &mut FileFlight,
    ) -> TestResult {
        fixture.0.singleflight(context)
    }
}

#[ktest::suite(fixture = MetadataIndependent, context = "Thread", min_cpus = 2, tags("cache", "metadata", "B15"), resources("root"), timeout_ms = 15000)]
mod metadata_parallel {
    use super::*;
    /// 元数据不同 key 同时持有真实设备请求，各自消费完整图样。
    #[test]
    fn independent_keys_two_inflight(
        context: &mut TestContext<'_>,
        fixture: &mut MetadataIndependent,
    ) -> TestResult {
        fixture.0.independent(context)
    }
}

#[ktest::suite(fixture = Independent, context = "Thread", min_cpus = 2, tags("cache", "file", "B15"), resources("root"), timeout_ms = 15000)]
mod file_parallel {
    use super::*;
    /// 两个独立 key 真正在途，恢复真实 used 后各读完整图样。
    #[test]
    fn independent_keys_two_inflight(
        context: &mut TestContext<'_>,
        fixture: &mut Independent,
    ) -> TestResult {
        fixture.0.independent(context)
    }
}

#[ktest::suite(fixture = Failure, context = "Thread", min_cpus = 2, tags("cache", "file", "QueueStopped", "B11", "B14"), resources("root"), timeout_ms = 15000)]
mod stopped_failure {
    use super::*;
    /// 真实 stopped Disk 拒绝 read owner，两个原生 waiter 同错误醒来且内容不可读。
    #[test]
    fn stopped_read_wakes_all(context: &mut TestContext<'_>, fixture: &mut Failure) -> TestResult {
        fixture.0.failure(context)
    }
}

#[ktest::suite(fixture = Bounded, context = "Thread", tags("cache", "prune", "B23"), resources("root"), timeout_ms = 15000)]
mod bounded {
    use super::*;
    /// 两种缓存预算内 clean 回收，全活 miss 明确 NoMemory。
    #[test]
    fn clean_prune_and_live_no_memory(
        context: &mut TestContext<'_>,
        fixture: &mut Bounded,
    ) -> TestResult {
        fixture.0.bounded(context)
    }
}

#[ktest::suite(fixture = Oom, context = "Thread", tags("cache", "oom", "B23"), resources("root"), timeout_ms = 15000)]
mod oom {
    use super::*;
    /// 真实物理页耗尽时旧 hit 仍读取原数据，新 miss 保 NoMemory。
    #[test]
    fn existing_hits_read_under_real_oom(
        context: &mut TestContext<'_>,
        fixture: &mut Oom,
    ) -> TestResult {
        fixture.0.oom(context)
    }
    /// 满预算但存在 clean 页时，先回收原页再准备新 key，不产生可避免的 NoMemory。
    #[test]
    fn clean_replacement_under_real_oom(
        context: &mut TestContext<'_>,
        fixture: &mut Oom,
    ) -> TestResult {
        fixture.0.reclaim_under_oom(context)
    }
}

#[ktest::suite(fixture = PrepareOom, context = "Thread", min_cpus = 2,
    tags("cache", "file", "prepare-oom", "B14"), resources("root"), timeout_ms = 60000)]
mod prepare_oom {
    use super::*;
    /// 正式准备真 OOM 不发表请求，两个同页 waiter 取得原 NoMemory。
    #[test]
    fn unsubmitted_read_wakes_all(
        context: &mut TestContext<'_>,
        fixture: &mut PrepareOom,
    ) -> TestResult {
        fixture.0.prepare_oom(context)
    }
}
