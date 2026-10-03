//! 真实记录、普通 printer Task 与 UART 中断的联合验收，不替换生产后端。

use alloc::sync::Arc;
use core::fmt::{self, Write};
use core::hint::black_box;
use core::pin::Pin;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use crate::klog::{self as log, Cursor, Level};
use crate::sync::{Completion, SpinLock};
use crate::task::{self, State, Task};
use crate::time::{self as clock, Duration};
use crate::timer::{Timer, TimerKind};
use crate::{arch, console, cpu, mm, sched};

use super::{baseline, reclaimed};

static PRODUCER: SpinLock<Option<Arc<Task>>> = SpinLock::new(None);
static START: Completion = Completion::new();
static EXIT: Completion = Completion::new();
static PRODUCED: AtomicBool = AtomicBool::new(false);
static TIMER_SEQ: AtomicU64 = AtomicU64::new(0);
static TIMER_TASK: AtomicUsize = AtomicUsize::new(0);
static TIMER_CPU: AtomicUsize = AtomicUsize::new(0);
static BUSINESS_START: Completion = Completion::new();
static BUSINESS: AtomicUsize = AtomicUsize::new(0);

struct UnicodeBody;

impl fmt::Display for UnicodeBody {
    /// 固定输出超过正文容量的多字节文本，截断必须保留最后一个完整字符。
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for _ in 0..180 {
            formatter.write_str("界")?;
        }
        Ok(())
    }
}

/// 测试协调具有固定原始 counter 截止，业务阻塞仍走真实 Completion 和 scheduler。
fn until(mut predicate: impl FnMut() -> bool, name: &str) {
    let end = arch::read_counter()
        .checked_add(arch::counter_frequency().checked_mul(10).unwrap())
        .unwrap();
    while !predicate() {
        assert!(
            arch::read_counter() < end,
            "klog execution timeout: {}",
            name
        );
        sched::yield_now();
    }
}

/// 独立普通生产者在预备完成后阻塞，记录完成仍保持存储直到协调者同步退出。
fn producer() -> i32 {
    *PRODUCER.lock() = Some(task::current_task());
    START.wait();
    let mut value = 37usize;
    crate::klog!(Level::Info, "X25 ordinary value={}", value);
    value = black_box(99);
    black_box(value);
    crate::klog!(Level::Warn, "{}", UnicodeBody);
    sched::test_klog();
    crate::workqueue::testing::test_klog();
    PRODUCED.store(true, Ordering::Release);
    EXIT.wait();
    125
}

/// 精确硬回调只读标量、记录和发表完成，没有睡眠或对象构造。
fn precise_callback(_: Pin<&Timer>) {
    assert!(0 < cpu::current().hardirq_depth.load(Ordering::Relaxed));
    TIMER_TASK.store(
        task::current_task_id().map_or(0, |id| id.as_usize()),
        Ordering::Relaxed,
    );
    TIMER_CPU.store(cpu::current().id, Ordering::Relaxed);
    let sequence = crate::klog!(Level::Info, "X25 precise timer");
    TIMER_SEQ.store(sequence, Ordering::Release);
}

/// 四个真实任务侧记录及一个精确 IRQ 记录共享同一环，生产阶段 GlobalAlloc 尝试必须为零。
fn test_records() {
    let initial = baseline();
    let timer = Timer::try_new(0, TimerKind::Precise, precise_callback, 0).unwrap();
    let worker = task::spawn_kernel_on(0, producer).unwrap();
    until(
        || {
            PRODUCER
                .lock()
                .as_ref()
                .is_some_and(|task| sched::test_state(task) == State::Blocked)
        },
        "producer prepared and blocked",
    );
    log::testing::wait_service_blocked();
    super::workqueue::wait_services_blocked();
    let first = log::next_sequence();
    let start_ns = clock::now().as_nanos();
    let attempts = mm::allocation_attempts();
    START.complete();
    until(
        || PRODUCED.load(Ordering::Acquire),
        "task producers complete",
    );
    assert!(timer
        .as_ref()
        .arm(crate::timer::after(Duration::from_millis(5)).unwrap())
        .unwrap());
    until(
        || TIMER_SEQ.load(Ordering::Acquire) != 0,
        "precise log callback",
    );
    let allocation_attempts = mm::allocation_attempts().checked_sub(attempts).unwrap();
    assert_eq!(allocation_attempts, 0);
    let end_ns = clock::now().as_nanos();
    let last = TIMER_SEQ.load(Ordering::Acquire);
    assert_eq!(last, first + 4);
    assert_eq!(log::next_sequence(), last + 1);
    let levels = [
        Level::Info,
        Level::Warn,
        Level::Debug,
        Level::Debug,
        Level::Info,
    ];
    let mut cursors = [Cursor::from_sequence(first), Cursor::from_sequence(first)];
    for cursor in &mut cursors {
        let mut previous_ns = start_ns;
        for (index, level) in levels.iter().enumerate() {
            let read = cursor.read().expect("committed record missing");
            let record = read.record;
            assert_eq!(read.lost, 0);
            assert_eq!(record.seq, first + index as u64);
            assert_eq!(record.level, *level);
            assert_eq!(record.len, record.text().len());
            assert!(record.len <= log::TEXT_CAPACITY);
            assert!(record.text().is_char_boundary(record.len));
            assert!(start_ns <= record.monotonic_ns && record.monotonic_ns <= end_ns);
            assert!(previous_ns <= record.monotonic_ns);
            previous_ns = record.monotonic_ns;
            if index < 4 {
                assert_eq!(record.task, Some(worker.task_id()));
                assert_eq!(record.cpu, 0);
            } else {
                assert_eq!(
                    record.task.map_or(0, |id| id.as_usize()),
                    TIMER_TASK.load(Ordering::Acquire)
                );
                assert_eq!(record.cpu, TIMER_CPU.load(Ordering::Acquire));
            }
            assert_eq!(record.truncated, index == 1);
            match index {
                0 => assert_eq!(record.text(), "X25 ordinary value=37"),
                1 => {
                    assert_eq!(record.len, 510);
                    assert_eq!(record.text().chars().count(), 170);
                    assert!(record.text().chars().all(|character| character == '界'));
                }
                2 => assert_eq!(record.text(), "X25 scheduler locked"),
                3 => assert_eq!(record.text(), "X25 pool locked"),
                _ => assert_eq!(record.text(), "X25 precise timer"),
            }
        }
        assert!(cursor.read().is_none());
        assert!(cursor.read().is_none());
    }
    timer.as_ref().close_sync();
    // SAFETY: 克隆只观察同一固定 Arc 的强计数，不移出 Timer；close_sync 已停止真实 callback 尾部。
    let observer = unsafe { Pin::into_inner_unchecked(timer.clone()) };
    until(
        || Arc::strong_count(&observer) == 2,
        "timer running reference returned",
    );
    drop(observer);
    EXIT.complete();
    assert_eq!(worker.join(), 125);
    let retained = PRODUCER.lock().take();
    drop((retained, timer));
    log::testing::catch_up(last + 1);
    let settled = reclaimed(initial);
    crate::test_println!("PASS execution-X25 seq_first={} seq_last={} records=5 task_records=4 timer_records=1 sched_locked=1 pool_locked=1 allocation_attempts={} utf8=1 truncated=1 trunc_len=510 independent=2 reclaimed={}", first, last, allocation_attempts, settled);
}

/// printer 真阻塞期间另一个普通 Task 完成明确业务计算，不能用控制者忙等冒充进度。
fn business_worker() -> i32 {
    BUSINESS_START.wait();
    let mut value = black_box(7usize);
    for index in 0..4096 {
        value = black_box(value.wrapping_mul(17).wrapping_add(index));
        BUSINESS.fetch_add(1, Ordering::Release);
    }
    black_box(value);
    126
}

/// 在固定 counter 截止及字节上界内填同一真实 UART，只有硬件首次拒收才算形成背压。
fn fill_uart() -> usize {
    const LIMIT: usize = 1024 * 1024;
    let end = arch::read_counter()
        .checked_add(arch::counter_frequency().checked_mul(2).unwrap())
        .unwrap();
    let mut filled = 0;
    let mut yield_at = 256;
    let newlines = [b'\n'; 16];
    loop {
        assert!(arch::read_counter() < end, "UART fill deadline exceeded");
        assert!(
            filled < LIMIT,
            "UART fill limit reached without backpressure"
        );
        let bytes = &newlines[..(LIMIT - filled).min(newlines.len())];
        let accepted = console::try_write(bytes);
        assert!(accepted <= bytes.len());
        if accepted == 0 {
            assert!(0 < filled, "UART rejected the initial fill");
            return filled;
        }
        filled += accepted;
        if yield_at <= filled {
            yield_at += 256;
            sched::yield_now();
        }
    }
}

/// 用已有定长栈缓冲核对完整编号正文，等长旧正文也不能通过 cursor 校验。
fn assert_numbered_body(message: &str, index: u64) {
    let mut expected = log::StackText::<32>::new();
    expected
        .write_fmt(format_args!("X26 record={}", index))
        .unwrap();
    assert_eq!(message.as_bytes(), expected.as_bytes());
}

/// 慢 UART 导致真实 TX 等待，稳定输出跨覆盖继续，独立 cursor 精确报告缺口而不重放。
fn test_uart() {
    let initial = baseline();
    let business = task::spawn_kernel_on(0, business_worker).unwrap();
    let received = console::received();
    crate::test_println!("EXECUTION UART HOLD");
    until(
        || received < console::received() && console::last_byte() == b'H',
        "fresh UART RX IRQ hold acknowledgement",
    );
    let before = console::testing::snapshot();
    let fill_bytes = fill_uart();
    let filled = console::testing::snapshot();
    assert_eq!(
        filled.accepted.checked_sub(before.accepted).unwrap(),
        fill_bytes
    );
    assert!(before.zero_writes < filled.zero_writes);
    let printer_before = log::testing::snapshot();
    log::testing::arm_pause(log::testing::PausePoint::AfterPartialWrite);
    let payload = [b'P'; 450];
    let text = core::str::from_utf8(&payload).unwrap();
    let held = crate::klog!(Level::Info, "X26 partial {}", text);
    {
        let saved = Cursor::from_sequence(held).read().unwrap();
        assert_eq!(saved.lost, 0);
        assert_eq!(saved.record.text().strip_prefix("X26 partial "), Some(text));
    }
    until(
        || log::testing::is_paused() && log::testing::service_state() == Some(State::Blocked),
        "printer blocked after real partial TX wait",
    );
    let paused = log::testing::snapshot();
    assert_eq!(paused.seq, held);
    assert!(0 < paused.offset && paused.offset < paused.length);
    assert!(printer_before.tx_waits < paused.tx_waits);
    let partial = console::testing::snapshot();
    assert!(filled.zero_writes < partial.zero_writes);
    assert!(before.tx_interrupts < partial.tx_interrupts);
    assert!(before.wakes < partial.wakes);
    let mut slow = Cursor::from_sequence(held + 1);
    let mut fast = Cursor::from_sequence(held + 1);
    BUSINESS_START.complete();
    assert_eq!(business.join(), 126);
    assert_eq!(BUSINESS.load(Ordering::Acquire), 4096);
    for index in 0..72u64 {
        let sequence = crate::klog!(Level::Info, "X26 record={}", index);
        assert_eq!(sequence, held + index + 1);
        let read = fast.read().unwrap();
        assert_eq!(read.record.seq, sequence);
        assert_eq!(read.lost, 0);
        assert_numbered_body(read.record.text(), index);
        assert!(fast.read().is_none());
    }
    assert!(fast.read().is_none());
    let first = slow.read().unwrap();
    assert_eq!(first.lost, 8);
    assert_eq!(first.record.seq, held + 9);
    assert_numbered_body(first.record.text(), 8);
    for index in 1..64u64 {
        let read = slow.read().unwrap();
        assert_eq!(read.lost, 0);
        assert_eq!(read.record.seq, held + 9 + index);
        assert_numbered_body(read.record.text(), 8 + index);
    }
    assert!(slow.read().is_none());
    assert!(slow.read().is_none());
    let still = log::testing::snapshot();
    assert_eq!(
        (still.seq, still.offset, still.length),
        (paused.seq, paused.offset, paused.length)
    );
    assert_eq!(log::testing::service_state(), Some(State::Blocked));
    log::testing::release_pause();
    let boundary = held + 73;
    log::testing::catch_up(boundary);
    log::testing::wait_service_blocked();
    let after = console::testing::snapshot();
    let complete = log::testing::snapshot();
    let zero_writes = after.zero_writes.checked_sub(before.zero_writes).unwrap();
    let tx_interrupts = after
        .tx_interrupts
        .checked_sub(before.tx_interrupts)
        .unwrap();
    let wakes = after.wakes.checked_sub(before.wakes).unwrap();
    let gaps = complete.gaps.checked_sub(printer_before.gaps).unwrap();
    assert!(0 < zero_writes && 0 < tx_interrupts && 0 < wakes);
    assert_eq!(gaps, 8);
    assert_eq!(complete.next_seq, boundary);
    assert_eq!(complete.progress - printer_before.progress, 65);
    assert!(printer_before.bytes < complete.bytes);
    let settled = reclaimed(initial);
    crate::test_println!("PASS execution-X26 held_seq={} offset={} length={} submitted=72 capacity=64 lost=8 slow_records=64 fast_records=72 no_replay=1 blocked=1 business={} zero_writes={} tx_interrupts={} wakes={} printer_gaps={} catchup={} reclaimed={} fill_bytes={} fill_zero=1 rx_ack=1", held, paused.offset, paused.length, BUSINESS.load(Ordering::Acquire), zero_writes, tx_interrupts, wakes, gaps, boundary, settled, fill_bytes);
}

/// 正常镜像仅在旧案例结束后追加两个完整真实案例。
pub(super) fn run() {
    test_records();
    test_uart();
}

/// 独立故障镜像先等待实际 printer Blocked，再进入生产 writer panic 或标准不可恢复堆分配。
#[cfg(any(
    feature = "execution-printer-oom-fault",
    feature = "execution-timekeeper-fault"
))]
pub(super) fn run_fault() -> ! {
    log::testing::wait_service_blocked();
    assert_eq!(log::testing::service_state(), Some(State::Blocked));
    let fault = if cfg!(feature = "execution-timekeeper-fault") {
        "timekeeper"
    } else {
        "printer-oom"
    };
    crate::test_println!(
        "EXECUTION X27 prepare fault={} printer_blocked=1 task={} cpu={}",
        fault,
        task::current_task().id().as_usize(),
        cpu::current().id
    );
    #[cfg(feature = "execution-timekeeper-fault")]
    {
        clock::testing::arm_writer_fault();
        clock::testing::maintain();
        panic!("timekeeper writer fault did not fire");
    }
    #[cfg(not(feature = "execution-timekeeper-fault"))]
    {
        mm::shrink_all();
        let (pages, objects, heap_null) = super::workqueue::exhaust_for_printer_fault();
        let page_null = usize::from(mm::alloc_pages(0, false).is_none());
        assert_eq!(page_null, 1);
        assert_eq!(heap_null, 12);
        assert_eq!(log::testing::service_state(), Some(State::Blocked));
        console::testing::emergency_write(format_args!(
            "EXECUTION X27 state fault=printer-oom pages={} objects={} page_null={} heap_null={}\n",
            pages, objects, page_null, heap_null
        ));
        let value = alloc::boxed::Box::new([black_box(0u8); 4096]);
        black_box(&value);
        panic!("standard allocation unexpectedly survived exhausted heap");
    }
}
