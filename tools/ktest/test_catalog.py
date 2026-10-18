"""宿主 catalog 回归；运行 python3 -m unittest discover -s tools/ktest。"""

from contextlib import redirect_stderr, redirect_stdout
import hashlib
import io
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch

from host_helpers import CLI, CONFIG, case_record, catalog_of, options
from ktest.catalog import (Catalog, HostError, batches, case_seed, invocations,
                           run_command, select, stable_hash)


class SelectionTests(unittest.TestCase):
    """精确、glob、参数、标签、排除和分片的外部语义。"""

    def test_exact_parameter_is_not_glob_or_prefix(self):
        """方括号必须逐字相等，不误选相似前缀或 glob 字符集。"""
        rows = [case_record(0, name="os::s::x[a]"), case_record(1, name="os::s::x[a]_tail"),
                case_record(2, name="os::s::xa")]
        actual = select(catalog_of(rows), options(exact=["os::s::x[a]"]), execution=True)
        self.assertEqual([r["name"] for r in actual], ["os::s::x[a]"])

    def test_union_tags_exclude_and_ignore_precedence(self):
        """正向取并集后仍须满足全部标签，并应用最后排除。"""
        rows = [case_record(0, tags=["smp", "mm"]), case_record(1, tags=["smp"]),
                case_record(2, tags=["smp", "mm"], ignore="manual"),
                case_record(3, tags=["smp", "mm"])]
        args = options(exact=[rows[0]["name"]], include=["*case*"], tag=["smp", "mm"], exclude=["*case3*"])
        self.assertEqual(select(catalog_of(rows), args, execution=True), [rows[0]])
        args.include_ignored = True
        self.assertEqual(select(catalog_of(rows), args, execution=True), [rows[0], rows[2]])

    def test_list_keeps_ignored_reasons_and_run_requires_opt_in(self):
        """目录保留 ignore 原因，执行默认只取普通项，显式允许后才取全部。"""
        rows = [case_record(0, name="os::s::z", ignore="requires manual hardware"),
                case_record(1, name="os::s::a"),
                case_record(2, name="os::s::m", ignore="destructive fixture")]
        catalog = catalog_of(rows)
        args = options()
        listed = select(catalog, args, execution=False)
        self.assertEqual(listed, [rows[1], rows[2], rows[0]])
        self.assertEqual([row["ignore"] for row in listed],
                         [None, "destructive fixture", "requires manual hardware"])
        self.assertEqual(select(catalog, args, execution=True), [rows[1]])
        args.include_ignored = True
        self.assertEqual(select(catalog, args, execution=True), listed)

    def test_only_ignored_selection_explains_actual_reasons(self):
        """仅命中 ignored 时报告每项实际原因，并给出执行选项。"""
        rows = [case_record(0, ignore="manual device"), case_record(1, ignore="writes scratch")]
        catalog = catalog_of(rows)
        for args in (options(), options(exact=[rows[0]["name"]])):
            with self.subTest(exact=args.exact):
                self.assertTrue(select(catalog, args, execution=False))
                with self.assertRaises(HostError) as raised:
                    select(catalog, args, execution=True)
                message = str(raised.exception)
                self.assertIn("--include-ignored", message)
                matched = rows if not args.exact else rows[:1]
                for row in matched:
                    self.assertIn(row["name"], message)
                    self.assertIn(row["ignore"], message)

    def test_each_unknown_exact_name_rejects_valid_union_matches(self):
        """任何未知精确全名都报错，不能被其它精确、glob 或参数命中掩盖。"""
        row = case_record()
        catalog = catalog_of([row])
        args = options(exact=[row["name"], "os::missing::first", "os::missing::second"],
                       include=["*"], parameter=["one"])
        for execution in (False, True):
            with self.subTest(execution=execution), self.assertRaises(HostError) as raised:
                select(catalog, args, execution=execution)
            self.assertIn("os::missing::first", str(raised.exception))
            self.assertIn("os::missing::second", str(raised.exception))

    def test_parameter_union_tags_exclude_and_list_ignore(self):
        """参数与精确和 glob 取并集，目录也应用标签及排除但不丢 ignored。"""
        rows = [case_record(0, parameter="alpha", tags=["mm"]),
                case_record(1, parameter="beta", tags=["mm"], ignore="manual"),
                case_record(2, parameter="gamma", tags=["mm"]),
                case_record(3, parameter="beta", tags=[]),
                case_record(4, parameter="beta", tags=["mm"])]
        args = options(exact=[rows[0]["name"]], include=["*case2*"], parameter=["beta"],
                       tag=["mm"], exclude=["*case4*"])
        catalog = catalog_of(rows)
        self.assertEqual(select(catalog, args, execution=False), rows[:3])
        self.assertEqual(select(catalog, args, execution=True), [rows[0], rows[2]])

    def test_list_shards_partition_ignored_and_regular_names(self):
        """目录分片对 ignored 使用同一全名散列，不重漏也不预判 CPU Skip。"""
        rows = [case_record(i, ignore="manual" if i % 2 else None, min_cpus=4)
                for i in range(100)]
        catalog = catalog_of(rows)
        partitions = [select(catalog, options(shard=(i, 3), cpus=1), execution=False)
                      for i in range(3)]
        names = [row["name"] for part in partitions for row in part]
        self.assertEqual(set(names), {row["name"] for row in rows})
        self.assertEqual(len(names), len(rows))
        self.assertEqual(select(catalog, options(cpus=1), execution=True),
                         sorted(rows[::2], key=lambda row: row["name"]))

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
            selected = select(catalog, args, execution=True)
            self.assertEqual([row["name"] for row in selected], expected)
            sequences.append([
                (item.case["name"], item.repeat, item.seed)
                for item in invocations(selected, args)
            ])
        self.assertEqual(sequences[0], sequences[1])

    def test_no_match_is_error(self):
        """无匹配不能产生空的成功。"""
        with self.assertRaises(HostError):
            select(catalog_of([case_record()]), options(exact=["missing"]), execution=True)

    def test_shard_partition_and_fixed_hash(self):
        """所有分片精确分割全名，并使用规定的大端 SHA 前缀。"""
        self.assertEqual(stable_hash("abc"), 0xba7816bf8f01cfea)
        rows = [case_record(i) for i in range(1000)]
        catalog = catalog_of(rows)
        partitions = [select(catalog, options(shard=(i, 7)), execution=True) for i in range(7)]
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
        items = invocations(select(catalog, options(), execution=True), options())
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

    def test_catalog_rejects_duplicate_identity_and_counts(self):
        """重复全名或错误尾数量不得构造可运行目录。"""
        with self.assertRaises(HostError):
            catalog_of([case_record(0), case_record(1, name=case_record(0)["name"])])
        catalog = Catalog(CONFIG)
        catalog.feed(dict(type="catalog", version=1, **CONFIG))
        with self.assertRaises(HostError):
            catalog.feed(dict(type="catalog_end", suites=0, cases=1))


class CatalogCommandTests(unittest.TestCase):
    """目录命令只输出选择与报告，不把静态记录登记为执行成功。"""

    def test_list_prints_ignored_without_execution_or_resource_validation(self):
        """发现后的 list 保留资源元数据及 ignored，报告没有虚构 Pass。"""
        rows = [case_record(0, resources=["root", "net-user"], ignore="manual NIC"),
                case_record(1, resources=["uart"])]
        catalog = catalog_of(rows)

        def fixed_input(args, directory):
            """提供独占输入字节，只替换本测试不涉及的 ELF 格式边界。"""
            kernel = directory / "kernel.elf"
            kernel.write_bytes(b"fixed catalog input")
            return kernel, hashlib.sha256(kernel.read_bytes()).hexdigest()

        with tempfile.TemporaryDirectory() as temporary:
            output = io.StringIO()
            with patch.object(CLI, "fixed_kernel", side_effect=fixed_input), \
                    patch.object(CLI, "discover", return_value=catalog), \
                    patch.object(CLI, "execute", side_effect=AssertionError("list executed cases")), \
                    patch.object(CLI, "validate_resources",
                                 side_effect=AssertionError("list validated runtime resources")), \
                    redirect_stdout(output), redirect_stderr(io.StringIO()):
                status = CLI.main(["list", "--arch", "riscv64", "--seed", "17",
                                   "--report-dir", temporary])
            self.assertEqual(status, 0)
            records = [json.loads(line) for line in output.getvalue().splitlines()]
            self.assertEqual([row for row in records if row["type"] == "case"], rows)
            self.assertEqual(records[-1], dict(type="catalog_end", suites=1, cases=2))
            reports = list(Path(temporary).glob("run-*/results.json"))
            self.assertEqual(len(reports), 1)
            report = json.loads(reports[0].read_text())
            self.assertEqual(report["results"], [])
            self.assertEqual(report["selected"], [row["name"] for row in rows])
            self.assertEqual(report["exit_code"], 0)
            self.assertFalse((reports[0].parent / "kernel.elf").exists())


if __name__ == "__main__":
    unittest.main()
