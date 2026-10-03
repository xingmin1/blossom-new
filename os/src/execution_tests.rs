//! 在生产任务栈上验证切换、迁核与离栈归还，协调者也通过普通 yield 让出执行权。

mod time;
mod timer;
mod wait;
mod workqueue;
use crate::{arch, cpu, mm, sched, task, time as clock};
use core::hint::black_box;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

const ROUNDS: usize = 8;
const DEPTH: usize = 8;
const STACK_BYTES: usize = 64 * 1024;
const PAGE_BYTES: usize = 4096;
static GO: AtomicBool = AtomicBool::new(false);
static ARRIVED: AtomicUsize = AtomicUsize::new(0);
static TURN: AtomicUsize = AtomicUsize::new(0);
static STACKS: [AtomicUsize; 2] = [const { AtomicUsize::new(0) }; 2];
static STACK_BASES: [AtomicUsize; 2] = [const { AtomicUsize::new(0) }; 2];
static IDS: [AtomicUsize; 2] = [const { AtomicUsize::new(0) }; 2];
static STARTS: [AtomicUsize; 2] = [const { AtomicUsize::new(0) }; 2];
static STEPS: [AtomicUsize; 2] = [const { AtomicUsize::new(0) }; 2];
static ACTIVE: [AtomicUsize; 2] = [const { AtomicUsize::new(0) }; 2];
static DROPS: [AtomicUsize; 2] = [const { AtomicUsize::new(0) }; 2];
static FRAMES: AtomicUsize = AtomicUsize::new(0);

struct LocalDrop(usize);

impl Drop for LocalDrop {
    /// 在入口正常返回时计数，观察句柄必须看到每个栈局部对象只析构一次。
    fn drop(&mut self) {
        assert_eq!(DROPS[self.0].fetch_add(1, Ordering::AcqRel), 0);
    }
}

/// 对协作条件实施有限单调时间约束，每次重试都走生产调度安全点。
fn until(mut predicate: impl FnMut() -> bool, name: &str) {
    let end = clock::now()
        .checked_add(clock::Duration::from_secs(5))
        .expect("execution deadline overflow");
    while !predicate() {
        assert!(clock::now() < end, "execution timeout: {}", name);
        sched::yield_now();
    }
}

/// 用保持存活的独立栈帧包围多轮切换，返回时逐层核对栈内容和返回路径。
#[inline(never)]
fn deep_calls(slot: usize, depth: usize, root: usize) -> usize {
    let pattern = 0x51a0 + slot * 32 + depth;
    let frame = [pattern; 64];
    let address = black_box(&frame) as *const _ as usize;
    assert_eq!(address & !(STACK_BYTES - 1), root & !(STACK_BYTES - 1));
    let frames = if depth == 0 {
        assert!(address < root, "deep task stack did not descend");
        ARRIVED.fetch_add(1, Ordering::Release);
        until(|| GO.load(Ordering::Acquire), "task-start-gate");
        for round in 0..ROUNDS {
            until(|| TURN.load(Ordering::Acquire) == slot, "task-turn");
            assert_eq!(ACTIVE[slot].fetch_add(1, Ordering::AcqRel), 0);
            assert_eq!(cpu::current().id, 0);
            assert_eq!(
                task::current_task().id().as_usize(),
                IDS[slot].load(Ordering::Acquire)
            );
            assert!(
                arch::test_task_context_preservation(),
                "task register preservation failed"
            );
            assert_eq!(STEPS[slot].fetch_add(1, Ordering::AcqRel), round);
            assert_eq!(ACTIVE[slot].fetch_sub(1, Ordering::AcqRel), 1);
            TURN.store(1 - slot, Ordering::Release);
            sched::yield_now();
        }
        1
    } else {
        deep_calls(slot, depth - 1, root) + 1
    };
    assert!(black_box(&frame).iter().all(|value| *value == pattern));
    FRAMES.fetch_add(1, Ordering::Relaxed);
    frames
}

/// 记录实际入口、TaskId 与栈地址，局部 RAII 在正常返回前完成。
fn switch_worker(slot: usize) -> i32 {
    assert_eq!(STARTS[slot].fetch_add(1, Ordering::AcqRel), 0);
    let local = LocalDrop(slot);
    let root = black_box(&local) as *const _ as usize;
    let (base, bytes) = task::test_current_stack_range();
    assert_eq!(bytes, STACK_BYTES);
    assert_eq!(base % bytes, 0);
    let physical_root = arch::virt_to_phys(root);
    assert!(base <= physical_root && physical_root < base + bytes);
    STACK_BASES[slot].store(base, Ordering::Release);
    STACKS[slot].store(root, Ordering::Release);
    IDS[slot].store(task::current_task().id().as_usize(), Ordering::Release);
    assert_eq!(deep_calls(slot, DEPTH, root), DEPTH + 1);
    20 + slot as i32
}

/// 用已有页描述符定位实际栈块，只读取永久分类和原子 slab 关联。
fn check_stack_pages(base: usize) {
    let first = base / PAGE_BYTES;
    for pfn in first..first + STACK_BYTES / PAGE_BYTES {
        let descriptor = mm::pages()
            .get(pfn)
            .expect("task stack has no page descriptor");
        assert!(mm::pages()
            .free_ranges()
            .iter()
            .any(|range| range.start <= pfn && pfn < range.end));
        assert_eq!(descriptor.slab_head.load(Ordering::Acquire), usize::MAX);
    }
}

/// 在同一 CPU 交替执行两个实际任务，句柄保留期间核对 32 个栈页已归还。
fn test_switch_and_return() {
    let first = task::spawn_kernel_on(0, || switch_worker(0))
        .expect("first execution task allocation failed");
    let second = task::spawn_kernel_on(0, || switch_worker(1))
        .expect("second execution task allocation failed");
    until(|| ARRIVED.load(Ordering::Acquire) == 2, "task-deep-arrival");
    let stack0 = STACKS[0].load(Ordering::Acquire);
    let stack1 = STACKS[1].load(Ordering::Acquire);
    let id0 = IDS[0].load(Ordering::Acquire);
    let id1 = IDS[1].load(Ordering::Acquire);
    assert_ne!(id0, id1);
    assert_eq!(first.task_id().as_usize(), id0);
    assert_eq!(second.task_id().as_usize(), id1);
    assert!(STACK_BYTES <= (stack0 & !(STACK_BYTES - 1)).abs_diff(stack1 & !(STACK_BYTES - 1)));
    let base0 = STACK_BASES[0].load(Ordering::Acquire);
    let base1 = STACK_BASES[1].load(Ordering::Acquire);
    assert!(STACK_BYTES <= base0.abs_diff(base1));
    check_stack_pages(base0);
    check_stack_pages(base1);
    let live = mm::stats();
    GO.store(true, Ordering::Release);
    until(
        || first.try_join().is_some() && second.try_join().is_some(),
        "task-return",
    );
    assert_eq!(first.try_join(), Some(20));
    assert_eq!(second.try_join(), Some(21));
    let returned = mm::stats();
    let live_pages = live.buddy_free_pages + live.pcp_cached_pages + live.slab_backing_pages;
    let returned_pages =
        returned.buddy_free_pages + returned.pcp_cached_pages + returned.slab_backing_pages;
    assert_eq!(returned_pages, live_pages + 32);
    for slot in 0..2 {
        assert_eq!(STARTS[slot].load(Ordering::Acquire), 1);
        assert_eq!(STEPS[slot].load(Ordering::Acquire), ROUNDS);
        assert_eq!(ACTIVE[slot].load(Ordering::Acquire), 0);
        assert_eq!(DROPS[slot].load(Ordering::Acquire), 1);
    }
    assert_eq!(FRAMES.load(Ordering::Acquire), 2 * (DEPTH + 1));
    check_stack_pages(base0);
    check_stack_pages(base1);
    crate::println!("PASS execution-X01 task0={} task1={} stack0={:#x} stack1={:#x} rounds={} registers={} deep_frames={} starts=2 drops=2 order=4 reclaimed={} observers=2", id0, id1, stack0, stack1, ROUNDS, 2 * ROUNDS, FRAMES.load(Ordering::Acquire), returned_pages - live_pages);
    drop((first, second));
}

/// 在每次恢复点核对目的硬件 CPU 基址、稳定任务身份和同一栈局部对象地址。
fn migration_record(hop: usize, target: usize, id: usize, local: &usize, stack: usize) {
    assert_eq!(cpu::current().id, target);
    assert_eq!(arch::cpu_local(), cpu::get(target) as *const _);
    assert_eq!(cpu::current() as *const _, arch::cpu_local());
    assert_eq!(task::current_task().id().as_usize(), id);
    assert_eq!(task::current_task_id().map(|id| id.as_usize()), Some(id));
    assert_eq!(black_box(local) as *const _ as usize, stack);
    assert_eq!(*black_box(local), 0x7301);
    crate::println!(
        "EXECUTION X02 hop={} task={} cpu={} local={:#x} stack={:#x}",
        hop,
        id,
        target,
        arch::cpu_local() as usize,
        stack
    );
}

/// 强制同一个 Any 任务在全部在线 CPU 上往返两轮，仍由生产队列移交执行权。
fn migration_worker() -> i32 {
    let local = 0x7301usize;
    let stack = black_box(&local) as *const _ as usize;
    let id = task::current_task().id().as_usize();
    sched::test_migrate_to(0);
    migration_record(0, 0, id, &local, stack);
    for hop in 1..=2 * cpu::count() {
        let target = hop % cpu::count();
        sched::test_migrate_to(target);
        migration_record(hop, target, id, &local, stack);
    }
    31
}

/// 单核明确记录不适用；多核等待真实迁核入口返回后才输出成功证据。
fn test_migration() {
    if cpu::count() == 1 {
        crate::println!("N/A execution-X02 requires=2 cpus=1");
        return;
    }
    let handle = task::spawn_kernel(migration_worker).expect("migration task allocation failed");
    until(|| handle.try_join().is_some(), "task-migration-return");
    assert_eq!(handle.try_join(), Some(31));
    crate::println!(
        "PASS execution-X02 task={} cpus={} hops={} rounds=2",
        handle.task_id().as_usize(),
        cpu::count(),
        2 * cpu::count()
    );
}

/// 在真实普通任务持有禁止切换 guard 时触发同一生产安全边界。
fn run_fault() -> ! {
    let guard_kind = if cfg!(feature = "execution-irq-fault") {
        1
    } else {
        2
    };
    crate::println!(
        "EXECUTION X09 guard={} task={} cpu={}",
        guard_kind,
        task::current_task().id().as_usize(),
        cpu::current().id
    );
    if cfg!(feature = "execution-irq-fault") {
        let _guard = crate::sync::IrqSaveGuard::new();
        sched::yield_now();
    } else {
        let _guard = crate::sync::PreemptGuard::new();
        sched::yield_now();
    }
    panic!("execution fault crossed prohibited switch boundary");
}

/// 从正式 init 任务驱动已实现的执行案例，最后沿实际架构关机协议结束客体。
pub(crate) fn run() -> ! {
    assert_eq!(cpu::current().id, 0);
    assert!(task::current_task_id().is_some());
    timer::wait_services_blocked();
    workqueue::wait_services_blocked();
    crate::println!(
        "EXECUTION TEST READY arch={} cpus={} mode={}",
        arch::NAME,
        cpu::count(),
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        }
    );
    if cfg!(any(
        feature = "execution-irq-fault",
        feature = "execution-preempt-fault"
    )) {
        run_fault();
    }
    test_switch_and_return();
    test_migration();
    wait::run();
    time::run();
    timer::run();
    workqueue::run();
    crate::println!("EXECUTION TESTS PASS");
    arch::shutdown();
}
