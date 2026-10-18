"""拥有 QEMU、调试器、工作盘、网络 peer 与一个独立 UART 通道。"""

import errno
import json
import os
from pathlib import Path
import signal
import socket
import subprocess
import tempfile
import threading
import time

from . import ap_freeze
from .catalog import require
from .net_user import NetUserPeer
from .protocol import ready
from .resources import (build_test_disk_arguments, build_test_network_arguments,
                        prepare_disks)
from .uart_channel import DeadlineExpired, UartChannel, VMExited

CANCELLED = threading.Event()


class QemuTestSession:
    """独占每次测试的进程与临时资源，所有串口操作由 uart 承担。"""

    def check(self, deadline):
        """日志数量和字节进度都不改变固定截止时刻。"""
        require(not CANCELLED.is_set(), "host run cancelled")
        if deadline <= time.monotonic():
            raise DeadlineExpired("absolute host deadline expired")
        if self.debugger is not None:
            require(self.debugger.poll() in (None, 0), "AP freeze GDB failed; inspect ap-freeze-gdb.log")


    def handshake(self, config, command, deadline):
        """唯一早期 READY 之后只发送一次 catalog 或 run 命令。"""
        self.connect(deadline)
        while True:
            line = self.uart.line(deadline)
            if line.startswith("KTEST READY"):
                ready(line, config)
                self.uart.send(command, deadline)
                return


    def verify_resources(self, deadline):
        """真实退出后核对 UART、网络 peer 原字节及本 VM 的冻结现场。"""
        self.uart.verify_consumed()
        if self.debugger is not None:
            try:
                self.debugger.wait(timeout=max(0, deadline - time.monotonic()))
            except subprocess.TimeoutExpired as error:
                raise DeadlineExpired("AP freeze debugger exceeded cleanup deadline") from error
            require(self.debugger.returncode == 0, "AP freeze GDB failed; inspect ap-freeze-gdb.log")
            ap_freeze.verify(self.directory)
        peer = getattr(self, "net_user", None)
        if peer is not None:
            peer.verify(deadline)


    def __init__(self, args, kernel, directory, names):
        """独占盘和 live peer 在启动前准备，QEMU 真正 wait 后才释放。"""
        self.args = args
        self.directory = directory
        self.process = None
        self.uart = None
        self._closed = False
        self.net_user = None
        self.debugger = None
        self.debug_log = None
        self.gdb_command = None
        self.socket_directory = tempfile.TemporaryDirectory(prefix="ktest-uart-")
        self.endpoint = str(Path(self.socket_directory.name) / "uart.sock")
        self.console = None
        self.disk_directory = None
        try:
            self.uart = UartChannel(args, directory, self.check)
            self.console = (directory / "qemu.log").open("xb", buffering=0)
            self.disk_directory = tempfile.TemporaryDirectory(prefix="disks-", dir=directory)
            disks = prepare_disks(names, args, Path(self.disk_directory.name))
            if "net-user" in names:
                self.net_user = NetUserPeer(directory)
            command = [args.qemu or f"qemu-system-{args.arch}", "-machine", "virt", "-accel", "tcg",
                       "-kernel", str(kernel), "-m", args.memory, "-smp", str(args.cpus),
                       "-display", "none", "-monitor", "none", "-no-reboot",
                       "-chardev", f"socket,id=ktest,path={self.endpoint},server=on,wait=on",
                       "-serial", "chardev:ktest"]
            if args.arch == "riscv64":
                command += ["-bios", "default"]
                if args.modern:
                    command += ["-global", "virtio-mmio.force-legacy=false"]
            command += build_test_network_arguments(
                args.arch, "user" if self.net_user is not None else "none", None)
            ports = self.net_user.ports if self.net_user is not None else None
            command += build_test_disk_arguments(
                disks, architecture=args.arch, read_only_roles=args.read_only,
                logical_block_size=args.logical_block_size,
                virtio_access_platform="virtio-access-platform" in names,
                root_device=args.root, root_mode=args.root_mode,
                extra_root_device=args.extra_root, network_peer_ports=ports)
            if "ap-freeze" in names and 2 <= args.cpus:
                endpoint = str(Path(self.socket_directory.name) / "gdb.sock")
                command += ["-chardev", f"socket,id=ktest-gdb,path={endpoint},server=on,wait=off",
                            "-gdb", "chardev:ktest-gdb", "-S"]
                self.gdb_command = ap_freeze.command(args, kernel, directory, endpoint)
                (directory / "gdb-command.json").write_text(json.dumps(self.gdb_command) + "\n", encoding="utf-8")
                self.debug_log = (directory / "ap-freeze-gdb.log").open("xb", buffering=0)
            (directory / "command.json").write_text(json.dumps(command) + "\n", encoding="utf-8")
            self.process = subprocess.Popen(command, stdin=subprocess.DEVNULL, stdout=self.console,
                                            stderr=subprocess.STDOUT, start_new_session=True)
        except BaseException:
            self.close()
            raise


    def connect(self, deadline):
        """在同一个启动绝对期限内等待真实 Unix UART 端点。"""
        self.uart.serial = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        while True:
            self.check(deadline)
            if self.process.poll() is not None:
                raise VMExited(f"QEMU exited {self.process.returncode}")
            try:
                self.uart.serial.connect(self.endpoint)
                self.uart.serial.setblocking(False)
                break
            except OSError as error:
                if error.errno not in (errno.ENOENT, errno.ECONNREFUSED):
                    raise
                time.sleep(min(0.01, max(0, deadline - time.monotonic())))
        if self.gdb_command is not None:
            self.debugger = subprocess.Popen(self.gdb_command, stdin=subprocess.DEVNULL,
                                             stdout=self.debug_log, stderr=subprocess.STDOUT,
                                             start_new_session=True)


    def close(self):
        """先真正 wait 进程，再取消并 join peer，最后关闭句柄及工作盘。"""
        if self._closed:
            return
        for process in (self.debugger, self.process):
            if process is None:
                continue
            try:
                os.killpg(process.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
            try:
                process.wait(timeout=2)
            except subprocess.TimeoutExpired:
                pass
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
            process.wait()
        peer = getattr(self, "net_user", None)
        if peer is not None:
            peer.close()
        if self.uart is not None:
            self.uart.close()
        if self.console is not None:
            self.console.close()
        if self.debug_log is not None:
            self.debug_log.close()
        self.socket_directory.cleanup()
        if self.disk_directory is not None:
            self.disk_directory.cleanup()

        self._closed = True
