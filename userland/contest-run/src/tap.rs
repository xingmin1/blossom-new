//! 固定 kselftest 输出的有界流式观察，仅消费 stdout。

use crate::outcome::{ExitOutcome, ProtocolResult, TapSummary, Verdict};

pub(crate) struct TapObserver {
    summary: TapSummary,
    line: Vec<u8>,
    overflow: bool,
    version: bool,
    count: u64,
    terminal_skip: bool,
    zero_skip: bool,
    trailing_plan: bool,
}

impl TapObserver {
    /// 每个程序只分配一次行前缀，计数不随输出正文增长。
    pub(crate) fn new() -> Self {
        Self {
            summary: TapSummary::default(),
            line: Vec::with_capacity(4096),
            overflow: false,
            version: false,
            count: 0,
            terminal_skip: false,
            zero_skip: false,
            trailing_plan: false,
        }
    }

    /// 保存首个格式错误，后续成功结果不能覆盖损坏事实。
    fn invalid(&mut self, message: &str) {
        if self.summary.error.is_none() {
            self.summary.error = Some(message.into());
        }
    }

    /// 任意读取块均按独立 stdout 行推进，长诊断只占固定前缀。
    pub(crate) fn feed(&mut self, bytes: &[u8]) {
        for &byte in bytes {
            if byte == b'\n' {
                self.consume();
            } else if self.line.len() < 4096 {
                self.line.push(byte);
            } else {
                self.overflow = true;
            }
        }
    }

    /// 暂时取走预分配行，解析后归还同一缓冲，避免逐行复制或分配。
    fn consume(&mut self) {
        let mut buffer = std::mem::take(&mut self.line);
        let line = buffer.strip_suffix(b"\r").unwrap_or(&buffer);
        self.consume_line(line);
        buffer.clear();
        self.line = buffer;
        self.overflow = false;
    }

    /// 只解释固定头的版本、计划、结果与 Bail out；普通业务行保留给 relay。
    fn consume_line(&mut self, line: &[u8]) {
        if line.starts_with(b"Bail out!") {
            self.summary.bailed_out = true;
            return;
        }
        let Ok(line) = std::str::from_utf8(line) else {
            if line.starts_with(b"ok ")
                || line.starts_with(b"not ok ")
                || line.starts_with(b"1..")
                || line.starts_with(b"TAP version")
            {
                self.invalid("non-UTF-8 TAP control line");
            }
            return;
        };
        if line.starts_with("TAP version") {
            if self.version
                || self.summary.plan.is_some()
                || self.count != 0
                || line != "TAP version 13"
                || self.overflow
            {
                self.invalid("invalid or repeated TAP version");
            }
            self.version = true;
            return;
        }
        if let Some(plan) = line.strip_prefix("1..") {
            let (number, comment) = plan.split_once('#').unwrap_or((plan, ""));
            let count = number.trim().parse::<u64>();
            match count {
                Ok(count) if self.summary.plan.is_none() && !self.overflow => {
                    self.summary.plan = Some(count);
                    self.zero_skip = count == 0 && directive(comment, "SKIP");
                    self.trailing_plan = self.count != 0;
                    if count == 0 && (!self.zero_skip || self.count != 0) {
                        self.invalid("zero plan requires an empty SKIP");
                    }
                }
                _ => self.invalid("invalid or repeated TAP plan"),
            }
            return;
        }
        let (ok, result) = if let Some(result) = line.strip_prefix("not ok ") {
            (false, result)
        } else if let Some(result) = line.strip_prefix("ok ") {
            (true, result)
        } else {
            return;
        };
        if self.overflow || self.terminal_skip || self.trailing_plan || self.zero_skip {
            self.invalid("truncated or late TAP result");
            return;
        }
        let (number, rest) = result
            .split_once(char::is_whitespace)
            .unwrap_or((result, ""));
        let Ok(number) = number.parse::<u64>() else {
            self.invalid("TAP result requires a number");
            return;
        };
        let comment = rest.split_once('#').map_or("", |(_, comment)| comment);
        let skip = ok && directive(comment, "SKIP");
        // ksft_exit_skip 先增加 skip 计数，再打印 1 + test_num；仅出口4允许这个终末缺号。
        if skip && number == self.count + 2 {
            self.terminal_skip = true;
        } else if number != self.count + 1 {
            self.invalid("duplicate or out-of-order TAP result");
        }
        self.count += 1;
        if !ok {
            self.summary.failed += 1;
        } else if skip {
            self.summary.skipped += 1;
        } else if directive(comment, "XFAIL") {
            self.summary.xfailed += 1;
        } else if directive(comment, "XPASS") {
            self.summary.xpassed += 1;
        } else {
            self.summary.passed += 1;
        }
    }

    /// 原始信号、超时和失败子项优先；只有真实跳过出口允许特殊终末缺号。
    pub(crate) fn finish(mut self, outcome: &ExitOutcome) -> ProtocolResult {
        if !self.line.is_empty() || self.overflow {
            self.consume();
        }
        let verdict = match outcome {
            ExitOutcome::Signaled { .. } => Verdict::Signaled,
            ExitOutcome::TimedOut { .. } => Verdict::TimedOut,
            ExitOutcome::SpawnError { .. } | ExitOutcome::IoError { .. } => {
                Verdict::InfrastructureError
            }
            ExitOutcome::Exited { code, .. } => self.exited(*code),
        };
        ProtocolResult {
            verdict,
            tap: Some(self.summary),
        }
    }

    /// 汇总一次已消费的计数，不从原始输出重建第二份结果。
    fn exited(&mut self, code: i32) -> Verdict {
        if self.summary.bailed_out || self.summary.failed != 0 || self.summary.xpassed != 0 {
            return Verdict::Failed;
        }
        if self.terminal_skip && code != 4 {
            self.invalid("terminal skip numbering requires exit 4");
        }
        let complete = self.summary.plan == Some(self.count);
        if !self.zero_skip && !complete && !(code == 4 && self.terminal_skip) {
            self.invalid("missing TAP plan or incomplete results");
        }
        if self.summary.error.is_some() {
            return Verdict::Broken;
        }
        match code {
            0 if self.zero_skip || (self.count != 0 && self.count == self.summary.skipped) => {
                Verdict::Skip
            }
            0 => Verdict::Pass,
            4 if self.zero_skip || self.summary.skipped != 0 => Verdict::Skip,
            _ => Verdict::Failed,
        }
    }
}

/// 指令按完整首词匹配，后续说明文字不参与判定。
fn directive(comment: &str, expected: &str) -> bool {
    comment
        .split_whitespace()
        .next()
        .is_some_and(|word| word.eq_ignore_ascii_case(expected))
}
