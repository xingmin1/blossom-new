//! 子进程启动、进程组超时控制与上下文收尾模块。
//!
//! ## 模块职责与架构定位
//! 本模块对外提供两个入口，共用同一套启动、等待与收尾机制：
//! - [`run_command`]：比赛路径与系统维护命令使用。标准输入/输出/错误直接继承父进程，环境表先清空再只装入调用方给出的变量；
//!   子进程在 `exec` 前依次 `chroot`（指定 root 时）、`chdir` 到工作目录并 `setsid()`：新会话没有控制终端，同时子进程成为独立进程组的组首。
//!   不用 `setpgid(0, 0)`：它让测试留在串口控制终端的后台进程组，读终端或 `tcsetattr` 会被内核按作业控制以 `SIGTTIN`/`SIGTTOU` 停住。不捕获、不解析、不转发输出。
//! - [`run_tap_command`]：`user-selftests` 使用。不 `chroot`，环境表沿用父进程再叠加调用方变量，标准输出/错误经管道读取：
//!   标准输出字节喂给 `TapObserver`，两路字节经 `Relay` 辅助进程转发到调用方给定的输出描述符。
//!
//! ## 进程组与收尾
//! 父进程用 `waitid(P_PID, pid, WEXITED | WNOHANG | WNOWAIT)` 观察组首退出而不回收：未回收的僵尸组首使进程组号在其残留成员清空前
//! 不会被内核复用。期限到达时向整组发 `SIGTERM`，200 ms 后升级 `SIGKILL`，再 200 ms 仍未排空则强制收尾。循环结束时（组首已退出且
//! 输出排空，或强制收尾）先向整组补发一次 `SIGKILL` 清理残留后台进程，再 `wait` 回收组首并据其状态生成 [`ExitOutcome`]。
//!
//! 组首退出不代表组内后台进程已经结束；普通进程组不追踪主动 `setsid` 脱离的后代，这类服务需要被测脚本或显式 cleanup 自行收尾。
//!
//! ## 自测路径的双层管道
//! 不能让主进程直接阻塞写向外部标准输出/错误（如串口终端），否则终端写阻塞时主进程无法及时响应期限；也不能把外部
//! `stdout`/`stderr` 的打开文件描述改成 `O_NONBLOCK`，因为终端上它们通常与 `stdin` 共享同一个打开文件描述。因此自测路径采用
//! “主进程非阻塞读取 + 独立 `Relay` 辅助进程阻塞写外部终端”的两层结构：
//!
//! ```text
//!   ┌────────────────┐  stdout/stderr 管道 (O_NONBLOCK)  ┌──────────────────────────────────┐
//!   │  被测子进程组   │ ────────────────────────────────► │         contest-run 主进程        │
//!   │ (独立 pgid=pid) │                                   │  1. 喂给 TapObserver 解析         │
//!   └────────────────┘                                   │  2. 存入 64 KiB 背压队列 (Flow)   │
//!                                                        │  3. 监控截止时间并发 SIGTERM/KILL │
//!                                                        └────────────────┬─────────────────┘
//!                                                                         │ relay 管道 (写端 O_NONBLOCK)
//!                                                                         ▼
//!                                                        ┌──────────────────────────────────┐
//!                                                        │      Relay 辅助子进程 (fork)      │
//!                                                        │  纯异步信号安全循环 (poll/read/write)│
//!                                                        │  阻塞写向外部 outputs [fd 1, fd 2] │
//!                                                        └──────────────────────────────────┘
//! ```
//!
//! 超时状态转换（`pump` 循环）：
//! 1. **正常运行**：轮询组首退出状态与管道 I/O，每轮 `poll` 等待上限 50 ms。
//! 2. **触发超时（`deadline <= now`）**：向整个被测进程组（`-pid`）发送 `SIGTERM`，设置 `terminate_at = now + 200ms`。
//! 3. **升级强杀（`terminate_at <= now`）**：向进程组发送 `SIGKILL`，设置 `kill_at = now + 200ms`。
//! 4. **强制收尾（`kill_at <= now`）**：停止等待残留管道，终止 `Relay` 辅助进程，转入统一收尾。
//!
//! ## 并发与锁契约
//! - 主进程内部在单线程事件循环中通过 `poll` 复用 I/O，不持有跨线程锁。
//! - `command` 的 `pre_exec` 闭包与 `relay_child` 运行在 `fork` 后的子进程上下文中，严禁堆内存分配、Rust 锁获取或任何非异步信号
//!   安全（Async-Signal-Safe）操作，遇到致命错误必须调用 `libc::_exit` 直接终止。

use crate::outcome::{Execution, ExitOutcome, ProtocolResult, Verdict};
use crate::tap::TapObserver;
use std::collections::{BTreeMap, VecDeque};
use std::ffi::CString;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::process::{CommandExt, ExitStatusExt};
use std::path::Path;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// 将操作系统的 `ExitStatus` 转换为结构化的 `ExitOutcome`（区分正常退出码与终止信号）。
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

/// 根据 `argv` 构造待执行的命令，并在 `fork` 前准备好路径 `CString`。
///
/// 标准流默认继承父进程，自测路径随后把输出改成管道。子进程在 `exec` 前先 `chroot`（`root` 为 `Some` 时），再 `chdir` 到 `cwd`，
/// 最后 `setsid()` 成为新会话与独立进程组的组首，不再带有控制终端。空 `argv` 或空程序名不派生子进程，直接以 `EINVAL` 的 `SpawnError` 返回。
fn command(root: Option<&Path>, cwd: &Path, argv: &[String]) -> Result<Command, ExitOutcome> {
    let invalid = |message: &str| ExitOutcome::SpawnError {
        errno: Some(libc::EINVAL),
        message: message.into(),
    };
    let (program, args) = argv.split_first().ok_or_else(|| invalid("empty argv"))?;
    if program.is_empty() {
        return Err(invalid("empty program"));
    }
    let root = root
        .map(|path| CString::new(path.as_os_str().as_bytes()))
        .transpose()
        .map_err(|error| invalid(&error.to_string()))?;
    let cwd =
        CString::new(cwd.as_os_str().as_bytes()).map_err(|error| invalid(&error.to_string()))?;
    let mut command = Command::new(program);
    command
        .args(args)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    // SAFETY:
    // 1. `root` 与 `cwd` 的 `CString` 已在 `fork` 前于父进程内分配并校验完毕，闭包内只读取其有效 C 字符串指针。
    // 2. 闭包运行在 `fork` 后、`exec` 前的子进程环境中，不进行任何堆内存分配或锁操作，仅调用 POSIX 异步信号安全的 `chroot`、`chdir` 与 `setsid`；新 `fork` 出的子进程不是进程组组长，`setsid` 不会因此失败。
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
            if libc::setsid() == -1 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    Ok(command)
}

/// 将主进程独占的匿名管道端点设置为非阻塞模式（`O_NONBLOCK`）。
///
/// 仅用于本次命令新建的私有管道端点，这些端点在命令结束关闭时即销毁，无需在退出时恢复原标志。
fn nonblocking_pipe(fd: RawFd) -> io::Result<()> {
    // SAFETY: 调用方传入的 `fd` 是当前进程独占持有的有效管道描述符；`F_GETFL` 无需额外指针参数。
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `fd` 为独占管道端点，`flags` 为上一行 `F_GETFL` 成功读取的标志位；设置 `O_NONBLOCK` 仅影响该私有管道，不会污染标准输入/输出共享的终端文件状态。
    if unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) } == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// 创建一对带有 `O_CLOEXEC` 标志的匿名管道，并将其读写两端封装为具有唯一所有权的 `OwnedFd`。
fn relay_pipe() -> io::Result<[OwnedFd; 2]> {
    let mut fds = [-1; 2];
    // SAFETY: `fds` 是栈上长度为 2 的可写 `c_int` 数组，满足 `pipe2` 对 `int pipefd[2]` 缓冲区有效性与对齐的要求。
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `pipe2` 返回非 -1 保证了 `fds[0]`（读端）与 `fds[1]`（写端）均为内核新分配且互不相同的有效文件描述符，此前无其他所有者。
    Ok(unsafe { [OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])] })
}

/// 负责将主进程缓冲的输出字节异步写入外部真实输出描述符的辅助子进程控制器。
struct Relay {
    /// 辅助子进程的 PID；当辅助进程已被 `waitpid` 回收后置为 `None`。
    pid: Option<libc::pid_t>,
    /// 主进程向辅助子进程发送 `stdout` 和 `stderr` 数据的非阻塞管道写端（关闭即向辅助进程发送 EOF）。
    writers: [Option<OwnedFd>; 2],
    /// 辅助子进程退出时的原始 `waitpid` 状态字。
    status: Option<i32>,
}

impl Relay {
    /// 创建两条转发管道并 `fork` 启动输出转发辅助子进程。
    ///
    /// 所有管道、栈缓冲区与线程本地 `errno` 地址均在 `fork` 前准备就绪；
    /// 子进程仅执行纯系统调用转发循环，不产生孙进程，也不触碰 Rust 运行时状态。
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
        // SAFETY: `libc::__errno_location()` 返回当前线程 TLS 中的 `errno` 整型指针；`fork` 后子进程继承相同的虚拟地址映射且单线程运行，可在子进程中直接解引用读取系统调用错误码。
        let errno = unsafe { libc::__errno_location() };
        // SAFETY:
        // 1. 当前程序为单线程运行，`fork` 前已完成全部管道描述符与栈缓冲区的构造。
        // 2. 子进程分支（`pid == 0`）立即进入永不返回的 `relay_child`，并在退出时调用 `libc::_exit`，绝不执行父进程栈上对象的 Rust `Drop` 析构或堆内存分配。
        let pid = unsafe { libc::fork() };
        if pid == -1 {
            return Err(io::Error::last_os_error());
        }
        if pid == 0 {
            // SAFETY: 当前处于 `fork` 后的子进程分支，`writers`、`outputs`、`fds`、`buffers` 与 `errno` 指针均在 `fork` 前初始化完毕且在子进程地址空间中独占有效。
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

    /// 返回主进程向辅助子进程写入 `stdout` 和 `stderr` 的非阻塞管道写端原始描述符。
    fn outputs(&self) -> [RawFd; 2] {
        [
            self.writers[0].as_ref().unwrap().as_raw_fd(),
            self.writers[1].as_ref().unwrap().as_raw_fd(),
        ]
    }

    /// 关闭主进程持有的两条转发管道写端，使辅助子进程在读尽管道残留数据后收到 EOF 并正常退出。
    fn close_inputs(&mut self) {
        self.writers[0].take();
        self.writers[1].take();
    }

    /// 以非阻塞方式（`WNOHANG`）检查并回收辅助子进程；若辅助子进程异常退出或未排空即退出，将错误信息记入 `errors`。
    fn check(&mut self, errors: &mut Vec<String>) {
        let Some(pid) = self.pid else {
            return;
        };
        let mut raw = 0;
        // SAFETY: `pid` 来自本实例 `Relay::new` 创建且尚未回收的直接子进程，`raw` 为栈上有效的可写 `c_int` 变量。
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

    /// 判断辅助子进程是否已在未正常排空的情况下异常终止（此时主进程应停止向转发队列追加数据，仅继续排空被测子进程管道）。
    fn failed(&self) -> bool {
        self.pid.is_none() && (self.status != Some(0) || self.writers.iter().any(Option::is_some))
    }

    /// 强制终止并阻塞回收辅助子进程（用于超时到期或 `Drop` 清理，防止外部终端阻塞导致辅助进程残留）。
    fn stop(&mut self) -> io::Result<()> {
        self.close_inputs();
        let Some(pid) = self.pid else {
            return Ok(());
        };
        // SAFETY: `self.pid` 为 `Some` 保证该辅助子进程尚未被 `waitpid` 回收，PID 仍有效且未发生重用；辅助进程内部不创建后代，直接向 `pid` 发送 `SIGKILL` 即可终止。
        if unsafe { libc::kill(pid, libc::SIGKILL) } == -1 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::ESRCH) {
                return Err(error);
            }
        }
        loop {
            let mut raw = 0;
            // SAFETY: `pid` 仍由当前 `Relay` 实例独占，直到本次阻塞 `waitpid` 成功返回后才将 `self.pid` 置为 `None`；`raw` 为栈上可写的 `c_int`。
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
    /// 析构时确保关闭转发管道并终止、回收辅助子进程，避免后续命令启动失败时遗留孤儿或僵尸进程。
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

/// 输出转发辅助子进程的主循环函数（在 `fork` 子分支中运行，永不返回，最终调用 `_exit` 退出）。
///
/// # Safety
/// 1. 必须在 `fork()` 返回 `0` 的子进程上下文中调用，且调用后不得再返回 Rust 栈帧执行任何析构函数。
/// 2. `writers` 必须是继承自父进程的两条转发管道写端描述符，子进程进入本函数后会立即将其关闭。
/// 3. `outputs` 必须是有效的目标输出文件描述符（如外部 `stdout`/`stderr`），在辅助进程存活期间保持有效。
/// 4. `fds` 与 `buffers` 必须是子进程栈上独占的可写数组引用，`errno` 必须指向当前线程有效的 `errno` 存储单元。
unsafe fn relay_child(
    writers: [RawFd; 2],
    outputs: [RawFd; 2],
    fds: &mut [libc::pollfd; 2],
    buffers: &mut [[u8; 4096]; 2],
    errno: *mut libc::c_int,
) -> ! {
    for fd in writers {
        // SAFETY: `fd` 是 `fork` 复制到子进程描述符表中的管道写端副本；子进程只读不写，立即关闭副本以便父进程关闭写端时能正常触发 EOF。
        unsafe {
            libc::close(fd);
        }
    }
    let mut open = 2;
    while 0 < open {
        // SAFETY: `fds` 是栈上长度为 2 的有效 `pollfd` 数组；传入 `-1` 表示无限期等待，直到管道可读、EOF 或被父进程 `SIGKILL` 终止。
        if unsafe { libc::poll(fds.as_mut_ptr(), 2, -1) } == -1 {
            // SAFETY: `errno` 指向当前子进程有效的线程本地错误码变量，紧接在 `poll` 返回 -1 后读取。
            if unsafe { *errno } == libc::EINTR {
                continue;
            }
            // SAFETY: `_exit` 是异步信号安全的系统调用，直接终止子进程而不触碰父进程继承来的 Rust 堆或对象析构。
            unsafe {
                libc::_exit(1);
            }
        }
        for index in 0..2 {
            if fds[index].fd == -1 || fds[index].revents == 0 {
                continue;
            }
            // SAFETY: `fds[index].fd` 在前面已检查不为 -1（尚未关闭），`buffers[index]` 是栈上 4096 字节的可写数组，指针与长度完全匹配。
            let count = unsafe {
                libc::read(
                    fds[index].fd,
                    buffers[index].as_mut_ptr().cast(),
                    buffers[index].len(),
                )
            };
            if count == 0 {
                // SAFETY: `read` 返回 0 表示该条转发管道已达 EOF，关闭子进程持有的管道读端描述符并在下一行将其置为 -1，防止重复关闭。
                unsafe {
                    libc::close(fds[index].fd);
                }
                fds[index].fd = -1;
                open -= 1;
                continue;
            }
            if count < 0 {
                // SAFETY: `errno` 指向当前线程有效的错误码地址，读取刚才 `read` 失败的原因。
                if unsafe { *errno } == libc::EINTR {
                    continue;
                }
                // SAFETY: 发生不可恢复的管道读取错误，直接调用异步信号安全的 `_exit(1)` 终止辅助进程。
                unsafe {
                    libc::_exit(1);
                }
            }
            let mut offset = 0;
            while offset < count as usize {
                // SAFETY: 循环条件 `offset < count as usize` 且 `count <= buffers[index].len()` 保证了切片 `buffers[index][offset..]` 在有效范围内，剩余字节数 `count as usize - offset` 精确无越界。
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
                    // SAFETY: 紧接在 `write` 返回负数后读取当前线程有效的 `errno` 指针。
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
                        // SAFETY: `output` 是栈上单个有效的 `pollfd` 结构，等待目标输出描述符恢复可写状态。
                        if unsafe { libc::poll(&mut output, 1, -1) } != -1 {
                            continue;
                        }
                        // SAFETY: 紧接在 `poll` 返回 -1 后解引用当前线程有效的 `errno` 指针检查是否为 `EINTR`。
                        if unsafe { *errno } == libc::EINTR {
                            continue;
                        }
                    }
                }
                // SAFETY: 遇到非 `EINTR`/`EAGAIN` 的写入失败或零字节写入，直接调用 `_exit(1)` 终止子进程，不执行任何 Rust 清理逻辑。
                unsafe {
                    libc::_exit(1);
                }
            }
        }
    }
    // SAFETY: 两条转发管道均已读到 EOF 且全部字节已完整写入目标描述符，调用 `_exit(0)` 正常结束辅助子进程。
    unsafe {
        libc::_exit(0);
    }
}

/// 子进程输出流通道类型，只用于同一套流控代码区分两路输出与拼装错误文本。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Stream {
    /// 标准输出流（文件描述符 1）。
    Stdout,
    /// 标准错误流（文件描述符 2）。
    Stderr,
}

/// 主进程内部用于单个输出流（`stdout` 或 `stderr`）的非阻塞读取、TAP 观察与背压转发通道。
struct Flow {
    /// 来自被测子进程的管道读端原始描述符（已设为 `O_NONBLOCK`）。
    input: RawFd,
    /// 通往 `Relay` 辅助子进程的管道写端原始描述符（已设为 `O_NONBLOCK`）。
    output: RawFd,
    /// 标识当前通道属于标准输出（`Stdout`）还是标准错误（`Stderr`）。
    kind: Stream,
    /// 被测子进程管道读端是否已达 EOF（或因读取错误而关闭读取）。
    eof: bool,
    /// 向 `Relay` 管道写入是否已发生失败（若失败则丢弃后续待转发字节，但继续读取被测子进程输出以避免子进程写管道阻塞）。
    failed: bool,
    /// 暂存已从被测子进程读出、但尚未写入 `Relay` 管道的字节环形队列（上限约 64 KiB）。
    queue: VecDeque<u8>,
}

impl Flow {
    /// 为指定的输出流创建预分配 64 KiB 容量的非阻塞流控通道。
    fn new(input: RawFd, output: RawFd, kind: Stream) -> Self {
        Self {
            input,
            output,
            kind,
            eof: false,
            failed: false,
            queue: VecDeque::with_capacity(65536),
        }
    }

    /// 从被测子进程管道非阻塞读取至多 8 KiB 数据，标准输出字节送入 `tap` 解析，并在转发未失效时压入待写队列。
    ///
    /// 当队列积压超过 56 KiB（`57344` 字节）时暂停读取，形成背压以限制主进程内存占用；每次最多读取一块（8 KiB），防止高频输出饿死超时检查或另一条流。
    fn read(&mut self, tap: Option<&mut TapObserver>, errors: &mut Vec<String>) {
        if self.eof || 57344 < self.queue.len() {
            return;
        }
        let mut bytes = [0u8; 8192];
        // SAFETY: `self.input` 由调用栈 `pump` 中的 `stdout`/`stderr` `ChildStdout`/`ChildStderr` 保活且有效；`bytes` 是栈上 8192 字节的可写数组。
        let count = unsafe { libc::read(self.input, bytes.as_mut_ptr().cast(), bytes.len()) };
        if count == 0 {
            self.eof = true;
        } else if 0 < count {
            let bytes = &bytes[..count as usize];
            if self.kind == Stream::Stdout {
                if let Some(tap) = tap {
                    tap.feed(bytes);
                }
            }
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

    /// 将队列头部的连续字节切片非阻塞写入 `Relay` 管道；若发生短写则仅移除已写出的前缀，若写入出错则清空队列并标记 `failed`。
    fn forward(&mut self, errors: &mut Vec<String>) {
        let (bytes, _) = self.queue.as_slices();
        if bytes.is_empty() {
            return;
        }
        // SAFETY: `self.output` 由 `pump` 中的 `Relay` 实例保活且有效；`bytes` 为 `VecDeque::as_slices` 返回的非空连续可读字节切片，指针与长度匹配。
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

    /// 生成当前流用于 `poll` 等待的两个描述符项：`[读入端 pollfd, 写出端 pollfd]`。
    ///
    /// 若读端已 EOF 或缓冲队列处于高水位背压状态，则读端 `fd` 置为 `-1`（暂不监听可读）；若队列为空，则写端 `fd` 置为 `-1`（暂不监听可写）。
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

/// 向被测子进程所在的整个进程组（PGID 等于子进程 PID）发送指定信号。
///
/// 子进程在 `pre_exec` 中已调用 `setsid()` 成为新会话与进程组的组长，因此向 `-pid` 发送信号可一并终止其派生的全部后代进程；若返回 `ESRCH` 说明进程组已无成员可接收信号，忽略该错误。主动 `setsid` 脱离的后代不属于该进程组，本函数不会触及。
fn signal_group(child: &Child, signal: i32, errors: &mut Vec<String>) {
    // SAFETY: `child.id()` 是当前尚未回收的被测子进程正整数 PID；取负值 `-(child.id() as i32)` 符合 POSIX `kill` 向对应进程组广播信号的语义。
    if unsafe { libc::kill(-(child.id() as i32), signal) } == -1 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            errors.push(format!("kill process group signal={signal}: {error}"));
        }
    }
}

/// 用 `waitid(P_PID, pid, WEXITED | WNOHANG | WNOWAIT)` 观察组首是否已退出，但不回收。
///
/// 不回收使已退出的组首继续保持僵尸态，进程组号在其残留成员清空前不会被内核复用，后续向整组发信号不会打到别的进程组。
/// 返回 `Ok(true)` 表示组首已退出，`Ok(false)` 表示仍在运行。
fn child_exited(pid: libc::pid_t) -> io::Result<bool> {
    // 无事件时 `WNOHANG` 不保证内核写回缓冲区，因此先清零再交给 `waitid`。
    let mut info = std::mem::MaybeUninit::<libc::siginfo_t>::zeroed();
    // SAFETY: `info` 是栈上按 `siginfo_t` 布局对齐的可写缓冲区，指针在 `waitid` 返回前不与任何读取重叠；`pid` 是本进程尚未回收的直接子进程，`WNOHANG` 保证调用不阻塞。
    let result = unsafe {
        libc::waitid(
            libc::P_PID,
            pid as libc::id_t,
            info.as_mut_ptr(),
            libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
        )
    };
    if result == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `info` 在调用前已整块清零；`waitid` 返回 0 时要么写入子进程事件，要么保持全零，结构体始终处于已初始化状态。
    let info = unsafe { info.assume_init() };
    // SAFETY: `info` 已初始化；`si_pid()` 读取子进程事件字段，空事件下为零。
    Ok(unsafe { 0 < info.si_pid() })
}

/// 在统一截止时间 `deadline` 下驱动被测子进程与可选 `Relay` 辅助进程的事件循环，完成输出采集、转发、超时终止与进程组收尾。
///
/// `tap` 与 `relay` 只在自测路径同时存在：此时接管子进程的 stdout/stderr 管道并转发；比赛路径两者均为 `None`，组首一退出即收尾。
/// 返回被测子进程的最终 `ExitOutcome`，运行期间发生的管道、信号或辅助进程错误追加到 `errors` 列表中。
fn pump(
    child: &mut Child,
    deadline: Instant,
    mut tap: Option<&mut TapObserver>,
    mut relay: Option<Relay>,
    errors: &mut Vec<String>,
) -> ExitOutcome {
    // Step 1: 自测路径接管被测子进程的 stdout/stderr 管道，设其非阻塞并绑定到双通道 Flow；失败时强杀整组并回收。
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
            Flow::new(stdout.as_raw_fd(), outputs[0], Stream::Stdout),
            Flow::new(stderr.as_raw_fd(), outputs[1], Stream::Stderr),
        ])
    } else {
        None
    };
    let pid = child.id() as libc::pid_t;
    // Step 2: 进入主事件循环，按单调时钟推进正常排空、超时 SIGTERM 宽限期与 SIGKILL 强制终止阶段。
    let mut exited = false;
    let mut timed_out = false;
    let mut terminate_at = None;
    let mut kill_at = None;
    let mut wait_error = None;
    loop {
        let now = Instant::now();
        if !timed_out
            && deadline <= now
            && exited
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
        // Step 3: 非阻塞观察组首是否已退出；`WNOWAIT` 只报告事件，不回收，组号继续由僵尸组首占住。
        if !exited && wait_error.is_none() {
            match child_exited(pid) {
                Ok(found) => exited = found,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => {
                    errors.push(format!("waitid: {error}"));
                    wait_error = Some(error);
                    signal_group(child, libc::SIGKILL, errors);
                    kill_at = Some(now);
                }
            }
        }
        // Step 4: 检查 Relay 辅助进程状态，并在两条流上执行非阻塞转发与读取。
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
                flow.read(tap.as_deref_mut(), errors);
                flow.forward(errors);
            }
        }
        // Step 5: 若两条输出流均已 EOF 且队列排空，关闭 Relay 输入端并检查是否满足循环结束条件。
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
        if exited && drained && relay_done && (!timed_out || kill_at.is_some()) {
            break;
        }
        // Step 6: 若已到达强制终止期限，记录未排空输出并终止 Relay，转入统一收尾。
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
            break;
        }
        // Step 7: 构造下一轮 poll 描述符集，按距离最近事件点（最多 50ms）进行有界阻塞等待。
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
        // SAFETY: `fds` 是栈上长度为 4 的可写 `pollfd` 数组（未激活的槽位 `fd` 为 -1，内核会直接忽略），`millis` 为限制在 `0..=50` 范围内的超时毫秒数。
        if unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, millis) } == -1 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                errors.push(format!("poll: {error}"));
                signal_group(child, libc::SIGKILL, errors);
                kill_at = Some(Instant::now());
            }
        }
    }
    // Step 8: 组首此时尚未回收，先向整组补发一次 SIGKILL 让残留后台进程退出，再回收组首并据此生成退出分类。
    signal_group(child, libc::SIGKILL, errors);
    let raw = match child.wait() {
        Ok(status) => Some(status.into_raw()),
        Err(error) => {
            errors.push(format!("wait child: {error}"));
            wait_error = Some(error);
            None
        }
    };
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

/// 启动并等待一条命令，返回进程事实记录。
///
/// 标准输入/输出/错误继承父进程，环境表先清空再只装入 `env`；`root` 为 `Some` 时子进程在 `exec` 前 `chroot` 到该目录，
/// 之后 `chdir` 到 `cwd` 并建立独立进程组。命令输出原样出现在本进程的终端上，本函数不捕获、不解析也不转发。
///
/// 空白 `argv` 或空程序名不派生子进程，直接返回 `SpawnError`（`EINVAL`）。
pub fn run_command(
    root: Option<&Path>,
    cwd: &Path,
    argv: &[String],
    env: &BTreeMap<String, String>,
    timeout: Duration,
) -> Execution {
    // Step 1: 计算单调时钟截止时间并准备错误收集器。
    let started = Instant::now();
    let deadline = started.checked_add(timeout);
    let mut io_errors = Vec::new();
    // Step 2: 构造继承全部标准流的命令，派生子进程后进入 pump 循环直至退出或期限到达。
    let outcome = match deadline {
        None => ExitOutcome::SpawnError {
            errno: Some(libc::EINVAL),
            message: "deadline exceeds monotonic clock range".into(),
        },
        Some(deadline) => match command(root, cwd, argv) {
            Err(outcome) => outcome,
            Ok(mut command) => {
                command.env_clear().envs(env);
                match command.spawn() {
                    Ok(mut child) => pump(&mut child, deadline, None, None, &mut io_errors),
                    Err(error) => ExitOutcome::SpawnError {
                        errno: error.raw_os_error(),
                        message: error.to_string(),
                    },
                }
            }
        },
    };
    Execution {
        outcome,
        io_errors,
        elapsed: started.elapsed(),
    }
}

/// 启动并等待一条 TAP 自测命令，返回进程事实记录与解析结果。
///
/// 不 `chroot`；环境表沿用父进程并叠加 `env`（同名变量覆盖）。子进程的标准输出/错误由本进程经管道读取，标准输出字节喂给
/// `TapObserver`，两路字节经 `Relay` 转发到 `outputs`。管道读取、转发或辅助进程回收产生任何 I/O 错误时，
/// 判定改为 [`Verdict::InfrastructureError`]。
pub fn run_tap_command(
    cwd: &Path,
    argv: &[String],
    env: &BTreeMap<String, String>,
    timeout: Duration,
    outputs: [RawFd; 2],
) -> (Execution, ProtocolResult) {
    // Step 1: 计算截止时间并准备 TAP 观察器与错误收集器。
    let started = Instant::now();
    let deadline = started.checked_add(timeout);
    let mut observer = TapObserver::new();
    let mut io_errors = Vec::new();
    // Step 2: 构造捕获输出流的命令与 Relay 转发进程，派生子进程后进入 pump 循环直至退出或期限到达。
    let outcome = match deadline {
        None => ExitOutcome::SpawnError {
            errno: Some(libc::EINVAL),
            message: "deadline exceeds monotonic clock range".into(),
        },
        Some(deadline) => match command(None, cwd, argv) {
            Err(outcome) => outcome,
            Ok(mut command) => {
                command
                    .envs(env)
                    .stdout(Stdio::piped())
                    .stderr(Stdio::piped());
                match Relay::new(outputs) {
                    Err(error) => ExitOutcome::IoError {
                        errno: error.raw_os_error(),
                        message: format!("create output relay: {error}"),
                        raw: None,
                    },
                    Ok(relay) => match command.spawn() {
                        Ok(mut child) => pump(
                            &mut child,
                            deadline,
                            Some(&mut observer),
                            Some(relay),
                            &mut io_errors,
                        ),
                        Err(error) => ExitOutcome::SpawnError {
                            errno: error.raw_os_error(),
                            message: error.to_string(),
                        },
                    },
                }
            }
        },
    };
    // Step 3: 闭合 TAP 观察器得出最终判定；管道层出现过任何 I/O 错误时按基础设施错误上报。
    let mut result = observer.finish(&outcome);
    if !io_errors.is_empty() {
        result.verdict = Verdict::InfrastructureError;
    }
    (
        Execution {
            outcome,
            io_errors,
            elapsed: started.elapsed(),
        },
        result,
    )
}
