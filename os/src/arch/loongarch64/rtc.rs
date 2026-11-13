//! LoongArch64 TOY（Time of Year）实时时钟（RTC）日历采样模块。
//!
//! ## 模块职责与架构定位
//! 本模块在启动阶段准备 TOY 计数器，并提供运行期只读日历采样，分别供系统墙钟初始化与 RTC 字符设备使用。
//! 启动准备完成后保存设备就绪状态；日历尚未配置时仍由时间层检查原始字段并报告错误。
//! - **下游依赖**：使用 `arch::mmio_address` 将 RTC 物理基址转换为非缓存强序 `DMW0` 虚拟地址，使用 `arch::dma_barrier`（`dbar 0`）保证多次 MMIO 读取之间的严格硬件顺序，并引用 `time::toy` 中定义的寄存器偏移与位域常量。
//!
//! ## 核心设计与 TOY 寄存器布局
//! LoongArch TOY 实时时钟将“年份”和“月/日/时/分/秒”拆分存放在两个独立的 32 位只读寄存器（`TOY_READ1_REG` 与 `TOY_READ0_REG`）中：
//!
//! ```text
//! RTC MMIO 寄存器窗口 (base = mmio_address(info.rtc.start))：
//! +------------------+------------------+-----------------------------------------------+
//! | 偏移量 (Offset)  | 寄存器名称       | 位域结构与含义                                |
//! +------------------+------------------+-----------------------------------------------+
//! | 0x2C             | TOY_READ0_REG    | [31:26] Month  (6 bits, 1..=12)               |
//! |                  |                  | [25:21] Day    (5 bits, 1..=31)               |
//! |                  |                  | [20:16] Hour   (5 bits, 0..=23)               |
//! |                  |                  | [15:10] Minute (6 bits, 0..=59)               |
//! |                  |                  | [ 9:4 ] Second (6 bits, 0..=59)               |
//! |                  |                  | [ 3:0 ] Sub-second / 保留位                   |
//! +------------------+------------------+-----------------------------------------------+
//! | 0x30             | TOY_READ1_REG    | [31:0 ] Year offset (相对于 1900 年的偏移量)  |
//! +------------------+------------------+-----------------------------------------------+
//! | 0x40             | RTC_CTRL_REG     | bit[11] TOY_ENABLE (写 1 使能 TOY 计数器)     |
//! +------------------+------------------+-----------------------------------------------+
//!
//! 防跨年撕裂的一致性重采样流程（最多循环 MAX_CALENDAR_SAMPLES = 4 次）：
//!   read(TOY_READ1_REG) -> year_before
//!        |  (dma_barrier)
//!        v
//!   read(TOY_READ0_REG) -> low (month/day/hour/min/sec)
//!        |  (dma_barrier)
//!        v
//!   read(TOY_READ1_REG) -> year_after
//!   若 year_before == year_after，说明采样期间未发生跨年进位，解包并返回；否则重试。
//! ```
//!
//! ## 同步与并发契约
//! - [`board_rtc_calendar`] 仅由 BSP 在关中断且 AP 尚未释放时准备设备；[`read_rtc_calendar`] 只读寄存器，允许并发采样，但调用期间不得有寄存器配置写入。

use core::sync::atomic::{AtomicBool, Ordering};

use crate::arch::{dma_barrier, mmio_address};
use crate::boot::{BootInfo, Region};
use crate::time::toy::*;

/// 读取日历寄存器时允许的最大一致性重试次数（防止在跨年进位瞬间读到新旧混合的年份与月日时分秒）。
const MAX_CALENDAR_SAMPLES: usize = 4;

/// 原启动准备已完成的 TOY 设备状态，与日历字段是否有效无关。
static BOARD_RTC_READY: AtomicBool = AtomicBool::new(false);

/// 查询板载 TOY 是否已经完成启动准备；无效日历不清除此状态。
pub fn board_rtc_ready() -> bool {
    BOARD_RTC_READY.load(Ordering::Acquire)
}

/// 在主核（BSP）启动阶段使能 TOY 实时时钟并采样一致的 `[年, 月, 日, 时, 分, 秒]` 日历数组。
///
/// - **调用上下文**：由主核在本地中断关闭、从核（AP）尚未唤醒的启动初始化阶段独占调用。
/// - **参数 `info`**：全局启动信息快照，其中 `info.rtc` 描述 RTC 控制器的物理地址区间。
/// - **返回值**：
///   - `Ok(None)`：当前平台未提供 RTC 硬件资源（`info.rtc` 为空区间）。
///   - `Ok(Some([year, month, day, hour, minute, second]))`：成功采样到一组无跨年撕裂的原始日历字段（年份已加上 `1900` 基数）。
///   - `Err(&'static str)`：RTC 资源窗口未按 4 字节对齐或长度不足、年份加法溢出、或连续重采样均遇到跨年跳变。
///
/// # Safety
/// 1. `info` 必须是已通过启动校验的平台启动信息，且非缓存强序直接映射窗口（`DMW0`）必须已建立并覆盖 `info.rtc` 物理地址区间。
/// 2. 调用时必须处于主核（BSP）关中断且从核（AP）尚未启动的单线程环境中，期间不得有任何其他执行流并发访问同一 RTC 控制器的 MMIO 寄存器。
pub unsafe fn board_rtc_calendar(info: &BootInfo) -> Result<Option<[u32; 6]>, &'static str> {
    // Step 1: 检查平台是否声明了 RTC 硬件区间；若存在，校验物理起始地址是否满足 4 字节对齐，且窗口大小足以覆盖最高偏移的 RTC_CTRL_REG (0x40 + 4)。
    let region = info.rtc;
    if region.start == region.end {
        return Ok(None);
    }
    if region.start & (core::mem::align_of::<u32>() - 1) != 0
        || region.end - region.start < RTC_CTRL_REG + core::mem::size_of::<u32>()
    {
        return Err("TOY RTC resource window is invalid");
    }

    // Step 2: 将 RTC 物理基址转换为强序 DMW0 虚拟地址，置位 RTC_CTRL_REG 中的 TOY_ENABLE 位以确保 TOY 计数器处于工作状态。
    let base = mmio_address(region.start);
    // SAFETY:
    // 1. 上方边界检查已确认 region.start 按 4 字节对齐且窗口长度 >= RTC_CTRL_REG + 4，因此 base + RTC_CTRL_REG 是合法且对齐的 32 位 MMIO 寄存器地址。
    // 2. 调用方契约保证了 DMW0 强序映射已建立且主核在关中断状态下独占访问 RTC 控制器。
    unsafe {
        let control = (base + RTC_CTRL_REG) as *mut u32;
        control.write_volatile(control.read_volatile() | TOY_ENABLE);
    }
    dma_barrier();

    // 运行期设备要求 TOY 与晶振均已使能；保留原启动路径的配置与日历错误处理。
    // SAFETY:
    // 1. 上方已验证控制寄存器对齐并位于永久 DMW 映射内；
    // 2. BSP 仍独占设备，此处只读回原准备后的控制状态。
    let enabled = unsafe { ((base + RTC_CTRL_REG) as *const u32).read_volatile() };
    dma_barrier();
    let required = TOY_ENABLE | TOY_32KHZ_ENABLE;
    BOARD_RTC_READY.store(enabled & required == required, Ordering::Release);
    // SAFETY:
    // 1. 上方已验证完整寄存器窗口，调用方保证永久 DMW 映射有效；
    // 2. BSP 已完成 TOY 使能，AP 尚未释放，期间没有寄存器配置写入。
    unsafe { read_rtc_calendar(region).map(Some) }
}

/// 从已准备的 TOY 设备只读采样年、月、日、时、分、秒，不修改控制寄存器。
///
/// # Safety
/// 1. `region` 必须是已永久映射的当前 TOY 设备资源，且启动准备已完成。
/// 2. 采样期间不得配置或复位该设备；其他执行流可以并发读取日历寄存器。
pub unsafe fn read_rtc_calendar(region: Region) -> Result<[u32; 6], &'static str> {
    if region.start & (core::mem::align_of::<u32>() - 1) != 0
        || region.end.saturating_sub(region.start) < TOY_READ1_REG + core::mem::size_of::<u32>()
    {
        return Err("TOY RTC resource window is invalid");
    }
    let base = mmio_address(region.start);
    // 年份寄存器不锁存低字，前后年份一致时才接受整组日历。
    for _ in 0..MAX_CALENDAR_SAMPLES {
        // SAFETY:
        // 1. 上方检查保证两个只读寄存器对齐且位于窗口内，调用方保证永久映射有效；
        // 2. 设备已经准备完成且没有配置写入，屏障保持年、低字、年的读取顺序。
        let (year, low, after) = unsafe {
            let year = ((base + TOY_READ1_REG) as *const u32).read_volatile();
            dma_barrier();
            let low = ((base + TOY_READ0_REG) as *const u32).read_volatile();
            dma_barrier();
            let after = ((base + TOY_READ1_REG) as *const u32).read_volatile();
            (year, low, after)
        };
        // Step 4: 若前后两次读取的年份一致，说明采样期间未发生跨年进位，将年份加上 1900 基准并按位域提取月、日、时、分、秒返回。
        if year == after {
            let year = year
                .checked_add(TOY_YEAR_BASE)
                .ok_or("TOY RTC year overflows")?;
            return Ok([
                year,
                (low >> TOY_MONTH_SHIFT) & TOY_MONTH_MASK,
                (low >> TOY_DAY_SHIFT) & TOY_DAY_MASK,
                (low >> TOY_HOUR_SHIFT) & TOY_HOUR_MASK,
                (low >> TOY_MINUTE_SHIFT) & TOY_MINUTE_MASK,
                (low >> TOY_SECOND_SHIFT) & TOY_SECOND_MASK,
            ]);
        }
    }
    Err("TOY RTC calendar did not stabilize")
}
