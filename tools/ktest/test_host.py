"""宿主正式回归；运行方式为 python3 -m unittest discover -s tools/ktest。"""

import copy
import errno
import importlib.util
import hashlib
import io
import json
from pathlib import Path
import select as select_io
import socket
import struct
import subprocess
import sys
import tempfile
import time
from types import SimpleNamespace
import unittest
from unittest.mock import patch
import xml.etree.ElementTree as ET

TOOLS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(TOOLS))
from ktest.boot import elf_layout, verify_trap
from ktest.catalog import (Catalog, HostError, Invocation, batches, case_seed, invocations,
                           run_command, select, stable_hash)
from ktest.protocol import KTAP, ready
from ktest.execution_uart import ExecutionUART
from ktest.resources import (aliases, crc32c_update, prepare_disks,
                             resource_names, validate_resources)
from ktest.runner import DeadlineExpired, VM, VMExited, execute_batch, finish_vm

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


class RootModeInputTests(unittest.TestCase):
    """真实宿主入口在资源准备前拒绝非法根挂载模式。"""

    def test_invalid_mode_is_rejected_without_replacing_work_disks(self):
        """命令行和 Make 拒绝未知或多 token 值，已有恢复盘与源文件必须保留。"""
        with tempfile.TemporaryDirectory() as temporary:
            base = Path(temporary)
            source = base / "source.img"
            source.write_bytes(b"original source")
            work = base / "work"
            work.mkdir()
            disk = work / "root.raw"
            disk.write_bytes(b"preserved working disk")
            commands = [
                [sys.executable, str(TOOLS / "run.py"), "--arch", "riscv64",
                 "--kernel", str(base / "missing.elf"), "--image", str(source),
                 "--work-dir", str(work)],
                [sys.executable, str(TOOLS / "ktest.py"), "list", "--arch", "riscv64",
                 "--report-dir", str(base / "report")],
                [sys.executable, str(TOOLS / "ktest.py"), "run", "--arch", "riscv64",
                 "--report-dir", str(base / "report")],
            ]
            for mode in ("", "RO", "rwx", "ro rw", "ro=foo", "rw=foo", "%"):
                for command in commands:
                    with self.subTest(command=command, mode=mode):
                        result = subprocess.run(command + ["--root-mode", mode],
                                                capture_output=True, text=True)
                        self.assertEqual(result.returncode, 2)
                with self.subTest(command="make", mode=mode):
                    result = subprocess.run(["make", "--no-print-directory", "-n", "run",
                                             f"ROOT_MODE={mode}"], cwd=TOOLS.parent,
                                            capture_output=True, text=True)
                    self.assertNotEqual(result.returncode, 0)
                self.assertEqual(source.read_bytes(), b"original source")
                self.assertEqual(disk.read_bytes(), b"preserved working disk")
                self.assertFalse((base / "report").exists())


class BootELFTests(unittest.TestCase):
    """实际装载格式的边界检查；合成ELF不充当客体执行或链接证明。"""

    def test_load_contract_rejects_corruption_and_allows_empty_optional_header(self):
        """两架构拒错入口/映射/权限/表界，合法零长eh_frame_hdr不应阻断发现。"""
        for arch in ("riscv64", "loongarch64"):
            with self.subTest(arch=arch), tempfile.TemporaryDirectory() as temporary:
                rv = arch == "riscv64"
                entry = 0x80200000 if rv else 0x9000000090000000
                text = 0xFFFFFFFF80200000 if rv else entry
                physical = 0x80400000 if rv else 0x90000000
                first = 0x2000 if rv else 0x1000
                loads = ([(5, 0x1000, entry, entry, 32, 32)] if rv else []) + [
                    (5, first, text, physical, 32, 32),
                    (4, first + 0x1000, text + 0x1000, physical + 0x1000, 32, 32),
                    (6, first + 0x2000, text + 0x2000, physical + 0x2000, 32, 128)]
                sections = [
                    (b".text", 1, 6, text, first, 32),
                    (b".rodata", 1, 2, text + 0x1000, first + 0x1000, 16),
                    (b".eh_frame_hdr", 1, 2, text + 0x1010, first + 0x1010, 0),
                    (b".data", 1, 3, text + 0x2000, first + 0x2000, 32),
                    (b".boot", 1 if rv else 8, 6 if rv else 3,
                     entry if rv else text + 0x2020, 0x1000 if rv else first + 0x2020, 32),
                    (b".bss", 8, 3, text + 0x2040, first + 0x2040, 32)]
                names = b"\0" + b"\0".join(section[0] for section in sections) + b"\0.shstrtab\0"
                image = bytearray(0x5800)
                struct.pack_into("<16sHHIQQQIHHHHHH", image, 0, b"\x7fELF\x02\x01\x01", 2,
                                 243 if rv else 258, 1, entry, 64, 0x5400, 5 if rv else 0x43,
                                 64, 56, len(loads), 64, len(sections) + 2, len(sections) + 1)
                for index, (flags, offset, address, pa, file_size, memory_size) in enumerate(loads):
                    struct.pack_into("<IIQQQQQQ", image, 64 + index * 56, 1, flags, offset,
                                     address, pa, file_size, memory_size, 0x1000)
                for index, (name, kind, flags, address, offset, length) in enumerate(sections, 1):
                    struct.pack_into("<IIQQQQIIQQ", image, 0x5400 + index * 64,
                                     names.index(name), kind, flags, address, offset, length, 0, 0, 1, 0)
                struct.pack_into("<IIQQQQIIQQ", image, 0x5400 + (len(sections) + 1) * 64,
                                 names.index(b".shstrtab"), 3, 0, 0, 0x5000, len(names), 0, 0, 1, 0)
                image[0x5000:0x5000 + len(names)] = names
                kernel = Path(temporary) / "kernel.elf"
                kernel.write_bytes(image)
                layout = elf_layout(kernel, arch)
                self.assertEqual(layout["entry"], entry)
                self.assertEqual(layout["sections"][".eh_frame_hdr"]["size"], 0)
                self.assertEqual(layout["sections"][".bss"]["flags"], 6)
                corruptions = [
                    ("<H", 16, 3), ("<H", 18, 258 if rv else 243), ("<Q", 24, entry + 4),
                    ("<Q", 40, 1 << 63), ("<I", 64 + 4, 7),
                    ("<Q", 64 + 24, loads[0][3] + 0x1000),
                    ("<Q", 64 + 32, 64), ("<Q", 0x5400 + 64 + 8, 7)]
                for fmt, offset, value in corruptions:
                    with self.subTest(offset=offset, value=value):
                        broken = bytearray(image)
                        struct.pack_into(fmt, broken, offset, value)
                        kernel.write_bytes(broken)
                        with self.assertRaises(HostError):
                            elf_layout(kernel, arch)
                kernel.write_bytes(image[:64])
                with self.assertRaises(HostError):
                    elf_layout(kernel, arch)


class SelectionTests(unittest.TestCase):
    """精确、glob、参数、标签、排除和分片的外部语义。"""

    def test_exact_parameter_is_not_glob_or_prefix(self):
        """方括号必须逐字相等，不误选相似前缀或 glob 字符集。"""
        rows = [case_record(0, name="os::s::x[a]"), case_record(1, name="os::s::x[a]_tail"),
                case_record(2, name="os::s::xa")]
        actual = select(catalog_of(rows), options(exact=["os::s::x[a]"]))
        self.assertEqual([r["name"] for r in actual], ["os::s::x[a]"])

    def test_union_tags_exclude_and_ignore_precedence(self):
        """正向取并集后仍须满足全部标签，并应用最后排除。"""
        rows = [case_record(0, tags=["smp", "mm"]), case_record(1, tags=["smp"]),
                case_record(2, tags=["smp", "mm"], ignore="manual"),
                case_record(3, tags=["smp", "mm"])]
        args = options(exact=[rows[0]["name"]], include=["*case*"], tag=["smp", "mm"], exclude=["*case3*"])
        self.assertEqual(select(catalog_of(rows), args), [rows[0]])
        args.include_ignored = True
        self.assertEqual(select(catalog_of(rows), args), [rows[0], rows[2]])

    def test_registration_order_does_not_change_selection_or_shuffle(self):
        """相同完整名集合先排序；ELF局部ID变化不得改变宿主顺序和每例种子。"""
        names = ["os::s::case[z]", "os::s::case[a]", "os::s::case[m]"]
        catalogs = [
            catalog_of([case_record(index, name=name) for index, name in enumerate(order)])
            for order in (names, list(reversed(names)))
        ]
        expected = ["os::s::case[a]", "os::s::case[m]", "os::s::case[z]"]
        sequences = []
        args = options(shuffle=True, repeat=2)
        for catalog in catalogs:
            selected = select(catalog, args)
            self.assertEqual([row["name"] for row in selected], expected)
            sequences.append([
                (item.case["name"], item.repeat, item.seed)
                for item in invocations(selected, args)
            ])
        self.assertEqual(sequences[0], sequences[1])

    def test_no_match_is_error(self):
        """无匹配不能产生空的成功。"""
        with self.assertRaises(HostError):
            select(catalog_of([case_record()]), options(exact=["missing"]))

    def test_shard_partition_and_fixed_hash(self):
        """所有分片精确分割全名，并使用规定的大端 SHA 前缀。"""
        self.assertEqual(stable_hash("abc"), 0xba7816bf8f01cfea)
        rows = [case_record(i) for i in range(1000)]
        catalog = catalog_of(rows)
        partitions = [select(catalog, options(shard=(i, 7))) for i in range(7)]
        names = [r["name"] for part in partitions for r in part]
        self.assertEqual(set(names), {r["name"] for r in rows})
        self.assertEqual(len(names), len(set(names)))

    def test_jobs_do_not_change_seed_or_shuffle(self):
        """VM 并发数量不改变重复实例的实际顺序和种子。"""
        rows = [case_record(i) for i in range(40)]
        one = invocations(rows, options(jobs=1, repeat=3, shuffle=True))
        many = invocations(rows, options(jobs=8, repeat=3, shuffle=True))
        self.assertEqual(one, many)
        expected = int.from_bytes(hashlib.sha256(b"17:0:os::tests0::case0[one]").digest()[:8], "big")
        self.assertEqual(next(item.seed for item in one if item.case == rows[0] and item.repeat == 0), expected)
        self.assertNotEqual(case_seed(17, 0, rows[0]["name"]), case_seed(17, 1, rows[0]["name"]))

    def test_large_catalog_and_command_batches_preserve_every_instance(self):
        """万例目录只分批，不丢失、不重复，也不越 UART 上限。"""
        rows = [case_record(i) for i in range(10000)]
        catalog = catalog_of(rows)
        items = invocations(select(catalog, options()), options())
        groups = batches(items, options())
        self.assertEqual([item for group in groups for item in group], items)
        for group in groups:
            self.assertLessEqual(len(group), 64)
            self.assertLessEqual(len(run_command(group, 17)), 1024)

    def test_case_isolation_only_strengthens(self):
        """独立和预期 fatal 实例不进入共用 VM。"""
        rows = [case_record(0), case_record(1, isolation="Case"), case_record(2),
                case_record(3, expected_fatal="panic")]
        groups = batches(invocations(rows, options()), options())
        self.assertEqual([len(group) for group in groups], [1, 1, 1, 1])
        self.assertTrue(all(len(group) == 1 for group in batches(invocations(rows, options()), options(isolation="case"))))


class ProtocolTests(unittest.TestCase):
    """结果身份、阶段和最终确认都是必需证据。"""

    def test_catalog_rejects_duplicate_identity_and_counts(self):
        """重复全名或错误尾数量不得构造可运行目录。"""
        with self.assertRaises(HostError):
            catalog_of([case_record(0), case_record(1, name=case_record(0)["name"])])
        catalog = Catalog(CONFIG)
        catalog.feed(dict(type="catalog", version=1, **CONFIG))
        with self.assertRaises(HostError):
            catalog.feed(dict(type="catalog_end", suites=0, cases=1))

    def test_ready_uses_real_cpu_count(self):
        """AP 数不符是启动错误，不能变成适用条件 Skip。"""
        with self.assertRaises(HostError):
            ready("KTEST READY 1 arch=riscv64 cpus=1 mode=debug panic=unwind", CONFIG)

    def test_complete_pass_with_diagnostics(self):
        """业务 panic 字样只是诊断，完整生命周期才决定结果。"""
        row = case_record()
        parser = parser_for([row])
        parser.feed("  # expected input contains panic and PASS")
        for line in case_lines(row):
            parser.feed(line)
        parser.feed("ok 1 os::tests0")
        parser.feed("# KTEST complete")
        self.assertTrue(parser.complete)
        self.assertEqual(parser.results[0]["status"], "Pass")

    def test_early_pass_missing_result_and_fake_suite_pass_rejected(self):
        """body 内 PASS、少结果和假总 PASS 各自是错误。"""
        row = case_record()
        parser = parser_for([row])
        for line in case_lines(row)[:3]:
            parser.feed(line)
        with self.assertRaises(HostError):
            parser.feed("  # KTEST status 0 Pass")
        parser = parser_for([row, case_record(1)])
        for line in case_lines(row):
            parser.feed(line)
        with self.assertRaises(HostError):
            parser.feed("ok 1 os::tests0")
        parser = parser_for([row])
        for line in case_lines(row, status="Fail"):
            parser.feed(line)
        with self.assertRaises(HostError):
            parser.feed("ok 1 os::tests0")

    def test_wrong_name_number_duplicate_and_indent_are_rejected(self):
        """看似合法结果也必须与目录全名、序号和层级一致。"""
        row = case_record()
        for invalid in ("  ok 1 wrong", f"  ok 2 {row['name']}", f"    ok 1 {row['name']}"):
            parser = parser_for([row])
            for line in case_lines(row)[:-1]:
                parser.feed(line)
            with self.assertRaises(HostError):
                parser.feed(invalid)
        parser = parser_for([row])
        for line in case_lines(row):
            parser.feed(line)
        with self.assertRaises(HostError):
            parser.feed(case_lines(row)[-1])

    def test_missing_complete_cannot_be_success(self):
        """完整结果列表不能替代顶层终结确认。"""
        row = case_record()
        parser = parser_for([row])
        for line in case_lines(row):
            parser.feed(line)
        parser.feed("ok 1 os::tests0")
        self.assertFalse(parser.complete)
        with self.assertRaises(HostError):
            parser.feed("# KTEST complete trailing")

    def test_actual_fatal_validates_identity_kind_uniqueness_and_noreturn(self):
        """实际故障与期望分离，但身份、固定类型和 NoReturn 后禁止控制仍严格。"""
        row = case_record(expected_fatal="illegal")
        for invalid in ("  # KTEST fatal 1 illegal", "  # KTEST fatal 0 invented"):
            with self.subTest(invalid=invalid):
                parser = parser_for([row])
                for line in case_lines(row)[:3]:
                    parser.feed(line)
                with self.assertRaises(HostError):
                    parser.feed(invalid)
        for late in ("  # KTEST fatal 0 oom", "  # KTEST phase 0 cleanup",
                     "  # KTEST armed 0 illegal"):
            with self.subTest(late=late):
                parser = parser_for([row])
                for line in case_lines(row)[:3]:
                    parser.feed(line)
                parser.feed("  # KTEST fatal 0 oom")
                self.assertEqual(parser.fatal, "oom")
                with self.assertRaises(HostError):
                    parser.feed(late)

    def test_skip_retains_reason_and_status(self):
        """全部 Skip 与 Pass 保持不同结果。"""
        row = case_record()
        parser = parser_for([row])
        for line in case_lines(row, status="Skip"):
            parser.feed(line)
        parser.feed("ok 1 os::tests0")
        parser.feed("# KTEST complete")
        self.assertEqual(parser.results[0], dict(status="Skip", reason="insufficient CPUs"))

    def test_fatal_message_only_matches_armed_actual_diagnostics(self):
        """arm 前的文本和 raw 声明不能冒充实际 panic 诊断。"""
        row = case_record(expected_fatal="panic", fatal_message="unique actual panic")
        parser = parser_for([row])
        for line in case_lines(row)[:3]:
            parser.feed(line)
        parser.feed("  # unique actual panic")
        self.assertFalse(parser.fatal_message_seen)
        parser.feed("  # KTEST armed 0 panic")
        parser.feed("  # KTEST fatal 0 panic")
        self.assertFalse(parser.fatal_message_seen)
        parser.feed("  # panic: unique actual panic")
        self.assertTrue(parser.fatal_message_seen)

    def test_phase_regression_and_outside_fixture_uart_are_errors(self):
        """阶段不能倒退，UART raw 声明只能出现在真实 fixture 阶段。"""
        row = case_record()
        parser = parser_for([row])
        parser.feed("  # KTEST case-begin 0")
        with self.assertRaises(HostError):
            parser.feed("  # KTEST io 0 tx 9 e3069283 0")
        parser.feed("  # KTEST phase 0 setup")
        parser.feed("  # KTEST phase 0 body")
        with self.assertRaises(HostError):
            parser.feed("  # KTEST phase 0 setup")

    def test_empty_pause_barrier_does_not_allow_empty_transfers(self):
        """零字节停读屏障保CRC零，tx/rx/slow或伪摘要仍不能声明空载荷。"""
        parser = parser_for([case_record()])
        for line in ("  # KTEST case-begin 0", "  # KTEST phase 0 setup", "  # KTEST phase 0 body"):
            parser.feed(line)
        self.assertEqual(parser.feed("  # KTEST io 0 pause 0 00000000 3000"),
                         dict(direction="pause", length=0, digest="00000000", delay_ms=3000, case=0))
        for declaration in ("tx 0 00000000 0", "rx 0 00000000 0", "slow 0 00000000 1",
                            "pause 0 00000001 3000", "pause 0 00000000 60001"):
            with self.subTest(declaration=declaration), self.assertRaises(HostError):
                parser.feed("  # KTEST io 0 " + declaration)

    def test_suite_setup_error_has_no_started_case(self):
        """suite 基础 setup 失败只产生独立错误，不能虚构首例 begin。"""
        parser = parser_for([case_record(0), case_record(1)])
        parser.feed("  # KTEST suite-error setup allocation failed\\nrollback complete")
        parser.feed("not ok 1 os::tests0")
        parser.feed("# KTEST complete")
        self.assertEqual(parser.started, set())
        self.assertEqual(parser.results, {})
        self.assertEqual(parser.suite_error,
                         dict(phase="setup", reason="allocation failed\\nrollback complete"))
        self.assertTrue(parser.complete)

    def test_suite_cleanup_error_preserves_all_case_passes(self):
        """真实用例全部通过也不能把 suite cleanup 错误变成 outer PASS。"""
        rows = [case_record(0), case_record(1)]
        parser = parser_for(rows)
        for position, row in enumerate(rows, 1):
            for line in case_lines(row, position):
                parser.feed(line)
        parser.feed("  # KTEST suite-error cleanup resource release failed")
        with self.assertRaises(HostError):
            parser.feed("ok 1 os::tests0")
        parser.feed("not ok 1 os::tests0")
        parser.feed("# KTEST complete")
        parser.feed("ordinary business diagnostic")
        self.assertEqual([r["status"] for r in parser.results.values()], ["Pass", "Pass"])
        self.assertEqual(parser.suite_error["phase"], "cleanup")
        for late in ("  # KTEST suite-error cleanup duplicate",
                     "  # KTEST suite-begin setup", case_lines(rows[0])[-1],
                     "not ok 1 os::tests0", "# KTEST complete"):
            with self.subTest(late=late), self.assertRaises(HostError):
                parser.feed(late)

    def test_missing_cases_need_explicit_valid_suite_error(self):
        """未开始项只有明确合法 suite-error 才可缺结果，活动项不能缺结果。"""
        rows = [case_record(0), case_record(1)]
        parser = parser_for(rows)
        for line in case_lines(rows[0]):
            parser.feed(line)
        with self.assertRaises(HostError):
            parser.feed("not ok 1 os::tests0")
        for invalid in ("  # KTEST suite-error setup failed",
                        "  # KTEST suite-error cleanup failed"):
            with self.subTest(invalid=invalid), self.assertRaises(HostError):
                parser.feed(invalid)
        parser.feed("  # KTEST suite-error execution dispatcher failed")
        for invalid in ("  # KTEST case-begin 1", "ok 1 os::tests0", "not ok 1 wrong"):
            with self.subTest(invalid=invalid), self.assertRaises(HostError):
                parser.feed(invalid)
        parser.feed("not ok 1 os::tests0")
        parser.feed("# KTEST complete")
        active = parser_for(rows)
        for line in case_lines(rows[0])[:3]:
            active.feed(line)
        with self.assertRaises(HostError):
            active.feed("  # KTEST suite-error execution missing active result")

    def test_case_abort_requires_current_unfinished_identity_and_actual_cleanup_phase(self):
        """中止不是普通 final；只能终结当前确定身份的真实清理阶段。"""
        rows = [case_record(0), case_record(1)]
        ledger = dict(primary="body failed", child=None, cleanup="unsafe to release",
                      expectations=2, reason="cleanup did not converge")
        payload = json.dumps(ledger)
        for phase in ("quiesce", "join", "cleanup"):
            parser = parser_for(rows)
            for line in case_lines(rows[0])[:3]:
                parser.feed(line)
            parser.feed(f"  # KTEST phase 0 {phase}")
            self.assertEqual(parser.feed(f"  # KTEST case-abort 0 {phase} {payload}"), "case-abort")
            self.assertEqual(parser.results, {}, "abort must not pretend normal cleanup finished")
            self.assertEqual(parser.case_abort, dict(phase=phase, **ledger))
            invalid = (f"  # KTEST case-abort 0 {phase} {payload}",
                       "  # KTEST case-begin 1", "  # KTEST phase 0 cleanup",
                       "  # KTEST status 0 Error", f"  not ok 1 {rows[0]['name']}",
                       "  # KTEST suite-error execution failed", "ok 1 os::tests0",
                       "not ok 1 wrong")
            for line in invalid:
                with self.subTest(phase=phase, line=line), self.assertRaises(HostError):
                    parser.feed(line)
            parser.feed("not ok 1 os::tests0")
            parser.feed("# KTEST complete")
            with self.assertRaises(HostError):
                parser.feed(f"  # KTEST case-abort 0 {phase} {payload}")
        for prefix, identity, phase in (([], 0, "cleanup"),
                                       (case_lines(rows[0])[:2], 0, "setup"),
                                       (case_lines(rows[0])[:3], 0, "body"),
                                       (case_lines(rows[0])[:4], 1, "cleanup"),
                                       (case_lines(rows[0])[:4], 0, "join"),
                                       (case_lines(rows[0])[:-1], 0, "cleanup"),
                                       (case_lines(rows[0]), 0, "cleanup")):
            parser = parser_for(rows)
            for line in prefix:
                parser.feed(line)
            with self.subTest(prefix=prefix, identity=identity, phase=phase), self.assertRaises(HostError):
                parser.feed(f"  # KTEST case-abort {identity} {phase} {payload}")

    def test_case_abort_rejects_malformed_duplicate_or_wrong_typed_json_fields(self):
        """JSON 对象必须恰有五个独立字段，不能靠重复键覆盖失败。"""
        ledger = dict(primary=None, child=None, cleanup=None, expectations=0, reason="stop")
        invalid = ["{", "null", "[]", json.dumps(list(ledger.items())),
                   json.dumps({key: value for key, value in ledger.items() if key != "child"}),
                   json.dumps(dict(ledger, extra="unsupported")),
                   json.dumps(ledger)[:-1] + ', "reason":"overwritten"}']
        for key, value in (("primary", 0), ("child", []), ("cleanup", {}),
                           ("expectations", True), ("expectations", -1),
                           ("expectations", 1.5), ("expectations", "0"), ("reason", None)):
            invalid.append(json.dumps(dict(ledger, **{key: value})))
        for payload in invalid:
            parser = parser_for([case_record()])
            for line in case_lines(case_record())[:4]:
                parser.feed(line)
            with self.subTest(payload=payload), self.assertRaises(HostError):
                parser.feed(f"  # KTEST case-abort 0 cleanup {payload}")
            self.assertIsNone(parser.case_abort)
        parser.feed("  # KTEST case-abort 0 cleanup " + json.dumps(ledger))
        self.assertEqual(parser.case_abort, dict(phase="cleanup", **ledger))



class ResourceAndUARTTests(unittest.TestCase):
    """资源隔离和真实 socket 的 raw 段检查。"""

    def test_missing_declared_root_is_error(self):
        """声明根盘资源后必须提供真实输入镜像。"""
        with self.assertRaises(HostError):
            validate_resources({"root"}, options())

    def test_work_copy_does_not_modify_source_and_rejects_alias(self):
        """硬链接和符号链接不能成为可写工作盘。"""
        with tempfile.TemporaryDirectory() as temporary:
            base = Path(temporary)
            source = base / "source.raw"
            source.write_bytes(b"original disk")
            work = base / "work"
            work.mkdir()
            args = options(disks={"root": source})
            disks = prepare_disks({"root"}, args, work)
            disks[0][1].write_bytes(b"changed work disk")
            self.assertEqual(source.read_bytes(), b"original disk")
            for symbolic in (False, True):
                alias_dir = base / ("symbolic" if symbolic else "hard")
                alias_dir.mkdir()
                target = alias_dir / "root.raw"
                if symbolic:
                    target.symlink_to(source)
                else:
                    target.hardlink_to(source)
                self.assertTrue(aliases(target, source))
                with self.assertRaises(HostError):
                    prepare_disks({"root"}, args, alias_dir)
                self.assertEqual(source.read_bytes(), b"original disk")

    def test_vm_disks_live_until_process_wait_and_cleanup_partial_construction(self):
        """真实进程退出前保盘，隔离并行实例并清掉半复制，保留输入及原文日志。"""
        with tempfile.TemporaryDirectory() as temporary:
            base = Path(temporary)
            source = base / "source.raw"
            source.write_bytes(b"original disk")
            source_hash = hashlib.sha256(source.read_bytes()).hexdigest()
            executable = base / "disk-user"
            executable.write_text(
                f"#!{sys.executable}\n"
                "import json,signal,subprocess,sys,time\n"
                "from pathlib import Path\n"
                "child_mode=sys.argv[1:2]==['--child']\n"
                "paths=[Path(p) for p in sys.argv[2:]] if child_mode else "
                "[Path(a.split('file=',1)[1].split(',readonly=',1)[0]) "
                "for a in sys.argv[1:] if a.startswith('if=none,')]\n"
                "report=paths[0].parent.parent\n"
                "handles=[p.open('r+b') for p in paths]\n"
                "child=None if child_mode else subprocess.Popen("
                "[sys.executable,__file__,'--child',*[str(p) for p in paths]])\n"
                "def stop(*unused):\n"
                "    if child is not None: child.wait(timeout=1)\n"
                "    (report/('child-stopped' if child_mode else 'stopped')).write_text("
                "json.dumps([p.exists() for p in paths]))\n"
                "    raise SystemExit(0)\n"
                "signal.signal(signal.SIGTERM,stop)\n"
                "if child_mode:\n"
                "    (report/'child-ready').write_text('ready')\n"
                "else:\n"
                "    while not (report/'child-ready').exists(): time.sleep(0.001)\n"
                "    print('working disks opened',flush=True)\n"
                "    (report/'ready').write_text('ready')\n"
                "while True: time.sleep(1)\n", encoding="utf-8")
            executable.chmod(0o700)
            args = options(qemu=str(executable), memory="128M", disks={"root": source, "extra": source})
            vms = []
            working_disks = []
            try:
                for index in range(2):
                    directory = base / str(index)
                    directory.mkdir()
                    (directory / "uart-segment.raw").write_bytes(b"raw UART evidence")
                    (directory / "root.raw").write_bytes(b"unowned report evidence")
                    vm = VM(args, Path("kernel.elf"), directory, {"root", "extra", "scratch"})
                    vms.append(vm)
                    deadline = time.monotonic() + 5
                    while not (directory / "ready").exists() and time.monotonic() < deadline:
                        time.sleep(0.01)
                    self.assertTrue((directory / "ready").exists())
                    self.assertIsNone(vm.process.poll())
                    command = json.loads((directory / "command.json").read_text())
                    paths = [Path(a.split("file=", 1)[1]) for a in command if a.startswith("if=none,")]
                    working_disks.append(paths)
                    for path in paths:
                        self.assertTrue(path.is_file())
                    vm.raw.write(b"UART original log\n")
                self.assertTrue(set(working_disks[0]).isdisjoint(working_disks[1]))
                working_disks[0][0].write_bytes(b"guest mutation")
                for index, vm in enumerate(vms):
                    vm.close()
                    self.assertEqual(vm.process.returncode, 0)
                    for path in working_disks[index]:
                        self.assertFalse(path.exists())
                    self.assertEqual(json.loads((vm.directory / "stopped").read_text()), [True] * 3)
                    self.assertEqual(json.loads((vm.directory / "child-stopped").read_text()), [True] * 3)
                    self.assertEqual((vm.directory / "uart.log").read_bytes(), b"UART original log\n")
                    self.assertEqual((vm.directory / "qemu.log").read_bytes(), b"working disks opened\n")
                    self.assertTrue((vm.directory / "command.json").is_file())
                    self.assertEqual((vm.directory / "uart-segment.raw").read_bytes(), b"raw UART evidence")
                    self.assertEqual((vm.directory / "root.raw").read_bytes(), b"unowned report evidence")
                    if index == 0:
                        for path in working_disks[1]:
                            self.assertTrue(path.is_file())
                        self.assertIsNone(vms[1].process.poll())
            finally:
                for vm in vms:
                    vm.close()
            partial = []
            no_space = OSError(errno.ENOSPC, "No space left on device")

            def fail_copy(source_file, target_file, length):
                """已实际写出半盘时注入原始宿主 ENOSPC。"""
                target_file.write(source_file.read(3))
                partial.append(Path(target_file.name))
                raise no_space

            directory = base / "partial"
            directory.mkdir()
            (directory / "uart-segment.raw").write_bytes(b"keep")
            with patch("ktest.resources.shutil.copyfileobj", side_effect=fail_copy):
                with self.assertRaises(OSError) as caught:
                    VM(args, Path("kernel.elf"), directory, {"root"})
            self.assertIs(caught.exception, no_space)
            self.assertFalse(partial[0].parent.exists())
            self.assertTrue((directory / "uart.log").is_file())
            self.assertTrue((directory / "qemu.log").is_file())
            self.assertEqual((directory / "uart-segment.raw").read_bytes(), b"keep")
            directory = base / "launch-failure"
            directory.mkdir()
            args.qemu = str(base / "missing-executable")
            with self.assertRaises(FileNotFoundError):
                VM(args, Path("kernel.elf"), directory, {"root"})
            command = json.loads((directory / "command.json").read_text())
            working = Path(next(a.split("file=", 1)[1] for a in command if a.startswith("if=none,")))
            self.assertFalse(working.parent.exists())
            self.assertEqual(hashlib.sha256(source.read_bytes()).hexdigest(), source_hash)

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

    def test_boot_fatal_requires_actual_postevent_register_frame(self):
        """正确kind不能洗掉缺现场、零PC/SP、错cause或只在fatal前出现的业务现场。"""
        frames = (
            ("riscv64", "illegal_instruction", "illegal",
             "synchronous trap pc=0xffffffff802f317e sp=0xffffffff80593760 cause=0x2 bad=0x0", "0x2", "0x3"),
            ("loongarch64", "illegal_instruction", "illegal",
             "synchronous trap pc=0x90000000900a4690 sp=0x9000000090604720 cause=0xd0000 bad=0x90000000900a4690", "0xd0000", "0xc0000"),
            ("loongarch64", "unmapped_refill", "refill",
             "refill trap pc=0x90000000900a46b4 sp=0x9000000090604720 cause=0x1 bad=0x1000", "0x1", "0x0"),
        )
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            for arch, name, fatal, frame, cause, wrong_cause in frames:
                case = case_record(cid=7, name="os::ktest::boot_cases::fatal::traps::" + name)
                event = f"  # KTEST fatal 7 {fatal}\n"
                line = "  # diagnostic: " + frame + "\n"
                (directory / "uart.log").write_text(event + line, encoding="utf-8")
                verify_trap(arch, case, directory, fatal)
                for bad in ("", line + line, line.replace("cause=" + cause, "cause=" + wrong_cause),
                            line.replace("pc=" + frame.split("pc=")[1].split()[0], "pc=0x0"),
                            line.replace("sp=" + frame.split("sp=")[1].split()[0], "sp=0x0")):
                    (directory / "uart.log").write_text(line + event + bad, encoding="utf-8")
                    with self.subTest(arch=arch, name=name, bad=bad), self.assertRaises(HostError):
                        verify_trap(arch, case, directory, fatal)

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
