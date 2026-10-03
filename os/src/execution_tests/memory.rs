//! 在同一普通任务中验证本核及迁核后的页、堆拥有、内容与完整停稳账务。

use alloc::alloc::{alloc, dealloc};
use core::alloc::Layout;
use core::hint::black_box;
use core::ptr::NonNull;

use super::{baseline, reclaimed, until};
use crate::mm::SLUB_SIZES as CLASSES;
use crate::sync::SpinLock;
use crate::{arch, cpu, mm, sched, task};

const PRESSURE_PAGES: usize = 64;
const PRESSURE_OBJECTS: usize = 96;
static RESULT: SpinLock<Option<Counts>> = SpinLock::new(None);

#[derive(Default)]
struct Counts {
    task: usize,
    visited: usize,
    hops: usize,
    local_rounds: usize,
    page_allocs: usize,
    page_frees: usize,
    heap_allocs: usize,
    heap_frees: usize,
    flushes: usize,
    pressure_pages: usize,
    pressure_objects: usize,
    content_checks: usize,
    cpu_checks: usize,
    stack_checks: usize,
}

struct StackWitness {
    address: usize,
    physical: (usize, usize),
}

struct HeapObject {
    pointer: NonNull<u8>,
    layout: Layout,
    pattern: u8,
}

impl HeapObject {
    /// 从标准全局堆取得原布局对象并写满内容，容量不足直接使本次验收失败。
    fn new(size: usize, pattern: u8) -> Self {
        let layout = Layout::from_size_align(size, 8).expect("X24 heap layout");
        // SAFETY: 固定类别均非零且 Layout 合法，唯一 MM 已初始化；普通任务不持锁或 IRQ guard。
        let pointer = NonNull::new(unsafe { alloc(layout) }).expect("X24 heap allocation failed");
        // SAFETY: alloc 返回满足 layout 的 live 独占对象，全部 size 字节均属于该分配；
        // 对象尚未发表给其他执行者，写入结束后才允许读取或归还。
        unsafe { pointer.as_ptr().write_bytes(pattern, size) };
        Self {
            pointer,
            layout,
            pattern,
        }
    }

    /// 在原拥有者仍存活时逐字节验证内容，借用在对象归还之前结束。
    fn check(&self) {
        // SAFETY: new 已初始化原布局的完整范围，当前任务独占对象且没有并发写者；
        // 本次只读切片不逃逸，Drop 尚未执行。
        let bytes = unsafe {
            core::slice::from_raw_parts(black_box(self.pointer.as_ptr()), self.layout.size())
        };
        assert!(
            bytes.iter().all(|byte| *byte == self.pattern),
            "X24 heap content changed"
        );
    }
}

impl Drop for HeapObject {
    /// 按保存的原布局交出唯一归还权，全部内容借用已在普通任务中结束。
    fn drop(&mut self) {
        // SAFETY: pointer 是 new 成功 alloc 返回的原首址，layout 从未修改；
        // 本对象不可复制，仍 live 且全部访问已结束，dealloc 只在唯一 Drop 中执行一次。
        unsafe { dealloc(self.pointer.as_ptr(), self.layout) };
    }
}

/// 取得唯一连续块并初始化完整字节范围，不将 MM 内部短锁或 CPU 借用带到调用者。
fn new_page(order: usize, pattern: u8, counts: &mut Counts) -> mm::OwnedPages {
    let mut pages = mm::alloc_pages(order, false).expect("X24 page allocation failed");
    assert_eq!(pages.order(), order);
    assert_eq!(pages.phys_addr() % pages.len(), 0);
    let len = pages.len();
    // SAFETY: pages 是生产入口返回的独占 live 连续块，直映永久可写，len 是完整块长度；
    // 页入口内部 guard 已结束，写入不与其他执行者共享，也未构造未初始化字节的引用。
    unsafe { pages.as_mut_ptr().cast::<u8>().write_bytes(pattern, len) };
    counts.page_allocs += 1;
    pages
}

/// 在一轮 MM 操作前后核对硬件 CPU 基址、同一任务身份、栈范围与局部对象。
fn check_execution(target: usize, local: &usize, witness: &StackWitness, counts: &mut Counts) {
    assert_eq!(cpu::current().id, target);
    assert_eq!(arch::cpu_local(), cpu::get(target) as *const _);
    assert_eq!(cpu::current() as *const _, arch::cpu_local());
    assert_eq!(task::current_task().id().as_usize(), counts.task);
    assert_eq!(
        task::current_task_id().map(|id| id.as_usize()),
        Some(counts.task)
    );
    counts.visited |= 1usize << target;
    counts.cpu_checks += 1;

    assert_eq!(black_box(local) as *const _ as usize, witness.address);
    assert_eq!(*black_box(local), 0x7324);
    assert_eq!(task::test_current_stack_range(), witness.physical);
    let physical = arch::virt_to_phys(witness.address);
    assert!(witness.physical.0 <= physical);
    assert!(physical + core::mem::size_of::<usize>() <= witness.physical.0 + witness.physical.1);
    counts.stack_checks += 1;
}

/// 在已预备的记录槽中持有单页、高阶页和全部十二类标准堆对象。
fn allocate_round(
    pages: &mut [Option<mm::OwnedPages>; PRESSURE_PAGES],
    objects: &mut [Option<HeapObject>; PRESSURE_OBJECTS],
    pattern: u8,
    counts: &mut Counts,
) {
    assert!(pages.iter().all(Option::is_none));
    assert!(objects.iter().all(Option::is_none));
    pages[0] = Some(new_page(0, pattern, counts));
    pages[1] = Some(new_page(3, pattern, counts));
    for (slot, size) in objects.iter_mut().zip(CLASSES) {
        *slot = Some(HeapObject::new(size, pattern));
        counts.heap_allocs += 1;
    }
}

/// 核验全部拥有对象的内容后逐个归还，最后一次访问与 Drop 均在锁外。
fn check_and_release(
    pages: &mut [Option<mm::OwnedPages>; PRESSURE_PAGES],
    objects: &mut [Option<HeapObject>; PRESSURE_OBJECTS],
    pattern: u8,
    counts: &mut Counts,
) {
    for slot in pages.iter_mut() {
        let Some(mut pages) = slot.take() else {
            continue;
        };
        let len = pages.len();
        // SAFETY: new_page 已写满整个 len 字节，当前任务仍持唯一连续块拥有；
        // 没有其他写者，只读切片在本块 Drop 前结束且不跨任务切换。
        let bytes =
            unsafe { core::slice::from_raw_parts(black_box(pages.as_mut_ptr().cast::<u8>()), len) };
        assert!(
            bytes.iter().all(|byte| *byte == pattern),
            "X24 page content changed"
        );
        counts.content_checks += 1;
        drop(pages);
        counts.page_frees += 1;
    }
    for slot in objects.iter_mut() {
        let Some(object) = slot.take() else {
            continue;
        };
        object.check();
        counts.content_checks += 1;
        drop(object);
        counts.heap_frees += 1;
    }
}

/// 普通可服务 IPI 的任务同步摘离各 CPU 缓存，长归并仍由调用任务完成。
fn flush(counts: &mut Counts) {
    mm::flush_all();
    counts.flushes += 1;
}

/// 同时持有固定六十四页与九十六个对象，排空 live 缓存后核验并完整释放。
fn pressure(
    pages: &mut [Option<mm::OwnedPages>; PRESSURE_PAGES],
    objects: &mut [Option<HeapObject>; PRESSURE_OBJECTS],
    counts: &mut Counts,
) {
    assert!(pages.iter().all(Option::is_none));
    assert!(objects.iter().all(Option::is_none));
    for slot in pages.iter_mut() {
        let owned = new_page(0, 0xd4, counts);
        counts.pressure_pages += owned.len() / 4096;
        *slot = Some(owned);
    }
    for (index, slot) in objects.iter_mut().enumerate() {
        *slot = Some(HeapObject::new(CLASSES[index % CLASSES.len()], 0xd4));
        counts.heap_allocs += 1;
        counts.pressure_objects += 1;
    }
    flush(counts);
    check_and_release(pages, objects, 0xd4, counts);
    flush(counts);
}

/// 一个真实 Any 任务完成两轮本核 MM，再携带已拥有对象往返全部在线 CPU。
fn performer() -> i32 {
    // 全部记录槽先存在于普通任务栈；压力期间不再创建 Task、完成对象或可扩容容器。
    let mut pages: [Option<mm::OwnedPages>; PRESSURE_PAGES] = [const { None }; PRESSURE_PAGES];
    let mut objects: [Option<HeapObject>; PRESSURE_OBJECTS] = [const { None }; PRESSURE_OBJECTS];
    let mut counts = Counts {
        task: task::current_task().id().as_usize(),
        ..Counts::default()
    };
    let local = 0x7324usize;
    let witness = StackWitness {
        address: black_box(&local) as *const _ as usize,
        physical: task::test_current_stack_range(),
    };

    // 首次分派允许任意核；建立 CPU0 起点不属于往返轮次，切换时没有 CPU 或 MM guard。
    if cpu::current().id != 0 {
        sched::test_migrate_to(0);
    }
    check_execution(0, &local, &witness, &mut counts);
    for round in 0..2 {
        let pattern = 0x24 + round as u8;
        allocate_round(&mut pages, &mut objects, pattern, &mut counts);
        check_and_release(&mut pages, &mut objects, pattern, &mut counts);
        flush(&mut counts);
        check_execution(0, &local, &witness, &mut counts);
        counts.local_rounds += 1;
    }
    if 1 < cpu::count() {
        for hop in 1..=2 * cpu::count() {
            let pattern = 0x40 + hop as u8;
            let origin = cpu::current().id;
            let target = hop % cpu::count();
            assert_ne!(origin, target);
            allocate_round(&mut pages, &mut objects, pattern, &mut counts);
            // 已拥有的页和原布局堆对象继续由同一栈持有；没有任何 IRQ、MM 锁或 CPU 借用跨核。
            sched::test_migrate_to(target);
            check_execution(target, &local, &witness, &mut counts);
            counts.hops += 1;
            check_and_release(&mut pages, &mut objects, pattern, &mut counts);
            flush(&mut counts);
        }
    }
    pressure(&mut pages, &mut objects, &mut counts);
    assert!(pages.iter().all(Option::is_none));
    assert!(objects.iter().all(Option::is_none));
    let previous = RESULT.lock().replace(counts);
    assert!(previous.is_none());
    24
}

/// 从普通协调任务等待唯一消费者离栈，归还完成句柄后比较完整 MM 与 metadata 基线。
pub(super) fn run() {
    assert!(RESULT.lock().is_none());
    let initial = baseline();
    let handle = task::spawn_kernel(performer).expect("X24 performer allocation failed");
    let id = handle.task_id().as_usize();
    until(|| handle.try_join().is_some(), "memory performer return");
    assert_eq!(handle.join(), 24);
    let counts = RESULT.lock().take().expect("X24 performer result missing");
    let cpus = cpu::count();
    assert_eq!(counts.task, id);
    assert_eq!(counts.visited, (1usize << cpus) - 1);
    assert_eq!(counts.hops, if cpus == 1 { 0 } else { 2 * cpus });
    assert_eq!(counts.local_rounds, 2);
    assert_eq!(counts.page_allocs, counts.page_frees);
    assert_eq!(counts.heap_allocs, counts.heap_frees);
    assert!(2 * (counts.hops + 2) <= counts.page_allocs);
    assert!(CLASSES.len() * (counts.hops + 2) <= counts.heap_allocs);
    assert!(counts.hops + 2 <= counts.flushes);
    assert!(PRESSURE_PAGES <= counts.pressure_pages);
    assert!(PRESSURE_OBJECTS <= counts.pressure_objects);
    assert!(counts.page_allocs + counts.heap_allocs <= counts.content_checks);
    assert_eq!(counts.cpu_checks, counts.stack_checks);
    assert!(counts.hops + 3 <= counts.cpu_checks);
    let returned = reclaimed(initial);
    crate::test_println!(
        "PASS execution-X24 task={} cpus={} visited={} hops={} local_rounds={} page_allocs={} page_frees={} heap_allocs={} heap_frees={} flushes={} pressure_pages={} pressure_objects={} content_checks={} cpu_checks={} stack_checks={} reclaimed={}",
        counts.task,
        cpus,
        counts.visited,
        counts.hops,
        counts.local_rounds,
        counts.page_allocs,
        counts.page_frees,
        counts.heap_allocs,
        counts.heap_frees,
        counts.flushes,
        counts.pressure_pages,
        counts.pressure_objects,
        counts.content_checks,
        counts.cpu_checks,
        counts.stack_checks,
        returned
    );
}
