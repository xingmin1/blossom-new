#!/usr/bin/env python3
"""运行给定执行测试 ELF，核对 X01–X17 的真实执行、等待、时间与 timer 证据。

正常镜像按唯一顺序输出 X01–X08、X10–X17；单核仅 X02 明确 N/A。
X10 检查重新武装后的硬 IRQ 延后切换与架构寄存器保存，X11 检查同一时间
算法的维护、快照交错拒绝重试及边界，X12 检查执行/IRQ/scheduler/idle 四账、
迁核和不依靠 timer tick 的远端 kick 与实际等待指令完成。X13–X17 检查原期限
睡眠、同步取消真尾部、复用关闭、远端重编程与有界粗 timer 批处理；故障镜像只接受 X09 真实拒绝。
"""

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
    """核对唯一有序记录、真实切换与等待，以及统一时间、四账和 timer 观测关系。"""
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
        require(not line.startswith("EXECUTION TESTS PASS") or line == "EXECUTION TESTS PASS",
                "malformed execution success marker")
        if line.startswith("PASS execution-"):
            match = re.fullmatch(r"PASS execution-(X0[1-8]|X1[0-7]) (.+)", line)
            require(match is not None and begin < index < done, "unexpected execution pass: " + line)
            name, payload = match.groups()
            require(name not in records, "duplicate execution record: " + name)
            records[name] = fields(payload)
        require(not line.startswith("EXECUTION X") or line.startswith("EXECUTION X02 "),
                "unexpected execution evidence: " + line)
        if line.startswith("EXECUTION X02"):
            require(begin < index < done and line.startswith("EXECUTION X02 "),
                    "migration evidence is out of order")
            require("X01" in records and "X02" not in records, "migration evidence preceded X01 or followed X02")
            hops.append(fields(line[len("EXECUTION X02 "):]))
    expected = ["X01"] + ([] if args.cpus == 1 else ["X02"]) + [
        "X03", "X04", "X05", "X06", "X07", "X08", "X10", "X11", "X12",
        "X13", "X14", "X15", "X16", "X17"]
    require(list(records) == expected, "missing or out-of-order execution records")
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
    check_time(args, records)
    check_waiting(args, records)
    check_timer(args, records)


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


def check_time(args, records):
    """核对时间四账、真实快照拒绝重试和等待指令完成，并区分架构与 CPU 数。"""
    tenth = records["X10"]
    require(set(tenth) == {"irq", "deferred", "safe_points", "registers"}
            and all(0 <= value < (1 << 64) for value in tenth.values())
            and 1 <= tenth["irq"] and tenth["deferred"] == 1
            and 1 <= tenth["safe_points"]
            and (2 if args.arch == "riscv64" else 1) <= tenth["registers"],
            "X10 did not establish IRQ deferral, legal safe points, and register preservation")
    eleventh = records["X11"]
    require(set(eleventh) == {"frequencies", "cases", "updates", "readers", "overlaps", "retries",
                              "monotonic", "wrap", "fraction", "max_delta"}
            and all(0 <= value < (1 << 64) for value in eleventh.values())
            and 4 <= eleventh["frequencies"] <= eleventh["cases"]
            and 1 <= eleventh["updates"]
            and (1 if args.cpus == 1 else 2) <= eleventh["readers"]
            and all(eleventh[name] == 1 for name in ("monotonic", "wrap", "fraction", "max_delta")),
            "X11 did not establish real maintenance/readers and production time arithmetic boundaries")
    if args.cpus == 1:
        require(eleventh["overlaps"] == 0 and eleventh["retries"] == 0,
                "single-CPU X11 reported cross-CPU snapshot overlap or rejection")
    else:
        require(1 <= eleventh["overlaps"] and 1 <= eleventh["retries"],
                "multi-CPU X11 did not overlap a real update or reject the changed snapshot sequence")
    twelfth = records["X12"]
    require(set(twelfth) == {"task_ns", "wall_ns", "offcpu_ns", "irq_ns", "scheduler_ns",
                             "idle_ns", "migrations", "kick_rounds", "waits", "no_tick"}
            and all(0 <= value < (1 << 64) for value in twelfth.values())
            and 0 < twelfth["task_ns"] < twelfth["wall_ns"]
            and 0 < twelfth["offcpu_ns"] <= twelfth["wall_ns"]
            and twelfth["task_ns"] + twelfth["offcpu_ns"] <= twelfth["wall_ns"]
            and 0 < twelfth["irq_ns"] and 0 < twelfth["scheduler_ns"],
            "X12 did not establish bounded execution time excluding off-CPU time and separate IRQ/scheduler accounts")
    if args.cpus == 1:
        require(twelfth["migrations"] == 0 and twelfth["kick_rounds"] == 0
                and twelfth["no_tick"] == 0 and twelfth["waits"] == 0,
                "single-CPU X12 reported migration or remote hardware-wait/kick evidence")
    else:
        require(0 < twelfth["idle_ns"] and 2 <= twelfth["migrations"]
                and 4 <= twelfth["kick_rounds"] <= twelfth["waits"]
                and twelfth["no_tick"] == 1,
                "multi-CPU X12 did not establish real hardware waits, migration, and tick-free asynchronous kicks")


def check_timer(args, records):
    """严格核对 timer 字段、无提前执行、真实尾部等待和各 CPU 数下的观测下界。"""
    expected = {
        "X13": {"zero", "past", "future", "blocked", "wakes", "deadline_kept",
                "early", "phase", "after"},
        "X14": {"queued", "after", "busy", "inflight", "sync_wait", "tail"},
        "X15": {"rearmed", "callbacks", "max_active", "reused", "closed", "rejected", "overlap"},
        "X16": {"remote", "kicks", "first", "stale", "idle"},
        "X17": {"armed", "cancelled", "fired", "batches", "continuations", "max_batch",
                "budget", "wheel_cases", "jump_ticks", "reclaimed"},
    }
    for name, names in expected.items():
        record = records[name]
        require(set(record) == names and all(0 <= value < (1 << 64) for value in record.values()),
                name + " has missing, extra, or non-u64 timer fields")
    thirteenth = records["X13"]
    require(all(thirteenth[name] == 1 for name in
                ("zero", "past", "future", "deadline_kept", "phase", "after"))
            and 1 <= thirteenth["blocked"] and 3 <= thirteenth["wakes"]
            and thirteenth["early"] == 0,
            "X13 did not preserve the original sleep deadline after early wakes or cover phase boundaries")
    fourteenth = records["X14"]
    require(all(fourteenth[name] == 1 for name in ("queued", "after", "tail"))
            and 1 <= fourteenth["busy"],
            "X14 did not establish queued cancellation, real callback Busy, and completion after the true tail")
    if args.cpus == 1:
        require(fourteenth["inflight"] == 0 and fourteenth["sync_wait"] == 0,
                "single-CPU X14 reported cross-CPU callback or synchronous tail-wait overlap")
    else:
        require(2 <= fourteenth["inflight"] and 2 <= fourteenth["sync_wait"],
                "multi-CPU X14 did not block synchronous waiters in both dequeued and callback-tail windows")
    fifteenth = records["X15"]
    require(1 <= fifteenth["rearmed"] and 2 <= fifteenth["callbacks"]
            and all(fifteenth[name] == 1 for name in ("max_active", "reused", "closed"))
            and 1 <= fifteenth["rejected"],
            "X15 did not establish non-reentrant self-rearm, cancellation reuse, and irreversible close")
    require(fifteenth["overlap"] == 0 if args.cpus == 1 else 1 <= fifteenth["overlap"],
            "X15 running/queued close overlap does not match the CPU count")
    sixteenth = records["X16"]
    require(sixteenth["first"] == 1 and sixteenth["stale"] == 0,
            "X16 did not fire the earlier timer first or let an old plan overwrite its deadline")
    if args.cpus == 1:
        require(all(sixteenth[name] == 0 for name in ("remote", "kicks", "idle")),
                "single-CPU X16 reported remote timer insertion, kick, or hardware idle evidence")
    else:
        require(sixteenth["remote"] == args.cpus - 1
                and sixteenth["remote"] <= sixteenth["kicks"]
                and sixteenth["remote"] <= sixteenth["idle"],
                "multi-CPU X16 did not reprogram every remote CPU after a real idle wait and timer kick")
    seventeenth = records["X17"]
    require(128 <= seventeenth["armed"]
            and seventeenth["armed"] // 2 < seventeenth["cancelled"]
            and seventeenth["fired"] == seventeenth["armed"] - seventeenth["cancelled"]
            and 2 <= seventeenth["batches"] and 1 <= seventeenth["continuations"]
            and 1 <= seventeenth["max_batch"] <= seventeenth["budget"]
            and 12 <= seventeenth["wheel_cases"] and 2 <= seventeenth["jump_ticks"]
            and seventeenth["reclaimed"] == 1,
            "X17 did not conserve timer totals, cancel the majority, run bounded continuation batches, or repay resources")


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
