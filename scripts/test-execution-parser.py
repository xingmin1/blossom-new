#!/usr/bin/env python3
"""验证实际 UART 正文的等长损坏与旧正文重放会被执行日志消费者拒绝。"""

from pathlib import Path
import runpy
from types import SimpleNamespace


def main():
    """直接运行正式解析函数，保留有效协议及两种等长正文错误的边界。"""
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
    print("PASS 执行日志消费者：有效正文、等长损坏拒绝、旧正文重放拒绝")


if __name__ == "__main__":
    main()
