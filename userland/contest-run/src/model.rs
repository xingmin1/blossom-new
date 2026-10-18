//! 计划输入、选择来源与已经展开的具体命令。

use serde::Deserialize;
use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SourceLocation {
    pub file: PathBuf,
    /// step 或 override 数组内从一开始的序号。
    pub entry: usize,
    pub catalog: Option<PathBuf>,
    pub line: Option<usize>,
}

impl fmt::Display for SourceLocation {
    /// 显示计划条目及可选的原清单行号。
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:entry {}", self.file.display(), self.entry)?;
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

#[derive(Clone, Debug)]
pub struct Config {
    pub roots: BTreeMap<String, RootSpec>,
    pub defaults: Defaults,
    pub steps: Vec<Step>,
    pub overrides: Vec<CaseOverride>,
    pub finish: Finish,
    pub plan: PathBuf,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct RootSpec {
    pub path: PathBuf,
    #[serde(default = "root_cwd")]
    pub cwd: PathBuf,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub mounts: Vec<String>,
    /// 命令在父系统根执行，路径不得按 case root 解释。
    #[serde(default)]
    pub prepare: Vec<Vec<String>>,
}

/// 根内缺省工作目录不依赖 runner 的当前目录。
fn root_cwd() -> PathBuf {
    PathBuf::from("/")
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct Defaults {
    pub timeout_sec: u64,
    pub repeat: u32,
    pub hooks: bool,
    pub env: BTreeMap<String, String>,
    pub setup: Vec<Vec<String>>,
    pub cleanup: Vec<Vec<String>>,
}

impl Default for Defaults {
    /// 缺省只提供运行参数，不生成任何可选择项。
    fn default() -> Self {
        Self {
            timeout_sec: 60,
            repeat: 1,
            hooks: false,
            env: BTreeMap::new(),
            setup: Vec::new(),
            cleanup: Vec::new(),
        }
    }
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Finish {
    #[default]
    Poweroff,
    Return,
}

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum StepKind {
    Ltp,
    Basic,
    Libctest,
    Exec,
    Shell,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum CatalogSource {
    #[default]
    Runtest,
    Executables,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Step {
    pub kind: StepKind,
    #[serde(default = "enabled")]
    pub enabled: bool,
    #[serde(default)]
    pub arch: Vec<String>,
    pub id: Option<String>,
    pub root: Option<String>,
    #[serde(default)]
    pub roots: Vec<String>,
    pub suite: Option<String>,
    pub source: Option<CatalogSource>,
    pub catalog: Option<PathBuf>,
    pub directory: Option<PathBuf>,
    pub variant: Option<String>,
    #[serde(default)]
    pub include: Vec<String>,
    #[serde(default)]
    pub exclude: Vec<String>,
    pub cwd: Option<PathBuf>,
    pub program: Option<String>,
    pub args: Option<Vec<String>>,
    pub shell: Option<String>,
    pub command: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub timeout_sec: Option<u64>,
    pub repeat: Option<u32>,
    pub hooks: Option<bool>,
    pub setup: Option<Vec<Vec<String>>>,
    pub cleanup: Option<Vec<Vec<String>>>,
    #[serde(skip)]
    pub location: Option<SourceLocation>,
}

/// 条目缺省启用；关闭条目仍接受同一份结构校验。
fn enabled() -> bool {
    true
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CaseOverride {
    pub id: String,
    pub root: Option<String>,
    pub cwd: Option<PathBuf>,
    pub program: Option<String>,
    pub args: Option<Vec<String>>,
    pub shell: Option<String>,
    pub command: Option<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub timeout_sec: Option<u64>,
    pub repeat: Option<u32>,
    pub hooks: Option<bool>,
    pub setup: Option<Vec<Vec<String>>>,
    pub cleanup: Option<Vec<Vec<String>>>,
    #[serde(skip)]
    pub location: Option<SourceLocation>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommandSpec {
    Exec { program: String, args: Vec<String> },
    Shell { shell: String, command: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Protocol {
    Plain,
    Ltp,
    Tap,
    Basic { name: String },
    Pre2025Libctest { variant: String, selector: String },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResolvedCase {
    pub id: String,
    pub source: SourceLocation,
    pub root: String,
    pub cwd: PathBuf,
    pub command: CommandSpec,
    pub env: BTreeMap<String, String>,
    pub timeout_sec: u64,
    /// 每轮沿用相同 id，轮次由执行结果单独记录。
    pub repeat: u32,
    pub hooks: bool,
    pub setup: Vec<Vec<String>>,
    pub cleanup: Vec<Vec<String>>,
    pub protocol: Protocol,
}
