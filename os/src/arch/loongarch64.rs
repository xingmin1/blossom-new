//! QEMU 9.2.1 virt 的 DMW、异常、一次定时器及 PCH-PIC/EXTIOI 机制。
use crate::boot::BootInfo;
use crate::sched::context::TaskContext;
use crate::sync::SpinLock;
use core::arch::{asm, global_asm};
use core::mem::{offset_of, size_of, MaybeUninit};
use core::sync::atomic::{AtomicBool, AtomicUsize};

pub const NAME: &str = "loongarch64";

mod mmu;
mod uaccess;
mod user;

pub use mmu::{
    activate_kernel_root, activate_user_root, flush_user_translations, initialize_user_root,
    sync_user_icache, user_empty_entry, user_entry_is_table, user_entry_physical, user_leaf_entry,
    user_leaf_permissions, user_table_entry, SIGNAL_RETURN_PAGE, USER_ADDRESS_END, USER_LEVELS,
    USER_STACK_TOP,
};
pub use uaccess::{copy_user_bytes, load_user_u32, store_user_u32};
pub use user::{run_user, signal_return_code, UserContext};

#[cfg(feature = "ktest")]
pub(crate) use user::{exit_offset, fault_offset, loop_offset, loop_spin_offset, user_code};

#[repr(C, align(16))]
struct KernelTrapFrame {
    gpr: [usize; 32],
    era: usize,
    prmd: usize,
    estat: usize,
    badv: usize,
    euen: usize,
    cpu_base: usize,
    // EUEN.FPE 关闭时汇编跳过这些字段，Rust 不得把它们视为已初始化值。
    fpr: MaybeUninit<[u64; 32]>,
    fcsr: MaybeUninit<usize>,
    fcc: MaybeUninit<[usize; 8]>,
}

// refill 有独立 CSR 现场，不借用普通异常的 ERA/PRMD/BADV。
#[repr(C, align(16))]
struct RefillFrame {
    gpr: [usize; 32],
    era: usize,
    prmd: usize,
    badv: usize,
}

global_asm!(include_str!("loongarch64/entry.S"),
    stack_size = const crate::cpu::STACK_SIZE,
    boot_stack = const offset_of!(crate::cpu::CpuBoot, stack_top),
    boot_local = const offset_of!(crate::cpu::CpuBoot, local),
);
global_asm!(include_str!("loongarch64/trap.S"),
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
global_asm!(include_str!("loongarch64/switch.S"),
    context_ra = const offset_of!(TaskContext, ra),
    context_sp = const offset_of!(TaskContext, sp),
    context_saved = const offset_of!(TaskContext, saved),
    word_size = const size_of::<usize>(),
    execution_tests = const cfg!(feature = "ktest") as usize,
);

global_asm!(
    include_str!("loongarch64/idle.S"),
    execution_tests = const cfg!(feature = "ktest") as usize,
);
extern "C" {
    static __direct_base: u8;
    static __mmio_base: u8;
    static __secondary_entry_phys: u8;
    /// 普通内核异常入口，由硬件跳转并按 KernelTrapFrame 布局保存本核现场。
    fn la_kernel_trap();
    /// 物理 refill 入口，使用独立 CSR 现场并在调用 Rust 前恢复高地址 DMW。
    fn la_kernel_refill();
    /// 按普通函数 ABI 保存旧整数现场并恢复下一执行者，不改写本核 KS0。
    fn la_switch_context(prev: *mut TaskContext, next: *const TaskContext);
    /// 在启 IE 到 IDLE 的显式窗口内复查软件提示；trap 可把返回地址改到函数入口重新检查。
    fn la_idle_once_asm(
        kick_pending: *const AtomicUsize,
        need_resched: *const AtomicBool,
        #[cfg(feature = "ktest")] wait_completions: *const AtomicUsize,
    );
    static la_idle_window_start: u8;
    static la_idle_window_end: u8;
}

/// 在引导入口确立的 PLV0 环境读取本核 CSR；寄存器编号由本模块常量指定。
#[inline]
fn csr_read<const R: u16>() -> usize {
    let value;
    // SAFETY: 两种 Rust 引导入口均处于 PLV0；调用点只选择本架构可读的 CSR。
    unsafe {
        asm!("csrrd {value}, {reg}", value = out(reg) value, reg = const R, options(nomem, nostack));
    }
    value
}
/// 写入本核 CSR，调用点负责保持寄存器对应的异常、映射和中断协议。
///
/// # Safety
/// 必须处于 PLV0，R 必须是可写 CSR，写入值不得破坏当前代码、栈和 per-CPU 对象的可达性。
#[inline]
unsafe fn csr_write<const R: u16>(value: usize) {
    // SAFETY: 调用者保证 CSR 可写、特权级正确，并保持当前执行现场所依赖的映射。
    unsafe {
        asm!("csrwr {value}, {reg}", value = inout(reg) value => _, reg = const R, options(nostack));
    }
}
/// 读取本模块选定的 QEMU IOCSR 状态，不通过普通内存指针访问设备。
#[inline]
fn iocsr_read32(reg: usize) -> u32 {
    let value: usize;
    // SAFETY: 调用点只传递 QEMU virt 的有效 IOCSR 编号，入口保证当前执行级为 PLV0。
    unsafe {
        asm!("iocsrrd.w {value}, {reg}", value = out(reg) value, reg = in(reg) reg, options(nostack));
    }
    value as u32
}
/// 读取 QEMU virt 的 64 位 IOCSR 状态。
#[inline]
fn iocsr_read64(reg: usize) -> u64 {
    let value;
    // SAFETY: 调用点只选择有效的 64 位 IOCSR，PLV0 已由引导入口建立。
    unsafe {
        asm!("iocsrrd.d {value}, {reg}", value = out(reg) value, reg = in(reg) reg, options(nostack));
    }
    value
}
/// 写入选定的 32 位 IOCSR；共享路由读改写由独立控制器锁串行化。
#[inline]
fn iocsr_write32(reg: usize, value: u32) {
    // SAFETY: 本模块调用点使用已验证平台的有效 IOCSR；设备状态不承载 Rust 引用。
    unsafe {
        asm!("iocsrwr.w {value}, {reg}", value = in(reg) value as usize, reg = in(reg) reg, options(nostack));
    }
}
/// 写入 QEMU mailbox 或 EXTIOI 的 64 位 IOCSR。
#[inline]
fn iocsr_write64(reg: usize, value: u64) {
    // SAFETY: 调用点选择 QEMU virt 的有效 IOCSR，mailbox 指针的生命周期由启动接口约定。
    unsafe {
        asm!("iocsrwr.d {value}, {reg}", value = in(reg) value, reg = in(reg) reg, options(nostack));
    }
}
/// 排序本核设备访问和已发表的共享启动记录。
fn barrier() {
    // SAFETY: dbar 在引导入口建立的 PLV0 可执行，不解引用任何指针。
    unsafe {
        asm!("dbar 0", options(nostack));
    }
}

/// 以完成型写屏障排序 coherent DMA 内存写入。
pub(super) fn dma_write_barrier() {
    // SAFETY: 引导入口已建立 PLV0，dbar 0x0a 排序前后写访问；
    // 汇编保留 memory 副作用，不声明 nomem/readonly。
    unsafe { asm!("dbar 0x0a", options(nostack)) };
}

/// 在观察设备完成后，以完成型读屏障排序 coherent 数据读取。
pub(super) fn dma_read_barrier() {
    // SAFETY: PLV0 可执行 dbar 0x05，完成前读并排序后读，不访问额外地址；
    // 汇编的 memory 副作用同时约束编译器读取顺序。
    unsafe { asm!("dbar 0x05", options(nostack)) };
}

/// 对 coherent 内存和设备 MMIO 执行完成型双向排序。
pub(super) fn dma_barrier() {
    // SAFETY: PLV0 可执行 dbar 0x00，覆盖全部前后访问；
    // 不声明 nomem/readonly，禁止编译器将内存访问移过此完整屏障。
    unsafe { asm!("dbar 0x00", options(nostack)) };
}

/// 从 CPUId CSR 取得当前硬件身份，不解释 QEMU 的引导参数。
pub fn boot_hardware_id(_: [usize; 3]) -> usize {
    csr_read::<0x20>() & 0x3ff
}
/// 将真实 RAM 物理地址转换为缓存 DMW 别名。
pub fn phys_to_virt(pa: usize) -> usize {
    let base: usize;
    // SAFETY: 链接脚本提供绝对 DMW 段值；la.abs 仅取符号值，不读取该地址。
    unsafe {
        asm!("la.abs {base}, {symbol}", base = out(reg) base, symbol = sym __direct_base, options(nomem, nostack));
    }
    pa | base
}
/// 从缓存 DMW 内的 RAM 地址取得物理地址；其他地址类别不适用此换算。
pub fn virt_to_phys(va: usize) -> usize {
    va & !phys_to_virt(0)
}
/// 将设备物理地址转换为强序 DMW 地址；返回整数地址，不验证设备资源范围。
pub fn mmio_address(pa: usize) -> usize {
    let base: usize;
    // SAFETY: 链接脚本提供强序 DMW 段值；此指令仅计算地址，不访问设备。
    unsafe {
        asm!("la.abs {base}, {symbol}", base = out(reg) base, symbol = sym __mmio_base, options(nomem, nostack));
    }
    pa | base
}
/// 确认最终 PLV0 DMW 并建立永久 invalid 页表链；普通内核不再使用 DA 低地址别名。
///
/// # Safety
/// 必须由 BSP 在 IRQ 关闭、辅核尚未释放时调用一次；入口已建立 PG、缓存与强序 DMW。
/// BootInfo 必须是已发布的真实平台对象，调用期间不得修改这些映射。
pub unsafe fn finish_boot_mappings(_: &BootInfo) {
    assert_eq!(csr_read::<0>() & 0x18, 0x10);
    assert_eq!(csr_read::<0x181>(), phys_to_virt(0) | 0x11);
    assert_eq!(csr_read::<0x180>(), mmio_address(0) | 1);
    // SAFETY: 本函数的 BSP 独占启动契约保证 AP 尚未发布，静态 DMW 映射已通过上面的断言。
    unsafe { mmu::initialize_invalid_tables() };
}
/// 将稳定的内核 per-CPU 指针放入 KS0；用户 TLS 寄存器不参与此约定。
///
/// # Safety
/// local 必须非空、满足 CpuLocal 对齐、已完整初始化且属于本核；对象和 DMW 映射必须永久有效。
/// 必须在本核 IRQ 关闭时安装，不得把同一个对象安装给另一个 CPU。
pub unsafe fn install_cpu_local(local: *const crate::cpu::CpuLocal) {
    // SAFETY: 调用者保证本核 IRQ 已关闭，指针是本核永久有效对象，KS0 只保存该基址。
    unsafe { csr_write::<0x30>(local as usize) };
}
/// 读取当前 CPU 的 KS0 基址。
pub fn cpu_local() -> *const crate::cpu::CpuLocal {
    csr_read::<0x30>() as *const _
}

/// 在调度专用 IRQ 关闭段切换整数现场；恢复后 KS0 仍属于实际执行的 CPU。
///
/// # Safety
/// 调用者必须独占 prev、next 的上下文访问，两者非空、对齐且互不重叠。
/// prev 可写，next 已初始化为合法函数返回现场，其独占栈满足 16 字节 ABI 对齐；
/// 两份现场及其栈必须保留到各自恢复。当前 IRQ 必须关闭、EUEN 必须为零，
/// 不得持有普通锁、IRQ/preempt guard 或跨切换保存本核借用。
pub unsafe fn switch_context(prev: *mut TaskContext, next: *const TaskContext) {
    assert!(!irq_enabled(), "context switch requires IRQ off");
    assert_eq!(csr_read::<2>(), 0, "context switch requires EUEN off");
    // SAFETY: 调用者独占两个有效现场及栈，断言确认 IRQ 与扩展单元关闭；
    // 汇编仅按 Rust 提供的布局访问整数字段，不恢复 CPU 基址或扩展单元现场。
    unsafe { la_switch_context(prev, next) };
}
/// 安装本核异常与三级用户 walker、关闭内核扩展单元，并初始化 timer/IPI。
pub fn init_current_cpu() {
    irq_save();
    // SAFETY: 入口已建立本核 PLV0/DMW 和有效栈；IRQ 已关闭，异常代码在永久内核映射内。
    unsafe {
        csr_write::<0xc>(la_kernel_trap as usize);
        csr_write::<0x88>(virt_to_phys(la_kernel_refill as usize));
        csr_write::<4>((1 << 11) | (1 << 12) | (1 << 3));
        csr_write::<2>(0);
    }
    mmu::initialize_current_cpu();
    disable_timer();
    iocsr_write32(0x100c, u32::MAX);
    iocsr_write32(0x1004, u32::MAX);
    barrier();
}
/// 在 PLV0 原子关闭本核 IRQ 并返回原 CRMD；编译器内存屏障保持临界区访问位于关 IRQ 之后。
pub fn irq_save() -> usize {
    let old;
    // SAFETY: 入口保证 PLV0；固定输入 $t0/r12 仅承载 IE 掩码 4，避开 csrxchg 禁用的 r0/r1。
    // csrxchg 不改写 $t0，inout 声明旧 CRMD 的回写并排除与 $t0 重叠；指令不访问栈。
    // 仅清除 IE；不使用 nomem/readonly，保留普通访存不能跨越 IRQ 边界的编译器屏障。
    unsafe {
        asm!("csrxchg {old}, $t0, 0", old = inout(reg) 0usize => old, in("$t0") 4usize, options(nostack));
    }
    old
}
/// 在 PLV0 只恢复原来的 IE 位；编译器内存屏障保持临界区访问位于恢复 IRQ 之前。
///
/// # Safety
/// saved 必须来自本核匹配的 irq_save，期间不能迁移 CPU；嵌套保存必须逆序恢复，
/// 恢复 IE 前必须释放依赖 IRQ 关闭的锁和借用。
pub unsafe fn irq_restore(saved: usize) {
    // SAFETY: 调用者保证同核恢复且临界区已结束；入口保证 PLV0，固定 $t0/r12 为合法掩码寄存器。
    // 只恢复掩码 4 指定的 IE；$t0 保持不变，inout 声明旧 CRMD 回写并排除与 $t0 重叠。
    // 指令不访问栈；不使用 nomem/readonly，保留锁释放及借用结束不能越过恢复 IRQ 的编译器屏障。
    unsafe {
        asm!("csrxchg {value}, $t0, 0", value = inout(reg) (saved & 4) => _, in("$t0") 4usize, options(nostack));
    }
}
/// 查询本核全局 IRQ 开关。
pub fn irq_enabled() -> bool {
    csr_read::<0>() & 4 != 0
}
/// 在 PLV0 打开本核全局 IRQ；编译器内存屏障保持初始化写入和 online 发布位于各自边界。
pub fn enable_interrupts() {
    // SAFETY: 入口保证 PLV0，本核异常及 IRQ 资源已安装；固定 $t0/r12 避开禁用的 r0/r1。
    // 掩码 4 只使能 IE；$t0 保持不变，inout 声明旧 CRMD 回写并排除与 $t0 重叠。
    // 指令不访问栈；不使用 nomem/readonly，保留普通访存不能跨越开 IRQ 边界的编译器屏障。
    unsafe {
        asm!("csrxchg {value}, $t0, 0", value = inout(reg) 4usize => _, in("$t0") 4usize, options(nostack));
    }
}
/// 等待真实硬件中断。
pub fn wait_for_interrupt() {
    // SAFETY: 当前在 PLV0，idle 不解引用指针，也不改变已建立的映射。
    unsafe {
        asm!("idle 0", options(nostack));
    }
}

/// 以启 IE 后重查与 trap 返回回滚避免通知已消费后进入 IDLE，返回保持 IRQ 关闭。
///
/// # Safety
/// 仅允许本核 scheduler 在 IRQ-off、无锁且无普通 guard 时调用；原子必须是本核
/// 永久有效字段，trap 与独立 IRQ 源已安装且开启，当前栈不得跨本次调用迁移。
/// 测试计数以自然对齐的 AtomicUsize 提供，整个生命周期仅使用原子访问。
pub(crate) unsafe fn idle_once(
    kick_pending: &AtomicUsize,
    need_resched: &AtomicBool,
    #[cfg(feature = "ktest")] wait_completions: &AtomicUsize,
) {
    assert!(!irq_enabled(), "idle entry requires IRQ off");
    // SAFETY: 调用者保证本核稳定原子与无锁 scheduler 栈；汇编遵守整数 ABI，
    // 在精确标记的启 IE/重查/IDLE 窗口可被 trap 重启，返回前关闭 IE，不改 FP/KS0。
    // 测试计数指针经 a2 传入；amadd_db.d 的输出 t1、增量 t0、地址 a2 互不重叠，
    // 64 位原子 RMW 的完整屏障与远端 Acquire 读取匹配，trap 保留全部参数和临时寄存器。
    unsafe {
        #[cfg(feature = "ktest")]
        la_idle_once_asm(kick_pending, need_resched, wait_completions);
        #[cfg(not(feature = "ktest"))]
        la_idle_once_asm(kick_pending, need_resched);
    };
    assert!(!irq_enabled(), "idle exit has IRQ enabled");
}
/// 读取稳定硬件计数器。
pub fn read_counter() -> u64 {
    let value;
    // SAFETY: rdtime 仅读取硬件计数器，入口已建立可执行该指令的环境。
    unsafe {
        asm!("rdtime.d {value}, $zero", value = out(reg) value, options(nomem, nostack));
    }
    value
}
/// 读取只读 CPUCFG 参数；本模块仅选择计数器频率的配置项。
fn cpucfg(index: usize) -> u64 {
    let value: usize;
    // SAFETY: CPUCFG4/5 是本架构有效的只读配置项，不访问普通内存。
    unsafe {
        asm!("cpucfg {value}, {index}", value = out(reg) value, index = in(reg) index, options(nomem, nostack));
    }
    value as u64 & 0xffff_ffff
}
/// 仅资源发现前从 CPUCFG4/5 取得实际 counter 频率，不作为运行期读数路径。
pub fn boot_counter_frequency() -> u64 {
    let base = cpucfg(4);
    let ratio = cpucfg(5);
    let mul = ratio & 0xffff;
    let div = ratio >> 16;
    assert!(
        base != 0 && mul != 0 && div != 0,
        "invalid CPUCFG counter frequency"
    );
    base * mul / div
}
/// 返回一次启动资源发现发布的 counter 频率。
pub fn counter_frequency() -> u64 {
    crate::boot::info().counter_frequency
}
/// 将绝对 deadline 向上量化到四个 tick，并编程一次事件。
pub fn program_timer(deadline: u64) {
    disable_timer();
    let delta = deadline.saturating_sub(read_counter()).max(4);
    let value = delta.saturating_add(3).min((1u64 << 48) - 4) & !3;
    // SAFETY: 在 PLV0 写本核 TCFG；上面的量化保证只写有效 tick 位与一次事件使能位。
    unsafe {
        csr_write::<0x41>(value as usize | 1);
    }
}
/// 关闭一次事件并清除真实 timer pending。
pub fn disable_timer() {
    // SAFETY: TCFG/TICLR 是本核定时器 CSR，写零关闭事件、写一清 pending 均不影响映射。
    unsafe {
        csr_write::<0x41>(0);
        csr_write::<0x44>(1);
    }
}
/// 按 QEMU MAIL_SEND 编码向目标 CPU 发表一个 64 位邮箱值。
fn mail_send(cpu: usize, mailbox: usize, data: usize) {
    let target = (1u64 << 31) | ((cpu as u64) << 16);
    iocsr_write64(
        0x1048,
        (data as u64 & 0xffff_ffff_0000_0000) | target | (((mailbox * 2 + 1) as u64) << 2),
    );
    iocsr_write64(
        0x1048,
        ((data as u64) << 32) | target | (((mailbox * 2) as u64) << 2),
    );
}
/// 通过真实 mailbox1 发表启动记录，mailbox0 发表物理入口，再发送启动 IPI。
///
/// # Safety
/// 必须由 BSP 串行启动尚未运行的已枚举辅核；物理地址须指向对齐、完整初始化且永久保留的 CpuBoot。
/// 记录中的栈须独占、满足 16 字节 ABI 对齐，CpuLocal 须属于目标核且完整初始化；两者与 DMW 均须永久有效。
/// 发布后不得再修改启动记录。
pub unsafe fn start_cpu(hardware_id: usize, boot_record_physical: usize) -> Result<(), isize> {
    if crate::boot::MAX_CPUS <= hardware_id {
        return Err(-22);
    }
    barrier();
    mail_send(hardware_id, 1, boot_record_physical);
    let entry: usize;
    // SAFETY: 链接脚本给出可执行的物理辅核入口；这里只取符号值，记录契约由调用者保证。
    unsafe {
        asm!("la.abs {entry}, {symbol}", entry = out(reg) entry, symbol = sym __secondary_entry_phys, options(nomem, nostack));
    }
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
/// 写入已验证 PCH-PIC 范围的 32 位寄存器，共享 word 由控制器锁保护。
fn pic_write(offset: usize, value: u32) {
    // SAFETY: 已发布 BootInfo 的控制器范围覆盖调用点的对齐 offset；入口保持强序 DMW 永久有效。
    unsafe {
        core::ptr::write_volatile(
            (mmio_address(crate::boot::info().interrupts.controller.start) + offset) as *mut u32,
            value,
        );
    }
}
/// 读取已验证 PCH-PIC 范围的 32 位寄存器。
fn pic_read(offset: usize) -> u32 {
    // SAFETY: BootInfo getter 返回已发布对象，调用点 offset 对齐且位于控制器范围，强序 DMW 永久有效。
    unsafe {
        core::ptr::read_volatile(
            (mmio_address(crate::boot::info().interrupts.controller.start) + offset) as *const u32,
        )
    }
}
/// 将已验证的 EXTIOI 基址与本模块寄存器偏移组合成 IOCSR 编号。
fn ext_reg(offset: usize) -> usize {
    crate::boot::info().interrupts.parent_controller.start + offset
}
static EXTERNAL_CONTROLLER: SpinLock<()> = SpinLock::new(());

/// 定位实际源所在的共享 32 位 word 与位掩码。
fn source_word(source: u32) -> (usize, u32) {
    assert!(source < crate::boot::info().interrupts.source_count);
    (source as usize / 32 * 4, 1 << (source % 32))
}

/// 为实际 PCH 源逐位建立 level-high、同号 EXTIOI 向量与固定 BSP 路由。
///
/// # Safety
/// BSP 必须在本核 IRQ 关闭且 AP 尚未释放时独占初始化；info 是永久发布的平台对象，
/// PCH-PIC 强序 DMW 与 EXTIOI IOCSR 范围已经验证且永久有效。
pub unsafe fn external_init(info: &BootInfo) {
    assert_eq!(info.interrupts.source_count, 64);
    assert!(info.bsp_hardware_id < 4);
    assert_eq!(info.interrupts.root_interrupt, 3);
    let _controller = EXTERNAL_CONTROLLER.lock();
    for source in 0..info.interrupts.source_count {
        let (word, bit) = source_word(source);
        pic_write(0x20 + word, pic_read(0x20 + word) | bit);
        pic_write(0x40 + word, pic_read(0x40 + word) | bit);
        pic_write(0x60 + word, pic_read(0x60 + word) & !bit);
        pic_write(0x3e0 + word, pic_read(0x3e0 + word) & !bit);
        // SAFETY: 实际源小于 64，两个字节寄存器完整落在已验证 PCH 窗口；
        // 控制器锁与 AP 前初始化保证独占，各源路由到同号 EXTIOI 向量。
        unsafe {
            let base = mmio_address(info.interrupts.controller.start);
            core::ptr::write_volatile((base + 0x100 + source as usize) as *mut u8, 1);
            core::ptr::write_volatile((base + 0x200 + source as usize) as *mut u8, source as u8);
        }
        let route_reg = ext_reg(0x800 + (source as usize & !3));
        let shift = source % 4 * 8;
        let route = (1u32 << info.bsp_hardware_id) << shift;
        iocsr_write32(
            route_reg,
            (iocsr_read32(route_reg) & !(0xff << shift)) | route,
        );
        iocsr_write32(
            ext_reg(0x200 + word),
            iocsr_read32(ext_reg(0x200 + word)) & !bit,
        );
        iocsr_write32(
            ext_reg(0x280 + word),
            iocsr_read32(ext_reg(0x280 + word)) | bit,
        );
        iocsr_write32(ext_reg(0x400 + word), bit);
    }
    // QEMU 的 IPMAP 每个字节选择一组 32 向量的 CPU IP，NODEMAP 每组占 16 位。
    for group in 0..2usize {
        let ip_reg = ext_reg(0xc0 + group / 4 * 4);
        let shift = group % 4 * 8;
        iocsr_write32(
            ip_reg,
            (iocsr_read32(ip_reg) & !(0xff << shift)) | (2 << shift),
        );
        let node_reg = ext_reg(0xa0 + group / 2 * 4);
        let shift = group % 2 * 16;
        iocsr_write32(
            node_reg,
            (iocsr_read32(node_reg) & !(0xffff << shift)) | ((1 << group) << shift),
        );
    }
    iocsr_write64(0x420, iocsr_read64(0x420) | (1u64 << 48));
    barrier();
}

/// 按实际源位屏蔽 PCH 与 EXTIOI，不改变同 word 的其他设备。
pub fn external_mask(source: u32) {
    let (word, bit) = source_word(source);
    let _controller = EXTERNAL_CONTROLLER.lock();
    pic_write(0x20 + word, pic_read(0x20 + word) | bit);
    let reg = ext_reg(0x200 + word);
    iocsr_write32(reg, iocsr_read32(reg) & !bit);
    barrier();
}

/// 清该源 PCH 锁存、先开放父向量再开放 level 源，保留相邻设备状态。
pub fn external_unmask(source: u32) {
    let (word, bit) = source_word(source);
    let _controller = EXTERNAL_CONTROLLER.lock();
    pic_write(0x80 + word, bit);
    let reg = ext_reg(0x200 + word);
    iocsr_write32(reg, iocsr_read32(reg) | bit);
    pic_write(0x20 + word, pic_read(0x20 + word) & !bit);
    barrier();
}

/// 遍历本核真实 EXTIOI pending 与准入位，返回最低待处理 PCH 源。
pub fn external_claim() -> Option<u32> {
    let _controller = EXTERNAL_CONTROLLER.lock();
    let enabled =
        iocsr_read32(ext_reg(0x200)) as u64 | ((iocsr_read32(ext_reg(0x204)) as u64) << 32);
    let pending = iocsr_read64(ext_reg(0x400)) & enabled;
    if pending == 0 {
        None
    } else {
        Some(pending.trailing_zeros())
    }
}

/// 对本核已 claim 的源单独 W1C 完成，不清除相邻源的 pending。
///
/// # Safety
/// source 必须来自本核尚未完成的真实 claim；源已屏蔽，处理者已退出，
/// 本核不可迁移，调用者持该源 descriptor 锁保护完成与重新开放。
pub unsafe fn external_complete(source: u32) {
    let (word, bit) = source_word(source);
    let _controller = EXTERNAL_CONTROLLER.lock();
    iocsr_write32(ext_reg(0x400 + word), bit);
    barrier();
}
/// 写入所选 virt FDT 的 GED syscon poweroff 字节。
pub fn shutdown() -> ! {
    irq_save();
    // SAFETY: QEMU virt 固定 GED poweroff 寄存器在永久强序 DMW 中，按硬件要求进行单字节写入。
    unsafe {
        core::ptr::write_volatile(mmio_address(0x100e001c) as *mut u8, 0x34);
    }
    barrier();
    loop {
        wait_for_interrupt();
    }
}
#[cfg(feature = "ktest")]
/// 触发明确的非法指令，保留真实 kernel fault 现场。
pub fn trigger_fault() -> ! {
    // SAFETY: 此接口有意执行非法指令并由已安装的内核异常入口终止，不访问 Rust 内存。
    unsafe {
        asm!(".word 0xffffffff", options(noreturn));
    }
}

/// 读取 DMW 之外的低虚拟地址，触发真实 TLB refill 并保留 TLBRBADV 现场。
#[cfg(feature = "ktest")]
pub fn trigger_unmapped_fault() -> ! {
    // SAFETY: 两种引导入口均启用 PG；0x1000 不属于任何 DMW，内核没有安装低地址 TLB 映射。
    // 使用汇编执行故障 load，避免用 Rust 指针解引用表达无效地址访问。
    unsafe {
        asm!("ld.d {value}, {address}, 0", value = out(reg) _, address = in(reg) 0x1000usize, options(nostack));
    }
    panic!("unmapped load unexpectedly returned");
}

/// 分派真实硬中断并记录本核深度；异常直接终止，FP 关闭时不读取条件字段。
#[no_mangle]
extern "C" fn loongarch_trap(frame: &mut KernelTrapFrame) {
    if (frame.estat >> 16) & 0x3f != 0 {
        #[cfg(feature = "ktest")]
        crate::ktest::fatal_event(
            if (frame.estat >> 16) & 0x3f == 0x0d {
                "illegal"
            } else {
                "panic"
            },
            format_args!(
                "synchronous trap pc={:#x} sp={:#x} cause={:#x} bad={:#x}",
                frame.era, frame.gpr[3], frame.estat, frame.badv
            ),
        );
        #[cfg(not(feature = "ktest"))]
        crate::console::fatal(frame.era, frame.gpr[3], frame.estat, frame.badv);
    }
    // 仅显式等待窗口允许回滚。重启包含 mask 初始化，不能复用 csrxchg 已改写的 t0。
    let (idle_start, idle_end) = (
        core::ptr::addr_of!(la_idle_window_start) as usize,
        core::ptr::addr_of!(la_idle_window_end) as usize,
    );
    if idle_start <= frame.era && frame.era < idle_end {
        frame.era = la_idle_once_asm as usize;
    }
    handle_hardware_interrupt(frame.estat);
}

/// 在当前 IRQ-off 核完成真实硬中断；返回前 guard 已退出，调用者才能请求普通调度。
fn handle_hardware_interrupt(estat: usize) {
    let _hardirq = crate::cpu::HardIrqGuard::enter();
    let pending = estat & csr_read::<4>() & 0x1fff;
    if pending & (1 << 11) != 0 {
        disable_timer();
        crate::cpu::handle_timer();
    }
    if pending & (1 << 12) != 0 {
        let bits = iocsr_read32(0x1000);
        iocsr_write32(0x100c, bits);
        barrier();
        crate::cpu::handle_ipi();
    }
    if pending & (1 << 3) != 0 {
        crate::irq::handle_external();
    }
}
/// 接收 refill 专用 CSR 现场并终止；TLBRERA 的 IsTLBR 位保留为真实 cause。
#[no_mangle]
extern "C" fn loongarch_refill(frame: &RefillFrame) -> ! {
    // refill 不产生普通 ESTAT.Ecode；cause 记录 TLBRERA.IsTLBR 的真实值。
    #[cfg(feature = "ktest")]
    crate::ktest::fatal_event(
        "refill",
        format_args!(
            "refill trap pc={:#x} sp={:#x} cause={:#x} bad={:#x}",
            frame.era & !1,
            frame.gpr[3],
            frame.era & 1,
            frame.badv
        ),
    );
    #[cfg(not(feature = "ktest"))]
    crate::console::fatal(frame.era & !1, frame.gpr[3], frame.era & 1, frame.badv)
}

#[cfg(feature = "ktest")]
global_asm!(include_str!("loongarch64/register_test.S"));
#[cfg(feature = "ktest")]
extern "C" {
    /// 在有效本核计数器与异常入口下暂时改写寄存器，等待真实 timer IRQ 后恢复调用现场。
    fn la_test_register_preservation(
        counter: *const core::sync::atomic::AtomicUsize,
        ticks: usize,
        fcc: usize,
        fp: usize,
    ) -> usize;
}

/// 以一次真实本核 timer IRQ 检查指定 FP 模式与八位 FCC 图样，不遍历其他参数。
///
/// # Safety
/// 本核异常入口、timer IRQ 源、永久 CpuLocal 和当前栈必须已安装且可服务中断；
/// 调用期间必须固定 CPU、禁止调度迁移，不得持有 IRQ-save 锁或 handler 所需锁，
/// 也不得有活跃 FP 上下文。原汇编恢复被触及的 f0/f24、FCSR、全部 FCC、EUEN、
/// KS0 和整数 ABI 现场；fp=false 跳过所有 FP 指令并忽略 fcc。
/// 函数恢复进入时的 IRQ 位；调用者必须在同核 IRQ-off 下用 time::reprogram_current
/// 交还生产 deadline（包括断言失败后的清理），不能把临时测试事件当成生产配置。
#[cfg(feature = "ktest")]
pub(crate) unsafe fn ktest_register_pattern(fp: bool, fcc: u8) -> bool {
    use core::sync::atomic::Ordering;
    let _irq = crate::sync::IrqSaveGuard::new();
    let counter = &crate::cpu::current().timer_interrupts;
    let before = counter.load(Ordering::Acquire);
    let ticks = ((counter_frequency() / 10000).max(0x600) as usize + 3) & !3;
    // SAFETY: 调用者保证固定本核、真实异常/timer 与栈有效、无活跃 FP 或持锁现场；
    // 永久 counter 属于本核，ticks 为原测试对齐周期，fcc 为八位值，fp 仅取 0/1。
    let good =
        unsafe { la_test_register_preservation(counter, ticks, usize::from(fcc), usize::from(fp)) }
            != 0;
    good && before < counter.load(Ordering::Acquire)
}

#[cfg(feature = "ktest")]
extern "C" {
    /// 保留调用者整数现场，调用真实 yield 并检查恢复现场，期间不读写 KS0。
    fn la_test_task_context_preservation(yield_fn: usize) -> usize;
}

/// 在真实普通 Task 中检查 yield 的 callee-saved、栈及返回路径，并恢复调用者现场。
#[cfg(feature = "ktest")]
pub fn test_task_context_preservation() -> bool {
    assert!(
        crate::task::current_task_id().is_some(),
        "context probe requires a running task"
    );
    // SAFETY: 仅向汇编传递真实 yield 函数入口；汇编保持 ABI 栈对齐及调用者现场，
    // yield 自身检查可调度边界，探针不启用 FP、不恢复旧 CPU 基址。
    unsafe { la_test_task_context_preservation(crate::sched::yield_now as usize) != 0 }
}

/// 用一次真实 timer trap 检验整数、SP、KS0 与返回路径，不执行任何 FP 指令。
///
/// # Safety
/// 本核异常入口、timer IRQ、CpuLocal 已安装；不得持锁或 guard，不得有活跃 FP 现场。
/// 测试临时开放 IRQ 并覆盖本核 timer，期间不得调度或迁移。
#[cfg(feature = "ktest")]
pub(crate) unsafe fn test_integer_register_preservation() -> usize {
    let saved = irq_save();
    let counter = &crate::cpu::current().timer_interrupts;
    let before = counter.load(core::sync::atomic::Ordering::Acquire);
    let ticks = ((counter_frequency() / 1000).max(4) as usize + 3) & !3;
    // SAFETY: 调用者保证本核 trap/timer 与栈可服务；counter 永久有效。
    // fp=0 使汇编跳过全部 FP/FCSR/FCC 访问，返回恢复同核整数与 IRQ-off 现场。
    let good = unsafe { la_test_register_preservation(counter, ticks, 0, 0) };
    assert!(good != 0, "integer timer trap register preservation failed");
    assert!(
        before < counter.load(core::sync::atomic::Ordering::Acquire),
        "integer timer trap was not observed"
    );
    crate::time::reprogram_current();
    // SAFETY: saved 来自同核，汇编已恢复现场，测试期间没有迁移或留存受 IRQ 保护的锁。
    unsafe { irq_restore(saved) };
    1
}
