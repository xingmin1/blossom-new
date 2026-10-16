"""宿主回归共用的目录、协议输入和最小 VM 控制状态。"""

import importlib.util
from pathlib import Path
import sys
from types import SimpleNamespace

TOOLS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(TOOLS))
from ktest.catalog import Catalog, Invocation, case_seed
from ktest.protocol import KTAP
from ktest.runner import VM

SPEC = importlib.util.spec_from_file_location("ktest_cli", TOOLS / "ktest.py")
CLI = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(CLI)
CONFIG = dict(arch="riscv64", cpus=2, mode="debug", panic="unwind")


def suite_record(sid=0):
    """构造协议允许的 suite 目录记录。"""
    return dict(type="suite", suite=sid, name=f"os::tests{sid}", context="Thread",
                isolation="Suite", tags=[], min_cpus=1, timeout_ms=10000, resources=[])


def case_record(cid=0, sid=0, **overrides):
    """构造带完整参数身份的真实目录形状。"""
    row = dict(type="case", suite=sid, case=cid, name=f"os::tests{sid}::case{cid}[one]",
               parameter="one", source="os/tests.rs", line=10, context="Thread", isolation="Suite",
               tags=[], ignore=None, min_cpus=1, timeout_ms=10000, resources=[], expected_fatal=None,
               fatal_message=None)
    row.update(overrides)
    return row


def catalog_of(cases):
    """只通过 JSONL 接收接口构造目录，避免测试绕开目录边界。"""
    catalog = Catalog(CONFIG)
    catalog.feed(dict(type="catalog", version=1, **CONFIG))
    for sid in sorted({row["suite"] for row in cases}):
        catalog.feed(suite_record(sid))
        for row in cases:
            if row["suite"] == sid:
                catalog.feed(row)
    catalog.feed(dict(type="catalog_end", suites=len(catalog.suites), cases=len(cases)))
    return catalog


def options(**overrides):
    """为纯行为回归提供明确选择和挂载模式，没有隐式机器资源。"""
    args = SimpleNamespace(exact=[], include=[], parameter=[], tag=[], exclude=[], include_ignored=False,
                           shard=None, repeat=1, shuffle=False, seed=17, isolation="suite", jobs=2,
                           arch="riscv64", cpus=2, mode="debug", panic="unwind", disks={}, read_only=[],
                           uart_payload=None, uart_expect=None, scratch_bytes=4096, logical_block_size=512,
                           ap_freeze=False, gdb=None,
                           iommu_platform=False, modern=False, root=None, root_mode="ro", extra_root=None,
                           startup_timeout=10, setup_timeout=5, case_timeout=10, cleanup_timeout=5)
    for key, value in overrides.items():
        setattr(args, key, value)
    return args


def vm_fixture():
    """为真实 socket 行为检查准备 VM 的控制状态，不创建模拟 guest。"""
    vm = VM.__new__(VM)
    vm.rx_offset = 0
    vm.rx_used = False
    vm.tx_offset = 0
    vm.tx_used = False
    vm.debugger = None
    vm.gdb_command = None
    return vm


def parser_for(cases):
    """初始化一组实际期待项的 KTAP 状态。"""
    items = [Invocation(row, 0, case_seed(17, 0, row["name"]), i) for i, row in enumerate(cases)]
    parser = KTAP(suite_record(cases[0]["suite"]), items)
    for line in ("KTAP version 1", "1..1", f"  # Subtest: os::tests{cases[0]['suite']}",
                 f"  1..{len(cases)}", "  # KTEST suite-begin setup"):
        parser.feed(line)
    return parser


def case_lines(case, position=1, status="Pass"):
    """输出经历完整清理后的最终记录。"""
    cid = case["case"]
    result = "ok" if status in ("Pass", "Skip") else "not ok"
    name = case["name"] + (" # SKIP insufficient CPUs" if status == "Skip" else "")
    return [f"  # KTEST case-begin {cid}", f"  # KTEST phase {cid} setup",
            f"  # KTEST phase {cid} body", f"  # KTEST phase {cid} cleanup",
            f"  # KTEST status {cid} {status}", f"  {result} {position} {name}"]
