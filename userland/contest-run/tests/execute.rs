//! 同源真实宿主进程覆盖；不模拟 mount/chroot 成功，也不签署 guest 结论。

use contest_run::custom::hook_device;
use contest_run::execute::{
    execute_case as execute_real_case, run_command as execute_real_command, CaseRecord, Execution,
    ExitOutcome,
};
use contest_run::model::{CommandSpec, Protocol, ResolvedCase, SourceLocation};
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

static SERIAL: AtomicU64 = AtomicU64::new(0);
static EXECUTION: Mutex<()> = Mutex::new(());

/// 锁只序列化测试而不保护数据，前例断言 panic 后仍允许其余真实回归独立运行。
fn exclusive_execution() -> std::sync::MutexGuard<'static, ()> {
    match EXECUTION.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// 宿主 harness 的并行 case 不得让 fork helper 继承另一用例尚未释放的控制端点。
fn run_command(
    root: Option<&Path>,
    cwd: &Path,
    spec: &CommandSpec,
    env: &BTreeMap<String, String>,
    timeout: Duration,
    protocol: &Protocol,
    outputs: [std::os::fd::RawFd; 2],
) -> Execution {
    let _exclusive = exclusive_execution();
    execute_real_command(root, cwd, spec, env, timeout, protocol, outputs)
}

/// 每个正式 case 按生产的单线程约束执行，锁不进入执行器或 fork 子分支。
fn execute_case(
    root: Option<&Path>,
    case: &ResolvedCase,
    iteration: u32,
    outputs: [std::os::fd::RawFd; 2],
    hook: impl FnMut(bool, &str) -> io::Result<()>,
) -> CaseRecord {
    let _exclusive = exclusive_execution();
    execute_real_case(root, case, iteration, outputs, hook)
}

struct Scratch(PathBuf);

impl Scratch {
    /// 每项测试拥有独立真实目录，避免平行测试覆盖输出。
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "contest-execute-{}-{}",
            std::process::id(),
            SERIAL.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    /// 输出文件由测试拥有，Command 不继承这些控制描述符。
    fn output(&self, name: &str) -> File {
        OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(self.0.join(name))
            .unwrap()
    }

    /// 使用正式执行函数运行 Plain，不另写进程或状态分类器。
    fn run(&self, spec: CommandSpec, timeout: Duration) -> (Execution, Vec<u8>, Vec<u8>) {
        self.run_protocol(spec, timeout, &Protocol::Plain)
    }

    /// 指定实际协议以覆盖直接输出和捕获 relay 两条正式路径。
    fn run_protocol(
        &self,
        spec: CommandSpec,
        timeout: Duration,
        protocol: &Protocol,
    ) -> (Execution, Vec<u8>, Vec<u8>) {
        let stdout = self.output("stdout");
        let stderr = self.output("stderr");
        let run = run_command(
            None,
            &self.0,
            &spec,
            &BTreeMap::new(),
            timeout,
            protocol,
            [stdout.as_raw_fd(), stderr.as_raw_fd()],
        );
        (
            run,
            fs::read(self.0.join("stdout")).unwrap(),
            fs::read(self.0.join("stderr")).unwrap(),
        )
    }
}

impl Drop for Scratch {
    /// 只移除本项测试生成的临时目录。
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

/// shell 仅作为测试程序，原文由正式 Shell 分支交给它。
fn shell(command: &str) -> CommandSpec {
    CommandSpec::Shell {
        shell: "/bin/sh".into(),
        command: command.into(),
    }
}

/// spawn 错误与真实 exit127、非零和信号分别保留原 wait 编码。
#[test]
fn real_exit_and_spawn_states_are_distinct() {
    for code in [0, 7, 127] {
        let scratch = Scratch::new();
        let (run, _, _) = scratch.run(shell(&format!("exit {code}")), Duration::from_secs(2));
        assert_eq!(
            run.outcome,
            ExitOutcome::Exited {
                code,
                raw: code << 8
            }
        );
        assert!(run.io_errors.is_empty());
    }
    let scratch = Scratch::new();
    let (run, _, _) = scratch.run(
        CommandSpec::Exec {
            program: scratch.0.join("absent").to_str().unwrap().into(),
            args: Vec::new(),
        },
        Duration::from_secs(2),
    );
    assert!(matches!(
        run.outcome,
        ExitOutcome::SpawnError {
            errno: Some(libc::ENOENT),
            ..
        }
    ));
    let scratch = Scratch::new();
    let (run, _, _) = scratch.run(shell("kill -TERM $$"), Duration::from_secs(2));
    assert!(matches!(
        run.outcome,
        ExitOutcome::Signaled {
            signal: libc::SIGTERM,
            raw: libc::SIGTERM
        }
    ));
}

/// exec 数组保留空参数、空格和 Unicode，cwd/env 由真实程序观察。
#[test]
fn real_cwd_environment_and_argument_boundaries() {
    let scratch = Scratch::new();
    let stdout = scratch.output("stdout");
    let stderr = scratch.output("stderr");
    let spec = CommandSpec::Exec {
        program: "/bin/sh".into(),
        args: vec![
            "-c".into(),
            "printf '%s\\000' \"$PWD\" \"$CHECK\" \"$1\" \"$2\" \"$3\"".into(),
            "argv0".into(),
            "".into(),
            "value with spaces".into(),
            "中文".into(),
        ],
    };
    let env = BTreeMap::from([("CHECK".into(), "env with spaces".into())]);
    let run = run_command(
        None,
        &scratch.0,
        &spec,
        &env,
        Duration::from_secs(2),
        &Protocol::Plain,
        [stdout.as_raw_fd(), stderr.as_raw_fd()],
    );
    assert!(matches!(run.outcome, ExitOutcome::Exited { code: 0, .. }));
    let expected = format!(
        "{}\0env with spaces\0\0value with spaces\0中文\0",
        scratch.0.display()
    );
    assert_eq!(
        fs::read(scratch.0.join("stdout")).unwrap(),
        expected.as_bytes()
    );
}

/// 内嵌 NUL 沿 std 的输入错误返回，空字符串参数仍是合法参数。
#[test]
fn nul_and_invalid_root_or_cwd_remain_spawn_errors() {
    let scratch = Scratch::new();
    let stdout = scratch.output("stdout");
    let stderr = scratch.output("stderr");
    for spec in [
        CommandSpec::Exec {
            program: "/bin/true".into(),
            args: vec!["bad\0arg".into()],
        },
        shell("bad\0command"),
    ] {
        let run = run_command(
            None,
            &scratch.0,
            &spec,
            &BTreeMap::new(),
            Duration::from_secs(1),
            &Protocol::Plain,
            [stdout.as_raw_fd(), stderr.as_raw_fd()],
        );
        assert!(matches!(run.outcome, ExitOutcome::SpawnError { .. }));
    }
    for (root, cwd) in [
        (None, scratch.0.join("missing")),
        (Some(scratch.0.join("missing")), PathBuf::from("/")),
    ] {
        let run = run_command(
            root.as_deref(),
            &cwd,
            &shell("exit 0"),
            &BTreeMap::new(),
            Duration::from_secs(1),
            &Protocol::Plain,
            [stdout.as_raw_fd(), stderr.as_raw_fd()],
        );
        assert!(matches!(run.outcome, ExitOutcome::SpawnError { .. }));
    }
}

/// Basic 的真实起止标记和双流大输出经捕获及 relay 原样交付，包含非文本字节。
#[test]
fn large_stdout_and_stderr_are_forwarded_without_deadlock() {
    let scratch = Scratch::new();
    let command = "printf '========== START test_bulk ==========\\n'; i=0; while [ $i -lt 8192 ]; do printf '0123456789abcdef0123456789abcdef'; printf 'fedcba9876543210fedcba9876543210' >&2; i=$((i+1)); done; printf '\\000\\377\\n========== END test_bulk ==========\\n'";
    let (run, stdout, stderr) = scratch.run_protocol(
        shell(command),
        Duration::from_secs(10),
        &Protocol::Basic {
            name: "bulk".into(),
        },
    );
    assert_eq!(run.outcome, ExitOutcome::Exited { code: 0, raw: 0 });
    assert!(run.verdict.is_success(), "{:?}", run.verdict);
    assert!(run.io_errors.is_empty(), "{:?}", run.io_errors);
    let mut expected = b"========== START test_bulk ==========\n".to_vec();
    expected.extend(b"0123456789abcdef0123456789abcdef".repeat(8192));
    expected.extend([0, 255]);
    expected.extend(b"\n========== END test_bulk ==========\n");
    assert_eq!(stdout, expected);
    assert_eq!(stderr, b"fedcba9876543210fedcba9876543210".repeat(8192));
}

/// Basic 持续产生真实输出也不能续期；忽略 TERM 的原进程组最终被 KILL 并 wait。
#[test]
fn continuous_output_obeys_one_total_deadline() {
    let scratch = Scratch::new();
    let started = Instant::now();
    let (run, stdout, _) = scratch.run_protocol(
        shell("printf '========== START test_deadline ==========\\n'; trap '' TERM; while :; do printf 'still running\\n'; done"),
        Duration::from_millis(100),
        &Protocol::Basic { name: "deadline".into() },
    );
    assert!(matches!(
        run.outcome,
        ExitOutcome::TimedOut {
            raw: Some(_),
            signal: Some(libc::SIGKILL)
        }
    ));
    assert!(started.elapsed() < Duration::from_secs(3));
    assert!(!stdout.is_empty());
}

/// 构造同一个正式 case，准备与尾部使用真实命令创建文件。
fn case(scratch: &Scratch) -> ResolvedCase {
    ResolvedCase {
        id: "runner:trace".into(),
        source: SourceLocation {
            file: PathBuf::from("host"),
            entry: 1,
            catalog: None,
            line: None,
        },
        root: "host".into(),
        cwd: scratch.0.clone(),
        command: CommandSpec::Exec {
            program: scratch.0.join("absent").to_str().unwrap().into(),
            args: Vec::new(),
        },
        env: BTreeMap::new(),
        timeout_sec: 2,
        repeat: 1,
        hooks: true,
        setup: vec![vec![
            "/bin/sh".into(),
            "-c".into(),
            "printf setup > setup".into(),
        ]],
        cleanup: vec![vec![
            "/bin/sh".into(),
            "-c".into(),
            "printf cleanup > cleanup".into(),
        ]],
        protocol: Protocol::Plain,
    }
}

/// before 已成功后真实 spawn 失败仍写 after，cleanup 失败不覆盖主结果。
#[test]
fn spawn_failure_still_runs_paired_after_and_cleanup() {
    let scratch = Scratch::new();
    let output = scratch.output("output");
    let device = scratch.0.join("hook");
    File::create(&device).unwrap();
    let mut case = case(&scratch);
    case.cleanup
        .push(vec!["/bin/sh".into(), "-c".into(), "exit 9".into()]);
    let mut calls = Vec::new();
    let record = execute_case(
        None,
        &case,
        2,
        [output.as_raw_fd(), output.as_raw_fd()],
        |before, id| {
            assert!(scratch.0.join("setup").exists());
            if !before {
                assert_eq!(fs::read(&device).unwrap(), b"before runner:trace\n");
            }
            calls.push(before);
            hook_device(&device, before, id)
        },
    );
    assert_eq!(calls, vec![true, false]);
    assert!(matches!(
        record.execution.unwrap().outcome,
        ExitOutcome::SpawnError {
            errno: Some(libc::ENOENT),
            ..
        }
    ));
    assert_eq!(record.iteration, 2);
    assert_eq!(fs::read(scratch.0.join("cleanup")).unwrap(), b"cleanup");
    assert_eq!(record.cleanup_errors.len(), 1);
}

/// after 的独立错误阻止后继执行，仍保留已取得的主 wait 结果和 cleanup。
#[test]
fn after_error_does_not_replace_primary_outcome() {
    let scratch = Scratch::new();
    let output = scratch.output("output");
    let mut case = case(&scratch);
    case.command = shell("exit 7");
    let record = execute_case(
        None,
        &case,
        1,
        [output.as_raw_fd(), output.as_raw_fd()],
        |before, _| {
            if before {
                Ok(())
            } else {
                Err(io::Error::new(io::ErrorKind::Other, "after failed"))
            }
        },
    );
    assert!(record.must_stop());
    assert_eq!(
        record.execution.as_ref().unwrap().outcome,
        ExitOutcome::Exited {
            code: 7,
            raw: 7 << 8
        }
    );
    assert_eq!(record.after_error.as_deref(), Some("after failed"));
    assert!(scratch.0.join("cleanup").exists());
}

/// 未启用 Hook 不打开控制设备，setup 失败仍执行 cleanup 且不启动主程序。
#[test]
fn setup_failure_and_disabled_hooks_have_explicit_tails() {
    let scratch = Scratch::new();
    let output = scratch.output("output");
    let mut case = case(&scratch);
    case.hooks = false;
    case.setup = vec![vec!["/bin/sh".into(), "-c".into(), "exit 3".into()]];
    let record = execute_case(
        None,
        &case,
        1,
        [output.as_raw_fd(), output.as_raw_fd()],
        |_, _| panic!("disabled hook called"),
    );
    assert!(record.execution.is_none());
    assert!(record.setup_error.is_some());
    assert!(record.must_stop());
    assert!(scratch.0.join("cleanup").exists());
}

/// 输出失败独立报告，不能把真实 exit7 改为成功或丢失 raw 状态。
#[test]
fn output_error_preserves_wait_and_shared_output_flags() {
    let scratch = Scratch::new();
    let full = OpenOptions::new().write(true).open("/dev/full").unwrap();
    // SAFETY: full 在本测试结束前保活，fcntl 不访问指针。
    let initial = unsafe { libc::fcntl(full.as_raw_fd(), libc::F_GETFL) };
    let run = run_command(
        None,
        &scratch.0,
        &shell("printf output; exit 7"),
        &BTreeMap::new(),
        Duration::from_secs(2),
        &Protocol::Basic {
            name: "output_error".into(),
        },
        [full.as_raw_fd(), full.as_raw_fd()],
    );
    assert_eq!(
        run.outcome,
        ExitOutcome::Exited {
            code: 7,
            raw: 7 << 8
        }
    );
    assert!(!run.io_errors.is_empty());
    // SAFETY: full 的拥有者仍在此作用域，读取从未被执行器修改的原标志。
    assert_eq!(
        unsafe { libc::fcntl(full.as_raw_fd(), libc::F_GETFL) },
        initial
    );
}

/// 单消息 Hook 拒绝 NUL、空白和过长输入，不在校验失败时写设备。
#[test]
fn hook_validation_happens_before_device_write() {
    let scratch = Scratch::new();
    let device = scratch.0.join("hook");
    fs::write(&device, b"original").unwrap();
    for id in ["", "bad id", "bad\0id", "bad\nid", &"a".repeat(512)] {
        assert!(hook_device(Path::new(&device), true, id).is_err());
        assert_eq!(fs::read(&device).unwrap(), b"original");
    }
}

/// 输出消费者停读也不能阻塞期限检查；未转发字节作为独立 I/O 错误保留。
#[test]
fn blocked_output_sink_does_not_suspend_deadline() {
    let scratch = Scratch::new();
    let (sink, _unread) = std::os::unix::net::UnixStream::pair().unwrap();
    let run = run_command(
        None,
        &scratch.0,
        &shell("trap '' TERM; while :; do printf '0123456789abcdef0123456789abcdef'; done"),
        &BTreeMap::new(),
        Duration::from_millis(100),
        &Protocol::Basic {
            name: "blocked".into(),
        },
        [sink.as_raw_fd(), sink.as_raw_fd()],
    );
    assert!(matches!(
        run.outcome,
        ExitOutcome::TimedOut { raw: Some(_), .. }
    ));
    assert!(!run.io_errors.is_empty());
    assert!(run.elapsed < Duration::from_secs(3));
}

/// 诊断按目标根解释绝对 symlink，既不 canonicalize 到宿主根也不改写 loader。
#[test]
fn interpreter_diagnostic_resolves_absolute_links_inside_target_root() {
    let scratch = Scratch::new();
    fs::create_dir(scratch.0.join("bin")).unwrap();
    fs::create_dir(scratch.0.join("lib")).unwrap();
    fs::write(scratch.0.join("lib/loader"), b"supplied loader bytes").unwrap();
    std::os::unix::fs::symlink("/lib/loader", scratch.0.join("lib/alias")).unwrap();
    let interp = b"/lib/alias\0";
    let mut elf = vec![0u8; 120];
    elf[..6].copy_from_slice(b"\x7fELF\x02\x01");
    elf[32..40].copy_from_slice(&64u64.to_le_bytes());
    elf[54..56].copy_from_slice(&56u16.to_le_bytes());
    elf[56..58].copy_from_slice(&1u16.to_le_bytes());
    elf[64..68].copy_from_slice(&3u32.to_le_bytes());
    elf[72..80].copy_from_slice(&120u64.to_le_bytes());
    elf[96..104].copy_from_slice(&(interp.len() as u64).to_le_bytes());
    elf.extend(interp);
    fs::write(scratch.0.join("bin/app"), elf).unwrap();
    std::os::unix::fs::symlink("/bin/app", scratch.0.join("application")).unwrap();
    let root = contest_run::model::RootSpec {
        path: scratch.0.clone(),
        cwd: "/".into(),
        env: BTreeMap::new(),
        mounts: Vec::new(),
        prepare: Vec::new(),
    };
    let diagnostic = contest_run::runtime::diagnose(
        &root,
        Path::new("/"),
        &CommandSpec::Exec {
            program: "/application".into(),
            args: Vec::new(),
        },
    );
    assert!(
        diagnostic.contains("present in target root"),
        "{diagnostic}"
    );
    assert_eq!(
        fs::read(scratch.0.join("lib/loader")).unwrap(),
        b"supplied loader bytes"
    );
}

/// 独立 worker 的 stdin/stdout 共用真实 OFD，直接输出和 Basic relay 都须保留阻塞式延迟读取。
#[test]
fn shared_console_ofd_preserves_blocking_stdin() {
    use std::io::{BufRead, Read, Write};
    use std::os::fd::OwnedFd;
    use std::os::unix::net::UnixStream;
    use std::process::{Command, Stdio};

    const TEST: &str = "shared_console_ofd_preserves_blocking_stdin";
    const ROLE: &str = "CONTEST_F4_STDIN_ROLE";
    const MODE: &str = "CONTEST_F4_STDIN_MODE";
    const GATE: &str = "CONTEST_F4_STDIN_GATE";
    const READY: &str = "F4 stdin probe ready";

    match std::env::var(ROLE).as_deref() {
        Ok("consumer") => {
            println!("\n========== START test_stdin ==========");
            println!("{READY}");
            io::stdout().flush().unwrap();
            let gate = PathBuf::from(std::env::var_os(GATE).unwrap());
            let deadline = Instant::now() + Duration::from_secs(2);
            while !gate.exists() {
                assert!(
                    Instant::now() < deadline,
                    "parent did not observe pump handshake"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
            // SAFETY: stdin 是该独立进程真实继承的描述符，fcntl 只读取当前 OFD 状态。
            let flags = unsafe { libc::fcntl(libc::STDIN_FILENO, libc::F_GETFL) };
            assert!(
                flags != -1,
                "child stdin is invalid: {}",
                io::Error::last_os_error()
            );
            assert_eq!(
                flags & libc::O_NONBLOCK,
                0,
                "runner changed inherited child stdin to nonblocking while pumping stdout"
            );
            let mut input = String::new();
            io::stdin().read_line(&mut input).unwrap();
            assert_eq!(input, "delayed stdin data\n");
            println!("========== END test_stdin ==========");
        }
        Ok("worker") => {
            let scratch = Scratch::new();
            let spec = CommandSpec::Exec {
                program: std::env::current_exe()
                    .unwrap()
                    .into_os_string()
                    .into_string()
                    .unwrap(),
                args: vec!["--exact".into(), TEST.into(), "--nocapture".into()],
            };
            let env = BTreeMap::from([(ROLE.into(), "consumer".into())]);
            let protocol = if std::env::var(MODE).unwrap() == "basic" {
                Protocol::Basic {
                    name: "stdin".into(),
                }
            } else {
                Protocol::Plain
            };
            let run = run_command(
                None,
                &scratch.0,
                &spec,
                &env,
                Duration::from_secs(2),
                &protocol,
                [libc::STDOUT_FILENO, libc::STDERR_FILENO],
            );
            assert!(run.io_errors.is_empty(), "{:?}", run.io_errors);
            assert_eq!(run.outcome, ExitOutcome::Exited { code: 0, raw: 0 });
            assert!(run.verdict.is_success(), "{:?}", run.verdict);
        }
        _ => {
            let _exclusive = exclusive_execution();
            for mode in ["plain", "basic"] {
                let scratch = Scratch::new();
                let gate = scratch.0.join("pump-forwarded-handshake");
                let (console, peer) = UnixStream::pair().unwrap();
                peer.set_read_timeout(Some(Duration::from_secs(4))).unwrap();
                let mut sender = peer.try_clone().unwrap();
                let stdin: OwnedFd = console.try_clone().unwrap().into();
                let stdout: OwnedFd = console.try_clone().unwrap().into();
                let mut worker = Command::new(std::env::current_exe().unwrap())
                    .args(["--exact", TEST, "--nocapture"])
                    .env(ROLE, "worker")
                    .env(MODE, mode)
                    .env(GATE, &gate)
                    .stdin(Stdio::from(stdin))
                    .stdout(Stdio::from(stdout))
                    .stderr(Stdio::inherit())
                    .spawn()
                    .unwrap();
                drop(console);
                let mut reader = io::BufReader::new(peer);
                let mut transcript = String::new();
                let observed = loop {
                    let mut line = String::new();
                    match reader.read_line(&mut line) {
                        Ok(0) => break false,
                        Ok(_) => {
                            let ready = line.contains(READY);
                            transcript.push_str(&line);
                            if ready {
                                break true;
                            }
                        }
                        Err(error) => {
                            transcript.push_str(&format!("console read: {error}"));
                            break false;
                        }
                    }
                };
                let sent = if observed {
                    fs::write(&gate, b"stdout forwarded by pump").unwrap();
                    std::thread::sleep(Duration::from_millis(100));
                    sender.write_all(b"delayed stdin data\n")
                } else {
                    Ok(())
                };
                let remaining = reader.read_to_string(&mut transcript);
                let status = worker.wait().unwrap();
                assert!(
                    observed,
                    "{mode} consumer handshake was not forwarded: {transcript}"
                );
                assert!(
                    sent.is_ok(),
                    "{mode} delayed stdin write failed: {sent:?}; {transcript}"
                );
                assert!(
                    remaining.is_ok(),
                    "{mode} console tail read failed: {remaining:?}; {transcript}"
                );
                assert!(
                    status.success(),
                    "{mode} shared-OFD worker failed: {status}; {transcript}"
                );
            }
        }
    }
}

/// 主 child 已退出且全部字节已交 helper 后，输出阻塞只产生独立清理错误，不改主 wait 状态。
#[test]
fn relay_drain_deadline_preserves_completed_child_status() {
    use std::io::Write;

    let scratch = Scratch::new();
    let (mut sink, _unread) = std::os::unix::net::UnixStream::pair().unwrap();
    sink.set_nonblocking(true).unwrap();
    let fill = [0u8; 8192];
    loop {
        match sink.write(&fill) {
            Ok(0) => panic!("socket stopped making progress without EAGAIN"),
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
            Err(error) => panic!("fill output socket: {error}"),
        }
    }
    sink.set_nonblocking(false).unwrap();
    let run = run_command(
        None,
        &scratch.0,
        &shell("printf '========== START test_tail ==========\\n========== END test_tail ==========\\n'; exit 7"),
        &BTreeMap::new(),
        Duration::from_millis(300),
        &Protocol::Basic { name: "tail".into() },
        [sink.as_raw_fd(), sink.as_raw_fd()],
    );
    assert_eq!(
        run.outcome,
        ExitOutcome::Exited {
            code: 7,
            raw: 7 << 8
        }
    );
    assert!(
        run.io_errors
            .iter()
            .any(|error| error.contains("output relay did not drain before deadline")),
        "{:?}",
        run.io_errors
    );
    assert!(run.elapsed < Duration::from_secs(3));
}
