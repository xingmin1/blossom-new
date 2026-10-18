#!/usr/bin/env python3
"""用明确的 ELF、盘角色与独占工作副本启动生产 QEMU。"""

import sys

# help 和命令展示不生成导入缓存。
sys.dont_write_bytecode = True

import argparse
from contextlib import contextmanager
from dataclasses import dataclass
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


def parse_arguments(argv, parser=None):
    """共用生产参数与默认值；自动调用者可在同一解析器增加自己的输入。"""
    if parser is None:
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


@dataclass(frozen=True)
class QemuConfiguration:
    """保存纯解析得到的路径和命令，尚未读取 ELF、来源盘或工作目录。"""

    architecture: str
    kernel_elf_path: Path
    system_image_path: Path
    contest_image_path: Path | None
    working_disk_directory: Path
    working_disk_action: str
    debug_enabled: bool
    gdb_port: int
    command: tuple


def configure_qemu(args):
    """只计算默认路径与完整 argv，展示和两种执行生命周期共用这一来源。"""
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
    return QemuConfiguration(
        args.arch, kernel_elf_path, system_image_path, contest_image_path, directory,
        args.working_disk_action, args.debug, gdb_port, tuple(command))


def summarize_configuration(configuration, disk_paths):
    """展示实际配置和所选工作盘，保持未消费来源的只读约定。"""
    print_summary(
        configuration.kernel_elf_path, configuration.system_image_path,
        configuration.contest_image_path, disk_paths, configuration.working_disk_directory,
        configuration.working_disk_action, configuration.debug_enabled, configuration.gdb_port)


@contextmanager
def prepare_qemu(configuration):
    """核对 ELF 与 QEMU 后准备工作盘；锁覆盖调用者的执行、等待和盘面读取。"""
    kernel = configuration.kernel_elf_path
    if not stat.S_ISREG(kernel.stat().st_mode):
        raise ValueError(f"kernel ELF is not a regular file: {kernel}")
    read_kernel_elf(kernel, configuration.architecture)
    command = list(configuration.command)
    executable = shutil.which(command[0])
    if executable is None:
        raise ValueError(f"required QEMU executable missing: {command[0]}")
    command[0] = os.path.abspath(executable)
    environment = os.environ.copy()
    environment.pop("LD_LIBRARY_PATH", None)
    environment.pop("LD_PRELOAD", None)
    with prepare_working_disks(
            configuration.working_disk_directory, configuration.system_image_path,
            configuration.contest_image_path, configuration.working_disk_action
    ) as (disk_paths, lock_file):
        summarize_configuration(configuration, disk_paths)
        print(f"run-qemu qemu-binary={command[0]}", file=sys.stderr, flush=True)
        yield command, environment, disk_paths, lock_file


def main(argv=None):
    """展示不读取输入；普通启动仍通过 execve 继承同一工作盘锁。"""
    parser, args = parse_arguments(argv)
    configuration = configure_qemu(args)
    if args.print_command:
        disk_paths = working_disk_paths(configuration.working_disk_directory,
                                        configuration.contest_image_path is not None)
        summarize_configuration(configuration, disk_paths)
        print(shlex.join(configuration.command))
        return
    try:
        with prepare_qemu(configuration) as (command, environment, _, lock_file):
            os.set_inheritable(lock_file.fileno(), True)
            os.execve(command[0], command, environment)
    except (ValueError, OSError) as error:
        parser.exit(1, f"run-qemu: {error}\n")


if __name__ == "__main__":
    main()
