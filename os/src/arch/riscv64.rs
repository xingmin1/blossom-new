//! 固定 Sv39/QEMU virt 后端；SBI 只使用 TIME、IPI、HSM、SRST。
use crate::{
    boot::{BootInfo, Region, MAX_MEMORY_REGIONS, MAX_VIRTIO_MMIO},
    cpu::{CpuBoot, CpuLocal},
    sched::context::TaskContext,
};
use core::{
    arch::{asm, global_asm},
    mem::{offset_of, size_of, MaybeUninit},
    ptr,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};

mod mmu;
mod user;
pub use mmu::{
    activate_kernel_root, activate_user_root, flush_user_translations, initialize_user_root,
    sync_user_icache, user_empty_entry, user_entry_is_table, user_entry_physical, user_leaf_entry,
    user_leaf_permissions, user_table_entry, SIGNAL_RETURN_PAGE, USER_ADDRESS_END, USER_LEVELS,
    USER_STACK_TOP,
};
#[cfg(feature = "ktest")]
pub use user::{exit_offset, fault_offset, loop_offset, loop_spin_offset, user_code};
pub use user::{run_user, UserContext};

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
    execution_tests = const cfg!(feature = "ktest") as usize,
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

#[cfg(feature = "ktest")]
extern "C" {
    /// 按普通调用 ABI 保存调用者寄存器，跨真实 yield 校验整数现场并恢复调用者。
    ///
    /// # Safety
    /// 必须由能合法 yield 的普通 Task 调用，FS 为 Off，栈满足 16 字节 ABI 对齐。
    fn rv_task_context_roundtrip() -> usize;
}

/// 在可调度的普通 Task 中跨真实 yield 检验全部 callee-saved、SP 与返回地址。
#[cfg(feature = "ktest")]
pub fn test_task_context_preservation() -> bool {
    // SAFETY: Rust 入口提供对齐普通调用栈；生产 yield 统一检查 Task、IRQ 和原子上下文，
    // switch_context 检查 FS Off；汇编恢复调用者 callee-saved，并始终保留目的 CPU 的 tp。
    unsafe { rv_task_context_roundtrip() != 0 }
}

/// 排序 coherent DMA 内存写入；通知设备 MMIO 时还须使用完整设备屏障。
pub(super) fn dma_write_barrier() {
    // SAFETY: 当前内核在 S-mode 执行，fence w,w 仅排序内存写入，不改变寄存器
    // 或地址空间；不声明 nomem/readonly，使编译器同样保留内存排序约束。
    unsafe { asm!("fence w, w", options(nostack)) };
}

/// 在观察设备写入的完成标记后，排序随后 coherent 内存读取。
pub(super) fn dma_read_barrier() {
    // SAFETY: S-mode 可执行 fence r,r；指令不解引用额外地址，汇编保留 memory 副作用。
    unsafe { asm!("fence r, r", options(nostack)) };
}

/// 对 coherent 内存及设备 MMIO 执行完整双向排序。
pub(super) fn dma_barrier() {
    // SAFETY: S-mode 可执行 fence iorw,iorw，覆盖 CPU 内存及设备输入输出访问；
    // 汇编不声明 nomem/readonly，编译器也不能跨该指令移动内存访问。
    unsafe { asm!("fence iorw, iorw", options(nostack)) };
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
            let huge = huge_leaf(va, pa, end);
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
            let leaf = current.add((va >> (12 + 9 * target)) & 511);
            assert_eq!(ptr::read(leaf), 0, "Sv39 leaf mappings overlap");
            ptr::write(leaf, (pa >> 2) | flags | 1);
            let step = if huge { 0x200000 } else { 4096 };
            pa += step;
            va += step;
        }
    }
}
/// 判断当前真实映射前缀能否采用 2MiB 叶项，预算与 PTE 写入共用此规则。
fn huge_leaf(va: usize, pa: usize, end: usize) -> bool {
    (va | pa) & 0x1fffff == 0 && 0x200000 <= end - pa
}

#[derive(Clone, Copy)]
struct BootMapping {
    virtual_start: usize,
    physical: Region,
    flags: usize,
}

const MAX_BOOT_MAPPINGS: usize = 1 + MAX_MEMORY_REGIONS + 2 + MAX_VIRTIO_MMIO + 1 + 4;

/// 在建表之前核对页边界、canonical VA 和所有实际映射的虚拟范围不重叠。
fn add_mapping(
    mappings: &mut [BootMapping; MAX_BOOT_MAPPINGS],
    count: &mut usize,
    virtual_start: usize,
    physical: Region,
    flags: usize,
) {
    assert!(
        physical.start < physical.end
            && (physical.start | physical.end | virtual_start) & 4095 == 0
    );
    let virtual_end = virtual_start
        .checked_add(physical.end - physical.start)
        .expect("Sv39 mapping address overflows");
    for address in [virtual_start, virtual_end - 1] {
        let upper = address >> 39;
        let expected = if address & (1usize << 38) == 0 {
            0
        } else {
            usize::MAX >> 39
        };
        assert_eq!(upper, expected, "Sv39 mapping is not canonical");
    }
    for old in &mappings[..*count] {
        let old_end = old.virtual_start + (old.physical.end - old.physical.start);
        assert!(
            virtual_end <= old.virtual_start || old_end <= virtual_start,
            "Sv39 mappings overlap"
        );
    }
    assert!(*count < mappings.len(), "Sv39 mapping capacity exceeded");
    mappings[*count] = BootMapping {
        virtual_start,
        physical,
        flags,
    };
    *count += 1;
}

/// 将已验证设备范围向外取页边界，仅映射真实 CPU 物理窗口。
fn add_device_mapping(
    mappings: &mut [BootMapping; MAX_BOOT_MAPPINGS],
    count: &mut usize,
    region: Region,
) {
    let physical = Region {
        start: region.start & !4095,
        end: region
            .end
            .checked_add(4095)
            .expect("MMIO page rounding overflows")
            & !4095,
    };
    add_mapping(
        mappings,
        count,
        mmio_address(physical.start),
        physical,
        0xc6,
    );
}

/// 按实际 2MiB/4KiB 叶项计算唯一 L1/L0 表数，64页不足时在写表前明确失败。
fn mapping_table_count(mappings: &[BootMapping]) -> usize {
    let mut branches = [usize::MAX; 64];
    let mut count = 0;
    for mapping in mappings {
        let mut va = mapping.virtual_start;
        let mut pa = mapping.physical.start;
        while pa < mapping.physical.end {
            let huge = huge_leaf(va, pa, mapping.physical.end);
            let keys = [va >> 30, (1usize << 63) | (va >> 21)];
            for &key in &keys[..if huge { 1 } else { 2 }] {
                if !branches[..count].contains(&key) {
                    assert!(count < 63, "Sv39 boot page table pool exhausted");
                    branches[count] = key;
                    count += 1;
                }
            }
            let step = if huge { 0x200000 } else { 4096 };
            va += step;
            pa += step;
        }
    }
    1 + count
}

/// 在 AP 释放前为实际 RAM、全部 MMIO 槽、PCI ECAM 和 MEM ranges 安装最终 PTE。
///
/// # Safety
/// 只能由关闭 IRQ 的 BSP 在使用 bootstrap 表且 AP 尚未释放时调用一次。
/// info 描述真实 QEMU RAM/设备，RAM 覆盖当前栈、CpuLocal、页表池及全部内核资源；
/// 不得与其他页表访问并发。IO ranges 不作为内存或 DMA RAM 映射。
pub unsafe fn finish_boot_mappings(info: &BootInfo) {
    let kernel = Region {
        start: address!(__kernel_phys_start),
        end: address!(__kernel_phys_end)
            .checked_add(4095)
            .expect("kernel page rounding overflows")
            & !4095,
    };
    let empty = BootMapping {
        virtual_start: 0,
        physical: Region { start: 0, end: 0 },
        flags: 0,
    };
    let mut mappings = [empty; MAX_BOOT_MAPPINGS];
    let mut count = 0;
    add_mapping(
        &mut mappings,
        &mut count,
        address!(__kernel_virt_start),
        kernel,
        0xce,
    );
    for &region in &info.memory[..info.memory_count] {
        add_mapping(
            &mut mappings,
            &mut count,
            phys_to_virt(region.start),
            region,
            0xc6,
        );
    }
    for region in [info.interrupts.uart, info.interrupts.controller] {
        add_device_mapping(&mut mappings, &mut count, region);
    }
    for resource in &info.virtio_mmio[..info.virtio_mmio_count] {
        add_device_mapping(&mut mappings, &mut count, resource.registers);
    }
    add_device_mapping(&mut mappings, &mut count, info.pci.configuration);
    for range in &info.pci.ranges[..info.pci.range_count] {
        if matches!(range.flags & 0x0300_0000, 0x0200_0000 | 0x0300_0000) {
            add_device_mapping(&mut mappings, &mut count, range.cpu);
        }
    }
    let required = mapping_table_count(&mappings[..count]);
    // SAFETY: BSP 独占未激活表池，全部 VA/PA 范围及准确表数已经核对；
    // 内核与 RAM 直映保证切表后取指、栈和表遍历有效，AP 只消费完成发布的 SATP。
    unsafe {
        ptr::write_bytes(
            ptr::addr_of_mut!(FINAL_TABLES).cast::<u8>(),
            0,
            size_of::<[PageTable; 64]>(),
        );
        let mut next = 1;
        for mapping in &mappings[..count] {
            map_range(
                mapping.virtual_start,
                mapping.physical.start,
                mapping.physical.end,
                mapping.flags,
                &mut next,
            );
        }
        assert_eq!(next, required, "Sv39 page table budget mismatch");
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

/// 关闭全局 SIE 时复查提示并执行 WFI，随后临时服务 IRQ，返回保持 IRQ 关闭。
///
/// # Safety
/// 仅允许本核 scheduler 的 IRQ-off、无锁且无普通 guard 的最终等待边界调用；
/// 原子必须属于本核并永久有效，trap 与各独立 IRQ 源已经使能；测试计数不得非原子访问。
pub(crate) unsafe fn idle_once(
    kick_pending: &AtomicUsize,
    need_resched: &AtomicBool,
    #[cfg(feature = "ktest")] wait_completions: &AtomicUsize,
) {
    assert!(!irq_enabled(), "idle entry requires IRQ off");
    if kick_pending.load(Ordering::Acquire) == 0 && !need_resched.load(Ordering::Acquire) {
        // SAFETY: 调用者保证 S-mode、本核有效 trap 栈与独立使能的 IRQ 源。
        // WFI 在全局 SIE 关闭时仍由独立使能源的 pending 唤醒，通知不会在检查与等待间被消费。
        unsafe { asm!("wfi", options(nostack)) };
        #[cfg(feature = "ktest")]
        wait_completions.fetch_add(1, Ordering::Release);
    }
    // SAFETY: 调用者保证无锁且可服务 IRQ；无论是否执行 WFI 都开放一次处理 pending，
    // 再关闭 SIE 返回调度段，不恢复任何旧 CPU 的 IRQ 状态。
    unsafe {
        asm!(
            "csrsi sstatus, 2",
            "nop",
            "csrci sstatus, 2",
            options(nostack)
        )
    };
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
/// 初始化所有合法源的 priority，并将固定路由只保留在实际 BSP 的 S context。
///
/// # Safety
/// info 是已发布的实际 QEMU PLIC 描述，设备窗口已映射；source_count 不超过 1024，
/// 所有 S context 的 enable/threshold/claim 槽完整落在窗口内。
/// BSP 必须关闭 IRQ 且 AP 尚未释放，不能与 claim/complete 或 enable 访问并发。
pub unsafe fn external_init(info: &BootInfo) {
    assert!(1 < info.interrupts.source_count && info.interrupts.source_count <= 1024);
    let words = (info.interrupts.source_count as usize).div_ceil(32);
    // SAFETY: 调用者保证实际 context 和已映射窗口；此时独占 enable/threshold/priority，
    // 固定路由中只使用实际合法非零源，屏蔽只改 priority 不撤 enable。
    unsafe {
        for &context in &info.interrupts.plic_contexts[..info.cpu_count] {
            for word in 0..32 {
                mmio_write(0x2000 + context * 0x80 + word * 4, 0);
            }
            mmio_write(0x200000 + context * 0x1000, 0);
        }
        for source in 1..info.interrupts.source_count {
            mmio_write(source as usize * 4, 0);
        }
        for word in 0..words {
            let remaining = info.interrupts.source_count as usize - word * 32;
            let mut bits = if 32 <= remaining {
                u32::MAX
            } else {
                (1u32 << remaining) - 1
            };
            if word == 0 {
                bits &= !1;
            }
            mmio_write(
                0x2000 + info.interrupts.plic_contexts[0] * 0x80 + word * 4,
                bits,
            );
        }
    }
}

/// 将实际源优先级置零，保留固定路由、pending 与已 claim 的完成资格。
pub fn external_mask(source: u32) {
    assert!(0 < source && source < crate::boot::info().interrupts.source_count);
    // SAFETY: 源号与实际窗口已验证，源 descriptor 锁串行化对应独立 priority word。
    unsafe { mmio_write(source as usize * 4, 0) };
}

/// 恢复实际源 priority=1，让屏蔽期间已有 pending 重新参与通知。
pub fn external_unmask(source: u32) {
    assert!(0 < source && source < crate::boot::info().interrupts.source_count);
    // SAFETY: 源 descriptor 锁串行化独立 priority，BSP enable 路由在初始化后保持不变。
    unsafe { mmio_write(source as usize * 4, 1) };
}

/// 从本核真实 PLIC context claim；零表示无待处理源。
pub fn external_claim() -> Option<u32> {
    // SAFETY: 启动入口初始化实际 PLIC，永久 CpuLocal 选择本核 S context；
    // 该 claim 寄存器只由本核 IRQ 流读取。
    let source = unsafe { mmio_read(claim_offset()) };
    if source == 0 {
        None
    } else {
        Some(source)
    }
}

/// 完成本核已 claim 的实际源，不改变 priority 或固定路由。
///
/// # Safety
/// source 必须来自本核尚未完成的真实 claim；调用者持对应 descriptor 锁，
/// 不能跨核、迁移或重复 complete；源号完整落在实际 PLIC 范围内。
pub unsafe fn external_complete(source: u32) {
    assert!(0 < source && source < crate::boot::info().interrupts.source_count);
    // SAFETY: 本核真实 claim 与已验证 S context 对应，固定 enable 保留其完成资格。
    unsafe { mmio_write(claim_offset(), source) };
}
/// 请求 SBI SRST 关机，失败或异常返回均报告。
pub fn shutdown() -> ! {
    sbi(0x53525354, 0, 0, 0, 0).expect("SBI SRST failed");
    panic!("SBI SRST unexpectedly returned");
}
#[cfg(feature = "ktest")]
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
        rv_kernel_fault(frame.sepc, frame.gpr[2], frame.scause, frame.stval);
    }
    rv_dispatch_interrupt(frame.scause, frame.sepc, frame.gpr[2], frame.stval);
}

/// 以真实 S 来源现场保持同步异常的原致命性质，不把内核故障转换成用户 trap。
extern "C" fn rv_kernel_fault(pc: usize, sp: usize, cause: usize, bad: usize) -> ! {
    #[cfg(feature = "ktest")]
    crate::ktest::fatal_event(
        if cause == 2 { "illegal" } else { "panic" },
        format_args!(
            "synchronous trap pc={:#x} sp={:#x} cause={:#x} bad={:#x}",
            pc, sp, cause, bad
        ),
    );
    #[cfg(not(feature = "ktest"))]
    crate::console::fatal(pc, sp, cause, bad);
}

/// 在当前本核 IRQ-off 栈分派真实硬件中断，返回前释放 HardIrqGuard。
fn rv_dispatch_interrupt(cause: usize, pc: usize, sp: usize, bad: usize) {
    let _hard_irq = crate::cpu::HardIrqGuard::enter();
    match cause {
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
        _ => {
            #[cfg(feature = "ktest")]
            crate::ktest::fatal_event(
                "panic",
                format_args!(
                    "unknown interrupt pc={:#x} sp={:#x} cause={:#x} bad={:#x}",
                    pc, sp, cause, bad
                ),
            );
            #[cfg(not(feature = "ktest"))]
            crate::console::fatal(pc, sp, cause, bad)
        }
    }
}

#[cfg(feature = "ktest")]
global_asm!(include_str!("riscv64/registers.S"));
#[cfg(feature = "ktest")]
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

/// 以一次真实本核 timer IRQ 检查指定 FP 模式的原始寄存器图样；RV 没有 FCC，忽略该参数。
///
/// # Safety
/// 本核 trap、timer IRQ 源、永久 CpuLocal 和当前栈必须已安装且可服务中断；调用期间
/// 必须固定 CPU、禁止调度迁移，不能持有 IRQ-save 锁或中断处理器会取得的锁。
/// CPU 必须支持 D，当前不得有活跃 FP 上下文：原汇编恢复 sstatus/FCSR 与整数 ABI
/// 现场，但 fp=true 会覆盖 f0/f31。函数恢复进入时的 IRQ 位；调用者必须在同核
/// IRQ-off 下用 time::reprogram_current 交还生产 deadline（包括断言失败后的清理）。
#[cfg(feature = "ktest")]
pub(crate) unsafe fn ktest_register_pattern(fp: bool, _fcc: u8) -> bool {
    let _irq = crate::sync::IrqSaveGuard::new();
    let counter = &crate::cpu::current().timer_interrupts;
    let expected = counter.load(Ordering::Acquire) + 1;
    program_timer(read_counter() + counter_frequency() / 100);
    // SAFETY: 调用者保证固定本核、真实 timer/trap 已安装、无活跃 FP 和持锁现场；
    // counter 为永久对齐的本核原子，fp 转换仅取 0/1，汇编恢复进入时的 IRQ-off 状态。
    let good = unsafe { rv_register_roundtrip(counter, expected, usize::from(fp)) } != 0;
    good && expected <= counter.load(Ordering::Acquire)
}

/// 在普通 Task 内以两次真实 timer trap 检验整数、SP、TP 与返回路径，不启用 FP。
///
/// # Safety
/// 本核 trap/timer/CpuLocal 已安装；不得持锁或 guard，不得有活跃 FP 现场。
/// 测试临时打开 IRQ 且改写本核 timer，期间不得切换或迁移任务。
#[cfg(feature = "ktest")]
pub(crate) unsafe fn test_integer_register_preservation() -> usize {
    let saved = irq_save();
    let counter = &crate::cpu::current().timer_interrupts;
    let mut rounds = 0;
    for _ in 0..2 {
        let expected = counter.load(Ordering::Acquire) + 1;
        program_timer(read_counter() + counter_frequency() / 1000);
        // SAFETY: 调用者保证已安装 trap/timer，无锁不调度；永久 counter 属于本核。
        // fp=0 只保存并检查整数现场，汇编返回时恢复进入时的 IRQ-off 状态。
        assert!(
            unsafe { rv_register_roundtrip(counter, expected, 0) } != 0,
            "integer timer trap register preservation failed"
        );
        assert!(
            expected <= counter.load(Ordering::Acquire),
            "integer timer trap was not observed"
        );
        rounds += 1;
    }
    crate::time::reprogram_current();
    // SAFETY: saved 来自本函数同核保存；全部汇编已经返回、无持锁借用且未发生迁移。
    unsafe { irq_restore(saved) };
    rounds
}
