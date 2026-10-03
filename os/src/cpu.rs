//! 固定在线 CPU 的执行基础；请求节点由同步调用者拥有，完成前不离开其栈。

use core::cell::UnsafeCell;
use core::hint::spin_loop;
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use spin::Once;

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

/// AP 使用的不可变启动记录；记录、独占栈和 CpuLocal 在内核生命周期内保留。
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

// SAFETY: 队列指针仅在短锁内访问；调用者固定节点地址并等到 release 完成才结束节点生命周期。
unsafe impl Send for CallQueue {}

impl CallQueue {
    /// 创建尚无请求的队列，头尾在同一锁域内维护。
    const fn new() -> Self {
        Self {
            head: ptr::null_mut(),
            tail: ptr::null_mut(),
        }
    }

    /// 发布一个固定地址的请求，由目标 CPU 负责取出并完成。
    ///
    /// # Safety
    /// 调用者持有所属队列锁，call 未在其他队列中且在完成确认前保持有效；
    /// 队列中已有节点也受同一生命周期协议约束。
    unsafe fn push(&mut self, call: *mut Call) {
        if self.tail.is_null() {
            self.head = call;
        } else {
            // SAFETY: 非空尾节点仍在本队列中；持锁独占其 next，发送者尚未收到完成。
            unsafe { *(*self.tail).next.get() = call };
        }
        self.tail = call;
    }

    /// 在锁域中移除队首，返回后由调用者在锁外执行动作。
    ///
    /// # Safety
    /// 调用者持有所属队列锁，所有节点保持有效直到各自的完成确认。
    unsafe fn pop(&mut self) -> Option<*mut Call> {
        let call = self.head;
        if call.is_null() {
            return None;
        }
        // SAFETY: 非空队首由 push 发布且尚未完成；锁保护 next 的读取与头尾更新。
        self.head = unsafe { *(*call).next.get() };
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
    /// 构造 CPU 身份和独立执行状态，发布以后只修改原子字段与请求队列。
    const fn new(id: usize, hardware_id: usize, stack_top: usize) -> Self {
        Self {
            id,
            hardware_id,
            stack_top,
            online: AtomicBool::new(false),
            timer_interrupts: AtomicUsize::new(0),
            ipi_interrupts: AtomicUsize::new(0),
            calls: SpinLock::new(CallQueue::new()),
        }
    }
}

static CPU_LOCALS: Once<[CpuLocal; MAX_CPUS]> = Once::new();
static mut BOOT_RECORDS: [CpuBoot; MAX_CPUS] = [CpuBoot {
    stack_top: 0,
    local: 0,
}; MAX_CPUS];

/// 构造 CPU 表和启动记录，并安装 BSP 的每核基址。
///
/// # Safety
/// 只能由 BSP 在 AP 未释放、IRQ 关闭时执行一次；info 是已发布的实际资源，
/// cpu_count 和 hardware_ids 已验证。启动栈映射有效，且不存在 AP 对记录的访问。
pub unsafe fn initialize(info: &BootInfo) {
    assert!(
        CPU_LOCALS.get().is_none(),
        "CPU table is already initialized"
    );
    let stack_base = ptr::addr_of!(BOOT_STACKS) as usize;
    let locals = CPU_LOCALS.call_once(|| {
        core::array::from_fn(|id| {
            CpuLocal::new(
                id,
                info.hardware_ids[id],
                stack_base + (id + 1) * STACK_SIZE,
            )
        })
    });
    for (id, local) in locals[..info.cpu_count].iter().enumerate() {
        // SAFETY: id 来自已验证的有效 CPU 范围；BSP 独占静态记录，AP 尚未读取。
        unsafe {
            ptr::addr_of_mut!(BOOT_RECORDS)
                .cast::<CpuBoot>()
                .add(id)
                .write(CpuBoot {
                    stack_top: local.stack_top,
                    local: local as *const CpuLocal as usize,
                });
        }
    }
    // SAFETY: 逻辑 0 是 BSP；Once 中的对象已完成构造且永久存在，IRQ 尚未开启。
    unsafe { crate::arch::install_cpu_local(&locals[0]) };
}

/// 借用当前核的稳定 CPU 对象；未安装基址时直接报告初始化错误。
pub fn current() -> &'static CpuLocal {
    let local = crate::arch::cpu_local();
    // SAFETY: BSP 入口清空基址；RV AP 只从永久启动记录预装指针，installer 只写有效对象，
    // trap 恢复同一被打断基址。空值由 as_ref 返回 None，不构造空引用。
    unsafe { local.as_ref() }.expect("CPU local base has not been installed")
}

/// 返回已解析的固定 CPU 数量。
pub fn count() -> usize {
    crate::boot::info().cpu_count
}

/// 借用有效逻辑编号的 CPU；构造时序和编号错误只在此对象边界报告。
pub fn get(id: usize) -> &'static CpuLocal {
    let locals = CPU_LOCALS
        .get()
        .expect("CPU table has not been initialized");
    &locals[..count()][id]
}

/// 在本核已能服务中断后发布上线，使等待者的 acquire 观察对应真实可用状态。
pub fn publish_online() {
    current().online.store(true, Ordering::Release);
}

/// 由 BSP 串行释放 AP，等待每核能服务中断；失败保留记录和栈。
///
/// # Safety
/// CPU 表和全部静态启动记录已构造，本核为 BSP，目标 AP 尚未启动。
/// 调用期间不得重写记录、回收栈或改变当前映射，且本核可以服务 IPI。
pub unsafe fn start_secondaries() {
    for id in 1..count() {
        let local = get(id);
        // SAFETY: id 位于已构造的 CPU 范围，静态记录已发布且不会被回收。
        let record = unsafe { ptr::addr_of!(BOOT_RECORDS).cast::<CpuBoot>().add(id) as usize };
        let physical = crate::arch::virt_to_phys(record);
        // SAFETY: 本核是唯一启动者；静态记录、不同栈和 CpuLocal 由 initialize 发布，
        // physical 使用当前内核的真实物理转换，所有指向对象保持永久有效。
        if let Err(error) = unsafe { crate::arch::start_cpu(local.hardware_id, physical) } {
            panic!(
                "AP start failed: hardware={} error={}",
                local.hardware_id, error
            );
        }
        let deadline = crate::arch::read_counter() + crate::boot::info().counter_frequency * 5;
        while !local.online.load(Ordering::Acquire) {
            if deadline <= crate::arch::read_counter() {
                panic!("AP online timed out: hardware={}", local.hardware_id);
            }
            spin_loop();
        }
    }
}

/// 记录真实本地 timer 中断；硬件确认由架构 handler 完成，内存验收 hook 只在专用 feature 下运行。
pub fn handle_timer() {
    current().timer_interrupts.fetch_add(1, Ordering::Relaxed);
    #[cfg(feature = "memory-tests")]
    crate::mm::tests::timer_irq();
}

/// 在硬件通知已清除后取出真实请求，锁外执行并发布完成。
pub fn handle_ipi() {
    let local = current();
    local.ipi_interrupts.fetch_add(1, Ordering::Relaxed);
    loop {
        // SAFETY: 队列短锁覆盖 pop；入队协议保证所有未完成节点仍然存活。
        let request = unsafe { local.calls.lock().pop() };
        let request = match request {
            Some(request) => request,
            None => break,
        };
        // SAFETY: 请求已取出但尚未发布完成，发送者仍保留节点和参数；动作由 call 的
        // unsafe 契约约束。完成是最后一次访问节点，随后仅继续处理队列。
        unsafe {
            ((*request).callback)((*request).argument);
            (*request).done.store(true, Ordering::Release);
        }
    }
}

/// 在目标 CPU 上同步执行一次短动作，真正完成后才归还参数拥有者。
///
/// # Safety
/// target 必须是在线表中的逻辑编号；callback 和 argument 在返回前保持有效。
/// 参数的共享写必须同步。callback 不得睡眠、访问用户内存或递归同步调用。
/// 调用者必须能服务本地 IPI，且不能持有 callback 需要的锁。
pub unsafe fn call(target: usize, callback: unsafe fn(*mut ()), argument: *mut ()) {
    if target == current().id {
        let _irq = IrqSaveGuard::new();
        // SAFETY: caller 提供有效动作和参数；本核短 IRQ-save 域覆盖整个直接动作。
        unsafe { callback(argument) };
        return;
    }
    let local = get(target);
    let pinned = core::pin::pin!(Call {
        callback,
        argument,
        next: UnsafeCell::new(ptr::null_mut()),
        done: AtomicBool::new(false),
    });
    let request = pinned.as_ref().get_ref();
    let node = request as *const Call as *mut Call;
    // SAFETY: pinned 固定节点地址，参数义务由 caller 承担；队列锁覆盖链接更新，
    // 本函数在 acquire 观察完成前不离开此栈帧。
    unsafe { local.calls.lock().push(node) };
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
        // SAFETY: 队列锁保护全部链接，未完成节点的拥有者尚未结束其生命周期。
        node = unsafe { *(*node).next.get() };
    }
    count
}

/// 以本地硬件等待保持 IRQ 可服务，不执行任务调度或申请运行期内存。
pub fn idle() -> ! {
    loop {
        crate::arch::wait_for_interrupt();
    }
}
