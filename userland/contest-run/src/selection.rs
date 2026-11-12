//! 把配置展开为按执行顺序排列的用例组，并应用覆盖规则与命令行过滤。
//!
//! 展开顺序固定为 分组 → 测试根 → 步骤 → 名字：同一分组在一个测试根下跑完全部步骤，再切换到下一个测试根。
//! 三种来源统一产出候选名字：`names` 保持声明顺序，`directory` 只取普通且带任一执行位的文件并按名字排序，
//! `catalog` 每行首个词为名字、其余为命令原文并保持文件顺序。
//!
//! 每个候选按 `{group}`、`{root}`、`{name}`、`{command}` 展开 ID；覆盖规则按声明顺序整体替换字段并合并环境变量，
//! 之后再用完整字段（含 `{id}`）单遍展开 argv 与 before。after 保留原始模板，等执行结束拿到 `{status}` 一次展开。
//! 命令行 `--only` 只在最后缩小集合。本模块不检查 argv[0] 在目标根内是否存在。

use crate::command::{Root, SourcePath};
use crate::expand::expand;
use crate::model::{
    CaseOverride, Config, Group, ResolvedCase, ResolvedGroup, RootSpec, SourceLocation, Step,
};
use crate::runtime::rooted_path;
use glob::Pattern;
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

/// 未显式给出 `id` 时的用例 ID 模板；占位符由 [`expand`] 展开，不是 `format!` 参数。
const DEFAULT_ID_TEMPLATE: &str = "{group}:{root}:{name}";

/// 用例选择结果。
pub struct Selection {
    /// 按 分组 → 测试根 顺序展开的非空用例组。
    pub groups: Vec<ResolvedGroup>,
    /// 未命中任何已选用例的覆盖规则，每项为 `{id} at {来源}`。
    pub unmatched: Vec<String>,
}

/// 来源中提取出的候选条目。
#[derive(Clone)]
struct Candidate {
    /// 参与 include/exclude 匹配与模板展开的名字。
    name: String,
    /// 清单来源的命令原文，即 `{command}` 的值。
    command: Option<String>,
    /// 候选的定义位置，已补上目录/清单路径与行号。
    source: SourceLocation,
}

/// 展开配置：按架构与 `only` 过滤，应用覆盖规则，返回分组顺序稳定的用例集合。
///
/// `arch` 用于跳过不适用于当前架构的步骤与覆盖规则；`only` 非空时只保留 ID 命中任一 glob 的用例，
/// 非法 glob、任一 include 无候选、ID 不合法或重复、最终集合为空都返回带来源的错误字符串。
pub fn resolve(config: &Config, arch: &str, only: &[String]) -> Result<Selection, String> {
    let mut groups: Vec<ResolvedGroup> = Vec::new();
    let mut index: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    for group in &config.groups {
        for root_name in &group.roots {
            let root = &config.roots[root_name];
            let mut cases: Vec<ResolvedCase> = Vec::new();
            for step in &group.steps {
                if !step.enabled || !matches_arch(&step.arch, arch) {
                    continue;
                }
                // 有效 cwd 与 env 由步骤、分组、测试根逐层覆盖得到。
                let cwd = step
                    .cwd
                    .clone()
                    .or_else(|| group.cwd.clone())
                    .unwrap_or_else(|| root.cwd.clone());
                let env = environment(root, group, step);
                let at = located(&step.location);
                let template = step.id.as_deref().unwrap_or(DEFAULT_ID_TEMPLATE);
                for candidate in candidates(step, root, &cwd, at)? {
                    let id = expand(
                        template,
                        &fields(
                            &group.name,
                            root_name,
                            &candidate.name,
                            candidate.command.as_deref(),
                            None,
                        ),
                    );
                    check_id(&id, at)?;
                    if let Some(&(other_group, other_case)) = index.get(&id) {
                        // 同一分组内的重复项还在 `cases` 中，尚未并入 `groups`。
                        let first = if other_group < groups.len() {
                            &groups[other_group].cases[other_case].source
                        } else {
                            &cases[other_case].source
                        };
                        return Err(format!(
                            "{at}: duplicate case ID {id:?}; first selected at {first}"
                        ));
                    }
                    let case = ResolvedCase {
                        id: id.clone(),
                        group: group.name.clone(),
                        root: root_name.clone(),
                        name: candidate.name,
                        catalog_command: candidate.command,
                        exec: step.exec.clone(),
                        cwd: cwd.clone(),
                        env: env.clone(),
                        timeout_sec: step.timeout_sec.unwrap_or(config.defaults.timeout_sec),
                        repeat: step.repeat.unwrap_or(config.defaults.repeat),
                        setup: step.setup.clone(),
                        cleanup: step.cleanup.clone(),
                        before: step.before.clone(),
                        after: step.after.clone(),
                        source: candidate.source,
                    };
                    index.insert(id, (groups.len(), cases.len()));
                    cases.push(case);
                }
            }
            if cases.is_empty() {
                continue;
            }
            groups.push(ResolvedGroup {
                name: group.name.clone(),
                root: root_name.clone(),
                start: group_text(&group.start, &group.name, root_name),
                end: group_text(&group.end, &group.name, root_name),
                cwd: group.cwd.clone().unwrap_or_else(|| root.cwd.clone()),
                env: {
                    let mut env = root.env.clone();
                    env.extend(group.env.clone());
                    env
                },
                setup: group.setup.clone(),
                cleanup: group.cleanup.clone(),
                cases,
            });
        }
    }
    // 覆盖规则只调整已选用例；架构不匹配的规则直接跳过，命中不了的目标记入 unmatched。
    let mut unmatched = Vec::new();
    for rule in &config.overrides {
        if !matches_arch(&rule.arch, arch) {
            continue;
        }
        match index.get(&rule.id) {
            Some(&(group, case)) => apply(&mut groups[group].cases[case], rule),
            None => unmatched.push(format!("{} at {}", rule.id, located(&rule.location))),
        }
    }
    if !only.is_empty() {
        let at = config.plan.display().to_string();
        let patterns = patterns(only, &at)?;
        for group in &mut groups {
            group
                .cases
                .retain(|case| patterns.iter().any(|pattern| pattern.matches(&case.id)));
        }
        groups.retain(|group| !group.cases.is_empty());
    }
    for group in &mut groups {
        let test_root = config.roots[&group.root].path.to_string_lossy();
        let pairs = [
            ("group", group.name.as_str()),
            ("root", group.root.as_str()),
            ("test_root", test_root.as_ref()),
        ];
        group.setup = group
            .setup
            .iter()
            .map(|command| command.expanded(&pairs))
            .collect();
        group.cleanup = group
            .cleanup
            .iter()
            .map(|command| command.expanded(&pairs))
            .collect();
        for case in &mut group.cases {
            expand_case(case, &test_root);
        }
    }
    if groups.is_empty() {
        return Err(format!(
            "{}: empty selection for architecture {arch}",
            config.plan.display()
        ));
    }
    Ok(Selection { groups, unmatched })
}

/// 判断架构列表是否放行当前架构；空列表表示不限制。
fn matches_arch(arch: &[String], current: &str) -> bool {
    arch.is_empty() || arch.iter().any(|value| value == current)
}

/// 合并环境变量：根默认值被分组与步骤按键覆盖。
fn environment(root: &RootSpec, group: &Group, step: &Step) -> BTreeMap<String, String> {
    let mut env = root.env.clone();
    env.extend(group.env.clone());
    env.extend(step.env.clone());
    env
}

/// 取出配置加载阶段填入的来源位置。
fn located(location: &Option<SourceLocation>) -> &SourceLocation {
    location
        .as_ref()
        .expect("config loader sets item locations")
}

/// 列出模板替换字段：分组、测试根、名字、可选清单命令，以及算出后的 ID。
///
/// ID 自身也由分组、测试根与名字展开，因此计算 ID 时传 `None`，之后展开 argv 与 before 时再带上 ID。
fn fields<'a>(
    group: &'a str,
    root: &'a str,
    name: &'a str,
    command: Option<&'a str>,
    id: Option<&'a str>,
) -> Vec<(&'a str, &'a str)> {
    let mut fields = vec![("group", group), ("root", root), ("name", name)];
    if let Some(command) = command {
        fields.push(("command", command));
    }
    if let Some(id) = id {
        fields.push(("id", id));
    }
    fields
}

/// 用分组与测试根展开组首尾文本。
fn group_text(template: &str, group: &str, root: &str) -> String {
    expand(template, &[("group", group), ("root", root)])
}

/// 校验 ID 可直接写入设备通知：非空、无空白与 NUL，且整条 `before ID\n` 不超过 512 字节。
fn check_id(id: &str, at: &SourceLocation) -> Result<(), String> {
    if id.is_empty() || id.contains('\0') || id.chars().any(char::is_whitespace) {
        return Err(format!("{at}: invalid case ID {id:?}"));
    }
    if 512 < id.len() + "before ".len() + 1 {
        return Err(format!("{at}: case ID too long for device message {id:?}"));
    }
    Ok(())
}

/// 按步骤声明的来源列出候选条目。
fn candidates(
    step: &Step,
    root: &RootSpec,
    cwd: &Path,
    at: &SourceLocation,
) -> Result<Vec<Candidate>, String> {
    // names 的元素就是数据，不展开 glob，也不经过 include/exclude。
    if let Some(names) = &step.names {
        return Ok(names
            .iter()
            .map(|name| Candidate {
                name: name.clone(),
                command: None,
                source: at.clone(),
            })
            .collect());
    }
    if let Some(directory) = &step.directory {
        let (source_root, source_cwd, path) = source_view(directory, root, cwd);
        return select(
            step,
            directory_entries(source_root, source_cwd, path, at)?,
            path,
            at,
        );
    }
    let catalog = step
        .catalog
        .as_ref()
        .expect("step declares exactly one source");
    let (source_root, source_cwd, path) = source_view(catalog, root, cwd);
    select(
        step,
        catalog_entries(source_root, source_cwd, path, at)?,
        path,
        at,
    )
}

/// 为读取来源选择根视图，不改变测例所属根或命令的执行根。
fn source_view<'a>(
    source: &'a SourcePath,
    root: &'a RootSpec,
    cwd: &'a Path,
) -> (&'a Path, &'a Path, &'a Path) {
    let (scope, path) = source.parts();
    match scope {
        Root::System => (Path::new("/"), Path::new("/"), path),
        Root::Test => (&root.path, cwd, path),
    }
}

/// 列出所选来源根内普通且有任一执行位的文件，按名字升序返回。
fn directory_entries(
    root: &Path,
    cwd: &Path,
    directory: &Path,
    at: &SourceLocation,
) -> Result<Vec<Candidate>, String> {
    let listing = rooted_path(root, cwd, directory)
        .and_then(fs::read_dir)
        .map_err(|error| format!("{at}: {}: {error}", directory.display()))?;
    let source = SourceLocation {
        catalog: Some(directory.to_path_buf()),
        ..at.clone()
    };
    let mut entries = Vec::new();
    for item in listing {
        let item = item.map_err(|error| format!("{at}: {}: {error}", directory.display()))?;
        let name = item
            .file_name()
            .into_string()
            .map_err(|_| format!("{at}: non-UTF-8 file name in {}", directory.display()))?;
        let metadata = rooted_path(root, cwd, &directory.join(&name))
            .and_then(fs::metadata)
            .map_err(|error| format!("{at}: {}: {error}", directory.display()))?;
        if !metadata.is_file() || metadata.permissions().mode() & 0o111 == 0 {
            continue;
        }
        entries.push(Candidate {
            name,
            command: None,
            source: source.clone(),
        });
    }
    entries.sort_by(|left, right| left.name.cmp(&right.name));
    Ok(entries)
}

/// 在所选来源根内读取清单，保留声明路径、行号和命令原文。
fn catalog_entries(
    root: &Path,
    cwd: &Path,
    catalog: &Path,
    at: &SourceLocation,
) -> Result<Vec<Candidate>, String> {
    let text = rooted_path(root, cwd, catalog)
        .and_then(fs::read_to_string)
        .map_err(|error| format!("{at}: {}: {error}", catalog.display()))?;
    let mut entries = Vec::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim_start();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let source = SourceLocation {
            catalog: Some(catalog.to_path_buf()),
            line: Some(index + 1),
            ..at.clone()
        };
        let Some(end) = line.find(char::is_whitespace) else {
            return Err(format!("{source}: line has no command"));
        };
        let command = line[end..].trim_start();
        if command.is_empty() {
            return Err(format!("{source}: empty command"));
        }
        entries.push(Candidate {
            name: line[..end].to_owned(),
            command: Some(command.to_owned()),
            source,
        });
    }
    Ok(entries)
}

/// 按 include 顺序取匹配候选，同一步骤内按名字去重，最后剔除 exclude 命中的条目。
///
/// include 省略时取 `["*"]`；任一 include 在原始候选中一个都没匹配即报错，exclude 无匹配不算错误。
fn select(
    step: &Step,
    entries: Vec<Candidate>,
    origin: &Path,
    at: &SourceLocation,
) -> Result<Vec<Candidate>, String> {
    let default = ["*".to_owned()];
    let prefix = at.to_string();
    let inclusions = patterns(step.include.as_deref().unwrap_or(&default), &prefix)?;
    let exclusions = patterns(&step.exclude, &prefix)?;
    let mut result = Vec::new();
    let mut seen = BTreeSet::new();
    for inclusion in &inclusions {
        let mut matched = false;
        for entry in &entries {
            if !inclusion.matches(&entry.name) {
                continue;
            }
            matched = true;
            if exclusions
                .iter()
                .any(|pattern| pattern.matches(&entry.name))
            {
                continue;
            }
            if !seen.insert(entry.name.clone()) {
                continue;
            }
            result.push(entry.clone());
        }
        if !matched {
            return Err(format!(
                "{at}: {inclusion:?} matched no candidate in {}",
                origin.display()
            ));
        }
    }
    Ok(result)
}

/// 编译 glob 模式列表，非法模式带着给定前缀报错。
fn patterns(texts: &[String], at: &str) -> Result<Vec<Pattern>, String> {
    texts
        .iter()
        .map(|text| {
            Pattern::new(text).map_err(|error| format!("{at}: invalid pattern {text:?}: {error}"))
        })
        .collect()
}

/// 把命中的覆盖规则整字段替换到用例上，环境变量按键合并，多条规则按声明顺序覆盖前者。
fn apply(case: &mut ResolvedCase, rule: &CaseOverride) {
    if let Some(exec) = &rule.exec {
        case.exec.clone_from(exec);
    }
    if let Some(cwd) = &rule.cwd {
        case.cwd.clone_from(cwd);
    }
    if let Some(timeout) = rule.timeout_sec {
        case.timeout_sec = timeout;
    }
    if let Some(repeat) = rule.repeat {
        case.repeat = repeat;
    }
    if let Some(setup) = &rule.setup {
        case.setup.clone_from(setup);
    }
    if let Some(cleanup) = &rule.cleanup {
        case.cleanup.clone_from(cleanup);
    }
    if let Some(before) = &rule.before {
        case.before.clone_from(before);
    }
    if let Some(after) = &rule.after {
        case.after.clone_from(after);
    }
    case.env.extend(rule.env.clone());
}

/// 用完整字段单遍展开所有命令与 before；after 保持原始模板，执行结束时再展开。
fn expand_case(case: &mut ResolvedCase, test_root: &str) {
    let mut pairs = fields(
        &case.group,
        &case.root,
        &case.name,
        case.catalog_command.as_deref(),
        Some(&case.id),
    );
    pairs.push(("test_root", test_root));
    let exec = case.exec.expanded(&pairs);
    let setup = case
        .setup
        .iter()
        .map(|command| command.expanded(&pairs))
        .collect();
    let cleanup = case
        .cleanup
        .iter()
        .map(|command| command.expanded(&pairs))
        .collect();
    let before = expand(&case.before, &pairs);
    case.exec = exec;
    case.setup = setup;
    case.cleanup = cleanup;
    case.before = before;
}
