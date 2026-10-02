//! 固定 Sv39/QEMU virt 后端；SBI 只使用 TIME、IPI、HSM、SRST。
use core::{arch::{asm, global_asm}, mem::{offset_of, size_of}, ptr};
use crate::{boot::BootInfo, cpu::{CpuBoot, CpuLocal}};

/// 当前唯一编译目标的架构名称。
pub const NAME: &str = "riscv64";
extern "C" {
    static __kernel_phys_start: u8;
    static __kernel_phys_end: u8;
    static __kernel_virt_start: u8;
    static __direct_base: u8;
    static __mmio_base: u8;
    static __phys_ram_base: u8;
    static __secondary_entry_phys: u8;
    fn rv_trap_entry();
}
// medany 的 PC 相对取址不能直接到达物理/直映绝对符号；从本地 literal 读取地址值。
macro_rules! address {
    ($name:ident) => {{
        let value: usize;
        unsafe {
            asm!("lla {value}, 2f", "ld {value}, 0({value})", "j 3f",
                 ".balign 8", "2: .quad {symbol}", "3:",
                 value = out(reg) value, symbol = sym $name,
                 options(nostack, readonly));
        }
        value
    }};
}
#[repr(C, align(4096))]
struct PageTable([usize; 512]);
#[no_mangle]
#[link_section = ".boot.tables"]
static mut RV_BOOT_ROOT: PageTable = PageTable([0; 512]);
#[no_mangle]
#[link_section = ".boot.tables"]
static mut RV_BOOT_IMAGE: PageTable = PageTable([0; 512]);
#[link_section = ".boot.tables"]
static mut FINAL_TABLES: [PageTable; 64] = [const { PageTable([0; 512]) }; 64];
#[no_mangle]
static mut RV_FINAL_SATP: usize = 0;

#[repr(C, align(16))]
struct KernelTrapFrame {
    gpr: [usize; 32],
    sepc: usize,
    sstatus: usize,
    scause: usize,
    stval: usize,
    fpr: [u64; 32],
    fcsr: usize,
}
global_asm!(include_str!("entry.S"),
    boot_stack = const offset_of!(CpuBoot, stack_top),
    boot_local = const offset_of!(CpuBoot, local),
    stack_size = const crate::cpu::STACK_SIZE);
global_asm!(include_str!("trap.S"),
    frame_size = const size_of::<KernelTrapFrame>(),
    gpr = const offset_of!(KernelTrapFrame, gpr),
    word = const size_of::<usize>(),
    sepc = const offset_of!(KernelTrapFrame, sepc),
    sstatus = const offset_of!(KernelTrapFrame, sstatus),
    scause = const offset_of!(KernelTrapFrame, scause),
    stval = const offset_of!(KernelTrapFrame, stval),
    fpr = const offset_of!(KernelTrapFrame, fpr),
    float_word = const size_of::<u64>(),
    fcsr = const offset_of!(KernelTrapFrame, fcsr));

/// 从固件原始参数取得真实 BSP hart ID。
pub fn boot_hardware_id(args: [usize; 3]) -> usize { args[0] }
/// 将 RAM 物理地址转换成直映地址。
pub fn phys_to_virt(pa: usize) -> usize { address!(__direct_base).wrapping_add(pa.wrapping_sub(address!(__phys_ram_base))) }
/// 转换内核镜像或 RAM 直映地址为物理地址。
pub fn virt_to_phys(va: usize) -> usize {
    if va >= address!(__kernel_virt_start) {
        va - address!(__kernel_virt_start) + address!(__kernel_phys_start)
    } else { va.wrapping_sub(address!(__direct_base)).wrapping_add(address!(__phys_ram_base)) }
}
/// 取得已映射的设备窗口地址。
pub fn mmio_address(pa: usize) -> usize { address!(__mmio_base).wrapping_add(pa) }

unsafe fn table(index: usize) -> *mut usize { ptr::addr_of_mut!(FINAL_TABLES).cast::<PageTable>().add(index).cast() }
unsafe fn map_range(mut va: usize, mut pa: usize, end: usize, flags: usize, next: &mut usize) {
    while pa < end {
        let huge = (va | pa) & 0x1fffff == 0 && end - pa >= 0x200000;
        let target = if huge { 1 } else { 0 };
        let mut current = table(0);
        for level in (target + 1..=2).rev() {
            let slot = current.add((va >> (12 + 9 * level)) & 511);
            let mut pte = ptr::read(slot);
            if pte == 0 {
                assert!(*next < 64, "Sv39 启动页表容量不足");
                let child = table(*next);
                *next += 1;
                pte = (virt_to_phys(child as usize) >> 2) | 1;
                ptr::write(slot, pte);
            }
            assert_eq!(pte & 0xe, 0, "Sv39 映射层级重叠");
            current = phys_to_virt((pte >> 10) << 12) as *mut usize;
        }
        ptr::write(current.add((va >> (12 + 9 * target)) & 511), (pa >> 2) | flags | 1);
        let step = if huge { 0x200000 } else { 4096 };
        pa += step; va += step;
    }
}
/// 建立实际 RAM/设备的最终表；保留只读 AP bootstrap 表而删除最终低别名。
pub fn finish_boot_mappings(info: &BootInfo) {
    unsafe {
        ptr::write_bytes(ptr::addr_of_mut!(FINAL_TABLES).cast::<u8>(), 0, size_of::<[PageTable; 64]>());
        let mut next = 1;
        let start = address!(__kernel_phys_start);
        let end = (address!(__kernel_phys_end) + 4095) & !4095;
        map_range(address!(__kernel_virt_start), start, end, 0xce, &mut next);
        for region in &info.memory[..info.memory_count] {
            assert_eq!(region.start & 4095, 0, "RAM 起点未页对齐");
            assert_eq!(region.end & 4095, 0, "RAM 终点未页对齐");
            map_range(phys_to_virt(region.start), region.start, region.end, 0xc6, &mut next);
        }
        for region in [info.interrupts.uart, info.interrupts.controller] {
            let start = region.start & !4095;
            map_range(mmio_address(start), start, (region.end + 4095) & !4095, 0xc6, &mut next);
        }
        RV_FINAL_SATP = (8usize << 60) | (virt_to_phys(table(0) as usize) >> 12);
        asm!("fence rw, rw", "csrw satp, {0}", "sfence.vma", in(reg) RV_FINAL_SATP, options(nostack));
    }
}
/// 安装本核唯一 CpuLocal 基址；调用者保证地址永久有效。
pub unsafe fn install_cpu_local(local: *const CpuLocal) { asm!("mv tp, {0}", in(reg) local, options(nostack)); }
/// 读取本核 CpuLocal 基址。
pub fn cpu_local() -> *const CpuLocal { let value; unsafe { asm!("mv {0}, tp", out(reg) value, options(nomem, nostack)); } value }
/// 关闭本核旧事件并安装可恢复 kernel trap。
pub fn init_current_cpu() {
    unsafe { asm!("csrw stvec, {0}", "csrw sscratch, zero", "csrci sip, 2", "li t0, 0x6000", "csrc sstatus, t0", "li t0, 0x222", "csrw sie, t0", in(reg) rv_trap_entry as usize, out("t0") _, options(nostack)); }
    disable_timer();
}
/// 保存并关闭本地中断。
pub fn irq_save() -> usize { let saved; unsafe { asm!("csrrci {0}, sstatus, 2", out(reg) saved, options(nomem, nostack)); } saved }
/// 恢复保存的本地中断使能位。
pub unsafe fn irq_restore(saved: usize) { if saved & 2 != 0 { asm!("csrsi sstatus, 2", options(nomem, nostack)); } else { asm!("csrci sstatus, 2", options(nomem, nostack)); } }
/// 查询本地中断是否使能。
pub fn irq_enabled() -> bool { let s: usize; unsafe { asm!("csrr {0}, sstatus", out(reg) s, options(nomem, nostack)); } s & 2 != 0 }
/// 开启本地中断。
pub fn enable_interrupts() { unsafe { asm!("csrsi sstatus, 2", options(nomem, nostack)); } }
/// 等待本地硬件事件。
pub fn wait_for_interrupt() { unsafe { asm!("wfi", options(nomem, nostack)); } }
/// 读取真实硬件时间计数器。
pub fn read_counter() -> u64 { let value; unsafe { asm!("rdtime {0}", out(reg) value, options(nomem, nostack)); } value }
/// 返回启动时解析的 timebase-frequency。
pub fn counter_frequency() -> u64 { crate::boot::info().counter_frequency }
fn sbi(extension: usize, function: usize, mut a0: usize, mut a1: usize, a2: usize) -> Result<usize, isize> {
    unsafe { asm!("ecall", inlateout("a0") a0, inlateout("a1") a1, in("a2") a2, in("a3") 0usize, in("a4") 0usize, in("a5") 0usize, in("a6") function, in("a7") extension, options(nostack)); }
    if a0 == 0 { Ok(a1) } else { Err(a0 as isize) }
}
/// 用 SBI TIME 编程一次绝对期限；固件错误直接报告。
pub fn program_timer(deadline: u64) { sbi(0x54494d45, 0, deadline as usize, 0, 0).expect("SBI TIME 失败"); }
/// 关闭本核 timer pending 和下一次事件。
pub fn disable_timer() { program_timer(u64::MAX); }
/// 通过 HSM 启动真实物理 AP 入口并传递静态启动记录。
pub fn start_cpu(hardware_id: usize, boot_record_physical: usize) -> Result<(), isize> {
    unsafe { asm!("fence rw, rw", options(nostack)); }
    sbi(0x48534d, 0, hardware_id, address!(__secondary_entry_phys), boot_record_physical).map(|_| ())
}
/// 发布软件队列后通知指定 hart；错误不丢弃。
pub fn send_ipi(hardware_id: usize) {
    unsafe { asm!("fence rw, rw", options(nostack)); }
    sbi(0x735049, 0, 1, hardware_id, 0).expect("SBI IPI 失败");
}
static PLIC_ENABLE_LOCK: crate::sync::SpinLock<()> = crate::sync::SpinLock::new(());
unsafe fn mmio_read(offset: usize) -> u32 { ptr::read_volatile((mmio_address(crate::boot::info().interrupts.controller.start) + offset) as *const u32) }
unsafe fn mmio_write(offset: usize, value: u32) { ptr::write_volatile((mmio_address(crate::boot::info().interrupts.controller.start) + offset) as *mut u32, value); asm!("fence iorw, iorw", options(nostack)); }
fn claim_offset() -> usize { 0x200004 + crate::boot::info().interrupts.plic_contexts[crate::cpu::current().id] * 0x1000 }
fn enable_offset() -> usize { let irq = &crate::boot::info().interrupts; 0x2000 + irq.plic_contexts[0] * 0x80 + (irq.uart_source as usize / 32) * 4 }
/// 初始化 FDT 指定的所有 S context，UART 仅路由到 BSP。
pub fn external_init(info: &BootInfo) {
    unsafe {
        for &context in &info.interrupts.plic_contexts[..info.cpu_count] {
            for word in 0..32 { mmio_write(0x2000 + context * 0x80 + word * 4, 0); }
            mmio_write(0x200000 + context * 0x1000, 0);
        }
        mmio_write(info.interrupts.uart_source as usize * 4, 1);
    }
}
/// 硬件屏蔽 BSP UART 源。
pub fn external_mask() {
    let _guard = PLIC_ENABLE_LOCK.lock();
    unsafe { mmio_write(enable_offset(), mmio_read(enable_offset()) & !(1 << (crate::boot::info().interrupts.uart_source % 32))); }
}
/// 硬件解除 BSP UART 源屏蔽。
pub fn external_unmask() {
    let _guard = PLIC_ENABLE_LOCK.lock();
    unsafe { mmio_write(enable_offset(), mmio_read(enable_offset()) | (1 << (crate::boot::info().interrupts.uart_source % 32))); }
}
/// 从本核真实 PLIC context claim；零表示无待处理源。
pub fn external_claim() -> Option<u32> { let source = unsafe { mmio_read(claim_offset()) }; if source == 0 { None } else { Some(source) } }
/// 完成本核已 claim 的硬件源。
pub fn external_complete(source: u32) {
    let _guard = PLIC_ENABLE_LOCK.lock();
    let context = crate::boot::info().interrupts.plic_contexts[crate::cpu::current().id];
    let offset = 0x2000 + context * 0x80 + source as usize / 32 * 4;
    let bit = 1 << (source % 32);
    unsafe {
        let enabled = mmio_read(offset);
        // PLIC 对 disabled 源的 complete 可能不接受；临时恢复后保持原屏蔽状态。
        if enabled & bit == 0 { mmio_write(offset, enabled | bit); }
        mmio_write(claim_offset(), source);
        if enabled & bit == 0 { mmio_write(offset, enabled); }
    }
}
/// 请求 SBI SRST 关机，失败或异常返回均报告。
pub fn shutdown() -> ! { sbi(0x53525354, 0, 0, 0, 0).expect("SBI SRST 失败"); panic!("SBI SRST 意外返回"); }
/// 触发真实非法指令异常供现场诊断。
pub fn trigger_fault() -> ! { unsafe { asm!(".word 0", options(noreturn)); } }
#[no_mangle]
extern "C" fn rv_handle_trap(frame: &mut KernelTrapFrame) {
    match frame.scause {
        value if value == (1usize << 63) | 5 => { disable_timer(); crate::cpu::handle_timer(); }
        value if value == (1usize << 63) | 1 => { unsafe { asm!("csrci sip, 2", "fence rw, rw", options(nostack)); } crate::cpu::handle_ipi(); }
        value if value == (1usize << 63) | 9 => crate::irq::handle_external(),
        _ => crate::console::fatal(frame.sepc, frame.gpr[2], frame.scause, frame.stval),
    }
}

#[cfg(feature = "boot-tests")]
global_asm!(include_str!("registers.S"));
#[cfg(feature = "boot-tests")]
extern "C" { fn rv_register_roundtrip(counter: *const core::sync::atomic::AtomicUsize, expected: usize, fp: usize) -> usize; }
/// 真实 timer 异常验证 GPR/SP/TP 以及启用/关闭 FP 两种现场。
#[cfg(feature = "boot-tests")]
pub fn test_register_preservation() -> bool {
    use core::sync::atomic::Ordering;
    let saved = irq_save();
    let mut success = true;
    for fp in [0, 1] {
        let counter = &crate::cpu::current().timer_interrupts;
        let expected = counter.load(Ordering::Acquire) + 1;
        program_timer(read_counter() + counter_frequency() / 100);
        success &= unsafe { rv_register_roundtrip(counter, expected, fp) } != 0;
        success &= counter.load(Ordering::Acquire) == expected;
    }
    unsafe { irq_restore(saved); }
    success
}
