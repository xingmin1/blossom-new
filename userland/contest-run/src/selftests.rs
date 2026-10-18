//! 已安装 selftests 的清单选择与执行，不挂载比赛盘或执行 Hook。

use crate::model::{CommandSpec, Protocol};
use crate::outcome::{Execution, ExitOutcome, Verdict};
use crate::process::run_command;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::{self, Write};
use std::os::fd::RawFd;
use std::path::PathBuf;
use std::time::{Duration, Instant};

const DEFAULT_PROGRAM_TIMEOUT: Duration = Duration::from_secs(60);

pub struct Options {
    pub manifest: PathBuf,
    pub collections: BTreeSet<String>,
    pub programs: BTreeSet<String>,
    pub list: bool,
    /// 每个程序从启动起独立计时，输出不能刷新此预算。
    pub program_timeout: Duration,
}

impl Options {
    /// 解析已安装集合的选择和每程序绝对期限，重复选项或无效预算均返回错误。
    pub fn parse(args: &[String]) -> Result<Self, String> {
        let mut options = Self {
            manifest: "/test/selftests/kselftest-list.txt".into(),
            collections: BTreeSet::new(),
            programs: BTreeSet::new(),
            list: false,
            program_timeout: DEFAULT_PROGRAM_TIMEOUT,
        };
        let mut manifest_seen = false;
        let mut program_timeout_seen = false;
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--list" if !options.list => options.list = true,
                "--manifest" if !manifest_seen => {
                    options.manifest = args.next().ok_or("missing value for --manifest")?.into();
                    manifest_seen = true;
                }
                "--program-timeout-seconds" if !program_timeout_seen => {
                    let value = args
                        .next()
                        .ok_or("missing value for --program-timeout-seconds")?;
                    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
                        return Err("program timeout must be a positive integer in seconds".into());
                    }
                    let seconds: u64 = value
                        .parse()
                        .map_err(|_| "program timeout exceeds the supported range")?;
                    let timeout = Duration::from_secs(seconds);
                    if seconds == 0 || Instant::now().checked_add(timeout).is_none() {
                        return Err(
                            "program timeout is zero or exceeds the monotonic clock range".into(),
                        );
                    }
                    options.program_timeout = timeout;
                    program_timeout_seen = true;
                }
                "--collection" | "--program" => {
                    let value = args
                        .next()
                        .ok_or_else(|| format!("missing value for {arg}"))?;
                    let set = if arg == "--collection" {
                        component(value)?;
                        &mut options.collections
                    } else {
                        identity(value)?;
                        &mut options.programs
                    };
                    if !set.insert(value.clone()) {
                        return Err(format!("duplicate selection: {value}"));
                    }
                }
                _ => return Err(format!("unknown or repeated argument: {arg}")),
            }
        }
        Ok(options)
    }
}

#[derive(Debug, PartialEq, Eq)]
pub struct Program {
    pub id: String,
    pub cwd: PathBuf,
    pub executable: PathBuf,
}

/// 集合和程序均为一个实际目录项，禁止路径越界及不可表示的清单分隔符。
fn component(value: &str) -> Result<(), String> {
    if value.is_empty()
        || value == "."
        || value == ".."
        || value
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() || matches!(byte, b'/' | b'\\' | b':' | 0))
    {
        return Err(format!("invalid selftest path component: {value:?}"));
    }
    Ok(())
}

/// 在唯一冒号处分开集合与程序，两个部分都必须留在安装目录中。
fn identity(value: &str) -> Result<(&str, &str), String> {
    let (collection, program) = value
        .split_once(':')
        .ok_or_else(|| format!("expected collection:program: {value:?}"))?;
    component(collection)?;
    component(program)?;
    Ok((collection, program))
}

/// 只读取清单并核对全部选择，不检查程序是否存在或可执行。
pub fn select(options: &Options) -> Result<Vec<Program>, String> {
    let manifest = fs::canonicalize(&options.manifest)
        .map_err(|error| format!("manifest {}: {error}", options.manifest.display()))?;
    let text = fs::read_to_string(&manifest)
        .map_err(|error| format!("manifest {}: {error}", manifest.display()))?;
    let directory = manifest
        .parent()
        .ok_or("manifest has no parent directory")?;
    let mut programs = Vec::new();
    let mut ids = BTreeSet::new();
    let mut collections = BTreeSet::new();
    for (index, line) in text.lines().enumerate() {
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let (collection, program) = identity(line)
            .map_err(|error| format!("{}:{}: {error}", manifest.display(), index + 1))?;
        if !ids.insert(line.to_string()) {
            return Err(format!("duplicate installed selftest: {line}"));
        }
        collections.insert(collection.to_string());
        let cwd = directory.join(collection);
        programs.push(Program {
            id: line.to_string(),
            executable: cwd.join(program),
            cwd,
        });
    }
    for requested in &options.collections {
        if !collections.contains(requested) {
            return Err(format!("unknown collection: {requested}"));
        }
    }
    for requested in &options.programs {
        if !ids.contains(requested) {
            return Err(format!("unknown program: {requested}"));
        }
    }
    programs.retain(|program| {
        let (collection, _) = program.id.split_once(':').unwrap();
        (options.collections.is_empty() && options.programs.is_empty())
            || options.collections.contains(collection)
            || options.programs.contains(&program.id)
    });
    if programs.is_empty() {
        return Err("no installed selftests selected".into());
    }
    Ok(programs)
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct Summary {
    pub passed: usize,
    pub failed: usize,
    pub skipped: usize,
    pub errors: usize,
}

impl Summary {
    /// 输出和等待错误均为基础设施错误，原始退出事实仍保存在 Execution。
    pub fn record(&mut self, run: &Execution) {
        if !run.io_errors.is_empty()
            || matches!(
                run.outcome,
                ExitOutcome::SpawnError { .. } | ExitOutcome::IoError { .. }
            )
        {
            self.errors += 1;
            return;
        }
        match run.protocol.verdict {
            Verdict::Pass => self.passed += 1,
            Verdict::Skip => self.skipped += 1,
            Verdict::Broken | Verdict::InfrastructureError => self.errors += 1,
            _ => self.failed += 1,
        }
    }

    /// 基础设施错误优先返回2，普通失败为1，全部真实跳过为4。
    pub fn status(&self) -> i32 {
        if self.errors != 0 {
            2
        } else if self.failed != 0 {
            1
        } else if self.passed != 0 {
            0
        } else if self.skipped != 0 {
            4
        } else {
            2
        }
    }
}

/// 按每程序独立期限复用真实进程执行器并报告预算，清单模式仅写出选中身份。
pub fn run(options: &Options, output: &mut impl Write, outputs: [RawFd; 2]) -> Result<i32, String> {
    let programs = select(options)?;
    if options.list {
        for program in programs {
            writeln!(output, "{}", program.id).map_err(output_error)?;
        }
        output.flush().map_err(output_error)?;
        return Ok(0);
    }
    let mut summary = Summary::default();
    for program in programs {
        writeln!(
            output,
            "SELFTEST start id={:?} timeout_seconds={}",
            program.id,
            options.program_timeout.as_secs()
        )
        .map_err(output_error)?;
        output.flush().map_err(output_error)?;
        let spec = CommandSpec::Exec {
            program: program
                .executable
                .to_str()
                .ok_or("selftest executable path is not UTF-8")?
                .into(),
            args: Vec::new(),
        };
        let run = run_command(
            None,
            &program.cwd,
            &spec,
            &BTreeMap::new(),
            options.program_timeout,
            &Protocol::Tap,
            outputs,
        );
        summary.record(&run);
        writeln!(
            output,
            "SELFTEST result id={:?} timeout_seconds={} outcome={} status={:?} elapsed_ms={} tap={:?} io_errors={:?}",
            program.id,
            options.program_timeout.as_secs(),
            run.protocol.verdict.as_str(),
            run.outcome,
            run.elapsed.as_millis(),
            run.protocol.tap,
            run.io_errors
        )
        .map_err(output_error)?;
        output.flush().map_err(output_error)?;
    }
    let status = summary.status();
    writeln!(
        output,
        "SELFTEST summary pass={} fail={} skip={} error={} status={status}",
        summary.passed, summary.failed, summary.skipped, summary.errors
    )
    .map_err(output_error)?;
    output.flush().map_err(output_error)?;
    Ok(status)
}

/// 将报告写失败保留为入口错误，不用成功汇总代替丢失的输出。
fn output_error(error: io::Error) -> String {
    format!("selftest report output: {error}")
}
