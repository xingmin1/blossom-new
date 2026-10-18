//! 选择错误、只读目录和汇总退出均使用正式用户入口。

use contest_run::model::Protocol;
use contest_run::outcome::{Execution, ExitOutcome};
use contest_run::report::{ProtocolObserver, StreamKind};
use contest_run::selftests::{self, Options, Summary};
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

struct Scratch(PathBuf);

impl Drop for Scratch {
    /// 只释放本测试拥有的清单目录。
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

/// 清单可以列出尚未安装的程序，执行时的真实 ENOENT 必须失败。
#[test]
fn list_reads_only_manifest_and_execution_reports_missing_program() {
    let directory = std::env::temp_dir().join(format!("selftest-list-{}", std::process::id()));
    fs::create_dir(&directory).unwrap();
    let scratch = Scratch(directory);
    let manifest = scratch.0.join("kselftest-list.txt");
    fs::write(&manifest, "fs/file_shared\n").unwrap();
    let args = vec![
        "--manifest".into(),
        manifest.to_str().unwrap().into(),
        "--list".into(),
    ];
    assert!(selftests::select(&Options::parse(&args).unwrap()).is_err());
    fs::write(&manifest, "fs:file_shared\nnet:tcp_udp\n").unwrap();
    let list = Command::new(env!("CARGO_BIN_EXE_user-selftests"))
        .args(&args)
        .output()
        .unwrap();
    assert_eq!(list.status.code(), Some(0));
    assert_eq!(list.stdout, b"fs:file_shared\nnet:tcp_udp\n");
    assert!(!scratch.0.join("fs").exists());
    let mut full = fs::OpenOptions::new()
        .write(true)
        .open("/dev/full")
        .unwrap();
    assert!(selftests::run(&Options::parse(&args).unwrap(), &mut full, [1, 2]).is_err());
    let options = Options::parse(&[
        "--manifest".into(),
        manifest.to_str().unwrap().into(),
        "--collection".into(),
        "fs".into(),
        "--program".into(),
        "net:tcp_udp".into(),
    ])
    .unwrap();
    assert_eq!(selftests::select(&options).unwrap().len(), 2);
    let missing = Command::new(env!("CARGO_BIN_EXE_user-selftests"))
        .args([
            "--manifest",
            manifest.to_str().unwrap(),
            "--program",
            "fs:file_shared",
        ])
        .output()
        .unwrap();
    assert_eq!(missing.status.code(), Some(2));
    for selection in ["fs:missing", "../bad:test", "fs:../bad"] {
        let result = Options::parse(&[
            "--manifest".into(),
            manifest.to_str().unwrap().into(),
            "--program".into(),
            selection.into(),
        ])
        .and_then(|options| selftests::select(&options));
        assert!(result.is_err(), "{selection}");
    }
    for contents in ["fs:a\nfs:a\n", "fs:../a\n", ""] {
        fs::write(&manifest, contents).unwrap();
        assert!(selftests::select(&options).is_err());
    }
    assert!(Options::parse(&[
        "--collection".into(),
        "fs".into(),
        "--collection".into(),
        "fs".into()
    ])
    .is_err());
}

/// 期限输入拒绝缺值、非正整数、溢出和重复覆盖，避免无效预算进入执行器。
#[test]
fn program_timeout_rejects_invalid_cli_values() {
    for values in [
        vec![],
        vec!["0"],
        vec!["-1"],
        vec!["+1"],
        vec!["1.5"],
        vec![""],
        vec!["18446744073709551615"],
        vec!["18446744073709551616"],
        vec!["1", "--program-timeout-seconds", "2"],
    ] {
        let args = std::iter::once("--program-timeout-seconds")
            .chain(values)
            .map(String::from)
            .collect::<Vec<_>>();
        assert!(Options::parse(&args).is_err(), "{args:?}");
    }
}

/// 同一份协议对象派生全跳过、失败和损坏的程序级退出，不复制另一份判定。
#[test]
fn summary_exit_codes_preserve_failure_and_skip_priorities() {
    for (text, code, expected) in [
        ("1..1\nok 1 passed\n", 0, 0),
        ("1..1\nok 1 # SKIP unavailable\n", 0, 4),
        ("1..0 # SKIP unavailable\n", 4, 4),
        ("1..1\nnot ok 1 failed\n", 0, 1),
        ("1..2\nok 1 incomplete\n", 0, 2),
        ("1..1\nok 1 # XFAIL expected\n", 2, 1),
    ] {
        let outcome = ExitOutcome::Exited {
            code,
            raw: code << 8,
        };
        let mut observer = ProtocolObserver::new(&Protocol::Tap);
        observer.feed(StreamKind::Stdout, text.as_bytes());
        let mut run = Execution {
            protocol: observer.finish(&outcome),
            outcome,
            io_errors: Vec::new(),
            elapsed: Duration::ZERO,
        };
        let mut summary = Summary::default();
        summary.record(&run);
        assert_eq!(summary.status(), expected, "{text}");
        run.io_errors
            .push("real output could not be delivered".into());
        summary.record(&run);
        assert_eq!(summary.status(), 2);
    }
}
