//! 每个 TCP 端点独立拥有核心、输入和期限；表与退休拥有覆盖回调的完整寿命。

use alloc::collections::VecDeque;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::ops::{Deref, DerefMut};
use core::pin::Pin;
use core::sync::atomic::{AtomicBool, Ordering};
use smoltcp::socket::tcp::State;
use smoltcp::wire::TcpControl;
use spin::Once;

use crate::cpu;
use crate::fs::FsError;
use crate::net::address::InetEndpoint;
use crate::net::core::{ProtocolDeadline, TcpCore};
use crate::net::packet::OwnedPacket;
use crate::net::table::{BindLease, TcpTuple, TransportProtocol};
use crate::net::{
    EndpointSlot, NetStack, NetStackRef, ENDPOINT_CAPACITY, LOOPBACK_CAPACITY, PROTOCOL_BUDGET,
};
use crate::sync::{SleepMutex, SleepMutexGuard, SpinLock};
use crate::time;
use crate::timer::{Timer, TimerKind};
use crate::wait::WaitQueue;
use crate::workqueue::Work;

/// 输入在用户持有核心时保持拥有，立即回应未接受时也留在同一节点。
struct Input {
    closed: bool,
    // 真实 Work 正在尝试或等待操作拥有，释放者移交此责任；它不代表另一个 pending 票。
    owner_waiting: bool,
    packets: VecDeque<OwnedPacket>,
    reply: Option<OwnedPacket>,
}

/// 绑定和 listener 关联只保存真实拥有，不复制 raw 协议状态。
struct Metadata {
    lease: Option<Arc<BindLease>>,
    parent: Weak<TcpEndpoint>,
    engaged: bool,
    error: Option<FsError>,
}

/// 半连接与可接受连接共享实际 backlog 上限，只有成熟 Established 才进入 ready。
struct Children {
    limit: usize,
    pending: Vec<Arc<TcpEndpoint>>,
    ready: VecDeque<Arc<TcpEndpoint>>,
}

/// 表强持稳定节点，节点弱指网络域和 listener；普通 Work 固定轮转 CPU。
pub struct TcpEndpoint {
    id: u64,
    stack: Weak<NetStack>,
    core: SleepMutex<TcpCore>,
    input: SpinLock<Input>,
    metadata: SpinLock<Metadata>,
    children: SpinLock<Children>,
    changed: WaitQueue,
    work: Once<Pin<Arc<Work>>>,
    timer: Once<Pin<Arc<Timer>>>,
    stopped: AtomicBool,
    _slot: EndpointSlot,
}

/// 修改操作主动安排协议；只读观察只移交已经受阻的真实 Work。
#[derive(Clone, Copy, Eq, PartialEq)]
enum OperationMode {
    Modify,
    Observe,
}

/// 普通操作保护只在释放核心后安排同一个协议 Work。
pub struct TcpOperationGuard<'a> {
    endpoint: &'a TcpEndpoint,
    core: Option<SleepMutexGuard<'a, TcpCore>>,
    mode: OperationMode,
}

impl Deref for TcpOperationGuard<'_> {
    type Target = TcpCore;
    /// 借用本次真正持有的核心保护，不创建第二个协议对象。
    fn deref(&self) -> &TcpCore {
        self.core.as_deref().unwrap()
    }
}

impl DerefMut for TcpOperationGuard<'_> {
    /// 可变借用仅在本连接 SleepMutex 的操作拥有期间存在。
    fn deref_mut(&mut self) -> &mut TcpCore {
        self.core.as_deref_mut().unwrap()
    }
}

impl Drop for TcpOperationGuard<'_> {
    /// 先释放核心，再以输入短锁领取真实受阻事实，封住失锁登记和拥有释放之间的窗口。
    fn drop(&mut self) {
        drop(self.core.take());
        let owner_waiting = {
            let mut input = self.endpoint.input.lock();
            core::mem::take(&mut input.owner_waiting)
        };
        if self.mode == OperationMode::Modify || owner_waiting {
            self.endpoint.schedule();
        }
    }
}

impl TcpEndpoint {
    /// 根取得额度和核心后准备所有异步节点；未发表失败对象从未产生回调票。
    pub(crate) fn try_new(
        net: &NetStackRef,
        id: u64,
        core: TcpCore,
        slot: EndpointSlot,
    ) -> Result<Arc<Self>, FsError> {
        let mut packets = VecDeque::new();
        packets
            .try_reserve_exact(LOOPBACK_CAPACITY)
            .map_err(|_| FsError::NoMemory)?;
        let endpoint = Arc::try_new(Self {
            id,
            stack: Arc::downgrade(net),
            core: SleepMutex::new(core),
            input: SpinLock::new(Input {
                closed: false,
                owner_waiting: false,
                packets,
                reply: None,
            }),
            metadata: SpinLock::new(Metadata {
                lease: None,
                parent: Weak::new(),
                engaged: false,
                error: None,
            }),
            children: SpinLock::new(Children {
                limit: 0,
                pending: Vec::new(),
                ready: VecDeque::new(),
            }),
            changed: WaitQueue::new(),
            work: Once::new(),
            timer: Once::new(),
            stopped: AtomicBool::new(false),
            _slot: slot,
        })
        .map_err(|_| FsError::NoMemory)?;
        let cpu = ((id - 1) % cpu::count() as u64) as usize;
        let context = Arc::as_ptr(&endpoint) as usize;
        let work = Work::try_new(net.work_queue(), cpu, run_tcp, context)
            .map_err(|_| FsError::NoMemory)?;
        let timer = Timer::try_new(cpu, TimerKind::Precise, tcp_expired, context)
            .map_err(|_| FsError::NoMemory)?;
        endpoint.work.call_once(|| work);
        endpoint.timer.call_once(|| timer);
        Ok(endpoint)
    }

    /// 返回本域不复用的身份，描述符编号不进入协议索引。
    pub fn id(&self) -> u64 {
        self.id
    }

    /// 返回普通 Work 和精确 Timer 共同固定的执行 CPU。
    pub fn work_cpu(&self) -> usize {
        self.work.get().unwrap().cpu()
    }

    /// 只比较弱域与真实 Arc 的身份，不延长域的服务寿命。
    pub fn belongs_to(&self, net: &NetStackRef) -> bool {
        self.stack.as_ptr() == Arc::as_ptr(net)
    }

    /// 普通任务取得本连接拥有，结束后由 guard 安排积存输入和发送责任。
    pub fn lock(&self) -> TcpOperationGuard<'_> {
        TcpOperationGuard {
            endpoint: self,
            core: Some(self.core.lock()),
            mode: OperationMode::Modify,
        }
    }

    /// 不等待另一操作拥有者，失败也不消耗输入或安排重复后台票。
    pub fn try_lock(&self) -> Option<TcpOperationGuard<'_>> {
        self.core.try_lock().map(|core| TcpOperationGuard {
            endpoint: self,
            core: Some(core),
            mode: OperationMode::Modify,
        })
    }

    /// 只观察成熟状态；释放时仅移交已经因本操作拥有受阻的真实 Work，不主动开始协议进度。
    pub fn state(&self) -> State {
        self.observe().state()
    }

    /// 普通只读访问也参与操作拥有交接，未有真实 Work 受阻时不安排后台票。
    fn observe(&self) -> TcpOperationGuard<'_> {
        TcpOperationGuard {
            endpoint: self,
            core: Some(self.core.lock()),
            mode: OperationMode::Observe,
        }
    }

    /// 取得仍存在的网络域，停止后的操作不得再发表新索引。
    fn net(&self) -> Result<NetStackRef, FsError> {
        self.stack.upgrade().ok_or(FsError::NotConnected)
    }

    /// 源地址校验与真正端口预留在控制发表阶段完成，通配值保持原样。
    pub fn bind(self: &Arc<Self>, local: InetEndpoint) -> Result<InetEndpoint, FsError> {
        let core = self.lock();
        if core.state() != State::Closed || self.metadata.lock().engaged {
            return Err(FsError::InvalidOperation);
        }
        let net = self.net()?;
        let _admission = net.admission()?;
        if self.metadata.lock().lease.is_some() {
            return Err(FsError::InvalidOperation);
        }
        let lease = net
            .tables()
            .reserve_binding(&net, self.id, TransportProtocol::Tcp, local)?;
        let local = lease.local();
        self.metadata.lock().lease = Some(lease);
        Ok(local)
    }

    /// listener 始终保 Listen；队列在表发表前 fallible 准备，半连接也占 backlog。
    pub fn listen(self: &Arc<Self>, backlog: usize) -> Result<(), FsError> {
        if backlog == 0 || ENDPOINT_CAPACITY < backlog {
            return Err(FsError::InvalidOperation);
        }
        let mut core = self.lock();
        if core.state() != State::Closed || self.metadata.lock().engaged {
            return Err(FsError::InvalidOperation);
        }
        let mut pending = Vec::new();
        pending
            .try_reserve_exact(backlog)
            .map_err(|_| FsError::NoMemory)?;
        let mut ready = VecDeque::new();
        ready
            .try_reserve_exact(backlog)
            .map_err(|_| FsError::NoMemory)?;
        let net = self.net()?;
        let _admission = net.admission()?;
        let lease = self.binding_or_ephemeral(&net)?;
        core.listen(lease.local())?;
        if let Err(error) = net.tables().publish_listener(self.id, lease.local()) {
            core.close();
            return Err(error);
        }
        *self.children.lock() = Children {
            limit: backlog,
            pending,
            ready,
        };
        self.metadata.lock().engaged = true;
        Ok(())
    }

    /// 路由选择实际源，租约仍保通配；明确四元组先发表再允许握手 Work。
    pub fn connect(self: &Arc<Self>, peer: InetEndpoint) -> Result<(), FsError> {
        let mut core = self.lock();
        if core.state() != State::Closed || self.metadata.lock().engaged {
            return Err(FsError::InvalidOperation);
        }
        let net = self.net()?;
        let admission = net.admission()?;
        let lease = self.binding_or_ephemeral(&net)?;
        let route = net.routes().resolve(peer.address, lease.local().address)?;
        let local = InetEndpoint {
            address: route.source,
            port: lease.local().port,
        };
        core.connect(local, peer, time::now())?;
        if let Err(error) = net
            .tables()
            .publish_tcp_tuple(self.id, TcpTuple { local, peer })
        {
            core.abort();
            self.metadata.lock().error = Some(error);
            self.metadata.lock().engaged = true;
            drop(admission);
            drop(core);
            net.retire_tcp(self.id);
            return Err(error);
        }
        self.metadata.lock().engaged = true;
        drop(admission);
        drop(core);
        Ok(())
    }

    /// 调用者持本核心及控制准入，首次自动绑定预留真正的临时端口。
    fn binding_or_ephemeral(&self, net: &NetStackRef) -> Result<Arc<BindLease>, FsError> {
        if let Some(lease) = self.metadata.lock().lease.as_ref().cloned() {
            return Ok(lease);
        }
        let lease = net.tables().reserve_binding(
            net,
            self.id,
            TransportProtocol::Tcp,
            InetEndpoint {
                address: [0; 4],
                port: 0,
            },
        )?;
        self.metadata.lock().lease = Some(lease.clone());
        Ok(lease)
    }

    /// 只领取已完成握手的真实 child，不替换 listener 或提前创建用户描述符。
    pub fn accept(&self) -> Result<Option<Arc<TcpEndpoint>>, FsError> {
        let core = self.observe();
        if core.state() != State::Listen {
            return Err(FsError::InvalidOperation);
        }
        let child = self.children.lock().ready.pop_front();
        Ok(child)
    }

    /// 最后打开对象只请求正常关闭，FIN 和 TIME_WAIT 继续由原 Work/Timer 推进。
    pub fn close(&self) {
        let mut core = self.lock();
        core.close();
        self.metadata.lock().engaged = true;
    }

    /// 未交给打开对象的节点终止协议，仍保留真实待发 RST 的 dispatch 责任。
    pub fn abandon(&self) {
        let mut core = self.lock();
        core.abort();
        self.metadata.lock().engaged = true;
    }

    /// 输入接收不等待用户核心，只在预留队列发表完整拥有，然后安排同 Work。
    pub(crate) fn enqueue(&self, packet: OwnedPacket) -> Result<(), FsError> {
        {
            let mut input = self.input.lock();
            if input.closed {
                return Err(FsError::NotConnected);
            }
            if input.packets.len() == LOOPBACK_CAPACITY {
                return Err(FsError::WouldBlock);
            }
            input.packets.push_back(packet);
        }
        self.schedule();
        Ok(())
    }

    /// 原 Work 的 pending 协议负责合并；停止时拒绝新增票，不维护第二 scheduled 位。
    pub(crate) fn schedule(&self) {
        if self.stopped.load(Ordering::Acquire) {
            return;
        }
        let Some(net) = self.stack.upgrade() else {
            return;
        };
        if net.is_stopping() {
            return;
        }
        let _ = self.work.get().unwrap().as_ref().queue();
    }

    /// 先登记真实拥有责任，释放输入锁再尝试核心；成功后清登记，失败由操作拥有者释放时交接。
    fn run(&self) {
        let Some(net) = self.stack.upgrade() else {
            return;
        };
        if net.is_stopping() || self.stopped.load(Ordering::Acquire) {
            return;
        }
        self.input.lock().owner_waiting = true;
        let Some(mut core) = self.core.try_lock() else {
            return;
        };
        self.input.lock().owner_waiting = false;
        let mut notification = None;
        let result = self.advance(&net, &mut core, &mut notification);
        if let Err(error) = result {
            if error == FsError::WouldBlock {
                self.timer.get().unwrap().as_ref().cancel_pending();
                drop(core);
                if let Some(parent) = notification {
                    parent.changed.wake_all();
                }
                net.wait_egress(self.id);
                return;
            }
            self.metadata.lock().error = Some(error);
            core.abort();
            crate::klog!(
                crate::klog::Level::Debug,
                "TCP endpoint {} failed: {:?}",
                self.id,
                error
            );
            drop(core);
            if let Some(parent) = notification {
                parent.changed.wake_all();
            }
            net.retire_tcp(self.id);
            return;
        }
        let deadline = core.deadline(time::now());
        let ended = core.state() == State::Closed
            && deadline == ProtocolDeadline::Ingress
            && self.metadata.lock().engaged;
        let queued = !self.input.lock().packets.is_empty();
        drop(core);
        if let Some(parent) = notification {
            parent.changed.wake_all();
        }
        if ended {
            net.retire_tcp(self.id);
            return;
        }
        if net.is_stopping() {
            return;
        }
        match deadline {
            ProtocolDeadline::At(at) => {
                let _ = self.timer.get().unwrap().as_ref().arm(at);
            }
            ProtocolDeadline::Now => {
                self.timer.get().unwrap().as_ref().cancel_pending();
                self.schedule();
            }
            ProtocolDeadline::Ingress => {
                self.timer.get().unwrap().as_ref().cancel_pending();
            }
        }
        if queued {
            self.schedule();
        }
    }

    /// 每轮先交付旧应答，再处理有限输入；未接受应答禁止继续消费 backlog。
    fn advance(
        &self,
        net: &NetStackRef,
        core: &mut TcpCore,
        notification: &mut Option<Arc<TcpEndpoint>>,
    ) -> Result<(), FsError> {
        self.emit_reply(net)?;
        let mut remaining = PROTOCOL_BUDGET;
        while 0 < remaining {
            let packet = self.input.lock().packets.pop_front();
            let Some(packet) = packet else {
                break;
            };
            remaining -= 1;
            if core.state() == State::Listen && self.children.lock().limit != 0 {
                if let Err(error) = self.receive_syn(net, core, packet) {
                    crate::klog!(
                        crate::klog::Level::Debug,
                        "TCP listener {} child preparation failed: {:?}",
                        self.id,
                        error
                    );
                }
            } else if core.state() != State::Closed {
                let previous = core.state();
                let reply = core.process(&packet, time::now())?;
                if previous != State::Established {
                    if let Some(parent) = self.report_established(core.state()) {
                        *notification = Some(parent);
                    }
                }
                if let Some(reply) = reply {
                    self.input.lock().reply = Some(reply);
                    self.emit_reply(net)?;
                }
            }
        }
        while 0 < remaining {
            remaining -= 1;
            core.dispatch(time::now(), |packet| net.send_ipv4(packet))?;
            if core.deadline(time::now()) != ProtocolDeadline::Now {
                break;
            }
        }
        Ok(())
    }

    /// try_emit 的失败完整包重新成为 pending，成功才结束立即应答责任。
    fn emit_reply(&self, net: &NetStackRef) -> Result<(), FsError> {
        let reply = self.input.lock().reply.take();
        let Some(reply) = reply else {
            return Ok(());
        };
        match net.try_emit(reply) {
            Ok(()) => Ok(()),
            Err((error, reply)) => {
                self.input.lock().reply = Some(reply);
                Err(error)
            }
        }
    }

    /// listener 核心只提供配置；child 先准备和发表，再由自己的 Work 处理原 SYN。
    fn receive_syn(
        &self,
        net: &NetStackRef,
        core: &TcpCore,
        packet: OwnedPacket,
    ) -> Result<(), FsError> {
        let (_, repr) = packet.tcp()?;
        if repr.control != TcpControl::Syn {
            return Ok(());
        }
        let (peer, local) = packet.tcp_endpoints()?;
        let tuple = TcpTuple { local, peer };
        let admission = net.admission()?;
        if let Some(child) = net.tables().lookup_tcp(tuple) {
            drop(admission);
            return child.enqueue(packet);
        }
        {
            let children = self.children.lock();
            if children.limit <= children.pending.len() + children.ready.len() {
                return Ok(());
            }
        }
        let lease = self
            .metadata
            .lock()
            .lease
            .as_ref()
            .cloned()
            .expect("listener has no binding");
        let slot = net.claim_endpoint_slot()?;
        let id = net.allocate_identity();
        let mut child_core = net.new_core(id, core.receive_capacity(), core.send_capacity())?;
        child_core.listen(local)?;
        let child = Self::try_new(net, id, child_core, slot)?;
        let listener = net
            .tables()
            .tcp_by_id(self.id)
            .expect("listener lost table ownership");
        {
            let mut metadata = child.metadata.lock();
            metadata.lease = Some(lease);
            metadata.parent = Arc::downgrade(&listener);
            metadata.engaged = true;
        }
        net.tables().insert_tcp(child.clone())?;
        if let Err(error) = net.tables().publish_tcp_tuple(id, tuple) {
            let unpublished = net.tables().take_tcp(id);
            drop(admission);
            drop(unpublished);
            return Err(error);
        }
        self.children.lock().pending.push(child.clone());
        drop(admission);
        child.enqueue(packet)
    }

    /// child 真正 Established 后转移同一 pending Arc，返回锁外通知所需的真实 parent 拥有。
    fn report_established(&self, state: State) -> Option<Arc<TcpEndpoint>> {
        if state != State::Established {
            return None;
        }
        let parent = self.metadata.lock().parent.upgrade()?;
        let ready = {
            let mut children = parent.children.lock();
            if let Some(index) = children
                .pending
                .iter()
                .position(|child| child.id == self.id)
            {
                let child = children.pending.swap_remove(index);
                children.ready.push_back(child);
                true
            } else {
                false
            }
        };
        if ready {
            Some(parent)
        } else {
            None
        }
    }

    /// 退休者或根拥有局部 Arc 后才能调用，先停 Timer 再等 Work，不持核心或短锁等待。
    pub(crate) fn stop_sync(&self) {
        assert!(
            !self.stopped.swap(true, Ordering::AcqRel),
            "TCP endpoint stopped twice"
        );
        self.input.lock().closed = true;
        self.timer.get().unwrap().as_ref().close_sync();
        self.work.get().unwrap().as_ref().close_sync();
        self.core.lock().abort();
        let (packets, reply) = {
            let mut input = self.input.lock();
            (core::mem::take(&mut input.packets), input.reply.take())
        };
        let (lease, parent) = {
            let mut metadata = self.metadata.lock();
            (metadata.lease.take(), core::mem::take(&mut metadata.parent))
        };
        let children = {
            let mut children = self.children.lock();
            children.limit = 0;
            (
                core::mem::take(&mut children.pending),
                core::mem::take(&mut children.ready),
            )
        };
        for child in children.0.into_iter().chain(children.1) {
            child.abandon();
        }
        if let Some(parent) = parent.upgrade() {
            parent.remove_child(self.id);
        }
        drop((packets, reply, lease));
        self.changed.wake_all();
    }

    /// 已结束的 child 从 listener 队列撤下，最后 Arc 析构在锁外发生。
    fn remove_child(&self, id: u64) {
        let removed = {
            let mut children = self.children.lock();
            if let Some(index) = children.pending.iter().position(|child| child.id == id) {
                Some(children.pending.swap_remove(index))
            } else if let Some(index) = children.ready.iter().position(|child| child.id == id) {
                children.ready.remove(index)
            } else {
                None
            }
        };
        drop(removed);
    }
}

/// 表、退休队列或停止者的局部 Arc 保活节点，Work 本身不猜测业务 Arc 控制块。
fn run_tcp(work: Pin<&Work>) {
    // SAFETY: context 来自构造期稳定 Arc；发表后真实表/退休/停止局部拥有一直覆盖 Work 全尾。
    let endpoint = unsafe { &*(work.context() as *const TcpEndpoint) };
    endpoint.run();
}

/// 硬中断只安排同一个普通 Work，不取得 SleepMutex 或运行协议。
fn tcp_expired(timer: Pin<&Timer>) {
    // SAFETY: Timer context 来自稳定节点 Arc；退休与根停止均先等本 Timer 全尾再释放节点。
    let endpoint = unsafe { &*(timer.context() as *const TcpEndpoint) };
    endpoint.schedule();
}
