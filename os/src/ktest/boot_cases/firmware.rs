//! 每个固件参数实例重新复制实际 DTB；副本锁不覆盖任何异步服务。

use crate::sync::{SpinLock, SpinLockGuard};
use crate::{arch, boot, platform};
use ::ktest::{Fixture, TestContext, TestError};

const CAPACITY: usize = 2 * 1024 * 1024;
static COPY_LOCK: SpinLock<()> = SpinLock::new(());
static mut COPY: [u8; CAPACITY] = [0; CAPACITY];

struct Dtb {
    bytes: &'static mut [u8],
    _exclusive: SpinLockGuard<'static, ()>,
}

impl Fixture for Dtb {
    /// 早期 IRQ 关闭且只有 BSP 使用此副本，锁拥有唯一可变借用。
    fn set_up(_: &mut TestContext<'_>, _: &()) -> Result<Self, TestError> {
        let exclusive = COPY_LOCK.lock();
        let info = boot::info();
        let length = info
            .dtb
            .end
            .checked_sub(info.dtb.start)
            .filter(|&n| n <= CAPACITY)
            .ok_or(TestError("DTB copy exceeds capacity"))?;
        // SAFETY: BootInfo 的原始 DTB 已验证并永久保留；目标静态副本不重叠，
        // 独占锁保活到 fixture 消费且任何其他访问 COPY 都取得同一锁。
        let bytes = unsafe {
            let copy = core::ptr::addr_of_mut!(COPY).cast::<u8>();
            core::ptr::copy_nonoverlapping(
                arch::phys_to_virt(info.dtb.start) as *const u8,
                copy,
                length,
            );
            core::slice::from_raw_parts_mut(copy, length)
        };
        Ok(Self {
            bytes,
            _exclusive: exclusive,
        })
    }
}

/// 从完整真实输入重新解析；不改写已发表 BootInfo。
fn parse(bytes: &[u8], bsp: usize) -> boot::BootInfo {
    let live = boot::info();
    platform::ktest_discover_tree(bytes, live.raw_args, bsp, live.dtb.start)
        .expect("actual DTB resource parse failed")
}

/// 比较 DTB 来源资源；LA 寄存器命令行与内核保留项由独立启动步骤取得。
fn same_resources(
    context: &mut TestContext<'_>,
    actual: &boot::BootInfo,
    expected: &boot::BootInfo,
) {
    context.expect_eq(actual.cpu_count, expected.cpu_count);
    context.expect_eq(actual.platform, expected.platform);
    context.expect_eq(actual.initrd, expected.initrd);
    context.expect_eq(actual.uart.reg_io_width, expected.uart.reg_io_width);
    context.expect_eq(actual.uart.reg_shift, expected.uart.reg_shift);
    context.expect_eq(actual.uart.clock_frequency, expected.uart.clock_frequency);
    context.expect_eq(actual.uart.baud, expected.uart.baud);
    context.expect_eq(actual.uart.dw_apb, expected.uart.dw_apb);
    context.expect_eq(
        actual.interrupts.source_count,
        expected.interrupts.source_count,
    );
    context.expect_eq(actual.board.ccache, expected.board.ccache);
    context.expect_eq(actual.board.cache_line_size, expected.board.cache_line_size);
    for (left, right) in actual
        .board
        .liointc
        .iter()
        .zip(expected.board.liointc.iter())
    {
        context.expect_eq(left.registers, right.registers);
        context.expect_eq(left.isr, right.isr);
        context.expect_eq(left.parent_interrupt, right.parent_interrupt);
        context.expect_eq(left.parent_map, right.parent_map);
    }
    context.expect_eq(actual.hardware_ids, expected.hardware_ids);
    context.expect_eq(actual.memory, expected.memory);
    context.expect_eq(actual.memory_count, expected.memory_count);
    context.expect_eq(actual.dtb, expected.dtb);
    context.expect_eq(actual.rtc, expected.rtc);
    context.expect_eq(actual.interrupts.uart, expected.interrupts.uart);
    context.expect_eq(actual.interrupts.controller, expected.interrupts.controller);
    context.expect_eq(
        actual.interrupts.parent_controller,
        expected.interrupts.parent_controller,
    );
    context.expect_eq(
        actual.interrupts.uart_source,
        expected.interrupts.uart_source,
    );
    context.expect_eq(
        actual.interrupts.plic_contexts,
        expected.interrupts.plic_contexts,
    );
    context.expect_eq(actual.pci.configuration, expected.pci.configuration);
    context.expect_eq(actual.pci.range_count, expected.pci.range_count);
    for (left, right) in actual.pci.ranges.iter().zip(&expected.pci.ranges) {
        context.expect_eq(left.flags, right.flags);
        context.expect_eq(left.bus_start, right.bus_start);
        context.expect_eq(left.cpu, right.cpu);
    }
    #[cfg(target_arch = "riscv64")]
    context.expect_eq(actual.bootargs(), expected.bootargs());
    context.expect_eq(actual.counter_frequency, expected.counter_frequency);
}

// 平台选择发生在 suite 注册展开前，公共测试和 QEMU RTC 测试保持原模块路径。
macro_rules! firmware_cases {
    ($($qemu_rtc:tt)*) => {
#[::ktest::suite(context = "Boot", fixture = Dtb, tags("boot", "firmware"), timeout_ms = 10000)]
pub(crate) mod cases {
    use super::*;

    /// 固件原始参数和完整 DTB 的身份来自这次真实启动。
    #[test]
    fn raw_arguments(context: &mut TestContext<'_>, fixture: &mut Dtb) {
        let live = boot::info();
        let parsed = parse(fixture.bytes, live.bsp_hardware_id);
        same_resources(context, &parsed, live);
        context.expect_eq(parsed.raw_args, live.raw_args);
        context.expect_eq(parsed.bsp_hardware_id, live.bsp_hardware_id);
        context.expect_eq(parsed.hardware_ids[0], live.bsp_hardware_id);
        #[cfg(target_arch = "riscv64")]
        {
            context.expect_eq(live.raw_args[0], live.bsp_hardware_id);
            context.expect_eq(live.raw_args[1], live.dtb.start);
        }
        assert_eq!(arch::boot_hardware_id(live.raw_args), live.bsp_hardware_id);
        context.diagnostic(format_args!(
            "firmware bsp={} args={:#x},{:#x},{:#x},{:#x} dtb={:#x}..{:#x}",
            live.bsp_hardware_id,
            live.raw_args[0],
            live.raw_args[1],
            live.raw_args[2],
            live.raw_args[3],
            live.dtb.start,
            live.dtb.end
        ));
    }

    /// RAM、固件保留区和 PCI 地址域直接核对解析结果与永久输入。
    #[test]
    fn resource_regions(context: &mut TestContext<'_>, fixture: &mut Dtb) {
        let live = boot::info();
        let parsed = parse(fixture.bytes, live.bsp_hardware_id);
        same_resources(context, &parsed, live);
        let kernel = boot::kernel_region();
        assert!(live.memory[..live.memory_count]
            .iter()
            .any(|r| r.start <= kernel.start && kernel.end <= r.end));
        assert!(live.reserved[..live.reserved_count].contains(&kernel));
        assert!(live.reserved[..live.reserved_count].contains(&live.dtb));
        if let Some(initrd) = live.initrd {
            assert!(live.reserved[..live.reserved_count].contains(&initrd));
            assert!(live.memory[..live.memory_count]
                .iter()
                .any(|ram| ram.start <= initrd.start && initrd.end <= ram.end));
        }
        assert!(parsed.reserved[..parsed.reserved_count]
            .iter()
            .all(|r| live.reserved[..live.reserved_count].contains(r)));
        for region in &live.memory[..live.memory_count] {
            assert!(region.start < region.end);
            context.diagnostic(format_args!(
                "RAM start={:#x} end={:#x}",
                region.start, region.end
            ));
        }
        for region in &live.reserved[..live.reserved_count] {
            context.diagnostic(format_args!(
                "RESERVED start={:#x} end={:#x}",
                region.start, region.end
            ));
        }
        context.diagnostic(format_args!(
            "PCI config={:#x}..{:#x} windows={}",
            live.pci.configuration.start, live.pci.configuration.end, live.pci.range_count
        ));
        assert!(!live.memory[..live.memory_count]
            .iter()
            .any(|ram| ram.overlaps(live.rtc)));
        context.diagnostic(format_args!(
            "RTC start={:#x} end={:#x}",
            live.rtc.start, live.rtc.end
        ));
        for range in &live.pci.ranges[..live.pci.range_count] {
            context.diagnostic(format_args!(
                "PCI WINDOW flags={:#x} bus={:#x} physical={:#x}..{:#x}",
                range.flags, range.bus_start, range.cpu.start, range.cpu.end
            ));
        }
    }

    /// 板资源消费真实 CPU、控制器和 UART 描述，不把硬件编号当成逻辑数组下标。
    #[test]
    fn platform_resources(context: &mut TestContext<'_>, fixture: &mut Dtb) {
        let live = boot::info();
        let parsed = parse(fixture.bytes, live.bsp_hardware_id);
        same_resources(context, &parsed, live);
        for (logical, hardware) in live.hardware_ids[..live.cpu_count].iter().enumerate() {
            context.expect_eq(crate::cpu::get(logical).hardware_id, *hardware);
            assert!(!live.hardware_ids[..logical].contains(hardware));
        }
        match live.platform {
            boot::Platform::VisionFive2 => {
                assert!(live.hardware_ids[..live.cpu_count]
                    .iter()
                    .all(|id| (1..=4).contains(id)));
                context.expect_eq(live.board.cache_line_size, 64);
                context.expect_eq(live.uart.reg_io_width, 4);
                context.expect_eq(live.uart.reg_shift, 2);
                assert!(live.uart.dw_apb);
                assert!(live.interrupts.source_count <= boot::MAX_INTERRUPT_SOURCES as u32);
            }
            boot::Platform::Ls2k1000la => {
                assert!(live.hardware_ids[..live.cpu_count].iter().all(|id| *id < 2));
                for (bank, resource) in live.board.liointc.iter().enumerate() {
                    context.expect_eq(resource.parent_interrupt, bank as u32 + 2);
                    assert!(0x38 <= resource.registers.end - resource.registers.start);
                }
            }
            boot::Platform::QemuVirt => {
                assert_eq!(live.board.ccache, boot::Region::default());
            }
        }
    }
    $($qemu_rtc)*

    /// 所有不完整前缀都必须由生产边界验证拒绝。
    #[test]
    fn truncated_prefixes(_: &mut TestContext<'_>, fixture: &mut Dtb) {
        assert!(platform::ktest_validate(fixture.bytes).is_ok());
        for end in 0..fixture.bytes.len() {
            assert!(
                platform::ktest_validate(&fixture.bytes[..end]).is_err(),
                "accepted truncated DTB prefix {}",
                end
            );
        }
    }

    /// 单个块字段损坏独立实例，真实总长保持不变。
    #[test]
    #[case("structure_offset", 8usize)]
    #[case("strings_offset", 12usize)]
    #[case("reservation_offset", 16usize)]
    #[case("strings_size", 32usize)]
    #[case("structure_size", 36usize)]
    fn block_boundary(_: &mut TestContext<'_>, fixture: &mut Dtb, field: usize) {
        fixture.bytes[field..field + 4].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(
            platform::ktest_validate(fixture.bytes).is_err(),
            "accepted damaged DTB block {}",
            field
        );
    }

    /// 原输入支持把实际最后一个硬件核作为 BSP，不假设 BSP 硬件编号为零。
    #[test]
    #[case("first", false)]
    #[case("last", true)]
    fn bsp_order(context: &mut TestContext<'_>, fixture: &mut Dtb, last: bool) {
        let live = boot::info();
        let hardware = live.hardware_ids[if last { live.cpu_count - 1 } else { 0 }];
        let actual = parse(fixture.bytes, hardware);
        context.expect_eq(actual.cpu_count, live.cpu_count);
        context.expect_eq(actual.hardware_ids[0], hardware);
        for id in &live.hardware_ids[..live.cpu_count] {
            assert!(actual.hardware_ids[..actual.cpu_count].contains(id));
        }
    }

    /// 把真实根 compatible 属性替换为 FDT_NOP 后资源发现仍完整。
    #[test]
    fn nop_property(context: &mut TestContext<'_>, fixture: &mut Dtb) {
        let live = boot::info();
        let hardware = live.hardware_ids[live.cpu_count - 1];
        let original = parse(fixture.bytes, hardware);
        let (start, end) = {
            let tree = fdt::Fdt::new(fixture.bytes).expect("actual DTB invalid");
            let property = tree
                .find_node("/")
                .and_then(|n| n.property("compatible"))
                .expect("actual root compatible absent");
            let value = property.value.as_ptr() as usize - fixture.bytes.as_ptr() as usize;
            (value - 12, (value + property.value.len() + 3) & !3)
        };
        for token in fixture.bytes[start..end].chunks_exact_mut(4) {
            token.copy_from_slice(&4u32.to_be_bytes());
        }
        let actual = parse(fixture.bytes, hardware);
        same_resources(context, &actual, &original);
    }
}
    };
}

#[cfg(blossom_platform = "qemu-virt")]
firmware_cases! {
    /// RTC 缺失、窗口损坏或覆盖 RAM 均由实际资源发现拒绝，不生成日历默认值。
    #[test]
    #[case("missing", 0u8)]
    #[case("unaligned", 1u8)]
    #[case("short_window", 2u8)]
    #[case("ram_overlap", 3u8)]
    fn rtc_resource_rejection(_: &mut TestContext<'_>, fixture: &mut Dtb, mode: u8) {
        let live = boot::info();
        #[cfg(target_arch = "riscv64")]
        let binding = "google,goldfish-rtc";
        #[cfg(target_arch = "loongarch64")]
        let binding = "loongson,ls7a-rtc";
        let (offset, length) = {
            let tree = fdt::Fdt::new(fixture.bytes).expect("actual DTB invalid");
            let rtc = tree.find_compatible(&[binding]).expect("actual RTC absent");
            let property = rtc
                .property(if mode == 0 { "compatible" } else { "reg" })
                .expect("actual RTC resource absent");
            (
                property.value.as_ptr() as usize - fixture.bytes.as_ptr() as usize,
                property.value.len(),
            )
        };
        if mode == 0 {
            fixture.bytes[offset] = b'x';
        } else {
            assert_eq!(
                length, 16,
                "fixed virt RTC reg is not two address and two size cells"
            );
            match mode {
                1 => fixture.bytes[offset + 7] |= 1,
                2 => fixture.bytes[offset + 8..offset + 16].copy_from_slice(&4u64.to_be_bytes()),
                3 => fixture.bytes[offset..offset + 8]
                    .copy_from_slice(&(live.memory[0].start as u64).to_be_bytes()),
                _ => unreachable!(),
            }
        }
        assert!(platform::ktest_discover_tree(
            fixture.bytes,
            live.raw_args,
            live.bsp_hardware_id,
            live.dtb.start
        )
        .is_err());
    }
}

#[cfg(not(blossom_platform = "qemu-virt"))]
firmware_cases! {}
