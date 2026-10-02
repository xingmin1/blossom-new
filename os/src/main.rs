#![no_std]
#![no_main]
#![feature(asm_experimental_arch)]
#![deny(unsafe_op_in_unsafe_fn)]

#[cfg(all(feature = "refill-test", not(target_arch = "loongarch64")))]
compile_error!("refill-test requires the loongarch64 target");

extern crate rlibc;

mod arch;
mod boot;
mod console;
mod cpu;
mod irq;
mod platform;
mod sync;

#[cfg(feature = "boot-tests")]
mod boot_tests;

/// 使用固件原始参数构造全局资源、CPU 表和中断设施，然后释放 AP。
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
    cpu::idle();
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
    cpu::idle();
}

/// 在不可继续的内核错误中记录真实 panic 信息并停止当前核。
#[panic_handler]
fn panic(info: &core::panic::PanicInfo<'_>) -> ! {
    console::panic(info)
}
