//! 子进程进程事实与 TAP 自测判定结果的公共数据类型模块。
//!
//! ## 模块职责与架构定位
//! 本模块定义 `process`、`execute` 与 `selftests` 之间共享的结果数据模型：
//! [`ExitOutcome`] 与 [`Execution`] 记录一次命令运行的进程事实（退出、信号、启动失败、超时、等待或管道错误）与耗时；
//! [`ProtocolResult`] 与 [`TapSummary`] 只承载 `user-selftests` 的 TAP 结论，比赛路径不产生协议判定。
//!
//! ## 数据流
//!
//! ```text
//!   操作系统进程状态
//!       │
//!       ├─► ExitOutcome (Exited / Signaled / SpawnError / TimedOut / IoError)
//!       ├─► io_errors: Vec<String>（管道转发、进程组信号、辅助进程回收等附加错误）
//!       └─► TapObserver（仅自测路径）
//!               │
//!               ▼
//!         ProtocolResult { verdict: Verdict, tap: Option<TapSummary> }
//! ```
//!
//! ## 并发与锁契约
//! 本模块仅包含纯数据结构体与枚举，无锁或跨线程共享状态。

use std::time::Duration;

/// 子进程生命周期结束时的底层退出分类结果。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExitOutcome {
    /// 子进程正常调用 `exit` / `_exit` 退出。
    Exited {
        /// 提取出的低 8 位进程退出码（`0..=255`）。
        code: i32,
        /// `waitpid` 返回的原始 32 位状态字（`wstatus`）。
        raw: i32,
    },
    /// 子进程在未超时的情况下被未捕获的信号终止。
    Signaled {
        /// 导致进程终止的信号编号（如 `SIGSEGV = 11`）。
        signal: i32,
        /// `waitpid` 返回的原始 32 位状态字（`wstatus`）。
        raw: i32,
    },
    /// 子进程在 `fork`/`exec` 启动阶段（含 `pre_exec` 中的 `chroot`/`chdir`/`setsid`）失败。
    SpawnError {
        /// 操作系统返回的错误码（`errno`）。
        errno: Option<i32>,
        /// 可读的错误描述信息。
        message: String,
    },
    /// 子进程运行或输出排空超过单次用例期限（`deadline`），由执行器发送信号终止。
    TimedOut {
        /// 回收子进程时取得的原始 `waitpid` 状态字（若 `wait` 失败则为 `None`）。
        raw: Option<i32>,
        /// 若子进程最终因信号退出，记录该信号编号（通常为 `SIGTERM` 或 `SIGKILL`）。
        signal: Option<i32>,
    },
    /// 子进程启动后在管道设置、输出转发建立或进程组收尾过程中发生系统 I/O 错误。
    IoError {
        /// 操作系统返回的错误码（`errno`）。
        errno: Option<i32>,
        /// 可读的错误描述信息。
        message: String,
        /// 回收子进程时取得的原始 `waitpid` 状态字（若尚未取得则为 `None`）。
        raw: Option<i32>,
    },
}

impl ExitOutcome {
    /// 返回 shell 风格的状态码，供 `after` 模板的 `{status}` 与单命令模式透传使用。
    ///
    /// 正常退出取原退出码；信号终止为 `128 + signal`；执行器超时为 124；启动失败按 `errno` 区分
    /// `ENOENT` = 127、`EACCES`/`ENOEXEC` = 126、其余 = 125；等待或管道收尾失败为 125。
    /// 后几项是执行器约定，不代表原程序自己返回过这些码。
    pub fn shell_status(&self) -> i32 {
        match self {
            Self::Exited { code, .. } => *code,
            Self::Signaled { signal, .. } => 128 + *signal,
            Self::TimedOut { .. } => 124,
            Self::SpawnError { errno, .. } => match *errno {
                Some(libc::ENOENT) => 127,
                Some(libc::EACCES | libc::ENOEXEC) => 126,
                _ => 125,
            },
            Self::IoError { .. } => 125,
        }
    }
}

/// 单次命令运行（主命令或准备命令）的完整进程记录。
#[derive(Debug)]
pub struct Execution {
    /// 子进程底层的退出分类状态。
    pub outcome: ExitOutcome,
    /// 运行期间在管道读取/转发、辅助进程回收或发送进程组信号时收集到的附加 I/O 错误列表。
    pub io_errors: Vec<String>,
    /// 命令从启动到收尾（含组内残留清理与组首回收）所耗费的单调时钟时长。
    pub elapsed: Duration,
}

/// `user-selftests` 单次 TAP 运行的判定结论；比赛路径不使用。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// 退出码与 TAP 计划全部满足，没有失败或意外通过的子项。
    Pass,
    /// 测试被主动跳过（如 TAP 协议中全部子项均带 `# SKIP` 指令）。
    Skip,
    /// 子项失败、`Bail out!`、意外通过或退出码不符合协议要求。
    Failed,
    /// TAP 协议格式损坏或计划数不完整，结果无法据此判定。
    Broken,
    /// 测试运行超时并被强制终止。
    TimedOut,
    /// 测试进程因未捕获信号崩溃或被杀死。
    Signaled,
    /// 执行器自身在进程启动、管道转发或回收环节发生基础设施错误。
    InfrastructureError,
}

impl Verdict {
    /// 返回自测结果行中使用的固定英文状态名称字符串。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "Pass",
            Self::Skip => "Skip",
            Self::Failed => "Failed",
            Self::Broken => "Broken",
            Self::TimedOut => "TimedOut",
            Self::Signaled => "Signaled",
            Self::InfrastructureError => "InfrastructureError",
        }
    }
}

/// TAP 观察器在命令结束时生成的最终解析结果。
#[derive(Debug, PartialEq, Eq)]
pub struct ProtocolResult {
    /// 综合进程退出状态与标准输出内容得出的高层判定结论。
    pub verdict: Verdict,
    /// 解析出的 TAP 子测试统计摘要。
    pub tap: Option<TapSummary>,
}

/// Test Anything Protocol (TAP) 协议单次运行的子测试统计摘要。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct TapSummary {
    /// `1..N` 计划行声明的预期子测试总数（若未出现计划行则为 `None`）。
    pub plan: Option<u64>,
    /// 状态为 `ok` 且无 `TODO`/`SKIP` 指令的通过子测试数量。
    pub passed: u64,
    /// 状态为 `not ok` 且无 `TODO`/`SKIP` 指令的失败子测试数量。
    pub failed: u64,
    /// 带有 `# SKIP` 指令的子测试数量（或 `1..0 # SKIP` 整集跳过）。
    pub skipped: u64,
    /// 状态为 `not ok` 且带有 `# TODO` 指令的预期失败子测试数量。
    pub xfailed: u64,
    /// 状态为 `ok` 但带有 `# TODO` 指令的意外通过子测试数量。
    pub xpassed: u64,
    /// 输出中是否出现了 `Bail out!` 紧急中止行。
    pub bailed_out: bool,
    /// 流式解析过程中遇到的首个 TAP 协议格式或计数不匹配错误描述。
    pub error: Option<String>,
}
