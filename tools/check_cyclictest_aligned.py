#!/usr/bin/env python3
"""检查独立 cyclictest 同步用例的串口记录，不计算比赛成绩。"""

import argparse
from pathlib import Path
import re
import sys


PREFIX = "CYCLICTEST_ALIGNED "
CASES = {
    "NO_STRESS_P1": 1,
    "NO_STRESS_P8": 8,
    "STRESS_P1": 1,
    "STRESS_P8": 8,
}
ROOTS = ("musl", "glibc")
ROW = re.compile(r"^T:\s*(\d+)\s+\(\s*\d+\)\s+.*?\bC:\s*(\d+)(?:\s|$)")


def _exactly_one(lines, marker):
    """定位唯一的完整标记，缺失或重复时保留实际数量。"""
    positions = [index for index, line in enumerate(lines) if line == marker]
    if len(positions) != 1:
        raise ValueError(f"标记 {marker!r} 出现 {len(positions)} 次")
    return positions[0]


def _status_marker(lines, prefix, fields):
    """检查状态标记唯一、格式完整且状态为零。"""
    candidates = [(index, line) for index, line in enumerate(lines)
                  if line.startswith(prefix)
                  and (len(line) == len(prefix) or line[len(prefix)].isspace())]
    if len(candidates) != 1:
        raise ValueError(f"状态标记 {prefix!r} 出现 {len(candidates)} 次")
    index, line = candidates[0]
    pattern = re.compile(re.escape(prefix) + r" " + r" ".join([r"(\d+)"] * fields))
    match = pattern.fullmatch(line)
    if match is None:
        raise ValueError(f"状态标记 {prefix!r} 格式错误: {line}")
    statuses = tuple(int(value) for value in match.groups())
    if any(status != 0 for status in statuses):
        raise ValueError(f"{prefix} 返回非零状态 {statuses}")
    return index


def _samples(lines, root, case):
    """核对测例要求的线程索引和实际采样数。"""
    thread_count = CASES[case]
    samples = {}
    for line in lines:
        if not re.match(r"^T\s*:", line):
            continue
        match = ROW.match(line)
        if match is None:
            raise ValueError(f"{root} {case} 的线程记录格式错误: {line}")
        thread, count = (int(value) for value in match.groups())
        if thread not in range(thread_count) or thread in samples:
            raise ValueError(f"{root} {case} 的线程索引无效或重复: {thread}")
        if count == 0:
            raise ValueError(f"{root} {case} 的线程 {thread} 没有样本")
        samples[thread] = count
    if set(samples) != set(range(thread_count)):
        raise ValueError(f"{root} {case} 的线程记录不完整: {sorted(samples)}")
    return [samples[thread] for thread in range(thread_count)]


def check_log(text, roots=ROOTS):
    """检查指定 libc 的四项同步测例和最终退出状态。

    每个 root 依次检查无压力 P1、无压力 P8、有压力 P1、有压力 P8。
    P1 要求唯一线程 0，P8 要求线程 0–7；每个线程的采样数必须为正。
    组、测例、压力、runner 和最终 SESSION 标记必须完整且状态成功。
    返回 root 与测例对应的线程采样数列表，检查失败时抛出 ValueError。
    """
    roots = tuple(roots)
    if not roots or len(set(roots)) != len(roots) or any(root not in ROOTS for root in roots):
        raise ValueError("检查的 root 必须是互不重复的 musl 或 glibc")
    lines = [line.strip() for line in text.splitlines()]
    session = _status_marker(lines, f"{PREFIX}SESSION", 1)
    results = {}
    for root in roots:
        begin = _exactly_one(lines, f"{PREFIX}GROUP {root} BEGIN")
        end = _exactly_one(lines, f"{PREFIX}GROUP {root} END")
        if end <= begin or session <= end:
            raise ValueError(f"{root} 的组或最终状态标记顺序错误")
        group = lines[begin + 1:end]
        case_positions = []
        for case in CASES:
            start = _exactly_one(group, f"{PREFIX}CASE {case} BEGIN")
            finish = _status_marker(group, f"{PREFIX}CASE {case} END", 1)
            case_positions.append((start, finish))
        pressure = _status_marker(group, f"{PREFIX}PRESSURE", 3)
        execution = _status_marker(group, f"{PREFIX}EXIT", 1)
        positions = [position for pair in case_positions for position in pair] + [pressure, execution]
        if any(following <= previous for previous, following in zip(positions, positions[1:])):
            raise ValueError(f"{root} 的测例或收尾标记顺序错误")
        markers = set(positions)
        for index, line in enumerate(group):
            if line.startswith(PREFIX) and index not in markers:
                raise ValueError(f"{root} 存在意外协议标记: {line}")
            if re.match(r"^T\s*:", line) and not any(
                start < index < finish for start, finish in case_positions
            ):
                raise ValueError(f"{root} 的线程记录位于测例之外")
        results[root] = {
            case: _samples(group[start + 1:finish], root, case)
            for case, (start, finish) in zip(CASES, case_positions)
        }
    return results


def main(argv=None):
    """检查指定串口文件；结果不完整返回 1，文件或参数错误返回 2。"""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("log", type=Path, help="本次运行的完整串口记录")
    parser.add_argument("--root", action="append", choices=ROOTS,
                        help="只检查指定 libc，可重复；默认检查两者")
    args = parser.parse_args(argv)
    if args.root and len(set(args.root)) != len(args.root):
        parser.error("--root 不得重复")
    try:
        text = args.log.read_text(encoding="utf-8", errors="replace")
    except OSError as error:
        print(f"读取串口记录失败: {error}", file=sys.stderr)
        return 2
    try:
        results = check_log(text, args.root or ROOTS)
    except ValueError as error:
        print(f"同步采样检查失败: {error}", file=sys.stderr)
        return 1
    for root, cases in results.items():
        for case, samples in cases.items():
            print(f"{root} {case}: {len(samples)}/{CASES[case]}，最少 {min(samples)} 次采样")
    return 0


if __name__ == "__main__":
    sys.exit(main())
