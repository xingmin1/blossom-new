#![no_std]
#![no_main]
#![feature(asm_experimental_arch)]

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

/// BSP 高地址主入口；底层汇编已保存引导寄存器并一次清理普通 BSS。
#[no_mangle]
pub extern "C" fn primary_main(arg0: usize, arg1: usize, arg2: usize) -> ! {
    console::initialize();
    let arguments = [arg0, arg1, arg2];
    let hardware_id = arch::boot_hardware_id(arguments);
    let mut resources = unsafe { platform::discover(arguments, hardware_id) }
        .unwrap_or_else(|error| panic!("启动输入解析失败：{}", error));
    let kernel = boot::kernel_region();
    assert!(resources.memory[..resources.memory_count].iter().any(|region| {
        region.start <= kernel.start && kernel.end <= region.end
    }), "内核物理装载区不在实际 RAM 内");
    resources.reserve(kernel).expect("内核启动资源保留失败");
    unsafe { boot::publish(resources) };
    let resources = boot::info();
    arch::finish_boot_mappings(resources);
    unsafe { cpu::initialize(resources) };
    arch::init_current_cpu();
    irq::initialize(resources);
    unsafe { irq::register_uart(console::uart_handler, core::ptr::null_mut()) };
    irq::enable_uart();
    cpu::publish_online();
    arch::enable_interrupts();

    println!("BOOT arch={} cpus={} args={:#x},{:#x},{:#x} dtb={:#x}..{:#x}",
        arch::NAME, resources.cpu_count, arg0, arg1, arg2, resources.dtb.start, resources.dtb.end);
    for region in &resources.memory[..resources.memory_count] {
        println!("RAM start={:#x} end={:#x}", region.start, region.end);
    }
    for region in &resources.reserved[..resources.reserved_count] {
        println!("RESERVED start={:#x} end={:#x}", region.start, region.end);
    }
    println!("PCI config={:#x}..{:#x} windows={}",
        resources.pci.configuration.start, resources.pci.configuration.end, resources.pci.range_count);
    cpu::start_secondaries();
    for id in 0..cpu::count() {
        let local = cpu::get(id);
        println!("CPU id={} hardware={} local={:#x} stack={:#x}",
            local.id, local.hardware_id, local as *const _ as usize, local.stack_top);
    }
    println!("READY arch={} cpus={}", arch::NAME, cpu::count());

    #[cfg(feature = "boot-tests")]
    boot_tests::primary();
    #[cfg(not(feature = "boot-tests"))]
    cpu::idle();
}

/// AP 只安装自己的执行现场和中断设施，不重新清 BSS 或解析全局资源。
#[no_mangle]
pub extern "C" fn secondary_main(local: *const cpu::CpuLocal) -> ! {
    unsafe { arch::install_cpu_local(local) };
    arch::init_current_cpu();
    cpu::publish_online();
    arch::enable_interrupts();
    #[cfg(feature = "boot-tests")]
    boot_tests::secondary();
    #[cfg(not(feature = "boot-tests"))]
    cpu::idle();
}

#[panic_handler]
fn panic(info: &core::panic::PanicInfo<'_>) -> ! {
    console::panic(info)
}
