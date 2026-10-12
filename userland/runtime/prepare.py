#!/usr/bin/env python3
"""从明确的本地 libc 来源准备两族运行库，不修改原程序或解释器字段。"""

import argparse
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile


SOURCES = {
    "riscv64": (
        "/opt/riscv64--musl--bleeding-edge-2020.08-1/"
        "riscv64-buildroot-linux-musl/sysroot/lib/libc.so",
        "/usr/riscv64-linux-gnu/lib",
        "RISC-V",
        "ld-musl-riscv64-sf.so.1",
        "ld-linux-riscv64-lp64d.so.1",
    ),
    "loongarch64": (
        "/opt/loongarch64-linux-musl-cross/loongarch64-linux-musl/lib/libc.so",
        "/opt/gcc-13.2.0-loongarch64-linux-gnu/sysroot/usr/lib64",
        "LoongArch",
        "ld-musl-loongarch-lp64d.so.1",
        "ld-linux-loongarch-lp64d.so.1",
    ),
}


def check_elf(source, machine):
    """保留 readelf 的原始输出，并要求来源是本架构的 ET_DYN 运行库。"""
    print(f"Checking ELF source: {source}", flush=True)
    result = subprocess.run(
        ["readelf", "-h", "-l", str(source)],
        stdout=subprocess.PIPE,
        env={**os.environ, "LC_ALL": "C"},
        check=False,
    )
    sys.stdout.buffer.write(result.stdout)
    sys.stdout.buffer.flush()
    result.check_returncode()
    header = result.stdout.decode("utf-8", errors="replace")
    kind = re.search(r"^\s*Type:\s+(\S+)", header, re.MULTILINE)
    actual_machine = re.search(r"^\s*Machine:\s+(.+)$", header, re.MULTILINE)
    if kind is None or kind.group(1) != "DYN":
        raise ValueError(f"expected ET_DYN runtime library: {source}")
    if actual_machine is None or actual_machine.group(1).strip() != machine:
        raise ValueError(f"expected {machine} runtime library: {source}")


def copy_library(source, destination):
    """在目标目录内复制并替换单个库文件，不跟随已有目标链接写入来源。"""
    with tempfile.NamedTemporaryFile(
        dir=destination.parent, prefix=f".{destination.name}.", delete=False
    ) as stream:
        temporary = Path(stream.name)
    try:
        shutil.copy2(source, temporary)
        temporary.replace(destination)
    finally:
        temporary.unlink(missing_ok=True)
    print(f"Copied: {source} -> {destination}")


def install_link(destination, target):
    """只替换具名文件或链接；已有目录保留并让调用失败。"""
    if destination.is_symlink() or destination.is_file():
        destination.unlink()
    destination.symlink_to(target)
    print(f"Linked: {destination} -> {target}")


def prepare(arch, musl_libc, glibc_libdir, output):
    """先核齐本次全部来源，再更新输出树内固定的四个库文件及路径链接。"""
    _, _, machine, musl_loader, glibc_loader = SOURCES[arch]
    output = output.absolute()
    libraries = [
        (musl_libc, output / "musl/lib/libc.so"),
        (glibc_libdir / "libc.so.6", output / "glibc/lib/libc.so.6"),
        (glibc_libdir / "libm.so.6", output / "glibc/lib/libm.so.6"),
        (glibc_libdir / glibc_loader, output / "glibc/lib" / glibc_loader),
    ]
    resolved = []
    for source, destination in libraries:
        source = source.resolve(strict=True)
        if not source.is_file():
            raise ValueError(f"required source is not a regular file: {source}")
        if source == destination.resolve():
            raise ValueError(f"source and output must be separate: {source}")
        resolved.append((source, destination))
    for source, _ in resolved:
        check_elf(source, machine)
    for source, destination in resolved:
        destination.parent.mkdir(parents=True, exist_ok=True)
        copy_library(source, destination)
    install_link(output / "musl/lib" / musl_loader, "libc.so")
    if arch == "loongarch64":
        install_link(output / "musl/lib64", "lib")
        install_link(output / "glibc/lib64", "lib")
    print(f"Runtime assets prepared: arch={arch} output={output}")


def main(argv=None):
    """解析架构和明确来源；缺库、工具、复制或链接失败都以非零状态结束。"""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--arch", choices=SOURCES, required=True)
    parser.add_argument("--musl-libc", type=Path)
    parser.add_argument("--glibc-libdir", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args(argv)
    default_musl, default_glibc, _, _, _ = SOURCES[args.arch]
    try:
        prepare(
            args.arch,
            args.musl_libc if args.musl_libc is not None else Path(default_musl),
            args.glibc_libdir if args.glibc_libdir is not None else Path(default_glibc),
            args.output,
        )
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        print(f"Runtime preparation failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
