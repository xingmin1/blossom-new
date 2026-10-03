//! 选定串口的低层拥有者；正常访问共享一把端口短锁，故障输出不等待业务锁。

use crate::sync::SpinLock;
use crate::wait::WaitQueue;
use core::fmt::{self, Write};
use core::hint::spin_loop;
use core::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

const DATA: usize = 0;
const IER: usize = 1;
const FCR: usize = 2;
const IIR: usize = 2;
const LCR: usize = 3;
const LSR: usize = 5;
const DATA_READY: u8 = 1;
const TRANSMIT_READY: u8 = 0x20;
const RECEIVE_INTERRUPT: u8 = 1;
const TRANSMIT_INTERRUPT: u8 = 2;
#[cfg(feature = "execution-tests")]
const THRE_INTERRUPT: u8 = 2;
const TX_BUDGET: usize = 16;

// 整条应急消息最多读取这些次 LSR，不依赖时钟或其他 CPU 的进度。
const EMERGENCY_POLL_BUDGET: usize = 10_000;

static PORT: SpinLock<()> = SpinLock::new(());
static TX_WAIT: WaitQueue = WaitQueue::new();
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

/// 引导阶段同步输出，不获取任务/PID、分配内存或重新探测硬件。
pub fn early_write(arguments: fmt::Arguments<'_>) {
    let _port = PORT.lock();
    PortWriter
        .write_fmt(arguments)
        .expect("UART formatting failed");
}

/// 测试协议复用原始同步输出，不将测试标记送入普通日志环。
#[cfg(any(feature = "boot-tests", feature = "execution-tests"))]
pub fn test_write(arguments: fmt::Arguments<'_>) {
    early_write(arguments);
}

/// 在端口短锁内最多提交十六个实际可接受字节，无等待、格式化或换行转换。
pub fn try_write(bytes: &[u8]) -> usize {
    let _port = PORT.lock();
    let mut accepted = 0;
    for &byte in bytes.iter().take(TX_BUDGET) {
        if read_register(LSR) & TRANSMIT_READY == 0 {
            break;
        }
        write_register(DATA, byte);
        accepted += 1;
    }
    #[cfg(feature = "execution-tests")]
    {
        testing::COUNTERS
            .accepted
            .fetch_add(accepted, Ordering::Relaxed);
        if accepted == 0 && !bytes.is_empty() {
            testing::COUNTERS
                .zero_writes
                .fetch_add(1, Ordering::Relaxed);
        }
    }
    accepted
}

/// 返回永久发送等待源；登记节点后才允许使能并复查硬件条件。
pub(crate) fn tx_wait_queue() -> &'static WaitQueue {
    &TX_WAIT
}

/// 登记者在同一端口锁内合并发送使能并重读真实空位，已有空位时撤销无必要通知。
pub(crate) fn arm_tx_interrupt() -> bool {
    let _port = PORT.lock();
    let ier = read_register(IER);
    write_register(IER, ier | TRANSMIT_INTERRUPT);
    let ready = read_register(LSR) & TRANSMIT_READY != 0;
    if ready {
        write_register(IER, ier & !TRANSMIT_INTERRUPT);
    }
    ready
}

/// 开启真实接收中断，源的注册与屏蔽由 IRQ 描述符负责。
pub fn enable_receive_interrupts() {
    let _port = PORT.lock();
    write_register(IER, read_register(IER) | RECEIVE_INTERRUPT);
}

/// 在真实 IRQ action 中接收有限原始字节并确认发送空位，锁外通知等待者。
///
/// # Safety
/// 只作为当前 UART 的 IRQ action 调用；端口映射已建立，当前是非嵌套硬中断，
/// 且调用者没有持有 PORT。此 handler 不使用 context。
pub unsafe fn uart_handler(_context: *mut ()) {
    let notify_tx = {
        let _port = PORT.lock();
        let first_interrupt = read_register(IIR) & 0x0f;
        for _ in 0..16 {
            if read_register(LSR) & DATA_READY == 0 {
                break;
            }
            let byte = read_register(DATA);
            LAST_BYTE.store(byte, Ordering::Relaxed);
            RECEIVED.fetch_add(1, Ordering::Release);
        }
        let last_interrupt = read_register(IIR) & 0x0f;
        let ier = read_register(IER);
        let ready = read_register(LSR) & TRANSMIT_READY != 0;
        let notify_tx = ier & TRANSMIT_INTERRUPT != 0 && ready;
        if notify_tx {
            write_register(IER, ier & !TRANSMIT_INTERRUPT);
            #[cfg(feature = "execution-tests")]
            if first_interrupt == THRE_INTERRUPT || last_interrupt == THRE_INTERRUPT {
                testing::COUNTERS
                    .tx_interrupts
                    .fetch_add(1, Ordering::Relaxed);
            }
        }
        // IIR 读取确认真实 THRE；接收优先级较高时仍以真实 LSR 关闭已满足的 TX 请求。
        let _ = (first_interrupt, last_interrupt);
        notify_tx
    };
    if notify_tx {
        let woken = TX_WAIT.wake_all();
        #[cfg(feature = "execution-tests")]
        testing::COUNTERS.wakes.fetch_add(woken, Ordering::Relaxed);
        let _ = woken;
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

/// 使用唯一 UART 端口输出引导阶段换行信息。
#[macro_export]
macro_rules! early_println {
    () => { $crate::console::early_write(format_args!("\n")) };
    ($($argument:tt)*) => {
        $crate::console::early_write(format_args!("{}\n", format_args!($($argument)*)))
    };
}

/// 使用原始同步串口输出测试协议，不存在普通日志的兼容别名。
#[cfg(any(feature = "boot-tests", feature = "execution-tests"))]
#[macro_export]
macro_rules! test_println {
    () => { $crate::console::test_write(format_args!("\n")) };
    ($($argument:tt)*) => {
        $crate::console::test_write(format_args!("{}\n", format_args!($($argument)*)))
    };
}

/// 仅执行测试观察真实端口活动与原始应急输出，不建立另一套 UART 后端。
#[cfg(feature = "execution-tests")]
pub mod testing {
    /// 测试故障现场复用原始应急消息预算，不取端口锁、时间或任务信息。
    pub fn emergency_write(arguments: fmt::Arguments<'_>) {
        let _ = emergency_writer().write_fmt(arguments);
    }

    use super::*;

    pub(super) struct Counters {
        pub(super) accepted: AtomicUsize,
        pub(super) zero_writes: AtomicUsize,
        pub(super) tx_interrupts: AtomicUsize,
        pub(super) wakes: AtomicUsize,
    }

    pub(super) static COUNTERS: Counters = Counters {
        accepted: AtomicUsize::new(0),
        zero_writes: AtomicUsize::new(0),
        tx_interrupts: AtomicUsize::new(0),
        wakes: AtomicUsize::new(0),
    };

    #[derive(Clone, Copy)]
    pub struct Snapshot {
        pub accepted: usize,
        pub zero_writes: usize,
        pub tx_interrupts: usize,
        pub wakes: usize,
    }

    /// 读取实际接受、拒写、THRE 中断与成功通知计数，不读取或消费 RX。
    pub fn snapshot() -> Snapshot {
        Snapshot {
            accepted: COUNTERS.accepted.load(Ordering::Relaxed),
            zero_writes: COUNTERS.zero_writes.load(Ordering::Relaxed),
            tx_interrupts: COUNTERS.tx_interrupts.load(Ordering::Relaxed),
            wakes: COUNTERS.wakes.load(Ordering::Relaxed),
        }
    }
}
