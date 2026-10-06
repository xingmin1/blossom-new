//! 真实用户指令、私有现场、权限异常及页表拥有的逐例回归。

use alloc::sync::Arc;
use core::hint::black_box;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::arch::{self, UserAccess, UserContext, UserTrap};
use crate::ktest::execution_core::{baseline, until, Baseline};
use crate::ktest::execution_services::SuiteBaseline;
use crate::ktest::{prepare_result, ChildHandle, ChildToken};
use crate::mm::space::Mm;
use crate::mm::user_table::{PagePermissions, PageTableError, PreparedTables, UserPageTable};
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
    handles: [Option<ChildHandle>; 2],
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
        })
    }

    /// 仅在真实用户中断返回的普通 Task 安全点停止，离栈前保留全部拥有。
    fn quiesce(&mut self, _: &mut TestContext<'_>) -> TestResult {
        self.stop.store(true, Ordering::Release);
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
        } = self;
        assert!(handles
            .iter()
            .flatten()
            .all(|handle| handle.try_join().is_some()));
        drop(handles);
        drop(stop);
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
    unsafe { mm.map_owned(address, pages, permissions) }
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
    context.fpr = core::array::from_fn(|index| 0x3ff0000000000000 + (slot * 32 + index) as u64);
    context.fpr[0] = if slot == 0 {
        0x3ff4000000000000
    } else {
        0x4020000000000000
    };
    context.fpr[1] = if slot == 0 {
        0x4004000000000000
    } else {
        0x4030000000000000
    };
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
    fpr: [u64; 32],
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
                }
                let saved = Registers::read(&user);
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

/// 只保留宏产生的真实注册槽，不建立用户用例中央列表。
pub(crate) fn anchors() -> [&'static ::ktest::SuiteSlot; 1] {
    [entry::__ktest_anchor()]
}
