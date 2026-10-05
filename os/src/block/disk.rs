//! 固定请求槽与真实 ring head 共同维持块 I/O 的唯一拥有关系。

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::marker::PhantomPinned;
use core::pin::{pin, Pin};
use core::ptr;

use virtio_drivers::device::blk::{decode_status, prepare_pdu, PDU_STRIDE, STATUS_OFFSET};
use virtio_drivers::transport::{DeviceStatus, InterruptStatus};
use virtio_drivers::{DmaRegion, Error, Hal, MappedSegment};

use crate::block::bio::BioInner;
use crate::block::{Bio, BlkOperation, BlockError, QueueLimits, MAX_BATCH};
use crate::drivers::virtio::{VirtioDevice, VirtioIdentity, VirtioInitError};
use crate::irq::{self, IrqRegistration};
use crate::mm::dma::KernelHal;
use crate::sync::{SleepMutex, SpinLock};
use crate::wait::{WaitNode, WaitQueue, Waiter};
use crate::workqueue::{Work, WorkQueue};

pub(super) const IRQ_BUDGET: usize = 8;

pub type DiskRef = Pin<Arc<Disk>>;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum QueuePhase {
    Prepared,
    Live,
    Draining,
    Faulting,
    Resetting,
    DmaStopped,
    Stopped,
    Quarantined,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SlotState {
    Free,
    Preparing,
    Submitted { head: u16 },
    Finishing,
}

pub(super) struct QueueState {
    pub(super) device: Option<VirtioDevice>,
    pub(super) phase: QueuePhase,
    pub(super) failure: Option<BlockError>,
    pub(super) generators: usize,
    pub(super) writer_generators: usize,
    pub(super) active_slots: usize,
    pub(super) writing_slots: usize,
    pub(super) notify_active: usize,
    pub(super) pending_notify: bool,
    pub(super) mounts: usize,
    pub(super) slot_states: Vec<SlotState>,
    pub(super) heads: Vec<Option<u16>>,
    #[cfg(any(feature = "storage-tests", feature = "ktest"))]
    counters: TestSnapshot,
    #[cfg(any(feature = "storage-tests", feature = "ktest"))]
    test_paused: bool,
    #[cfg(feature = "ktest")]
    pub(super) ktest_stop: StopTestState,
    #[cfg(feature = "ktest")]
    ktest_folio_dma: Option<(usize, u64, usize, usize)>,
}

impl QueueState {
    /// 已接受生成者在排空阶段仍能继续发表，故障之后不能新增设备引用。
    fn accepts_children(&self) -> bool {
        matches!(self.phase, QueuePhase::Live | QueuePhase::Draining)
    }

    /// 停机或故障返回队列事实，不让消费者把额度暂满误作设备错误。
    fn stopped_error(&self) -> BlockError {
        self.failure.unwrap_or(BlockError::QueueStopped)
    }
}

pub(super) struct RequestPart {
    pub(super) bio: Arc<BioInner>,
    pub(super) offset: usize,
    pub(super) length: usize,
}

struct SlotResources {
    sg: Vec<MappedSegment>,
    parts: [Option<RequestPart>; MAX_BATCH],
    part_count: usize,
    operation: BlkOperation,
    sector: u64,
    bytes: usize,
    used_len: u32,
    forced_error: Option<BlockError>,
    configured: bool,
}

pub(super) struct SlotStorage {
    resources: UnsafeCell<SlotResources>,
    pub(super) completion_work: Option<Pin<Arc<Work>>>,
    pub(super) tag: u16,
    pub(super) disk: *const Disk,
    pdu: DmaRegion,
}

// SAFETY: resources 仅由 Preparing guard 或真实 Submitted→Finishing 的唯一认领者访问；
// 固定 Vec 不再增长，context 在登记前写定，Work 与 IRQ 尾部结束前 Disk owner 保活。
unsafe impl Send for SlotStorage {}
// SAFETY: 同一槽的两个独占访问期由 queue 状态转换隔开；不向外提供共享资源引用。
unsafe impl Sync for SlotStorage {}

/// 一个固定设备 owner 保存全部槽、控制页与回调上下文，不能从 Arc 中移出后移动。
pub struct Disk {
    pub(super) queue: SpinLock<QueueState>,
    pub(super) control: SleepMutex<()>,
    pub(super) irq: SpinLock<Option<IrqRegistration>>,
    pub(super) progress_work: Option<Pin<Arc<Work>>>,
    pub(super) slots: Vec<SlotStorage>,
    pub(super) available: WaitQueue,
    pub(super) quiescent: WaitQueue,
    pub(super) limits: QueueLimits,
    pub(super) identity: VirtioIdentity,
    pub(super) _pinned: PhantomPinned,
}

// SAFETY: 所有设备/ring 状态只在 queue 锁中访问；槽资源使用独占状态协议，
// 固定裸 context 的寿命由显式 stop 的 IRQ 注销与 Work close_sync 共同终止。
unsafe impl Send for Disk {}
// SAFETY: 安全接口不暴露 DMA 引用；跨 Task 的准备和完成只访问分别认领的槽。
unsafe impl Sync for Disk {}

/// 仅验收镜像保存真实队列事件计数，不参与正常请求协议。
#[cfg(any(feature = "storage-tests", feature = "ktest"))]
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct TestSnapshot {
    pub submitted: usize,
    pub completed: usize,
    pub peak_inflight: usize,
    pub notifications: usize,
    pub resource_parks: usize,
    pub splits: usize,
    pub merges: usize,
    pub budget_continuations: usize,
    pub inflight: usize,
    /// 原 IRQ action 实际读取并确认本设备 ISR 的次数。
    #[cfg(feature = "ktest")]
    pub irq_calls: usize,
    /// 实际 ISR 含 QUEUE 位的次数，与完成请求数分开。
    #[cfg(feature = "ktest")]
    pub irq_queue: usize,
    /// 实际 ISR 完全为空的次数，不包含仅配置变更的中断。
    #[cfg(feature = "ktest")]
    pub irq_empty: usize,
}

/// 停机用例只观察本盘生产转换；拒读确认开关不伪造硬件状态。
#[cfg(feature = "ktest")]
#[derive(Clone, Copy, Debug, Default)]
pub(super) struct StopTestState {
    pub(super) pause_used: bool,
    pub(super) withhold_confirmation: bool,
    pub(super) published: usize,
    pub(super) finished: usize,
    pub(super) reset_requests: usize,
    pub(super) reset_confirmations: usize,
}

impl Disk {
    /// 在普通 Task 中完整准备固定表与全部 Work；此时硬件尚不知道任何队列地址。
    pub fn prepare(device: VirtioDevice, workqueue: &WorkQueue) -> Result<DiskRef, BlockError> {
        let limits = QueueLimits::from_info(device.info())?;
        Self::prepare_with_limits(
            device,
            workqueue,
            limits,
            #[cfg(any(feature = "storage-tests", feature = "ktest"))]
            None,
        )
    }

    /// 只有构造期能降低测试限额，固定 Disk 发表后不再修改共享能力。
    #[cfg(any(feature = "storage-tests", feature = "ktest"))]
    pub(crate) fn test_prepare_with_limits(
        device: VirtioDevice,
        workqueue: &WorkQueue,
        max_bytes: usize,
        max_segments: usize,
        max_segment_bytes: u32,
        dma_boundary_mask: u64,
    ) -> Result<DiskRef, BlockError> {
        let mut limits = QueueLimits::from_info(device.info())?;
        if max_bytes < limits.logical_block_size as usize
            || limits.max_bytes < max_bytes
            || max_bytes % limits.logical_block_size as usize != 0
            || max_segments == 0
            || limits.max_segments < max_segments
            || max_segment_bytes == 0
            || limits.max_segment_bytes < max_segment_bytes
            || (dma_boundary_mask != u64::MAX
                && !dma_boundary_mask.wrapping_add(1).is_power_of_two())
        {
            return Err(BlockError::InvalidRange);
        }
        limits.max_bytes = max_bytes;
        limits.max_segments = max_segments;
        limits.max_segment_bytes = max_segment_bytes;
        limits.dma_boundary_mask = dma_boundary_mask;
        Self::prepare_with_limits(
            device,
            workqueue,
            limits,
            #[cfg(any(feature = "storage-tests", feature = "ktest"))]
            None,
        )
    }

    /// 串行验收通过本次调用的栈输出保存实际 SG reserve 失败，不武装全局分配器。
    #[cfg(any(feature = "storage-tests", feature = "ktest"))]
    pub(crate) fn test_prepare_observed(
        device: VirtioDevice,
        workqueue: &WorkQueue,
        observation: &mut Option<(u16, crate::drivers::virtio::TestPrepareSnapshot)>,
    ) -> Result<DiskRef, BlockError> {
        assert!(observation.is_none(), "stale SG preparation observation");
        let limits = QueueLimits::from_info(device.info())?;
        Self::prepare_with_limits(device, workqueue, limits, Some(observation))
    }

    /// 锁外构造唯一固定表，生产和受控限额镜像共用全部实际资源路径。
    fn prepare_with_limits(
        device: VirtioDevice,
        workqueue: &WorkQueue,
        limits: QueueLimits,
        #[cfg(any(feature = "storage-tests", feature = "ktest"))] mut observation: Option<
            &mut Option<(u16, crate::drivers::virtio::TestPrepareSnapshot)>,
        >,
    ) -> Result<DiskRef, BlockError> {
        assert!(
            !device.requires_reset(),
            "preparing an already published disk"
        );
        let identity = device.identity();
        let pdu = device.control_regions().1;
        let mut slots = Vec::new();
        let mut slot_states = Vec::new();
        let mut heads = Vec::new();
        slots
            .try_reserve_exact(usize::from(limits.tag_count))
            .map_err(|_| BlockError::NoMemory)?;
        slot_states
            .try_reserve_exact(usize::from(limits.tag_count))
            .map_err(|_| BlockError::NoMemory)?;
        heads
            .try_reserve_exact(usize::from(limits.queue_depth))
            .map_err(|_| BlockError::NoMemory)?;
        for tag in 0..limits.tag_count {
            let mut sg = Vec::new();
            sg.try_reserve_exact(limits.max_segments).map_err(|_| {
                #[cfg(any(feature = "storage-tests", feature = "ktest"))]
                if let Some(output) = observation.as_deref_mut() {
                    // 此时前三张固定表已成功，device 尚未移入 Disk，未登记 IRQ 或发表队列。
                    *output = Some((tag, device.test_prepare_snapshot()));
                }
                BlockError::NoMemory
            })?;
            slots.push(SlotStorage {
                resources: UnsafeCell::new(SlotResources {
                    sg,
                    parts: core::array::from_fn(|_| None),
                    part_count: 0,
                    operation: BlkOperation::Read,
                    sector: 0,
                    bytes: 0,
                    used_len: 0,
                    forced_error: None,
                    configured: false,
                }),
                completion_work: None,
                tag,
                disk: ptr::null(),
                pdu,
            });
            slot_states.push(SlotState::Free);
        }
        heads.resize(usize::from(limits.queue_depth), None);
        let disk = Arc::try_new(Self {
            queue: SpinLock::new(QueueState {
                device: Some(device),
                phase: QueuePhase::Prepared,
                failure: None,
                generators: 0,
                writer_generators: 0,
                active_slots: 0,
                writing_slots: 0,
                notify_active: 0,
                pending_notify: false,
                mounts: 0,
                slot_states,
                heads,
                #[cfg(any(feature = "storage-tests", feature = "ktest"))]
                counters: TestSnapshot::default(),
                #[cfg(any(feature = "storage-tests", feature = "ktest"))]
                test_paused: false,
                #[cfg(feature = "ktest")]
                ktest_stop: StopTestState::default(),
                #[cfg(feature = "ktest")]
                ktest_folio_dma: None,
            }),
            control: SleepMutex::new(()),
            irq: SpinLock::new(None),
            progress_work: None,
            slots,
            available: WaitQueue::new(),
            quiescent: WaitQueue::new(),
            limits,
            identity,
            _pinned: PhantomPinned,
        })
        .map_err(|_| BlockError::NoMemory)?;
        let raw = Arc::into_raw(disk);
        // SAFETY: into_raw 的唯一强引用尚未发表；构造过程中不队列化 Work，不注册 IRQ。
        // 原 Arc 立刻恢复，失败仍以 Prepared 正常析构；固定 Vec 不再重新分配。
        let disk = unsafe { Pin::new_unchecked(Arc::from_raw(raw)) };
        let progress = Work::try_new_reserved(workqueue, 0, progress_callback, raw as usize)
            .map_err(|_| BlockError::NoMemory)?;
        // SAFETY: 还没有第二份 Disk 引用或异步票；写入不移动 Disk 或槽 Vec。
        unsafe { (*raw.cast_mut()).progress_work = Some(progress) };
        for index in 0..usize::from(limits.tag_count) {
            // SAFETY: 固定 Vec 已完整构造且从不增长；仅未发表构造者访问此槽。
            let slot = unsafe { &mut (&mut (*raw.cast_mut()).slots)[index] };
            slot.disk = raw;
            slot.completion_work = Some(
                Work::try_new_reserved(
                    workqueue,
                    0,
                    completion_callback,
                    slot as *const SlotStorage as usize,
                )
                .map_err(|_| BlockError::NoMemory)?,
            );
        }
        Ok(disk)
    }

    /// 串行注册真实 disabled action，再发表硬件；失败仍保留 owner 供显式停止。
    pub fn start(&self) -> Result<(), BlockError> {
        let _control = self.control.lock();
        let number = {
            let q = self.queue.lock();
            if q.phase != QueuePhase::Prepared {
                return Err(BlockError::NotReady);
            }
            q.device
                .as_ref()
                .expect("prepared disk has no device")
                .interrupt()
        };
        // SAFETY: 私有构造器返回 pinned Arc；外部 owner 必须显式 stop 后才释放。
        // callback 有固定预算，不调用同线 token 或睡眠；同步注销结束其 context 寿命。
        let registration =
            unsafe { irq::register(number, irq_callback, self as *const Self as *mut ()) }
                .map_err(|_| BlockError::Busy)?;
        *self.irq.lock() = Some(registration);
        let result = {
            let mut q = self.queue.lock();
            let result = q
                .device
                .as_mut()
                .expect("prepared disk has no device")
                .publish();
            match result {
                Ok(()) => {
                    q.phase = QueuePhase::Live;
                    Ok(())
                }
                Err(error) => {
                    let error = initialization_error(error);
                    q.failure = Some(error);
                    q.phase = QueuePhase::Faulting;
                    Err(error)
                }
            }
        };
        if result.is_ok() {
            self.irq
                .lock()
                .as_ref()
                .expect("registered disk has no IRQ")
                .enable();
        }
        result
    }

    /// 返回 Bio 构造时绑定的真实设备身份。
    pub fn identity(&self) -> VirtioIdentity {
        self.identity
    }

    /// 返回本次协商的限额，不在请求路径重新读取配置。
    pub fn limits(&self) -> QueueLimits {
        self.limits
    }

    /// 非 slot 的 Task 最后义务复用原域，并在 Bio 构造期预留独占执行栈及一次性尾部。
    pub(super) fn prepare_finalizer(
        &self,
        callback: fn(Pin<&Work>),
        context: usize,
    ) -> Result<Pin<Arc<Work>>, BlockError> {
        self.progress_work
            .as_ref()
            .expect("prepared disk has no progress work")
            .try_new_related_reserved(callback, context)
            .map_err(|_| BlockError::NoMemory)
    }

    /// 私有构造保证 self 来自未移动的原 Arc；仅无 child 的完成移交临时保活。
    pub(super) fn clone_ref(&self) -> DiskRef {
        let raw = self as *const Self;
        // SAFETY: Disk 只能由 prepare 的 pinned Arc 构造，当前借用保原分配存活；
        // 增加真实现存控制块强引用，立刻原样恢复并固定，未从 Arc 移出对象。
        unsafe {
            Arc::increment_strong_count(raw);
            Pin::new_unchecked(Arc::from_raw(raw))
        }
    }

    /// 普通生成者或其独立完成 Work 在真实回调尾部归还同一 accepted 责任。
    pub(super) fn finish_generation_tail(&self, operation: BlkOperation) {
        {
            let mut q = self.queue.lock();
            assert!(q.generators != 0, "block generator tail ended twice");
            q.generators -= 1;
            if operation == BlkOperation::Write {
                q.writer_generators -= 1;
            }
        }
        self.quiescent.wake_all();
    }

    /// 只在 Live 接受一个 root，挂载 claim 与写生成者检查属于同一临界区。
    pub(super) fn admit(&self, bio: Bio) -> Result<ActiveBio<'_>, (Bio, BlockError)> {
        {
            let mut q = self.queue.lock();
            let error = if !matches!(q.phase, QueuePhase::Live) {
                Some(q.stopped_error())
            } else if bio.inner.target() != self.identity {
                Some(BlockError::InvalidRange)
            } else if bio.inner.operation() == BlkOperation::Write
                && (self.limits.readonly || 0 < q.mounts)
            {
                Some(BlockError::ReadOnly)
            } else {
                None
            };
            if let Some(error) = error {
                return Err((bio, error));
            }
            q.generators += 1;
            if bio.inner.operation() == BlkOperation::Write {
                q.writer_generators += 1;
            }
        }
        bio.inner.begin();
        Ok(ActiveBio {
            bio: bio.inner,
            offset: 0,
            disk: self,
            finished: false,
        })
    }

    /// 先 kick 已接受前缀，再栈登记、复查、真实 park；排空只拒新 root。
    pub(super) fn reserve_slot(&self) -> Result<PreparingSlot<'_>, BlockError> {
        loop {
            if let Some(slot) = self.try_reserve_slot()? {
                return Ok(slot);
            }
            self.unplug();
            let waiter = Waiter::new();
            let mut node = pin!(WaitNode::new(waiter.waker()));
            // SAFETY: self 与栈 pin 节点一直保活，registration 在本轮任何退出前先撤链。
            let registration = unsafe { self.available.register(node.as_mut()) };
            if let Some(slot) = self.try_reserve_slot()? {
                return Ok(slot);
            }
            self.count_park();
            waiter.park();
            drop(registration);
        }
    }

    /// 在唯一 queue 锁中把空闲 tag 交给一个准备者，不触碰任何槽资源。
    fn try_reserve_slot(&self) -> Result<Option<PreparingSlot<'_>>, BlockError> {
        let mut q = self.queue.lock();
        if !q.accepts_children() {
            return Err(q.stopped_error());
        }
        let Some(index) = q
            .slot_states
            .iter()
            .position(|state| *state == SlotState::Free)
        else {
            return Ok(None);
        };
        q.slot_states[index] = SlotState::Preparing;
        q.active_slots += 1;
        Ok(Some(PreparingSlot {
            disk: self,
            tag: index as u16,
            published: false,
        }))
    }

    /// 在锁内认领唯一通知责任，锁外 doorbell 的真实尾部阻止并发 reset。
    pub(super) fn unplug(&self) {
        let notification = {
            let mut q = self.queue.lock();
            if !q.accepts_children() || !q.pending_notify {
                return;
            }
            q.pending_notify = false;
            let driver = q.device.as_ref().expect("live disk has no device").driver();
            if !driver.should_notify() {
                return;
            }
            let notification = driver.notification();
            q.notify_active += 1;
            notification
        };
        // SAFETY: notify_active 保持设备与映射有效，reset 先关闭认领并等此计数归零。
        // 门铃前屏障发布描述符；门铃后屏障还必须先完成设备输出，再向另一CPU归还尾部计数。
        unsafe { notification.notify::<KernelHal>() };
        KernelHal::dma_barrier();
        {
            let mut q = self.queue.lock();
            q.notify_active -= 1;
            #[cfg(any(feature = "storage-tests", feature = "ktest"))]
            {
                q.counters.notifications += 1;
            }
        }
        self.quiescent.wake_all();
    }

    /// 关闭发布和通知认领，只修改本 queue flags，不屏蔽共享中断线。
    pub(super) fn mark_fault(&self, error: BlockError) {
        let claimed = {
            let mut q = self.queue.lock();
            if matches!(
                q.phase,
                QueuePhase::Prepared | QueuePhase::Live | QueuePhase::Draining
            ) {
                q.failure = Some(error);
                q.phase = QueuePhase::Faulting;
                q.pending_notify = false;
                if let Some(device) = q.device.as_mut() {
                    // SAFETY: queue 锁串行后端，原控制区与全部在途 owner 仍保活，未交换 driver。
                    unsafe { device.driver_mut() }.set_interrupts(false);
                }
                true
            } else {
                false
            }
        };
        self.available.wake_all();
        self.quiescent.wake_all();
        if claimed {
            self.queue_progress();
        }
    }

    /// 续处理使用预建 Work 的现存票协议，不在 IRQ 分配。
    pub(super) fn queue_progress(&self) {
        if let Some(work) = &self.progress_work {
            let _ = work.as_ref().queue();
        }
    }

    /// 本设备真实 ISR 决定共享线处理归属，配置变更与 NEEDS_RESET 都进入故障停机。
    fn interrupt(&self) -> bool {
        let (status, fault, live) = {
            let mut q = self.queue.lock();
            let live = q.accepts_children();
            let Some(device) = q.device.as_mut() else {
                return false;
            };
            let status = device.ack_interrupt();
            let fault = live
                && (device
                    .status()
                    .intersects(DeviceStatus::DEVICE_NEEDS_RESET | DeviceStatus::FAILED)
                    || status.contains(InterruptStatus::CONFIG));
            #[cfg(feature = "ktest")]
            {
                // 复用本次 ack 返回的真实 ISR；统计与快照同受 queue 锁保护。
                q.counters.irq_calls += 1;
                if status.contains(InterruptStatus::QUEUE) {
                    q.counters.irq_queue += 1;
                } else if status.is_empty() {
                    q.counters.irq_empty += 1;
                }
            }
            (status, fault, live)
        };
        if fault {
            self.request_retire(BlockError::DeviceFault);
        }
        if live && status.contains(InterruptStatus::QUEUE) && !fault {
            self.drain_used();
        }
        !status.is_empty() || fault
    }

    /// 最多消费八个真实 used；重装并复查后由预建 Work 接续，不依赖下一次 IRQ。
    fn drain_used(&self) {
        let mut tags = [None; IRQ_BUDGET];
        let mut count = 0;
        let mut fault = None;
        let mut continuation = false;
        {
            let mut q = self.queue.lock();
            if !q.accepts_children() {
                return;
            }
            #[cfg(any(feature = "storage-tests", feature = "ktest"))]
            if q.test_paused {
                return;
            }
            #[cfg(feature = "ktest")]
            if q.ktest_stop.pause_used {
                return;
            }
            // SAFETY: 全部 ring 操作只在 queue 锁内；控制页在 stop 全尾部结束前不释放。
            unsafe {
                q.device
                    .as_mut()
                    .expect("live disk has no device")
                    .driver_mut()
            }
            .set_interrupts(false);
            while count < IRQ_BUDGET {
                // SAFETY: 此短借用结束后才访问 head 表，不留跨锁 driver 引用。
                let used = unsafe {
                    q.device
                        .as_mut()
                        .expect("live disk has no device")
                        .driver_mut()
                }
                .pop_used();
                match used {
                    Ok(Some(used)) => {
                        let Some(tag) = q.heads.get(usize::from(used.head)).copied().flatten()
                        else {
                            fault = Some(BlockError::DeviceFault);
                            break;
                        };
                        if q.slot_states[usize::from(tag)]
                            != (SlotState::Submitted { head: used.head })
                        {
                            fault = Some(BlockError::DeviceFault);
                            break;
                        }
                        q.heads[usize::from(used.head)] = None;
                        q.slot_states[usize::from(tag)] = SlotState::Finishing;
                        // SAFETY: 本次转换认领此槽，设备已真实 used 且不会再访问 PDU/payload。
                        unsafe {
                            (*self.slots[usize::from(tag)].resources.get()).used_len = used.len;
                        }
                        tags[count] = Some(tag);
                        count += 1;
                    }
                    Ok(None) => break,
                    Err(_) => {
                        fault = Some(BlockError::DeviceFault);
                        break;
                    }
                }
            }
            if fault.is_none() {
                // SAFETY: 同一锁内重装真实 flags 与 used 复查；无借用越过此临界区。
                continuation = unsafe {
                    q.device
                        .as_mut()
                        .expect("live disk has no device")
                        .driver_mut()
                }
                .enable_interrupts_and_recheck();
                #[cfg(any(feature = "storage-tests", feature = "ktest"))]
                if continuation {
                    q.counters.budget_continuations += 1;
                }
            }
        }
        if let Some(error) = fault {
            self.mark_fault(error);
        }
        // descriptor 已经归还；准备者可以继续使用自己的 SG，无需等待回调释放 tag。
        if 0 < count {
            self.available.wake_all();
        }
        for tag in tags[..count].iter().flatten() {
            self.dispatch_slot(*tag);
        }
        if continuation {
            self.queue_progress();
        }
    }

    /// Task parts 始终交专用槽 Work，进度和 reset 只分派；轻完成仍直接结束。
    pub(super) fn dispatch_slot(&self, tag: u16) {
        let slot = &self.slots[usize::from(tag)];
        // SAFETY: 调用者唯一拥有 Submitted→Finishing 的认领，本槽不可准备或二次派发。
        let needs_task = unsafe {
            (&*slot.resources.get())
                .parts
                .iter()
                .flatten()
                .any(|part| part.bio.needs_task())
        };
        if needs_task {
            slot.completion_work
                .as_ref()
                .expect("slot has no completion work")
                .as_ref()
                .queue()
                .expect("active slot completion work is closed");
        } else {
            self.finish_slot(tag);
        }
    }

    /// 在 Finishing 独占期锁外读取稳定 PDU，按真实前缀结束 parts，最后才归还 tag。
    pub(super) fn finish_slot(&self, tag: u16) {
        let slot = &self.slots[usize::from(tag)];
        // SAFETY: 本函数由唯一完成认领者或其专用 Work 调用；Finishing 阻止 prepare/reuse。
        let resources = unsafe { &mut *slot.resources.get() };
        let (completed, error) = completion_result(slot, resources);
        if error == Some(BlockError::DeviceFault) {
            self.mark_fault(BlockError::DeviceFault);
        }
        let writing = resources.operation == BlkOperation::Write;
        let mut remaining = completed;
        for index in 0..resources.part_count {
            let part = resources.parts[index].take().expect("slot part is missing");
            let progress = remaining.min(part.length);
            remaining -= progress;
            let part_error = if progress < part.length || part.length == 0 {
                error
            } else {
                None
            };
            part.bio
                .finish_child(part.offset, part.length, progress, part_error);
            drop(part);
        }
        resources.sg.clear();
        resources.part_count = 0;
        resources.configured = false;
        resources.forced_error = None;
        // 从此处起不再访问本轮 slot 数据；其他 producer 在状态清空后可以马上重用。
        {
            let mut q = self.queue.lock();
            assert_eq!(q.slot_states[usize::from(tag)], SlotState::Finishing);
            q.slot_states[usize::from(tag)] = SlotState::Free;
            q.active_slots -= 1;
            if writing {
                q.writing_slots -= 1;
            }
            #[cfg(feature = "ktest")]
            {
                q.ktest_stop.finished += 1;
            }
            #[cfg(any(feature = "storage-tests", feature = "ktest"))]
            {
                q.counters.completed += 1;
                q.counters.inflight -= 1;
            }
        }
        self.available.wake_all();
        self.quiescent.wake_all();
    }

    /// 只有真实 reset 已确认才能撤 Submitted 的设备拥有期；已认领完成槽不会重复完成。
    pub(super) fn fail_submitted_after_reset(&self, error: BlockError) {
        for index in 0..self.slots.len() {
            let claimed = {
                let mut q = self.queue.lock();
                assert_eq!(q.phase, QueuePhase::DmaStopped);
                if let SlotState::Submitted { head } = q.slot_states[index] {
                    q.heads[usize::from(head)] = None;
                    q.slot_states[index] = SlotState::Finishing;
                    // SAFETY: 真实 reset 已确认并屏障，本次转换唯一认领，设备不能再访问。
                    unsafe {
                        (*self.slots[index].resources.get()).forced_error = Some(error);
                    }
                    true
                } else {
                    false
                }
            };
            if claimed {
                self.dispatch_slot(index as u16);
            }
        }
        self.available.wake_all();
        self.quiescent.wake_all();
    }

    /// 返回验收镜像的真实计数快照，不读取活动 DMA。
    #[cfg(any(feature = "storage-tests", feature = "ktest"))]
    pub(crate) fn test_snapshot(&self) -> TestSnapshot {
        self.queue.lock().counters
    }

    /// 只在本例全部请求和回调结束后捕获实际 Work 票尾，不能用 idle 或定长sleep代替。
    #[cfg(any(feature = "storage-tests", feature = "ktest"))]
    pub(crate) fn test_flush_completions(&self) {
        if let Some(work) = &self.progress_work {
            work.as_ref().flush();
        }
        for slot in &self.slots {
            if let Some(work) = &slot.completion_work {
                work.as_ref().flush();
            }
        }
    }

    /// 成功发表后才累计额外 child 与跨 Bio 合并的真实数量。
    #[cfg(any(feature = "storage-tests", feature = "ktest"))]
    pub(super) fn record_generation(&self, splits: usize, merges: usize) {
        let mut q = self.queue.lock();
        q.counters.splits += splits;
        q.counters.merges += merges;
    }

    /// 暂停本 queue 的完成消费，仍确认真实 ISR，不 mask 共享源也不伪造 used。
    #[cfg(any(feature = "storage-tests", feature = "ktest"))]
    pub(crate) fn test_suspend_completions(&self) {
        let mut q = self.queue.lock();
        assert!(q.accepts_children(), "pausing a stopped disk");
        q.test_paused = true;
        // SAFETY: queue 锁串行真实 flags，设备和控制页均保活，不改变 DMA owner。
        unsafe {
            q.device
                .as_mut()
                .expect("live disk has no device")
                .driver_mut()
        }
        .set_interrupts(false);
    }

    /// 重装真实 queue flags 后复查已发表的 used，用预建 Work 保证恢复进度。
    #[cfg(any(feature = "storage-tests", feature = "ktest"))]
    pub(crate) fn test_resume_completions(&self) {
        let pending = {
            let mut q = self.queue.lock();
            q.test_paused = false;
            if !q.accepts_children() {
                return;
            }
            // SAFETY: 后端只在 queue 锁内借用，reset 使用同锁且控制区仍有效。
            unsafe {
                q.device
                    .as_mut()
                    .expect("live disk has no device")
                    .driver_mut()
            }
            .enable_interrupts_and_recheck()
        };
        if pending {
            self.queue_progress();
        }
    }

    /// 已有真实 used 时交预建进度 Work；保持 NO_INTERRUPT 到生产 drain 重装。
    #[cfg(any(feature = "storage-tests", feature = "ktest"))]
    pub(crate) fn test_resume_via_progress(&self) {
        self.queue.lock().test_paused = false;
        self.queue_progress();
    }

    /// 本例暂停真实 used 消费；仍允许设备 DMA 与 ISR，不改变共享 IRQ 线。
    #[cfg(feature = "ktest")]
    pub(super) fn ktest_hold_used(&self) {
        let mut q = self.queue.lock();
        assert_eq!(q.phase, QueuePhase::Live);
        q.ktest_stop.pause_used = true;
        // SAFETY: 原控制区保活且同一 queue 锁串行 driver，仅关闭此 queue 通知。
        unsafe { q.device.as_mut().expect("live stop fixture").driver_mut() }.set_interrupts(false);
    }

    /// 撤销本例暂停并交原进度 Work 消费真实 used；故障阶段仍不收未确认 DMA。
    #[cfg(feature = "ktest")]
    pub(super) fn ktest_release_used(&self) {
        self.queue.lock().ktest_stop.pause_used = false;
        self.queue_progress();
    }

    /// 返回最近成功发表的单 folio 首 SG：原页物理地址、设备地址、页内偏移和字节数。
    /// 观测者须串行安排该盘 producer，后续原页 I/O 会替换这一记录。
    #[cfg(feature = "ktest")]
    pub(crate) fn ktest_last_folio_dma(&self) -> Option<(usize, u64, usize, usize)> {
        self.queue.lock().ktest_folio_dma
    }

    /// 只在登记后复查仍无额度、即将调用真实 park 时计数。
    fn count_park(&self) {
        #[cfg(any(feature = "storage-tests", feature = "ktest"))]
        {
            self.queue.lock().counters.resource_parks += 1;
        }
    }
}

impl Drop for Disk {
    /// 析构不代替显式停止，IRQ 与 Work raw context 必须先完成全部真实尾部。
    fn drop(&mut self) {
        let q = self.queue.lock();
        assert!(
            matches!(q.phase, QueuePhase::Prepared | QueuePhase::Stopped),
            "dropping an active disk"
        );
        assert!(self.irq.lock().is_none(), "dropping a registered disk");
        assert_eq!(q.generators, 0);
        assert_eq!(q.active_slots, 0);
        assert_eq!(q.notify_active, 0);
    }
}

pub(super) struct ActiveBio<'a> {
    pub(super) bio: Arc<BioInner>,
    pub(super) offset: usize,
    disk: &'a Disk,
    finished: bool,
}

impl ActiveBio<'_> {
    /// 普通 Task 结束父生成者，未知 callback 返回后才减少 accepted 责任。
    pub(super) fn finish(mut self, error: Option<(usize, BlockError)>) {
        self.finish_inner(error);
    }

    /// 若生成者是最后义务，将可睡尾部交独立 Work，不阻塞其它父的生成结束。
    fn finish_inner(&mut self, error: Option<(usize, BlockError)>) {
        if self.finished {
            return;
        }
        let deferred = self.bio.finish_generation(error, Some(self.disk));
        self.finished = true;
        if !deferred {
            self.disk.finish_generation_tail(self.bio.operation());
        }
    }
}

impl Drop for ActiveBio<'_> {
    /// 任意生成错误出口仍结束生成者义务，错误位置保已经完整发表的逻辑前缀。
    fn drop(&mut self) {
        if !self.finished {
            self.disk.unplug();
        }
        self.finish_inner(Some((self.offset, BlockError::QueueStopped)));
    }
}

pub(super) struct PreparingSlot<'a> {
    disk: &'a Disk,
    tag: u16,
    published: bool,
}

impl PreparingSlot<'_> {
    /// guard 唯一拥有 Preparing，资源引用不能长于 guard 的当前借用。
    fn resources(&self) -> &SlotResources {
        // SAFETY: guard 唯一且尚未发表，设备和其他 CPU 都不访问此槽资源。
        unsafe { &*self.disk.slots[usize::from(self.tag)].resources.get() }
    }

    /// Preparing 只有一个 Task 修改固定 SG 和 parts，queue 锁不携带这些引用。
    fn resources_mut(&mut self) -> &mut SlotResources {
        // SAFETY: &mut guard 排除同一准备者内别名；状态尚未 Submitted/Finishing。
        unsafe { &mut *self.disk.slots[usize::from(self.tag)].resources.get() }
    }

    /// 借用当前准备中的小 SG 元数据，不借 DMA payload。
    pub(super) fn sg(&self) -> &[MappedSegment] {
        &self.resources().sg
    }

    /// SG capacity 已在 prepare 完整预留，调用者不得 reserve 或超出 limits 增长。
    pub(super) fn sg_mut(&mut self) -> &mut Vec<MappedSegment> {
        &mut self.resources_mut().sg
    }

    /// 固定 parts 限额供显式 batch 判定，尚未登记任何 child。
    pub(super) fn part_count(&self) -> usize {
        self.resources().part_count
    }

    /// 保存一个原 Bio 的有界责任，child 仅在真正发表前登记。
    pub(super) fn push_part(
        &mut self,
        bio: Arc<BioInner>,
        offset: usize,
        length: usize,
    ) -> Result<(), BlockError> {
        let resources = self.resources_mut();
        if resources.part_count == MAX_BATCH
            || bio.len() < offset.checked_add(length).ok_or(BlockError::InvalidRange)?
        {
            return Err(BlockError::InvalidRange);
        }
        resources.parts[resources.part_count] = Some(RequestPart {
            bio,
            offset,
            length,
        });
        resources.part_count += 1;
        Ok(())
    }

    /// 在 queue 锁外准备本轮稳定 PDU，原视图没有 driver 借用跨锁别名。
    pub(super) fn configure(
        &mut self,
        operation: BlkOperation,
        sector: u64,
        bytes: usize,
    ) -> Result<(), BlockError> {
        let limits = self.disk.limits;
        let resources = self.resources();
        if resources.part_count == 0
            || limits.max_segments < resources.sg.len()
            || limits.max_bytes < bytes
        {
            return Err(BlockError::InvalidRange);
        }
        let sg_bytes = resources
            .sg
            .iter()
            .try_fold(0usize, |total, segment| {
                total.checked_add(segment.length as usize)
            })
            .ok_or(BlockError::InvalidRange)?;
        let part_bytes = resources
            .parts
            .iter()
            .flatten()
            .try_fold(0usize, |total, part| total.checked_add(part.length))
            .ok_or(BlockError::InvalidRange)?;
        if sg_bytes != bytes
            || part_bytes != bytes
            || (operation == BlkOperation::Flush
                && (sector != 0 || bytes != 0 || !resources.sg.is_empty()))
            || (operation != BlkOperation::Flush
                && (bytes == 0 || bytes % limits.logical_block_size as usize != 0))
        {
            return Err(BlockError::InvalidRange);
        }
        let slot = &self.disk.slots[usize::from(self.tag)];
        // SAFETY: Preparing 独占 tag，真实 PDU 原 owner 保活；范围/能力由 Bio 与拆分入口验证。
        unsafe { prepare_pdu(slot.pdu, self.tag, operation, sector) }.map_err(BlockError::from)?;
        let resources = self.resources_mut();
        resources.operation = operation;
        resources.sector = sector;
        resources.bytes = bytes;
        resources.used_len = 0;
        resources.forced_error = None;
        resources.configured = true;
        Ok(())
    }

    /// descriptor 暂满保留整份锁外准备，先 kick、栈登记复查，再真实 park。
    pub(super) fn publish(mut self) -> Result<(), BlockError> {
        if !self.resources().configured {
            return Err(BlockError::InvalidRange);
        }
        loop {
            if self.try_publish()? {
                self.published = true;
                return Ok(());
            }
            self.disk.unplug();
            let waiter = Waiter::new();
            let mut node = pin!(WaitNode::new(waiter.waker()));
            // SAFETY: 栈 pin 节点和 disk 借用覆盖此轮，registration 在任何退出先撤链。
            let registration = unsafe { self.disk.available.register(node.as_mut()) };
            if self.try_publish()? {
                self.published = true;
                return Ok(());
            }
            self.disk.count_park();
            waiter.park();
            drop(registration);
        }
    }

    /// queue 锁中登记 child 并发表真实 avail，安装 head→tag 后才让 IRQ 观察。
    fn try_publish(&mut self) -> Result<bool, BlockError> {
        let disk = self.disk;
        let tag = self.tag;
        let result = {
            let mut q = disk.queue.lock();
            let resources = self.resources();
            if !q.accepts_children() {
                return Err(q.stopped_error());
            }
            assert_eq!(q.slot_states[usize::from(tag)], SlotState::Preparing);
            if q.device
                .as_ref()
                .expect("live disk has no device")
                .driver()
                .available_descriptors()
                < resources.sg.len() + 2
            {
                return Ok(false);
            }
            for part in resources.parts.iter().flatten() {
                part.bio.register_child(part.offset, part.length);
            }
            // SAFETY: tag 独占且 PDU 已准备，父 memory 在 admission 交权并由 parts 保活；
            // queue 锁保护 ring 和 reset，后端只借当前调用，未移出或交换。
            let result = unsafe {
                q.device
                    .as_mut()
                    .expect("live disk has no device")
                    .driver_mut()
                    .submit(tag, resources.operation, &resources.sg)
            };
            if let Ok(head) = result {
                assert!(
                    q.heads[usize::from(head)].is_none(),
                    "submitted head is already owned"
                );
                q.heads[usize::from(head)] = Some(tag);
                q.slot_states[usize::from(tag)] = SlotState::Submitted { head };
                q.pending_notify = true;
                if resources.operation == BlkOperation::Write {
                    q.writing_slots += 1;
                }
                #[cfg(feature = "ktest")]
                {
                    q.ktest_stop.published += 1;
                    if let Some((part, segment)) =
                        resources.parts[0].as_ref().zip(resources.sg.first())
                    {
                        if let Some((physical, offset)) = part.bio.ktest_folio_origin(part.offset) {
                            q.ktest_folio_dma = Some((
                                physical,
                                segment.address.value(),
                                offset,
                                segment.length as usize,
                            ));
                        }
                    }
                }
                #[cfg(any(feature = "storage-tests", feature = "ktest"))]
                {
                    q.counters.submitted += 1;
                    q.counters.inflight += 1;
                    q.counters.peak_inflight = q.counters.peak_inflight.max(q.counters.inflight);
                }
            }
            result
        };
        match result {
            Ok(_) => Ok(true),
            Err(error) => {
                let fault = matches!(
                    error,
                    Error::QueueFull | Error::DeviceNeedsReset | Error::WrongToken
                );
                let error = if fault {
                    BlockError::DeviceFault
                } else {
                    BlockError::from(error)
                };
                let resources = self.resources();
                for part in resources.parts.iter().flatten() {
                    part.bio
                        .finish_child(part.offset, part.length, 0, Some(error));
                }
                if fault {
                    disk.request_retire(error);
                }
                disk.unplug();
                Err(error)
            }
        }
    }
}

impl Drop for PreparingSlot<'_> {
    /// 未发表槽只清普通准备数据，绝不读取或归还任何活动 DMA。
    fn drop(&mut self) {
        if self.published {
            return;
        }
        let disk = self.disk;
        let tag = self.tag;
        let resources = self.resources_mut();
        for part in &mut resources.parts {
            drop(part.take());
        }
        resources.sg.clear();
        resources.part_count = 0;
        resources.configured = false;
        {
            let mut q = disk.queue.lock();
            assert_eq!(q.slot_states[usize::from(tag)], SlotState::Preparing);
            q.slot_states[usize::from(tag)] = SlotState::Free;
            q.active_slots -= 1;
        }
        disk.available.wake_all();
        disk.quiescent.wake_all();
    }
}

/// 状态成功仍须核 writable used 长度，Read 正前缀按 merged parts 的逻辑次序分配。
fn completion_result(slot: &SlotStorage, resources: &SlotResources) -> (usize, Option<BlockError>) {
    if let Some(error) = resources.forced_error {
        return (0, Some(error));
    }
    KernelHal::dma_read_barrier();
    // SAFETY: Finishing 来自真实 used；tag 不可重用，设备已结束本 PDU 的写入，原控制页保活。
    let status = unsafe {
        ptr::read_volatile(
            slot.pdu
                .cpu
                .as_ptr()
                .add(usize::from(slot.tag) * PDU_STRIDE + STATUS_OFFSET),
        )
    };
    if let Err(error) = decode_status(status) {
        return (0, Some(BlockError::from(error)));
    }
    let used = resources.used_len as usize;
    match resources.operation {
        BlkOperation::Read => {
            let expected = resources.bytes + 1;
            if used == expected {
                (resources.bytes, None)
            } else if used < expected {
                (used.saturating_sub(1), Some(BlockError::ShortIo))
            } else {
                (0, Some(BlockError::DeviceFault))
            }
        }
        BlkOperation::Write | BlkOperation::Flush => {
            if used == 1 {
                (resources.bytes, None)
            } else {
                (0, Some(BlockError::DeviceFault))
            }
        }
    }
}

/// 真实初始化失败只保实际协议错误，可能发表 DMA 的 owner 始终留在 Disk。
fn initialization_error(error: VirtioInitError) -> BlockError {
    match error {
        VirtioInitError::Protocol(error) => error.into(),
        VirtioInitError::NoMemory { .. } => BlockError::NoMemory,
        _ => BlockError::DeviceFault,
    }
}

/// IRQ context 来自 pinned Disk，注销同步返回前由外部控制 owner 保活。
///
/// # Safety
/// context 必须是 register 保存的原 Disk，登记寿命内不可移动或析构。
unsafe fn irq_callback(context: *mut ()) -> bool {
    // SAFETY: IRQ 注册契约保活固定 Disk，执行引用直到此 handler 返回才归零。
    unsafe { &*context.cast::<Disk>() }.interrupt()
}

/// 普通 Work 续处理预算或执行唯一故障 reset，不取得外部 control 锁也不关闭自己。
fn progress_callback(work: Pin<&Work>) {
    // SAFETY: 外部 stop 在关闭此 Work 的全部票尾部后才释放 pinned Disk。
    let disk = unsafe { &*(work.context() as *const Disk) };
    let phase = disk.queue.lock().phase;
    if matches!(
        phase,
        QueuePhase::Faulting | QueuePhase::Resetting | QueuePhase::DmaStopped
    ) {
        let _ = disk.fault_reset();
    } else if matches!(phase, QueuePhase::Live | QueuePhase::Draining) {
        disk.drain_used();
    }
}

/// 专用完成 Work 只消费其本轮 Finishing 责任，槽释放后不再读任何旧数据。
fn completion_callback(work: Pin<&Work>) {
    // SAFETY: SlotStorage 位于固定 Vec，Disk 外部 owner 保活至此 Work close_sync 尾部。
    let slot = unsafe { &*(work.context() as *const SlotStorage) };
    let tag = slot.tag;
    // SAFETY: disk context 构造时写定，Work 关闭同步返回前 Disk 不释放。
    let disk = unsafe { &*slot.disk };
    disk.finish_slot(tag);
}
