"""共用 UART 协议与绝对期限，分别拥有 QEMU 进程或有限板观察生命周期。"""

from concurrent.futures import ThreadPoolExecutor, as_completed
import errno
import json
import os
from pathlib import Path
import select
import signal
import socket
import subprocess
import tempfile
import time
import threading

from storage import disk_arguments, network_arguments

from . import ap_freeze, boot
from .catalog import Catalog, HostError, require, run_command
from .protocol import KTAP, ready
from .execution_uart import ExecutionUART
from .net_user import NetUserPeer
from .resources import (crc32c_update, prepare_disks,
                        resource_names, validate_resources)

CANCELLED = threading.Event()


class DeadlineExpired(RuntimeError):
    """宿主单调绝对期限已经到达。"""


class VMExited(RuntimeError):
    """UART 已关闭或具体 transport 已到不可继续的观察终点。"""


class UART:
    """共享串口协议和 raw 阶段；具体 transport 独占其端点与终结责任。"""


    def check(self, deadline):
        """日志数量和字节进度都不改变固定截止时刻。"""
        require(not CANCELLED.is_set(), "host run cancelled")
        if deadline <= time.monotonic():
            raise DeadlineExpired("absolute host deadline expired")
        if self.debugger is not None:
            require(self.debugger.poll() in (None, 0), "AP freeze GDB failed; inspect ap-freeze-gdb.log")

    def receive(self, deadline, limit=65536):
        """逐块落盘原文；真实 EOF 与进程/板停止诊断分别判断。"""
        while True:
            self.check(deadline)
            readable, _, _ = select.select([self.serial], [], [], max(0, min(0.1, deadline - time.monotonic())))
            self.check(deadline)
            if not readable:
                continue
            try:
                data = self.serial.recv(limit)
            except BlockingIOError:
                continue
            if not data:
                raise VMExited("UART closed")
            self.raw.write(data)
            return data

    def line(self, deadline):
        """最多保留 64KiB 单行；raw fixture 字节走独立长度路径。"""
        while True:
            self.check(deadline)
            newline = self.buffer.find(b"\n")
            if newline >= 0:
                require(newline <= 65536, "UART protocol line exceeds limit")
                raw = bytes(self.buffer[:newline + 1])
                del self.buffer[:newline + 1]
                fixture = getattr(self, "execution_uart", None)
                if fixture is not None:
                    fixture.observe(raw)
                return raw[:-1].rstrip(b"\r").decode("utf-8", errors="strict")
            require(len(self.buffer) <= 65536, "UART protocol line exceeds limit")
            self.buffer.extend(self.receive(deadline))

    def send(self, data, deadline):
        """非阻塞发送在同一绝对期限内完成，不开启另一个 RX 通道。"""
        view = memoryview(data)
        while view:
            self.check(deadline)
            _, writable, _ = select.select([], [self.serial], [], max(0, min(0.1, deadline - time.monotonic())))
            self.check(deadline)
            if writable:
                try:
                    count = self.serial.send(view)
                except BlockingIOError:
                    continue
                require(0 < count, "UART send made no progress")
                view = view[count:]

    def delay(self, seconds, deadline):
        """故意停读阶段仍遵守宿主期限，不能借 sleep 延期。"""
        end = time.monotonic() + seconds
        while time.monotonic() < end:
            self.check(deadline)
            time.sleep(max(0, min(0.01, end - time.monotonic(), deadline - time.monotonic())))

    def raw_io(self, declaration, deadline, names):
        """H 后独占原始段；RX 按本 VM 游标逐段核对长度与 CRC。"""
        require("uart" in names, "UART IO without declared resource")
        direction = declaration["direction"]
        length = declaration["length"] if direction != "duplex" else 0
        chunk = 64 if direction == "slow" else 65536
        state = 0xFFFFFFFF
        payload = self.args.uart_payload
        expected_path = self.args.uart_expect
        expected = None
        try:
            if direction == "duplex":
                self._duplex_io(declaration, deadline)
                return
            fixture = getattr(self, "execution_uart", None)
            if fixture is not None and fixture.kind == "backpressure":
                require(getattr(self.args, "execution_uart", False),
                        "backpressure fixture requires --execution-uart")
                fixture.capture(self, declaration, deadline)
                return
            if direction == "rx":
                require(payload is not None, "UART receive fixture requires --uart-payload")
                input_state = 0xFFFFFFFF
                remaining = length
                with payload.open("rb") as source:
                    source.seek(self.rx_offset)
                    while remaining:
                        self.check(deadline)
                        block = source.read(min(chunk, remaining))
                        require(block, "UART input ended before declared phase length")
                        input_state = crc32c_update(input_state, block)
                        remaining -= len(block)
                require(f"{input_state ^ 0xFFFFFFFF:08x}" == declaration["digest"], "UART input CRC mismatch")
                self.rx_used = True
            else:
                require(expected_path is not None, "UART transmit fixture requires --uart-expect")
                require(expected_path.stat().st_size == length, "UART expected length mismatch")
                expected = expected_path.open("rb")
            self.send(b"H", deadline)
            if direction == "pause":
                self.delay(declaration["delay_ms"] / 1000, deadline)
            if direction == "rx":
                remaining = length
                with payload.open("rb") as source:
                    source.seek(self.rx_offset)
                    while remaining:
                        block = source.read(min(chunk, remaining))
                        require(block, "UART input ended during declared phase")
                        state = crc32c_update(state, block)
                        self.send(block, deadline)
                        remaining -= len(block)
                        self.rx_offset += len(block)
            else:
                remaining = length
                with (self.directory / f"uart-{declaration['case']}-{time.monotonic_ns()}.raw").open("xb") as output:
                    while remaining:
                        self.check(deadline)
                        if direction == "slow":
                            self.delay(declaration["delay_ms"] / 1000, deadline)
                        count = min(chunk, remaining)
                        if self.buffer:
                            block = bytes(self.buffer[:count])
                            del self.buffer[:len(block)]
                        else:
                            block = self.receive(deadline, count)
                        state = crc32c_update(state, block)
                        output.write(block)
                        if expected is not None:
                            require(expected.read(len(block)) == block, "UART full payload mismatch")
                        remaining -= len(block)
            require(f"{state ^ 0xFFFFFFFF:08x}" == declaration["digest"], "UART full payload CRC mismatch")
            self.send(f"KTEST IO OK {declaration['case']}\n".encode("ascii"), deadline)
        except (HostError, OSError) as error:
            try:
                self.send(f"KTEST IO ERROR {declaration['case']}\n".encode("ascii"), deadline)
            except (OSError, DeadlineExpired, VMExited):
                pass
            raise error
        finally:
            if expected is not None:
                expected.close()

    def _duplex_io(self, declaration, deadline):
        """同一原始串口交错收发 payload，按本次真实偏移逐段校验。"""
        rx_length = declaration["rx_length"]
        tx_length = declaration["tx_length"]
        payload = self.args.uart_payload
        expected_path = self.args.uart_expect
        if rx_length:
            require(payload is not None, "UART duplex receive requires --uart-payload")
            state = 0xFFFFFFFF
            with payload.open("rb") as source:
                source.seek(self.rx_offset)
                remaining = rx_length
                while remaining:
                    self.check(deadline)
                    block = source.read(min(65536, remaining))
                    require(block, "UART duplex input ended before declared phase length")
                    state = crc32c_update(state, block)
                    remaining -= len(block)
            require(f"{state ^ 0xFFFFFFFF:08x}" == declaration["rx_digest"],
                    "UART duplex input CRC mismatch")
            self.rx_used = True
        if tx_length:
            require(expected_path is not None, "UART duplex transmit requires --uart-expect")
            require(self.tx_offset + tx_length <= expected_path.stat().st_size,
                    "UART duplex expectation ended before declared phase length")
            self.tx_used = True

        source = expected = output = None

        try:
            source = payload.open("rb") if rx_length else None
            expected = expected_path.open("rb") if tx_length else None
            if source is not None:
                source.seek(self.rx_offset)
            if expected is not None:
                expected.seek(self.tx_offset)
                output = (self.directory / f"uart-{declaration['case']}-{time.monotonic_ns()}.raw").open("xb")


            self.send(b"H", deadline)
            receive_at = time.monotonic() + declaration["rx_delay_ms"] / 1000
            receive_after = time.monotonic()
            pending = memoryview(b"")
            rx_remaining = rx_length
            tx_remaining = tx_length
            rx_state = tx_state = 0xFFFFFFFF
            while rx_remaining or tx_remaining:
                self.check(deadline)
                now = time.monotonic()
                if rx_remaining and not pending and receive_at <= now:
                    block = source.read(min(65536, rx_remaining))
                    require(block, "UART duplex input ended during declared phase")
                    pending = memoryview(block)

                read_sockets = [self.serial] if tx_remaining and receive_after <= now else []
                write_sockets = [self.serial] if pending else []
                timeout = max(0, min(0.1, deadline - now))
                if read_sockets and self.buffer:
                    timeout = 0
                if rx_remaining and not pending and now < receive_at:
                    timeout = min(timeout, receive_at - now)
                if tx_remaining and not read_sockets and now < receive_after:
                    timeout = min(timeout, receive_after - now)
                readable, writable, _ = select.select(read_sockets, write_sockets, [], timeout)
                self.check(deadline)

                if readable or (read_sockets and self.buffer):
                    chunk = 64 if declaration["tx_delay_ms"] else 65536
                    count = min(chunk, tx_remaining)
                    if self.buffer:
                        block = bytes(self.buffer[:count])
                        del self.buffer[:len(block)]
                    else:
                        try:
                            block = self.serial.recv(count)
                        except BlockingIOError:
                            continue
                        if not block:
                            raise VMExited("UART closed during duplex payload")
                        self.raw.write(block)
                    output.write(block)
                    tx_state = crc32c_update(tx_state, block)
                    require(expected.read(len(block)) == block, "UART duplex payload mismatch")
                    tx_remaining -= len(block)
                    self.tx_offset += len(block)
                    receive_after = time.monotonic() + declaration["tx_delay_ms"] / 1000

                if writable:
                    try:
                        count = self.serial.send(pending)
                    except BlockingIOError:
                        continue
                    require(0 < count, "UART duplex input made no progress")
                    sent = pending[:count]
                    rx_state = crc32c_update(rx_state, sent)
                    pending = pending[count:]
                    rx_remaining -= count
                    self.rx_offset += count

            if rx_length:
                require(f"{rx_state ^ 0xFFFFFFFF:08x}" == declaration["rx_digest"],
                        "UART duplex transmitted input CRC mismatch")
            if tx_length:
                require(f"{tx_state ^ 0xFFFFFFFF:08x}" == declaration["tx_digest"],
                        "UART duplex output CRC mismatch")
        finally:
            if source is not None:
                source.close()
            if expected is not None:
                expected.close()
            if output is not None:
                output.close()

        self.send(f"KTEST IO OK {declaration['case']}\n".encode("ascii"), deadline)

    def handshake(self, config, command, deadline):
        """唯一早期 READY 之后只发送一次 catalog 或 run 命令。"""
        self.connect(deadline)
        while True:
            line = self.line(deadline)
            if line.startswith("KTEST READY"):
                ready(line, config)
                self.send(command, deadline)
                return

    def verify_resources(self, deadline):
        """真实退出后核对 UART、网络 peer 原字节及本 VM 的冻结现场。"""
        if self.rx_used:
            require(self.args.uart_payload.stat().st_size == self.rx_offset,
                    "UART input has unconsumed bytes at VM exit")
        if self.tx_used:
            require(self.args.uart_expect.stat().st_size == self.tx_offset,
                    "UART expected output has unconsumed bytes at VM exit")
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

class VM(UART):
    """每个实例有独立串口 socket、工作盘和进程组，不使用 stdio 后备。"""

    def __init__(self, args, kernel, directory, names):
        """独占盘和 live peer 在启动前准备，QEMU 真正 wait 后才释放。"""
        self.args = args
        self.directory = directory
        self.process = None
        self.serial = None
        self.buffer = bytearray()
        self.rx_offset = 0
        self.rx_used = False
        self.tx_offset = 0
        self.tx_used = False
        self.execution_uart = None
        self.net_user = None
        self.debugger = None
        self.debug_log = None
        self.gdb_command = None
        self.socket_directory = tempfile.TemporaryDirectory(prefix="ktest-uart-")
        self.endpoint = str(Path(self.socket_directory.name) / "uart.sock")
        self.raw = None
        self.console = None
        self.disk_directory = None
        try:
            self.raw = (directory / "uart.log").open("xb", buffering=0)
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
            command += network_arguments(args.arch, "user" if self.net_user is not None else "none")
            ports = self.net_user.ports if self.net_user is not None else None
            command += disk_arguments(disks, args, names, network_ports=ports, rootdir="/")
            if "ap-freeze" in names and args.cpus >= 2:
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
        self.serial = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        while True:
            self.check(deadline)
            if self.process.poll() is not None:
                raise VMExited(f"QEMU exited {self.process.returncode}")
            try:
                self.serial.connect(self.endpoint)
                self.serial.setblocking(False)
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
        if self.serial is not None:
            self.serial.close()
        if self.raw is not None:
            self.raw.close()
        if self.console is not None:
            self.console.close()
        if self.debug_log is not None:
            self.debug_log.close()
        self.socket_directory.cleanup()
        if self.disk_directory is not None:
            self.disk_directory.cleanup()


def transport(args, kernel, directory, names):
    """只在明确平台选择处替换装载/端点，QEMU 调用和测试替换点保持原样。"""
    if getattr(args, "platform", "qemu-virt") == "qemu-virt":
        return VM(args, kernel, directory, names)
    from .board import Board
    return Board(args, kernel, directory, names)


def validate_transport_resources(names, args):
    """选择同一资源声明的真实承载方，不让 QEMU 外部控制冒充板输入。"""
    if getattr(args, "platform", "qemu-virt") == "qemu-virt":
        validate_resources(names, args)
    else:
        from .board import validate_resources as validate_board_resources
        validate_board_resources(names, args)


def config(args):
    """发现与运行共享同一个实际配置。"""
    return dict(arch=args.arch, cpus=args.cpus, mode=args.mode, panic=args.panic)


def discover(args, kernel, directory):
    """从实际 transport 读取注册 JSONL，目录不执行 fixture。"""
    catalog = Catalog(config(args))
    vm = transport(args, kernel, directory, set())
    deadline = time.monotonic() + args.startup_timeout
    try:
        vm.handshake(config(args), b"catalog\n", deadline)
        while not catalog.done:
            line = vm.line(deadline)
            if line.startswith("KTEST READY"):
                raise HostError("duplicate READY")
            if line.startswith("{"):
                catalog.feed(json.loads(line))
            else:
                require(not line.strip() or line.lstrip().startswith("#"), "unexpected catalog output")
        finish_vm(vm, time.monotonic() + args.cleanup_timeout, catalog=True)
        return catalog
    finally:
        vm.close()


def looks_control(value):
    """EOF 的半条框架记录必须拒绝，普通未换行业务日志仍保留。"""
    value = value.lstrip()
    if not value:
        return False
    markers = ("KTAP version", "KTEST READY", "# KTEST", "# Subtest:", "ok ", "not ok ", "1..")
    if any(value.startswith(marker) or marker.startswith(value) for marker in markers):
        return True
    if value.startswith("{"):
        kinds = ("catalog", "suite", "case", "catalog_end")
        try:
            row = json.loads(value)
            return isinstance(row, dict) and row.get("type") in kinds
        except json.JSONDecodeError:
            compact = "".join(value.split())
            prefixes = [f'{{"type":"{kind}' for kind in kinds]
            return any(compact.startswith(prefix) or prefix.startswith(compact) for prefix in prefixes)
    return False


def finish_vm(vm, deadline, parser=None, catalog=False):
    """final 后继续解析，QEMU 等真实退出，板等停止诊断并观察完整清理窗口。"""
    while True:
        try:
            line = vm.line(deadline)
        except VMExited:
            fragment = bytes(vm.buffer).decode("utf-8", errors="replace").rstrip("\r")
            require(not looks_control(fragment), "partial control record at EOF")
            if getattr(vm, "terminal", None) is not None:
                require(getattr(vm, "observed_until_deadline", False),
                        "board UART closed before terminal observation completed")
                require(vm.terminal == "success" and vm.terminal_scope == "all-online",
                        "board lacks all-online successful stop after final")
                vm.verify_resources(deadline)
                return
            require(getattr(getattr(vm, "args", None), "platform", "qemu-virt") == "qemu-virt",
                    "board UART closed without a stop decision")
            while vm.process.poll() is None:
                vm.check(deadline)
                time.sleep(min(0.01, max(0, deadline - time.monotonic())))
            vm.check(deadline)
            require(vm.process.returncode == 0, f"QEMU failed after final: {vm.process.returncode}")
            vm.verify_resources(deadline)
            return
        if catalog:
            require(not looks_control(line), "control record after catalog_end")
        else:
            require(not (line.lstrip().startswith("{") and looks_control(line)),
                    "catalog control record after run complete")
            parser.feed(line)




def result_row(item, status, reason, directory, case_abort=None):
    """报告记录真实身份；中止账本也进入同次 JUnit 的错误原因。"""
    case = item.case
    row = dict(name=case["name"], suite=case["suite"], case=case["case"], parameter=case["parameter"],
               source=case["source"], line=case["line"], repeat=item.repeat, seed=item.seed,
               order=item.order, status=status, reason=reason, log=str(directory / "uart.log"))
    if case_abort is not None:
        row["case_abort"] = dict(case_abort)
        row["reason"] += "\ncase-abort: " + json.dumps(case_abort, ensure_ascii=False, sort_keys=True)
    return row


def suite_error_row(suite, item, error, directory):
    """独立记录 suite 基础设施错误，不能冒充未开始的用例结果。"""
    return dict(name=f"{suite['name']} [SuiteError:{error['phase']}]", suite=suite["suite"],
                case=None, parameter=None, source=None, line=None, repeat=item.repeat,
                seed=item.seed, order=item.order, status="Error", kind="SuiteError",
                reason=f"{error['phase']}: {error['reason']}", suite_error=dict(error),
                log=str(directory / "uart.log"))

def execute_batch(args, kernel, catalog, items, directory):
    """保持原 KTAP、fatal 和绝对期限，peer 消费边界完成后才回执放行本例或后继。"""
    output = []
    pending = list(items)
    attempt = 0
    while pending:
        current = directory / f"part-{attempt}"
        current.mkdir()
        attempt += 1
        suite = catalog.suites[pending[0].case["suite"]]
        parser = KTAP(suite, pending)
        vm = None
        infrastructure = None
        external = None
        safe_suffix = False
        deadline = time.monotonic() + args.startup_timeout
        setup_deadline = case_deadline = cleanup_deadline = None
        try:
            names = resource_names(suite, pending)
            validate_transport_resources(names, args)
            if getattr(args, "platform", "qemu-virt") != "qemu-virt":
                from .board import validate_cases
                validate_cases(pending)
            vm = transport(args, kernel, current, names)
            vm.execution_uart = ExecutionUART(args.cpus, current)
            vm.handshake(config(args), run_command(pending, args.seed), deadline)
            while not parser.complete:
                effective = min(d for d in (deadline, setup_deadline, case_deadline, cleanup_deadline) if d is not None)
                event = parser.feed(vm.line(effective))
                now = time.monotonic()
                if event == "suite-setup":
                    setup_deadline = now + args.setup_timeout
                elif event in ("suite-error", "case-abort"):
                    setup_deadline = case_deadline = None
                    if cleanup_deadline is None:
                        cleanup_deadline = now + args.cleanup_timeout
                    deadline = cleanup_deadline
                elif event == "begin":
                    item = pending[len(parser.results)]
                    vm.execution_uart.begin(item.case)
                    case_deadline = now + min(item.case["timeout_ms"] / 1000, args.case_timeout)
                    peer = getattr(vm, "net_user", None)
                    if peer is not None:
                        peer.begin(item.case, case_deadline)
                    setup_deadline = now + args.setup_timeout
                    cleanup_deadline = None
                    deadline = case_deadline
                    if peer is not None:
                        vm.send(f"KTEST PEER BEGIN {item.case['case']}\n".encode("ascii"),
                                min(case_deadline, setup_deadline))
                elif event == "body":
                    setup_deadline = None
                    vm.execution_uart.body = True
                elif event == "fatal" and getattr(args, "platform", "qemu-virt") != "qemu-virt":
                    setup_deadline = case_deadline = None
                    cleanup_deadline = deadline = now + args.cleanup_timeout
                elif event in ("quiesce", "join", "cleanup"):
                    setup_deadline = None
                    if cleanup_deadline is None:
                        cleanup_deadline = now + args.cleanup_timeout
                elif isinstance(event, dict):
                    vm.raw_io(event, effective, names)
                elif event == "status":
                    vm.execution_uart.finish(parser.status)
                    peer = getattr(vm, "net_user", None)
                    if peer is not None:
                        peer.finish(parser.status, case_deadline)
                        vm.send(f"KTEST PEER END {parser.active}\n".encode("ascii"), effective)
                elif event == "result":
                    setup_deadline = case_deadline = None
                    if cleanup_deadline is None:
                        cleanup_deadline = now + args.cleanup_timeout
                    deadline = cleanup_deadline
            finish_vm(vm, cleanup_deadline or time.monotonic() + args.cleanup_timeout, parser)
            if parser.case_abort is not None:
                # 完整 outer/complete 与实际 transport 终点后才确认明确的中止 Error。
                parser.results[parser.active] = dict(status="Error", reason=parser.case_abort["reason"],
                                                     case_abort=parser.case_abort)
                parser.active = None
        except DeadlineExpired as error:
            external = ("Timeout", str(error))
        except VMExited as error:
            board_terminal = (getattr(vm, "terminal", None)
                              if getattr(vm, "observed_until_deadline", False) else None)
            board = getattr(args, "platform", "qemu-virt") != "qemu-virt"
            terminated = (board_terminal is not None if board else
                          vm is not None and vm.process.poll() is not None)
            if (not board and parser.active is not None and parser.case_abort is None
                    and parser.fatal is not None and not terminated):
                try:
                    vm.process.wait(timeout=max(0, min(args.cleanup_timeout, effective - time.monotonic())))
                    terminated = True
                except subprocess.TimeoutExpired:
                    pass
            case = pending[len(parser.results)].case if parser.active is not None else None
            fragment = bytes(vm.buffer).decode("utf-8", errors="replace").rstrip("\r")
            if looks_control(fragment):
                infrastructure = "partial control record at EOF"
            elif case is not None and not board_terminal and effective <= time.monotonic():
                external = ("Timeout", "absolute host deadline expired")
            else:
                # fatal 的不可返回终点必须由真实自行退出或板停止决策提供；
                # close()、UART EOF 和宿主超时都不能证明后继尚未开始。
                fatal_matches = (case is not None and parser.phase == "body"
                                 and parser.fatal is not None
                                 and parser.armed == parser.fatal == case["expected_fatal"]
                                 and (case["fatal_message"] is None or parser.fatal_message_seen))
                safe_suffix = (case is not None and parser.case_abort is None
                               and parser.fatal is not None and terminated
                               and ((board_terminal == "failure" and vm.terminal_scope == "current-cpu"
                                     and vm.loaded and len(pending) == 1 and fatal_matches)
                                    if board else vm.process.returncode == 0)
                               and not vm.buffer.strip())
                if safe_suffix:
                    try:
                        vm.verify_resources(effective)
                        if fatal_matches:
                            boot.verify_trap(args.arch, case, vm.directory, parser.fatal)
                    except DeadlineExpired as resource_error:
                        external = ("Timeout", str(resource_error))
                        safe_suffix = False
                    except (HostError, OSError, ValueError, KeyError) as resource_error:
                        infrastructure = str(resource_error)
                        safe_suffix = False
                if safe_suffix and fatal_matches:
                    terminal = ("board stop scope=current-cpu reason=failure" if board else
                                f"QEMU={vm.process.returncode}")
                    parser.results[parser.active] = dict(
                        status="Pass", reason=f"expected fatal: {parser.fatal}; {terminal}")
                    parser.active = None
                else:
                    external = external or ("Crash", str(error))
        except (HostError, OSError, ValueError, UnicodeError) as error:
            infrastructure = str(error)
        finally:
            if vm is not None:
                vm.close()
        for item in pending:
            cid = item.case["case"]
            if cid in parser.results:
                result = parser.results[cid]
                peer = getattr(vm, "net_user", None)
                peer_error = peer.failures.get(cid) if peer is not None else None
                if result["status"] == "Pass" and peer_error:
                    result = dict(result, status="Error", reason=peer_error)
                output.append(result_row(item, result["status"], result["reason"], current,
                                         result.get("case_abort")))
            elif cid == parser.active:
                status, reason = ("Error", infrastructure) if infrastructure else external or ("Error", "missing final result")
                output.append(result_row(item, status, reason, current, parser.case_abort))
        if parser.case_abort is not None:
            # 明确中止不重放；不完整终结仍保真实协议/期限/进程错误。
            for item in pending:
                if item.case["case"] not in parser.started:
                    output.append(result_row(item, "NotRun", "case-abort ended suite", current))
            if infrastructure or external:
                aborted = next(row for row in output if row["case"] == parser.active)
                aborted["infrastructure_error"] = infrastructure or (
                    "VM failed case-abort protocol/shutdown completion: " + external[1])
            break
        suite_error = parser.suite_error
        if (suite_error is None and parser.suite_started and not parser.started
                and (infrastructure or external)):
            suite_error = dict(phase="setup", reason=infrastructure or external[1])
        elif (suite_error is None and external and parser.active is None
              and len(parser.results) == len(pending)):
            suite_error = dict(phase="cleanup", reason=external[1])
        if suite_error is not None:
            for item in pending:
                if item.case["case"] not in parser.started:
                    output.append(result_row(item, "NotRun",
                                             f"suite {suite_error['phase']}: {suite_error['reason']}", current))
            row = suite_error_row(suite, pending[-1], suite_error, current)
            if infrastructure:
                row["infrastructure_error"] = infrastructure
            if external:
                row["infrastructure_error"] = "VM failed protocol/shutdown completion: " + external[1]
            output.append(row)
            break
        if external and parser.active is None and parser.results and output:
            output[-1]["infrastructure_error"] = "VM failed protocol/shutdown completion: " + external[1]
        unstarted = [item for item in pending if item.case["case"] not in parser.started]
        if safe_suffix:
            pending = unstarted
            continue
        if infrastructure or (external and not parser.started):
            reason = infrastructure or external[1]
            # 无 begin 的崩溃无法证明执行命令已经生效，不自动重放整批。
            for item in unstarted:
                output.append(result_row(item, "NotRun", reason, current))
            if not parser.active and not parser.results and output:
                output[-len(unstarted)]["infrastructure_error"] = reason
            elif infrastructure and output:
                output[-1]["infrastructure_error"] = infrastructure
            break
        if external:
            # begin/final 可能仍在串口或缓冲区；缺少记录不意味着尚未执行。
            for item in unstarted:
                output.append(result_row(item, "NotRun", external[1], current))
            break
        if unstarted:
            for item in unstarted:
                output.append(result_row(item, "NotRun", "VM ended before case-begin", current))
        break
    return output


def execute(args, kernel, catalog, groups, directory):
    """并行只作用于 VM；每个 suite 内的顶层 case 顺序不变。"""
    completed = {}
    pool = ThreadPoolExecutor(max_workers=args.jobs)
    futures = {}
    interrupted = False
    try:
        for index, group in enumerate(groups):
            target = directory / f"vm-{index:05d}"
            target.mkdir()
            futures[pool.submit(execute_batch, args, kernel, catalog, group, target)] = index
        for future in as_completed(futures):
            completed[futures[future]] = future.result()
    except KeyboardInterrupt:
        interrupted = True
        CANCELLED.set()
        for future in futures:
            future.cancel()
    except BaseException:
        CANCELLED.set()
        for future in futures:
            future.cancel()
        raise
    finally:
        pool.shutdown(wait=True, cancel_futures=True)
    if interrupted:
        started_groups = set()
        for future, index in futures.items():
            if not future.cancelled():
                started_groups.add(index)
                if index not in completed:
                    completed[index] = future.result()
        for index, group in enumerate(groups):
            if index not in started_groups:
                target = directory / f"vm-{index:05d}"
                completed[index] = [result_row(item, "NotRun", "host interrupted before VM start", target)
                                    for item in group]
        results = [row for rows in completed.values() for row in rows]
        if results:
            results[0]["infrastructure_error"] = "host run interrupted"
    results = [row for rows in completed.values() for row in rows]
    return sorted(results, key=lambda row: row["order"])
