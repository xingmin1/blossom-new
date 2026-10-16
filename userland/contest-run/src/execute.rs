//! 单线程进程执行、有限输出队列与逐轮尾部责任。

use crate::model::{CommandSpec, Protocol, ResolvedCase};
use crate::report::{ProtocolObserver, StreamKind, Verdict};
use std::collections::{BTreeMap, VecDeque};
use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

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
    pub verdict: Verdict,
}

#[derive(Debug)]
pub struct CaseRecord {
    pub execution: Option<Execution>,
    pub setup_error: Option<String>,
    pub before_error: Option<String>,
    pub after_error: Option<String>,
    pub cleanup_errors: Vec<String>,
    pub iteration: u32,
}

impl CaseRecord {
    /// 基础设施失败后禁止在未知状态下继续后续用例。
    pub fn must_stop(&self) -> bool {
        self.setup_error.is_some()
            || self.before_error.is_some()
            || self.after_error.is_some()
            || !self.cleanup_errors.is_empty()
            || self.execution.as_ref().is_some_and(|run| {
                !run.io_errors.is_empty() || matches!(run.outcome, ExitOutcome::IoError { .. })
            })
    }

    /// 只有协议成功且没有独立基础设施错误才算本轮成功。
    pub fn is_success(&self) -> bool {
        !self.must_stop()
            && self
                .execution
                .as_ref()
                .is_some_and(|run| run.verdict.is_success())
    }
}

/// 将真实 wait 编码转换为互斥的退出事实。
fn exit_outcome(status: ExitStatus) -> ExitOutcome {
    let raw = status.into_raw();
    let status = ExitStatus::from_raw(raw);
    match status.code() {
        Some(code) => ExitOutcome::Exited { code, raw },
        None => ExitOutcome::Signaled {
            signal: status.signal().unwrap_or(0),
            raw,
        },
    }
}

/// 在 fork 前形成全部路径和 Command 参数；子闭包只调用短底层操作。
fn command(
    root: Option<&Path>,
    cwd: &Path,
    spec: &CommandSpec,
    env: &BTreeMap<String, String>,
    capture: bool,
    outputs: [RawFd; 2],
) -> io::Result<Command> {
    let root = root
        .map(|path| CString::new(path.as_os_str().as_bytes()))
        .transpose()?;
    let cwd = CString::new(cwd.as_os_str().as_bytes())?;
    let mut command = match spec {
        CommandSpec::Exec { program, args } => {
            let mut command = Command::new(program);
            command.args(args);
            command
        }
        CommandSpec::Shell {
            shell,
            command: text,
        } => {
            let mut command = Command::new(shell);
            command.arg("-c").arg(text);
            command
        }
    };
    command.envs(env).stdin(Stdio::inherit());
    if capture {
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
    } else {
        command.stdout(Stdio::from(duplicate_output(outputs[0])?));
        command.stderr(Stdio::from(duplicate_output(outputs[1])?));
    }
    // SAFETY: 路径 CString 与 argv/env 在父进程准备；闭包不分配、不取锁，
    // 只调用异步信号安全的 chroot/chdir/setpgid 并立即保存失败 errno。
    unsafe {
        command.pre_exec(move || {
            if let Some(root) = &root {
                if libc::chroot(root.as_ptr()) == -1 {
                    return Err(io::Error::last_os_error());
                }
            }
            if libc::chdir(cwd.as_ptr()) == -1 {
                return Err(io::Error::last_os_error());
            }
            if libc::setpgid(0, 0) == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok(command)
}

/// 只复制描述符和 CLOEXEC 位，不修改与继承 stdin 可能共用的 OFD 状态。
fn duplicate_output(fd: RawFd) -> io::Result<OwnedFd> {
    // SAFETY: fcntl 只借用调用者保活的 fd；成功产生全新、由返回值唯一拥有的描述符。
    let copied = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
    if copied == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: copied 是刚分配且未交给其它拥有者的有效描述符。
    Ok(unsafe { OwnedFd::from_raw_fd(copied) })
}

/// 仅用于本次独占的 pipe 端点；这些端点关闭时销毁，无借用输出恢复责任。
fn nonblocking_pipe(fd: RawFd) -> io::Result<()> {
    // SAFETY: fd 是调用者保活的独占 pipe，fcntl 不访问用户指针。
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: 仅修改本次独占 pipe 的状态，不触及标准流或其共享 OFD。
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// 创建完整拥有且跨 exec 自动关闭的匿名 pipe，失败时不发表端点。
fn relay_pipe() -> io::Result<[OwnedFd; 2]> {
    let mut fds = [-1; 2];
    // SAFETY: fds 是完整可写的两个 c_int，pipe2 成功后分别返回新描述符。
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: 两个成功新建的描述符互不相同，且尚无其它 Rust 拥有者。
    Ok(unsafe { [OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])] })
}

struct Relay {
    pid: Option<libc::pid_t>,
    writers: [Option<OwnedFd>; 2],
    status: Option<i32>,
}

impl Relay {
    /// fork 前准备所有端点和固定缓冲；子进程只转发输出，无后代和 Rust 运行时操作。
    fn new(outputs: [RawFd; 2]) -> io::Result<Self> {
        let [read_stdout, write_stdout] = relay_pipe()?;
        let [read_stderr, write_stderr] = relay_pipe()?;
        nonblocking_pipe(write_stdout.as_raw_fd())?;
        nonblocking_pipe(write_stderr.as_raw_fd())?;
        let readers = [read_stdout.as_raw_fd(), read_stderr.as_raw_fd()];
        let writers = [write_stdout.as_raw_fd(), write_stderr.as_raw_fd()];
        let mut buffers = [[0u8; 4096]; 2];
        let mut fds = [
            libc::pollfd {
                fd: readers[0],
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: readers[1],
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        // SAFETY: errno 是本线程的 TLS 单元；fork 后同一地址在子地址空间保留，供纯底层重试判断。
        let errno = unsafe { libc::__errno_location() };
        // SAFETY: fork 后子分支只使用预建端点、栈数组和异步信号安全调用，
        // 不执行析构、分配、格式化或 Rust 锁；父独占 PID 并负责 kill/wait。
        let pid = unsafe { libc::fork() };
        if pid == -1 {
            return Err(io::Error::last_os_error());
        }
        if pid == 0 {
            // SAFETY: 本分支位于 fork 子进程；所有 fd、缓冲和 errno 指针均继承自上述预备状态。
            unsafe { relay_child(writers, outputs, &mut fds, &mut buffers, errno) }
        }
        drop(read_stdout);
        drop(read_stderr);
        Ok(Self {
            pid: Some(pid),
            writers: [Some(write_stdout), Some(write_stderr)],
            status: None,
        })
    }

    /// 父只向自己的非阻塞 pipe 写端发送原字节，不改变真实输出端点。
    fn outputs(&self) -> [RawFd; 2] {
        [
            self.writers[0].as_ref().unwrap().as_raw_fd(),
            self.writers[1].as_ref().unwrap().as_raw_fd(),
        ]
    }

    /// 队列排空后关闭写端，让 helper 取得真实 EOF 并完成阻塞输出。
    fn close_inputs(&mut self) {
        self.writers[0].take();
        self.writers[1].take();
    }

    /// 非阻塞确认 helper 的实际退出；提前失败只记录独立 I/O 错误。
    fn check(&mut self, errors: &mut Vec<String>) {
        let Some(pid) = self.pid else {
            return;
        };
        let mut raw = 0;
        // SAFETY: pid 是本实例唯一拥有的直接子进程，raw 是完整可写的 wait 状态单元。
        let result = unsafe { libc::waitpid(pid, &mut raw, libc::WNOHANG) };
        if result == 0 {
            return;
        }
        if result == -1 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                return;
            }
            errors.push(format!("output relay wait: {error}"));
            if error.raw_os_error() == Some(libc::ECHILD) {
                self.pid = None;
            }
            return;
        }
        self.pid = None;
        self.status = Some(raw);
        if raw != 0 || self.writers.iter().any(Option::is_some) {
            errors.push(format!(
                "output relay exited before successful drain: raw={raw}"
            ));
        }
    }

    /// helper 已退出而输入尚未全部交付时，后续只观察主 child，不再堆积转发队列。
    fn failed(&self) -> bool {
        self.pid.is_none() && (self.status != Some(0) || self.writers.iter().any(Option::is_some))
    }

    /// 输出期限结束时定向强杀并 wait 自己的 helper，不依赖阻塞输出自行恢复。
    fn stop(&mut self) -> io::Result<()> {
        self.close_inputs();
        let Some(pid) = self.pid else {
            return Ok(());
        };
        // SAFETY: pid 尚未 reap，仍由本实例独占；仅向这个无后代 helper 发送 SIGKILL。
        if unsafe { libc::kill(pid, libc::SIGKILL) } == -1 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(error);
            }
        }
        loop {
            let mut raw = 0;
            // SAFETY: PID 拥有持续到同一 waitpid 确认退出；raw 在调用期间可写。
            if unsafe { libc::waitpid(pid, &mut raw, 0) } == pid {
                self.pid = None;
                self.status = Some(raw);
                return Ok(());
            }
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            if error.raw_os_error() == Some(libc::ECHILD) {
                self.pid = None;
            }
            return Err(error);
        }
    }
}

impl Drop for Relay {
    /// 构造后发生 spawn 或 pipe 错误也不得遗留 helper。
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

/// fork 子分支只运行异步信号安全调用；调用者须提供本子进程独占的有效端点、栈数组和 errno。
unsafe fn relay_child(
    writers: [RawFd; 2],
    outputs: [RawFd; 2],
    fds: &mut [libc::pollfd; 2],
    buffers: &mut [[u8; 4096]; 2],
    errno: *mut libc::c_int,
) -> ! {
    for fd in writers {
        // SAFETY: 子分支不用父写端；这些 fd 是 fork 复制的本地描述符，关闭不影响父拥有。
        unsafe {
            libc::close(fd);
        }
    }
    let mut open = 2;
    while 0 < open {
        // SAFETY: fds 为预建的两项可写数组；helper 的阻塞等待由父定向 kill/wait 结束。
        if unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) } == -1 {
            // SAFETY: errno 指向 fork 继承的本线程 TLS，未经过任何其它可能改写 errno 的调用。
            if unsafe { *errno } == libc::EINTR {
                continue;
            }
            // SAFETY: 子分支不能运行 Rust 析构，_exit 立即报告真实 I/O 失败。
            unsafe {
                libc::_exit(1);
            }
        }
        for index in 0..2 {
            if fds[index].fd == -1 || fds[index].revents == 0 {
                continue;
            }
            // SAFETY: 当前 pipe 端点尚未关闭，固定数组完整可写且长度准确。
            let count = unsafe {
                libc::read(
                    fds[index].fd,
                    buffers[index].as_mut_ptr().cast(),
                    buffers[index].len(),
                )
            };
            if count == 0 {
                // SAFETY: EOF 后不再使用此子进程本地读端，poll 中立即删除对应 fd。
                unsafe {
                    libc::close(fds[index].fd);
                }
                fds[index].fd = -1;
                open -= 1;
                continue;
            }
            if count < 0 {
                // SAFETY: errno 是上一个 read 留下的本线程错误。
                if unsafe { *errno } == libc::EINTR {
                    continue;
                }
                // SAFETY: 不运行 Rust 运行时或析构，父会独立保存 helper 的 wait 状态。
                unsafe {
                    libc::_exit(1);
                }
            }
            let mut offset = 0;
            while offset < count as usize {
                // SAFETY: 已读取字节在栈数组内，offset 小于有效长度；真实输出只借用不改 OFD。
                let written = unsafe {
                    libc::write(
                        outputs[index],
                        buffers[index][offset..].as_ptr().cast(),
                        count as usize - offset,
                    )
                };
                if 0 < written {
                    offset += written as usize;
                    continue;
                }
                if written < 0 {
                    // SAFETY: 在后续 syscall 前读取该 write 的线程本地 errno。
                    let error = unsafe { *errno };
                    if error == libc::EINTR {
                        continue;
                    }
                    if error == libc::EAGAIN {
                        let mut output = libc::pollfd {
                            fd: outputs[index],
                            events: libc::POLLOUT,
                            revents: 0,
                        };
                        // SAFETY: output 为有效的单项数组，借用输出仍由父调用作用域保活。
                        if unsafe { libc::poll(&mut output, 1, -1) } != -1 {
                            continue;
                        }
                        // SAFETY: 读取刚发生的 poll 错误，不跨其它库调用。
                        if unsafe { *errno } == libc::EINTR {
                            continue;
                        }
                    }
                }
                // SAFETY: 短写尾部仅在本 helper 内推进；错误直接退出，绝不执行 Rust 清理。
                unsafe {
                    libc::_exit(1);
                }
            }
        }
    }
    // SAFETY: 两条输入已取得 EOF，所有接收字节均已实际写出；_exit 不调用 Rust 析构。
    unsafe {
        libc::_exit(0);
    }
}

struct Flow {
    input: RawFd,
    output: RawFd,
    kind: StreamKind,
    eof: bool,
    failed: bool,
    queue: VecDeque<u8>,
}

impl Flow {
    /// 为单个输出流建立固定容量背压队列。
    fn new(input: RawFd, output: RawFd, kind: StreamKind) -> Self {
        Self {
            input,
            output,
            kind,
            eof: false,
            failed: false,
            queue: VecDeque::with_capacity(65536),
        }
    }

    /// 每轮最多读一块，持续输出不能饿死 deadline 与另一条流。
    fn read(&mut self, observer: &mut ProtocolObserver, errors: &mut Vec<String>) {
        if self.eof || 57344 < self.queue.len() {
            return;
        }
        let mut bytes = [0u8; 8192];
        // SAFETY: input 是 Child 持有的有效管道，目标数组完整可写且长度准确。
        let count = unsafe { libc::read(self.input, bytes.as_mut_ptr().cast(), bytes.len()) };
        if count == 0 {
            self.eof = true;
        } else if 0 < count {
            let bytes = &bytes[..count as usize];
            observer.feed(self.kind, bytes);
            if !self.failed {
                self.queue.extend(bytes);
            }
        } else {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::WouldBlock
                && error.kind() != io::ErrorKind::Interrupted
            {
                errors.push(format!("read {:?}: {error}", self.kind));
                self.eof = true;
            }
        }
    }

    /// 单次非阻塞写保留短写尾部，写失败后仍消费子流以免堵死 child。
    fn forward(&mut self, errors: &mut Vec<String>) {
        let (bytes, _) = self.queue.as_slices();
        if bytes.is_empty() {
            return;
        }
        // SAFETY: output 由调用者保活，bytes 在本次 write 返回前保持可读。
        let count = unsafe { libc::write(self.output, bytes.as_ptr().cast(), bytes.len()) };
        if 0 < count {
            self.queue.drain(..count as usize);
        } else if count == 0 {
            errors.push(format!("write {:?}: zero progress", self.kind));
            self.queue.clear();
            self.failed = true;
        } else {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::WouldBlock
                && error.kind() != io::ErrorKind::Interrupted
            {
                errors.push(format!("write {:?}: {error}", self.kind));
                self.queue.clear();
                self.failed = true;
            }
        }
    }

    /// 两个描述符分别表达读背压与待转发字节，不依赖流间精确交错。
    fn poll_fds(&self) -> [libc::pollfd; 2] {
        [
            libc::pollfd {
                fd: if self.eof || 57344 < self.queue.len() {
                    -1
                } else {
                    self.input
                },
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: if self.queue.is_empty() {
                    -1
                } else {
                    self.output
                },
                events: libc::POLLOUT,
                revents: 0,
            },
        ]
    }
}

/// 向原进程组发送停止信号；ESRCH 仅表示当前已无该组。
fn signal_group(child: &Child, signal: i32, errors: &mut Vec<String>) {
    // SAFETY: Child 的 pid 是本次 spawn 创建的正 pid，负号按 kill ABI 指向原组。
    if unsafe { libc::kill(-(child.id() as i32), signal) } == -1 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            errors.push(format!("kill process group signal={signal}: {error}"));
        }
    }
}

/// 同一绝对期限协调 child 与可选输出 helper；真实输出的阻塞仅发生在可单独停止的 helper。
fn pump(
    child: &mut Child,
    deadline: Instant,
    observer: &mut ProtocolObserver,
    mut relay: Option<Relay>,
    errors: &mut Vec<String>,
) -> ExitOutcome {
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let mut flows = if let Some(relay) = &relay {
        let stdout = stdout.as_ref().expect("captured stdout");
        let stderr = stderr.as_ref().expect("captured stderr");
        if let Err(error) =
            nonblocking_pipe(stdout.as_raw_fd()).and_then(|()| nonblocking_pipe(stderr.as_raw_fd()))
        {
            signal_group(child, libc::SIGKILL, errors);
            let raw = match child.wait() {
                Ok(status) => Some(status.into_raw()),
                Err(wait) => {
                    errors.push(format!("wait after pipe failure: {wait}"));
                    None
                }
            };
            return ExitOutcome::IoError {
                errno: error.raw_os_error(),
                message: error.to_string(),
                raw,
            };
        }
        let outputs = relay.outputs();
        Some([
            Flow::new(stdout.as_raw_fd(), outputs[0], StreamKind::Stdout),
            Flow::new(stderr.as_raw_fd(), outputs[1], StreamKind::Stderr),
        ])
    } else {
        None
    };
    let mut status = None;
    let mut timed_out = false;
    let mut terminate_at = None;
    let mut kill_at = None;
    let mut wait_error = None;
    loop {
        let now = Instant::now();
        if !timed_out
            && deadline <= now
            && status.is_some()
            && flows
                .as_ref()
                .is_none_or(|flows| flows.iter().all(|flow| flow.eof && flow.queue.is_empty()))
        {
            if let Some(relay) = &mut relay {
                relay.check(errors);
                if relay.pid.is_some() {
                    errors.push("output relay did not drain before deadline".into());
                    if let Err(error) = relay.stop() {
                        errors.push(format!("stop output relay: {error}"));
                    }
                }
            }
            break;
        }
        if !timed_out && deadline <= now {
            timed_out = true;
            signal_group(child, libc::SIGTERM, errors);
            terminate_at = Some(now + Duration::from_millis(200));
        }
        if terminate_at.is_some_and(|at| at <= now) {
            signal_group(child, libc::SIGKILL, errors);
            terminate_at = None;
            kill_at = Some(now + Duration::from_millis(200));
        }
        if status.is_none() && wait_error.is_none() {
            match child.try_wait() {
                Ok(found) => status = found,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => {
                    errors.push(format!("try_wait: {error}"));
                    wait_error = Some(error);
                    signal_group(child, libc::SIGKILL, errors);
                    kill_at = Some(now);
                }
            }
        }
        if let Some(relay) = &mut relay {
            relay.check(errors);
            if relay.failed() {
                if let Some(flows) = &mut flows {
                    for flow in flows {
                        flow.queue.clear();
                        flow.failed = true;
                    }
                }
                relay.close_inputs();
            }
        }
        if let Some(flows) = &mut flows {
            for flow in flows {
                flow.forward(errors);
                flow.read(observer, errors);
                flow.forward(errors);
            }
        }
        let drained = flows
            .as_ref()
            .is_none_or(|flows| flows.iter().all(|flow| flow.eof && flow.queue.is_empty()));
        if drained {
            if let Some(relay) = &mut relay {
                relay.close_inputs();
                relay.check(errors);
            }
        }
        let relay_done = relay.as_ref().is_none_or(|relay| relay.pid.is_none());
        if status.is_some() && drained && relay_done && (!timed_out || kill_at.is_some()) {
            break;
        }
        if kill_at.is_some_and(|at| at <= now) {
            if let Some(flows) = &flows {
                for flow in flows {
                    if !flow.eof {
                        errors.push(format!(
                            "{:?} remains open after process-group stop",
                            flow.kind
                        ));
                    }
                    if !flow.queue.is_empty() {
                        errors.push(format!(
                            "{:?} output queue did not drain before deadline",
                            flow.kind
                        ));
                    }
                }
            }
            if let Some(relay) = &mut relay {
                if relay.pid.is_some() {
                    errors.push("output relay did not drain before deadline".into());
                    if let Err(error) = relay.stop() {
                        errors.push(format!("stop output relay: {error}"));
                    }
                }
            }
            if status.is_none() {
                match child.wait() {
                    Ok(found) => status = Some(found),
                    Err(error) => {
                        errors.push(format!("wait after stop: {error}"));
                        wait_error = Some(error);
                    }
                }
            }
            break;
        }
        let mut fds = [libc::pollfd {
            fd: -1,
            events: 0,
            revents: 0,
        }; 4];
        if let Some(flows) = &flows {
            let first = flows[0].poll_fds();
            let second = flows[1].poll_fds();
            fds = [first[0], first[1], second[0], second[1]];
        }
        let next = terminate_at.or(kill_at).unwrap_or(deadline);
        let millis = next
            .saturating_duration_since(Instant::now())
            .as_millis()
            .min(50) as i32;
        // SAFETY: fds 是完整可写数组；无捕获时全部 fd=-1，仅作有界等待，不借用或修改真实输出。
        if unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, millis) } == -1 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                errors.push(format!("poll: {error}"));
                signal_group(child, libc::SIGKILL, errors);
                kill_at = Some(Instant::now());
            }
        }
    }
    let raw = status.map(ExitStatusExt::into_raw);
    if timed_out {
        return ExitOutcome::TimedOut {
            raw,
            signal: raw.and_then(|raw| ExitStatus::from_raw(raw).signal()),
        };
    }
    if let Some(error) = wait_error {
        return ExitOutcome::IoError {
            errno: error.raw_os_error(),
            message: error.to_string(),
            raw,
        };
    }
    match raw {
        Some(raw) => exit_outcome(ExitStatus::from_raw(raw)),
        None => ExitOutcome::IoError {
            errno: None,
            message: "child wait status unavailable".into(),
            raw: None,
        },
    }
}

/// Plain/LTP 直接交付描述符；只有正文协议建立捕获与 relay，所有分支保留继承 stdin 和总期限。
pub fn run_command(
    root: Option<&Path>,
    cwd: &Path,
    spec: &CommandSpec,
    env: &BTreeMap<String, String>,
    timeout: Duration,
    protocol: &Protocol,
    outputs: [RawFd; 2],
) -> Execution {
    let started = Instant::now();
    let deadline = started.checked_add(timeout);
    let mut observer = ProtocolObserver::new(protocol);
    let mut io_errors = Vec::new();
    let outcome = match deadline {
        None => ExitOutcome::SpawnError {
            errno: Some(libc::EINVAL),
            message: "deadline exceeds monotonic clock range".into(),
        },
        Some(deadline) => {
            let capture = matches!(
                protocol,
                Protocol::Basic { .. } | Protocol::Pre2025Libctest { .. }
            );
            match command(root, cwd, spec, env, capture, outputs) {
                Err(error) => ExitOutcome::SpawnError {
                    errno: error.raw_os_error(),
                    message: error.to_string(),
                },
                Ok(mut command) => {
                    let relay = if capture {
                        Relay::new(outputs).map(Some)
                    } else {
                        Ok(None)
                    };
                    match relay {
                        Err(error) => ExitOutcome::IoError {
                            errno: error.raw_os_error(),
                            message: format!("create output relay: {error}"),
                            raw: None,
                        },
                        Ok(relay) => match command.spawn() {
                            Ok(mut child) => {
                                pump(&mut child, deadline, &mut observer, relay, &mut io_errors)
                            }
                            Err(error) => ExitOutcome::SpawnError {
                                errno: error.raw_os_error(),
                                message: error.to_string(),
                            },
                        },
                    }
                }
            }
        }
    };
    let verdict = observer.finish(&outcome);
    Execution {
        outcome,
        io_errors,
        elapsed: started.elapsed(),
        verdict,
    }
}

/// 逐条执行显式准备命令，不把非零或超时转换成测例结果。
pub fn run_steps(
    root: Option<&Path>,
    cwd: &Path,
    commands: &[Vec<String>],
    env: &BTreeMap<String, String>,
    timeout: Duration,
    outputs: [RawFd; 2],
    stop_on_error: bool,
) -> Vec<String> {
    let mut errors = Vec::new();
    for argv in commands {
        let Some((program, args)) = argv.split_first() else {
            errors.push("empty preparation command".into());
            break;
        };
        let spec = CommandSpec::Exec {
            program: program.clone(),
            args: args.to_vec(),
        };
        let run = run_command(root, cwd, &spec, env, timeout, &Protocol::Plain, outputs);
        if !matches!(run.outcome, ExitOutcome::Exited { code: 0, .. }) || !run.io_errors.is_empty()
        {
            errors.push(format!(
                "command {argv:?}: {:?}; I/O errors: {:?}",
                run.outcome, run.io_errors
            ));
            if stop_on_error {
                break;
            }
        }
    }
    errors
}

/// 每轮固定执行 setup、before、主命令、配对 after 与 cleanup，不提前绕过尾部。
pub fn execute_case(
    root: Option<&Path>,
    case: &ResolvedCase,
    iteration: u32,
    outputs: [RawFd; 2],
    mut hook: impl FnMut(bool, &str) -> io::Result<()>,
) -> CaseRecord {
    let timeout = Duration::from_secs(case.timeout_sec);
    let setup = run_steps(
        root,
        &case.cwd,
        &case.setup,
        &case.env,
        timeout,
        outputs,
        true,
    );
    let mut record = CaseRecord {
        execution: None,
        setup_error: setup.into_iter().next(),
        before_error: None,
        after_error: None,
        cleanup_errors: Vec::new(),
        iteration,
    };
    let mut before_succeeded = false;
    if record.setup_error.is_none() && case.hooks {
        match hook(true, &case.id) {
            Ok(()) => before_succeeded = true,
            Err(error) => record.before_error = Some(error.to_string()),
        }
    }
    if record.setup_error.is_none() && record.before_error.is_none() {
        record.execution = Some(run_command(
            root,
            &case.cwd,
            &case.command,
            &case.env,
            timeout,
            &case.protocol,
            outputs,
        ));
    }
    if before_succeeded {
        if let Err(error) = hook(false, &case.id) {
            record.after_error = Some(error.to_string());
        }
    }
    record.cleanup_errors = run_steps(
        root,
        &case.cwd,
        &case.cleanup,
        &case.env,
        timeout,
        outputs,
        false,
    );
    record
}
