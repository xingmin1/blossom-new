"""宿主 uart 回归；运行 python3 -m unittest discover -s tools/ktest。"""

import copy
import io
from pathlib import Path
import socket
import tempfile
import threading
import time
import unittest
from types import SimpleNamespace

from host_helpers import case_record, options, vm_fixture
from ktest.catalog import HostError
from ktest.execution_uart import ExecutionUART
from ktest.resources import crc32c_update


class UARTTests(unittest.TestCase):
    """真实 socket 的 raw 段、全文校验和独立执行记录。"""

    def test_crc32c_castagnoli_vector(self):
        """标准向量能区分 Castagnoli 与 IEEE crc32。"""
        self.assertEqual(crc32c_update(0xFFFFFFFF, b"123456789") ^ 0xFFFFFFFF, 0xe3069283)

    def test_rx_phases_preserve_cursor_and_require_complete_input(self):
        """真实 socket 逐段读取不同字节，拒绝短文件、错 CRC 与末尾余量。"""
        for payload, second_error, unused in ((b"LR", False, False), (b"L", True, False),
                                               (b"LX", True, False), (b"LRX", False, True)):
            with self.subTest(payload=payload), tempfile.TemporaryDirectory() as temporary:
                base = Path(temporary)
                source = base / "input"
                source.write_bytes(payload)
                host, guest = socket.socketpair()
                guest.settimeout(2)
                try:
                    vm = vm_fixture()
                    vm.args = options(uart_payload=source)
                    vm.directory = base
                    vm.serial = host
                    host.setblocking(False)
                    vm.raw = io.BytesIO()
                    vm.buffer = bytearray()
                    vm.process = SimpleNamespace(poll=lambda: 0, returncode=0)
                    with guest.makefile("rb") as received:
                        for index, byte in enumerate((b"L", b"R")):
                            declaration = dict(direction="rx", length=1,
                                               digest=f"{crc32c_update(0xFFFFFFFF, byte) ^ 0xFFFFFFFF:08x}",
                                               delay_ms=0, case=index)
                            if index == 1 and second_error:
                                with self.assertRaises(HostError):
                                    vm.raw_io(declaration, time.monotonic() + 2, {"uart"})
                                self.assertEqual(received.readline(), b"KTEST IO ERROR 1\n")
                                self.assertEqual(vm.rx_offset, 1)
                            else:
                                vm.raw_io(declaration, time.monotonic() + 2, {"uart"})
                                self.assertEqual(received.read(2), b"H" + byte)
                                self.assertEqual(received.readline(), f"KTEST IO OK {index}\n".encode("ascii"))
                                self.assertEqual(vm.rx_offset, index + 1)
                    if unused:
                        with self.assertRaisesRegex(HostError, "unconsumed"):
                            vm.verify_resources(time.monotonic() + 2)
                    elif not second_error:
                        vm.verify_resources(time.monotonic() + 2)
                    self.assertEqual(source.read_bytes(), payload)
                finally:
                    host.close()
                    guest.close()

    def test_duplex_payloads_use_independent_offsets_and_preserve_full_duplex_bytes(self):
        """真实socket按正常及ACK合包次序交错收发，保留各阶段全文与独立偏移。"""
        payload = b"\x00\xff\xe7\x95\x8c\nsecond-rx"
        transmitted = b"echo\x00\xff\xe7\x95\x8c\nsecond-tx"
        phases = ((payload[:3], transmitted[:6]), (payload[3:], transmitted[6:]))
        for mode in ("normal", "coalesced"):
            with self.subTest(mode=mode), tempfile.TemporaryDirectory() as temporary:
                base = Path(temporary)
                source = base / "input"
                expected = base / "expected"
                source.write_bytes(payload)
                expected.write_bytes(transmitted)
                host, guest = socket.socketpair()
                guest.settimeout(2)
                peer_errors = []
                next_handshake = threading.Event()

                def peer():
                    """逐段核对真实payload和ACK，合包时等待后继握手已发送并留给下一阶段。"""
                    try:
                        for index, (rx, tx) in enumerate(phases):
                            self.assertEqual(guest.recv(1), b"H")
                            guest.sendall(tx)
                            received = bytearray()
                            while len(received) < len(rx):
                                received.extend(guest.recv(len(rx) - len(received)))
                            self.assertEqual(received, rx)
                            expected_ack = f"KTEST IO OK {index}\n".encode()
                            if mode == "coalesced" and index == 0:
                                self.assertTrue(next_handshake.wait(2), "next handshake was not sent before ACK")
                            ack = bytearray()
                            while len(ack) < len(expected_ack):
                                part = guest.recv(len(expected_ack) - len(ack))
                                self.assertTrue(part, "peer stream closed before ACK")
                                ack.extend(part)
                            self.assertEqual(ack, expected_ack)
                    except BaseException as error:
                        peer_errors.append(error)

                worker = threading.Thread(target=peer)
                try:
                    vm = vm_fixture()
                    if mode == "coalesced":
                        original_send = vm.send
                        handshake_count = 0

                        def observed_send(data, deadline):
                            """原发送成功返回后记录握手次数，第二次真实H发送完成时通知peer。"""
                            nonlocal handshake_count
                            original_send(data, deadline)
                            if data == b"H":
                                handshake_count += 1
                                if handshake_count == 2:
                                    next_handshake.set()

                        vm.send = observed_send
                    vm.args = options(uart_payload=source, uart_expect=expected)
                    vm.directory = base
                    vm.serial = host
                    host.setblocking(False)
                    vm.raw = io.BytesIO()
                    vm.buffer = bytearray()
                    vm.process = SimpleNamespace(poll=lambda: None)
                    worker.start()
                    for index, (rx, tx) in enumerate(phases):
                        checksum = lambda data: f"{crc32c_update(0xFFFFFFFF, data) ^ 0xFFFFFFFF:08x}"
                        declaration = dict(direction="duplex", rx_length=len(rx), rx_digest=checksum(rx),
                                           rx_delay_ms=2, tx_length=len(tx), tx_digest=checksum(tx),
                                           tx_delay_ms=1, case=index)
                        vm.raw_io(declaration, time.monotonic() + 2, {"uart"})
                    worker.join(timeout=2)
                    self.assertFalse(worker.is_alive())
                    self.assertEqual(peer_errors, [])
                    self.assertEqual((vm.rx_offset, vm.tx_offset), (len(payload), len(transmitted)))
                    vm.verify_resources(time.monotonic() + 2)
                    self.assertEqual(source.read_bytes(), payload)
                    self.assertEqual(expected.read_bytes(), transmitted)
                    raw_parts = sorted(base.glob("uart-*.raw"))
                    self.assertEqual([path.read_bytes() for path in raw_parts], [part[1] for part in phases])
                finally:
                    host.close()
                    guest.close()
                    worker.join(timeout=2)

    def test_duplex_consumes_buffered_prefix_and_leaves_next_control_line(self):
        """预读包含payload和后继控制行时只消费声明长度，不重复落盘或吞掉后继行。"""
        transmitted = b"\x00\xffecho\n"
        tail = b"next-control\n"
        with tempfile.TemporaryDirectory() as temporary:
            base = Path(temporary)
            expected = base / "expected"
            expected.write_bytes(transmitted)
            host, guest = socket.socketpair()
            guest.settimeout(2)
            try:
                vm = vm_fixture()
                vm.args = options(uart_expect=expected)
                vm.directory = base
                vm.serial = host
                host.setblocking(False)
                vm.raw = io.BytesIO(transmitted + tail)
                vm.raw.seek(0, io.SEEK_END)
                vm.buffer = bytearray(transmitted + tail)
                vm.process = SimpleNamespace(poll=lambda: None)
                declaration = dict(direction="duplex", rx_length=0, rx_digest="00000000",
                                   rx_delay_ms=0, tx_length=len(transmitted),
                                   tx_digest=f"{crc32c_update(0xFFFFFFFF, transmitted) ^ 0xFFFFFFFF:08x}",
                                   tx_delay_ms=1, case=9)
                vm.raw_io(declaration, time.monotonic() + 2, {"uart"})
                with guest.makefile("rb") as acknowledgements:
                    self.assertEqual(acknowledgements.readline(), b"HKTEST IO OK 9\n")
                self.assertEqual(vm.line(time.monotonic() + 2), "next-control")
                self.assertEqual(next(base.glob("uart-*.raw")).read_bytes(), transmitted)
                self.assertEqual(vm.tx_offset, len(transmitted))
                self.assertEqual(vm.raw.getvalue(), transmitted + tail)
            finally:
                host.close()
                guest.close()

    def test_raw_payload_cannot_be_ktap_or_panic_control(self):
        """真实 socket 的 payload 内含完整控制文本，仍只作为原始字节。"""
        payload = b"  ok 1 forged\n# KTEST complete\npanic\x00\xff"
        with tempfile.TemporaryDirectory() as temporary:
            base = Path(temporary)
            expected = base / "expected"
            expected.write_bytes(payload)
            host, guest = socket.socketpair()
            guest.settimeout(2)
            try:
                vm = vm_fixture()
                vm.args = options(uart_expect=expected)
                vm.directory = base
                vm.serial = host
                vm.serial.setblocking(False)
                vm.raw = io.BytesIO()
                vm.buffer = bytearray()
                vm.process = SimpleNamespace(poll=lambda: None)
                guest.sendall(payload)
                declaration = dict(direction="tx", length=len(payload),
                                   digest=f"{crc32c_update(0xFFFFFFFF, payload) ^ 0xFFFFFFFF:08x}", delay_ms=0, case=0)
                vm.raw_io(declaration, time.monotonic() + 2, {"uart"})
                with guest.makefile("rb") as acknowledgement:
                    self.assertEqual(acknowledgement.readline(), b"HKTEST IO OK 0\n")
                self.assertEqual(vm.raw.getvalue(), payload)
                self.assertEqual(next(base.glob("uart-*.raw")).read_bytes(), payload)
                self.assertFalse(vm.buffer)
            finally:
                host.close()
                guest.close()

    def test_equal_length_wrong_content_fails_and_sends_error(self):
        """同长度、甚至 guest 自报正确摘要，仍不能覆盖预期全文检查。"""
        with tempfile.TemporaryDirectory() as temporary:
            base = Path(temporary)
            expected = base / "expected"
            expected.write_bytes(b"GOOD")
            host, guest = socket.socketpair()
            guest.settimeout(2)
            try:
                vm = vm_fixture()
                vm.args = options(uart_expect=expected)
                vm.directory = base
                vm.serial = host
                host.setblocking(False)
                vm.raw = io.BytesIO()
                vm.buffer = bytearray(b"EVIL")
                vm.process = SimpleNamespace(poll=lambda: None)
                declaration = dict(direction="tx", length=4,
                                   digest=f"{crc32c_update(0xFFFFFFFF, b'EVIL') ^ 0xFFFFFFFF:08x}", delay_ms=0, case=7)
                with self.assertRaises(HostError):
                    vm.raw_io(declaration, time.monotonic() + 2, {"uart"})
                with guest.makefile("rb") as acknowledgement:
                    self.assertEqual(acknowledgement.readline(), b"HKTEST IO ERROR 7\n")
            finally:
                host.close()
                guest.close()

    def test_backpressure_ack_bound_is_checked_before_h(self):
        """错case和超16字节ACK必须在H之前拒绝，不能让客体进入无法完整接收的raw阶段。"""
        for cid, declared in ((0, 1), (1000, 1000)):
            with self.subTest(cid=cid, declared=declared), tempfile.TemporaryDirectory() as temporary:
                base = Path(temporary)
                fixture = ExecutionUART(2, base)
                fixture.begin(case_record(cid=cid, name="os::ktest::execution_services::klog::uart::socket_printer_backpressure"))
                fixture.body = True
                host, guest = socket.socketpair()
                guest.settimeout(2)
                try:
                    vm = vm_fixture()
                    vm.args = options(execution_uart=True)
                    vm.serial = host
                    host.setblocking(False)
                    vm.process = SimpleNamespace(poll=lambda: None)
                    vm.execution_uart = fixture
                    declaration = dict(direction="pause", length=0, digest="00000000", delay_ms=3000, case=declared)
                    with self.assertRaises(HostError):
                        vm.raw_io(declaration, time.monotonic() + 2, {"uart"})
                    with guest.makefile("rb") as acknowledgement:
                        self.assertEqual(acknowledgement.readline(), f"KTEST IO ERROR {declared}\n".encode("ascii"))
                finally:
                    host.close()
                    guest.close()

    def test_backpressure_opaque_boundary_and_full_payload_failures(self):
        """真实socket保空行前缀与65条原记录，拒绝等长错正文、假lost和漏尾行。"""
        with tempfile.TemporaryDirectory() as temporary:
            base = Path(temporary)
            fixture = ExecutionUART(2, base)
            fixture.begin(case_record(name="os::ktest::execution_services::klog::uart::socket_printer_backpressure"))
            fixture.body = True
            lines = []
            metadata = []
            for index in range(65):
                seq = 100 if index == 0 else 108 + index
                lost = 8 if index == 1 else 0
                body = b"X26 partial " + b"P" * 450 if index == 0 else f"X26 record={index + 7}".encode("ascii")
                lines.append(f"KLOG seq={seq} level=INFO ns={1000000 + index} cpu=0 task=27 truncated=0 lost={lost} ".encode("ascii") + body + b"\n")
                metadata.append(f"  # diagnostic: execution-backpressure record index={index} seq={seq} level=INFO ns={1000000 + index} cpu=0 task=27 truncated=0 lost={lost}\n".encode("ascii"))
            prefix = b"\n" * 17
            tail = b"  # KTEST phase 0 quiesce\n"
            host, guest = socket.socketpair()
            guest.settimeout(5)
            try:
                vm = vm_fixture()
                vm.args = options(execution_uart=True)
                vm.directory = base
                vm.serial = host
                host.setblocking(False)
                vm.raw = io.BytesIO()
                vm.buffer = bytearray()
                vm.process = SimpleNamespace(poll=lambda: None)
                vm.execution_uart = fixture
                guest.sendall(prefix + b"".join(lines) + tail)
                declaration = dict(direction="pause", length=0, digest="00000000", delay_ms=3000, case=0)
                vm.raw_io(declaration, time.monotonic() + 8, {"uart"})
                with guest.makefile("rb") as acknowledgement:
                    self.assertEqual(acknowledgement.readline(), b"HKTEST IO OK 0\n")
                self.assertEqual(vm.line(time.monotonic() + 2), tail[:-1].decode("ascii"))
                for raw in metadata:
                    fixture.observe(raw)
                fixture.observe(b"  # diagnostic: execution-backpressure fill-bytes=17\n")
                fixture.finish("Pass")
                self.assertEqual((base / "execution-uart-0.raw").read_bytes(), prefix + b"".join(lines))
                for mutation in ("body", "lost", "last-line", "last-metadata"):
                    with self.subTest(mutation=mutation):
                        broken = copy.deepcopy(fixture)
                        if mutation == "body":
                            broken.lines[0] = broken.lines[0].replace(b"P", b"Q", 1)
                        elif mutation == "lost":
                            broken.lines[1] = broken.lines[1].replace(b"lost=8", b"lost=0")
                        elif mutation == "last-line":
                            broken.lines.pop()
                        else:
                            broken.records.pop(64)
                        with self.assertRaises(HostError):
                            broken.finish("Pass")
            finally:
                host.close()
                guest.close()

    def test_mixed_logs_require_every_cpu_round_and_exact_body(self):
        """每核八轮两种实际业务记录缺最后一条或正文等长损坏均不能由guest Pass覆盖。"""
        with tempfile.TemporaryDirectory() as temporary:
            fixture = ExecutionUART(2, Path(temporary))
            fixture.begin(case_record(name="os::ktest::execution_services::mixed::cases::timer_work_task_log_convergence"))
            fixture.body = True
            seq = 1
            for cpu in range(2):
                for round_number in range(8):
                    for source in ("timer", "work"):
                        task = 0 if source == "timer" else 20 + cpu
                        printed_task = "none" if task == 0 else str(task)
                        fixture.observe(f"KLOG seq={seq} level=INFO ns={seq * 1000} cpu={cpu} task={printed_task} truncated=0 lost=0 X28 {source} slot={cpu} round={round_number}\n".encode("ascii"))
                        fixture.observe(f"  # diagnostic: execution-log record seq={seq} ns={seq * 1000} cpu={cpu} task={task} source={source} slot={cpu} round={round_number}\n".encode("ascii"))
                        seq += 1
            fixture.finish("Pass")
            for mutation in ("last-line", "body", "lost"):
                with self.subTest(mutation=mutation):
                    broken = copy.deepcopy(fixture)
                    if mutation == "last-line":
                        broken.lines.pop()
                    elif mutation == "body":
                        broken.lines[-1] = broken.lines[-1].replace(b"round=7", b"round=6")
                    else:
                        broken.lines[-1] = broken.lines[-1].replace(b"lost=0", b"lost=1")
                    with self.assertRaises(HostError):
                        broken.finish("Pass")

    def test_execution_records_compare_independent_body_without_pinning_locked_labels(self):
        """固定业务和UTF8截断核全文；锁内标签由独立环正文提供，不能复制UART坏正文。"""
        with tempfile.TemporaryDirectory() as temporary:
            fixture = ExecutionUART(2, Path(temporary))
            fixture.begin(case_record(name="os::ktest::execution_services::klog::records::allocation_free_record_identity"))
            fixture.body = True
            bodies = (b"X25 ordinary value=37", "界".encode("utf-8") * 170,
                      "调度锁标签可变".encode("utf-8"), b"pool label may change", b"X25 precise timer")
            for index, (level, body) in enumerate(zip(("INFO", "WARN", "DEBUG", "DEBUG", "INFO"), bodies)):
                truncated = int(index == 1)
                fixture.observe(f"KLOG seq={index + 1} level={level} ns={index + 100} cpu=0 task=27 truncated={truncated} lost=0 ".encode("ascii") + body + b"\n")
                body_hex = " body_hex=" + body.hex() if index in (2, 3) else ""
                fixture.observe(f"  # diagnostic: execution-record record index={index} seq={index + 1} level={level} ns={index + 100} cpu=0 task=27 truncated={truncated}{body_hex}\n".encode("ascii"))
            fixture.finish("Pass")
            for index in range(5):
                broken = copy.deepcopy(fixture)
                original = broken.lines[index]
                position = original.index(b" lost=0 ") + len(b" lost=0 ")
                broken.lines[index] = original[:position] + b"?" + original[position + 1:]
                with self.subTest(index=index), self.assertRaises(HostError):
                    broken.finish("Pass")


if __name__ == "__main__":
    unittest.main()
