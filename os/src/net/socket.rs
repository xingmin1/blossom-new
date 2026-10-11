//! 打开对象和 TCP 协议拥有分离；本连接的睡眠互斥不限制其它核心。

use alloc::sync::Arc;
use core::ops::Range;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use crate::fs::epoll::WatchHead;
use crate::fs::poll::{PollContext, PollMask, PollNotify, PollTable, Pollable};
use crate::fs::{DirectoryEntry, FileStat, FsError, InodeKind, InodeRef, InodeTime, Path};
use crate::fs::{FileIoContext, FileLike, ReadBuffer, SeekFrom, WriteBuffer};
use crate::net::address::{InetEndpoint, SocketAddress};
use crate::net::tcp::TcpEndpoint;
use crate::net::{IoProgress, MessageResult, NetStackRef};
use crate::wait::WaitResult;

pub const TCP_BUFFER_LEN: usize = 16 * 1024;
const O_RDWR: u32 = 2;
const O_APPEND: u32 = 0o2000;
const O_NONBLOCK: u32 = 0o4000;
const O_CLOEXEC: u32 = 0o2000000;
const MSG_PEEK: u32 = 0x2;
const MSG_DONTWAIT: u32 = 0x40;
const MSG_NOSIGNAL: u32 = 0x4000;
const STREAM_PREFIX_LEN: usize = 4096;

/// 打开对象持稳定协议端点、网络域和创建者凭据，并保存共享 OFD 状态位。
/// 构造只准备打开实例，不发表用户 fd。
pub struct SocketFile {
    endpoint: Arc<TcpEndpoint>,
    net: NetStackRef,
    uid: u32,
    gid: u32,
    flags: AtomicU32,
    watches: Arc<WatchHead>,
    /// 阻塞 connect 尚未领取完成时保留该事实，信号重启仍消费原握手结果。
    connect_unclaimed: AtomicBool,
    opened: bool,
}

impl SocketFile {
    /// 从同一网络域准备默认有界 TCP 核心与新的打开实例，CLOEXEC 留给描述符槽。
    pub fn try_new_tcp(
        net: NetStackRef,
        flags: u32,
        uid: u32,
        gid: u32,
    ) -> Result<Arc<Self>, FsError> {
        if flags & !(O_NONBLOCK | O_CLOEXEC) != 0 {
            return Err(FsError::InvalidOperation);
        }
        let endpoint = net.try_new_tcp(TCP_BUFFER_LEN, TCP_BUFFER_LEN)?;
        Self::allocate(net, endpoint, flags, uid, gid)
    }

    /// 为已拥有的本域协议端点构造独立 OFD，不搬走 raw 核心或替换稳定 listener。
    pub fn from_endpoint(
        net: NetStackRef,
        endpoint: Arc<TcpEndpoint>,
        flags: u32,
        uid: u32,
        gid: u32,
    ) -> Result<Arc<Self>, FsError> {
        if flags & !(O_NONBLOCK | O_CLOEXEC) != 0 {
            return Err(FsError::InvalidOperation);
        }
        if !endpoint.belongs_to(&net) {
            return Err(FsError::InvalidOperation);
        }
        Self::allocate(net, endpoint, flags, uid, gid)
    }

    /// 分配失败撤回未交给打开实例的协议拥有，成功后才赋予末 OFD 关闭责任。
    fn allocate(
        net: NetStackRef,
        endpoint: Arc<TcpEndpoint>,
        flags: u32,
        uid: u32,
        gid: u32,
    ) -> Result<Arc<Self>, FsError> {
        let watches = match Arc::try_new(WatchHead::new()) {
            Ok(watches) => watches,
            Err(_) => {
                endpoint.abandon();
                return Err(FsError::NoMemory);
            }
        };
        let result = Arc::try_new(Self {
            endpoint: endpoint.clone(),
            net,
            uid,
            gid,
            flags: AtomicU32::new(O_RDWR | (flags & O_NONBLOCK)),
            watches,
            connect_unclaimed: AtomicBool::new(false),
            opened: false,
        });
        match result {
            Ok(mut file) => {
                Arc::get_mut(&mut file)
                    .expect("unpublished socket file is shared")
                    .opened = true;
                Ok(file)
            }
            Err(_) => {
                endpoint.abandon();
                Err(FsError::NoMemory)
            }
        }
    }

    /// 借用稳定端点拥有者，协议索引可 clone 此 Arc 而不保整张文件表。
    pub fn endpoint(&self) -> &Arc<TcpEndpoint> {
        &self.endpoint
    }

    /// 返回构造时的同一网络域；dup 和 fork 共享打开实例及这个引用。
    pub fn net(&self) -> &NetStackRef {
        &self.net
    }

    /// 访问模式固定为双向流，非阻塞状态属于真正的 OFD。
    pub fn flags(&self) -> u32 {
        self.flags.load(Ordering::Acquire)
    }

    /// 沿文件对象已有规则保存可变状态位，descriptor 的 CLOEXEC 不进入 OFD。
    pub fn set_status_flags(&self, raw: u32) -> Result<(), FsError> {
        self.flags
            .store(O_RDWR | (raw & (O_APPEND | O_NONBLOCK)), Ordering::Release);
        Ok(())
    }

    /// 在本域预留真实地址，通配与自动端口由原端点统一处理。
    pub fn bind(&self, local: InetEndpoint) -> Result<(), FsError> {
        self.endpoint.bind(local).map(|_| ())
    }

    /// 更新稳定 listener 的有限 backlog，原握手和 ready child 仍归同一端点。
    pub fn listen(&self, backlog: usize) -> Result<(), FsError> {
        self.endpoint.listen(backlog)
    }

    /// 首次发起实际握手；阻塞重启只等待既有握手，不再次发表四元组。
    /// 中断或等待资源准备失败保留未领取结果，领取实际成功或连接失败结果后才清除。
    pub fn connect(&self, peer: InetEndpoint, caller: FileIoContext<'_>) -> Result<(), FsError> {
        let nonblock = self.flags() & O_NONBLOCK != 0;
        match self.endpoint.connect(peer) {
            Ok(()) if nonblock => return Err(FsError::InProgress),
            Ok(()) => self.connect_unclaimed.store(true, Ordering::Release),
            Err(FsError::AlreadyInProgress) if !nonblock => {
                self.connect_unclaimed.store(true, Ordering::Release);
            }
            Err(FsError::AlreadyConnected)
                if !nonblock && self.connect_unclaimed.load(Ordering::Acquire) => {}
            Err(error) => return Err(error),
        }
        let mut waiting: Option<(Arc<PollContext>, PollTable)> = None;
        let mut interrupted = false;
        loop {
            if let Some((context, _)) = &waiting {
                context.clear_triggered();
            }
            match self.endpoint.connection_result() {
                Ok(Some(())) => {
                    self.connect_unclaimed.store(false, Ordering::Release);
                    return Ok(());
                }
                Err(error) => {
                    self.connect_unclaimed.store(false, Ordering::Release);
                    return Err(error);
                }
                Ok(None) => {}
            }
            if interrupted {
                return Err(FsError::Interrupted);
            }
            if let Some((context, _)) = &waiting {
                interrupted = context.park_until(matches!(caller, FileIoContext::User(_)), None)
                    == WaitResult::Interrupted;
            } else {
                waiting = Some(match self.prepare_wait() {
                    Ok(waiting) => waiting,
                    Err(error) => {
                        return Err(error);
                    }
                });
            }
        }
    }

    /// 从稳定 listener 领取成熟 child，为它准备新 OFD；新状态位只来自 accept4。
    pub fn accept(
        &self,
        flags: u32,
        uid: u32,
        gid: u32,
        caller: FileIoContext<'_>,
    ) -> Result<Arc<SocketFile>, FsError> {
        if flags & !(O_NONBLOCK | O_CLOEXEC) != 0 {
            return Err(FsError::InvalidOperation);
        }
        let mut waiting: Option<(Arc<PollContext>, PollTable)> = None;
        let mut interrupted = false;
        loop {
            if let Some((context, _)) = &waiting {
                context.clear_triggered();
            }
            if let Some(child) = self.endpoint.accept()? {
                return Self::from_endpoint(self.net.clone(), child, flags, uid, gid);
            }
            if self.flags() & O_NONBLOCK != 0 {
                return Err(FsError::WouldBlock);
            }
            if interrupted {
                return Err(FsError::Interrupted);
            }
            if let Some((context, _)) = &waiting {
                interrupted = context.park_until(matches!(caller, FileIoContext::User(_)), None)
                    == WaitResult::Interrupted;
            } else {
                waiting = Some(self.prepare_wait()?);
            }
        }
    }

    /// 交付同一 TCP 流的连续前缀，PEEK 不消费；名称沿流语义保持为空。
    pub fn receive(
        &self,
        target: &mut ReadBuffer<'_>,
        flags: u32,
        caller: FileIoContext<'_>,
    ) -> Result<MessageResult, FsError> {
        if flags & !(MSG_DONTWAIT | MSG_PEEK) != 0 {
            return Err(FsError::Unsupported);
        }
        let start = target.copied();
        if start == target.len() {
            return Ok(Self::stream_result(0));
        }
        let mut waiting: Option<(Arc<PollContext>, PollTable)> = None;
        let mut interrupted = false;
        loop {
            if let Some((context, _)) = &waiting {
                context.clear_triggered();
            }
            match self.endpoint.receive(target, flags & MSG_PEEK != 0) {
                Ok(IoProgress::Ready(count)) => return Ok(Self::stream_result(count)),
                Ok(IoProgress::Eof) => return Ok(Self::stream_result(0)),
                Err(error) => {
                    return Self::progress(target.copied() - start, error).map(Self::stream_result);
                }
                Ok(IoProgress::Blocked) => {}
            }
            if self.nonblocking(flags) {
                return Err(FsError::WouldBlock);
            }
            if interrupted {
                return Err(FsError::Interrupted);
            }
            if let Some((context, _)) = &waiting {
                interrupted = context.park_until(matches!(caller, FileIoContext::User(_)), None)
                    == WaitResult::Interrupted;
            } else {
                waiting = Some(self.prepare_wait()?);
            }
        }
    }

    /// 锁外取得至多一段用户前缀，再提交 raw 实际接受量；Blocked 保留原段重试。
    pub fn send(
        &self,
        source: &mut WriteBuffer<'_>,
        flags: u32,
        caller: FileIoContext<'_>,
    ) -> Result<usize, FsError> {
        if flags & !(MSG_DONTWAIT | MSG_NOSIGNAL) != 0 {
            return Err(FsError::Unsupported);
        }
        if source.copied() == source.len() {
            return Ok(0);
        }
        let mut prefix = [0u8; STREAM_PREFIX_LEN];
        let prepared = source.pull(&mut prefix)?;
        assert!(0 < prepared, "nonempty socket input made no progress");
        let mut waiting: Option<(Arc<PollContext>, PollTable)> = None;
        let mut interrupted = false;
        loop {
            if let Some((context, _)) = &waiting {
                context.clear_triggered();
            }
            // 临时输入只借内核前缀；端点调用结束后已释放核心和 raw 借用。
            let result = self
                .endpoint
                .send(&mut WriteBuffer::kernel(&prefix[..prepared]));
            match result {
                Ok(IoProgress::Ready(count)) => return Ok(count),
                Ok(IoProgress::Blocked) => {}
                Ok(IoProgress::Eof) => unreachable!("TCP send returned receive EOF"),
                Err(FsError::BrokenPipe) => {
                    if flags & MSG_NOSIGNAL == 0 {
                        if let FileIoContext::User(user) = caller {
                            user.send_kernel_signal(13);
                        }
                    }
                    return Err(FsError::BrokenPipe);
                }
                Err(error) => return Err(error),
            }
            if self.nonblocking(flags) {
                return Err(FsError::WouldBlock);
            }
            if interrupted {
                return Err(FsError::Interrupted);
            }
            if let Some((context, _)) = &waiting {
                interrupted = context.park_until(matches!(caller, FileIoContext::User(_)), None)
                    == WaitResult::Interrupted;
            } else {
                waiting = Some(self.prepare_wait()?);
            }
        }
    }

    /// 关闭指定流方向，发送 FIN 后接收仍使用相同端点。
    pub fn shutdown(&self, how: u32) -> Result<(), FsError> {
        self.endpoint.shutdown(how)
    }

    /// 查询实际绑定或路由选择后的本地地址，不制造流消息来源。
    pub fn local_address(&self) -> Result<SocketAddress, FsError> {
        self.endpoint.local_address().map(SocketAddress::Inet)
    }

    /// 只返回真正已连接的对端，未连接状态由协议对象判定。
    pub fn peer_address(&self) -> Result<SocketAddress, FsError> {
        self.endpoint.peer_address().map(SocketAddress::Inet)
    }

    /// 消费一次协议实际记录的 pending error，不发起连接或推进协议。
    pub fn take_error(&self) -> Option<FsError> {
        self.endpoint.take_error()
    }

    /// 观察同一协议端点是否为稳定 listener。
    pub fn is_listener(&self) -> bool {
        self.endpoint.is_listener()
    }

    /// 查询实际接收 ring 容量，不把 Linux 缓冲翻倍规则套到 raw 存储。
    pub fn receive_capacity(&self) -> usize {
        self.endpoint.observe().receive_capacity()
    }

    /// 查询实际发送 ring 容量，接受量与空闲量仍由数据面决定。
    pub fn send_capacity(&self) -> usize {
        self.endpoint.observe().send_capacity()
    }

    /// 真实 Blocked 后才准备原等待上下文与一源表，登记结束后由调用者重新扫描。
    fn prepare_wait(&self) -> Result<(Arc<PollContext>, PollTable), FsError> {
        let context = PollContext::try_new()?;
        let sink: Arc<dyn PollNotify> = context.clone();
        let mut table = PollTable::new(sink, 1)?;
        self.poll(Some(&mut table))?;
        Ok((context, table))
    }

    /// 每次扫描共同消费 OFD 状态和本次消息的非阻塞要求。
    fn nonblocking(&self, flags: u32) -> bool {
        self.flags() & O_NONBLOCK != 0 || flags & MSG_DONTWAIT != 0
    }

    /// TCP 的消息元数据只描述本次实际交付的流前缀。
    fn stream_result(count: usize) -> MessageResult {
        MessageResult {
            copied: count,
            original_len: count,
            source: None,
            flags: 0,
        }
    }

    /// 已交付正前缀优先于后续 fault 或协议错误，零进度保留原错误。
    fn progress(count: usize, error: FsError) -> Result<usize, FsError> {
        if count == 0 {
            Err(error)
        } else {
            Ok(count)
        }
    }

    /// 匿名 socket 属性使用本域稳定身份和创建者凭据，不伪装 regular 或磁盘路径。
    pub fn stat(&self) -> FileStat {
        let zero = InodeTime {
            seconds: 0,
            nanoseconds: 0,
        };
        FileStat {
            inode: self.endpoint.id(),
            kind: InodeKind::Socket,
            mode: 0o140777,
            size: 0,
            uid: self.uid,
            gid: self.gid,
            links: 1,
            blocks: 0,
            device: 0,
            rdev: 0,
            block_size: self.net.loopback_mtu() as u32,
            atime: zero,
            mtime: zero,
            ctime: zero,
        }
    }
}

impl Pollable for SocketFile {
    /// 首轮登记端点唯一持续源，其后只查询原协议状态，不收包或 dispatch。
    fn poll(&self, table: Option<&mut PollTable>) -> Result<PollMask, FsError> {
        self.endpoint.poll(table)
    }
}

impl FileLike for SocketFile {
    /// 普通 read/readv 与 recv 使用同一流交付及等待路径。
    fn read_into(
        &self,
        target: &mut ReadBuffer<'_>,
        caller: FileIoContext<'_>,
    ) -> Result<usize, FsError> {
        self.receive(target, 0, caller).map(|result| result.copied)
    }

    /// TCP 流没有可定位位置，pread 不访问用户数据即返回 ESPIPE。
    fn read_at_into(
        &self,
        _offset: u64,
        _target: &mut ReadBuffer<'_>,
        _caller: FileIoContext<'_>,
    ) -> Result<usize, FsError> {
        Err(FsError::IllegalSeek)
    }

    /// 匿名 socket 不提供目录路径。
    fn path(&self) -> Option<&Path> {
        None
    }

    /// 流数据不提供文件映射 inode。
    fn mapping_inode(&self) -> Option<&InodeRef> {
        None
    }

    /// 事件扫描器直接观察本 OFD 对应的端点源。
    fn pollable(&self) -> Option<&dyn Pollable> {
        Some(self)
    }

    /// 网络控制只取得此真实 SocketFile，不从 stat 类型猜协议对象。
    fn socket(&self) -> Option<&SocketFile> {
        Some(self)
    }

    /// 复制描述符共享同一反向 watch 和末 OFD 注销责任。
    fn watch_head(&self) -> Option<&Arc<WatchHead>> {
        Some(&self.watches)
    }

    /// 读取共享 OFD 状态，CLOEXEC 始终归描述符槽。
    fn flags(&self) -> u32 {
        SocketFile::flags(self)
    }

    /// F_SETFL 修改原 OFD 状态，不改变双向访问模式。
    fn set_status_flags(&self, raw: u32) -> Result<(), FsError> {
        SocketFile::set_status_flags(self, raw)
    }

    /// 网络流没有文件长度可截断。
    fn truncate(&self, _new_size: u64) -> Result<(), FsError> {
        Err(FsError::InvalidOperation)
    }

    /// 网络流没有文件存储同步范围。
    fn sync_range(&self, _range: Range<u64>, _data_only: bool) -> Result<(), FsError> {
        Err(FsError::InvalidOperation)
    }

    /// 流没有共享字节位置，不创建虚假游标。
    fn seek(&self, _from: SeekFrom) -> Result<u64, FsError> {
        Err(FsError::IllegalSeek)
    }

    /// fstat 使用真实创建者、本域稳定身份及 S_IFSOCK0777。
    fn stat(&self) -> Result<FileStat, FsError> {
        Ok(SocketFile::stat(self))
    }

    /// 匿名 socket 不能读取目录项。
    fn readdir(&self) -> Result<Option<DirectoryEntry>, FsError> {
        Err(FsError::NotDirectory)
    }

    /// 匿名 socket 不授予目录枚举能力。
    fn for_each_dirent(
        &self,
        _emit: &mut dyn FnMut(&DirectoryEntry) -> Result<bool, FsError>,
    ) -> Result<(), FsError> {
        Err(FsError::NotDirectory)
    }

    /// 普通 write/writev 与 send 共用 raw 接受量及调用线程 SIGPIPE 路径。
    fn write_from(
        &self,
        source: &mut WriteBuffer<'_>,
        caller: FileIoContext<'_>,
    ) -> Result<usize, FsError> {
        self.send(source, 0, caller)
    }

    /// TCP 流不能执行 pwrite。
    fn write_at_from(
        &self,
        _offset: u64,
        _source: &mut WriteBuffer<'_>,
        _caller: FileIoContext<'_>,
    ) -> Result<usize, FsError> {
        Err(FsError::IllegalSeek)
    }
}

impl Drop for SocketFile {
    /// 末打开实例先同步撤反向 watch，再请求正常协议关闭，不等待 peer 或后台 Work。
    fn drop(&mut self) {
        self.watches.release();
        if self.opened {
            self.endpoint.close();
        }
    }
}
