#!/usr/bin/env python3
"""验证 UART 正文损坏、旧正文重放与混合回调身份错误会被正式消费者拒绝。"""

from pathlib import Path
import runpy
from types import SimpleNamespace


def main():
    """运行正式解析函数，核对有界混合正文、来源及虚报覆盖缺口的拒绝边界。"""
    parser = runpy.run_path(str(Path(__file__).with_name("test-execution.py")))
    args = SimpleNamespace(cpus=1)
    held = 6
    lines = ["PASS execution-X23 "]
    bodies = ["X25 ordinary value=37", "界" * 170, "X25 sched locked",
              "X25 pool locked", "X25 precise timer"]
    levels = ["INFO", "WARN", "DEBUG", "DEBUG", "INFO"]
    for number, (body, level) in enumerate(zip(bodies, levels), 1):
        lines.append(f"KLOG seq={number} level={level} ns=1 cpu=0 task=8 "
                     f"truncated={int(number == 2)} lost=0 {body}")
    lines += ["PASS execution-X25 ", "EXECUTION UART HOLD"]
    held_position = len(lines)
    lines.append(f"KLOG seq={held} level=INFO ns=1 cpu=0 task=8 "
                 "truncated=0 lost=0 X26 partial " + "P" * 450)
    first_survivor = len(lines)
    for number in range(held + 9, held + 73):
        lines.append(f"KLOG seq={number} level=INFO ns=1 cpu=0 task=8 "
                     f"truncated=0 lost={8 if number == held + 9 else 0} "
                     f"X26 record={number - held - 1}")
    lines.append("PASS execution-X26 ")
    records = {
        "X25": dict(seq_first=1, seq_last=5, records=5, task_records=4,
                    timer_records=1, sched_locked=1, pool_locked=1,
                    allocation_attempts=0, utf8=1, truncated=1, trunc_len=510,
                    independent=2, reclaimed=1),
        "X26": dict(held_seq=held, offset=224,
                    length=len(lines[held_position].encode("utf-8")) + 1,
                    submitted=72, capacity=64, lost=8, slow_records=64,
                    fast_records=72, no_replay=1, blocked=1, business=4096,
                    zero_writes=12, tx_interrupts=11, wakes=11, printer_gaps=8,
                    catchup=held + 73, reclaimed=1, fill_bytes=280, fill_zero=1,
                    rx_ack=1),
    }
    mixed_first = records["X26"]["catchup"]
    mixed_positions = []
    for round_number in range(8):
        for source in ("timer", "work"):
            sequence = mixed_first + 2 * round_number + (source == "work")
            mixed_positions.append(len(lines))
            lines.append(
                f"KLOG seq={sequence} level=INFO ns=1 cpu=0 "
                f"task={'none' if source == 'timer' else 8} "
                "truncated=0 lost=0 "
                f"X28 {source} slot=0 round={round_number}")
    lines.append("PASS execution-X28 ")
    records["X28"] = dict(cpus=1, tasks=1, rounds=8, timer_callbacks=8,
                          work_callbacks=8, log_records=16, waited=16, blocked=1,
                          drops=1, stopped=2, pending=0, waiters=0, reclaimed=1,
                          suite_reclaimed=1, seq_first=mixed_first,
                          seq_last=mixed_first + 15, catchup=mixed_first + 16)
    parser["check_mixed"](args, records, lines, parser["check_klog"](args, lines))
    parser["check_logging"](args, records, lines, parser["check_klog"](args, lines))
    for position, replacement in ((held_position, "Q"), (first_survivor, "7")):
        corrupted = list(lines)
        corrupted[position] = corrupted[position][:-1] + replacement
        assert len(corrupted[position].encode("utf-8")) == len(lines[position].encode("utf-8"))
        try:
            parser["check_logging"](args, records, corrupted,
                                    parser["check_klog"](args, corrupted))
        except RuntimeError:
            continue
        raise AssertionError("等长 UART 正文损坏或旧正文重放未被拒绝")
    for position, old, replacement in (
            (mixed_positions[2], "round=1", "round=0"),
            (mixed_positions[1], "task=8", "task=none")):
        corrupted = list(lines)
        corrupted[position] = corrupted[position].replace(old, replacement)
        try:
            parser["check_mixed"](args, records, corrupted,
                                  parser["check_klog"](args, corrupted))
        except RuntimeError:
            continue
        raise AssertionError("重复混合回调正文或无 Task 的 work 来源未被拒绝")
    missing = lines[:mixed_positions[0]] + lines[mixed_positions[2]:]
    missing[mixed_positions[0]] = missing[mixed_positions[0]].replace("lost=0", "lost=2", 1)
    try:
        parser["check_mixed"](args, records, missing,
                              parser["check_klog"](args, missing))
    except RuntimeError:
        pass
    else:
        raise AssertionError("环容量内缺失混合回调却虚报覆盖仍被接受")
    print("PASS 执行日志消费者：有效有界正文、等长损坏拒绝、重放拒绝、混合身份及虚报缺口拒绝")


if __name__ == "__main__":
    main()
