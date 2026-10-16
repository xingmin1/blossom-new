"""宿主 lifecycle 回归；运行 python3 -m unittest discover -s tools/ktest。"""

import io
import json
import os
from pathlib import Path
import select as select_io
import socket
import subprocess
import sys
import tempfile
import time
import unittest
from types import SimpleNamespace
from unittest.mock import patch
import xml.etree.ElementTree as ET

from host_helpers import (CLI, CONFIG, case_record, case_lines, catalog_of,
                          options, parser_for, vm_fixture)
from ktest.catalog import HostError, invocations
from ktest.net_user import NetUserPeer
from ktest.runner import DeadlineExpired, VM, execute_batch, finish_vm


class TerminalLifecycleTests(unittest.TestCase):
    """绝对期限、真实退出与 UART 尾文的终结顺序。"""

    def test_buffered_log_flood_cannot_extend_deadline(self):
        """已缓冲的大量行也要检查绝对期限，不能只检查 recv。"""
        vm = vm_fixture()
        vm.buffer = bytearray(b"log\n" * 1000)
        with self.assertRaises(DeadlineExpired):
            vm.line(time.monotonic() - 1)

    def test_final_waits_for_exit_and_rejects_late_or_partial_control(self):
        """真实 socket 终结后普通诊断合法，迟到 final 和半条结果不能通过。"""
        for payload, rejected in ((b'ordinary diagnostic\n{"type":"business","value":1}\nunfinished business log', False),
                                  (b"# KTEST complete\n", True), (b"  ok 1", True),
                                  (b'{"type":"catalog_end"', True)):
            with self.subTest(payload=payload), tempfile.TemporaryDirectory() as temporary:
                row = case_record()
                parser = parser_for([row])
                for line in case_lines(row):
                    parser.feed(line)
                parser.feed("ok 1 os::tests0")
                parser.feed("# KTEST complete")
                host, guest = socket.socketpair()
                try:
                    vm = vm_fixture()
                    vm.serial = host
                    host.setblocking(False)
                    vm.buffer = bytearray()
                    vm.raw = io.BytesIO()
                    vm.process = SimpleNamespace(poll=lambda: 0, returncode=0)
                    guest.sendall(payload)
                    guest.shutdown(socket.SHUT_WR)
                    if rejected:
                        with self.assertRaises(HostError):
                            finish_vm(vm, time.monotonic() + 2, parser)
                    else:
                        finish_vm(vm, time.monotonic() + 2, parser)
                        self.assertEqual(vm.raw.getvalue(), payload)
                finally:
                    host.close()
                    guest.close()

    def test_uart_exit_does_not_skip_bytes_after_readiness_check(self):
        """真实退出发生在空就绪检查后，普通尾文仍落盘，迟到及半条控制仍拒绝。"""
        for payload, rejected in ((b"final diagnostic\n", False),
                                  (b"# KTEST complete\n", True), (b"  ok 1", True)):
            with self.subTest(payload=payload):
                row = case_record()
                parser = parser_for([row])
                for line in case_lines(row):
                    parser.feed(line)
                parser.feed("ok 1 os::tests0")
                parser.feed("# KTEST complete")
                host, guest = socket.socketpair()
                script = ("import socket,sys\n"
                          "uart=socket.socket(fileno=int(sys.argv[1]))\n"
                          "sys.stdin.buffer.read(1)\n"
                          "uart.sendall(bytes.fromhex(sys.argv[2]))\n"
                          "uart.close()\n")
                process = subprocess.Popen(
                    [sys.executable, "-c", script, str(guest.fileno()), payload.hex()],
                    stdin=subprocess.PIPE, pass_fds=(guest.fileno(),))
                guest.close()
                original_select = select_io.select
                crossed = False

                def exit_after_readiness(readers, writers, errors, timeout):
                    """先取真实空就绪结果，再放子进程写尾文并等待其真实退出。"""
                    nonlocal crossed
                    if not crossed:
                        crossed = True
                        result = original_select(readers, writers, errors, 0)
                        self.assertEqual(result, ([], [], []))
                        process.stdin.write(b"G")
                        process.stdin.flush()
                        self.assertEqual(process.wait(timeout=2), 0)
                        return result
                    return original_select(readers, writers, errors, timeout)

                try:
                    vm = vm_fixture()
                    vm.serial = host
                    host.setblocking(False)
                    vm.buffer = bytearray()
                    vm.raw = io.BytesIO()
                    vm.process = process
                    with patch("ktest.runner.select.select", side_effect=exit_after_readiness):
                        if rejected:
                            with self.assertRaises(HostError):
                                finish_vm(vm, time.monotonic() + 2, parser)
                        else:
                            finish_vm(vm, time.monotonic() + 2, parser)
                            self.assertEqual(vm.raw.getvalue(), payload)
                finally:
                    host.close()
                    process.stdin.close()
                    if process.poll() is None:
                        process.terminate()
                    process.wait(timeout=2)


class LifecycleAndReportTests(unittest.TestCase):
    """崩溃续跑、首次失败和报告退出合同。"""

    def execute_serial(self, rows, lines, directory, args=None, guest_tail=""):
        """通过真实 socket、独立进程和宿主解析器执行给定 guest 输出。"""
        args = args or options()
        host, guest = socket.socketpair()
        script = (
            "import socket,sys,time\n"
            "uart=socket.socket(fileno=int(sys.argv[1]))\n"
            "uart.sendall(b'KTEST READY 1 arch=riscv64 cpus=2 mode=debug panic=unwind\\n')\n"
            "command=b''\n"
            "while not command.endswith(b'\\n'): command+=uart.recv(1024)\n"
            "uart.sendall(sys.argv[2].encode('utf-8'))\n"
            "uart.close()\n"
            + guest_tail + "\n"
        )
        process = subprocess.Popen([sys.executable, "-c", script, str(guest.fileno()),
                                    "\n".join(lines) + "\n"], pass_fds=(guest.fileno(),))
        guest.close()
        vm = vm_fixture()
        vm.serial = host
        host.setblocking(False)
        vm.buffer = bytearray()
        vm.raw = io.BytesIO()
        vm.process = process
        vm.connect = lambda deadline: None

        def close():
            """测试范围始终收回真实子进程及 socket。"""
            host.close()
            if process.poll() is None:
                process.terminate()
            process.wait(timeout=2)

        vm.close = close
        with patch("ktest.runner.VM", return_value=vm) as factory:
            results = execute_batch(args, Path("kernel"), catalog_of(rows),
                                    invocations(rows, args), directory)
        self.assertEqual(factory.call_count, 1, "suite failure must not retry unstarted cases")
        return results

    def test_network_peer_ack_orders_real_consumers_and_successor(self):
        """真实跨例 P/Q 在 BEGIN 后产生；管道固定 begin 和 finish/drain 尚未完成的窗口。"""
        rows = [case_record(index, tags=["network-nic-native"], resources=["net-user"])
                for index in range(2)]
        script = """
import json,os,select,socket,sys,time
uart=socket.socket(fileno=int(sys.argv[1]))
ports_reader,gate_reader,feedback_writer=map(int,sys.argv[2:5])
rows=json.loads(sys.argv[5])
deadline=time.monotonic()+5

def ready(readers):
    '''所有协调和真实 socket 读取共用子进程的一次绝对期限。'''
    remaining=deadline-time.monotonic()
    assert 0 < remaining, "child absolute deadline expired"
    readable,_,_=select.select(readers,[],[],remaining)
    assert readable, "child absolute deadline expired"
    return readable

def line():
    '''逐字节消费真实 UART，避免吞掉后继回执或运行命令。'''
    value=b""
    while not value.endswith(b"\\n"):
        ready([uart])
        part=uart.recv(1)
        assert part, "UART EOF before acknowledgement"
        value+=part
        assert len(value)<=1024, "UART line exceeds test bound"
    return value

def acknowledgement(phase,cid):
    '''只有完整真实回执才放行，窗口管道同时记录客体是否已经越过消费边界。'''
    expected=f"KTEST PEER {phase} {cid}\\n".encode("ascii")
    gate_seen=False
    while True:
        readable=ready([uart,gate_reader])
        if uart in readable:
            assert line()==expected, "wrong peer acknowledgement"
            return gate_seen
        marker=os.read(gate_reader,1)
        assert marker, "window pipe closed"
        matching=(phase=="BEGIN" and marker==b"B") or (phase=="END" and marker==b"F")
        os.write(feedback_writer,b"b" if phase=="BEGIN" and matching else
                 (b"f" if phase=="END" and matching else b"x"))
        gate_seen=matching

ready([ports_reader])
port=int(os.read(ports_reader,32))
udp=socket.socket(socket.AF_INET,socket.SOCK_DGRAM)
udp.bind(("127.0.0.1",0))
uart.sendall(b"KTEST READY 1 arch=riscv64 cpus=2 mode=debug panic=unwind\\n")
assert line().startswith(b"run "), "missing real run command"
uart.sendall(b"KTAP version 1\\n1..1\\n  # Subtest: os::tests0\\n"
             b"  1..2\\n  # KTEST suite-begin setup\\n")
for position,row in enumerate(rows,1):
    cid=row["case"]
    uart.sendall(f"  # KTEST case-begin {cid}\\n".encode("ascii"))
    begin_gate_seen=acknowledgement("BEGIN",cid)
    uart.sendall(f"  # KTEST phase {cid} setup\\n  # KTEST phase {cid} body\\n".encode("ascii"))
    assert udp.sendto(b"P",("127.0.0.1",port))==1
    if not begin_gate_seen:
        os.write(feedback_writer,b"p")
    ready([udp])
    data,address=udp.recvfrom(2)
    assert data==b"Q" and address==("127.0.0.1",port), "real native peer response missing"
    uart.sendall((f"  # KTEST phase {cid} cleanup\\n  # KTEST status {cid} Pass\\n"
                  f"  ok {position} {row['name']}\\n").encode("utf-8"))
    acknowledgement("END",cid)
uart.sendall(b"ok 1 os::tests0\\n# KTEST complete\\n")
udp.close()
uart.close()
os._exit(0)
"""
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            host, guest = socket.socketpair()
            ports_reader, ports_writer = os.pipe()
            gate_reader, gate_writer = os.pipe()
            feedback_reader, feedback_writer = os.pipe()
            process = subprocess.Popen(
                [sys.executable, "-c", script, str(guest.fileno()), str(ports_reader),
                 str(gate_reader), str(feedback_writer), json.dumps(rows)],
                pass_fds=(guest.fileno(), ports_reader, gate_reader, feedback_writer))
            guest.close()
            os.close(ports_reader)
            os.close(gate_reader)
            os.close(feedback_writer)
            windows = []
            peers = []
            raw = None

            def window(marker, deadline):
                """子进程在无回执的真实等待中应答管道，不通过 sleep 推测它已经停住。"""
                os.write(gate_writer, marker)
                readable, _, _ = select_io.select(
                    [feedback_reader], [], [], max(0, deadline - time.monotonic()))
                self.assertTrue(readable, "child did not reach the fixed consumer window")
                feedback = os.read(feedback_reader, 1)
                self.assertTrue(feedback, "child closed before the fixed consumer window")
                return feedback

            def construct_vm(args, kernel, current, names):
                """仅代替 QEMU 启动，UART、进程、具体 peer 和正式 VM 消费方法保持真实。"""
                nonlocal raw
                self.assertIn("net-user", names)
                peer = NetUserPeer(current)
                peers.append(peer)
                os.write(ports_writer, str(peer.ports[1]).encode("ascii"))
                vm = vm_fixture()
                vm.args = args
                vm.directory = current
                vm.serial = host
                host.setblocking(False)
                vm.buffer = bytearray()
                raw = (current / "uart.log").open("xb", buffering=0)
                vm.raw = raw
                vm.process = process
                vm.net_user = peer
                original_begin = peer.begin
                original_finish = peer.finish

                def begin(case, deadline):
                    """固定消费者未准备窗口；错误提前放行时先确认 P 已在真实端点排队。"""
                    feedback = window(b"B", deadline)
                    windows.append(("begin", case["case"], feedback))
                    if feedback == b"p":
                        readable, _, _ = select_io.select(
                            [peer.udp], [], [], max(0, deadline - time.monotonic()))
                        self.assertTrue(readable, "early producer did not reach the actual UDP socket")
                    original_begin(case, deadline)

                def finish(status, deadline):
                    """固定原 finish/join/drain 前窗口，后继不得在其返回前进入 UART。"""
                    feedback = window(b"F", deadline)
                    windows.append(("finish", peer.active["row"]["case"], feedback))
                    if feedback == b"x":
                        readable, _, _ = select_io.select(
                            [host], [], [], max(0, deadline - time.monotonic()))
                        self.assertTrue(readable, "successor did not enter the actual UART socket")
                    original_finish(status, deadline)

                def connect(deadline):
                    """UART 已由真实继承句柄建立，原 handshake 仍消费 READY 和发送命令。"""
                    vm.check(deadline)

                def close():
                    """先终结并 wait 子进程，再关闭 peer 全尾及实际 UART 句柄。"""
                    if process.poll() is None:
                        process.terminate()
                    process.wait(timeout=2)
                    peer.close()
                    host.close()
                    raw.close()

                peer.begin = begin
                peer.finish = finish
                vm.connect = connect
                vm.close = close
                return vm

            try:
                args = options(startup_timeout=2, setup_timeout=2, case_timeout=2, cleanup_timeout=2)
                with patch("ktest.runner.VM", side_effect=construct_vm) as factory:
                    results = execute_batch(args, Path("kernel"), catalog_of(rows),
                                            invocations(rows, args), directory)
                self.assertEqual(factory.call_count, 1)
                self.assertEqual([row["status"] for row in results], ["Pass", "Pass"])
                self.assertEqual(process.returncode, 0)
                self.assertEqual(windows, [("begin", 0, b"b"), ("finish", 0, b"f"),
                                           ("begin", 1, b"b"), ("finish", 1, b"f")])
                report = json.loads((directory / "part-0" / "peer.json").read_text())
                self.assertFalse(report["failures"])
                self.assertEqual(len(report["cases"]), 2)
                for row in report["cases"]:
                    received = [event for event in row["events"] if event["kind"] == "udp-recv"]
                    sent = [event for event in row["events"] if event["kind"] == "udp-send"]
                    self.assertEqual([event["bytes_hex"] for event in received], ["50"])
                    self.assertEqual([event["bytes_hex"] for event in sent], ["51"])
                    self.assertEqual(received[0]["source"], sent[0]["destination"])
                    self.assertTrue(row["complete"])
                    self.assertFalse(row["errors"])
                    self.assertIsNotNone(row["thread_exited_at"])
                    self.assertIsNotNone(row["joined_at"])
                self.assertIsNotNone(report["closed_at"])
            finally:
                if process.poll() is None:
                    process.terminate()
                process.wait(timeout=2)
                for peer in peers:
                    peer.close()
                host.close()
                if raw is not None:
                    raw.close()
                for descriptor in (ports_writer, gate_writer, feedback_reader):
                    os.close(descriptor)

    def test_suite_errors_keep_case_states_and_independent_json_junit_error(self):
        """suite 各阶段错误经真实 UART 落到独立报告，保全各例状态。"""
        rows = [case_record(0), case_record(1)]
        for phase, statuses in (("setup", ["NotRun", "NotRun"]),
                                ("execution", ["Fail", "NotRun"]),
                                ("cleanup", ["Pass", "Pass"])):
            with self.subTest(phase=phase), tempfile.TemporaryDirectory() as temporary:
                directory = Path(temporary)
                lines = ["KTAP version 1", "1..1", "  # Subtest: os::tests0",
                         "  1..2", "  # KTEST suite-begin setup"]
                if phase == "cleanup":
                    for position, row in enumerate(rows, 1):
                        lines.extend(case_lines(row, position))
                elif phase == "execution":
                    lines.extend(case_lines(rows[0], status="Fail"))
                lines.extend([f"  # KTEST suite-error {phase} fixture failed",
                              "not ok 1 os::tests0", "# KTEST complete",
                              "ordinary shutdown diagnostic"])
                results = self.execute_serial(rows, lines, directory)
                cases = [row for row in results if row["case"] is not None]
                errors = [row for row in results if row.get("kind") == "SuiteError"]
                self.assertEqual([row["status"] for row in cases], statuses)
                self.assertEqual(len(errors), 1)
                self.assertEqual(errors[0]["suite_error"], dict(phase=phase, reason="fixture failed"))
                self.assertEqual(errors[0]["status"], "Error")
                self.assertEqual(CLI.exit_status(results), 2)
                CLI.export_reports(directory, dict(results=results, **CONFIG))
                self.assertEqual(json.loads((directory / "results.json").read_text())["results"], results)
                xml = ET.parse(directory / "results.xml")
                for row in cases:
                    node = next(node for node in xml.findall("testcase") if node.attrib["name"] == row["name"])
                    self.assertEqual(node.find("properties/property[@name='status']").attrib["value"],
                                     row["status"])
                    self.assertEqual(node.find("error") is not None, row["status"] == "NotRun")
                node = next(node for node in xml.findall("testcase")
                            if node.attrib["name"] == errors[0]["name"])
                self.assertEqual(node.find("error").attrib["message"], errors[0]["reason"])

    def test_case_abort_cleanup_failures_preserve_independent_ledgers_in_json_and_junit(self):
        """三处真实中止经 socket/进程终结与双报告保全主体、子任务和清理失败。"""
        rows = [case_record(0), case_record(1), case_record(2)]
        for phase in ("quiesce", "join", "cleanup"):
            with self.subTest(phase=phase), tempfile.TemporaryDirectory() as temporary:
                directory = Path(temporary)
                ledger = dict(primary='body "failed"\n主体', child="child returned error",
                              cleanup=f"{phase} cannot converge", expectations=3,
                              reason="活资源不得释放")
                lines = ["KTAP version 1", "1..1", "  # Subtest: os::tests0",
                         "  1..3", "  # KTEST suite-begin setup",
                         *case_lines(rows[0], status="Fail"), *case_lines(rows[1], 2)[:3],
                         f"  # KTEST phase 1 {phase}",
                         f"  # KTEST case-abort 1 {phase} " + json.dumps(ledger),
                         "not ok 1 os::tests0", "# KTEST complete"]
                results = self.execute_serial(rows, lines, directory)
                self.assertEqual([row["status"] for row in results], ["Fail", "Error", "NotRun"])
                self.assertEqual([row["case"] for row in results], [0, 1, 2])
                self.assertFalse(any(row.get("kind") == "SuiteError" for row in results))
                aborted = results[1]
                self.assertNotIn("infrastructure_error", aborted)
                self.assertEqual(aborted["case_abort"], dict(phase=phase, **ledger))
                self.assertEqual(CLI.exit_status(results), 2)
                CLI.export_reports(directory, dict(results=results, **CONFIG))
                self.assertEqual(json.loads((directory / "results.json").read_text())["results"], results)
                nodes = ET.parse(directory / "results.xml").findall("testcase")
                self.assertEqual(nodes[0].find("failure").attrib["type"], "Fail")
                message = nodes[1].find("error").attrib["message"]
                self.assertEqual(message, aborted["reason"])
                self.assertEqual(json.loads(message.split("\ncase-abort: ", 1)[1]),
                                 dict(phase=phase, **ledger))
                self.assertEqual(nodes[2].find("error").attrib["type"], "NotRun")

    def test_case_abort_requires_valid_terminal_protocol_and_real_zero_exit_without_replay(self):
        """缺终结、假 PASS、迟到记录、断 UART、非零退出和活 VM 均不得确认中止。"""
        rows = [case_record(0), case_record(1), case_record(2)]
        ledger = dict(primary=None, child="child failed", cleanup="join unsafe",
                      expectations=1, reason="cannot safely converge")
        abort = "  # KTEST case-abort 1 join " + json.dumps(ledger)
        prefix = ["KTAP version 1", "1..1", "  # Subtest: os::tests0",
                  "  1..3", "  # KTEST suite-begin setup", *case_lines(rows[0]),
                  *case_lines(rows[1], 2)[:3], "  # KTEST phase 1 join", abort]
        ending = ["not ok 1 os::tests0", "# KTEST complete"]
        scenarios = (([], "", "Crash"),
                     (ending[:1], "", "Crash"),
                     (["ok 1 os::tests0", "# KTEST complete"], "", "Error"),
                     ([abort, *ending], "", "Error"),
                     ([*ending, abort], "", "Error"),
                     ([*ending, "  # KTEST suite-error cleanup late"], "", "Error"),
                     ([*ending, "  # KTEST status 1 Error"], "", "Error"),
                     (ending, "import os; os._exit(7)", "Error"),
                     (ending, "time.sleep(1)", "Timeout"),
                     ([], "time.sleep(1)", "Crash"))
        for suffix, tail, status in scenarios:
            with self.subTest(suffix=suffix, tail=tail), tempfile.TemporaryDirectory() as temporary:
                results = self.execute_serial(rows, prefix + suffix, Path(temporary),
                                              options(cleanup_timeout=0.15), tail)
                self.assertEqual([row["status"] for row in results], ["Pass", status, "NotRun"])
                self.assertEqual(results[1]["case_abort"], dict(phase="join", **ledger))
                self.assertTrue(results[1]["infrastructure_error"])
                self.assertEqual(CLI.exit_status(results), 2)

    def test_case_abort_does_not_renew_existing_absolute_cleanup_deadline(self):
        """中止控制行不能给已经开始的清理期限重新计时。"""
        rows = [case_record(0), case_record(1), case_record(2)]
        ledger = dict(primary=None, child=None, cleanup="join failed", expectations=0, reason="unsafe")
        lines = ["KTAP version 1", "1..1", "  # Subtest: os::tests0",
                 "  1..3", "  # KTEST suite-begin setup", *case_lines(rows[0], status="Fail"),
                 *case_lines(rows[1], 2)[:3], "  # KTEST phase 1 join", "before-abort progress",
                 "  # KTEST case-abort 1 join " + json.dumps(ledger), "after-abort progress",
                 "not ok 1 os::tests0", "# KTEST complete"]
        clock = [100.0]
        vm = vm_fixture()
        vm.buffer = bytearray(("\n".join(lines) + "\n").encode("utf-8"))
        vm.handshake = lambda config, command, deadline: None
        vm.close = lambda: None

        def line(deadline):
            """使用正式缓冲行读取，仅控制业务日志所消耗的单调时间。"""
            value = VM.line(vm, deadline)
            if value.endswith("progress"):
                clock[0] += 1.5
            return value

        vm.line = line
        args = options(cleanup_timeout=2)
        with tempfile.TemporaryDirectory() as temporary, \
                patch("ktest.runner.VM", return_value=vm) as factory, \
                patch("ktest.runner.time.monotonic", side_effect=lambda: clock[0]):
            results = execute_batch(args, Path("kernel"), catalog_of(rows),
                                    invocations(rows, args), Path(temporary))
        self.assertEqual(factory.call_count, 1)
        self.assertEqual([row["status"] for row in results], ["Fail", "Timeout", "NotRun"])
        self.assertEqual(results[1]["case_abort"], dict(phase="join", **ledger))
        self.assertTrue(results[1]["infrastructure_error"])
        self.assertEqual(CLI.exit_status(results), 2)

    def test_suite_stage_deadlines_are_absolute_and_do_not_rewrite_cases(self):
        """suite setup 与末尾 cleanup 均受绝对期限约束，业务进度不延期。"""
        rows = [case_record(0), case_record(1)]
        for phase, statuses in (("setup", ["NotRun", "NotRun"]),
                                ("cleanup", ["Pass", "Pass"])):
            with self.subTest(phase=phase), tempfile.TemporaryDirectory() as temporary:
                lines = ["KTAP version 1", "1..1", "  # Subtest: os::tests0",
                         "  1..2", "  # KTEST suite-begin setup"]
                if phase == "cleanup":
                    for position, row in enumerate(rows, 1):
                        lines.extend(case_lines(row, position))
                lines.extend(["ordinary progress", "ordinary progress"])
                clock = [100.0]
                vm = vm_fixture()
                vm.buffer = bytearray(("\n".join(lines) + "\n").encode("utf-8"))
                vm.handshake = lambda config, command, deadline: None
                vm.close = lambda: None

                def line(deadline):
                    """先走真实缓冲行读取，再让业务日志消耗单调时间。"""
                    value = VM.line(vm, deadline)
                    if value == "ordinary progress":
                        clock[0] += 3
                    return value

                vm.line = line
                args = options(setup_timeout=2, cleanup_timeout=2)
                with patch("ktest.runner.VM", return_value=vm), \
                        patch("ktest.runner.time.monotonic", side_effect=lambda: clock[0]):
                    results = execute_batch(args, Path("kernel"), catalog_of(rows),
                                            invocations(rows, args), Path(temporary))
                self.assertEqual([row["status"] for row in results if row["case"] is not None],
                                 statuses)
                error = next(row for row in results if row.get("kind") == "SuiteError")
                self.assertEqual(error["suite_error"]["phase"], phase)
                self.assertEqual(CLI.exit_status(results), 2)

    def test_exit_priority_and_same_run_json_junit(self):
        """Error 优先于 Fail，Skip 不等于成功，两个报告来自同一结果。"""
        self.assertEqual(CLI.exit_status([dict(status="Skip")]), 4)
        self.assertEqual(CLI.exit_status([dict(status="Pass")]), 0)
        self.assertEqual(CLI.exit_status([dict(status="Fail"), dict(status="Error")]), 2)
        self.assertEqual(CLI.exit_status([dict(status="NotRun")]), 1)
        row = dict(name="os::s::x[p]", suite=0, case=0, status="Timeout", reason="absolute deadline",
                   repeat=2, seed=123, order=0, log="raw.log")
        report = dict(results=[row], elf_sha256="actual elf", seed=17, **CONFIG)
        with tempfile.TemporaryDirectory() as temporary:
            base = Path(temporary)
            CLI.export_reports(base, report)
            self.assertEqual(json.loads((base / "results.json").read_text())["results"], [row])
            case = ET.parse(base / "results.xml").find("testcase")
            self.assertEqual(case.attrib["name"], row["name"])
            self.assertEqual(case.find("failure").attrib["type"], "Timeout")
            self.assertEqual(case.find("properties/property[@name='seed']").attrib["value"], "123")


class HostFatalReplayTests(unittest.TestCase):
    """真实 VM 传输、独立 guest 和副作用验证 fatal 与不可重放边界。"""

    def execute_guest(self, rows, body, directory, args=None):
        """只替换 QEMU 可执行文件，正式 VM、socket、解析和进程退出保持真实。"""
        executable = directory / "guest.py"
        executable.write_text(
            "#!" + sys.executable + "\n"
            "import pathlib,socket,sys,time\n"
            f"base=pathlib.Path({str(directory)!r})\n"
            "endpoint=next(value for value in sys.argv if value.startswith('socket,id=ktest,'))"
            ".split('path=',1)[1].split(',',1)[0]\n"
            "listener=socket.socket(socket.AF_UNIX,socket.SOCK_STREAM)\n"
            "listener.bind(endpoint)\nlistener.listen(1)\n"
            "uart,_=listener.accept()\n"
            "uart.sendall(b'KTEST READY 1 arch=riscv64 cpus=2 mode=debug panic=unwind\\n')\n"
            "command=b''\n"
            "while not command.endswith(b'\\n'): command+=uart.recv(1024)\n"
            "with (base/'commands').open('ab') as output: output.write(command)\n"
            "ids=[int(value) for value in command.split()[4].split(b',')]\n"
            + body + "\nuart.close()\nlistener.close()\n",
            encoding="utf-8")
        executable.chmod(0o700)
        args = args or options()
        args.qemu = str(executable)
        args.memory = "128M"
        args.modern = False
        return execute_batch(args, Path("unchanged-kernel.elf"), catalog_of(rows),
                             invocations(rows, args), directory)

    def payload(self, rows, tail):
        """保留完整 suite/case 身份，再加入实际事件序列。"""
        return "\n".join(["KTAP version 1", "1..1", "  # Subtest: os::tests0",
                          f"  1..{len(rows)}", "  # KTEST suite-begin setup",
                          *case_lines(rows[0])[:3], *tail]) + "\n"

    def assert_reports(self, directory, results, statuses, exit_code):
        """同次 JSON/JUnit 必须与真实生命周期结果及出口等级一致。"""
        self.assertEqual([row["status"] for row in results], statuses)
        self.assertEqual(CLI.exit_status(results), exit_code)
        CLI.export_reports(directory, dict(results=results, **CONFIG))
        self.assertEqual(json.loads((directory / "results.json").read_text())["results"], results)
        nodes = ET.parse(directory / "results.xml").findall("testcase")
        self.assertEqual([node.find("properties/property[@name='status']").attrib["value"]
                          for node in nodes], statuses)
        for node, status in zip(nodes, statuses):
            tag = "failure" if status in ("Fail", "Crash", "Timeout") else "error"
            if status in ("Pass", "Skip"):
                self.assertIsNone(node.find("failure"))
                self.assertIsNone(node.find("error"))
            else:
                self.assertEqual(node.find(tag).attrib["type"], status)

    def test_buffered_executed_successor_is_not_replayed_after_deadline(self):
        """A 日志耗尽期限时，B 已真实执行但 begin/final 未读，不能重放其副作用。"""
        rows = [case_record(0), case_record(1)]
        first = self.payload(rows, ["deadline-consuming-business-log",
                                  *case_lines(rows[0])[3:], *case_lines(rows[1], 2),
                                  "ok 1 os::tests0", "# KTEST complete"])
        successor = self.payload([rows[1]], [*case_lines(rows[1])[3:],
                                             "ok 1 os::tests0", "# KTEST complete"])
        body = ("with (base/'effects').open('ab') as output: output.write(b'B\\n')\n"
                f"uart.sendall(({first!r} if ids==[0,1] else {successor!r}).encode())")
        original_line = VM.line

        def consuming_line(vm, deadline):
            """真实读到业务日志后消耗固定时间，其余串口内容保持排队。"""
            line = original_line(vm, deadline)
            if line == "deadline-consuming-business-log":
                time.sleep(0.6)
            return line

        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            with patch.object(VM, "line", consuming_line):
                results = self.execute_guest(rows, body, directory, options(case_timeout=0.5))
            self.assertEqual((directory / "effects").read_bytes(), b"B\n")
            self.assertEqual((directory / "commands").read_bytes(), b"run 0 17 0 0,1\n")
            self.assert_reports(directory, results, ["Timeout", "NotRun"], 1)

    def test_body_return_disarms_expected_fatal_before_quiesce_oom(self):
        """正常返回后的 quiesce OOM 是实际 Crash，不能借 body 的 armed 得到 Pass。"""
        rows = [case_record(expected_fatal="oom")]
        payload = self.payload(rows, ["  # KTEST armed 0 oom",
                                      "  # KTEST phase 0 quiesce", "  # KTEST fatal 0 oom"])
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            results = self.execute_guest(rows, f"uart.sendall({payload.encode()!r})", directory)
            self.assert_reports(directory, results, ["Crash"], 1)
            self.assertNotIn("infrastructure_error", results[0])

    def test_prefatal_business_message_cannot_match_wrong_actual_panic(self):
        """旧业务文本含 wanted，真正 fatal 后只含 wrong 时必须失败而非 Pass。"""
        rows = [case_record(expected_fatal="panic", fatal_message="wanted")]
        payload = self.payload(rows, ["  # KTEST armed 0 panic", "business wanted",
                                      "  # KTEST fatal 0 panic", "actual panic: wrong"])
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            results = self.execute_guest(rows, f"uart.sendall({payload.encode()!r})", directory)
            self.assert_reports(directory, results, ["Crash"], 1)
            self.assertNotIn("infrastructure_error", results[0])

    def test_valid_unexpected_or_mismatched_fatal_is_crash_not_infrastructure(self):
        """同 ID 的真实 fatal 不因未 armed 或类型不符而伪装协议 Error。"""
        for expected, tail in ((None, ["  # KTEST fatal 0 oom"]),
                               ("illegal", ["  # KTEST armed 0 illegal",
                                            "  # KTEST fatal 0 oom"])):
            with self.subTest(expected=expected), tempfile.TemporaryDirectory() as temporary:
                rows = [case_record(expected_fatal=expected)]
                directory = Path(temporary)
                payload = self.payload(rows, tail)
                results = self.execute_guest(rows, f"uart.sendall({payload.encode()!r})", directory)
                self.assert_reports(directory, results, ["Crash"], 1)
                self.assertNotIn("infrastructure_error", results[0])

    def test_noreturn_fatal_self_exit_confirms_safe_successor_without_repeating_failure(self):
        """真实 fatal 自退出0证明后缀未开始，保留先前 Fail 且后缀副作用仅一次。"""
        rows = [case_record(0), case_record(1), case_record(2)]
        first = "\n".join(["KTAP version 1", "1..1", "  # Subtest: os::tests0",
                           "  1..3", "  # KTEST suite-begin setup",
                           *case_lines(rows[0], status="Fail"), *case_lines(rows[1], 2)[:3],
                           "  # KTEST fatal 1 oom", "actual allocation failure"]) + "\n"
        suffix = self.payload([rows[2]], [*case_lines(rows[2])[3:],
                                          "ok 1 os::tests0", "# KTEST complete"])
        body = (f"if ids==[0,1,2]: uart.sendall({first.encode()!r})\n"
                "else:\n"
                " with (base/'effects').open('ab') as output: output.write(b'C\\n')\n"
                f" uart.sendall({suffix.encode()!r})")
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            results = self.execute_guest(rows, body, directory)
            self.assertEqual((directory / "commands").read_bytes(),
                             b"run 0 17 0 0,1,2\nrun 0 17 0 2\n")
            self.assertEqual((directory / "effects").read_bytes(), b"C\n")
            self.assert_reports(directory, results, ["Fail", "Crash", "Pass"], 1)
            self.assertFalse(any("infrastructure_error" in row for row in results))

    def test_unconfirmed_process_crash_preserves_failure_and_does_not_retry_suffix(self):
        """仅 begin 加真实进程退出9不足以证明后缀未开始，首次 Fail/Crash 不重放。"""
        rows = [case_record(0), case_record(1), case_record(2)]
        payload = "\n".join(["KTAP version 1", "1..1", "  # Subtest: os::tests0",
                             "  1..3", "  # KTEST suite-begin setup",
                             *case_lines(rows[0], status="Fail"),
                             *case_lines(rows[1], 2)[:3]]) + "\n"
        body = f"uart.sendall({payload.encode()!r})\nuart.close()\nraise SystemExit(9)"
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            results = self.execute_guest(rows, body, directory)
            self.assertEqual((directory / "commands").read_bytes(), b"run 0 17 0 0,1,2\n")
            self.assert_reports(directory, results, ["Fail", "Crash", "NotRun"], 1)

    def test_expected_fatal_needs_postevent_message_and_real_zero_exit(self):
        """正确消息只在事件后有效；活进程、非零退出和缺实际事件均不能 Pass。"""
        rows = [case_record(expected_fatal="panic", fatal_message="wanted")]
        for tail, process_tail, status in (
                (["  # KTEST armed 0 panic", "business wanted",
                  "  # KTEST fatal 0 panic", "actual panic: wanted"],
                 "uart.close()\nlistener.close()\nimport os\nos._exit(0)", "Pass"),
                (["  # KTEST armed 0 panic", "  # KTEST fatal 0 panic",
                  "actual panic: wanted"], "uart.close()\nraise SystemExit(9)", "Crash"),
                (["  # KTEST armed 0 panic", "  # KTEST fatal 0 panic",
                  "actual panic: wanted"], "uart.shutdown(socket.SHUT_WR)\ntime.sleep(1)", "Crash"),
                (["  # KTEST armed 0 panic", "actual panic: wanted"], "", "Crash")):
            with self.subTest(status=status, process_tail=process_tail), \
                    tempfile.TemporaryDirectory() as temporary:
                directory = Path(temporary)
                payload = self.payload(rows, tail)
                body = f"uart.sendall({payload.encode()!r})\n{process_tail}"
                results = self.execute_guest(rows, body, directory, options(cleanup_timeout=0.1))
                self.assert_reports(directory, results, [status], 0 if status == "Pass" else 1)
                self.assertEqual((directory / "commands").read_bytes(), b"run 0 17 0 0\n")

    def test_wrong_identity_duplicate_unknown_or_late_fatal_remains_protocol_error(self):
        """真实传输中的错 ID、重复、未知类型、终结后记录仍为 Error/出口2。"""
        rows = [case_record(0)]
        for tail, status in ((["  # KTEST fatal 9 oom"], "Error"),
                             (["  # KTEST fatal 0 invented"], "Error"),
                             (["  # KTEST fatal 0 oom", "  # KTEST fatal 0 oom"], "Error"),
                             ([*case_lines(rows[0])[3:], "ok 1 os::tests0", "# KTEST complete",
                               "  # KTEST fatal 0 oom"], "Pass")):
            with self.subTest(tail=tail), tempfile.TemporaryDirectory() as temporary:
                directory = Path(temporary)
                payload = self.payload(rows, tail)
                results = self.execute_guest(rows, f"uart.sendall({payload.encode()!r})", directory)
                # 迟到控制不能覆盖已经发布的首次 Pass，只在同一行补协议错误。
                if status == "Pass":
                    self.assertEqual([row["status"] for row in results], ["Pass"])
                    self.assertEqual(CLI.exit_status(results), 2)
                    self.assertTrue(results[0]["infrastructure_error"])
                    CLI.export_reports(directory, dict(results=results, **CONFIG))
                    self.assertEqual(json.loads((directory / "results.json").read_text())["results"], results)
                    self.assertEqual(ET.parse(directory / "results.xml").find("testcase/error").attrib["type"],
                                     "Protocol")
                else:
                    self.assert_reports(directory, results, ["Error"], 2)


if __name__ == "__main__":
    unittest.main()
