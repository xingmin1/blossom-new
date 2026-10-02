//! 选定串口的低层拥有者；正常访问共享一把端口短锁，故障输出不等待业务锁。

use crate::sync::SpinLock;
use core::fmt::{self, Write};
use core::hint::spin_loop;
use core::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

const DATA: usize = 0;
const IER: usize = 1;
const FCR: usize = 2;
const LCR: usize = 3;
const LSR: usize = 5;
const DATA_READY: u8 = 1;
const TRANSMIT_READY: u8 = 0x20;

// 整条应急消息最多读取这些次 LSR，不依赖时钟或其他 CPU 的进度。
const EMERGENCY_POLL_BUDGET: usize = 10_000;

static PORT: SpinLock<()> = SpinLock::new(());
static RECEIVED: AtomicUsize = AtomicUsize::new(0);
static LAST_BYTE: AtomicU8 = AtomicU8::new(0);

/// 取得当前平台固定 UART 的字节寄存器地址，Rust 执行入口始终保留其映射。
fn register(offset: usize) -> *mut u8 {
    (crate::arch::mmio_address(crate::platform::EARLY_UART_BASE) + offset) as *mut u8
}

/// 读取字节寄存器；仅使用本模块定义的硬件偏移。
fn read_register(offset: usize) -> u8 {
    // SAFETY: 入口建立并永久保留选定 UART 映射；offset 来自有效 u8 寄存器常量，
    // 普通访问由 PORT 串行化，故障路径只读取/写入同一硬件字节端口。
    unsafe { register(offset).read_volatile() }
}

/// 写入硬件字节寄存器，不缓存或复制设备状态。
fn write_register(offset: usize, value: u8) {
    // SAFETY: 选定 UART 映射永久有效，offset 是本模块的字节寄存器，u8 无额外对齐。
    unsafe { register(offset).write_volatile(value) };
}

/// 等待真实 TX 空位后写一个字节，保持输出顺序。
fn put_byte(byte: u8) {
    while read_register(LSR) & TRANSMIT_READY == 0 {
        spin_loop();
    }
    write_register(DATA, byte);
}

struct PortWriter;

impl Write for PortWriter {
    /// 按原始 UTF-8 字节输出一个有限字符串，不申请内存。
    fn write_str(&mut self, text: &str) -> fmt::Result {
        for byte in text.bytes() {
            put_byte(byte);
        }
        Ok(())
    }
}

struct EmergencyWriter<R, W> {
    remaining: usize,
    read_status: R,
    write_byte: W,
}

impl<R: FnMut() -> u8, W: FnMut(u8)> Write for EmergencyWriter<R, W> {
    /// 每次状态读取消耗整条消息的共享预算；耗尽后丢弃剩余字节并报告格式化错误。
    fn write_str(&mut self, text: &str) -> fmt::Result {
        for byte in text.bytes() {
            loop {
                if self.remaining == 0 {
                    return Err(fmt::Error);
                }
                self.remaining -= 1;
                if (self.read_status)() & TRANSMIT_READY != 0 {
                    break;
                }
                spin_loop();
            }
            (self.write_byte)(byte);
        }
        Ok(())
    }
}

/// 在栈上建立一次应急输出预算；UART 映射由所有 Rust 引导入口建立并永久保留。
fn emergency_writer() -> impl Write {
    EmergencyWriter {
        remaining: EMERGENCY_POLL_BUDGET,
        read_status: || read_register(LSR),
        write_byte: |byte| write_register(DATA, byte),
    }
}

/// 用可控 LSR 输入验证真实应急 writer 的停止条件和跨字节、跨字符串预算，不访问硬件。
#[cfg(feature = "boot-tests")]
pub fn test_emergency_budget() -> bool {
    let mut reads = 0;
    let mut writes = 0;
    let stalled = {
        let mut writer = EmergencyWriter {
            remaining: EMERGENCY_POLL_BUDGET,
            read_status: || {
                reads += 1;
                0
            },
            write_byte: |_| writes += 1,
        };
        writer.write_str("a").is_err() && writer.write_str("b").is_err()
    };
    if !stalled || reads != EMERGENCY_POLL_BUDGET || writes != 0 {
        return false;
    }

    let mut reads = 0;
    let mut output = [0; 4];
    let mut written = 0;
    let shared = {
        let mut writer = EmergencyWriter {
            remaining: 5,
            read_status: || {
                reads += 1;
                if reads == 1 || reads == 4 {
                    0
                } else {
                    TRANSMIT_READY
                }
            },
            write_byte: |byte| {
                if written < output.len() {
                    output[written] = byte;
                }
                written += 1;
            },
        };
        writer.write_str("ab").is_ok()
            && writer.write_str("cd").is_err()
            && writer.write_str("e").is_err()
    };
    if !shared || reads != 5 || written != 3 || output != *b"abc\0" {
        return false;
    }

    let mut reads = 0;
    let mut output = [0; 4];
    let mut written = 0;
    let ready = {
        let mut writer = EmergencyWriter {
            remaining: 3,
            read_status: || {
                reads += 1;
                TRANSMIT_READY
            },
            write_byte: |byte| {
                if written < output.len() {
                    output[written] = byte;
                }
                written += 1;
            },
        };
        writer.write_str("ab").is_ok() && writer.write_str("cd").is_err()
    };
    ready && reads == 3 && written == 3 && output == *b"abc\0"
}

/// 在 UART 接管时一次设置字节模式与 FIFO，尚不接入 TTY 行规程。
pub fn initialize() {
    let _port = PORT.lock();
    write_register(IER, 0);
    write_register(LCR, 3);
    write_register(FCR, 7);
}

/// 输出一个低频完整信息，不获取任务/PID、分配内存或重新探测硬件。
pub fn write(arguments: fmt::Arguments<'_>) {
    let _port = PORT.lock();
    PortWriter
        .write_fmt(arguments)
        .expect("UART formatting failed");
}

/// 开启真实接收中断，源的注册与屏蔽由 IRQ 描述符负责。
pub fn enable_receive_interrupts() {
    let _port = PORT.lock();
    write_register(IER, 1);
}

/// 在真实 IRQ action 中接收有限字节，记录原始数据而非 UTF-8 过滤结果。
///
/// # Safety
/// 只作为当前 UART 的 IRQ action 调用；端口映射已建立，当前是非嵌套硬中断，
/// 且调用者没有持有 PORT。此 handler 不使用 context。
pub unsafe fn uart_handler(_context: *mut ()) {
    let _port = PORT.lock();
    for _ in 0..16 {
        if read_register(LSR) & DATA_READY == 0 {
            break;
        }
        let byte = read_register(DATA);
        LAST_BYTE.store(byte, Ordering::Relaxed);
        RECEIVED.fetch_add(1, Ordering::Release);
    }
}

/// 返回由真实接收中断消费的字节数量。
pub fn received() -> usize {
    RECEIVED.load(Ordering::Acquire)
}

/// 返回最后一个已经由中断消费的原始字节。
pub fn last_byte() -> u8 {
    LAST_BYTE.load(Ordering::Relaxed)
}

/// 测试仅观察硬件已到达的数据，不从轮询路径消费 RX。
#[cfg(feature = "boot-tests")]
pub fn receive_pending() -> bool {
    let _port = PORT.lock();
    read_register(LSR) & DATA_READY != 0
}

/// 在共享轮询预算内记录真实致命异常现场并停止，不等待故障核持有的锁。
pub fn fatal(pc: usize, sp: usize, cause: usize, bad_address: usize) -> ! {
    let _ = crate::arch::irq_save();
    let _ = emergency_writer().write_fmt(format_args!(
        "FATAL pc={:#x} sp={:#x} cause={:#x} bad={:#x}\n",
        pc, sp, cause, bad_address,
    ));
    loop {
        crate::arch::wait_for_interrupt();
    }
}

/// 在共享轮询预算内打印内部不变量失败的原始位置和原因，随后永久停止当前核。
pub fn panic(info: &core::panic::PanicInfo<'_>) -> ! {
    let _ = crate::arch::irq_save();
    let _ = emergency_writer().write_fmt(format_args!("FAIL panic: {}\n", info));
    loop {
        crate::arch::wait_for_interrupt();
    }
}

/// 使用同一个 UART 端口输出换行信息。
#[macro_export]
macro_rules! println {
    () => { $crate::console::write(format_args!("\n")) };
    ($($argument:tt)*) => {
        $crate::console::write(format_args!("{}\n", format_args!($($argument)*)))
    };
}
