//! 逻辑请求拥有原页；生成者与已登记 child 共同决定唯一父完成。

use alloc::sync::Arc;
use alloc::vec::Vec;
use core::pin::{pin, Pin};
use core::sync::atomic::{AtomicBool, Ordering};

use virtio_drivers::MappedSegment;

use crate::block::{BlkOperation, BlockError, Disk, DiskRef, EndIo, IoResult};
use crate::drivers::virtio::VirtioIdentity;
use crate::mm::dma::MappedPages;
use crate::sched;
use crate::sync::SpinLock;
use crate::wait::{WaitNode, WaitQueue, Waiter};
use crate::workqueue::Work;

/// 唯一可提交凭证；完成观察者只克隆同一父对象，不能重新提交。
pub struct Bio {
    pub(super) inner: Arc<BioInner>,
}

/// 完成观察者不拥有 Disk，也不创建新的等待存储或内存副本。
#[derive(Clone)]
pub struct BioCompletion {
    inner: Arc<BioInner>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Phase {
    New,
    Beginning,
    Active,
    Finishing,
    Complete,
}

struct BioState {
    phase: Phase,
    memory: Option<Vec<MappedPages>>,
    remaining: usize,
    generating: bool,
    registered_end: usize,
    registered_children: usize,
    first_error: Option<(usize, BlockError)>,
    result: Option<IoResult>,
    completion_owner: Option<DiskRef>,
}

/// 不可变范围与常数完成账本分开；队列 part 只强拥有此对象。
pub(super) struct BioInner {
    target: VirtioIdentity,
    operation: BlkOperation,
    sector: u64,
    len: usize,
    segment_count: usize,
    end_io: Option<EndIo>,
    finalizer_work: Option<Pin<Arc<Work>>>,
    state: SpinLock<BioState>,
    ready: AtomicBool,
    waiters: WaitQueue,
}

impl Bio {
    /// 在普通可切换 Task、所有队列锁外消费 CPU 拥有的原映射并校验逻辑范围。
    /// 分配失败不发表请求，原页由输入拥有者正常归还。
    ///
    /// # Safety
    /// Write 的整个传输范围必须已初始化；所有此前导出的 CPU 裸别名必须结束访问，
    /// 直到完成回调或完成观察者重新取得 CPU 拥有权。Read 不要求未写尾部初始化，
    /// 每个输入映射都必须当前 CPU-owned，且由本次调用移交唯一原页归还权。
    /// 回调若调用 MappedPages::bytes，仍须独立证明整个映射初始化。
    /// Inline 回调不得睡眠、动态准备或递归停止本 Disk；Task 回调可以睡眠，
    /// 但不得等待自身父完成、等待本 Disk 停止或复用仍由回调访问的 slot。
    /// 两种回调都不得泄露 memory 借用或在返回后继续访问其裸别名。
    /// Task 回调的现存工作域须保开放至本 Bio 被拒绝或完成，沿用域 owner 停外部生产源后关闭的规则。
    /// Task Bio 构造期另备同域独占执行栈；拒绝或生成者最后完成也不能耗尽设备完成的准入。
    pub unsafe fn try_new(
        disk: &Disk,
        operation: BlkOperation,
        sector: u64,
        memory: Vec<MappedPages>,
        attributes: u32,
        end_io: Option<EndIo>,
    ) -> Result<Self, BlockError> {
        sched::assert_switchable();
        let mut len = 0_usize;
        for mapped in &memory {
            if mapped.is_device_owned() {
                return Err(BlockError::Busy);
            }
            len = len
                .checked_add(mapped.len())
                .ok_or(BlockError::InvalidRange)?;
        }
        disk.limits().validate(
            operation,
            sector,
            len,
            memory.iter().map(MappedPages::segment),
            attributes,
        )?;
        let segment_count = memory.len();
        let inner = Arc::try_new(BioInner {
            target: disk.identity(),
            operation,
            sector,
            len,
            segment_count,
            end_io,
            finalizer_work: None,
            state: SpinLock::new(BioState {
                phase: Phase::New,
                memory: Some(memory),
                remaining: 0,
                generating: false,
                registered_end: 0,
                registered_children: 0,
                first_error: None,
                result: None,
                completion_owner: None,
            }),
            ready: AtomicBool::new(false),
            waiters: WaitQueue::new(),
        })
        .map_err(|_| BlockError::NoMemory)?;
        if inner.needs_task() {
            let raw = Arc::into_raw(inner);
            // SAFETY: 此原 Arc 尚未发表、未创建弱引用，局部恢复唯一构造拥有。
            let owner = unsafe { Arc::from_raw(raw) };
            let work = disk.prepare_finalizer(finalizer_callback, raw as usize)?;
            // SAFETY: Work 尚无票，没有任何回调或第二份 Bio 引用，固定分配内初始化一次。
            unsafe {
                (*raw.cast_mut()).finalizer_work = Some(work);
            }
            return Ok(Self { inner: owner });
        }
        Ok(Self { inner })
    }

    /// 克隆现存父引用供提交后的观察，不分配或复制 payload。
    pub fn completion(&self) -> BioCompletion {
        BioCompletion {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl BioCompletion {
    /// 普通可切换 Task 登记栈节点、复查再阻塞；通知只用于重查 Release 完成事实。
    pub fn wait(&self) -> IoResult {
        sched::assert_switchable();
        loop {
            if let Some(result) = self.result() {
                return result;
            }
            let waiter = Waiter::new();
            let mut node = pin!(WaitNode::new(waiter.waker()));
            // SAFETY: self 强拥有等待源，pin! 固定栈节点；registration 不被遗忘，
            // 所有返回均先撤 registration，再析构节点与 waiter，源始终有效。
            let _registration = unsafe { self.inner.waiters.register(node.as_mut()) };
            if let Some(result) = self.result() {
                return result;
            }
            waiter.park();
        }
    }

    /// Acquire 观察最终 DMA 收权和回调尾部，完成结果不被其他观察者消费。
    pub fn result(&self) -> Option<IoResult> {
        if !self.is_complete() {
            return None;
        }
        self.inner.state.lock().result
    }

    /// 只在所有义务、收权及回调已经结束后为真。
    pub fn is_complete(&self) -> bool {
        self.inner.ready.load(Ordering::Acquire)
    }

    /// 完成后只允许一个观察者移出原 memory，未完成或重复领取返回 Busy。
    /// Read 错误和短读的尾部仍可能未初始化；返回映射只提供安全 MaybeUninit 写入口，
    /// unsafe bytes 的全范围初始化义务不因领取 memory 而解除。
    pub fn take_memory(&self) -> Result<Vec<MappedPages>, BlockError> {
        if !self.is_complete() {
            return Err(BlockError::Busy);
        }
        self.inner
            .state
            .lock()
            .memory
            .take()
            .ok_or(BlockError::Busy)
    }

    /// 验收等待已预备父 finalizer 的真实票尾，不以 ready 推测 worker 已返还。
    #[cfg(any(feature = "storage-tests", feature = "ktest"))]
    pub(crate) fn test_flush_finalizer(&self) {
        if let Some(work) = &self.inner.finalizer_work {
            work.as_ref().flush();
        }
    }

    /// 只观察已有父 finalizer 的原生任务；弱观察升级不申请新的执行资源。
    #[cfg(feature = "ktest")]
    pub(crate) fn ktest_finalizer_task(&self) -> Option<Arc<crate::task::Task>> {
        self.inner
            .finalizer_work
            .as_ref()
            .and_then(|work| work.ktest_reserved_task())
    }

    /// 只在原 manager 完成该 finalizer 原生任务的真实 join 后为真。
    #[cfg(feature = "ktest")]
    pub(crate) fn ktest_finalizer_joined(&self) -> bool {
        self.inner
            .finalizer_work
            .as_ref()
            .is_some_and(|work| work.ktest_reserved_joined())
    }
}

impl BioInner {
    /// 返回构造时固定的完整设备身份，不强拥有设备。
    pub(super) fn target(&self) -> VirtioIdentity {
        self.target
    }

    /// 返回父逻辑操作，所有 child 必须保持此操作。
    pub(super) fn operation(&self) -> BlkOperation {
        self.operation
    }

    /// 返回固定的 512 字节协议 sector 起点。
    pub(super) fn sector(&self) -> u64 {
        self.sector
    }

    /// 返回原映射总字节数，Flush 的范围为空。
    pub(super) fn len(&self) -> usize {
        self.len
    }

    /// 返回真实原映射数量，不把拆分后的 SG 数当作原页数量。
    pub(super) fn segment_count(&self) -> usize {
        self.segment_count
    }

    /// 从唯一原拥有者复制一份有界 DMA 元数据，不导出页借用。
    pub(super) fn segment(&self, index: usize) -> Option<MappedSegment> {
        self.state
            .lock()
            .memory
            .as_ref()?
            .get(index)
            .map(MappedPages::segment)
    }

    /// slot 在 IRQ 分派前检查是否必须使用预建完成 Work。
    pub(super) fn needs_task(&self) -> bool {
        matches!(self.end_io, Some(EndIo::Task(_)))
    }

    /// 唯一 admission 在普通 Task 中建立生成者义务，再锁外交整个原 memory 给设备。
    pub(super) fn begin(&self) {
        sched::assert_switchable();
        let mut memory = {
            let mut state = self.state.lock();
            assert_eq!(state.phase, Phase::New, "bio admitted twice");
            state.phase = Phase::Beginning;
            state.remaining = 1;
            state.generating = true;
            state
                .memory
                .take()
                .expect("bio memory missing at admission")
        };
        for mapped in &mut memory {
            // SAFETY: unsafe 构造保证 Write 初始化及 CPU 裸别名访问结束；唯一 admission
            // 尚未发表 child，原拥有者保存在此父，全部 child 和生成者结束前不能收权。
            unsafe { mapped.begin_device_access() };
        }
        let mut state = self.state.lock();
        assert_eq!(state.phase, Phase::Beginning);
        state.memory = Some(memory);
        state.phase = Phase::Active;
    }

    /// 按父逻辑次序在 avail 发表前登记一个 child，不分配完整 child 列表。
    pub(super) fn register_child(&self, offset: usize, length: usize) {
        let end = offset
            .checked_add(length)
            .expect("bio child range overflow");
        assert!(end <= self.len);
        assert!(0 < length || self.operation == BlkOperation::Flush);
        let mut state = self.state.lock();
        assert_eq!(state.phase, Phase::Active);
        assert!(state.generating, "bio child registered after generation");
        assert_eq!(
            offset, state.registered_end,
            "bio child ranges are not contiguous"
        );
        if self.operation == BlkOperation::Flush {
            assert_eq!(state.registered_children, 0, "flush registered twice");
        }
        state.registered_end = end;
        state.registered_children = state
            .registered_children
            .checked_add(1)
            .expect("bio child count overflow");
        state.remaining = state
            .remaining
            .checked_add(1)
            .expect("bio pending count overflow");
    }

    /// 唯一 slot 完成者在 used 或已确认 reset 后结束 child；Task 回调须由 slot Work 执行。
    /// 未发表的失败 child 也消费同一登记义务，生成者仍保住此前在途原页。
    pub(super) fn finish_child(
        self: &Arc<Self>,
        offset: usize,
        length: usize,
        completed: usize,
        error: Option<BlockError>,
    ) {
        let end = offset
            .checked_add(length)
            .expect("bio child range overflow");
        assert!(end <= self.len && completed <= length);
        let error = error.or_else(|| (completed < length).then_some(BlockError::ShortIo));
        let completion = {
            let mut state = self.state.lock();
            assert_eq!(state.phase, Phase::Active);
            assert!(end <= state.registered_end);
            assert!(
                usize::from(state.generating) < state.remaining,
                "bio child ended twice"
            );
            if let Some(error) = error {
                state.record_error(offset + completed, error);
            }
            state.end_obligation(self.len)
        };
        if let Some((memory, result)) = completion {
            self.complete(memory, result, true, None, false);
        }
    }

    /// 结束生成者账本；true 表示可睡完成及 accepted 尾部已移交构造期同域预留 Work。
    pub(super) fn finish_generation(
        self: &Arc<Self>,
        error: Option<(usize, BlockError)>,
        owner: Option<&Disk>,
    ) -> bool {
        sched::assert_switchable();
        let completion = {
            let mut state = self.state.lock();
            assert_eq!(state.phase, Phase::Active);
            assert!(state.generating, "bio generation ended twice");
            if let Some((offset, error)) = error {
                assert!(offset <= state.registered_end);
                state.record_error(offset, error);
            } else {
                assert_eq!(
                    state.registered_end, self.len,
                    "bio generation is incomplete"
                );
                assert!(0 < state.registered_children, "bio generated no child");
            }
            state.generating = false;
            state.end_obligation(self.len)
        };
        if let Some((memory, result)) = completion {
            return self.complete(memory, result, true, owner, true);
        }
        false
    }

    /// 普通 Task 在未 admission 时立即失败，原 memory 从未交设备，回调仍在锁外。
    pub(super) fn reject(self: &Arc<Self>, error: BlockError) {
        sched::assert_switchable();
        let memory = {
            let mut state = self.state.lock();
            assert_eq!(state.phase, Phase::New, "rejecting an admitted bio");
            state.phase = Phase::Finishing;
            state.memory.take().expect("rejected bio memory missing")
        };
        self.complete(
            memory,
            IoResult {
                completed: 0,
                error: Some(error),
            },
            false,
            None,
            true,
        );
    }

    /// 最后义务先锁外收原映射；非 slot 的 Task 完成独立排队，不阻塞批次生成者。
    fn complete(
        self: &Arc<Self>,
        mut memory: Vec<MappedPages>,
        result: IoResult,
        device_owned: bool,
        owner: Option<&Disk>,
        queue_task: bool,
    ) -> bool {
        if device_owned {
            for mapped in &mut memory {
                // SAFETY: remaining 已归零并唯一移出 memory；已发表 child 只在真实 used
                // 或确认 reset 后结束，未发表 child 没有设备引用，生成者也已结束。
                unsafe { mapped.finish_device_access() };
            }
        }
        if queue_task && self.needs_task() {
            {
                let mut state = self.state.lock();
                assert_eq!(state.phase, Phase::Finishing);
                assert!(
                    state.memory.is_none()
                        && state.result.is_none()
                        && state.completion_owner.is_none()
                );
                state.memory = Some(memory);
                state.result = Some(result);
                // 所有 child 已结束并从槽移出，临时 owner 没有 Disk→本 Bio 的返回边。
                state.completion_owner = owner.map(Disk::clone_ref);
            }
            let owned = Arc::into_raw(Arc::clone(self));
            let work = self
                .finalizer_work
                .as_ref()
                .expect("Task bio has no prepared finalizer");
            assert_eq!(owned as usize, work.context());
            assert!(
                work.as_ref()
                    .queue()
                    .expect("bio finalizer domain closed before producers stopped"),
                "bio finalizer queued twice"
            );
            return true;
        }
        self.finalize(memory, result);
        false
    }

    /// 唯一完成者在锁外执行回调，回调返回后才 Release 发布 memory 与完成结果。
    fn finalize(&self, memory: Vec<MappedPages>, result: IoResult) {
        match self.end_io {
            Some(EndIo::Inline(callback)) => callback(&result, &memory),
            Some(EndIo::Task(callback)) => {
                // child 尾部属于专用 slot Work，其它尾部属于构造期 Bio Work。
                sched::assert_switchable();
                callback(&result, &memory);
            }
            None => {}
        }
        {
            let mut state = self.state.lock();
            assert_eq!(state.phase, Phase::Finishing);
            assert!(state.memory.is_none() && state.result.is_none());
            state.memory = Some(memory);
            state.result = Some(result);
            state.phase = Phase::Complete;
        }
        self.ready.store(true, Ordering::Release);
        self.waiters.wake_all();
    }
}

/// 消费队列化的真实 Bio 拥有，归还生成者/临时 Disk；Work 票尾部自动释放一次性执行预留。
fn finalizer_callback(work: Pin<&Work>) {
    // SAFETY: complete 在唯一最后义务移交同一 Arc::into_raw 强引用；Work 只能排一次，
    // 该强拥有保 context 和 memory 至此 callback，原 Work 票另保自身至执行尾部。
    let bio = unsafe { Arc::from_raw(work.context() as *const BioInner) };
    let (memory, result, owner) = {
        let mut state = bio.state.lock();
        assert_eq!(state.phase, Phase::Finishing);
        (
            state.memory.take().expect("finalizer memory missing"),
            state.result.take().expect("finalizer result missing"),
            state.completion_owner.take(),
        )
    };
    bio.finalize(memory, result);
    if let Some(owner) = owner {
        owner.finish_generation_tail(bio.operation());
    }
}

impl BioState {
    /// 记录最小错误字节位置，后部反序成功不跨过前部失败形成虚假成功前缀。
    fn record_error(&mut self, offset: usize, error: BlockError) {
        if self.first_error.is_none_or(|(first, _)| offset < first) {
            self.first_error = Some((offset, error));
        }
    }

    /// 消费一个已登记义务，只在最后归零时把唯一原页向锁外完成者移交。
    fn end_obligation(&mut self, len: usize) -> Option<(Vec<MappedPages>, IoResult)> {
        assert!(0 < self.remaining, "bio obligation ended twice");
        self.remaining -= 1;
        if self.remaining != 0 {
            return None;
        }
        assert!(!self.generating);
        self.phase = Phase::Finishing;
        let result = match self.first_error {
            Some((completed, error)) => IoResult {
                completed,
                error: Some(error),
            },
            None => IoResult {
                completed: len,
                error: None,
            },
        };
        Some((
            self.memory.take().expect("completed bio memory missing"),
            result,
        ))
    }
}
