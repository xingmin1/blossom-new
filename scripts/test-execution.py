#!/usr/bin/env python3
"""运行给定执行测试 ELF，核对真实执行、等待、时间、工作队列与记录输出。

正常镜像按唯一顺序输出 X01–X08、X10–X26、X28；单核仅 X02 明确 N/A。
X10 检查重新武装后的硬 IRQ 延后切换与架构寄存器保存，X11 检查同一时间
算法的维护、快照交错拒绝重试及边界，X12 检查执行/IRQ/scheduler/idle 四账、
迁核和不依靠 timer tick 的远端 kick 与实际等待指令完成。X13–X17 检查原期限
睡眠、同步取消真尾部、复用关闭、远端重编程与有界粗 timer 批处理。
X18–X23 检查真实 worker 扩展与回收、OOM rescuer、有限 flush 边界、
DelayedWork 移交寿命和显式停止；X24 检查同一 Task 的本核/迁核 MM 消费，
X25–X26 检查有界记录与真实 UART 进度，X28 检查混合消费者与整套资源归还。
故障镜像只接受 X09 或 X27 对应的真实不可恢复错误。
"""

import argparse
import errno
import os
from pathlib import Path
import re
import selectors
import socket
import subprocess
import sys
import tempfile
import time
from urllib.parse import quote


def require(condition, message):
    """拒绝缺失、重复或与本次客体目标不一致的证据。"""
    if not condition:
        raise RuntimeError(message)


def fields(payload):
    """读取有限测试记录中的 u64 键值，不接受重复键、非整数或越界值。"""
    result = {}
    for item in payload.split():
        pair = item.split("=", 1)
        require(len(pair) == 2 and pair[0] not in result, "malformed execution field: " + item)
        result[pair[0]] = int(pair[1], 0)
        require(0 <= result[pair[0]] < (1 << 64), "non-u64 execution field: " + item)
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


def check_klog(args, lines):
    """核对完整记录的严格 UTF-8 字节界限、真实身份和逐条声明的覆盖缺口。"""
    records = {}
    previous = 0
    for line in lines:
        if not line.startswith("KLOG"):
            continue
        match = re.fullmatch(
            r"KLOG seq=([0-9]+) level=(ERROR|WARN|INFO|DEBUG) ns=([0-9]+) "
            r"cpu=([0-9]+) task=(none|[0-9]+) truncated=([01]) lost=([0-9]+) (.*)",
            line)
        require(match is not None and "\r" not in line, "malformed KLOG record: " + line)
        seq, level, ns, cpu, task, truncated, lost, message = match.groups()
        seq, ns, cpu, truncated, lost = map(int, (seq, ns, cpu, truncated, lost))
        require(all(0 <= value < (1 << 64) for value in (seq, ns, cpu, lost))
                and seq == previous + 1 + lost and cpu in range(args.cpus)
                and (task == "none" or 0 < int(task) < (1 << 64))
                and len(message.encode("utf-8")) <= 512
                and len(line[:match.start(8)].encode("utf-8")) <= 256,
                "invalid KLOG metadata, byte length, duplicate sequence, or undeclared gap")
        records[seq] = {"level": level, "ns": ns, "cpu": cpu, "task": task,
                        "truncated": truncated, "lost": lost, "message": message}
        previous = seq
    return records


def check_execution(args):
    """核对唯一有序记录及执行、等待、MM、混合消费与整套归还的真实观测关系。"""
    raw_lines = args.output.read_bytes().decode("utf-8", errors="strict").split("\n")
    lines = [line.rstrip("\r") for line in raw_lines]
    begin, cpu_locals = check_header(args, lines)
    require(lines.count("EXECUTION TESTS PASS") == 1,
            "missing or duplicate execution success marker")
    done = lines.index("EXECUTION TESTS PASS")
    require(begin < done, "execution success preceded readiness")
    records = {}
    hops = []
    holds = []
    for index, line in enumerate(lines):
        reject_failure(line.encode("utf-8"))
        require(not line.startswith(("EXECUTION X09", "EXECUTION X27")),
                "normal image armed an execution fault")
        require(not line.startswith("EXECUTION TESTS PASS") or line == "EXECUTION TESTS PASS",
                "malformed execution success marker")
        if line.startswith("PASS execution-"):
            match = re.fullmatch(r"PASS execution-(X0[1-8]|X1[0-9]|X2[0-6]|X28) (.+)", line)
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
        if line.startswith("EXECUTION UART"):
            require(line == "EXECUTION UART HOLD" and "X25" in records
                    and "X26" not in records and begin < index < done,
                    "UART hold is malformed or outside the X25/X26 interval")
            holds.append(index)
    expected = ["X01"] + ([] if args.cpus == 1 else ["X02"]) + [
        "X03", "X04", "X05", "X06", "X07", "X08", "X10", "X11", "X12",
        "X13", "X14", "X15", "X16", "X17", "X18", "X19", "X20", "X21", "X22", "X23",
        "X24", "X25", "X26", "X28"]
    require(list(records) == expected, "missing or out-of-order execution records")
    require(len(holds) == 1, "missing or duplicate UART hold")
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
    check_workqueue(args, records)
    check_memory(args, records)
    logs = check_klog(args, raw_lines)
    check_logging(args, records, lines, logs)
    check_mixed(args, records, lines, logs)


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


def check_workqueue(args, records):
    """严格核对工作队列业务、真实进度与停止回收，区分单核和跨 CPU 证据。"""
    expected = {
        "X18": {"before_duplicates", "running_duplicates", "submitted", "consumed", "callbacks",
                "max_active", "overlap", "ordered", "ordered_fifo", "reclaimed"},
        "X19": {"depth", "blocked", "creators", "released", "paired", "early_wake",
                "retired", "reclaimed"},
        "X20": {"page_null", "heap_null", "create_failures", "rescuer_callbacks",
                "worker_callbacks", "recovered", "reclaimed"},
        "X21": {"remote", "earlier_blocked", "later_done", "future_submitted", "boundary_done",
                "future_running", "ordered", "reclaimed"},
        "X22": {"delayed", "modified", "transfers", "cancelled", "sync_wait", "inflight",
                "orphan", "access_after_close", "reclaimed"},
        "X23": {"self_requeue", "callbacks", "max_active", "reused", "closed", "rejected",
                "stops", "drops", "reclaimed"},
    }
    for name, names in expected.items():
        record = records[name]
        require(set(record) == names and all(0 <= value < (1 << 64) for value in record.values()),
                name + " has missing, extra, or non-u64 workqueue fields")
    eighteenth = records["X18"]
    require(3 <= eighteenth["before_duplicates"] and 3 <= eighteenth["running_duplicates"]
            and 16 <= eighteenth["submitted"] == eighteenth["consumed"]
            and 2 <= eighteenth["callbacks"] and 1 <= eighteenth["overlap"]
            and all(eighteenth[name] == 1 for name in
                    ("max_active", "ordered", "ordered_fifo", "reclaimed")),
            "X18 did not conserve submitted work, reject duplicates, or establish non-reentrant overlap and global ordered FIFO")
    nineteenth = records["X19"]
    require(3 <= nineteenth["depth"] <= nineteenth["blocked"]
            and nineteenth["depth"] - 1 <= nineteenth["creators"]
            and nineteenth["released"] == nineteenth["depth"] and nineteenth["paired"] == 1
            and 1 <= nineteenth["early_wake"] and 1 <= nineteenth["retired"]
            and nineteenth["reclaimed"] == 1,
            "X19 did not create real workers for blocked dependency depth, pair early-wake feedback, or retire and reclaim resources")
    twentieth = records["X20"]
    require(all(1 <= twentieth[name] for name in
                ("page_null", "heap_null", "create_failures", "rescuer_callbacks"))
            and twentieth["worker_callbacks"] == 0
            and twentieth["recovered"] == 1 and twentieth["reclaimed"] == 1,
            "X20 did not exhaust real page/heap allocation, fail worker creation, execute the reserved rescuer, or recover resources")
    twenty_first = records["X21"]
    require(1 <= twenty_first["earlier_blocked"] and 1 <= twenty_first["later_done"]
            and 4 <= twenty_first["future_submitted"] and 1 <= twenty_first["future_running"]
            and all(twenty_first[name] == 1 for name in ("boundary_done", "ordered", "reclaimed")),
            "X21 did not wait for the earlier work prefix, leave future work pending at flush return, or reclaim ordered work")
    require(twenty_first["remote"] == 0 if args.cpus == 1 else 1 <= twenty_first["remote"],
            "X21 remote flush-prefix evidence does not match the CPU count")
    twenty_second = records["X22"]
    require(2 <= twenty_second["delayed"] and 1 <= twenty_second["modified"]
            and 1 <= twenty_second["transfers"] and 1 <= twenty_second["cancelled"]
            and twenty_second["orphan"] == 0 and twenty_second["access_after_close"] == 0
            and twenty_second["reclaimed"] == 1,
            "X22 did not modify, transfer, and cancel real delayed work or preserve storage lifetime through close")
    if args.cpus == 1:
        require(twenty_second["sync_wait"] == 0 and twenty_second["inflight"] == 0,
                "single-CPU X22 reported cross-CPU transfer or synchronous stop-wait overlap")
    else:
        require(2 <= twenty_second["sync_wait"] and 2 <= twenty_second["inflight"],
                "multi-CPU X22 did not block synchronous waiters before and after real timer-to-pool transfer")
    twenty_third = records["X23"]
    require(2 <= twenty_third["self_requeue"] and 3 <= twenty_third["callbacks"]
            and 1 <= twenty_third["rejected"]
            and all(twenty_third[name] == 1 for name in
                    ("max_active", "reused", "closed", "stops", "drops", "reclaimed")),
            "X23 did not establish non-reentrant self-requeue, cancellation reuse, irreversible close, and exactly-once stop/drop")


def check_memory(args, records):
    """核对同一 Task 的本核和迁核 MM 消费、完整内容检查及停稳资源归还。"""
    memory = records["X24"]
    require(set(memory) == {
        "task", "cpus", "visited", "hops", "local_rounds", "page_allocs", "page_frees",
        "heap_allocs", "heap_frees", "flushes", "pressure_pages", "pressure_objects",
        "content_checks", "cpu_checks", "stack_checks", "reclaimed",
    } and all(0 <= value < (1 << 64) for value in memory.values()),
            "X24 has missing, extra, or non-u64 memory fields")
    hops = 0 if args.cpus == 1 else 2 * args.cpus
    require(0 < memory["task"] and memory["cpus"] == args.cpus
            and memory["visited"] == (1 << args.cpus) - 1
            and memory["local_rounds"] == 2 and memory["hops"] == hops
            and 2 * (hops + 2) <= memory["page_allocs"] == memory["page_frees"]
            and 12 * (hops + 2) <= memory["heap_allocs"] == memory["heap_frees"]
            and hops + 2 <= memory["flushes"]
            and 64 <= memory["pressure_pages"] and 96 <= memory["pressure_objects"]
            and memory["page_allocs"] + memory["heap_allocs"] <= memory["content_checks"]
            and hops + 3 <= memory["cpu_checks"] == memory["stack_checks"]
            and memory["reclaimed"] == 1,
            "X24 did not establish complete local/migrating MM consumption, content/CPU/stack checks, pressure, or repayment")


def check_logging(args, records, lines, logs):
    """核对两个日志区间、独立读者缺口和 UART 追赶边界，不混入后续混合日志。"""
    expected = {
        "X25": {"seq_first", "seq_last", "records", "task_records", "timer_records",
                "sched_locked", "pool_locked", "allocation_attempts", "utf8", "truncated",
                "trunc_len", "independent", "reclaimed"},
        "X26": {"held_seq", "offset", "length", "submitted", "capacity", "lost",
                "slow_records", "fast_records", "no_replay", "blocked", "business",
                "zero_writes", "tx_interrupts", "wakes", "printer_gaps", "catchup", "reclaimed",
                "fill_bytes", "fill_zero", "rx_ack"},
    }
    for name, names in expected.items():
        require(set(records[name]) == names
                and all(0 <= value < (1 << 64) for value in records[name].values()),
                name + " has missing, extra, or non-u64 logging fields")
    first = records["X25"]
    require(first["seq_first"] == 1 and first["seq_last"] == first["seq_first"] + 4
            and first["records"] == 5 and first["task_records"] == 4
            and first["timer_records"] == 1 and first["allocation_attempts"] == 0
            and first["trunc_len"] == 510 and first["independent"] == 2
            and all(first[name] == 1 for name in
                    ("sched_locked", "pool_locked", "utf8", "truncated", "reclaimed")),
            "X25 did not preserve complete identities, bounded UTF-8, allocation-free locked producers, or resource repayment")
    positions = {}
    for index, line in enumerate(lines):
        if line.startswith("KLOG "):
            positions[int(line.split(" ", 2)[1][4:])] = index
    markers = {name: next(index for index, line in enumerate(lines)
                          if line.startswith("PASS execution-" + name + " "))
               for name in ("X23", "X25", "X26")}
    levels = ("INFO", "WARN", "DEBUG", "DEBUG", "INFO")
    for offset, number in enumerate(range(first["seq_first"], first["seq_last"] + 1)):
        require(number in logs and markers["X23"] < positions[number] < markers["X25"],
                "X25 did not emit its complete records before its success marker")
        record = logs[number]
        require(record["lost"] == 0 and 0 < record["ns"] and record["level"] == levels[offset]
                and record["truncated"] == (offset == 1),
                "X25 emitted inconsistent level, truncation, time, or lost metadata")
        if offset < 4:
            require(record["task"] != "none" and record["cpu"] == 0
                    and record["task"] == logs[first["seq_first"]]["task"],
                    "X25 ordinary and locked producers changed their actual Task/CPU identity")
    require(logs[first["seq_first"]]["message"] == "X25 ordinary value=37"
            and logs[first["seq_first"] + 1]["message"] == "界" * 170
            and logs[first["seq_last"]]["message"] == "X25 precise timer",
            "X25 did not print its actual formatted value, Unicode truncation, or timer record")
    second = records["X26"]
    held = second["held_seq"]
    holds = [index for index, line in enumerate(lines) if line.startswith("EXECUTION UART")]
    require(len(holds) == 1 and lines[holds[0]] == "EXECUTION UART HOLD"
            and markers["X25"] < holds[0] < positions.get(held, -1),
            "X26 held record did not follow its unique UART hold")
    require(held == first["seq_last"] + 1 and second["catchup"] == held + 73
            and second["submitted"] == second["fast_records"] == 72
            and second["capacity"] == second["slow_records"] == 64
            and second["lost"] == second["printer_gaps"] == 8
            and second["business"] == 4096
            and all(second[name] == 1 for name in ("no_replay", "blocked", "reclaimed"))
            and all(0 < second[name] for name in ("zero_writes", "tx_interrupts", "wakes"))
            and 0 < second["offset"] < second["length"]
            and 0 < second["fill_bytes"] <= 1048576
            and second["fill_zero"] == second["rx_ack"] == 1,
            "X26 did not establish genuine blocked UART progress, exact independent cursor loss, or a fixed catchup boundary")
    sequences = [held] + list(range(held + 9, second["catchup"]))
    require([seq for seq in logs if held <= seq < second["catchup"]] == sequences,
            "X26 replayed an overwritten/current record or omitted its surviving prefix")
    for number in sequences:
        expected_message = ("X26 partial " + "P" * 450 if number == held
                            else f"X26 record={number - held - 1}")
        require(markers["X25"] < positions[number] < markers["X26"]
                and logs[number]["lost"] == (8 if number == held + 9 else 0)
                and logs[number]["message"] == expected_message,
                "X26 printed corrupt/replayed body, declared the wrong gap, or completed outside its marker interval")
    held_line = lines[positions[held]]
    require(second["length"] == len(held_line.encode("utf-8")) + 1,
            "X26 partial offset did not refer to the actual complete held UART record")


def check_mixed(args, records, lines, logs):
    """核对有界混合回调的完整正文、真实身份，以及局部和全套资源归还。"""
    mixed = records["X28"]
    require(set(mixed) == {
        "cpus", "tasks", "rounds", "timer_callbacks", "work_callbacks", "log_records",
        "waited", "blocked", "drops", "stopped", "pending", "waiters", "reclaimed",
        "suite_reclaimed", "seq_first", "seq_last", "catchup",
    } and all(0 <= value < (1 << 64) for value in mixed.values()),
            "X28 has missing, extra, or non-u64 mixed fields")
    callbacks = args.cpus * 8
    require(mixed["cpus"] == mixed["tasks"] == args.cpus and mixed["rounds"] == 8
            and mixed["timer_callbacks"] == mixed["work_callbacks"] == callbacks
            and mixed["log_records"] == mixed["waited"] == 2 * callbacks
            and mixed["blocked"] == mixed["drops"] == args.cpus
            and mixed["stopped"] == 2 * args.cpus
            and mixed["pending"] == mixed["waiters"] == 0
            and mixed["reclaimed"] == mixed["suite_reclaimed"] == 1
            and mixed["seq_first"] == records["X26"]["catchup"]
            and mixed["seq_last"] == mixed["seq_first"] + mixed["log_records"] - 1
            and mixed["catchup"] == mixed["seq_last"] + 1,
            "X28 did not conserve mixed callback/wait/stop totals, repay both baselines, or fix its log boundary")
    markers = {name: next(index for index, line in enumerate(lines)
                          if line.startswith("PASS execution-" + name + " "))
               for name in ("X26", "X28")}
    positions = {int(line.split(" ", 2)[1][4:]): index
                 for index, line in enumerate(lines) if line.startswith("KLOG ")}
    seen = set()
    for sequence, record in logs.items():
        if sequence < mixed["seq_first"]:
            continue
        require(sequence < mixed["catchup"]
                and markers["X26"] < positions[sequence] < markers["X28"],
                "X28 emitted a record outside its fixed sequence or marker interval")
        match = re.fullmatch(r"X28 (timer|work) slot=([0-9]+) round=([0-9]+)", record["message"])
        require(match is not None, "X28 printed corrupt or unrelated callback body")
        source, slot, round_number = match.groups()
        slot, round_number = int(slot), int(round_number)
        require(record["message"] == f"X28 {source} slot={slot} round={round_number}",
                "X28 callback body did not preserve its actual scalar formatting")
        identity = (source, slot, round_number)
        require(slot in range(args.cpus) and round_number in range(8)
                and record["level"] == "INFO" and record["cpu"] == slot
                and 0 < record["ns"] and record["truncated"] == 0
                and (source == "timer" or record["task"] != "none")
                and identity not in seen,
                "X28 printed invalid level, CPU/Task identity, round, truncation, or duplicate callback")
        seen.add(identity)
    require(len(seen) == mixed["log_records"] and mixed["seq_last"] in logs,
            "X28 omitted bounded-capacity callback records or its final record before catchup")



def fault_reason(args):
    """选择目标真实路径的完整 panic 原因，不接受其他异常代替预期故障。"""
    if args.fault == "irq":
        return "task switch with IRQ disabled"
    if args.fault == "preempt":
        return "assertion `left == right` failed: task switch with preemption disabled"
    if args.fault == "timekeeper":
        return "execution timekeeper writer fault"
    return "heap allocation failed: size=4096 align=1"


def is_fault_panic(args, text):
    """识别指定故障生产路径的正式 panic 位置，允许相对或绝对源码路径。"""
    source = ("sched" if args.fault in ("irq", "preempt")
              else "time" if args.fault == "timekeeper" else "main")
    return re.fullmatch(
        rf"FAIL panic: panicked at (?:[^\s]*/)?src/{source}\.rs:[0-9]+:[0-9]+:", text) is not None


def check_fault(args, lines):
    """要求真实准备及实际状态先于指定路径 panic，拒绝正常成功和其他故障路径。"""
    begin, cpu_locals = check_header(args, lines)
    check_klog(args, lines)
    declarations = [(index, line) for index, line in enumerate(lines)
                    if line.startswith("EXECUTION X")]
    panics = [index for index, line in enumerate(lines) if is_fault_panic(args, line)]
    require(len(panics) == 1, "missing or duplicate target-path panic")
    if args.fault in ("irq", "preempt"):
        require(len(declarations) == 1, "missing or duplicate X09 fault declaration")
        index, line = declarations[0]
        require(line.startswith("EXECUTION X09 "), "fault image armed a different execution path")
        evidence = fields(line[len("EXECUTION X09 "):])
        require(set(evidence) == {"guard", "task", "cpu"}
                and all(0 <= value < (1 << 64) for value in evidence.values())
                and evidence["guard"] == (1 if args.fault == "irq" else 2)
                and 0 < evidence["task"] and evidence["cpu"] in cpu_locals,
                "fault did not arm the requested real task/guard/CPU")
        require(begin < index < panics[0], "fault panic preceded readiness or declaration")
    else:
        require(len(declarations) == 2, "missing or duplicate X27 preparation/state")
        prepared, prepare = declarations[0]
        stated, state = declarations[1]
        prepare_prefix = f"EXECUTION X27 prepare fault={args.fault} "
        state_prefix = f"EXECUTION X27 state fault={args.fault} "
        require(prepare.startswith(prepare_prefix) and state.startswith(state_prefix),
                "X27 armed a different fault or malformed its preparation/state")
        evidence = fields(prepare[len(prepare_prefix):])
        require(set(evidence) == {"printer_blocked", "task", "cpu"}
                and all(0 <= value < (1 << 64) for value in evidence.values())
                and evidence["printer_blocked"] == 1 and 0 < evidence["task"]
                and evidence["cpu"] == 0 and evidence["cpu"] in cpu_locals,
                "X27 did not establish a real CPU0 Task and genuinely blocked printer")
        actual = fields(state[len(state_prefix):])
        require(all(0 <= value < (1 << 64) for value in actual.values()),
                "X27 actual state contains a non-u64 field")
        if args.fault == "timekeeper":
            require(set(actual) == {"sequence", "sequence_odd", "raw"}
                    and 0 < actual["sequence"] and actual["sequence"] % 2 == 1
                    and actual["sequence_odd"] == 1,
                    "X27 timekeeper did not report the actual odd writer sequence and raw counter")
        else:
            require(set(actual) == {"pages", "objects", "page_null", "heap_null"}
                    and 0 < actual["pages"] and actual["page_null"] == 1
                    and actual["heap_null"] == 12,
                    "X27 allocator did not exhaust real pages and all twelve heap classes to NULL")
        require(begin < prepared < stated < panics[0],
                "X27 state or target panic preceded actual fault preparation")
    reason = fault_reason(args)
    require(lines.count(reason) == 1 and panics[0] < lines.index(reason),
            "missing or duplicate complete target-path panic reason")
    for text in lines:
        require(not text.startswith(("PASS execution-", "N/A execution-",
                                     "EXECUTION TESTS PASS", "EXECUTION UART"))
                and re.search(r"\bSUCCESS\b", text) is None,
                "fault image reported normal execution or success")
        if not is_fault_panic(args, text):
            reject_failure(text.encode("utf-8"))


def observe_fault(args, lines, line):
    """逐行保留故障证据，完整看到指定生产路径、实际状态和原因前不允许成功。"""
    text = line.decode("utf-8", errors="strict")
    require(not text.startswith("KLOG") or "\r" not in text,
            "KLOG output inserted a carriage return")
    text = text.rstrip("\r")
    lines.append(text)
    require(not text.startswith(("PASS execution-", "N/A execution-", "EXECUTION X02",
                                 "EXECUTION TESTS PASS", "EXECUTION UART"))
            and re.search(r"\bSUCCESS\b", text) is None,
            "fault image reported normal execution or success")
    if text == fault_reason(args):
        check_fault(args, lines)
        return True
    if not is_fault_panic(args, text):
        reject_failure(line)
    return False


def run(args):
    """唯一串口先连接再启动；只限时节制 UART 读取，诊断、截止和故障终止保持运行。"""
    with tempfile.TemporaryDirectory(prefix="execution-uart-") as temporary:
        with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as uart, \
                selectors.DefaultSelector() as selector, args.output.open("wb") as output:
            path = str(Path(temporary) / "uart")
            command = [f"qemu-system-{args.arch}", "-machine", "virt", "-accel", "tcg",
                       "-kernel", str(args.kernel), "-m", args.memory, "-smp", str(args.cpus),
                       "-display", "none", "-monitor", "none", "-chardev",
                       f"socket,id=uart,path={path},server=on,wait=on",
                       "-serial", "chardev:uart", "-no-reboot", "-nic", "none"]
            if args.arch == "riscv64":
                command += ["-bios", "default"]
            uart.setblocking(False)
            process = subprocess.Popen(command, stdin=subprocess.DEVNULL, stdout=subprocess.PIPE,
                                       stderr=subprocess.PIPE, bufsize=0)
            deadline = time.monotonic() + args.timeout
            pending = {"uart": bytearray(), "stdout": bytearray(), "stderr": bytearray()}
            fault_lines = []
            fault_proven = False
            termination = None
            stop_deadline = None
            connected = False
            uart_eof = False
            uart_events = 0
            ack_pending = False
            held = False
            next_read = None
            slow_end = None
            try:
                os.set_blocking(process.stdout.fileno(), False)
                os.set_blocking(process.stderr.fileno(), False)
                selector.register(process.stdout, selectors.EVENT_READ, "stdout")
                selector.register(process.stderr, selectors.EVENT_READ, "stderr")
                while selector.get_map() or (connected and not uart_eof) \
                        or (not connected and process.poll() is None):
                    now = time.monotonic()
                    if stop_deadline is not None and stop_deadline <= now:
                        if process.poll() is None:
                            process.kill()
                            termination = "kill"
                        stop_deadline = None
                    remaining = deadline - now
                    if remaining <= 0:
                        raise TimeoutError(f"QEMU timeout after {args.timeout:g}s")
                    if not connected and process.poll() is None:
                        status = uart.connect_ex(path)
                        if status in (0, errno.EISCONN):
                            connected = True
                        elif status not in (errno.ENOENT, errno.ECONNREFUSED, errno.EAGAIN,
                                            errno.EINPROGRESS, errno.EALREADY):
                            raise OSError(status, os.strerror(status), path)
                    if slow_end is not None and slow_end <= now:
                        next_read = None
                        slow_end = None
                    events = 0
                    if connected and not uart_eof:
                        if next_read is None or next_read <= now:
                            events |= selectors.EVENT_READ
                        if ack_pending:
                            events |= selectors.EVENT_WRITE
                    if events != uart_events:
                        if uart_events:
                            selector.unregister(uart)
                        if events:
                            selector.register(uart, events, "uart")
                        uart_events = events
                    wait = min(remaining, 0.1)
                    if not connected:
                        wait = min(wait, 0.01)
                    if next_read is not None and now < next_read:
                        wait = min(wait, next_read - now)
                    if stop_deadline is not None:
                        wait = min(wait, max(0, stop_deadline - now))
                    for key, ready in selector.select(wait):
                        source = key.data
                        if source == "uart" and ready & selectors.EVENT_WRITE:
                            try:
                                require(uart.send(b"H") == 1, "UART hold acknowledgement closed")
                                ack_pending = False
                            except BlockingIOError:
                                pass
                        if not ready & selectors.EVENT_READ:
                            continue
                        try:
                            if source == "uart":
                                data = uart.recv(32 if slow_end is not None else 65536)
                                if slow_end is not None:
                                    next_read = time.monotonic() + 0.02
                            else:
                                data = os.read(key.fd, 65536)
                        except BlockingIOError:
                            continue
                        if not data:
                            selector.unregister(key.fileobj)
                            if source == "uart":
                                uart_eof = True
                                uart_events = 0
                                require(not ack_pending, "UART ended before hold acknowledgement")
                            continue
                        output.write(data)
                        output.flush()
                        sys.stdout.buffer.write(data)
                        sys.stdout.buffer.flush()
                        buffer = pending[source]
                        buffer.extend(data)
                        complete = buffer.split(b"\n")
                        buffer[:] = complete.pop()
                        for line in complete:
                            if args.fault:
                                require(not line.startswith(b"EXECUTION UART"),
                                        "fault image requested a UART hold")
                                fault_proven = observe_fault(args, fault_lines, line) or fault_proven
                            else:
                                reject_failure(line)
                                if line.startswith(b"EXECUTION UART"):
                                    require(source == "uart" and line == b"EXECUTION UART HOLD"
                                            and not held, "malformed or duplicate UART hold")
                                    held = True
                                    ack_pending = True
                                    start = time.monotonic()
                                    next_read = start + 0.5
                                    slow_end = start + 2.0
                        if fault_proven and termination is None:
                            require(process.poll() is None, "fault guest exited before host termination")
                            process.terminate()
                            termination = "terminate"
                            stop_deadline = time.monotonic() + 2
                require(connected, "QEMU exited before connecting its UART")
                for buffer in pending.values():
                    if buffer:
                        if args.fault:
                            observe_fault(args, fault_lines, bytes(buffer))
                        else:
                            reject_failure(bytes(buffer))
                code = process.wait(timeout=max(0.001, deadline - time.monotonic()))
                if args.fault:
                    require(fault_proven and termination is not None,
                            "fault guest ended without observed panic and explicit host termination")
                    check_fault(args, fault_lines)
                    case = "X09" if args.fault in ("irq", "preempt") else "X27"
                    print(f"HOST OBSERVED: execution-{case} fault={args.fault} method={termination} status={code}")
                else:
                    require(code == 0, f"QEMU exited with status {code}")
                    check_execution(args)
            finally:
                try:
                    if process.poll() is None:
                        process.terminate()
                        try:
                            process.wait(timeout=2)
                        except subprocess.TimeoutExpired:
                            process.kill()
                            process.wait()
                    # 失败路径也保存已经到达的 UART 和诊断尾部；不再依赖宿主 stdout 可写。
                    streams = ([uart] if connected else []) + [process.stdout, process.stderr]
                    for stream in streams:
                        while True:
                            try:
                                data = (uart.recv(65536) if stream is uart
                                        else os.read(stream.fileno(), 65536))
                            except (BlockingIOError, ConnectionResetError):
                                break
                            if not data:
                                break
                            output.write(data)
                    output.flush()
                finally:
                    process.stdout.close()
                    process.stderr.close()


def reject_failure(line):
    """收到真实 panic、失败或致命异常时立刻拒绝，不等待后续成功标记。"""
    text = line.decode("utf-8", errors="strict").rstrip("\r")
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
    parser.add_argument("--fault", choices=("irq", "preempt", "printer-oom", "timekeeper"),
                        help="expect the selected real unrecoverable fault instead of normal completion")
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
    case = "X09" if args.fault in ("irq", "preempt") else "X27"
    outcome = "execution" if args.fault is None else f"execution-{case} fault={args.fault}"
    print(f"HOST PASS: {args.arch} cpus={args.cpus} memory={args.memory} mode={args.mode} {outcome}; raw output: {args.output}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
