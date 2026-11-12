//! 单个测例的生命周期编排：内核通知、`setup`、文本输出、主命令与 `cleanup`。
//!
//! 本模块位于测例选择层（`crate::selection`）与子进程执行器（`crate::process`）之间，
//! 只记录进程事实与编排错误，不做判题。一次执行的顺序固定为：
//!
//! ```text
//! before 通知 ─> setup ─> before 文本 ─> 主命令 ─> after 文本 ─> cleanup ─> after 通知
//! ```
//!
//! `before` 通知失败时不再进入本轮的其余阶段。任一命令的进程控制失败（unquiesced）时，
//! 不再执行后续阶段或发送 `after` 通知，由上层停止会话。setup 或主命令失控时也不写 `after` 文本。
//! 其余阶段的错误分别保留，清理与 `after` 通知仍然执行。普通非零退出不是执行错误；超时与
//! 信号终止属于执行错误，但进程组已可靠收尾，会话可以继续下一例。

use crate::command::CommandSpec;
use crate::model::ResolvedCase;
use crate::outcome::{Execution, ExitOutcome};
use std::collections::BTreeMap;
use std::io::{self, Write};
use std::path::Path;
use std::time::Duration;

/// 单个测例一次执行的完整记录。
///
/// 各阶段的错误分别保存，后一阶段不会覆盖前一阶段的信息；`execution` 只在主命令真正启动后存在。
#[derive(Debug, Default)]
pub struct CaseRecord {
    /// 内核 `before` 通知失败的原因。
    pub before_error: Option<String>,
    /// `setup` 阶段的命令错误；`stop_on_error` 使这里最多保留首个错误。
    pub setup_errors: Vec<String>,
    /// `setup` 命令的进程控制已失效，其进程组可能仍在运行。
    pub setup_unquiesced: bool,
    /// 写出 `before`/`after` 文本时发生的 I/O 错误。
    pub output_errors: Vec<String>,
    /// 主命令的进程事实；未运行时为 `None`。
    pub execution: Option<Execution>,
    /// `cleanup` 阶段的命令错误；进程控制失效后不再执行其余命令。
    pub cleanup_errors: Vec<String>,
    /// `cleanup` 命令的进程控制已失效，其进程组可能仍在运行。
    pub cleanup_unquiesced: bool,
    /// 内核 `after` 通知失败的原因。
    pub after_error: Option<String>,
}

impl CaseRecord {
    /// 报告准备、主命令或清理阶段是否存在无法确认已停止的进程组。
    ///
    /// 命令以 `IoError` 结束，或执行过程中记录了进程组信号、等待等 I/O 错误时为 `true`；
    /// 此时不能再假定进程组已静止，不能继续执行清理或恢复全局状态。
    pub fn unquiesced(&self) -> bool {
        self.setup_unquiesced
            || self.cleanup_unquiesced
            || self.execution.as_ref().is_some_and(control_lost)
    }

    /// 报告本次执行是否要求停止整个会话。
    ///
    /// 通知失败、输出失败、清理失败或进程控制失败都会让后续测例在不可信的状态下运行，
    /// 因此调用方在记录到 `true` 后不再开始下一例。
    pub fn must_stop(&self) -> bool {
        self.before_error.is_some()
            || self.after_error.is_some()
            || !self.cleanup_errors.is_empty()
            || !self.output_errors.is_empty()
            || self.unquiesced()
    }

    /// 报告本次执行是否计入计划状态中的执行错误。
    ///
    /// 除会话停止条件外，`setup` 失败、主命令未运行或主命令不是正常退出（超时、信号、启动失败）
    /// 也算执行错误；正常非零退出码不是执行错误。
    pub fn failed(&self) -> bool {
        if self.must_stop() || !self.setup_errors.is_empty() {
            return true;
        }
        match &self.execution {
            Some(run) => !matches!(run.outcome, ExitOutcome::Exited { .. }),
            None => true,
        }
    }
}

/// 一组准备或清理命令的执行结果。
#[derive(Debug, Default)]
pub struct StepReport {
    /// 失败命令的单行诊断。
    pub errors: Vec<String>,
    /// 某条命令的进程控制已失效，其进程组可能仍在运行。
    pub unquiesced: bool,
}

/// 判断一次命令执行是否丢失了进程控制：以 `IoError` 结束，或记录了信号、等待等 I/O 错误。
fn control_lost(run: &Execution) -> bool {
    !run.io_errors.is_empty() || matches!(run.outcome, ExitOutcome::IoError { .. })
}

/// 顺序执行一组准备或清理命令，返回失败诊断与进程控制是否失效。
///
/// 根准备、组边界与测例阶段同步调用；每条命令按自身选择的根解析 cwd、env 和单条超时。
/// 只有正常退出码 0 才算成功，空 argv 直接记为错误。`stop_on_error` 为真时遇首错停止；
/// 为假时继续尝试其余命令，但进程控制一旦失效就立即停止。
pub fn run_steps(
    root: Option<&Path>,
    cwd: &Path,
    commands: &[CommandSpec],
    env: &BTreeMap<String, String>,
    timeout: Duration,
    stop_on_error: bool,
) -> StepReport {
    let mut report = StepReport::default();
    for command in commands {
        let invocation = command.resolve(root, cwd, env, timeout);
        if invocation.argv.is_empty() {
            report.errors.push("empty command".into());
            if stop_on_error {
                break;
            }
            continue;
        }
        let run = invocation.run();
        let lost = control_lost(&run);
        if !matches!(run.outcome, ExitOutcome::Exited { code: 0, .. }) || lost {
            report.errors.push(describe(invocation.argv, &run));
            report.unquiesced |= lost;
            if stop_on_error || lost {
                break;
            }
        }
    }
    report
}

/// 执行单个测例的一次完整生命周期，返回各阶段记录。
///
/// `root`、`case.cwd` 与 `case.env` 提供 test 命令的默认上下文，system 命令不继承它们。
/// 准备与清理默认采用 `setup_timeout`，主命令默认采用 `case.timeout_sec`；命令局部值优先。
/// `out` 接收 before/after 文本，`notify` 接收内核通知。
/// 函数不判题，也不会因普通非零退出而提前结束；`before` 通知失败或任一命令的进程控制
/// 失效时立即返回，此时调用方必须停止会话。
pub fn execute_case(
    root: Option<&Path>,
    case: &ResolvedCase,
    setup_timeout: Duration,
    out: &mut impl Write,
    mut notify: impl FnMut(bool, &str) -> io::Result<()>,
) -> CaseRecord {
    let mut record = CaseRecord::default();
    // Step 1: 发送 before 通知；失败时通知实现自行撤销局部操作，本轮不再执行任何阶段。
    if let Err(error) = notify(true, &case.id) {
        record.before_error = Some(error.to_string());
        return record;
    }
    // Step 2: 执行 setup；只有全部成功才写出 before 文本并启动主命令。
    let setup = run_steps(root, &case.cwd, &case.setup, &case.env, setup_timeout, true);
    record.setup_errors = setup.errors;
    record.setup_unquiesced = setup.unquiesced;
    if record.setup_unquiesced {
        return record;
    }
    if record.setup_errors.is_empty() {
        match emit(out, &case.before) {
            Err(error) => record.output_errors.push(error.to_string()),
            Ok(()) => {
                let run = case
                    .exec
                    .resolve(
                        root,
                        &case.cwd,
                        &case.env,
                        Duration::from_secs(case.timeout_sec),
                    )
                    .run();
                let status = run.outcome.shell_status();
                record.execution = Some(run);
                // 进程控制失败时工作负载可能仍在运行，此时写 after 文本或执行 cleanup 都是冒险。
                if record.unquiesced() {
                    return record;
                }
                let after = render_after(case, root, status);
                if let Err(error) = emit(out, &after) {
                    record.output_errors.push(error.to_string());
                }
            }
        }
    }
    // Step 3: 清理命令的普通失败不阻止后续清理；进程控制失效时不得恢复全局状态。
    let cleanup = run_steps(
        root,
        &case.cwd,
        &case.cleanup,
        &case.env,
        setup_timeout,
        false,
    );
    record.cleanup_errors = cleanup.errors;
    record.cleanup_unquiesced = cleanup.unquiesced;
    if record.cleanup_unquiesced {
        return record;
    }
    if let Err(error) = notify(false, &case.id) {
        record.after_error = Some(error.to_string());
    }
    record
}

/// 把测例协议文本原文写到输出并刷出；空文本不产生写入。
///
/// 返回的 I/O 错误由调用方记入 `output_errors`，不在此处忽略或重试。
fn emit(out: &mut impl Write, text: &str) -> io::Result<()> {
    if text.is_empty() {
        return Ok(());
    }
    out.write_all(text.as_bytes())?;
    out.flush()
}

/// 用完整上下文单次展开测例的 `after` 模板。
///
/// `status` 是主命令的 shell 状态码；`command` 仅由 catalog 提供，`test_root` 保持测试根视图。
/// `after` 保留原始模板到此刻才展开，插入的清单命令与名字不再递归解释。
fn render_after(case: &ResolvedCase, root: Option<&Path>, status: i32) -> String {
    let status = status.to_string();
    let test_root = root.unwrap_or_else(|| Path::new("/")).to_string_lossy();
    let mut values = vec![
        ("group", case.group.as_str()),
        ("root", case.root.as_str()),
        ("test_root", test_root.as_ref()),
        ("name", case.name.as_str()),
        ("id", case.id.as_str()),
        ("status", status.as_str()),
    ];
    if let Some(command) = &case.catalog_command {
        values.push(("command", command.as_str()));
    }
    crate::expand::expand(&case.after, &values)
}

/// 把一条准备命令的退出事实格式化为单行诊断，供 `setup`/`cleanup` 错误列表使用。
fn describe(argv: &[String], run: &Execution) -> String {
    if run.io_errors.is_empty() {
        format!("command {argv:?}: {:?}", run.outcome)
    } else {
        format!(
            "command {argv:?}: {:?}; I/O errors: {:?}",
            run.outcome, run.io_errors
        )
    }
}
