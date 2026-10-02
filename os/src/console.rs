//! 选定串口的低层拥有者；正常访问共享一把端口短锁，故障输出不等待业务锁。

use core::fmt::{self, Write};
use core::hint::spin_loop;
use core::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use crate::sync::SpinLock;

const DATA: usize = 0;
const IER: usize = 1;
const FCR: usize = 2;
const LCR: usize = 3;
const LSR: usize = 5;
const DATA_READY: u8 = 1;
const TRANSMIT_READY: u8 = 0x20;

static PORT: SpinLock<()> = SpinLock::new(());
static RECEIVED: AtomicUsize = AtomicUsize::new(0);
static LAST_BYTE: AtomicU8 = AtomicU8::new(0);

fn register(offset: usize) -> *mut u8 {
    (crate::arch::mmio_address(crate::platform::EARLY_UART_BASE) + offset) as *mut u8
}

fn read_register(offset: usize) -> u8 {
    unsafe { register(offset).read_volatile() }
}

fn write_register(offset: usize, value: u8) {
    unsafe { register(offset).write_volatile(value) };
}

fn put_byte(byte: u8) {
    while read_register(LSR) & TRANSMIT_READY == 0 {
        spin_loop();
    }
    write_register(DATA, byte);
}

struct PortWriter;

impl Write for PortWriter {
    fn write_str(&mut self, text: &str) -> fmt::Result {
        for byte in text.bytes() {
            put_byte(byte);
        }
        Ok(())
    }
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
    PortWriter.write_fmt(arguments).unwrap();
}

/// 开启真实接收中断，源的注册与屏蔽由 IRQ 描述符负责。
pub fn enable_receive_interrupts() {
    let _port = PORT.lock();
    write_register(IER, 1);
}

/// 在真实 IRQ action 中接收有限字节，记录原始数据而非 UTF-8 过滤结果。
///
/// # 安全
/// 只能作为当前 UART 的 IRQ action 调用；此 handler 不使用 context。
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

/// 记录真实致命异常现场并停止，不反取旧用户现场或等待故障核持有的锁。
pub fn fatal(pc: usize, sp: usize, cause: usize, bad_address: usize) -> ! {
    let _ = crate::arch::irq_save();
    let _ = PortWriter.write_fmt(format_args!(
        "FATAL pc={:#x} sp={:#x} cause={:#x} bad={:#x}\n", pc, sp, cause, bad_address,
    ));
    loop {
        crate::arch::wait_for_interrupt();
    }
}

/// 打印内部不变量失败的原始位置和原因，不自动恢复或切回旧内核。
pub fn panic(info: &core::panic::PanicInfo<'_>) -> ! {
    let _ = crate::arch::irq_save();
    let _ = PortWriter.write_fmt(format_args!("FAIL panic: {}\n", info));
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
