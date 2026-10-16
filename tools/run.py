#!/usr/bin/env python3
"""用独占工作盘启动普通内核，源镜像只读取，保留盘必须显式 --resume。"""

import argparse
import fcntl
import os
from pathlib import Path
import shutil
import stat
import tempfile

from storage import disk_arguments, network_arguments

ROOT = Path(__file__).resolve().parents[1]


def regular_file(path):
    """输入与恢复盘必须是普通文件，避免把设备或 FIFO 交给复制和 QEMU。"""
    if not stat.S_ISREG(path.stat().st_mode):
        raise ValueError(f"not a regular file: {path}")


def working_disks(sources, directory, resume, refresh_system=False):
    """持工作锁时制作副本；恢复只在明确刷新时原子替换系统盘。"""
    if refresh_system and not resume:
        raise ValueError("system refresh requires resume")
    if refresh_system and not any(name == "root" for name, _ in sources):
        raise ValueError("system refresh requires a root disk")
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
    copies = [(source, target) for (name, source), (_, target) in zip(sources, disks)
              if not resume or (refresh_system and name == "root")]
    if copies:
        with tempfile.TemporaryDirectory(prefix="fresh-", dir=directory) as temporary:
            prepared = []
            for source, target in copies:
                copy = Path(temporary) / target.name
                with source.open("rb") as input_stream, copy.open("xb") as output_stream:
                    shutil.copyfileobj(input_stream, output_stream, 1024 * 1024)
                prepared.append((copy, target))
            for copy, target in prepared:
                os.replace(copy, target)
    return disks


def clean_work_directory(directory):
    """沿启动的同一锁删除明确的工作盘；保留锁 inode，避免并发清理绕过互斥。"""
    if not directory.exists():
        return
    if directory.is_symlink() or not directory.is_dir():
        raise ValueError("work directory must be an ordinary directory")
    with (directory / ".lock").open("a+b") as lock:
        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        targets = [directory / f"{name}.raw" for name in ("root", "contest", "extra", "scratch")]
        for target in targets:
            if target.is_symlink():
                raise ValueError(f"working disk is a symbolic link: {target}")
            if target.exists():
                regular_file(target)
                if target.stat().st_nlink != 1:
                    raise ValueError(f"working disk aliases another file: {target}")
        for target in targets:
            if target.exists():
                target.unlink()


def qemu_arguments(args, disks):
    """普通 stdio 运行保留明确的根视图、init、网络与真实 restart，不启用测试协议。"""
    command = [args.qemu or f"qemu-system-{args.arch}", "-machine", "virt", "-accel", "tcg",
               "-kernel", str(args.kernel), "-m", args.memory, "-smp", str(args.cpus),
               "-display", "none", "-monitor", "none", "-serial", "stdio"]
    if args.arch == "riscv64":
        command += ["-bios", "default"]
        if args.modern:
            command += ["-global", "virtio-mmio.force-legacy=false"]
    if args.debug:
        command += ["-S", "-gdb", "tcp:127.0.0.1:1234"]
    return (command + network_arguments(args.arch, args.net, args.net_forward)
            + disk_arguments(disks, args, set(), init=args.init, rootdir=args.rootdir))


def main(argv=None):
    """核启动输入并锁住工作目录；工作清理也取得同一非阻塞锁。"""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--arch", choices=("riscv64", "loongarch64"))
    parser.add_argument("--kernel", type=Path)
    parser.add_argument("--cpus", type=int, choices=(1, 2, 4), default=1)
    parser.add_argument("--memory")
    parser.add_argument("--image", type=Path)
    parser.add_argument("--contest-image", type=Path, help="original contest image; attach its writable copy before the system image")
    parser.add_argument("--extra-image", type=Path)
    parser.add_argument("--scratch-image", type=Path)
    parser.add_argument("--root", help="explicit guest root device; defaults to vda normally and vdb with a contest image")
    parser.add_argument("--rootdir", default="/", help="explicit guest root directory on the system volume")
    parser.add_argument("--root-mode", choices=("ro", "rw"), default="ro", help="guest root mount mode; independent of device read-only")
    parser.add_argument("--extra-root", help="explicit guest extra mount device; absent means no extra mount")
    parser.add_argument("--init", default="/sbin/init", help="explicit guest init path; empty or missing paths fail in the kernel")
    parser.add_argument("--net", choices=("none", "user"), default="none")
    parser.add_argument("--net-forward", default="", help="explicit QEMU user-network hostfwd rule")
    parser.add_argument("--resume", action="store_true")
    parser.add_argument("--refresh-system", action="store_true", help="with resume, replace only root.raw from the current system image")
    parser.add_argument("--work-clean", action="store_true", help="remove known working disks under their existing lock")
    parser.add_argument("--modern", action="store_true")
    parser.add_argument("--debug", action="store_true", help="pause with the local GDB endpoint")
    parser.add_argument("--work-dir", type=Path)
    parser.add_argument("--qemu")
    parser.add_argument("--read-only", choices=("root", "contest", "extra", "scratch"), action="append", default=[])
    parser.add_argument("--logical-block-size", type=int, choices=(512, 1024, 2048, 4096), default=512)
    args = parser.parse_args(argv)
    if args.work_clean:
        if args.work_dir is None:
            parser.error("--work-clean requires --work-dir")
        if args.resume or args.refresh_system:
            parser.error("--work-clean cannot resume or refresh disks")
        try:
            clean_work_directory(args.work_dir)
        except (ValueError, OSError) as error:
            parser.exit(1, f"run: {error}\n")
        return
    if args.arch is None or args.kernel is None or args.image is None:
        parser.error("--arch, --kernel and --image are required to run")
    if args.refresh_system and not args.resume:
        parser.error("--refresh-system requires --resume")
    if args.modern and args.arch != "riscv64":
        parser.error("--modern is only valid for RISC-V MMIO")
    if args.contest_image is not None:
        if args.root not in (None, "/dev/vdb"):
            parser.error("--contest-image requires the system root /dev/vdb")
        if "contest" in args.read_only:
            parser.error("the contest working disk must be writable")
    args.root = args.root if args.root is not None else ("/dev/vdb" if args.contest_image is not None else "/dev/vda")
    args.memory = args.memory or ("512M" if args.arch == "riscv64" else "768M")
    role = "contest-run" if args.contest_image is not None else "run"
    directory = args.work_dir or ROOT / "build" / f"{role}-{args.arch}"
    sources = []
    if args.contest_image is not None:
        sources.append(("contest", args.contest_image))
    sources.append(("root", args.image))
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
            disks = working_disks(sources, directory, args.resume, args.refresh_system)
            command = qemu_arguments(args, disks)
            print(f"run work-dir={directory.resolve()} resume={int(args.resume)} refresh-system={int(args.refresh_system)}", flush=True)
            os.set_inheritable(lock.fileno(), True)
            os.execvp(command[0], command)
    except (ValueError, OSError) as error:
        parser.exit(1, f"run: {error}\n")


if __name__ == "__main__":
    main()
