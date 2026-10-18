#!/usr/bin/env python3
"""用明确的 ELF、盘角色与独占工作副本启动生产 QEMU。"""

import sys

# help 和命令展示不生成导入缓存。
sys.dont_write_bytecode = True

import argparse
import os
from pathlib import Path
import shlex
import shutil
import stat

from kernel_elf import read_kernel_elf
from qemu_command import build_qemu_command
from working_disks import (default_working_disk_directory, prepare_working_disks,
                           working_disk_paths)

ROOT = Path(__file__).resolve().parents[1]
DEFAULT_CPU_COUNT = 1
DEFAULT_MEMORY_SIZE = "1G"
DEFAULT_NETWORK_MODE = "user"
DEFAULT_GDB_PORT = 1234


def repository_path(value):
    """词法解释仓库相对路径，不读取未消费来源或解引用符号链接。"""
    return Path(os.path.abspath(ROOT / value))


def parse_arguments(argv):
    """仅在入口定义默认值并拒绝互斥选择，后端只接收明确字段。"""
    parser = argparse.ArgumentParser(description=__doc__, allow_abbrev=False)
    parser.add_argument("--arch", required=True, choices=("riscv64", "loongarch64"))
    parser.add_argument("--kernel-elf", type=repository_path)
    parser.add_argument("--system-image", type=repository_path)
    parser.add_argument("--contest-image", type=repository_path)
    parser.add_argument("--working-disk-dir", type=repository_path)
    parser.add_argument("--working-disk-action", choices=("fresh", "reuse", "refresh-system"),
                        default="fresh")
    session = parser.add_mutually_exclusive_group()
    session.add_argument("--shell", action="store_true")
    session.add_argument("--init-program")
    parser.add_argument("--system-only", action="store_true")
    parser.add_argument("--debug", action="store_true")
    parser.add_argument("--gdb-port", type=int)
    parser.add_argument("--cpus", type=int, default=DEFAULT_CPU_COUNT)
    parser.add_argument("--memory", default=DEFAULT_MEMORY_SIZE)
    parser.add_argument("--network", choices=("user", "none"), default=DEFAULT_NETWORK_MODE)
    parser.add_argument("--host-forward")
    parser.add_argument("--root-mode", choices=("ro", "rw"))
    parser.add_argument("--qemu-binary", type=repository_path)
    parser.add_argument("--print-command", action="store_true")
    args = parser.parse_args(argv)
    if args.system_only and not (args.shell or args.init_program is not None):
        parser.error("--system-only requires --shell or --init-program")
    if args.host_forward is not None and args.network == "none":
        parser.error("--host-forward requires --network user")
    if args.gdb_port is not None:
        if not args.debug:
            parser.error("--gdb-port requires --debug")
        if not 1 <= args.gdb_port <= 65535:
            parser.error("--gdb-port must be in 1..65535")
    if args.cpus < 1:
        parser.error("--cpus must be a positive integer")
    if args.init_program is not None and (not args.init_program or any(
            character in " \t\n\r\v\f\0" for character in args.init_program)):
        parser.error("--init-program must be nonempty and contain no ASCII whitespace or NUL")
    return parser, args


def boot_arguments(system_only, root_mode, shell, init_program):
    """只把显式覆盖写入客体命令行，普通双盘运行不增加 append。"""
    arguments = []
    if system_only:
        arguments.append("root=/dev/vda")
    if root_mode is not None:
        arguments.append(root_mode)
    if shell:
        arguments.append("blossom.boot=shell")
    if init_program is not None:
        arguments.append(f"init={init_program}")
    return " ".join(arguments) if arguments else None


def print_summary(kernel_elf_path, system_image_path, contest_image_path,
                  disk_paths, working_disk_directory, working_disk_action,
                  debug_enabled, gdb_port):
    """展示绝对路径与本次动作的来源消费责任，不读取来源摘要。"""
    print(f"run-qemu kernel-elf={kernel_elf_path}", file=sys.stderr, flush=True)
    print(f"run-qemu working-disk-dir={working_disk_directory} "
          f"working-disk-action={working_disk_action}", file=sys.stderr, flush=True)
    print(f"run-qemu role=system source={system_image_path} "
          f"source-consumed={str(working_disk_action != 'reuse').lower()} "
          f"working-disk={disk_paths['system']}", file=sys.stderr, flush=True)
    if contest_image_path is not None:
        print(f"run-qemu role=contest source={contest_image_path} "
              f"source-consumed={str(working_disk_action == 'fresh').lower()} "
              f"working-disk={disk_paths['contest']}", file=sys.stderr, flush=True)
    if debug_enabled:
        print(f"run-qemu gdb-endpoint=127.0.0.1:{gdb_port}", file=sys.stderr, flush=True)


def main(argv=None):
    """先纯构造命令；实际运行才核 ELF、准备工作盘并把独占锁交给 exec。"""
    parser, args = parse_arguments(argv)
    suffix = "rv" if args.arch == "riscv64" else "la"
    kernel_elf_path = args.kernel_elf or ROOT / f"kernel-{suffix}"
    system_image_path = args.system_image or ROOT / ("disk.img" if suffix == "rv" else "disk-la.img")
    contest_image_path = None if args.system_only else (
        args.contest_image or ROOT / f"sdcard-{suffix}.img")
    directory = args.working_disk_dir or default_working_disk_directory(args.arch)
    disk_paths = working_disk_paths(directory, contest_image_path is not None)
    qemu_binary = str(args.qemu_binary) if args.qemu_binary is not None else f"qemu-system-{args.arch}"
    gdb_port = DEFAULT_GDB_PORT if args.gdb_port is None else args.gdb_port
    command = build_qemu_command(
        architecture=args.arch, qemu_binary=qemu_binary, kernel_elf_path=kernel_elf_path,
        system_working_disk_path=disk_paths["system"],
        contest_working_disk_path=disk_paths.get("contest"), cpu_count=args.cpus,
        memory_size=args.memory, network_mode=args.network, host_forward_rule=args.host_forward,
        boot_arguments=boot_arguments(args.system_only, args.root_mode, args.shell, args.init_program),
        debug_enabled=args.debug, gdb_port=gdb_port)
    if args.print_command:
        print_summary(kernel_elf_path, system_image_path, contest_image_path, disk_paths,
                      directory, args.working_disk_action, args.debug, gdb_port)
        print(shlex.join(command))
        return
    try:
        if not stat.S_ISREG(kernel_elf_path.stat().st_mode):
            raise ValueError(f"kernel ELF is not a regular file: {kernel_elf_path}")
        read_kernel_elf(kernel_elf_path, args.arch)
        executable = shutil.which(qemu_binary)
        if executable is None:
            raise ValueError(f"required QEMU executable missing: {qemu_binary}")
        command[0] = os.path.abspath(executable)
        with prepare_working_disks(directory, system_image_path, contest_image_path,
                                   args.working_disk_action) as (prepared_paths, lock_file):
            print_summary(kernel_elf_path, system_image_path, contest_image_path, prepared_paths,
                          directory, args.working_disk_action, args.debug, gdb_port)
            print(f"run-qemu qemu-binary={command[0]}", file=sys.stderr, flush=True)
            os.set_inheritable(lock_file.fileno(), True)
            environment = os.environ.copy()
            environment.pop("LD_LIBRARY_PATH", None)
            environment.pop("LD_PRELOAD", None)
            os.execve(command[0], command, environment)
    except (ValueError, OSError) as error:
        parser.exit(1, f"run-qemu: {error}\n")


if __name__ == "__main__":
    main()
