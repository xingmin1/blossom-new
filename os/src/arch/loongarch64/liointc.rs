//! 两组 LIOINTC 共享原 IRQ domain；源号按 bank×32 规范化，路由固定到实际 BSP。

use crate::arch::loongarch64::{barrier, mmio_address, EXTERNAL_CONTROLLER};
use crate::boot::{BootInfo, LioIntcResources};

/// 从已验证的 main 窗口读取对齐寄存器，不建立设备内存引用。
fn read(resource: LioIntcResources, offset: usize) -> u32 {
    assert!(offset + 4 <= resource.registers.end - resource.registers.start);
    // SAFETY: BootInfo 已验证窗口与四字节对齐，DMW 保留实际设备映射。
    unsafe { ((mmio_address(resource.registers.start) + offset) as *const u32).read_volatile() }
}

/// 写 enable/disable 等硬件寄存器；调用者持唯一控制器短锁。
fn write(resource: LioIntcResources, offset: usize, value: u32) {
    assert!(offset + 4 <= resource.registers.end - resource.registers.start);
    // SAFETY: main 窗口已验证且永久映射，offset 是硬件定义的对齐寄存器。
    unsafe {
        ((mmio_address(resource.registers.start) + offset) as *mut u32).write_volatile(value)
    };
}

/// BSP 在 AP 释放前屏蔽全部源并发布 level-high 与固定物理核路由。
pub(super) fn initialize(info: &BootInfo) {
    assert_eq!(info.interrupts.source_count, 64);
    assert!(info.bsp_hardware_id < 2);
    let _controller = EXTERNAL_CONTROLLER.lock();
    for resource in info.board.liointc {
        assert!((2..=3).contains(&resource.parent_interrupt));
        write(resource, 0x2c, u32::MAX);
        write(resource, 0x30, 0);
        write(resource, 0x34, 0);
        for source in 0..32 {
            let route =
                (1u8 << (resource.parent_interrupt - 2 + 4)) | (1u8 << info.bsp_hardware_id);
            // SAFETY: 32 个独立字节路由完整位于 main 的前 32 字节，初始化尚无并发访问。
            unsafe {
                ((mmio_address(resource.registers.start) + source) as *mut u8)
                    .write_volatile(route);
            }
        }
    }
    barrier();
}

/// 按规范化 source 定位实际 bank 与单个位；不修改相邻设备。
pub(super) fn set_enabled(source: u32, enabled: bool) {
    assert!(source < 64);
    let _controller = EXTERNAL_CONTROLLER.lock();
    let resource = crate::boot::info().board.liointc[source as usize / 32];
    write(
        resource,
        if enabled { 0x28 } else { 0x2c },
        1u32 << (source % 32),
    );
    barrier();
}

/// 只读取本核 ISR 与真实 enable 状态，保留另一组尚未处理的 level 源。
pub(super) fn claim() -> Option<u32> {
    let info = crate::boot::info();
    let hardware_id = crate::cpu::current().hardware_id;
    assert!(hardware_id < 2);
    let _controller = EXTERNAL_CONTROLLER.lock();
    for (bank, resource) in info.board.liointc.iter().copied().enumerate() {
        let region = resource.isr[hardware_id];
        // SAFETY: 解析边界验证本核 ISR 四字节窗口，永久 DMW 映射且读取无写副作用。
        let pending = unsafe { (mmio_address(region.start) as *const u32).read_volatile() }
            & read(resource, 0x24);
        if pending != 0 {
            return Some(bank as u32 * 32 + pending.trailing_zeros());
        }
    }
    None
}

/// 设备 ISR 已应答且源保持屏蔽时完成设备访问排序；level 源没有独立 W1C 完成寄存器。
pub(super) fn complete(source: u32) {
    assert!(source < 64);
    barrier();
}
