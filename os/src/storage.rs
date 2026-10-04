//! 一次库存建立有界设备拥有表；未确认停止的设备始终留在原表槽内。

use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};
use spin::Once;

use crate::block::{BlockError, Disk, DiskRef};
use crate::drivers::virtio::{VirtioDevice, VirtioInitError};
use crate::drivers::DeviceInventory;
use crate::mm::dma::DmaError;
use crate::sync::{SleepMutex, SpinLock};
use crate::workqueue::{WorkQueue, WorkQueueConfig};

static STORAGE: Once<Storage> = Once::new();

/// 表容量由一次真实库存固定，停止失败不转移到新的隔离容器。
pub(crate) struct Storage {
    disks: SpinLock<Vec<Option<DiskRef>>>,
    queue: WorkQueue,
    closed: SleepMutex<bool>,
    ready: AtomicBool,
}

impl Storage {
    /// 在唯一普通初始化 Task 中准备全部设备，发表前把原拥有者放进永久表。
    ///
    /// # Safety
    /// inventory 必须来自本次唯一真实发现；调用者独占其中全部块设备，完整强序映射
    /// 永久有效。不能与其他设备 owner 并行准备或重复调用此初始化入口。
    pub(crate) unsafe fn initialize(
        inventory: &DeviceInventory,
    ) -> Result<&'static Self, BlockError> {
        if STORAGE.get().is_some() {
            return Err(BlockError::Busy);
        }
        // SAFETY: 调用者传递一次真实库存、永久映射和全部设备独占准备义务。
        let prepared = unsafe { Self::prepare(inventory) }?;
        let storage = STORAGE.call_once(|| prepared);
        for index in 0..storage.count() {
            let disk = storage
                .owned_disk(index)
                .expect("prepared storage table has an empty slot");
            if let Err(error) = disk.start() {
                // 原表已经保住所有 owner；无法确认停止时 stop_all 不摘原槽。
                return Err(storage.stop_all().err().unwrap_or(error));
            }
        }
        storage.ready.store(true, Ordering::Release);
        Ok(storage)
    }

    /// 锁外一次预备所有 fallible ring、PDU、固定 slot 和共享完成域，不发表硬件。
    ///
    /// # Safety
    /// 调用者须拥有库存全部块设备且保持实际永久强序映射，与 initialize 同一义务。
    unsafe fn prepare(inventory: &DeviceInventory) -> Result<Self, BlockError> {
        let pci_blocks = inventory.pci.devices().iter().filter(|device| {
            device.vendor_id == 0x1af4 && matches!(device.device_id, 0x1001 | 0x1042)
        });
        let count = inventory
            .mmio_blocks
            .len()
            .checked_add(pci_blocks.clone().count())
            .ok_or(BlockError::InvalidRange)?;
        if count == 0 {
            return Err(BlockError::NotReady);
        }
        let mut devices = Vec::new();
        devices
            .try_reserve_exact(count)
            .map_err(|_| BlockError::NoMemory)?;
        for candidate in &inventory.mmio_blocks {
            // SAFETY: 同一真实库存的候选和完整映射已验证，调用者为此槽唯一 owner。
            let device = unsafe { VirtioDevice::prepare_mmio(candidate) }.map_err(init_error)?;
            devices.push(device);
        }
        for device in pci_blocks {
            // SAFETY: 选定真实 BDF 来自本库存，调用者独占，BAR 强序映射永久存活。
            let prepared =
                unsafe { VirtioDevice::prepare_pci(&inventory.pci, device) }.map_err(init_error)?;
            devices.push(prepared);
        }
        // 普通 Work 的默认域额度保持原值；设备控制/槽及 Task Bio 各自预备有限执行资源，
        // 非 slot 的未知可睡回调不借此额度，也不能占满设备完成的准入或 OOM 执行栈。
        let max_active = devices.iter().try_fold(0_usize, |count, device| {
            count
                .checked_add(usize::from(device.info().tag_count) + 1)
                .ok_or(BlockError::InvalidRange)
        })?;
        let queue = WorkQueue::try_new(WorkQueueConfig {
            max_active,
            ordered: false,
            reclaim: true,
        })
        .map_err(|_| BlockError::NoMemory)?;
        let mut disks = Vec::new();
        let result = (|| {
            disks
                .try_reserve_exact(count)
                .map_err(|_| BlockError::NoMemory)?;
            for device in devices {
                disks.push(Some(Disk::prepare(device, &queue)?));
            }
            Ok(())
        })();
        if let Err(error) = result {
            // 这里从未 start；所有已准备 Disk 没有设备 DMA、IRQ action 或异步票。
            for disk in disks.iter().flatten() {
                disk.stop()
                    .expect("unpublished storage owner could not stop");
            }
            drop(disks);
            queue.close_sync();
            return Err(error);
        }
        Ok(Self {
            disks: SpinLock::new(disks),
            queue,
            closed: SleepMutex::new(false),
            ready: AtomicBool::new(false),
        })
    }

    /// 返回一次库存的固定位置数，不把停止后的空槽重新编号。
    pub(crate) fn count(&self) -> usize {
        self.disks.lock().len()
    }

    /// 已完成全表初始化才取得现存 pinned owner，表锁不跨 I/O 或最终析构。
    pub(crate) fn disk(&self, index: usize) -> Option<DiskRef> {
        if !self.ready.load(Ordering::Acquire) {
            return None;
        }
        self.owned_disk(index)
    }

    /// 控制上下文在初始化失败时也能取原 owner，不凭 ready 状态丢失停止责任。
    fn owned_disk(&self, index: usize) -> Option<DiskRef> {
        self.disks.lock().get(index).and_then(Clone::clone)
    }

    /// 外部 Task 逐设备同步停止；不可确认的原槽保留，共享完成域也不提前关闭。
    pub(crate) fn stop_all(&self) -> Result<(), BlockError> {
        let mut closed = self.closed.lock();
        if *closed {
            return Ok(());
        }
        self.ready.store(false, Ordering::Release);
        let mut first_error = None;
        for index in 0..self.count() {
            let Some(disk) = self.owned_disk(index) else {
                continue;
            };
            let result = disk.stop();
            if result == Ok(()) || result == Err(BlockError::DeviceFault) {
                let removed = self.disks.lock()[index].take();
                drop(removed);
            }
            if let Err(error) = result {
                first_error.get_or_insert(error);
            }
            drop(disk);
        }
        let empty = self.disks.lock().iter().all(Option::is_none);
        if empty {
            self.queue.close_sync();
            *closed = true;
        }
        first_error.map_or(Ok(()), Err)
    }
}

/// 保存准备时真实错误日志，既有设备状态快照不由下一次 reset 覆盖。
fn init_error(error: VirtioInitError) -> BlockError {
    crate::early_println!("STORAGE INIT error={:?}", error);
    match error {
        VirtioInitError::NoMemory { .. }
        | VirtioInitError::Dma {
            error: DmaError::NoMemory,
            ..
        } => BlockError::NoMemory,
        VirtioInitError::Protocol(error) => error.into(),
        _ => BlockError::InvalidRange,
    }
}
