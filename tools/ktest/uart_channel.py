"""独占测试串口的原始日志、字节游标和具体执行日志观察。"""

import select
import time

from .catalog import HostError, require
from .execution_uart import ExecutionUART
from .resources import crc32c_update


class DeadlineExpired(RuntimeError):
    """宿主单调绝对期限已经到达。"""


class VMExited(RuntimeError):
    """QEMU 已退出或其 UART socket 已关闭。"""


class UartChannel:
    """持有串口和原始字节状态；期限与取消由会话提供的 check 判断。"""

    def __init__(self, args, directory, check):
        """准备日志与独立 RX/TX 游标；真实 socket 由会话连接时交付。"""
        self.args = args
        self.directory = directory
        self.check = check
        self.serial = None
        self.buffer = bytearray()
        self.rx_offset = 0
        self.rx_used = False
        self.tx_offset = 0
        self.tx_used = False
        self.execution_uart = ExecutionUART(args.cpus, directory)
        self.raw = (directory / "uart.log").open("xb", buffering=0)

    def receive(self, deadline, limit=65536):
        """逐块落盘原文；UART EOF 后仍由调用者确认 QEMU 真实退出。"""
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
            if 0 <= newline:
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


    def verify_consumed(self):
        """真实退出后检查声明输入和 duplex 预期都已按游标完整消费。"""
        if self.rx_used:
            require(self.args.uart_payload.stat().st_size == self.rx_offset,
                    "UART input has unconsumed bytes at VM exit")
        if self.tx_used:
            require(self.args.uart_expect.stat().st_size == self.tx_offset,
                    "UART expected output has unconsumed bytes at VM exit")

    def close(self):
        """会话已 wait 且 peer 已 join 后关闭串口和原文日志。"""
        if self.serial is not None:
            self.serial.close()
        self.raw.close()
