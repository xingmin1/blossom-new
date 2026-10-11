//! 匿名 UNIX 流由两条独立方向拥有字节；根只保活 pair，不反持端点或打开对象。

use alloc::sync::{Arc, Weak};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use smoltcp::storage::RingBuffer;

use crate::fs::poll::{PollMask, PollTable, PollWaitHead};
use crate::fs::{FsError, ReadBuffer};
use crate::net::address::SocketAddress;
use crate::net::packet::zeroed_bytes;
use crate::net::{EndpointSlot, IoProgress, NetStack, NetStackRef};
use crate::sync::{SleepMutex, SpinLock};

const STREAM_CAPACITY: usize = 16 * 1024;
const SENDER_CLOSED: usize = 1 << 16;
const RECEIVER_CLOSED: usize = 1 << 17;
const BYTES_MASK: usize = SENDER_CLOSED - 1;
const CLOSED_MASK: usize = SENDER_CLOSED | RECEIVER_CLOSED;

/// 每个方向只串行该方向的生产、交付及关闭，用户故障不占反向锁。
struct Direction {
    state: SleepMutex<DirectionState>,
    snapshot: AtomicUsize,
}

struct DirectionState {
    ring: RingBuffer<'static, u8>,
    sender_closed: bool,
    receiver_closed: bool,
}

impl Direction {
    /// 准备真实固定容量环，尚未发表时的错误由拥有值完整回收。
    fn try_new() -> Result<Self, FsError> {
        Ok(Self {
            state: SleepMutex::new(DirectionState {
                ring: RingBuffer::new(zeroed_bytes(STREAM_CAPACITY)?),
                sender_closed: false,
                receiver_closed: false,
            }),
            snapshot: AtomicUsize::new(0),
        })
    }

    /// 持方向保护发布只读快照，poll 不等待正在交付用户字节的方向。
    fn publish(&self, state: &DirectionState) {
        let snapshot = state.ring.len()
            | if state.sender_closed {
                SENDER_CLOSED
            } else {
                0
            }
            | if state.receiver_closed {
                RECEIVER_CLOSED
            } else {
                0
            };
        self.snapshot.store(snapshot, Ordering::Release);
    }
}

/// 两个端点共享字节与持续源；弱域归属不形成根到打开对象的强引用环。
pub struct UnixPair {
    id: u64,
    net: Weak<NetStack>,
    /// 下标是生产侧，接收侧为另一个下标。
    directions: [Direction; 2],
    sources: [Arc<PollWaitHead>; 2],
    errors: [SpinLock<Option<FsError>>; 2],
    closed: [AtomicBool; 2],
    stopped: AtomicBool,
}

/// 每侧节点持自己的真实额度，最后节点拥有结束才归还该额度。
pub struct UnixEndpoint {
    id: u64,
    side: usize,
    pair: Arc<UnixPair>,
    reuse: AtomicBool,
    _slot: EndpointSlot,
}

impl UnixPair {
    /// 完整准备两个环、持续源及节点；根只能在全部成功后取得 pair 拥有。
    pub fn try_new(
        net: &NetStackRef,
        ids: [u64; 2],
        slots: [EndpointSlot; 2],
    ) -> Result<(Arc<Self>, [Arc<UnixEndpoint>; 2]), FsError> {
        let pair = Arc::try_new(Self {
            id: ids[0],
            net: Arc::downgrade(net),
            directions: [Direction::try_new()?, Direction::try_new()?],
            sources: [
                Arc::try_new(PollWaitHead::new()).map_err(|_| FsError::NoMemory)?,
                Arc::try_new(PollWaitHead::new()).map_err(|_| FsError::NoMemory)?,
            ],
            errors: [SpinLock::new(None), SpinLock::new(None)],
            closed: [AtomicBool::new(false), AtomicBool::new(false)],
            stopped: AtomicBool::new(false),
        })
        .map_err(|_| FsError::NoMemory)?;
        let [first_slot, second_slot] = slots;
        let first = Arc::try_new(UnixEndpoint {
            id: ids[0],
            side: 0,
            pair: Arc::clone(&pair),
            reuse: AtomicBool::new(false),
            _slot: first_slot,
        })
        .map_err(|_| FsError::NoMemory)?;
        let second = Arc::try_new(UnixEndpoint {
            id: ids[1],
            side: 1,
            pair: Arc::clone(&pair),
            reuse: AtomicBool::new(false),
            _slot: second_slot,
        })
        .map_err(|_| FsError::NoMemory)?;
        Ok((pair, [first, second]))
    }

    /// 根表以第一侧端点的本域身份登记这份 pair。
    pub fn id(&self) -> u64 {
        self.id
    }

    /// 从真实环提交的快照查询就绪，不获取睡眠锁或产生通知。
    fn mask(&self, side: usize) -> PollMask {
        let incoming = self.directions[1 - side].snapshot.load(Ordering::Acquire);
        let outgoing = self.directions[side].snapshot.load(Ordering::Acquire);
        let read_closed = incoming & CLOSED_MASK != 0;
        let write_closed = outgoing & CLOSED_MASK != 0;
        let mut mask = PollMask::default();
        if incoming & BYTES_MASK != 0 || read_closed {
            mask |= PollMask::POLLIN | PollMask::POLLRDNORM;
        }
        if read_closed {
            mask |= PollMask::POLLRDHUP;
        }
        // 关闭后的发送会立即报错，仍须允许等待发送的任务重新查询。
        if outgoing & BYTES_MASK < STREAM_CAPACITY || write_closed {
            mask |= PollMask::POLLOUT | PollMask::POLLWRNORM;
        }
        if read_closed && write_closed {
            mask |= PollMask::POLLHUP;
        }
        if self.errors[side].lock().is_some() {
            mask |= PollMask::POLLERR;
        }
        mask
    }

    /// 一次只取得一个方向保护，提交关闭后解锁再报告真实方向变化。
    fn close_direction(
        &self,
        sender: usize,
        close_sender: bool,
        close_receiver: bool,
        discard: bool,
    ) {
        let receiver = 1 - sender;
        let direction = &self.directions[sender];
        let (changed, reset) = {
            let mut state = direction.state.lock();
            let was_closed = state.sender_closed || state.receiver_closed;
            let reset = discard && !state.ring.is_empty();
            state.sender_closed |= close_sender;
            state.receiver_closed |= close_receiver;
            if discard {
                state.ring.clear();
            }
            if reset {
                self.errors[sender]
                    .lock()
                    .get_or_insert(FsError::ConnectionReset);
            }
            direction.publish(&state);
            (
                !was_closed && (state.sender_closed || state.receiver_closed),
                reset,
            )
        };
        if changed {
            self.sources[receiver]
                .notify(PollMask::POLLIN | PollMask::POLLRDNORM | PollMask::POLLRDHUP);
            self.sources[sender].notify(PollMask::POLLOUT | PollMask::POLLWRNORM);
            if self.mask(receiver).contains(PollMask::POLLHUP) {
                self.sources[receiver].notify(PollMask::POLLHUP);
            }
            if self.mask(sender).contains(PollMask::POLLHUP) {
                self.sources[sender].notify(PollMask::POLLHUP);
            }
        }
        if reset {
            self.sources[sender].notify(PollMask::POLLERR);
        }
    }

    /// 末端放弃自己的未读字节，保留已交给 peer 环的字节；撤根拥有在方向锁外。
    fn close_side(&self, side: usize) {
        self.close_direction(1 - side, true, true, true);
        self.close_direction(side, true, true, false);
        self.closed[side].store(true, Ordering::Release);
        if self.closed[1 - side].load(Ordering::Acquire) {
            if let Some(net) = self.net.upgrade() {
                let removed = net.tables().take_unix(self.id);
                drop(removed);
            }
        }
    }

    /// 根停止拒绝后继数据操作，逐方向等待当前 copyout、消费及快照提交，再锁外通知关闭。
    /// 外部调用的锁外通知可与停止交错，其端点强拥有保活 pair 和源；这里不等待外部 Arc 或调用返回。
    /// slot 随各节点最后强拥有归还，环随 pair 最后强拥有归还；资源基线须在外部线程真实结束后核对。
    pub(crate) fn stop_sync(&self) {
        self.stopped.store(true, Ordering::Release);
        self.close_side(0);
        self.close_side(1);
    }
}

impl UnixEndpoint {
    /// 匿名 inode 身份来自本域端点编号，两侧不会共享同一个编号。
    pub fn id(&self) -> u64 {
        self.id
    }

    /// 比较真实弱域归属，不以地址族或匿名名称猜测网络域。
    pub fn belongs_to(&self, net: &NetStackRef) -> bool {
        Weak::ptr_eq(&self.pair.net, &Arc::downgrade(net))
    }

    /// 接收只占 incoming 方向；结束环借用后 copyout，按实际差量消费前缀。
    pub fn receive(&self, target: &mut ReadBuffer<'_>, peek: bool) -> Result<IoProgress, FsError> {
        let direction = &self.pair.directions[1 - self.side];
        let mut state = direction.state.lock();
        self.check_open()?;
        if target.copied() == target.len() {
            return Ok(IoProgress::Ready(0));
        }
        if state.ring.is_empty() {
            if let Some(error) = self.pair.errors[self.side].lock().take() {
                return Err(error);
            }
            return Ok(if state.sender_closed || state.receiver_closed {
                IoProgress::Eof
            } else {
                IoProgress::Blocked
            });
        }
        let before = target.copied();
        let mut prefix = [0u8; 4096];
        let capacity = (target.len() - before).min(prefix.len());
        let length = state.ring.read_allocated(0, &mut prefix[..capacity]);
        let result = target.push(&prefix[..length]);
        let copied = target.copied() - before;
        if !peek && 0 < copied {
            state.ring.dequeue_allocated(copied);
            direction.publish(&state);
        }
        let writable = !state.sender_closed && !state.receiver_closed;
        drop(state);
        if !peek && 0 < copied && writable {
            self.pair.sources[1 - self.side].notify(PollMask::POLLOUT | PollMask::POLLWRNORM);
        }
        if 0 < copied {
            Ok(IoProgress::Ready(copied))
        } else {
            result.map(|_| IoProgress::Ready(0))
        }
    }

    /// 只提交调用者已准备的内核前缀；空输入也先检查真实错误和发送关闭事实。
    pub fn send(&self, source: &[u8]) -> Result<IoProgress, FsError> {
        let direction = &self.pair.directions[self.side];
        let mut state = direction.state.lock();
        self.check_open()?;
        if state.sender_closed || state.receiver_closed {
            return Err(FsError::BrokenPipe);
        }
        if let Some(error) = self.pair.errors[self.side].lock().take() {
            return Err(error);
        }
        if source.is_empty() {
            return Ok(IoProgress::Ready(0));
        }
        let accepted = state.ring.enqueue_slice(source);
        if accepted == 0 {
            return Ok(IoProgress::Blocked);
        }
        direction.publish(&state);
        drop(state);
        self.pair.sources[1 - self.side].notify(PollMask::POLLIN | PollMask::POLLRDNORM);
        Ok(IoProgress::Ready(accepted))
    }

    /// 指定方向关闭保留已有 incoming；任一路径均先释放一方向再取得另一方向。
    pub fn shutdown(&self, how: u32) -> Result<(), FsError> {
        if 2 < how {
            return Err(FsError::InvalidOperation);
        }
        self.check_open()?;
        if how == 0 || how == 2 {
            self.pair.close_direction(1 - self.side, false, true, false);
        }
        if how == 1 || how == 2 {
            self.pair.close_direction(self.side, true, false, false);
        }
        Ok(())
    }

    /// 匿名本地名称仅编码 AF_UNIX，不制造路径或 IPv4 地址。
    pub fn local_address(&self) -> Result<SocketAddress, FsError> {
        self.check_open()?;
        Ok(SocketAddress::UnixUnnamed)
    }

    /// pair 的匿名 peer 名称不随对端末关闭改变，本侧拥有结束前仍保留关联。
    pub fn peer_address(&self) -> Result<SocketAddress, FsError> {
        self.check_open()?;
        Ok(SocketAddress::UnixUnnamed)
    }

    /// 首轮只登记本侧持续源，查询不等待任何用户复制或改变事件事实。
    pub fn poll(&self, table: Option<&mut PollTable>) -> Result<PollMask, FsError> {
        if let Some(table) = table {
            table.register(&self.pair.sources[self.side])?;
        }
        Ok(self.pair.mask(self.side))
    }

    /// SO_ERROR 领取真正未读关闭造成的错误，查询消费本身不产生通知。
    pub fn take_error(&self) -> Option<FsError> {
        self.pair.errors[self.side].lock().take()
    }

    /// 查询当前真实固定接收存储容量，不承诺动态修改缓冲。
    pub fn receive_capacity(&self) -> usize {
        STREAM_CAPACITY
    }

    /// 查询 outgoing 环的固定容量，不套用 skb 记账翻倍。
    pub fn send_capacity(&self) -> usize {
        STREAM_CAPACITY
    }

    /// 匿名 socket 保留一般复用选项事实，本版本没有 UNIX 绑定索引。
    pub fn reuse_address(&self) -> bool {
        self.reuse.load(Ordering::Acquire)
    }

    /// 设置本侧一般 socket 选项，不据此发表路径或改变 pair 连接。
    pub fn set_reuse_address(&self, reuse: bool) -> Result<(), FsError> {
        let _direction = self.pair.directions[self.side].state.lock();
        self.check_open()?;
        self.reuse.store(reuse, Ordering::Release);
        Ok(())
    }

    /// 拒绝已经完成末关闭或根停止的对象，根停止不能被残留 OFD 重新使用。
    fn check_open(&self) -> Result<(), FsError> {
        if self.pair.stopped.load(Ordering::Acquire)
            || self.pair.closed[self.side].load(Ordering::Acquire)
        {
            return Err(FsError::NotConnected);
        }
        Ok(())
    }

    /// 末 OFD 在撤 watch 后关闭本侧；此处不等待 peer 节点拥有结束。
    pub fn close(&self) {
        self.pair.close_side(self.side);
    }
}

impl Drop for UnixEndpoint {
    /// 未发表节点也结束自己的方向，最后 slot 与 pair 拥有由字段自然归还。
    fn drop(&mut self) {
        self.close();
    }
}
