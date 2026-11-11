//! 比赛计划的数据模型：Root、Group、Step 三级配置与展开后的可执行用例。
//!
//! Root 是测试程序看到的根目录及其运行环境（cwd、env、挂载点、准备命令）；Group 是一个完整输出分组，
//! 含一个或多个 Step 与分组首尾文本；Step 按 `names`、`directory`、`catalog` 三种来源之一展开成具体用例。
//! 展开后的 [`ResolvedCase`] 只保留执行数据：argv、cwd、env、超时、重复轮数、setup/cleanup 与 before/after 文本。
//!
//! 模板字段固定为 `{group}`、`{root}`、`{name}`、`{id}`、清单来源的 `{command}`，以及 after 文本执行结束时
//! 才提供的 `{status}`；单次替换，插入的文本不再展开。after 保持原始模板，避免先插入命令名再被二次替换。

use serde::Deserialize;
use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

/// 配置条目或清单行的定义位置，用于错误定位、`--list` 与 `override` 未命中提示。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceLocation {
    /// 定义该条目的 TOML 文件路径。
    pub file: PathBuf,
    /// 条目在文件内的位置，如 `group 2`、`group 2 step 1`、`override 3`（序号从 1 起）。
    pub entry: String,
    /// 目录或清单来源在测试 root 内的路径；`names` 来源没有此项。
    pub catalog: Option<PathBuf>,
    /// 清单来源的行号（从 1 起）；目录来源没有行号。
    pub line: Option<usize>,
}

impl fmt::Display for SourceLocation {
    /// 渲染为 `文件 条目`，来源路径存在时追加 ` (路径:行号)` 或 ` (路径)`。
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} {}", self.file.display(), self.entry)?;
        if let Some(catalog) = &self.catalog {
            write!(f, " ({}", catalog.display())?;
            if let Some(line) = self.line {
                write!(f, ":{line}")?;
            }
            write!(f, ")")?;
        }
        Ok(())
    }
}

/// 合并主计划文件及其引用文件后的完整运行配置。
#[derive(Clone, Debug)]
pub struct Config {
    /// 主计划文件路径，用于错误前缀与相对路径解析基准。
    pub plan: PathBuf,
    /// 命名测试根目录，键为 root 名（如 `"musl"`、`"glibc"`）。
    pub roots: BTreeMap<String, RootSpec>,
    /// 全局默认 timeout/repeat，作为 step 未显式指定时的取值。
    pub defaults: Defaults,
    /// 全部用例结束后的收尾动作。
    pub finish: Finish,
    /// 按声明顺序排列的测试分组。
    pub groups: Vec<Group>,
    /// 按声明顺序生效的用例覆盖规则。
    pub overrides: Vec<CaseOverride>,
}

/// 单个测试根目录的规格。
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RootSpec {
    /// 根目录在宿主文件系统中的绝对路径。
    pub path: PathBuf,
    /// 根内用例的默认工作目录（chroot 后解释，默认为 `"/"`）。
    #[serde(default = "root_cwd")]
    pub cwd: PathBuf,
    /// 根内用例默认注入的环境变量。
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// 按声明顺序挂载的项目：`dev`、`shm`（`/dev/shm`）、`proc`、`tmp`；省略时不挂载。
    #[serde(default)]
    pub mounts: Vec<String>,
    /// 激活该根时在系统根内执行的准备命令。
    #[serde(default)]
    pub prepare: Vec<Vec<String>>,
}

/// 提供 `RootSpec::cwd` 的默认值 `"/"`。
fn root_cwd() -> PathBuf {
    PathBuf::from("/")
}

/// 所有 step 未显式指定时使用的默认执行参数。
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Defaults {
    /// 单次运行超时秒数。
    pub timeout_sec: u64,
    /// 单轮重复执行次数。
    pub repeat: u32,
}

impl Default for Defaults {
    /// 超时 60 秒、重复 1 次。
    fn default() -> Self {
        Self {
            timeout_sec: 60,
            repeat: 1,
        }
    }
}

/// 全部用例结束后的收尾动作。
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Finish {
    /// `sync` 后关闭系统（默认，无人值守评测用）。
    #[default]
    Poweroff,
    /// `sync` 后返回调用方。
    Return,
}

/// 一个完整输出分组：同一组名与 root 的全部 step 归入同一组。
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Group {
    /// 组名，参与默认 ID 与组首尾文本的 `{group}` 展开。
    pub name: String,
    /// 该组在各测试根目录下依次展开。
    pub roots: Vec<String>,
    /// 组内用例的默认工作目录。
    pub cwd: Option<PathBuf>,
    /// 组内用例默认叠加的环境变量。
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// 组开始时输出的文本（`{group}`、`{root}` 已展开）。
    #[serde(default)]
    pub start: String,
    /// 组结束时输出的文本。
    #[serde(default)]
    pub end: String,
    /// 组内步骤，按声明顺序展开。
    #[serde(rename = "step")]
    pub steps: Vec<Step>,
    /// 在 TOML 文件中的定义位置，由配置加载器填充。
    #[serde(skip)]
    pub location: Option<SourceLocation>,
}

/// 一个步骤：选择一组名字并给出唯一的执行模板，展开为同组内多个用例。
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    /// 是否启用，`false` 时仍参与静态校验但不产生用例。
    #[serde(default = "enabled")]
    pub enabled: bool,
    /// 适用架构列表，空表示不限制。
    #[serde(default)]
    pub arch: Vec<String>,
    /// 显式名字列表，名字即数据，不要求存在同名文件。
    pub names: Option<Vec<String>>,
    /// 扫描该目录中的普通可执行文件，相对路径基于有效 cwd。
    pub directory: Option<PathBuf>,
    /// 读取该清单文件（每行首个词为名字，其余为命令原文），必须为绝对路径。
    pub catalog: Option<PathBuf>,
    /// 按顺序取匹配项的模式列表，省略等于 `["*"]`。
    pub include: Option<Vec<String>>,
    /// 最后剔除匹配这些模式的候选。
    #[serde(default)]
    pub exclude: Vec<String>,
    /// 用例 argv 模板，首项即解释器或程序。
    pub exec: Vec<String>,
    /// 用例 ID 模板，省略时为 `{group}:{root}:{name}`。
    pub id: Option<String>,
    /// 用例工作目录，覆盖组与根的默认值。
    pub cwd: Option<PathBuf>,
    /// 叠加在根与组环境变量之上的变量。
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// 单次运行超时秒数，省略时取 defaults。
    pub timeout_sec: Option<u64>,
    /// 重复执行次数，省略时取 defaults。
    pub repeat: Option<u32>,
    /// 每轮主命令前执行的准备命令。
    #[serde(default)]
    pub setup: Vec<Vec<String>>,
    /// 每轮主命令后执行的清理命令。
    #[serde(default)]
    pub cleanup: Vec<Vec<String>>,
    /// 主命令运行前输出的文本模板。
    #[serde(default)]
    pub before: String,
    /// 主命令结束后输出的文本模板，保留原始模板待执行结束时展开。
    #[serde(default)]
    pub after: String,
    /// 在 TOML 文件中的定义位置，由配置加载器填充。
    #[serde(skip)]
    pub location: Option<SourceLocation>,
}

/// 提供 `Step::enabled` 的默认值 `true`。
fn enabled() -> bool {
    true
}

/// 针对已选用例的精确覆盖规则；不能变更所属 root、group 或 ID。
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaseOverride {
    /// 目标用例 ID，精确匹配。
    pub id: String,
    /// 仅在这些架构上生效，空表示不限制。
    #[serde(default)]
    pub arch: Vec<String>,
    /// 整体替换的 argv 模板。
    pub exec: Option<Vec<String>>,
    /// 整体替换的工作目录。
    pub cwd: Option<PathBuf>,
    /// 按键合并进用例环境变量。
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// 整体替换的超时秒数。
    pub timeout_sec: Option<u64>,
    /// 整体替换的重复次数。
    pub repeat: Option<u32>,
    /// 整体替换的准备命令。
    pub setup: Option<Vec<Vec<String>>>,
    /// 整体替换的清理命令。
    pub cleanup: Option<Vec<Vec<String>>>,
    /// 整体替换的 before 模板。
    pub before: Option<String>,
    /// 整体替换的 after 模板。
    pub after: Option<String>,
    /// 在 TOML 文件中的定义位置，由配置加载器填充。
    #[serde(skip)]
    pub location: Option<SourceLocation>,
}

/// 展开完成、可直接交付执行的用例。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedCase {
    /// 全局唯一且可直接写入设备通知的 ID。
    pub id: String,
    /// 所属组名。
    pub group: String,
    /// 所属测试根名，对应 [`Config::roots`] 的键。
    pub root: String,
    /// 用例名：names 项、目录文件名或清单首词。
    pub name: String,
    /// 清单来源的 `{command}` 原文；其他来源为 `None`。
    pub catalog_command: Option<String>,
    /// 已展开的 exec 模板。
    pub argv: Vec<String>,
    /// 根内工作目录，chroot 后解释。
    pub cwd: PathBuf,
    /// 合并后的环境变量。
    pub env: BTreeMap<String, String>,
    /// 单次运行超时秒数。
    pub timeout_sec: u64,
    /// 重复执行次数。
    pub repeat: u32,
    /// 每轮主命令前执行的准备命令。
    pub setup: Vec<Vec<String>>,
    /// 每轮主命令后执行的清理命令。
    pub cleanup: Vec<Vec<String>>,
    /// 已展开的 before 文本。
    pub before: String,
    /// 原始 after 模板，执行结束时连同 `{status}` 一次展开。
    pub after: String,
    /// 用例定义来源。
    pub source: SourceLocation,
}

/// 同一组名在某个测试根下展开得到的用例集合。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedGroup {
    /// 组名。
    pub name: String,
    /// 测试根名。
    pub root: String,
    /// 已用 `{group}`、`{root}` 展开的组开始文本。
    pub start: String,
    /// 已展开的组结束文本。
    pub end: String,
    /// 按 root-major 顺序展开的全部用例。
    pub cases: Vec<ResolvedCase>,
}
