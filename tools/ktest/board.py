"""有限板 transport：POSIX 串口独占、显式 reset/reload 与同一 ktest 协议。

loader 是本地可执行文件，每次 catalog/batch 都调用：
  LOADER [--loader-arg ...] --request ABS_JSON --serial-fd FD
JSON 保存本轮固定 kernel、arch/platform/cpus、bootargs 和资源名。loader 必须
实际 reset、加载该 ELF 并按 bootargs 启动；不得消费 guest READY，退出后不得
保留串口后代或更改串口配置。FD 是已独占、非阻塞的原始 8N1 串口，可用于固件交互。
本模块不附盘、不刷写固件/介质，也不将 loader 的退出码当成 guest 退出码。
"""

import errno
import fcntl
import json
import os
import signal
import stat
import subprocess
import select
import termios
import time

from .catalog import HostError, require
from .resources import regular_file, validate_resources as validate_common_resources
from .runner import UART, DeadlineExpired, VMExited

PLATFORMS = {"visionfive2": "riscv64", "ls2k1000la": "loongarch64"}


def validate_arguments(args):
    """在打开串口或构建前拒绝错误组合，QEMU 默认参数不改变。"""
    if args.platform == "qemu-virt":
        require(args.serial is None and args.loader is None and not args.loader_arg
                and args.baud is None and not args.bootargs, "board arguments require a board platform")
        return
    require(PLATFORMS.get(args.platform) == args.arch, "board platform architecture mismatch")
    require(args.platform != "ls2k1000la" or args.cpus <= 2, "ls2k1000la supports at most two CPUs")
    require(args.jobs == 1, "physical board requires --jobs 1")
    require(args.serial is not None and args.loader is not None,
            "board requires --serial and an explicit --loader reset/reload executable")
    require(not (args.qemu or args.modern or args.ap_freeze or args.gdb or args.iommu_platform),
            "QEMU external controls are unavailable on a physical board")
    require(not args.disk and not args.read_only, "board media must be provisioned explicitly; --disk is QEMU-only")
    require(not any(c in args.bootargs for c in "\r\n\0"), "invalid board boot arguments")
    require(all(token not in ("ro", "rw") and not token.startswith(("root=", "extra=", "rootdir="))
                for token in args.bootargs.split()),
            "use --root, --extra-root and --root-mode; board rootdir is /")
    for name in (args.root, args.extra_root):
        require(name is None or (name and not any(c.isspace() or c == "\0" for c in name)),
                "invalid board block name")
    args.baud = args.baud or 115200
    require(hasattr(termios, f"B{args.baud}"), "unsupported POSIX serial baud")
    args.loader = args.loader.resolve()
    regular_file(args.loader)
    require(os.access(args.loader, os.X_OK), "board loader must be executable")


def validate_resources(names, args):
    """仅允许真实串口和显式板根；QEMU peer、虚拟附盘及 scratch 布局不冒充板资源。"""
    unavailable = names & {"ap-freeze", "virtio-access-platform", "net-user", "scratch"}
    require(not unavailable, f"QEMU-only resources unavailable on board: {sorted(unavailable)}")
    require("root" not in names or args.root is not None, "board root resource requires explicit --root")
    require("extra" not in names or args.extra_root is not None,
            "board extra resource requires explicit --extra-root")
    validate_common_resources(names - {"root", "extra"}, args)


def validate_cases(items):
    """socket 停读可阻塞 QEMU TX；普通无流控 UART 不具备该外部控制能力。"""
    require(all(item.case["name"] !=
                "os::ktest::execution_services::klog::uart::socket_printer_backpressure"
                for item in items), "socket backpressure fixture is QEMU-only")


class SerialPort:
    """一个命令全程持有同一 tty 的 advisory lock 和内核独占，loader 只借继承 FD。"""

    def __init__(self, path, baud):
        """以无控制终端、非阻塞 8N1 打开普通串口，失败时恢复已取得的资源。"""
        self.fd = None
        self.saved = None
        self.exclusive = False
        try:
            self.fd = os.open(path, os.O_RDWR | os.O_NOCTTY | os.O_NONBLOCK | os.O_CLOEXEC)
            require(stat.S_ISCHR(os.fstat(self.fd).st_mode) and os.isatty(self.fd), "serial port must be a tty")
            fcntl.flock(self.fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
            fcntl.ioctl(self.fd, termios.TIOCEXCL)
            self.exclusive = True
            self.saved = termios.tcgetattr(self.fd)
            settings = termios.tcgetattr(self.fd)
            settings[0] = 0
            settings[1] = 0
            settings[2] = termios.CS8 | termios.CREAD | termios.CLOCAL
            settings[3] = 0
            settings[4] = settings[5] = getattr(termios, f"B{baud}")
            settings[6][termios.VMIN] = 1
            settings[6][termios.VTIME] = 0
            termios.tcsetattr(self.fd, termios.TCSANOW, settings)
            self.settings = termios.tcgetattr(self.fd)
        except BaseException as error:
            self.close()
            if isinstance(error, termios.error):
                raise HostError(f"serial configuration failed: {error}") from error
            raise

    def fileno(self):
        """让原 select 路径观察已独占的串口描述符。"""
        return self.fd

    def recv(self, length):
        """保留非阻塞和真实断线语义，不将暂时无字节变为 EOF。"""
        try:
            return os.read(self.fd, length)
        except OSError as error:
            if error.errno in (errno.EIO, errno.ENODEV):
                raise VMExited("board serial disconnected") from error
            raise

    def send(self, data):
        """只提交驱动接受的字节，未发送尾由共享绝对期限循环持有。"""
        return os.write(self.fd, data)

    def close(self):
        """释放宿主端口；恢复 tty 失败不妨碍 fd 关闭，也不声称板已停止。"""
        if self.fd is None:
            return
        try:
            if self.saved is not None:
                try:
                    termios.tcsetattr(self.fd, termios.TCSANOW, self.saved)
                except (OSError, termios.error):
                    pass
            if self.exclusive:
                try:
                    fcntl.ioctl(self.fd, termios.TIOCNXCL)
                except OSError:
                    pass
        finally:
            os.close(self.fd)
            self.fd = None


class BoardStopped(VMExited):
    """收到平台不可返回的停止决策诊断，并观察到清理绝对期限。"""


class Board(UART):
    """复用同一行/raw parser，不拥有虚构的 QEMU 进程或物理电源控制。"""

    def __init__(self, args, kernel, directory, names):
        """借全命令独占的串口，每个新批次保存独立 loader 输入和原始输出。"""
        validate_resources(names, args)
        self.args = args
        self.directory = directory
        self.serial = args.board_serial
        self.buffer = bytearray()
        self.rx_offset = self.tx_offset = 0
        self.rx_used = self.tx_used = False
        self.execution_uart = self.net_user = self.debugger = None
        self.terminal = None
        self.terminal_scope = None
        self.observed_until_deadline = False
        self.loaded = False
        self.raw = (directory / "uart.log").open("xb", buffering=0)
        self.request = directory / "loader-request.json"
        bootargs = [args.root_mode, "rootdir=/"]
        if args.root is not None:
            bootargs.append(f"root={args.root}")
        if args.extra_root is not None:
            bootargs.append(f"extra={args.extra_root}")
        if args.bootargs:
            bootargs.append(args.bootargs)
        try:
            self.request.write_text(json.dumps(dict(kernel=str(kernel.resolve()), arch=args.arch,
                platform=args.platform, cpus=args.cpus, serial=str(args.serial.resolve()), baud=args.baud,
                bootargs=" ".join(bootargs), resources=sorted(names)), indent=2) + "\n", encoding="utf-8")
        except BaseException:
            self.raw.close()
            raise

    def connect(self, deadline):
        """在启动期限内执行真正 reset/reload；只清输入或 Python 状态从不构成 fresh。"""
        require(not self.loaded, "board loader already invoked for this batch")
        self.check(deadline)
        try:
            termios.tcflush(self.serial.fileno(), termios.TCIOFLUSH)
        except termios.error as error:
            raise HostError(f"serial flush before reset/reload failed: {error}") from error
        command = [str(self.args.loader), *self.args.loader_arg, "--request", str(self.request),
                   "--serial-fd", str(self.serial.fileno())]
        (self.directory / "loader-command.json").write_text(json.dumps(command) + "\n", encoding="utf-8")
        with (self.directory / "loader.log").open("xb", buffering=0) as log:
            process = subprocess.Popen(command, stdin=subprocess.DEVNULL, stdout=log,
                stderr=subprocess.STDOUT, pass_fds=(self.serial.fileno(),), start_new_session=True)
            try:
                while process.poll() is None:
                    self.check(deadline)
                    try:
                        process.wait(timeout=max(0, min(0.05, deadline - time.monotonic())))
                    except subprocess.TimeoutExpired:
                        pass
                self.check(deadline)
                require(process.returncode == 0, f"board reset/reload failed: {process.returncode}")
                try:
                    settings = termios.tcgetattr(self.serial.fileno())
                except termios.error as error:
                    raise HostError(f"serial configuration after reset/reload failed: {error}") from error
                require(not os.get_blocking(self.serial.fileno()) and settings == self.serial.settings,
                        "board loader changed serial configuration")
                self.loaded = True
            finally:
                # loader 后代不得保活串口；这里只停止宿主装载进程组，不操作板电源。
                for sig in (signal.SIGTERM, signal.SIGKILL):
                    try:
                        os.killpg(process.pid, sig)
                    except ProcessLookupError:
                        pass
                process.wait()

    def line(self, deadline):
        """结果后仍观察到固定期限，平台停止诊断后的任何迟到正文都拒绝。"""
        while True:
            try:
                line = super().line(deadline)
            except DeadlineExpired:
                if self.terminal is None:
                    raise
                require(not self.buffer.strip(), "partial UART output after board stop decision")
                # 不延长期限；仍核对截至期限已排入驱动的尾字节，避免迟到失败留在 tty 队列。
                readable, _, _ = select.select([self.serial], [], [], 0)
                if readable:
                    try:
                        tail = self.serial.recv(65536)
                    except BlockingIOError:
                        tail = b""
                    if tail:
                        self.raw.write(tail)
                        raise HostError("UART bytes queued after board stop decision")
                self.observed_until_deadline = True
                raise BoardStopped(f"board stop decision observed: {self.terminal}") from None
            require(self.terminal is None or not line.strip(), "UART output after board stop decision")
            prefix = "# diagnostic: board-stop "
            if line.startswith(prefix):
                expected = f"{prefix}platform={self.args.platform} reason="
                terminals = {expected + "success scope=all-online": ("success", "all-online"),
                             expected + "failure scope=current-cpu": ("failure", "current-cpu")}
                require(line in terminals, "invalid board stop diagnostic")
                self.terminal, self.terminal_scope = terminals[line]
                continue
            return line

    def close(self):
        """仅结束本次观察；超时、EOF 和取消不证明 guest 已清理或物理断电。"""
        self.raw.close()
        (self.directory / "board-observation.json").write_text(json.dumps(dict(
            platform=self.args.platform, loader_completed=self.loaded, stop_decision=self.terminal,
            stop_scope=self.terminal_scope,
            observed_until_deadline=self.observed_until_deadline,
            physical_poweroff_confirmed=False, observation="ended"), indent=2) + "\n", encoding="utf-8")
