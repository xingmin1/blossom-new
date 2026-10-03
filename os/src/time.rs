//! 有限硬件差值维护单调时间；每核一次事件只覆盖维护期限与当前任务时间片。

pub use core::time::Duration;

use core::cell::UnsafeCell;
use core::convert::TryFrom;
use core::hint::spin_loop;
use core::sync::atomic::{compiler_fence, fence, AtomicU64, Ordering};
use spin::Once;

use crate::arch;
use crate::boot::BootInfo;
use crate::cpu;
use crate::sync::SpinLock;

const NSEC_PER_SEC: u64 = 1_000_000_000;
const CONVERSION_SECONDS: u64 = 3600;
const QUANTUM: Duration = Duration::from_millis(10);

/// 自时间源注册起累计的单调纳秒，不表示日历时间。
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct MonotonicInstant(u64);

impl MonotonicInstant {
    /// 构造内部绝对纳秒期限，不赋予墙钟含义。
    pub const fn from_nanos(nanos: u64) -> Self {
        Self(nanos)
    }

    /// 返回统一时间域的纳秒值。
    pub const fn as_nanos(self) -> u64 {
        self.0
    }

    /// 计算有限期限，拒绝 Duration 或结果超出内部纳秒空间。
    pub fn checked_add(self, duration: Duration) -> Option<Self> {
        let nanos = u64::try_from(duration.as_nanos()).ok()?;
        self.0.checked_add(nanos).map(Self)
    }

    /// 返回两个单调时刻的间隔，逆序时刻不产生负 Duration。
    pub fn checked_duration_since(self, earlier: Self) -> Option<Duration> {
        self.0.checked_sub(earlier.0).map(Duration::from_nanos)
    }
}

/// 注册后不变的定点换算与安全采样界限。
#[derive(Clone, Copy)]
pub(crate) struct Scale {
    pub(crate) mask: u64,
    pub(crate) mult: u64,
    pub(crate) shift: u32,
    pub(crate) max_delta: u64,
    pub(crate) maintenance_delta: u64,
}

impl Scale {
    /// 验证连续低位 mask 和频率，并选择覆盖一小时乘法的最高精度。
    fn new(frequency: u64, mask: u64) -> Self {
        assert!(
            0 < frequency && frequency <= u32::MAX as u64,
            "invalid counter frequency"
        );
        assert!(
            3 <= mask && mask & mask.wrapping_add(1) == 0,
            "invalid counter mask"
        );
        let required_cycles = frequency * CONVERSION_SECONDS;
        let (mult, shift) = (0..=32)
            .rev()
            .find_map(|shift| {
                let mult = ((NSEC_PER_SEC << shift) + frequency / 2) / frequency;
                let residue_mask = (1u64 << shift) - 1;
                if 0 < mult
                    && mult <= u32::MAX as u64
                    && required_cycles <= (u64::MAX - residue_mask) / mult
                {
                    Some((mult, shift))
                } else {
                    None
                }
            })
            .expect("counter has no valid scale");
        let residue_mask = (1u64 << shift) - 1;
        let max_delta = ((u64::MAX - residue_mask) / mult).min(mask >> 1);
        let maintenance_delta = max_delta / 2;
        let scale = Self {
            mask,
            mult,
            shift,
            max_delta,
            maintenance_delta,
        };
        assert!(
            0 < scale.maintenance_ns(),
            "counter maintenance interval is below one nanosecond"
        );
        scale
    }

    /// 对有限差值保留小数余数；超界输入不会返回饱和的伪时间。
    pub(crate) fn convert(self, delta: u64, fraction: u64) -> Option<(u64, u64)> {
        let residue_mask = (1u64 << self.shift) - 1;
        if self.max_delta < delta || residue_mask < fraction {
            return None;
        }
        let scaled = delta.checked_mul(self.mult)?.checked_add(fraction)?;
        Some((scaled >> self.shift, scaled & residue_mask))
    }

    /// 返回提前于最大安全差值的维护间隔，留出一半差值空间供 IRQ 延迟。
    fn maintenance_ns(self) -> u64 {
        self.convert(self.maintenance_delta, 0)
            .expect("invalid maintenance delta")
            .0
    }

    /// 从同一读数的小数位置向上取整至硬件 tick，并校验最大转换范围。
    fn deadline_cycles(self, sample: Sample, deadline: MonotonicInstant) -> u64 {
        if deadline <= sample.instant {
            return sample.cycle;
        }
        let nanos = deadline.0 - sample.instant.0;
        let scaled = nanos
            .checked_mul(1u64 << self.shift)
            .and_then(|value| value.checked_sub(sample.fraction))
            .expect("clockevent deadline conversion overflows");
        let ticks = scaled / self.mult + u64::from(scaled % self.mult != 0);
        assert!(
            ticks <= self.max_delta,
            "clockevent deadline exceeds safe delta"
        );
        sample
            .cycle
            .checked_add(ticks)
            .expect("clockevent absolute counter overflows")
    }
}

#[derive(Clone, Copy)]
struct Sample {
    instant: MonotonicInstant,
    cycle: u64,
    fraction: u64,
}

/// 原子基准字段与序号共同提供合法的一代快照；写侧短锁只串行维护。
pub(crate) struct Timekeeper {
    scale: Scale,
    writer: SpinLock<()>,
    sequence: AtomicU64,
    base_ns: AtomicU64,
    cycle_last: AtomicU64,
    fraction: AtomicU64,
    #[cfg(feature = "execution-tests")]
    updates: AtomicU64,
}

impl Timekeeper {
    /// 建立相对原始计数起点，完整验证配置后才可向读者发表。
    pub(crate) fn new(frequency: u64, mask: u64, initial_cycle: u64) -> Self {
        Self {
            scale: Scale::new(frequency, mask),
            writer: SpinLock::new(()),
            sequence: AtomicU64::new(0),
            base_ns: AtomicU64::new(0),
            cycle_last: AtomicU64::new(initial_cycle & mask),
            fraction: AtomicU64::new(0),
            #[cfg(feature = "execution-tests")]
            updates: AtomicU64::new(0),
        }
    }

    /// 从匹配基准外推当前纳秒；采样必须在序号验证区间内完成。
    fn sample(&self, counter: impl Fn() -> u64) -> Sample {
        loop {
            let sequence = self.sequence.load(Ordering::Acquire);
            if sequence & 1 != 0 {
                spin_loop();
                continue;
            }
            let base = self.base_ns.load(Ordering::Relaxed);
            #[cfg(feature = "execution-tests")]
            let overlap = testing::pause_snapshot(self);
            let last = self.cycle_last.load(Ordering::Relaxed);
            let fraction = self.fraction.load(Ordering::Relaxed);
            // 原始 counter 指令声明 nomem；编译器屏障将它保留在两次序号读取之间。
            compiler_fence(Ordering::SeqCst);
            let cycle = counter() & self.scale.mask;
            compiler_fence(Ordering::SeqCst);
            // 若任一字段读到新写，写侧 Release fence 与此 Acquire fence 同步，
            // 奇序号先于最后序号读取；原子一致性禁止后者仍接受旧偶序号。
            fence(Ordering::Acquire);
            if self.sequence.load(Ordering::Relaxed) != sequence {
                #[cfg(feature = "execution-tests")]
                if overlap {
                    testing::reject_snapshot();
                }
                continue;
            }
            return self.advance(base, last, fraction, cycle);
        }
    }

    /// 对已经稳定的基准做算术，避免把重试期间的混合字段误报为超界。
    fn advance(&self, base: u64, last: u64, fraction: u64, cycle: u64) -> Sample {
        let delta = cycle.wrapping_sub(last) & self.scale.mask;
        let (nanos, fraction) = self
            .scale
            .convert(delta, fraction)
            .expect("counter delta exceeds safe sampling interval");
        Sample {
            instant: MonotonicInstant(
                base.checked_add(nanos)
                    .expect("monotonic nanoseconds exhausted"),
            ),
            cycle,
            fraction,
        }
    }

    /// 读取同一算法的当前时刻；采样函数只能非阻塞读取同一计数源，不能递归维护。
    pub(crate) fn read(&self, counter: impl Fn() -> u64) -> MonotonicInstant {
        self.sample(counter).instant
    }

    /// 在 IRQ-safe 短锁内重新采样并推进基准，小数余数跨维护保留。
    /// 采样函数必须非阻塞且无分配，不能重入此基准；原始采样不得在调用前缓存。
    pub(crate) fn update(&self, counter: impl Fn() -> u64) -> MonotonicInstant {
        let _writer = self.writer.lock();
        compiler_fence(Ordering::SeqCst);
        let cycle = counter() & self.scale.mask;
        compiler_fence(Ordering::SeqCst);
        let sample = self.advance(
            self.base_ns.load(Ordering::Relaxed),
            self.cycle_last.load(Ordering::Relaxed),
            self.fraction.load(Ordering::Relaxed),
            cycle,
        );
        let sequence = self.sequence.load(Ordering::Relaxed);
        let complete = sequence
            .checked_add(2)
            .expect("timekeeper sequence exhausted");
        #[cfg(feature = "execution-tests")]
        let updates = self
            .updates
            .load(Ordering::Relaxed)
            .checked_add(1)
            .expect("timekeeper update count exhausted");
        // 全部可能失败的算术在奇序号前结束；写侧关 IRQ，不能被本核 reader 打断。
        self.sequence.store(sequence + 1, Ordering::Relaxed);
        fence(Ordering::Release);
        self.base_ns.store(sample.instant.0, Ordering::Relaxed);
        self.cycle_last.store(sample.cycle, Ordering::Relaxed);
        self.fraction.store(sample.fraction, Ordering::Relaxed);
        #[cfg(feature = "execution-tests")]
        self.updates.store(updates, Ordering::Relaxed);
        self.sequence.store(complete, Ordering::Release);
        #[cfg(feature = "execution-tests")]
        testing::publish_snapshot_update(self);
        sample.instant
    }

    /// 向专用验收公开注册后的真实换算参数。
    #[cfg(feature = "execution-tests")]
    pub(crate) fn scale(&self) -> Scale {
        self.scale
    }

    /// 返回稳定基准中的小数余数，不把观察操作当作维护更新。
    #[cfg(feature = "execution-tests")]
    pub(crate) fn fraction(&self) -> u64 {
        loop {
            let sequence = self.sequence.load(Ordering::Acquire);
            if sequence & 1 != 0 {
                spin_loop();
                continue;
            }
            let fraction = self.fraction.load(Ordering::Relaxed);
            fence(Ordering::Acquire);
            if self.sequence.load(Ordering::Relaxed) == sequence {
                return fraction;
            }
        }
    }
}

static TIMEKEEPER: Once<Timekeeper> = Once::new();

/// 在 BSP 上注册一次已发现的稳定计数器，不访问运行期 CPUCFG 或 FDT。
///
/// # Safety
/// 仅 BSP 在 MM 初始化后、IRQ 关闭且 AP 尚未释放时调用一次；BootInfo 频率
/// 必须属于当前跨 CPU 一致的 64 位硬件 counter，之后不得切换计数源。
pub unsafe fn initialize(info: &BootInfo) {
    assert!(!arch::irq_enabled(), "time initialization requires IRQ-off");
    assert!(
        TIMEKEEPER.get().is_none(),
        "timekeeper is already initialized"
    );
    TIMEKEEPER
        .call_once(|| Timekeeper::new(info.counter_frequency, u64::MAX, arch::read_counter()));
}

/// 借用已经完整发表的唯一时间基准，注册前调用直接失败。
fn keeper() -> &'static Timekeeper {
    TIMEKEEPER
        .get()
        .expect("timekeeper has not been initialized")
}

/// 读取生产单调时间；panic 与不可屏蔽异常不得等待可能被其打断的写者。
pub fn now() -> MonotonicInstant {
    keeper().read(arch::read_counter)
}

#[derive(Clone, Copy)]
struct EventState {
    active: bool,
    maintenance: MonotonicInstant,
    quantum: Option<MonotonicInstant>,
}

/// CpuLocal 中预备的本核事件状态，只由所属 CPU 在 IRQ-off 边界访问。
pub(crate) struct LocalEvent {
    state: UnsafeCell<EventState>,
}

// SAFETY: state 只经 with_current 访问；它校验所属 CPU 和 IRQ-off，远端只发送重编程提示。
unsafe impl Sync for LocalEvent {}

impl LocalEvent {
    /// 静态准备尚未接管硬件的本核事件，早期手动 timer 保持原行为。
    pub(crate) const fn new() -> Self {
        Self {
            state: UnsafeCell::new(EventState {
                active: false,
                maintenance: MonotonicInstant::from_nanos(0),
                quantum: None,
            }),
        }
    }

    /// 将可变事件借用限制在所属 CPU 的 IRQ-off 调用内，不跨任务切换。
    fn with_current<R>(&self, operation: impl FnOnce(&mut EventState) -> R) -> R {
        assert!(!arch::irq_enabled(), "local clockevent requires IRQ-off");
        assert!(
            core::ptr::eq(self, &cpu::current().time_event),
            "foreign CPU clockevent access"
        );
        // SAFETY: 对象为当前核的内嵌事件，IRQ 已关闭，私有方法不逃逸引用或调用调度。
        unsafe { operation(&mut *self.state.get()) }
    }
}

/// 在本核 scheduler 开始时接管一次事件；每个已激活 CPU 独立保证维护期限。
pub(crate) fn activate_current_cpu() {
    cpu::current().time_event.with_current(|event| {
        assert!(!event.active, "local clockevent already activated");
        let instant = keeper().update(arch::read_counter);
        event.active = true;
        event.maintenance = maintenance_deadline(instant);
        program(event);
    });
}

/// 当前任务真实切入时建立新时间片，不继承旧 CPU 或旧执行段的期限。
pub(crate) fn task_switch_in() {
    cpu::current().time_event.with_current(|event| {
        assert!(
            event.active && event.quantum.is_none(),
            "invalid task clockevent entry"
        );
        arm_quantum(event);
    });
}

/// 当前任务真实切出时移除时间片，保留本核计数器维护事件。
pub(crate) fn task_switch_out() {
    cpu::current().time_event.with_current(|event| {
        assert!(event.active, "task clockevent is not activated");
        event.quantum = None;
        program(event);
    });
}

/// 处理本核已经确认的 timer IRQ，只推进维护与事件，不做硬中断调度。
pub(crate) fn handle_timer() {
    cpu::current().time_event.with_current(|event| {
        if !event.active {
            return;
        }
        let instant = now();
        if event.maintenance <= instant {
            event.maintenance = maintenance_deadline(keeper().update(arch::read_counter));
        }
        if event.quantum.is_some_and(|deadline| deadline <= instant) {
            event.quantum = None;
        }
        program(event);
    });
}

/// 重查本核真实最早事件并编程；远端提示不携带可能失效的旧期限。
pub(crate) fn reprogram_current() {
    cpu::current().time_event.with_current(|event| {
        if event.active {
            program(event);
        }
    });
}

/// 从本次真实维护基准建立下一次期限，纳秒空间耗尽直接失败。
fn maintenance_deadline(instant: MonotonicInstant) -> MonotonicInstant {
    instant
        .checked_add(Duration::from_nanos(keeper().scale.maintenance_ns()))
        .expect("counter maintenance deadline overflows")
}

/// 对当前执行段重建一次真实调度期限，并提交同一本核事件选择。
fn arm_quantum(event: &mut EventState) {
    event.quantum = Some(now().checked_add(QUANTUM).expect("task quantum overflows"));
    program(event);
}

/// 将维护与当前任务期限中的最早者转换为一次硬件事件。
fn program(event: &EventState) {
    let deadline = event
        .quantum
        .map_or(event.maintenance, |quantum| quantum.min(event.maintenance));
    let timekeeper = keeper();
    let sample = timekeeper.sample(arch::read_counter);
    arch::program_timer(timekeeper.scale.deadline_cycles(sample, deadline));
}

/// 专用验收只公开生产对象与同一算法，不维护第二套时间模型。
#[cfg(feature = "execution-tests")]
pub(crate) mod testing {
    pub(crate) use super::Timekeeper;
    use super::{arm_quantum, keeper, MonotonicInstant};
    use crate::arch;
    use crate::cpu;
    use crate::task::{self, TaskId};
    use core::hint::spin_loop;
    use core::sync::atomic::{AtomicUsize, Ordering};

    const ARMED: usize = 1;
    const PAUSED: usize = 2;
    const PUBLISHED: usize = 3;
    const REJECTED: usize = 4;
    static OVERLAP_PHASE: AtomicUsize = AtomicUsize::new(0);
    static OVERLAP_READER: AtomicUsize = AtomicUsize::new(0);
    static OVERLAP_WRITER: AtomicUsize = AtomicUsize::new(0);
    static OVERLAPS: AtomicUsize = AtomicUsize::new(0);
    static RETRIES: AtomicUsize = AtomicUsize::new(0);

    /// 在当前普通任务的 IRQ-off 段重建时间片，沿用生产事件和硬件提交。
    ///
    /// # Safety
    /// 调用者必须为本核普通任务，IRQ 已关闭且不跨任务切换；本核事件已由
    /// scheduler 激活，调用期间不得持有会重入时间事件的锁或借用。
    pub(crate) unsafe fn rearm_current_quantum() {
        assert_ordinary_task();
        cpu::current().time_event.with_current(|event| {
            assert!(event.active, "task clockevent is not activated");
            arm_quantum(event);
        });
    }

    /// 为唯一生产基准准备一次指定任务的跨核快照交错。
    ///
    /// # Safety
    /// reader 必须在本核普通任务的 IRQ-off 段调用，并在同一段立即调用 now，
    /// 期间不切换或先调用其他时间读接口；writer 必须已在另一 CPU 可运行，
    /// 以原始 counter 有限等待 reader_paused 后调用 maintain。双方不得持有
    /// 对方推进所需的锁；reader 的 now 返回并关闭握手前不得重新注册。
    pub(crate) unsafe fn arm_snapshot_overlap(reader: TaskId, writer: TaskId) {
        assert_ordinary_task();
        assert_eq!(task::current_task_id(), Some(reader));
        assert_ne!(reader, writer, "snapshot overlap requires distinct tasks");
        assert_eq!(
            OVERLAP_PHASE.load(Ordering::Acquire),
            0,
            "snapshot overlap is already armed"
        );
        OVERLAP_READER.store(reader.as_usize(), Ordering::Relaxed);
        OVERLAP_WRITER.store(writer.as_usize(), Ordering::Relaxed);
        OVERLAPS.store(0, Ordering::Relaxed);
        RETRIES.store(0, Ordering::Relaxed);
        OVERLAP_PHASE.store(ARMED, Ordering::Release);
    }

    /// 无锁观察 reader 是否已经读取旧偶序号和 base，写者不调用 now 等待。
    pub(crate) fn reader_paused() -> bool {
        OVERLAP_PHASE.load(Ordering::Acquire) == PAUSED
    }

    /// 返回真实重叠更新数和由末序号不匹配产生的拒绝重试数。
    pub(crate) fn snapshot_overlap_result() -> (usize, usize) {
        (
            OVERLAPS.load(Ordering::Acquire),
            RETRIES.load(Ordering::Acquire),
        )
    }

    /// 在 reader 的 now 已返回后关闭一次完成的握手，不留下生产停点。
    pub(crate) fn finish_snapshot_overlap() {
        assert_eq!(
            OVERLAP_PHASE.load(Ordering::Acquire),
            REJECTED,
            "snapshot overlap has not rejected the old sequence"
        );
        OVERLAP_PHASE.store(0, Ordering::Release);
    }

    /// 校验停点只位于本核普通执行段，IRQ 不能在等待期间重入时间读取。
    fn assert_ordinary_task() {
        assert!(!arch::irq_enabled(), "snapshot test requires IRQ-off");
        let local = cpu::current();
        assert_eq!(local.hardirq_depth.load(Ordering::Relaxed), 0);
        assert_eq!(local.bh_depth.load(Ordering::Relaxed), 0);
        assert!(
            task::current_task_id().is_some(),
            "snapshot test requires an ordinary task"
        );
    }

    /// 只识别唯一生产基准与指定普通任务，独立算术实例不访问 CPU 状态。
    fn matches_task(timekeeper: &Timekeeper, target: &AtomicUsize) -> bool {
        if !core::ptr::eq(timekeeper, keeper()) {
            return false;
        }
        let local = cpu::current();
        local.hardirq_depth.load(Ordering::Relaxed) == 0
            && local.bh_depth.load(Ordering::Relaxed) == 0
            && task::current_task_id()
                .is_some_and(|id| id.as_usize() == target.load(Ordering::Relaxed))
    }

    /// 在旧代 even 和 base 之后暂停一次 reader；等待只依赖原始 counter。
    pub(super) fn pause_snapshot(timekeeper: &Timekeeper) -> bool {
        if OVERLAP_PHASE.load(Ordering::Acquire) != ARMED
            || !matches_task(timekeeper, &OVERLAP_READER)
        {
            return false;
        }
        assert_ordinary_task();
        let limit = arch::counter_frequency()
            .checked_mul(5)
            .expect("snapshot raw timeout overflows");
        let start = arch::read_counter();
        OVERLAP_PHASE.store(PAUSED, Ordering::Release);
        while OVERLAP_PHASE.load(Ordering::Acquire) == PAUSED {
            assert!(
                arch::read_counter().wrapping_sub(start) < limit,
                "snapshot overlapping update timed out"
            );
            spin_loop();
        }
        assert_eq!(OVERLAP_PHASE.load(Ordering::Acquire), PUBLISHED);
        true
    }

    /// 写者完成真实字段写和 even 发布以后才释放持旧 base 的 reader。
    pub(super) fn publish_snapshot_update(timekeeper: &Timekeeper) {
        if OVERLAP_PHASE.load(Ordering::Acquire) == PAUSED
            && matches_task(timekeeper, &OVERLAP_WRITER)
        {
            OVERLAPS.fetch_add(1, Ordering::Relaxed);
            OVERLAP_PHASE.store(PUBLISHED, Ordering::Release);
        }
    }

    /// 只在原 sample 的末序号实际不匹配时记录拒绝，不把暂停当成重试。
    pub(super) fn reject_snapshot() {
        RETRIES.fetch_add(1, Ordering::Relaxed);
        OVERLAP_PHASE.store(REJECTED, Ordering::Release);
    }

    /// 真实推进生产基准，写锁与序号协议和本地维护 IRQ 完全相同。
    pub(crate) fn maintain() -> MonotonicInstant {
        keeper().update(arch::read_counter)
    }

    /// 返回生产基准真实完成的维护更新次数。
    pub(crate) fn updates() -> u64 {
        keeper().updates.load(Ordering::Acquire)
    }
}
