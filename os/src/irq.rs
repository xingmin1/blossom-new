//! 当前真实 UART 源的最小 domain/descriptor/flow；回调不在控制器或描述符锁内执行。

use crate::boot::BootInfo;
use crate::sync::SpinLock;
use core::hint::spin_loop;

#[derive(Clone, Copy)]
struct Action {
    handler: unsafe fn(*mut ()),
    context: *mut (),
}

// context 的拥有者在 unregister 同步结束前保活，实际访问由 handler 自行同步。
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

/// 消费已解析的控制器资源，建立唯一 UART 硬件源映射。
pub fn initialize(info: &BootInfo) {
    crate::arch::external_init(info);
    UART.lock().hardware_source = info.interrupts.uart_source;
    crate::console::enable_receive_interrupts();
}

/// 安装当前 UART 源的真实处理者；注册期间源必须已停止并同步退出。
///
/// # 安全
/// context 及 handler 在 unregister_uart 返回前有效，共享访问遵守自身同步。
/// 调用者不能用此函数覆盖仍在执行的 action。
pub unsafe fn register_uart(handler: unsafe fn(*mut ()), context: *mut ()) {
    let mut descriptor = UART.lock();
    assert!(descriptor.action.is_none(), "UART action 尚未移除");
    descriptor.action = Some(Action { handler, context });
}

/// 将已注册的 action 发布到真实硬件源。
pub fn enable_uart() {
    let mut descriptor = UART.lock();
    assert!(descriptor.action.is_some(), "UART action 尚未注册");
    descriptor.enabled = true;
    crate::arch::external_unmask();
}

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

/// 屏蔽源并同步等待当前 handler 退出，等待时不持有 handler 所需锁。
pub fn disable_uart() {
    stop_and_synchronize(false);
}

/// 先撤销 action 发布并屏蔽源，再同步退出；返回后旧 context 可以释放。
pub fn unregister_uart() {
    stop_and_synchronize(true);
}

/// 消费控制器的真实待处理中断；源为空时不猜默认设备号。
pub fn handle_external() {
    while let Some(source) = crate::arch::external_claim() {
        let action = {
            let mut descriptor = UART.lock();
            assert_eq!(source, descriptor.hardware_source, "收到未接入的外部中断源");
            crate::arch::external_mask();
            if descriptor.enabled {
                descriptor.in_flight += 1;
                descriptor.action
            } else {
                None
            }
        };
        if let Some(action) = action {
            unsafe { (action.handler)(action.context) };
        }
        let mut descriptor = UART.lock();
        if action.is_some() {
            descriptor.in_flight -= 1;
        }
        crate::arch::external_complete(source);
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
