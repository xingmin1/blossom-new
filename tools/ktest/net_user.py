"""net-user 的具体宿主消费者，端口和原字节归同一 VM，QEMU wait 后才释放。

TCP 两向原文单独落盘；UDP 每次 recvfrom/sendto 的边界、地址和全文留在 peer.json。
仅三个明确 case tag 选择协议，宿主观察不替代客体 NIC、DMA 或资源账的断言。
"""

import json
import os
import select
import socket
import threading
import time

from .catalog import HostError, require


class PeerStopped(Exception):
    """实际取消管道已就绪，消费者不再开始下一次 socket 操作。"""


class NetUserPeer:
    """每 VM 两个独占 loopback 端点，具体 case 线程只使用原绝对期限。"""

    def __init__(self, directory):
        """QEMU 启动前实际绑定动态端口，任一步失败都归还已取得句柄。"""
        self.directory = directory
        self.tcp = self.udp = None
        self.active = None
        self.last = None
        self.unassigned = dict(case=None, status=None, errors=[], events=[], udp_received=0)
        self.cases = []
        self.failures = {}
        self.closed = False
        self.closed_at = None
        self.verified_at = None
        try:
            self.tcp = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
            self.tcp.bind(("127.0.0.1", 0))
            self.tcp.listen(1)
            self.tcp.setblocking(False)
            self.udp = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
            self.udp.bind(("127.0.0.1", 0))
            self.udp.setblocking(False)
            self.addresses = dict(tcp=self.tcp.getsockname(), udp=self.udp.getsockname())
            self.ports = (self.addresses["tcp"][1], self.addresses["udp"][1])
            self._save()
        except BaseException:
            for endpoint in (self.tcp, self.udp):
                if endpoint is not None:
                    endpoint.close()
            raise

    def _save(self):
        """只有线程准备前或实际 join 后写快照，错误与原字节不归一化。"""
        report = dict(addresses=self.addresses, ports=self.ports, cases=self.cases,
                      unassigned=self.unassigned, verified_at=self.verified_at,
                      closed_at=self.closed_at, failures=self.failures)
        (self.directory / "peer.json").write_text(
            json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")

    def _error(self, row, error):
        """保留首次及后继实际错误，只有客体 Pass 才参与宿主失败判定。"""
        row["errors"].append(dict(type=type(error).__name__, message=str(error),
                                  repr=repr(error), errno=getattr(error, "errno", None),
                                  at=time.monotonic()))
        if row["status"] == "Pass":
            self.failures[row["case"]] = "; ".join(entry["message"] for entry in row["errors"])

    def begin(self, case, deadline):
        """在原 begin 的 case deadline 确定后，以目录 tags 绑定唯一具体消费者。"""
        if self.active is not None:
            require(not self.active["thread"].is_alive(), "previous network peer thread is still active")
            self._release_pipe()
            self.active = None
        self._drain(self.last or self.unassigned)
        tags = case["tags"]
        kinds = [kind for tag, kind in (("network-nic-tcp", "tcp"),
                                       ("network-nic-udp", "udp"),
                                       ("network-nic-native", "native")) if tag in tags]
        require(len(kinds) <= 1, "conflicting network peer tags")
        if not kinds:
            self.active = None
            self._save()
            return
        row = dict(case=case["case"], name=case["name"], tags=list(tags), kind=kinds[0],
                   deadline=deadline, begin_at=time.monotonic(), status=None, finish_at=None,
                   thread_entered_at=None, thread_exited_at=None, joined_at=None,
                   complete=False, errors=[], events=[], tcp_eof=False,
                   tcp_received=0, tcp_sent=0, udp_received=0, udp_sent=0)
        reader, writer = os.pipe()
        self.cases.append(row)
        self.last = row
        thread = threading.Thread(target=self._serve, args=(row, reader),
                                  name=f"net-user-{case['case']}")
        self.active = dict(row=row, reader=reader, writer=writer, cancelled=False, thread=thread)
        try:
            self._save()
            thread.start()
        except BaseException as error:
            self._error(row, error)
            self._release_pipe()
            self.active = None
            raise

    def _ready(self, endpoint, row, reader, writing=False):
        """同一绝对期限内等待真实 socket 或取消管道，不用 sleep 猜网络窗口。"""
        remaining = row["deadline"] - time.monotonic()
        require(0 < remaining, "network peer absolute deadline expired")
        readable, writable, _ = select.select(
            [reader] if writing else [endpoint, reader], [endpoint] if writing else [], [], remaining)
        if endpoint in (writable if writing else readable):
            return
        if reader in readable:
            raise PeerStopped("network peer cancelled")
        raise HostError("network peer absolute deadline expired")

    def _serve(self, row, reader):
        """具体业务错误留在本例观察中，普通 UDP 须四条完整收发才结束消费者。"""
        row["thread_entered_at"] = time.monotonic()
        try:
            if row["kind"] == "tcp":
                self._tcp(row, reader)
                row["complete"] = True
            else:
                try:
                    self._udp(row, reader)
                except PeerStopped:
                    if row["kind"] == "udp":
                        require(row["udp_received"] == 4 and row["udp_sent"] == 4,
                                "UDP peer did not complete exactly four messages")
                    row["complete"] = True
        except (HostError, OSError, PeerStopped) as error:
            self._error(row, error)
        finally:
            if "tcp_peer" in row:
                row["events"].append(dict(kind="tcp-close", at=time.monotonic()))
            row["thread_exited_at"] = time.monotonic()

    def _tcp(self, row, reader):
        """一次真实连接全文核对 98317 字节及 EOF，再发送另一图样的 65539 字节。"""
        self._ready(self.tcp, row, reader)
        connection, address = self.tcp.accept()
        with connection:
            row["tcp_peer"] = address
            connection.setblocking(False)
            row["events"].append(dict(kind="tcp-accept", source=address,
                                       destination=connection.getsockname(), at=time.monotonic()))
            received_path = f"peer-{row['case']}-tcp-received.raw"
            sent_path = f"peer-{row['case']}-tcp-sent.raw"
            row["tcp_received_file"] = received_path
            row["tcp_sent_file"] = sent_path
            with (self.directory / received_path).open("xb", buffering=0) as received, \
                    (self.directory / sent_path).open("xb", buffering=0) as sent:
                while True:
                    self._ready(connection, row, reader)
                    block = connection.recv(65536)
                    offset = row["tcp_received"]
                    received.write(block)
                    row["tcp_received"] += len(block)
                    row["events"].append(dict(kind="tcp-recv", offset=offset, length=len(block),
                                               source=address, destination=connection.getsockname(),
                                               at=time.monotonic()))
                    if not block:
                        row["tcp_eof"] = True
                        require(row["tcp_received"] == 98317, "TCP request ended before full payload")
                        break
                    require(row["tcp_received"] <= 98317, "TCP request exceeds expected payload")
                    expected = bytes((i * 37 + (i >> 8) + 19) & 255
                                     for i in range(offset, row["tcp_received"]))
                    require(block == expected, f"TCP request payload mismatch at offset {offset}")
                response = bytes((i * 53 + (i >> 7) + 91) & 255 for i in range(65539))
                while row["tcp_sent"] < len(response):
                    self._ready(connection, row, reader, writing=True)
                    offset = row["tcp_sent"]
                    count = connection.send(memoryview(response)[offset:])
                    require(0 < count, "TCP response send made no progress")
                    sent.write(response[offset:offset + count])
                    row["tcp_sent"] += count
                    row["events"].append(dict(kind="tcp-send", offset=offset, length=count,
                                               source=connection.getsockname(), destination=address,
                                               at=time.monotonic()))
                connection.shutdown(socket.SHUT_WR)
                row["events"].append(dict(kind="tcp-shutdown-write", at=time.monotonic()))

    def _udp_record(self, row, kind, data, address):
        """每条 UDP 记录保实际消息边界、两端地址和完整原 bytes，包括零长消息。"""
        sending = kind == "udp-send"
        row["events"].append(dict(kind=kind, length=len(data), bytes_hex=data.hex(),
                                   source=self.addresses["udp"] if sending else address,
                                   destination=address if sending else self.addresses["udp"],
                                   at=time.monotonic()))

    def _udp_send(self, row, reader, data, address):
        """每条应答只真实发送一次，返回零只有零长消息合法。"""
        self._ready(self.udp, row, reader, writing=True)
        count = self.udp.sendto(data, address)
        self._udp_record(row, "udp-send", data[:count], address)
        require(count == len(data), "UDP response length mismatch")
        row["udp_sent"] += 1

    def _udp(self, row, reader):
        """普通 UDP 核四条并接收第五条；native 响应 NB/count、P 和 NP/index。"""
        source = None
        while True:
            self._ready(self.udp, row, reader)
            data, address = self.udp.recvfrom(65535)
            self._udp_record(row, "udp-recv", data, address)
            index = row["udp_received"]
            row["udp_received"] += 1
            if row["kind"] == "udp":
                require(index < 4, "unexpected fifth UDP message; oversized send reached host")
                if source is None:
                    source = address
                require(address == source, "UDP request source changed between messages")
                length = (17, 0, 1472, 8193)[index]
                expected = bytes((i * 29 + (i >> 4) + 45) & 255 for i in range(length))
                require(data == expected, f"UDP request payload mismatch at message {index}")
                self._udp_send(row, reader, bytes(byte ^ 0xa5 for byte in data), address)
            elif data == b"P":
                self._udp_send(row, reader, b"Q", address)
            elif data[:2] == b"NP":
                require(len(data) == 4, "native UDP indexed request must be exactly four bytes")
                index = int.from_bytes(data[2:], "little")
                require(index <= 32, "native UDP request index outside 0..32")
                self._udp_send(row, reader, b"NQ" + data[2:], address)
            else:
                require(len(data) == 4 and data[:2] == b"NB", "unexpected native UDP request")
                count = int.from_bytes(data[2:], "little")
                require(1 <= count <= 32, "native UDP burst count outside 1..32")
                for index in range(count):
                    body = bytes((index * 17 + i * 31 + 7) & 255 for i in range(64))
                    self._udp_send(row, reader, b"NR" + index.to_bytes(2, "little") + body, address)

    def _cancel(self):
        """管道唤醒正在 select 的实际消费者，不提前释放 live 端口。"""
        if self.active is not None and not self.active["cancelled"]:
            os.write(self.active["writer"], b"X")
            self.active["cancelled"] = True
            self.active["row"]["cancel_at"] = time.monotonic()

    def _join(self, deadline):
        """线程等待只使用原绝对期限，首次真实 join 时间保留到 VM 结束。"""
        thread = self.active["thread"]
        thread.join(timeout=max(0, deadline - time.monotonic()))
        if thread.is_alive():
            return False
        if self.active["row"]["joined_at"] is None:
            self.active["row"]["joined_at"] = time.monotonic()
        return True

    def finish(self, status, deadline):
        """原 status 后只有 Pass 要求完整消费者；Fail/Error 保原观察且不被覆盖。"""
        if self.active is None:
            return
        row = self.active["row"]
        require(deadline == row["deadline"], "network peer case deadline changed")
        row["status"] = status
        row["finish_at"] = time.monotonic()
        if row["kind"] != "tcp" or status != "Pass":
            self._cancel()
        joined = self._join(deadline)
        if not joined:
            self._error(row, HostError("network peer thread exceeded absolute case deadline"))
        else:
            try:
                self._drain(row)
            except OSError as error:
                self._error(row, error)
            if status == "Pass" and not row["complete"] and not row["errors"]:
                self._error(row, HostError("network peer consumer is incomplete"))
            if status == "Pass" and row["errors"]:
                self.failures[row["case"]] = "; ".join(entry["message"] for entry in row["errors"])
            self._save()
        if status == "Pass":
            require(joined and row["complete"] and not row["errors"],
                    self.failures.get(row["case"], "network peer consumer is incomplete"))

    def _drain(self, row):
        """线程停稳后及 QEMU wait 后读尽真正已排队的额外消息，不用静默窗口证明。"""
        while select.select([self.udp], [], [], 0)[0]:
            data, address = self.udp.recvfrom(65535)
            self._udp_record(row, "udp-recv", data, address)
            row["udp_received"] += 1
            self._error(row, HostError("unexpected UDP message after network consumer finished"))
        while select.select([self.tcp], [], [], 0)[0]:
            connection, address = self.tcp.accept()
            with connection:
                row["events"].append(dict(kind="unexpected-tcp-accept", source=address,
                                           destination=connection.getsockname(), at=time.monotonic()))
            self._error(row, HostError("unexpected additional TCP connection"))

    def verify(self, deadline):
        """VM 真实退出后保存完整错误和尾部证据，Pass 才受具体 peer 错误约束。"""
        if self.active is not None:
            self._cancel()
            joined = self._join(deadline)
            require(joined, "network peer thread exceeded VM cleanup deadline")
        self._drain(self.last or self.unassigned)
        self.verified_at = time.monotonic()
        self._save()
        require(not self.failures, "network peer verification failed: " + str(self.failures))

    def _release_pipe(self):
        """对应线程已经结束或尚未启动时，归还本例的两个取消管道句柄。"""
        os.close(self.active["reader"])
        os.close(self.active["writer"])

    def close(self):
        """调用者已真正 wait QEMU；取消并 join 全尾后才关闭两个原 bound socket。"""
        if self.closed:
            return
        self._cancel()
        if self.active is not None:
            self.active["thread"].join()
            self.active["row"]["close_joined_at"] = time.monotonic()
            self._release_pipe()
        row = self.last or self.unassigned
        try:
            self._drain(row)
        except OSError as error:
            self._error(row, error)
        self.tcp.close()
        self.udp.close()
        self.closed = True
        self.closed_at = time.monotonic()
        self._save()
