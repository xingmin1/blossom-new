//! QEMU 9.2.1 virt 的 DMW、异常、一次定时器及 PCH-PIC/EXTIOI 机制。
use core::arch::{asm, global_asm};
use core::mem::{offset_of, size_of};
use crate::boot::BootInfo;

pub const NAME: &str = "loongarch64";

#[repr(C, align(16))]
struct KernelTrapFrame {
    gpr: [usize; 32],
    era: usize,
    prmd: usize,
    estat: usize,
    badv: usize,
    euen: usize,
    cpu_base: usize,
    fpr: [u64; 32],
    fcsr: usize,
    fcc: [usize; 8],
}

// refill 有独立 CSR 现场，不借用普通异常的 ERA/PRMD/BADV。
#[repr(C, align(16))]
struct RefillFrame {
    gpr: [usize; 32],
    era: usize,
    prmd: usize,
    badv: usize,
}

global_asm!(include_str!("entry.S"),
    stack_size = const crate::cpu::STACK_SIZE,
    boot_stack = const offset_of!(crate::cpu::CpuBoot, stack_top),
    boot_local = const offset_of!(crate::cpu::CpuBoot, local),
);
global_asm!(include_str!("trap.S"),
    frame_size = const size_of::<KernelTrapFrame>(),
    gpr = const offset_of!(KernelTrapFrame, gpr),
    era = const offset_of!(KernelTrapFrame, era),
    prmd = const offset_of!(KernelTrapFrame, prmd),
    estat = const offset_of!(KernelTrapFrame, estat),
    badv = const offset_of!(KernelTrapFrame, badv),
    euen = const offset_of!(KernelTrapFrame, euen),
    cpu_base = const offset_of!(KernelTrapFrame, cpu_base),
    fpr = const offset_of!(KernelTrapFrame, fpr),
    fcsr = const offset_of!(KernelTrapFrame, fcsr),
    fcc = const offset_of!(KernelTrapFrame, fcc),
    refill_size = const size_of::<RefillFrame>(),
    refill_gpr = const offset_of!(RefillFrame, gpr),
    refill_era = const offset_of!(RefillFrame, era),
    refill_prmd = const offset_of!(RefillFrame, prmd),
    refill_badv = const offset_of!(RefillFrame, badv),
);

extern "C" {
    static __direct_base: u8;
    static __mmio_base: u8;
    static __secondary_entry_phys: u8;
    fn __kernel_trap();
    fn __kernel_refill();
}

#[inline]
fn csr_read<const R: u16>() -> usize {
    let value;
    unsafe { asm!("csrrd {value}, {reg}", value = out(reg) value, reg = const R, options(nomem, nostack)); }
    value
}
#[inline]
unsafe fn csr_write<const R: u16>(value: usize) {
    asm!("csrwr {value}, {reg}", value = inout(reg) value => _, reg = const R, options(nostack));
}
#[inline]
fn iocsr_read32(reg: usize) -> u32 {
    let value: usize;
    unsafe { asm!("iocsrrd.w {value}, {reg}", value = out(reg) value, reg = in(reg) reg, options(nostack)); }
    value as u32
}
#[inline]
fn iocsr_read64(reg: usize) -> u64 {
    let value;
    unsafe { asm!("iocsrrd.d {value}, {reg}", value = out(reg) value, reg = in(reg) reg, options(nostack)); }
    value
}
#[inline]
fn iocsr_write32(reg: usize, value: u32) {
    unsafe { asm!("iocsrwr.w {value}, {reg}", value = in(reg) value as usize, reg = in(reg) reg, options(nostack)); }
}
#[inline]
fn iocsr_write64(reg: usize, value: u64) {
    unsafe { asm!("iocsrwr.d {value}, {reg}", value = in(reg) value, reg = in(reg) reg, options(nostack)); }
}
fn barrier() { unsafe { asm!("dbar 0", options(nostack)); } }

/// 从 CPUId CSR 取得当前硬件身份，不解释 QEMU 的引导参数。
pub fn boot_hardware_id(_: [usize; 3]) -> usize { csr_read::<0x20>() & 0x3ff }
/// 将真实 RAM 物理地址转换为缓存 DMW 别名。
pub fn phys_to_virt(pa: usize) -> usize {
    let base: usize;
    unsafe { asm!("la.abs {base}, {symbol}", base = out(reg) base, symbol = sym __direct_base, options(nomem, nostack)); }
    pa | base
}
/// 去掉内核缓存 DMW 段，取得实际物理地址。
pub fn virt_to_phys(va: usize) -> usize { va & !phys_to_virt(0) }
/// 将设备物理地址转换为强序 DMW 地址。
pub fn mmio_address(pa: usize) -> usize {
    let base: usize;
    unsafe { asm!("la.abs {base}, {symbol}", base = out(reg) base, symbol = sym __mmio_base, options(nomem, nostack)); }
    pa | base
}
/// 确认入口已安装最终 DMW；普通内核不再使用 DA 低地址别名。
pub fn finish_boot_mappings(_: &BootInfo) {
    assert_eq!(csr_read::<0>() & 0x18, 0x10);
    assert_eq!(csr_read::<0x181>(), phys_to_virt(0) | 0x11);
    assert_eq!(csr_read::<0x180>(), mmio_address(0) | 1);
}
/// 将稳定的内核 per-CPU 指针放入 KS0；用户 TLS 寄存器不参与此约定。
pub unsafe fn install_cpu_local(local: *const crate::cpu::CpuLocal) { csr_write::<0x30>(local as usize); }
/// 读取当前 CPU 的 KS0 基址。
pub fn cpu_local() -> *const crate::cpu::CpuLocal { csr_read::<0x30>() as *const _ }
/// 安装本核异常入口，并只初始化本核 timer/IPI 状态。
pub fn init_current_cpu() {
    irq_save();
    unsafe {
        csr_write::<0xc>(__kernel_trap as usize);
        csr_write::<0x88>(virt_to_phys(__kernel_refill as usize));
        csr_write::<4>((1 << 11) | (1 << 12) | (1 << 3));
        csr_write::<2>(1);
    }
    disable_timer();
    iocsr_write32(0x100c, u32::MAX);
    iocsr_write32(0x1004, u32::MAX);
    barrier();
}
/// 原子关闭本核 IRQ，返回原 CRMD 状态。
pub fn irq_save() -> usize {
    let old;
    unsafe { asm!("csrxchg {old}, {mask}, 0", old = inout(reg) 0usize => old, mask = in(reg) 4usize, options(nostack)); }
    old
}
/// 只恢复原来的 IE 位，不修改分页或特权状态。
pub unsafe fn irq_restore(saved: usize) {
    asm!("csrxchg {value}, {mask}, 0", value = inout(reg) (saved & 4) => _, mask = in(reg) 4usize, options(nostack));
}
/// 查询本核全局 IRQ 开关。
pub fn irq_enabled() -> bool { csr_read::<0>() & 4 != 0 }
/// 打开本核全局 IRQ。
pub fn enable_interrupts() { unsafe { irq_restore(4); } }
/// 等待真实硬件中断。
pub fn wait_for_interrupt() { unsafe { asm!("idle 0", options(nostack)); } }
/// 读取稳定硬件计数器。
pub fn read_counter() -> u64 {
    let value;
    unsafe { asm!("rdtime.d {value}, $zero", value = out(reg) value, options(nomem, nostack)); }
    value
}
fn cpucfg(index: usize) -> u64 {
    let value: usize;
    unsafe { asm!("cpucfg {value}, {index}", value = out(reg) value, index = in(reg) index, options(nomem, nostack)); }
    value as u64 & 0xffff_ffff
}
/// 按 CPUCFG4/5 的基频、乘数、除数计算实际 counter 频率。
pub fn counter_frequency() -> u64 {
    let base = cpucfg(4);
    let ratio = cpucfg(5);
    let mul = ratio & 0xffff;
    let div = ratio >> 16;
    assert!(base != 0 && mul != 0 && div != 0, "CPUCFG counter frequency 无效");
    base * mul / div
}
/// 将绝对 deadline 向上量化到四个 tick，并编程一次事件。
pub fn program_timer(deadline: u64) {
    disable_timer();
    let delta = deadline.saturating_sub(read_counter()).max(4);
    let value = delta.saturating_add(3).min((1u64 << 48) - 4) & !3;
    unsafe { csr_write::<0x41>(value as usize | 1); }
}
/// 关闭一次事件并清除真实 timer pending。
pub fn disable_timer() {
    unsafe { csr_write::<0x41>(0); csr_write::<0x44>(1); }
}
fn mail_send(cpu: usize, mailbox: usize, data: usize) {
    let target = (1u64 << 31) | ((cpu as u64) << 16);
    iocsr_write64(0x1048, (data as u64 & 0xffff_ffff_0000_0000) | target | (((mailbox * 2 + 1) as u64) << 2));
    iocsr_write64(0x1048, ((data as u64) << 32) | target | (((mailbox * 2) as u64) << 2));
}
/// 通过真实 mailbox1 发表启动记录，mailbox0 发表物理入口，再发送启动 IPI。
pub fn start_cpu(hardware_id: usize, boot_record_physical: usize) -> Result<(), isize> {
    if hardware_id >= crate::boot::MAX_CPUS { return Err(-22); }
    barrier();
    mail_send(hardware_id, 1, boot_record_physical);
    let entry: usize;
    unsafe { asm!("la.abs {entry}, {symbol}", entry = out(reg) entry, symbol = sym __secondary_entry_phys, options(nomem, nostack)); }
    mail_send(hardware_id, 0, entry);
    barrier();
    iocsr_write32(0x1040, (1 << 31) | ((hardware_id as u32) << 16));
    Ok(())
}
/// 队列发表屏障后发送 bit1 的真实 IOCSR IPI。
pub fn send_ipi(hardware_id: usize) {
    barrier();
    iocsr_write32(0x1040, (1 << 31) | ((hardware_id as u32) << 16) | 1);
}
fn pic_write(offset: usize, value: u32) {
    unsafe { core::ptr::write_volatile((mmio_address(crate::boot::info().interrupts.controller.start) + offset) as *mut u32, value); }
}
fn pic_read(offset: usize) -> u32 {
    unsafe { core::ptr::read_volatile((mmio_address(crate::boot::info().interrupts.controller.start) + offset) as *const u32) }
}
fn ext_reg(offset: usize) -> usize { crate::boot::info().interrupts.parent_controller.start + offset }
fn uart_bit() -> u32 { 1 << crate::boot::info().interrupts.uart_source }
/// 将已选 UART 的 PCH 源向量路由到 BSP 的 EXTIOI/IP1，初始保持屏蔽。
pub fn external_init(info: &BootInfo) {
    let source = info.interrupts.uart_source as usize;
    assert!(source < 32 && info.bsp_hardware_id < 4);
    assert_eq!(info.interrupts.root_interrupt, 3);
    pic_write(0x20, u32::MAX);
    pic_write(0x24, u32::MAX);
    pic_write(0x60, 0);
    pic_write(0x64, 0);
    pic_write(0x3e0, 0);
    pic_write(0x3e4, 0);
    pic_write(0x40, uart_bit());
    unsafe {
        let base = mmio_address(info.interrupts.controller.start);
        core::ptr::write_volatile((base + 0x100 + source) as *mut u8, 1);
        core::ptr::write_volatile((base + 0x200 + source) as *mut u8, source as u8);
    }
    // QEMU EXTIOI 使用绝对 IOCSR 0x14c0/0x1600/0x1800/0x1c00，reg 基址是 0x1400。
    for group in 0..8 { iocsr_write32(ext_reg(0x200 + group * 4), 0); }
    iocsr_write32(ext_reg(0xc0), 0x0202_0202);
    iocsr_write32(ext_reg(0xa0), 0x0002_0001);
    let route = 1u32 << info.bsp_hardware_id;
    iocsr_write32(ext_reg(0x800 + (source & !3)), route * 0x0101_0101);
    iocsr_write32(ext_reg(0x280), uart_bit());
    iocsr_write32(ext_reg(0x400), uart_bit());
    iocsr_write64(0x420, iocsr_read64(0x420) | (1u64 << 48));
    barrier();
}
/// 同时屏蔽 PCH 源与 EXTIOI 向量；通用 IRQ 锁负责串行化此读改写。
pub fn external_mask() {
    pic_write(0x20, pic_read(0x20) | uart_bit());
    iocsr_write32(ext_reg(0x200), 0);
    barrier();
}
/// 先使能 EXTIOI，再解除 PCH 源屏蔽，让仍存在的 level 请求重发。
pub fn external_unmask() {
    iocsr_write32(ext_reg(0x200), uart_bit());
    pic_write(0x20, pic_read(0x20) & !uart_bit());
    barrier();
}
/// 从本核 EXTIOI pending 与 PCH 真实状态共同 claim UART。
pub fn external_claim() -> Option<u32> {
    if iocsr_read32(ext_reg(0x400)) & pic_read(0x3a0) & uart_bit() != 0 {
        Some(crate::boot::info().interrupts.uart_source)
    } else { None }
}
/// UART 设备已消除 level 请求后，清除 EXTIOI 的真实 W1C pending。
pub fn external_complete(source: u32) {
    assert_eq!(source, crate::boot::info().interrupts.uart_source);
    iocsr_write32(ext_reg(0x400), 1 << source);
    barrier();
}
/// 写入所选 virt FDT 的 GED syscon poweroff 字节。
pub fn shutdown() -> ! {
    irq_save();
    unsafe { core::ptr::write_volatile(mmio_address(0x100e001c) as *mut u8, 0x34); }
    barrier();
    loop { wait_for_interrupt(); }
}
/// 触发明确的非法指令，保留真实 kernel fault 现场。
pub fn trigger_fault() -> ! { unsafe { asm!(".word 0xffffffff", options(noreturn)); } }

#[no_mangle]
extern "C" fn loongarch_trap(frame: &KernelTrapFrame) {
    if (frame.estat >> 16) & 0x3f != 0 {
        crate::console::fatal(frame.era, frame.gpr[3], frame.estat, frame.badv);
    }
    let pending = frame.estat & csr_read::<4>() & 0x1fff;
    if pending & (1 << 11) != 0 { disable_timer(); crate::cpu::handle_timer(); }
    if pending & (1 << 12) != 0 {
        let bits = iocsr_read32(0x1000);
        iocsr_write32(0x100c, bits);
        barrier();
        crate::cpu::handle_ipi();
    }
    if pending & (1 << 3) != 0 { crate::irq::handle_external(); }
}
#[no_mangle]
extern "C" fn loongarch_refill(frame: &RefillFrame) -> ! {
    // refill 不产生普通 ESTAT.Ecode；cause 记录 TLBRERA.IsTLBR 的真实值。
    crate::console::fatal(frame.era & !1, frame.gpr[3], frame.era & 1, frame.badv)
}

#[cfg(feature = "boot-tests")]
global_asm!(include_str!("register_test.S"));
#[cfg(feature = "boot-tests")]
extern "C" { fn __test_registers(counter: *const core::sync::atomic::AtomicUsize, ticks: usize, fcc: usize, fp: usize) -> usize; }
/// 以真实 timer 返回检查寄存器、FP/FCSR、全部 FCC 图样及 FP 关闭路径。
#[cfg(feature = "boot-tests")]
pub fn test_register_preservation() -> bool {
    let counter = &crate::cpu::current().timer_interrupts;
    let saved = irq_save();
    let ticks = ((counter_frequency() / 10000).max(0x600) as usize + 3) & !3;
    let mut good = true;
    for fcc in 0..256 {
        if unsafe { __test_registers(counter, ticks, fcc, 1) } == 0 { good = false; break; }
    }
    if unsafe { __test_registers(counter, ticks, 0, 0) } == 0 { good = false; }
    unsafe { irq_restore(saved); }
    good
}
