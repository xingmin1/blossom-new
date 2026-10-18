//! 进程退出与协议结果的共享事实，不依赖执行或报告模块。

use std::time::Duration;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ExitOutcome {
    Exited {
        code: i32,
        raw: i32,
    },
    Signaled {
        signal: i32,
        raw: i32,
    },
    SpawnError {
        errno: Option<i32>,
        message: String,
    },
    TimedOut {
        raw: Option<i32>,
        signal: Option<i32>,
    },
    IoError {
        errno: Option<i32>,
        message: String,
        raw: Option<i32>,
    },
}

#[derive(Debug)]
pub struct Execution {
    pub outcome: ExitOutcome,
    /// 输出与停止错误独立保存，不覆盖已经取得的 wait 状态。
    pub io_errors: Vec<String>,
    pub elapsed: Duration,
    pub protocol: ProtocolResult,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Pass,
    Skip,
    Completed,
    Failed,
    Broken,
    Warning,
    Configuration,
    NotRun,
    TimedOut,
    Signaled,
    InfrastructureError,
}

impl Verdict {
    /// 返回报告中稳定的状态名，不将配置缺失改签为支持性跳过。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pass => "Pass",
            Self::Skip => "Skip",
            Self::Completed => "Completed",
            Self::Failed => "Failed",
            Self::Broken => "Broken",
            Self::Warning => "Warning",
            Self::Configuration => "Configuration",
            Self::NotRun => "NotRun",
            Self::TimedOut => "TimedOut",
            Self::Signaled => "Signaled",
            Self::InfrastructureError => "InfrastructureError",
        }
    }

    /// 只有真实通过与完整执行可使本轮继续按成功处理。
    pub fn is_success(self) -> bool {
        matches!(self, Self::Pass | Self::Completed)
    }
}

/// 一个程序只保存一份最终判定，TAP 计数来自同一次流式观察。
#[derive(Debug, PartialEq, Eq)]
pub struct ProtocolResult {
    pub verdict: Verdict,
    pub tap: Option<TapSummary>,
}

/// 仅保存子例计数与首个格式错误，不保存测试正文或逐例数据库。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct TapSummary {
    pub plan: Option<u64>,
    pub passed: u64,
    pub failed: u64,
    pub skipped: u64,
    pub xfailed: u64,
    pub xpassed: u64,
    pub bailed_out: bool,
    pub error: Option<String>,
}
