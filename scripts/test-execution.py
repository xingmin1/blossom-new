#!/usr/bin/env python3
"""运行给定执行测试 ELF，核对真实等待同步或原子上下文中的切换拒绝。"""

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


def check_header(args, lines):
    """核对本次架构、模式、在线 CPU 与唯一准备记录，不接受其他测试镜像。"""
    ready = f"EXECUTION TEST READY arch={args.arch} cpus={args.cpus} mode={args.mode}"
    require(lines.count(ready) == 1
            and sum(line.startswith("EXECUTION TEST READY") for line in lines) == 1,
            "missing, duplicate, or mismatched execution readiness")
    require(not any(line.startswith(("STAGE1 PASS", "MEMORY TESTS PASS")) for line in lines),
            "execution image ran another test path")
    cpu_locals = {}
    for line in lines:
        if line.startswith("CPU id="):
            match = re.fullmatch(r"CPU id=([0-9]+) hardware=([0-9]+) local=(0x[0-9a-f]+) stack=(0x[0-9a-f]+)", line)
            require(match is not None, "malformed boot CPU identity")
            cpu, _, local, stack = (int(value, 0) for value in match.groups())
            require(cpu in range(args.cpus) and cpu not in cpu_locals and 0 < local and 0 < stack,
                    "invalid or duplicate boot CPU identity")
            cpu_locals[cpu] = local
    require(set(cpu_locals) == set(range(args.cpus))
            and len(set(cpu_locals.values())) == args.cpus,
            "missing or aliased CPU local identities")
    return lines.index(ready), cpu_locals


def check_execution(args):
    """核对真实切换、等待窗口、无分配进度、同步可见性及持观察者时的离栈归页。"""
    lines = args.output.read_text(encoding="utf-8", errors="replace").splitlines()
    begin, cpu_locals = check_header(args, lines)
    require(lines.count("EXECUTION TESTS PASS") == 1,
            "missing or duplicate execution success marker")
    done = lines.index("EXECUTION TESTS PASS")
    require(begin < done, "execution success preceded readiness")
    records = {}
    hops = []
    for index, line in enumerate(lines):
        reject_failure(line.encode("utf-8"))
        require(not line.startswith("EXECUTION X09"), "normal image armed a switch fault")
        if line.startswith("PASS execution-"):
            match = re.fullmatch(r"PASS execution-(X0[1-8]) (.+)", line)
            require(match is not None and begin < index < done, "unexpected execution pass: " + line)
            name, payload = match.groups()
            require(name not in records, "duplicate execution record: " + name)
            records[name] = fields(payload)
        if line.startswith("EXECUTION X02"):
            require(begin < index < done and line.startswith("EXECUTION X02 "),
                    "migration evidence is out of order")
            require("X01" in records and "X02" not in records, "migration evidence preceded X01 or followed X02")
            hops.append(fields(line[len("EXECUTION X02 "):]))
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
        require(scopes == [scope]
                and next(index for index, line in enumerate(lines)
                         if line.startswith("PASS execution-X01 ")) < lines.index(scope)
                < next((index for index, line in enumerate(lines)
                        if line.startswith("PASS execution-X03 ")), done)
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
    expected = ["X01"] + ([] if args.cpus == 1 else ["X02"]) + [
        "X03", "X04", "X05", "X06", "X07", "X08"]
    require(list(records) == expected, "missing or out-of-order execution records")
    check_waiting(args, records)


def check_waiting(args, records):
    """按客体实际计数与页账关系核对等待和睡眠同步，不接受空成功记录。"""
    third = records["X03"]
    require(set(third) == {"tasks", "switches", "max_active", "ready_max"}
            and 4 <= third["tasks"] <= third["switches"]
            and third["max_active"] == 1
            and (third["ready_max"] == 1 if args.cpus == 1 else 0 <= third["ready_max"] <= 1),
            "X03 did not establish unique execution and ready membership")
    fourth = records["X04"]
    require(set(fourth) == {"pre_register", "post_register", "blocked", "switching",
                            "ready", "duplicate", "remote", "wake_initial", "wake_pops", "wake_rearms"}
            and all(fourth[name] == 1 for name in
                    ("pre_register", "post_register", "blocked", "switching", "ready"))
            and 5 <= fourth["duplicate"]
            and (fourth["remote"] == 0 if args.cpus == 1 else 1 <= fourth["remote"])
            and all(fourth[name] == (0 if args.cpus == 1 else 2) for name in
                    ("wake_initial", "wake_pops", "wake_rearms")),
            "X04 did not cover real wake windows, duplicate rejection, and bounded wake-all progress")
    fifth = records["X05"]
    require(set(fifth) == {"stale", "blocked", "fresh"}
            and fifth["stale"] == 1 and fifth["blocked"] == 1 and fifth["fresh"] == 1,
            "X05 did not reject the old waker while preserving the new blocked wait")
    sixth = records["X06"]
    require(set(sixth) == {"pages", "classes", "nulls", "blocked", "progress",
                           "baseline", "recovered"}
            and 0 < sixth["pages"] and sixth["classes"] == 12
            and 2 * (sixth["classes"] + 1) <= sixth["nulls"]
            and 1 <= sixth["blocked"] and 1 <= sixth["progress"]
            and 0 < sixth["baseline"] and sixth["recovered"] == sixth["baseline"],
            "X06 did not prove real page/class OOM, blocked progress, and resource repayment")
    seventh = records["X07"]
    require(set(seventh) == {"contenders", "failed_try", "updates", "visibility",
                             "parked", "migrations", "completion_waiters", "tokens", "stolen"}
            and 4 <= seventh["contenders"]
            and 2 * seventh["contenders"] <= seventh["failed_try"]
            and seventh["updates"] == 4 * seventh["contenders"]
            and seventh["visibility"] == seventh["updates"] and 1 <= seventh["parked"]
            and (seventh["migrations"] == 0 if args.cpus == 1 else 1 <= seventh["migrations"])
            and 4 <= seventh["completion_waiters"]
            and seventh["tokens"] == seventh["completion_waiters"] and seventh["stolen"] == 1,
            "X07 did not establish mutex ownership/visibility or real completion token competition")
    eighth = records["X08"]
    require(set(eighth) == {"drops", "retained", "stack_pages", "result"}
            and eighth["drops"] == 1 and eighth["retained"] == 2
            and eighth["stack_pages"] == 16 and eighth["result"] == sum(range(1, 9)),
            "X08 did not repay the stack with observers alive or consume the real join result")


def fault_reason(args):
    """选择集中调度边界实际生成的 panic 原因，不接受其他异常代替拒绝切换。"""
    if args.fault == "irq":
        return "task switch with IRQ disabled"
    return "assertion `left == right` failed: task switch with preemption disabled"


def is_switch_panic(text):
    """识别正式 panic 输出中的调度源码位置，允许构建器使用相对或绝对路径。"""
    return re.fullmatch(
        r"FAIL panic: panicked at (?:[^\s]*/)?src/sched\.rs:[0-9]+:[0-9]+:", text) is not None


def check_fault(args, lines):
    """要求真实 Task 的一次故障声明之后发生对应集中边界 panic，拒绝正常成功。"""
    begin, cpu_locals = check_header(args, lines)
    armed = [(index, line) for index, line in enumerate(lines) if line.startswith("EXECUTION X09")]
    panics = [index for index, line in enumerate(lines) if is_switch_panic(line)]
    require(len(armed) == 1 and len(panics) == 1, "missing or duplicate fault declaration/panic")
    index, line = armed[0]
    require(line.startswith("EXECUTION X09 "), "malformed fault declaration")
    evidence = fields(line[len("EXECUTION X09 "):])
    require(set(evidence) == {"guard", "task", "cpu"}
            and evidence["guard"] == (1 if args.fault == "irq" else 2)
            and 0 < evidence["task"] and evidence["cpu"] in cpu_locals,
            "fault did not arm the requested real task/guard/CPU")
    require(begin < index < panics[0], "fault panic preceded readiness or declaration")
    reason = fault_reason(args)
    require(lines.count(reason) == 1 and panics[0] < lines.index(reason),
            "missing or duplicate centralized guard rejection reason")
    for text in lines:
        require(not text.startswith(("PASS execution-", "N/A execution-", "EXECUTION X02",
                                     "EXECUTION TESTS PASS"))
                and re.search(r"\bSUCCESS\b", text) is None,
                "fault image reported normal execution or success")
        if not is_switch_panic(text):
            reject_failure(text.encode("utf-8"))


def observe_fault(args, lines, line):
    """逐行保留故障证据，完整看到指定拒绝原因前不允许宿主判定成功。"""
    text = line.decode("utf-8", errors="replace").rstrip("\r")
    lines.append(text)
    require(not text.startswith(("PASS execution-", "N/A execution-", "EXECUTION X02",
                                 "EXECUTION TESTS PASS"))
            and re.search(r"\bSUCCESS\b", text) is None,
            "fault image reported normal execution or success")
    if text == fault_reason(args):
        check_fault(args, lines)
        return True
    if not is_switch_panic(text):
        reject_failure(line)
    return False


def run(args):
    """沿唯一 QEMU 驱动保存原始输出；正常等关机，故障看见真实拒绝后有限终止。"""
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
    fault_lines = []
    fault_proven = False
    termination = None
    stop_deadline = None
    try:
        selector.register(process.stdout, selectors.EVENT_READ)
        with args.output.open("wb") as output:
            eof = False
            while not eof:
                if (stop_deadline is not None and stop_deadline <= time.monotonic()
                        and process.poll() is None):
                    process.kill()
                    termination = "kill"
                    stop_deadline = None
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
                        if args.fault:
                            fault_proven = observe_fault(args, fault_lines, line) or fault_proven
                        else:
                            reject_failure(line)
                    if fault_proven and termination is None:
                        require(process.poll() is None, "fault guest exited before host termination")
                        process.terminate()
                        termination = "terminate"
                        stop_deadline = time.monotonic() + 2
            if pending:
                if args.fault:
                    observe_fault(args, fault_lines, bytes(pending))
                else:
                    reject_failure(bytes(pending))
            code = process.wait(timeout=max(0.001, deadline - time.monotonic()))
            if args.fault:
                require(fault_proven and termination is not None,
                        "fault guest ended without observed panic and explicit host termination")
            else:
                require(code == 0, f"QEMU exited with status {code}")
        if args.fault:
            check_fault(args, fault_lines)
            print(f"HOST OBSERVED: execution-X09 fault={args.fault} method={termination} status={code}")
        else:
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
    parser.add_argument("--fault", choices=("irq", "preempt"),
                        help="expect a real centralized switch rejection instead of normal completion")
    parser.add_argument("--timeout", type=float, default=30, help="whole-machine timeout in seconds")
    parser.add_argument("--output", type=Path, help="raw guest/QEMU output (default: beside ELF)")
    args = parser.parse_args()
    if not args.kernel.is_file():
        parser.error("--kernel must name an already built ELF")
    if not 0 < args.timeout < float("inf"):
        parser.error("--timeout must be finite and positive")
    if args.output is None:
        memory = quote(args.memory, safe="")
        suffix = "execution" if args.fault is None else f"execution-fault-{args.fault}"
        args.output = args.kernel.with_name(
            f"{args.kernel.name}.{args.arch}.{args.cpus}.{memory}.{args.mode}.{suffix}.log")
    if (args.output.resolve() == args.kernel.resolve()
            or (args.output.exists() and args.output.samefile(args.kernel))):
        parser.error("--output cannot overwrite --kernel")
    try:
        run(args)
    except (OSError, RuntimeError, TimeoutError, subprocess.TimeoutExpired, KeyError, TypeError, ValueError) as error:
        print(f"\nHOST FAIL: {error}; mode={args.mode}; memory={args.memory}; raw output: {args.output}", file=sys.stderr)
        return 1
    outcome = "execution" if args.fault is None else f"execution-X09 fault={args.fault}"
    print(f"HOST PASS: {args.arch} cpus={args.cpus} memory={args.memory} mode={args.mode} {outcome}; raw output: {args.output}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
