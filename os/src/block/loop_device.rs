//! 单个全局 `loop0` 的控制、真实块 I/O 和延迟解绑。
//!
//! 所有 devfs 节点只标记同一个设备身份；每次 open 创建一个 OFD，dup/fork 只复制其 Arc。
//! 绑定同时持有 `DiskRef` 与 `Arc<LoopBacking>`，普通读写和同步均提交给该 Disk 的 Bio。
//! 生命周期为未绑定、已绑定、正在清理及清理失败；CLR/AUTOCLEAR 等最后一个 OFD、挂载租约
//! 与在途 I/O 退出后，由独立管理 Work 等待实际 FsClaim、停止 Disk、同步 backing。
//! 清理失败时保留绑定和原始错误；成功时释放旧资源后才允许重新绑定。
//!
//! 控制睡眠锁串行化绑定、状态修改和清理，最终 backing/Disk 析构在控制锁外完成。
//! OFD 游标锁在数据睡眠锁之外；状态自旋锁只保护计数、标志和引用移动，
//! 不跨分配、uaccess、I/O 或等待。管理 Work 与文件 I/O 使用不同的非 ordered 工作域，
//! 不等待自身 progress/completion Work。

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::convert::TryFrom;
use core::ops::Range;
use core::pin::Pin;
use core::sync::atomic::{AtomicU32, Ordering};
use spin::Once;
use virtio_drivers::BufferDirection;

use crate::block::loop_abi::{
    LoopInfo64, LOOP_CLR_FD, LOOP_GET_STATUS64, LOOP_INFO64_BYTES, LOOP_SET_FD, LOOP_SET_STATUS64,
    LO_FLAGS_AUTOCLEAR, LO_FLAGS_READ_ONLY, LO_NAME_SIZE,
};
use crate::block::loop_backing::LoopBacking;
use crate::block::{
    Bio, BioAllocator, BioMemory, BlkOperation, BlockError, BlockView, Disk, DiskRef, SECTOR_BYTES,
};
use crate::fs::{
    require_read, require_write, set_nonblocking, set_status_flags, stat_path, DirectoryEntry,
    FileIoContext, FileLike, FileRef, FileStat, FsError, InodeRef, Path, ReadBuffer, SeekFrom,
    WriteBuffer, O_ACCMODE, O_DSYNC, O_TRUNC,
};
use crate::klog::{self, Level};
use crate::mm::alloc_pages;
use crate::mm::dma::MappedPages;
use crate::sync::{SleepMutex, SpinLock};
use crate::wait::WaitQueue;
use crate::workqueue::{self, Work, WorkQueue, WorkQueueConfig};

/// 固定 loop0 的块设备号：主号 7，次号 0。
pub(crate) const LOOP0_RDEV: u64 = 7 << u8::BITS;
/// 字节 I/O 与 Bio 之间的最小读改写单位。
const SECTOR_SIZE: usize = SECTOR_BYTES as usize;
/// 全局设备的永久控制对象，不依赖任何 devfs 实例的寿命。
static LOOP0: LoopDevice = LoopDevice {
    control: SleepMutex::new(()),
    data: SleepMutex::new(()),
    state: SpinLock::new(LoopState {
        binding: None,
        openers: 0,
        mounted: false,
        active_io: 0,
        rundown: false,
        cleanup_error: None,
    }),
    quiescent: WaitQueue::new(),
};
/// 仅串行化首次服务准备，准备失败不会发表半成品。
static SERVICE_INIT: SleepMutex<()> = SleepMutex::new(());
/// 首次打开前准备的永久工作域和管理任务。
static SERVICE: Once<LoopService> = Once::new();

/// 设备的控制锁、数据锁与可由 Drop 安全修改的状态。
struct LoopDevice {
    /// 串行化 SET_FD、SET_STATUS64、CLR_FD 与管理任务。
    control: SleepMutex<()>,
    /// 串行化裸设备的读改写及 Flush，避免不同 OFD 覆盖同一扇区。
    data: SleepMutex<()>,
    /// 只在有限自旋锁临界区中更新的绑定与使用计数。
    state: SpinLock<LoopState>,
    /// 在途裸设备 I/O 归零时唤醒清理任务。
    quiescent: WaitQueue,
}

/// 由设备状态自旋锁保护的全部可变生命周期状态。
struct LoopState {
    /// 当前绑定；管理任务结束前一直持有原 Disk 和 backing。
    binding: Option<LoopBinding>,
    /// 独立 OFD 数量，不包括同一 OFD 的 dup/fork 引用数。
    openers: usize,
    /// 唯一挂载候选或已挂载实例是否仍持有租约。
    mounted: bool,
    /// 已取得原 Disk 的裸设备操作数量。
    active_io: usize,
    /// 已禁止新打开、挂载与 I/O，等待或正在执行管理清理。
    rundown: bool,
    /// 清理失败时保留的原始错误；此时 rundown 保持为 true，binding 仍由设备持有。
    cleanup_error: Option<FsError>,
}

/// 一次完整绑定持有的资源及查询状态。
struct LoopBinding {
    /// 与读写、挂载及所有 Bio 相同的块设备实例。
    disk: DiskRef,
    /// 即使 stop 已释放 Backend，仍保活 backing 直到最终同步结束。
    backing: Arc<LoopBacking>,
    /// 实际 READ_ONLY 与用户设置的 AUTOCLEAR 位。
    flags: u32,
    /// 已保证最后一个字节为 NUL 的诊断文件名。
    file_name: [u8; LO_NAME_SIZE],
}

/// 两个相互独立的永久工作域与预建管理任务。
struct LoopService {
    /// 文件后端的 progress/completion 工作域，不与物理盘共用。
    io_queue: WorkQueue,
    /// 管理 Work 的独立工作域，随永久服务共同保活。
    _management_queue: WorkQueue,
    /// 构造期预分配执行线程，最终关闭不需要临时创建任务。
    cleanup: Pin<Arc<Work>>,
}

/// 一次真实 open 创建的 OFD；最后一个 Arc 释放时才归还一个 opener。
struct LoopFile {
    /// 实际打开的 devfs 路径，用于节点属性与挂载保活。
    path: Path,
    /// 与同一 OFD 的全部描述符共享的状态标志。
    flags: AtomicU32,
    /// 顺序读写与 seek 共享的字节偏移。
    cursor: SleepMutex<u64>,
}

/// 一个挂载候选或卷实例独占的 loop 使用租约。
///
/// 卷必须先销毁 fatfs/FatIo 及其 FsClaim，再在可睡眠任务上下文释放本租约。
/// 租约应作为卷的最后一个字段，不能存入 Bio 或 progress/completion 回调。
/// 最后释放可能等待独立管理 Work 完成，但不在 Drop 中执行文件 I/O。
pub(crate) struct LoopMountLease {
    /// 保活租约取得时的原 Disk，不能从后续绑定重新查找设备。
    disk: DiskRef,
}

/// 已准入的一次裸设备操作，固定其整个调用期间的 Disk 身份。
struct LoopIo {
    /// 本次调用实际提交 Bio 的设备。
    disk: DiskRef,
}

/// 单次文件操作重复使用的一个独占扇区页和 Bio 容器。
struct SectorBuffer {
    /// 每次 Bio 完成后收回的同一页映射，传输期间暂时为空。
    memory: BioMemory,
}

impl LoopState {
    /// 最后使用者退出时关闭新请求准入，并告知调用者需要排入清理工作。
    fn request_last_cleanup(&mut self) -> bool {
        let autoclear = self
            .binding
            .as_ref()
            .is_some_and(|binding| binding.flags & LO_FLAGS_AUTOCLEAR != 0);
        if autoclear && self.openers == 0 && !self.mounted {
            self.rundown = true;
            return true;
        }
        false
    }
}

/// 在首次 open 的可睡眠上下文准备两个工作域和管理 Work；失败时同步关闭临时工作域。
fn service() -> Result<&'static LoopService, FsError> {
    if let Some(service) = SERVICE.get() {
        return Ok(service);
    }
    let _init = SERVICE_INIT.lock();
    if let Some(service) = SERVICE.get() {
        return Ok(service);
    }
    let config = WorkQueueConfig {
        max_active: 1,
        ordered: false,
        reclaim: false,
    };
    let io_queue = WorkQueue::try_new(config).map_err(|_| FsError::NoMemory)?;
    let management_queue = match WorkQueue::try_new(config) {
        Ok(queue) => queue,
        Err(_) => {
            io_queue.close_sync();
            return Err(FsError::NoMemory);
        }
    };
    let cleanup = match Work::try_new_reserved(&management_queue, 0, cleanup_callback, 0) {
        Ok(work) => work,
        Err(_) => {
            management_queue.close_sync();
            io_queue.close_sync();
            return Err(FsError::NoMemory);
        }
    };
    Ok(SERVICE.call_once(|| LoopService {
        io_queue,
        _management_queue: management_queue,
        cleanup,
    }))
}

/// 打开固定 loop0；未绑定也允许配置，清理失败则返回保留的错误，不增加 opener。
pub(crate) fn open(path: Path, flags: u32, _caller: FileIoContext<'_>) -> Result<FileRef, FsError> {
    if flags & O_ACCMODE == O_ACCMODE {
        return Err(FsError::InvalidOperation);
    }
    service()?;
    {
        let mut state = LOOP0.state.lock();
        if state.rundown {
            return Err(state.cleanup_error.unwrap_or(FsError::NoDeviceAddress));
        }
        if let Some(binding) = &state.binding {
            if flags & O_ACCMODE != 0 && binding.flags & LO_FLAGS_READ_ONLY != 0 {
                return Err(FsError::ReadOnly);
            }
        }
        state.openers = state.openers.checked_add(1).ok_or(FsError::NoMemory)?;
    }
    // Arc 分配失败也会析构该值，由同一个 Drop 归还已经取得的 opener。
    Ok(Arc::try_new(LoopFile {
        path,
        flags: AtomicU32::new(flags & !O_TRUNC),
        cursor: SleepMutex::new(0),
    })
    .map_err(|_| FsError::NoMemory)?)
}

/// 取得唯一挂载租约；清理失败返回原始错误，候选准备失败后仍可重新尝试挂载。
pub(crate) fn acquire_mount() -> Result<LoopMountLease, FsError> {
    let mut state = LOOP0.state.lock();
    if state.rundown {
        return Err(state.cleanup_error.unwrap_or(FsError::NoDeviceAddress));
    }
    let binding = state.binding.as_ref().ok_or(FsError::NoDeviceAddress)?;
    if state.mounted {
        return Err(FsError::Busy);
    }
    let disk = binding.disk.clone();
    state.mounted = true;
    Ok(LoopMountLease { disk })
}

impl LoopMountLease {
    /// 返回租约保活的原 Disk 整盘视图；调用者必须让租约活过所有由此创建的卷对象。
    pub(crate) fn view(&self) -> BlockView {
        BlockView::whole(self.disk.clone())
    }
}

impl Drop for LoopMountLease {
    /// 归还唯一挂载名额；最后使用者在可睡眠上下文等待管理 Work 完成清理。
    fn drop(&mut self) {
        let cleanup = {
            let mut state = LOOP0.state.lock();
            assert!(state.mounted, "loop mount lease released twice");
            state.mounted = false;
            state.request_last_cleanup()
        };
        finish_last_release(cleanup);
    }
}

impl Drop for LoopFile {
    /// 在表锁外归还一次 OFD 打开计数；最终关闭只等待管理 Work，不直接执行 backing I/O。
    fn drop(&mut self) {
        let cleanup = {
            let mut state = LOOP0.state.lock();
            assert!(0 < state.openers, "loop opener count underflow");
            state.openers -= 1;
            state.request_last_cleanup()
        };
        finish_last_release(cleanup);
    }
}

/// 在最终释放者的可睡眠上下文等待实际管理任务结束，使连续的解绑、绑定具有确定顺序。
fn finish_last_release(cleanup: bool) {
    if cleanup {
        let work = &SERVICE.get().expect("loop service missing").cleanup;
        work.as_ref().queue().expect("loop management work closed");
        work.as_ref().flush();
    }
}

/// 在独立管理线程等待 I/O、FsClaim 与 Disk 工作结束；失败保留绑定，成功后锁外释放旧资源。
fn cleanup_callback(_work: Pin<&Work>) {
    let control = LOOP0.control.lock();
    let (disk, backing) = {
        let state = LOOP0.state.lock();
        assert!(state.rundown && state.openers == 0 && !state.mounted);
        let binding = state.binding.as_ref().expect("loop cleanup has no binding");
        (binding.disk.clone(), binding.backing.clone())
    };
    workqueue::wait_until(&LOOP0.quiescent, || LOOP0.state.lock().active_io == 0);
    disk.wait_no_fs_claims();
    let result = disk
        .stop()
        .map_err(FsError::from)
        .and_then(|()| backing.flush());
    let retired = {
        let mut state = LOOP0.state.lock();
        assert!(state.rundown && state.openers == 0 && !state.mounted && state.active_io == 0);
        if let Err(error) = result {
            state.cleanup_error = Some(error);
            None
        } else {
            state.binding.take()
        }
    };
    // rundown 在控制锁释放后继续拒绝新绑定，最后一个文件引用的析构可以取得其他睡眠锁。
    drop(control);
    drop(retired);
    drop(backing);
    drop(disk);
    if let Err(error) = result {
        klog::write(
            Level::Error,
            format_args!("loop0 cleanup failed: {:?}", error),
        );
        return;
    }
    let _control = LOOP0.control.lock();
    let mut state = LOOP0.state.lock();
    assert!(state.rundown && state.binding.is_none() && state.cleanup_error.is_none());
    state.rundown = false;
}

/// 在状态锁内取得当前 Disk 并登记 I/O；后续的分配、uaccess 和 Bio 提交均在锁外执行。
fn acquire_io(write: bool) -> Result<LoopIo, FsError> {
    let mut state = LOOP0.state.lock();
    if state.rundown {
        return Err(FsError::NoDeviceAddress);
    }
    let binding = state.binding.as_ref().ok_or(FsError::NoDeviceAddress)?;
    if write && binding.flags & LO_FLAGS_READ_ONLY != 0 {
        return Err(FsError::ReadOnly);
    }
    let disk = binding.disk.clone();
    state.active_io = state.active_io.checked_add(1).ok_or(FsError::NoMemory)?;
    Ok(LoopIo { disk })
}

impl Drop for LoopIo {
    /// 结束裸 I/O 使用期；这里只通知管理任务，不执行睡眠清理。
    fn drop(&mut self) {
        {
            let mut state = LOOP0.state.lock();
            assert!(0 < state.active_io, "loop I/O count underflow");
            state.active_io -= 1;
        }
        LOOP0.quiescent.wake_all();
    }
}

/// 检查只读 loop OFD 修改 SET_STATUS64 所需的管理身份；其他命令沿设备打开权限准入。
fn require_control(caller: FileIoContext<'_>) -> Result<(), FsError> {
    if let FileIoContext::User(user) = caller {
        let runtime = user.runtime().ok_or(FsError::InvalidOperation)?;
        if runtime.credentials.uid != 0 {
            return Err(FsError::NotPermitted);
        }
    }
    Ok(())
}

/// 从当前调用者的文件表取得强引用；lookup 返回后表锁已经释放，后续可以睡眠。
fn backing_fd(argument: usize, caller: FileIoContext<'_>) -> Result<FileRef, FsError> {
    let FileIoContext::User(user) = caller else {
        return Err(FsError::BadFd);
    };
    let fd = i32::try_from(argument).map_err(|_| FsError::BadFd)? as usize;
    let runtime = user.runtime().ok_or(FsError::InvalidOperation)?;
    runtime.files.lookup(fd)
}

impl LoopFile {
    /// 将已脱离文件表锁的真实文件构造为 backing，并在全部准备成功后原子发表绑定。
    fn set_fd(&self, file: FileRef) -> Result<(), FsError> {
        let _control = LOOP0.control.lock();
        {
            let state = LOOP0.state.lock();
            if state.binding.is_some() || state.rundown {
                return Err(FsError::Busy);
            }
        }
        let backing = Arc::try_new(LoopBacking::new(file, self.flags() & O_ACCMODE != 0)?)
            .map_err(|_| FsError::NoMemory)?;
        let disk = Disk::prepare_file(backing.clone(), &service()?.io_queue)?;
        if let Err(error) = disk.start() {
            let stopped = disk.stop();
            if matches!(stopped, Err(BlockError::Busy | BlockError::StopUnconfirmed)) {
                let mut state = LOOP0.state.lock();
                state.binding = Some(LoopBinding {
                    disk,
                    backing,
                    flags: LO_FLAGS_AUTOCLEAR,
                    file_name: [0; LO_NAME_SIZE],
                });
                state.rundown = true;
            }
            return Err(FsError::from(stopped.err().unwrap_or(error)));
        }
        let flags = if backing.readonly() {
            LO_FLAGS_READ_ONLY
        } else {
            0
        };
        LOOP0.state.lock().binding = Some(LoopBinding {
            disk,
            backing,
            flags,
            file_name: [0; LO_NAME_SIZE],
        });
        Ok(())
    }

    /// 标记 AUTOCLEAR；只有本 OFD 且没有挂载时提前关闭准入，资源仍等实际最后关闭。
    fn clear_fd(&self) -> Result<(), FsError> {
        let _control = LOOP0.control.lock();
        let mut state = LOOP0.state.lock();
        if state.rundown {
            return Err(FsError::NoDeviceAddress);
        }
        let binding = state.binding.as_mut().ok_or(FsError::NoDeviceAddress)?;
        binding.flags |= LO_FLAGS_AUTOCLEAR;
        if state.openers == 1 && !state.mounted {
            state.rundown = true;
        }
        Ok(())
    }

    /// 在设备锁外取得 backing 属性，用真实身份和当次状态构造 GET_STATUS64。
    fn status(&self) -> Result<LoopInfo64, FsError> {
        let (backing, flags, file_name) = {
            let state = LOOP0.state.lock();
            if state.rundown {
                return Err(FsError::NoDeviceAddress);
            }
            let binding = state.binding.as_ref().ok_or(FsError::NoDeviceAddress)?;
            (binding.backing.clone(), binding.flags, binding.file_name)
        };
        Ok(LoopInfo64::status(backing.backing_stat(), flags, file_name))
    }

    /// 只更新诊断名与 AUTOCLEAR；READ_ONLY 始终保留实际能力，不能由用户状态升级写权限。
    fn set_status(&self, info: LoopInfo64) -> Result<(), FsError> {
        let _control = LOOP0.control.lock();
        let mut state = LOOP0.state.lock();
        if state.rundown {
            return Err(FsError::NoDeviceAddress);
        }
        let binding = state.binding.as_mut().ok_or(FsError::NoDeviceAddress)?;
        info.check_supported()?;
        binding.file_name = info.file_name;
        binding.file_name[LO_NAME_SIZE - 1] = 0;
        binding.flags = (binding.flags & LO_FLAGS_READ_ONLY) | (info.flags & LO_FLAGS_AUTOCLEAR);
        Ok(())
    }

    /// 按扇区读取并逐段交付字节，失败时保留已经成功交付的前缀。
    fn read_bytes(&self, offset: u64, target: &mut ReadBuffer<'_>) -> Result<usize, FsError> {
        require_read(self.flags())?;
        let _data = LOOP0.data.lock();
        let io = acquire_io(false)?;
        let capacity = io.disk.limits().capacity_sectors * SECTOR_BYTES;
        let start = target.copied();
        let mut position = offset;
        if (i64::MAX as u64) < position {
            return Err(FsError::InvalidOperation);
        }
        if capacity <= position || target.len() == target.copied() {
            return Ok(0);
        }
        let mut buffer = SectorBuffer::new(BufferDirection::DeviceToDriver)?;
        while target.copied() < target.len() && position < capacity {
            let within = (position % SECTOR_BYTES) as usize;
            let count = (SECTOR_SIZE - within)
                .min(target.len() - target.copied())
                .min((capacity - position) as usize);
            let result = buffer
                .read(&io.disk, position / SECTOR_BYTES)
                .and_then(|bytes| target.push(&bytes[within..within + count]).map(|_| ()));
            if let Err(error) = result {
                return prefix_or_error(target.copied() - start, error);
            }
            position += count as u64;
        }
        Ok(target.copied() - start)
    }

    /// 对非整扇区执行真实读改写，完整扇区直接写入；源访问和 Bio 等待均不持有自旋锁。
    fn write_bytes(&self, offset: u64, source: &mut WriteBuffer<'_>) -> Result<usize, FsError> {
        require_write(self.flags())?;
        let _data = LOOP0.data.lock();
        let io = acquire_io(true)?;
        if (i64::MAX as u64) < offset {
            return Err(FsError::InvalidOperation);
        }
        let capacity = io.disk.limits().capacity_sectors * SECTOR_BYTES;
        if source.copied() == source.len() {
            return Ok(0);
        }
        if capacity <= offset {
            return Err(FsError::NoSpace);
        }
        let mut buffer = SectorBuffer::new(BufferDirection::DriverToDevice)?;
        let mut accepted = 0;
        let mut position = offset;
        while source.copied() < source.len() && position < capacity {
            let within = (position % SECTOR_BYTES) as usize;
            let count = (SECTOR_SIZE - within)
                .min(source.len() - source.copied())
                .min((capacity - position) as usize);
            let mut bytes = [0; SECTOR_SIZE];
            let result = (|| {
                let count = source.pull(&mut bytes[..count])?;
                if count == 0 {
                    return Err(FsError::BadAddress);
                }
                let sector = position / SECTOR_BYTES;
                if within != 0 || count != SECTOR_SIZE {
                    let mut original = [0; SECTOR_SIZE];
                    io.disk
                        .read_exact_at(sector * SECTOR_BYTES, &mut original)?;
                    original[within..within + count].copy_from_slice(&bytes[..count]);
                    buffer.write(&io.disk, sector, &original)?;
                } else {
                    buffer.write(&io.disk, sector, &bytes)?;
                }
                Ok(count)
            })();
            match result {
                Ok(count) => {
                    accepted += count;
                    position += count as u64;
                }
                Err(error) => return prefix_or_error(accepted, error),
            }
        }
        Ok(accepted)
    }

    /// 在游标与裸数据锁已经释放后按同步打开标志执行真实块 Flush。
    fn finish_write(&self, count: usize) -> Result<usize, FsError> {
        if 0 < count && self.flags() & O_DSYNC != 0 {
            self.sync_range(0..u64::MAX, true)?;
        }
        Ok(count)
    }
}

/// 已有成功字节时保留真实前缀，否则返回原始错误。
fn prefix_or_error(count: usize, error: FsError) -> Result<usize, FsError> {
    if count == 0 {
        Err(error)
    } else {
        Ok(count)
    }
}

impl SectorBuffer {
    /// 在队列锁外为本次字节操作准备一个独占物理页和可反复回收的单段容器。
    fn new(direction: BufferDirection) -> Result<Self, FsError> {
        let mut memory = Vec::new_in(BioAllocator::Heap);
        memory.try_reserve_exact(1).map_err(|_| FsError::NoMemory)?;
        let pages = alloc_pages(0, false).ok_or(FsError::NoMemory)?;
        memory.push(MappedPages::new(pages, 0, SECTOR_SIZE, direction).map_err(BlockError::from)?);
        Ok(Self { memory })
    }

    /// 将当前扇区提交为一个 Bio，等待完成并收回原页；短完成始终报告错误。
    fn transfer(
        &mut self,
        disk: &DiskRef,
        operation: BlkOperation,
        sector: u64,
    ) -> Result<(), FsError> {
        let memory = core::mem::replace(&mut self.memory, Vec::new_in(BioAllocator::Heap));
        // SAFETY:
        // 1. memory 独占本对象分配的页；Write 之前由 write 初始化全部 512 字节，Read 不读取旧字节。
        // 2. 本函数没有完成回调；移交至 wait 和 take_memory 完成期间不保留任何页内容引用。
        let bio = unsafe { Bio::try_new(disk, operation, sector, memory, 0, None) }?;
        let completion = bio.completion();
        disk.submit_bio(bio);
        let result = completion.wait();
        self.memory = completion.take_memory()?;
        if let Some(error) = result.error {
            return Err(FsError::from(error));
        }
        if result.completed != SECTOR_SIZE {
            return Err(FsError::from(BlockError::ShortIo));
        }
        Ok(())
    }

    /// 完整读取一个扇区后借用本对象独占的已初始化页内容。
    fn read(&mut self, disk: &DiskRef, sector: u64) -> Result<&[u8], FsError> {
        self.transfer(disk, BlkOperation::Read, sector)?;
        // SAFETY:
        // 1. transfer 已确认全部 512 字节成功读取，wait 和 take_memory 已收回设备访问权。
        // 2. memory 仅属于当前 SectorBuffer，借用期间没有后续传输或并发写入。
        Ok(unsafe { self.memory[0].bytes() })
    }

    /// 初始化完整扇区页后写入同一 Disk，不直接调用 backing 文件替代块路径。
    fn write(
        &mut self,
        disk: &DiskRef,
        sector: u64,
        bytes: &[u8; SECTOR_SIZE],
    ) -> Result<(), FsError> {
        for (target, byte) in self.memory[0].bytes_mut().iter_mut().zip(bytes) {
            target.write(*byte);
        }
        self.transfer(disk, BlkOperation::Write, sector)
    }
}

/// 通过实际 Disk/Bio 提交 Flush，保持与前序读写相同的错误及完成语义。
fn flush_disk(disk: &DiskRef) -> Result<(), FsError> {
    let memory = Vec::new_in(BioAllocator::Heap);
    // SAFETY:
    // 1. Flush 不携带物理页，不存在初始化或内容别名要求。
    // 2. 请求无回调，调用者持有 DiskRef 并等待完整完成后才返回。
    let bio = unsafe { Bio::try_new(disk, BlkOperation::Flush, 0, memory, 0, None) }?;
    let completion = bio.completion();
    disk.submit_bio(bio);
    let result = completion.wait();
    drop(completion.take_memory()?);
    result
        .error
        .map_or(Ok(()), |error| Err(FsError::from(error)))
}

impl FileLike for LoopFile {
    /// loop 块设备支持不改变共享游标的定位 I/O。
    fn positioned_io(&self) -> bool {
        true
    }

    /// 持 OFD 游标睡眠锁顺序读取，只按实际交付字节推进位置。
    fn read_into(
        &self,
        target: &mut ReadBuffer<'_>,
        _caller: FileIoContext<'_>,
    ) -> Result<usize, FsError> {
        let mut cursor = self.cursor.lock();
        let count = self.read_bytes(*cursor, target)?;
        *cursor += count as u64;
        Ok(count)
    }

    /// 定位读取实际块内容，不获取或改变 OFD 游标。
    fn read_at_into(
        &self,
        offset: u64,
        target: &mut ReadBuffer<'_>,
        _caller: FileIoContext<'_>,
    ) -> Result<usize, FsError> {
        self.read_bytes(offset, target)
    }

    /// 保活实际 devfs 节点所在的挂载路径。
    fn path(&self) -> Option<&Path> {
        Some(&self.path)
    }

    /// 块设备没有普通文件 mmap 的 inode 映射能力。
    fn mapping_inode(&self) -> Option<&InodeRef> {
        None
    }

    /// 分派四个旧式 loop 命令；SET_FD/CLR_FD 沿打开权限准入，用户指针复制在设备锁外。
    fn ioctl(
        &self,
        command: u32,
        argument: usize,
        caller: FileIoContext<'_>,
    ) -> Result<usize, FsError> {
        match command {
            LOOP_SET_FD => {
                let file = backing_fd(argument, caller)?;
                self.set_fd(file)?;
            }
            LOOP_CLR_FD => {
                self.clear_fd()?;
            }
            LOOP_GET_STATUS64 => {
                let bytes = self.status()?.to_bytes();
                let FileIoContext::User(user) = caller else {
                    return Err(FsError::BadAddress);
                };
                let mm = user.mm().ok_or(FsError::BadAddress)?;
                let progress = mm.copyout(argument, &bytes);
                if progress.error.is_some() || progress.copied != bytes.len() {
                    return Err(FsError::BadAddress);
                }
            }
            LOOP_SET_STATUS64 => {
                if self.flags() & O_ACCMODE == 0 {
                    require_control(caller)?;
                }
                let FileIoContext::User(user) = caller else {
                    return Err(FsError::BadAddress);
                };
                let mm = user.mm().ok_or(FsError::BadAddress)?;
                let mut bytes = [0; LOOP_INFO64_BYTES];
                let progress = mm.copyin(&mut bytes, argument);
                if progress.error.is_some() || progress.copied != bytes.len() {
                    return Err(FsError::BadAddress);
                }
                self.set_status(LoopInfo64::from_bytes(&bytes))?;
            }
            _ => return Err(FsError::NotTty),
        }
        Ok(0)
    }

    /// 返回同一 OFD 共享的访问模式和状态标志。
    fn flags(&self) -> u32 {
        self.flags.load(Ordering::Relaxed)
    }

    /// 沿公共 fcntl 规则修改可变标志，不修改绑定能力。
    fn set_status_flags(&self, raw: u32) -> Result<(), FsError> {
        set_status_flags(&self.flags, raw)
    }

    /// 原子切换本 OFD 的非阻塞标志，不改变块设备的完成协议。
    fn set_nonblocking(&self, enabled: bool) -> Result<(), FsError> {
        set_nonblocking(&self.flags, enabled)
    }

    /// 固定块设备的容量不能通过文件 truncate 改变。
    fn truncate(&self, _new_size: u64) -> Result<(), FsError> {
        Err(FsError::InvalidOperation)
    }

    /// 同步当前真实 Disk；只读 OFD 也可请求同步，但未绑定设备不能成功。
    fn sync_range(&self, range: Range<u64>, _data_only: bool) -> Result<(), FsError> {
        if range.end < range.start {
            return Err(FsError::InvalidOperation);
        }
        let _data = LOOP0.data.lock();
        let io = acquire_io(false)?;
        flush_disk(&io.disk)
    }

    /// 在当前设备容量内改变 OFD 字节游标，非法偏移保留原位置。
    fn seek(&self, from: SeekFrom) -> Result<u64, FsError> {
        let mut cursor = self.cursor.lock();
        let capacity = {
            let state = LOOP0.state.lock();
            if state.rundown {
                return Err(FsError::NoDeviceAddress);
            }
            state
                .binding
                .as_ref()
                .ok_or(FsError::NoDeviceAddress)?
                .disk
                .limits()
                .capacity_sectors
                * SECTOR_BYTES
        };
        let position = match from {
            SeekFrom::Start(position) => Some(position),
            SeekFrom::Current(offset) => cursor.checked_add_signed(offset),
            SeekFrom::End(offset) => capacity.checked_add_signed(offset),
        }
        .ok_or(FsError::InvalidOperation)?;
        if capacity < position || (i64::MAX as u64) < position {
            return Err(FsError::InvalidOperation);
        }
        *cursor = position;
        Ok(position)
    }

    /// 查询实际 S_IFBLK 节点及其固定 rdev，不把 backing 属性替代设备节点属性。
    fn stat(&self) -> Result<FileStat, FsError> {
        stat_path(&self.path)
    }

    /// 块设备不能作为目录枚举。
    fn readdir(&self) -> Result<Option<DirectoryEntry>, FsError> {
        Err(FsError::NotDirectory)
    }

    /// 块设备不能向 getdents 提供目录项。
    fn for_each_dirent(
        &self,
        _emit: &mut dyn FnMut(&DirectoryEntry) -> Result<bool, FsError>,
    ) -> Result<(), FsError> {
        Err(FsError::NotDirectory)
    }

    /// 顺序写入并提交实际接受的位置，释放游标后按同步打开位执行 Flush。
    fn write_from(
        &self,
        source: &mut WriteBuffer<'_>,
        _caller: FileIoContext<'_>,
    ) -> Result<usize, FsError> {
        let mut cursor = self.cursor.lock();
        let count = self.write_bytes(*cursor, source)?;
        *cursor += count as u64;
        drop(cursor);
        self.finish_write(count)
    }

    /// 定位写入不改变 OFD 游标，仍保留同步打开位的真实 Flush 行为。
    fn write_at_from(
        &self,
        offset: u64,
        source: &mut WriteBuffer<'_>,
        _caller: FileIoContext<'_>,
    ) -> Result<usize, FsError> {
        let count = self.write_bytes(offset, source)?;
        self.finish_write(count)
    }
}
