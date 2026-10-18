"""宿主回归共用的目录、协议输入和最小会话／UART 状态。"""

import importlib.util
import io
from pathlib import Path
import sys
from types import SimpleNamespace

TOOLS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(TOOLS))
from ktest.catalog import Catalog, Invocation, case_seed
from ktest.execution_uart import ExecutionUART
from ktest.protocol import KtapParser
from ktest.qemu_session import QemuTestSession
from ktest.uart_channel import UartChannel

SPEC = importlib.util.spec_from_file_location("ktest_cli", TOOLS / "run_kernel_tests.py")
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


def session_fixture():
    """为真实 socket 与子进程回归准备组合状态，不启动或模拟 guest。"""
    session = QemuTestSession.__new__(QemuTestSession)
    session.debugger = None
    session.gdb_command = None
    session.net_user = None
    uart = UartChannel.__new__(UartChannel)
    uart.args = options()
    uart.directory = None
    uart.check = session.check
    uart.serial = None
    uart.raw = io.BytesIO()
    uart.buffer = bytearray()
    uart.rx_offset = 0
    uart.rx_used = False
    uart.tx_offset = 0
    uart.tx_used = False
    uart.execution_uart = ExecutionUART(2, None)
    session.uart = uart
    return session


def parser_for(cases):
    """初始化一组实际期待项的 KTAP 状态。"""
    items = [Invocation(row, 0, case_seed(17, 0, row["name"]), i) for i, row in enumerate(cases)]
    parser = KtapParser(suite_record(cases[0]["suite"]), items)
    for line in ("KTAP version 1", "1..1", f"  # Subtest: os::tests{cases[0]['suite']}",
                 f"  1..{len(cases)}", "  # KTEST suite-begin setup"):
        parser.feed(line)
    return parser


def case_lines(case, position=1, status="Pass"):
    """输出经历完整清理后的最终记录。"""
    cid = case["case"]
    result = "ok" if status in ("Pass", "Skip") else "not ok"
    directive = " # SKIP insufficient CPUs" if status == "Skip" else (" # ERROR" if status == "Error" else "")
    name = case["name"] + directive
    return [f"  # KTEST case-begin {cid}", f"  # KTEST phase {cid} setup",
            f"  # KTEST phase {cid} body", f"  # KTEST phase {cid} cleanup",
            f"  {result} {position} {name}"]
