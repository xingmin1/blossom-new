"""宿主 catalog 回归；运行 python3 -m unittest discover -s tools/ktest。"""

import hashlib
import unittest

from host_helpers import CONFIG, case_record, catalog_of, options
from ktest.catalog import (Catalog, HostError, batches, case_seed, invocations,
                           run_command, select, stable_hash)


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

    def test_catalog_rejects_duplicate_identity_and_counts(self):
        """重复全名或错误尾数量不得构造可运行目录。"""
        with self.assertRaises(HostError):
            catalog_of([case_record(0), case_record(1, name=case_record(0)["name"])])
        catalog = Catalog(CONFIG)
        catalog.feed(dict(type="catalog", version=1, **CONFIG))
        with self.assertRaises(HostError):
            catalog.feed(dict(type="catalog_end", suites=0, cases=1))


if __name__ == "__main__":
    unittest.main()
