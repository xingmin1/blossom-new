#!/usr/bin/env python3
"""复用启动串口驱动运行已构建的内存测试 ELF，再核对同一份原始日志。"""

import argparse
from pathlib import Path
import re
import subprocess
import sys
from urllib.parse import quote


def require(condition, message):
    """拒绝缺失或与本次目标不一致的客体证据。"""
    if not condition:
        raise RuntimeError(message)


def check_memory(args):
    """核对 M01–M19 的真实客体字段与必需还账证据，不固定并发计数。"""
    lines = args.output.read_text(encoding="utf-8", errors="replace").splitlines()
    ready = f"MEMORY TEST READY arch={args.arch} cpus={args.cpus} mode={args.mode}"
    require(lines.count(ready) == 1
            and sum(line.startswith("MEMORY TEST READY") for line in lines) == 1,
            "missing, duplicate, or mismatched memory test readiness")
    require(lines.count("MEMORY TESTS PASS") == 1
            and sum(line.startswith("MEMORY TESTS PASS") for line in lines) == 1,
            "missing or duplicate memory success marker")
    require(lines.count("STAGE1 PASS") == 1, "missing stage-one success marker")
    done = lines.index("MEMORY TESTS PASS")
    require(lines.index(ready) < done < lines.index("STAGE1 PASS"),
            "memory and stage-one success markers are out of order")

    records = {}
    record_indices = {}
    for index, line in enumerate(lines):
        if not line.startswith("PASS memory-"):
            continue
        require(index < done, "memory evidence appeared after memory success")
        if line.startswith("PASS memory-try-lock"):
            continue
        match = re.fullmatch(r"PASS (memory-[a-z-]+)(?: (.*))?", line)
        require(match is not None, "malformed memory record: " + line)
        name, payload = match.groups()
        fields = {}
        for field in (payload or "").split():
            pair = field.split("=", 1)
            require(len(pair) == 2 and pair[0] not in fields,
                    "malformed memory field: " + line)
            key, value = pair
            fields[key] = int(value) if value.isdecimal() else value
        if name == "memory-pressure":
            name += "-" + fields["kind"]
        require(name not in records, "duplicate memory record: " + name)
        records[name] = fields
        record_indices[name] = index

    for name in ("boot-ranges", "page-map", "buddy", "owned-pages"):
        require("memory-" + name in records, "missing memory record: " + name)
    for cpu in range(args.cpus):
        require(lines.count(f"PASS memory-try-lock cpu={cpu}") == 1,
                f"missing or duplicate memory try-lock evidence for CPU {cpu}")
    require(sum(line.startswith("PASS memory-try-lock") for line in lines) == args.cpus,
            "unexpected memory try-lock CPU record")

    partial = records["memory-pcp-partial"]
    require("restored" in partial, "missing memory restored field: memory-pcp-partial")
    initial = partial["restored"]
    require(0 < initial and partial["free"] == 2
            and 0 < partial["partial"] < partial["batch"]
            and partial["partial"] == partial["free"]
            and 0 < partial["pressure_handles"], "invalid PCP partial refill evidence")
    transfer = records["memory-pcp-transfer"]
    cross_cpu = 1 < args.cpus
    require(transfer["cpus"] == args.cpus
            and transfer["mode"] == ("cross-cpu" if cross_cpu else "local")
            and 0 < transfer["live"]
            and (0 < transfer["cross_cpu_drops"] if cross_cpu
                 else transfer["cross_cpu_drops"] == 0),
            "invalid PCP ownership transfer evidence")
    drain = records["memory-pcp-drain"]
    require(drain["cpus"] == args.cpus
            and drain["mode"] == ("parallel-direct" if cross_cpu else "local")
            and 0 < drain["batch"] <= drain["high"]
            and drain["high_events"] == args.cpus
            and drain["cached_after_high"] == drain["high"] - drain["batch"]
            and drain["remote_cached"] == drain["batch"] * (args.cpus - 1)
            and 0 < drain["rounds"] and 0 < drain["drains"],
            "invalid PCP high-water or direct-drain evidence")

    local = records["memory-slub-local"]
    require(0 < local["objects"] and 0 < local["local_hits"]
            and 1 < local["active_switches"], "missing SLUB local transitions")
    remote_scope = "SCOPE memory-slub-remote requires=2"
    if cross_cpu:
        remote = records["memory-slub-remote"]
        require(remote["owner"] == 0 and remote["remote"] == 1
                and 0 < remote["objects"] and remote["shared_handoffs"] == 3
                and remote_scope not in lines, "invalid SLUB shared remote handoffs")
    else:
        require(lines.count(remote_scope) == 1 and "memory-slub-remote" not in records,
                "missing single-CPU SLUB scope")
    slub_partial = records["memory-slub-partial"]
    require(all(slub_partial[field] == 1 for field in
                ("full_to_cpu", "cpu_to_node", "node_to_active", "empty_retained", "live_across_shrink"))
            and 0 < slub_partial["bound"], "missing SLUB partial/shrink transitions")
    fallback = records["memory-slub-fallback"]
    require(0 < fallback["preferred"] and fallback["actual"] == 0
            and fallback["objects"] == 1 and 0 < fallback["pressure_handles"],
            "invalid SLUB real minimum-order fallback")
    tail = records["memory-slub-tail"]
    require(0 < tail["order"] and 1 < tail["objects"]
            and tail["head_pfn"] < tail["tail_pfn"] < tail["head_pfn"] + (1 << tail["order"]),
            "missing SLUB non-head-page free evidence")

    layout = records["memory-heap-layout"]
    require(28 <= layout["layouts"] and 0 < layout["max_order"],
            "missing heap Layout coverage")
    zeroed = records["memory-heap-zeroed"]
    require(zeroed["small_neighbors"] == 2 and 4096 < zeroed["large_align"],
            "missing heap zeroed neighbor/alignment evidence")
    realloc = records["memory-heap-realloc"]
    require(0 < realloc["shrink"] < realloc["grow"] and 0 < realloc["pressure_handles"]
            and realloc["failed_old_live"] == 65, "missing failed realloc old-object preservation")
    alloc = records["memory-heap-alloc"]
    require(alloc["mode"] == ("cross-cpu" if cross_cpu else "local")
            and alloc["vec_elements"] == 1025 and 0 < alloc["growths"]
            and alloc["arc_drops"] == 1 and alloc["drop_cpu"] == int(cross_cpu),
            "invalid standard alloc consumer or final Arc Drop evidence")
    alloc_scope = "SCOPE memory-heap-alloc cross-cpu-last-drop requires=2 mode=local"
    require(lines.count(alloc_scope) == (0 if cross_cpu else 1),
            "invalid standard alloc single-CPU scope")

    controlled = records["memory-pressure-controlled"]
    require("restored" in controlled, "missing memory restored field: memory-pressure-controlled")
    require(0 < controlled["pages"] and controlled["restored"] == controlled["pages"],
            "invalid controlled-pool exhaustion evidence")
    global_oom = records["memory-pressure-global"]
    require(global_oom["free"] == 0 and 0 < global_oom["pressure_handles"],
            "missing real global page exhaustion")
    fragmented = records["memory-pressure-fragmented"]
    require(0 < fragmented["order"] and (1 << fragmented["order"]) <= fragmented["free"],
            "missing fragmentation failure with sufficient total free pages")
    over_max = records["memory-pressure-over-max"]
    require(layout["max_order"] < over_max["order"], "missing over-maximum page request refusal")

    irq = records["memory-irq"]
    require("remote_completed" in irq, "missing memory IRQ remote_completed field")
    require(irq["cpus"] == args.cpus
            and irq["mode"] == ("parallel-direct" if cross_cpu else "local")
            and 0 < irq["attempts"] <= irq["budget"] <= 16
            and 0 < irq["window_hits"] == irq["completed"] <= irq["handlers"] <= irq["budget"]
            and irq["stopped"] == args.cpus
            and irq["remote_completed"] == args.cpus - 1
            and irq["remote_frees"] == irq["remote_drains"]
            and ((args.cpus - 1) <= irq["remote_frees"] if cross_cpu
                 else irq["remote_frees"] == 0),
            "missing bounded real allocator-window IRQ alloc/free or stopped remote workers")
    irq_scope = "SCOPE memory-irq cross-cpu-free-drain requires=2 mode=local"
    require(lines.count(irq_scope) == (0 if cross_cpu else 1), "invalid IRQ single-CPU scope")

    live = records["memory-stats-live"]
    final = records["memory-stats-final"]
    require(record_indices["memory-stats-live"] < record_indices["memory-stats-final"],
            "final memory accounting appeared before live-object accounting")
    require(0 < live["objects"] and 0 < live["whole_pages"] and 0 < live["slab"]
            and live["pcp"] == 0 and 0 <= live["buddy"]
            and live["present"] == (live["reserved"] + live["metadata"] + live["buddy"]
                                    + live["pcp"] + live["slab"] + live["whole_pages"]),
            "invalid stopped live-page/slab accounting")
    require(final["stopped"] == args.cpus and final["buddy"] == initial
            and final["pcp"] == 0 and final["slab"] == 0
            and final["present"] == final["reserved"] + final["metadata"] + final["buddy"]
            and all(0 <= live[field] == final[field] for field in ("present", "reserved", "metadata")),
            "invalid final memory repayment or boot/metadata preservation")
    repayment_records = (
        "pcp-partial", "pcp-transfer", "pcp-drain",
        "slub-local", "slub-partial", "slub-fallback", "slub-tail",
        "heap-layout", "heap-zeroed", "heap-realloc", "heap-alloc",
        "pressure-global", "pressure-fragmented", "pressure-over-max", "irq",
    )
    if cross_cpu:
        repayment_records += ("slub-remote",)
    for suffix in repayment_records:
        name = "memory-" + suffix
        fields = records[name]
        require("restored" in fields, "missing memory restored field: " + name)
        require(fields["restored"] == initial, "memory accounting did not restore: " + name)


def main():
    """传递唯一已构建目标给原启动驱动，日志名称区分架构、核数、内存和构建模式。"""
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
        memory_name = quote(args.memory, safe="")
        args.output = args.kernel.with_name(
            f"{args.kernel.name}.{args.arch}.{args.cpus}.{memory_name}.{args.mode}.memory.log")
    if (args.output.resolve() == args.kernel.resolve()
            or (args.output.exists() and args.output.samefile(args.kernel))):
        parser.error("--output cannot overwrite --kernel")
    command = [sys.executable, str(Path(__file__).with_name("test-boot.py")),
               "--arch", args.arch, "--cpus", str(args.cpus), "--memory", args.memory,
               "--kernel", str(args.kernel), "--timeout", str(args.timeout),
               "--output", str(args.output)]
    try:
        subprocess.run(command, check=True)
        check_memory(args)
    except (OSError, RuntimeError, KeyError, TypeError, ValueError, subprocess.CalledProcessError) as error:
        print(f"\nHOST FAIL: {error}; mode={args.mode}; memory={args.memory}; raw output: {args.output}",
              file=sys.stderr)
        return 1
    print(f"HOST PASS: {args.arch} cpus={args.cpus} memory={args.memory} mode={args.mode} memory; raw output: {args.output}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
