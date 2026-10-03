#!/usr/bin/env python3
"""运行唯一已构建执行测试 ELF，以真实任务切换和迁核记录核对客体退出。"""

import argparse
import os
from pathlib import Path
import re
import selectors
import subprocess
import sys
import time
from urllib.parse import quote


def require(condition, message):
    """拒绝缺失、重复或与本次客体目标不一致的证据。"""
    if not condition:
        raise RuntimeError(message)


def fields(payload):
    """读取有限测试记录中的整数键值，不接受重复键或非整数值。"""
    result = {}
    for item in payload.split():
        pair = item.split("=", 1)
        require(len(pair) == 2 and pair[0] not in result, "malformed execution field: " + item)
        result[pair[0]] = int(pair[1], 0)
    return result


def check_execution(args):
    """核对同 CPU 真切换归页与同一任务实际目的 CPU，单核迁核必须不适用。"""
    lines = args.output.read_text(encoding="utf-8", errors="replace").splitlines()
    ready = f"EXECUTION TEST READY arch={args.arch} cpus={args.cpus} mode={args.mode}"
    require(lines.count(ready) == 1
            and sum(line.startswith("EXECUTION TEST READY") for line in lines) == 1,
            "missing, duplicate, or mismatched execution readiness")
    require(lines.count("EXECUTION TESTS PASS") == 1,
            "missing or duplicate execution success marker")
    begin, done = lines.index(ready), lines.index("EXECUTION TESTS PASS")
    require(begin < done, "execution success preceded readiness")
    require(not any(line.startswith(("STAGE1 PASS", "MEMORY TESTS PASS")) for line in lines),
            "execution image ran another test path")
    records = {}
    hops = []
    cpu_locals = {}
    for index, line in enumerate(lines):
        cpu_match = re.fullmatch(r"CPU id=([0-9]+) hardware=([0-9]+) local=(0x[0-9a-f]+) stack=(0x[0-9a-f]+)", line)
        if cpu_match:
            cpu, _, local, _ = (int(value, 0) for value in cpu_match.groups())
            require(cpu in range(args.cpus) and cpu not in cpu_locals and 0 < local,
                    "invalid or duplicate boot CPU identity")
            cpu_locals[cpu] = local
        if line.startswith("PASS execution-"):
            match = re.fullmatch(r"PASS execution-(X01|X02) (.*)", line)
            require(match is not None and begin < index < done, "unexpected execution pass: " + line)
            name, payload = match.groups()
            require(name not in records, "duplicate execution record: " + name)
            records[name] = fields(payload)
        if line.startswith("EXECUTION X02"):
            require(begin < index < done and line.startswith("EXECUTION X02 "),
                    "migration evidence is out of order")
            require("X01" in records and "X02" not in records, "migration evidence preceded X01 or followed X02")
            hops.append(fields(line[len("EXECUTION X02 "):]))
    require(set(cpu_locals) == set(range(args.cpus))
            and len(set(cpu_locals.values())) == args.cpus,
            "missing or aliased CPU local identities")
    first = records["X01"]
    require(set(first) == {"task0", "task1", "stack0", "stack1", "rounds", "registers",
                           "deep_frames", "starts", "drops", "order", "reclaimed", "observers"},
            "invalid X01 record fields")
    require(0 < first["task0"] and 0 < first["task1"] and first["task0"] != first["task1"]
            and 0 < first["stack0"] and 0 < first["stack1"]
            and 65536 <= abs((first["stack0"] & ~65535) - (first["stack1"] & ~65535)),
            "X01 did not establish independent tasks and stacks")
    require(first["rounds"] == 8 and first["registers"] == 16 and first["deep_frames"] == 18
            and first["starts"] == 2 and first["drops"] == 2 and first["order"] == 4
            and first["reclaimed"] == 32 and first["observers"] == 2,
            "missing X01 register, deep return, exactly-once, or stack repayment evidence")
    scope = "N/A execution-X02 requires=2 cpus=1"
    scopes = [line for line in lines if line.startswith("N/A execution-")]
    if args.cpus == 1:
        require(scopes == [scope] and begin < lines.index(scope) < done
                and "X02" not in records and not hops, "single-CPU migration must be N/A")
    else:
        require(not scopes, "multi-CPU migration was skipped")
        second = records["X02"]
        require(set(second) == {"task", "cpus", "hops", "rounds"}
                and 0 < second["task"] and second["task"] not in (first["task0"], first["task1"])
                and second["cpus"] == args.cpus and second["hops"] == 2 * args.cpus
                and second["rounds"] == 2 and len(hops) == 2 * args.cpus + 1,
                "missing X02 complete real migration sequence")
        stack = hops[0]["stack"]
        require(0 < stack, "migration stack is zero")
        for number, hop in enumerate(hops):
            cpu = number % args.cpus
            require(set(hop) == {"hop", "task", "cpu", "local", "stack"}
                    and hop["hop"] == number and hop["task"] == second["task"]
                    and hop["cpu"] == cpu and hop["local"] == cpu_locals[cpu]
                    and hop["stack"] == stack,
                    "migration changed identity, stack, or destination CPU base")


def run(args):
    """复用启动驱动的 QEMU 管道与有限期限协议，完整关机后再读取原始证据。"""
    command = [f"qemu-system-{args.arch}", "-machine", "virt", "-accel", "tcg",
               "-kernel", str(args.kernel), "-m", args.memory, "-smp", str(args.cpus),
               "-display", "none", "-monitor", "none", "-serial", "stdio",
               "-no-reboot", "-nic", "none"]
    if args.arch == "riscv64":
        command += ["-bios", "default"]
    process = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                               stderr=subprocess.STDOUT, bufsize=0)
    selector = selectors.DefaultSelector()
    deadline = time.monotonic() + args.timeout
    pending = bytearray()
    try:
        selector.register(process.stdout, selectors.EVENT_READ)
        with args.output.open("wb") as output:
            eof = False
            while not eof:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise TimeoutError(f"QEMU timeout after {args.timeout:g}s")
                for key, _ in selector.select(min(remaining, 0.1)):
                    data = os.read(key.fd, 65536)
                    if not data:
                        eof = True
                        break
                    output.write(data)
                    output.flush()
                    sys.stdout.buffer.write(data)
                    sys.stdout.buffer.flush()
                    pending.extend(data)
                    while b"\n" in pending:
                        line, _, rest = pending.partition(b"\n")
                        pending[:] = rest
                        reject_failure(line)
            if pending:
                reject_failure(bytes(pending))
            code = process.wait(timeout=max(0.001, deadline - time.monotonic()))
            require(code == 0, f"QEMU exited with status {code}")
        check_execution(args)
    finally:
        selector.close()
        if process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=2)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
        process.stdin.close()
        process.stdout.close()


def reject_failure(line):
    """收到真实 panic、失败或致命异常时立刻拒绝，不等待后续成功标记。"""
    text = line.decode("utf-8", errors="replace").rstrip("\r")
    if "FAIL" in text or "FATAL" in text or "panicked" in text or re.search(r"\b[Pp][Aa][Nn][Ii][Cc]\b", text):
        raise RuntimeError("guest reported failure: " + text)


def main():
    """仅接受给定 ELF 和有限超时，禁止原日志路径、软链接或硬链接覆盖输入。"""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--arch", required=True, choices=("riscv64", "loongarch64"))
    parser.add_argument("--cpus", required=True, type=int, choices=(1, 2, 4))
    parser.add_argument("--memory", default="1G", help="QEMU memory specification (default: 1G)")
    parser.add_argument("--mode", required=True, choices=("debug", "release"))
    parser.add_argument("--kernel", required=True, type=Path)
    parser.add_argument("--timeout", type=float, default=30, help="whole-machine timeout in seconds")
    parser.add_argument("--output", type=Path, help="raw guest/QEMU output (default: beside ELF)")
    args = parser.parse_args()
    if not args.kernel.is_file():
        parser.error("--kernel must name an already built ELF")
    if not 0 < args.timeout < float("inf"):
        parser.error("--timeout must be finite and positive")
    if args.output is None:
        memory = quote(args.memory, safe="")
        args.output = args.kernel.with_name(
            f"{args.kernel.name}.{args.arch}.{args.cpus}.{memory}.{args.mode}.execution.log")
    if (args.output.resolve() == args.kernel.resolve()
            or (args.output.exists() and args.output.samefile(args.kernel))):
        parser.error("--output cannot overwrite --kernel")
    try:
        run(args)
    except (OSError, RuntimeError, TimeoutError, subprocess.TimeoutExpired, KeyError, TypeError, ValueError) as error:
        print(f"\nHOST FAIL: {error}; mode={args.mode}; memory={args.memory}; raw output: {args.output}", file=sys.stderr)
        return 1
    print(f"HOST PASS: {args.arch} cpus={args.cpus} memory={args.memory} mode={args.mode} execution; raw output: {args.output}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
