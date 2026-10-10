"""按完整身份和生命周期检查 KTAP，不把任意业务日志当作成功。"""

import json
import re
from .catalog import HostError, require

READY = re.compile(r"KTEST READY 1 arch=(riscv64|loongarch64) cpus=(\d+) mode=(debug|release) panic=(unwind|abort)")
RESULT = re.compile(r"(  )?(not ok|ok) (\d+) (.+)")
CONTROL = re.compile(r"  # KTEST (case-begin|phase|status|armed|fatal|io|case-abort) (\d+)(?: (.*))?")
SUITE_CONTROL = re.compile(r"  # KTEST (suite-begin|suite-error) (setup|execution|cleanup)(?: ([^\x00-\x1f\x7f]+))?")
PHASES = ("setup", "body", "quiesce", "join", "cleanup")
FATAL_KINDS = ("illegal", "refill", "irq-schedule", "preempt-schedule", "oom",
               "timekeeper", "panic", "double-panic", "abi")


def ready(line, config):
    """就绪必须与实际请求 CPU 和镜像策略逐字段相等。"""
    match = READY.fullmatch(line)
    require(match is not None, "malformed READY")
    arch, cpus, mode, panic = match.groups()
    require(dict(arch=arch, cpus=int(cpus), mode=mode, panic=panic) == config,
            "READY configuration mismatch")


class KTAP:
    """一次 VM 仅一个 suite，结果发布晚于本例清理。"""

    def __init__(self, suite, items):
        """按命令实际顺序记录期待列表，结果不能覆盖首次结果。"""
        self.suite = suite
        self.items = items
        self.results = {}
        self.started = set()
        self.active = None
        self.phase = None
        self.status = None
        self.armed = None
        self.fatal = None
        self.fatal_message_seen = False
        self.version = False
        self.outer_plan = False
        self.inner_plan = False
        self.inner_version = False
        self.subtest = False
        self.suite_started = False
        self.suite_error = None
        self.case_abort = None
        self.outer_result = False
        self.complete = False

    def feed(self, line):
        """只接受固定缩进的控制记录；返回阶段或 raw UART 声明。"""
        if line == "KTAP version 1":
            require(not self.version and not self.outer_plan, "duplicate KTAP version")
            self.version = True
            return None
        if line == "1..1":
            require(self.version and not self.outer_plan, "invalid outer plan")
            self.outer_plan = True
            return None
        if line == f"  # Subtest: {self.suite['name']}":
            require(self.outer_plan and not self.subtest, "invalid subtest")
            self.subtest = True
            return None
        if line == "  KTAP version 1":
            require(self.subtest and not self.inner_plan and not self.inner_version, "invalid inner version")
            self.inner_version = True
            return None
        if line.startswith("  1.."):
            require(self.subtest and not self.inner_plan and line == f"  1..{len(self.items)}",
                    "invalid inner plan")
            self.inner_plan = True
            return None
        if line == "# KTEST complete":
            require(self.outer_result and not self.complete, "premature or duplicate complete")
            self.complete = True
            return "complete"
        suite_control = SUITE_CONTROL.fullmatch(line)
        if suite_control:
            require(self.inner_plan and not self.outer_result and not self.complete
                    and self.case_abort is None, "suite control outside suite")
            kind, phase, reason = suite_control.groups()
            if kind == "suite-begin":
                require(phase == "setup" and reason is None and not self.suite_started
                        and not self.started and self.suite_error is None, "invalid suite-begin")
                self.suite_started = True
                return "suite-setup"
            require(self.suite_started and self.suite_error is None and reason is not None
                    and reason.strip() and self.active is None, "invalid suite-error")
            require(phase != "setup" or not self.started, "suite setup error after case-begin")
            require(phase != "cleanup" or len(self.results) == len(self.items),
                    "suite cleanup error before case results")
            self.suite_error = dict(phase=phase, reason=reason)
            return "suite-error"
        control = CONTROL.fullmatch(line)
        if control:
            require(not self.complete and not self.outer_result and self.inner_plan
                    and self.suite_started and self.suite_error is None
                    and self.case_abort is None, "control outside suite")
            require(self.fatal is None, "control after actual fatal")
            kind, cid, value = control.groups()
            cid = int(cid)
            if kind == "case-begin":
                require(value is None and self.active is None and not self.outer_result, "overlapping case")
                position = len(self.results)
                require(position < len(self.items) and self.items[position].case["case"] == cid,
                        "case-begin identity/order mismatch")
                require(cid not in self.started, "duplicate case-begin")
                self.started.add(cid)
                self.active = cid
                self.phase = None
                self.status = None
                self.armed = None
                self.fatal = None
                self.fatal_message_seen = False
                return "begin"
            require(self.active == cid, "control identity mismatch")
            require(self.status is None, "control after final status")
            if kind == "case-abort":
                phase, separator, payload = (value or "").partition(" ")
                require(separator and phase in ("quiesce", "join", "cleanup")
                        and phase == self.phase, "case-abort outside actual cleanup phase")
                # 保留重复键，不能让 JSON 的末值覆盖首个独立失败账本。
                try:
                    pairs = json.loads(payload, object_pairs_hook=list)
                except json.JSONDecodeError as error:
                    raise HostError("invalid case-abort JSON") from error
                keys = ("primary", "child", "cleanup", "expectations", "reason")
                require(isinstance(pairs, list) and len(pairs) == len(keys)
                        and all(isinstance(pair, tuple) and len(pair) == 2 for pair in pairs),
                        "case-abort must be a JSON object")
                require({key for key, _ in pairs} == set(keys), "invalid case-abort fields")
                ledger = dict(pairs)
                require(all(ledger[key] is None or isinstance(ledger[key], str)
                            for key in ("primary", "child", "cleanup"))
                        and type(ledger["expectations"]) is int and ledger["expectations"] >= 0
                        and isinstance(ledger["reason"], str), "invalid case-abort field types")
                self.case_abort = dict(phase=phase, **ledger)
                return "case-abort"
            if kind == "phase":
                require(value in PHASES, "unknown case phase")
                require((self.phase is None and value == "setup") or
                        (self.phase is not None and PHASES.index(self.phase) < PHASES.index(value)),
                        "case phase regression or missing setup")
                self.phase = value
                if value != "body":
                    self.armed = None
                return value
            if kind == "status":
                require(value in ("Pass", "Fail", "Skip", "Error"), "unknown final status")
                require(self.phase == "cleanup" or (self.phase == "setup" and value != "Pass"),
                        "final status before cleanup")
                case = self.items[len(self.results)].case
                require(value != "Pass" or case["expected_fatal"] is None,
                        "expected fatal requires actual fault and VM termination")
                self.status = value
                return "status"
            if kind == "armed":
                case = self.items[len(self.results)].case
                require(self.phase == "body" and self.armed is None
                        and case["expected_fatal"] is not None and value == case["expected_fatal"],
                        "unexpected fatal arming")
                self.armed = value
                return "armed"
            if kind == "fatal":
                require(self.phase in PHASES and value in FATAL_KINDS,
                        "unknown or out-of-phase actual fatal")
                self.fatal = value
                self.fatal_message_seen = False
                return "fatal"
            require(kind == "io" and self.phase in ("setup", "body", "quiesce", "cleanup"),
                    "raw UART outside fixture phase")
            parts = (value or "").split()
            if parts and parts[0] == "duplex":
                require(len(parts) == 7 and parts[1].isdigit()
                        and re.fullmatch(r"[0-9a-f]{8}", parts[2]) and parts[3].isdigit()
                        and parts[4].isdigit() and re.fullmatch(r"[0-9a-f]{8}", parts[5])
                        and parts[6].isdigit(), "invalid UART duplex declaration")
                _, rx_length, rx_digest, rx_delay, tx_length, tx_digest, tx_delay = parts
                rx_length, rx_delay, tx_length, tx_delay = map(
                    int, (rx_length, rx_delay, tx_length, tx_delay))
                require(rx_length + tx_length <= 64 * 1024 * 1024 and rx_delay <= 60000
                        and tx_delay <= 60000 and (rx_length or tx_length)
                        and (rx_length or rx_digest == "00000000")
                        and (tx_length or tx_digest == "00000000")
                        and (rx_length or rx_delay == 0)
                        and (tx_length or tx_delay == 0),
                        "UART duplex declaration exceeds bounds")
                return dict(direction="duplex", rx_length=rx_length, rx_digest=rx_digest,
                            rx_delay_ms=rx_delay, tx_length=tx_length, tx_digest=tx_digest,
                            tx_delay_ms=tx_delay, case=cid)
            require(len(parts) == 4 and parts[0] in ("tx", "rx", "pause", "slow")
                    and parts[1].isdigit() and re.fullmatch(r"[0-9a-f]{8}", parts[2])
                    and parts[3].isdigit(), "invalid UART declaration")
            direction, length, digest, delay = parts
            length, delay = int(length), int(delay)
            require(0 <= length <= 64 * 1024 * 1024 and delay <= 60000
                    and (length != 0 or (direction == "pause" and digest == "00000000")),
                    "UART declaration exceeds bounds")
            return dict(direction=direction, length=length, digest=digest, delay_ms=delay,
                        case=cid)
        result = RESULT.fullmatch(line)
        if result:
            indent, polarity, number, name = result.groups()
            number = int(number)
            if indent:
                require(self.case_abort is None, "ordinary result after case-abort")
                position = len(self.results)
                require(self.active is not None and self.status is not None
                        and number == position + 1 and position < len(self.items),
                        "missing, duplicate or out-of-order result")
                expected = self.items[position].case["name"]
                if self.status == "Skip":
                    require(name.startswith(expected + " # SKIP ") and polarity == "ok",
                            "skip identity/directive mismatch")
                    reason = name[len(expected + " # SKIP "):]
                else:
                    require(name == expected, "case result identity mismatch")
                    require(polarity == ("ok" if self.status == "Pass" else "not ok"),
                            "case result contradicts final status")
                    reason = ""
                self.results[self.active] = dict(status=self.status, reason=reason)
                self.active = None
                self.phase = None
                self.status = None
                return "result"
            require(self.inner_plan and self.suite_started
                    and (self.active is None or self.case_abort is not None)
                    and (len(self.results) == len(self.items) or self.suite_error is not None
                         or self.case_abort is not None)
                    and not self.outer_result and number == 1 and name == self.suite["name"],
                    "invalid outer result")
            passed = self.suite_error is None and self.case_abort is None and all(
                r["status"] in ("Pass", "Skip") for r in self.results.values())
            require(polarity == ("ok" if passed else "not ok"), "false suite PASS")
            self.outer_result = True
            return "outer"
        if line.lstrip().startswith(("ok ", "not ok ", "KTAP version", "KTEST READY", "1..", "# KTEST", "# Subtest:")):
            raise HostError(f"malformed or unexpected KTAP record: {line[:160]}")
        # 业务诊断不决定结果；raw 字节由 runner 独占处理，永远不进入此函数。
        if self.active is not None and self.fatal is not None:
            message = self.items[len(self.results)].case["fatal_message"]
            if message is not None and message in line:
                self.fatal_message_seen = True
        return None
