//! 逐例 setup、Hook 配对与清理尾部，不拥有底层进程或输出状态。

use crate::model::{CommandSpec, Protocol, ResolvedCase};
use crate::outcome::{Execution, ExitOutcome};
use crate::process::run_command;
use std::collections::BTreeMap;
use std::io;
use std::os::fd::RawFd;
use std::path::Path;
use std::time::Duration;

#[derive(Debug)]
pub struct CaseRecord {
    pub execution: Option<Execution>,
    pub setup_error: Option<String>,
    pub before_error: Option<String>,
    pub after_error: Option<String>,
    pub cleanup_errors: Vec<String>,
    pub iteration: u32,
}

impl CaseRecord {
    /// 基础设施失败后禁止在未知状态下继续后续用例。
    pub fn must_stop(&self) -> bool {
        self.setup_error.is_some()
            || self.before_error.is_some()
            || self.after_error.is_some()
            || !self.cleanup_errors.is_empty()
            || self.execution.as_ref().is_some_and(|run| {
                !run.io_errors.is_empty() || matches!(run.outcome, ExitOutcome::IoError { .. })
            })
    }

    /// 只有协议成功且没有独立基础设施错误才算本轮成功。
    pub fn is_success(&self) -> bool {
        !self.must_stop()
            && self
                .execution
                .as_ref()
                .is_some_and(|run| run.protocol.verdict.is_success())
    }
}

/// 逐条执行显式准备命令，不把非零或超时转换成测例结果。
pub fn run_steps(
    root: Option<&Path>,
    cwd: &Path,
    commands: &[Vec<String>],
    env: &BTreeMap<String, String>,
    timeout: Duration,
    outputs: [RawFd; 2],
    stop_on_error: bool,
) -> Vec<String> {
    let mut errors = Vec::new();
    for argv in commands {
        let Some((program, args)) = argv.split_first() else {
            errors.push("empty preparation command".into());
            break;
        };
        let spec = CommandSpec::Exec {
            program: program.clone(),
            args: args.to_vec(),
        };
        let run = run_command(root, cwd, &spec, env, timeout, &Protocol::Plain, outputs);
        if !matches!(run.outcome, ExitOutcome::Exited { code: 0, .. }) || !run.io_errors.is_empty()
        {
            errors.push(format!(
                "command {argv:?}: {:?}; I/O errors: {:?}",
                run.outcome, run.io_errors
            ));
            if stop_on_error {
                break;
            }
        }
    }
    errors
}

/// 每轮固定执行 setup、before、主命令、配对 after 与 cleanup，不提前绕过尾部。
pub fn execute_case(
    root: Option<&Path>,
    case: &ResolvedCase,
    iteration: u32,
    outputs: [RawFd; 2],
    mut hook: impl FnMut(bool, &str) -> io::Result<()>,
) -> CaseRecord {
    let timeout = Duration::from_secs(case.timeout_sec);
    let setup = run_steps(
        root,
        &case.cwd,
        &case.setup,
        &case.env,
        timeout,
        outputs,
        true,
    );
    let mut record = CaseRecord {
        execution: None,
        setup_error: setup.into_iter().next(),
        before_error: None,
        after_error: None,
        cleanup_errors: Vec::new(),
        iteration,
    };
    let mut before_succeeded = false;
    if record.setup_error.is_none() && case.hooks {
        match hook(true, &case.id) {
            Ok(()) => before_succeeded = true,
            Err(error) => record.before_error = Some(error.to_string()),
        }
    }
    if record.setup_error.is_none() && record.before_error.is_none() {
        record.execution = Some(run_command(
            root,
            &case.cwd,
            &case.command,
            &case.env,
            timeout,
            &case.protocol,
            outputs,
        ));
    }
    if before_succeeded {
        if let Err(error) = hook(false, &case.id) {
            record.after_error = Some(error.to_string());
        }
    }
    record.cleanup_errors = run_steps(
        root,
        &case.cwd,
        &case.cleanup,
        &case.env,
        timeout,
        outputs,
        false,
    );
    record
}
