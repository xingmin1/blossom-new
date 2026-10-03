//! 独立普通任务逐字节推进稳定记录，等待源不依赖周期扫描。

use core::alloc::AllocError;
use core::fmt::Write;
use core::pin::pin;

use crate::console;
use crate::task;
use crate::wait::{WaitNode, WaitQueue, Waiter};

use super::{Cursor, ReadRecord, StackText};

static RECORD_WAIT: WaitQueue = WaitQueue::new();

/// 在 BSP 开放中断与启动 AP 前准备唯一固定 CPU 的打印任务，再无分配发表。
pub fn initialize_service() -> Result<(), AllocError> {
    let prepared = task::prepare_kernel_on(0, run)?;
    drop(prepared.publish());
    Ok(())
}

/// 只由已取走固定原因位的 IPI 分支通知，生产者不直接进入调度锁。
pub(crate) fn wake_printer() {
    RECORD_WAIT.wake_one();
}

/// 先尝试读记录，再登记真实永久等待源并复查，关闭发布与阻塞之间的窗口。
fn read_next(cursor: &mut Cursor) -> ReadRecord {
    loop {
        if let Some(record) = cursor.read() {
            return record;
        }
        let record = {
            let waiter = Waiter::new();
            let mut node = pin!(WaitNode::new(waiter.waker()));
            // SAFETY: 永久 RECORD_WAIT 保活源，pin! 保持节点地址；局部 registration
            // 不被遗忘且先于 node/waiter 析构，正常返回或展开均先真实撤链。
            let _registration = unsafe { RECORD_WAIT.register(node.as_mut()) };
            if let Some(record) = cursor.read() {
                Some(record)
            } else {
                waiter.park();
                None
            }
        };
        if let Some(record) = record {
            return record;
        }
    }
}

/// 零接受时先登记 TX 源，再使能并复查真实空间；离开作用域才允许测试停点。
fn wait_for_tx() -> bool {
    let waiter = Waiter::new();
    let mut node = pin!(WaitNode::new(waiter.waker()));
    // SAFETY: console 提供永久 TX 源，栈节点由 pin! 固定；唯一 registration
    // 在本函数返回前撤链，随后析构节点与本轮 waiter，不遗忘任何登记。
    let _registration = unsafe { console::tx_wait_queue().register(node.as_mut()) };
    if !console::arm_tx_interrupt() {
        waiter.park();
        #[cfg(feature = "execution-tests")]
        testing::after_tx_wait();
        true
    } else {
        false
    }
}

/// 一份完整记录与固定头部留在本任务栈上，UART 只接收尚未接受的真实字节前缀。
fn run() -> i32 {
    #[cfg(feature = "execution-tests")]
    testing::install_service();
    let mut cursor = Cursor::oldest();
    loop {
        let current = read_next(&mut cursor);
        let record = &current.record;
        let mut header = StackText::<256>::new();
        write!(
            header,
            "KLOG seq={} level={} ns={} cpu={} task=",
            record.seq,
            record.level.as_str(),
            record.monotonic_ns,
            record.cpu
        )
        .expect("log header formatting failed");
        match record.task {
            Some(task) => write!(header, "{}", task.as_usize()),
            None => header.write_str("none"),
        }
        .expect("log task formatting failed");
        write!(
            header,
            " truncated={} lost={} ",
            u8::from(record.truncated),
            current.lost
        )
        .expect("log metadata formatting failed");
        assert!(!header.is_truncated(), "log header capacity exhausted");
        let header = header.as_bytes();
        let body = record.text().as_bytes();
        let length = header.len() + body.len() + 1;
        let mut offset = 0;
        #[cfg(feature = "execution-tests")]
        testing::begin_record(record.seq, length, current.lost);
        while offset < length {
            let remaining = if offset < header.len() {
                &header[offset..]
            } else if offset < header.len() + body.len() {
                &body[offset - header.len()..]
            } else {
                b"\n"
            };
            let accepted = console::try_write(remaining);
            assert!(
                accepted <= remaining.len(),
                "UART accepted an invalid prefix"
            );
            if accepted == 0 {
                #[cfg(not(feature = "execution-tests"))]
                wait_for_tx();
                #[cfg(feature = "execution-tests")]
                if wait_for_tx() {
                    testing::pause_after_partial_write(offset, length);
                }
            } else {
                offset += accepted;
                #[cfg(feature = "execution-tests")]
                testing::advance(offset, accepted);
            }
        }
        #[cfg(feature = "execution-tests")]
        testing::finish_record(record.seq);
    }
}

#[cfg(feature = "execution-tests")]
pub mod testing {
    use alloc::sync::{Arc, Weak};
    use core::pin::pin;
    use core::sync::atomic::{AtomicUsize, Ordering};

    use crate::sched;
    use crate::sync::{Completion, SpinLock};
    use crate::task::{self, State, Task};
    use crate::wait::{WaitNode, WaitQueue, Waiter};

    /// 仅观察生产任务当前稳定记录与真实 UART 接受进度，不保留第二份正文。
    #[derive(Clone, Copy)]
    pub(crate) struct Snapshot {
        pub seq: u64,
        pub offset: usize,
        pub length: usize,
        pub bytes: u64,
        pub gaps: u64,
        pub progress: u64,
        pub next_seq: u64,
        pub tx_waits: u64,
    }

    static OBSERVED: SpinLock<Snapshot> = SpinLock::new(Snapshot {
        seq: 0,
        offset: 0,
        length: 0,
        bytes: 0,
        gaps: 0,
        progress: 0,
        next_seq: 1,
        tx_waits: 0,
    });
    static SERVICE: SpinLock<Option<Weak<Task>>> = SpinLock::new(None);
    static PROGRESS: WaitQueue = WaitQueue::new();
    static PAUSE: AtomicUsize = AtomicUsize::new(0);
    static RESUME: Completion = Completion::new();

    #[derive(Clone, Copy)]
    pub(crate) enum PausePoint {
        AfterPartialWrite,
    }

    /// 入口只保存弱观察，任务的强拥有与执行栈仍由真实调度器负责。
    pub(super) fn install_service() {
        let task = task::current_task();
        *SERVICE.lock() = Some(Arc::downgrade(&task));
    }

    /// 在生产记录已稳定复制之后公开其当前输出身份与精确覆盖缺口。
    pub(super) fn begin_record(seq: u64, length: usize, lost: u64) {
        let mut state = OBSERVED.lock();
        state.seq = seq;
        state.offset = 0;
        state.length = length;
        state.gaps = state
            .gaps
            .checked_add(lost)
            .expect("log gap counter overflow");
    }

    /// 只累计实际接受的字节，偏移含头部、UTF8 正文与行尾。
    pub(super) fn advance(offset: usize, accepted: usize) {
        let mut state = OBSERVED.lock();
        state.offset = offset;
        state.bytes = state
            .bytes
            .checked_add(accepted as u64)
            .expect("log byte counter overflow");
    }

    /// 实际 TX park 返回后记账；测试停点另在登记与 guard 全部释放后执行。
    pub(super) fn after_tx_wait() {
        let mut state = OBSERVED.lock();
        state.tx_waits = state
            .tx_waits
            .checked_add(1)
            .expect("log TX wait counter overflow");
    }

    /// 完成整行后才推进消费边界，释放观察锁再通知固定边界等待者。
    pub(super) fn finish_record(seq: u64) {
        {
            let mut state = OBSERVED.lock();
            state.seq = 0;
            state.offset = 0;
            state.length = 0;
            state.next_seq = seq.checked_add(1).expect("log sequence exhausted");
            state.progress = state
                .progress
                .checked_add(1)
                .expect("log progress counter overflow");
        }
        PROGRESS.wake_all();
    }

    /// 只有真实 TX 等待后且尚有当前记录字节时，使用 Completion 阻塞普通服务任务。
    pub(super) fn pause_after_partial_write(offset: usize, length: usize) {
        if offset == 0 || length <= offset {
            return;
        }
        if PAUSE
            .compare_exchange(1, 2, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            RESUME.wait();
            PAUSE.store(0, Ordering::Release);
        }
    }

    /// 复制唯一生产观察，锁内不读取调度状态或升级弱任务引用。
    pub(crate) fn snapshot() -> Snapshot {
        *OBSERVED.lock()
    }

    /// 弱观察在观察锁外升级，真实状态只由调度器锁域给出。
    pub(crate) fn service_state() -> Option<State> {
        let service = SERVICE.lock().clone();
        service
            .and_then(|weak| weak.upgrade())
            .map(|task| sched::test_state(&task))
    }

    /// 武装单次真实输出停点，不修改 UART 状态或生产记录消费路径。
    pub(crate) fn arm_pause(point: PausePoint) {
        match point {
            PausePoint::AfterPartialWrite => {
                assert!(
                    PAUSE
                        .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
                        .is_ok(),
                    "log pause already armed"
                );
            }
        }
    }

    /// 此值仅说明停点已到达，调用者仍须用 service_state 验证真实 Blocked。
    pub(crate) fn is_paused() -> bool {
        PAUSE.load(Ordering::Acquire) == 2
    }

    /// 发布一个真实完成 token，让服务继续同一记录的尚未接受字节。
    pub(crate) fn release_pause() {
        assert!(is_paused(), "log printer has not reached its pause");
        RESUME.complete();
    }

    /// 等待调用者给出的固定排他序号，不追赶之后的生产者或依赖周期轮询。
    pub(crate) fn catch_up(boundary: u64) {
        sched::assert_switchable();
        assert!(1 <= boundary && boundary <= super::super::next_sequence());
        loop {
            if boundary <= snapshot().next_seq {
                return;
            }
            let waiter = Waiter::new();
            let mut node = pin!(WaitNode::new(waiter.waker()));
            // SAFETY: PROGRESS 为永久源，pin! 固定节点；局部唯一 registration
            // 在每轮返回或展开时先撤链，随后才析构 node 和 waiter，绝不遗忘登记。
            let _registration = unsafe { PROGRESS.register(node.as_mut()) };
            if boundary <= snapshot().next_seq {
                return;
            }
            waiter.park();
        }
    }

    /// 控制任务沿现有服务观察协议有限让出 CPU，确认无当前输出且服务真正 Blocked。
    pub(crate) fn wait_service_blocked() {
        sched::assert_switchable();
        let start = crate::arch::read_counter();
        let limit = crate::arch::counter_frequency()
            .checked_mul(10)
            .expect("log service deadline overflow");
        loop {
            let observed = snapshot();
            if observed.length == 0
                && super::super::next_sequence() <= observed.next_seq
                && service_state() == Some(State::Blocked)
            {
                return;
            }
            assert!(
                crate::arch::read_counter().wrapping_sub(start) < limit,
                "log service did not block"
            );
            sched::yield_now();
        }
    }
}
