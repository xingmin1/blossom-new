//! 一层计划组合与输入边界校验，不访问比赛目录。

use crate::model::{
    CaseOverride, Config, Defaults, Finish, RootSpec, SourceLocation, Step, StepKind,
};
use glob::Pattern;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Plan {
    #[serde(default)]
    files: Vec<PathBuf>,
    #[serde(default)]
    override_files: Vec<PathBuf>,
    #[serde(default)]
    roots: BTreeMap<String, RootSpec>,
    #[serde(default)]
    defaults: Defaults,
    #[serde(default)]
    finish: Finish,
    #[serde(default, rename = "step")]
    steps: Vec<Step>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StepFile {
    #[serde(default, rename = "step")]
    steps: Vec<Step>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OverrideFile {
    #[serde(default, rename = "override")]
    overrides: Vec<CaseOverride>,
}

/// 读取入口及其直接引用；命令行列表仅替换 files，所有相对路径基于入口目录。
pub fn load(plan: &Path, files_override: &[PathBuf]) -> Result<Config, String> {
    let input: Plan = decode(plan)?;
    let base = plan.parent().unwrap_or_else(|| Path::new("."));
    let mut config = Config {
        roots: input.roots,
        defaults: input.defaults,
        steps: located_steps(input.steps, plan),
        overrides: Vec::new(),
        finish: input.finish,
        plan: plan.to_path_buf(),
    };
    let files = if files_override.is_empty() {
        &input.files
    } else {
        files_override
    };
    for file in files {
        let path = base.join(file);
        let input: StepFile = decode(&path)?;
        config.steps.extend(located_steps(input.steps, &path));
    }
    for file in input.override_files {
        let path = base.join(file);
        let input: OverrideFile = decode(&path)?;
        config
            .overrides
            .extend(
                input
                    .overrides
                    .into_iter()
                    .enumerate()
                    .map(|(index, mut item)| {
                        item.location = Some(location(&path, index));
                        item
                    }),
            );
    }
    validate(&config)?;
    Ok(config)
}

/// 解码错误保留实际文件名以及 TOML 提供的行列和字段路径。
fn decode<T: DeserializeOwned>(path: &Path) -> Result<T, String> {
    let text = fs::read_to_string(path).map_err(|error| format!("{}: {error}", path.display()))?;
    toml::from_str(&text).map_err(|error| format!("{}: {error}", path.display()))
}

/// 将 TOML 数组位置绑定到原文件，组合后不重新编号。
fn located_steps(steps: Vec<Step>, path: &Path) -> Vec<Step> {
    steps
        .into_iter()
        .enumerate()
        .map(|(index, mut step)| {
            step.location = Some(location(path, index));
            step
        })
        .collect()
}

/// 输入位置采用面向配置文件的从一开始序号。
fn location(path: &Path, index: usize) -> SourceLocation {
    SourceLocation {
        file: path.to_path_buf(),
        entry: index + 1,
        catalog: None,
        line: None,
    }
}

/// 校验只运行一次；目录、清单和选择命中留给实际 root 的展开。
fn validate(config: &Config) -> Result<(), String> {
    let plan = config.plan.display().to_string();
    timing(config.defaults.timeout_sec, config.defaults.repeat, &plan)?;
    environment(&config.defaults.env, &plan)?;
    commands(&config.defaults.setup, &plan)?;
    commands(&config.defaults.cleanup, &plan)?;
    for (name, root) in &config.roots {
        let at = format!("{plan}:roots.{name}");
        identifier(name, &at)?;
        absolute(&root.path, &at)?;
        absolute(&root.cwd, &at)?;
        environment(&root.env, &at)?;
        commands(&root.prepare, &at)?;
        let mut mounts = BTreeSet::new();
        for mount in &root.mounts {
            if !matches!(mount.as_str(), "dev" | "proc" | "tmp") || !mounts.insert(mount) {
                return Err(format!("{at}: invalid or repeated mount {mount:?}"));
            }
        }
    }
    for step in &config.steps {
        validate_step(step, config)?;
    }
    for item in &config.overrides {
        let at = item.location.as_ref().unwrap().to_string();
        identifier(&item.id, &at)?;
        if let Some(root) = &item.root {
            check_root(root, config, &at)?;
        }
        fields(
            item.cwd.as_deref(),
            item.timeout_sec,
            item.repeat,
            &item.env,
            item.setup.as_deref(),
            item.cleanup.as_deref(),
            &at,
        )?;
        for text in [&item.program, &item.shell, &item.command]
            .into_iter()
            .flatten()
        {
            nonempty(text, &at)?;
        }
        strings(item.args.as_deref().unwrap_or_default(), &at)?;
        if (item.program.is_some() || item.args.is_some())
            && (item.shell.is_some() || item.command.is_some())
        {
            return Err(format!("{at}: override mixes exec and shell fields"));
        }
    }
    Ok(())
}

/// 每种 step 只接受自身有意义的字段，避免拼成另一种隐式命令。
fn validate_step(step: &Step, config: &Config) -> Result<(), String> {
    let at = step.location.as_ref().unwrap().to_string();
    fields(
        step.cwd.as_deref(),
        step.timeout_sec,
        step.repeat,
        &step.env,
        step.setup.as_deref(),
        step.cleanup.as_deref(),
        &at,
    )?;
    for arch in &step.arch {
        if !matches!(arch.as_str(), "riscv64" | "loongarch64") {
            return Err(format!("{at}: unknown architecture {arch:?}"));
        }
    }
    let direct = matches!(step.kind, StepKind::Exec | StepKind::Shell);
    if direct {
        identifier(
            step.id
                .as_deref()
                .ok_or_else(|| format!("{at}: missing id"))?,
            &at,
        )?;
        if !step.roots.is_empty()
            || !step.include.is_empty()
            || !step.exclude.is_empty()
            || step.catalog.is_some()
            || step.directory.is_some()
            || step.source.is_some()
            || step.suite.is_some()
            || step.variant.is_some()
        {
            return Err(format!("{at}: exec/shell cannot contain selection fields"));
        }
        check_root(
            step.root
                .as_deref()
                .ok_or_else(|| format!("{at}: missing root"))?,
            config,
            &at,
        )?;
    } else {
        if step.root.is_some()
            || step.id.is_some()
            || step.program.is_some()
            || step.args.is_some()
            || step.command.is_some()
        {
            return Err(format!(
                "{at}: catalog step cannot contain direct command fields"
            ));
        }
        if step.roots.is_empty() || step.include.is_empty() {
            return Err(format!("{at}: roots and include must not be empty"));
        }
        for root in &step.roots {
            check_root(root, config, &at)?;
        }
        for pattern in step.include.iter().chain(&step.exclude) {
            nonempty(pattern, &at)?;
            Pattern::new(pattern)
                .map_err(|error| format!("{at}: invalid pattern {pattern:?}: {error}"))?;
        }
        if let Some(catalog) = &step.catalog {
            absolute(catalog, &at)?;
        }
        if let Some(directory) = &step.directory {
            absolute(directory, &at)?;
        }
    }
    match step.kind {
        StepKind::Exec => {
            nonempty(
                step.program
                    .as_deref()
                    .ok_or_else(|| format!("{at}: missing program"))?,
                &at,
            )?;
            if step.shell.is_some() || step.command.is_some() {
                return Err(format!("{at}: exec has shell fields"));
            }
        }
        StepKind::Shell => {
            nonempty(
                step.shell
                    .as_deref()
                    .ok_or_else(|| format!("{at}: missing shell"))?,
                &at,
            )?;
            nonempty(
                step.command
                    .as_deref()
                    .ok_or_else(|| format!("{at}: missing command"))?,
                &at,
            )?;
            if step.program.is_some() || step.args.is_some() {
                return Err(format!("{at}: shell has exec fields"));
            }
        }
        StepKind::Ltp => {
            identifier(
                step.suite
                    .as_deref()
                    .ok_or_else(|| format!("{at}: missing suite"))?,
                &at,
            )?;
            if step.variant.is_some() {
                return Err(format!("{at}: ltp has variant"));
            }
            match step.source.unwrap_or_default() {
                crate::model::CatalogSource::Runtest => {
                    if step.catalog.is_none() || step.directory.is_some() {
                        return Err(format!("{at}: runtest requires catalog and forbids directory (no directory fallback)"));
                    }
                    if let Some(shell) = &step.shell {
                        nonempty(shell, &at)?;
                    }
                }
                crate::model::CatalogSource::Executables => {
                    if step.directory.is_none() || step.catalog.is_some() || step.shell.is_some() {
                        return Err(format!(
                            "{at}: executables requires directory and forbids catalog/shell"
                        ));
                    }
                }
            }
        }
        StepKind::Basic | StepKind::Libctest => {
            if step.source.is_some() || step.suite.is_some() || step.shell.is_some() {
                return Err(format!("{at}: basic/libctest has ltp fields"));
            }
            if step.kind == StepKind::Libctest {
                if step.directory.is_some() {
                    return Err(format!("{at}: libctest uses catalog, not directory"));
                }
                if !matches!(step.variant.as_deref(), Some("dynamic" | "static")) {
                    return Err(format!("{at}: libctest variant must be dynamic or static"));
                }
            } else if step.variant.is_some() || step.catalog.is_some() {
                return Err(format!("{at}: basic uses directory and has no variant"));
            }
        }
    }
    strings(step.args.as_deref().unwrap_or_default(), &at)
}

/// 引用只绑定显式声明的 root，不按名字猜测 libc 或盘布局。
fn check_root(name: &str, config: &Config, at: &str) -> Result<(), String> {
    if !config.roots.contains_key(name) {
        return Err(format!("{at}: unknown root {name:?}"));
    }
    Ok(())
}

/// 公共运行字段在形成具体 case 前完成边界校验。
fn fields(
    cwd: Option<&Path>,
    timeout: Option<u64>,
    repeat: Option<u32>,
    env: &BTreeMap<String, String>,
    setup: Option<&[Vec<String>]>,
    cleanup: Option<&[Vec<String>]>,
    at: &str,
) -> Result<(), String> {
    if let Some(cwd) = cwd {
        absolute(cwd, at)?;
    }
    timing(timeout.unwrap_or(1), repeat.unwrap_or(1), at)?;
    environment(env, at)?;
    commands(setup.unwrap_or_default(), at)?;
    commands(cleanup.unwrap_or_default(), at)
}

/// 零轮次和零期限属于配置错误，不能产生空成功。
fn timing(timeout: u64, repeat: u32, at: &str) -> Result<(), String> {
    if timeout == 0 || repeat == 0 {
        return Err(format!("{at}: timeout_sec and repeat must be positive"));
    }
    Ok(())
}

/// 不解析链接，只限制路径仍位于声明的系统根或执行根。
pub(crate) fn absolute(path: &Path, at: &str) -> Result<(), String> {
    if !path.is_absolute()
        || path.components().any(|part| part == Component::ParentDir)
        || path.as_os_str().as_encoded_bytes().contains(&0)
    {
        return Err(format!(
            "{at}: expected absolute path without parent components: {}",
            path.display()
        ));
    }
    Ok(())
}

/// Hook 的基本 ID 不含空白或 NUL，保留冒号分段的稳定名称。
fn identifier(text: &str, at: &str) -> Result<(), String> {
    nonempty(text, at)?;
    if text.chars().any(char::is_whitespace) {
        return Err(format!("{at}: invalid identifier {text:?}"));
    }
    Ok(())
}

/// 命令名和选择器不得为空或带无法传给 exec 的 NUL。
fn nonempty(text: &str, at: &str) -> Result<(), String> {
    if text.is_empty() || text.contains('\0') {
        return Err(format!("{at}: empty or NUL-containing value"));
    }
    Ok(())
}

/// argv 保留空参数、空格与 Unicode，只拒绝系统调用无法承载的 NUL。
fn strings(values: &[String], at: &str) -> Result<(), String> {
    if values.iter().any(|text| text.contains('\0')) {
        return Err(format!("{at}: argument contains NUL"));
    }
    Ok(())
}

/// 环境以 key 合并，非法 key 在输入边界报告。
fn environment(env: &BTreeMap<String, String>, at: &str) -> Result<(), String> {
    for (key, value) in env {
        if key.is_empty() || key.contains(['=', '\0']) || value.contains('\0') {
            return Err(format!("{at}: invalid environment key/value {key:?}"));
        }
    }
    Ok(())
}

/// 准备与清理使用 argv 数组，不引入第二份 Shell 拆词规则。
fn commands(commands: &[Vec<String>], at: &str) -> Result<(), String> {
    for command in commands {
        nonempty(
            command
                .first()
                .ok_or_else(|| format!("{at}: empty command array"))?,
            at,
        )?;
        strings(command, at)?;
    }
    Ok(())
}
