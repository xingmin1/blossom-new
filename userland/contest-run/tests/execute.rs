//! 比赛执行路径（`process::run_command`）、TAP 自测路径（`process::run_tap_command`）与
//! 单用例生命周期编排（`execute::execute_case`）的集成测试。
//!
//! 比赛路径直接继承本进程的 stdin/stdout/stderr，因此凡是可能产生输出的 `run_command` 用例都在
//! 被测 `sh -c` 命令内部把输出重定向到临时文件或 `/dev/null`；需要逐字节检查输出的用例走
//! `run_tap_command` 的管道与 Relay。
//!
//! # 并发契约
//! 测试框架并行运行各用例，而 `fork` 出的子进程会继承其他线程当时打开的文件描述符。因此本文件用
//! [`EXECUTION`] 把所有会派生进程的调用（`run_command`、`run_tap_command`、`run_steps`、
//! `execute_case`，以及共享控制台用例里的自举进程）串行化；锁中毒时用 `into_inner` 继续，让后续
//! 用例独立执行。

use contest_run::custom::notify_device;
use contest_run::execute::{
    execute_case as execute_real_case, run_steps as execute_real_steps, CaseRecord, StepReport,
};
use contest_run::model::{ResolvedCase, RootSpec, SourceLocation};
use contest_run::outcome::{Execution, ExitOutcome, Verdict};
use contest_run::process::{
    run_command as execute_real_command, run_tap_command as execute_real_tap_command,
};
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::{AsRawFd, RawFd};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// 为每个测试实例分配唯一临时目录编号的单调递增计数器。
static SERIAL: AtomicU64 = AtomicU64::new(0);
/// 串行化本文件中各测试函数的子进程派生操作，防止多线程并发 `fork` 导致描述符跨测试泄漏。
static EXECUTION: Mutex<()> = Mutex::new(());

/// 获取子进程执行互斥锁守卫；若前一个用例因断言 panic 导致锁中毒，则取出内部守卫继续执行。
fn exclusive_execution() -> std::sync::MutexGuard<'static, ()> {
    match EXECUTION.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// 在串行化守卫下运行比赛路径命令。
fn run_command(
    root: Option<&Path>,
    cwd: &Path,
    argv: &[String],
    env: &BTreeMap<String, String>,
    timeout: Duration,
) -> Execution {
    let _exclusive = exclusive_execution();
    execute_real_command(root, cwd, argv, env, timeout)
}

/// 在串行化守卫下运行 TAP 自测路径命令。
fn run_tap_command(
    cwd: &Path,
    argv: &[String],
    env: &BTreeMap<String, String>,
    timeout: Duration,
    outputs: [RawFd; 2],
) -> (Execution, Verdict) {
    let _exclusive = exclusive_execution();
    let (run, result) = execute_real_tap_command(cwd, argv, env, timeout, outputs);
    (run, result.verdict)
}

/// 在串行化守卫下顺序执行一组准备或清理命令，返回错误列表与进程控制状态。
fn run_steps(
    root: Option<&Path>,
    cwd: &Path,
    commands: &[Vec<String>],
    env: &BTreeMap<String, String>,
    timeout: Duration,
    stop_on_error: bool,
) -> StepReport {
    let _exclusive = exclusive_execution();
    execute_real_steps(root, cwd, commands, env, timeout, stop_on_error)
}

/// 在串行化守卫下运行一个用例的完整生命周期。
fn execute_case(
    root: Option<&Path>,
    case: &ResolvedCase,
    setup_timeout: Duration,
    out: &mut impl Write,
    notify: impl FnMut(bool, &str) -> io::Result<()>,
) -> CaseRecord {
    let _exclusive = exclusive_execution();
    execute_real_case(root, case, setup_timeout, out, notify)
}

/// 测试专用的临时工作目录守卫：离开作用域时递归删除本实例创建的目录。
struct Scratch(PathBuf);

impl Scratch {
    /// 创建并返回一个全新的测试专属临时目录。
    fn new() -> Self {
        let path = std::env::temp_dir().join(format!(
            "contest-execute-{}-{}",
            std::process::id(),
            SERIAL.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    /// 在临时目录下创建名为 `name` 的读写输出文件。
    fn output(&self, name: &str) -> File {
        OpenOptions::new()
            .create_new(true)
            .read(true)
            .write(true)
            .open(self.0.join(name))
            .unwrap()
    }

    /// 以两条输出文件运行 TAP 路径命令，返回进程事实、判定与两路已转发字节。
    fn run_tap(
        &self,
        argv: &[String],
        timeout: Duration,
    ) -> (Execution, Verdict, Vec<u8>, Vec<u8>) {
        let stdout = self.output("stdout");
        let stderr = self.output("stderr");
        let (run, verdict) = run_tap_command(
            &self.0,
            argv,
            &BTreeMap::new(),
            timeout,
            [stdout.as_raw_fd(), stderr.as_raw_fd()],
        );
        (
            run,
            verdict,
            fs::read(self.0.join("stdout")).unwrap(),
            fs::read(self.0.join("stderr")).unwrap(),
        )
    }
}

impl Drop for Scratch {
    /// 测试结束时递归删除本实例创建的临时目录及其内部文件。
    fn drop(&mut self) {
        fs::remove_dir_all(&self.0).unwrap();
    }
}

/// 构造通过 `/bin/sh -c` 执行指定文本的 argv。
fn shell(text: &str) -> Vec<String> {
    vec!["/bin/sh".into(), "-c".into(), text.into()]
}

/// 构造带 setup、cleanup 与 before/after 文本的基准用例；主命令默认指向不存在的程序，用于覆盖启动失败路径。
///
/// setup 与 cleanup 各向 cwd 下的 `order` 文件追加一行，供生命周期顺序断言使用。
fn case(scratch: &Scratch) -> ResolvedCase {
    ResolvedCase {
        id: "runner:trace".into(),
        group: "trace".into(),
        root: "host".into(),
        name: "trace".into(),
        catalog_command: None,
        argv: vec![scratch.0.join("absent").to_str().unwrap().into()],
        cwd: scratch.0.clone(),
        env: BTreeMap::new(),
        timeout_sec: 2,
        repeat: 1,
        setup: vec![shell("printf 'setup\\n' >> order")],
        cleanup: vec![shell("printf 'cleanup\\n' >> order")],
        before: "BEFORE\n".into(),
        after: "AFTER {status}\n".into(),
        source: SourceLocation {
            file: PathBuf::from("host"),
            entry: "group 1 step 1".into(),
            catalog: None,
            line: None,
        },
    }
}

/// 构造正常退出的进程事实。
fn exited(code: i32) -> ExitOutcome {
    ExitOutcome::Exited {
        code,
        raw: code << 8,
    }
}

/// 构造只含退出分类、没有附加 I/O 错误的执行记录。
fn execution(outcome: ExitOutcome) -> Execution {
    Execution {
        outcome,
        io_errors: Vec::new(),
        elapsed: Duration::ZERO,
    }
}

/// 记录通知调用与协议文本写出顺序的共享事件日志。
#[derive(Clone)]
struct Journal(Rc<RefCell<Vec<String>>>);

impl Journal {
    /// 创建空日志。
    fn new() -> Self {
        Self(Rc::new(RefCell::new(Vec::new())))
    }

    /// 追加一条事件。
    fn push(&self, event: String) {
        self.0.borrow_mut().push(event);
    }

    /// 取出已记录的全部事件。
    fn events(&self) -> Vec<String> {
        self.0.borrow().clone()
    }

    /// 读取 `path` 当前内容并把换行折叠成逗号，用于把阶段顺序固定进事件文本。
    fn snapshot(path: &Path) -> String {
        fs::read_to_string(path)
            .unwrap_or_default()
            .trim_end()
            .replace('\n', ",")
    }
}

/// 接收用例 before/after 文本的写入器：保留原始字节并记录每次写入的边界文件内容。
struct TextRecorder {
    /// 共享事件日志。
    journal: Journal,
    /// setup 与 cleanup 追加标记的边界文件路径。
    order: PathBuf,
    /// 收到的全部协议文本字节。
    bytes: Vec<u8>,
}

impl TextRecorder {
    /// 创建记录器：文本同时进入 `bytes` 与事件日志。
    fn new(journal: Journal, order: PathBuf) -> Self {
        Self {
            journal,
            order,
            bytes: Vec::new(),
        }
    }
}

impl Write for TextRecorder {
    /// 记录一次协议文本写入及当时的边界文件内容。
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.journal.push(format!(
            "text:{}|order={}",
            String::from_utf8_lossy(bytes).trim_end(),
            Journal::snapshot(&self.order)
        ));
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    /// 实际写出由日志完成，无需额外刷新。
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// 创建一对 UnixStream 并把写端发送缓冲区填满到不再有进展。
///
/// 返回（已满的阻塞写端, 从未读取的对端）；用于构造无法排空的输出接收端。
fn filled_sink() -> (
    std::os::unix::net::UnixStream,
    std::os::unix::net::UnixStream,
) {
    let (mut sink, unread) = std::os::unix::net::UnixStream::pair().unwrap();
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
    (sink, unread)
}

/// 在 2 秒内轮询 `/proc/<pid>/stat`，直到进程不存在或已处于僵尸态；超时则判定收尾失败。
fn wait_for_group_member_gone(pid: i32) {
    let stat = PathBuf::from(format!("/proc/{pid}/stat"));
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match fs::read_to_string(&stat) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return,
            Err(error) => panic!("read {}: {error}", stat.display()),
            Ok(text) => {
                let state = text
                    .rsplit_once(')')
                    .unwrap()
                    .1
                    .split_whitespace()
                    .next()
                    .unwrap();
                if state == "Z" {
                    return;
                }
            }
        }
        assert!(
            Instant::now() < deadline,
            "background process {pid} still running after group kill"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// 验证 `shell_status` 对照表：正常退出原值、信号 128+signal、超时 124、启动失败按 errno 归并、收尾失败 125。
///
/// 真实执行必须区分“程序自己返回 127”和“exec 找不到程序（ENOENT）”；二者 `shell_status` 都是 127，
/// 但 `ExitOutcome` 不同。其余 errno 归并用构造的退出分类检查。
#[test]
fn shell_status_table_covers_exit_signal_timeout_and_spawn_errors() {
    let scratch = Scratch::new();
    for code in [0, 7, 127] {
        let run = run_command(
            None,
            &scratch.0,
            &shell(&format!("exit {code}")),
            &BTreeMap::new(),
            Duration::from_secs(2),
        );
        assert_eq!(
            run.outcome,
            exited(code),
            "exit {code} must stay an ordinary exit"
        );
        assert_eq!(run.outcome.shell_status(), code);
        assert!(run.io_errors.is_empty(), "{:?}", run.io_errors);
    }
    let absent = vec![scratch.0.join("absent").to_str().unwrap().to_string()];
    let run = run_command(
        None,
        &scratch.0,
        &absent,
        &BTreeMap::new(),
        Duration::from_secs(2),
    );
    assert!(matches!(
        run.outcome,
        ExitOutcome::SpawnError {
            errno: Some(libc::ENOENT),
            ..
        }
    ));
    assert_eq!(run.outcome.shell_status(), 127);
    let run = run_command(
        None,
        &scratch.0,
        &shell("kill -TERM $$"),
        &BTreeMap::new(),
        Duration::from_secs(2),
    );
    assert!(matches!(
        run.outcome,
        ExitOutcome::Signaled {
            signal: libc::SIGTERM,
            ..
        }
    ));
    assert_eq!(run.outcome.shell_status(), 143);
    let run = run_command(
        None,
        &scratch.0,
        &shell("sleep 30"),
        &BTreeMap::new(),
        Duration::from_millis(100),
    );
    assert!(matches!(run.outcome, ExitOutcome::TimedOut { .. }));
    assert_eq!(run.outcome.shell_status(), 124);
    assert!(run.elapsed < Duration::from_secs(3));
    for (errno, expected) in [
        (Some(libc::EACCES), 126),
        (Some(libc::ENOEXEC), 126),
        (Some(libc::EBUSY), 125),
        (None, 125),
    ] {
        let outcome = ExitOutcome::SpawnError {
            errno,
            message: "spawn failed".into(),
        };
        assert_eq!(outcome.shell_status(), expected, "errno {errno:?}");
    }
    let outcome = ExitOutcome::IoError {
        errno: Some(libc::EIO),
        message: "wait failed".into(),
        raw: None,
    };
    assert_eq!(outcome.shell_status(), 125);
}

/// 验证比赛路径清空父进程环境，只保留配置给出的变量。
///
/// 父进程设置唯一变量后运行 `sh -c`；子进程用 `${VAR-unset}` 读取并写入文件。若清空失效，读到的会是
/// 父进程值而不是 `unset`。变量修改与恢复都在串行化守卫内完成。
#[test]
fn contest_path_clears_parent_environment() {
    const LEAK: &str = "CONTEST_EXECUTE_PARENT_ONLY";
    let scratch = Scratch::new();
    let argv = shell(&format!("printf '%s' \"${{{LEAK}-unset}}\" > seen"));
    let _exclusive = exclusive_execution();
    std::env::set_var(LEAK, "leaked");
    let run = execute_real_command(
        None,
        &scratch.0,
        &argv,
        &BTreeMap::new(),
        Duration::from_secs(2),
    );
    std::env::remove_var(LEAK);
    assert_eq!(run.outcome, exited(0));
    assert_eq!(fs::read(scratch.0.join("seen")).unwrap(), b"unset");
}

/// 验证 cwd、配置环境变量与 argv 边界值原样到达子进程。
///
/// 含空格的环境值、空字符串参数、含空格参数与 UTF-8 中文都必须按字节传递，不做二次分词或编码转换；
/// 输出由被测命令自己重定向到文件，不污染测试进程输出。
#[test]
fn contest_path_passes_cwd_environment_and_argument_bytes() {
    let scratch = Scratch::new();
    let argv = vec![
        "/bin/sh".into(),
        "-c".into(),
        "printf '%s\\000' \"$PWD\" \"$CHECK\" \"$1\" \"$2\" \"$3\" > out".into(),
        "argv0".into(),
        "".into(),
        "value with spaces".into(),
        "中文".into(),
    ];
    let env = BTreeMap::from([("CHECK".into(), "env with spaces".into())]);
    let run = run_command(None, &scratch.0, &argv, &env, Duration::from_secs(2));
    assert_eq!(run.outcome, exited(0));
    let expected = format!(
        "{}\0env with spaces\0\0value with spaces\0中文\0",
        scratch.0.display()
    );
    assert_eq!(
        fs::read(scratch.0.join("out")).unwrap(),
        expected.as_bytes()
    );
}

/// 验证 argv 内嵌 NUL、空 argv/程序名与不存在的 cwd/root 都在启动阶段成为 SpawnError。
#[test]
fn nul_and_invalid_root_or_cwd_remain_spawn_errors() {
    let scratch = Scratch::new();
    let variants = vec![
        vec!["/bin/true".to_string(), "bad\0arg".into()],
        vec!["/bin/tru\0e".to_string()],
        shell("bad\0command"),
        Vec::new(),
    ];
    for argv in variants {
        let run = run_command(
            None,
            &scratch.0,
            &argv,
            &BTreeMap::new(),
            Duration::from_secs(1),
        );
        assert!(
            matches!(run.outcome, ExitOutcome::SpawnError { .. }),
            "{argv:?}"
        );
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
        );
        assert!(
            matches!(run.outcome, ExitOutcome::SpawnError { .. }),
            "{cwd:?}"
        );
    }
}

/// 验证 TAP 路径在双流各 256 KiB 交错输出下不死锁，且两路字节逐字节转发。
///
/// 两侧数据远超管道容量；标准输出末尾追加合法 TAP 计划，判定必须为 Pass。
#[test]
fn large_stdout_and_stderr_are_forwarded_without_deadlock() {
    let scratch = Scratch::new();
    let command = "i=0; while [ $i -lt 8192 ]; do printf '0123456789abcdef0123456789abcdef\\n'; printf 'fedcba9876543210fedcba9876543210\\n' >&2; i=$((i+1)); done; printf '\\000\\377\\n1..1\\nok 1 bulk\\n'";
    let (run, verdict, stdout, stderr) = scratch.run_tap(&shell(command), Duration::from_secs(10));
    assert_eq!(run.outcome, exited(0));
    assert!(run.io_errors.is_empty(), "{:?}", run.io_errors);
    assert_eq!(verdict, Verdict::Pass);
    let mut expected = b"0123456789abcdef0123456789abcdef\n".repeat(8192);
    expected.extend([0, 255]);
    expected.extend(b"\n1..1\nok 1 bulk\n");
    assert_eq!(stdout, expected);
    assert_eq!(stderr, b"fedcba9876543210fedcba9876543210\n".repeat(8192));
}

/// 验证 TAP 路径的持续输出不能续期总期限：忽略 SIGTERM 的子进程必须被 SIGKILL 收尾，已转发部分输出保留。
#[test]
fn tap_output_flow_obeys_one_total_deadline() {
    let scratch = Scratch::new();
    let (run, verdict, stdout, _) = scratch.run_tap(
        &shell("trap '' TERM; while :; do printf 'still running\\n'; done"),
        Duration::from_millis(100),
    );
    assert!(
        matches!(
            run.outcome,
            ExitOutcome::TimedOut {
                signal: Some(libc::SIGKILL),
                ..
            }
        ),
        "{:?}",
        run.outcome
    );
    assert_eq!(verdict, Verdict::TimedOut);
    assert!(run.elapsed < Duration::from_secs(3));
    assert!(
        !stdout.is_empty(),
        "partial TAP path output must survive timeout"
    );
}

/// 验证比赛路径同样在固定期限后升级 SIGKILL，不等待忽略 SIGTERM 的组首自行退出。
#[test]
fn contest_path_obeys_one_total_deadline() {
    let scratch = Scratch::new();
    let run = run_command(
        None,
        &scratch.0,
        &shell("trap '' TERM; while :; do :; done"),
        &BTreeMap::new(),
        Duration::from_millis(100),
    );
    assert!(
        matches!(
            run.outcome,
            ExitOutcome::TimedOut {
                signal: Some(libc::SIGKILL),
                ..
            }
        ),
        "{:?}",
        run.outcome
    );
    assert!(run.elapsed < Duration::from_secs(3));
}

/// 验证组首正常退出后，`run_command` 返回前会清理同组后台进程。
///
/// `sh -c 'sleep 30 & echo $! > pid; exit 3'` 留下一个后台 sleep：返回时该 PID 必须已不存在或成为僵尸，
/// 总耗时远小于 sleep 的 30 秒；outcome 仍是组首自己的退出码 3。
#[test]
fn contest_path_kills_background_group_after_leader_exit() {
    let scratch = Scratch::new();
    let run = run_command(
        None,
        &scratch.0,
        &shell("sleep 30 & echo $! > pid; exit 3"),
        &BTreeMap::new(),
        Duration::from_secs(5),
    );
    assert_eq!(run.outcome, exited(3));
    assert!(run.elapsed < Duration::from_secs(3));
    let pid: i32 = fs::read_to_string(scratch.0.join("pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    wait_for_group_member_gone(pid);
}

/// 验证组首在超时 SIGTERM 后退出时仍按 TimedOut 返回，并且同组后台进程同样被收尾。
#[test]
fn contest_path_timeout_still_kills_background_group() {
    let scratch = Scratch::new();
    let run = run_command(
        None,
        &scratch.0,
        &shell("sleep 30 & echo $! > pid; sleep 30"),
        &BTreeMap::new(),
        Duration::from_millis(100),
    );
    assert!(
        matches!(run.outcome, ExitOutcome::TimedOut { .. }),
        "{:?}",
        run.outcome
    );
    assert!(run.elapsed < Duration::from_secs(3));
    let pid: i32 = fs::read_to_string(scratch.0.join("pid"))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    wait_for_group_member_gone(pid);
}

/// 验证 `CaseRecord` 三个判定的真值表：进程控制失败、会话停止条件与执行错误各自独立。
///
/// 正常非零退出（1、32）不是执行错误；setup 错误只计入执行错误；通知、清理、输出错误与进程控制失败
/// （主命令或 setup 的 unquiesced）计入会话停止；主命令未运行或不是正常退出计入执行错误。
#[test]
fn case_record_truth_table_separates_stop_and_failure() {
    let normal = || CaseRecord {
        execution: Some(execution(exited(0))),
        ..CaseRecord::default()
    };
    let record = normal();
    assert!(!record.unquiesced() && !record.must_stop() && !record.failed());
    for code in [1, 32] {
        let record = CaseRecord {
            execution: Some(execution(exited(code))),
            ..CaseRecord::default()
        };
        assert!(
            !record.unquiesced() && !record.must_stop(),
            "exit {code} must not stop the session"
        );
        assert!(!record.failed(), "exit {code} is not an execution error");
    }
    for outcome in [
        ExitOutcome::TimedOut {
            raw: None,
            signal: None,
        },
        ExitOutcome::Signaled {
            signal: libc::SIGTERM,
            raw: libc::SIGTERM,
        },
        ExitOutcome::SpawnError {
            errno: Some(libc::ENOENT),
            message: "absent".into(),
        },
    ] {
        let record = CaseRecord {
            execution: Some(execution(outcome)),
            ..CaseRecord::default()
        };
        assert!(
            !record.unquiesced() && !record.must_stop(),
            "{:?}",
            record.execution
        );
        assert!(record.failed(), "{:?}", record.execution);
    }
    let mut record = normal();
    record
        .execution
        .as_mut()
        .unwrap()
        .io_errors
        .push("signal group: EPERM".into());
    assert!(record.unquiesced() && record.must_stop() && record.failed());
    let record = CaseRecord {
        setup_unquiesced: true,
        ..CaseRecord::default()
    };
    assert!(record.unquiesced() && record.must_stop() && record.failed());
    let record = CaseRecord {
        execution: Some(execution(ExitOutcome::IoError {
            errno: Some(libc::EIO),
            message: "wait failed".into(),
            raw: None,
        })),
        ..CaseRecord::default()
    };
    assert!(record.unquiesced() && record.must_stop() && record.failed());
    for record in [
        CaseRecord {
            before_error: Some("before".into()),
            ..normal()
        },
        CaseRecord {
            after_error: Some("after".into()),
            ..normal()
        },
        CaseRecord {
            cleanup_errors: vec!["cleanup".into()],
            ..normal()
        },
        CaseRecord {
            output_errors: vec!["output".into()],
            ..normal()
        },
    ] {
        assert!(!record.unquiesced());
        assert!(record.must_stop() && record.failed());
    }
    let record = CaseRecord {
        setup_errors: vec!["setup".into()],
        ..normal()
    };
    assert!(
        !record.must_stop(),
        "setup failure alone must not stop the session"
    );
    assert!(record.failed());
    let record = CaseRecord::default();
    assert!(!record.unquiesced() && !record.must_stop() && record.failed());
}

/// 验证一次正常执行的阶段顺序固定为 通知 → setup → before 文本 → 主命令 → after 文本 → cleanup → 通知。
///
/// 事件日志在写入协议文本时同时记录边界文件的当前内容，从而区分各阶段，而不是只看最终文件。
#[test]
fn lifecycle_runs_notify_setup_texts_main_cleanup_and_notify() {
    let scratch = Scratch::new();
    let order = scratch.0.join("order");
    let journal = Journal::new();
    let mut case = case(&scratch);
    case.argv = shell("printf 'main\\n' >> order");
    let mut out = TextRecorder::new(journal.clone(), order.clone());
    let record = execute_case(
        None,
        &case,
        Duration::from_secs(1),
        &mut out,
        |before, id| {
            journal.push(format!(
                "notify:{before}|{id}|order={}",
                Journal::snapshot(&order)
            ));
            Ok(())
        },
    );
    assert!(
        record.before_error.is_none() && record.setup_errors.is_empty(),
        "{record:?}"
    );
    assert!(
        record.output_errors.is_empty() && record.cleanup_errors.is_empty(),
        "{record:?}"
    );
    assert!(record.after_error.is_none(), "{record:?}");
    assert_eq!(record.execution.unwrap().outcome, exited(0));
    assert_eq!(
        journal.events(),
        vec![
            "notify:true|runner:trace|order=".to_string(),
            "text:BEFORE|order=setup".to_string(),
            "text:AFTER 0|order=setup,main".to_string(),
            "notify:false|runner:trace|order=setup,main,cleanup".to_string(),
        ]
    );
    assert_eq!(out.bytes.as_slice(), b"BEFORE\nAFTER 0\n");
    assert_eq!(
        fs::read_to_string(&order).unwrap(),
        "setup\nmain\ncleanup\n"
    );
}

/// 验证 before 通知失败立即结束本轮：不执行 setup、不写文本、不运行主命令、不 cleanup、不发 after。
#[test]
fn before_notify_failure_stops_before_setup_and_cleanup() {
    let scratch = Scratch::new();
    let case = case(&scratch);
    let mut calls = Vec::new();
    let mut out = Vec::new();
    let record = execute_case(
        None,
        &case,
        Duration::from_secs(1),
        &mut out,
        |before, _| {
            calls.push(before);
            Err(io::Error::new(io::ErrorKind::Other, "before failed"))
        },
    );
    assert_eq!(calls, vec![true]);
    assert_eq!(record.before_error.as_deref(), Some("before failed"));
    assert!(record.execution.is_none() && record.setup_errors.is_empty());
    assert!(record.cleanup_errors.is_empty() && record.after_error.is_none());
    assert!(out.is_empty());
    assert!(
        !scratch.0.join("order").exists(),
        "neither setup nor cleanup may run"
    );
    assert!(record.must_stop() && record.failed());
}

/// 验证 setup 失败时不写 before 文本、不运行主命令，但仍执行 cleanup 并发送 after 通知。
///
/// setup 错误只计入执行错误（`failed`），不是会话停止条件（`must_stop`）。
#[test]
fn setup_failure_skips_main_but_still_cleans_and_notifies_after() {
    let scratch = Scratch::new();
    let order = scratch.0.join("order");
    let journal = Journal::new();
    let mut case = case(&scratch);
    case.setup = vec![shell("exit 3")];
    let mut out = TextRecorder::new(journal.clone(), order.clone());
    let record = execute_case(
        None,
        &case,
        Duration::from_secs(1),
        &mut out,
        |before, id| {
            journal.push(format!("notify:{before}|{id}"));
            Ok(())
        },
    );
    assert_eq!(record.setup_errors.len(), 1);
    assert!(record.execution.is_none());
    assert!(record.before_error.is_none() && record.output_errors.is_empty());
    assert!(record.after_error.is_none() && record.cleanup_errors.is_empty());
    assert_eq!(
        journal.events(),
        vec!["notify:true|runner:trace", "notify:false|runner:trace"]
    );
    assert_eq!(fs::read_to_string(&order).unwrap(), "cleanup\n");
    assert!(
        out.bytes.is_empty(),
        "setup failure must not write protocol text"
    );
    assert!(!record.must_stop());
    assert!(record.failed());
}

/// 验证主程序启动失败时 after 文本仍以 `{status}`=127 输出，且 cleanup 与 after 通知都执行。
///
/// cleanup 中的失败命令单独记录，不覆盖主程序的 SpawnError；后续 cleanup 命令仍会尝试。
#[test]
fn spawn_failure_writes_status_127_and_still_cleans_and_notifies_after() {
    let scratch = Scratch::new();
    let order = scratch.0.join("order");
    let journal = Journal::new();
    let mut case = case(&scratch);
    case.cleanup = vec![
        shell("printf 'cleanup\\n' >> order; exit 9"),
        shell("touch cleaned"),
    ];
    let mut out = TextRecorder::new(journal.clone(), order.clone());
    let record = execute_case(
        None,
        &case,
        Duration::from_secs(1),
        &mut out,
        |before, id| {
            journal.push(format!(
                "notify:{before}|{id}|order={}",
                Journal::snapshot(&order)
            ));
            Ok(())
        },
    );
    assert!(matches!(
        &record.execution.as_ref().unwrap().outcome,
        ExitOutcome::SpawnError {
            errno: Some(libc::ENOENT),
            ..
        }
    ));
    assert_eq!(record.cleanup_errors.len(), 1);
    assert!(scratch.0.join("cleaned").exists());
    assert!(record.after_error.is_none() && record.output_errors.is_empty());
    assert_eq!(
        journal.events(),
        vec![
            "notify:true|runner:trace|order=".to_string(),
            "text:BEFORE|order=setup".to_string(),
            "text:AFTER 127|order=setup".to_string(),
            "notify:false|runner:trace|order=setup,cleanup".to_string(),
        ]
    );
}

/// 验证主命令超时后 after 文本使用 `{status}`=124，并且 cleanup 与 after 通知照常执行。
#[test]
fn main_timeout_writes_status_124_and_finishes_lifecycle() {
    let scratch = Scratch::new();
    let order = scratch.0.join("order");
    let journal = Journal::new();
    let mut case = case(&scratch);
    case.argv = shell("sleep 30");
    case.before = String::new();
    case.timeout_sec = 1;
    let mut out = TextRecorder::new(journal.clone(), order.clone());
    let record = execute_case(
        None,
        &case,
        Duration::from_secs(1),
        &mut out,
        |before, id| {
            journal.push(format!("notify:{before}|{id}"));
            Ok(())
        },
    );
    let execution = record.execution.as_ref().unwrap();
    assert!(matches!(&execution.outcome, ExitOutcome::TimedOut { .. }));
    assert!(execution.elapsed < Duration::from_secs(3));
    assert_eq!(
        journal.events(),
        vec![
            "notify:true|runner:trace".to_string(),
            "text:AFTER 124|order=setup".to_string(),
            "notify:false|runner:trace".to_string(),
        ]
    );
    assert_eq!(fs::read_to_string(&order).unwrap(), "setup\ncleanup\n");
    assert!(!record.must_stop() && record.failed());
}

/// 验证 cleanup 失败逐项记录并保留主结果，after 通知仍然发送，后续 cleanup 命令继续尝试。
#[test]
fn cleanup_failure_is_recorded_and_after_notify_still_runs() {
    let scratch = Scratch::new();
    let journal = Journal::new();
    let mut case = case(&scratch);
    case.argv = shell("exit 0");
    case.before = String::new();
    case.cleanup = vec![shell("exit 9"), shell("touch cleaned")];
    let mut out = Vec::new();
    let record = execute_case(
        None,
        &case,
        Duration::from_secs(1),
        &mut out,
        |before, id| {
            journal.push(format!("notify:{before}|{id}"));
            Ok(())
        },
    );
    assert_eq!(record.execution.as_ref().unwrap().outcome, exited(0));
    assert_eq!(record.cleanup_errors.len(), 1);
    assert!(
        scratch.0.join("cleaned").exists(),
        "later cleanup commands must still run"
    );
    assert_eq!(
        journal.events(),
        vec!["notify:true|runner:trace", "notify:false|runner:trace"]
    );
    assert_eq!(out.as_slice(), b"AFTER 0\n");
    assert!(record.must_stop() && record.failed());
}

/// 验证清理命令丢失等待状态后停止剩余清理且不发送 after 通知。
///
/// 独立测试进程在主命令结束后忽略 SIGCHLD，使清理命令被自动回收并触发真实 ECHILD；
/// 信号处置只影响该子进程，不干扰并行运行的其他测试。
#[test]
fn cleanup_control_loss_stops_commands_and_skips_after_notification() {
    const TEST: &str = "cleanup_control_loss_stops_commands_and_skips_after_notification";
    const WORKER: &str = "CONTEST_CLEANUP_WAIT_FAILURE";

    if std::env::var_os(WORKER).is_none() {
        let _exclusive = exclusive_execution();
        let worker = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST, "--test-threads=1"])
            .env(WORKER, "1")
            .output()
            .unwrap();
        assert!(
            worker.status.success(),
            "cleanup wait-failure worker failed: {}\n{}\n{}",
            worker.status,
            String::from_utf8_lossy(&worker.stdout),
            String::from_utf8_lossy(&worker.stderr)
        );
        return;
    }

    /// 在 after 文本刷出时改变独立测试进程的 SIGCHLD 处置，使下一条清理命令无法等待。
    struct IgnoreChildExit;

    impl Write for IgnoreChildExit {
        /// 消费主命令结束后的协议文本，信号处置留到随后 flush 时修改。
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            Ok(bytes.len())
        }

        /// 让内核自动回收随后退出的子进程，以覆盖清理命令的真实 wait 失败路径。
        fn flush(&mut self) -> io::Result<()> {
            // SAFETY: SIGCHLD 和 SIG_IGN 是有效的信号与处置常量；当前是只运行本用例的独立进程，
            // 没有仍需等待的前序子进程，处置变化不会影响父测试进程或其他用例。
            if unsafe { libc::signal(libc::SIGCHLD, libc::SIG_IGN) } == libc::SIG_ERR {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        }
    }

    let scratch = Scratch::new();
    let mut case = case(&scratch);
    case.argv = shell("exit 0");
    case.before.clear();
    case.cleanup = vec![shell("exit 0"), shell("touch later-cleanup")];
    let mut notifications = Vec::new();
    let record = execute_case(
        None,
        &case,
        Duration::from_secs(1),
        &mut IgnoreChildExit,
        |before, _| {
            notifications.push(before);
            Ok(())
        },
    );
    assert_eq!(record.execution.as_ref().unwrap().outcome, exited(0));
    assert_eq!(record.cleanup_errors.len(), 1, "{record:?}");
    assert!(record.cleanup_unquiesced && record.unquiesced());
    assert!(record.must_stop() && record.failed());
    assert!(!scratch.0.join("later-cleanup").exists());
    assert_eq!(notifications, vec![true]);
}

/// 验证 after 通知失败保留主命令退出码与 after 错误，不覆盖已写入的 after 文本，清理照常执行。
#[test]
fn after_notify_failure_does_not_replace_primary_outcome() {
    let scratch = Scratch::new();
    let mut case = case(&scratch);
    case.argv = shell("exit 7");
    let mut out = Vec::new();
    let record = execute_case(
        None,
        &case,
        Duration::from_secs(1),
        &mut out,
        |before, _| {
            if before {
                Ok(())
            } else {
                Err(io::Error::new(io::ErrorKind::Other, "after failed"))
            }
        },
    );
    assert_eq!(record.execution.as_ref().unwrap().outcome, exited(7));
    assert_eq!(record.after_error.as_deref(), Some("after failed"));
    assert!(record.cleanup_errors.is_empty() && record.output_errors.is_empty());
    assert_eq!(out.as_slice(), b"BEFORE\nAFTER 7\n");
    assert_eq!(
        fs::read_to_string(scratch.0.join("order")).unwrap(),
        "setup\ncleanup\n"
    );
    assert!(record.must_stop() && record.failed());
}

/// 验证 after 模板的 `{name}` 与 `{status}` 在同一次展开中各自替换一次：插入的文本不再二次展开。
///
/// 用例名本身含 `{status}` 字面量时，插入后保持字面量；只有模板自己的 `{status}` 变成退出码。
#[test]
fn after_template_expands_name_and_status_once() {
    let scratch = Scratch::new();
    let mut case = case(&scratch);
    case.argv = shell("exit 0");
    case.name = "literal-{status}".into();
    case.before = String::new();
    case.after = "{name} : {status}\n".into();
    let mut out = Vec::new();
    let record = execute_case(None, &case, Duration::from_secs(1), &mut out, |_, _| Ok(()));
    assert!(record.output_errors.is_empty());
    assert_eq!(out.as_slice(), b"literal-{status} : 0\n");
}

/// 验证 LTP 风格的 after 模板在退出码 0 时仍输出字面量 `FAIL LTP CASE x : 0`，runner 不改写失败标记。
#[test]
fn ltp_after_template_keeps_literal_fail_on_success() {
    let scratch = Scratch::new();
    let mut case = case(&scratch);
    case.argv = shell("exit 0");
    case.name = "x".into();
    case.before = String::new();
    case.after = "FAIL LTP CASE {name} : {status}\n".into();
    let mut out = Vec::new();
    let record = execute_case(None, &case, Duration::from_secs(1), &mut out, |_, _| Ok(()));
    assert!(record.output_errors.is_empty());
    assert_eq!(out.as_slice(), b"FAIL LTP CASE x : 0\n");
}

/// 验证通知设备不存在时返回错误而不是静默跳过，合法 ID 在普通文件上分别写入恰好一行
/// `before ID\n` 与 `after ID\n`。
#[test]
fn notify_device_requires_device_and_writes_exact_message() {
    let scratch = Scratch::new();
    let device = scratch.0.join("hook");
    assert!(
        notify_device(&device, true, "runner:trace").is_err(),
        "missing device must fail"
    );
    fs::write(&device, b"").unwrap();
    notify_device(&device, true, "runner:trace").unwrap();
    assert_eq!(fs::read(&device).unwrap(), b"before runner:trace\n");
    let closing = scratch.0.join("hook-after");
    fs::write(&closing, b"").unwrap();
    notify_device(&closing, false, "runner:trace").unwrap();
    assert_eq!(fs::read(&closing).unwrap(), b"after runner:trace\n");
}

/// 验证非法 ID（空串、空白、NUL、换行、超长）在写设备前被拒绝，设备内容保持不变。
#[test]
fn notify_device_rejects_invalid_ids_before_writing() {
    let scratch = Scratch::new();
    let device = scratch.0.join("hook");
    fs::write(&device, b"original").unwrap();
    for id in ["", "bad id", "bad\0id", "bad\nid", &"a".repeat(512)] {
        assert!(notify_device(&device, true, id).is_err(), "id {id:?}");
        assert_eq!(fs::read(&device).unwrap(), b"original");
    }
}

/// 验证外部输出接收端阻塞且子进程忽略 SIGTERM 时，TAP 路径仍按总期限收尾，残余输出记为 io_errors。
#[test]
fn blocked_output_sink_does_not_suspend_deadline() {
    let scratch = Scratch::new();
    let (sink, _unread) = filled_sink();
    let argv = shell("trap '' TERM; while :; do printf '0123456789abcdef0123456789abcdef'; done");
    let (run, _) = run_tap_command(
        &scratch.0,
        &argv,
        &BTreeMap::new(),
        Duration::from_millis(100),
        [sink.as_raw_fd(), sink.as_raw_fd()],
    );
    assert!(
        matches!(run.outcome, ExitOutcome::TimedOut { raw: Some(_), .. }),
        "{:?}",
        run.outcome
    );
    assert!(!run.io_errors.is_empty());
    assert!(run.elapsed < Duration::from_secs(3));
}

/// 验证子进程已退出但输出接收端无法排空时，退出码 7 保留，排空超时单独记入 io_errors。
#[test]
fn relay_drain_deadline_preserves_completed_child_status() {
    let scratch = Scratch::new();
    let (sink, _unread) = filled_sink();
    let argv = shell("printf '1..1\\nok 1 tail\\n'; exit 7");
    let (run, _) = run_tap_command(
        &scratch.0,
        &argv,
        &BTreeMap::new(),
        Duration::from_millis(300),
        [sink.as_raw_fd(), sink.as_raw_fd()],
    );
    assert_eq!(run.outcome, exited(7));
    assert!(
        run.io_errors
            .iter()
            .any(|error| error.contains("output relay did not drain before deadline")),
        "{:?}",
        run.io_errors
    );
    assert!(run.elapsed < Duration::from_secs(3));
}

/// 验证 `/dev/full` 写入失败时 TAP 路径保留退出码 0，并把判定降为 InfrastructureError。
#[test]
fn tap_output_error_preserves_wait_and_fails_result() {
    let scratch = Scratch::new();
    let full = OpenOptions::new().write(true).open("/dev/full").unwrap();
    let argv = shell("printf 'TAP version 13\\n1..1\\nok 1 completed\\n'");
    let (run, result) = run_tap_command(
        &scratch.0,
        &argv,
        &BTreeMap::new(),
        Duration::from_secs(2),
        [full.as_raw_fd(), full.as_raw_fd()],
    );
    assert_eq!(run.outcome, exited(0));
    assert!(!run.io_errors.is_empty());
    assert_eq!(result, Verdict::InfrastructureError);
}

/// 验证输出转发失败不会修改调用方共享输出描述符的打开状态标志，主进程退出码保持 7。
#[test]
fn output_error_preserves_wait_and_shared_output_flags() {
    let scratch = Scratch::new();
    let full = OpenOptions::new().write(true).open("/dev/full").unwrap();
    // SAFETY: `full` 在整个用例期间保持打开；`F_GETFL` 只读取该描述符的状态标志。
    let initial = unsafe { libc::fcntl(full.as_raw_fd(), libc::F_GETFL) };
    let argv = shell("printf '1..1\\nok 1 output\\n'; exit 7");
    let (run, _) = run_tap_command(
        &scratch.0,
        &argv,
        &BTreeMap::new(),
        Duration::from_secs(2),
        [full.as_raw_fd(), full.as_raw_fd()],
    );
    assert_eq!(run.outcome, exited(7));
    assert!(!run.io_errors.is_empty());
    // SAFETY: `full` 仍然打开，再次读取其打开文件描述的状态标志。
    assert_eq!(
        unsafe { libc::fcntl(full.as_raw_fd(), libc::F_GETFL) },
        initial
    );
}

/// 验证 `diagnose` 在目标根内解析绝对符号链接与 ELF `PT_INTERP`，不会到宿主根读取同名路径。
#[test]
fn interpreter_diagnostic_resolves_absolute_links_inside_target_root() {
    // Step 1: 在临时目标根目录内创建模拟加载器、绝对路径符号链接以及带 PT_INTERP 段头的最小 64 位 ELF 文件。
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
    // Step 2: 以该临时目录为 root 诊断绝对符号链接入口，验证链接与解释器都只在目标根内解析。
    let root = RootSpec {
        path: scratch.0.clone(),
        cwd: "/".into(),
        env: BTreeMap::new(),
        mounts: Vec::new(),
        prepare: Vec::new(),
    };
    let diagnostic = contest_run::runtime::diagnose(&root, Path::new("/"), "/application");
    assert!(
        diagnostic.contains("present in target root"),
        "{diagnostic}"
    );
    assert_eq!(
        fs::read(scratch.0.join("lib/loader")).unwrap(),
        b"supplied loader bytes"
    );
}

/// 验证 worker 与被测子进程的 stdin/stdout 共享同一打开文件描述时，两种执行路径都不会把它改成非阻塞，
/// 子进程仍能阻塞读到稍后写入的 stdin。
///
/// plain 模式走比赛路径（直接继承），tap 模式走自测路径（管道 + Relay）；consumer 输出合法 TAP，
/// tap 模式的判定必须是 Pass。三个角色通过环境变量区分，主进程用 gate 文件确认转发已工作。
#[test]
fn shared_console_ofd_preserves_blocking_stdin() {
    use std::io::{BufRead, Read};
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
            // 角色 A (consumer)：打印就绪行，等主进程确认转发已工作后检查 stdin 未被设为非阻塞，再阻塞读取。
            println!("{READY}");
            io::stdout().flush().unwrap();
            let gate = PathBuf::from(std::env::var_os(GATE).unwrap());
            let deadline = Instant::now() + Duration::from_secs(2);
            while !gate.exists() {
                assert!(
                    Instant::now() < deadline,
                    "parent did not observe the console handshake"
                );
                std::thread::sleep(Duration::from_millis(5));
            }
            // SAFETY: `STDIN_FILENO` 是继承的有效标准输入；`F_GETFL` 只读取状态标志。
            let flags = unsafe { libc::fcntl(libc::STDIN_FILENO, libc::F_GETFL) };
            assert!(
                flags != -1,
                "child stdin is invalid: {}",
                io::Error::last_os_error()
            );
            assert_eq!(
                flags & libc::O_NONBLOCK,
                0,
                "runner made the shared console nonblocking"
            );
            let mut input = String::new();
            io::stdin().read_line(&mut input).unwrap();
            assert_eq!(input, "delayed stdin data\n");
            println!("1..1");
            println!("ok 1 stdin");
        }
        Ok("worker") => {
            // 角色 B (worker)：调用被测执行函数启动 consumer，并把输出写回与 stdin 共享 OFD 的 stdout。
            let scratch = Scratch::new();
            let gate = std::env::var_os(GATE).unwrap();
            let env = BTreeMap::from([
                (ROLE.to_string(), "consumer".to_string()),
                (GATE.to_string(), gate.to_string_lossy().into_owned()),
            ]);
            let argv = vec![
                std::env::current_exe()
                    .unwrap()
                    .into_os_string()
                    .into_string()
                    .unwrap(),
                "--exact".into(),
                TEST.into(),
                "--nocapture".into(),
            ];
            let mode = std::env::var(MODE).unwrap();
            if mode == "tap" {
                let (run, result) = run_tap_command(
                    &scratch.0,
                    &argv,
                    &env,
                    Duration::from_secs(2),
                    [libc::STDOUT_FILENO, libc::STDERR_FILENO],
                );
                assert!(run.io_errors.is_empty(), "{:?}", run.io_errors);
                assert_eq!(run.outcome, exited(0));
                assert_eq!(result, Verdict::Pass, "{result:?}");
            } else {
                let run = run_command(None, &scratch.0, &argv, &env, Duration::from_secs(2));
                assert!(run.io_errors.is_empty(), "{:?}", run.io_errors);
                assert_eq!(run.outcome, exited(0));
            }
        }
        _ => {
            // 角色 C (主测试进程)：创建 stdin/stdout 共享同一底层 OFD 的套接字，验证两种模式下延迟写入的 stdin 都能被阻塞读取。
            let _exclusive = exclusive_execution();
            for mode in ["plain", "tap"] {
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
                    fs::write(&gate, b"stdout forwarded by runner").unwrap();
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

/// 验证准备命令以退出码 0 为成功：非零退出与空 argv 记错误，`stop_on_error=false` 时继续尝试后续命令。
#[test]
fn run_steps_records_failures_and_continues_when_not_stopping() {
    let scratch = Scratch::new();
    let commands = vec![
        shell("touch first"),
        Vec::new(),
        shell("exit 5"),
        shell("touch last"),
    ];
    let report = run_steps(
        None,
        &scratch.0,
        &commands,
        &BTreeMap::new(),
        Duration::from_secs(1),
        false,
    );
    assert_eq!(report.errors.len(), 2, "{:?}", report.errors);
    assert!(!report.unquiesced);
    assert!(
        scratch.0.join("first").exists() && scratch.0.join("last").exists(),
        "commands after a failure must still run"
    );
}

/// 验证 `stop_on_error=true` 在首个失败命令后停止，不再运行后续准备命令。
#[test]
fn run_steps_stops_at_first_failure_when_requested() {
    let scratch = Scratch::new();
    let commands = vec![shell("exit 5"), shell("touch after")];
    let report = run_steps(
        None,
        &scratch.0,
        &commands,
        &BTreeMap::new(),
        Duration::from_secs(1),
        true,
    );
    assert_eq!(report.errors.len(), 1, "{:?}", report.errors);
    assert!(!report.unquiesced);
    assert!(!scratch.0.join("after").exists());
}
