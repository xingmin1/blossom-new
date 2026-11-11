//! 比赛计划（TOML）的一层文件组合与静态输入边界校验。
//!
//! 主计划给出 roots、defaults、finish，可直接内联 `[[group]]`，再按顺序加载 `files` 与 `override_files`；
//! 所有相对路径都相对主计划所在目录解析，外部文件只允许一层、不递归。命令行 `--file` 列表非空时整体替换
//! 主计划的 `files`，roots/defaults 仍取自主计划。
//!
//! 静态校验只覆盖会直接造成误运行的问题：字段拼写、路径与架构取值、三种来源互斥、超时/重复数为正、环境变量与
//! 准备命令的 argv 形状、同一组名与测试根被两个分组重复声明。目录内容、清单行与 glob 命中情况留给
//! [`crate::selection`] 在挂载后检查。错误文本始终带上文件与条目来源。

use crate::model::{CaseOverride, Config, Defaults, Finish, Group, RootSpec, SourceLocation, Step};
use glob::Pattern;
use serde::de::DeserializeOwned;
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};

/// 主计划文件的 TOML 结构。
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Plan {
    /// 外部分组文件，按声明顺序加载。
    #[serde(default)]
    files: Vec<PathBuf>,
    /// 外部覆盖文件，按声明顺序加载。
    #[serde(default)]
    override_files: Vec<PathBuf>,
    /// 全部用例结束后的收尾动作。
    #[serde(default)]
    finish: Finish,
    /// 全局默认执行参数。
    #[serde(default)]
    defaults: Defaults,
    /// 命名测试根目录。
    roots: BTreeMap<String, RootSpec>,
    /// 主计划内直接声明的分组。
    #[serde(default, rename = "group")]
    groups: Vec<Group>,
}

/// 外部分组文件的结构，只允许 `[[group]]`。
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct GroupFile {
    /// 该文件声明的全部分组。
    #[serde(rename = "group")]
    groups: Vec<Group>,
}

/// 外部覆盖文件的结构，只允许 `[[override]]`。
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct OverrideFile {
    /// 该文件声明的全部覆盖规则。
    #[serde(rename = "override")]
    overrides: Vec<CaseOverride>,
}

/// 加载主计划及其引用文件，填入来源位置并完成静态校验。
///
/// 在挂载比赛盘之前调用；`files_override` 来自命令行 `--file`，非空时替换主计划的 `files`。
/// 读取失败、TOML 语法或字段错误、静态校验失败都返回带文件与条目位置的错误字符串。
pub fn load(plan: &Path, files_override: &[PathBuf]) -> Result<Config, String> {
    // Step 1: 解析主计划，保留自身声明的分组并记录来源。
    let input: Plan = decode(plan)?;
    let base = plan.parent().unwrap_or_else(|| Path::new("."));
    let mut config = Config {
        plan: plan.to_path_buf(),
        roots: input.roots,
        defaults: input.defaults,
        finish: input.finish,
        groups: located(input.groups, plan),
        overrides: Vec::new(),
    };
    // Step 2: 依次加载分组文件，每个文件内的组号与步骤号重新从 1 开始。
    let files = if files_override.is_empty() {
        &input.files
    } else {
        files_override
    };
    for file in files {
        let path = base.join(file);
        let input: GroupFile = decode(&path)?;
        config.groups.extend(located(input.groups, &path));
    }
    // Step 3: 依次加载覆盖文件，覆盖条目按文件内的声明顺序生效。
    for file in &input.override_files {
        let path = base.join(file);
        let input: OverrideFile = decode(&path)?;
        for (index, mut item) in input.overrides.into_iter().enumerate() {
            item.location = Some(entry(&path, format!("override {}", index + 1)));
            config.overrides.push(item);
        }
    }
    validate(&config)?;
    Ok(config)
}

/// 读取指定路径的 TOML 文件并在失败时附上文件路径。
fn decode<T: DeserializeOwned>(path: &Path) -> Result<T, String> {
    let text = fs::read_to_string(path).map_err(|error| format!("{}: {error}", path.display()))?;
    toml::from_str(&text).map_err(|error| format!("{}: {error}", path.display()))
}

/// 构造不带目录/清单来源的位置描述。
fn entry(file: &Path, text: String) -> SourceLocation {
    SourceLocation {
        file: file.to_path_buf(),
        entry: text,
        catalog: None,
        line: None,
    }
}

/// 为文件内的分组与步骤填入来源位置：组号为 `group N`，步骤为 `group N step M`。
fn located(groups: Vec<Group>, path: &Path) -> Vec<Group> {
    groups
        .into_iter()
        .enumerate()
        .map(|(group_index, mut group)| {
            let group_entry = format!("group {}", group_index + 1);
            for (step_index, step) in group.steps.iter_mut().enumerate() {
                step.location = Some(entry(
                    path,
                    format!("{group_entry} step {}", step_index + 1),
                ));
            }
            group.location = Some(entry(path, group_entry));
            group
        })
        .collect()
}

/// 取出配置加载阶段填入的来源位置文本，用作错误前缀。
fn source(location: &Option<SourceLocation>) -> String {
    location
        .as_ref()
        .expect("config loader sets item locations")
        .to_string()
}

/// 对合并后的配置执行一次静态校验。
fn validate(config: &Config) -> Result<(), String> {
    // Step 1: 校验全局默认参数。
    let plan = config.plan.display().to_string();
    timing(
        config.defaults.timeout_sec,
        config.defaults.repeat,
        &format!("{plan}: defaults"),
    )?;
    // Step 2: 校验每个测试根目录的名称、路径、环境变量、挂载点与准备命令。
    for (name, root) in &config.roots {
        let at = format!("{plan}: roots.{name}");
        identifier(name, &at)?;
        absolute(&root.path, &at)?;
        absolute(&root.cwd, &at)?;
        environment(&root.env, &at)?;
        commands(&root.prepare, &at)?;
        let mut mounts = BTreeSet::new();
        for mount in &root.mounts {
            if !matches!(mount.as_str(), "dev" | "shm" | "proc" | "tmp") || !mounts.insert(mount) {
                return Err(format!("{at}: invalid or repeated mount {mount:?}"));
            }
        }
    }
    // Step 3: 校验每个分组及其步骤，并拒绝两个分组声明同一 (组名, 测试根)。
    let mut declared: BTreeMap<(&str, &str), String> = BTreeMap::new();
    for group in &config.groups {
        validate_group(group, config)?;
        let location = source(&group.location);
        for root in &group.roots {
            if let Some(first) =
                declared.insert((group.name.as_str(), root.as_str()), location.clone())
            {
                return Err(format!(
                    "{location}: duplicate group {:?} root {root:?}; first declared at {first}",
                    group.name
                ));
            }
        }
    }
    // Step 4: 校验覆盖规则的字段与架构过滤条件。
    for item in &config.overrides {
        validate_override(item)?;
    }
    Ok(())
}

/// 校验分组名称、测试根引用、默认工作目录与每个步骤。
fn validate_group(group: &Group, config: &Config) -> Result<(), String> {
    let at = source(&group.location);
    identifier(&group.name, &at)?;
    if group.roots.is_empty() {
        return Err(format!("{at}: roots must not be empty"));
    }
    for root in &group.roots {
        if !config.roots.contains_key(root) {
            return Err(format!("{at}: unknown root {root:?}"));
        }
    }
    if let Some(cwd) = &group.cwd {
        absolute(cwd, &at)?;
    }
    environment(&group.env, &at)?;
    if group.steps.is_empty() {
        return Err(format!("{at}: step must not be empty"));
    }
    for step in &group.steps {
        validate_step(step)?;
    }
    Ok(())
}

/// 校验单个步骤的来源互斥、argv、路径、环境变量、超时与架构取值。
fn validate_step(step: &Step) -> Result<(), String> {
    let at = source(&step.location);
    let sources = [
        step.names.is_some(),
        step.directory.is_some(),
        step.catalog.is_some(),
    ];
    if sources.iter().filter(|present| **present).count() != 1 {
        return Err(format!(
            "{at}: exactly one of names, directory and catalog is required"
        ));
    }
    if let Some(names) = &step.names {
        if names.is_empty() {
            return Err(format!("{at}: names must not be empty"));
        }
        for name in names {
            identifier(name, &at)?;
        }
        if step.include.is_some() || !step.exclude.is_empty() {
            return Err(format!("{at}: names is already the explicit selection"));
        }
    }
    if let Some(catalog) = &step.catalog {
        absolute(catalog, &at)?;
    }
    for text in step.include.iter().flatten().chain(&step.exclude) {
        pattern(text, &at)?;
    }
    if step.exec.is_empty() {
        return Err(format!("{at}: exec must not be empty"));
    }
    nonempty(&step.exec[0], &at)?;
    strings(&step.exec, &at)?;
    if let Some(cwd) = &step.cwd {
        absolute(cwd, &at)?;
    }
    environment(&step.env, &at)?;
    timing(step.timeout_sec.unwrap_or(1), step.repeat.unwrap_or(1), &at)?;
    commands(&step.setup, &at)?;
    commands(&step.cleanup, &at)?;
    for arch in &step.arch {
        architecture(arch, &at)?;
    }
    Ok(())
}

/// 校验覆盖规则的目标 ID、架构过滤与整体替换字段。
fn validate_override(item: &CaseOverride) -> Result<(), String> {
    let at = source(&item.location);
    identifier(&item.id, &at)?;
    for arch in &item.arch {
        architecture(arch, &at)?;
    }
    if let Some(exec) = &item.exec {
        if exec.is_empty() {
            return Err(format!("{at}: exec must not be empty"));
        }
        nonempty(&exec[0], &at)?;
        strings(exec, &at)?;
    }
    if let Some(cwd) = &item.cwd {
        absolute(cwd, &at)?;
    }
    environment(&item.env, &at)?;
    timing(item.timeout_sec.unwrap_or(1), item.repeat.unwrap_or(1), &at)?;
    if let Some(setup) = &item.setup {
        commands(setup, &at)?;
    }
    if let Some(cleanup) = &item.cleanup {
        commands(cleanup, &at)?;
    }
    Ok(())
}

/// 校验超时秒数与重复次数必须为正整数。
fn timing(timeout: u64, repeat: u32, at: &str) -> Result<(), String> {
    if timeout == 0 || repeat == 0 {
        return Err(format!("{at}: timeout_sec and repeat must be positive"));
    }
    Ok(())
}

/// 校验架构名只使用支持的两个目标名称。
fn architecture(text: &str, at: &str) -> Result<(), String> {
    if !matches!(text, "riscv64" | "loongarch64") {
        return Err(format!("{at}: unknown architecture {text:?}"));
    }
    Ok(())
}

/// 校验 include/exclude 的每个模式都是合法 glob。
fn pattern(text: &str, at: &str) -> Result<(), String> {
    nonempty(text, at)?;
    Pattern::new(text).map_err(|error| format!("{at}: invalid pattern {text:?}: {error}"))?;
    Ok(())
}

/// 校验路径必须是绝对路径、不含 `..` 组件且不含 NUL 字节。
fn absolute(path: &Path, at: &str) -> Result<(), String> {
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

/// 校验标识符（根名、组名、names 项、覆盖 ID）非空且不含空白字符与 NUL。
fn identifier(text: &str, at: &str) -> Result<(), String> {
    nonempty(text, at)?;
    if text.chars().any(char::is_whitespace) {
        return Err(format!("{at}: invalid identifier {text:?}"));
    }
    Ok(())
}

/// 校验字符串非空且不含 `execve` 无法表示的 NUL。
fn nonempty(text: &str, at: &str) -> Result<(), String> {
    if text.is_empty() || text.contains('\0') {
        return Err(format!("{at}: empty or NUL-containing value"));
    }
    Ok(())
}

/// 校验 argv 中每个字符串都不含 NUL（空格与其他字符允许）。
fn strings(values: &[String], at: &str) -> Result<(), String> {
    if values.iter().any(|text| text.contains('\0')) {
        return Err(format!("{at}: argument contains NUL"));
    }
    Ok(())
}

/// 校验环境变量键名非空且不含 `=`/NUL，键值不含 NUL。
fn environment(env: &BTreeMap<String, String>, at: &str) -> Result<(), String> {
    for (key, value) in env {
        if key.is_empty() || key.contains(['=', '\0']) || value.contains('\0') {
            return Err(format!("{at}: invalid environment key/value {key:?}"));
        }
    }
    Ok(())
}

/// 校验准备/清理命令列表：每条 argv 非空、首项非空且全部参数不含 NUL。
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
