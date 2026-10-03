#![no_std]
#![no_main]
#![feature(asm_experimental_arch)]
#![feature(alloc_error_handler)]
#![feature(allocator_api)]
#![deny(unsafe_op_in_unsafe_fn)]

#[cfg(all(feature = "refill-test", not(target_arch = "loongarch64")))]
compile_error!("refill-test requires the loongarch64 target");
#[cfg(all(feature = "execution-tests", feature = "boot-tests"))]
compile_error!("execution-tests cannot be combined with boot-tests");
#[cfg(all(feature = "execution-irq-fault", feature = "execution-preempt-fault"))]
compile_error!("execution IRQ and preempt faults require separate images");

extern crate alloc;
extern crate rlibc;

mod accounting;
mod arch;
mod boot;
mod console;
mod cpu;
mod irq;
mod mm;
mod platform;
mod sched;
mod sync;
mod task;
mod time;
mod timer;
mod wait;
mod workqueue;

#[cfg(feature = "boot-tests")]
mod boot_tests;
#[cfg(feature = "execution-tests")]
mod execution_tests;

/// 使用固件资源准备 CPU、MM、统一时间与中断，再发表内核任务并释放 AP。
///
/// # Safety
/// 仅由选定机器的 BSP 引导汇编调用一次，IRQ 关闭且 AP 未释放。
/// 汇编已建立内核/RAM/早期 UART 映射、独占启动栈，并清理普通 BSS。
/// 原始参数遵守固件协议，DTB 头部及声明的完整存储范围可读且不覆盖内核。
#[no_mangle]
pub unsafe extern "C" fn primary_main(arg0: usize, arg1: usize, arg2: usize) -> ! {
    console::initialize();
    let arguments = [arg0, arg1, arg2];
    let hardware_id = arch::boot_hardware_id(arguments);
    // SAFETY: BSP 汇编保留实际固件参数并建立 DTB 访问映射；本核为唯一解析者。
    let mut resources = unsafe { platform::discover(arguments, hardware_id) }
        .unwrap_or_else(|error| panic!("boot resource discovery failed: {}", error));
    let kernel = boot::kernel_region();
    assert!(
        resources.memory[..resources.memory_count]
            .iter()
            .any(|region| { region.start <= kernel.start && kernel.end <= region.end }),
        "kernel load region is outside actual RAM"
    );
    resources
        .reserve(kernel)
        .expect("kernel reservation failed");
    // SAFETY: DTB 解析和内核保留已完成，AP 尚未释放，BSP 仅发布一次。
    unsafe { boot::publish(resources) };
    let resources = boot::info();
    // SAFETY: 仍在独占 bootstrap 执行阶段；实际 RAM 包含当前镜像、栈和静态页表。
    unsafe { arch::finish_boot_mappings(resources) };
    // SAFETY: 当前为 BSP，IRQ 关闭、AP 未释放；静态栈和全部 CPU 输入均有效。
    unsafe { cpu::initialize(resources) };
    arch::init_current_cpu();
    // SAFETY: BSP 尚未释放 AP，IRQ 仍关闭；实际 RAM 永久可写的一一直映已完成，
    // BootInfo 已保留内核、启动栈、页表与固件资源。此时没有其他空闲 RAM 使用者，
    // 后续物理页使用以此描述符表为唯一边界，不通过别名修改或回收元数据 backing。
    unsafe { mm::initialize(resources) };
    // SAFETY: 唯一 BSP 在 MM 初始化后、IRQ 与 AP 开放前发表 BootInfo 的计数器频率。
    unsafe { time::initialize(resources) };
    #[cfg(not(feature = "boot-tests"))]
    timer::initialize_services().expect("timer service task allocation failed");
    #[cfg(not(feature = "boot-tests"))]
    workqueue::initialize_services().expect("worker pool service task allocation failed");
    #[cfg(not(feature = "boot-tests"))]
    task::spawn_kernel_on(0, init).expect("initial kernel task allocation failed");
    // SAFETY: 本核基址/异常入口已安装，实际控制器已映射；仅 BSP 初始化路由一次。
    unsafe { irq::initialize(resources) };
    // SAFETY: 默认 handler 不使用 context；函数和端口均永久存在，源尚未启用。
    unsafe { irq::register_uart(console::uart_handler, core::ptr::null_mut()) };
    irq::enable_uart();
    arch::enable_interrupts();
    cpu::publish_online();

    println!(
        "BOOT arch={} cpus={} bsp={} args={:#x},{:#x},{:#x} dtb={:#x}..{:#x}",
        arch::NAME,
        resources.cpu_count,
        resources.bsp_hardware_id,
        resources.raw_args[0],
        resources.raw_args[1],
        resources.raw_args[2],
        resources.dtb.start,
        resources.dtb.end
    );
    for region in &resources.memory[..resources.memory_count] {
        println!("RAM start={:#x} end={:#x}", region.start, region.end);
    }
    for region in &resources.reserved[..resources.reserved_count] {
        println!("RESERVED start={:#x} end={:#x}", region.start, region.end);
    }
    println!(
        "PCI config={:#x}..{:#x} windows={}",
        resources.pci.configuration.start,
        resources.pci.configuration.end,
        resources.pci.range_count
    );
    for window in &resources.pci.ranges[..resources.pci.range_count] {
        println!(
            "PCI WINDOW flags={:#x} bus={:#x} physical={:#x}..{:#x}",
            window.flags, window.bus_start, window.cpu.start, window.cpu.end
        );
    }
    // SAFETY: 全部记录已构造，本核能服务 IPI；每个 AP 的独占栈和 CPU 对象永久保留。
    unsafe { cpu::start_secondaries() };
    for id in 0..cpu::count() {
        let local = cpu::get(id);
        println!(
            "CPU id={} hardware={} local={:#x} stack={:#x}",
            local.id, local.hardware_id, local as *const _ as usize, local.stack_top
        );
    }
    println!("READY arch={} cpus={}", arch::NAME, cpu::count());

    #[cfg(feature = "boot-tests")]
    // SAFETY: 唯一 BSP 在全部 CPU 上线以后运行一次；测试缓冲区与串口协议由它独占。
    unsafe {
        boot_tests::primary()
    };
    #[cfg(not(feature = "boot-tests"))]
    sched::run();
}

/// 使用 BSP 发布的 CpuLocal 安装本核执行状态，只初始化本地中断。
///
/// # Safety
/// 仅由 AP 引导汇编调用一次；local 是本核已构造、对齐且永久有效的 CpuLocal。
/// 独占栈和所需映射已安装，记录来自 BSP，当前 IRQ 关闭。
#[no_mangle]
pub unsafe extern "C" fn secondary_main(local: *const cpu::CpuLocal) -> ! {
    // SAFETY: AP 汇编从 BSP 的静态记录读出本核对象，尚无本核 IRQ 访问。
    unsafe { arch::install_cpu_local(local) };
    arch::init_current_cpu();
    arch::enable_interrupts();
    cpu::publish_online();
    #[cfg(feature = "boot-tests")]
    // SAFETY: 本核对象、异常入口和 IRQ 已就绪；仅当前 AP 使用自身的测试状态。
    unsafe {
        boot_tests::secondary()
    };
    #[cfg(not(feature = "boot-tests"))]
    sched::run();
}

/// 在普通任务栈执行内核初始入口，返回后由调度器完成离栈回收。
#[cfg(not(feature = "boot-tests"))]
fn init() -> i32 {
    #[cfg(feature = "execution-tests")]
    execution_tests::run();
    #[cfg(not(feature = "execution-tests"))]
    {
        init_consumers();
        0
    }
}

/// 在普通任务之间传送有限数据，源登记后复查槽状态，结束时消费真实 join。
#[cfg(not(any(feature = "boot-tests", feature = "execution-tests")))]
fn init_consumers() {
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicBool, Ordering};
    use sync::SleepMutex;
    use wait::queue::{WaitNode, WaitQueue};
    use wait::Waiter;

    static INIT_DONE: AtomicBool = AtomicBool::new(false);

    struct Channel {
        slot: SleepMutex<Option<usize>>,
        changed: WaitQueue,
    }

    impl Channel {
        /// 在同一槽上执行生产或消费，失败时登记源并再次检查后才 park。
        fn transfer(&self, input: Option<usize>) -> usize {
            loop {
                {
                    let mut slot = self.slot.lock();
                    if input.is_some() == slot.is_none() {
                        let result = slot.take().unwrap_or(0);
                        *slot = input;
                        drop(slot);
                        self.changed.wake_all();
                        return result;
                    }
                }
                let waiter = Waiter::new();
                let mut node = core::pin::pin!(WaitNode::new(waiter.waker()));
                // SAFETY: node 在当前栈固定，Arc Channel 保持源地址有效；registration 不被遗忘，
                // 成功、恢复与正常退栈路径都在 node 和 waiter 析构之前撤销链接。
                let registration = unsafe { self.changed.register(node.as_mut()) };
                {
                    // 已登记资源等待时不能再创建 mutex 等待轮次；先撤该轮再等待锁。
                    let Some(mut slot) = self.slot.try_lock() else {
                        drop(registration);
                        drop(waiter);
                        drop(self.slot.lock());
                        continue;
                    };
                    if input.is_some() == slot.is_none() {
                        let result = slot.take().unwrap_or(0);
                        *slot = input;
                        drop(slot);
                        drop(registration);
                        drop(waiter);
                        self.changed.wake_all();
                        return result;
                    }
                }
                waiter.park();
                drop(registration);
                drop(waiter);
            }
        }
    }

    let watchdog = task::spawn_kernel_on(0, || {
        let deadline = time::now()
            .checked_add(time::Duration::from_secs(20))
            .expect("initial task deadline overflows");
        while !INIT_DONE.load(Ordering::Acquire) {
            assert!(time::now() < deadline, "initial task deadline");
            sched::yield_now();
        }
        0
    })
    .expect("init watchdog allocation failed");

    let channel = Arc::new(Channel {
        slot: SleepMutex::new(None),
        changed: WaitQueue::new(),
    });
    let producer_channel = Arc::clone(&channel);
    let producer = task::spawn_kernel(move || {
        for value in 1..=8 {
            producer_channel.transfer(Some(value));
        }
        8
    })
    .expect("producer allocation failed");
    let consumer =
        task::spawn_kernel(move || (0..8).map(|_| channel.transfer(None)).sum::<usize>() as i32)
            .expect("consumer allocation failed");

    let counter = Arc::new(SleepMutex::new(0usize));
    let guard = counter.lock();
    let first_counter = Arc::clone(&counter);
    let first = task::spawn_kernel_on(0, move || {
        for _ in 0..16 {
            *first_counter.lock() += 1;
            sched::yield_now();
        }
        16
    })
    .expect("counter task allocation failed");
    let second_counter = Arc::clone(&counter);
    let second = task::spawn_kernel_on(0, move || {
        for _ in 0..16 {
            *second_counter.lock() += 1;
            sched::yield_now();
        }
        16
    })
    .expect("counter task allocation failed");
    sched::yield_now();
    drop(guard);
    assert_eq!(producer.join(), 8);
    let sum = consumer.join();
    assert_eq!(sum, (1..=8).sum::<i32>());
    assert_eq!(first.join() + second.join(), 32);
    assert_eq!(*counter.lock(), 32);
    INIT_DONE.store(true, Ordering::Release);
    assert_eq!(watchdog.join(), 0);
    println!(
        "INIT completed task={} cpu={} produced=8 consumed=8 sum={} counter={}",
        task::current_task().id().as_usize(),
        cpu::current().id,
        sum,
        *counter.lock()
    );
}

/// 在不可继续的内核错误中记录真实 panic 信息并停止当前核。
#[panic_handler]
fn panic(info: &core::panic::PanicInfo<'_>) -> ! {
    console::panic(info)
}

/// 记录不可恢复的标准堆分配错误，复用不分配内存的有界应急 panic 输出。
#[alloc_error_handler]
fn allocation_error(layout: core::alloc::Layout) -> ! {
    panic!(
        "heap allocation failed: size={} align={}",
        layout.size(),
        layout.align()
    )
}
