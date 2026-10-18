#!/usr/bin/env python3
"""从已安装用户程序、运行库和普通系统资产重建单个架构的根目录。"""

import argparse
import os
from pathlib import Path
import shutil
import sys
import tempfile
from rootfs_paths import output_path, reject_overlap


REPOSITORY_ROOT = Path(__file__).resolve().parents[1]


def copy_rootfs_entries(source_directory, destination_directory):
    """合并只读安装源，替换目标链接后才复制，避免覆盖操作沿链接写回来源。"""
    if destination_directory.is_symlink() or destination_directory.is_file():
        destination_directory.unlink()
    destination_directory.mkdir(parents=True, exist_ok=True)
    for source in source_directory.iterdir():
        destination = destination_directory / source.name
        if source.is_dir() and not source.is_symlink():
            copy_rootfs_entries(source, destination)
        else:
            if destination.is_dir() and not destination.is_symlink():
                shutil.rmtree(destination)
            else:
                destination.unlink(missing_ok=True)
            if source.is_symlink():
                destination.symlink_to(os.readlink(source))
            else:
                shutil.copy2(source, destination)
    shutil.copystat(source_directory, destination_directory)


def assemble_rootfs_tree(architecture, guest_userland_directory,
                         runtime_libraries_directory, output_directory,
                         init_config_path=None):
    """在临时树完成安装后发布项目拥有的根树；重复调用完整替换，所有输入保持只读。"""
    if architecture not in ("riscv64", "loongarch64"):
        raise ValueError(f"unsupported architecture: {architecture}")
    userland = Path(guest_userland_directory).resolve(strict=True)
    runtime = Path(runtime_libraries_directory).resolve(strict=True)
    system = REPOSITORY_ROOT / "userland/system"
    contest = REPOSITORY_ROOT / "userland/contest"
    output = output_path(output_directory, directory=True)
    init_config = Path(init_config_path).resolve(strict=True) if init_config_path is not None else None
    sources = (userland, runtime, system.resolve(), contest.resolve())
    for source in sources:
        if not source.is_dir():
            raise ValueError(f"required source is not a directory: {source}")
        reject_overlap(source, output)
    if init_config is not None:
        if not init_config.is_file():
            raise ValueError(f"init config is not a regular file: {init_config}")
        reject_overlap(init_config, output)
    for name in ("bin/busybox", "bin/sh", "sbin/init", "sbin/contest-run", "bin/libc-smoke"):
        if not (userland / name).is_file():
            raise ValueError(f"required installed guest program missing: {userland / name}")
    for name in ("musl/lib/libc.so", "glibc/lib/libc.so.6", "glibc/lib/libm.so.6"):
        if not (runtime / name).is_file():
            raise ValueError(f"required runtime library missing: {runtime / name}")
    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="rootfs-", dir=output.parent) as temporary:
        tree = Path(temporary) / "root"
        copy_rootfs_entries(userland, tree)
        copy_rootfs_entries(system / "etc", tree / "etc")
        copy_rootfs_entries(system / "sbin", tree / "sbin")
        for family in ("musl", "glibc"):
            if (tree / "opt").is_symlink() or (tree / "opt/runtime").is_symlink():
                raise ValueError("installed runtime parent must be a directory, not a symbolic link")
            copy_rootfs_entries(runtime / family, tree / "opt/runtime" / family)
        copy_rootfs_entries(contest, tree / "etc/contest")
        if init_config is not None:
            (tree / "etc/inittab").unlink(missing_ok=True)
            shutil.copy2(init_config, tree / "etc/inittab")
        for name in ("dev", "proc", "run", "tmp", "var", "var/tmp", "root", "home",
                     "usr", "usr/bin", "usr/sbin", "mnt", "mnt/extra", "mnt/contest"):
            destination = tree / name
            if destination.is_symlink():
                raise ValueError(f"root filesystem directory must not be a symbolic link: {name}")
            destination.mkdir(parents=True, exist_ok=True)
        tree.chmod(0o755)
        (tree / "tmp").chmod(0o1777)
        (tree / "var/tmp").chmod(0o1777)
        previous = Path(temporary) / "previous"
        if output.exists():
            output.rename(previous)
        try:
            tree.rename(output)
        except OSError:
            if previous.exists():
                previous.rename(output)
            raise
    print(f"Root filesystem assembled: arch={architecture} output={output}")
    return output


def main(argv=None):
    """只解析明确安装输入，不编译程序、准备运行库或读取比赛磁盘。"""
    parser = argparse.ArgumentParser(description=__doc__, allow_abbrev=False)
    parser.add_argument("--arch", choices=("riscv64", "loongarch64"), required=True)
    parser.add_argument("--guest-userland-directory", type=Path, required=True)
    parser.add_argument("--runtime-libraries-directory", type=Path, required=True)
    parser.add_argument("--output-directory", type=Path, required=True)
    parser.add_argument("--init-config", type=Path)
    args = parser.parse_args(argv)
    try:
        assemble_rootfs_tree(args.arch, args.guest_userland_directory,
                             args.runtime_libraries_directory, args.output_directory,
                             args.init_config)
    except (OSError, ValueError) as error:
        print(f"Root filesystem assembly failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
