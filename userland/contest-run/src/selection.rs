//! 只从本次实际 root 读取目录和清单，不执行准备动作。

use crate::model::{
    CaseOverride, CatalogSource, CommandSpec, Config, Protocol, ResolvedCase, RootSpec,
    SourceLocation, Step, StepKind,
};
use crate::runtime::rooted_path;
use glob::Pattern;
use std::collections::BTreeMap;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

struct Entry {
    name: String,
    command: CommandSpec,
    source: SourceLocation,
    cwd: Option<PathBuf>,
    protocol: Protocol,
}

/// 按文件、step、root、include 的源顺序形成稳定 ID；重复轮次不复制 case。
pub fn resolve(config: &Config, arch: &str) -> Result<Vec<ResolvedCase>, String> {
    if !matches!(arch, "riscv64" | "loongarch64") {
        return Err(format!(
            "{}: unknown architecture {arch:?}",
            config.plan.display()
        ));
    }
    validate_overrides(&config.overrides)?;
    let mut cases = Vec::new();
    let mut selected = BTreeMap::<String, usize>::new();
    for step in &config.steps {
        if !step.enabled || (!step.arch.is_empty() && !step.arch.iter().any(|value| value == arch))
        {
            continue;
        }
        if matches!(step.kind, StepKind::Exec | StepKind::Shell) {
            let root_name = step.root.as_ref().unwrap();
            let entry = direct_entry(step);
            let case = resolved(config, step, root_name, step.id.clone().unwrap(), &entry);
            insert(case, config, &mut selected, &mut cases)?;
            continue;
        }
        let inclusions: Vec<Pattern> = step
            .include
            .iter()
            .map(|text| Pattern::new(text).unwrap())
            .collect();
        let exclusions: Vec<Pattern> = step
            .exclude
            .iter()
            .map(|text| Pattern::new(text).unwrap())
            .collect();
        for root_name in &step.roots {
            let entries = catalog(step, &config.roots[root_name])?;
            let mut count = 0;
            for (include, pattern) in step.include.iter().zip(&inclusions) {
                let mut matched = false;
                for entry in entries.iter().filter(|entry| pattern.matches(&entry.name)) {
                    matched = true;
                    if exclusions
                        .iter()
                        .any(|pattern| pattern.matches(&entry.name))
                    {
                        continue;
                    }
                    let id = case_id(step, root_name, &entry.name);
                    let case = resolved(config, step, root_name, id, entry);
                    insert(case, config, &mut selected, &mut cases)?;
                    count += 1;
                }
                if !matched {
                    let kind = if include.contains(['*', '?', '[']) {
                        "pattern"
                    } else {
                        "name"
                    };
                    return Err(format!(
                        "{}: root {root_name:?}: {kind} {include:?} matched no entries in {}",
                        step.location.as_ref().unwrap(),
                        catalog_path(step).display()
                    ));
                }
            }
            if count == 0 {
                return Err(format!(
                    "{}: root {root_name:?}: empty selection after exclude in {}",
                    step.location.as_ref().unwrap(),
                    catalog_path(step).display()
                ));
            }
        }
    }
    for item in &config.overrides {
        if !selected.contains_key(&item.id) {
            return Err(format!(
                "{}: override ID {:?} is not selected",
                item.location.as_ref().unwrap(),
                item.id
            ));
        }
    }
    if cases.is_empty() {
        return Err(format!(
            "{}: empty selection for architecture {arch}",
            config.plan.display()
        ));
    }
    Ok(cases)
}

/// 直接命令的 argv 不经过 Shell，空参数保持不变。
fn direct_entry(step: &Step) -> Entry {
    let command = match step.kind {
        StepKind::Exec => CommandSpec::Exec {
            program: step.program.clone().unwrap(),
            args: step.args.clone().unwrap_or_default(),
        },
        StepKind::Shell => CommandSpec::Shell {
            shell: step.shell.clone().unwrap(),
            command: step.command.clone().unwrap(),
        },
        _ => unreachable!(),
    };
    Entry {
        name: step.id.clone().unwrap(),
        command,
        source: step.location.clone().unwrap(),
        cwd: None,
        protocol: Protocol::Plain,
    }
}

/// 固定输入的缺省路径就是实际目录或原脚本，未命中时不改用其他来源。
fn catalog_path(step: &Step) -> PathBuf {
    match step.kind {
        StepKind::Ltp if step.source.unwrap_or_default() == CatalogSource::Executables => {
            step.directory.clone().unwrap()
        }
        StepKind::Basic => step
            .directory
            .clone()
            .unwrap_or_else(|| PathBuf::from("/basic")),
        StepKind::Ltp => step.catalog.clone().unwrap(),
        StepKind::Libctest => step.catalog.clone().unwrap_or_else(|| {
            PathBuf::from(format!("/run-{}.sh", step.variant.as_ref().unwrap()))
        }),
        _ => unreachable!(),
    }
}

/// 选择来源完全由 kind/source 决定，runtest 打不开直接报告原错误。
fn catalog(step: &Step, root: &RootSpec) -> Result<Vec<Entry>, String> {
    let path = catalog_path(step);
    match step.kind {
        StepKind::Basic => executables(step, root, &path),
        StepKind::Ltp if step.source.unwrap_or_default() == CatalogSource::Executables => {
            executables(step, root, &path)
        }
        StepKind::Ltp | StepKind::Libctest => text_catalog(step, root, &path),
        _ => unreachable!(),
    }
}

/// 在指定 root 内跟随目录和条目链接，按原别名排序并保留执行路径。
fn executables(step: &Step, root: &RootSpec, path: &Path) -> Result<Vec<Entry>, String> {
    let at = step.location.as_ref().unwrap();
    let directory = rooted_path(&root.path, Path::new("/"), path)
        .and_then(fs::read_dir)
        .map_err(|error| {
            format!(
                "{at}: root {}: {}: {error}",
                root.path.display(),
                path.display()
            )
        })?;
    let mut entries = Vec::new();
    for item in directory {
        let item = item.map_err(|error| {
            format!(
                "{at}: root {}: {}: {error}",
                root.path.display(),
                path.display()
            )
        })?;
        let entry_path = path.join(item.file_name());
        let metadata = rooted_path(&root.path, Path::new("/"), &entry_path)
            .and_then(fs::metadata)
            .map_err(|error| {
                format!(
                    "{at}: root {}: {}: {error}",
                    root.path.display(),
                    entry_path.display()
                )
            })?;
        if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
            continue;
        }
        let name = item.file_name().into_string().map_err(|_| {
            format!(
                "{at}: non-UTF-8 executable name in root {}: {}",
                root.path.display(),
                path.display()
            )
        })?;
        catalog_name(&name, at)?;
        let mut source = at.clone();
        source.catalog = Some(path.to_path_buf());
        let protocol = if step.kind == StepKind::Basic {
            Protocol::Basic { name: name.clone() }
        } else {
            Protocol::Ltp
        };
        entries.push(Entry {
            command: CommandSpec::Exec {
                program: entry_path.to_str().unwrap().to_owned(),
                args: Vec::new(),
            },
            name,
            source,
            cwd: if step.kind == StepKind::Basic {
                Some(path.to_path_buf())
            } else {
                None
            },
            protocol,
        });
    }
    entries.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(entries)
}

/// 从指定 root 解析清单链接，保留原行顺序与命令正文及固定 libctest argv。
fn text_catalog(step: &Step, root: &RootSpec, path: &Path) -> Result<Vec<Entry>, String> {
    let at = step.location.as_ref().unwrap();
    let text = rooted_path(&root.path, Path::new("/"), path)
        .and_then(fs::read_to_string)
        .map_err(|error| {
            format!(
                "{at}: root {}: {}: {error}",
                root.path.display(),
                path.display()
            )
        })?;
    let mut entries = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim_start();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut source = at.clone();
        source.catalog = Some(path.to_path_buf());
        source.line = Some(index + 1);
        let entry = if step.kind == StepKind::Ltp {
            let end = line
                .find(char::is_whitespace)
                .ok_or_else(|| format!("{source}: runtest tag has no command"))?;
            let name = &line[..end];
            catalog_name(name, &source)?;
            let command = line[end..].trim_start();
            if command.is_empty() || command.contains('\0') {
                return Err(format!("{source}: empty or NUL-containing runtest command"));
            }
            Entry {
                name: name.to_owned(),
                command: CommandSpec::Shell {
                    shell: step.shell.clone().unwrap_or_else(|| "/bin/sh".to_owned()),
                    command: command.to_owned(),
                },
                source,
                cwd: None,
                protocol: Protocol::Ltp,
            }
        } else {
            libctest_entry(step.variant.as_ref().unwrap(), line, source)?
        };
        entries.push(entry);
    }
    Ok(entries)
}

/// 固定 pre-2025 的 runtest argv，工作目录留给 root、step 和精确覆盖决定。
fn libctest_entry(variant: &str, line: &str, source: SourceLocation) -> Result<Entry, String> {
    let words: Vec<&str> = line.split_whitespace().collect();
    let entry = format!("entry-{variant}.exe");
    if words.len() != 4 || words[0] != "./runtest.exe" || words[1] != "-w" || words[2] != entry {
        return Err(format!(
            "{source}: expected ./runtest.exe -w {entry} SELECTOR"
        ));
    }
    let selector = words[3];
    catalog_name(selector, &source)?;
    Ok(Entry {
        name: selector.to_owned(),
        source,
        cwd: None,
        command: CommandSpec::Exec {
            program: words[0].to_owned(),
            args: words[1..].iter().map(|word| (*word).to_owned()).collect(),
        },
        protocol: Protocol::Pre2025Libctest {
            variant: variant.to_owned(),
            selector: selector.to_owned(),
        },
    })
}

/// 名称用于稳定 ID 和 Hook 消息，冒号留给明确的 ID 分段。
fn catalog_name(name: &str, at: &SourceLocation) -> Result<(), String> {
    if name.is_empty() || name.contains([':', '\0']) || name.chars().any(char::is_whitespace) {
        return Err(format!("{at}: invalid catalog name {name:?}"));
    }
    Ok(())
}

/// ID 与具体选择来源绑定，重复轮次和 include 的拼写不影响名称。
fn case_id(step: &Step, root: &str, name: &str) -> String {
    match step.kind {
        StepKind::Ltp => format!("ltp:{root}:{}:{name}", step.suite.as_ref().unwrap()),
        StepKind::Basic => format!("basic:{root}:{name}"),
        StepKind::Libctest => format!("libctest:{root}:{}:{name}", step.variant.as_ref().unwrap()),
        _ => unreachable!(),
    }
}

/// defaults、root 环境与 step 依次合并，命令数组整字段替换。
fn resolved(
    config: &Config,
    step: &Step,
    root_name: &str,
    id: String,
    entry: &Entry,
) -> ResolvedCase {
    let root_name = config
        .overrides
        .iter()
        .filter(|item| item.id == id)
        .find_map(|item| item.root.as_deref())
        .unwrap_or(root_name);
    let root = &config.roots[root_name];
    let defaults = &config.defaults;
    let mut env = defaults.env.clone();
    env.extend(root.env.clone());
    env.extend(step.env.clone());
    ResolvedCase {
        id,
        source: entry.source.clone(),
        root: root_name.to_owned(),
        cwd: step
            .cwd
            .clone()
            .or_else(|| entry.cwd.clone())
            .unwrap_or_else(|| root.cwd.clone()),
        command: entry.command.clone(),
        env,
        timeout_sec: step.timeout_sec.unwrap_or(defaults.timeout_sec),
        repeat: step.repeat.unwrap_or(defaults.repeat),
        hooks: step.hooks.unwrap_or(defaults.hooks),
        setup: step.setup.clone().unwrap_or_else(|| defaults.setup.clone()),
        cleanup: step
            .cleanup
            .clone()
            .unwrap_or_else(|| defaults.cleanup.clone()),
        protocol: entry.protocol.clone(),
    }
}

/// 覆盖先作用于每次命中；相同最终语义保首项，冲突保留两处来源。
fn insert(
    mut case: ResolvedCase,
    config: &Config,
    selected: &mut BTreeMap<String, usize>,
    cases: &mut Vec<ResolvedCase>,
) -> Result<(), String> {
    for item in &config.overrides {
        if item.id == case.id {
            apply_override(&mut case, item)?;
        }
    }
    if let Some(index) = selected.get(&case.id) {
        let first = &cases[*index];
        if !same_semantics(first, &case) {
            return Err(format!("{}: conflicting selection ID {:?}; first selected at {} (use an exact override or another ID)", case.source, case.id, first.source));
        }
    } else {
        selected.insert(case.id.clone(), cases.len());
        cases.push(case);
    }
    Ok(())
}

/// 来源位置不属于运行语义，重复引用同一项只保留最早的来源。
fn same_semantics(left: &ResolvedCase, right: &ResolvedCase) -> bool {
    left.root == right.root
        && left.cwd == right.cwd
        && left.command == right.command
        && left.env == right.env
        && left.timeout_sec == right.timeout_sec
        && left.repeat == right.repeat
        && left.hooks == right.hooks
        && left.setup == right.setup
        && left.cleanup == right.cleanup
        && left.protocol == right.protocol
}

/// 相同 ID 的多份覆盖可补充不同字段，同字段冲突必须显式修正输入。
fn validate_overrides(items: &[CaseOverride]) -> Result<(), String> {
    let mut seen: BTreeMap<&str, Vec<&CaseOverride>> = BTreeMap::new();
    for item in items {
        for previous in seen.entry(&item.id).or_default().iter() {
            macro_rules! conflict {
                ($($field:ident),+) => { $(
                    if let (Some(left), Some(right)) = (&previous.$field, &item.$field) {
                        if left != right { return Err(format!("{}: conflicting override field {} for {:?}; previous at {}", item.location.as_ref().unwrap(), stringify!($field), item.id, previous.location.as_ref().unwrap())); }
                    }
                )+ };
            }
            conflict!(
                root,
                cwd,
                program,
                args,
                shell,
                command,
                timeout_sec,
                repeat,
                hooks,
                setup,
                cleanup
            );
            for (key, value) in &item.env {
                if previous.env.get(key).is_some_and(|old| old != value) {
                    return Err(format!(
                        "{}: conflicting override env {key:?} for {:?}; previous at {}",
                        item.location.as_ref().unwrap(),
                        item.id,
                        previous.location.as_ref().unwrap()
                    ));
                }
            }
        }
        seen.get_mut(item.id.as_str()).unwrap().push(item);
    }
    Ok(())
}

/// 精确覆盖不改变选择 ID；root 的环境和 cwd 已在形成 case 时选定。
fn apply_override(case: &mut ResolvedCase, item: &CaseOverride) -> Result<(), String> {
    let at = item.location.as_ref().unwrap();
    match &mut case.command {
        CommandSpec::Exec { program, args } => {
            if item.shell.is_some() || item.command.is_some() {
                return Err(format!("{at}: shell override for exec ID {:?}", item.id));
            }
            if let Some(value) = &item.program {
                program.clone_from(value);
            }
            if let Some(value) = &item.args {
                args.clone_from(value);
            }
        }
        CommandSpec::Shell { shell, command } => {
            if item.program.is_some() || item.args.is_some() {
                return Err(format!("{at}: exec override for shell ID {:?}", item.id));
            }
            if let Some(value) = &item.shell {
                shell.clone_from(value);
            }
            if let Some(value) = &item.command {
                command.clone_from(value);
            }
        }
    }
    if let Some(value) = &item.root {
        case.root.clone_from(value);
    }
    if let Some(value) = &item.cwd {
        case.cwd.clone_from(value);
    }
    if let Some(value) = item.timeout_sec {
        case.timeout_sec = value;
    }
    if let Some(value) = item.repeat {
        case.repeat = value;
    }
    if let Some(value) = item.hooks {
        case.hooks = value;
    }
    if let Some(value) = &item.setup {
        case.setup.clone_from(value);
    }
    if let Some(value) = &item.cleanup {
        case.cleanup.clone_from(value);
    }
    case.env.extend(item.env.clone());
    Ok(())
}
