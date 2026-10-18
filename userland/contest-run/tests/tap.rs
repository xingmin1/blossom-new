//! 固定头真实输出与流式边界的消费者回归；编译样本只在宿主测试执行时发生。

use contest_run::model::{CommandSpec, Protocol};
use contest_run::outcome::{ExitOutcome, Verdict};
use contest_run::process::run_command;
use contest_run::report::{ProtocolObserver, StreamKind};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::os::fd::AsRawFd;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

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

/// 同一固定头样本核对 TAP 和真实退出，再经用户入口核对慢程序与持续输出的绝对期限。
#[test]
fn fixed_header_outputs_keep_skip_and_failure_exit_semantics() {
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
        let spec = CommandSpec::Exec {
            program: executable.to_str().unwrap().into(),
            args: vec![mode.into()],
        };
        let run = run_command(
            None,
            &scratch.0,
            &spec,
            &BTreeMap::new(),
            Duration::from_secs(3),
            &Protocol::Tap,
            [stdout.as_raw_fd(), stderr.as_raw_fd()],
        );
        assert_eq!(
            run.outcome,
            ExitOutcome::Exited {
                code,
                raw: code << 8
            },
            "{mode}"
        );
        assert!(run.io_errors.is_empty(), "{mode}: {:?}", run.io_errors);
        assert_eq!(run.protocol.verdict, expected, "{mode}: {:?}", run.protocol);
        let bytes = fs::read(scratch.0.join(format!("{mode}.stdout"))).unwrap();
        for size in 1..=bytes.len() {
            let mut observer = ProtocolObserver::new(&Protocol::Tap);
            for chunk in bytes.chunks(size) {
                observer.feed(StreamKind::Stdout, chunk);
            }
            assert_eq!(
                observer.finish(&run.outcome),
                run.protocol,
                "{mode}: chunk {size}"
            );
        }
    }
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

struct Scratch(PathBuf);

impl Drop for Scratch {
    /// 测试仅删除自己创建的临时样本和输出。
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

/// 缺项、重复、超时和另一个流的文字均不能被 exit0 或后续 ok 修复。
#[test]
fn tap_completion_requires_one_intact_stdout_protocol() {
    let exited = ExitOutcome::Exited { code: 0, raw: 0 };
    for bytes in [
        b"1..2\nok 1 first\n".as_slice(),
        b"1..1\nok 1 first\nok 1 duplicate\n",
        b"1..1\nok 2 # SKIP terminal\n",
        b"1..1\nok 1 first\n1..1\n",
        b"1..1\nBail out! failed\nok 1 last\n",
    ] {
        let mut observer = ProtocolObserver::new(&Protocol::Tap);
        observer.feed(StreamKind::Stdout, bytes);
        assert!(!observer.finish(&exited).verdict.is_success());
    }
    let mut observer = ProtocolObserver::new(&Protocol::Tap);
    observer.feed(StreamKind::Stdout, b"1..1\nok ");
    observer.feed(StreamKind::Stderr, b"1 success\n");
    assert_eq!(observer.finish(&exited).verdict, Verdict::Broken);
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
        let mut observer = ProtocolObserver::new(&Protocol::Tap);
        observer.feed(StreamKind::Stdout, b"1..1\nok 1 success\n");
        assert_eq!(observer.finish(&outcome).verdict, verdict);
    }
    let mut observer = ProtocolObserver::new(&Protocol::Tap);
    observer.feed(StreamKind::Stdout, b"1..1\nok 1 success\n");
    observer.feed(
        StreamKind::Stderr,
        b"not ok 1 diagnostic only\nBail out! diagnostic only\n",
    );
    assert_eq!(observer.finish(&exited).verdict, Verdict::Pass);
}
