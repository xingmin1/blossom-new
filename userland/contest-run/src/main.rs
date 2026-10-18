//! 正式计划与显式命令共用同一运行根、执行函数和释放路径。

use contest_run::config;
use contest_run::custom;
use contest_run::execute::{execute_case, run_command, CaseRecord, ExitOutcome};
use contest_run::model::{
    CommandSpec, Config, Defaults, Finish, Protocol, ResolvedCase, RootSpec, SourceLocation,
};
use contest_run::runtime::{diagnose, Mounts, CONTEST_ROOT};
use contest_run::selection;
use std::collections::BTreeMap;
use std::env;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;

struct Cli {
    device: PathBuf,
    plan: PathBuf,
    files: Vec<PathBuf>,
    list: bool,
    command: Option<(PathBuf, PathBuf, Vec<String>)>,
}

/// 参数错误在挂载前结束；设备名称由实际挂载边界检验，命令数组不经 shell 重拆。
fn parse_cli() -> Result<Cli, String> {
    let args = env::args_os()
        .skip(1)
        .map(|arg| {
            arg.into_string()
                .map_err(|_| "CLI requires UTF-8 arguments".to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;
    let device = args.first().ok_or_else(|| "Usage: contest-run DEVICE [--plan FILE] [--file FILE ...] [--list] | DEVICE --root ROOT --cwd CWD -- COMMAND [ARG...]".to_string())?;
    if Path::new(device).parent() != Some(Path::new("/dev")) {
        return Err(format!("invalid contest device: {device}"));
    }
    let mut cli = Cli {
        device: device.into(),
        plan: "/etc/contest/plan.toml".into(),
        files: Vec::new(),
        list: false,
        command: None,
    };
    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--plan" | "--file" => {
                let value = args
                    .get(index + 1)
                    .ok_or_else(|| format!("missing value for {}", args[index]))?;
                if args[index] == "--plan" {
                    cli.plan = value.into();
                } else {
                    cli.files.push(value.into());
                }
                index += 2;
            }
            "--list" => {
                cli.list = true;
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

/// 显式命令只构造普通 Config/ResolvedCase，完成后返回原状态。
fn explicit_config(
    cli: &Cli,
    root: &Path,
    cwd: &Path,
    argv: &[String],
) -> (Config, Vec<ResolvedCase>) {
    let root = RootSpec {
        path: Path::new(CONTEST_ROOT).join(root.strip_prefix("/").unwrap()),
        cwd: cwd.to_path_buf(),
        env: BTreeMap::new(),
        mounts: vec!["dev".into(), "proc".into(), "tmp".into()],
        prepare: Vec::new(),
    };
    let source = SourceLocation {
        file: PathBuf::from("<command-line>"),
        entry: 1,
        catalog: None,
        line: None,
    };
    let case = ResolvedCase {
        id: "command".into(),
        source,
        root: "command".into(),
        cwd: cwd.into(),
        command: CommandSpec::Exec {
            program: argv[0].clone(),
            args: argv[1..].to_vec(),
        },
        env: BTreeMap::new(),
        timeout_sec: 60,
        repeat: 1,
        hooks: false,
        setup: Vec::new(),
        cleanup: Vec::new(),
        protocol: Protocol::Plain,
    };
    let config = Config {
        roots: BTreeMap::from([("command".into(), root)]),
        defaults: Defaults::default(),
        steps: Vec::new(),
        overrides: Vec::new(),
        finish: Finish::Return,
        plan: cli.plan.clone(),
    };
    (config, vec![case])
}

/// 固定旧协议只在连续 root/协议段输出一次原组边界。
fn group(case: &ResolvedCase) -> Option<(String, String)> {
    let name = match case.protocol {
        Protocol::Basic { .. } => "basic",
        Protocol::Pre2025Libctest { .. } => "libctest",
        _ => return None,
    };
    Some((name.into(), case.root.clone()))
}

/// 原组边界用于外部输出，不输入单测例协议观察器。
fn group_marker(group: &(String, String), start: bool) {
    println!(
        "#### OS COMP TEST GROUP {} {}-{} ####",
        if start { "START" } else { "END" },
        group.0,
        group.1
    );
}

/// 每一轮都输出真实状态与独立错误，重复运行不覆盖先前记录。
fn print_record(case: &ResolvedCase, record: &CaseRecord) {
    if let Some(run) = &record.execution {
        println!(
            "CONTEST result id={:?} iteration={} outcome={} status={:?} elapsed_ms={}",
            case.id,
            record.iteration,
            run.verdict.as_str(),
            run.outcome,
            run.elapsed.as_millis()
        );
        for error in &run.io_errors {
            eprintln!("CONTEST I/O error id={:?}: {error}", case.id);
        }
    } else {
        println!(
            "CONTEST result id={:?} iteration={} outcome=NotRun",
            case.id, record.iteration
        );
    }
    for (phase, error) in [
        ("setup", &record.setup_error),
        ("before", &record.before_error),
        ("after", &record.after_error),
    ] {
        if let Some(error) = error {
            eprintln!(
                "CONTEST infrastructure error id={:?} phase={phase}: {error}",
                case.id
            );
        }
    }
    for error in &record.cleanup_errors {
        eprintln!(
            "CONTEST infrastructure error id={:?} phase=cleanup: {error}",
            case.id
        );
    }
}

/// 命令模式保留 Unix shell 可表示的退出状态，同时另行打印完整原状态。
fn command_status(outcome: &ExitOutcome) -> i32 {
    match outcome {
        ExitOutcome::Exited { code, .. } => *code,
        ExitOutcome::Signaled { signal, .. } => 128 + signal,
        ExitOutcome::TimedOut { .. } => 124,
        ExitOutcome::SpawnError { .. } | ExitOutcome::IoError { .. } => 125,
    }
}

/// 执行真实展开结果；根段结束时先释放自己取得的挂载再切换下一根。
fn run_cases(config: &Config, cases: &[ResolvedCase], explicit: bool) -> Result<i32, String> {
    let mut mounts = Mounts::default();
    let mut active_root: Option<&str> = None;
    let mut active_group = None;
    let mut result = 0;
    let mut failure = None;
    let mut stopped = false;
    for case in cases {
        if stopped {
            for iteration in 1..=case.repeat {
                println!(
                    "CONTEST result id={:?} iteration={iteration} outcome=NotRun",
                    case.id
                );
            }
            continue;
        }
        if active_group != group(case) {
            if let Some(previous) = active_group.take() {
                group_marker(&previous, false);
            }
        }
        if active_root != Some(case.root.as_str()) {
            if let Err(error) = mounts.release() {
                failure = Some(error);
                stopped = true;
            }
            if !stopped {
                let root = &config.roots[&case.root];
                if let Err(error) = mounts.prepare_root(
                    root,
                    Duration::from_secs(config.defaults.timeout_sec),
                    [1, 2],
                ) {
                    failure = Some(error);
                    stopped = true;
                }
                active_root = Some(&case.root);
            }
        }
        if stopped {
            for iteration in 1..=case.repeat {
                println!(
                    "CONTEST result id={:?} iteration={iteration} outcome=NotRun",
                    case.id
                );
            }
            continue;
        }
        if active_group.is_none() {
            active_group = group(case);
            if let Some(current) = &active_group {
                group_marker(current, true);
            }
        }
        let root = &config.roots[&case.root];
        for iteration in 1..=case.repeat {
            if stopped {
                println!(
                    "CONTEST result id={:?} iteration={iteration} outcome=NotRun",
                    case.id
                );
                continue;
            }
            if let Protocol::Basic { name } = &case.protocol {
                println!("Testing {name} :");
            }
            let record = execute_case(Some(&root.path), case, iteration, [1, 2], custom::hook);
            print_record(case, &record);
            if let Some(run) = &record.execution {
                if matches!(run.outcome, ExitOutcome::SpawnError { .. }) {
                    eprintln!(
                        "CONTEST diagnostic id={:?}: {}",
                        case.id,
                        diagnose(root, &case.cwd, &case.command)
                    );
                }
                if explicit {
                    result = command_status(&run.outcome);
                }
            }
            if !record.is_success() && (!explicit || result == 0) {
                result = 1;
            }
            if record.must_stop() {
                stopped = true;
                result = if result == 0 { 1 } else { result };
            }
        }
    }
    if !stopped {
        if let Some(current) = &active_group {
            group_marker(current, false);
        }
    }
    if let Err(error) = mounts.release() {
        if let Some(primary) = &failure {
            eprintln!("CONTEST primary infrastructure error: {primary}");
        }
        failure = Some(error);
    }
    if let Some(error) = failure {
        Err(error)
    } else {
        Ok(result)
    }
}

/// list 只读取实际输入，不执行根准备、Hook 或关机动作。
fn mounted(config: &Config, cli: &Cli, explicit: Option<Vec<ResolvedCase>>) -> Result<i32, String> {
    let cases = match explicit {
        Some(cases) => cases,
        None => selection::resolve(config, env::consts::ARCH)?,
    };
    if cli.list {
        for case in &cases {
            let root = &config.roots[&case.root];
            println!("CONTEST selected id={:?} source={} root={:?} path={:?} cwd={:?} command={:?} hooks={} repeat={} diagnostic={:?}", case.id, case.source, case.root, root.path, case.cwd, case.command, case.hooks, case.repeat, diagnose(root, &case.cwd, &case.command));
        }
        return Ok(0);
    }
    run_cases(config, &cases, cli.command.is_some())
}

/// 同步和关机沿既有 BusyBox 请求，不把普通命令返回变为默认关机。
fn finish(mode: Finish, status: i32) -> i32 {
    let env = BTreeMap::new();
    let sync = CommandSpec::Exec {
        program: "/bin/busybox".into(),
        args: vec!["sync".into()],
    };
    let run = run_command(
        None,
        Path::new("/"),
        &sync,
        &env,
        Duration::from_secs(60),
        &Protocol::Plain,
        [1, 2],
    );
    let mut status = status;
    if !matches!(run.outcome, ExitOutcome::Exited { code: 0, .. }) || !run.io_errors.is_empty() {
        eprintln!("CONTEST sync error: {:?} {:?}", run.outcome, run.io_errors);
        if status == 0 {
            status = 1;
        }
    }
    println!("CONTEST finished status={status}");
    if mode == Finish::Poweroff {
        let poweroff = CommandSpec::Exec {
            program: "/bin/busybox".into(),
            args: vec!["poweroff".into()],
        };
        let run = run_command(
            None,
            Path::new("/"),
            &poweroff,
            &env,
            Duration::from_secs(60),
            &Protocol::Plain,
            [1, 2],
        );
        if !matches!(run.outcome, ExitOutcome::Exited { code: 0, .. }) || !run.io_errors.is_empty()
        {
            eprintln!(
                "CONTEST poweroff error: {:?} {:?}",
                run.outcome, run.io_errors
            );
            if status == 0 {
                status = 1;
            }
        }
    }
    status
}

/// 挂载后的所有普通返回都经过实际拥有的释放与明确 finish 选择。
fn run() -> Result<i32, String> {
    let cli = parse_cli()?;
    let (config, explicit) = match &cli.command {
        Some((root, cwd, argv)) => {
            let (config, cases) = explicit_config(&cli, root, cwd, argv);
            (config, Some(cases))
        }
        None => (config::load(&cli.plan, &cli.files)?, None),
    };
    // 同一工作盘的卷可由 canonical 缓存继续保活，list 不切换卷的读写模式。
    let mut contest = Mounts::default();
    contest
        .mount(&cli.device, Path::new(CONTEST_ROOT), "ext4", false)
        .map_err(|error| format!("mount contest {}: {error}", cli.device.display()))?;
    let result = mounted(&config, &cli, explicit);
    let released = contest.release();
    let mut status = match result {
        Ok(status) => status,
        Err(error) => {
            eprintln!("CONTEST infrastructure error: {error}");
            1
        }
    };
    if let Err(error) = released {
        eprintln!("CONTEST cleanup error: {error}");
        if status == 0 {
            status = 1;
        }
    }
    if cli.list {
        Ok(status)
    } else {
        Ok(finish(config.finish, status))
    }
}

/// 只在全部尾部完成后返回进程状态，参数和配置错误保留英文诊断。
fn main() {
    let status = match run() {
        Ok(status) => status,
        Err(error) => {
            eprintln!("CONTEST error: {error}");
            2
        }
    };
    std::process::exit(status);
}
