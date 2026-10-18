//! TOY 日历只读取真实寄存器，未设置的年月日由时间层判定为无可信 epoch。

use crate::arch::{dma_barrier, mmio_address};
use crate::boot::BootInfo;

/// BSP 独占 RTC 时取得年月日时分秒，有限重采样避免跨年混合两组日历。
///
/// # Safety
/// info 必须是已验证且已发布的实际板资源，RTC 永久强序 DMW 映射已建立；
/// 仅由 IRQ 关闭、AP 尚未释放的 BSP 调用，没有其它 RTC 寄存器访问者。
pub unsafe fn board_rtc_calendar(info: &BootInfo) -> Result<Option<[u32; 6]>, &'static str> {
    let region = info.rtc;
    if region.start == region.end {
        return Ok(None);
    }
    if region.start & 3 != 0 || region.end - region.start < 0x44 {
        return Err("TOY RTC resource window is invalid");
    }
    let base = mmio_address(region.start);
    // SAFETY: 调用契约保证真实 RTC 资源和永久映射，上面的检查覆盖全部 u32 寄存器；
    // BSP 独占控制寄存器，TOY enable 不写日历，不生成默认时间。
    unsafe {
        let control = (base + 0x40) as *mut u32;
        control.write_volatile(control.read_volatile() | (1 << 11));
    }
    dma_barrier();
    for _ in 0..4 {
        // SAFETY: year/read0 寄存器均在已验证且对齐的资源内，只有 BSP 访问；
        // 每组 volatile 读之间设备屏障保证硬件采样顺序，年变化时放弃该组。
        let (year, low, after) = unsafe {
            let year = ((base + 0x30) as *const u32).read_volatile();
            dma_barrier();
            let low = ((base + 0x2c) as *const u32).read_volatile();
            dma_barrier();
            let after = ((base + 0x30) as *const u32).read_volatile();
            (year, low, after)
        };
        if year == after {
            let year = year.checked_add(1900).ok_or("TOY RTC year overflows")?;
            return Ok(Some([
                year,
                (low >> 26) & 63,
                (low >> 21) & 31,
                (low >> 16) & 31,
                (low >> 10) & 63,
                (low >> 4) & 63,
            ]));
        }
    }
    Err("TOY RTC calendar did not stabilize")
}
