//! 阶段一目标验收：只驱动真实公共底座，协调状态不进入生产 CpuLocal。

use crate::{arch, console, cpu, irq, sync::IrqSaveGuard};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

const MAX: usize = crate::boot::MAX_CPUS;
const PAIR: usize = 1;
const FAN_IN: usize = 2;
const DELAY: usize = 3;
const ARRIVAL: usize = 4;
const UART_REMOVE: usize = 5;
const STOP: usize = 6;
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

fn check(ok: bool, name: &str) {
    if !ok {
        crate::println!("FAIL {} cpu={}", name, cpu::current().id);
        arch::shutdown();
    }
}

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

unsafe fn identity(argument: *mut ()) {
    check(!arch::irq_enabled(), "call-callback-irq-state");
    let payload = &mut *argument.cast::<Payload>();
    HITS[payload.sender].fetch_add(1, Ordering::Relaxed);
    payload.result = cpu::current().id + 0x100;
}

fn call_identity(target: usize) {
    let id = cpu::current().id;
    let before = HITS[id].load(Ordering::Relaxed);
    let mut payload = Payload {
        sender: id,
        result: usize::MAX,
    };
    unsafe { cpu::call(target, identity, (&mut payload as *mut Payload).cast()) };
    check(arch::irq_enabled(), "call-sender-irq-restored");
    check(payload.result == target + 0x100, "call-result-visible");
    check(
        HITS[id].load(Ordering::Relaxed) == before + 1,
        "call-exactly-once",
    );
}

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
    identity(argument);
    check(!RETURNED.load(Ordering::Acquire), "call-parameter-lifetime");
}

fn send_delayed() {
    let before = HITS[1].load(Ordering::Relaxed);
    let mut payload = Payload {
        sender: 1,
        result: usize::MAX,
    };
    unsafe { cpu::call(0, delayed, (&mut payload as *mut Payload).cast()) };
    RETURNED.store(true, Ordering::Release);
    check(payload.result == 0x100, "delayed-result-visible");
    check(
        HITS[1].load(Ordering::Relaxed) == before + 1,
        "delayed-exactly-once",
    );
}

/// 在当前核验证真实异常返回、独立栈、IRQ 保存恢复和自调用。
pub fn local_checks() {
    let local = cpu::current();
    let stack_probe = 0usize;
    let address = &stack_probe as *const usize as usize;
    check(core::ptr::eq(local, cpu::get(local.id)), "cpu-local-base");
    check(
        address < local.stack_top && address >= local.stack_top - crate::cpu::STACK_SIZE,
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
    unsafe { arch::irq_restore(saved) };
    let timer_before = local.timer_interrupts.load(Ordering::Acquire);
    let deadline = arch::read_counter() + arch::counter_frequency() / 1000;
    arch::program_timer(deadline);
    until(
        || local.timer_interrupts.load(Ordering::Acquire) > timer_before,
        "programmed-timer",
    );
    check(arch::read_counter() >= deadline, "timer-not-early");
    let timer_before = local.timer_interrupts.load(Ordering::Acquire);
    check(arch::test_register_preservation(), "register-preservation");
    check(
        local.timer_interrupts.load(Ordering::Acquire) > timer_before,
        "real-timer",
    );
    check(arch::irq_enabled(), "register-test-irq-restored");
    call_identity(local.id);
    let ipi_before = local.ipi_interrupts.load(Ordering::Acquire);
    arch::send_ipi(local.hardware_id);
    until(
        || local.ipi_interrupts.load(Ordering::Acquire) > ipi_before,
        "self-ipi",
    );
    READY[local.id].store(true, Ordering::Release);
}

/// AP 仅在测试构建运行普通执行流，所有同步 call 都在 IRQ callback 之外发起。
pub fn secondary() -> ! {
    local_checks();
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

fn start(phase: usize) {
    ENTERED.store(false, Ordering::Relaxed);
    RETURNED.store(false, Ordering::Relaxed);
    PHASE.store(phase, Ordering::Release);
}

fn join(phase: usize) {
    for id in 1..cpu::count() {
        until(
            || ACK[id].load(Ordering::Acquire) == phase,
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
    fn new(concurrent: bool) -> Self {
        Self {
            concurrent,
            alive: AtomicBool::new(true),
            hits: AtomicUsize::new(0),
            exited: AtomicBool::new(false),
        }
    }
}

unsafe fn uart_action(argument: *mut ()) {
    let context = &*argument.cast::<UartContext>();
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
    console::uart_handler(core::ptr::null_mut());
    context.exited.store(true, Ordering::Release);
}

fn receive(byte: u8, before: usize) {
    until(|| console::received() > before, "uart-receive");
    check(
        console::received() == before + 1 && console::last_byte() == byte,
        "uart-known-byte",
    );
}

fn uart_checks() {
    let mut context = UartContext::new(false);
    irq::disable_uart();
    irq::unregister_uart();
    unsafe { irq::register_uart(uart_action, (&mut context as *mut UartContext).cast()) };
    irq::enable_uart();
    let before = console::received();
    crate::println!("UART READY normal");
    receive(b'U', before);
    check(context.hits.load(Ordering::Acquire) > 0, "uart-real-action");
    crate::println!("PASS uart normal");

    irq::disable_uart();
    let hits = context.hits.load(Ordering::Acquire);
    let before = console::received();
    crate::println!("UART READY masked");
    until(console::receive_pending, "uart-masked-physical-input");
    check(
        console::received() == before && context.hits.load(Ordering::Acquire) == hits,
        "uart-mask-no-handler",
    );
    irq::enable_uart();
    receive(b'M', before);
    crate::println!("PASS uart mask-unmask");
    irq::disable_uart();
    irq::unregister_uart();
    context.alive.store(false, Ordering::Release);

    let mut old = UartContext::new(cpu::count() > 1);
    unsafe { irq::register_uart(uart_action, (&mut old as *mut UartContext).cast()) };
    irq::enable_uart();
    let before = console::received();
    if cpu::count() > 1 {
        start(UART_REMOVE);
    }
    crate::println!("UART READY lifecycle");
    receive(b'L', before);
    if cpu::count() > 1 {
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
    crate::println!("UART READY unregistered");
    until(console::receive_pending, "uart-unregistered-physical-input");
    check(
        console::received() == before && old.hits.load(Ordering::Acquire) == hits,
        "uart-unregister-no-old-access",
    );
    let mut new = UartContext::new(false);
    unsafe { irq::register_uart(uart_action, (&mut new as *mut UartContext).cast()) };
    irq::enable_uart();
    receive(b'R', before);
    check(
        new.hits.load(Ordering::Acquire) > 0 && old.hits.load(Ordering::Acquire) == hits,
        "uart-reregister-new-context",
    );
    irq::disable_uart();
    irq::unregister_uart();
    new.alive.store(false, Ordering::Release);
    crate::println!(
        "PASS uart lifecycle mode={}",
        if cpu::count() > 1 {
            "concurrent"
        } else {
            "local"
        }
    );
}

/// BSP 汇总所有实际核的结果，驱动有限串口协议；独立故障构建只触发真实异常。
pub fn primary() -> ! {
    #[cfg(feature = "fault-test")]
    {
        crate::println!("FAULT READY arch={} cpus={}", arch::NAME, cpu::count());
        arch::trigger_fault();
    }
    #[cfg(not(feature = "fault-test"))]
    {
        crate::println!("TEST READY arch={} cpus={}", arch::NAME, cpu::count());
        check(
            crate::platform::test_input_boundaries(),
            "boot-input-boundaries",
        );
        crate::println!("PASS boot-input");
        local_checks();
        for id in 0..cpu::count() {
            until(|| READY[id].load(Ordering::Acquire), "cpu-local-ready");
            let local = cpu::get(id);
            for other in 0..id {
                check(
                    LOCAL[id].load(Ordering::Relaxed) != LOCAL[other].load(Ordering::Relaxed),
                    "unique-cpu-local",
                );
                check(
                    local.stack_top.abs_diff(cpu::get(other).stack_top) >= crate::cpu::STACK_SIZE,
                    "nonoverlapping-cpu-stacks",
                );
                check(
                    local.hardware_id != cpu::get(other).hardware_id,
                    "unique-hardware-id",
                );
            }
            check(local.id == id, "logical-cpu-id");
            crate::println!(
                "PASS cpu-local cpu={} hw={} stack={:#x} local={:#x}",
                id,
                local.hardware_id,
                STACK[id].load(Ordering::Relaxed),
                LOCAL[id].load(Ordering::Relaxed)
            );
            crate::println!(
                "PASS registers cpu={} timer={}",
                id,
                local.timer_interrupts.load(Ordering::Acquire)
            );
            crate::println!("PASS irq-save cpu={}", id);
            crate::println!("PASS self-call cpu={}", id);
            crate::println!(
                "PASS ipi cpu={} count={}",
                id,
                local.ipi_interrupts.load(Ordering::Acquire)
            );
        }
        if cpu::count() > 1 {
            start(PAIR);
            call_identity(1);
            join(PAIR);
            crate::println!("PASS smp-call pair");
            start(FAN_IN);
            join(FAN_IN);
            crate::println!("PASS smp-call fan-in senders={}", cpu::count() - 1);
            start(DELAY);
            join(DELAY);
            check(
                ENTERED.load(Ordering::Acquire) && RETURNED.load(Ordering::Acquire),
                "delayed-call-ran",
            );
            crate::println!("PASS smp-call delayed");
            if cpu::count() == 4 {
                start(ARRIVAL);
                join(ARRIVAL);
                crate::println!("PASS smp-call processing-arrival");
            } else {
                crate::println!("SCOPE smp-call processing-arrival requires=4");
            }
        } else {
            let before = HITS[0].load(Ordering::Relaxed);
            let mut payload = Payload {
                sender: 0,
                result: usize::MAX,
            };
            unsafe { cpu::call(0, delayed, (&mut payload as *mut Payload).cast()) };
            RETURNED.store(true, Ordering::Release);
            check(
                payload.result == 0x100 && HITS[0].load(Ordering::Relaxed) == before + 1,
                "delayed-self-result",
            );
            crate::println!("PASS smp-call delayed-local");
            crate::println!("SCOPE smp-call local-only");
        }
        uart_checks();
        PHASE.store(STOP, Ordering::Release);
        crate::println!("STAGE1 PASS");
        arch::shutdown();
    }
}
