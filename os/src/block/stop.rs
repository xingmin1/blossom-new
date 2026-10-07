//! 设备停机先终止 DMA，再等待本设备的中断与工作执行尾部。

use alloc::sync::Arc;
use core::time::Duration;

use crate::arch;
use crate::block::disk::{Disk, DiskRef, QueuePhase, QueueState};
use crate::block::{AccessMode, BlockError};
use crate::wait::{WaitNode, Waiter};
use crate::{sched, time, timer};

/// 文件系统占用共享同一个真实磁盘及模式，克隆不增加队列占用数。
#[derive(Clone)]
pub struct FsClaim {
    inner: Arc<FsClaimInner>,
}

struct FsClaimInner {
    disk: DiskRef,
    mode: AccessMode,
    claimed: bool,
}

impl FsClaim {
    /// 借用本次占用的访问模式，不从设备可写能力推导视图权限。
    pub fn mode(&self) -> AccessMode {
        self.inner.mode
    }

    /// 借用实际磁盘拥有者，调用方不得以设备身份值伪造该占用。
    pub fn disk(&self) -> &DiskRef {
        &self.inner.disk
    }
}

impl Disk {
    /// 外部普通 Task 串行停止设备；不得由本设备的完成回调或进度 Work 调用。
    /// 活挂载返回 Busy，无法确认 reset 时保留原设备及全部 DMA 拥有者。
    pub fn stop(&self) -> Result<(), BlockError> {
        sched::assert_switchable();
        let _control = self.control.lock();
        {
            let mut queue = self.queue.lock();
            if queue.mounts != 0 {
                return Err(BlockError::Busy);
            }
            match queue.phase {
                QueuePhase::Stopped => return stop_result(&queue),
                QueuePhase::Quarantined => return Err(BlockError::StopUnconfirmed),
                QueuePhase::Prepared => queue.phase = QueuePhase::DmaStopped,
                QueuePhase::Live => queue.phase = QueuePhase::Draining,
                _ => {}
            }
        }
        self.available.wake_all();
        self.unplug();
        loop {
            self.wait_quiescent_until(|queue| {
                queue.phase != QueuePhase::Draining || accepted_finished(queue)
            });
            let reset = {
                let mut queue = self.queue.lock();
                match queue.phase {
                    QueuePhase::Draining => {
                        assert!(accepted_finished(&queue));
                        queue.phase = QueuePhase::Resetting;
                        queue.pending_notify = false;
                        true
                    }
                    QueuePhase::Quarantined => return Err(BlockError::StopUnconfirmed),
                    QueuePhase::DmaStopped => break,
                    QueuePhase::Faulting | QueuePhase::Resetting => false,
                    _ => panic!("invalid block stop phase"),
                }
            };
            if reset {
                self.reset_claimed()?;
            } else {
                self.fault_reset()?;
            }
        }
        self.wait_quiescent_until(accepted_finished);
        let registration = { self.irq.lock().take() };
        if let Some(registration) = registration {
            registration.unregister_sync();
        }
        if let Some(work) = &self.progress_work {
            work.as_ref().close_sync();
        }
        for slot in &self.slots {
            if let Some(work) = &slot.completion_work {
                work.as_ref().close_sync();
            }
        }
        let device = { self.queue.lock().device.take() };
        drop(device);
        let result = {
            let mut queue = self.queue.lock();
            assert_eq!(queue.phase, QueuePhase::DmaStopped);
            assert!(accepted_finished(&queue));
            queue.phase = QueuePhase::Stopped;
            stop_result(&queue)
        };
        self.quiescent.wake_all();
        result
    }

    /// 由队列 phase 唯一认领故障 reset，不等待外部 stop 正持有的 control 锁。
    /// 进度回调只完成 DMA 停止与失败移交，IRQ 注销及 Work 关闭留给外部 stop。
    pub(super) fn fault_reset(&self) -> Result<(), BlockError> {
        sched::assert_switchable();
        loop {
            let reset = {
                let mut queue = self.queue.lock();
                match queue.phase {
                    QueuePhase::Faulting => {
                        queue.phase = QueuePhase::Resetting;
                        queue.pending_notify = false;
                        true
                    }
                    QueuePhase::Resetting => false,
                    QueuePhase::DmaStopped => {
                        drop(queue);
                        self.fail_submitted_after_reset(BlockError::DeviceFault);
                        return Ok(());
                    }
                    QueuePhase::Quarantined => return Err(BlockError::StopUnconfirmed),
                    QueuePhase::Prepared
                    | QueuePhase::Live
                    | QueuePhase::Draining
                    | QueuePhase::Stopped => return Ok(()),
                }
            };
            if reset {
                self.reset_claimed()?;
                self.fail_submitted_after_reset(BlockError::DeviceFault);
                return Ok(());
            }
            self.wait_quiescent_until(|queue| queue.phase != QueuePhase::Resetting);
        }
    }

    /// IRQ 或任务报告真实设备故障后闭门并排预建进度 Work，不在报告者中睡眠。
    pub(super) fn request_retire(&self, error: BlockError) {
        self.mark_fault(error);
    }

    /// 唯一 Resetting 认领者等通知真尾部，再请求并有限轮询真实 reset 完成。
    /// 状态读回零和完整设备屏障以前不收回任何在途页面。
    fn reset_claimed(&self) -> Result<(), BlockError> {
        self.wait_quiescent_until(|queue| queue.notify_active == 0);
        {
            let mut queue = self.queue.lock();
            assert_eq!(queue.phase, QueuePhase::Resetting);
            queue
                .device
                .as_mut()
                .expect("resetting disk has no device")
                .request_reset();
            #[cfg(feature = "ktest")]
            {
                queue.ktest_stop.reset_requests += 1;
            }
        }
        let deadline = time::now()
            .checked_add(Duration::from_secs(2))
            .expect("block reset deadline overflows monotonic time");
        loop {
            if deadline <= time::now() {
                {
                    let mut queue = self.queue.lock();
                    assert_eq!(queue.phase, QueuePhase::Resetting);
                    queue.phase = QueuePhase::Quarantined;
                    queue.failure = Some(BlockError::StopUnconfirmed);
                }
                self.available.wake_all();
                self.quiescent.wake_all();
                return Err(BlockError::StopUnconfirmed);
            }
            let stopped = {
                let mut queue = self.queue.lock();
                assert_eq!(queue.phase, QueuePhase::Resetting);
                // 测试控制者只拒绝读取/承认确认；真实 reset 写入和单调期限不变。
                #[cfg(feature = "ktest")]
                let withheld = queue.ktest_stop.withhold_confirmation;
                #[cfg(not(feature = "ktest"))]
                let withheld = false;
                let stopped = !withheld
                    && queue
                        .device
                        .as_mut()
                        .expect("resetting disk has no device")
                        .reset_complete();
                if stopped {
                    arch::dma_barrier();
                    queue.phase = QueuePhase::DmaStopped;
                    #[cfg(feature = "ktest")]
                    {
                        queue.ktest_stop.reset_confirmations += 1;
                    }
                }
                stopped
            };
            if stopped {
                self.available.wake_all();
                self.quiescent.wake_all();
                return Ok(());
            }
            timer::sleep(Duration::from_millis(1));
        }
    }

    /// 登记固定栈等待节点后复查队列事实，所有 park 和通知均在队列锁外。
    fn wait_quiescent_until(&self, done: impl Fn(&QueueState) -> bool) {
        if done(&self.queue.lock()) {
            return;
        }
        loop {
            let waiter = Waiter::new();
            let mut node = core::pin::pin!(WaitNode::new(waiter.waker()));
            // SAFETY: self 保活永久源，栈节点固定到 registration 析构，登记不会被遗忘。
            let registration = unsafe { self.quiescent.register(node.as_mut()) };
            if done(&self.queue.lock()) {
                return;
            }
            waiter.park();
            drop(registration);
            drop(waiter);
            if done(&self.queue.lock()) {
                return;
            }
        }
    }

    /// 首次占用在原队列锁下排斥既有 raw 写者；已有占用可与合法 FS 写并存。
    /// 共享 inner 在锁外 fallible 准备，未发表失败不改变队列计数。
    pub fn claim_fs(disk: &DiskRef, mode: AccessMode) -> Result<FsClaim, BlockError> {
        if mode == AccessMode::ReadWrite && disk.limits.readonly {
            return Err(BlockError::ReadOnly);
        }
        let mut owner = Arc::try_new(FsClaimInner {
            disk: disk.clone(),
            mode,
            claimed: false,
        })
        .map_err(|_| BlockError::NoMemory)?;
        {
            let inner = Arc::get_mut(&mut owner).expect("unpublished FS claim is shared");
            let mut queue = disk.queue.lock();
            if queue.phase != QueuePhase::Live
                || (queue.mounts == 0 && (queue.writer_generators != 0 || queue.writing_slots != 0))
            {
                return Err(BlockError::Busy);
            }
            queue.mounts = queue.mounts.checked_add(1).ok_or(BlockError::Busy)?;
            inner.claimed = true;
        }
        Ok(FsClaim { inner: owner })
    }
}

impl Drop for FsClaimInner {
    /// 最后一个真实占用在队列锁下撤销计数，通知及磁盘最后 put 均在锁外。
    fn drop(&mut self) {
        if !self.claimed {
            return;
        }
        {
            let mut queue = self.disk.queue.lock();
            assert!(0 < queue.mounts, "block mount claim underflow");
            queue.mounts -= 1;
        }
        self.disk.available.wake_all();
        self.disk.quiescent.wake_all();
    }
}

/// 生成者计数覆盖父完成回调，slot 计数覆盖每轮资源使用，通知计数覆盖真实门铃尾部。
fn accepted_finished(queue: &QueueState) -> bool {
    queue.generators == 0 && queue.active_slots == 0 && queue.notify_active == 0
}

/// 停机完成保留故障语义；重复 stop 不重复关闭 IRQ 或 Work。
fn stop_result(queue: &QueueState) -> Result<(), BlockError> {
    if queue.failure.is_some() {
        Err(BlockError::DeviceFault)
    } else {
        Ok(())
    }
}
