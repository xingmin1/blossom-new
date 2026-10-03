//! BSP/AP 普通执行流中的真实页压力、对象拥有权交接及 PCP/SLUB 缓存检查。

use crate::arch;
use crate::cpu;
use crate::mm::bootmem::PAGE_SIZE;
use crate::mm::page::SlabLocation;
use crate::mm::{
    alloc_pages, buddy, drain_all_pages, flush_all, pcp, shrink_all, slub, OwnedPages,
};
use crate::sync::{IrqSaveGuard, SpinLock};
use core::mem::MaybeUninit;
use core::ptr::NonNull;
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
const SLUB_REMOTE: usize = STREAM + ROUNDS * 2;
const SLUB_STOPPED: usize = SLUB_REMOTE + 1;
const SLUB_DONE: usize = SLUB_STOPPED + 1;
const SLUB_SLOTS: usize = 96;
const SLUB_TRANSFER: usize = 6;
const SLUB_CLASS: usize = 11;
static OBJECTS: [AtomicUsize; SLUB_TRANSFER] = [const { AtomicUsize::new(0) }; SLUB_TRANSFER];
static REMOTE_COMMAND: AtomicUsize = AtomicUsize::new(0);
static REMOTE_ACK: AtomicUsize = AtomicUsize::new(0);
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
        assert!(arch::read_counter() < end, "memory timeout: {}", name);
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

/// 将固定拥有槽中的 live 地址转为对象指针；零值只表示已经交出的释放权。
fn object_pointer(address: usize) -> NonNull<u8> {
    NonNull::new(address as *mut u8).expect("memory SLUB object ownership is missing")
}

/// 在固定 live 集中核验地址唯一；仅检查身份，不产生第二份释放权。
fn unique_objects(held: &[usize]) {
    for (index, address) in held.iter().enumerate() {
        if *address == 0 {
            continue;
        }
        assert!(
            !held[..index].contains(address),
            "memory SLUB returned duplicate live ownership"
        );
    }
}

/// 在唯一 live 对象内初始化整段内容；地址签名同时覆盖对象首尾。
fn initialize_object(pointer: NonNull<u8>) {
    let address = pointer.as_ptr() as usize;
    let signature = address ^ 0x5a39_26c1_78ef_04bdu64 as usize;
    // SAFETY: pointer 刚由 4096 class 返回且尚未发布或释放；整段 4096 字节可写，
    // class 步长和页基址满足 usize 对齐，首尾 usize 均在同一个对象内。
    unsafe {
        pointer.as_ptr().write_bytes(0xa7, PAGE_SIZE);
        pointer.as_ptr().cast::<usize>().write(signature);
        pointer
            .as_ptr()
            .add(PAGE_SIZE - core::mem::size_of::<usize>())
            .cast::<usize>()
            .write(!signature);
    }
}

/// 读取由本核拥有槽或 acquire 交接取得的已初始化 live 对象，检查首尾与正文。
fn check_object(address: usize) {
    let pointer = object_pointer(address);
    let signature = address ^ 0x5a39_26c1_78ef_04bdu64 as usize;
    let word = core::mem::size_of::<usize>();
    // SAFETY: 所有调用点仍独占该 live 4096 对象，初始化已完成；远端调用先通过
    // release/acquire 取得唯一权，读取结束前没有归还者或其他写者。
    unsafe {
        assert_eq!(
            pointer.as_ptr().cast::<usize>().read(),
            signature,
            "memory SLUB live first word changed"
        );
        assert_eq!(
            pointer
                .as_ptr()
                .add(PAGE_SIZE - word)
                .cast::<usize>()
                .read(),
            !signature,
            "memory SLUB live last word changed"
        );
        for offset in word..PAGE_SIZE - word {
            assert_eq!(
                pointer.as_ptr().add(offset).read(),
                0xa7,
                "memory SLUB live object contents changed"
            );
        }
    }
}

/// 把新对象唯一权写入空槽；发布或读取前先检查全部 live 地址并完成初始化。
fn allocate_object(held: &mut [usize], index: usize) {
    assert_eq!(held[index], 0, "memory SLUB allocation slot is occupied");
    let pointer = slub::allocate(SLUB_CLASS).expect("memory SLUB allocation failed");
    held[index] = pointer.as_ptr() as usize;
    unique_objects(held);
    initialize_object(pointer);
}

/// 接收已由远端 release 确认归还的一对对象；先核验身份，再写入重新取得的唯一权。
fn receive_remote_object(held: &mut [usize], index: usize, available: &[usize]) {
    assert_eq!(
        held[index], 0,
        "memory SLUB remote receive slot is occupied"
    );
    let pointer = slub::allocate(SLUB_CLASS).expect("memory SLUB remote refill failed");
    let address = pointer.as_ptr() as usize;
    assert!(
        available.contains(&address),
        "memory SLUB refill returned an object still owned by remote CPU"
    );
    held[index] = address;
    unique_objects(held);
    initialize_object(pointer);
}

/// 消耗一个本核 live 槽的唯一释放权；先检查内容，清槽后不再访问对象。
fn release_object(held: &mut [usize], index: usize) {
    let address = core::mem::replace(&mut held[index], 0);
    check_object(address);
    // SAFETY: replace 已移动本槽唯一 live 对象释放权；没有别的拥有槽或远端访问者，
    // 内容读取已结束，本次 deallocate 后不再通过该地址访问对象。
    unsafe { slub::deallocate(object_pointer(address)) };
}

/// 在所有普通参与者等待时回收空 slab，再把 PCP 归还到真实 buddy 账务。
fn slub_restored(initial: usize) {
    shrink_all();
    assert_eq!(
        slub::backing_pages(),
        0,
        "memory SLUB shrink retained empty backing"
    );
    for class in 0..slub::SIZES.len() {
        assert_eq!(
            slub::node_snapshot(class).count,
            0,
            "memory SLUB shrink left node members"
        );
        for id in 0..cpu::count() {
            let state = slub::cpu_snapshot(class, id);
            assert_eq!(
                state.active,
                usize::MAX,
                "memory SLUB shrink left active responsibility"
            );
            assert_eq!(
                state.local_count, 0,
                "memory SLUB shrink left local objects"
            );
            assert_eq!(state.local, 0, "memory SLUB shrink left local freelist");
            assert_eq!(
                state.partial_count, 0,
                "memory SLUB shrink left CPU partial members"
            );
            assert_eq!(
                state.partial,
                usize::MAX,
                "memory SLUB shrink left CPU partial head"
            );
        }
    }
    restored(initial);
}

/// 通过真实 local 数量和 active 身份验证多次本地命中及跨 slab 切换。
fn slub_local(initial: usize) {
    let geometry = slub::geometry(SLUB_CLASS);
    assert_eq!(
        geometry.stride, PAGE_SIZE,
        "memory SLUB large class stride differs"
    );
    let count = geometry.objects * 3 + 1;
    assert!(
        count <= SLUB_SLOTS,
        "memory SLUB local storage is too small"
    );
    let mut held = [0; SLUB_SLOTS];
    let mut switches = 0;
    let mut hits = 0;
    for index in 0..count {
        let before = slub::cpu_snapshot(SLUB_CLASS, 0);
        allocate_object(&mut held, index);
        let after = slub::cpu_snapshot(SLUB_CLASS, 0);
        let slab = slub::slab_snapshot(object_pointer(held[index]));
        assert_eq!(
            slab.head_pfn, after.active,
            "memory SLUB allocation is not owner active"
        );
        assert_eq!(
            slab.location,
            SlabLocation::Active(0),
            "memory SLUB active role differs"
        );
        if 0 < before.local_count {
            assert_eq!(
                after.active, before.active,
                "memory SLUB local hit changed active"
            );
            assert_eq!(
                after.local_count + 1,
                before.local_count,
                "memory SLUB local hit lost objects"
            );
            hits += 1;
        } else if before.active != usize::MAX && before.active != after.active {
            switches += 1;
            let previous = slub::slab_snapshot(object_pointer(held[index - 1]));
            assert_eq!(
                previous.location,
                SlabLocation::Full,
                "memory SLUB exhausted active was not full"
            );
            assert_eq!(
                previous.inuse, previous.objects,
                "memory SLUB full count differs"
            );
        }
    }
    assert!(
        1 < switches && 0 < hits,
        "memory SLUB did not exercise local hits and active switches"
    );
    for index in 0..count {
        release_object(&mut held, index);
    }
    slub_restored(initial);
    crate::println!(
        "PASS memory-slub-local objects={} local_hits={} active_switches={} restored={}",
        count,
        hits,
        switches,
        initial
    );
}

/// AP 在普通执行流按有限命令消费对象交接；最终 release 确认停稳后不再操作 allocator。
fn slub_secondary(id: usize) {
    if id == 1 {
        wait_step(SLUB_REMOTE);
        for round in 1..=SLUB_TRANSFER / 2 {
            until(
                || REMOTE_COMMAND.load(Ordering::Acquire) == round,
                "SLUB remote command",
            );
            for slot in &OBJECTS[(round - 1) * 2..round * 2] {
                let address = slot.swap(0, Ordering::AcqRel);
                check_object(address);
                // SAFETY: BSP 在发布命令前已清除其 owning 槽，并 release 发布该地址；
                // swap 的 acquire 接收唯一 live 权且原子清空交接槽，内容访问已结束。
                unsafe { slub::deallocate(object_pointer(address)) };
            }
            REMOTE_ACK.store(round, Ordering::Release);
        }
    }
    wait_step(SLUB_STOPPED);
    ACK[id].store(SLUB_STOPPED, Ordering::Release);
    wait_step(SLUB_DONE);
}

/// 在停点证明远端只写 shared，并与 owner 剩余 local 命中并行，随后完成真实交接。
fn slub_remote(initial: usize) {
    if cpu::count() == 1 {
        crate::println!("SCOPE memory-slub-remote requires=2");
        return;
    }
    let objects = slub::geometry(SLUB_CLASS).objects;
    assert!(
        SLUB_TRANSFER < objects,
        "memory SLUB remote test needs retained live object"
    );
    let mut held = [0; SLUB_SLOTS];
    for index in 0..objects {
        allocate_object(&mut held, index);
    }
    let anchor = object_pointer(held[SLUB_TRANSFER]);
    let head = slub::slab_snapshot(anchor).head_pfn;
    assert_eq!(
        slub::cpu_snapshot(SLUB_CLASS, 0).local_count,
        0,
        "memory SLUB remote owner local was not exhausted"
    );
    // 只保留无拥有权的身份快照；原槽清空并发布后，这些整数不得用于对象访问或释放。
    let transferred: [usize; SLUB_TRANSFER] = core::array::from_fn(|index| held[index]);
    for (index, slot) in OBJECTS.iter().enumerate() {
        let address = core::mem::replace(&mut held[index], 0);
        slot.store(address, Ordering::Release);
    }
    STEP.store(SLUB_REMOTE, Ordering::Release);
    REMOTE_COMMAND.store(1, Ordering::Release);
    for round in 1..=SLUB_TRANSFER / 2 {
        until(
            || REMOTE_ACK.load(Ordering::Acquire) == round,
            "SLUB remote frees",
        );
        let before = slub::cpu_snapshot(SLUB_CLASS, 0);
        let shared = slub::slab_snapshot(anchor);
        assert_eq!(
            before.active, head,
            "memory SLUB remote free changed owner active"
        );
        assert_eq!(
            before.local_count, 0,
            "memory SLUB remote free changed owner local"
        );
        assert_eq!(
            shared.location,
            SlabLocation::Active(0),
            "memory SLUB remote free unfroze active"
        );
        assert_eq!(
            shared.inuse,
            objects - 2,
            "memory SLUB remote shared count differs"
        );
        assert_ne!(
            shared.shared, 0,
            "memory SLUB remote free did not populate shared"
        );
        let first = (round - 1) * 2;
        receive_remote_object(&mut held, first, &transferred[first..first + 2]);
        let received = slub::cpu_snapshot(SLUB_CLASS, 0);
        assert_eq!(
            received.active, head,
            "memory SLUB shared handoff switched active"
        );
        assert_eq!(
            received.local_count, 1,
            "memory SLUB shared handoff lost a remote object"
        );
        let state = slub::slab_snapshot(anchor);
        assert_eq!(
            state.shared, 0,
            "memory SLUB shared handoff left objects behind"
        );
        assert_eq!(
            state.inuse, objects,
            "memory SLUB shared handoff inuse differs"
        );
        if round < SLUB_TRANSFER / 2 {
            REMOTE_COMMAND.store(round + 1, Ordering::Release);
        }
        // 下一对 remote free 与本次 owner local 命中同时获准运行；本地命中不会
        // 触碰仍在交接的对象，下一轮 acquire 停点才检查 shared 的真实数量。
        receive_remote_object(&mut held, first + 1, &transferred[first..first + 2]);
        assert_eq!(
            slub::cpu_snapshot(SLUB_CLASS, 0).local_count,
            0,
            "memory SLUB owner did not consume local"
        );
    }
    assert!(
        OBJECTS.iter().all(|slot| slot.load(Ordering::Acquire) == 0),
        "memory SLUB remote transfer retained ownership"
    );
    for index in 0..objects {
        release_object(&mut held, index);
    }
    STEP.store(SLUB_STOPPED, Ordering::Release);
    join(SLUB_STOPPED, 1);
    slub_restored(initial);
    crate::println!(
        "PASS memory-slub-remote owner=0 remote=1 objects={} shared_handoffs={} restored={}",
        objects,
        SLUB_TRANSFER / 2,
        initial
    );
}

/// 在静止边界遍历真实成员链，检查数量、角色以及 CPU/node 成员不重叠。
fn partial_members() {
    let node = slub::node_snapshot(SLUB_CLASS);
    let mut node_heads = [usize::MAX; SLUB_SLOTS];
    assert!(
        node.count <= SLUB_SLOTS,
        "memory SLUB node snapshot exceeds storage"
    );
    let mut head = node.partial;
    let mut previous = usize::MAX;
    let mut empty = 0;
    for index in 0..node.count {
        assert!(
            !node_heads[..index].contains(&head),
            "memory SLUB node list contains duplicate member"
        );
        node_heads[index] = head;
        let state =
            slub::slab_head_snapshot(head).expect("memory SLUB node points at freed backing");
        assert_eq!(
            state.location,
            SlabLocation::NodePartial,
            "memory SLUB node member role differs"
        );
        assert_eq!(
            state.previous, previous,
            "memory SLUB node previous link differs"
        );
        assert!(
            state.inuse < state.objects,
            "memory SLUB node member is full"
        );
        assert_eq!(
            state.cache, SLUB_CLASS,
            "memory SLUB node member cache differs"
        );
        if state.inuse == 0 {
            empty += 1;
        }
        previous = head;
        head = state.next;
    }
    assert_eq!(
        head,
        usize::MAX,
        "memory SLUB node count does not cover list"
    );
    assert!(
        empty <= slub::MIN_PARTIAL,
        "memory SLUB node retained too many empty slabs"
    );
    for id in 0..cpu::count() {
        let state = slub::cpu_snapshot(SLUB_CLASS, id);
        assert!(
            state.partial_count <= slub::CPU_PARTIAL_LIMIT,
            "memory SLUB CPU partial exceeded bound"
        );
        assert!(
            !node_heads[..node.count].contains(&state.active),
            "memory SLUB active is also node member"
        );
        let mut cpu_heads = [usize::MAX; slub::CPU_PARTIAL_LIMIT];
        let mut head = state.partial;
        for index in 0..state.partial_count {
            assert_ne!(head, state.active, "memory SLUB CPU partial is also active");
            assert!(
                !node_heads[..node.count].contains(&head),
                "memory SLUB partial belongs to node and CPU"
            );
            assert!(
                !cpu_heads[..index].contains(&head),
                "memory SLUB CPU list contains duplicate member"
            );
            cpu_heads[index] = head;
            let slab =
                slub::slab_head_snapshot(head).expect("memory SLUB CPU points at freed backing");
            assert_eq!(
                slab.location,
                SlabLocation::CpuPartial(id),
                "memory SLUB CPU partial was frozen or misplaced"
            );
            assert_eq!(
                slab.cache, SLUB_CLASS,
                "memory SLUB CPU partial cache differs"
            );
            head = slab.next;
        }
        assert_eq!(
            head,
            usize::MAX,
            "memory SLUB CPU partial count does not cover list"
        );
    }
}

/// 强制 full→CPU partial→node→active，验证 empty 责任及跨 shrink 的 live 内容寿命。
fn slub_partial(initial: usize) {
    let objects = slub::geometry(SLUB_CLASS).objects;
    let count = objects * 4 + 1;
    assert!(
        count + objects * 4 <= SLUB_SLOTS,
        "memory SLUB partial storage is too small"
    );
    let mut held = [0; SLUB_SLOTS];
    for index in 0..count {
        allocate_object(&mut held, index);
    }
    let first = slub::slab_snapshot(object_pointer(held[0]));
    let target = slub::slab_snapshot(object_pointer(held[objects + 1])).head_pfn;
    let backing = slub::backing_pages();
    for slab in 0..4 {
        assert_eq!(
            slub::slab_snapshot(object_pointer(held[slab * objects])).location,
            SlabLocation::Full,
            "memory SLUB full setup failed"
        );
    }
    release_object(&mut held, 0);
    let partial = slub::slab_head_snapshot(first.head_pfn)
        .expect("memory SLUB first free destroyed live slab");
    assert_eq!(
        partial.location,
        SlabLocation::CpuPartial(0),
        "memory SLUB full first free did not enter CPU partial"
    );
    partial_members();
    for index in 1..objects {
        release_object(&mut held, index);
    }
    let empty = slub::slab_head_snapshot(first.head_pfn)
        .expect("memory SLUB empty CPU partial was freed early");
    assert_eq!(
        empty.location,
        SlabLocation::CpuPartial(0),
        "memory SLUB empty lost CPU responsibility"
    );
    assert_eq!(
        empty.inuse, 0,
        "memory SLUB empty CPU partial is still in use"
    );
    assert_eq!(
        slub::backing_pages(),
        backing,
        "memory SLUB empty CPU partial released backing early"
    );
    release_object(&mut held, objects);
    release_object(&mut held, objects * 2);
    partial_members();
    assert!(
        0 < slub::node_snapshot(SLUB_CLASS).count,
        "memory SLUB partial bound did not move any slab to node"
    );
    flush_all();
    partial_members();
    assert_eq!(
        slub::slab_head_snapshot(target).unwrap().location,
        SlabLocation::NodePartial,
        "memory SLUB flush lost live node partial"
    );
    let mut allocated = count;
    while slub::cpu_snapshot(SLUB_CLASS, 0).active != target {
        assert!(
            allocated < count + objects * 4,
            "memory SLUB node partial was never reactivated"
        );
        allocate_object(&mut held, allocated);
        allocated += 1;
    }
    assert_eq!(
        slub::slab_head_snapshot(target).unwrap().location,
        SlabLocation::Active(0),
        "memory SLUB node acquisition did not freeze active"
    );
    partial_members();
    shrink_all();
    partial_members();
    for address in held.iter().copied().filter(|address| *address != 0) {
        check_object(address);
    }
    assert_eq!(
        slub::slab_snapshot(object_pointer(held[objects + 1])).head_pfn,
        target,
        "memory SLUB shrink changed live slab identity"
    );
    assert!(
        0 < slub::backing_pages(),
        "memory SLUB shrink freed live backing"
    );
    for index in 0..allocated {
        if held[index] != 0 {
            release_object(&mut held, index);
        }
    }
    slub_restored(initial);
    crate::println!("PASS memory-slub-partial full_to_cpu=1 cpu_to_node=1 node_to_active=1 empty_retained=1 live_across_shrink=1 bound={} restored={}", slub::CPU_PARTIAL_LIMIT, initial);
}

/// 复用全局压力句柄并只放回一个真实零阶页，证明高阶失败及实际 minimum 接管。
fn slub_fallback(initial: usize) {
    slub_restored(initial);
    let geometry = slub::geometry(SLUB_CLASS);
    assert!(
        0 < geometry.preferred_order,
        "memory SLUB fallback requires preferred high order"
    );
    assert_eq!(
        geometry.minimum_order, 0,
        "memory SLUB fallback minimum is not order zero"
    );
    let sample = alloc_pages(0, false).expect("memory SLUB fallback sample allocation failed");
    let expected = sample.phys_addr();
    drain_all_pages();
    // SAFETY: 所有 AP 已 release 确认 SLUB_STOPPED 并等待 DONE；全部 slab/PCP 已排空，
    // BSP 独占空 PRESSURE 存储，唯一额外页由 sample 持有，不与任何消费者并发。
    let pressure = unsafe { exhaust_global_pages() };
    assert_eq!(
        total_free(),
        0,
        "memory SLUB pressure did not exhaust actual pages"
    );
    assert!(
        alloc_pages(0, false).is_none(),
        "memory SLUB real page OOM returned a page"
    );
    assert!(
        slub::allocate(SLUB_CLASS).is_none(),
        "memory SLUB real OOM returned an object"
    );
    assert_eq!(
        slub::backing_pages(),
        0,
        "memory SLUB failed allocation retained backing"
    );
    drop(sample);
    drain_all_pages();
    assert_eq!(
        buddy::free_pages(),
        1,
        "memory SLUB fallback did not release exactly one page"
    );
    assert!(
        alloc_pages(geometry.preferred_order, false).is_none(),
        "memory SLUB preferred order succeeded with one page"
    );
    let mut held = [0; 1];
    allocate_object(&mut held, 0);
    let actual = slub::slab_snapshot(object_pointer(held[0]));
    assert_eq!(
        actual.order, 0,
        "memory SLUB fallback recorded preferred order"
    );
    assert_eq!(
        actual.objects, 1,
        "memory SLUB fallback capacity is not actual"
    );
    assert_eq!(
        actual.cache, SLUB_CLASS,
        "memory SLUB fallback cache differs"
    );
    assert_eq!(
        actual.head_pfn * PAGE_SIZE,
        expected,
        "memory SLUB fallback did not consume released page"
    );
    assert_eq!(
        slub::backing_pages(),
        1,
        "memory SLUB fallback backing count used preferred order"
    );
    assert_eq!(
        total_free(),
        0,
        "memory SLUB fallback did not consume only free page"
    );
    release_object(&mut held, 0);
    shrink_all();
    drain_all_pages();
    assert_eq!(
        slub::backing_pages(),
        0,
        "memory SLUB fallback backing was not returned"
    );
    assert_eq!(
        buddy::free_pages(),
        1,
        "memory SLUB fallback release used incorrect order"
    );
    // SAFETY: pressure 是本次 exhaust 原始前缀且从未归还；sample 和实际 fallback slab
    // 均已完整归还，AP 仍停稳，所有压力页访问结束并由 BSP 独占移动消费。
    unsafe { release_pressure_pages(pressure) };
    slub_restored(initial);
    crate::println!("PASS memory-slub-fallback preferred={} actual={} objects={} pressure_handles={} restored={}", geometry.preferred_order, actual.order, actual.objects, pressure, initial);
}

/// 在正常高阶 4096 slab 中释放非首页对象，检查真正 head/cache 和其他 live 内容。
fn slub_tail(initial: usize) {
    let geometry = slub::geometry(SLUB_CLASS);
    assert!(
        0 < geometry.preferred_order && 1 < geometry.objects,
        "memory SLUB tail requires normal high-order slab"
    );
    let mut held = [0; SLUB_SLOTS];
    for index in 0..geometry.objects {
        allocate_object(&mut held, index);
    }
    let first = slub::slab_snapshot(object_pointer(held[0]));
    let tail_index = geometry.objects - 1;
    let tail = slub::slab_snapshot(object_pointer(held[tail_index]));
    assert_eq!(
        first.order, geometry.preferred_order,
        "memory SLUB normal allocation did not use preferred order"
    );
    assert_eq!(
        first.objects, geometry.objects,
        "memory SLUB normal capacity differs from actual geometry"
    );
    assert_eq!(first.cache, SLUB_CLASS, "memory SLUB normal cache differs");
    assert_eq!(
        first.head_pfn,
        arch::virt_to_phys(held[0]) / PAGE_SIZE,
        "memory SLUB first object did not locate its actual backing head"
    );
    assert_eq!(
        tail.head_pfn, first.head_pfn,
        "memory SLUB tail lookup selected another head"
    );
    assert_eq!(tail.cache, SLUB_CLASS, "memory SLUB tail cache differs");
    assert_eq!(
        tail.order, first.order,
        "memory SLUB tail actual order differs"
    );
    let tail_pfn = arch::virt_to_phys(held[tail_index]) / PAGE_SIZE;
    assert!(
        first.head_pfn < tail_pfn && tail_pfn < first.head_pfn + (1 << first.order),
        "memory SLUB tail object is not on a backing tail page"
    );
    let backing = slub::backing_pages();
    release_object(&mut held, tail_index);
    assert_eq!(
        slub::backing_pages(),
        backing,
        "memory SLUB tail free released live backing"
    );
    for index in 0..tail_index {
        check_object(held[index]);
        let state = slub::slab_snapshot(object_pointer(held[index]));
        assert_eq!(
            state.head_pfn, first.head_pfn,
            "memory SLUB tail free changed surviving head"
        );
        assert_eq!(
            state.cache, SLUB_CLASS,
            "memory SLUB tail free changed surviving cache"
        );
        release_object(&mut held, index);
    }
    slub_restored(initial);
    crate::println!(
        "PASS memory-slub-tail order={} objects={} tail_pfn={} head_pfn={} restored={}",
        first.order,
        first.objects,
        tail_pfn,
        first.head_pfn,
        initial
    );
}

/// 在既有 MEMORY 协作内完成对象验收；真实压力前 acquire 确认全部 AP 已停止操作。
fn slub_primary(initial: usize) {
    slub_restored(initial);
    slub_local(initial);
    slub_remote(initial);
    if cpu::count() == 1 {
        STEP.store(SLUB_STOPPED, Ordering::Release);
    }
    join(SLUB_STOPPED, 1);
    slub_partial(initial);
    slub_fallback(initial);
    slub_tail(initial);
    STEP.store(SLUB_DONE, Ordering::Release);
}

/// AP 在同一个内存动作中处理页与对象交接；真实耗尽期间停止申请和归还。
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
    slub_secondary(id);
}

/// BSP 在一个内存动作中验证真实页压力和经典 SLUB，停止参与者后恢复精确账务。
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
    slub_primary(initial);
}
