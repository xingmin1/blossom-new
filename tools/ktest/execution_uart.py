"""执行日志的具体宿主业务 fixture；KTAP 层不解释记录、缺口或正文。"""

import re

from .catalog import require

NUMBER = rb"([0-9]{1,20})"
RECORD = re.compile(rb"  # diagnostic: execution-record record index=" + NUMBER
                    + rb" seq=" + NUMBER + rb" level=(INFO|WARN|DEBUG) ns=" + NUMBER
                    + rb" cpu=" + NUMBER + rb" task=" + NUMBER
                    + rb" truncated=([01])(?: body_hex=([0-9a-f]{2,1024}))?\n")
MIXED = re.compile(rb"  # diagnostic: execution-log record seq=" + NUMBER
                   + rb" ns=" + NUMBER + rb" cpu=" + NUMBER + rb" task=" + NUMBER
                   + rb" source=(timer|work) slot=" + NUMBER + rb" round=" + NUMBER + rb"\n")
PRESSURE = re.compile(rb"  # diagnostic: execution-backpressure record index=" + NUMBER
                      + rb" seq=" + NUMBER + rb" level=(INFO) ns=" + NUMBER
                      + rb" cpu=" + NUMBER + rb" task=" + NUMBER
                      + rb" truncated=([01]) lost=" + NUMBER + rb"\n")
FILL = re.compile(rb"  # diagnostic: execution-backpressure fill-bytes=" + NUMBER + rb"\n")


def record_line(row):
    """由独立环快照字段和固定业务正文生成生产 printer 的完整线字节。"""
    task = str(row["task"]) if row["task"] else "none"
    header = (f"KLOG seq={row['seq']} level={row['level']} ns={row['ns']} "
              f"cpu={row['cpu']} task={task} truncated={row['truncated']} "
              f"lost={row['lost']} ").encode("ascii")
    return header + row["body"] + b"\n"


class ExecutionUART:
    """仅承接三个具体日志场景；每个 case 独立保存完整正文和真实元数据。"""

    def __init__(self, cpus, directory):
        """固定本 VM 的真实核数和证据目录，不读取源码建立目录。"""
        self.cpus = cpus
        self.directory = directory
        self.kind = None
        self.case = None
        self.body = False
        self.lines = []
        self.records = {}
        self.blank_bytes = 0
        self.fill_bytes = None
        self.captured = False

    def begin(self, case):
        """具体 fixture 绑定实际自动目录全名，不参与通用注册或选择。"""
        kinds = {
            "os::ktest::execution_services::klog::records::allocation_free_record_identity": "records",
            "os::ktest::execution_services::klog::uart::socket_printer_backpressure": "backpressure",
            "os::ktest::execution_services::mixed::cases::timer_work_task_log_convergence": "mixed",
        }
        self.kind = kinds.get(case["name"])
        self.case = case["case"]
        self.body = False
        self.lines = []
        self.records = {}
        self.blank_bytes = 0
        self.fill_bytes = None
        self.captured = False

    def observe(self, raw):
        """保留原换行字节；业务元数据绝不作为 KTAP 控制事件。"""
        if self.kind is None or not self.body:
            return
        limit = 5 if self.kind == "records" else (65 if self.kind == "backpressure" else 16 * self.cpus)
        if raw.startswith(b"KLOG "):
            require(self.kind != "backpressure", "backpressure bytes outside its opaque fixture")
            require(len(self.lines) < limit and len(raw) <= 769, "unexpected execution log record")
            self.lines.append(raw)
        pattern = RECORD if self.kind == "records" else (PRESSURE if self.kind == "backpressure" else MIXED)
        match = pattern.fullmatch(raw)
        if match:
            if self.kind == "mixed":
                seq, ns, cpu, task, source, slot, round_number = match.groups()
                slot, round_number = int(slot), int(round_number)
                require(slot < self.cpus and round_number < 8, "mixed log producer outside fixture")
                key = (slot, round_number, source)
                row = dict(seq=int(seq), ns=int(ns), cpu=int(cpu), task=int(task), level="INFO",
                           truncated=0, lost=0,
                           body=f"X28 {source.decode('ascii')} slot={slot} round={round_number}".encode("ascii"))
                require(row["cpu"] == slot and (source != b"work" or row["task"] != 0),
                        "mixed log producer identity mismatch")
            else:
                values = match.groups()
                key, seq, level, ns, cpu, task, truncated = values[:7]
                key = int(key)
                require(key < limit, "execution log index outside fixture")
                row = dict(seq=int(seq), ns=int(ns), cpu=int(cpu), task=int(task),
                           level=level.decode("ascii"), truncated=int(truncated), lost=0)
                if self.kind == "records":
                    bodies = (b"X25 ordinary value=37", "界".encode("utf-8") * 170,
                              None, None, b"X25 precise timer")
                    require(row["level"] == ("INFO", "WARN", "DEBUG", "DEBUG", "INFO")[key]
                            and row["truncated"] == int(key == 1), "record metadata mismatch")
                    body_hex = values[7]
                    if key in (2, 3):
                        require(body_hex is not None and len(body_hex) % 2 == 0,
                                "locked record requires complete independent ring body")
                        row["body"] = bytes.fromhex(body_hex.decode("ascii"))
                        row["body"].decode("utf-8", errors="strict")
                    else:
                        require(body_hex is None, "unexpected dynamic business body")
                        row["body"] = bodies[key]
                else:
                    row["lost"] = int(values[7])
                    require(row["truncated"] == 0 and row["lost"] == (8 if key == 1 else 0),
                            "backpressure lost/truncated metadata mismatch")
                    row["body"] = b"X26 partial " + b"P" * 450 if key == 0 else f"X26 record={key + 7}".encode("ascii")
            require(0 < row["seq"] < 1 << 64 and 0 <= row["ns"] < 1 << 64
                    and 0 <= row["task"] < 1 << 64 and row["cpu"] < self.cpus,
                    "execution log metadata outside native bounds")
            require(key not in self.records, "duplicate execution log metadata")
            self.records[key] = row
        elif raw.startswith((b"  # diagnostic: execution-record record ",
                             b"  # diagnostic: execution-log record ",
                             b"  # diagnostic: execution-backpressure record ")):
            require(False, "malformed or wrong execution fixture metadata")
        fill = FILL.fullmatch(raw)
        if fill:
            require(self.kind == "backpressure" and self.fill_bytes is None,
                    "duplicate or misplaced UART fill evidence")
            self.fill_bytes = int(fill[1])

    def capture(self, vm, declaration, deadline):
        """H 后保原首次拒写和停点；以固定65条业务记录结束，不搜索文本 sentinel。"""
        require(self.kind == "backpressure" and not self.captured and self.body,
                "opaque UART capture outside backpressure fixture")
        require(declaration["direction"] == "pause" and declaration["length"] == 0
                and declaration["digest"] == "00000000" and declaration["delay_ms"] >= 3000,
                "invalid backpressure hold barrier")
        require(declaration["case"] == self.case, "backpressure case mismatch")
        acknowledgement = f"KTEST IO OK {self.case}\n".encode("ascii")
        require(len(acknowledgement) <= 16, "backpressure ACK exceeds FIFO bound")
        vm.send(b"H", deadline)
        vm.delay(declaration["delay_ms"] / 1000, deadline)
        with (self.directory / f"execution-uart-{self.case}.raw").open("xb") as output:
            while len(self.lines) < 65:
                vm.check(deadline)
                newline = vm.buffer.find(b"\n")
                if newline < 0:
                    require(len(vm.buffer) <= 769, "opaque execution record exceeds native bound")
                    vm.delay(0.005, deadline)
                    vm.buffer.extend(vm.receive(deadline, 64))
                    continue
                require(newline < 769, "opaque execution record exceeds native bound")
                raw = bytes(vm.buffer[:newline + 1])
                del vm.buffer[:newline + 1]
                output.write(raw)
                if not self.lines and raw == b"\n":
                    self.blank_bytes += 1
                    require(self.blank_bytes < 1024 * 1024, "UART fill reached its failure bound")
                else:
                    self.lines.append(raw)
        self.captured = True
        vm.send(acknowledgement, deadline)

    def finish(self, status):
        """客体 Pass 还须完整外部正文、所有生产者及原填充字节一致。"""
        if self.kind is None:
            return
        self.body = False
        if status != "Pass":
            return
        count = 5 if self.kind == "records" else (65 if self.kind == "backpressure" else 16 * self.cpus)
        require(len(self.lines) == count and len(self.records) == count,
                "execution log records or metadata missing")
        if self.kind == "mixed":
            keys = {(cpu, round_number, source) for cpu in range(self.cpus)
                    for round_number in range(8) for source in (b"timer", b"work")}
            require(self.records.keys() == keys, "mixed log producer/round gap")
            rows = sorted(self.records.values(), key=lambda row: row["seq"])
        else:
            require(self.records.keys() == set(range(count)), "execution log index gap")
            rows = [self.records[index] for index in range(count)]
        first = rows[0]["seq"]
        for index, row in enumerate(rows):
            offset = index + 8 if self.kind == "backpressure" and index else index
            require(row["seq"] == first + offset, "execution log sequence gap or reordering")
        if self.kind == "backpressure":
            require(self.captured and self.fill_bytes == self.blank_bytes and 0 < self.blank_bytes < 1024 * 1024,
                    "UART fill does not match its actual opaque prefix")
        expected = b"".join(record_line(row) for row in rows)
        (self.directory / f"execution-uart-{self.case}.expected").write_bytes(expected)
        require(b"".join(self.lines) == expected, "execution UART full payload mismatch")
