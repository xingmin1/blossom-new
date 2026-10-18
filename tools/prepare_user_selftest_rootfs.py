#!/usr/bin/env python3
"""从安装输入准备一次用户 selftests 工作负载，不编译程序或启动 QEMU。"""

import argparse
import os
from pathlib import Path
import re
import shlex
import subprocess
import sys
import tempfile

from assemble_rootfs import ROOT, assemble_rootfs_tree
from create_ext4_image import PROFILES, create_ext4_image


def prepare_user_selftest_rootfs(architecture, guest_userland_directory,
                                 runtime_libraries_directory, output_directory,
                                 output_image, collections=(), programs=(),
                                 profile="seed4k", image_size_mib=256, *,
                                 run_id, program_timeout_seconds=None):
    """重建本轮 PID1 输入并清除旧结果；程序期限默认值只由用户执行器定义。"""
    if re.fullmatch(r"[0-9a-f]{32}", run_id) is None:
        raise ValueError("run ID must contain exactly 32 lowercase hexadecimal digits")
    if program_timeout_seconds is not None and (
            not isinstance(program_timeout_seconds, int) or program_timeout_seconds < 1):
        raise ValueError("program timeout must be a positive integer")
    userland = Path(guest_userland_directory).resolve(strict=True)
    runtime = Path(runtime_libraries_directory).resolve(strict=True)
    output = Path(os.path.abspath(output_directory))
    image = Path(os.path.abspath(output_image))
    sources = (userland, runtime, (ROOT / "userland/system").resolve(),
               (ROOT / "userland/contest").resolve())
    for target in (output, image, image.with_suffix(".log")):
        if any(path.is_symlink() for path in (target, *target.parents)):
            raise ValueError(f"output paths must not contain symbolic links: {target}")
        for source in sources:
            if target == source or target in source.parents or source in target.parents:
                raise ValueError(f"source and output paths overlap: {source}")
    if image == image.with_suffix(".log"):
        raise ValueError("image path must be separate from its .log file")
    for target in (image, image.with_suffix(".log")):
        if output == target or output in target.parents or target in output.parents:
            raise ValueError("image and log must be separate from the output tree")
        if target.exists() and not target.is_file():
            raise ValueError(f"image and log outputs must be regular files: {target}")
    if profile not in PROFILES or image_size_mib < 128:
        raise ValueError("unsupported ext4 profile or image size below 128 MiB")
    for name in ("sbin/user-selftests", "test/selftests/kselftest-list.txt"):
        if not (userland / name).is_file():
            raise ValueError(f"required installed selftest asset missing: {userland / name}")
    arguments = ["/sbin/user-selftests"]
    for option, values in (("--collection", collections), ("--program", programs)):
        for value in values:
            if not value or "\0" in value:
                raise ValueError(f"{option} must be nonempty and contain no NUL")
            arguments.extend((option, value))
    if program_timeout_seconds is not None:
        arguments.extend(("--program-timeout-seconds", str(program_timeout_seconds)))
    with tempfile.TemporaryDirectory(prefix="user-selftest-init-") as temporary:
        config = Path(temporary) / "inittab"
        config.write_text(
            "::sysinit:/bin/sh /etc/init.d/rcS\n"
            "ttyS0::once:/bin/sh /sbin/run-user-selftests\n"
            "::shutdown:/bin/sh /etc/init.d/rcK\n", encoding="utf-8")
        tree = assemble_rootfs_tree(architecture, userland, runtime, output, config)
    (tree / "test/selftests/exit-status").unlink(missing_ok=True)
    action = tree / "sbin/run-user-selftests"
    action.unlink(missing_ok=True)
    action.write_text(
        "#!/bin/sh\n"
        "/bin/busybox rm -f /test/selftests/exit-status || exit 2\n"
        + shlex.join(arguments) + "\n"
        "status=$?\n"
        "case \"$status\" in 0|1|2|4) ;; *) status=2 ;; esac\n"
        "printf 'user-selftests exit=%s\\n' \"$status\"\n"
        f"printf 'run={run_id} status=%s\\n' \"$status\" > /test/selftests/exit-status\n"
        "/bin/busybox poweroff\n"
        "exit \"$status\"\n", encoding="utf-8")
    action.chmod(0o755)
    return create_ext4_image(tree, image, profile, image_size_mib)


def main(argv=None):
    """解析安装源和本次输出，筛选参数安全引用后交给真实用户程序解释。"""
    parser = argparse.ArgumentParser(description=__doc__, allow_abbrev=False)
    parser.add_argument("--arch", choices=("riscv64", "loongarch64"), required=True)
    parser.add_argument("--guest-userland-directory", type=Path, required=True)
    parser.add_argument("--runtime-libraries-directory", type=Path, required=True)
    parser.add_argument("--output-directory", type=Path, required=True)
    parser.add_argument("--output-image", type=Path, required=True)
    parser.add_argument("--run-id", required=True)
    parser.add_argument("--program-timeout-seconds", type=int)
    parser.add_argument("--collection", action="append", default=[])
    parser.add_argument("--program", action="append", default=[])
    parser.add_argument("--profile", choices=PROFILES, default="seed4k")
    parser.add_argument("--size-mib", type=int, default=256)
    args = parser.parse_args(argv)
    try:
        prepare_user_selftest_rootfs(
            args.arch, args.guest_userland_directory, args.runtime_libraries_directory,
            args.output_directory, args.output_image, args.collection, args.program,
            args.profile, args.size_mib, run_id=args.run_id,
            program_timeout_seconds=args.program_timeout_seconds)
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        print(f"User selftest preparation failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
