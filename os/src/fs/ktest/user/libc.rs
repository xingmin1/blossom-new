//! 静态 libc、真实脚本解释和确定迁核复用同一 UserOwner 生命周期。

use super::*;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::fs::{DescriptorFlags, FileRef};
use crate::task::exec::{self, Arguments, LoadedImage};
use crate::task::{self, Credentials, Placement, State, UserRuntime};
use crate::{cpu, sched};

const LIBC: &[u8] = b"/bin/libc-smoke";
const BUSYBOX: &[u8] = b"/bin/busybox";
const BASE_ABI: &[u8] = b"/test/scripts/base-abi";
const SCRIPT_ENV: &[u8] = b"BLOSSOM_SCRIPT=stage7";

impl UserOwner {
    /// 在完整标准流和真实执行映像上运行静态 libc smoke，并消费原 wait/reap 资源。
    pub(super) fn libc_static_smoke(&mut self, context: &mut TestContext<'_>) -> TestResult {
        self.prepare_tty(context, 4)?;
        let image = self.load_named(context, LIBC, b"stage7")?;
        let target = (cpu::current().id + 1) % cpu::count();
        self.start_console_program(context, image, Placement::Cpu(target), Credentials::ROOT)?;
        let event = self.wait(context)?;
        context.expect_eq(event.status, 0);
        if event.status != 0 {
            self.graph().request_root_exit(9);
        }
        self.reap_children(context, event.status != 0)?;
        context.diagnostic(format_args!(
            "libc-static status={} target_cpu={} console_fds=0,1,2",
            event.status, target
        ));
        Ok(())
    }

    /// 让同一 Any Task 在两个真实 pipe 阻塞窗口指定 CPU0、CPU1，再消费完整用户树。
    pub(super) fn libc_migration(&mut self, context: &mut TestContext<'_>) -> TestResult {
        self.prepare_tty(context, 4)?;
        let (ready_reader, ready_writer) = checked(context, crate::fs::pipe::try_new(0, 0, 0))?;
        let (go_reader, go_writer) = checked(context, crate::fs::pipe::try_new(0, 0, 0))?;
        let runtime = checked(
            context,
            UserRuntime::try_new(
                self.path.vfs().clone(),
                self.path.context().clone(),
                Credentials::ROOT,
            ),
        )?;
        checked(context, runtime.install_console())?;
        let ready_write = publish_descriptor(context, &runtime, ready_writer)?;
        let go_read = publish_descriptor(context, &runtime, go_reader)?;
        let (ready_text, ready_start) = decimal_fd(ready_write);
        let (go_text, go_start) = decimal_fd(go_read);
        let arguments = Arguments::from_kernel(
            LIBC,
            &[
                LIBC,
                b"stage7",
                b"--migrate",
                &ready_text[ready_start..],
                &go_text[go_start..],
            ],
            &[],
        )
        .map_err(|_| TestError("libc migration arguments failed"))?;
        let image = exec::prepare(
            self.path.vfs(),
            self.path.context(),
            arguments,
            Credentials::ROOT,
        )
        .map_err(|error| {
            context.diagnostic(format_args!("libc-image-prepare error={:?}", error));
            TestError("libc image preparation failed")
        })?;
        let prepared = task::prepare_program(self.graph(), None, image, runtime, Placement::Any)
            .map_err(|_| TestError("libc migration task preparation failed"))?;
        let pid = prepared.pid();
        let user = prepared.user();
        let task = user
            .executor()
            .ok_or(TestError("libc migration executor disappeared"))?;
        let initial_mask = Arc::try_new(AtomicUsize::new(0))
            .map_err(|_| TestError("migration initial mask allocation failed"))?;
        let first_mask = Arc::try_new(AtomicUsize::new(0))
            .map_err(|_| TestError("migration first mask allocation failed"))?;
        let coordination_task = task.clone();
        let coordination_user = user.clone();
        let initial_observed = initial_mask.clone();
        let first_observed = first_mask.clone();
        let coordinator = task::prepare_kernel(move || {
            for target in 0..2 {
                let mut byte = [0u8; 1];
                if ready_reader.read(&mut byte) != Ok(1) || byte[0] != b'R' {
                    return 1;
                }
                while sched::test_state(&coordination_task) != State::Blocked {
                    if coordination_user.mm().is_none() {
                        return 1;
                    }
                    sched::yield_now();
                }
                if !coordination_user
                    .executor()
                    .is_some_and(|executor| Arc::ptr_eq(&executor, &coordination_task))
                {
                    return 1;
                }
                let mask = coordination_user.ktest_take_user_cpu_mask();
                if target == 0 {
                    initial_observed.store(mask, Ordering::Release);
                } else {
                    first_observed.store(mask, Ordering::Release);
                }
                if mask == 0 || (target == 1 && mask & 1 == 0) {
                    return 1;
                }
                sched::test_request_migration(&coordination_task, target);
                if go_writer.write(b"M") != Ok(1) {
                    return 1;
                }
            }
            0
        })
        .map_err(|_| TestError("libc migration coordinator preparation failed"))?;
        let join = prepared
            .publish()
            .map_err(|_| TestError("libc migration publication failed"))?;
        self.running = Some(RunningProgram {
            pid,
            target: None,
            coordinator: cpu::current().id,
            expected_task: Some(join.task_id()),
            join,
        });
        self.observer = Some(coordinator.publish());
        let event = self.wait(context)?;
        let final_mask = user.ktest_take_user_cpu_mask();
        context.expect_eq(final_mask & (1usize << 1) != 0, true);
        context.expect_eq(event.status, 0);
        if event.status != 0 {
            self.graph().request_root_exit(9);
        }
        self.reap_children(context, event.status != 0)?;
        context.expect_eq(self.observer.take().unwrap().join(), 0);
        context.diagnostic(format_args!(
            "libc-migration task={} status={} blocked_windows=2 initial_mask={:#x} cpu0_mask={:#x} final_mask={:#x}",
            task.id().as_usize(),
            event.status,
            initial_mask.load(Ordering::Acquire),
            first_mask.load(Ordering::Acquire),
            final_mask
        ));
        Ok(())
    }

    /// 逐个运行普通脚本和 BusyBox 基础 ABI，并让 C 自身核对脚本失败后的旧映像。
    pub(super) fn script_exec(&mut self, context: &mut TestContext<'_>) -> TestResult {
        self.prepare_tty(context, 8)?;
        for script in [
            b"/test/scripts/no-option".as_slice(),
            b"/test/scripts/option".as_slice(),
            b"/test/scripts/chain1".as_slice(),
        ] {
            let arguments = Arguments::from_kernel(script, &[script, b"tail"], &[SCRIPT_ENV])
                .map_err(|_| TestError("script arguments failed"))?;
            let image = exec::prepare(
                self.path.vfs(),
                self.path.context(),
                arguments,
                Credentials::ROOT,
            )
            .map_err(|error| {
                context.diagnostic(format_args!("script-image-prepare error={:?}", error));
                TestError("script image preparation failed")
            })?;
            self.start_console_program(context, image, Placement::Any, Credentials::ROOT)?;
            let event = self.wait(context)?;
            context.expect_eq(event.status, 0);
            if event.status != 0 {
                self.graph().request_root_exit(9);
            }
            self.reap_children(context, event.status != 0)?;
        }
        self.run_console_named(
            context,
            LIBC,
            &[b"--script-errors", b"stage7"],
            Credentials::ROOT,
        )?;
        self.run_console_named(context, BUSYBOX, &[b"sh", BASE_ABI], Credentials::ROOT)?;
        self.run_console_named(
            context,
            LIBC,
            &[b"--abi-user", b"stage7"],
            Credentials {
                uid: 1000,
                gid: 2000,
            },
        )?;
        context.diagnostic(format_args!(
            "script-exec scripts=no-option,option,chain1 errors=eloop,enoent,eacces,enoexec,e2big base-abi=busybox"
        ));
        Ok(())
    }

    /// 给明确选择 console 的普通入口安装标准流后发表用户任务，旧汇编入口不经过此路径。
    fn start_console_program(
        &mut self,
        context: &TestContext<'_>,
        image: LoadedImage,
        placement: Placement,
        credentials: Credentials,
    ) -> Result<(), TestError> {
        let runtime = checked(
            context,
            UserRuntime::try_new(
                self.path.vfs().clone(),
                self.path.context().clone(),
                credentials,
            ),
        )?;
        checked(context, runtime.install_console())?;
        let target = match &placement {
            Placement::Cpu(target) => Some(*target),
            Placement::Any => None,
        };
        let prepared = task::prepare_program(self.graph(), None, image, runtime, placement)
            .map_err(|_| TestError("console user task preparation failed"))?;
        let pid = prepared.pid();
        let expected_task = prepared.user().executor();
        let join = prepared
            .publish()
            .map_err(|_| TestError("console user publication failed"))?;
        self.running = Some(RunningProgram {
            pid,
            target,
            coordinator: cpu::current().id,
            expected_task: expected_task.map(|task| task.id()),
            join,
        });
        Ok(())
    }

    /// 以实际 argv/env 准备一个明确 console 的普通程序并消费退出树。
    fn run_console_named(
        &mut self,
        context: &mut TestContext<'_>,
        program: &[u8],
        tail: &[&[u8]],
        credentials: Credentials,
    ) -> TestResult {
        let mut argv = Vec::new();
        argv.try_reserve_exact(tail.len() + 1)
            .map_err(|_| TestError("console argv allocation failed"))?;
        argv.push(program);
        argv.extend_from_slice(tail);
        let env = if program == BUSYBOX {
            &[SCRIPT_ENV][..]
        } else {
            &[][..]
        };
        let arguments = Arguments::from_kernel(program, &argv, env)
            .map_err(|_| TestError("console named arguments failed"))?;
        let image = exec::prepare(self.path.vfs(), self.path.context(), arguments, credentials)
            .map_err(|error| {
                context.diagnostic(format_args!("console-image-prepare error={:?}", error));
                TestError("console image preparation failed")
            })?;
        self.start_console_program(context, image, Placement::Any, credentials)?;
        let event = self.wait(context)?;
        context.expect_eq(event.status, 0);
        if event.status != 0 {
            self.graph().request_root_exit(9);
        }
        self.reap_children(context, event.status != 0)
    }
}

/// 把真实文件表预留发布为一个普通继承描述符，不依赖数值槽特判。
fn publish_descriptor(
    context: &TestContext<'_>,
    runtime: &UserRuntime,
    file: FileRef,
) -> Result<usize, TestError> {
    Ok(checked(context, runtime.files.reserve())?.publish(file, DescriptorFlags::default()))
}

/// 为迁核 argv 生成栈内有界十进制描述符字符串，不引入准备期堆分配。
fn decimal_fd(fd: usize) -> ([u8; 20], usize) {
    let mut digits = [0u8; 20];
    let mut cursor = digits.len();
    let mut value = fd;
    loop {
        cursor -= 1;
        digits[cursor] = b'0' + (value % 10) as u8;
        value /= 10;
        if value == 0 {
            break;
        }
    }
    (digits, cursor)
}
