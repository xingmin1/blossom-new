//! 所有执行阶段共用的命令配置与独立的目录、清单来源。
//!
//! argv 简写始终表示 `root="test"`；完整对象必须显式选择 `system` 或 `test`。
//! system 是客体内 runner 自己的根，不是 QEMU 宿主机。argv 始终是字符串，
//! `{test_root}` 只做单遍替换，不推断参数路径或重新拆词。进程控制由 `process` 负责。

use crate::expand::expand;
use crate::outcome::Execution;
use crate::process::run_command;
use serde::Deserialize;
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// 命令执行或来源读取所使用的根视图。
#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Root {
    /// runner 自己的系统根，不执行 chroot。
    System,
    /// 当前分组选择的测试根。
    Test,
}

/// 各阶段统一使用的命令，数组简写固定采用测试根。
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum CommandSpec {
    /// 仅指定 argv，继承测试根上下文。
    Argv(Vec<String>),
    /// 显式选择根并允许覆盖命令局部运行参数。
    Full(Command),
}

/// 一条显式选择执行根的命令。
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Command {
    /// 执行根，完整对象不得省略。
    pub root: Root,
    /// 程序与参数，首项为程序，不进行 shell 拆词。
    pub argv: Vec<String>,
    /// 命令执行根内的绝对工作目录，省略时使用该根的默认上下文。
    pub cwd: Option<PathBuf>,
    /// 叠加到该命令根环境中的变量；system 不继承测试环境。
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// 单条命令超时秒数，省略时使用调用阶段的默认期限。
    pub timeout_sec: Option<u64>,
}

/// 借用已展开配置的一次执行视图，只有合并环境变量时才需要复制环境表。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Invocation<'a> {
    /// 子进程的 chroot 目标，system 命令为 None。
    pub root: Option<&'a Path>,
    /// 子进程执行根内的工作目录。
    pub cwd: &'a Path,
    /// 已单次展开的程序与参数。
    pub argv: &'a [String],
    /// 清空父进程环境后注入的完整环境表。
    pub env: Cow<'a, BTreeMap<String, String>>,
    /// 交给既有进程执行器的单条期限。
    pub timeout: Duration,
}

impl CommandSpec {
    /// 取得命令的原始或已展开 argv，不解释其中的路径。
    pub fn argv(&self) -> &[String] {
        match self {
            Self::Argv(argv) => argv,
            Self::Full(command) => &command.argv,
        }
    }

    /// 取得执行根，数组简写始终返回测试根。
    pub fn root(&self) -> Root {
        match self {
            Self::Argv(_) => Root::Test,
            Self::Full(command) => command.root,
        }
    }

    /// 单遍展开 argv、命令局部 cwd 与环境值，插入的文本不再解释。
    pub fn expanded(&self, fields: &[(&str, &str)]) -> Self {
        let argv = self.argv().iter().map(|arg| expand(arg, fields)).collect();
        match self {
            Self::Argv(_) => Self::Argv(argv),
            Self::Full(command) => Self::Full(Command {
                root: command.root,
                argv,
                cwd: command
                    .cwd
                    .as_ref()
                    .map(|path| PathBuf::from(expand(&path.to_string_lossy(), fields))),
                env: command
                    .env
                    .iter()
                    .map(|(key, value)| (key.clone(), expand(value, fields)))
                    .collect(),
                timeout_sec: command.timeout_sec,
            }),
        }
    }

    /// 根据执行根合并阶段默认值与命令局部参数，不改变父进程的根、cwd 或环境。
    ///
    /// system 默认 cwd 为 `/`、环境为空；test 继承传入上下文。命令局部超时优先于阶段默认值，
    /// CLI 对主命令的覆盖应在调用前写入对应命令，不能修改准备或清理命令。
    pub fn resolve<'a>(
        &'a self,
        test_root: Option<&'a Path>,
        test_cwd: &'a Path,
        test_env: &'a BTreeMap<String, String>,
        timeout: Duration,
    ) -> Invocation<'a> {
        let (root, mut cwd, mut env) = match self.root() {
            Root::System => (None, Path::new("/"), Cow::Owned(BTreeMap::new())),
            Root::Test => (test_root, test_cwd, Cow::Borrowed(test_env)),
        };
        let mut timeout = timeout;
        if let Self::Full(command) = self {
            if let Some(path) = &command.cwd {
                cwd = path;
            }
            if command.root == Root::System {
                env = Cow::Borrowed(&command.env);
            } else if !command.env.is_empty() {
                env.to_mut().extend(command.env.clone());
            }
            if let Some(seconds) = command.timeout_sec {
                timeout = Duration::from_secs(seconds);
            }
        }
        Invocation {
            root,
            cwd,
            argv: self.argv(),
            env,
            timeout,
        }
    }
}

impl Invocation<'_> {
    /// 通过既有进程组与绝对期限执行器同步运行命令，不捕获或判定比赛输出。
    pub fn run(&self) -> Execution {
        run_command(self.root, self.cwd, self.argv, &self.env, self.timeout)
    }
}

/// runner 读取的目录或清单路径，与主命令的执行根相互独立。
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(untagged)]
pub enum SourcePath {
    /// 测试根视图内的路径，目录相对路径以有效 cwd 为基准。
    Test(PathBuf),
    /// 显式指定读取根的路径。
    Scoped(ScopedPath),
}

/// 一项显式选择读取根的来源路径。
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct ScopedPath {
    /// runner 读取此来源时使用的根视图。
    pub root: Root,
    /// 该根视图中的路径，不作为 argv 解析。
    pub path: PathBuf,
}

impl SourcePath {
    /// 取得来源根与原始路径，保留配置路径用于错误和来源展示。
    pub fn parts(&self) -> (Root, &Path) {
        match self {
            Self::Test(path) => (Root::Test, path),
            Self::Scoped(path) => (path.root, &path.path),
        }
    }
}
