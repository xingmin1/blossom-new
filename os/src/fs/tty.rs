//! 唯一 UART 的常驻行规程端点；输入、回显与输出各有固定有界队列。

pub mod ntty;
pub mod termios;

use alloc::sync::Arc;
use core::ops::Range;
use core::pin::Pin;
use core::sync::atomic::{AtomicU32, Ordering};
use core::time::Duration;
use spin::Once;

use crate::console::{RxSample, SerialSnapshot};
use crate::fs::epoll::WatchHead;
use crate::fs::file::{
    FileIoContext, FileLike, FileRef, FileStat, ReadBuffer, SeekFrom, WriteBuffer,
};
use crate::fs::memfs::MemorySpecial;
use crate::fs::poll::{PollContext, PollMask, PollNotify, PollTable, PollWaitHead, Pollable};
use crate::fs::{DirectoryEntry, FsError, InodeRef, Path};
use crate::sync::{SleepMutex, SpinLock};
use crate::task::UserThread;
use crate::time::{self, MonotonicInstant};
use crate::wait::WaitResult;
use crate::workqueue::{Work, WorkQueue, WorkQueueConfig};

use ntty::{NttyState, SEGMENT_BYTES};
use termios::{Termios, VMIN, VTIME};

/// TCGETS 命令。
const TCGETS: u32 = 0x5401;
/// TCSETS 命令。
const TCSETS: u32 = 0x5402;
/// TCSETSW 命令。
const TCSETSW: u32 = 0x5403;
/// TCSETSF 命令。
const TCSETSF: u32 = 0x5404;
/// TCSBRK 命令。
const TCSBRK: u32 = 0x5409;
/// TCXONC 命令。
const TCXONC: u32 = 0x540a;
/// TCFLSH 命令。
const TCFLSH: u32 = 0x540b;
/// O_NONBLOCK 状态位。
const O_NONBLOCK: u32 = 0o4000;
/// 工作每轮最多消费的软件采样数。
const WORK_RX_BUDGET: usize = 64;
/// 写出片段固定 512 字节。
const WRITE_SEGMENT: usize = 512;

/// 常驻端点。
struct Endpoint {
    /// 行规程短锁状态，指向永久静态对象。
    state: &'static SpinLock<NttyState>,
    /// 一次逻辑 read 的串行锁。
    reader: SleepMutex<()>,
    /// 用户输出与有序 setter 的串行锁。
    writer: SleepMutex<()>,
    /// 短复制与输入 flush 串行锁。
    copy: SleepMutex<()>,
    /// 读持续源。
    read_head: Arc<PollWaitHead>,
    /// 写持续源。
    write_head: Arc<PollWaitHead>,
}

/// 静态服务。
struct Service {
    /// 常驻端点。
    endpoint: Endpoint,
    /// TTY 工作域。
    _queue: WorkQueue,
    /// TTY 固定工作。
    work: Pin<Arc<Work>>,
}

/// 唯一行规程状态的永久静态对象。
static STATE: SpinLock<NttyState> = SpinLock::new(NttyState::new());

static SERVICE: Once<Service> = Once::new();

/// 返回已发表的常驻服务。
fn service() -> &'static Service {
    SERVICE.get().expect("tty service is not initialized")
}

/// 返回常驻端点。
fn endpoint() -> &'static Endpoint {
    &service().endpoint
}

/// 一次逻辑 read 的串行 guard，析构先释锁再通知读源。
struct ReaderGuard {
    /// 底层串行锁 guard。
    inner: Option<crate::sync::SleepMutexGuard<'static, ()>>,
}

impl Drop for ReaderGuard {
    /// 先释放串行锁，再通知读源唤醒竞争者。
    fn drop(&mut self) {
        if let Some(guard) = self.inner.take() {
            drop(guard);
            endpoint()
                .read_head
                .notify(PollMask::POLLIN | PollMask::POLLRDNORM);
        }
    }
}

/// 用户输出串行 guard，析构先释锁再通知写源。
struct WriterGuard {
    /// 底层串行锁 guard。
    inner: Option<crate::sync::SleepMutexGuard<'static, ()>>,
}

impl Drop for WriterGuard {
    /// 先释放串行锁，再通知写源唤醒竞争者。
    fn drop(&mut self) {
        if let Some(guard) = self.inner.take() {
            drop(guard);
            endpoint()
                .write_head
                .notify(PollMask::POLLOUT | PollMask::POLLWRNORM);
        }
    }
}

/// 在原工作队列初始化后准备常驻端点、域与 CPU0 工作。
pub fn initialize_service() -> Result<(), core::alloc::AllocError> {
    let read_head = Arc::try_new(PollWaitHead::new())?;
    let write_head = Arc::try_new(PollWaitHead::new())?;
    let queue = WorkQueue::try_new(WorkQueueConfig {
        max_active: 1,
        ordered: false,
        reclaim: false,
    })?;
    let work = Work::try_new(&queue, 0, run_work, 0)?;
    SERVICE.call_once(move || Service {
        endpoint: Endpoint {
            state: &STATE,
            reader: SleepMutex::new(()),
            writer: SleepMutex::new(()),
            copy: SleepMutex::new(()),
            read_head,
            write_head,
        },
        _queue: queue,
        work,
    });
    Ok(())
}

/// 接收中断在端口锁外提示行规程消费软件环。
pub fn schedule_receive() {
    if let Some(service) = SERVICE.get() {
        let _ = service.work.as_ref().queue();
    }
}

/// 发送就绪或成功写入在端口锁外提示行规程复查实际发送队列。
pub fn schedule_transmit() {
    let Some(service) = SERVICE.get() else {
        return;
    };
    if service.endpoint.state.lock().output_empty() {
        return;
    }
    let _ = service.work.as_ref().queue();
}

/// TTY 工作回调：批处理 RX、展开回显，再非阻塞泵实际 TX。
fn run_work(_work: Pin<&Work>) {
    let service = service();
    let _copy = service.endpoint.copy.lock();
    let mut samples = [RxSample { byte: 0, status: 0 }; WORK_RX_BUDGET];
    let count = crate::console::read_received(&mut samples);
    let mut read_changed = false;
    let mut write_changed = false;
    let mut progressed = false;
    {
        let mut state = service.endpoint.state.lock();
        if 0 < count {
            progressed = true;
        }
        if state.opens == 0 {
            // 无打开实例时丢弃新采样，已接受回显仍继续展开。
        } else {
            for sample in samples[..count].iter() {
                let (read, write) = state.receive_sample(*sample);
                read_changed |= read;
                write_changed |= write;
            }
        }
        if state.echo.len() != 0 {
            write_changed = true;
        }
        let echo_before = state.echo.len();
        let output_before = state.output.len();
        let reprint_before = state.pending_reprint.as_ref().map(|p| p.done);
        let echo_pending = state.process_echo();
        let reprint_after = state.pending_reprint.as_ref().map(|p| p.done);
        if state.echo.len() != echo_before
            || state.output.len() != output_before
            || reprint_before != reprint_after
        {
            progressed = true;
        }
        if echo_pending {
            write_changed = true;
        }
    }
    drop(_copy);
    let pumped = pump_transmit(service, false);
    if pumped {
        progressed = true;
    }
    if read_changed {
        service
            .endpoint
            .read_head
            .notify(PollMask::POLLIN | PollMask::POLLRDNORM);
    }
    if write_changed {
        service
            .endpoint
            .write_head
            .notify(PollMask::POLLOUT | PollMask::POLLWRNORM);
    }
    // 只在有真实进度且可继续时重提；STOP/背压依 START RX/THRE 通知。
    if progressed {
        let state = service.endpoint.state.lock();
        let more_rx = 0 < count && count == WORK_RX_BUDGET;
        let more_echo = !state.echo.is_empty();
        let more_tx =
            !state.output_empty() && !state.stopped && !state.tco_stopped && !state.flusho();
        drop(state);
        if more_rx || more_echo || more_tx {
            let _ = service.work.as_ref().queue();
        }
    }
}

/// 非阻塞泵实际 TX，不等用户 read、不等 Ctrl-Q、不取 writer 锁，返回是否真实发送。
/// 锁序：全程持 copy，短取 state peek 后释放 state，仍持 copy 调 try_write
/// （它只取有限 PORT，锁外 schedule_transmit 取 state 不反取 copy），成功后
/// 短取 state 消费队首；flush/settings 同样持 copy 才可移除队首。
/// 端口满时 arm 真实 THRE 中断，已就绪立即重提，不定期轮询。
fn pump_transmit(service: &'static Service, force: bool) -> bool {
    let _copy = service.endpoint.copy.lock();
    let mut pumped = false;
    loop {
        enum Peek {
            /// 优先字符。
            Xchar(u8),
            /// 输出队首字节。
            Output(u8),
            /// 丢弃中或流控中，无可发送字节。
            None,
        }
        let peeked = {
            let mut state = service.endpoint.state.lock();
            if state.flusho() {
                state.output.clear();
                state.echo.clear();
                state.xchar = None;
                state.pending_reprint = None;
                Peek::None
            } else if let Some(xchar) = state.xchar {
                Peek::Xchar(xchar)
            } else if state.stopped || state.tco_stopped {
                Peek::None
            } else if let Some(byte) = state.output.peek() {
                Peek::Output(byte)
            } else {
                Peek::None
            }
        };
        let byte = match peeked {
            Peek::Xchar(byte) | Peek::Output(byte) => byte,
            Peek::None => {
                if force {
                    service
                        .endpoint
                        .write_head
                        .notify(PollMask::POLLOUT | PollMask::POLLWRNORM);
                }
                return pumped;
            }
        };
        if crate::console::try_write(&[byte]) == 0 {
            if crate::console::arm_tx_interrupt() {
                let _ = service.work.as_ref().queue();
            }
            return pumped;
        }
        {
            let mut state = service.endpoint.state.lock();
            match peeked {
                Peek::Xchar(_) => {
                    state.xchar = None;
                }
                Peek::Output(_) => {
                    state.output.pop();
                }
                Peek::None => {}
            }
        }
        pumped = true;
        service
            .endpoint
            .write_head
            .notify(PollMask::POLLOUT | PollMask::POLLWRNORM);
    }
}

/// 阶段边界等待真实执行票尾部，常规 read/poll 不得借它泵硬件。
#[cfg(feature = "ktest")]
pub fn flush_work() {
    service().work.as_ref().flush();
}

/// TTY 打开实例，各路径共享同端点。
pub struct TtyFile {
    /// 真实设备路径。
    path: Path,
    /// OFD 访问模式与状态。
    flags: AtomicU32,
    /// 反向 watch 头。
    watches: Arc<WatchHead>,
    /// 是否已计入端点打开数。
    opened: bool,
}

impl TtyFile {
    /// 确需阻塞时建立有限源登记，之后必须重新扫描业务状态。
    fn prepare_wait(&self) -> Result<(Arc<PollContext>, PollTable), FsError> {
        let context = PollContext::try_new()?;
        let sink: Arc<dyn PollNotify> = context.clone();
        let mut table = PollTable::new(sink, 2)?;
        self.poll(Some(&mut table))?;
        Ok((context, table))
    }
}

impl Drop for TtyFile {
    /// 先同步撤自身反向 watch，未发表候选不碰端点计数。
    fn drop(&mut self) {
        self.watches.release();
        if !self.opened {
            return;
        }
        let copy = endpoint().copy.lock();
        let config = {
            let mut state = endpoint().state.lock();
            assert!(0 < state.opens, "tty open count underflow");
            state.opens -= 1;
            if state.opens != 0 || state.termios.cflag & termios::HUPCL == 0 {
                return;
            }
            let dropped = state.dtr || state.rts;
            state.dtr = false;
            state.rts = false;
            if !dropped {
                return;
            }
            crate::console::SerialConfig {
                divisor: state.divisor,
                line_control: termios::line_control(state.termios.cflag),
                receiver: false,
                dtr: false,
                rts: false,
            }
        };
        let _ = copy;
        crate::console::configure_serial(config);
    }
}

impl Pollable for TtyFile {
    /// 首轮登记读写两个真实源，再取独立的 read/MIN 与写谓词。
    fn poll(&self, table: Option<&mut PollTable>) -> Result<PollMask, FsError> {
        if let Some(table) = table {
            table.register(&endpoint().read_head)?;
            table.register(&endpoint().write_head)?;
        }
        let state = endpoint().state.lock();
        let mut mask = PollMask::default();
        if state.poll_readable() {
            mask |= PollMask::POLLIN | PollMask::POLLRDNORM;
        }
        if state.poll_writable() {
            mask |= PollMask::POLLOUT | PollMask::POLLWRNORM;
        }
        Ok(mask)
    }
}

/// 从真实 devfs 特殊角色打开 TTY，共享常驻端点。
pub fn open(
    path: Path,
    flags: u32,
    _caller: FileIoContext<'_>,
    special: MemorySpecial,
) -> Result<FileRef, FsError> {
    match special {
        MemorySpecial::TtySerial | MemorySpecial::Console => {}
        _ => return Err(FsError::InvalidOperation),
    }
    if flags & 0o200000 != 0 {
        return Err(FsError::NotDirectory);
    }
    let watches = Arc::try_new(WatchHead::new()).map_err(|_| FsError::NoMemory)?;
    let mut file = Arc::try_new(TtyFile {
        path,
        flags: AtomicU32::new(flags),
        watches,
        opened: false,
    })
    .map_err(|_| FsError::NoMemory)?;
    {
        let copy = endpoint().copy.lock();
        let config = {
            let mut state = endpoint().state.lock();
            let baud =
                termios::baud_rate(state.termios.output_baud()).ok_or(FsError::InvalidOperation)?;
            let hung_up = baud == 0;
            let divisor = if hung_up {
                state.divisor
            } else {
                termios::divisor_for_baud(crate::console::baud_base(), baud)
                    .ok_or(FsError::InvalidOperation)?
            };
            if state.opens == 0 {
                state.dtr = !hung_up;
                state.rts = !hung_up;
                state.divisor = divisor;
            }
            state.opens += 1;
            Arc::get_mut(&mut file)
                .expect("tty open candidate is shared")
                .opened = true;
            if state.opens != 1 {
                return Ok(file);
            }
            crate::console::SerialConfig {
                divisor,
                line_control: termios::line_control(state.termios.cflag),
                receiver: state.termios.cflag & termios::CREAD != 0,
                dtr: !hung_up,
                rts: !hung_up,
            }
        };
        let snapshot = crate::console::configure_serial(config);
        endpoint().state.lock().divisor = snapshot.divisor;
        drop(copy);
    }
    Ok(file)
}

impl FileLike for TtyFile {
    /// 按规范/非规范与 MIN/TIME 交付输入，只消费复制成功的正前缀。
    fn read_into(
        &self,
        target: &mut ReadBuffer<'_>,
        caller: FileIoContext<'_>,
    ) -> Result<usize, FsError> {
        crate::fs::file::require_read(self.flags())?;
        if target.len() == target.copied() {
            return Ok(0);
        }
        let interruptible = matches!(caller, FileIoContext::User(_));
        let _reader = acquire_reader(self.flags(), interruptible)?;
        let termios = endpoint().state.lock().termios;
        if termios.lflag & termios::ICANON != 0 {
            canonical_read(self, target, caller)
        } else {
            noncanonical_read(self, target, caller, termios)
        }
    }

    /// TTY 没有可定位位置。
    fn read_at_into(
        &self,
        _offset: u64,
        _target: &mut ReadBuffer<'_>,
        _caller: FileIoContext<'_>,
    ) -> Result<usize, FsError> {
        Err(FsError::IllegalSeek)
    }

    /// 返回真实设备路径。
    fn path(&self) -> Option<&Path> {
        Some(&self.path)
    }

    /// TTY 没有文件映射能力。
    fn mapping_inode(&self) -> Option<&InodeRef> {
        None
    }

    /// 提供读写两个真实持续源。
    fn pollable(&self) -> Option<&dyn Pollable> {
        Some(self)
    }

    /// 反向登记属于此打开实例。
    fn watch_head(&self) -> Option<&Arc<WatchHead>> {
        Some(&self.watches)
    }

    /// 只对 User 上下文复制指针命令，Kernel 裸指针无可证明存储契约。
    fn ioctl(
        &self,
        command: u32,
        argument: usize,
        caller: FileIoContext<'_>,
    ) -> Result<usize, FsError> {
        let FileIoContext::User(user) = caller else {
            return match command {
                TCGETS | TCSETS | TCSETSW | TCSETSF | TCSBRK | TCXONC | TCFLSH => {
                    Err(FsError::InvalidOperation)
                }
                _ => Err(FsError::NotTty),
            };
        };
        match command {
            TCGETS => ioctl_get(user, argument),
            TCSETS => ioctl_set(user, argument, 0, caller),
            TCSETSW => ioctl_set(user, argument, 1, caller),
            TCSETSF => ioctl_set(user, argument, 2, caller),
            TCSBRK => ioctl_break(argument, caller),
            TCXONC => endpoint_flow(argument as u32),
            TCFLSH => endpoint_flush(argument as u32).map(|()| 0),
            _ => Err(FsError::NotTty),
        }
    }

    /// 返回 OFD 访问模式与状态。
    fn flags(&self) -> u32 {
        self.flags.load(Ordering::Acquire)
    }

    /// 只改变支持的可变状态位。
    fn set_status_flags(&self, raw: u32) -> Result<(), FsError> {
        crate::fs::file::set_status_flags(&self.flags, raw)
    }

    /// TTY 不能截断。
    fn truncate(&self, _new_size: u64) -> Result<(), FsError> {
        Err(FsError::InvalidOperation)
    }

    /// TTY 没有 fsync 语义。
    fn sync_range(&self, _range: Range<u64>, _data_only: bool) -> Result<(), FsError> {
        Err(FsError::InvalidOperation)
    }

    /// TTY 没有共享字节位置。
    fn seek(&self, _from: SeekFrom) -> Result<u64, FsError> {
        Err(FsError::IllegalSeek)
    }

    /// stat 来自真实路径。
    fn stat(&self) -> Result<FileStat, FsError> {
        crate::fs::file::stat_path(&self.path)
    }

    /// TTY 不提供目录项。
    fn readdir(&self) -> Result<Option<DirectoryEntry>, FsError> {
        Err(FsError::NotDirectory)
    }

    /// TTY 不能作为目录枚举起点。
    fn for_each_dirent(
        &self,
        _emit: &mut dyn FnMut(&DirectoryEntry) -> Result<bool, FsError>,
    ) -> Result<(), FsError> {
        Err(FsError::NotDirectory)
    }

    /// 按输出模式加工输入并返回实际接受字节；一次逻辑写保 writer 跨空间等待。
    fn write_from(
        &self,
        source: &mut WriteBuffer<'_>,
        caller: FileIoContext<'_>,
    ) -> Result<usize, FsError> {
        crate::fs::file::require_write(self.flags())?;
        let start = source.copied();
        if source.len() == start {
            return Ok(0);
        }
        let interruptible = matches!(caller, FileIoContext::User(_));
        // 先快扫 writer 与空间，确需等待才建上下文；writer 竞争与队列满分开处理。
        let mut writer = match endpoint().writer.try_lock() {
            Some(guard) => Some(WriterGuard { inner: Some(guard) }),
            None => {
                if self.flags() & O_NONBLOCK != 0 {
                    return Err(FsError::WouldBlock);
                }
                None
            }
        };
        let mut waiting: Option<(Arc<PollContext>, PollTable)> = None;
        // writer 未得时用独立上下文等串行锁，不与队列满混用同一 clear/park。
        while writer.is_none() {
            if waiting.is_none() {
                waiting = Some(self.prepare_wait()?);
            }
            let (context, _) = waiting.as_ref().expect("tty writer wait without context");
            context.clear_triggered();
            if let Some(guard) = endpoint().writer.try_lock() {
                writer = Some(WriterGuard { inner: Some(guard) });
                break;
            }
            if context.park_until(interruptible, None) == WaitResult::Interrupted {
                return Err(FsError::Interrupted);
            }
        }
        let mut waiting = waiting;
        let _writer = writer.take().expect("tty writer was not acquired");
        let mut accepted = 0;
        let mut segment = [0u8; WRITE_SEGMENT];
        let mut pending_buf = [0u8; WRITE_SEGMENT];
        let mut pending_len = 0;
        let mut pending_off = 0;
        loop {
            if pending_off == pending_len {
                let remaining = source.len() - source.copied();
                if remaining == 0 {
                    return Ok(accepted);
                }
                let count = remaining.min(segment.len());
                match source.pull(&mut segment[..count]) {
                    Ok(0) => return progress(accepted, FsError::BadAddress),
                    Ok(prepared) => {
                        pending_buf[..prepared].copy_from_slice(&segment[..prepared]);
                        pending_len = prepared;
                        pending_off = 0;
                    }
                    Err(error) => return progress(accepted, error),
                }
            }
            // 持 writer 短取 copy/state 提交；输出满时释 copy/state 但保 writer 等空间。
            let consumed = {
                let _copy = endpoint().copy.lock();
                let mut state = endpoint().state.lock();
                let termios = state.termios;
                let mut consumed = 0;
                for &byte in pending_buf[pending_off..pending_len].iter() {
                    if state.flusho() {
                        consumed += 1;
                        continue;
                    }
                    if !state.output_byte(termios, byte) {
                        break;
                    }
                    consumed += 1;
                }
                consumed
            };
            pending_off += consumed;
            accepted += consumed;
            schedule_transmit();
            endpoint()
                .write_head
                .notify(PollMask::POLLOUT | PollMask::POLLWRNORM);
            if pending_off < pending_len {
                if self.flags() & O_NONBLOCK != 0 {
                    return progress(accepted, FsError::WouldBlock);
                }
                if 0 < accepted {
                    return Ok(accepted);
                }
                // 零进度阻塞：保 writer 跨空间等待，复查完整 reserve 后 park。
                if waiting.is_none() {
                    waiting = Some(self.prepare_wait()?);
                }
                let (context, _) = waiting.as_ref().expect("tty space wait without context");
                context.clear_triggered();
                let ready = {
                    let state = endpoint().state.lock();
                    state.output_can_accept(state.termios, pending_buf[pending_off])
                };
                if !ready {
                    if context.park_until(interruptible, None) == WaitResult::Interrupted {
                        return Err(FsError::Interrupted);
                    }
                }
                continue;
            }
            pending_len = 0;
            pending_off = 0;
            if source.copied() == source.len() {
                return Ok(accepted);
            }
        }
    }

    /// TTY 不能按位置写入。
    fn write_at_from(
        &self,
        _offset: u64,
        _source: &mut WriteBuffer<'_>,
        _caller: FileIoContext<'_>,
    ) -> Result<usize, FsError> {
        Err(FsError::IllegalSeek)
    }
}

/// 无进度返回具体错误，已有正前缀优先保留。
fn progress(count: usize, error: FsError) -> Result<usize, FsError> {
    if count == 0 {
        Err(error)
    } else {
        Ok(count)
    }
}

/// 取得一次逻辑 read 的串行责任，归还时通知读源唤醒竞争者。
fn acquire_reader(flags: u32, interruptible: bool) -> Result<ReaderGuard, FsError> {
    if let Some(guard) = endpoint().reader.try_lock() {
        return Ok(ReaderGuard { inner: Some(guard) });
    }
    if flags & O_NONBLOCK != 0 {
        return Err(FsError::WouldBlock);
    }
    if !interruptible {
        return Ok(ReaderGuard {
            inner: Some(endpoint().reader.lock()),
        });
    }
    let context = PollContext::try_new()?;
    let sink: Arc<dyn PollNotify> = context.clone();
    let mut table = PollTable::new(sink, 2)?;
    table.register(&endpoint().read_head)?;
    table.register(&endpoint().write_head)?;
    loop {
        context.clear_triggered();
        if let Some(guard) = endpoint().reader.try_lock() {
            return Ok(ReaderGuard { inner: Some(guard) });
        }
        if context.park_until(true, None) == WaitResult::Interrupted {
            return Err(FsError::Interrupted);
        }
    }
}

/// 取得用户输出串行锁，归还时通知写源唤醒竞争者。
fn acquire_writer(flags: u32, interruptible: bool) -> Result<WriterGuard, FsError> {
    if let Some(guard) = endpoint().writer.try_lock() {
        return Ok(WriterGuard { inner: Some(guard) });
    }
    if flags & O_NONBLOCK != 0 {
        return Err(FsError::WouldBlock);
    }
    if !interruptible {
        return Ok(WriterGuard {
            inner: Some(endpoint().writer.lock()),
        });
    }
    let context = PollContext::try_new()?;
    let sink: Arc<dyn PollNotify> = context.clone();
    let mut table = PollTable::new(sink, 2)?;
    table.register(&endpoint().read_head)?;
    table.register(&endpoint().write_head)?;
    loop {
        context.clear_triggered();
        if let Some(guard) = endpoint().writer.try_lock() {
            return Ok(WriterGuard { inner: Some(guard) });
        }
        if context.park_until(true, None) == WaitResult::Interrupted {
            return Err(FsError::Interrupted);
        }
    }
}

/// 规范模式 read：持同一 copy 跨 peek→push→consume，park 前结束 guard。
fn canonical_read(
    file: &TtyFile,
    target: &mut ReadBuffer<'_>,
    caller: FileIoContext<'_>,
) -> Result<usize, FsError> {
    let start = target.copied();
    let interruptible = matches!(caller, FileIoContext::User(_));
    let mut waiting: Option<(Arc<PollContext>, PollTable)> = None;
    let mut interrupted = false;
    let mut segment = [0u8; SEGMENT_BYTES];
    loop {
        if let Some((context, _)) = &waiting {
            context.clear_triggered();
        }
        let copy = endpoint().copy.lock();
        let (peeked, eof, switched) = {
            let state = endpoint().state.lock();
            if state.termios.lflag & termios::ICANON == 0 {
                (0, false, true)
            } else {
                let count = (target.len() - target.copied()).min(segment.len());
                let (peeked, eof) = state.peek_canonical(&mut segment[..count]);
                (peeked, eof, false)
            }
        };
        if switched {
            drop(copy);
            let termios = endpoint().state.lock().termios;
            return noncanonical_read(file, target, caller, termios);
        }
        if 0 < peeked || eof {
            let before = target.copied();
            let result = target.push(&segment[..peeked]);
            let delivered = target.copied() - before;
            {
                let mut state = endpoint().state.lock();
                state.consume_canonical(delivered, eof && delivered == 0);
            }
            drop(copy);
            endpoint()
                .read_head
                .notify(PollMask::POLLIN | PollMask::POLLRDNORM);
            endpoint()
                .write_head
                .notify(PollMask::POLLOUT | PollMask::POLLWRNORM);
            if let Err(error) = result {
                return progress(target.copied() - start, error);
            }
            if eof && target.copied() == start {
                return Ok(0);
            }
            return Ok(target.copied() - start);
        }
        drop(copy);
        if file.flags() & O_NONBLOCK != 0 {
            return Err(FsError::WouldBlock);
        }
        if interrupted {
            return Err(FsError::Interrupted);
        }
        if let Some((context, _)) = &waiting {
            interrupted = context.park_until(interruptible, None) == WaitResult::Interrupted;
        } else {
            waiting = Some(file.prepare_wait()?);
        }
    }
}

/// 非规范交付结果：本段交付字节数或已切规范。
enum RawDeliver {
    /// 本段交付字节数；Err 保原始复制错误，调用者按总前缀合成。
    Delivered(Result<usize, FsError>),
    /// 模式已切规范，无进度时调用者转规范读。
    Switched,
}

/// 持 copy 短查 live 模式并 peek，锁外 push，同 copy 消费；切规范返回切换。
fn deliver_raw(target: &mut ReadBuffer<'_>, segment: &mut [u8]) -> RawDeliver {
    let copy = endpoint().copy.lock();
    let peeked = {
        let state = endpoint().state.lock();
        if state.termios.lflag & termios::ICANON != 0 {
            return RawDeliver::Switched;
        }
        let want = (target.len() - target.copied()).min(segment.len());
        state.peek_raw(&mut segment[..want])
    };
    if peeked == 0 {
        drop(copy);
        return RawDeliver::Delivered(Ok(0));
    }
    let before = target.copied();
    let result = target.push(&segment[..peeked]);
    let delivered = target.copied() - before;
    {
        let mut state = endpoint().state.lock();
        state.consume_raw(delivered);
    }
    drop(copy);
    match result {
        Ok(_) => RawDeliver::Delivered(Ok(delivered)),
        Err(error) => RawDeliver::Delivered(Err(error)),
    }
}

/// 非规范模式 read：四类 MIN/TIME 各有本次 read 局部 deadline。
fn noncanonical_read(
    file: &TtyFile,
    target: &mut ReadBuffer<'_>,
    caller: FileIoContext<'_>,
    termios: Termios,
) -> Result<usize, FsError> {
    let start = target.copied();
    let request = target.len() - start;
    let min = usize::from(termios.cc[VMIN]);
    let time = termios.cc[VTIME];
    if min == 0 && time == 0 {
        let mut segment = [0u8; SEGMENT_BYTES];
        match deliver_raw(target, &mut segment) {
            RawDeliver::Delivered(Ok(count)) => return Ok(count),
            RawDeliver::Delivered(Err(error)) => return progress(target.copied() - start, error),
            RawDeliver::Switched => return canonical_read(file, target, caller),
        }
    }
    if min != 0 && time == 0 {
        return noncanonical_min(file, target, caller, min.min(request));
    }
    if min == 0 {
        return noncanonical_timeout(file, target, caller, time);
    }
    noncanonical_min_timeout(file, target, caller, min.min(request), time)
}

/// MIN>0/TIME==0：等待 min(MIN,count) 字节；信号无进度才中断。
fn noncanonical_min(
    file: &TtyFile,
    target: &mut ReadBuffer<'_>,
    caller: FileIoContext<'_>,
    minimum: usize,
) -> Result<usize, FsError> {
    let start = target.copied();
    let interruptible = matches!(caller, FileIoContext::User(_));
    let mut waiting: Option<(Arc<PollContext>, PollTable)> = None;
    let mut interrupted = false;
    let mut segment = [0u8; SEGMENT_BYTES];
    loop {
        if let Some((context, _)) = &waiting {
            context.clear_triggered();
        }
        match deliver_raw(target, &mut segment) {
            RawDeliver::Switched => {
                if start < target.copied() {
                    return Ok(target.copied() - start);
                }
                return canonical_read(file, target, caller);
            }
            RawDeliver::Delivered(Ok(0)) => {}
            RawDeliver::Delivered(Ok(_)) => {
                if minimum <= target.copied() - start {
                    return Ok(target.copied() - start);
                }
                continue;
            }
            RawDeliver::Delivered(Err(error)) => {
                return progress(target.copied() - start, error);
            }
        }
        if file.flags() & O_NONBLOCK != 0 {
            return progress(target.copied() - start, FsError::WouldBlock);
        }
        if interrupted {
            return progress(target.copied() - start, FsError::Interrupted);
        }
        if let Some((context, _)) = &waiting {
            interrupted = context.park_until(interruptible, None) == WaitResult::Interrupted;
        } else {
            waiting = Some(file.prepare_wait()?);
        }
    }
}

/// MIN==0/TIME>0：从本次 read 开始一次绝对总 deadline；信号返回已有前缀。
fn noncanonical_timeout(
    file: &TtyFile,
    target: &mut ReadBuffer<'_>,
    caller: FileIoContext<'_>,
    time: u8,
) -> Result<usize, FsError> {
    let start = target.copied();
    let interruptible = matches!(caller, FileIoContext::User(_));
    let deadline = time::now()
        .checked_add(Duration::from_millis(u64::from(time) * 100))
        .ok_or(FsError::InvalidOperation)?;
    let mut waiting: Option<(Arc<PollContext>, PollTable)> = None;
    let mut segment = [0u8; SEGMENT_BYTES];
    loop {
        if let Some((context, _)) = &waiting {
            context.clear_triggered();
        }
        match deliver_raw(target, &mut segment) {
            RawDeliver::Switched => {
                if start < target.copied() {
                    return Ok(target.copied() - start);
                }
                return canonical_read(file, target, caller);
            }
            RawDeliver::Delivered(Ok(0)) => {}
            RawDeliver::Delivered(Ok(_)) => return Ok(target.copied() - start),
            RawDeliver::Delivered(Err(error)) => {
                return progress(target.copied() - start, error);
            }
        }
        if file.flags() & O_NONBLOCK != 0 {
            return progress(target.copied() - start, FsError::WouldBlock);
        }
        if deadline <= time::now() {
            return Ok(target.copied() - start);
        }
        if let Some((context, _)) = &waiting {
            if context.park_until(interruptible, Some(deadline)) == WaitResult::Interrupted {
                return progress(target.copied() - start, FsError::Interrupted);
            }
        } else {
            waiting = Some(file.prepare_wait()?);
        }
    }
}

/// MIN>0/TIME>0：首字节前无 timer，实际新字节进度后更新间隔 deadline。
fn noncanonical_min_timeout(
    file: &TtyFile,
    target: &mut ReadBuffer<'_>,
    caller: FileIoContext<'_>,
    minimum: usize,
    time: u8,
) -> Result<usize, FsError> {
    let start = target.copied();
    let interruptible = matches!(caller, FileIoContext::User(_));
    let mut waiting: Option<(Arc<PollContext>, PollTable)> = None;
    let mut interrupted = false;
    let mut deadline: Option<MonotonicInstant> = None;
    let mut segment = [0u8; SEGMENT_BYTES];
    loop {
        if let Some((context, _)) = &waiting {
            context.clear_triggered();
        }
        match deliver_raw(target, &mut segment) {
            RawDeliver::Switched => {
                if start < target.copied() {
                    return Ok(target.copied() - start);
                }
                return canonical_read(file, target, caller);
            }
            RawDeliver::Delivered(Ok(0)) => {}
            RawDeliver::Delivered(Ok(_)) => {
                deadline = Some(
                    time::now()
                        .checked_add(Duration::from_millis(u64::from(time) * 100))
                        .ok_or(FsError::InvalidOperation)?,
                );
                if minimum <= target.copied() - start {
                    return Ok(target.copied() - start);
                }
                continue;
            }
            RawDeliver::Delivered(Err(error)) => {
                return progress(target.copied() - start, error);
            }
        }
        if file.flags() & O_NONBLOCK != 0 {
            return progress(target.copied() - start, FsError::WouldBlock);
        }
        if interrupted {
            return progress(target.copied() - start, FsError::Interrupted);
        }
        if 0 < target.copied() - start {
            if let Some(limit) = deadline {
                if limit <= time::now() {
                    return Ok(target.copied() - start);
                }
            }
        }
        if let Some((context, _)) = &waiting {
            let result = context.park_until(interruptible, deadline);
            if result == WaitResult::Interrupted {
                interrupted = true;
            } else if 0 < target.copied() - start {
                if let Some(limit) = deadline {
                    if limit <= time::now() {
                        return Ok(target.copied() - start);
                    }
                }
            }
        } else {
            waiting = Some(file.prepare_wait()?);
        }
    }
}

/// 从真实 Mm 复制 36 字节 termios 指针参数；完整 36 字节才成功。
fn ioctl_get(user: &UserThread, argument: usize) -> Result<usize, FsError> {
    let mm = user.mm().ok_or(FsError::BadAddress)?;
    let bytes = endpoint().state.lock().termios.to_bytes();
    let progress = mm.copyout(argument, &bytes);
    if progress.error.is_some() || progress.copied != bytes.len() {
        return Err(FsError::BadAddress);
    }
    Ok(0)
}

/// 从真实 Mm 复制并提交 36 字节 termios，action 0/1/2 对应 NOW/DRAIN/FLUSH。
fn ioctl_set(
    user: &UserThread,
    argument: usize,
    action: u32,
    caller: FileIoContext<'_>,
) -> Result<usize, FsError> {
    let mm = user.mm().ok_or(FsError::BadAddress)?;
    let mut bytes = [0u8; termios::TERMIOS_BYTES];
    let progress = mm.copyin(&mut bytes, argument);
    if progress.error.is_some() || progress.copied != bytes.len() {
        return Err(FsError::BadAddress);
    }
    let termios = Termios::from_bytes(bytes)?;
    set_termios(termios, action, caller)?;
    Ok(0)
}

/// TCSBRK 非零沿真实 drain，0 返回未选择的 InvalidOperation。
fn ioctl_break(argument: usize, caller: FileIoContext<'_>) -> Result<usize, FsError> {
    if argument == 0 {
        return Err(FsError::InvalidOperation);
    }
    drain(caller)?;
    Ok(0)
}

/// 返回当前 termios 一致快照。
pub fn termios() -> Termios {
    endpoint().state.lock().termios
}

/// 提交已验证 termios，action 0/1/2 分别为 NOW/DRAIN/FLUSH。
/// NOW 只取 copy，不取 writer；DRAIN/FLUSH 先 drain 再持 writer 重查提交。
pub fn set_termios(
    termios: Termios,
    action: u32,
    caller: FileIoContext<'_>,
) -> Result<(), FsError> {
    if 2 < action {
        return Err(FsError::InvalidOperation);
    }
    termios.validate()?;
    if action == 0 {
        return commit_termios(termios, action, None);
    }
    drain(caller)?;
    loop {
        let writer = acquire_writer(0, matches!(caller, FileIoContext::User(_)))?;
        let copy = endpoint().copy.lock();
        let pending = {
            let state = endpoint().state.lock();
            !state.output_empty()
        } || !crate::console::transmitter_empty();
        if pending {
            drop(copy);
            drop(writer);
            drain(caller)?;
            continue;
        }
        let result = commit_termios(termios, action, Some(copy));
        drop(writer);
        return result;
    }
}

/// 在 copy 下先算硬件目标、释 state 后配硬件，回读成功再发表软件。
fn commit_termios(
    termios: Termios,
    action: u32,
    held: Option<crate::sync::SleepMutexGuard<'static, ()>>,
) -> Result<(), FsError> {
    let copy = held.unwrap_or_else(|| endpoint().copy.lock());
    let (config, canonical, hung_up, restore, old_ixon) = {
        let state = endpoint().state.lock();
        let canonical = termios.lflag & termios::ICANON != 0;
        let current_divisor = state.divisor;
        let old_baud = termios::baud_rate(state.termios.output_baud());
        let new_baud = termios::baud_rate(termios.output_baud());
        let hung_up = new_baud == Some(0);
        let restore = old_baud == Some(0) && !hung_up && 0 < state.opens;
        let old_ixon = state.termios.iflag & termios::IXON != 0;
        let dtr = if hung_up {
            false
        } else if restore {
            true
        } else {
            state.dtr
        };
        let rts = if hung_up {
            false
        } else if restore {
            true
        } else {
            state.rts
        };
        let config = termios::serial_config(termios, current_divisor, dtr, rts)
            .ok_or(FsError::InvalidOperation)?;
        (config, canonical, hung_up, restore, old_ixon)
    };
    let snapshot = crate::console::configure_serial(config);
    {
        let mut state = endpoint().state.lock();
        if (state.termios.lflag & termios::ICANON != 0) != canonical {
            state.set_canonical(canonical);
        }
        state.termios = termios;
        if old_ixon && termios.iflag & termios::IXON == 0 && !state.tco_stopped {
            state.stopped = false;
        }
        if hung_up {
            state.dtr = false;
            state.rts = false;
        } else if restore {
            state.dtr = true;
            state.rts = true;
        }
        if action == 2 {
            flush_input_locked(&mut state);
        }
        if termios.lflag & termios::FLUSHO != 0 {
            state.flush_output();
        }
        state.divisor = snapshot.divisor;
    }
    drop(copy);
    endpoint()
        .read_head
        .notify(PollMask::POLLIN | PollMask::POLLRDNORM);
    endpoint()
        .write_head
        .notify(PollMask::POLLOUT | PollMask::POLLWRNORM);
    schedule_transmit();
    Ok(())
}

/// 按选择器丢弃输入、输出或全部队列。
pub fn flush(selector: u32) -> Result<(), FsError> {
    endpoint_flush(selector)
}

/// 持 copy 清行规程输入并丢弃软件 RX 环已排队样本。
fn flush_input_locked(state: &mut ntty::NttyState) {
    state.flush_input();
    crate::console::discard_received();
}

/// TCFLSH 的 typed 实现。
fn endpoint_flush(selector: u32) -> Result<(), FsError> {
    {
        let _copy = endpoint().copy.lock();
        let mut state = endpoint().state.lock();
        match selector {
            0 => flush_input_locked(&mut state),
            1 => state.flush_output(),
            2 => {
                flush_input_locked(&mut state);
                state.flush_output();
            }
            _ => return Err(FsError::InvalidOperation),
        }
    }
    endpoint()
        .read_head
        .notify(PollMask::POLLIN | PollMask::POLLRDNORM);
    endpoint()
        .write_head
        .notify(PollMask::POLLOUT | PollMask::POLLWRNORM);
    Ok(())
}

/// TCXONC 的 typed 实现，恢复后 queue 同 Work。
fn endpoint_flow(action: u32) -> Result<usize, FsError> {
    match action {
        0 => {
            let _copy = endpoint().copy.lock();
            let mut state = endpoint().state.lock();
            state.tco_stopped = true;
            state.stopped = true;
            Ok(0)
        }
        1 => {
            {
                let _copy = endpoint().copy.lock();
                let mut state = endpoint().state.lock();
                state.tco_stopped = false;
                state.stopped = false;
            }
            schedule_transmit();
            endpoint()
                .write_head
                .notify(PollMask::POLLOUT | PollMask::POLLWRNORM);
            Ok(0)
        }
        2 | 3 => {
            let byte = {
                let state = endpoint().state.lock();
                if action == 2 {
                    state.termios.cc[termios::VSTOP]
                } else {
                    state.termios.cc[termios::VSTART]
                }
            };
            if byte == 0 {
                return Err(FsError::InvalidOperation);
            }
            {
                let _copy = endpoint().copy.lock();
                let mut state = endpoint().state.lock();
                state.xchar = Some(byte);
            }
            schedule_transmit();
            Ok(0)
        }
        _ => Err(FsError::InvalidOperation),
    }
}

/// TCXONC 的 typed 别名。
pub fn flow(action: u32) -> Result<(), FsError> {
    endpoint_flow(action).map(|_| ())
}

/// 等待软件队列为空，再按字符时间复查真实 TEMT。
pub fn drain(caller: FileIoContext<'_>) -> Result<(), FsError> {
    let interruptible = matches!(caller, FileIoContext::User(_));
    let mut waiting: Option<(Arc<PollContext>, PollTable)> = None;
    loop {
        if let Some((context, _)) = &waiting {
            context.clear_triggered();
        }
        let empty = endpoint().state.lock().output_empty();
        if empty {
            break;
        }
        if let Some((context, _)) = &waiting {
            if context.park_until(interruptible, None) == WaitResult::Interrupted {
                return Err(FsError::Interrupted);
            }
        } else {
            let context = PollContext::try_new()?;
            let sink: Arc<dyn PollNotify> = context.clone();
            let mut table = PollTable::new(sink, 2)?;
            table.register(&endpoint().read_head)?;
            table.register(&endpoint().write_head)?;
            waiting = Some((context, table));
        }
    }
    // 软件队列已空，按当前回读分频计算字符时间并复查真实 TEMT。
    loop {
        if crate::console::transmitter_empty() {
            return Ok(());
        }
        let snapshot: SerialSnapshot = crate::console::serial_snapshot();
        let cflag = endpoint().state.lock().termios.cflag;
        let nanos = termios::character_nanos(snapshot.divisor, snapshot.baud_base, cflag)
            .ok_or(FsError::InvalidOperation)?;
        let deadline = time::now()
            .checked_add(Duration::from_nanos(nanos))
            .ok_or(FsError::InvalidOperation)?;
        if interruptible {
            if crate::timer::sleep_until_interruptible(deadline) == WaitResult::Interrupted {
                return Err(FsError::Interrupted);
            }
        } else {
            crate::timer::sleep_until(deadline);
        }
    }
}
