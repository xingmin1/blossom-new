"""宿主 protocol 回归；运行 python3 -m unittest discover -s tools/ktest。"""

import json
import unittest

from host_helpers import CONFIG, case_record, case_lines, parser_for
from ktest.catalog import HostError
from ktest.protocol import CaseResult, ready


class ProtocolTests(unittest.TestCase):
    """结果身份、阶段和最终确认都是必需证据。"""

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
            parser.feed(f"  ok 1 {row['name']}")
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

    def test_complete_result_event_keeps_identity_and_rejects_private_status(self):
        """唯一普通终态携带清理后的身份；清 active 不影响回执消费者。"""
        row = case_record(7)
        samples = (("ok", "", "Pass", ""),
                   ("not ok", "", "Fail", ""),
                   ("ok", " # SKIP", "Skip", ""),
                   ("ok", " # SKIP one CPU", "Skip", "one CPU"),
                   ("not ok", " # ERROR", "Error", ""))
        for polarity, directive, status, reason in samples:
            with self.subTest(status=status, directive=directive):
                parser = parser_for([row])
                for line in case_lines(row)[:-1]:
                    self.assertNotIsInstance(parser.feed(line), CaseResult)
                event = parser.feed(f"  {polarity} 1 {row['name']}{directive}")
                self.assertEqual(event, CaseResult(7, status, reason))
                self.assertIsNone(parser.active)
                self.assertIsNone(parser.phase)
                self.assertEqual(parser.results[7], dict(status=status, reason=reason))
        for suffix in (" # SKIP bad", " # ERROR", " # UNKNOWN"):
            parser = parser_for([row])
            for line in case_lines(row)[:-1]:
                parser.feed(line)
            with self.subTest(suffix=suffix), self.assertRaises(HostError):
                parser.feed(f"  {'not ok' if 'SKIP' in suffix else 'ok'} 1 {row['name']}{suffix}")
        parser = parser_for([row])
        for line in case_lines(row)[:-1]:
            parser.feed(line)
        with self.assertRaises(HostError):
            parser.feed("  # KTEST status 7 Pass")

    def test_parameter_directive_text_is_part_of_full_result_identity(self):
        """参数中的分隔符属于完整名字，只有名字后追加的指令改变终态。"""
        for label in ("value # comment", "value # SKIP reason", "value # ERROR"):
            row = case_record(7, parameter=label, name=f"os::tests0::case7[{label}]")
            for status in ("Pass", "Fail", "Skip", "Error"):
                with self.subTest(label=label, status=status):
                    parser = parser_for([row])
                    for line in case_lines(row, status=status)[:-1]:
                        parser.feed(line)
                    event = parser.feed(case_lines(row, status=status)[-1])
                    reason = "insufficient CPUs" if status == "Skip" else ""
                    self.assertEqual(event, CaseResult(7, status, reason))
                    self.assertEqual(parser.results[7], dict(status=status, reason=reason))
                    self.assertIsNone(parser.active)
                    outer = "ok" if status in ("Pass", "Skip") else "not ok"
                    parser.feed(f"{outer} 1 os::tests0")
                    parser.feed("# KTEST complete")
                    self.assertTrue(parser.complete)

    def test_result_directive_starts_only_after_exact_full_name(self):
        """追加指令必须紧邻完整身份，额外名字尾缀或近似名字不能冒充同例。"""
        label = "value # comment"
        row = case_record(7, parameter=label, name=f"os::tests0::case7[{label}]")
        for polarity, suffix, status in (("ok", " # SKIP reason # detail", "Skip"),
                                         ("not ok", " # ERROR actual failure", "Error")):
            with self.subTest(suffix=suffix):
                parser = parser_for([row])
                for line in case_lines(row)[:-1]:
                    parser.feed(line)
                event = parser.feed(f"  {polarity} 1 {row['name']}{suffix}")
                self.assertEqual(event, CaseResult(
                    7, status, "reason # detail" if status == "Skip" else "actual failure"))
        invalid = (
            ("ok", row["name"] + "extra"),
            ("ok", row["name"] + " extra # SKIP reason"),
            ("ok", row["name"] + " # SKIPPER reason"),
            ("ok", row["name"] + " # ERROR"),
            ("not ok", row["name"] + " # SKIP reason"),
            ("ok", row["name"].replace("comment", "different") + " # SKIP reason"),
            ("ok", row["name"][:-1] + " # SKIP reason"),
        )
        for polarity, name in invalid:
            with self.subTest(polarity=polarity, name=name):
                parser = parser_for([row])
                for line in case_lines(row)[:-1]:
                    parser.feed(line)
                with self.assertRaises(HostError):
                    parser.feed(f"  {polarity} 1 {name}")
                self.assertEqual(parser.results, {})
                self.assertEqual(parser.active, 7)

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

    def test_duplex_declaration_validates_both_lengths_crc_and_delays(self):
        """双向协议分别校验RX/TX长度与CRC、每段延迟及非空传输方向。"""
        invalid = ("duplex 0 00000000 0 0 00000000 0",
                   "duplex 1 01234567 0 0 00000001 0",
                   "duplex 0 00000001 0 1 00000000 0",
                   "duplex 0 00000000 60001 1 00000000 0",
                   "duplex 0 00000000 0 1 00000000 60001",
                   "duplex 33554433 01234567 0 33554432 89abcdef 0",
                   "duplex 1 00000000 0 1 00000000")
        for declaration in invalid:
            case = case_record(resources=["uart"])
            parser = parser_for([case])
            for line in case_lines(case)[:3]:
                parser.feed(line)
            with self.subTest(declaration=declaration), self.assertRaises(HostError):
                parser.feed(f"  # KTEST io 0 {declaration}")

        case = case_record(resources=["uart"])
        parser = parser_for([case])
        for line in case_lines(case)[:3]:
            parser.feed(line)
        self.assertEqual(parser.feed("  # KTEST io 0 duplex 1 01234567 10 2 89abcdef 20"),
                         dict(direction="duplex", rx_length=1, rx_digest="01234567",
                              rx_delay_ms=10, tx_length=2, tx_digest="89abcdef",
                              tx_delay_ms=20, case=0))


if __name__ == "__main__":
    unittest.main()
