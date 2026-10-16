//! 原始退出事实与固定比赛输出协议的流式判定。

use crate::execute::ExitOutcome;
use crate::model::Protocol;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StreamKind {
    Stdout,
    Stderr,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    Pass,
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

pub struct ProtocolObserver<'a> {
    protocol: &'a Protocol,
    start: String,
    end: String,
    failure: String,
    streams: [LineState; 2],
}

impl<'a> ProtocolObserver<'a> {
    /// 为一个具体选择项建立双流状态；缓冲上界只取决于所选标记长度。
    pub fn new(protocol: &'a Protocol) -> Self {
        let (start, end, failure) = match protocol {
            Protocol::Basic { name } => {
                let marker = if name == "mkdir_" { "mkdir" } else { name };
                (
                    format!("========== START test_{marker} =========="),
                    format!("========== END test_{marker} =========="),
                    String::new(),
                )
            }
            Protocol::Pre2025Libctest { variant, selector } => (
                format!("========== START entry-{variant}.exe {selector} =========="),
                format!("========== END entry-{variant}.exe {selector} =========="),
                format!("FAIL {selector} "),
            ),
            Protocol::Plain | Protocol::Ltp => (String::new(), String::new(), String::new()),
        };
        let limit = if start.is_empty() {
            0
        } else {
            128.max(start.len() + 1)
                .max(end.len() + 1)
                .max(failure.len() + 1)
        };
        Self {
            protocol,
            start,
            end,
            failure,
            streams: [LineState::new(limit), LineState::new(limit)],
        }
    }

    /// 观察原始字节而不转发；两个流独立分行，长行仅保留有界前缀。
    pub fn feed(&mut self, stream: StreamKind, bytes: &[u8]) {
        if matches!(self.protocol, Protocol::Plain | Protocol::Ltp) {
            return;
        }
        let state = &mut self.streams[match stream {
            StreamKind::Stdout => 0,
            StreamKind::Stderr => 1,
        }];
        for &byte in bytes {
            if byte == b'\n' {
                state.consume(self.protocol, &self.start, &self.end, &self.failure);
            } else if state.line.len() < state.limit {
                state.line.push(byte);
            } else {
                state.overflow = true;
            }
        }
    }

    /// 消费各流无换行的末行，再按基础设施边界及原协议解释真实退出。
    pub fn finish(mut self, outcome: &ExitOutcome) -> Verdict {
        for state in &mut self.streams {
            if !state.line.is_empty() || state.overflow {
                state.consume(self.protocol, &self.start, &self.end, &self.failure);
            }
        }
        let code = match outcome {
            ExitOutcome::Exited { code, .. } => *code,
            ExitOutcome::Signaled { .. } => return Verdict::Signaled,
            ExitOutcome::TimedOut { .. } => return Verdict::TimedOut,
            ExitOutcome::SpawnError { .. } | ExitOutcome::IoError { .. } => {
                return Verdict::InfrastructureError
            }
        };
        match self.protocol {
            Protocol::Plain => {
                if code == 0 {
                    Verdict::Pass
                } else {
                    Verdict::Failed
                }
            }
            Protocol::Ltp => ltp_verdict(code),
            Protocol::Basic { .. } => self.marked_verdict(code, 0, false),
            Protocol::Pre2025Libctest { .. } => self.marked_verdict(code, 1, true),
        }
    }

    /// 完成证据必须来自同一流的同一个选择区间，失败证据永久优先。
    fn marked_verdict(&self, code: i32, expected: i32, needs_pass: bool) -> Verdict {
        if code != expected || self.streams.iter().any(|state| state.failed) {
            return Verdict::Failed;
        }
        if !self.streams.iter().any(|state| state.started) {
            return Verdict::NotRun;
        }
        if self
            .streams
            .iter()
            .any(|state| state.ended && (!needs_pass || state.passed))
        {
            if needs_pass {
                Verdict::Pass
            } else {
                Verdict::Completed
            }
        } else {
            Verdict::Failed
        }
    }
}

struct LineState {
    line: Vec<u8>,
    limit: usize,
    overflow: bool,
    started: bool,
    active: bool,
    foreign: bool,
    ended: bool,
    passed: bool,
    failed: bool,
}

impl LineState {
    /// 仅分配一次有界行前缀；后续输出不增加保存的字节数。
    fn new(limit: usize) -> Self {
        Self {
            line: Vec::with_capacity(limit),
            limit,
            overflow: false,
            started: false,
            active: false,
            foreign: false,
            ended: false,
            passed: false,
            failed: false,
        }
    }

    /// 完整行才能形成成功标记，超长 FAIL 行仍按其有界前缀保留失败。
    fn consume(&mut self, protocol: &Protocol, start: &str, end: &str, failure: &str) {
        let mut line = self.line.as_slice();
        if !self.overflow {
            line = line.strip_suffix(b"\r").unwrap_or(line);
        }
        if matches!(protocol, Protocol::Pre2025Libctest { .. })
            && line.starts_with(failure.as_bytes())
        {
            self.failed = true;
        }
        if let Protocol::Basic { name } = protocol {
            if !self.foreign && basic_error(name, line, self.overflow) {
                self.failed = true;
            }
        }
        if !self.overflow {
            if line == start.as_bytes() {
                if self.started {
                    self.failed = true;
                }
                self.started = true;
                self.active = true;
                self.foreign = false;
                self.ended = false;
                self.passed = false;
            } else if line == end.as_bytes()
                || (matches!(protocol, Protocol::Basic { name } if name == "execve")
                    && line == b"========== END main ==========")
            {
                if self.active {
                    self.ended = true;
                }
                self.active = false;
            } else if line.starts_with(b"========== START ") || line.starts_with(b"========== END ")
            {
                if self.active {
                    self.failed = true;
                }
                self.active = false;
                self.foreign = true;
            } else if self.active && line == b"Pass!" {
                self.passed = true;
            }
        }
        self.line.clear();
        self.overflow = false;
    }
}

/// 只识别固定 basic 源码确实输出的错误，不将任意 FAIL 文本当成判题结果。
fn basic_error(name: &str, line: &[u8], overflow: bool) -> bool {
    let literal = match name {
        "clone" => "clone process error.",
        "close" => "  close error.",
        "execve" => "  execve error.",
        "exit" => "exit ERR.",
        "getcwd" => "getcwd ERROR.",
        "getppid" => "  getppid error.",
        "gettimeofday" => "gettimeofday error.",
        "mkdir_" => "  mkdir error.",
        "mmap" | "munmap" => "mmap error.",
        "sleep" => "sleep error.",
        "unlink" => "  unlink error!",
        "wait" => "wait child error.",
        "waitpid" => "waitpid error.",
        _ => "",
    };
    if !overflow
        && ((!literal.is_empty() && line == literal.as_bytes())
            || line == b" --- Assert Fatal ! ---")
    {
        return true;
    }
    let fields: &[&[u8]] = match name {
        "mount" => &[b"mount return: ", b"umount return: "],
        "umount" => &[b"mount return: ", b"return: "],
        "munmap" => &[b"munmap return: "],
        _ => &[],
    };
    fields.iter().any(|prefix| {
        line.strip_prefix(*prefix)
            .is_some_and(|value| overflow || value != b"0")
    })
}

/// 按固定结果位判定，原 code/raw 仍由执行记录保存；信息位不代表通过。
fn ltp_verdict(code: i32) -> Verdict {
    const TFAIL: i32 = 1;
    const TBROK: i32 = 2;
    const TWARN: i32 = 4;
    const TCONF: i32 = 32;
    if code < 0 || (code & !(TFAIL | TBROK | TWARN | TCONF)) != 0 || (code & TBROK) != 0 {
        Verdict::Broken
    } else if (code & TFAIL) != 0 {
        Verdict::Failed
    } else if (code & TWARN) != 0 {
        Verdict::Warning
    } else if (code & TCONF) != 0 {
        Verdict::Configuration
    } else {
        Verdict::Pass
    }
}
