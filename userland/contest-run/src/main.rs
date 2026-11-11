//! `contest-run` 二进制主入口：命令行解析、计划预览、分组执行与收尾。
//!
//! ## 两种运行模式
//! - 计划模式：`contest-run DEVICE [--plan FILE] [--file FILE]... [--list] [--only PATTERN]...`
//!   从 TOML 计划加载配置，展开为 [`ResolvedGroup`] 列表后逐组执行。
//! - 单命令模式：`contest-run DEVICE --root ROOT --cwd CWD -- COMMAND [ARG...]`
//!   构造单个 root/case，与计划模式共用同一套挂载、执行与通知路径，退出码透传主命令状态。
//!
//! ## 执行流程
//! ```text
//! parse_cli
//!   └─ 计划模式: config::load（失败 → 2，不挂载）
//!         │
//!         ▼
//! 挂载 DEVICE 到 /mnt/contest（失败 → 1）
//!         │
//!         ├─ 计划模式: selection::resolve（失败 → 2，仍卸载并按模式收尾）
//!         ├─ --list: 打印分组/测例/未命中覆盖项，不执行任何准备或通知
//!         └─ 运行: 逐组激活 root → 组 start 文本 → 逐 case 逐 repeat 执行 → 组 end 文本
//!                  会话停止时立即结束，已开始的组仍尽量写 end
//!         │
//!         ▼
//! 卸载比赛盘（失败 → 1）→ 非 --list 时 sync 与可选 poweroff
//! ```
//!
//! 默认只输出协议文本与必要错误；`--verbose` 额外给出每次执行的 outcome/elapsed、
//! 未命中的覆盖项与最终状态。

use contest_run::config;
use contest_run::custom;
use contest_run::execute::{execute_case, CaseRecord};
use contest_run::model::{
    Config, Defaults, Finish, ResolvedCase, ResolvedGroup, RootSpec, SourceLocation,
};
use contest_run::outcome::ExitOutcome;
use contest_run::process::run_command;
use contest_run::runtime::{diagnose, Mounts, CONTEST_ROOT};
use contest_run::selection;
use std::collections::BTreeMap;
use std::env;
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

/// 命令行参数解析结果。
struct Cli {
    /// 承载比赛测试套件的块设备路径，必须位于 `/dev` 下。
    device: PathBuf,
    /// 主测试计划文件路径。
    plan: PathBuf,
    /// 通过 `--file` 给出、整体替换主计划 `files` 的计划文件列表。
    files: Vec<PathBuf>,
    /// 是否只打印选中用例而不执行（`--list`）。
    list: bool,
    /// 重复给出的 `--only` glob，只缩小选择结果。
    only: Vec<String>,
    /// 覆盖全部已选用例主命令超时的秒数。
    timeout_sec: Option<u64>,
    /// 覆盖全部已选用例重复次数的值。
    repeat: Option<u32>,
    /// 是否打印每次执行细节与最终状态。
    verbose: bool,
    /// 覆盖计划收尾方式的取值。
    finish: Option<Finish>,
    /// 单命令模式参数：目标 root、工作目录与完整 argv。
    command: Option<(PathBuf, PathBuf, Vec<String>)>,
}

/// 解析并校验命令行参数。
///
/// 在挂载任何文件系统之前调用；参数缺失、数值非正或格式错误都返回错误文本，由主入口以
/// 退出码 2 结束。设备路径必须直接位于 `/dev` 下；单命令模式仍要求 `--root` 紧跟 DEVICE。
fn parse_cli() -> Result<Cli, String> {
    // Step 1: 收集 UTF-8 参数并校验必填的块设备。
    let args = env::args_os()
        .skip(1)
        .map(|arg| {
            arg.into_string()
                .map_err(|_| "CLI requires UTF-8 arguments".to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;
    let device = args.first().ok_or_else(|| {
        "Usage: contest-run DEVICE [--plan FILE] [--file FILE]... [--list] [--only PATTERN]... \
         [--timeout-sec N] [--repeat N] [--verbose] [--finish poweroff|return]\n       \
         contest-run DEVICE --root ROOT --cwd CWD -- COMMAND [ARG...]"
            .to_string()
    })?;
    if Path::new(device).parent() != Some(Path::new("/dev")) {
        return Err(format!("invalid contest device: {device}"));
    }
    let mut cli = Cli {
        device: device.into(),
        plan: "/etc/contest/plan.toml".into(),
        files: Vec::new(),
        list: false,
        only: Vec::new(),
        timeout_sec: None,
        repeat: None,
        verbose: false,
        finish: None,
        command: None,
    };
    // Step 2: 顺序扫描标志；`--root` 只允许紧跟 DEVICE，命中后剩余参数全部属于显式命令。
    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--plan" => {
                cli.plan = flag_value(&args, index)?.into();
                index += 2;
            }
            "--file" => {
                cli.files.push(flag_value(&args, index)?.into());
                index += 2;
            }
            "--only" => {
                cli.only.push(flag_value(&args, index)?.into());
                index += 2;
            }
            "--timeout-sec" => {
                cli.timeout_sec = Some(parse_positive(flag_value(&args, index)?, "--timeout-sec")?);
                index += 2;
            }
            "--repeat" => {
                let repeat = parse_positive(flag_value(&args, index)?, "--repeat")?;
                cli.repeat = Some(
                    u32::try_from(repeat)
                        .map_err(|_| format!("--repeat out of range: {repeat}"))?,
                );
                index += 2;
            }
            "--finish" => {
                cli.finish = Some(match flag_value(&args, index)? {
                    "poweroff" => Finish::Poweroff,
                    "return" => Finish::Return,
                    value => return Err(format!("invalid --finish: {value}")),
                });
                index += 2;
            }
            "--list" => {
                cli.list = true;
                index += 1;
            }
            "--verbose" => {
                cli.verbose = true;
                index += 1;
            }
            "--root" if index == 1 => {
                if args.get(3).map(String::as_str) != Some("--cwd")
                    || args.get(5).map(String::as_str) != Some("--")
                    || args.len() < 7
                {
                    return Err("expected DEVICE --root ROOT --cwd CWD -- COMMAND [ARG...]".into());
                }
                let root = PathBuf::from(&args[2]);
                let cwd = PathBuf::from(&args[4]);
                if !root.is_absolute()
                    || root
                        .components()
                        .any(|part| matches!(part, Component::ParentDir))
                    || !cwd.is_absolute()
                    || args[6].is_empty()
                {
                    return Err("explicit root/cwd must be absolute, root must not contain '..', and command must not be empty".into());
                }
                cli.command = Some((root, cwd, args[6..].to_vec()));
                return Ok(cli);
            }
            argument => return Err(format!("unknown CLI argument: {argument}")),
        }
    }
    Ok(cli)
}

/// 取出当前标志后紧跟的参数值。
fn flag_value(args: &[String], index: usize) -> Result<&str, String> {
    args.get(index + 1)
        .map(String::as_str)
        .ok_or_else(|| format!("missing value for {}", args[index]))
}

/// 解析必须为正整数的命令行数值。
fn parse_positive(value: &str, flag: &str) -> Result<u64, String> {
    let number = value
        .parse::<u64>()
        .map_err(|_| format!("{flag} requires a positive integer: {value}"))?;
    if number == 0 {
        return Err(format!("{flag} requires a positive integer: {value}"));
    }
    Ok(number)
}

/// 把单命令模式参数包装成最小的 [`Config`] 与单个 [`ResolvedGroup`]。
///
/// 目标 root 位于比赛盘挂载点之下，依次挂载 `dev`/`shm`/`proc`/`tmp`；case 固定 ID `command`，
/// 超时 60 秒、重复一次，收尾方式为 `Finish::Return`，不触发关机。
fn explicit_config(
    cli: &Cli,
    root: &Path,
    cwd: &Path,
    argv: &[String],
) -> (Config, Option<Vec<ResolvedGroup>>) {
    let root = RootSpec {
        path: Path::new(CONTEST_ROOT).join(root.strip_prefix("/").unwrap()),
        cwd: cwd.to_path_buf(),
        env: BTreeMap::new(),
        mounts: vec!["dev".into(), "shm".into(), "proc".into(), "tmp".into()],
        prepare: Vec::new(),
    };
    let case = ResolvedCase {
        id: "command".into(),
        group: "command".into(),
        root: "command".into(),
        name: "command".into(),
        catalog_command: None,
        argv: argv.to_vec(),
        cwd: cwd.into(),
        env: BTreeMap::new(),
        timeout_sec: 60,
        repeat: 1,
        setup: Vec::new(),
        cleanup: Vec::new(),
        before: String::new(),
        after: String::new(),
        source: SourceLocation {
            file: PathBuf::from("<command-line>"),
            entry: "command".into(),
            catalog: None,
            line: None,
        },
    };
    let config = Config {
        plan: cli.plan.clone(),
        roots: BTreeMap::from([("command".into(), root)]),
        defaults: Defaults::default(),
        finish: Finish::Return,
        groups: Vec::new(),
        overrides: Vec::new(),
    };
    let group = ResolvedGroup {
        name: "command".into(),
        root: "command".into(),
        start: String::new(),
        end: String::new(),
        cases: vec![case],
    };
    (config, Some(vec![group]))
}

/// 完成选择与 CLI 覆盖后打印预览或执行，返回计划状态码。
///
/// 显式命令模式直接使用传入的分组；计划模式调用 [`selection::resolve`]，失败时打印一行错误
/// 并返回 2。`--list` 只打印分组、用例与未命中的覆盖项，不执行任何准备、通知或收尾动作。
fn execute_plan(config: &Config, cli: &Cli, explicit: Option<Vec<ResolvedGroup>>) -> i32 {
    let (mut groups, unmatched) = match explicit {
        Some(groups) => (groups, Vec::new()),
        None => match selection::resolve(config, env::consts::ARCH, &cli.only) {
            Ok(selection) => (selection.groups, selection.unmatched),
            Err(error) => {
                eprintln!("contest-run: {error}");
                return 2;
            }
        },
    };
    for group in &mut groups {
        for case in &mut group.cases {
            if let Some(timeout) = cli.timeout_sec {
                case.timeout_sec = timeout;
            }
            if let Some(repeat) = cli.repeat {
                case.repeat = repeat;
            }
        }
    }
    if cli.list {
        list(config, &groups, &unmatched);
        return 0;
    }
    run_groups(
        config,
        &groups,
        cli.command.is_some(),
        cli.verbose,
        &unmatched,
    )
}

/// 打印选中分组、用例与未命中的覆盖项。
///
/// 每组一行头信息，每个用例一行给出 ID、cwd、超时、重复次数、完整 argv 与来源位置。
fn list(config: &Config, groups: &[ResolvedGroup], unmatched: &[String]) {
    for group in groups {
        let root = &config.roots[&group.root];
        println!(
            "GROUP {} / {} path={}",
            group.name,
            group.root,
            root.path.display()
        );
        for case in &group.cases {
            println!(
                "{} cwd={} timeout={}s repeat={} argv={:?} source={}",
                case.id,
                case.cwd.display(),
                case.timeout_sec,
                case.repeat,
                case.argv,
                case.source
            );
        }
    }
    for item in unmatched {
        println!("override {item} matched no selected case");
    }
}

/// 按分组顺序执行全部选中用例，返回计划状态码。
///
/// 组切换时先卸载上一个 root 的挂载再 [`Mounts::prepare_root`] 新 root；准备或卸载失败时置
/// 状态 1 并停止。组开始后写出 `start` 文本，跑完全部 case 与 repeat 后写出 `end` 文本；
/// 出现会话停止条件时立即停止后续执行，但已开始的组仍尽量写 `end`。`explicit` 为真时退出码
/// 透传主命令的 `shell_status()`。
fn run_groups(
    config: &Config,
    groups: &[ResolvedGroup],
    explicit: bool,
    verbose: bool,
    unmatched: &[String],
) -> i32 {
    let mut mounts = Mounts::default();
    let mut active: Option<&str> = None;
    let mut status = 0;
    let mut stopped = false;
    for group in groups {
        if stopped {
            break;
        }
        // Step 1: 需要换 root 时先释放旧挂载，再运行 prepare 并按配置顺序挂载。
        if active != Some(group.root.as_str()) {
            if let Err(error) = mounts.release() {
                eprintln!("contest-run: {error}");
                status = 1;
                break;
            }
            let root = &config.roots[&group.root];
            if let Err(error) =
                mounts.prepare_root(root, Duration::from_secs(config.defaults.timeout_sec))
            {
                eprintln!("contest-run: {error}");
                status = 1;
                break;
            }
            active = Some(&group.root);
        }
        let root = &config.roots[&group.root];
        // Step 2: 写出组开始文本；无法写出时协议已不可维持，按会话停止处理。
        if let Err(error) = emit(&group.start) {
            eprintln!("contest-run: {}: write start: {error}", group.name);
            status = 1;
            break;
        }
        // Step 3: 逐 case 逐 repeat 执行；会话停止条件立即跳出两层循环。
        'cases: for case in &group.cases {
            for _ in 0..case.repeat {
                let record = execute_case(
                    Some(&root.path),
                    case,
                    Duration::from_secs(config.defaults.timeout_sec),
                    &mut io::stdout(),
                    custom::notify,
                );
                report(case, root, &record, verbose);
                if explicit {
                    if let Some(run) = &record.execution {
                        status = run.outcome.shell_status();
                    }
                    if status == 0 && record.failed() {
                        status = 1;
                    }
                } else if record.failed() {
                    status = 1;
                }
                if record.must_stop() {
                    stopped = true;
                    break 'cases;
                }
            }
        }
        // Step 4: 无论本组是否提前停止，都尽量闭合已经写出的组标记。
        if let Err(error) = emit(&group.end) {
            eprintln!("contest-run: {}: write end: {error}", group.name);
            status = 1;
            stopped = true;
        }
    }
    // Step 5: 卸下最后一个 root 的挂载，并给出可选的运行诊断。
    if let Err(error) = mounts.release() {
        eprintln!("contest-run: {error}");
        status = 1;
    }
    if verbose {
        for item in unmatched {
            eprintln!("contest-run: override {item} matched no selected case");
        }
        eprintln!("contest-run: status {status}");
    }
    status
}

/// 把组 `start`/`end` 协议文本原文写到标准输出并刷出；空文本不写入。
fn emit(text: &str) -> io::Result<()> {
    if text.is_empty() {
        return Ok(());
    }
    let mut out = io::stdout();
    out.write_all(text.as_bytes())?;
    out.flush()
}

/// 把一次执行的阶段错误与主命令异常按行写到标准错误。
///
/// 正常非零退出不是错误，默认不输出；`verbose` 为真时额外打印主命令 outcome 与耗时。
fn report(case: &ResolvedCase, root: &RootSpec, record: &CaseRecord, verbose: bool) {
    for (phase, error) in [
        ("before", &record.before_error),
        ("after", &record.after_error),
    ] {
        if let Some(error) = error {
            eprintln!("contest-run: {}: {phase} notify: {error}", case.id);
        }
    }
    for error in &record.setup_errors {
        eprintln!("contest-run: {}: setup: {error}", case.id);
    }
    for error in &record.output_errors {
        eprintln!("contest-run: {}: output: {error}", case.id);
    }
    for error in &record.cleanup_errors {
        eprintln!("contest-run: {}: cleanup: {error}", case.id);
    }
    let Some(run) = &record.execution else {
        if verbose {
            eprintln!("contest-run: {}: not run", case.id);
        }
        return;
    };
    match &run.outcome {
        ExitOutcome::SpawnError { errno, message } => eprintln!(
            "contest-run: {}: spawn failed ({errno:?}): {message}; {}",
            case.id,
            diagnose(root, &case.cwd, &case.argv[0])
        ),
        ExitOutcome::TimedOut { .. } => {
            eprintln!(
                "contest-run: {}: timed out after {}s",
                case.id, case.timeout_sec
            )
        }
        ExitOutcome::Signaled { signal, .. } => {
            eprintln!("contest-run: {}: signaled by {signal}", case.id)
        }
        ExitOutcome::IoError { errno, message, .. } => eprintln!(
            "contest-run: {}: process control failed ({errno:?}): {message}",
            case.id
        ),
        ExitOutcome::Exited { .. } => {}
    }
    for error in &run.io_errors {
        eprintln!("contest-run: {}: {error}", case.id);
    }
    if verbose {
        eprintln!(
            "contest-run: {}: {:?} in {} ms",
            case.id,
            run.outcome,
            run.elapsed.as_millis()
        );
    }
}

/// 执行一条收尾命令；没有以退出码 0 正常结束或带有 I/O 错误时返回一行诊断。
fn finish_command(name: &str, argv: &[String]) -> Result<(), String> {
    let run = run_command(
        None,
        Path::new("/"),
        argv,
        &BTreeMap::new(),
        Duration::from_secs(60),
    );
    if matches!(run.outcome, ExitOutcome::Exited { code: 0, .. }) && run.io_errors.is_empty() {
        return Ok(());
    }
    Err(format!("{name}: {:?} {:?}", run.outcome, run.io_errors))
}

/// 先 `sync`，`Poweroff` 时再关机；失败时把状态 0 提升为 1，不打印完成行。
fn finish(mode: Finish, status: i32) -> i32 {
    let mut status = status;
    let sync = vec!["/bin/busybox".to_string(), "sync".to_string()];
    if let Err(error) = finish_command("sync", &sync) {
        eprintln!("contest-run: {error}");
        if status == 0 {
            status = 1;
        }
    }
    if mode == Finish::Poweroff {
        let poweroff = vec!["/bin/busybox".to_string(), "poweroff".to_string()];
        if let Err(error) = finish_command("poweroff", &poweroff) {
            eprintln!("contest-run: {error}");
            if status == 0 {
                status = 1;
            }
        }
    }
    status
}

/// 执行完整生命周期：解析 CLI、加载配置、挂载比赛盘、预览或执行、卸载与收尾。
///
/// 返回的错误只表示 CLI 或配置在挂载前就无法使用，由主入口映射为退出码 2；挂载、执行与卸载
/// 的失败计为计划状态 1，并在非 `--list` 模式下继续执行 `finish`。
fn run() -> Result<i32, String> {
    // Step 1: 解析 CLI，并构造计划模式配置或单命令模式的最小配置。
    let cli = parse_cli()?;
    let (config, explicit) = match &cli.command {
        Some((root, cwd, argv)) => explicit_config(&cli, root, cwd, argv),
        None => (config::load(&cli.plan, &cli.files)?, None),
    };
    // Step 2: 挂载比赛盘；挂载失败直接计为状态 1，不再尝试选择或执行。
    let mut contest = Mounts::default();
    let mut status = match contest.mount(&cli.device, Path::new(CONTEST_ROOT), "ext4", false) {
        Ok(()) => execute_plan(&config, &cli, explicit),
        Err(error) => {
            eprintln!("contest-run: mount {}: {error}", cli.device.display());
            1
        }
    };
    if let Err(error) = contest.release() {
        eprintln!("contest-run: {error}");
        if status == 0 {
            status = 1;
        }
    }
    // Step 3: 非 --list 模式执行 sync 与配置要求的关机动作。
    if !cli.list {
        status = finish(cli.finish.unwrap_or(config.finish), status);
    }
    Ok(status)
}

/// 进程主入口：运行顶层流程，把 CLI/配置错误映射为退出码 2。
fn main() {
    let status = match run() {
        Ok(status) => status,
        Err(error) => {
            eprintln!("contest-run: {error}");
            2
        }
    };
    std::process::exit(status);
}
