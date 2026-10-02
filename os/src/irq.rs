//! 当前真实 UART 源的最小 domain/descriptor/flow；回调不在控制器或描述符锁内执行。

use crate::boot::BootInfo;
use crate::sync::SpinLock;
use core::hint::spin_loop;

#[derive(Clone, Copy)]
struct Action {
    handler: unsafe fn(*mut ()),
    context: *mut (),
}

// SAFETY: 注册者保证 context 至同步注销返回仍有效；访问由 handler 协议同步。
unsafe impl Send for Action {}

struct Descriptor {
    hardware_source: u32,
    enabled: bool,
    action: Option<Action>,
    in_flight: usize,
}

static UART: SpinLock<Descriptor> = SpinLock::new(Descriptor {
    hardware_source: 0,
    enabled: false,
    action: None,
    in_flight: 0,
});

/// 在 BSP 初始化真实控制器并建立当前 UART 源映射，接收 action 尚未启用。
///
/// # Safety
/// info 是已发布的实际设备资源，MMIO/IOCSR 映射有效；本核 IRQ 关闭，
/// AP 尚未处理此源，且该 domain 仅初始化一次。
pub unsafe fn initialize(info: &BootInfo) {
    // SAFETY: caller 保证实际资源和映射；本核在源未启用时独占路由初始化。
    unsafe { crate::arch::external_init(info) };
    UART.lock().hardware_source = info.interrupts.uart_source;
    crate::console::enable_receive_interrupts();
}

/// 安装当前 UART 源的真实处理者；注册期间源必须已停止并同步退出。
///
/// # Safety
/// context 及 handler 在 unregister_uart 返回前有效，共享访问遵守自身同步。
/// 拥有者串行化注册、启用和注销；注册前源已停止且所有旧 handler 已退出。
/// handler 不得调用本源的同步停止/注销，也不得等待调用者持有的资源。
pub unsafe fn register_uart(handler: unsafe fn(*mut ()), context: *mut ()) {
    let mut descriptor = UART.lock();
    assert!(
        descriptor.action.is_none(),
        "UART action is still registered"
    );
    descriptor.action = Some(Action { handler, context });
}

/// 将已注册的 action 发布到真实硬件源。
pub fn enable_uart() {
    let mut descriptor = UART.lock();
    assert!(
        descriptor.action.is_some(),
        "UART action has not been registered"
    );
    descriptor.enabled = true;
    crate::arch::external_unmask();
}

/// 在同一锁域停止源并按需要取消发布，锁外等待已借出动作结束。
/// 由生命周期拥有者在普通执行流调用，不持有 handler 退出所需的锁。
fn stop_and_synchronize(remove_action: bool) {
    {
        let mut descriptor = UART.lock();
        descriptor.enabled = false;
        crate::arch::external_mask();
        if remove_action {
            // 先撤销发布，防止同步等待期间再次启用旧 action。
            descriptor.action = None;
        }
    }
    while UART.lock().in_flight != 0 {
        spin_loop();
    }
}

/// 屏蔽源并等待当前 handler 退出；不得从此源 handler 内调用或持有它需要的锁。
pub fn disable_uart() {
    stop_and_synchronize(false);
}

/// 撤销 action、屏蔽源并同步退出；不得从此源 handler 调用，返回后旧 context 可释放。
pub fn unregister_uart() {
    stop_and_synchronize(true);
}

/// 消费控制器的真实待处理中断；源为空时不猜默认设备号。
pub fn handle_external() {
    while let Some(source) = crate::arch::external_claim() {
        let action = {
            let mut descriptor = UART.lock();
            assert_eq!(
                source, descriptor.hardware_source,
                "unsupported external interrupt source"
            );
            crate::arch::external_mask();
            if descriptor.enabled {
                descriptor.in_flight += 1;
                descriptor.action
            } else {
                None
            }
        };
        if let Some(action) = action {
            // SAFETY: 描述符已计入 in_flight；注册者保持动作/参数直到同步结束，
            // 此处没有持有描述符或控制器锁，允许动作完成其设备访问。
            unsafe { (action.handler)(action.context) };
        }
        let mut descriptor = UART.lock();
        if action.is_some() {
            descriptor.in_flight -= 1;
        }
        // SAFETY: source 来自本核真实 claim 且已核对 UART 映射；当前持描述符锁，
        // action 已退出，硬件完成和是否重新开放以同一 enabled 状态决定。
        unsafe { crate::arch::external_complete(source) };
        // 处理者退出期间注销核可能已关闭源；只有当前启用状态允许重新开放。
        if descriptor.enabled {
            crate::arch::external_unmask();
        }
    }
}

/// 测试读取唯一描述符状态，以确定同步停止的真实发布点。
#[cfg(feature = "boot-tests")]
pub fn uart_enabled() -> bool {
    UART.lock().enabled
}
