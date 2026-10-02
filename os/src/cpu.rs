//! 固定在线 CPU 的执行基础；请求节点由同步调用者拥有，完成前不离开其栈。

use core::cell::UnsafeCell;
use core::hint::spin_loop;
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::boot::{BootInfo, MAX_CPUS};
use crate::sync::{IrqSaveGuard, SpinLock};

pub const STACK_SIZE: usize = 64 * 1024;

/// 链接期保留的独立启动栈，不由旧页分配器申请。
#[repr(C, align(16))]
#[derive(Clone, Copy)]
pub struct BootStack(pub [u8; STACK_SIZE]);

/// 汇编主辅入口共同使用的四份静态栈。
#[no_mangle]
#[link_section = ".boot.stacks"]
#[used]
pub static mut BOOT_STACKS: [BootStack; MAX_CPUS] = [BootStack([0; STACK_SIZE]); MAX_CPUS];

/// AP 消费的不可变启动记录；记录和所指资源在本阶段始终保留。
#[repr(C)]
#[derive(Clone, Copy)]
pub struct CpuBoot {
    pub stack_top: usize,
    pub local: usize,
}

struct Call {
    callback: unsafe fn(*mut ()),
    argument: *mut (),
    next: UnsafeCell<*mut Call>,
    done: AtomicBool,
}

struct CallQueue {
    head: *mut Call,
    tail: *mut Call,
}

// 指针只在所属队列的短锁内修改，调用者保持节点存活直到完成确认。
unsafe impl Send for CallQueue {}

impl CallQueue {
    const fn new() -> Self {
        Self { head: ptr::null_mut(), tail: ptr::null_mut() }
    }

    unsafe fn push(&mut self, call: *mut Call) {
        if self.tail.is_null() {
            self.head = call;
        } else {
            *(*self.tail).next.get() = call;
        }
        self.tail = call;
    }

    unsafe fn pop(&mut self) -> Option<*mut Call> {
        let call = self.head;
        if call.is_null() {
            return None;
        }
        self.head = *(*call).next.get();
        if self.head.is_null() {
            self.tail = ptr::null_mut();
        }
        Some(call)
    }
}

/// 一个 CPU 的不可变身份和真正需要同步的执行状态。
pub struct CpuLocal {
    pub id: usize,
    pub hardware_id: usize,
    pub stack_top: usize,
    pub online: AtomicBool,
    pub timer_interrupts: AtomicUsize,
    pub ipi_interrupts: AtomicUsize,
    calls: SpinLock<CallQueue>,
}

impl CpuLocal {
    const fn new(id: usize, hardware_id: usize, stack_top: usize) -> Self {
        Self {
            id, hardware_id, stack_top,
            online: AtomicBool::new(false),
            timer_interrupts: AtomicUsize::new(0),
            ipi_interrupts: AtomicUsize::new(0),
            calls: SpinLock::new(CallQueue::new()),
        }
    }
}

struct CpuStorage(UnsafeCell<[CpuLocal; MAX_CPUS]>);
// BSP 在发布 AP 前构造表；随后身份不可变，其余共享状态使用原子或短锁。
unsafe impl Sync for CpuStorage {}

static CPU_LOCALS: CpuStorage = CpuStorage(UnsafeCell::new([const { CpuLocal::new(0, 0, 0) }; MAX_CPUS]));
static mut BOOT_RECORDS: [CpuBoot; MAX_CPUS] = [CpuBoot { stack_top: 0, local: 0 }; MAX_CPUS];

/// 消费一次启动 CPU 输入，并安装 BSP 的真实每核基址。
///
/// # 安全
/// 只能由 BSP 在 AP 尚未发布、IRQ 尚未启用时执行一次。
pub unsafe fn initialize(info: &BootInfo) {
    let locals = CPU_LOCALS.0.get().cast::<CpuLocal>();
    for id in 0..info.cpu_count {
        let stack_top = ptr::addr_of!(BOOT_STACKS).cast::<BootStack>().add(id) as usize + STACK_SIZE;
        let local = locals.add(id);
        ptr::write(local, CpuLocal::new(id, info.hardware_ids[id], stack_top));
        ptr::addr_of_mut!(BOOT_RECORDS).cast::<CpuBoot>().add(id).write(CpuBoot { stack_top, local: local as usize });
    }
    crate::arch::install_cpu_local(locals);
}

/// 返回当前执行核的稳定身份；初始化顺序保证基址已安装。
pub fn current() -> &'static CpuLocal {
    unsafe { &*crate::arch::cpu_local() }
}

/// 返回已解析的固定 CPU 数量。
pub fn count() -> usize {
    crate::boot::info().cpu_count
}

/// 返回有效逻辑编号的 CPU 对象，调用者使用已建立的 CPU 表编号。
pub fn get(id: usize) -> &'static CpuLocal {
    unsafe { &*CPU_LOCALS.0.get().cast::<CpuLocal>().add(id) }
}

/// 在当前核的异常和本地中断设施就绪后发布上线。
pub fn publish_online() {
    current().online.store(true, Ordering::Release);
}

/// 串行启动实际 AP，并等待其可服务中断；失败不改写请求核数或释放记录。
pub fn start_secondaries() {
    for id in 1..count() {
        let local = get(id);
        let record = unsafe { ptr::addr_of!(BOOT_RECORDS).cast::<CpuBoot>().add(id) as usize };
        let physical = crate::arch::virt_to_phys(record);
        if let Err(error) = crate::arch::start_cpu(local.hardware_id, physical) {
            panic!("AP 启动请求失败：硬件ID={}，错误={}", local.hardware_id, error);
        }
        let deadline = crate::arch::read_counter() + crate::boot::info().counter_frequency * 5;
        while !local.online.load(Ordering::Acquire) {
            if crate::arch::read_counter() >= deadline {
                panic!("AP 上线超时：硬件ID={}", local.hardware_id);
            }
            spin_loop();
        }
    }
}

/// 记录真实本地 timer 中断；硬件确认由架构 handler 完成。
pub fn handle_timer() {
    current().timer_interrupts.fetch_add(1, Ordering::Relaxed);
}

/// 在硬件通知已清除后取出真实请求，锁外执行并发布完成。
pub fn handle_ipi() {
    let local = current();
    local.ipi_interrupts.fetch_add(1, Ordering::Relaxed);
    loop {
        let request = unsafe { local.calls.lock().pop() };
        let request = match request {
            Some(request) => request,
            None => break,
        };
        unsafe {
            ((*request).callback)((*request).argument);
            (*request).done.store(true, Ordering::Release);
        }
    }
}

/// 在目标 CPU 上同步执行一次短动作，真正完成后才归还参数拥有者。
///
/// # 安全
/// target 必须是在线表中的逻辑编号；callback 和 argument 在返回前保持有效。
/// 参数的共享写必须同步。callback 不得睡眠、访问用户内存或递归同步调用。
/// 调用者必须能服务本地 IPI，且不能持有 callback 需要的锁。
pub unsafe fn call(target: usize, callback: unsafe fn(*mut ()), argument: *mut ()) {
    if target == current().id {
        let _irq = IrqSaveGuard::new();
        callback(argument);
        return;
    }
    let local = get(target);
    let pinned = core::pin::pin!(Call {
        callback, argument, next: UnsafeCell::new(ptr::null_mut()), done: AtomicBool::new(false),
    });
    let request = pinned.as_ref().get_ref();
    let node = request as *const Call as *mut Call;
    local.calls.lock().push(node);
    crate::arch::send_ipi(local.hardware_id);
    while !request.done.load(Ordering::Acquire) {
        spin_loop();
    }
}

/// 测试只读真实队列深度，用于确定交错已经发生，不维护第二份计数。
#[cfg(feature = "boot-tests")]
pub fn pending_calls(target: usize) -> usize {
    let queue = get(target).calls.lock();
    let mut node = queue.head;
    let mut count = 0;
    while !node.is_null() {
        count += 1;
        node = unsafe { *(*node).next.get() };
    }
    count
}

/// 保持当前阶段 CPU 能服务本地 IRQ 的空闲循环，不接入旧调度器。
pub fn idle() -> ! {
    loop {
        crate::arch::wait_for_interrupt();
    }
}
