//! 每个固件参数实例重新复制实际 DTB；副本锁不覆盖任何异步服务。

use crate::sync::{SpinLock, SpinLockGuard};
use crate::{arch, boot, platform};
use ::ktest::{Fixture, TestContext, TestError};
use core::convert::{TryFrom, TryInto};
use core::mem::MaybeUninit;

const CAPACITY: usize = 2 * 1024 * 1024;
static COPY_LOCK: SpinLock<()> = SpinLock::new(());
static mut COPY: [u8; CAPACITY] = [0; CAPACITY];
static mut ACTUAL: MaybeUninit<boot::BootInfo> = MaybeUninit::uninit();
static mut EXPECTED: MaybeUninit<boot::BootInfo> = MaybeUninit::uninit();

struct Dtb {
    /// 完整独占容量仅供原地扩展；解析器和旧边界用例只读取实际前缀。
    bytes: &'static mut [u8],
    length: usize,
    actual: &'static mut MaybeUninit<boot::BootInfo>,
    expected: &'static mut MaybeUninit<boot::BootInfo>,
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
            core::slice::from_raw_parts_mut(copy, CAPACITY)
        };
        Ok(Self {
            bytes,
            length,
            // SAFETY: COPY_LOCK同时独占两个永久快照槽；所有解析借用在fixture释放锁前结束。
            actual: unsafe { &mut *core::ptr::addr_of_mut!(ACTUAL) },
            // SAFETY: 此槽与COPY及ACTUAL均不重叠，同一独占锁保持到fixture消费。
            expected: unsafe { &mut *core::ptr::addr_of_mut!(EXPECTED) },
            _exclusive: exclusive,
        })
    }
}

impl Dtb {
    /// 在真实 chosen 中定位 bootargs；缺失时先插入空属性，供同一替换路径填写边界值。
    fn prepare_bootargs(&mut self) -> (usize, usize) {
        let offset = {
            let tree = fdt::Fdt::new(&self.bytes[..self.length]).expect("actual DTB invalid");
            let chosen = tree.find_node("/chosen").expect("actual chosen absent");
            if let Some(property) = chosen.property("bootargs") {
                return (
                    (property.value.as_ptr() as usize)
                        .checked_sub(self.bytes.as_ptr() as usize)
                        .expect("DTB bootargs pointer outside copy"),
                    property.value.len(),
                );
            }
            let first = chosen
                .properties()
                .next()
                .expect("actual chosen properties absent");
            (first.value.as_ptr() as usize)
                .checked_sub(self.bytes.as_ptr() as usize)
                .and_then(|n| n.checked_sub(12))
                .expect("DTB chosen property offset overflow")
        };
        let word = |field: usize| {
            let end = field.checked_add(4).expect("DTB header offset overflow");
            u32::from_be_bytes(self.bytes[..self.length][field..end].try_into().unwrap()) as usize
        };
        let structure = word(8);
        let strings = word(12);
        let strings_size = word(32);
        let structure_size = word(36);
        let structure_end = structure
            .checked_add(structure_size)
            .expect("DTB structure overflow");
        let strings_end = strings
            .checked_add(strings_size)
            .expect("DTB strings overflow");
        let value = offset
            .checked_add(12)
            .expect("DTB inserted property overflow");
        assert_eq!(word(4), self.length);
        assert_eq!(word(offset), 3);
        assert_eq!(structure & 3, 0);
        assert_eq!(structure_size & 3, 0);
        assert_eq!(offset & 3, 0);
        assert!(40 <= structure && word(16) < structure);
        assert!(structure <= offset && value <= structure_end);
        assert!(structure_end <= strings && strings_end <= self.length);

        // 仅支持保留表、结构、字符串依次排列的真实副本；末尾填充可容纳新增属性名。
        let name = b"bootargs\0";
        let name_start = strings_end
            .checked_add(12)
            .expect("DTB property name offset overflow");
        let name_end = name_start
            .checked_add(name.len())
            .expect("DTB property name overflow");
        let total = self
            .length
            .checked_add(12)
            .expect("DTB insertion overflow")
            .max(name_end);
        assert!(total <= self.bytes.len());
        let fields = [
            (4, total),
            (
                12,
                strings
                    .checked_add(12)
                    .expect("DTB strings offset overflow"),
            ),
            (
                32,
                strings_size
                    .checked_add(name.len())
                    .expect("DTB strings size overflow"),
            ),
            (
                36,
                structure_size
                    .checked_add(12)
                    .expect("DTB structure size overflow"),
            ),
            (offset, 3),
            (
                offset
                    .checked_add(4)
                    .expect("DTB property length offset overflow"),
                0,
            ),
            (
                offset
                    .checked_add(8)
                    .expect("DTB property name field overflow"),
                strings_size,
            ),
        ]
        .map(|(field, word)| {
            let end = field.checked_add(4).expect("DTB field write overflow");
            assert!(end <= total);
            (
                field,
                end,
                u32::try_from(word).expect("DTB inserted field exceeds u32"),
            )
        });
        self.bytes.copy_within(offset..self.length, value);
        self.bytes[name_start..name_end].copy_from_slice(name);
        for (field, end, word) in fields {
            self.bytes[field..end].copy_from_slice(&word.to_be_bytes());
        }
        self.length = total;
        (value, 0)
    }

    /// 在独占副本内替换现有属性，移动结构尾部及字符串块后同步全部长度字段。
    fn replace_property(&mut self, offset: usize, old_length: usize, value: &[u8]) {
        let word = |field: usize| {
            let end = field.checked_add(4).expect("DTB header offset overflow");
            u32::from_be_bytes(self.bytes[field..end].try_into().unwrap()) as usize
        };
        let structure = word(8);
        let strings = word(12);
        let structure_size = word(36);
        let structure_end = structure
            .checked_add(structure_size)
            .expect("DTB structure overflow");
        let header = offset
            .checked_sub(12)
            .expect("DTB property header underflow");
        let length_field = offset
            .checked_sub(8)
            .expect("DTB property length underflow");
        assert_eq!(word(4), self.length);
        assert_eq!(word(header), 3);
        assert_eq!(word(length_field), old_length);
        assert!(structure <= header && structure_end <= strings && strings <= self.length);
        assert!(word(16) < structure);
        assert_eq!(offset & 3, 0);
        let old_padded = old_length.checked_add(3).expect("DTB property overflow") & !3;
        let new_padded = value
            .len()
            .checked_add(3)
            .expect("DTB replacement overflow")
            & !3;
        let old_end = offset
            .checked_add(old_padded)
            .expect("DTB old property overflow");
        let new_end = offset
            .checked_add(new_padded)
            .expect("DTB new property overflow");
        assert!(old_end <= structure_end);
        let resized = |size: usize| {
            size.checked_sub(old_padded)
                .and_then(|n| n.checked_add(new_padded))
                .expect("DTB resized block overflow")
        };
        let total = resized(self.length);
        let structure_size = resized(structure_size);
        let strings = resized(strings);
        assert!(total <= self.bytes.len());
        let value_end = offset.checked_add(value.len()).expect("DTB value overflow");
        let fields = [
            (4, u32::try_from(total).expect("DTB total exceeds u32")),
            (12, u32::try_from(strings).expect("DTB strings exceeds u32")),
            (
                36,
                u32::try_from(structure_size).expect("DTB structure exceeds u32"),
            ),
            (
                length_field,
                u32::try_from(value.len()).expect("DTB property exceeds u32"),
            ),
        ];
        self.bytes.copy_within(old_end..self.length, new_end);
        self.bytes[offset..value_end].copy_from_slice(value);
        self.bytes[value_end..new_end].fill(0);
        for (field, value) in fields {
            let end = field.checked_add(4).expect("DTB header write overflow");
            self.bytes[field..end].copy_from_slice(&value.to_be_bytes());
        }
        self.length = total;
    }
}

/// 从完整真实输入在独占槽重新解析，不改写已发表BootInfo或按值返回大型快照。
fn parse<'a>(
    bytes: &[u8],
    output: &'a mut MaybeUninit<boot::BootInfo>,
    bsp: usize,
) -> &'a mut boot::BootInfo {
    let live = boot::info();
    platform::ktest_discover_tree(bytes, live.raw_args, bsp, live.dtb.start, output)
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
        let parsed = parse(&fixture.bytes[..fixture.length], fixture.actual, live.bsp_hardware_id);
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
        let parsed = parse(&fixture.bytes[..fixture.length], fixture.actual, live.bsp_hardware_id);
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
        let parsed = parse(&fixture.bytes[..fixture.length], fixture.actual, live.bsp_hardware_id);
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

    /// FDT 来源的命令行完整保存容量内字节并拒绝超限；不覆盖 LA 原始 a1 命令行。
    #[test]
    #[case("maximum", 4095usize)]
    #[case("overflow", 4096usize)]
    fn bootargs_capacity(context: &mut TestContext<'_>, fixture: &mut Dtb, length: usize) {
        let live = boot::info();
        let (offset, old_length) = fixture.prepare_bootargs();
        let mut command = [0u8; boot::MAX_BOOTARGS_BYTES + 1];
        for (index, byte) in command[..length].iter_mut().enumerate() {
            *byte = b'a' + (index % 26) as u8;
        }
        let value_length = length.checked_add(1).expect("bootargs length overflow");
        fixture.replace_property(offset, old_length, &command[..value_length]);
        assert!(platform::ktest_validate(&fixture.bytes[..fixture.length]).is_ok());
        let result = platform::ktest_discover_tree(
            &fixture.bytes[..fixture.length],
            live.raw_args,
            live.bsp_hardware_id,
            live.dtb.start,
            fixture.actual,
        );
        if length < boot::MAX_BOOTARGS_BYTES {
            let parsed = result.expect("capacity-sized bootargs rejected");
            context.expect_eq(parsed.bootargs(), &command[..length]);
            context.expect_eq(parsed.bootargs_len, length);
            context.expect_eq(parsed.bootargs[length], 0);
        } else {
            assert!(result.is_err(), "oversized bootargs accepted");
            // SAFETY: ktest_discover_tree在任何错误返回前已逐字段初始化此独占槽；
            // result已消费且没有返回的BootInfo借用，当前读取不修改永久启动快照。
            let parsed = unsafe { fixture.actual.assume_init_ref() };
            context.expect_eq(parsed.bootargs_len, 0);
            assert!(parsed.bootargs.iter().all(|byte| *byte == 0));
        }
    }

    /// 真实 RAM 的合法分割覆盖内核及原有资源，分别检查最后合法条目与首个超限条目。
    #[test]
    #[case("32_ranges", 32usize)]
    #[case("33_ranges", 33usize)]
    fn ram_capacity(context: &mut TestContext<'_>, fixture: &mut Dtb, count: usize) {
        let live = boot::info();
        let kernel = boot::kernel_region();
        let ram = *live.memory[..live.memory_count].iter()
            .find(|ram| ram.start <= kernel.start && kernel.end <= ram.end)
            .expect("kernel RAM absent");
        let extra = count.checked_sub(live.memory_count).expect("RAM input already exceeds case");
        let split_bytes = extra.checked_mul(4096).expect("RAM split overflow");
        let mut protected_start = kernel.start;
        let mut protected_end = kernel.end;
        for region in core::iter::once(live.dtb).chain(live.initrd) {
            if ram.overlaps(region) {
                protected_start = protected_start.min(region.start);
                protected_end = protected_end.max(region.end);
            }
        }
        let head = split_bytes <= protected_start.checked_sub(ram.start).expect("RAM start overflow");
        let split = if head {
            ram.start.checked_add(split_bytes).expect("RAM head overflow")
        } else {
            assert!(split_bytes <= ram.end.checked_sub(protected_end).expect("RAM end overflow"));
            ram.end.checked_sub(split_bytes).expect("RAM tail overflow")
        };
        let mut replacement = [0u8; (boot::MAX_MEMORY_REGIONS + 1) * 16];
        let (offset, old_length, replacement_length) = {
            let tree = fdt::Fdt::new(&fixture.bytes[..fixture.length]).expect("actual DTB invalid");
            let (node, tuple) = tree.all_nodes()
                .filter(|node| node.property("device_type").and_then(|p| p.as_str()) == Some("memory"))
                .find_map(|node| {
                    node.raw_reg()?.find(|tuple| {
                        let address = tuple.address.iter().fold(0usize, |n, b| (n << 8) | usize::from(*b));
                        let size = tuple.size.iter().fold(0usize, |n, b| (n << 8) | usize::from(*b));
                        address == ram.start && size == ram.end - ram.start
                    }).map(|tuple| (node, tuple))
                }).expect("kernel RAM reg absent");
            let property = node.property("reg").expect("RAM reg absent");
            let address_bytes = tuple.address.len();
            let size_bytes = tuple.size.len();
            assert!(matches!(address_bytes, 4 | 8) && matches!(size_bytes, 4 | 8));
            let stride = address_bytes.checked_add(size_bytes).expect("RAM tuple overflow");
            let prefix = tuple.address.as_ptr() as usize - property.value.as_ptr() as usize;
            let suffix = prefix.checked_add(stride).expect("RAM suffix overflow");
            let growth = extra.checked_mul(stride).expect("RAM property growth overflow");
            let length = property.value.len().checked_add(growth).expect("RAM property overflow");
            assert!(length <= replacement.len());
            replacement[..prefix].copy_from_slice(&property.value[..prefix]);
            let mut cursor = prefix;
            for index in 0..=extra {
                let region = if head && index == extra {
                    boot::Region { start: split, end: ram.end }
                } else if !head && index == 0 {
                    boot::Region { start: ram.start, end: split }
                } else {
                    let page = if head { index } else { index - 1 };
                    let base = if head { ram.start } else { split };
                    let start = base.checked_add(page.checked_mul(4096).expect("RAM page overflow"))
                        .expect("RAM address overflow");
                    boot::Region { start, end: start.checked_add(4096).expect("RAM size overflow") }
                };
                for (value, width) in [(region.start, address_bytes), (region.end - region.start, size_bytes)] {
                    assert!(width == 8 || u32::try_from(value).is_ok());
                    let end = cursor.checked_add(width).expect("RAM cell overflow");
                    replacement[cursor..end].copy_from_slice(&(value as u64).to_be_bytes()[8 - width..]);
                    cursor = end;
                }
            }
            replacement[cursor..length].copy_from_slice(&property.value[suffix..]);
            (
                property.value.as_ptr() as usize - fixture.bytes.as_ptr() as usize,
                property.value.len(),
                length,
            )
        };
        fixture.replace_property(offset, old_length, &replacement[..replacement_length]);
        assert!(platform::ktest_validate(&fixture.bytes[..fixture.length]).is_ok());
        let result = platform::ktest_discover_tree(
            &fixture.bytes[..fixture.length], live.raw_args, live.bsp_hardware_id,
            live.dtb.start, fixture.actual,
        );
        if count <= boot::MAX_MEMORY_REGIONS {
            let parsed = result.expect("legal RAM split rejected");
            context.expect_eq(parsed.memory_count, count);
            assert!(parsed.memory[..count].iter().any(|r| r.start <= kernel.start && kernel.end <= r.end));
            for (index, region) in parsed.memory[..count].iter().enumerate() {
                assert!(region.start < region.end);
                assert!(live.memory[..live.memory_count].iter()
                    .any(|old| old.start <= region.start && region.end <= old.end));
                assert!(!parsed.memory[..index].iter().any(|old| old.overlaps(*region)));
            }
            context.expect_eq(
                parsed.memory[..count].iter().map(|r| r.end - r.start).sum::<usize>(),
                live.memory[..live.memory_count].iter().map(|r| r.end - r.start).sum::<usize>(),
            );
        } else {
            assert!(result.is_err(), "oversized RAM inventory accepted");
        }
    }

    /// 独占快照连续登记真实 RAM 子区间，容量错误不得改变已发表的前128条记录。
    #[test]
    fn reservation_capacity(context: &mut TestContext<'_>, fixture: &mut Dtb) {
        let live = boot::info();
        let actual = parse(&fixture.bytes[..fixture.length], fixture.actual, live.bsp_hardware_id);
        let expected = parse(&fixture.bytes[..fixture.length], fixture.expected, live.bsp_hardware_id);
        actual.reserved_count = 0;
        let slots = boot::MAX_RESERVED_REGIONS.checked_add(1).expect("reservation count overflow");
        let bytes = slots.checked_mul(4096).expect("reservation size overflow");
        let ram = live.memory[..live.memory_count].iter()
            .find(|ram| bytes <= ram.end - ram.start).expect("reservation RAM absent");
        for index in 0..boot::MAX_RESERVED_REGIONS {
            let start = ram.start.checked_add(index.checked_mul(4096).expect("reservation offset overflow"))
                .expect("reservation address overflow");
            let region = boot::Region { start, end: start.checked_add(4096).expect("reservation end overflow") };
            actual.reserve(region).expect("legal reservation rejected");
            expected.reserved[index] = region;
            context.expect_eq(actual.reserved_count, index + 1);
        }
        expected.reserved_count = boot::MAX_RESERVED_REGIONS;
        context.expect_eq(&actual.reserved[..], &expected.reserved[..]);
        let start = ram.start.checked_add(boot::MAX_RESERVED_REGIONS.checked_mul(4096)
            .expect("last reservation offset overflow")).expect("last reservation address overflow");
        assert!(actual.reserve(boot::Region {
            start, end: start.checked_add(4096).expect("last reservation end overflow"),
        }).is_err());
        context.expect_eq(actual.reserved_count, expected.reserved_count);
        context.expect_eq(&actual.reserved[..], &expected.reserved[..]);
    }
    $($qemu_rtc)*

    /// 所有不完整前缀都必须由生产边界验证拒绝。
    #[test]
    fn truncated_prefixes(_: &mut TestContext<'_>, fixture: &mut Dtb) {
        assert!(platform::ktest_validate(&fixture.bytes[..fixture.length]).is_ok());
        for end in 0..fixture.length {
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
            platform::ktest_validate(&fixture.bytes[..fixture.length]).is_err(),
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
        let actual = parse(&fixture.bytes[..fixture.length], fixture.actual, hardware);
        context.expect_eq(actual.cpu_count, live.cpu_count);
        context.expect_eq(actual.hardware_ids[0], hardware);
        for id in &live.hardware_ids[..live.cpu_count] {
            assert!(actual.hardware_ids[..actual.cpu_count].contains(id));
        }
    }

    /// 只改 CPU reg 保留原 phandle 和控制器描述，正式 parser 必须保稀疏硬件编号及 BSP 重排。
    #[test]
    #[case("first", false)]
    #[case("last", true)]
    fn sparse_hardware_ids(context: &mut TestContext<'_>, fixture: &mut Dtb, last: bool) {
        let live = boot::info();
        let bsp_logical = if last { live.cpu_count - 1 } else { 0 };
        let expected = parse(&fixture.bytes[..fixture.length], fixture.expected, live.hardware_ids[bsp_logical]);
        let mut offsets = [0usize; boot::MAX_CPUS];
        let mut lengths = [0usize; boot::MAX_CPUS];
        {
            let tree = fdt::Fdt::new(&fixture.bytes[..fixture.length]).expect("actual DTB invalid");
            let cpus = tree.find_node("/cpus").expect("actual CPUs absent");
            for node in cpus.children() {
                if node.property("device_type").and_then(|p| p.as_str()) != Some("cpu") {
                    continue;
                }
                let reg = node.property("reg").expect("actual CPU reg absent");
                let hardware = reg.value.iter().fold(0usize, |value, byte| {
                    (value << 8) | usize::from(*byte)
                });
                if let Some(logical) = live.hardware_ids[..live.cpu_count]
                    .iter().position(|id| *id == hardware)
                {
                    offsets[logical] = reg.value.as_ptr() as usize - fixture.bytes.as_ptr() as usize;
                    lengths[logical] = reg.value.len();
                }
            }
        }
        for logical in 0..live.cpu_count {
            let length = lengths[logical];
            assert!(length == 4 || length == 8);
            let hardware = (boot::MAX_CPUS + 1 + logical * 3) as u64;
            let bytes = hardware.to_be_bytes();
            fixture.bytes[offsets[logical]..offsets[logical] + length]
                .copy_from_slice(&bytes[8 - length..]);
        }
        for hardware in &mut expected.hardware_ids[..expected.cpu_count] {
            let logical = live.hardware_ids[..live.cpu_count]
                .iter().position(|id| *id == *hardware).unwrap();
            *hardware = boot::MAX_CPUS + 1 + logical * 3;
        }
        let bsp = boot::MAX_CPUS + 1 + bsp_logical * 3;
        expected.bsp_hardware_id = bsp;
        let actual = parse(&fixture.bytes[..fixture.length], fixture.actual, bsp);
        same_resources(context, &actual, &expected);
        context.expect_eq(actual.bsp_hardware_id, bsp);
        context.expect_eq(actual.hardware_ids[0], bsp);
        assert!(actual.hardware_ids[..actual.cpu_count].iter().all(|id| boot::MAX_CPUS < *id));
        context.diagnostic(format_args!(
            "sparse CPU parser count={} bsp={} last={}",
            actual.cpu_count, bsp, actual.hardware_ids[actual.cpu_count - 1]
        ));
    }

    /// 把真实根 compatible 属性替换为 FDT_NOP 后资源发现仍完整。
    #[test]
    fn nop_property(context: &mut TestContext<'_>, fixture: &mut Dtb) {
        let live = boot::info();
        let hardware = live.hardware_ids[live.cpu_count - 1];
        let original = parse(&fixture.bytes[..fixture.length], fixture.expected, hardware);
        let (start, end) = {
            let tree = fdt::Fdt::new(&fixture.bytes[..fixture.length]).expect("actual DTB invalid");
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
        let actual = parse(&fixture.bytes[..fixture.length], fixture.actual, hardware);
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
            let tree = fdt::Fdt::new(&fixture.bytes[..fixture.length]).expect("actual DTB invalid");
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
            &fixture.bytes[..fixture.length],
            live.raw_args,
            live.bsp_hardware_id,
            live.dtb.start,
            fixture.actual,
        )
        .is_err());
    }
}

#[cfg(not(blossom_platform = "qemu-virt"))]
firmware_cases! {}
