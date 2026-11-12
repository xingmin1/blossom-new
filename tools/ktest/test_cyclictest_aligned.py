"""验证同步 cyclictest 串口结果的线程完整性与失败传播。"""

from contextlib import redirect_stderr, redirect_stdout
from io import StringIO
from pathlib import Path
import sys
import tempfile
import unittest


sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from check_cyclictest_aligned import check_log, main


def row(thread, count):
    """生成与实际 cyclictest 输出相同字段形状的一条记录。"""
    return (f"T: {thread} (  {thread + 100}) P:99 I:1000 C: {count:5} "
            "Min:1000000 Act: 0 Avg: 0 Max: 0")


def case(name, counts):
    """构造带评分器标记和采样记录的一次测例输出。"""
    return "\n".join([
        f"====== cyclictest {name} begin ======",
        f"CYCLICTEST_ALIGNED CASE {name} BEGIN",
        *(row(thread, count) for thread, count in enumerate(counts)),
        f"CYCLICTEST_ALIGNED CASE {name} END 0",
        f"====== cyclictest {name} end: success ======",
    ]) + "\n"


def group(root):
    """构造包含四项测例、压力状态和评分器标记的独立串口分组。"""
    return "".join([
        f"#### OS COMP TEST GROUP START cyclictest-{root} ####\n",
        f"CYCLICTEST_ALIGNED GROUP {root} BEGIN\n",
        case("NO_STRESS_P1", [11]),
        case("NO_STRESS_P8", range(21, 29)),
        "====== start hackbench ======\n",
        "Running hackbench\n",
        case("STRESS_P1", [31]),
        case("STRESS_P8", range(41, 49)),
        "====== kill hackbench: success ======\n",
        "CYCLICTEST_ALIGNED PRESSURE 0 0 0\n",
        "CYCLICTEST_ALIGNED EXIT 0\n",
        f"CYCLICTEST_ALIGNED GROUP {root} END\n",
        f"#### OS COMP TEST GROUP END cyclictest-{root} ####\n",
    ])


class AlignedSamplingTests(unittest.TestCase):
    """采样检查只接受完整、各线程有样本且真实运行成功的结果。"""

    def setUp(self):
        """准备两个 libc 的独立分组。"""
        self.log = "boot output\n" + group("musl") + group("glibc") + "CYCLICTEST_ALIGNED SESSION 0\n"

    def test_complete_groups_keep_each_thread_count(self):
        """两个 libc 的四项测例按约定顺序返回各线程的实际采样数。"""
        result = check_log(self.log.replace("\n", "\r\n"))
        expected = {
            "NO_STRESS_P1": [11],
            "NO_STRESS_P8": list(range(21, 29)),
            "STRESS_P1": [31],
            "STRESS_P8": list(range(41, 49)),
        }
        for root in ("musl", "glibc"):
            self.assertEqual(result[root], expected)
            self.assertEqual(tuple(result[root]), tuple(expected))

    def test_zero_sample_with_unspaced_min_is_failure(self):
        """四项测例分别拒绝零样本，即使命令成功且 Min 哨兵没有空格。"""
        for name, thread, count in (
            ("NO_STRESS_P1", 0, 11),
            ("NO_STRESS_P8", 4, 25),
            ("STRESS_P1", 0, 31),
            ("STRESS_P8", 4, 45),
        ):
            bad = self.log.replace(row(thread, count), row(thread, 0), 1)
            with self.subTest(case=name), self.assertRaisesRegex(
                ValueError, f"{name} 的线程 {thread} 没有样本"
            ):
                check_log(bad)

    def test_missing_duplicate_out_of_range_and_malformed_rows_fail(self):
        """P1 和 P8 均拒绝缺行、重复或多余索引及损坏的采样字段。"""
        changes = {
            "p1_missing": self.log.replace(row(0, 11) + "\n", "", 1),
            "p1_duplicate": self.log.replace(row(0, 11), row(0, 11) + "\n" + row(0, 11), 1),
            "p1_extra": self.log.replace(row(0, 11), row(0, 11) + "\n" + row(1, 12), 1),
            "p8_missing": self.log.replace(row(7, 28) + "\n", "", 1),
            "p8_duplicate": self.log.replace(row(7, 28), row(6, 27), 1),
            "p8_out_of_range": self.log.replace(row(7, 28), row(8, 28), 1),
            "p8_malformed": self.log.replace(row(0, 21), row(0, 21).replace("C:", "X:"), 1),
        }
        for name, bad in changes.items():
            with self.subTest(name=name), self.assertRaises(ValueError):
                check_log(bad)

    def test_missing_case_fails(self):
        """缺少任一完整测例时不能把其余测例当作完整结果。"""
        for name, counts in (
            ("NO_STRESS_P1", [11]),
            ("NO_STRESS_P8", range(21, 29)),
            ("STRESS_P1", [31]),
            ("STRESS_P8", range(41, 49)),
        ):
            bad = self.log.replace(case(name, counts), "", 1)
            with self.subTest(case=name), self.assertRaisesRegex(
                ValueError, f"{name} BEGIN.*出现 0 次"
            ):
                check_log(bad)

    def test_complete_cases_out_of_order_fail(self):
        """即使每项测例本身完整，同一负载下 P1 和 P8 换序仍失败。"""
        for first, second in (
            (case("NO_STRESS_P1", [11]), case("NO_STRESS_P8", range(21, 29))),
            (case("STRESS_P1", [31]), case("STRESS_P8", range(41, 49))),
        ):
            bad = self.log.replace(first + second, second + first, 1)
            with self.subTest(first=first.splitlines()[0]), self.assertRaisesRegex(
                ValueError, "测例或收尾标记顺序错误"
            ):
                check_log(bad)

    def test_incomplete_misordered_and_failed_execution_fail(self):
        """截断、重复、乱序和所有真实非零状态都不能通过。"""
        no_end = "CYCLICTEST_ALIGNED CASE NO_STRESS_P8 END 0"
        stress_begin = "CYCLICTEST_ALIGNED CASE STRESS_P8 BEGIN"
        lines = self.log.splitlines()
        left, right = lines.index(no_end), lines.index(stress_begin)
        lines[left], lines[right] = lines[right], lines[left]
        changes = {
            "missing_group_end": self.log.replace("CYCLICTEST_ALIGNED GROUP musl END\n", "", 1),
            "missing_case_end": self.log.replace(no_end + "\n", "", 1),
            "misordered": "\n".join(lines),
            "pressure_sleep_failed": self.log.replace("CYCLICTEST_ALIGNED PRESSURE 0 0 0", "CYCLICTEST_ALIGNED PRESSURE 1 0 0", 1),
            "pressure_kill_failed": self.log.replace("CYCLICTEST_ALIGNED PRESSURE 0 0 0", "CYCLICTEST_ALIGNED PRESSURE 0 1 0", 1),
            "pressure_wait_failed": self.log.replace("CYCLICTEST_ALIGNED PRESSURE 0 0 0", "CYCLICTEST_ALIGNED PRESSURE 0 0 1", 1),
            "runner_timed_out": self.log.replace("CYCLICTEST_ALIGNED EXIT 0", "CYCLICTEST_ALIGNED EXIT 124", 1),
            "extra_row": self.log.replace("CYCLICTEST_ALIGNED EXIT 0", row(0, 1) + "\nCYCLICTEST_ALIGNED EXIT 0", 1),
            "duplicate_group": self.log + group("musl"),
            "duplicate_case": self.log.replace(stress_begin, stress_begin + "\n" + stress_begin, 1),
            "unexpected_marker": self.log.replace("CYCLICTEST_ALIGNED EXIT 0", "CYCLICTEST_ALIGNED UNEXPECTED\nCYCLICTEST_ALIGNED EXIT 0", 1),
            "missing_session": self.log.replace("CYCLICTEST_ALIGNED SESSION 0\n", "", 1),
            "duplicate_session": self.log + "CYCLICTEST_ALIGNED SESSION 0\n",
            "malformed_duplicate_session": self.log + "CYCLICTEST_ALIGNED SESSION broken\n",
            "session_before_group_end": self.log.replace(
                "CYCLICTEST_ALIGNED GROUP glibc END\n#### OS COMP TEST GROUP END cyclictest-glibc ####\nCYCLICTEST_ALIGNED SESSION 0\n",
                "CYCLICTEST_ALIGNED SESSION 0\nCYCLICTEST_ALIGNED GROUP glibc END\n#### OS COMP TEST GROUP END cyclictest-glibc ####\n", 1),
        }
        for name in ("NO_STRESS_P1", "NO_STRESS_P8", "STRESS_P1", "STRESS_P8"):
            marker = f"CYCLICTEST_ALIGNED CASE {name} END"
            changes[f"{name}_failed"] = self.log.replace(marker + " 0", marker + " 1", 1)
        for name, bad in changes.items():
            with self.subTest(name=name), self.assertRaises(ValueError):
                check_log(bad)

    def test_runner_failure_after_successful_cases_fails(self):
        """测例结束后的通知或卸载失败不能被完整采样掩盖。"""
        end = "CYCLICTEST_ALIGNED GROUP glibc END"
        bad = self.log.replace(end, "contest-run: after notify: error\n" + end, 1)
        bad = bad.replace("CYCLICTEST_ALIGNED SESSION 0", "CYCLICTEST_ALIGNED SESSION 1", 1)
        with self.assertRaises(ValueError):
            check_log(bad)

    def test_explicit_root_does_not_substitute_missing_root(self):
        """显式选择 musl 可单独检查，默认检查不能遗漏 glibc。"""
        only_musl = group("musl") + "CYCLICTEST_ALIGNED SESSION 0\n"
        self.assertEqual(tuple(check_log(only_musl, ("musl",))), ("musl",))
        with self.assertRaises(ValueError):
            check_log(only_musl)

    def test_cli_returns_distinct_status_for_result_and_file_errors(self):
        """命令接口报告四项线程数，并区分成功、缺样和无法读取文件。"""
        output = StringIO()
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "uart.log"
            with redirect_stdout(output), redirect_stderr(StringIO()):
                path.write_text(self.log, encoding="utf-8")
                self.assertEqual(main([str(path)]), 0)
                path.write_text(self.log.replace(row(0, 11), row(0, 0), 1), encoding="utf-8")
                self.assertEqual(main([str(path)]), 1)
                self.assertEqual(main([str(path / "missing")]), 2)
                with self.assertRaises(SystemExit) as error:
                    main([str(path), "--root", "musl", "--root", "musl"])
                self.assertEqual(error.exception.code, 2)
        self.assertEqual(output.getvalue().splitlines(), [
            f"{root} {name}: {threads}/{threads}，最少 {minimum} 次采样"
            for root in ("musl", "glibc")
            for name, threads, minimum in (
                ("NO_STRESS_P1", 1, 11),
                ("NO_STRESS_P8", 8, 21),
                ("STRESS_P1", 1, 31),
                ("STRESS_P8", 8, 41),
            )
        ])


if __name__ == "__main__":
    unittest.main()
