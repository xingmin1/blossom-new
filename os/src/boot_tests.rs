//! 普通 CPU 执行流驱动的硬件与生命周期自检，测试协调数据独立于 CpuLocal。

use crate::{arch, console, cpu, irq, sync::IrqSaveGuard};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

const MAX: usize = crate::boot::MAX_CPUS;
const PAIR: usize = 1;
const FAN_IN: usize = 2;
const DELAY: usize = 3;
const ARRIVAL: usize = 4;
const UART_REMOVE: usize = 5;
#[cfg(feature = "memory-tests")]
const MEMORY: usize = 6;
const STOP: usize = 7;
static PHASE: AtomicUsize = AtomicUsize::new(0);
static READY: [AtomicBool; MAX] = [const { AtomicBool::new(false) }; MAX];
static ACK: [AtomicUsize; MAX] = [const { AtomicUsize::new(0) }; MAX];
static STACK: [AtomicUsize; MAX] = [const { AtomicUsize::new(0) }; MAX];
static LOCAL: [AtomicUsize; MAX] = [const { AtomicUsize::new(0) }; MAX];
static HITS: [AtomicUsize; MAX] = [const { AtomicUsize::new(0) }; MAX];
static ENTERED: AtomicBool = AtomicBool::new(false);
static RETURNED: AtomicBool = AtomicBool::new(false);
static UART_ENTERED: AtomicBool = AtomicBool::new(false);
static UART_RETURNED: AtomicBool = AtomicBool::new(false);

/// 记录首个真实失败并结束客体，宿主仍须拒绝该次运行。
fn check(ok: bool, name: &str) {
    if !ok {
        crate::test_println!("FAIL {} cpu={}", name, cpu::current().id);
        arch::shutdown();
    }
}

/// 等待指定硬件或并发状态，在实际 counter 截止时间前必须满足条件。
fn until(mut predicate: impl FnMut() -> bool, name: &str) {
    let end = arch::read_counter().wrapping_add(arch::counter_frequency() * 5);
    while !predicate() {
        check(arch::read_counter() < end, name);
        core::hint::spin_loop();
    }
}

struct Payload {
    sender: usize,
    result: usize,
}

/// 在请求目标写入自己的 CPU 身份，并累计发送者对应的真实执行次数。
///
/// # Safety
/// argument 指向发送者独占且已初始化的 Payload，发送者等待 cpu::call 完成，
/// 此动作只在目标非嵌套 IRQ-save 域执行。
unsafe fn identity(argument: *mut ()) {
    check(!arch::irq_enabled(), "call-callback-irq-state");
    // SAFETY: 请求拥有者保持 Payload 地址和独占借用，直到本动作发布完成。
    let payload = unsafe { &mut *argument.cast::<Payload>() };
    HITS[payload.sender].fetch_add(1, Ordering::Relaxed);
    payload.result = cpu::current().id + 0x100;
}

/// 用当前调用者的栈参数验证一次同步请求及结果可见性。
fn call_identity(target: usize) {
    let id = cpu::current().id;
    let before = HITS[id].load(Ordering::Relaxed);
    let mut payload = Payload {
        sender: id,
        result: usize::MAX,
    };
    // SAFETY: 测试编号来自已上线表，本执行流可服务 IPI；payload 在同步返回前保活，
    // 动作独占修改它，完成后的 acquire 为下面的读取建立顺序。
    unsafe { cpu::call(target, identity, (&mut payload as *mut Payload).cast()) };
    check(arch::irq_enabled(), "call-sender-irq-restored");
    check(payload.result == target + 0x100, "call-result-visible");
    check(
        HITS[id].load(Ordering::Relaxed) == before + 1,
        "call-exactly-once",
    );
}

/// 在真实 callback 中暂停，并检查发送者尚未结束参数生命周期。
///
/// # Safety
/// argument 和执行环境满足 identity 的契约，调用者仍在同步等待此请求。
unsafe fn delayed(argument: *mut ()) {
    ENTERED.store(true, Ordering::Release);
    if PHASE.load(Ordering::Acquire) == ARRIVAL {
        // 队列深度来自生产队列本身，证明后续请求已入队，而非猜发送者已运行。
        until(
            || cpu::pending_calls(0) == cpu::count() - 2,
            "call-arrival-queued",
        );
    }
    let end = arch::read_counter() + (arch::counter_frequency() / 500).max(1);
    while arch::read_counter() < end {
        check(
            !RETURNED.load(Ordering::Acquire),
            "call-returned-before-callback",
        );
        core::hint::spin_loop();
    }
    // SAFETY: 此动作继承发送者同一 Payload 的有效性和独占访问条件。
    unsafe { identity(argument) };
    check(!RETURNED.load(Ordering::Acquire), "call-parameter-lifetime");
}

/// 从 AP 发出带有限暂停的动作，以验证 callback 返回后的结果发布。
fn send_delayed() {
    let before = HITS[1].load(Ordering::Relaxed);
    let mut payload = Payload {
        sender: 1,
        result: usize::MAX,
    };
    // SAFETY: BSP 已上线，当前 AP 可处理 IPI；payload 在同步返回前不移动或读取，
    // delayed 仅在目标 IRQ 域修改结果并在完成以前检查存活状态。
    unsafe { cpu::call(0, delayed, (&mut payload as *mut Payload).cast()) };
    RETURNED.store(true, Ordering::Release);
    check(payload.result == 0x100, "delayed-result-visible");
    check(
        HITS[1].load(Ordering::Relaxed) == before + 1,
        "delayed-exactly-once",
    );
}

/// 在当前核验证异常返回、独占栈、IRQ 恢复及本核动作。
///
/// # Safety
/// 本核异常入口、CPU 基址和 timer/IPI 已初始化，在线且 IRQ 开启；
/// 当前为普通执行流，不持有锁，不与另一测试重入同核寄存器检查。
pub unsafe fn local_checks() {
    let local = cpu::current();
    let stack_probe = 0usize;
    let address = &stack_probe as *const usize as usize;
    check(core::ptr::eq(local, cpu::get(local.id)), "cpu-local-base");
    check(
        address < local.stack_top && local.stack_top - crate::cpu::STACK_SIZE <= address,
        "cpu-stack-range",
    );
    check(local.online.load(Ordering::Acquire), "cpu-online");
    STACK[local.id].store(address, Ordering::Relaxed);
    LOCAL[local.id].store(local as *const _ as usize, Ordering::Relaxed);
    check(arch::irq_enabled(), "irq-initial-enabled");
    {
        let outer = IrqSaveGuard::new();
        check(!arch::irq_enabled(), "irq-save-enabled");
        {
            let _inner = IrqSaveGuard::new();
            check(!arch::irq_enabled(), "irq-nested");
        }
        check(!arch::irq_enabled(), "irq-inner-restored-disabled");
        drop(outer);
    }
    check(arch::irq_enabled(), "irq-outer-restored-enabled");
    let saved = arch::irq_save();
    {
        let _outer = IrqSaveGuard::new();
        {
            let _inner = IrqSaveGuard::new();
        }
        check(!arch::irq_enabled(), "irq-initial-disabled-inner");
    }
    check(!arch::irq_enabled(), "irq-initial-disabled-outer");
    // SAFETY: 保存值来自同核 irq_save，此期间仅运行本核嵌套 guard，无迁移或重入。
    unsafe { arch::irq_restore(saved) };
    let timer_before = local.timer_interrupts.load(Ordering::Acquire);
    let deadline = arch::read_counter() + arch::counter_frequency() / 1000;
    arch::program_timer(deadline);
    until(
        || timer_before < local.timer_interrupts.load(Ordering::Acquire),
        "programmed-timer",
    );
    check(deadline <= arch::read_counter(), "timer-not-early");
    let timer_before = local.timer_interrupts.load(Ordering::Acquire);
    // SAFETY: caller 保证本地 IRQ 设施已安装，当前不在 handler 或持锁域，
    // counter 对象永久有效，本核独占寄存器和一次 timer 测试。
    check(
        unsafe { arch::test_register_preservation() },
        "register-preservation",
    );
    check(
        timer_before < local.timer_interrupts.load(Ordering::Acquire),
        "real-timer",
    );
    check(arch::irq_enabled(), "register-test-irq-restored");
    call_identity(local.id);
    let ipi_before = local.ipi_interrupts.load(Ordering::Acquire);
    arch::send_ipi(local.hardware_id);
    until(
        || ipi_before < local.ipi_interrupts.load(Ordering::Acquire),
        "self-ipi",
    );
    READY[local.id].store(true, Ordering::Release);
}

/// 在当前 AP 普通流处理有限阶段；内存测试构建在同一协调入口内完成 MEMORY 动作。
///
/// # Safety
/// 仅由完成本核设施初始化的 AP 入口调用一次，拥有独占内核栈和有效 CpuLocal。
pub unsafe fn secondary() -> ! {
    // SAFETY: AP 引导入口已安装异常/CPU 基址并开启 IRQ，当前执行流不持有锁。
    unsafe { local_checks() };
    let id = cpu::current().id;
    let mut last = 0;
    loop {
        let phase = PHASE.load(Ordering::Acquire);
        if phase == last {
            core::hint::spin_loop();
            continue;
        }
        match phase {
            PAIR => call_identity(id ^ 1),
            FAN_IN => call_identity(0),
            DELAY if id == 1 => send_delayed(),
            ARRIVAL if id == 1 => send_delayed(),
            ARRIVAL => {
                until(
                    || ENTERED.load(Ordering::Acquire),
                    "arrival-callback-entered",
                );
                call_identity(0);
            }
            UART_REMOVE if id == 1 => {
                until(
                    || UART_ENTERED.load(Ordering::Acquire),
                    "uart-handler-entered",
                );
                irq::unregister_uart();
                UART_RETURNED.store(true, Ordering::Release);
            }
            #[cfg(feature = "memory-tests")]
            MEMORY => crate::mm::tests::secondary(),
            STOP => loop {
                arch::wait_for_interrupt();
            },
            DELAY | UART_REMOVE => {}
            _ => check(false, "unknown-test-phase"),
        }
        ACK[id].store(phase, Ordering::Release);
        last = phase;
    }
}

/// 清理当前协调状态并发布下一测试阶段，AP 在 acquire 后执行动作。
fn start(phase: usize) {
    ENTERED.store(false, Ordering::Relaxed);
    RETURNED.store(false, Ordering::Relaxed);
    PHASE.store(phase, Ordering::Release);
}

/// 等待全部实际 AP 完成本阶段，不为未上线 CPU 填入通过结果。
fn join(phase: usize) {
    for acknowledgement in &ACK[1..cpu::count()] {
        until(
            || acknowledgement.load(Ordering::Acquire) == phase,
            "test-phase-completion",
        );
    }
}

struct UartContext {
    concurrent: bool,
    alive: AtomicBool,
    hits: AtomicUsize,
    exited: AtomicBool,
}

impl UartContext {
    /// 构造只在注册至同步注销之间存活的测试 context。
    fn new(concurrent: bool) -> Self {
        Self {
            concurrent,
            alive: AtomicBool::new(true),
            hits: AtomicUsize::new(0),
            exited: AtomicBool::new(false),
        }
    }
}

/// 通过真实 UART 接收并在注销期间检查 context 生命周期。
///
/// # Safety
/// argument 指向尚未同步注销的 UartContext，注册拥有者保持其存活；
/// 本函数由当前 UART IRQ action 调用，未持有端口或描述符锁。
unsafe fn uart_action(argument: *mut ()) {
    // SAFETY: 注册者保持对象到 unregister 返回；这里只读取原子字段和不可变配置。
    let context = unsafe { &*argument.cast::<UartContext>() };
    check(
        context.alive.load(Ordering::Acquire),
        "uart-context-after-unregister",
    );
    context.hits.fetch_add(1, Ordering::Relaxed);
    if context.concurrent {
        UART_ENTERED.store(true, Ordering::Release);
        until(|| !irq::uart_enabled(), "uart-disable-published");
        check(
            !UART_RETURNED.load(Ordering::Acquire),
            "uart-remove-before-handler-exit",
        );
        let end = arch::read_counter() + (arch::counter_frequency() / 500).max(1);
        while arch::read_counter() < end {
            check(
                context.alive.load(Ordering::Acquire),
                "uart-context-in-flight",
            );
            check(
                !UART_RETURNED.load(Ordering::Acquire),
                "uart-disable-not-synchronized",
            );
        }
    }
    // SAFETY: 当前正处于非嵌套 UART action，未持有 PORT；默认 handler 不使用 context。
    unsafe { console::uart_handler(core::ptr::null_mut()) };
    context.exited.store(true, Ordering::Release);
}

/// 等待真实 RX action 消费一个指定字节，不从轮询路径绕过中断处理者。
fn receive(byte: u8, before: usize) {
    until(|| before < console::received(), "uart-receive");
    check(
        console::received() == before + 1 && console::last_byte() == byte,
        "uart-known-byte",
    );
}

/// 验证硬件接收、屏蔽、同步注销和新 context 注册，并在结束前停止 action。
fn uart_checks() {
    let mut context = UartContext::new(false);
    irq::disable_uart();
    irq::unregister_uart();
    // SAFETY: 旧 action 已同步退出；context 在注销前保持栈地址且只通过原子共享访问。
    unsafe { irq::register_uart(uart_action, (&mut context as *mut UartContext).cast()) };
    irq::enable_uart();
    let before = console::received();
    crate::test_println!("UART READY normal");
    receive(b'U', before);
    check(0 < context.hits.load(Ordering::Acquire), "uart-real-action");
    crate::test_println!("PASS uart normal");

    irq::disable_uart();
    let hits = context.hits.load(Ordering::Acquire);
    let before = console::received();
    crate::test_println!("UART READY masked");
    until(console::receive_pending, "uart-masked-physical-input");
    check(
        console::received() == before && context.hits.load(Ordering::Acquire) == hits,
        "uart-mask-no-handler",
    );
    irq::enable_uart();
    receive(b'M', before);
    crate::test_println!("PASS uart mask-unmask");
    irq::disable_uart();
    irq::unregister_uart();
    context.alive.store(false, Ordering::Release);

    let mut old = UartContext::new(1 < cpu::count());
    // SAFETY: 源已停止并同步注销，old 保持有效到本次同步注销完成。
    unsafe { irq::register_uart(uart_action, (&mut old as *mut UartContext).cast()) };
    irq::enable_uart();
    let before = console::received();
    if 1 < cpu::count() {
        start(UART_REMOVE);
    }
    crate::test_println!("UART READY lifecycle");
    receive(b'L', before);
    if 1 < cpu::count() {
        join(UART_REMOVE);
        check(
            UART_RETURNED.load(Ordering::Acquire),
            "uart-unregister-returned",
        );
        check(
            old.exited.load(Ordering::Acquire),
            "uart-unregister-synchronized",
        );
    } else {
        irq::disable_uart();
        irq::unregister_uart();
    }
    old.alive.store(false, Ordering::Release);
    check(!irq::uart_enabled(), "uart-tail-kept-disabled");
    let hits = old.hits.load(Ordering::Acquire);
    let before = console::received();
    crate::test_println!("UART READY unregistered");
    until(console::receive_pending, "uart-unregistered-physical-input");
    check(
        console::received() == before && old.hits.load(Ordering::Acquire) == hits,
        "uart-unregister-no-old-access",
    );
    let mut new = UartContext::new(false);
    // SAFETY: old 已同步退出；new 保持栈地址和有效原子状态直到其注销返回。
    unsafe { irq::register_uart(uart_action, (&mut new as *mut UartContext).cast()) };
    irq::enable_uart();
    receive(b'R', before);
    check(
        0 < new.hits.load(Ordering::Acquire) && old.hits.load(Ordering::Acquire) == hits,
        "uart-reregister-new-context",
    );
    irq::disable_uart();
    irq::unregister_uart();
    new.alive.store(false, Ordering::Release);
    crate::test_println!(
        "PASS uart lifecycle mode={}",
        if 1 < cpu::count() {
            "concurrent"
        } else {
            "local"
        }
    );
}

/// BSP 汇总硬件结果；内存测试构建增加唯一 MEMORY 动作，故障构建只触发指定异常。
///
/// # Safety
/// 仅由 BSP 在所有 CPU 上线后调用一次；本核 IRQ 开启且没有持锁。
/// 普通构建独占平台自检副本和串口生命周期，AP 只运行各自的协作执行流。
pub unsafe fn primary() -> ! {
    #[cfg(feature = "fault-test")]
    {
        crate::test_println!("FAULT READY arch={} cpus={}", arch::NAME, cpu::count());
        #[cfg(feature = "refill-test")]
        arch::trigger_unmapped_fault();
        #[cfg(not(feature = "refill-test"))]
        arch::trigger_fault();
    }
    #[cfg(not(feature = "fault-test"))]
    {
        crate::test_println!("TEST READY arch={} cpus={}", arch::NAME, cpu::count());
        check(console::test_emergency_budget(), "console-emergency-budget");
        crate::test_println!("PASS console-emergency-budget");
        // SAFETY: 只有 BSP 在此执行一次，AP 不访问平台 COPY，实际 DTB 映射仍然保留。
        check(
            unsafe { crate::platform::test_input_boundaries() },
            "boot-input-boundaries",
        );
        crate::test_println!("PASS boot-input");
        // SAFETY: 本核普通执行流、IRQ 开启且没有持锁，资源已由主入口构造。
        unsafe { local_checks() };
        for id in 0..cpu::count() {
            until(|| READY[id].load(Ordering::Acquire), "cpu-local-ready");
            let local = cpu::get(id);
            for (other, other_local) in LOCAL[..id].iter().enumerate() {
                check(
                    LOCAL[id].load(Ordering::Relaxed) != other_local.load(Ordering::Relaxed),
                    "unique-cpu-local",
                );
                check(
                    crate::cpu::STACK_SIZE <= local.stack_top.abs_diff(cpu::get(other).stack_top),
                    "nonoverlapping-cpu-stacks",
                );
                check(
                    local.hardware_id != cpu::get(other).hardware_id,
                    "unique-hardware-id",
                );
            }
            check(local.id == id, "logical-cpu-id");
            crate::test_println!(
                "PASS cpu-local cpu={} hw={} stack={:#x} local={:#x}",
                id,
                local.hardware_id,
                STACK[id].load(Ordering::Relaxed),
                LOCAL[id].load(Ordering::Relaxed)
            );
            crate::test_println!(
                "PASS registers cpu={} timer={}",
                id,
                local.timer_interrupts.load(Ordering::Acquire)
            );
            crate::test_println!("PASS irq-save cpu={}", id);
            crate::test_println!("PASS self-call cpu={}", id);
            crate::test_println!(
                "PASS ipi cpu={} count={}",
                id,
                local.ipi_interrupts.load(Ordering::Acquire)
            );
        }
        if 1 < cpu::count() {
            start(PAIR);
            call_identity(1);
            join(PAIR);
            crate::test_println!("PASS smp-call pair");
            start(FAN_IN);
            join(FAN_IN);
            crate::test_println!("PASS smp-call fan-in senders={}", cpu::count() - 1);
            start(DELAY);
            join(DELAY);
            check(
                ENTERED.load(Ordering::Acquire) && RETURNED.load(Ordering::Acquire),
                "delayed-call-ran",
            );
            crate::test_println!("PASS smp-call delayed");
            if cpu::count() == 4 {
                start(ARRIVAL);
                join(ARRIVAL);
                crate::test_println!("PASS smp-call processing-arrival");
            } else {
                crate::test_println!("SCOPE smp-call processing-arrival requires=4");
            }
        } else {
            let before = HITS[0].load(Ordering::Relaxed);
            let mut payload = Payload {
                sender: 0,
                result: usize::MAX,
            };
            // SAFETY: 单核执行流直接调用，payload 在 IRQ-save callback 完成前保持有效；
            // delayed 的参数与动作条件和远端路径相同。
            unsafe { cpu::call(0, delayed, (&mut payload as *mut Payload).cast()) };
            RETURNED.store(true, Ordering::Release);
            check(
                payload.result == 0x100 && HITS[0].load(Ordering::Relaxed) == before + 1,
                "delayed-self-result",
            );
            crate::test_println!("PASS smp-call delayed-local");
            crate::test_println!("SCOPE smp-call local-only");
        }
        #[cfg(feature = "memory-tests")]
        {
            start(MEMORY);
            // AP 的 MEMORY 返回保证压力页和最后 Arc 权已归还；随后继续串口生命周期阶段。
            crate::mm::tests::primary();
            join(MEMORY);
        }
        uart_checks();
        PHASE.store(STOP, Ordering::Release);
        crate::test_println!("STAGE1 PASS");
        arch::shutdown();
    }
}
