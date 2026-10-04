#!/usr/bin/env python3
"""给已构建的存储验收 ELF 附真实磁盘工作副本，保存原始输出并清理 QEMU。"""

import argparse
from pathlib import Path
import re
import shutil
import subprocess
import sys
from urllib.parse import quote


def require(condition, message):
    """拒绝缺失、重复或与本次目标不一致的客体证据。"""
    if not condition:
        raise RuntimeError(message)


def aliases(path, target):
    """覆盖符号链接与既有硬链接，阻止日志和工作副本损坏输入。"""
    return (path.resolve() == target.resolve()
            or (path.exists() and target.exists() and path.samefile(target)))


def run(args):
    """原镜像从不交给可写设备；明确工作副本或显式 resume 旧副本。"""
    args.work_dir.mkdir(parents=True, exist_ok=True)
    copies = []
    protected = [args.kernel, *args.disk]
    command_output = args.output.with_suffix(args.output.suffix + ".command")
    require(not any(aliases(path, source)
                    for path in (args.output, command_output) for source in protected),
            "output would overwrite an input")
    require(not aliases(args.output, command_output), "output aliases command log")
    destinations = [args.work_dir / f"disk{index}.raw" for index in range(len(args.disk))]
    require(not any(aliases(path, previous)
                    for index, path in enumerate(destinations)
                    for previous in destinations[:index]),
            "working disks alias each other")
    for source, destination in zip(args.disk, destinations):
        require(not any(aliases(destination, source) for source in protected),
                "working copy would overwrite an input")
        require(not any(aliases(destination, path) for path in (args.output, command_output)),
                "working copy would overwrite output")
        if args.resume:
            require(destination.is_file(), "resume requires every existing working disk")
        else:
            shutil.copyfile(source, destination)
        copies.append(destination)

    command = [f"qemu-system-{args.arch}", "-machine", "virt", "-accel", "tcg",
               "-kernel", str(args.kernel), "-m", args.memory, "-smp", str(args.cpus),
               "-display", "none", "-monitor", "none", "-serial", "stdio",
               "-no-reboot", "-nic", "none", "-append", f"root={args.root}"]
    if args.arch == "riscv64":
        command += ["-bios", "default"]
        if args.modern:
            command += ["-global", "virtio-mmio.force-legacy=false"]
    for index, image in enumerate(copies):
        file_option = str(image).replace(",", ",,")
        drive = f"if=none,id=disk{index},format=raw,file={file_option}"
        if args.read_only:
            drive += ",readonly=on"
        command += ["-drive", drive]
        if args.arch == "riscv64":
            device = f"virtio-blk-device,drive=disk{index},bus=virtio-mmio-bus.{index}"
        else:
            device = f"virtio-blk-pci,drive=disk{index},addr=0x{5 + 4 * index:x}"
        device += (f",logical_block_size={args.logical_block_size}"
                   f",physical_block_size={args.logical_block_size}")
        command += ["-device", device]

    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.with_suffix(args.output.suffix + ".command").write_text(
        repr(command) + "\n", encoding="utf-8")
    try:
        completed = subprocess.run(command, input=b"", stdout=subprocess.PIPE,
                                   stderr=subprocess.STDOUT, timeout=args.timeout,
                                   check=False)
    except subprocess.TimeoutExpired as error:
        args.output.write_bytes(error.stdout or b"")
        raise
    args.output.write_bytes(completed.stdout)
    sys.stdout.buffer.write(completed.stdout)
    sys.stdout.buffer.flush()
    require(completed.returncode == 0,
            f"QEMU exited with status {completed.returncode}")
    text = completed.stdout.decode("utf-8", errors="replace")
    require(not re.search(r"\b(?:FAIL|FATAL|panic|panicked)\b", text, re.IGNORECASE),
            "guest reported a failure")
    lines = text.splitlines()
    ready = (f"STORAGE READY arch={args.arch} cpus={args.cpus} "
             f"suite=block mode={args.mode}")
    final = (f"STORAGE BLOCK PASS devices={len(copies)} "
             "scope=real-io-with-controlled-ledger")
    require(lines.count(ready) == 1 and lines.count(final) == 1,
            "missing, duplicate, or mismatched storage markers")
    for index in range(len(copies)):
        for prefix in ("PASS block-entry", "PASS B04 block", "PASS B05 block",
                       "PASS B08 block", "PASS B09 block", "PASS B09 block-tail",
                       "PASS B10 block", "PASS B11 block-fault",
                       "PASS block-finalizer", "PASS block-suite"):
            records = [line for line in lines if line.startswith(f"{prefix} index={index} ")]
            require(len(records) == 1, f"missing or duplicate {prefix} for disk {index}")
            fields = dict(field.split("=", 1) for field in records[0].split() if "=" in field)
            if prefix == "PASS B04 block":
                require(int(fields["peak"]) > 1 and fields["tasks"] == "8"
                        and fields["real-data"] == "1", "missing actual multi-task IO")
            if prefix == "PASS B10 block":
                require(fields["write"] == str(int(not args.read_only))
                        and fields["read-back"] == str(int(not args.read_only))
                        and fields["mount-protected"] == "1", "scratch/mount evidence mismatch")
            if prefix == "PASS block-suite":
                require(fields["reclaimed"] == "1", "storage memory did not return")
        for quota in ("tag", "descriptor"):
            prefix = f"PASS B07 block index={index} quota={quota} "
            records = [line for line in lines if line.startswith(prefix)]
            require(len(records) == 1 and "real-blocked=1 wake=1 kicked=1" in records[0],
                    f"missing actual {quota} backpressure")
        backend = [line for line in lines if line.startswith(f"STORAGE BACKEND index={index} ")]
        require(len(backend) == 1 and f"logical-block={args.logical_block_size} " in backend[0],
                "negotiated logical block does not match the real device")


def main():
    """指定一个真实 ELF 与一至两盘，不构建、不下载、不隐式修改输入盘。"""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--arch", required=True, choices=("riscv64", "loongarch64"))
    parser.add_argument("--cpus", required=True, type=int, choices=(1, 2, 4))
    parser.add_argument("--memory", default="1G")
    parser.add_argument("--mode", choices=("debug", "release"), default="debug")
    parser.add_argument("--kernel", required=True, type=Path)
    parser.add_argument("--disk", required=True, type=Path, action="append")
    parser.add_argument("--root", choices=("/dev/vda", "/dev/vdb"), default="/dev/vda")
    parser.add_argument("--read-only", action="store_true")
    parser.add_argument("--logical-block-size", type=int, choices=(512, 1024, 4096), default=512)
    parser.add_argument("--modern", action="store_true")
    parser.add_argument("--resume", action="store_true")
    parser.add_argument("--timeout", type=float, default=120)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--work-dir", type=Path)
    args = parser.parse_args()
    if len(args.disk) not in (1, 2):
        parser.error("storage gate requires one or two disks")
    if not args.kernel.is_file() or not all(path.is_file() for path in args.disk):
        parser.error("kernel and disks must be existing files")
    if not 0 < args.timeout < float("inf"):
        parser.error("timeout must be finite and positive")
    if args.modern and args.arch != "riscv64":
        parser.error("modern MMIO selection requires riscv64")
    if args.root == "/dev/vdb" and len(args.disk) != 2:
        parser.error("vdb root requires two disks")
    if args.output is None:
        memory = quote(args.memory, safe="")
        args.output = args.kernel.with_name(
            f"{args.kernel.name}.{args.arch}.{args.cpus}.{memory}.{args.mode}.storage.log")
    if args.work_dir is None:
        args.work_dir = args.output.with_suffix(".work")
    try:
        run(args)
    except (OSError, RuntimeError, subprocess.TimeoutExpired) as error:
        print(f"\nHOST FAIL: {error}; raw output: {args.output}", file=sys.stderr)
        return 1
    print(f"HOST PASS: {args.arch} cpus={args.cpus} memory={args.memory} "
          f"mode={args.mode} disks={len(args.disk)}; raw output: {args.output}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
