//! 真实用户指令、私有现场、权限异常及页表拥有的逐例回归。

use alloc::sync::{Arc, Weak};
use core::hint::black_box;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::arch::{self, UserAccess, UserContext, UserTrap};
#[cfg(target_arch = "loongarch64")]
use crate::arch::{user_fp_lanes, FpRegister};
use crate::ktest::execution_core::{baseline, until, Baseline};
use crate::ktest::execution_services::SuiteBaseline;
use crate::ktest::{prepare_result, ChildHandle, ChildToken};
use crate::mm::space::{Mm, VmError};
use crate::mm::uaccess::CStringError;
use crate::mm::user_table::{PagePermissions, PageTableError, PreparedTables, UserPageTable};
use crate::sync::{Completion, SleepRwLock, SpinLock};
use crate::task::UserThread;
use crate::{cpu, mm, sched, task};
use ::ktest::{Fixture, TestContext, TestError, TestResult};

const CODE: usize = 0x10000;
const DATA: usize = 0x20000;
const STACK: usize = 0x30000;
const STACK_TOP: usize = STACK + mm::PAGE_SIZE;
const TARGET: usize = 0x40000;
const RX: PagePermissions = PagePermissions {
    read: true,
    write: false,
    execute: true,
};
const RW: PagePermissions = PagePermissions {
    read: true,
    write: true,
    execute: false,
};
const RO: PagePermissions = PagePermissions {
    read: true,
    write: false,
    execute: false,
};
const EX: PagePermissions = PagePermissions {
    read: false,
    write: false,
    execute: true,
};
const NONE: PagePermissions = PagePermissions {
    read: false,
    write: false,
    execute: false,
};

#[cfg(target_arch = "riscv64")]
const TP: usize = 4;
#[cfg(target_arch = "loongarch64")]
const TP: usize = 2;
#[cfg(target_arch = "riscv64")]
const GP: usize = 3;
#[cfg(target_arch = "loongarch64")]
const GP: usize = 21;
#[cfg(target_arch = "riscv64")]
const A0: usize = 10;
#[cfg(target_arch = "loongarch64")]
const A0: usize = 4;
#[cfg(target_arch = "riscv64")]
const A5: usize = 15;
#[cfg(target_arch = "loongarch64")]
const A5: usize = 9;
#[cfg(target_arch = "riscv64")]
const A6: usize = 16;
#[cfg(target_arch = "loongarch64")]
const A6: usize = 10;
#[cfg(target_arch = "riscv64")]
const A7: usize = 17;
#[cfg(target_arch = "loongarch64")]
const A7: usize = 11;
#[cfg(target_arch = "riscv64")]
const T0: usize = 5;
#[cfg(target_arch = "loongarch64")]
const T0: usize = 12;

struct Users {
    initial: Baseline,
    stop: Arc<AtomicBool>,
    handles: [Option<ChildHandle>; 4],
    controls: Option<Arc<Controls>>,
}

impl Fixture<SuiteBaseline> for Users {
    /// 页表与执行者准备前保存停稳资源，每例只拥有自己的停止和离栈观察对象。
    fn set_up(_: &mut TestContext<'_>, _: &SuiteBaseline) -> Result<Self, TestError> {
        let initial = baseline()?;
        let stop = Arc::try_new(AtomicBool::new(false))
            .map_err(|_| TestError("NoMemory: user execution controls"))?;
        Ok(Self {
            initial,
            stop,
            handles: core::array::from_fn(|_| None),
            controls: None,
        })
    }

    /// 停止原普通 Task 并通知本例真实等待源；用户只在返回 Rust 后消费停止，离栈前保留全部拥有。
    fn quiesce(&mut self, _: &mut TestContext<'_>) -> TestResult {
        self.stop.store(true, Ordering::Release);
        if let Some(controls) = &self.controls {
            for gate in &controls.gates {
                gate.complete();
            }
        }
        until(
            || {
                self.handles
                    .iter()
                    .flatten()
                    .all(|handle| handle.try_join().is_some())
            },
            "user execution did not leave its original Task stack",
        )
    }

    /// 原框架 join 后释放观察与控制对象，记录并比较完整六域和任务元数据。
    fn tear_down(self, context: &mut TestContext<'_>) -> TestResult {
        let Self {
            initial,
            stop,
            handles,
            controls,
        } = self;
        assert!(handles
            .iter()
            .flatten()
            .all(|handle| handle.try_join().is_some()));
        drop(handles);
        drop(stop);
        drop(controls);
        let actual = baseline()?;
        context.diagnostic(format_args!(
            "USER RESOURCES initial={:?} final={:?}",
            initial, actual
        ));
        if actual == initial {
            Ok(())
        } else {
            Err(TestError("user execution resources did not converge"))
        }
    }
}

impl Users {
    /// 先绑定私有用户对象再用原 scope 发表，同一槽的上次执行者必须已经离栈。
    fn start(
        &mut self,
        slot: usize,
        user: Arc<UserThread>,
        body: impl FnOnce(Arc<UserThread>, ChildToken, Arc<AtomicBool>) -> Result<i32, TestError>
            + Send
            + 'static,
    ) -> TestResult {
        assert!(self.handles[slot]
            .as_ref()
            .is_none_or(|handle| handle.try_join().is_some()));
        let owned = Arc::clone(&user);
        let stop = Arc::clone(&self.stop);
        let prepared = prepare_result(move |token| body(owned, token, stop))?;
        prepared.bind_user(user);
        self.handles[slot] = Some(prepared.publish()?);
        Ok(())
    }

    /// 原 JoinState 的实际返回值只能在任务离栈后观察，子错误仍由 scope 单独报告。
    fn joined(&self, slot: usize, expected: i32) -> TestResult {
        let handle = self.handles[slot]
            .as_ref()
            .expect("user child was not published");
        until(|| handle.try_join().is_some(), "user child did not finish")?;
        assert_eq!(handle.try_join(), Some(expected));
        Ok(())
    }

    /// 睡眠锁参与者沿同一 scope 发表普通 Task，观察槽仍由 fixture 停止和原 join 收回。
    fn start_task(
        &mut self,
        slot: usize,
        body: impl FnOnce(ChildToken, Arc<AtomicBool>) -> Result<i32, TestError> + Send + 'static,
    ) -> TestResult {
        assert!(self.handles[slot]
            .as_ref()
            .is_none_or(|handle| handle.try_join().is_some()));
        let stop = Arc::clone(&self.stop);
        self.handles[slot] = Some(prepare_result(move |token| body(token, stop))?.publish()?);
        Ok(())
    }
}

struct Controls {
    gates: [Completion; 4],
    ready: [AtomicBool; 4],
    tasks: [SpinLock<Option<Weak<task::Task>>>; 4],
    phase: AtomicUsize,
    lock: SleepRwLock<u64>,
}

impl Controls {
    /// 保存每例真实等待源与弱任务观察，停止时只通知原参与者，不取得执行或离栈责任。
    fn new(fixture: &mut Users) -> Result<Arc<Self>, TestError> {
        let controls = Arc::try_new(Self {
            gates: core::array::from_fn(|_| Completion::new()),
            ready: core::array::from_fn(|_| AtomicBool::new(false)),
            tasks: core::array::from_fn(|_| SpinLock::new(None)),
            phase: AtomicUsize::new(0),
            lock: SleepRwLock::new(0x9817),
        })
        .map_err(|_| TestError("NoMemory: user VM controls"))?;
        fixture.controls = Some(Arc::clone(&controls));
        Ok(controls)
    }

    /// 在当前普通 Task 发布弱身份；观察方只能沿调度器读取实际 Blocked 状态。
    fn record(&self, slot: usize) {
        *self.tasks[slot].lock() = Some(Arc::downgrade(&task::current_task()));
    }

    /// 释放观察短锁后升级弱引用，真实调度状态决定等待是否已交出执行权。
    fn blocked(&self, slot: usize) -> bool {
        let task = self.tasks[slot]
            .lock()
            .clone()
            .and_then(|task| task.upgrade());
        task.is_some_and(|task| sched::test_state(&task) == task::State::Blocked)
    }
}

/// 受控访问每次从真实用户指令入口开始，缺页只交给当前 Mm 解决并在同一 PC 重进。
fn access(
    user: &UserThread,
    token: &ChildToken,
    stop: &AtomicBool,
    address: usize,
    direction: UserAccess,
) -> Result<(usize, usize), TestError> {
    {
        let mut context = user.context();
        context.pc = CODE + arch::fault_offset(direction);
        context.gpr[A5] = address;
    }
    let mm = user.mm().expect("user access has no MM");
    let mut faults = 0;
    loop {
        match event(user, token, stop)? {
            UserTrap::PageFault {
                address: actual,
                access,
            } => {
                assert_eq!(actual, address);
                assert!(
                    access == direction
                        || direction == UserAccess::Write && access == UserAccess::Read
                );
                let pc = user.context().pc;
                mm.fault(actual, access)
                    .map_err(|_| TestError("user VM fault failed"))?;
                assert_eq!(user.context().pc, pc);
                faults += 1;
                assert!(faults <= 2, "resolved user fault repeated");
            }
            UserTrap::Syscall => return Ok((faults, user.context().gpr[T0])),
            trap => panic!("unexpected user VM trap: {:?}", trap),
        }
    }
}

/// 完整字段由逐页入口复制，失败前缀不得被当作已初始化的用户数值。
fn word(mm: &Mm, address: usize) -> u64 {
    mm.read_u64(address).expect("user VM word is inaccessible")
}

/// 在尚未共享的 Mm 中安装完整初始化的真实单页；返回整数只用于同例受约束观察。
fn page(
    mm: &mut Mm,
    address: usize,
    permissions: PagePermissions,
    contents: &[u8],
) -> Result<usize, TestError> {
    if mm::PAGE_SIZE < contents.len() {
        return Err(TestError("user probe does not fit a basic page"));
    }
    let mut pages = mm::alloc_pages(0, true).ok_or(TestError("NoMemory: user probe page"))?;
    let direct = pages.as_mut_ptr().cast::<u8>();
    // SAFETY: pages 独占完整清零的真实页，contents 为稳定内核字节且长度已检查；
    // 两块存储不重叠，用户根尚未发表，本次原始初始化在转交拥有前结束。
    unsafe { core::ptr::copy_nonoverlapping(contents.as_ptr(), direct, contents.len()) };
    // SAFETY: 全页已清零并完成内容复制，无排他引用或并发访问；只作为用户代码或数据，
    // 原页唯一归还权移入尚未共享的 Mm，旧裸地址从不用于恢复或释放拥有。
    unsafe { mm.map_owned(address, pages, permissions, permissions) }
        .map_err(|_| TestError("user probe mapping failed"))?;
    Ok(direct as usize)
}

/// 构造同一正式 Mm 和硬件页表，受控代码仅用于验证真实用户特权入口。
fn image(offset: usize, slot: usize) -> Result<(Mm, UserContext, usize), TestError> {
    let mut mm = Mm::try_new().map_err(|_| TestError("NoMemory: user probe root"))?;
    page(&mut mm, CODE, RX, arch::user_code())?;
    let data = page(&mut mm, DATA, RW, &[])?;
    page(&mut mm, STACK, RW, &[])?;
    let mut context = UserContext::new(CODE + offset, STACK_TOP);
    for (index, register) in context.gpr.iter_mut().enumerate().skip(1) {
        *register = 0x7100 + slot * 0x100 + index;
    }
    #[cfg(target_arch = "riscv64")]
    {
        context.gpr[2] = STACK_TOP;
        context.fcsr = 1 << 5;
    }
    #[cfg(target_arch = "loongarch64")]
    {
        context.gpr[3] = STACK_TOP;
        context.fcsr = 1 << 8;
        context.fcc = 0x0100010001000100;
    }
    context.gpr[TP] = 0x5100 + slot;
    context.gpr[GP] = 0x6100 + slot;
    context.gpr[A6] = DATA;
    let mut scalar: [u64; 32] =
        core::array::from_fn(|index| 0x3ff0000000000000 + (slot * 32 + index) as u64);
    scalar[0] = if slot == 0 {
        0x3ff4000000000000
    } else {
        0x4020000000000000
    };
    scalar[1] = if slot == 0 {
        0x4004000000000000
    } else {
        0x4030000000000000
    };
    #[cfg(target_arch = "riscv64")]
    {
        context.fpr = scalar;
    }
    #[cfg(target_arch = "loongarch64")]
    {
        let lanes = user_fp_lanes();
        context.fpr = core::array::from_fn(|index| FpRegister {
            lanes: core::array::from_fn(|lane| {
                if lane == 0 {
                    scalar[index]
                } else if lane < lanes {
                    0x7180 + (slot * 128 + index * 4 + lane) as u64
                } else {
                    0
                }
            }),
        });
        for lane in 0..4 {
            let value = 0x8190 + (slot * 4 + lane) as u64;
            let progress = mm.copyout(DATA + 96 + lane * 8, &value.to_le_bytes());
            assert_eq!((progress.copied, progress.error), (8, None));
        }
    }
    Ok((mm, context, data))
}

/// 在完整映像发表前建立用户私有拥有，失败仍按未发表对象正常归还。
fn user(mm: Mm, context: UserContext) -> Result<Arc<UserThread>, TestError> {
    let mm = Arc::try_new(mm).map_err(|_| TestError("NoMemory: user probe MM owner"))?;
    UserThread::try_new(mm, context).map_err(|_| TestError("NoMemory: user probe context"))
}

/// 只跳过已完成分派的硬件中断，不推进 PC；停止仅在返回原 Rust 栈后消费。
fn event(user: &UserThread, token: &ChildToken, stop: &AtomicBool) -> Result<UserTrap, TestError> {
    loop {
        if stop.load(Ordering::Acquire) || token.stop_requested() {
            return Err(TestError("user probe was stopped"));
        }
        let trap = user.run_user();
        if trap != UserTrap::Interrupt {
            return Ok(trap);
        }
        sched::cond_resched();
    }
}

/// 在本例唯一用户执行者已返回内核后读取其实际 store 结果，不产生指向用户页的 Rust 引用。
///
/// # Safety
/// direct 必须仍由本例 Mm 保活且完整初始化，至少可读 64 字节；所有用户执行者已停止写入。
unsafe fn probe_words(direct: usize) -> [u64; 8] {
    let mut words = [0; 8];
    // SAFETY: 调用者保活完整初始化的数据页并保证没有并发写，words 是独占内核局部；
    // 两块存储不重叠且按 u64 对齐，访问在 Mm 解除或归页之前结束。
    unsafe {
        core::ptr::copy_nonoverlapping(direct as *const u64, words.as_mut_ptr(), words.len())
    };
    words
}

/// 比较用户真正算出的浮点和栈/TLS/GP存取结果，未执行过的 Rust 初始值不能满足结果。
fn check_probe(words: [u64; 8], slot: usize, fcsr: usize) {
    assert_eq!(words[0], STACK_TOP as u64);
    assert_eq!(words[1], (0x5100 + slot) as u64);
    assert_eq!(words[2], (0x6100 + slot) as u64);
    assert_eq!(
        words[3],
        if slot == 0 {
            0x400e000000000000
        } else {
            0x4038000000000000
        }
    );
    assert_eq!(words[4], fcsr as u64);
    assert_eq!(words[5], (0x5100 + slot) as u64);
}

#[derive(PartialEq, Eq)]
struct Registers {
    gpr: [usize; 32],
    #[cfg(target_arch = "riscv64")]
    fpr: [u64; 32],
    #[cfg(target_arch = "loongarch64")]
    fpr: [FpRegister; 32],
    fcsr: usize,
    #[cfg(target_arch = "loongarch64")]
    fcc: usize,
}

impl Registers {
    /// 短暂复制用户真实现场后立即结束 RefMut，快照可以跨原 Task 的迁核交接。
    fn read(user: &UserThread) -> Self {
        let context = user.context();
        Self {
            gpr: context.gpr,
            fpr: context.fpr,
            fcsr: context.fcsr,
            #[cfg(target_arch = "loongarch64")]
            fcc: context.fcc,
        }
    }
}

/// 确认一次完整用户中断已经在本 Task 栈返回，CpuLocal 和原局部帧没有被用户 TP/SP 替换。
fn interrupted(user: &UserThread, expected_cpu: usize) {
    let local = [0x8165usize; 16];
    let stack = black_box(&local) as *const _ as usize;
    let id = task::current_task().id();
    assert_eq!(user.run_user(), UserTrap::Interrupt);
    assert_eq!(cpu::current().id, expected_cpu);
    assert_eq!(arch::cpu_local(), cpu::get(expected_cpu) as *const _);
    assert_eq!(task::current_task().id(), id);
    assert_eq!(black_box(&local) as *const _ as usize, stack);
    assert!(black_box(&local).iter().all(|word| *word == 0x8165));
}

#[::ktest::suite(fixture = Users, suite_resources = crate::ktest::execution_services::SuiteBaseline, tags("user-entry"), timeout_ms = 15000)]
pub(crate) mod entry {
    use super::*;

    /// 用户实际 stack/FP 运算后两次 syscall 保持完整现场，原 Task 调度后仍回原 Rust 栈。
    #[test]
    fn user_round_trip(_: &mut TestContext<'_>, fixture: &mut Users) -> TestResult {
        let (mm, context, direct) = image(0, 0)?;
        let fcsr = context.fcsr;
        fixture.start(0, user(mm, context)?, move |user, token, stop| {
            let local = [0x8167usize; 16];
            let stack = black_box(&local) as *const _ as usize;
            let id = task::current_task().id();
            let mut saved: Option<Registers> = None;
            for expected in [41, 42] {
                assert_eq!(event(&user, &token, &stop)?, UserTrap::Syscall);
                {
                    let mut context = user.context();
                    assert_eq!(context.gpr[A7], 93);
                    assert_eq!(context.gpr[A0], expected);
                    context.pc = context.pc.checked_add(4).expect("user syscall PC overflow");
                }
                assert_eq!(task::current_task().id(), id);
                assert_eq!(black_box(&local) as *const _ as usize, stack);
                assert!(black_box(&local).iter().all(|word| *word == 0x8167));
                if let Some(saved) = &mut saved {
                    saved.gpr[A0] = expected;
                    assert!(
                        *saved == Registers::read(&user),
                        "user registers changed across syscalls"
                    );
                } else {
                    saved = Some(Registers::read(&user));
                }
                sched::yield_now();
            }
            // SAFETY: 本例 Mm 保活映像的完整数据页，唯一用户执行者已返回内核，
            // 代码全部 store 已在首个 syscall 前完成，不存在其他写者或 Rust 存储别名。
            check_probe(unsafe { probe_words(direct) }, 0, fcsr);
            Ok(42)
        })?;
        fixture.joined(0, 42)
    }

    /// 两个独立现场实际计算 FP/FCC后，在中断、重复进入和全部在线CPU迁核间保持完整寄存器。
    #[test]
    fn user_private_contexts(_: &mut TestContext<'_>, fixture: &mut Users) -> TestResult {
        for slot in 0..2 {
            let (mm, context, direct) = image(arch::loop_offset(), slot)?;
            let fcsr = context.fcsr;
            fixture.start(slot, user(mm, context)?, move |user, token, stop| {
                until(
                    || {
                        if stop.load(Ordering::Acquire) || token.stop_requested() {
                            return true;
                        }
                        let current_cpu = cpu::current().id;
                        interrupted(&user, current_cpu);
                        user.context().pc == CODE + arch::loop_spin_offset()
                    },
                    "user probe did not enter its steady instruction loop",
                )?;
                if stop.load(Ordering::Acquire) || token.stop_requested() {
                    return Ok(0);
                }
                // SAFETY: 本例唯一用户执行者已到不写数据的稳态分支，Mm 仍保活完整数据页，
                // 当前只在其原 Task 中读取已完成的 store，其他 CPU 没有该映像的写者。
                let words = unsafe { probe_words(direct) };
                check_probe(words, slot, fcsr);
                #[cfg(target_arch = "riscv64")]
                assert_eq!(words[7], 1);
                #[cfg(target_arch = "loongarch64")]
                {
                    assert_eq!(words[6], 1);
                    assert_eq!(words[7], 0);
                    let fcc = user.context().fcc;
                    assert_eq!((fcc >> 24) & 0xff, 1);
                    assert_eq!((fcc >> 56) & 0xff, 0);
                    let context = user.context();
                    for lane in 0..user_fp_lanes() {
                        let expected = 0x8190 + (slot * 4 + lane) as u64;
                        assert_eq!(context.fpr[31].lanes[lane], expected);
                        let mut bytes = [0u8; 8];
                        let progress = user.mm().unwrap().copyin(&mut bytes, DATA + 128 + lane * 8);
                        assert_eq!((progress.copied, progress.error), (8, None));
                        assert_eq!(u64::from_le_bytes(bytes), expected);
                    }
                }
                let saved = Registers::read(&user);
                // fork 从真正执行过的完整现场派生，再让复制结果经过同一硬件进入/退出路径。
                let copied = user.context().fork();
                *user.context() = copied;
                interrupted(&user, cpu::current().id);
                assert!(
                    Registers::read(&user) == saved,
                    "copied user registers changed"
                );
                let id = task::current_task().id();
                for target in (0..cpu::count()).chain(0..cpu::count()) {
                    if stop.load(Ordering::Acquire) || token.stop_requested() {
                        return Ok(0);
                    }
                    sched::test_migrate_to(target);
                    assert_eq!(cpu::current().id, target);
                    assert_eq!(task::current_task().id(), id);
                    interrupted(&user, target);
                    assert!(
                        Registers::read(&user) == saved,
                        "user registers changed across interruption/migration"
                    );
                    assert_eq!(user.context().pc, CODE + arch::loop_spin_offset());
                    sched::cond_resched();
                }
                user.context().pc = CODE + arch::exit_offset();
                assert_eq!(event(&user, &token, &stop)?, UserTrap::Syscall);
                assert_eq!(user.context().gpr[A0], 43);
                assert_eq!(
                    user.context().fpr,
                    saved.fpr,
                    "user FP lanes changed across syscall"
                );
                Ok(43)
            })?;
        }
        fixture.joined(0, 43)?;
        fixture.joined(1, 43)
    }

    /// 真实偶奇页读写及未映射、PROT_NONE、缓存只读modify和独立NR权限错误。
    #[test]
    fn user_permissions(_: &mut TestContext<'_>, fixture: &mut Users) -> TestResult {
        for odd in [false, true] {
            let (mut mm, mut context, _) = image(arch::fault_offset(UserAccess::Read), 0)?;
            page(&mut mm, TARGET, RW, &0x9851u64.to_ne_bytes())?;
            page(
                &mut mm,
                TARGET + mm::PAGE_SIZE,
                RW,
                &0x9852u64.to_ne_bytes(),
            )?;
            context.gpr[A5] = TARGET + usize::from(odd) * mm::PAGE_SIZE;
            fixture.start(0, user(mm, context)?, move |user, token, stop| {
                assert_eq!(event(&user, &token, &stop)?, UserTrap::Syscall);
                assert_eq!(user.context().gpr[T0], if odd { 0x9852 } else { 0x9851 });
                user.context().pc = CODE + arch::fault_offset(UserAccess::Write);
                assert_eq!(event(&user, &token, &stop)?, UserTrap::Syscall);
                user.context().pc = CODE + arch::fault_offset(UserAccess::Read);
                assert_eq!(event(&user, &token, &stop)?, UserTrap::Syscall);
                assert_eq!(user.context().gpr[T0], 0);
                Ok(43)
            })?;
            fixture.joined(0, 43)?;
        }
        for (permissions, accesses) in [
            (
                None,
                &[UserAccess::Read, UserAccess::Write, UserAccess::Execute][..],
            ),
            (
                Some(NONE),
                &[UserAccess::Read, UserAccess::Write, UserAccess::Execute][..],
            ),
            (Some(RO), &[UserAccess::Write][..]),
            (Some(EX), &[UserAccess::Read, UserAccess::Write][..]),
        ] {
            for &access in accesses {
                permission_fault(fixture, permissions, access)?;
            }
        }
        Ok(())
    }

    /// 真正从已驻留只读数据页取指，保留硬件拒绝后的原目标PC与访问方向。
    #[test]
    fn user_no_execute(_: &mut TestContext<'_>, fixture: &mut Users) -> TestResult {
        permission_fault(fixture, Some(RO), UserAccess::Execute)
    }

    /// 同一正式用户入口验证当前权限，fault不推进PC，fixture仍等待原Task离栈。
    fn permission_fault(
        fixture: &mut Users,
        permissions: Option<PagePermissions>,
        access: UserAccess,
    ) -> TestResult {
        let (mut mm, mut context, _) = image(arch::fault_offset(access), 0)?;
        if let Some(permissions) = permissions {
            page(&mut mm, TARGET, permissions, &[])?;
        }
        context.gpr[A5] = TARGET;
        // LA只读modify先load填入TLB，未映射/PROT_NONE/纯执行页的store直接执行。
        #[cfg(target_arch = "loongarch64")]
        if access == UserAccess::Write && permissions != Some(RO) {
            context.pc += 4;
        }
        let fault_pc = context.pc;
        fixture.start(0, user(mm, context)?, move |user, token, stop| {
            assert_eq!(
                event(&user, &token, &stop)?,
                UserTrap::PageFault {
                    address: TARGET,
                    access
                },
                "permissions={:?} access={:?} fault_pc={:#x}",
                permissions,
                access,
                fault_pc
            );
            let pc = user.context().pc;
            if access == UserAccess::Execute {
                assert_eq!(pc, TARGET);
            } else {
                #[cfg(target_arch = "loongarch64")]
                let fault_pc = if access == UserAccess::Write && permissions == Some(RO) {
                    fault_pc + 4
                } else {
                    fault_pc
                };
                assert_eq!(pc, fault_pc);
            }
            Ok(19)
        })?;
        fixture.joined(0, 19)
    }

    /// 未准备中间页不发表槽，替换交回旧叶，解除末叶将表页保留到调用方完成失效后。
    #[test]
    fn user_table_ownership(_: &mut TestContext<'_>, _: &mut Users) -> TestResult {
        let mut table = UserPageTable::try_new().map_err(|_| TestError("NoMemory: table root"))?;
        let first = mm::alloc_pages(0, true).ok_or(TestError("NoMemory: first table leaf"))?;
        let second =
            mm::alloc_pages(0, true).ok_or(TestError("NoMemory: replacement table leaf"))?;
        let mut prepared = PreparedTables::default();
        // SAFETY: 本地表从未激活且由当前任务独占，两页完整清零、无用户或Rust排他访问；
        // 原页拥有保留到解除与失效后，prepared始终在所有软件修改结束后析构。
        unsafe {
            assert_eq!(
                table.map(TARGET, first.phys_addr(), RW, &mut prepared),
                Err(PageTableError::MissingTables)
            );
            assert!(table.lookup(TARGET).is_none());
            prepared = PreparedTables::try_new(table.needed_tables(TARGET))
                .map_err(|_| TestError("NoMemory: table middle pages"))?;
            assert!(table
                .map(TARGET, first.phys_addr(), RW, &mut prepared)
                .unwrap()
                .is_none());
            let old = table
                .map(TARGET, second.phys_addr(), RO, &mut prepared)
                .unwrap()
                .unwrap();
            assert_eq!(old.physical, first.phys_addr());
            assert_eq!(
                table.lookup(TARGET + 17).unwrap().physical,
                second.phys_addr()
            );
            let (old, retired) = table.unmap(TARGET);
            assert_eq!(old.unwrap().physical, second.phys_addr());
            assert!(table.lookup(TARGET).is_none());
            assert_eq!(table.needed_tables(TARGET), arch::USER_LEVELS - 1);
            arch::flush_user_translations();
            drop(retired);
        }
        drop(prepared);
        drop(table);
        drop(first);
        drop(second);
        Ok(())
    }
}

#[::ktest::suite(fixture = Users, suite_resources = crate::ktest::execution_services::SuiteBaseline, tags("user-vm"), timeout_ms = 15000)]
pub(crate) mod vm {
    use super::*;

    /// 首次实际用户 load/store 才安装匿名叶，相邻页及跨页复制保持完整零填语义。
    #[test]
    fn anonymous_first_touch(_: &mut TestContext<'_>, fixture: &mut Users) -> TestResult {
        let (mm, context, _) = image(arch::fault_offset(UserAccess::Read), 0)?;
        mm.map_anonymous(TARGET, 3 * mm::PAGE_SIZE, RW, RW).unwrap();
        assert!(mm.mapping(TARGET).is_none());
        assert!(mm.mapping(TARGET + mm::PAGE_SIZE).is_none());
        assert!(mm.mapping(TARGET + 2 * mm::PAGE_SIZE).is_none());
        fixture.start(0, user(mm, context)?, move |user, token, stop| {
            let mm = user.mm().unwrap();
            for address in [TARGET, TARGET + mm::PAGE_SIZE] {
                assert_eq!(
                    access(&user, &token, &stop, address, UserAccess::Read)?,
                    (1, 0)
                );
                let progress = mm.copyout(address, &0x9851u64.to_le_bytes());
                assert_eq!((progress.copied, progress.error), (8, None));
                assert_eq!(
                    access(&user, &token, &stop, address, UserAccess::Write)?.0,
                    0
                );
                assert_eq!(
                    access(&user, &token, &stop, address, UserAccess::Read)?.1,
                    0
                );
            }
            assert_eq!(
                access(
                    &user,
                    &token,
                    &stop,
                    TARGET + 2 * mm::PAGE_SIZE,
                    UserAccess::Write
                )?
                .0,
                1
            );
            assert_eq!(
                access(
                    &user,
                    &token,
                    &stop,
                    TARGET + 2 * mm::PAGE_SIZE,
                    UserAccess::Read
                )?
                .1,
                0
            );
            let mut crossing = [0xa5; 32];
            let progress = mm.copyin(&mut crossing, TARGET + mm::PAGE_SIZE - 16);
            assert_eq!((progress.copied, progress.error), (32, None));
            assert_eq!(crossing, [0; 32]);
            mm.unmap(TARGET, 3 * mm::PAGE_SIZE).unwrap();
            user.context().pc = CODE + arch::fault_offset(UserAccess::Read);
            user.context().gpr[A5] = TARGET;
            assert_eq!(
                event(&user, &token, &stop)?,
                UserTrap::PageFault {
                    address: TARGET,
                    access: UserAccess::Read,
                }
            );
            assert_eq!(mm.fault(TARGET, UserAccess::Read), Err(VmError::Unmapped));
            Ok(43)
        })?;
        fixture.joined(0, 43)
    }

    /// 私有 fork 后 copyout 与真实用户 store 分别拆页，父子内容互不污染且未写页继续共享。
    #[test]
    fn fork_copyout_and_user_store(_: &mut TestContext<'_>, fixture: &mut Users) -> TestResult {
        let (parent, context, _) = image(arch::fault_offset(UserAccess::Read), 0)?;
        parent
            .map_anonymous(TARGET, 2 * mm::PAGE_SIZE, RW, RW)
            .unwrap();
        for address in [TARGET, TARGET + mm::PAGE_SIZE] {
            let copied = parent.copyout(address, &0x9851u64.to_le_bytes());
            assert_eq!((copied.copied, copied.error), (8, None));
        }
        let child = parent.fork().unwrap();
        assert_eq!(
            parent.mapping(TARGET).unwrap().physical,
            child.mapping(TARGET).unwrap().physical
        );
        assert!(!parent.mapping(TARGET).unwrap().permissions.write);
        assert!(!child.mapping(TARGET).unwrap().permissions.write);
        let copied = child.copyout(TARGET, &0x9852u64.to_le_bytes());
        assert_eq!((copied.copied, copied.error), (8, None));
        assert_eq!(word(&parent, TARGET), 0x9851);
        assert_eq!(word(&child, TARGET), 0x9852);
        assert_ne!(
            parent.mapping(TARGET).unwrap().physical,
            child.mapping(TARGET).unwrap().physical
        );
        assert_eq!(
            parent.mapping(TARGET + mm::PAGE_SIZE).unwrap().physical,
            child.mapping(TARGET + mm::PAGE_SIZE).unwrap().physical
        );
        let parent = Arc::try_new(parent).map_err(|_| TestError("NoMemory: COW parent"))?;
        let child = Arc::try_new(child).map_err(|_| TestError("NoMemory: COW child"))?;
        let thread = UserThread::try_new(Arc::clone(&parent), context)
            .map_err(|_| TestError("NoMemory: COW user"))?;
        let child_check = Arc::clone(&child);
        fixture.start(0, thread, move |user, token, stop| {
            assert_eq!(
                access(&user, &token, &stop, TARGET, UserAccess::Write)?.0,
                1
            );
            assert_eq!(access(&user, &token, &stop, TARGET, UserAccess::Read)?.1, 0);
            assert_eq!(word(&child_check, TARGET), 0x9852);
            assert_eq!(word(&child_check, TARGET + mm::PAGE_SIZE), 0x9851);
            Ok(43)
        })?;
        fixture.joined(0, 43)?;
        assert_eq!(word(&parent, TARGET), 0);
        assert_eq!(word(&child, TARGET), 0x9852);
        drop(child);
        drop(parent);
        Ok(())
    }

    /// 只读但允许未来写的页在降升权后仍走 COW，两份同时故障和同 Mm 竞争都重新核对叶。
    #[test]
    #[ktest(min_cpus = 2)]
    fn readonly_future_write_and_racing_faults(
        _: &mut TestContext<'_>,
        fixture: &mut Users,
    ) -> TestResult {
        for same_mm in [false, true] {
            let controls = Controls::new(fixture)?;
            let (parent, _, _) = image(arch::fault_offset(UserAccess::Write), 0)?;
            parent.map_anonymous(TARGET, mm::PAGE_SIZE, RW, RW).unwrap();
            assert_eq!(parent.copyout(TARGET, &0x9851u64.to_le_bytes()).error, None);
            parent.protect(TARGET, mm::PAGE_SIZE, RO).unwrap();
            let child = parent.fork().unwrap();
            let untouched =
                Arc::try_new(child).map_err(|_| TestError("NoMemory: readonly child"))?;
            let parent =
                Arc::try_new(parent).map_err(|_| TestError("NoMemory: readonly parent"))?;
            let shared = parent.mapping(TARGET).unwrap().physical;
            assert_eq!(shared, untouched.mapping(TARGET).unwrap().physical);
            parent.protect(TARGET, mm::PAGE_SIZE, RW).unwrap();
            parent.protect(TARGET, mm::PAGE_SIZE, RO).unwrap();
            parent.protect(TARGET, mm::PAGE_SIZE, RW).unwrap();
            if !same_mm {
                untouched.protect(TARGET, mm::PAGE_SIZE, RW).unwrap();
            }
            for slot in 0..2 {
                let owner = if slot == 0 || same_mm {
                    Arc::clone(&parent)
                } else {
                    Arc::clone(&untouched)
                };
                let mut context =
                    UserContext::new(CODE + arch::fault_offset(UserAccess::Write), STACK_TOP);
                context.gpr[A5] = TARGET;
                let thread = UserThread::try_new(owner, context)
                    .map_err(|_| TestError("NoMemory: racing COW user"))?;
                let controls = Arc::clone(&controls);
                fixture.start(slot, thread, move |user, token, stop| {
                    sched::test_migrate_to(slot);
                    assert_eq!(
                        event(&user, &token, &stop)?,
                        UserTrap::PageFault {
                            address: TARGET,
                            access: UserAccess::Write,
                        }
                    );
                    let pc = user.context().pc;
                    controls.ready[slot].store(true, Ordering::Release);
                    controls.gates[slot].wait();
                    if stop.load(Ordering::Acquire) || token.stop_requested() {
                        return Ok(0);
                    }
                    sched::test_migrate_to(slot);
                    let mm = user.mm().unwrap();
                    mm.fault(TARGET, UserAccess::Write).unwrap();
                    assert_eq!(user.context().pc, pc);
                    assert_eq!(event(&user, &token, &stop)?, UserTrap::Syscall);
                    assert_eq!(access(&user, &token, &stop, TARGET, UserAccess::Read)?.1, 0);
                    Ok(43)
                })?;
            }
            until(
                || {
                    controls.ready[..2]
                        .iter()
                        .all(|ready| ready.load(Ordering::Acquire))
                },
                "COW participants did not fault before release",
            )?;
            controls.gates[0].complete();
            controls.gates[1].complete();
            fixture.joined(0, 43)?;
            fixture.joined(1, 43)?;
            assert_eq!(word(&parent, TARGET), 0);
            assert_ne!(parent.mapping(TARGET).unwrap().physical, shared);
            assert_eq!(word(&untouched, TARGET), if same_mm { 0x9851 } else { 0 });
            if !same_mm {
                assert_ne!(
                    parent.mapping(TARGET).unwrap().physical,
                    untouched.mapping(TARGET).unwrap().physical
                );
            }
            drop(untouched);
            drop(parent);
        }
        Ok(())
    }

    /// 真实跨页访问保留失败前缀，字符串保留原始字节，原子独立访问遵守对齐与长度边界。
    #[test]
    fn bounded_uaccess(_: &mut TestContext<'_>, fixture: &mut Users) -> TestResult {
        let (mm, context, _) = image(arch::fault_offset(UserAccess::Read), 0)?;
        mm.map_anonymous(TARGET, 2 * mm::PAGE_SIZE, RW, RW).unwrap();
        let high = arch::USER_ADDRESS_END - mm::PAGE_SIZE;
        mm.map_anonymous(high, mm::PAGE_SIZE, RW, RW).unwrap();
        fixture.start(0, user(mm, context)?, move |user, token, stop| {
            let mm = user.mm().unwrap();
            let boundary = TARGET + mm::PAGE_SIZE;
            for offset in 0..8 {
                for length in 0..17 {
                    let address = boundary - 8 + offset;
                    let source = core::array::from_fn::<_, 24, _>(|index| (index * 13 + 7) as u8);
                    let copied = mm.copyout(address, &source[..length]);
                    assert_eq!((copied.copied, copied.error), (length, None));
                    let mut result = [0xa5; 24];
                    let copied = mm.copyin(&mut result[..length], address);
                    assert_eq!((copied.copied, copied.error), (length, None));
                    assert_eq!(&result[..length], &source[..length]);
                    assert_eq!(&result[length..], &[0xa5; 24][length..]);
                }
            }
            let number = 0x0102030405060708u64;
            assert_eq!(mm.copyout(boundary - 3, &number.to_le_bytes()).error, None);
            assert_eq!(mm.read_u64(boundary - 3), Ok(number));
            assert_eq!(mm.read_u32(boundary - 3), Ok(0x05060708));
            assert_eq!(mm.read_usize(boundary - 3), Ok(number as usize));
            assert_eq!(mm.copyout(boundary - 2, &[0xff, 0xfe, 0x80, 0]).error, None);
            assert_eq!(
                mm.read_c_string(boundary - 2, 4).unwrap(),
                [0xff, 0xfe, 0x80]
            );
            assert_eq!(
                mm.read_c_string(boundary - 2, 3),
                Err(CStringError::Unterminated)
            );
            assert_eq!(
                mm.read_c_string(usize::MAX, 0),
                Err(CStringError::Unterminated)
            );
            assert_eq!(
                mm.read_c_string(usize::MAX, 2),
                Err(CStringError::Vm(VmError::InvalidRange))
            );
            let high = arch::USER_ADDRESS_END - 2;
            assert_eq!(mm.copyout(high, &[0xfe, 0]).error, None);
            assert_eq!(mm.read_c_string(high, 32).unwrap(), [0xfe]);
            let mut high_bytes = [0; 2];
            assert_eq!(mm.copyin(&mut high_bytes, high).error, None);
            assert_eq!(high_bytes, [0xfe, 0]);
            assert_eq!(mm.copyout(usize::MAX, &[]).copied, 0);
            assert_eq!(mm.copyout(usize::MAX, &[]).error, None);
            assert_eq!(mm.copyin(&mut [], usize::MAX).error, None);
            let overflow = mm.copyout(usize::MAX - 1, &[1, 2, 3]);
            assert_eq!(
                (overflow.copied, overflow.error),
                (0, Some(VmError::InvalidRange))
            );
            let mut overflow_bytes = [0xa5; 3];
            let overflow = mm.copyin(&mut overflow_bytes, usize::MAX - 1);
            assert_eq!(
                (overflow.copied, overflow.error),
                (0, Some(VmError::InvalidRange))
            );
            assert_eq!(overflow_bytes, [0xa5; 3]);
            for address in [TARGET, boundary - 4, boundary] {
                mm.store_user_u32(address, 0x81234567).unwrap();
                assert_eq!(mm.load_user_u32(address), Ok(0x81234567));
                assert_eq!(mm.read_u32(address), Ok(0x81234567));
            }
            let child = mm.fork().unwrap();
            child.store_user_u32(TARGET + 128, 0x78563412).unwrap();
            assert_eq!(mm.load_user_u32(TARGET + 128), Ok(0));
            mm.store_user_u32(TARGET + 128, 0x12345678).unwrap();
            assert_eq!(child.load_user_u32(TARGET + 128), Ok(0x78563412));
            assert_eq!(mm.load_user_u32(TARGET + 128), Ok(0x12345678));
            drop(child);
            for address in [TARGET + 1, TARGET + 2, TARGET + 3, boundary - 2, usize::MAX] {
                assert_eq!(mm.load_user_u32(address), Err(VmError::InvalidRange));
                assert_eq!(mm.store_user_u32(address, 7), Err(VmError::InvalidRange));
            }
            assert_eq!(
                access(&user, &token, &stop, boundary, UserAccess::Read)?.1 & 0xffffffff,
                0x81234567
            );
            mm.protect(boundary, mm::PAGE_SIZE, RO).unwrap();
            let partial = mm.copyout(boundary - 2, &[11, 12, 13, 14]);
            assert_eq!(
                (partial.copied, partial.error),
                (2, Some(VmError::Permission))
            );
            let mut prefix = [0; 4];
            assert_eq!(mm.copyin(&mut prefix, boundary - 2).error, None);
            assert_eq!(prefix, [11, 12, 0x67, 0x45]);
            assert_eq!(mm.store_user_u32(boundary, 7), Err(VmError::Permission));
            mm.unmap(boundary, mm::PAGE_SIZE).unwrap();
            let mut result = [0xa5; 4];
            let partial = mm.copyin(&mut result, boundary - 2);
            assert_eq!(
                (partial.copied, partial.error),
                (2, Some(VmError::Unmapped))
            );
            assert_eq!(result, [11, 12, 0xa5, 0xa5]);
            assert_eq!(mm.read_u32(boundary - 2), Err(VmError::Unmapped));
            assert_eq!(
                mm.read_c_string(boundary - 2, 4),
                Err(CStringError::Vm(VmError::Unmapped))
            );
            assert_eq!(mm.load_user_u32(boundary), Err(VmError::Unmapped));
            let partial = mm.copyout(boundary - 2, &[21, 22, 23, 24]);
            assert_eq!(
                (partial.copied, partial.error),
                (2, Some(VmError::Unmapped))
            );
            let mut prefix = [0; 2];
            assert_eq!(mm.copyin(&mut prefix, boundary - 2).error, None);
            assert_eq!(prefix, [21, 22]);
            Ok(43)
        })?;
        fixture.joined(0, 43)
    }

    /// 原读者并持并真睡眠，写者和后来读者真实 park；最后读者放锁才依次交给写者、读者。
    #[test]
    #[ktest(min_cpus = 2)]
    fn sleeping_rwlock_order(_: &mut TestContext<'_>, fixture: &mut Users) -> TestResult {
        let controls = Controls::new(fixture)?;
        for slot in 0..2 {
            let controls = Arc::clone(&controls);
            fixture.start_task(slot, move |token, stop| {
                controls.record(slot);
                let id = task::current_task().id();
                let read = controls.lock.read();
                assert_eq!(*read, 0x9817);
                {
                    let waiter = crate::wait::Waiter::new();
                    assert!(waiter.waker().notify());
                    waiter.park();
                    assert_eq!(*read, 0x9817);
                }
                controls.ready[slot].store(true, Ordering::Release);
                controls.gates[slot].wait();
                if stop.load(Ordering::Acquire) || token.stop_requested() {
                    return Ok(0);
                }
                for target in 0..cpu::count() {
                    sched::test_migrate_to(target);
                    assert_eq!(task::current_task().id(), id);
                    assert_eq!(cpu::current().id, target);
                    assert_eq!(*read, 0x9817);
                }
                controls.phase.fetch_add(1, Ordering::AcqRel);
                drop(read);
                Ok(11 + slot as i32)
            })?;
        }
        until(
            || {
                controls.ready[..2]
                    .iter()
                    .all(|ready| ready.load(Ordering::Acquire))
                    && controls.blocked(0)
                    && controls.blocked(1)
            },
            "rwlock readers did not sleep while sharing the guard",
        )?;
        let writer = Arc::clone(&controls);
        fixture.start_task(2, move |token, stop| {
            writer.record(2);
            let mut write = writer.lock.write();
            if stop.load(Ordering::Acquire) || token.stop_requested() {
                return Ok(0);
            }
            assert_eq!(writer.phase.load(Ordering::Acquire), 2);
            assert_eq!(*write, 0x9817);
            *write = 0x9818;
            writer.phase.store(3, Ordering::Release);
            writer.ready[2].store(true, Ordering::Release);
            writer.gates[2].wait();
            if stop.load(Ordering::Acquire) || token.stop_requested() {
                return Ok(0);
            }
            sched::test_migrate_to((cpu::current().id + 1) % cpu::count());
            assert_eq!(*write, 0x9818);
            drop(write);
            Ok(13)
        })?;
        until(
            || controls.blocked(2),
            "rwlock writer did not park behind two readers",
        )?;
        assert!(controls.lock.try_read().is_none());
        assert!(controls.lock.try_write().is_none());
        let later = Arc::clone(&controls);
        fixture.start_task(3, move |token, stop| {
            later.record(3);
            let read = later.lock.read();
            if stop.load(Ordering::Acquire) || token.stop_requested() {
                return Ok(0);
            }
            assert_eq!(later.phase.load(Ordering::Acquire), 3);
            assert_eq!(*read, 0x9818);
            later.phase.store(4, Ordering::Release);
            Ok(14)
        })?;
        until(
            || controls.blocked(3),
            "late rwlock reader bypassed a waiting writer",
        )?;
        controls.gates[0].complete();
        fixture.joined(0, 11)?;
        assert_eq!(controls.phase.load(Ordering::Acquire), 1);
        assert!(controls.blocked(2));
        assert!(controls.blocked(3));
        controls.gates[1].complete();
        fixture.joined(1, 12)?;
        until(
            || controls.ready[2].load(Ordering::Acquire) && controls.blocked(2),
            "rwlock writer did not acquire after the last reader",
        )?;
        assert!(controls.blocked(3));
        assert_eq!(controls.phase.load(Ordering::Acquire), 3);
        controls.gates[2].complete();
        fixture.joined(2, 13)?;
        fixture.joined(3, 14)?;
        assert_eq!(controls.phase.load(Ordering::Acquire), 4);
        assert_eq!(*controls.lock.try_read().unwrap(), 0x9818);
        assert_eq!(*controls.lock.try_write().unwrap(), 0x9818);
        Ok(())
    }

    /// 独立顶层路径的末叶解除归还全部空表，真实用户根先离栈才进入最终全域归还账。
    #[test]
    fn last_leaf_and_inactive_root(_: &mut TestContext<'_>, fixture: &mut Users) -> TestResult {
        let address = 0x40000000;
        let (mm, mut context, _) = image(arch::fault_offset(UserAccess::Read), 0)?;
        mm.map_anonymous(address, mm::PAGE_SIZE, RW, RW).unwrap();
        let mm = Arc::try_new(mm).map_err(|_| TestError("NoMemory: final leaf MM"))?;
        let controls = Controls::new(fixture)?;
        context.gpr[A5] = address;
        let thread = UserThread::try_new(Arc::clone(&mm), context)
            .map_err(|_| TestError("NoMemory: final leaf user"))?;
        let control = Arc::clone(&controls);
        fixture.start(0, thread, move |user, token, stop| {
            control.record(0);
            assert_eq!(
                access(&user, &token, &stop, address, UserAccess::Read)?,
                (1, 0)
            );
            control.gates[0].wait();
            if stop.load(Ordering::Acquire) || token.stop_requested() {
                return Ok(0);
            }
            assert_eq!(
                access(&user, &token, &stop, address, UserAccess::Write)?.0,
                0
            );
            Ok(43)
        })?;
        until(
            || controls.gates[0].test_waiting() == 1 && controls.blocked(0),
            "final leaf user did not pause after activating its root",
        )?;
        controls.gates[0].complete();
        fixture.joined(0, 43)?;
        let resident = baseline()?;
        assert!(mm.mapping(address).is_some());
        mm.unmap(address, mm::PAGE_SIZE).unwrap();
        assert!(mm.mapping(address).is_none());
        let empty = baseline()?;
        assert_eq!(
            empty.0.buddy_free_pages + empty.0.slab_backing_pages,
            resident.0.buddy_free_pages + resident.0.slab_backing_pages + arch::USER_LEVELS,
        );
        mm.map_anonymous(address, mm::PAGE_SIZE, RW, RW).unwrap();
        mm.fault(address, UserAccess::Read).unwrap();
        let refilled = baseline()?;
        assert_eq!(
            refilled.0.buddy_free_pages + refilled.0.slab_backing_pages,
            resident.0.buddy_free_pages + resident.0.slab_backing_pages,
        );
        mm.unmap(address, mm::PAGE_SIZE).unwrap();
        assert!(mm.mapping(address).is_none());
        // SLUB 的空页归还可改变临时分布，独占数据页与表页的归还总数须恢复。
        let released = baseline()?.0;
        assert_eq!(
            released.buddy_free_pages + released.slab_backing_pages,
            empty.0.buddy_free_pages + empty.0.slab_backing_pages,
        );
        drop(mm);
        Ok(())
    }

    /// 远核保持同一 Running Task 和活动根；有界握手只安排改变，结果来自后续真实用户指令。
    fn phase(
        controls: &Controls,
        expected: usize,
        token: &ChildToken,
        stop: &AtomicBool,
    ) -> TestResult {
        let end = crate::time::now()
            .checked_add(crate::time::Duration::from_secs(5))
            .ok_or(TestError("live translation deadline overflow"))?;
        while controls.phase.load(Ordering::Acquire) < expected {
            if stop.load(Ordering::Acquire) || token.stop_requested() {
                return Err(TestError("live translation participant stopped"));
            }
            if end <= crate::time::now() {
                return Err(TestError("live translation coordinator timeout"));
            }
            core::hint::spin_loop();
        }
        assert_eq!(cpu::current().id, 1);
        Ok(())
    }

    /// 不经调度安全点重进用户指令，保住上次真正命中的远核翻译以检验同步失效。
    fn live_access(
        user: &UserThread,
        token: &ChildToken,
        stop: &AtomicBool,
        direction: UserAccess,
    ) -> Result<UserTrap, TestError> {
        {
            let mut context = user.context();
            context.pc = CODE + arch::fault_offset(direction);
            context.gpr[A5] = TARGET;
        }
        let end = crate::time::now()
            .checked_add(crate::time::Duration::from_secs(5))
            .ok_or(TestError("live user deadline overflow"))?;
        loop {
            if stop.load(Ordering::Acquire) || token.stop_requested() {
                return Err(TestError("live user participant stopped"));
            }
            if end <= crate::time::now() {
                return Err(TestError("live user instruction timeout"));
            }
            assert_eq!(cpu::current().id, 1);
            let trap = user.run_user();
            if trap != UserTrap::Interrupt {
                return Ok(trap);
            }
        }
    }

    /// 撤权、COW换页、解除重建与最终解除后，同核同根重进必须观察新内容和真实拒绝方向。
    #[test]
    #[ktest(min_cpus = 2)]
    fn remote_cached_translation(_: &mut TestContext<'_>, fixture: &mut Users) -> TestResult {
        sched::test_migrate_to(0);
        let controls = Controls::new(fixture)?;
        let (mm, context, _) = image(arch::fault_offset(UserAccess::Read), 0)?;
        mm.map_anonymous(TARGET, mm::PAGE_SIZE, RW, RW).unwrap();
        assert_eq!(mm.copyout(TARGET, &0x9851u64.to_le_bytes()).error, None);
        let mm = Arc::try_new(mm).map_err(|_| TestError("NoMemory: live translation MM"))?;
        let thread = UserThread::try_new(Arc::clone(&mm), context)
            .map_err(|_| TestError("NoMemory: live translation user"))?;
        let control = Arc::clone(&controls);
        fixture.start(0, thread, move |user, token, stop| {
            sched::test_migrate_to(1);
            let id = task::current_task().id();
            assert_eq!(
                live_access(&user, &token, &stop, UserAccess::Read)?,
                UserTrap::Syscall
            );
            assert_eq!(user.context().gpr[T0], 0x9851);
            assert_eq!(
                live_access(&user, &token, &stop, UserAccess::Write)?,
                UserTrap::Syscall
            );
            assert_eq!(
                live_access(&user, &token, &stop, UserAccess::Read)?,
                UserTrap::Syscall
            );
            assert_eq!(user.context().gpr[T0], 0);
            control.ready[0].store(true, Ordering::Release);
            phase(&control, 1, &token, &stop)?;
            assert_eq!(
                live_access(&user, &token, &stop, UserAccess::Write)?,
                UserTrap::PageFault {
                    address: TARGET,
                    access: UserAccess::Write,
                }
            );
            assert_eq!(
                user.mm().unwrap().fault(TARGET, UserAccess::Write),
                Err(VmError::Permission)
            );
            control.ready[1].store(true, Ordering::Release);
            phase(&control, 2, &token, &stop)?;
            assert_eq!(
                live_access(&user, &token, &stop, UserAccess::Read)?,
                UserTrap::Syscall
            );
            assert_eq!(user.context().gpr[T0], 0x9852);
            assert_eq!(
                live_access(&user, &token, &stop, UserAccess::Write)?,
                UserTrap::Syscall
            );
            control.ready[2].store(true, Ordering::Release);
            phase(&control, 3, &token, &stop)?;
            assert_eq!(
                live_access(&user, &token, &stop, UserAccess::Read)?,
                UserTrap::Syscall
            );
            assert_eq!(user.context().gpr[T0], 0x9853);
            assert_eq!(
                live_access(&user, &token, &stop, UserAccess::Write)?,
                UserTrap::PageFault {
                    address: TARGET,
                    access: UserAccess::Write,
                }
            );
            control.ready[3].store(true, Ordering::Release);
            phase(&control, 4, &token, &stop)?;
            assert_eq!(
                live_access(&user, &token, &stop, UserAccess::Read)?,
                UserTrap::PageFault {
                    address: TARGET,
                    access: UserAccess::Read,
                }
            );
            assert_eq!(
                user.mm().unwrap().fault(TARGET, UserAccess::Read),
                Err(VmError::Unmapped)
            );
            assert_eq!(task::current_task().id(), id);
            Ok(43)
        })?;
        until(
            || controls.ready[0].load(Ordering::Acquire),
            "remote user did not warm its translation",
        )?;
        sched::test_migrate_to(0);
        mm.protect(TARGET, mm::PAGE_SIZE, RO).unwrap();
        controls.phase.store(1, Ordering::Release);
        until(
            || controls.ready[1].load(Ordering::Acquire),
            "remote user retained stale write permission",
        )?;
        sched::test_migrate_to(0);
        mm.protect(TARGET, mm::PAGE_SIZE, RW).unwrap();
        let old = mm.fork().unwrap();
        let copied = mm.copyout(TARGET, &0x9852u64.to_le_bytes());
        assert_eq!((copied.copied, copied.error), (8, None));
        assert_eq!(word(&old, TARGET), 0);
        assert_ne!(
            old.mapping(TARGET).unwrap().physical,
            mm.mapping(TARGET).unwrap().physical
        );
        controls.phase.store(2, Ordering::Release);
        until(
            || controls.ready[2].load(Ordering::Acquire),
            "remote user retained stale page contents",
        )?;
        sched::test_migrate_to(0);
        let replaced = mm.fork().unwrap();
        mm.unmap(TARGET, mm::PAGE_SIZE).unwrap();
        mm.map_anonymous(TARGET, mm::PAGE_SIZE, RW, RW).unwrap();
        assert_eq!(mm.copyout(TARGET, &0x9853u64.to_le_bytes()).error, None);
        mm.protect(TARGET, mm::PAGE_SIZE, RO).unwrap();
        assert_ne!(
            replaced.mapping(TARGET).unwrap().physical,
            mm.mapping(TARGET).unwrap().physical
        );
        assert_eq!(word(&replaced, TARGET), 0);
        controls.phase.store(3, Ordering::Release);
        until(
            || controls.ready[3].load(Ordering::Acquire),
            "remote user did not observe rebuilt read-only leaf",
        )?;
        sched::test_migrate_to(0);
        mm.unmap(TARGET, mm::PAGE_SIZE).unwrap();
        controls.phase.store(4, Ordering::Release);
        fixture.joined(0, 43)?;
        drop(replaced);
        drop(old);
        drop(mm);
        Ok(())
    }
}

/// 只保留宏产生的真实注册槽，不建立用户用例中央列表。
pub(crate) fn anchors() -> [&'static ::ktest::SuiteSlot; 2] {
    [entry::__ktest_anchor(), vm::__ktest_anchor()]
}
