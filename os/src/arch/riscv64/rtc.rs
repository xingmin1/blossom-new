//! 板 RTC 只消费固件树声明的 AON 时钟、复位和日历，不写入构建时间。

use crate::arch::{dma_barrier, mmio_address};
use crate::boot::{BootInfo, Region};

/// 读取已映射、完整对齐的设备寄存器，调用者已验证资源来源。
fn read(region: Region, offset: usize) -> u32 {
    assert!(offset % 4 == 0 && offset + 4 <= region.end - region.start);
    // SAFETY: board_rtc_calendar 的调用契约保证实际资源与永久映射，范围检查覆盖完整 u32。
    unsafe { ((mmio_address(region.start) + offset) as *const u32).read_volatile() }
}

/// BSP 独占初始化时写设备寄存器，并在下一次访问前完成设备排序。
fn write(region: Region, offset: usize, value: u32) {
    assert!(offset % 4 == 0 && offset + 4 <= region.end - region.start);
    // SAFETY: 资源来自已验证的 BootInfo，BSP 独占初始化；寄存器偏移及四字节宽度有效。
    unsafe { ((mmio_address(region.start) + offset) as *mut u32).write_volatile(value) };
    dma_barrier();
}

/// 拒绝非十进制 BCD 数位，保留硬件未设置日历的失败状态。
fn bcd(value: u32) -> Result<u32, &'static str> {
    let low = value & 15;
    let high = value >> 4;
    if low < 10 && high < 10 {
        Ok(high * 10 + low)
    } else {
        Err("RTC BCD digit is invalid")
    }
}

/// 读取实际板日历的年月日时分秒；无启用 RTC 时明确没有可信 epoch。
///
/// # Safety
/// 只能由 BSP 在 IRQ 关闭、AP 释放前调用，info 必须是已经发布并验证的真实资源；
/// RTC 与所有时钟/复位 provider 的永久 MMIO 映射必须已建立，且没有其它 RTC 使用者。
pub unsafe fn board_rtc_calendar(info: &BootInfo) -> Result<Option<[u32; 6]>, &'static str> {
    if info.rtc.start == info.rtc.end {
        return Ok(None);
    }
    let setup = &info.board.rtc_setup;
    if setup.clocks.count == 0 || setup.reset_count == 0 {
        return Ok(None);
    }
    for clock in &setup.clocks.entries[..setup.clocks.count] {
        let reference = clock.reference;
        match reference.id {
            10 | 13 => {
                let offset = reference.id as usize * 4;
                write(
                    reference.provider,
                    offset,
                    read(reference.provider, offset) | (1 << 31),
                );
            }
            11 => {
                if read(reference.provider, 44) & 0x00ff_ffff == 0 {
                    return Err("RTC internal clock divider is zero");
                }
            }
            12 => {
                let parent = (read(reference.provider, 48) >> 24) & 15;
                if 1 < parent {
                    return Err("RTC 32K clock parent is invalid");
                }
                if parent == 1 && read(reference.provider, 44) & 0x00ff_ffff == 0 {
                    return Err("RTC selected internal clock divider is zero");
                }
            }
            _ => return Err("RTC clock is outside the supported AON binding"),
        }
    }
    for reset in &setup.resets[..setup.reset_count] {
        if !(5..=7).contains(&reset.id) {
            return Err("RTC reset is outside the AON binding");
        }
        write(
            reset.provider,
            0x38,
            read(reset.provider, 0x38) & !(1 << reset.id),
        );
    }
    let mut ready = false;
    for _ in 0..100_000 {
        if setup.resets[..setup.reset_count]
            .iter()
            .all(|reset| read(reset.provider, 0x3c) & (1 << reset.id) != 0)
        {
            ready = true;
            break;
        }
        core::hint::spin_loop();
    }
    if !ready {
        return Err("RTC AON reset deassertion timed out");
    }
    let config = read(info.rtc, 0);
    write(info.rtc, 0, config | 1 | (1 << 3));
    for _ in 0..4 {
        let before = read(info.rtc, 0x3c);
        let date = read(info.rtc, 0x40);
        let after = read(info.rtc, 0x3c);
        if before == after {
            return Ok(Some([
                2000 + bcd((date >> 11) & 255)?,
                bcd((date >> 6) & 31)?,
                bcd(date & 63)?,
                bcd((before >> 14) & 127)?,
                bcd((before >> 7) & 127)?,
                bcd(before & 127)?,
            ]));
        }
    }
    Err("RTC calendar did not stabilize")
}
