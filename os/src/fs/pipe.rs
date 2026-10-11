//! 匿名管道的两个真实 OFD 共享一个按需原页环，描述符复制不增加端点。

use alloc::sync::Arc;
use core::ops::Range;
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

use crate::fs::epoll::WatchHead;
use crate::fs::file::{
    FileIoContext, FileLike, FileRef, FileStat, ReadBuffer, SeekFrom, WriteBuffer,
};
use crate::fs::poll::{PollContext, PollMask, PollNotify, PollTable, PollWaitHead, Pollable};
use crate::fs::{DirectoryEntry, FsError, InodeKind, InodeRef, InodeTime, Path};
use crate::mm::{self, OwnedPages};
use crate::sync::SleepMutex;
use crate::wait::WaitResult;

pub const PIPE_BUF: usize = 4096;
pub const PIPE_CAPACITY: usize = 16 * PIPE_BUF;
const SLOTS: usize = 16;
const O_NONBLOCK: u32 = 0o4000;
const O_CLOEXEC: u32 = 0o2000000;
const O_APPEND: u32 = 0o2000;
const SPACE_SHIFT: u32 = 17;
const BYTE_MASK: u64 = (1 << SPACE_SHIFT) - 1;
static NEXT_INODE: AtomicU64 = AtomicU64::new(1);

struct Slot {
    pages: OwnedPages,
    offset: usize,
    length: usize,
}

/// 按需拥有原页的固定槽 FIFO，管道和匿名 socket 方向各自决定锁与关闭协议。
pub(crate) struct PipeBuffer<const N: usize> {
    slots: [Option<Slot>; N],
    head: usize,
    tail: usize,
    used: usize,
    bytes: usize,
}

impl<const N: usize> PipeBuffer<N> {
    /// 空 FIFO 不预分配数据页，槽数必须为正且每槽恰好拥有一个原页。
    pub(crate) fn new() -> Self {
        assert!(0 < N);
        assert_eq!(mm::PAGE_SIZE, PIPE_BUF);
        Self {
            slots: core::array::from_fn(|_| None),
            head: 0,
            tail: 0,
            used: 0,
            bytes: 0,
        }
    }

    /// 查询已经提交且尚未消费的字节量。
    pub(crate) fn len(&self) -> usize {
        self.bytes
    }

    /// 查询是否还有可交付数据，不把空槽当成已提交字节。
    pub(crate) fn is_empty(&self) -> bool {
        self.bytes == 0
    }

    /// 返回全部物理槽均为空时的固定字节容量。
    pub(crate) fn capacity(&self) -> usize {
        N * PIPE_BUF
    }

    /// 只计算末页的可合并后缀，读过的前端空隙不能重新成为物理槽。
    fn merge_space(&self) -> usize {
        if self.used == 0 {
            return 0;
        }
        let slot = self.slots[(self.head + N - 1) % N].as_ref().unwrap();
        PIPE_BUF - slot.offset - slot.length
    }

    /// 容量由空闲物理槽及末页后缀组成，不由逻辑剩余字节推断。
    pub(crate) fn space(&self) -> usize {
        self.empty_slot_capacity() + self.merge_space()
    }

    /// 只报告空物理槽的容量，管道 poll 不因满槽末页仍可合并而写就绪。
    pub(crate) fn empty_slot_capacity(&self) -> usize {
        self.capacity() - self.used * PIPE_BUF
    }

    /// 立即归还全部占用页并恢复空 FIFO 的索引和容量。
    pub(crate) fn clear(&mut self) {
        self.slots.iter_mut().for_each(|slot| drop(slot.take()));
        self.head = 0;
        self.tail = 0;
        self.used = 0;
        self.bytes = 0;
    }

    /// 仅消费实际交付量，最后一个字节交付后结束该槽页拥有。
    fn consume(&mut self, count: usize) {
        let slot = self.slots[self.tail].as_mut().unwrap();
        assert!(count <= slot.length);
        slot.offset += count;
        slot.length -= count;
        self.bytes -= count;
        if slot.length == 0 {
            drop(self.slots[self.tail].take());
            self.tail = (self.tail + 1) % N;
            self.used -= 1;
        }
    }

    /// 在调用者的睡眠锁内复制输入到末页或新槽；调用前必须有真实空间。
    /// 一次只提交一个页后缀，失败不影响此前已发表的数据页。
    pub(crate) fn append(&mut self, source: &mut WriteBuffer<'_>) -> Result<usize, FsError> {
        if self.merge_space() == 0 {
            assert!(self.used < N);
            let pages = mm::alloc_pages(0, true).ok_or(FsError::NoMemory)?;
            self.slots[self.head] = Some(Slot {
                pages,
                offset: 0,
                length: 0,
            });
            self.head = (self.head + 1) % N;
            self.used += 1;
        }
        let index = (self.head + N - 1) % N;
        let slot = self.slots[index].as_mut().unwrap();
        let offset = slot.offset + slot.length;
        // SAFETY: 本槽独占一个已清零的原页，调用者的睡眠锁排除读写和归还；
        // 后缀完全位于页内，pull 不保存独占借用，结束后才更新长度或释放空槽。
        let dst = unsafe {
            core::slice::from_raw_parts_mut(
                slot.pages.as_mut_ptr().cast::<u8>().add(offset),
                PIPE_BUF - offset,
            )
        };
        let result = source.pull(dst);
        if let Ok(count) = result {
            slot.length += count;
            self.bytes += count;
        }
        if slot.length == 0 {
            drop(self.slots[index].take());
            self.head = index;
            self.used -= 1;
        }
        result
    }

    /// 在调用者的睡眠锁内逐槽交付，非 PEEK 只消费实际 copyout 差量。
    /// 故障保留本次合法正前缀；PEEK 的局部游标不改变槽、页及 FIFO 索引。
    pub(crate) fn receive(
        &mut self,
        target: &mut ReadBuffer<'_>,
        peek: bool,
    ) -> Result<usize, FsError> {
        let start = target.copied();
        let available = self.len();
        let mut index = self.tail;
        let mut offset = 0;
        while target.copied() - start < available && target.copied() < target.len() {
            let slot = self.slots[index].as_mut().unwrap();
            let length = slot.length;
            let count = (length - offset).min(target.len() - target.copied());
            // SAFETY: 槽独占的原页仍然存活，片段仅覆盖已提交的初始化字节；
            // 调用者的睡眠锁排除并发写入及归页，push 不保存本页借用。
            let bytes = unsafe {
                core::slice::from_raw_parts(
                    slot.pages
                        .as_mut_ptr()
                        .cast::<u8>()
                        .add(slot.offset + offset),
                    count,
                )
            };
            let before = target.copied();
            let result = target.push(bytes);
            let delivered = target.copied() - before;
            if peek {
                offset += delivered;
                if offset == length {
                    index = (index + 1) % N;
                    offset = 0;
                }
            } else {
                self.consume(delivered);
                index = self.tail;
            }
            if let Err(error) = result {
                let copied = target.copied() - start;
                return if copied == 0 { Err(error) } else { Ok(copied) };
            }
            assert!(0 < delivered, "FIFO copyout made no progress");
        }
        Ok(target.copied() - start)
    }
}

struct Pipe {
    ring: SleepMutex<PipeBuffer<SLOTS>>,
    /// 一次原子观察同时保存数据量和物理空槽容量，末页合并后缀不授予poll写就绪。
    observation: AtomicU64,
    reader_open: AtomicBool,
    writer_open: AtomicBool,
    read_head: Arc<PollWaitHead>,
    write_head: Arc<PollWaitHead>,
    inode: u64,
    uid: u32,
    gid: u32,
}

impl Pipe {
    /// 状态提交后发布查询快照并通知真正的读写源；通知不取得 ring 锁。
    fn publish(&self, ring: &PipeBuffer<SLOTS>, read_changed: bool, write_changed: bool) {
        self.observation.store(
            (ring.len() as u64) | (ring.empty_slot_capacity() as u64) << SPACE_SHIFT,
            Ordering::Release,
        );
        if read_changed {
            self.read_head
                .notify(PollMask::POLLIN | PollMask::POLLRDNORM);
        }
        if write_changed {
            self.write_head
                .notify(PollMask::POLLOUT | PollMask::POLLWRNORM);
        }
    }
}

struct PipeEnd {
    pipe: Arc<Pipe>,
    read: bool,
    flags: AtomicU32,
    watches: Arc<WatchHead>,
}

/// 按创建者凭据准备两个实际OFD，CLOEXEC留给描述符槽；未发表失败归还原拥有。
pub fn try_new(flags: u32, uid: u32, gid: u32) -> Result<(FileRef, FileRef), FsError> {
    if flags & !(O_NONBLOCK | O_CLOEXEC) != 0 {
        return Err(FsError::InvalidOperation);
    }
    let read_head = Arc::try_new(PollWaitHead::new()).map_err(|_| FsError::NoMemory)?;
    let write_head = Arc::try_new(PollWaitHead::new()).map_err(|_| FsError::NoMemory)?;
    let inode = NEXT_INODE
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
        .expect("pipe inode identity exhausted");
    let pipe = Arc::try_new(Pipe {
        ring: SleepMutex::new(PipeBuffer::new()),
        observation: AtomicU64::new((PIPE_CAPACITY as u64) << SPACE_SHIFT),
        reader_open: AtomicBool::new(true),
        writer_open: AtomicBool::new(true),
        read_head,
        write_head,
        inode,
        uid,
        gid,
    })
    .map_err(|_| FsError::NoMemory)?;
    let read_watches = Arc::try_new(WatchHead::new()).map_err(|_| FsError::NoMemory)?;
    let write_watches = Arc::try_new(WatchHead::new()).map_err(|_| FsError::NoMemory)?;
    let reader: FileRef = Arc::try_new(PipeEnd {
        pipe: pipe.clone(),
        read: true,
        flags: AtomicU32::new(flags & O_NONBLOCK),
        watches: read_watches,
    })
    .map_err(|_| FsError::NoMemory)?;
    let writer: FileRef = Arc::try_new(PipeEnd {
        pipe,
        read: false,
        flags: AtomicU32::new(1 | (flags & O_NONBLOCK)),
        watches: write_watches,
    })
    .map_err(|_| FsError::NoMemory)?;
    Ok((reader, writer))
}

impl PipeEnd {
    /// 确需阻塞时建立有限源登记，之后必须重新扫描业务状态，不能直接睡眠。
    fn prepare_wait(&self) -> Result<(Arc<PollContext>, PollTable), FsError> {
        let context = PollContext::try_new()?;
        let sink: Arc<dyn PollNotify> = context.clone();
        let mut table = PollTable::new(sink, 1)?;
        self.poll(Some(&mut table))?;
        Ok((context, table))
    }

    /// 无进度返回具体错误，已交付或已提交的正前缀优先保留。
    fn progress(count: usize, error: FsError) -> Result<usize, FsError> {
        if count == 0 {
            Err(error)
        } else {
            Ok(count)
        }
    }

    /// 无读端的非零用户写发送线程定向内核信号，内核调用没有用户信号副作用。
    fn broken_pipe(&self, caller: FileIoContext<'_>, count: usize) -> Result<usize, FsError> {
        if let FileIoContext::User(user) = caller {
            user.send_kernel_signal(13);
        }
        Self::progress(count, FsError::BrokenPipe)
    }
}

impl Pollable for PipeEnd {
    /// 首轮先登记该端实际源，再以原子快照观察容量与最后端点关闭事实。
    fn poll(&self, table: Option<&mut PollTable>) -> Result<PollMask, FsError> {
        if let Some(table) = table {
            table.register(if self.read {
                &self.pipe.read_head
            } else {
                &self.pipe.write_head
            })?;
        }
        let state = self.pipe.observation.load(Ordering::Acquire);
        let mut mask = PollMask::default();
        if self.read {
            if 0 < state & BYTE_MASK {
                mask |= PollMask::POLLIN | PollMask::POLLRDNORM;
            }
            if !self.pipe.writer_open.load(Ordering::Acquire) {
                mask |= PollMask::POLLHUP;
            }
        } else {
            if 0 < state >> SPACE_SHIFT {
                mask |= PollMask::POLLOUT | PollMask::POLLWRNORM;
            }
            if !self.pipe.reader_open.load(Ordering::Acquire) {
                mask |= PollMask::POLLERR;
            }
        }
        Ok(mask)
    }
}

impl FileLike for PipeEnd {
    /// 先交付现有尾部，只消费 copyout 的实际差量；空且写 OFD 已结束才返回 EOF。
    fn read_into(
        &self,
        target: &mut ReadBuffer<'_>,
        caller: FileIoContext<'_>,
    ) -> Result<usize, FsError> {
        if !self.read {
            return Err(FsError::BadFd);
        }
        let start = target.copied();
        if target.len() == start {
            return Ok(0);
        }
        let mut waiting: Option<(Arc<PollContext>, PollTable)> = None;
        let mut interrupted = false;
        loop {
            if let Some((context, _)) = &waiting {
                context.clear_triggered();
            }
            {
                let mut ring = self.pipe.ring.lock();
                let before = target.copied();
                let result = ring.receive(target, false);
                self.pipe.publish(&ring, false, before < target.copied());
                if let Err(error) = result {
                    return Self::progress(target.copied() - start, error);
                }
                let copied = target.copied() - start;
                if 0 < copied || !self.pipe.writer_open.load(Ordering::Acquire) {
                    return Ok(copied);
                }
                if self.flags() & O_NONBLOCK != 0 {
                    return Err(FsError::WouldBlock);
                }
                if interrupted {
                    return Err(FsError::Interrupted);
                }
            }
            if let Some((context, _)) = &waiting {
                interrupted = context.park_until(matches!(caller, FileIoContext::User(_)), None)
                    == WaitResult::Interrupted;
            } else {
                waiting = Some(self.prepare_wait()?);
            }
        }
    }

    /// 管道没有可定位位置，pread 总是返回 ESPIPE。
    fn read_at_into(
        &self,
        _offset: u64,
        _target: &mut ReadBuffer<'_>,
        _caller: FileIoContext<'_>,
    ) -> Result<usize, FsError> {
        Err(FsError::IllegalSeek)
    }
    /// 匿名端点没有目录路径。
    fn path(&self) -> Option<&Path> {
        None
    }
    /// 匿名流没有文件映射 inode。
    fn mapping_inode(&self) -> Option<&InodeRef> {
        None
    }
    /// 每个端点提供自己的实际持续源。
    fn pollable(&self) -> Option<&dyn Pollable> {
        Some(self)
    }
    /// 反向登记属于此打开端点，复制描述符共享同一份末释放责任。
    fn watch_head(&self) -> Option<&Arc<WatchHead>> {
        Some(&self.watches)
    }
    /// 访问模式属于该 OFD，CLOEXEC 不进入对象。
    fn flags(&self) -> u32 {
        self.flags.load(Ordering::Acquire)
    }
    /// 只保存 Linux 可变状态位，访问模式保持不变。
    fn set_status_flags(&self, raw: u32) -> Result<(), FsError> {
        self.flags.store(
            (if self.read { 0 } else { 1 }) | (raw & (O_APPEND | O_NONBLOCK)),
            Ordering::Release,
        );
        Ok(())
    }
    /// 流不能截断。
    fn truncate(&self, _new_size: u64) -> Result<(), FsError> {
        Err(FsError::InvalidOperation)
    }
    /// 管道没有需要同步的文件存储。
    fn sync_range(&self, _range: Range<u64>, _data_only: bool) -> Result<(), FsError> {
        Err(FsError::InvalidOperation)
    }
    /// 管道没有共享字节位置。
    fn seek(&self, _from: SeekFrom) -> Result<u64, FsError> {
        Err(FsError::IllegalSeek)
    }
    /// 两端共享稳定匿名 FIFO 身份，不创建磁盘 Path、inode 或 Mount。
    fn stat(&self) -> Result<FileStat, FsError> {
        let zero = InodeTime {
            seconds: 0,
            nanoseconds: 0,
        };
        Ok(FileStat {
            inode: self.pipe.inode,
            kind: InodeKind::Fifo,
            mode: 0o010600,
            size: 0,
            uid: self.pipe.uid,
            gid: self.pipe.gid,
            links: 1,
            blocks: 0,
            device: 0,
            rdev: 0,
            block_size: PIPE_BUF as u32,
            atime: zero,
            mtime: zero,
            ctime: zero,
        })
    }
    /// 端点不提供目录项。
    fn readdir(&self) -> Result<Option<DirectoryEntry>, FsError> {
        Err(FsError::NotDirectory)
    }
    /// 端点不提供目录交付能力。
    fn for_each_dirent(
        &self,
        _emit: &mut dyn FnMut(&DirectoryEntry) -> Result<bool, FsError>,
    ) -> Result<(), FsError> {
        Err(FsError::NotDirectory)
    }

    /// 小总请求在一个状态持有期内提交，容量不足先等待；大请求保留真实正前缀。
    fn write_from(
        &self,
        source: &mut WriteBuffer<'_>,
        caller: FileIoContext<'_>,
    ) -> Result<usize, FsError> {
        if self.read {
            return Err(FsError::BadFd);
        }
        let start = source.copied();
        if source.len() == start {
            return Ok(0);
        }
        let small = source.len() <= PIPE_BUF;
        let mut written = 0;
        let mut waiting: Option<(Arc<PollContext>, PollTable)> = None;
        let mut interrupted = false;
        loop {
            if let Some((context, _)) = &waiting {
                context.clear_triggered();
            }
            {
                let mut ring = self.pipe.ring.lock();
                if !self.pipe.reader_open.load(Ordering::Acquire) {
                    return self.broken_pipe(caller, written);
                }
                let remaining = source.len() - source.copied();
                if !small || remaining <= ring.space() {
                    while source.copied() < source.len() && 0 < ring.space() {
                        if !self.pipe.reader_open.load(Ordering::Acquire) {
                            return self.broken_pipe(caller, written);
                        }
                        let expected =
                            (source.len() - source.copied()).min(if ring.merge_space() == 0 {
                                PIPE_BUF
                            } else {
                                ring.merge_space()
                            });
                        let result = ring.append(source);
                        self.pipe.publish(
                            &ring,
                            result.as_ref().is_ok_and(|count| 0 < *count),
                            false,
                        );
                        match result {
                            Ok(count) => {
                                assert!(0 < count, "pipe copyin made no progress");
                                written += count;
                                if count < expected {
                                    let error = source
                                        .pull(&mut [])
                                        .expect_err("short pipe copyin without error");
                                    return Self::progress(written, error);
                                }
                            }
                            Err(error) => return Self::progress(written, error),
                        }
                    }
                }
                if source.copied() == source.len() {
                    return Ok(written);
                }
                if self.flags() & O_NONBLOCK != 0 {
                    return Self::progress(written, FsError::WouldBlock);
                }
                if interrupted {
                    return Self::progress(written, FsError::Interrupted);
                }
            }
            if let Some((context, _)) = &waiting {
                interrupted = context.park_until(matches!(caller, FileIoContext::User(_)), None)
                    == WaitResult::Interrupted;
            } else {
                waiting = Some(match self.prepare_wait() {
                    Ok(waiting) => waiting,
                    Err(error) => return Self::progress(written, error),
                });
            }
        }
    }
    /// 管道不能执行按位置写入。
    fn write_at_from(
        &self,
        _offset: u64,
        _source: &mut WriteBuffer<'_>,
        _caller: FileIoContext<'_>,
    ) -> Result<usize, FsError> {
        Err(FsError::IllegalSeek)
    }
}

impl Drop for PipeEnd {
    /// 末OFD先同步撤全部持久watch，再发布端点关闭和通知对端；复制描述符不提前关闭。
    fn drop(&mut self) {
        self.watches.release();
        if self.read {
            self.pipe.reader_open.store(false, Ordering::Release);
            self.pipe.write_head.notify(PollMask::POLLERR);
        } else {
            self.pipe.writer_open.store(false, Ordering::Release);
            self.pipe.read_head.notify(PollMask::POLLHUP);
        }
    }
}
