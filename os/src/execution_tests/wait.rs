//! 使用生产 Task、等待源与页分配器验证一次等待和睡眠同步。

use super::{baseline, reclaimed};
use alloc::alloc::{alloc, dealloc};
use alloc::sync::Arc;
use core::alloc::Layout;
use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::execution_tests::until;
use crate::mm::SLUB_SIZES as CLASSES;
use crate::sync::{Completion, SleepMutex, SpinLock};
use crate::task::{self, State, Task};
use crate::wait::queue::{WaitNode, WaitQueue};
use crate::wait::{Waiter, Waker};
use crate::{cpu, mm, sched};

const TASKS: usize = 4;
const ROUNDS: usize = 4;
const PRESSURE_CAPACITY: usize = 65536;
static FINISHED: AtomicBool = AtomicBool::new(false);

struct Slot {
    task: SpinLock<Option<Arc<Task>>>,
    waker: SpinLock<Option<Waker>>,
    phase: AtomicUsize,
    active: AtomicUsize,
    maximum: AtomicUsize,
    switches: AtomicUsize,
}

impl Slot {
    /// 为固定数量的执行者预备观察存储，不在等待阶段分配。
    const fn new() -> Self {
        Self {
            task: SpinLock::new(None),
            waker: SpinLock::new(None),
            phase: AtomicUsize::new(0),
            active: AtomicUsize::new(0),
            maximum: AtomicUsize::new(0),
            switches: AtomicUsize::new(0),
        }
    }

    /// 在源锁外取得当前执行者的强引用，再向协调者发布只读身份。
    fn publish_task(&self) {
        let current = task::current_task();
        let previous = self.task.lock().replace(current);
        assert!(previous.is_none());
    }

    /// 克隆已经准备的控制块，只读调度状态时不保留观察锁。
    fn task(&self) -> Arc<Task> {
        Arc::clone(
            self.task
                .lock()
                .as_ref()
                .expect("worker task not published"),
        )
    }

    /// 对真实等待轮次发出通知，不持有保存 Waker 的测试锁。
    fn notify(&self) -> bool {
        let waker = self
            .waker
            .lock()
            .as_ref()
            .expect("worker waker not published")
            .clone();
        waker.notify()
    }

    /// 清空已经结束的任务与弱引用，所有最终析构均在短锁外发生。
    fn clear(&self) {
        let task = self.task.lock().take();
        let waker = self.waker.lock().take();
        drop((task, waker));
        self.phase.store(0, Ordering::Relaxed);
        self.active.store(0, Ordering::Relaxed);
        self.maximum.store(0, Ordering::Relaxed);
        self.switches.store(0, Ordering::Relaxed);
    }
}

static SLOTS: [Slot; TASKS] = [const { Slot::new() }; TASKS];
static QUEUE: WaitQueue = WaitQueue::new();
static DRAIN_QUEUE: WaitQueue = WaitQueue::new();
static DRAIN_RUNNING: AtomicBool = AtomicBool::new(true);
static DRAIN_INITIAL: AtomicUsize = AtomicUsize::new(0);
static DRAIN_REARMS: AtomicUsize = AtomicUsize::new(0);
static OLD_WAKER: SpinLock<Option<Waker>> = SpinLock::new(None);
static PAYLOAD: SleepMutex<usize> = SleepMutex::new(0);
static START_OOM: Completion = Completion::new();
static FINISH_OOM: Completion = Completion::new();
static OOM_PROGRESS: AtomicUsize = AtomicUsize::new(0);
static FAILED_TRY: AtomicUsize = AtomicUsize::new(0);
static PARKED: AtomicUsize = AtomicUsize::new(0);
static MIGRATIONS: AtomicUsize = AtomicUsize::new(0);
static EXIT_GATE: Completion = Completion::new();
static LOCAL_DROPS: AtomicUsize = AtomicUsize::new(0);
static TOKENS: Completion = Completion::new();
static CONSUMED: AtomicUsize = AtomicUsize::new(0);
static mut PRESSURE: [MaybeUninit<mm::OwnedPages>; PRESSURE_CAPACITY] =
    [const { MaybeUninit::uninit() }; PRESSURE_CAPACITY];
static mut OBJECTS: [MaybeUninit<(*mut u8, Layout)>; PRESSURE_CAPACITY] =
    [const { MaybeUninit::uninit() }; PRESSURE_CAPACITY];

/// 仅等待协调状态，不代替被测资源的阻塞等待。
fn blocked(slot: &Slot, phase: usize) {
    until(
        || {
            slot.phase.load(Ordering::Acquire) == phase
                && sched::test_state(&slot.task()) == State::Blocked
        },
        "wait-worker-blocked",
    );
    assert_eq!(sched::test_ready_occurrences(&slot.task()), 0);
}

/// 混合真实 yield 与源登记 park，执行计数跨切换保留，重复执行会使计数超一。
fn mixed_worker(index: usize) -> i32 {
    let slot = &SLOTS[index];
    slot.publish_task();
    for round in 1..=ROUNDS {
        let active = slot.active.fetch_add(1, Ordering::AcqRel) + 1;
        slot.maximum.fetch_max(active, Ordering::AcqRel);
        assert_eq!(active, 1);
        sched::yield_now();
        slot.switches.fetch_add(1, Ordering::Relaxed);
        let waiter = Waiter::new();
        let mut node = core::pin::pin!(WaitNode::new(waiter.waker()));
        // SAFETY: 静态 QUEUE 永久有效，node 在任务栈固定；registration 不被遗忘，
        // 每轮显式先撤链接，再销毁 waiter 和 node，park 不结束该栈借用。
        let registration = unsafe { QUEUE.register(node.as_mut()) };
        let previous = slot.waker.lock().replace(waiter.waker());
        drop(previous);
        slot.phase.store(round, Ordering::Release);
        waiter.park();
        slot.switches.fetch_add(1, Ordering::Relaxed);
        assert_eq!(
            sched::test_state(&slot.task()),
            State::Running(cpu::current().id)
        );
        assert_eq!(sched::test_ready_occurrences(&slot.task()), 0);
        assert_eq!(slot.active.fetch_sub(1, Ordering::AcqRel), 1);
        drop(registration);
        drop(waiter);
    }
    (slot.switches.load(Ordering::Acquire) / 2) as i32
}

/// 核对四个生产执行者的独占执行与真实 ready 成员数量。
fn test_exclusive() {
    let handles = core::array::from_fn::<_, TASKS, _>(|index| {
        task::spawn_kernel(move || mixed_worker(index)).expect("mixed worker allocation failed")
    });
    let mut ready_max = 0;
    for round in 1..=ROUNDS {
        for slot in &SLOTS {
            blocked(slot, round);
            assert_eq!(slot.active.load(Ordering::Acquire), 1);
            let waker = slot
                .waker
                .lock()
                .as_ref()
                .expect("mixed worker waker not published")
                .clone();
            assert!(waker.notify());
            assert!(!waker.notify());
            let ready = sched::test_ready_occurrences(&slot.task());
            assert!(ready <= 1);
            ready_max = ready_max.max(ready);
        }
    }
    let mut completed = 0;
    for handle in handles {
        assert_eq!(handle.join(), ROUNDS as i32);
        completed += 1;
    }
    let maximum = SLOTS
        .iter()
        .map(|slot| slot.maximum.load(Ordering::Acquire))
        .max()
        .unwrap();
    let switches: usize = SLOTS
        .iter()
        .map(|slot| slot.switches.load(Ordering::Acquire))
        .sum();
    assert_eq!(maximum, 1);
    assert_eq!(switches, TASKS * ROUNDS * 2);
    crate::test_println!(
        "PASS execution-X03 tasks={} switches={} max_active={} ready_max={}",
        completed,
        switches,
        maximum,
        ready_max
    );
    for slot in &SLOTS {
        assert_eq!(slot.active.load(Ordering::Acquire), 0);
        slot.clear();
    }
    assert_eq!(QUEUE.wake_all(), 0);
}

/// 发布一个真实等待轮次并在同一普通任务栈阻塞。
fn blocked_worker() -> i32 {
    let slot = &SLOTS[0];
    slot.publish_task();
    let waiter = Waiter::new();
    let mut node = core::pin::pin!(WaitNode::new(waiter.waker()));
    // SAFETY: QUEUE 永久有效且 node 已固定；registration 不被遗忘，
    // 恢复后先撤链接再退栈，等待期间 Task 执行栈仍由生产调度器拥有。
    let registration = unsafe { QUEUE.register(node.as_mut()) };
    let previous = slot.waker.lock().replace(waiter.waker());
    drop(previous);
    slot.phase.store(1, Ordering::Release);
    waiter.park();
    drop(registration);
    1
}

/// 每次真实恢复先撤旧轮再登记新 WaitId，停止后沿普通返回结束全部栈节点。
fn drain_worker(index: usize) -> i32 {
    let slot = &SLOTS[index];
    slot.publish_task();
    let mut round = 0;
    while DRAIN_RUNNING.load(Ordering::Acquire) {
        let waiter = Waiter::new();
        let mut node = core::pin::pin!(WaitNode::new(waiter.waker()));
        // SAFETY: DRAIN_QUEUE 永久有效，node 在本任务栈固定；registration 不被遗忘，
        // 每轮恢复显式先撤链再结束 waiter 与 node，停止只在本轮清理之后生效。
        let registration = unsafe { DRAIN_QUEUE.register(node.as_mut()) };
        round += 1;
        if round == 1 {
            DRAIN_INITIAL.fetch_add(1, Ordering::Release);
        } else {
            DRAIN_REARMS.fetch_add(1, Ordering::Release);
        }
        slot.phase.store(round, Ordering::Release);
        waiter.park();
        drop(registration);
        drop(waiter);
    }
    round as i32
}

/// 真实摘离后等待两任务重登记，入口成员预算不得追逐新轮次而延长本次 wake_all。
fn test_finite_drain() -> (usize, usize, usize) {
    if cpu::count() == 1 {
        return (0, 0, 0);
    }
    let first =
        task::spawn_kernel_on(1, || drain_worker(0)).expect("first drain task allocation failed");
    blocked(&SLOTS[0], 1);
    let second =
        task::spawn_kernel_on(1, || drain_worker(1)).expect("second drain task allocation failed");
    blocked(&SLOTS[1], 1);
    let initial = DRAIN_INITIAL.load(Ordering::Acquire);
    assert_eq!(initial, 2);
    let mut pops = 0;
    let accepted = DRAIN_QUEUE.test_wake_all_after_notify(|| {
        pops += 1;
        if pops <= initial {
            blocked(&SLOTS[pops - 1], 2);
        } else {
            // 旧无界摘离会进入第三回调；停止再登记使其有限返回，随后计数断言真实失败。
            DRAIN_RUNNING.store(false, Ordering::Release);
        }
    });
    assert_eq!(
        accepted, initial,
        "wake_all consumed newly registered waits"
    );
    assert_eq!(pops, initial, "wake_all exceeded initial member budget");
    let rearms = DRAIN_REARMS.load(Ordering::Acquire);
    assert_eq!(rearms, initial);
    DRAIN_RUNNING.store(false, Ordering::Release);
    assert_eq!(DRAIN_QUEUE.wake_all(), initial);
    assert_eq!(first.join(), 2);
    assert_eq!(second.join(), 2);
    SLOTS[0].clear();
    SLOTS[1].clear();
    assert_eq!(DRAIN_QUEUE.wake_all(), 0);
    (initial, pops, rearms)
}

/// 保留已登记的一轮等待主动 yield，使生产 scheduler 将该任务提交为 Ready。
fn ready_wait_worker() -> i32 {
    let slot = &SLOTS[0];
    slot.publish_task();
    let waiter = Waiter::new();
    let mut node = core::pin::pin!(WaitNode::new(waiter.waker()));
    // SAFETY: QUEUE 永久有效，node 在当前任务栈固定；registration 不被遗忘，
    // yield 保留该栈帧，恢复后先撤链，再结束 waiter 与 node 的栈借用。
    let registration = unsafe { QUEUE.register(node.as_mut()) };
    let previous = slot.waker.lock().replace(waiter.waker());
    drop(previous);
    slot.phase.store(1, Ordering::Release);
    sched::yield_now();
    waiter.park();
    assert_eq!(
        sched::test_state(&slot.task()),
        State::Running(cpu::current().id)
    );
    drop(registration);
    slot.phase.store(2, Ordering::Release);
    1
}

/// 在同一 CPU 上通知已经 Ready 但仍持有 active wait 的任务，不重复加入 ready。
fn notify_ready_wait() -> (usize, usize) {
    let slot = &SLOTS[0];
    let handle =
        task::spawn_kernel_on(0, ready_wait_worker).expect("ready wait task allocation failed");
    until(
        || {
            slot.phase.load(Ordering::Acquire) == 1
                && sched::test_state(&slot.task()) == State::Ready
        },
        "wait-worker-ready",
    );
    assert_eq!(sched::test_ready_occurrences(&slot.task()), 1);
    let waker = slot
        .waker
        .lock()
        .as_ref()
        .expect("ready wait waker not published")
        .clone();
    let accepted = waker.notify() as usize;
    let duplicate = (!waker.notify()) as usize;
    assert_eq!(sched::test_state(&slot.task()), State::Ready);
    assert_eq!(sched::test_ready_occurrences(&slot.task()), 1);
    assert_eq!((accepted, duplicate), (1, 1));
    assert_eq!(handle.join(), 1);
    assert_eq!(slot.phase.load(Ordering::Acquire), 2);
    slot.clear();
    (accepted, duplicate)
}

/// 让另一在线 CPU 的普通任务通知已经提交 Blocked 的协调者。
fn remote_notify() -> usize {
    if cpu::count() == 1 {
        return 0;
    }
    let slot = &SLOTS[0];
    slot.publish_task();
    let waiter = Waiter::new();
    let previous = slot.waker.lock().replace(waiter.waker());
    drop(previous);
    slot.phase.store(1, Ordering::Release);
    let handle = task::spawn_kernel_on(1, || {
        blocked(&SLOTS[0], 1);
        assert_eq!(cpu::current().id, 1);
        SLOTS[0].notify() as i32
    })
    .expect("remote notifier allocation failed");
    waiter.park();
    drop(waiter);
    let accepted = handle.join();
    assert_eq!(accepted, 1);
    slot.clear();
    accepted as usize
}

/// 制造登记前后、Blocked、SwitchingOut 和持 active wait 的 Ready 窗口。
fn test_windows() {
    let mut duplicate = 0;
    let pre_register = {
        let waiter = Waiter::new();
        let waker = waiter.waker();
        let accepted = waker.notify() as usize;
        duplicate += (!waker.notify()) as usize;
        let mut node = core::pin::pin!(WaitNode::new(waker));
        // SAFETY: QUEUE 永久有效，局部 node 已固定；registration 不被遗忘，
        // park 返回后显式撤链接，早通知不会使 node 提前离开该栈帧。
        let registration = unsafe { QUEUE.register(node.as_mut()) };
        waiter.park();
        assert_eq!(
            sched::test_state(&task::current_task()),
            State::Running(cpu::current().id)
        );
        drop(registration);
        accepted
    };
    let post_register = {
        let waiter = Waiter::new();
        let waker = waiter.waker();
        let mut node = core::pin::pin!(WaitNode::new(waker.clone()));
        // SAFETY: QUEUE 永久有效，局部 node 已固定；registration 不被遗忘，
        // 唤醒摘链或退出撤链都在 node 退栈前结束源锁访问。
        let registration = unsafe { QUEUE.register(node.as_mut()) };
        let accepted = QUEUE.wake_one() as usize;
        duplicate += (!waker.notify()) as usize;
        waiter.park();
        drop(registration);
        accepted
    };
    let handle = task::spawn_kernel_on(0, blocked_worker).expect("window task allocation failed");
    blocked(&SLOTS[0], 1);
    let blocked_accept = SLOTS[0].notify() as usize;
    duplicate += (!SLOTS[0].notify()) as usize;
    assert_eq!(sched::test_state(&SLOTS[0].task()), State::Ready);
    assert_eq!(sched::test_ready_occurrences(&SLOTS[0].task()), 1);
    assert_eq!(handle.join(), 1);
    SLOTS[0].clear();
    let mut switching = 0;
    {
        let waiter = Waiter::new();
        waiter.test_park_notify();
        switching += 1;
        duplicate += (!waiter.waker().notify()) as usize;
    }
    let (ready, ready_duplicate) = notify_ready_wait();
    duplicate += ready_duplicate;
    let remote = remote_notify();
    let (wake_initial, wake_pops, wake_rearms) = test_finite_drain();
    assert_eq!(
        (
            pre_register,
            post_register,
            blocked_accept,
            switching,
            ready,
            duplicate
        ),
        (1, 1, 1, 1, 1, 5)
    );
    assert_eq!(QUEUE.wake_all(), 0);
    crate::test_println!("PASS execution-X04 pre_register={} post_register={} blocked={} switching={} ready={} duplicate={} remote={} wake_initial={} wake_pops={} wake_rearms={}", pre_register, post_register, blocked_accept, switching, ready, duplicate, remote, wake_initial, wake_pops, wake_rearms);
}

/// 在同一任务结束 A 后登记 B，保留的旧 Waker 不得改变 B 的状态。
fn generation_worker() -> i32 {
    let slot = &SLOTS[0];
    slot.publish_task();
    let old_id = {
        let waiter = Waiter::new();
        let mut node = core::pin::pin!(WaitNode::new(waiter.waker()));
        // SAFETY: QUEUE 永久有效且 node 已固定；旧轮 registration 不被遗忘，
        // 显式撤链接后才结束 A 的 waiter 和 node，保留 Waker 不保留节点访问权。
        let registration = unsafe { QUEUE.register(node.as_mut()) };
        let previous = OLD_WAKER.lock().replace(waiter.waker());
        drop(previous);
        let id = waiter.id();
        drop(registration);
        id
    };
    let waiter = Waiter::new();
    assert_ne!(old_id, waiter.id());
    let mut node = core::pin::pin!(WaitNode::new(waiter.waker()));
    // SAFETY: QUEUE 永久有效且 B 的 node 已固定；registration 不被遗忘，
    // 真实恢复后先撤链接再结束 B 的 waiter 与栈帧，旧 Waker 不访问该节点。
    let registration = unsafe { QUEUE.register(node.as_mut()) };
    let previous = slot.waker.lock().replace(waiter.waker());
    drop(previous);
    slot.phase.store(1, Ordering::Release);
    waiter.park();
    drop(registration);
    slot.phase.store(2, Ordering::Release);
    2
}

/// 使用真正的两轮 WaitId 核对旧通知拒绝与新通知恢复。
fn test_generation() {
    let handle =
        task::spawn_kernel_on(0, generation_worker).expect("generation task allocation failed");
    blocked(&SLOTS[0], 1);
    let old = OLD_WAKER.lock().as_ref().unwrap().clone();
    let stale = (!old.notify()) as usize;
    assert_eq!(SLOTS[0].phase.load(Ordering::Acquire), 1);
    let blocked_count = (sched::test_state(&SLOTS[0].task()) == State::Blocked) as usize;
    assert_eq!(sched::test_ready_occurrences(&SLOTS[0].task()), 0);
    let fresh = SLOTS[0].notify() as usize;
    assert_eq!((stale, blocked_count, fresh), (1, 1, 1));
    assert_eq!(handle.join(), 2);
    assert_eq!(SLOTS[0].phase.load(Ordering::Acquire), 2);
    SLOTS[0].clear();
    let retained = OLD_WAKER.lock().take();
    drop((retained, old));
    assert_eq!(QUEUE.wake_all(), 0);
    crate::test_println!(
        "PASS execution-X05 stale={} blocked={} fresh={}",
        stale,
        blocked_count,
        fresh
    );
}

/// 对已经停稳的参与者读取 buddy、PCP 和 slab backing 合计。
fn page_total() -> usize {
    let stats = mm::stats();
    stats.buddy_free_pages + stats.pcp_cached_pages + stats.slab_backing_pages
}

/// 真正耗尽剩余页并保存每个拥有者，容量边界只能报错，不能作为 OOM 证据。
fn exhaust_pages() -> (usize, usize) {
    let mut count = 0;
    let mut held_pages = 0;
    mm::drain_all_pages();
    for order in (0..=10).rev() {
        loop {
            assert!(
                count < PRESSURE_CAPACITY,
                "execution page pressure storage is full"
            );
            let Some(pages) = mm::alloc_pages(order, false) else {
                break;
            };
            held_pages += pages.len() / 4096;
            // SAFETY: 唯一 CPU0 协调者独占 PRESSURE；前缀之外尚未初始化，保存唯一页拥有权。
            unsafe {
                core::ptr::addr_of_mut!(PRESSURE)
                    .cast::<MaybeUninit<mm::OwnedPages>>()
                    .add(count)
                    .write(MaybeUninit::new(pages));
            }
            count += 1;
        }
    }
    assert!(0 < held_pages);
    assert!(mm::alloc_pages(0, false).is_none());
    (count, held_pages)
}

/// 在页耗尽后消费每个 SLUB 类的剩余真实对象，直到 GlobalAlloc 返回 null。
fn exhaust_objects() -> (usize, usize) {
    let mut count = 0;
    let mut nulls = 0;
    for size in CLASSES {
        let layout = Layout::from_size_align(size, 8).unwrap();
        loop {
            assert!(
                count < PRESSURE_CAPACITY,
                "execution object pressure storage is full"
            );
            // SAFETY: MM 已初始化，layout 是有效非零请求；每个成功结果由本函数独占保存归还权。
            let pointer = unsafe { alloc(layout) };
            if pointer.is_null() {
                nulls += 1;
                break;
            }
            // SAFETY: 唯一协调者独占 OBJECTS，count 在边界内且尚未初始化，保存原指针与 Layout。
            unsafe {
                core::ptr::addr_of_mut!(OBJECTS)
                    .cast::<MaybeUninit<(*mut u8, Layout)>>()
                    .add(count)
                    .write(MaybeUninit::new((pointer, layout)));
            }
            count += 1;
        }
    }
    (count, nulls)
}

/// 在仍然 OOM 的恢复点再次请求每类对象与页，返回真实失败次数。
fn verify_oom() -> usize {
    assert!(mm::alloc_pages(0, false).is_none());
    let mut nulls = 1;
    for size in CLASSES {
        let layout = Layout::from_size_align(size, 8).unwrap();
        // SAFETY: MM 已初始化且 layout 有效；失败结果无拥有权，意外成功直接终止客体验收。
        let pointer = unsafe { alloc(layout) };
        assert!(
            pointer.is_null(),
            "execution allocator recovered before pressure release: size={} pointer={:p}",
            size,
            pointer
        );
        nulls += 1;
    }
    nulls
}

/// 移动消费完整压力前缀，真实对象与页的最后 Drop 均在 MM 与源锁外。
fn release_pressure(pages: usize, objects: usize) {
    for index in (0..objects).rev() {
        // SAFETY: 该前缀由本次 exhaust_objects 完整初始化，尚未读取或释放，只有协调者访问。
        let (pointer, layout) = unsafe {
            core::ptr::addr_of_mut!(OBJECTS)
                .cast::<MaybeUninit<(*mut u8, Layout)>>()
                .add(index)
                .read()
                .assume_init()
        };
        // SAFETY: pointer 是同 layout 的原 live 分配，唯一归还权刚移出；压力对象从未被解引用。
        unsafe { dealloc(pointer, layout) };
    }
    for index in (0..pages).rev() {
        // SAFETY: 该前缀由本次 exhaust_pages 完整初始化，拥有者只在此移动消费一次。
        let pages = unsafe {
            core::ptr::addr_of_mut!(PRESSURE)
                .cast::<MaybeUninit<mm::OwnedPages>>()
                .add(index)
                .read()
                .assume_init()
        };
        drop(pages);
    }
}

/// 已预备的 Task 在物理压力期间争锁，恢复后保留全部任务资源直到压力归还。
fn oom_worker() -> i32 {
    SLOTS[0].publish_task();
    SLOTS[0].phase.store(1, Ordering::Release);
    START_OOM.wait();
    SLOTS[0].phase.store(2, Ordering::Release);
    let mut guard = PAYLOAD.lock();
    *guard += 1;
    OOM_PROGRESS.fetch_add(1, Ordering::Release);
    drop(guard);
    FINISH_OOM.wait();
    1
}

/// 在真实页与全部 SLUB 类耗尽期间验证 Blocked→wake→锁竞争进度，并完整归还账务。
fn test_oom() {
    mm::shrink_all();
    mm::drain_all_pages();
    let baseline = page_total();
    let guard = PAYLOAD.lock();
    let handle = task::spawn_kernel_on(0, oom_worker).expect("OOM worker allocation failed");
    blocked(&SLOTS[0], 1);
    mm::shrink_all();
    let (page_handles, pages) = exhaust_pages();
    let (objects, class_nulls) = exhaust_objects();
    let mut nulls = 1 + class_nulls;
    START_OOM.complete();
    blocked(&SLOTS[0], 2);
    let blocked_count = (sched::test_state(&SLOTS[0].task()) == State::Blocked) as usize;
    drop(guard);
    until(
        || {
            OOM_PROGRESS.load(Ordering::Acquire) == 1
                && sched::test_state(&SLOTS[0].task()) == State::Blocked
        },
        "OOM-worker-progress",
    );
    nulls += verify_oom();
    let progress = OOM_PROGRESS.load(Ordering::Acquire);
    assert_eq!(*PAYLOAD.lock(), 1);
    release_pressure(page_handles, objects);
    FINISH_OOM.complete();
    assert_eq!(handle.join(), progress as i32);
    SLOTS[0].clear();
    mm::shrink_all();
    mm::drain_all_pages();
    let recovered = page_total();
    assert_eq!(recovered, baseline);
    crate::test_println!("PASS execution-X06 pages={} classes={} nulls={} blocked={} progress={} baseline={} recovered={}", pages, CLASSES.len(), nulls, blocked_count, progress, baseline, recovered);
}

/// 在同一 Task 持有 guard 时真实迁核并 park，其他竞争者只能等待 Release 解锁。
fn payload_worker(index: usize) -> i32 {
    sched::test_migrate_to(0);
    let slot = &SLOTS[index];
    slot.publish_task();
    assert!(PAYLOAD.try_lock().is_none());
    FAILED_TRY.fetch_add(1, Ordering::Relaxed);
    slot.phase.store(1, Ordering::Release);
    let mut updates = 0;
    for round in 0..ROUNDS {
        let mut guard = PAYLOAD.lock();
        let before = *guard;
        if index == 0 && round == 0 {
            if 1 < cpu::count() {
                sched::test_migrate_to(1);
                assert_eq!(cpu::current().id, 1);
                MIGRATIONS.fetch_add(1, Ordering::Relaxed);
                assert_eq!(*guard, before);
                sched::test_migrate_to(0);
                assert_eq!(cpu::current().id, 0);
                MIGRATIONS.fetch_add(1, Ordering::Relaxed);
            }
            let waiter = Waiter::new();
            let previous = slot.waker.lock().replace(waiter.waker());
            drop(previous);
            slot.phase.store(2, Ordering::Release);
            waiter.park();
            assert_eq!(*guard, before);
            PARKED.fetch_add(1, Ordering::Release);
        }
        assert_eq!(*guard, before);
        *guard = before + 1;
        updates += 1;
        drop(guard);
        sched::yield_now();
    }
    updates
}

/// 锁内非原子 payload 只能由 Acquire/Release 传递，失败 try_lock 不释放已有占有者。
fn test_mutex() {
    *PAYLOAD.lock() = 0;
    let guard = PAYLOAD.lock();
    for _ in 0..TASKS {
        assert!(PAYLOAD.try_lock().is_none());
        FAILED_TRY.fetch_add(1, Ordering::Relaxed);
    }
    let handles = core::array::from_fn::<_, TASKS, _>(|index| {
        task::spawn_kernel(move || payload_worker(index)).expect("payload task allocation failed")
    });
    for slot in &SLOTS {
        blocked(slot, 1);
    }
    drop(guard);
    blocked(&SLOTS[0], 2);
    assert!(PAYLOAD.try_lock().is_none());
    FAILED_TRY.fetch_add(1, Ordering::Relaxed);
    assert!(SLOTS[0].notify());
    let mut contenders = 0;
    let updates: usize = IntoIterator::into_iter(handles)
        .map(|handle| {
            let updates = handle.join() as usize;
            contenders += 1;
            updates
        })
        .sum();
    let visibility = *PAYLOAD.lock();
    let failed_try = FAILED_TRY.load(Ordering::Acquire);
    let parked = PARKED.load(Ordering::Acquire);
    let migrations = MIGRATIONS.load(Ordering::Acquire);
    assert_eq!(visibility, TASKS * ROUNDS);
    assert_eq!(updates, visibility);
    assert_eq!(parked, 1);
    assert_eq!(migrations, if cpu::count() == 1 { 0 } else { 2 });
    for slot in &SLOTS {
        slot.clear();
    }
    let (completion_waiters, tokens, stolen) = test_completion();
    crate::test_println!("PASS execution-X07 contenders={} failed_try={} updates={} visibility={} parked={} migrations={} completion_waiters={} tokens={} stolen={}", contenders, failed_try, updates, visibility, parked, migrations, completion_waiters, tokens, stolen);
}

/// 消费共享完成源的一个真实 token，普通返回以前发布实际消费次数。
fn token_worker(index: usize) -> i32 {
    let slot = &SLOTS[index];
    slot.publish_task();
    slot.phase.store(1, Ordering::Release);
    TOKENS.wait();
    CONSUMED.fetch_add(1, Ordering::Release);
    slot.phase.store(2, Ordering::Release);
    1
}

/// 四个已阻塞消费者取得累计 token，并检验被抢走的 token 不使已唤醒者误完成。
fn test_completion() -> (usize, usize, usize) {
    let handles = core::array::from_fn::<_, TASKS, _>(|index| {
        task::spawn_kernel_on(0, move || token_worker(index)).expect("token task allocation failed")
    });
    for slot in &SLOTS {
        blocked(slot, 1);
    }
    let mut tokens = 0;
    for _ in 0..TASKS {
        TOKENS.complete();
        tokens += 1;
    }
    let consumers: usize = IntoIterator::into_iter(handles)
        .map(|handle| handle.join() as usize)
        .sum();
    assert_eq!(consumers, tokens);
    assert_eq!(CONSUMED.load(Ordering::Acquire), consumers);
    assert!(!TOKENS.try_wait());
    for slot in &SLOTS {
        slot.clear();
    }

    let handle =
        task::spawn_kernel_on(0, || token_worker(0)).expect("token race task allocation failed");
    blocked(&SLOTS[0], 1);
    TOKENS.complete();
    assert_eq!(sched::test_state(&SLOTS[0].task()), State::Ready);
    let stolen = TOKENS.try_wait() as usize;
    assert_eq!(stolen, 1);
    blocked(&SLOTS[0], 1);
    assert_eq!(CONSUMED.load(Ordering::Acquire), consumers);
    TOKENS.complete();
    assert_eq!(handle.join(), 1);
    assert_eq!(CONSUMED.load(Ordering::Acquire), consumers + 1);
    assert!(!TOKENS.try_wait());
    SLOTS[0].clear();
    (consumers, tokens, stolen)
}

struct LocalDrop;

impl Drop for LocalDrop {
    /// 栈局部对象只能在真实入口正常返回时完成一次析构。
    fn drop(&mut self) {
        assert_eq!(LOCAL_DROPS.fetch_add(1, Ordering::AcqRel), 0);
    }
}

/// 留下真实外部 Arc<Task> 后等待退出门，局部值计算实际 join 返回结果。
fn exit_worker() -> i32 {
    let _local = LocalDrop;
    SLOTS[0].publish_task();
    SLOTS[0].phase.store(1, Ordering::Release);
    EXIT_GATE.wait();
    (1..=8).sum()
}

/// 外部 Task Arc 与观察句柄保留时，核对局部析构和完整 64KiB 栈的实际页账差。
fn test_exit() {
    let handle = task::spawn_kernel_on(0, exit_worker).expect("exit task allocation failed");
    blocked(&SLOTS[0], 1);
    let live = page_total();
    EXIT_GATE.complete();
    until(|| handle.try_join().is_some(), "wait-exit-result");
    let external = SLOTS[0]
        .task
        .lock()
        .take()
        .expect("exit task Arc not retained");
    assert_eq!(sched::test_state(&external), State::Exited);
    assert_eq!(Arc::strong_count(&external), 1);
    let retained = Arc::strong_count(&external) + handle.try_join().is_some() as usize;
    let drops = LOCAL_DROPS.load(Ordering::Acquire);
    let returned = page_total();
    let stack_pages = returned
        .checked_sub(live)
        .expect("exit stack page accounting decreased");
    assert_eq!(stack_pages, 16);
    assert_eq!(drops, 1);
    assert_eq!(retained, 2);
    let result = handle.join();
    assert_eq!(result, (1..=8).sum::<i32>());
    SLOTS[0].clear();
    drop(external);
    crate::test_println!(
        "PASS execution-X08 drops={} retained={} stack_pages={} result={}",
        drops,
        retained,
        stack_pages,
        result
    );
}

/// 顺序运行真实等待与同步案例，每个案例退出全部临时 Task 和源登记。
pub(crate) fn run() {
    let initial = baseline();
    let watchdog = task::spawn_kernel_on(0, || {
        let deadline =
            crate::arch::read_counter().wrapping_add(crate::arch::counter_frequency() * 20);
        while !FINISHED.load(Ordering::Acquire) {
            assert!(
                crate::arch::read_counter() < deadline,
                "execution wait suite deadline"
            );
            sched::yield_now();
        }
        0
    })
    .expect("wait watchdog allocation failed");
    test_exclusive();
    test_windows();
    test_generation();
    test_oom();
    test_mutex();
    test_exit();
    assert_eq!(QUEUE.wake_all(), 0);
    FINISHED.store(true, Ordering::Release);
    assert_eq!(watchdog.join(), 0);
    assert_eq!(QUEUE.test_members(), 0);
    assert_eq!(DRAIN_QUEUE.test_members(), 0);
    assert!(OLD_WAKER.lock().is_none());
    assert!(SLOTS
        .iter()
        .all(|slot| slot.task.lock().is_none() && slot.waker.lock().is_none()));
    for source in [&START_OOM, &FINISH_OOM, &EXIT_GATE, &TOKENS] {
        assert_eq!(source.test_waiting(), 0);
        assert!(!source.is_ready());
    }
    assert_eq!(reclaimed(initial), 1);
}
