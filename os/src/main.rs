#![no_std]
#![no_main]
#![feature(asm_experimental_arch)]
#![feature(alloc_error_handler)]
#![feature(allocator_api)]
#![deny(unsafe_op_in_unsafe_fn)]
#![cfg_attr(feature = "ktest", feature(panic_can_unwind))]

extern crate alloc;
extern crate rlibc;

mod accounting;
mod arch;
mod block;
mod boot;
mod cache;
mod console;
mod cpu;
mod drivers;
mod fs;
mod irq;
mod klog;
#[cfg(feature = "ktest")]
mod ktest;
mod mm;
mod platform;
mod sched;
mod storage;
mod sync;
mod syscall;
mod task;
mod time;
mod timer;
mod wait;
mod workqueue;

#[cfg(not(feature = "ktest"))]
use alloc::sync::Arc;

#[cfg(not(feature = "ktest"))]
use crate::drivers::DeviceInventory;

static EXECUTION_READY: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// 完成一次性固件资源与 CPU 准备，返回后不再保留构造数组。
///
/// # Safety
/// 调用者须满足 primary_main 的 BSP、映射和固件参数约束。
#[inline(never)]
unsafe fn initialize_boot(arguments: [usize; 3]) {
    console::initialize();
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
}

/// 使用固件资源准备 CPU、MM、统一时间与中断，再发表内核任务并释放 AP。
///
/// # Safety
/// 仅由选定机器的 BSP 引导汇编调用一次，IRQ 关闭且 AP 未释放。
/// 汇编已建立内核/RAM/早期 UART 映射、独占启动栈，并清理普通 BSS。
/// 原始参数遵守固件协议，DTB 头部及声明的完整存储范围可读且不覆盖内核；
/// LA 的 a1 命令行在 BootInfo 容量内可读，并由固件低 RAM 保留区保护。
#[no_mangle]
pub unsafe extern "C" fn primary_main(arg0: usize, arg1: usize, arg2: usize) -> ! {
    // 引导构造帧先真实返回，避免 LTO 把大数组留在 Boot 展开的 64KiB 栈上。
    // SAFETY: BSP 汇编满足本入口的独占状态、实际映射和固件参数约束。
    unsafe { initialize_boot([arg0, arg1, arg2]) };
    #[cfg(feature = "ktest")]
    let test_command = {
        let command = ktest::early();
        if ktest::requested_context(&command) == ::ktest::Context::Boot {
            ktest::run_boot(command);
        }
        command
    };
    #[cfg(feature = "ktest")]
    // SAFETY: 引导资源与本核状态已发表；命令已确认只走 Thread 路径。
    unsafe {
        run_kernel(test_command)
    }
    #[cfg(not(feature = "ktest"))]
    // SAFETY: 引导资源与本核状态已发表，IRQ 关闭且 AP 尚未释放。
    unsafe {
        run_kernel()
    }
}

/// 从已完成的引导进入完整服务；其长期构造帧不得覆盖 Boot 用例。
///
/// # Safety
/// 固件资源、永久映射和本核 CPU 状态已发表；IRQ 关闭且 AP 尚未释放。
#[inline(never)]
unsafe fn run_kernel(#[cfg(feature = "ktest")] test_command: ktest::RunCommand) -> ! {
    let resources = boot::info();
    // SAFETY: BSP 尚未释放 AP，IRQ 仍关闭；实际 RAM 永久可写的一一直映已完成，
    // BootInfo 已保留内核、启动栈、页表与固件资源。此时没有其他空闲 RAM 使用者，
    // 后续物理页使用以此描述符表为唯一边界，不通过别名修改或回收元数据 backing。
    unsafe { mm::initialize(resources) };
    // SAFETY: 唯一 BSP 在 MM 初始化后、IRQ 与 AP 开放前发表 BootInfo 的计数器频率。
    unsafe { time::initialize(resources) };
    timer::initialize_services().expect("timer service task allocation failed");
    workqueue::initialize_services().expect("worker pool service task allocation failed");
    fs::initialize_inode_eviction().expect("inode eviction service allocation failed");
    klog::initialize_service().expect("log printer task allocation failed");
    fs::tty::initialize_service().expect("TTY worker preparation failed");
    #[cfg(not(feature = "ktest"))]
    task::spawn_kernel_on(0, init).expect("initial kernel task allocation failed");
    #[cfg(feature = "ktest")]
    let _test_task = ktest::spawn_thread(test_command).expect("test suite Task allocation failed");
    // SAFETY: 本核基址/异常入口已安装，实际控制器已映射；仅 BSP 初始化路由一次。
    unsafe { irq::initialize(resources) };
    let uart_number = irq::map_source(resources.interrupts.uart_source)
        .expect("UART interrupt source is outside the IRQ domain");
    // SAFETY: handler 不使用 context；端口、映射和函数永久存在，尚未开放源。
    // token 在普通启动栈中保活到调度器停机，测试镜像将其移动给同步注销拥有者。
    let _uart_registration =
        unsafe { irq::register(uart_number, console::uart_handler, core::ptr::null_mut()) }
            .expect("UART IRQ action registration failed");
    #[cfg(feature = "ktest")]
    console::testing::restore_uart_registration(_uart_registration)
        .expect("default UART registration cannot be installed");
    console::enable_receive_interrupts();
    #[cfg(not(feature = "ktest"))]
    _uart_registration.enable();
    arch::enable_interrupts();
    cpu::publish_online();

    early_println!(
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
        early_println!("RAM start={:#x} end={:#x}", region.start, region.end);
    }
    for region in &resources.reserved[..resources.reserved_count] {
        early_println!("RESERVED start={:#x} end={:#x}", region.start, region.end);
    }
    early_println!(
        "PCI config={:#x}..{:#x} windows={}",
        resources.pci.configuration.start,
        resources.pci.configuration.end,
        resources.pci.range_count
    );
    for window in &resources.pci.ranges[..resources.pci.range_count] {
        early_println!(
            "PCI WINDOW flags={:#x} bus={:#x} physical={:#x}..{:#x}",
            window.flags,
            window.bus_start,
            window.cpu.start,
            window.cpu.end
        );
    }
    // SAFETY: 全部记录已构造，本核能服务 IPI；每个 AP 的独占栈和 CPU 对象永久保留。
    unsafe { cpu::start_secondaries() };
    for id in 0..cpu::count() {
        let local = cpu::get(id);
        early_println!(
            "CPU id={} hardware={} local={:#x} stack={:#x}",
            local.id,
            local.hardware_id,
            local as *const _ as usize,
            local.stack_top
        );
    }
    early_println!("READY arch={} cpus={}", arch::NAME, cpu::count());

    EXECUTION_READY.store(true, core::sync::atomic::Ordering::Release);
    enter_scheduler();
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
    enter_scheduler();
}

/// 全部 CPU 已上线且启动输出完成后才允许分派，等待中的 AP 仍能服务 IRQ/IPI。
fn enter_scheduler() -> ! {
    while !EXECUTION_READY.load(core::sync::atomic::Ordering::Acquire) {
        core::hint::spin_loop();
    }
    sched::run()
}

/// 在普通任务栈一次发现并保活存储，按明确启动参数读取系统盘，再完成有限消费者。
#[cfg(not(feature = "ktest"))]
fn init() -> i32 {
    let inventory = drivers::discover().expect("device resource discovery failed");
    report_device_resources(&inventory);
    let has_block = !inventory.mmio_blocks.is_empty()
        || inventory.pci.devices().iter().any(|device| {
            device.vendor_id == 0x1af4 && matches!(device.device_id, 0x1001 | 0x1042)
        });
    assert!(
        has_block,
        "filesystem boot initialization failed: {:?}",
        fs::FsError::MissingRoot
    );
    // SAFETY: init 是一次发现后全部块设备的唯一准备者；Bootstrap 永久映射仍 live，
    // 固定 Storage 表在发表前持全部 owner，未确认停止的设备不会在 init 返回时丢失。
    let storage =
        unsafe { storage::Storage::initialize(&inventory) }.expect("storage initialization failed");
    crate::klog!(
        klog::Level::Info,
        "STORAGE online devices={}",
        storage.count()
    );
    init_filesystems(storage).expect("filesystem boot initialization failed");
    init_consumers();
    drop(inventory);
    0
}

/// 普通 init 按明确根权限运行磁盘程序，全部路径、回写和缓存退场后才结束文件系统消费者。
#[cfg(not(feature = "ktest"))]
fn init_filesystems(storage: &storage::Storage) -> Result<(), task::ProgramError> {
    let selected = fs::system::boot_devices(boot::info().bootargs())?;
    let root = storage
        .disk(selected.root)
        .ok_or(fs::FsError::MissingRoot)?;
    let extra = selected
        .extra
        .map(|index| storage.disk(index).ok_or(fs::FsError::MissingRoot))
        .transpose()?;
    let inodes = Arc::try_new(fs::InodeCache::try_new(64)?).map_err(|_| fs::FsError::NoMemory)?;
    let inode_eviction = inodes.watch();
    let vfs = Arc::try_new(fs::system::mount_disks(
        root,
        extra,
        inodes,
        selected.root_mode,
    )?)
    .map_err(|_| fs::FsError::NoMemory)?;
    let context = fs::FsContext::new(vfs.root(), vfs.root())?;
    {
        let mountpoint = vfs.resolve(&context, b"/dev", fs::ResolveMode::Follow)?;
        let devfs = fs::devfs::try_new(0o755)?;
        vfs.mount(
            &mountpoint,
            fs::memfs::Filesystem::Memory(devfs),
            block::AccessMode::ReadOnly,
        )?;
    }
    let files = fs::FileTable::try_new(4)?;
    crate::klog!(
        klog::Level::Info,
        "ROOTFS selected root={} extra={:?} mode={:?}",
        selected.root,
        selected.extra,
        selected.root_mode
    );
    const PATHS: &[(&str, bool)] = &[
        ("/test/identity", false),
        ("/test/data.bin", false),
        ("/test/empty", false),
        ("/test/sparse.bin", false),
        ("/test/bin/probe.elf", false),
        ("/mnt/extra/test/identity", true),
        ("/mnt/extra/test/data.bin", true),
        ("/mnt/extra/test/empty", true),
        ("/mnt/extra/test/sparse.bin", true),
        ("/mnt/extra/test/bin/probe.elf", true),
    ];
    for &(pathname, extra) in PATHS {
        if extra && selected.extra.is_none() {
            continue;
        }
        let summary = fs::system::read_summary(&files, &vfs, &context, pathname.as_bytes())?;
        crate::klog!(
            klog::Level::Info,
            "fs-system-summary path={} bytes={} crc32c={:08x}",
            pathname,
            summary.bytes,
            summary.crc32c
        );
    }
    let graph = task::process::TaskGraph::try_new(128)?;
    run_disk_program(
        &graph,
        &vfs,
        &context,
        b"/test/bin/user-smoke",
        &[b"/test/bin/user-smoke", b"raw"],
        &[b"USER_SMOKE=1"],
        false,
    )?;
    // 该普通程序断言 EROFS，只在真实只读根验证；可写根由独立写入程序消费。
    if selected.root_mode == block::AccessMode::ReadOnly {
        run_disk_program(
            &graph,
            &vfs,
            &context,
            b"/test/bin/user-io",
            &[b"/test/bin/user-io", b"raw"],
            &[],
            false,
        )?;
    }
    const PROGRAMS: &[(&[u8], &[u8])] = &[
        (b"/test/bin/user-clone", b"F0"),
        (b"/test/bin/user-clone", b"t3"),
        (b"/test/bin/user-signal", b"f"),
        (b"/test/bin/user-lifecycle", b"u"),
    ];
    for &(program, argument) in PROGRAMS {
        run_disk_program(
            &graph,
            &vfs,
            &context,
            program,
            &[program, argument],
            &[],
            false,
        )?;
    }
    if selected.root_mode == block::AccessMode::ReadWrite {
        run_disk_program(
            &graph,
            &vfs,
            &context,
            b"/test/bin/user-writeback",
            &[b"/test/bin/user-writeback", b"a"],
            &[],
            false,
        )?;
        let pathname = "/test/writeback-dir/result";
        let summary = fs::system::read_summary(&files, &vfs, &context, pathname.as_bytes())?;
        assert_eq!(summary.bytes, 8229, "writeback result size is inconsistent");
        crate::klog!(
            klog::Level::Info,
            "fs-system-summary path={} bytes={} crc32c={:08x}",
            pathname,
            summary.bytes,
            summary.crc32c
        );
    }
    run_disk_program(
        &graph,
        &vfs,
        &context,
        b"/bin/libc-smoke",
        &[b"/bin/libc-smoke", b"stage7"],
        &[],
        true,
    )?;
    for script in [
        b"/test/scripts/no-option".as_slice(),
        b"/test/scripts/option".as_slice(),
    ] {
        run_disk_program(
            &graph,
            &vfs,
            &context,
            script,
            &[script, b"tail"],
            &[b"BLOSSOM_SCRIPT=stage7"],
            true,
        )?;
    }
    run_disk_program(
        &graph,
        &vfs,
        &context,
        b"/bin/busybox",
        &[b"/bin/busybox", b"echo", b"stage7 busybox"],
        &[],
        true,
    )?;
    drop(files);
    drop(graph);
    drop(context);
    vfs.prune_unused();
    drop(vfs);
    inode_eviction.wait_retired()?;
    drop(inode_eviction);
    crate::klog!(klog::Level::Info, "ROOTFS completed");
    Ok(())
}

/// 以完整 argv 准备磁盘程序，按调用者约定安装标准流，再核退出、离栈与全部子组回收。
#[cfg(not(feature = "ktest"))]
fn run_disk_program(
    graph: &Arc<task::process::TaskGraph>,
    vfs: &Arc<fs::Vfs>,
    context: &fs::FsContext,
    program: &[u8],
    argv: &[&[u8]],
    environment: &[&[u8]],
    console: bool,
) -> Result<(), task::ProgramError> {
    let arguments = task::exec::Arguments::from_kernel(program, argv, environment)?;
    let image = task::exec::prepare(vfs, context, arguments, task::Credentials::ROOT)?;
    let runtime =
        task::UserRuntime::try_new(Arc::clone(vfs), context.clone(), task::Credentials::ROOT)?;
    if console {
        runtime.install_console()?;
    }
    let prepared = task::prepare_program(graph, None, image, runtime, task::Placement::Any)?;
    let pid = prepared.pid();
    let child = prepared.publish()?;
    let task_id = child.task_id();
    let event = graph.wait(None, pid)?;
    let task_status = child.join();
    crate::klog!(
        klog::Level::Info,
        "USER exited program={} argc={} pid={} task={} status={} task_status={} interrupts={} timers={} cpu={}",
        core::str::from_utf8(program).expect("static program path is ASCII"),
        argv.len(),
        event.pid,
        event.task_id.as_usize(),
        event.status,
        task_status,
        event.user_interrupts,
        event.user_timer_interrupts,
        event.cpu
    );
    assert_eq!(event.pid, pid, "user wait returned another process");
    assert_eq!(event.task_id, task_id, "user wait returned another task");
    assert_eq!(event.status, 0, "ordinary user program failed");
    assert_eq!(task_status, event.status, "user join disagrees with wait");
    loop {
        match graph.wait_root() {
            Ok(event) => {
                crate::klog!(
                    klog::Level::Info,
                    "USER child-reaped pid={} task={} status={} interrupts={} timers={} cpu={}",
                    event.pid,
                    event.task_id.as_usize(),
                    event.status,
                    event.user_interrupts,
                    event.user_timer_interrupts,
                    event.cpu
                );
                assert_eq!(event.status, 0, "ordinary user child failed");
            }
            Err(task::process::ProcessError::NoChild) => return Ok(()),
            Err(error) => return Err(error.into()),
        }
    }
}

/// 只记录启用 MMIO 槽、实际 block 候选与 PCI 功能资源，不触碰设备队列或读取磁盘。
#[cfg(not(feature = "ktest"))]
fn report_device_resources(inventory: &DeviceInventory) {
    let resources = boot::info();
    for (slot, resource) in resources.virtio_mmio[..resources.virtio_mmio_count]
        .iter()
        .enumerate()
    {
        crate::klog!(
            klog::Level::Info,
            "MMIO slot={} physical={:#x}..{:#x} source={}",
            slot,
            resource.registers.start,
            resource.registers.end,
            resource.interrupt_source
        );
    }
    for candidate in &inventory.mmio_blocks {
        crate::klog!(
            klog::Level::Info,
            "MMIO BLOCK physical={:#x}..{:#x} version={} vendor={:#x} irq={:?}",
            candidate.registers.start,
            candidate.registers.end,
            candidate.version,
            candidate.vendor_id,
            candidate.interrupt
        );
    }
    for device in inventory.pci.devices() {
        crate::klog!(
            klog::Level::Info,
            "PCI BDF={:02x}:{:02x}.{} vendor={:04x} device={:04x} header={:#x} caps={} irq={:?}",
            device.bus,
            device.device,
            device.function,
            device.vendor_id,
            device.device_id,
            device.header_type,
            device.capability_count,
            device.interrupt
        );
        for (index, bar) in device.bars.iter().enumerate() {
            if let Some(bar) = bar {
                crate::klog!(
                    klog::Level::Info,
                    "PCI BAR BDF={:02x}:{:02x}.{} index={} bus={:#x} physical={:#x}..{:#x} prefetch={}",
                    device.bus,
                    device.device,
                    device.function,
                    index,
                    bar.bus_address,
                    bar.physical.start,
                    bar.physical.end,
                    bar.prefetchable
                );
            }
        }
    }
    crate::klog!(
        klog::Level::Info,
        "DEVICE discovery mmio-blocks={} pci-functions={}",
        inventory.mmio_blocks.len(),
        inventory.pci.devices().len()
    );
}

/// 在普通任务之间传送有限数据，源登记后复查槽状态，结束时消费真实 join。
#[cfg(not(feature = "ktest"))]
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
    let final_counter = *counter.lock();
    crate::klog!(
        klog::Level::Info,
        "INIT completed task={} cpu={} produced=8 consumed=8 sum={} counter={}",
        task::current_task_id()
            .expect("initial kernel task")
            .as_usize(),
        cpu::current().id,
        sum,
        final_counter
    );
}

/// 在不可继续的内核错误中记录真实 panic 信息并停止当前核。
#[panic_handler]
fn panic(info: &core::panic::PanicInfo<'_>) -> ! {
    #[cfg(feature = "ktest")]
    {
        if cfg!(panic = "unwind") && !arch::cpu_local().is_null() {
            ktest::panic::raise(info);
        }
        let kind = if info.can_unwind() { "panic" } else { "abi" };
        ktest::fatal_event(kind, format_args!("{}", info));
    }
    #[cfg(not(feature = "ktest"))]
    console::panic(info)
}

/// 记录不可恢复的标准堆分配错误，复用不分配内存的有界应急 panic 输出。
#[alloc_error_handler]
fn allocation_error(layout: core::alloc::Layout) -> ! {
    #[cfg(feature = "ktest")]
    ktest::fatal_event(
        "oom",
        format_args!(
            "heap allocation failed: size={} align={}",
            layout.size(),
            layout.align()
        ),
    );
    #[cfg(not(feature = "ktest"))]
    panic!(
        "heap allocation failed: size={} align={}",
        layout.size(),
        layout.align()
    )
}
