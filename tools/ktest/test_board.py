"""普通 PTY 与本地 loader 的宿主边界回归，不代表实体板 reset、UART 或停机验证。"""

from contextlib import contextmanager, redirect_stderr
import io
import json
import os
from pathlib import Path
import pty
import subprocess
import sys
import tempfile
import time
import unittest

from host_helpers import CLI, TOOLS, case_lines, case_record, parser_for
from ktest.board import Board, SerialPort, validate_resources
from ktest.catalog import HostError
from ktest.runner import CANCELLED, DeadlineExpired, VMExited, finish_vm


@contextmanager
def endpoint():
    """持有 PTY master，测试只消费真实 tty fd 的锁、期限及断线语义。"""
    master, slave = pty.openpty()
    path = Path(os.ttyname(slave))
    os.close(slave)
    try:
        yield master, path
    finally:
        try:
            os.close(master)
        except OSError:
            pass


def board_args(path, loader, *extra):
    """使用正式 CLI 解析板输入，不自行构造第二套参数默认值。"""
    return CLI.arguments(["run", "--arch", "riscv64", "--platform", "visionfive2",
                          "--serial", str(path), "--loader", str(loader), *extra])


@contextmanager
def observer(directory, path):
    """仅建真实串口观察者，不伪造 loader 成功或 QEMU process。"""
    args = board_args(path, Path(sys.executable))
    args.board_serial = SerialPort(path, args.baud)
    board = None
    try:
        board = Board(args, directory / "kernel.elf", directory, set())
        yield board
    finally:
        if board is not None:
            board.close()
        args.board_serial.close()


def complete_parser():
    """为 late-control 边界建立已完成的正式 parser 状态，不替代串口生命周期。"""
    case = case_record()
    parser = parser_for([case])
    for line in [*case_lines(case), "ok 1 os::tests0", "# KTEST complete"]:
        parser.feed(line)
    return parser


class BoardBoundaryTests(unittest.TestCase):
    """只覆盖消费者可见的不确定边界；PTY 没有物理 UART 背压或板电源语义。"""

    def tearDown(self):
        """取消状态不能污染其他宿主回归。"""
        CANCELLED.clear()

    def test_cli_rejects_invalid_transport_combinations(self):
        """不合法架构、并发、baud 和 QEMU 控制在任何设备动作前拒绝。"""
        base = ["run", "--arch", "riscv64", "--platform", "visionfive2",
                "--serial", "/dev/unused", "--loader", sys.executable]
        invalid = (["--jobs", "2"], ["--baud", "12345"], ["--qemu", "qemu"],
                   ["--ap-freeze"], ["--modern"], ["--disk", "root=unused"],
                   ["--platform", "ls2k1000la"], ["--bootargs", "root=x\nrun"])
        for extra in invalid:
            with self.subTest(extra=extra), redirect_stderr(io.StringIO()), self.assertRaises(SystemExit) as error:
                CLI.arguments(base + extra)
            self.assertEqual(error.exception.code, 2)
        with redirect_stderr(io.StringIO()), self.assertRaises(SystemExit):
            CLI.arguments(["list", "--arch", "riscv64", "--platform", "visionfive2"])
        args = board_args(Path("/dev/unused"), Path(sys.executable))
        for resource in ("scratch", "net-user", "ap-freeze", "virtio-access-platform", "root"):
            with self.subTest(resource=resource), self.assertRaises(HostError):
                validate_resources({resource}, args)

    def test_serial_exclusion_survives_other_process_and_releases(self):
        """另一进程的正式端口拥有者被拒绝，关闭后普通 tty 能再次打开。"""
        with endpoint() as (_, path):
            owner = SerialPort(path, 115200)
            try:
                attempt = subprocess.run([sys.executable, "-c",
                    "import sys; sys.path.insert(0,sys.argv[1]); "
                    "from ktest.board import SerialPort; SerialPort(sys.argv[2],115200)", str(TOOLS), str(path)],
                    stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=False)
                self.assertNotEqual(attempt.returncode, 0)
                with self.assertRaises(OSError):
                    SerialPort(path, 115200)
            finally:
                owner.close()
            reopened = SerialPort(path, 115200)
            reopened.close()

    def test_deadline_cancel_and_eof_do_not_claim_guest_stop(self):
        """无字节超时、主动取消与真实断线都不生成停止终点或伪进程。"""
        with tempfile.TemporaryDirectory() as temporary, endpoint() as (master, path):
            directory = Path(temporary)
            with observer(directory, path) as board:
                with self.assertRaises(DeadlineExpired):
                    board.line(time.monotonic() + 0.03)
                CANCELLED.set()
                with self.assertRaisesRegex(HostError, "cancelled"):
                    board.line(time.monotonic() + 1)
                CANCELLED.clear()
                os.close(master)
                with self.assertRaises(VMExited):
                    board.line(time.monotonic() + 1)
                self.assertIsNone(board.terminal)
                self.assertFalse(hasattr(board, "process"))
            evidence = json.loads((directory / "board-observation.json").read_text())
            self.assertIsNone(evidence["stop_decision"])
            self.assertFalse(evidence["physical_poweroff_confirmed"])

    def test_final_requires_terminal_and_rejects_late_control(self):
        """完整 KTAP 仍需真实终点；终点前后迟到控制均不能藏在 Pass 后面。"""
        marker = b"# diagnostic: board-stop platform=visionfive2 reason=success scope=all-online\n"
        for payload, error in ((b"", DeadlineExpired),
                               (b"  # KTEST fatal 0 panic\n" + marker, HostError),
                               (marker + b"not ok 2 late\n", HostError),
                               (marker.replace(b"all-online", b"current-cpu"), HostError),
                               (b"# diagnostic: board-stop platform=visionfive2 reason=failure scope=current-cpu\n",
                                HostError)):
            with self.subTest(payload=payload), tempfile.TemporaryDirectory() as temporary, endpoint() as (master, path):
                directory = Path(temporary)
                with observer(directory, path) as board:
                    if payload:
                        os.write(master, payload)
                    with self.assertRaises(error):
                        finish_vm(board, time.monotonic() + 0.05, complete_parser())

    def test_terminal_keeps_observing_and_eof_is_not_poweroff(self):
        """只有完整观察窗结束才接受软件终点；提前 EOF 不能替代该窗口。"""
        marker = b"# diagnostic: board-stop platform=visionfive2 reason=success scope=all-online\n"
        with tempfile.TemporaryDirectory() as temporary, endpoint() as (master, path):
            directory = Path(temporary)
            with observer(directory, path) as board:
                os.write(master, marker)
                deadline = time.monotonic() + 0.04
                finish_vm(board, deadline, complete_parser())
                self.assertLessEqual(deadline, time.monotonic())
                self.assertTrue(board.observed_until_deadline)
                self.assertEqual(board.terminal, "success")
                self.assertEqual(board.terminal_scope, "all-online")
        with tempfile.TemporaryDirectory() as temporary, endpoint() as (master, path):
            directory = Path(temporary)
            with observer(directory, path) as board:
                board.buffer.extend(marker)
                os.close(master)
                with self.assertRaisesRegex(HostError, "observation completed"):
                    finish_vm(board, time.monotonic() + 1, complete_parser())

    def test_each_fresh_load_executes_loader_with_current_image_and_bootargs(self):
        """本地 loader 实际重写装载目标；只重建 Python 对象无法恢复被改动的目标。"""
        with tempfile.TemporaryDirectory() as temporary, endpoint() as (_, path):
            root = Path(temporary)
            script = root / "loader.py"
            script.write_text(
                "import json,os,pathlib,sys\n"
                "target=pathlib.Path(sys.argv[1])\n"
                "request=json.loads(pathlib.Path(sys.argv[sys.argv.index('--request')+1]).read_text())\n"
                "fd=int(sys.argv[sys.argv.index('--serial-fd')+1])\n"
                "assert os.isatty(fd)\n"
                "target.unlink(missing_ok=True)\n"
                "target.write_bytes(pathlib.Path(request['kernel']).read_bytes())\n"
                "with target.with_suffix('.jsonl').open('a') as log: log.write(json.dumps(request)+'\\n')\n",
                encoding="utf-8")
            target = root / "loaded"
            kernel = root / "kernel.elf"
            kernel.write_bytes(b"current fixed ELF bytes")
            args = board_args(path, Path(sys.executable), "--loader-arg=" + str(script),
                              "--loader-arg=" + str(target), "--root", "/dev/ram0",
                              "--bootargs", "console=ttyS0")
            args.board_serial = SerialPort(path, args.baud)
            try:
                for index in range(2):
                    directory = root / str(index)
                    directory.mkdir()
                    target.write_bytes(b"state left by previous guest")
                    board = Board(args, kernel, directory, {"root"})
                    try:
                        board.connect(time.monotonic() + 2)
                        self.assertEqual(target.read_bytes(), kernel.read_bytes())
                        with self.assertRaisesRegex(HostError, "already invoked"):
                            board.connect(time.monotonic() + 2)
                    finally:
                        board.close()
                requests = [json.loads(line) for line in target.with_suffix('.jsonl').read_text().splitlines()]
                self.assertEqual(len(requests), 2)
                self.assertEqual([row["kernel"] for row in requests], [str(kernel)] * 2)
                self.assertEqual([row["bootargs"] for row in requests],
                                 ["ro rootdir=/ root=/dev/ram0 console=ttyS0"] * 2)
            finally:
                args.board_serial.close()

    def test_loader_failure_and_deadline_never_become_guest_exit(self):
        """真实 loader 非零退出或超时都保留未完成状态，不生成 guest 终态。"""
        for code, expected in (("import os; os._exit(7)", HostError),
                               ("import time; time.sleep(30)", DeadlineExpired)):
            with self.subTest(code=code), tempfile.TemporaryDirectory() as temporary, endpoint() as (_, path):
                directory = Path(temporary)
                args = board_args(path, Path(sys.executable), "--loader-arg=-c", "--loader-arg=" + code)
                args.board_serial = SerialPort(path, args.baud)
                board = Board(args, directory / "kernel.elf", directory, set())
                try:
                    limit = 2 if expected is HostError else 0.03
                    with self.assertRaises(expected):
                        board.connect(time.monotonic() + limit)
                    self.assertFalse(board.loaded)
                    self.assertIsNone(board.terminal)
                    self.assertFalse(hasattr(board, "process"))
                finally:
                    board.close()
                    args.board_serial.close()


if __name__ == "__main__":
    unittest.main()
