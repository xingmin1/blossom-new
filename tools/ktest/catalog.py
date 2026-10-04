"""读取真实 ELF 输出的目录，并实现与调度无关的选择和种子。"""

from dataclasses import dataclass
import fnmatch
import hashlib
import random


class HostError(RuntimeError):
    """表示选择、资源或协议基础设施错误。"""


def require(condition, message):
    """在外部协议边界拒绝不满足合同的数据。"""
    if not condition:
        raise HostError(message)


def uint(value, bits, label):
    """拒绝布尔值、负数和超出协议位宽的编号。"""
    require(type(value) is int and 0 <= value < 1 << bits, f"invalid {label}")
    return value


def text(value, label):
    """名字按 UTF-8 字节身份处理，控制字符不能进入按行协议。"""
    require(isinstance(value, str) and value and not any(ord(c) < 32 or ord(c) == 127 for c in value),
            f"invalid {label}")
    return value


def metadata(row):
    """验证已由 guest 解析默认值的静态属性。"""
    require(row.get("context") in ("Boot", "Thread"), "invalid context")
    require(row.get("isolation") in ("Suite", "Case"), "invalid isolation")
    for field in ("tags", "resources"):
        values = row.get(field)
        require(isinstance(values, list), f"missing {field}")
        for value in values:
            text(value, field)
        require(len(set(values)) == len(values), f"duplicate {field}")
    uint(row.get("min_cpus"), 32, "min_cpus")
    require(row["min_cpus"] > 0, "zero min_cpus")
    uint(row.get("timeout_ms"), 64, "timeout_ms")
    require(row["timeout_ms"] > 0, "zero timeout_ms")


class Catalog:
    """一次固定 ELF 的目录；局部编号不允许跨目录复用。"""

    def __init__(self, config):
        """保存本次启动要求，按流接收目录而不运行 fixture。"""
        self.config = config
        self.header = None
        self.suites = {}
        self.cases = []
        self.names = set()
        self.ids = set()
        self.done = False

    def feed(self, row):
        """检查 JSONL 记录的身份、数量、顺序与重复。"""
        require(isinstance(row, dict) and not self.done, "record after catalog_end")
        kind = row.get("type")
        if self.header is None:
            require(kind == "catalog" and row.get("version") == 1, "missing catalog header")
            uint(row.get("version"), 32, "catalog version")
            uint(row.get("cpus"), 32, "catalog cpus")
            require(all(row.get(k) == v for k, v in self.config.items()), "catalog configuration mismatch")
            self.header = row
            return
        if kind == "suite":
            sid = uint(row.get("suite"), 32, "suite")
            name = text(row.get("name"), "suite name")
            metadata(row)
            require(sid not in self.suites and name not in self.names, "duplicate suite identity")
            self.names.add(name)
            self.suites[sid] = row
        elif kind == "case":
            sid = uint(row.get("suite"), 32, "suite")
            cid = uint(row.get("case"), 32, "case")
            name = text(row.get("name"), "case name")
            require(sid in self.suites, "case before its suite")
            metadata(row)
            require((sid, cid) not in self.ids and name not in self.names, "duplicate case identity")
            parameter = row.get("parameter")
            require(parameter is None or isinstance(parameter, str), "invalid parameter")
            if parameter is not None:
                text(parameter, "parameter")
            text(row.get("source"), "source")
            uint(row.get("line"), 32, "source line")
            for field in ("ignore", "expected_fatal", "fatal_message"):
                require(field in row and (row[field] is None or isinstance(row[field], str)), f"invalid {field}")
                if row[field] is not None:
                    text(row[field], field)
            fatal = row["expected_fatal"]
            require(fatal is None or fatal in ("illegal", "refill", "irq-schedule", "preempt-schedule",
                                             "oom", "timekeeper", "panic", "double-panic", "abi"),
                    "unknown expected fatal type")
            require(fatal != "panic" or row["fatal_message"] is not None, "panic fatal requires fatal_message")
            require(fatal is not None or row["fatal_message"] is None, "fatal_message without expected_fatal")
            require(row["context"] == self.suites[sid]["context"], "case context differs from suite")
            require(self.suites[sid]["isolation"] != "Case" or row["isolation"] == "Case",
                    "case weakens suite isolation")
            self.ids.add((sid, cid))
            self.names.add(name)
            self.cases.append(row)
        elif kind == "catalog_end":
            uint(row.get("suites"), 32, "suite count")
            uint(row.get("cases"), 32, "case count")
            require(row.get("suites") == len(self.suites) and row.get("cases") == len(self.cases),
                    "catalog count mismatch")
            self.done = True
        else:
            raise HostError(f"unexpected catalog record: {kind}")


def stable_hash(value):
    """取 SHA256 前八字节的大端值，避免 Python 随机 hash。"""
    return int.from_bytes(hashlib.sha256(value.encode("utf-8")).digest()[:8], "big")


def case_seed(seed, repeat, name):
    """每例种子只依赖总种子、轮次和完整名字。"""
    return stable_hash(f"{seed}:{repeat}:{name}")


def select(catalog, args):
    """正向并集、标签交集、排除与 ignore 后稳定分片，结果按完整名排序。"""
    selected = []
    for case in catalog.cases:
        name = case["name"]
        positive = not (args.exact or args.include or args.parameter)
        positive |= name in args.exact
        positive |= any(fnmatch.fnmatchcase(name, p) for p in args.include)
        positive |= case["parameter"] in args.parameter
        if not positive or not set(args.tag).issubset(case["tags"]):
            continue
        if any(fnmatch.fnmatchcase(name, p) for p in args.exclude):
            continue
        if case["ignore"] is not None and not args.include_ignored:
            continue
        if args.shard is not None:
            index, count = args.shard
            if stable_hash(name) % count != index:
                continue
        selected.append(case)
    require(selected, "selection matched no cases")
    selected.sort(key=lambda case: case["name"])
    return selected


@dataclass(frozen=True)
class Invocation:
    """一个参数实例的一次重复执行，保留实际顺序与稳定种子。"""
    case: dict
    repeat: int
    seed: int
    order: int


def invocations(cases, args):
    """每轮固定 shuffle 顺序，jobs 不参与随机输入。"""
    result = []
    for repeat in range(args.repeat):
        ordered = list(cases)
        if args.shuffle:
            random.Random(case_seed(args.seed, repeat, "shuffle")).shuffle(ordered)
        for case in ordered:
            result.append(Invocation(case, repeat, case_seed(args.seed, repeat, case["name"]), len(result)))
    return result


def batches(items, args):
    """只合并相邻同 suite 项，满足一千字节和六十四项 UART 上限。"""
    result = []
    for item in items:
        isolated = args.isolation == "case" or item.case["isolation"] == "Case" or item.case["expected_fatal"] is not None
        if result and not isolated:
            previous = result[-1]
            first = previous[0]
            same = (first.repeat == item.repeat and first.case["suite"] == item.case["suite"]
                    and first.case["isolation"] == "Suite" and first.case["expected_fatal"] is None)
            candidate = previous + [item]
            if same and len(candidate) <= 64 and len(run_command(candidate, args.seed)) <= 1024:
                previous.append(item)
                continue
        result.append([item])
    return result


def run_command(items, seed):
    """唯一早期命令只传本 ELF 的 suite/case 编号与总种子。"""
    first = items[0]
    command = f"run {first.case['suite']} {seed} {first.repeat} "
    command += ",".join(str(item.case["case"]) for item in items)
    encoded = (command + "\n").encode("ascii")
    require(len(encoded) <= 1024 and len(items) <= 64, "run command exceeds UART limit")
    return encoded
