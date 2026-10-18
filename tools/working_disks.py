#!/usr/bin/env python3
"""独占准备与清理生产 QEMU 的系统、比赛工作副本。"""

import argparse
from contextlib import contextmanager
import fcntl
import os
from pathlib import Path
import shutil
import stat
import tempfile

ROOT = Path(__file__).resolve().parents[1]


def default_working_disk_directory(architecture):
    """生产启动和清理共用每架构唯一的默认工作目录。"""
    return ROOT / "build" / f"qemu-{architecture}"


def working_disk_paths(working_disk_directory, include_contest):
    """仅计算所选角色的路径，不读取目录或来源状态。"""
    directory = Path(working_disk_directory)
    paths = {"system": directory / "root.raw"}
    if include_contest:
        paths["contest"] = directory / "contest.raw"
    return paths


@contextmanager
def _working_disk_lock(directory, create):
    """准备和清理共用非阻塞独占锁，关闭句柄前保留原锁 inode。"""
    if create:
        directory.mkdir(parents=True, exist_ok=True)
    try:
        mode = directory.lstat().st_mode
    except FileNotFoundError:
        if create:
            raise
        yield None
        return
    if not stat.S_ISDIR(mode):
        raise ValueError(f"working disk directory is not an ordinary directory: {directory}")
    descriptor = os.open(directory / ".lock", os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW, 0o600)
    with os.fdopen(descriptor, "r+b") as lock_file:
        status = os.fstat(lock_file.fileno())
        if not stat.S_ISREG(status.st_mode) or status.st_nlink != 1:
            raise ValueError(f"working disk lock is not an independent ordinary file: {directory / '.lock'}")
        fcntl.flock(lock_file, fcntl.LOCK_EX | fcntl.LOCK_NB)
        yield lock_file


def _check_working_disk(path, required):
    """工作盘必须独占普通文件，缺盘只在 fresh 或清理时允许。"""
    try:
        status = path.lstat()
    except FileNotFoundError:
        if required:
            raise ValueError(f"working disk missing: {path}") from None
        return False
    if not stat.S_ISREG(status.st_mode):
        raise ValueError(f"working disk is not an ordinary file: {path}")
    if status.st_nlink != 1:
        raise ValueError(f"working disk aliases another file: {path}")
    return True


@contextmanager
def prepare_working_disks(working_disk_directory, system_image_path,
                          contest_image_path, working_disk_action):
    """在持锁期间准备所选副本；调用者必须让锁覆盖整个 QEMU 使用期。"""
    if working_disk_action not in ("fresh", "reuse", "refresh-system"):
        raise ValueError(f"unsupported working disk action: {working_disk_action}")
    directory = Path(working_disk_directory)
    paths = working_disk_paths(directory, contest_image_path is not None)
    sources = {"system": Path(system_image_path)}
    if contest_image_path is not None:
        sources["contest"] = Path(contest_image_path)
    with _working_disk_lock(directory, create=True) as lock_file:
        existing = {role: _check_working_disk(path, working_disk_action != "fresh")
                    for role, path in paths.items()}
        # 未消费来源仅比较词法路径；reuse 不访问来源的目录项或内容。
        for source in sources.values():
            if any(os.path.abspath(source) == os.path.abspath(path) for path in paths.values()):
                raise ValueError(f"source image names a working disk: {source}")
        copied_roles = (list(paths) if working_disk_action == "fresh" else
                        ["system"] if working_disk_action == "refresh-system" else [])
        for role in copied_roles:
            source = sources[role]
            if not stat.S_ISREG(source.stat().st_mode):
                raise ValueError(f"source image is not a regular file: {source}")
            if any(existing[name] and path.samefile(source) for name, path in paths.items()):
                raise ValueError(f"source image aliases a working disk: {source}")
        if copied_roles:
            # 所有副本写完后才逐盘替换；多盘替换不提供事务语义。
            with tempfile.TemporaryDirectory(prefix="fresh-", dir=directory) as temporary:
                prepared = []
                for role in copied_roles:
                    copy = Path(temporary) / paths[role].name
                    with sources[role].open("rb") as source, copy.open("xb") as target:
                        shutil.copyfileobj(source, target, 1024 * 1024)
                    prepared.append((copy, paths[role]))
                for copy, target in prepared:
                    os.replace(copy, target)
        yield paths, lock_file


def clean_working_disks(working_disk_directory):
    """同锁检查后只删正式两角色文件，保留锁、目录与其他实验盘。"""
    directory = Path(working_disk_directory)
    with _working_disk_lock(directory, create=False) as lock_file:
        if lock_file is None:
            return
        targets = [path for path in working_disk_paths(directory, True).values()
                   if _check_working_disk(path, required=False)]
        for target in targets:
            target.unlink()


def main(argv=None):
    """清理目录按项目根作词法绝对化，与启动使用同一工作盘路径。"""
    parser = argparse.ArgumentParser(description=__doc__, allow_abbrev=False)
    commands = parser.add_subparsers(dest="command", required=True)
    clean = commands.add_parser("clean", allow_abbrev=False)
    clean.add_argument("--arch", choices=("riscv64", "loongarch64"), required=True)
    clean.add_argument("--working-disk-dir", type=Path)
    args = parser.parse_args(argv)
    directory = args.working_disk_dir
    if directory is None:
        directory = default_working_disk_directory(args.arch)
    directory = Path(os.path.abspath(ROOT / directory))
    try:
        clean_working_disks(directory)
    except (ValueError, OSError) as error:
        parser.exit(1, f"working-disks: {error}\n")


if __name__ == "__main__":
    main()
