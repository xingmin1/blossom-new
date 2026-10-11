#!/usr/bin/env python3
"""用独占工作盘启动普通内核，源镜像只读取，保留盘必须显式 --resume。"""

import argparse
import fcntl
import os
from pathlib import Path
import shutil
import stat
import tempfile

from storage import disk_arguments

ROOT = Path(__file__).resolve().parents[1]


def regular_file(path):
    """输入与恢复盘必须是普通文件，避免把设备或 FIFO 交给复制和 QEMU。"""
    if not stat.S_ISREG(path.stat().st_mode):
        raise ValueError(f"not a regular file: {path}")


def working_disks(sources, directory, resume):
    """鲜运行原子换入独立副本；恢复只用已有工作盘，不重新读取源内容。"""
    disks = [(name, directory / f"{name}.raw") for name, _ in sources]
    for _, source in sources:
        regular_file(source)
    for _, target in disks:
        if target.is_symlink():
            raise ValueError(f"working disk is a symbolic link: {target}")
        if target.exists():
            regular_file(target)
            if target.stat().st_nlink != 1 or any(target.samefile(source) for _, source in sources):
                raise ValueError(f"working disk aliases another file: {target}")
        elif resume:
            raise ValueError(f"resume disk missing: {target}")
    if not resume:
        with tempfile.TemporaryDirectory(prefix="fresh-", dir=directory) as temporary:
            prepared = []
            for (name, source), (_, target) in zip(sources, disks):
                copy = Path(temporary) / f"{name}.raw"
                with source.open("rb") as input_stream, copy.open("xb") as output_stream:
                    shutil.copyfileobj(input_stream, output_stream, 1024 * 1024)
                prepared.append((copy, target))
            for copy, target in prepared:
                os.replace(copy, target)
    return disks


def qemu_arguments(args, disks):
    """普通 stdio 运行传递显式 init 并保留真实 restart，不启动测试协议或后台控制进程。"""
    command = [args.qemu or f"qemu-system-{args.arch}", "-machine", "virt", "-accel", "tcg",
               "-kernel", str(args.kernel), "-m", args.memory, "-smp", str(args.cpus),
               "-display", "none", "-monitor", "none", "-serial", "stdio", "-nic", "none"]
    if args.arch == "riscv64":
        command += ["-bios", "default"]
        if args.modern:
            command += ["-global", "virtio-mmio.force-legacy=false"]
    if args.debug:
        command += ["-S", "-gdb", "tcp:127.0.0.1:1234"]
    return command + disk_arguments(disks, args, set(), init=args.init)


def main(argv=None):
    """核启动输入并锁住工作目录，再复制或恢复盘，最后以 QEMU 替换宿主进程。"""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--arch", choices=("riscv64", "loongarch64"), required=True)
    parser.add_argument("--kernel", type=Path, required=True)
    parser.add_argument("--cpus", type=int, choices=(1, 2, 4), default=1)
    parser.add_argument("--memory")
    parser.add_argument("--image", type=Path, required=True)
    parser.add_argument("--extra-image", type=Path)
    parser.add_argument("--scratch-image", type=Path)
    parser.add_argument("--root", default="/dev/vda", help="explicit guest root device; no fallback")
    parser.add_argument("--root-mode", choices=("ro", "rw"), default="ro", help="guest root mount mode; independent of device read-only")
    parser.add_argument("--extra-root", help="explicit guest extra mount device; absent means no extra mount")
    parser.add_argument("--init", default="/sbin/init", help="explicit guest init path; empty or missing paths fail in the kernel")
    parser.add_argument("--resume", action="store_true")
    parser.add_argument("--modern", action="store_true")
    parser.add_argument("--debug", action="store_true", help="pause with the local GDB endpoint")
    parser.add_argument("--work-dir", type=Path)
    parser.add_argument("--qemu")
    parser.add_argument("--read-only", choices=("root", "extra", "scratch"), action="append", default=[])
    parser.add_argument("--logical-block-size", type=int, choices=(512, 1024, 2048, 4096), default=512)
    args = parser.parse_args(argv)
    if args.modern and args.arch != "riscv64":
        parser.error("--modern is only valid for RISC-V MMIO")
    args.memory = args.memory or ("512M" if args.arch == "riscv64" else "768M")
    directory = args.work_dir or ROOT / "build" / f"run-{args.arch}"
    sources = [("root", args.image)]
    if args.extra_image is not None:
        sources.append(("extra", args.extra_image))
    if args.scratch_image is not None:
        sources.append(("scratch", args.scratch_image))
    try:
        regular_file(args.kernel)
        binary = args.qemu or f"qemu-system-{args.arch}"
        if shutil.which(binary) is None:
            raise ValueError(f"required QEMU missing: {binary}")
        directory.mkdir(parents=True, exist_ok=True)
        with (directory / ".lock").open("a+b") as lock:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
            disks = working_disks(sources, directory, args.resume)
            command = qemu_arguments(args, disks)
            print(f"run work-dir={directory.resolve()} resume={int(args.resume)}", flush=True)
            os.set_inheritable(lock.fileno(), True)
            os.execvp(command[0], command)
    except (ValueError, OSError) as error:
        parser.exit(1, f"run: {error}\n")


if __name__ == "__main__":
    main()
