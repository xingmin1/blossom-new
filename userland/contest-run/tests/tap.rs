//! `kselftest` TAP（Test Anything Protocol）解析与程序级超时控制的集成测试。
//!
//! # 模块职责与定位
//! 本文件覆盖三条被测边界：
//! - 自测执行路径 `process::run_tap_command`：把真实 `kselftest.h` 程序或构造脚本的标准输出经管道交给 `tap::TapObserver`，
//!   验证进程退出状态、输出转发错误与 TAP 判定结论三者一致；
//! - 纯解析边界：直接向 `TapObserver` 喂入构造字节流，覆盖任意分块、无换行末行、超长行截断与异常退出状态优先级；
//! - 程序级超时：通过 `user-selftests` 入口验证总超时预算不会被持续输出重置。
//!
//! # 并发与锁契约
//! 涉及文件系统的测试使用带进程 ID 的独立临时目录；派生进程的路径共用互斥锁，避免 Relay 继承其他测试的管道写端。

use contest_run::outcome::{Execution, ExitOutcome, ProtocolResult, Verdict};
use contest_run::process::run_tap_command;
use contest_run::tap::TapObserver;
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::process::Command;
use std::sync::Mutex;
use std::time::Duration;

/// 串行化本文件中的进程派生，防止 Relay 继承其他测试仍打开的管道端点。
static EXECUTION: Mutex<()> = Mutex::new(());

/// 内联的 C 语言测试样本源码，直接引用仓库中的 `kselftest.h` 头文件以生成与真实内核自测完全一致的 TAP 输出与退出码。
const SAMPLE: &str = r#"
#include "kselftest.h"
#include <time.h>
int main(int argc, char **argv)
{
    if (argc != 1 && argc != 2) return 1;
    const char *mode = argc == 2 ? argv[1] : strrchr(argv[0], '/');
    if (argc == 1) mode = mode ? mode + 1 : argv[0];
    ksft_print_header();
    if (!strcmp(mode, "zero-skip")) ksft_exit_skip("no support\n");
    if (!strcmp(mode, "unplanned-skip")) {
        ksft_test_result_pass("first\n");
        ksft_exit_skip("remaining\n");
    }
    if (!strcmp(mode, "plan-skip") || !strcmp(mode, "partial-skip")) {
        ksft_set_plan(3);
        if (!strcmp(mode, "partial-skip")) ksft_test_result_pass("first\n");
        ksft_exit_skip("remaining\n");
    }
    ksft_set_plan(1);
    if (!strcmp(mode, "slow") || !strcmp(mode, "noisy")) {
        struct timespec tick = { .tv_nsec = 20000000 };
        for (int i = 0; i < 100; ++i) {
            if (!strcmp(mode, "noisy")) ksft_print_msg("progress %d\n", i);
            if (nanosleep(&tick, NULL)) ksft_exit_fail_msg("nanosleep failed\n");
        }
        ksft_test_result_pass("completed within program budget\n");
    } else if (!strcmp(mode, "pass")) ksft_test_result_pass("ordinary\n");
    else if (!strcmp(mode, "all-skip")) ksft_test_result_skip("no support\n");
    else if (!strcmp(mode, "fail-zero")) {
        ksft_test_result_fail("failed\n");
        ksft_exit_pass();
    } else if (!strcmp(mode, "fail-skip")) {
        ksft_test_result_fail("failed\n");
        ksft_exit_skip("remaining\n");
    } else if (!strcmp(mode, "xfail")) ksft_test_result_xfail("expected\n");
    else if (!strcmp(mode, "xfail-exit")) {
        ksft_test_result_xfail("expected\n");
        ksft_exit_xfail();
    } else if (!strcmp(mode, "xpass")) {
        ksft_test_result_code(KSFT_XPASS, "unexpected", NULL);
        ksft_exit_pass();
    } else ksft_exit_fail_msg("aborted\n");
    ksft_finished();
}
"#;

/// 按正常退出码 `code` 构造 `ExitOutcome::Exited`，并把 `wait` 原始状态字按 `code << 8` 编码。
fn exited(code: i32) -> ExitOutcome {
    ExitOutcome::Exited {
        code,
        raw: code << 8,
    }
}

/// 把 `bytes` 作为完整标准输出喂给 `TapObserver`，返回结合 `outcome` 的最终判定；只关心判定结论，不保留统计摘要。
fn observe(bytes: &[u8], outcome: &ExitOutcome) -> Verdict {
    let mut observer = TapObserver::new();
    observer.feed(bytes);
    observer.finish(outcome).verdict
}

/// 验证 `kselftest.h` 各类真实输出在自测执行路径上的进程状态与 TAP 判定，以及分块重放的一致性和程序级超时预算。
///
/// 覆盖的缺陷类：跳过、预期失败、意外通过与 `Bail out!` 被误判为通过或失败；退出码与判定不一致；
/// 流式行缓冲在任意块长下丢行；持续输出重置总超时计时器。
#[test]
fn fixed_header_outputs_keep_skip_and_failure_exit_semantics() {
    let _exclusive = EXECUTION.lock().unwrap_or_else(|error| error.into_inner());
    // Step 1: 创建临时目录并调用宿主 C 编译器编译内联的 kselftest 样本程序。
    let directory = std::env::temp_dir().join(format!("selftest-tap-{}", std::process::id()));
    fs::create_dir(&directory).unwrap();
    let scratch = Scratch(directory);
    let source = scratch.0.join("sample.c");
    let executable = scratch.0.join("sample");
    fs::write(&source, SAMPLE).unwrap();
    let include = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/user/include");
    let compiled = Command::new("cc")
        .arg("-std=gnu11")
        .arg("-I")
        .arg(include)
        .arg(&source)
        .arg("-o")
        .arg(&executable)
        .output()
        .unwrap();
    assert!(
        compiled.status.success(),
        "{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    // Step 2: 遍历 12 种典型 kselftest 运行模式，校验进程退出状态、输出转发无错误、TAP 判定以及任意分块大小下的流式解析一致性。
    for (mode, code, expected) in [
        ("pass", 0, Verdict::Pass),
        ("all-skip", 0, Verdict::Skip),
        ("zero-skip", 4, Verdict::Skip),
        ("plan-skip", 4, Verdict::Skip),
        ("partial-skip", 4, Verdict::Skip),
        ("unplanned-skip", 4, Verdict::Skip),
        ("fail-zero", 0, Verdict::Failed),
        ("fail-skip", 4, Verdict::Failed),
        ("xfail", 0, Verdict::Pass),
        ("xfail-exit", 2, Verdict::Failed),
        ("xpass", 0, Verdict::Failed),
        ("bail", 1, Verdict::Failed),
    ] {
        let stdout = File::create(scratch.0.join(format!("{mode}.stdout"))).unwrap();
        let stderr = File::create(scratch.0.join(format!("{mode}.stderr"))).unwrap();
        let argv = [executable.to_str().unwrap().to_owned(), mode.to_owned()];
        let (run, result) = run_tap_command(
            &scratch.0,
            &argv,
            &BTreeMap::new(),
            Duration::from_secs(3),
            [stdout.as_raw_fd(), stderr.as_raw_fd()],
        );
        assert_eq!(
            run.outcome,
            exited(code),
            "{mode}: unexpected process outcome"
        );
        assert!(
            run.io_errors.is_empty(),
            "{mode}: output forwarding failed: {:?}",
            run.io_errors
        );
        assert_eq!(result.verdict, expected, "{mode}: {result:?}");
        // 将捕获到的完整 stdout 按 1..=len 的所有块长切分重放，验证流式行缓冲无边界漏洞。
        let bytes = fs::read(scratch.0.join(format!("{mode}.stdout"))).unwrap();
        for size in 1..=bytes.len() {
            let mut observer = TapObserver::new();
            for chunk in bytes.chunks(size) {
                observer.feed(chunk);
            }
            assert_eq!(
                observer.finish(&run.outcome),
                result,
                "{mode}: chunk size {size} disagrees with streamed run"
            );
        }
    }
    // Step 3: 构造 timing 集合清单，通过 user-selftests 二进制验证慢速静默程序与持续输出程序均受总超时预算约束。
    let collection = scratch.0.join("timing");
    fs::create_dir(&collection).unwrap();
    for mode in ["slow", "noisy"] {
        fs::hard_link(&executable, collection.join(mode)).unwrap();
    }
    let manifest = scratch.0.join("kselftest-list.txt");
    fs::write(&manifest, "timing:slow\ntiming:noisy\n").unwrap();
    for (program, seconds, expected) in [
        ("timing:slow", "1", 1),
        ("timing:slow", "5", 0),
        ("timing:noisy", "1", 1),
    ] {
        let run = Command::new(env!("CARGO_BIN_EXE_user-selftests"))
            .args([
                "--manifest",
                manifest.to_str().unwrap(),
                "--program",
                program,
                "--program-timeout-seconds",
                seconds,
            ])
            .output()
            .unwrap();
        assert_eq!(
            run.status.code(),
            Some(expected),
            "{program} budget={seconds}: stdout={} stderr={}",
            String::from_utf8_lossy(&run.stdout),
            String::from_utf8_lossy(&run.stderr)
        );
    }
}

/// 测试专用的临时目录守卫（RAII Fixture）。
///
/// 在测试开始时绑定已创建的临时目录（元组字段 `0` 为目录路径），测试结束或 panic 展开时递归删除其中全部产物。
struct Scratch(PathBuf);

impl Drop for Scratch {
    /// 测试退出时递归删除临时目录及其全部测试产物。
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

/// 验证只有完整且合法的标准输出 TAP 序列才能成功，且进程级异常状态优先于输出文本。
///
/// 覆盖的缺陷类：计划未跑满、重复或越界序号、重复计划行、`Bail out!` 之后的结果行被当成成功；
/// 半行被跨流拼接成合法结果；标准错误上的 `not ok`/`Bail out!` 污染判定；超时与信号状态被完整输出覆盖。
#[test]
fn tap_completion_requires_one_intact_stdout_protocol() {
    // Step 1: 各类结构残缺或违背 TAP 约束的 stdout 序列即便配合退出码 0 也不能判定为成功。
    let outcome = exited(0);
    for (bytes, expected) in [
        (b"1..2\nok 1 first\n".as_slice(), Verdict::Broken),
        (b"1..1\nok 1 first\nok 1 duplicate\n", Verdict::Broken),
        (b"1..1\nok 2 # SKIP terminal\n", Verdict::Broken),
        (b"1..1\nok 1 first\n1..1\n", Verdict::Broken),
        (b"1..1\nBail out! failed\nok 1 last\n", Verdict::Failed),
    ] {
        assert_eq!(
            observe(bytes, &outcome),
            expected,
            "{}",
            String::from_utf8_lossy(bytes)
        );
    }
    // Step 2: stdout 与 stderr 的半行数据经真实管道转发后仍不得跨流拼成一条合法的 ok 记录。
    let (run, result) = run_script("printf '1..1\\nok '; printf '1 success\\n' >&2");
    assert_eq!(
        run.outcome, outcome,
        "split line changed the process outcome"
    );
    assert_eq!(result.verdict, Verdict::Broken);
    // Step 3: 即使 stdout 输出了完整的通过文本，进程超时或信号终止依然优先决定最终 Verdict。
    for (outcome, verdict) in [
        (
            ExitOutcome::TimedOut {
                raw: None,
                signal: None,
            },
            Verdict::TimedOut,
        ),
        (
            ExitOutcome::Signaled { signal: 9, raw: 9 },
            Verdict::Signaled,
        ),
    ] {
        assert_eq!(observe(b"1..1\nok 1 success\n", &outcome), verdict);
    }
    // Step 4: stderr 中的 "not ok" 或 "Bail out!" 仅被视作普通诊断输出，不会污染合法的 stdout TAP 结果。
    for complaint in ["not ok 1 diagnostic only", "Bail out! diagnostic only"] {
        let script = format!("printf '1..1\\nok 1 success\\n'; printf '{complaint}\\n' >&2");
        let (run, result) = run_script(&script);
        assert_eq!(run.outcome, outcome, "{complaint}: process did not exit 0");
        assert_eq!(result.verdict, Verdict::Pass, "{complaint}");
    }
}

/// 用 `/bin/sh -c` 运行一段只写标准输出的短脚本，两路输出丢弃到 `/dev/null`，返回进程记录与 TAP 判定。
fn run_script(script: &str) -> (Execution, ProtocolResult) {
    let _exclusive = EXECUTION.lock().unwrap_or_else(|error| error.into_inner());
    let sink = File::create("/dev/null").unwrap();
    run_tap_command(
        &std::env::temp_dir(),
        &["/bin/sh".to_owned(), "-c".to_owned(), script.to_owned()],
        &BTreeMap::new(),
        Duration::from_secs(3),
        [sink.as_raw_fd(), sink.as_raw_fd()],
    )
}

/// 验证 TAP 解析结果与输入如何被切分为 `feed` 块无关，防止跨调用被截断的行丢失或重复。
#[test]
fn arbitrary_chunk_boundaries_preserve_markers() {
    let outcome = exited(0);
    for (text, expected) in [
        ("TAP version 13\n1..2\nok 1 a\nok 2 b\n", Verdict::Pass),
        (
            "TAP version 13\n1..2\nok 1 a\nnot ok 2 b\n",
            Verdict::Failed,
        ),
        ("TAP version 13\n1..1\nBail out! aborted\n", Verdict::Failed),
    ] {
        assert_eq!(observe(text.as_bytes(), &outcome), expected, "{text}");
        for split in 0..=text.len() {
            let mut observer = TapObserver::new();
            observer.feed(&text.as_bytes()[..split]);
            observer.feed(&text.as_bytes()[split..]);
            assert_eq!(
                observer.finish(&outcome).verdict,
                expected,
                "{text}: split at {split}"
            );
        }
        for size in 1..=text.len() {
            let mut observer = TapObserver::new();
            for chunk in text.as_bytes().chunks(size) {
                observer.feed(chunk);
            }
            assert_eq!(
                observer.finish(&outcome).verdict,
                expected,
                "{text}: chunk size {size}"
            );
        }
    }
}

/// 验证 `TapObserver::finish` 会解析末尾不含换行的残留行，并且对 CRLF 与 LF 行尾给出相同判定。
///
/// 覆盖的缺陷类：最后一行缺少换行时结果被丢弃，导致完整测试被误判为损坏；CRLF 输出被整体误判。
#[test]
fn finish_consumes_unterminated_lines() {
    let outcome = exited(0);
    assert_eq!(observe(b"1..1\nok 1 last", &outcome), Verdict::Pass);
    assert_eq!(observe(b"1..1\r\nok 1 last\r\n", &outcome), Verdict::Pass);
    assert_eq!(observe(b"1..1\nnot ok 1 last", &outcome), Verdict::Failed);
}

/// 验证超长行不会破坏后续解析，也不会把被截断的控制行当作合法结果或忽略其失败含义。
///
/// 覆盖的缺陷类：无换行的超长无关行耗尽缓冲后丢失后续合法 TAP；截断的 `ok` 行被当成通过；
/// 截断的 `Bail out!` 行被忽略后误判为通过。
#[test]
fn long_output_does_not_hide_failure_or_invent_marker() {
    let outcome = exited(0);
    // 无换行的超长无关行在行尾被丢弃，后续合法 TAP 仍完整解析。
    let mut observer = TapObserver::new();
    for _ in 0..4096 {
        observer.feed(b"unrelated output without a newline");
    }
    observer.feed(b"\n");
    observer.feed(b"1..1\nok 1 passed\n");
    assert_eq!(observer.finish(&outcome).verdict, Verdict::Pass);
    // 被 4096 字节上限截断的 ok 结果行必须判定为协议损坏，而不是通过。
    let mut observer = TapObserver::new();
    observer.feed(b"1..1\nok 1 ");
    for _ in 0..4096 {
        observer.feed(b"x");
    }
    observer.feed(b"\n");
    assert_eq!(observer.finish(&outcome).verdict, Verdict::Broken);
    // 被截断的 Bail out! 行仍要生效，不能因为超长而漏掉中止。
    let mut observer = TapObserver::new();
    observer.feed(b"1..1\nBail out! ");
    for _ in 0..4096 {
        observer.feed(b"x");
    }
    observer.feed(b"\n");
    assert_eq!(observer.finish(&outcome).verdict, Verdict::Failed);
}

/// 验证进程级异常状态压过输出的 TAP 文本，防止启动失败、信号终止、超时与 I/O 错误被完整输出掩盖成通过。
#[test]
fn abnormal_outcomes_keep_infrastructure_boundaries() {
    let tap = b"1..1\nok 1 passed\n";
    for (outcome, expected) in [
        (
            ExitOutcome::SpawnError {
                errno: Some(2),
                message: "No such file or directory".into(),
            },
            Verdict::InfrastructureError,
        ),
        (
            ExitOutcome::Signaled { signal: 9, raw: 9 },
            Verdict::Signaled,
        ),
        (
            ExitOutcome::Signaled {
                signal: 11,
                raw: 139,
            },
            Verdict::Signaled,
        ),
        (
            ExitOutcome::TimedOut {
                raw: None,
                signal: None,
            },
            Verdict::TimedOut,
        ),
        (
            ExitOutcome::TimedOut {
                raw: Some(9),
                signal: Some(9),
            },
            Verdict::TimedOut,
        ),
        (
            ExitOutcome::IoError {
                errno: Some(5),
                message: "Input/output error".into(),
                raw: Some(0),
            },
            Verdict::InfrastructureError,
        ),
    ] {
        assert_eq!(observe(tap, &outcome), expected, "{outcome:?}");
    }
}
