use contest_run::execute::ExitOutcome;
use contest_run::model::Protocol;
use contest_run::report::{ProtocolObserver, StreamKind, Verdict};

/// 构造正常 wait 状态，保留其原始编码供消费者检查。
fn exited(code: i32) -> ExitOutcome {
    ExitOutcome::Exited {
        code,
        raw: code << 8,
    }
}

/// 直接调用正式观察器，不替协议实现或输出转发建立副本。
fn observe(protocol: &Protocol, stdout: &[u8], stderr: &[u8], outcome: &ExitOutcome) -> Verdict {
    let mut observer = ProtocolObserver::new(protocol);
    observer.feed(StreamKind::Stdout, stdout);
    observer.feed(StreamKind::Stderr, stderr);
    observer.finish(outcome)
}

/// 构造固定 libc 选择，variant 与 selector 都参与原标记匹配。
fn libctest(variant: &str, selector: &str) -> Protocol {
    Protocol::Pre2025Libctest {
        variant: variant.into(),
        selector: selector.into(),
    }
}

/// 使用固定 runtest.c 的真实 START、Pass!、END 行。
fn libc_output(variant: &str, selector: &str) -> String {
    format!("========== START entry-{variant}.exe {selector} ==========\nPass!\n========== END entry-{variant}.exe {selector} ==========\n")
}

/// 使用 basic TEST_START/TEST_END 的真实行，mkdir_ 对应 test_mkdir。
fn basic_output(name: &str, body: &str) -> String {
    let marker = if name == "mkdir_" { "mkdir" } else { name };
    format!("========== START test_{marker} ==========\n{body}========== END test_{marker} ==========\n")
}

#[test]
/// Plain 只根据真实退出结果判定，不解释正文中的评分单词。
fn plain_keeps_exit_status_semantics() {
    assert_eq!(
        observe(
            &Protocol::Plain,
            b"FAIL LTP CASE x : 0\n",
            b"FAIL\n",
            &exited(0)
        ),
        Verdict::Pass
    );
    for code in [1, 2, 32, 127, 255] {
        assert_eq!(
            observe(&Protocol::Plain, b"PASS\nPass!\n", b"", &exited(code)),
            Verdict::Failed
        );
    }
}

#[test]
/// 完整 basic 只证明执行结束，不签评分 Pass。
fn basic_requires_its_own_markers() {
    let protocol = Protocol::Basic {
        name: "mmap".into(),
    };
    let output = basic_output("mmap", "mmap content: data\n");
    assert_eq!(
        observe(&protocol, output.as_bytes(), b"", &exited(0)),
        Verdict::Completed
    );
    assert_eq!(
        observe(&protocol, b"PASS\n", b"", &exited(0)),
        Verdict::NotRun
    );
    assert_eq!(
        observe(
            &protocol,
            basic_output("pipe", "").as_bytes(),
            b"",
            &exited(0)
        ),
        Verdict::NotRun
    );
    assert_eq!(
        observe(
            &protocol,
            b"========== START test_mmap ==========\n",
            b"",
            &exited(0)
        ),
        Verdict::Failed
    );
    assert_eq!(
        observe(&protocol, output.as_bytes(), b"", &exited(1)),
        Verdict::Failed
    );
    let mismatched =
        b"========== START test_mmap ==========\n========== END test_pipe ==========\n";
    assert_eq!(
        observe(&protocol, mismatched, b"", &exited(0)),
        Verdict::Failed
    );
}

#[test]
/// execve 成功换映像后只输出 END main；其它 basic 不接受该结束标记。
fn basic_execve_and_mkdir_use_original_names() {
    let output = b"========== START test_execve ==========\n  I am test_echo.\nexecve success.\n========== END main ==========";
    assert_eq!(
        observe(
            &Protocol::Basic {
                name: "execve".into()
            },
            output,
            b"",
            &exited(0)
        ),
        Verdict::Completed
    );
    let not_execve = b"========== START test_pipe ==========\n========== END main ==========\n";
    assert_eq!(
        observe(
            &Protocol::Basic {
                name: "pipe".into()
            },
            not_execve,
            b"",
            &exited(0)
        ),
        Verdict::Failed
    );
    assert_eq!(
        observe(
            &Protocol::Basic {
                name: "mkdir_".into()
            },
            basic_output("mkdir_", "").as_bytes(),
            b"",
            &exited(0)
        ),
        Verdict::Completed
    );
}

#[test]
/// 已知错误即使带 END 或后继 Pass! 也不能被覆盖。
fn basic_known_errors_take_priority() {
    let cases = [
        ("clone", "clone process error."),
        ("close", "  close error."),
        ("execve", "  execve error."),
        ("exit", "exit ERR."),
        ("getcwd", "getcwd ERROR."),
        ("getppid", "  getppid error."),
        ("gettimeofday", "gettimeofday error."),
        ("mkdir_", "  mkdir error."),
        ("mmap", "mmap error."),
        ("munmap", "mmap error."),
        ("sleep", "sleep error."),
        ("unlink", "  unlink error!"),
        ("wait", "wait child error."),
        ("waitpid", "waitpid error."),
        ("mount", "mount return: -1"),
        ("mount", "umount return: -1"),
        ("umount", "mount return: -1"),
        ("umount", "return: -1"),
        ("munmap", "munmap return: -1"),
        ("write", " --- Assert Fatal ! ---"),
    ];
    for (name, error) in cases {
        let protocol = Protocol::Basic { name: name.into() };
        let output = basic_output(name, &format!("{error}\nPass!\n"));
        assert_eq!(
            observe(&protocol, output.as_bytes(), b"", &exited(0)),
            Verdict::Failed,
            "{name}: {error}"
        );
        assert_eq!(
            observe(
                &protocol,
                basic_output(name, "").as_bytes(),
                format!("{error}\n").as_bytes(),
                &exited(0)
            ),
            Verdict::Failed
        );
    }
    let protocol = Protocol::Basic {
        name: "mount".into(),
    };
    assert_eq!(
        observe(
            &protocol,
            basic_output(
                "mount",
                "mount return: 0\numount return: 0\nFAIL is payload\n"
            )
            .as_bytes(),
            b"",
            &exited(0)
        ),
        Verdict::Completed
    );
}

#[test]
/// return1 必须与同一选择项的完整成功正文共同出现。
fn libctest_return_one_is_not_plain_failure_or_automatic_pass() {
    for variant in ["dynamic", "static"] {
        let protocol = libctest(variant, "argv");
        let output = libc_output(variant, "argv");
        assert_eq!(
            observe(&protocol, output.as_bytes(), b"", &exited(1)),
            Verdict::Pass
        );
        for code in [0, 2, 127, 255] {
            assert_eq!(
                observe(&protocol, output.as_bytes(), b"", &exited(code)),
                Verdict::Failed
            );
        }
        assert_eq!(observe(&protocol, b"", b"", &exited(1)), Verdict::NotRun);
        assert_eq!(
            observe(&protocol, b"Pass!\n", b"", &exited(1)),
            Verdict::NotRun
        );
        assert_eq!(
            observe(
                &protocol,
                b"========== START entry-dynamic.exe argv ==========\nPass!\n",
                b"",
                &exited(1)
            ),
            if variant == "dynamic" {
                Verdict::Failed
            } else {
                Verdict::NotRun
            }
        );
    }
}

#[test]
/// 错误 variant、selector 或仅另一个 case 的成功行不能补齐所选 case。
fn libctest_scopes_success_to_variant_and_selector() {
    let protocol = libctest("dynamic", "argv");
    for output in [libc_output("static", "argv"), libc_output("dynamic", "env")] {
        assert_eq!(
            observe(&protocol, output.as_bytes(), b"", &exited(1)),
            Verdict::NotRun
        );
    }
    let output = b"========== START entry-dynamic.exe argv ==========\n========== END entry-dynamic.exe argv ==========\n========== START entry-dynamic.exe env ==========\nPass!\n========== END entry-dynamic.exe env ==========\n";
    assert_eq!(observe(&protocol, output, b"", &exited(1)), Verdict::Failed);
    let no_end = b"========== START entry-dynamic.exe argv ==========\nPass!\n========== END entry-static.exe argv ==========\n";
    assert_eq!(observe(&protocol, no_end, b"", &exited(1)), Verdict::Failed);
    let no_pass = b"========== START entry-dynamic.exe argv ==========\n========== END entry-dynamic.exe argv ==========\n";
    assert_eq!(
        observe(&protocol, no_pass, b"", &exited(1)),
        Verdict::Failed
    );
}

#[test]
/// 对应 FAIL 在成功行之前、之后或另一个流中都保持失败优先。
fn libctest_failure_is_sticky_and_selector_exact() {
    let protocol = libctest("dynamic", "argv");
    let valid = libc_output("dynamic", "argv");
    for failure in [
        "FAIL argv [internal]\n",
        "FAIL argv [status 1]\n",
        "FAIL argv [timed out]\n",
        "FAIL argv [signal Killed]\n",
        "FAIL argv [unknown]\n",
    ] {
        for output in [format!("{failure}{valid}"), format!("{valid}{failure}")] {
            assert_eq!(
                observe(&protocol, output.as_bytes(), b"", &exited(1)),
                Verdict::Failed
            );
        }
        assert_eq!(
            observe(&protocol, valid.as_bytes(), failure.as_bytes(), &exited(1)),
            Verdict::Failed
        );
    }
    assert_eq!(
        observe(
            &protocol,
            valid.as_bytes(),
            b"FAIL argv_extra [status 1]\nFAIL env [status 1]\n",
            &exited(1)
        ),
        Verdict::Pass
    );
}

#[test]
/// 每一个切分位置与多种块长都必须产生同一真实协议判定。
fn arbitrary_chunk_boundaries_preserve_markers() {
    let cases = [
        (
            libctest("dynamic", "argv"),
            libc_output("dynamic", "argv"),
            exited(1),
            Verdict::Pass,
        ),
        (
            Protocol::Basic {
                name: "mmap".into(),
            },
            basic_output("mmap", "mmap content: ok\n"),
            exited(0),
            Verdict::Completed,
        ),
        (
            Protocol::Basic {
                name: "mmap".into(),
            },
            basic_output("mmap", "mmap error.\n"),
            exited(0),
            Verdict::Failed,
        ),
        (
            libctest("dynamic", "argv"),
            format!("{}FAIL argv [status 1]\n", libc_output("dynamic", "argv")),
            exited(1),
            Verdict::Failed,
        ),
    ];
    for (protocol, text, outcome, expected) in cases {
        for stream in [StreamKind::Stdout, StreamKind::Stderr] {
            for split in 0..=text.len() {
                let mut observer = ProtocolObserver::new(&protocol);
                observer.feed(stream, &text.as_bytes()[..split]);
                observer.feed(stream, &text.as_bytes()[split..]);
                assert_eq!(observer.finish(&outcome), expected, "split {split}");
            }
            for size in 1..=text.len() {
                let mut observer = ProtocolObserver::new(&protocol);
                for chunk in text.as_bytes().chunks(size) {
                    observer.feed(stream, chunk);
                }
                assert_eq!(observer.finish(&outcome), expected, "chunk {size}");
            }
        }
    }
}

#[test]
/// stdout 和 stderr 的半行以及各自的成功区间均不能互相补齐。
fn streams_never_synthesize_lines_or_success() {
    let protocol = libctest("dynamic", "argv");
    let text = libc_output("dynamic", "argv");
    let split = 20;
    let mut observer = ProtocolObserver::new(&protocol);
    observer.feed(StreamKind::Stdout, &text.as_bytes()[..split]);
    observer.feed(StreamKind::Stderr, &text.as_bytes()[split..]);
    assert_eq!(observer.finish(&exited(1)), Verdict::NotRun);
    let framed = b"========== START entry-dynamic.exe argv ==========\n========== END entry-dynamic.exe argv ==========\n";
    assert_eq!(
        observe(&protocol, framed, b"Pass!\n", &exited(1)),
        Verdict::Failed
    );
    let start = b"========== START entry-dynamic.exe argv ==========\nPass!\n";
    let end = b"========== END entry-dynamic.exe argv ==========\n";
    assert_eq!(observe(&protocol, start, end, &exited(1)), Verdict::Failed);
    let valid = libc_output("dynamic", "argv");
    assert_eq!(
        observe(
            &protocol,
            valid.as_bytes(),
            b"FAIL argv [status 1]",
            &exited(1)
        ),
        Verdict::Failed
    );
}

#[test]
/// 无 LF 末行仍消费，CRLF 与原 LF 有相同的完整行语义。
fn finish_consumes_unterminated_lines() {
    let protocol = libctest("static", "env");
    let output = libc_output("static", "env");
    assert_eq!(
        observe(
            &protocol,
            output.trim_end_matches('\n').as_bytes(),
            b"",
            &exited(1)
        ),
        Verdict::Pass
    );
    assert_eq!(
        observe(
            &protocol,
            output.replace('\n', "\r\n").as_bytes(),
            b"",
            &exited(1)
        ),
        Verdict::Pass
    );
    let basic = Protocol::Basic {
        name: "mmap".into(),
    };
    assert_eq!(
        observe(
            &basic,
            basic_output("mmap", "").as_bytes(),
            b"mmap error.",
            &exited(0)
        ),
        Verdict::Failed
    );
}

#[test]
/// 输出规模与单行长度不影响状态容量，长失败前缀仍不得被丢弃。
fn long_output_does_not_hide_failure_or_invent_marker() {
    let protocol = libctest("dynamic", "argv");
    let mut observer = ProtocolObserver::new(&protocol);
    for _ in 0..4096 {
        observer.feed(StreamKind::Stdout, b"unrelated output without a newline");
    }
    observer.feed(StreamKind::Stdout, b"\n");
    observer.feed(
        StreamKind::Stdout,
        libc_output("dynamic", "argv").as_bytes(),
    );
    assert_eq!(observer.finish(&exited(1)), Verdict::Pass);
    let mut observer = ProtocolObserver::new(&protocol);
    observer.feed(
        StreamKind::Stdout,
        libc_output("dynamic", "argv").as_bytes(),
    );
    observer.feed(StreamKind::Stderr, b"FAIL argv ");
    for _ in 0..4096 {
        observer.feed(StreamKind::Stderr, b"long diagnostic ");
    }
    assert_eq!(observer.finish(&exited(1)), Verdict::Failed);
    let mut observer = ProtocolObserver::new(&protocol);
    observer.feed(
        StreamKind::Stdout,
        b"========== START entry-dynamic.exe argv ==========",
    );
    for _ in 0..4096 {
        observer.feed(StreamKind::Stdout, b"x");
    }
    observer.feed(
        StreamKind::Stdout,
        b"\nPass!\n========== END entry-dynamic.exe argv ==========\n",
    );
    assert_eq!(observer.finish(&exited(1)), Verdict::NotRun);
}

#[test]
/// LTP 退出位来自固定头定义，包装中恒有的 FAIL LTP CASE 文字没有评分权。
fn ltp_preserves_result_bits_and_mixed_configuration() {
    let cases = [
        (0, Verdict::Pass),
        (1, Verdict::Failed),
        (2, Verdict::Broken),
        (4, Verdict::Warning),
        (32, Verdict::Configuration),
        (32 | 1, Verdict::Failed),
        (32 | 2, Verdict::Broken),
        (32 | 4, Verdict::Warning),
        (32 | 1 | 4, Verdict::Failed),
        (32 | 1 | 2 | 4, Verdict::Broken),
        (8, Verdict::Broken),
        (16, Verdict::Broken),
        (64, Verdict::Broken),
        (127, Verdict::Broken),
        (255, Verdict::Broken),
    ];
    for (code, expected) in cases {
        let outcome = exited(code);
        let original = outcome.clone();
        assert_eq!(
            observe(
                &Protocol::Ltp,
                b"FAIL LTP CASE fork01 : 0\nTPASS: payload\n",
                b"",
                &outcome
            ),
            expected
        );
        assert_eq!(outcome, original);
        assert_eq!(
            outcome,
            ExitOutcome::Exited {
                code,
                raw: code << 8
            }
        );
    }
}

#[test]
/// 任意成功正文不能覆盖 spawn、signal、timeout 或 I/O 的原始边界。
fn abnormal_outcomes_keep_infrastructure_boundaries() {
    let cases = [
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
    ];
    let protocols = [
        Protocol::Plain,
        Protocol::Ltp,
        Protocol::Basic {
            name: "mmap".into(),
        },
        libctest("dynamic", "argv"),
    ];
    for protocol in protocols {
        for (outcome, expected) in &cases {
            let original = outcome.clone();
            let text = format!(
                "{}{}",
                basic_output("mmap", ""),
                libc_output("dynamic", "argv")
            );
            assert_eq!(
                observe(&protocol, text.as_bytes(), b"Pass!\n", outcome),
                *expected
            );
            assert_eq!(*outcome, original);
        }
    }
}
