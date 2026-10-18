#!/usr/bin/env python3
"""只读消费完整目录树，使用 e2fsprogs 生成并检查 ext4 后替换输出镜像。"""

import argparse
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile
from rootfs_paths import output_path, reject_overlap


PROFILES = {
    "legacy1k": (1024, 128, "none,ext_attr,resize_inode,dir_index,filetype,sparse_super,large_file,uninit_bg"),
    "metadata2k": (2048, 256, "none,ext_attr,resize_inode,dir_index,filetype,sparse_super,large_file,extent,metadata_csum,extra_isize"),
    "seed4k": (4096, 256, "none,ext_attr,resize_inode,dir_index,filetype,sparse_super,large_file,extent,64bit,metadata_csum,metadata_csum_seed,extra_isize"),
}


def run_checked_command(arguments, log_path, accepted=(0,)):
    """保存工具的完整输出，只接受调用处明确列出的退出状态。"""
    result = subprocess.run([str(item) for item in arguments], check=False,
                            stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                            env={**os.environ, "LC_ALL": "C", "E2FSPROGS_FAKE_TIME": "1700000000"})
    with log_path.open("ab") as stream:
        stream.write(("$ " + " ".join(map(str, arguments)) + "\n").encode())
        stream.write(result.stdout)
    if result.returncode not in accepted:
        raise subprocess.CalledProcessError(result.returncode, arguments, output=result.stdout)
    return result.stdout.decode("ascii", errors="replace")


def check_debugfs_result(text, request):
    """debugfs 有些失败返回零状态，仍须拒绝其明确错误诊断。"""
    if re.search(r"(?:Command not found|Usage:|Invalid field|Unknown feature|Filesystem not open|File not found|while (?:opening|setting|allocating|punching))", text, re.IGNORECASE):
        raise ValueError(f"debugfs rejected request: {request}")
    return text


def run_debugfs_command(image_path, request, writable=False):
    """默认只读查询镜像；夹具修改必须显式请求写模式，全部输出进入同一日志。"""
    arguments = ["debugfs"]
    if writable:
        arguments.append("-w")
    arguments.extend(["-R", request, image_path])
    return check_debugfs_result(
        run_checked_command(arguments, image_path.with_suffix(".log")), request)


def check_ext4_image(image_path, profile):
    """检查实际超级块几何和目录，不执行修复或 journal 恢复。"""
    with image_path.open("rb") as stream:
        stream.seek(1024)
        superblock = stream.read(1024)
    block_size, inode_size, _ = PROFILES[profile]
    if (len(superblock) != 1024 or superblock[56:58] != b"\x53\xef"
            or int.from_bytes(superblock[24:28], "little") != block_size.bit_length() - 11
            or int.from_bytes(superblock[88:90], "little") != inode_size):
        raise ValueError("generated superblock does not match requested geometry")
    run_checked_command(["dumpe2fs", "-h", image_path], image_path.with_suffix(".log"))
    run_debugfs_command(image_path, "stat /")
    run_checked_command(["e2fsck", "-fn", image_path], image_path.with_suffix(".log"))


def create_ext4_image(source_tree_dir, output_image_path, profile="seed4k", image_size_mib=256):
    """完整源树仅作输入；新卷检查成功才替换输出文件，失败保留原卷及工具诊断。"""
    source = Path(source_tree_dir).resolve(strict=True)
    output = output_path(output_image_path)
    log = output_path(output.with_suffix(".log"))
    if not source.is_dir():
        raise ValueError("source tree must be an existing directory")
    if profile not in PROFILES:
        raise ValueError(f"unsupported ext4 profile: {profile}")
    if image_size_mib <= 0:
        raise ValueError("image size must be positive")
    reject_overlap(output, log)
    for target in (output, log):
        reject_overlap(source, target)
    for binary in ("mke2fs", "debugfs", "e2fsck", "dumpe2fs"):
        if shutil.which(binary) is None:
            raise ValueError(f"required tool missing: {binary}")
    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="ext4-image-", dir=output.parent) as temporary:
        image = Path(temporary) / "root.ext4"
        temporary_log = image.with_suffix(".log")
        try:
            block_size, inode_size, features = PROFILES[profile]
            with image.open("xb") as stream:
                stream.truncate(image_size_mib * 1024 * 1024)
            run_checked_command([
                "mke2fs", "-q", "-F", "-t", "ext4", "-b", block_size,
                "-I", inode_size, "-O", features,
                "-E", "lazy_itable_init=0", "-L", profile,
                "-U", "ae649880-7392-4424-a21c-392fdc0d1357", "-d", source, image,
            ], temporary_log)
            check_ext4_image(image, profile)
        except (ValueError, OSError, OverflowError, subprocess.CalledProcessError):
            if temporary_log.exists():
                os.replace(temporary_log, log)
            raise
        os.replace(temporary_log, log)
        os.replace(image, output)
    print(f"Ext4 image created: output={output} profile={profile} source={source}")
    return output


def main(argv=None):
    """只接受完整树、镜像路径及几何，不安装资产或触发编译。"""
    parser = argparse.ArgumentParser(description=__doc__, allow_abbrev=False)
    parser.add_argument("--source-tree", type=Path, required=True)
    parser.add_argument("--output-image", type=Path, required=True)
    parser.add_argument("--profile", choices=PROFILES, default="seed4k")
    parser.add_argument("--size-mib", type=int, default=256)
    args = parser.parse_args(argv)
    try:
        create_ext4_image(args.source_tree, args.output_image, args.profile, args.size_mib)
    except (OSError, ValueError, OverflowError, subprocess.CalledProcessError) as error:
        print(f"Ext4 image creation failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
