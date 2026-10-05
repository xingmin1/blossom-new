#!/usr/bin/env python3
"""唯一 ktest list/run 入口：固定 ELF、发现目录、选择、执行与同次报告。"""

import argparse
import json
import math
import os
from pathlib import Path
import secrets
import shutil
import signal
import subprocess
import sys
import tempfile
import xml.etree.ElementTree as ET

from ktest.catalog import HostError, require, select, invocations, batches
from ktest.resources import regular_file, hash_file, resource_names, validate_resources
from ktest.boot import elf_layout
from ktest.runner import CANCELLED, discover, execute, config

ROOT = Path(__file__).resolve().parents[1]


def positive(value):
    """CLI 时间必须有限且为正数。"""
    number = float(value)
    if not math.isfinite(number) or number <= 0:
        raise argparse.ArgumentTypeError("must be finite and positive")
    return number


def natural(value):
    """计数不能是零或负数。"""
    number = int(value)
    if number <= 0:
        raise argparse.ArgumentTypeError("must be positive")
    return number


def seed_value(value):
    """种子严格匹配 UART u64 范围。"""
    number = int(value, 0)
    if not 0 <= number < 1 << 64:
        raise argparse.ArgumentTypeError("seed must be u64")
    return number


def shard_value(value):
    """分片索引从零开始，并且小于总片数。"""
    try:
        index, count = map(int, value.split("/"))
        if not 0 <= index < count:
            raise ValueError()
    except ValueError:
        raise argparse.ArgumentTypeError("shard must be INDEX/COUNT with 0 <= INDEX < COUNT") from None
    return index, count


def arguments(argv=None):
    """两个子命令共享构建、资源与明确设备选择，不维护第二套规则。"""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("command", choices=("list", "run"))
    parser.add_argument("--arch", required=True, choices=("riscv64", "loongarch64"))
    parser.add_argument("--cpus", type=int, choices=(1, 2, 4), default=2)
    parser.add_argument("--memory")
    parser.add_argument("--mode", choices=("debug", "release"), default="debug")
    parser.add_argument("--panic", choices=("unwind", "abort"), default="unwind")
    parser.add_argument("--kernel", type=Path)
    parser.add_argument("--qemu", help="explicit QEMU binary")
    parser.add_argument("--exact", action="append", default=[])
    parser.add_argument("--glob", "--include", dest="include", action="append", default=[])
    parser.add_argument("--parameter", action="append", default=[])
    parser.add_argument("--exclude", action="append", default=[])
    parser.add_argument("--tag", action="append", default=[])
    parser.add_argument("--include-ignored", action="store_true")
    parser.add_argument("--shard", type=shard_value)
    parser.add_argument("--repeat", type=natural, default=1)
    parser.add_argument("--shuffle", action="store_true")
    parser.add_argument("--seed", type=seed_value)
    parser.add_argument("--jobs", type=natural, default=1)
    parser.add_argument("--isolation", choices=("suite", "case"), default="suite")
    parser.add_argument("--disk", action="append", default=[], metavar="root|extra|scratch=PATH")
    parser.add_argument("--root", help="explicit root block name, such as /dev/vda")
    parser.add_argument("--extra-root", help="explicit extra filesystem block name")
    parser.add_argument("--read-only", action="append", default=[], choices=("root", "extra", "scratch"))
    parser.add_argument("--scratch-bytes", type=natural, default=64 * 1024 * 1024)
    parser.add_argument("--logical-block-size", type=int, choices=(512, 1024, 2048, 4096), default=512)
    parser.add_argument("--modern", action="store_true")
    parser.add_argument("--uart-payload", type=Path)
    parser.add_argument("--execution-uart", action="store_true",
                        help="enable the concrete opaque execution-log backpressure fixture")
    parser.add_argument("--ap-freeze", action="store_true", help="freeze the actual first AP entry with GDB")
    parser.add_argument("--gdb", help="explicit GDB binary for the AP freeze fixture")
    parser.add_argument("--iommu-platform", action="store_true", help="require ACCESS_PLATFORM in its declared status-only fixture")
    parser.add_argument("--startup-timeout", type=positive, default=30)
    parser.add_argument("--setup-timeout", type=positive, default=30)
    parser.add_argument("--case-timeout", type=positive, default=120)
    parser.add_argument("--cleanup-timeout", type=positive, default=30)
    parser.add_argument("--build-timeout", type=positive, default=600)
    parser.add_argument("--report-dir", type=Path, default=ROOT / "build" / "ktest")
    parser.add_argument("--uart-expect", type=Path, help="expected full tx/pause/slow payload")
    args = parser.parse_args(argv)
    if args.repeat >= 1 << 32:
        parser.error("repeat must be u32")
    if args.modern and args.arch != "riscv64":
        parser.error("--modern requires riscv64")
    args.memory = args.memory or ("512M" if args.arch == "riscv64" else "768M")
    if not args.memory or any(c in args.memory for c in "\n\r\0"):
        parser.error("invalid memory")
    args.seed = args.seed if args.seed is not None else secrets.randbits(64)
    args.disks = {}
    for specification in args.disk:
        name, separator, path = specification.partition("=")
        if separator != "=" or name not in ("root", "extra", "scratch") or not path or name in args.disks:
            parser.error("--disk requires unique root|extra|scratch=PATH")
        args.disks[name] = Path(path).resolve()
    return args


def terminate_group(process):
    """构建子命令的后代也必须退出，超时不能遗留 rustc 或链接进程。"""
    try:
        os.killpg(process.pid, signal.SIGTERM)
    except ProcessLookupError:
        pass
    try:
        process.wait(timeout=2)
    except subprocess.TimeoutExpired:
        pass
    try:
        os.killpg(process.pid, signal.SIGKILL)
    except ProcessLookupError:
        pass
    process.wait()


def fixed_kernel(args, directory):
    """一次运行固定独占 ELF 副本，构建与 catalog 不引用可变外部文件。"""
    kernel = directory / "kernel.elf"
    if args.kernel is not None:
        source = args.kernel.resolve()
        regular_file(source)
        with source.open("rb") as input_file, kernel.open("xb") as output:
            shutil.copyfileobj(input_file, output, 1024 * 1024)
    else:
        command = ["make", "ktest-build", f"ARCH={args.arch}", f"CPUS={args.cpus}",
                   f"MEMORY={args.memory}", f"MODE={args.mode}", f"PANIC={args.panic}",
                   f"OUTPUT={kernel}"]
        (directory / "build-command.json").write_text(json.dumps(command) + "\n", encoding="utf-8")
        with (directory / "build.log").open("xb") as log:
            process = subprocess.Popen(command, cwd=ROOT, stdin=subprocess.DEVNULL,
                                       stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
            try:
                status = process.wait(timeout=args.build_timeout)
            finally:
                terminate_group(process)
        require(status == 0, f"ktest-build failed: {status}")
    regular_file(kernel)
    require(not kernel.is_symlink() and kernel.stat().st_nlink == 1, "fixed ELF must be an independent file")
    layout = elf_layout(kernel, args.arch)
    (directory / "elf-layout.json").write_text(json.dumps(layout, indent=2) + "\n", encoding="utf-8")
    kernel.chmod(0o400)
    return kernel, hash_file(kernel)


def exit_status(results, infrastructure=None):
    """基础设施错误优先，其次真实失败；全 Skip 单独返回四。"""
    if infrastructure or any(r["status"] == "Error" or r.get("infrastructure_error") for r in results):
        return 2
    if any(r["status"] in ("Fail", "Crash", "Timeout", "NotRun") for r in results):
        return 1
    if results and all(r["status"] == "Skip" for r in results):
        return 4
    return 0 if results else 2


def export_reports(directory, report):
    """JSON 和 JUnit 使用同一结果对象，不再次解析日志猜测成功。"""
    (directory / "results.json").write_text(json.dumps(report, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    results = report["results"]
    suite = ET.Element("testsuite", name="ktest", tests=str(len(results) + bool(report.get("error"))),
                       failures=str(sum(r["status"] in ("Fail", "Crash", "Timeout") for r in results)),
                       errors=str(sum(r["status"] in ("Error", "NotRun") or bool(r.get("infrastructure_error")) for r in results)
                                  + bool(report.get("error"))),
                       skipped=str(sum(r["status"] == "Skip" for r in results)))
    properties = ET.SubElement(suite, "properties")
    for name in ("elf_sha256", "seed", "arch", "cpus", "mode", "panic"):
        ET.SubElement(properties, "property", name=name, value=str(report.get(name, "")))
    for row in results:
        case = ET.SubElement(suite, "testcase", name=row["name"], classname=str(row["suite"]))
        attrs = ET.SubElement(case, "properties")
        for name in ("status", "repeat", "seed", "order", "log"):
            ET.SubElement(attrs, "property", name=name, value=str(row[name]))
        status = row["status"]
        if status != "Pass":
            kind = "skipped" if status == "Skip" else "failure" if status in ("Fail", "Crash", "Timeout") else "error"
            ET.SubElement(case, kind, type=status, message=row["reason"] or status)
        if row.get("infrastructure_error"):
            ET.SubElement(case, "error", type="Protocol", message=row["infrastructure_error"])
        ET.SubElement(case, "system-out").text = "raw UART log: " + row["log"]
    if report.get("error"):
        case = ET.SubElement(suite, "testcase", name="host infrastructure")
        ET.SubElement(case, "error", type="HostError", message=report["error"])
    ET.ElementTree(suite).write(directory / "results.xml", encoding="utf-8", xml_declaration=True)


def main(argv=None):
    """完整 list/run 生命周期只产生本次证据，原输入不修改。"""
    args = arguments(argv)
    CANCELLED.clear()
    try:
        args.report_dir.mkdir(parents=True, exist_ok=True)
        directory = Path(tempfile.mkdtemp(prefix="run-", dir=args.report_dir)).resolve()
    except OSError as error:
        print(f"KTEST host error: {error}", file=sys.stderr)
        return 2
    report = dict(version=1, command=sys.argv if argv is None else argv, seed=args.seed,
                  memory=args.memory, jobs=args.jobs, isolation=args.isolation,
                  **config(args), results=[], report_dir=str(directory))
    try:
        kernel, digest = fixed_kernel(args, directory)
        report["elf_sha256"] = digest
        report["kernel"] = str(kernel)
        catalog_dir = directory / "catalog"
        catalog_dir.mkdir()
        catalog = discover(args, kernel, catalog_dir)
        selected = select(catalog, args)
        report["selected"] = [case["name"] for case in selected]
        if args.command == "list":
            print(json.dumps(catalog.header))
            for sid in dict.fromkeys(case["suite"] for case in selected):
                print(json.dumps(catalog.suites[sid], ensure_ascii=False))
            for case in selected:
                print(json.dumps(case, ensure_ascii=False))
            print(json.dumps(dict(type="catalog_end", suites=len({case["suite"] for case in selected}),
                                  cases=len(selected))))
            report["exit_code"] = 0
        else:
            items = invocations(selected, args)
            groups = batches(items, args)
            for group in groups:
                validate_resources(resource_names(catalog.suites[group[0].case["suite"]], group), args)
            report["resources"] = {name: str(path) for name, path in args.disks.items()}
            report["results"] = execute(args, kernel, catalog, groups, directory)
            report["exit_code"] = exit_status(report["results"])
        require(hash_file(kernel) == digest, "fixed ELF content changed during discovery/execution")
    except (HostError, OSError, RuntimeError, ValueError, subprocess.TimeoutExpired, KeyboardInterrupt) as error:
        report["error"] = str(error) or "host run interrupted"
        report["exit_code"] = 2
    export_reports(directory, report)
    print(f"KTEST report={directory} seed={args.seed} exit={report['exit_code']}", file=sys.stderr)
    return report["exit_code"]


def interrupted(signum, frame):
    """将终止信号交给统一异常回收路径，不遗留 VM 子进程。"""
    CANCELLED.set()
    raise KeyboardInterrupt()


if __name__ == "__main__":
    signal.signal(signal.SIGTERM, interrupted)
    sys.exit(main())
