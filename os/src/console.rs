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
#[cfg(feature = "ktest")]
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

/// 独立验证一种真实 writer 预算场景，不将多个旧断言组绑定为一个测试实例。
#[cfg(feature = "ktest")]
fn emergency_budget(mode: u8) -> bool {
    match mode {
        0 => {
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
            stalled && reads == EMERGENCY_POLL_BUDGET && writes == 0
        }
        1 => {
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
            shared && reads == 5 && written == 3 && output == *b"abc\0"
        }
        2 => {
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
        _ => false,
    }
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

/// 测试控制者轮询消费 RX；调用前未开放接收中断，或已在同一端口锁下关闭 IER。
/// 已登记的 action 不得在本次控制字节握手期间重新开启 RX，避免与 FIFO 消费竞争。
#[cfg(feature = "ktest")]
pub(crate) fn ktest_read_byte() -> u8 {
    loop {
        let _port = PORT.lock();
        if read_register(LSR) & DATA_READY != 0 {
            return read_register(DATA);
        }
        drop(_port);
        spin_loop();
    }
}

/// 真实 H 已由默认 RX IRQ 消费后，只关闭接收中断，让后续 ACK 留在 FIFO。
/// 同一 fixture 独占控制输入直到 ACK 轮询完成；TX 的实际中断与唤醒路径保持开放。
#[cfg(feature = "ktest")]
pub(crate) fn ktest_disable_receive_interrupts() {
    let _port = PORT.lock();
    write_register(IER, read_register(IER) & !RECEIVE_INTERRUPT);
}

/// 真实故障控制者使用整条消息的有界预算，不等待 PORT 或任何任务进度。
#[cfg(feature = "ktest")]
pub(crate) fn emergency_write(arguments: fmt::Arguments<'_>) {
    let _ = emergency_writer().write_fmt(arguments);
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
    #[cfg(feature = "ktest")]
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

/// 在同步注销 action 前停止 UART 的 RX/TX 中断产生，不消费 FIFO 中的待收字节。
#[cfg(feature = "ktest")]
pub fn disable_interrupts() {
    let _port = PORT.lock();
    write_register(IER, 0);
}

/// 先核对本端口真实 IIR，仅处理自身的有限 RX/TX，并在锁外通知等待者。
///
/// # Safety
/// 只作为当前 UART 的 IRQ action 调用；端口映射已建立，当前是非嵌套硬中断，
/// 且调用者没有持有 PORT。此 handler 不使用 context。
pub unsafe fn uart_handler(_context: *mut ()) -> bool {
    handle_uart::<16, false>()
}

/// 测试 action 仅消费一个真实 payload 字节，在同一端口锁内立即关闭 RX/TX 中断。
///
/// # Safety
/// 仅作为已登记的真实 UART IRQ action 调用，遵守 uart_handler 的入口要求。
/// 原 default action 已停止并同步注销；调用者先轮询消费 H，再开启 RX，且本阶段
/// 仅允许一个 payload 字节，直到轮询 ACK 完成前不得再次开启 UART 中断。
#[cfg(feature = "ktest")]
pub(crate) unsafe fn ktest_uart_handler_one(_context: *mut ()) -> bool {
    handle_uart::<1, true>()
}

/// 两个入口共享实际 IIR/RX/TX 流程；编译期参数使普通入口不承担测试分支或状态成本。
#[inline(always)]
fn handle_uart<const RX_BUDGET: usize, const STOP_RX: bool>() -> bool {
    let notify_tx = {
        let _port = PORT.lock();
        let first_interrupt = read_register(IIR) & 0x0f;
        if first_interrupt & 1 != 0 {
            return false;
        }
        if read_register(IER) & RECEIVE_INTERRUPT != 0 {
            for _ in 0..RX_BUDGET {
                if read_register(LSR) & DATA_READY == 0 {
                    break;
                }
                let byte = read_register(DATA);
                if STOP_RX {
                    // 必须紧跟 DATA 读取并留在 PORT 锁内；不得轮询或读取后续 ACK 字节。
                    write_register(IER, 0);
                }
                LAST_BYTE.store(byte, Ordering::Relaxed);
                RECEIVED.fetch_add(1, Ordering::Release);
            }
        }
        let last_interrupt = read_register(IIR) & 0x0f;
        let ier = read_register(IER);
        let ready = read_register(LSR) & TRANSMIT_READY != 0;
        let notify_tx = ier & TRANSMIT_INTERRUPT != 0 && ready;
        if notify_tx {
            write_register(IER, ier & !TRANSMIT_INTERRUPT);
            #[cfg(feature = "ktest")]
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
        #[cfg(feature = "ktest")]
        testing::COUNTERS.wakes.fetch_add(woken, Ordering::Relaxed);
        let _ = woken;
    }
    true
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
#[cfg(feature = "ktest")]
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

/// 测试观察唯一真实端口活动；ktest 另保存可移动的 default action 责任，不建立第二套驱动。
#[cfg(feature = "ktest")]
pub mod testing {
    /// 测试故障现场复用原始应急消息预算，不取端口锁、时间或任务信息。
    pub fn emergency_write(arguments: fmt::Arguments<'_>) {
        super::emergency_write(arguments);
    }

    use super::*;

    #[cfg(feature = "ktest")]
    static UART_REGISTRATION: SpinLock<Option<crate::irq::IrqRegistration>> = SpinLock::new(None);

    /// 取走唯一 default UART action 的真实 token，不改变设备 IER 或 action 准入。
    ///
    /// 仅 Thread 控制者调用；成功后调用者拥有停止设备、同步注销和归还新 token 的责任。
    /// 空槽返回错误，不凭空创建 token；不得从同线 handler 或持有 PORT 时调用。
    #[cfg(feature = "ktest")]
    pub fn take_uart_registration() -> Result<crate::irq::IrqRegistration, ::ktest::TestError> {
        UART_REGISTRATION.lock().take().ok_or(::ktest::TestError(
            "default UART registration is not installed",
        ))
    }

    /// 首次登记或归还新 default UART token，并开放该真实 action 的准入。
    ///
    /// 传入 token 必须属于当前 UART 的永久 uart_handler/null context；归还前已停止测试
    /// action 并同步注销，UART IER 仍关闭，成功后再开启接收。允许首次登记的 BSP 或
    /// 后续 Thread 控制者调用，不得从同线 handler 或持有其退出所需的 PORT/其他锁调用。
    /// 双拥有时保留原槽责任，拒绝的传入 token 在槽锁外同步注销；不可启用的 token
    /// 同样在锁外同步注销后返回错误。因此任何返回路径都不会遗失活 action 责任。
    #[cfg(feature = "ktest")]
    pub fn restore_uart_registration(
        registration: crate::irq::IrqRegistration,
    ) -> ::ktest::TestResult {
        let mut slot = UART_REGISTRATION.lock();
        let error = if slot.is_some() {
            ::ktest::TestError("default UART registration is already installed")
        } else {
            // enable/is_enabled 仅取短锁，不等待 handler；handler 不访问此拥有者槽。
            registration.enable();
            if registration.is_enabled() {
                *slot = Some(registration);
                return Ok(());
            }
            ::ktest::TestError("default UART registration cannot be enabled")
        };
        drop(slot);
        registration.unregister_sync();
        Err(error)
    }

    /// 独立检查真实应急 writer 的预算：0 停滞，1 跨字符串共享，2 恒可发送。
    /// 这是确定性 writer 算法验收，不伪称 UART 硬件 IRQ 或硬件 LSR。
    #[cfg(feature = "ktest")]
    pub(crate) fn emergency_budget(mode: u8) -> bool {
        super::emergency_budget(mode)
    }

    #[cfg(feature = "ktest")]
    pub(super) struct Counters {
        pub(super) accepted: AtomicUsize,
        pub(super) zero_writes: AtomicUsize,
        pub(super) tx_interrupts: AtomicUsize,
        pub(super) wakes: AtomicUsize,
    }

    #[cfg(feature = "ktest")]
    pub(super) static COUNTERS: Counters = Counters {
        accepted: AtomicUsize::new(0),
        zero_writes: AtomicUsize::new(0),
        tx_interrupts: AtomicUsize::new(0),
        wakes: AtomicUsize::new(0),
    };

    #[cfg(feature = "ktest")]
    #[derive(Clone, Copy)]
    pub struct Snapshot {
        pub accepted: usize,
        pub zero_writes: usize,
        pub tx_interrupts: usize,
        pub wakes: usize,
    }

    /// 读取实际接受、拒写、THRE 中断与成功通知计数，不读取或消费 RX。
    #[cfg(feature = "ktest")]
    pub fn snapshot() -> Snapshot {
        Snapshot {
            accepted: COUNTERS.accepted.load(Ordering::Relaxed),
            zero_writes: COUNTERS.zero_writes.load(Ordering::Relaxed),
            tx_interrupts: COUNTERS.tx_interrupts.load(Ordering::Relaxed),
            wakes: COUNTERS.wakes.load(Ordering::Relaxed),
        }
    }
}
