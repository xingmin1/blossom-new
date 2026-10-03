//! 固定 Sv39/QEMU virt 后端；SBI 只使用 TIME、IPI、HSM、SRST。
use crate::{
    boot::BootInfo,
    cpu::{CpuBoot, CpuLocal},
    sched::context::TaskContext,
};
use core::{
    arch::{asm, global_asm},
    mem::{offset_of, size_of, MaybeUninit},
    ptr,
};

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
        // SAFETY: literal 与指令同处内核只读映射，链接器填入符号值，汇编只读取该对齐 literal。
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
    // FS 为 Off 时汇编不写入这些字段；Rust 不读取未保存的浮点现场。
    fpr: MaybeUninit<[u64; 32]>,
    fcsr: MaybeUninit<usize>,
}
global_asm!(include_str!("riscv64/entry.S"),
    boot_stack = const offset_of!(CpuBoot, stack_top),
    boot_local = const offset_of!(CpuBoot, local),
    stack_size = const crate::cpu::STACK_SIZE);
global_asm!(include_str!("riscv64/trap.S"),
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
global_asm!(include_str!("riscv64/switch.S"),
    context_size = const size_of::<TaskContext>(),
    context_ra = const offset_of!(TaskContext, ra),
    context_sp = const offset_of!(TaskContext, sp),
    context_saved = const offset_of!(TaskContext, saved),
    word = const size_of::<usize>(),
    execution_tests = const cfg!(feature = "execution-tests") as usize,
    yield_now = sym crate::sched::yield_now);

extern "C" {
    /// 保存当前普通调用现场并返回到下一执行者的调用现场，不改变目的 CPU 的 tp。
    ///
    /// # Safety
    /// 调用者必须独占有效且不重叠的上下文，IRQ 关闭且 FS 为 Off；
    /// next 的栈、返回地址与 callee-saved 必须组成有效普通调用现场。
    fn rv_switch_context(prev: *mut TaskContext, next: *const TaskContext);
}

/// 在 IRQ 关闭的调度交接段切换整数调用现场，恢复后使用目的 CPU 的 CpuLocal。
///
/// # Safety
/// 当前执行拥有者必须独占两个有效、对齐且不重叠的上下文；prev 可写，next 已初始化。
/// next 的栈与返回地址必须有效且按 16 字节对齐，两个执行栈均须存活到各自恢复或退出。
/// 调用者必须已关闭 IRQ，不能持普通 IRQ/锁/preempt guard 跨此调用；
/// 普通内核 FS 必须为 Off，受控浮点测试不得调用此接口。
pub unsafe fn switch_context(prev: *mut TaskContext, next: *const TaskContext) {
    assert!(!irq_enabled(), "task switch requires IRQ disabled");
    let status: usize;
    // SAFETY: S-mode 可读取本核 sstatus；只检查普通内核不持有浮点现场的调用约束。
    unsafe { asm!("csrr {0}, sstatus", out(reg) status, options(nomem, nostack)) };
    assert_eq!(status & 0x6000, 0, "task switch requires FS Off");
    // SAFETY: 调用者保证上下文独占、栈/返回地址有效和交接生命周期；上面已检查 IRQ 与 FS。
    unsafe { rv_switch_context(prev, next) };
}

#[cfg(feature = "execution-tests")]
extern "C" {
    /// 按普通调用 ABI 保存调用者寄存器，跨真实 yield 校验整数现场并恢复调用者。
    ///
    /// # Safety
    /// 必须由能合法 yield 的普通 Task 调用，FS 为 Off，栈满足 16 字节 ABI 对齐。
    fn rv_task_context_roundtrip() -> usize;
}

/// 在可调度的普通 Task 中跨真实 yield 检验全部 callee-saved、SP 与返回地址。
#[cfg(feature = "execution-tests")]
pub fn test_task_context_preservation() -> bool {
    // SAFETY: Rust 入口提供对齐普通调用栈；生产 yield 统一检查 Task、IRQ 和原子上下文，
    // switch_context 检查 FS Off；汇编恢复调用者 callee-saved，并始终保留目的 CPU 的 tp。
    unsafe { rv_task_context_roundtrip() != 0 }
}

/// 从固件原始参数取得真实 BSP hart ID。
pub fn boot_hardware_id(args: [usize; 3]) -> usize {
    args[0]
}
/// 将实际 RAM 物理地址转换成直映地址；只计算地址，不证明该地址已经映射。
pub fn phys_to_virt(pa: usize) -> usize {
    address!(__direct_base).wrapping_add(pa.wrapping_sub(address!(__phys_ram_base)))
}
/// 转换内核镜像或 RAM 直映地址为物理地址。
pub fn virt_to_phys(va: usize) -> usize {
    if address!(__kernel_virt_start) <= va {
        va - address!(__kernel_virt_start) + address!(__kernel_phys_start)
    } else {
        va.wrapping_sub(address!(__direct_base))
            .wrapping_add(address!(__phys_ram_base))
    }
}
/// 计算物理设备地址在 MMIO 窗口中的位置；解引用者必须保证设备范围已映射。
pub fn mmio_address(pa: usize) -> usize {
    address!(__mmio_base).wrapping_add(pa)
}

/// 取得最终页表池中的一张表，不创建可别名的 Rust 引用。
///
/// # Safety
/// `index` 必须小于 64；调用者必须独占尚未激活的最终表池。
unsafe fn table(index: usize) -> *mut usize {
    // SAFETY: 调用者保证索引落在完整的 64 张静态 PageTable 内。
    unsafe {
        ptr::addr_of_mut!(FINAL_TABLES)
            .cast::<PageTable>()
            .add(index)
            .cast()
    }
}
/// 将页对齐物理区间映射到连续 Sv39 虚拟地址，按需消耗最终表池。
///
/// # Safety
/// BSP 必须独占未激活的表池；va/pa/end 必须页对齐、无地址溢出且位于实际映射窗口。
/// va 必须为 Sv39 canonical 地址；已有非叶 PTE 必须仅指向该池，
/// 映射不得覆盖既有叶项；next 必须是 1..=64 中首张未使用表的索引。
unsafe fn map_range(mut va: usize, mut pa: usize, end: usize, flags: usize, next: &mut usize) {
    // SAFETY: 调用者保证表池独占、地址范围有效；槽索引被限制到 512 项内，
    // 新表的容量在取址前检查，非叶项只由本函数写入池中表的物理地址。
    unsafe {
        while pa < end {
            let huge = (va | pa) & 0x1fffff == 0 && 0x200000 <= end - pa;
            let target = if huge { 1 } else { 0 };
            let mut current = table(0);
            for level in (target + 1..=2).rev() {
                let slot = current.add((va >> (12 + 9 * level)) & 511);
                let mut pte = ptr::read(slot);
                if pte == 0 {
                    assert!(*next < 64, "Sv39 boot page table pool exhausted");
                    let child = table(*next);
                    *next += 1;
                    pte = (virt_to_phys(child as usize) >> 2) | 1;
                    ptr::write(slot, pte);
                }
                assert_eq!(pte & 0xe, 0, "Sv39 mapping levels overlap");
                current = phys_to_virt((pte >> 10) << 12) as *mut usize;
            }
            ptr::write(
                current.add((va >> (12 + 9 * target)) & 511),
                (pa >> 2) | flags | 1,
            );
            let step = if huge { 0x200000 } else { 4096 };
            pa += step;
            va += step;
        }
    }
}
/// 建立实际 RAM/设备的最终表；保留只读 AP bootstrap 表而删除最终低别名。
///
/// # Safety
/// 只能由关闭 IRQ 的 BSP 在使用 bootstrap 表且 AP 尚未释放时调用一次。
/// info 必须描述实际 QEMU RAM/设备，区间算术不得溢出、映射窗口不得重叠；
/// RAM 必须覆盖当前栈、CpuLocal、页表池及全部内核运行资源。
pub unsafe fn finish_boot_mappings(info: &BootInfo) {
    let start = address!(__kernel_phys_start);
    let end = (address!(__kernel_phys_end) + 4095) & !4095;
    let kernel_virtual = address!(__kernel_virt_start);
    // SAFETY: 调用者保证 BSP 独占未激活的表池和真实映射范围；内核与直映覆盖
    // 切表后的取指、栈及表遍历，fence/sfence 在发布 SATP 前后同步表项与 TLB。
    unsafe {
        ptr::write_bytes(
            ptr::addr_of_mut!(FINAL_TABLES).cast::<u8>(),
            0,
            size_of::<[PageTable; 64]>(),
        );
        let mut next = 1;
        map_range(kernel_virtual, start, end, 0xce, &mut next);
        for region in &info.memory[..info.memory_count] {
            assert_eq!(region.start & 4095, 0, "RAM start is not page aligned");
            assert_eq!(region.end & 4095, 0, "RAM end is not page aligned");
            map_range(
                phys_to_virt(region.start),
                region.start,
                region.end,
                0xc6,
                &mut next,
            );
        }
        for region in [info.interrupts.uart, info.interrupts.controller] {
            let start = region.start & !4095;
            map_range(
                mmio_address(start),
                start,
                (region.end + 4095) & !4095,
                0xc6,
                &mut next,
            );
        }
        RV_FINAL_SATP = (8usize << 60) | (virt_to_phys(table(0) as usize) >> 12);
        asm!("fence rw, rw", "csrw satp, {0}", "sfence.vma", in(reg) RV_FINAL_SATP, options(nostack));
    }
}
/// 安装本核唯一 CpuLocal 基址，随后 trap 与普通代码共享该对象。
///
/// # Safety
/// local 必须非空、对齐且指向已初始化、永久存活的本核 CpuLocal；
/// 本核必须关闭 IRQ，且不得把同一个对象安装给其他 CPU。
pub unsafe fn install_cpu_local(local: *const CpuLocal) {
    // SAFETY: 调用者保证 local 的本核归属与永久生命周期；tp 是内核专用基址寄存器。
    unsafe {
        asm!("mv tp, {0}", in(reg) local, options(nostack));
    }
}
/// 读取本核 CpuLocal 基址。
pub fn cpu_local() -> *const CpuLocal {
    let value;
    // SAFETY: 只读取内核专用 tp，不解引用；未安装时由 cpu::current 拒绝空指针。
    unsafe {
        asm!("mv {0}, tp", out(reg) value, options(nomem, nostack));
    }
    value
}
/// 关闭本核旧事件并安装可恢复 kernel trap。
pub fn init_current_cpu() {
    // SAFETY: Rust 引导入口保证 S-mode、内核代码映射及专用栈；stvec 指向
    // 按 KernelTrapFrame 布局保存现场的入口，关闭 FS 后 Rust 不使用浮点寄存器。
    unsafe {
        asm!("csrw stvec, {0}", "csrw sscratch, zero", "csrci sip, 2", "li t0, 0x6000", "csrc sstatus, t0", "li t0, 0x222", "csrw sie, t0", in(reg) rv_trap_entry as usize, out("t0") _, options(nostack));
    }
    disable_timer();
}
/// 保存并关闭本地中断；memory clobber 阻止临界区内存访问越过关 IRQ 边界。
pub fn irq_save() -> usize {
    let saved;
    // SAFETY: 内核运行于 S-mode；仅清除本核 SIE，并返回原状态供同核恢复。
    unsafe {
        asm!("csrrci {0}, sstatus, 2", out(reg) saved, options(nostack));
    }
    saved
}
/// 仅恢复保存的 SIE 位；memory clobber 阻止受保护访问越过恢复边界。
///
/// # Safety
/// saved 必须来自本核对应的 irq_save；所有受保护访问和锁释放必须已经完成，
/// 嵌套保存必须按逆序恢复，不能借此在原本关闭 IRQ 的上下文中开启 IRQ。
pub unsafe fn irq_restore(saved: usize) {
    // SAFETY: 调用者保证同核、逆序恢复及临界区结束，只恢复 SIE，不覆盖其他状态位。
    unsafe {
        if saved & 2 != 0 {
            asm!("csrsi sstatus, 2", options(nostack));
        } else {
            asm!("csrci sstatus, 2", options(nostack));
        }
    }
}
/// 查询本地中断是否使能。
pub fn irq_enabled() -> bool {
    let s: usize;
    // SAFETY: S-mode 可读取本核 sstatus，本操作不改变 IRQ 状态或内存。
    unsafe {
        asm!("csrr {0}, sstatus", out(reg) s, options(nomem, nostack));
    }
    s & 2 != 0
}
/// 开启本地中断；memory clobber 保持初始化写入和后续 online 发布位于正确边界。
pub fn enable_interrupts() {
    // SAFETY: 两种 Rust 引导入口在允许调用此接口前安装本核现场、trap 与 IRQ 处理资源。
    unsafe {
        asm!("csrsi sstatus, 2", options(nostack));
    }
}
/// 等待本地硬件事件。
pub fn wait_for_interrupt() {
    // SAFETY: S-mode 的 wfi 仅等待事件，trap 使用已经安装的本核栈与入口。
    unsafe {
        asm!("wfi", options(nomem, nostack));
    }
}
/// 读取真实硬件时间计数器。
pub fn read_counter() -> u64 {
    let value;
    // SAFETY: 固件已向 S-mode 开放 time 计数器；读取不涉及内存或修改计时状态。
    unsafe {
        asm!("rdtime {0}", out(reg) value, options(nomem, nostack));
    }
    value
}
/// 返回启动时解析的 timebase-frequency。
pub fn counter_frequency() -> u64 {
    crate::boot::info().counter_frequency
}
/// 按 SBI ABI 发起固件调用；仅本模块选择无借用指针的 TIME/IPI/HSM/SRST 参数。
fn sbi(
    extension: usize,
    function: usize,
    mut a0: usize,
    mut a1: usize,
    a2: usize,
) -> Result<usize, isize> {
    // SAFETY: S-mode 通过 ecall 进入固件，参数/返回值使用 SBI 指定的 a0–a7；
    // 本模块仅 HSM 传入物理记录，其生命周期由 start_cpu 的调用者保证。
    unsafe {
        asm!("ecall", inlateout("a0") a0, inlateout("a1") a1, in("a2") a2, in("a3") 0usize, in("a4") 0usize, in("a5") 0usize, in("a6") function, in("a7") extension, options(nostack));
    }
    if a0 == 0 {
        Ok(a1)
    } else {
        Err(a0 as isize)
    }
}
/// 用 SBI TIME 编程一次绝对期限；固件错误直接报告。
pub fn program_timer(deadline: u64) {
    sbi(0x54494d45, 0, deadline as usize, 0, 0).expect("SBI TIME failed");
}
/// 关闭本核 timer pending 和下一次事件。
pub fn disable_timer() {
    program_timer(u64::MAX);
}
/// 通过 HSM 启动真实物理 AP 入口并传递静态启动记录。
///
/// # Safety
/// BSP 必须串行启动尚未运行的目标 hart；物理记录必须对齐、已初始化并永久存活。
/// 记录内的独占栈必须满足 16 字节 ABI 对齐，CpuLocal 必须属于目标核并永久有效；
/// bootstrap/最终表必须已发布并映射记录、栈、CpuLocal 及 AP 内核代码。
pub unsafe fn start_cpu(hardware_id: usize, boot_record_physical: usize) -> Result<(), isize> {
    // SAFETY: 调用者保证启动记录、栈与页表已初始化；fence 在 HSM 唤醒前发布这些写入。
    unsafe {
        asm!("fence rw, rw", options(nostack));
    }
    sbi(
        0x48534d,
        0,
        hardware_id,
        address!(__secondary_entry_phys),
        boot_record_physical,
    )
    .map(|_| ())
}
/// 发布软件队列后通知指定 hart；错误不丢弃。
pub fn send_ipi(hardware_id: usize) {
    // SAFETY: fence 只发布本核先前队列写入，目标 hart 由公共 CPU 表选择。
    unsafe {
        asm!("fence rw, rw", options(nostack));
    }
    sbi(0x735049, 0, 1, hardware_id, 0).expect("SBI IPI failed");
}
/// 读取已发布 PLIC 窗口中的一个 32 位寄存器。
///
/// # Safety
/// 引导入口必须已映射实际 PLIC；offset 必须四字节对齐且完整落在已验证窗口的可读寄存器内。
/// claim 只能由对应本核读取。
unsafe fn mmio_read(offset: usize) -> u32 {
    let region = crate::boot::info().interrupts.controller;
    // SAFETY: 调用者保证实际设备映射、窗口内完整的对齐 32 位访问和寄存器读语义。
    unsafe { ptr::read_volatile((mmio_address(region.start) + offset) as *const u32) }
}
/// 写入已发布 PLIC 窗口中的一个 32 位寄存器并同步设备访问。
///
/// # Safety
/// 引导入口必须已映射实际 PLIC；offset 必须四字节对齐且完整落在已验证窗口内，
/// value 必须符合寄存器写语义；共享 priority 由 IRQ 描述符锁串行化，
/// enable 只在 IRQ 关闭、AP 未释放的初始化过程写入。
unsafe fn mmio_write(offset: usize, value: u32) {
    let region = crate::boot::info().interrupts.controller;
    // SAFETY: 调用者保证窗口内完整的对齐 32 位访问、寄存器写语义和共享状态串行化；
    // fence 在返回前完成设备访问排序。
    unsafe {
        ptr::write_volatile((mmio_address(region.start) + offset) as *mut u32, value);
        asm!("fence iorw, iorw", options(nostack));
    }
}
/// 从已安装的本核对象选择 FDT 对应的 S-mode claim/complete 寄存器。
fn claim_offset() -> usize {
    0x200004 + crate::boot::info().interrupts.plic_contexts[crate::cpu::current().id] * 0x1000
}
/// 初始化 FDT 指定的所有 S context，UART 仅路由到 BSP。
///
/// # Safety
/// info 必须是已发布的实际 QEMU PLIC 描述，设备窗口已映射；
/// UART 源号必须小于 1024 且非零，所有 S context 的 enable/threshold/claim 槽必须完整落在窗口内。
/// BSP 必须关闭 IRQ 且 AP 尚未释放，不能与 claim/complete 或 enable 读改写并发。
pub unsafe fn external_init(info: &BootInfo) {
    // SAFETY: 调用者保证实际 S context、源号和已发布窗口一致；启动串行阶段
    // 独占 enable/threshold/priority 寄存器，32 位寄存器地址按 PLIC 布局计算。
    unsafe {
        for &context in &info.interrupts.plic_contexts[..info.cpu_count] {
            for word in 0..32 {
                mmio_write(0x2000 + context * 0x80 + word * 4, 0);
            }
            mmio_write(0x200000 + context * 0x1000, 0);
        }
        let source = info.interrupts.uart_source as usize;
        mmio_write(source * 4, 0);
        let offset = 0x2000 + info.interrupts.plic_contexts[0] * 0x80 + source / 32 * 4;
        mmio_write(offset, 1 << (source % 32));
    }
}
/// 将 UART 源优先级置零，屏蔽通知但保留固定路由与待处理状态。
pub fn external_mask() {
    // SAFETY: 实际源号和窗口已经验证；公共 IRQ 描述符锁串行化此源状态，
    // priority=0 禁止 claim，enable 保留以允许已 claim 的源正常 complete。
    unsafe { mmio_write(crate::boot::info().interrupts.uart_source as usize * 4, 0) };
}
/// 恢复 UART 源优先级，使屏蔽期间已有的 pending 重新参与通知。
pub fn external_unmask() {
    // SAFETY: 描述符锁串行化有效源的 priority；固定 BSP 路由已经在独占初始化时建立。
    // priority 写入使控制器重新评估 pending，不能只改变路由 enable 位。
    unsafe { mmio_write(crate::boot::info().interrupts.uart_source as usize * 4, 1) };
}
/// 从本核真实 PLIC context claim；零表示无待处理源。
pub fn external_claim() -> Option<u32> {
    // SAFETY: 引导入口已初始化实际 PLIC；已安装的 CpuLocal 选择本核 S context，
    // claim 由本核 IRQ 流读取，不与其他核共用该寄存器。
    let source = unsafe { mmio_read(claim_offset()) };
    if source == 0 {
        None
    } else {
        Some(source)
    }
}
/// 完成本核已 claim 的硬件源，不改变源的屏蔽状态或固定路由。
///
/// # Safety
/// source 必须来自本核尚未完成的真实 claim，且等于已验证的 UART 硬件源；
/// 调用者必须持有对应 IRQ 描述符锁，不能跨核或重复 complete。
pub unsafe fn external_complete(source: u32) {
    // SAFETY: source 是本核已 claim 的 UART 源，complete 槽由已验证的 context 定位；
    // 路由 enable 永久保留，priority 屏蔽不影响这次真实完成。
    unsafe { mmio_write(claim_offset(), source) };
}
/// 请求 SBI SRST 关机，失败或异常返回均报告。
pub fn shutdown() -> ! {
    sbi(0x53525354, 0, 0, 0, 0).expect("SBI SRST failed");
    panic!("SBI SRST unexpectedly returned");
}
#[cfg(feature = "fault-test")]
/// 触发真实非法指令异常供现场诊断。
pub fn trigger_fault() -> ! {
    // SAFETY: 内核 trap 已安装；非法指令进入诊断入口并终止执行，不返回 Rust。
    unsafe {
        asm!(".word 0", options(noreturn));
    }
}
/// 处理汇编构造的本核独占 trap frame；FS 为 Off 时不访问未保存的浮点字段。
#[no_mangle]
extern "C" fn rv_handle_trap(frame: &mut KernelTrapFrame) {
    if frame.scause & (1usize << 63) == 0 {
        crate::console::fatal(frame.sepc, frame.gpr[2], frame.scause, frame.stval);
    }
    let _hard_irq = crate::cpu::HardIrqGuard::enter();
    match frame.scause {
        value if value == (1usize << 63) | 5 => {
            disable_timer();
            crate::cpu::handle_timer();
        }
        value if value == (1usize << 63) | 1 => {
            // SAFETY: 当前为 S-mode 软件中断，清除本核 SSIP 后用 fence 获取发送方队列写入。
            unsafe {
                asm!("csrci sip, 2", "fence rw, rw", options(nostack));
            }
            crate::cpu::handle_ipi();
        }
        value if value == (1usize << 63) | 9 => crate::irq::handle_external(),
        _ => crate::console::fatal(frame.sepc, frame.gpr[2], frame.scause, frame.stval),
    }
}

#[cfg(feature = "boot-tests")]
global_asm!(include_str!("riscv64/registers.S"));
#[cfg(feature = "boot-tests")]
extern "C" {
    /// 按整数调用 ABI 临时启用 IRQ，等待本核计数器推进并检验寄存器现场。
    ///
    /// # Safety
    /// counter 必须指向永久、对齐的本核 AtomicUsize，expected 必须由已编程的 timer 推进；
    /// 必须支持 D 且没有活跃浮点上下文，fp 只能为 0 或 1，trap/栈/CpuLocal 必须已安装。
    fn rv_register_roundtrip(
        counter: *const core::sync::atomic::AtomicUsize,
        expected: usize,
        fp: usize,
    ) -> usize;
}
/// 真实 timer 异常验证 GPR/SP/TP 以及启用/关闭 FP 两种现场。
///
/// # Safety
/// 只能在本核 trap/timer 已安装、CpuLocal 永久有效且未持有 IRQ-save 锁时调用；
/// 当前不能有活跃浮点上下文，QEMU CPU 必须支持 D，测试会启用 IRQ 并临时覆盖 timer。
#[cfg(feature = "boot-tests")]
pub unsafe fn test_register_preservation() -> bool {
    use core::sync::atomic::Ordering;
    let saved = irq_save();
    let mut success = true;
    for fp in [0, 1] {
        let counter = &crate::cpu::current().timer_interrupts;
        let expected = counter.load(Ordering::Acquire) + 1;
        program_timer(read_counter() + counter_frequency() / 100);
        // SAFETY: counter 来自永久本核对象且按 AtomicUsize 对齐；调用者保证 timer
        // trap 可运行、没有活跃浮点上下文，汇编遵守整数 ABI 并恢复原 sstatus/FCSR。
        success &= unsafe { rv_register_roundtrip(counter, expected, fp) } != 0;
        success &= counter.load(Ordering::Acquire) == expected;
    }
    // SAFETY: saved 在本函数同核保存，汇编测试已返回且没有尚未释放的临界区锁。
    unsafe {
        irq_restore(saved);
    }
    success
}
