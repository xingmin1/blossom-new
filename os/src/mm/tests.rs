//! BSP/AP 普通执行流中的真实页压力、拥有权转移及直接 PCP 排空检查。

use crate::arch;
use crate::cpu;
use crate::mm::bootmem::PAGE_SIZE;
use crate::mm::{alloc_pages, buddy, drain_all_pages, pcp, OwnedPages};
use crate::sync::{IrqSaveGuard, SpinLock};
use core::mem::MaybeUninit;
use core::sync::atomic::{AtomicUsize, Ordering};

const MAX: usize = crate::boot::MAX_CPUS;
const PRESSURE_CAPACITY: usize = 65536;
const TRANSFER_PAGES: usize = 8;
const WORK_PAGES: usize = 32;
const ROUNDS: usize = 16;
const TRANSFER_ALLOC: usize = 1;
const TRANSFER_DROP: usize = 2;
const HIGH: usize = 3;
const REMOTE_CACHE: usize = 4;
const STREAM: usize = 5;
static STEP: AtomicUsize = AtomicUsize::new(0);
static ACK: [AtomicUsize; MAX] = [const { AtomicUsize::new(0) }; MAX];
static TURN: AtomicUsize = AtomicUsize::new(0);
static DRAINS: AtomicUsize = AtomicUsize::new(0);
static DRAIN_TICKET: AtomicUsize = AtomicUsize::new(0);
static HIGH_COUNTS: [AtomicUsize; MAX] = [const { AtomicUsize::new(0) }; MAX];
static LIVE: [AtomicUsize; MAX * WORK_PAGES] =
    [const { AtomicUsize::new(usize::MAX) }; MAX * WORK_PAGES];
static RING: [SpinLock<Option<OwnedPages>>; MAX * TRANSFER_PAGES] =
    [const { SpinLock::new(None) }; MAX * TRANSFER_PAGES];
// BSP 只通过原始指针写入/移动已初始化前缀；AP 在压力期间不访问此存储。
static mut PRESSURE: [MaybeUninit<OwnedPages>; PRESSURE_CAPACITY] =
    [const { MaybeUninit::uninit() }; PRESSURE_CAPACITY];

/// 在实际 counter 截止前等待协作状态，超时终止客体而不假设参与者已经停止。
fn until(mut ready: impl FnMut() -> bool, name: &str) {
    let end = arch::read_counter().wrapping_add(arch::counter_frequency() * 5);
    while !ready() {
        assert!(arch::read_counter() < end, "memory PCP timeout: {}", name);
        core::hint::spin_loop();
    }
}

/// 等待 acquire 观察到 BSP 发布的指定动作，不占用任何内存锁。
fn wait_step(step: usize) {
    until(|| STEP.load(Ordering::Acquire) == step, "phase");
}

/// 汇总停止分配/释放的参与者的完成状态，只有静止边界可作精确页账务。
fn join(step: usize, first: usize) {
    until(
        || {
            ACK[first..cpu::count()]
                .iter()
                .all(|ack| ack.load(Ordering::Acquire) == step)
        },
        "participants",
    );
}

/// 合计自由页；调用者只有在参与者静止时才可将两个锁域快照当成精确账务。
fn total_free() -> usize {
    buddy::free_pages() + pcp::cached_pages()
}

/// 排空静止的各核缓存，并验证全部自由页已回到唯一 buddy。
fn restored(initial: usize) {
    drain_all_pages();
    assert_eq!(pcp::cached_pages(), 0, "memory PCP drain left cached pages");
    assert_eq!(
        buddy::free_pages(),
        initial,
        "memory PCP free accounting did not recover"
    );
}

/// 用固定句柄先持有全部可用 order10 块，再真实耗尽 order0 页，返回已写入前缀长度。
///
/// # Safety
/// 只能由 BSP 独占调用；AP 必须等待，其他消费者不得持有并发页资源。
/// PRESSURE 必须为空，前次返回的前缀须已通过 release_pressure_pages 完整归还。
/// 容量不足会明确 panic，不会被报告成物理内存耗尽。
pub(crate) unsafe fn exhaust_global_pages() -> usize {
    assert_eq!(cpu::current().id, 0, "memory pressure requires BSP");
    let mut count = 0;
    for order in [buddy::MAX_ORDER, 0] {
        loop {
            assert!(
                count < PRESSURE_CAPACITY,
                "memory pressure handle storage is full"
            );
            let Some(pages) = alloc_pages(order, false) else {
                break;
            };
            // SAFETY: BSP 独占静态存储；count 已检查有界，该槽不在已初始化前缀内。
            // 写入移动唯一拥有者，不先读取未初始化的 MaybeUninit，也不构造数组引用。
            unsafe {
                core::ptr::addr_of_mut!(PRESSURE)
                    .cast::<MaybeUninit<OwnedPages>>()
                    .add(count)
                    .write(MaybeUninit::new(pages));
            }
            count += 1;
        }
    }
    count
}

/// 在 BSP 普通流逐个移动并归还压力句柄；返回后整个前缀重新为空。
///
/// # Safety
/// count 必须是最近一次 exhaust_global_pages 返回的原值，尚未归还过。
/// BSP 独占该静态存储，所有压力页的访问已经结束，AP 仍在等待。
pub(crate) unsafe fn release_pressure_pages(count: usize) {
    assert_eq!(cpu::current().id, 0, "memory pressure release requires BSP");
    assert!(
        count <= PRESSURE_CAPACITY,
        "memory pressure prefix is invalid"
    );
    for index in (0..count).rev() {
        // SAFETY: caller 保证整个前缀逐槽已写入且尚未归还；每槽仅移动读取一次，
        // 下一次压力开始只会先写槽，不会读取已移动的值；页 Drop 在任何锁外执行。
        let pages = unsafe {
            core::ptr::addr_of_mut!(PRESSURE)
                .cast::<MaybeUninit<OwnedPages>>()
                .add(index)
                .read()
                .assume_init()
        };
        drop(pages);
    }
}

/// 耗尽实际全局页后只归还两个非相邻页，验证高阶失败和不足 batch 的真实补充。
fn partial_batch() {
    drain_all_pages();
    let initial = total_free();
    let (batch, _) = pcp::parameters();
    assert!(
        2 < batch,
        "memory partial test requires PCP batch above two"
    );
    let mut samples: [Option<OwnedPages>; TRANSFER_PAGES] = [const { None }; TRANSFER_PAGES];
    for slot in &mut samples {
        *slot = Some(alloc_pages(0, false).expect("memory partial sample allocation failed"));
    }
    let mut pair = None;
    for (first, first_page) in samples.iter().enumerate() {
        for (second, second_page) in samples.iter().enumerate().skip(first + 1) {
            let a = first_page.as_ref().unwrap().phys_addr() / PAGE_SIZE;
            let b = second_page.as_ref().unwrap().phys_addr() / PAGE_SIZE;
            if 1 < a.abs_diff(b) {
                pair = Some((first, second));
                break;
            }
        }
        if pair.is_some() {
            break;
        }
    }
    let (first, second) = pair.expect("memory partial samples have no nonadjacent pair");
    let expected = [
        samples[first].as_ref().unwrap().phys_addr(),
        samples[second].as_ref().unwrap().phys_addr(),
    ];
    drain_all_pages();
    // SAFETY: BSP 独占压力句柄，AP 等待 TRANSFER_ALLOC；固定样本是唯一其他 live 页，
    // 只由本函数持有，不与任何参与者并发申请、释放或访问。
    let pressure = unsafe { exhaust_global_pages() };
    assert_eq!(
        total_free(),
        0,
        "memory partial pressure did not exhaust actual pages"
    );
    assert!(
        alloc_pages(0, false).is_none(),
        "memory partial OOM returned a page"
    );
    drop(samples[first].take());
    drop(samples[second].take());
    drain_all_pages();
    assert_eq!(
        buddy::free_pages(),
        2,
        "memory partial buddy does not hold two free pages"
    );
    assert_eq!(pcp::cached_pages(), 0, "memory partial cache was not empty");
    assert!(
        alloc_pages(1, false).is_none(),
        "memory partial combined fragmented pages"
    );
    assert_eq!(
        total_free(),
        2,
        "memory partial failed high order changed accounting"
    );
    let a = alloc_pages(0, false).expect("memory partial refill rejected first page");
    let refilled = 1 + pcp::cpu_cached_pages(0);
    assert_eq!(
        refilled, 2,
        "memory partial refill did not retain its partial batch"
    );
    assert_eq!(
        buddy::free_pages(),
        0,
        "memory partial refill left a buddy page"
    );
    let b = alloc_pages(0, false).expect("memory partial refill rejected second page");
    assert_ne!(
        a.phys_addr(),
        b.phys_addr(),
        "memory partial returned duplicate ownership"
    );
    assert!(
        expected.contains(&a.phys_addr()) && expected.contains(&b.phys_addr()),
        "memory partial returned an unfreed page"
    );
    assert_eq!(
        total_free(),
        0,
        "memory partial second allocation left a free page"
    );
    assert!(
        alloc_pages(0, false).is_none(),
        "memory partial third allocation succeeded"
    );
    drop(a);
    drop(b);
    for slot in &mut samples {
        drop(slot.take());
    }
    // SAFETY: pressure 是本次独占 exhaust 的原始前缀长度，压力页从未读写或归还，
    // AP 仍等待，全部样本和补充页已经归还，没有并发消费者。
    unsafe { release_pressure_pages(pressure) };
    restored(initial);
    crate::println!(
        "PASS memory-pcp-partial free=2 partial={} batch={} pressure_handles={} restored={}",
        refilled,
        batch,
        pressure,
        initial
    );
}

/// 验证开/关 IRQ 时尝试成功、竞争失败和 guard 析构状态；私有锁无 handler 使用者。
fn try_lock_checks() {
    let lock = SpinLock::new(0usize);
    assert!(arch::irq_enabled(), "memory try-lock entry IRQ is disabled");
    for enabled in [true, false] {
        let disabled = if enabled {
            None
        } else {
            Some(IrqSaveGuard::new())
        };
        let guard = lock
            .try_lock()
            .expect("memory try-lock uncontended acquisition failed");
        assert!(
            !arch::irq_enabled(),
            "memory try-lock guard left IRQ enabled"
        );
        drop(guard);
        assert_eq!(
            arch::irq_enabled(),
            enabled,
            "memory try-lock success did not restore IRQ"
        );
        let saved = arch::irq_save();
        // SAFETY: 保存值来自当前 CPU，栈上 lock 无 IRQ action 或异核访问者；
        // 此处恢复仅用于构造随后 try-lock 的原始 IRQ 状态，当前没有锁 guard。
        unsafe { arch::irq_restore(saved) };
        let owner = lock.lock();
        // SAFETY: lock 是当前栈的私有测试对象，无 handler 或异核访问，CPU 不迁移；
        // 允许 IRQ 的期间只尝试同一锁，绝不阻塞取锁，析构 owner 前重新关闭 IRQ。
        unsafe { arch::irq_restore(saved) };
        assert!(
            lock.try_lock().is_none(),
            "memory try-lock acquired a held lock"
        );
        assert_eq!(
            arch::irq_enabled(),
            enabled,
            "memory try-lock failure did not restore IRQ"
        );
        arch::irq_save();
        drop(owner);
        assert_eq!(
            arch::irq_enabled(),
            enabled,
            "memory try-lock owner did not restore IRQ"
        );
        drop(disabled);
        assert!(
            arch::irq_enabled(),
            "memory try-lock outer guard did not restore IRQ"
        );
    }
    crate::println!("PASS memory-try-lock cpu={}", cpu::current().id);
}

/// 每核申请并初始化真实页，将唯一拥有者放入固定 ring，发布前保持全部页 live。
fn transfer_allocate(id: usize) {
    for index in 0..TRANSFER_PAGES {
        let mut pages = alloc_pages(0, false).expect("memory transfer allocation failed");
        LIVE[id * WORK_PAGES + index].store(pages.phys_addr() / PAGE_SIZE, Ordering::Relaxed);
        // SAFETY: pages 唯一持有真实页；首 usize 在本核独占期间写入，尚无读取者，
        // ring 发布以后只由邻核取得拥有权并读取该已初始化 word。
        unsafe {
            pages
                .as_mut_ptr()
                .cast::<usize>()
                .write(id * TRANSFER_PAGES + index)
        };
        let mut slot = RING[id * TRANSFER_PAGES + index].lock();
        assert!(slot.is_none(), "memory transfer ring slot is occupied");
        *slot = Some(pages);
    }
    ACK[id].store(TRANSFER_ALLOC, Ordering::Release);
}

/// 同时 live 的页以真实 PFN 去重；参与者已发布且等待下一动作，期间不会归还页。
fn unique_live(first: usize, pages: usize) {
    for id in first..cpu::count() {
        for index in 0..pages {
            let pfn = LIVE[id * WORK_PAGES + index].load(Ordering::Relaxed);
            assert_ne!(pfn, usize::MAX, "memory live PFN was not published");
            for other in first..=id {
                let limit = if other == id { index } else { pages };
                for previous in 0..limit {
                    assert_ne!(
                        pfn,
                        LIVE[other * WORK_PAGES + previous].load(Ordering::Relaxed),
                        "memory duplicate live page ownership"
                    );
                }
            }
        }
    }
}

/// 按 TURN 取得邻核 ring 并锁外最终归还，逐页核验当前释放 CPU 的缓存归属。
fn transfer_release(id: usize) {
    until(|| TURN.load(Ordering::Acquire) == id, "transfer CPU turn");
    let (batch, high) = pcp::parameters();
    let mut expected = 0;
    assert_eq!(
        pcp::cpu_cached_pages(id),
        0,
        "memory transfer release cache was not empty"
    );
    let source = (id + cpu::count() - 1) % cpu::count();
    for index in 0..TRANSFER_PAGES {
        let mut pages = {
            let mut slot = RING[source * TRANSFER_PAGES + index].lock();
            slot.take().expect("memory transfer ring page is missing")
        };
        assert_eq!(
            pages.phys_addr() / PAGE_SIZE,
            LIVE[source * WORK_PAGES + index].load(Ordering::Relaxed),
            "memory transfer changed page identity"
        );
        // SAFETY: source 在发布 ring 前已写入首 word，锁取得与 BSP phase acquire
        // 保证写入可见；此时 pages 是唯一拥有者，读取结束后才在槽锁外 Drop。
        let marker = unsafe { pages.as_mut_ptr().cast::<usize>().read() };
        assert_eq!(
            marker,
            source * TRANSFER_PAGES + index,
            "memory transfer initialized data changed"
        );
        drop(pages);
        expected += 1;
        if high <= expected {
            expected -= batch;
        }
        assert_eq!(
            pcp::cpu_cached_pages(id),
            expected,
            "memory transfer did not cache on releasing CPU"
        );
    }
    TURN.store(id + 1, Ordering::Release);
    ACK[id].store(TRANSFER_DROP, Ordering::Release);
}

/// 各核按 TURN 依次验证真实 high 边界，其他参与者等待，避免 drain 改变精确计数。
fn high_check(id: usize) {
    let (batch, high) = pcp::parameters();
    assert!(
        high <= WORK_PAGES && batch < high,
        "memory PCP parameters exceed fixed work storage"
    );
    drain_all_pages();
    let initial = total_free();
    let mut held: [Option<OwnedPages>; WORK_PAGES] = [const { None }; WORK_PAGES];
    for slot in &mut held[..high] {
        *slot = Some(alloc_pages(0, false).expect("memory high allocation failed"));
    }
    drain_all_pages();
    assert_eq!(
        buddy::free_pages(),
        initial - high,
        "memory high live accounting differs"
    );
    for slot in &mut held[..high - 1] {
        drop(slot.take());
    }
    assert_eq!(
        pcp::cpu_cached_pages(id),
        high - 1,
        "memory high threshold returned pages early"
    );
    assert_eq!(
        buddy::free_pages(),
        initial - high,
        "memory high below-threshold changed buddy"
    );
    drop(held[high - 1].take());
    let cached = pcp::cpu_cached_pages(id);
    assert_eq!(
        cached,
        high - batch,
        "memory high threshold did not return one batch"
    );
    assert_eq!(
        buddy::free_pages(),
        initial - high + batch,
        "memory high batch accounting differs"
    );
    HIGH_COUNTS[id].store(cached, Ordering::Relaxed);
    restored(initial);
    TURN.store(id + 1, Ordering::Release);
    ACK[id].store(HIGH, Ordering::Release);
}

/// 在远端留下恰好 batch 个真实缓存页，并停止操作直到 BSP 验证直接排空。
fn cache_checkpoint(id: usize) {
    let (batch, _) = pcp::parameters();
    let mut held: [Option<OwnedPages>; WORK_PAGES] = [const { None }; WORK_PAGES];
    for slot in &mut held[..batch] {
        *slot = Some(alloc_pages(0, false).expect("memory remote cache allocation failed"));
    }
    for slot in &mut held[..batch] {
        drop(slot.take());
    }
    assert_eq!(
        pcp::cpu_cached_pages(id),
        batch,
        "memory remote cache checkpoint count differs"
    );
    ACK[id].store(REMOTE_CACHE, Ordering::Release);
}

/// 等待当前阶段确实发生一次 BSP drain，防止参与者在排空者尚未运行时提前完成。
fn wait_drain() {
    let ticket = DRAIN_TICKET.load(Ordering::Acquire);
    until(
        || ticket <= DRAINS.load(Ordering::Acquire),
        "direct drain acknowledgement",
    );
}

/// 在固定轮次中持有真实页并等待 live 去重，再与 BSP 直接排空并行归还。
fn stream_worker(id: usize) {
    let (_, high) = pcp::parameters();
    for round in 0..ROUNDS {
        let allocate = STREAM + round * 2;
        wait_step(allocate);
        wait_drain();
        let mut held: [Option<OwnedPages>; WORK_PAGES] = [const { None }; WORK_PAGES];
        for (index, slot) in held[..high].iter_mut().enumerate() {
            let pages = alloc_pages(0, false).expect("memory parallel allocation failed");
            LIVE[id * WORK_PAGES + index].store(pages.phys_addr() / PAGE_SIZE, Ordering::Relaxed);
            *slot = Some(pages);
        }
        ACK[id].store(allocate, Ordering::Release);
        wait_step(allocate + 1);
        wait_drain();
        for slot in &mut held[..high] {
            drop(slot.take());
            assert!(
                pcp::cpu_cached_pages(id) < high,
                "memory parallel free exceeded high boundary"
            );
        }
        ACK[id].store(allocate + 1, Ordering::Release);
    }
}

/// 发布动作和本阶段最少一次 drain 的票号，所有参与者 acquire 后才开始该轮。
fn start_stream(step: usize) {
    DRAIN_TICKET.store(DRAINS.load(Ordering::Acquire) + 1, Ordering::Relaxed);
    STEP.store(step, Ordering::Release);
}

/// BSP 在 AP 正常申请/归还期间直接排空，计数仅作单份锁内边界断言。
fn drain_until(step: usize) {
    let (_, high) = pcp::parameters();
    let end = arch::read_counter().wrapping_add(arch::counter_frequency() * 5);
    loop {
        drain_all_pages();
        DRAINS.fetch_add(1, Ordering::Release);
        for id in 0..cpu::count() {
            assert!(
                pcp::cpu_cached_pages(id) < high,
                "memory direct drain observed invalid cache count"
            );
        }
        if ACK[1..cpu::count()]
            .iter()
            .all(|ack| ack.load(Ordering::Acquire) == step)
        {
            break;
        }
        assert!(arch::read_counter() < end, "memory parallel drain timeout");
        core::hint::spin_loop();
    }
}

/// 单核运行固定压力轮次并验证本地计数，不报告异核申请或远端排空证据。
fn local_stream(initial: usize) {
    let (_, high) = pcp::parameters();
    for _ in 0..ROUNDS {
        let mut held: [Option<OwnedPages>; WORK_PAGES] = [const { None }; WORK_PAGES];
        for (index, slot) in held[..high].iter_mut().enumerate() {
            let pages = alloc_pages(0, false).expect("memory local stream allocation failed");
            LIVE[index].store(pages.phys_addr() / PAGE_SIZE, Ordering::Relaxed);
            *slot = Some(pages);
        }
        unique_live(0, high);
        assert_eq!(
            total_free(),
            initial - high,
            "memory local stream live accounting differs"
        );
        for slot in &mut held[..high] {
            drop(slot.take());
            assert!(
                pcp::cpu_cached_pages(0) < high,
                "memory local stream exceeded high boundary"
            );
        }
        restored(initial);
        DRAINS.fetch_add(1, Ordering::Release);
    }
}

/// AP 只在普通执行流参加拥有权转移、高水位与有限压力；耗尽全局页期间保持等待。
pub(crate) fn secondary() {
    let id = cpu::current().id;
    assert!(0 < id, "memory secondary requires AP");
    try_lock_checks();
    wait_step(TRANSFER_ALLOC);
    transfer_allocate(id);
    wait_step(TRANSFER_DROP);
    transfer_release(id);
    wait_step(HIGH);
    until(|| TURN.load(Ordering::Acquire) == id, "high CPU turn");
    high_check(id);
    wait_step(REMOTE_CACHE);
    cache_checkpoint(id);
    stream_worker(id);
}

/// BSP 依次执行真实耗尽、异核最终归还、高水位和直接远端排空，静止后恢复精确账务。
pub(crate) fn primary() {
    assert_eq!(cpu::current().id, 0, "memory primary requires BSP");
    try_lock_checks();
    partial_batch();
    drain_all_pages();
    let initial = total_free();
    STEP.store(TRANSFER_ALLOC, Ordering::Release);
    transfer_allocate(0);
    join(TRANSFER_ALLOC, 0);
    unique_live(0, TRANSFER_PAGES);
    assert_eq!(
        total_free(),
        initial - TRANSFER_PAGES * cpu::count(),
        "memory transfer live accounting differs"
    );
    drain_all_pages();
    assert_eq!(
        pcp::cached_pages(),
        0,
        "memory transfer live boundary cache was not empty"
    );
    TURN.store(0, Ordering::Relaxed);
    STEP.store(TRANSFER_DROP, Ordering::Release);
    transfer_release(0);
    join(TRANSFER_DROP, 0);
    for slot in &RING[..cpu::count() * TRANSFER_PAGES] {
        assert!(
            slot.lock().is_none(),
            "memory transfer left an owned ring page"
        );
    }
    restored(initial);
    crate::println!(
        "PASS memory-pcp-transfer mode={} cpus={} live={} cross_cpu_drops={} restored={}",
        if 1 < cpu::count() {
            "cross-cpu"
        } else {
            "local"
        },
        cpu::count(),
        cpu::count() * TRANSFER_PAGES,
        if 1 < cpu::count() {
            cpu::count() * TRANSFER_PAGES
        } else {
            0
        },
        initial
    );

    TURN.store(0, Ordering::Relaxed);
    STEP.store(HIGH, Ordering::Release);
    high_check(0);
    join(HIGH, 0);
    let (batch, high) = pcp::parameters();
    for count in &HIGH_COUNTS[..cpu::count()] {
        assert_eq!(
            count.load(Ordering::Relaxed),
            high - batch,
            "memory CPU did not observe high return"
        );
    }
    if 1 < cpu::count() {
        STEP.store(REMOTE_CACHE, Ordering::Release);
        join(REMOTE_CACHE, 1);
        for id in 1..cpu::count() {
            assert_eq!(
                pcp::cpu_cached_pages(id),
                batch,
                "memory remote checkpoint lost cache pages"
            );
        }
        assert_eq!(
            total_free(),
            initial,
            "memory remote checkpoint accounting differs"
        );
        restored(initial);
        for round in 0..ROUNDS {
            let allocate = STREAM + round * 2;
            start_stream(allocate);
            drain_until(allocate);
            unique_live(1, high);
            drain_all_pages();
            assert_eq!(
                pcp::cached_pages(),
                0,
                "memory parallel held boundary cache is not empty"
            );
            assert_eq!(
                buddy::free_pages(),
                initial - high * (cpu::count() - 1),
                "memory parallel live accounting differs"
            );
            start_stream(allocate + 1);
            drain_until(allocate + 1);
            restored(initial);
        }
    } else {
        local_stream(initial);
    }
    restored(initial);
    crate::println!("PASS memory-pcp-drain mode={} cpus={} batch={} high={} high_events={} cached_after_high={} remote_cached={} rounds={} drains={} restored={}", if 1 < cpu::count() { "parallel-direct" } else { "local" }, cpu::count(), batch, high, cpu::count(), high - batch, batch * (cpu::count() - 1), ROUNDS, DRAINS.load(Ordering::Acquire), initial);
}
