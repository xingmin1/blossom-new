#!/usr/bin/env python3
"""根文件系统目录树组装脚本。

本脚本位于用户态构建与文件系统镜像制作之间，负责将预先编译安装好的用户态程序、
glibc 补缺资产（`libgcc_s.so.1`、测试根运行库身份记录与 `C.utf8` locale）以及仓库内的静态系统配置
合并为单个架构（`riscv64` 或 `loongarch64`）的完整根文件系统目录树，供后续 `create_ext4_image.py` 打包为 ext4 镜像。
系统根不携带 musl 或 glibc 的 libc、libm 与加载器，它们由测试盘自带。

输入：
- 目标架构名称、已安装的客户机用户态目录、运行库目录（只读取其中的 `glibc/lib/libgcc_s.so.1`、`glibc/identity`
  与对应架构的 `glibc/usr/{lib|lib64}/locale/C.utf8`），以及可选的自定义 `inittab` 和 FAT32 夹具文件；
- 仓库内置的 `userland/system`（系统脚本与 `/etc` 配置）和 `userland/contest`（比赛运行配置）。

输出：
- 组装完成的根文件系统目录树，glibc 补缺资产位于 `/opt/runtime/glibc`。组装过程先在同级临时目录中构建完整目录并设置好权限，
  最后通过目录重命名替换旧输出目录，确保所有输入源保持只读且失败时不会留下半成品目录。
"""

import argparse
import os
from pathlib import Path
import re
import shutil
import sys
import tempfile
from rootfs_paths import output_path, reject_overlap


REPOSITORY_ROOT = Path(__file__).resolve().parents[1]

# 系统根只装入 glibc 补缺资产。`identity` 一行依次记录测试根 libc 的 SHA-256、加载器的 SHA-256 与加载器文件名；
# 哈希值由宿主运行库准备脚本按架构生成，这里只校验格式，不再复制一份哈希表。
RUNTIME_IDENTITY_PATTERN = re.compile(rb"[0-9a-f]{64} [0-9a-f]{64} [A-Za-z0-9][A-Za-z0-9._-]*\n")


def copy_rootfs_entries(source_directory, destination_directory):
    """将源目录树递归合并到目标目录树中，并安全处理符号链接与文件类型替换。

    复制时若目标位置已存在符号链接，会先删除该链接再创建目录或写入文件，防止沿着符号链接
    意外修改只读安装源中的文件。普通文件使用 `copy2` 保留元数据，符号链接按原样重建链接目标。

    参数：
    - `source_directory`: 只读源目录 `Path`。
    - `destination_directory`: 待写入的目标目录 `Path`。
    """
    # 若目标目录本身是符号链接或普通文件，先将其移除，避免 mkdir 失败或沿链接写入源目录
    if destination_directory.is_symlink() or destination_directory.is_file():
        destination_directory.unlink()
    destination_directory.mkdir(parents=True, exist_ok=True)
    for source in source_directory.iterdir():
        destination = destination_directory / source.name
        if source.is_dir() and not source.is_symlink():
            copy_rootfs_entries(source, destination)
        else:
            # 源条目为普通文件或符号链接时，先清理目标位置已有的同名目录或文件，再按源类型复制或重建链接
            if destination.is_dir() and not destination.is_symlink():
                shutil.rmtree(destination)
            else:
                destination.unlink(missing_ok=True)
            if source.is_symlink():
                destination.symlink_to(os.readlink(source))
            else:
                shutil.copy2(source, destination)
    shutil.copystat(source_directory, destination_directory)


def select_runtime_assets(architecture, runtime_directory):
    """校验运行库目录中的三类 glibc 补缺资产，并返回 glibc 源根目录与资产相对路径。

    只选择 `glibc/lib/libgcc_s.so.1`、`glibc/identity` 与对应架构的 `glibc/usr/{lib|lib64}/locale/C.utf8`
    （RISC-V 使用 `lib`，LoongArch 使用 `lib64`）。运行库目录中的其他内容，例如旧版 musl 或 glibc 的
    libc、libm 与加载器残留，既不检查也不装入。资产可以经符号链接到达，只要解析后 `libgcc_s.so.1` 与 `identity` 是普通文件、
    `C.utf8` 是目录；`identity` 必须恰为一行 `<libc SHA-256> <加载器 SHA-256> <加载器文件名>` 并以换行结束。

    参数：
    - `architecture`: 目标架构，`"riscv64"` 或 `"loongarch64"`。
    - `runtime_directory`: 已解析为真实路径的运行库目录 `Path`。

    返回：
    - `(glibc 源根目录, 资产相对路径列表)`，相对路径均相对于 `glibc` 源根目录。

    异常：
    - `ValueError`: 缺少任一资产、资产类型不符，或 `identity` 格式不符。
    """
    glibc = runtime_directory / "glibc"
    locale_directory = Path("usr", "lib" if architecture == "riscv64" else "lib64", "locale/C.utf8")
    assets = [Path("lib/libgcc_s.so.1"), Path("identity"), locale_directory]
    for relative in assets:
        source = glibc / relative
        if not (source.is_dir() if relative == locale_directory else source.is_file()):
            raise ValueError(f"required runtime asset missing: {source}")
    identity = glibc / "identity"
    if RUNTIME_IDENTITY_PATTERN.fullmatch(identity.read_bytes()) is None:
        raise ValueError(f"invalid runtime identity record: {identity}")
    return glibc, assets


def install_runtime_assets(root, glibc, assets, output):
    """将三类 glibc 补缺资产解引用后，直接复制到根目录树的 `/opt/runtime/glibc` 下。

    每个资产先解析为实际的只读源（资产本身、其父目录或 `C.utf8` 内部的符号链接都会被跟随），再复制成
    普通文件和目录，因此客户机不会得到悬空或被拒绝的链接。解析后的源在复制前都必须与输出目录无重叠，
    且只能是普通文件或目录，符号链接循环同样被拒绝。目标侧沿用 `copy_rootfs_entries` 的安全替换语义：
    逐层建立父目录，目标处的符号链接或同名的不同类型条目先移除而不被跟随，已有目录则合并，其中无关的文件保持不动。
    父目录的权限和时间戳取自运行库目录，待三项都写完后再设置，避免只读父目录阻断后续写入。
    出错时只留下未发布的临时树，旧输出不受影响。

    参数：
    - `root`: 根目录树中的 `opt/runtime/glibc` 路径 `Path`；其上层 `opt`、`opt/runtime` 须已确认不是符号链接。
    - `glibc`: 运行库目录中的 `glibc` 源根目录 `Path`。
    - `assets`: `select_runtime_assets` 返回的资产相对路径列表。
    - `output`: 规范化后的输出目录 `Path`，用于拒绝与解析后的源重叠。

    异常：
    - `ValueError`: 解析后的源与输出重叠、目录循环，或资产不是普通文件与目录。
    - `RuntimeError`: Python 3.12 及以前解析直接循环的符号链接。
    - `OSError`: 解析、建立目录或复制失败。
    """
    def make_directory(directory):
        """建立目标目录；符号链接或普通文件占位时先移除，避免沿链接写入树外。"""
        if directory.is_symlink() or directory.is_file():
            directory.unlink()
        directory.mkdir(parents=True, exist_ok=True)

    def copy_resolved(source, destination, ancestors=()):
        """复制解析后的单个条目；目录递归复制，`ancestors` 记录当前链上的真实目录以识别循环。"""
        source = source.resolve(strict=True)
        reject_overlap(source, output)
        if source.is_dir():
            if source in ancestors:
                raise ValueError(f"runtime asset contains a symbolic link cycle: {source}")
            make_directory(destination)
            for entry in source.iterdir():
                copy_resolved(entry, destination / entry.name, ancestors + (source,))
            shutil.copystat(source, destination)
        elif source.is_file():
            # 先清理目标位置已有的同名目录或文件，再复制
            if destination.is_dir() and not destination.is_symlink():
                shutil.rmtree(destination)
            else:
                destination.unlink(missing_ok=True)
            shutil.copy2(source, destination)
        else:
            raise ValueError(f"runtime asset is not a regular file or directory: {source}")

    for relative in assets:
        for parent in reversed(relative.parents):
            make_directory(root / parent)
        copy_resolved(glibc / relative, root / relative)
    # 三项都写完后再设置父目录元数据，避免只读目录阻断后续写入
    for directory in {parent for asset in assets for parent in asset.parents}:
        shutil.copystat(glibc / directory, root / directory)


def assemble_rootfs_tree(architecture, guest_userland_directory,
                         runtime_libraries_directory, output_directory,
                         init_config_path=None, basic_fat32_fixture_path=None):
    """校验安装输入并在临时目录中组装完整的单架构根文件系统目录树，完成后替换到输出目录。

    参数：
    - `architecture`: 目标架构，支持 `"riscv64"` 或 `"loongarch64"`。
    - `guest_userland_directory`: 已安装的客户机用户态程序根目录。
    - `runtime_libraries_directory`: 已准备好的运行库目录；只读取其中的 `glibc/lib/libgcc_s.so.1`、`glibc/identity`
      与对应架构的 `glibc/usr/{lib|lib64}/locale/C.utf8`（可经符号链接到达，装入时解引用为普通文件与目录），
      其余内容（含旧版 libc、加载器与 musl 残留）不会装入系统根。
    - `output_directory`: 输出根文件系统目录路径。
    - `init_config_path`: 可选的自定义 `inittab` 文件路径；若指定则覆盖默认的 `/etc/inittab`。
    - `basic_fat32_fixture_path`: 可选的 FAT32 夹具文件；若指定则装入 `/etc/contest/fixtures/basic-fat32.img`。

    返回：
    - 规范化后的输出目录 `Path`。

    异常：
    - `ValueError`: 架构不支持、输入源缺失关键程序或 glibc 补缺资产、`identity` 格式不符、输入输出路径或解析后的资产与输出重叠、
      资产含目录循环或特殊文件，或运行时目录被符号链接篡改。
    - `RuntimeError`: Python 3.12 及以前解析直接循环的符号链接。
    - `OSError`: 路径解析、文件复制、权限修改或输出目录替换失败。
    """
    if architecture not in ("riscv64", "loongarch64"):
        raise ValueError(f"unsupported architecture: {architecture}")
    userland = Path(guest_userland_directory).resolve(strict=True)
    runtime = Path(runtime_libraries_directory).resolve(strict=True)
    system = REPOSITORY_ROOT / "userland/system"
    contest = REPOSITORY_ROOT / "userland/contest"
    output = output_path(output_directory, directory=True)
    init_config = Path(init_config_path).resolve(strict=True) if init_config_path is not None else None
    basic_fat32_fixture = (Path(basic_fat32_fixture_path).resolve(strict=True)
                           if basic_fat32_fixture_path is not None else None)
    # 校验所有输入目录与可选配置文件存在且类型正确，并确保输出目录不与任何输入源重叠或互为硬链接
    sources = (userland, runtime, system.resolve(), contest.resolve())
    for source in sources:
        if not source.is_dir():
            raise ValueError(f"required source is not a directory: {source}")
        reject_overlap(source, output)
    if init_config is not None:
        if not init_config.is_file():
            raise ValueError(f"init config is not a regular file: {init_config}")
        reject_overlap(init_config, output)
    if basic_fat32_fixture is not None:
        if not basic_fat32_fixture.is_file():
            raise ValueError(f"FAT32 fixture is not a regular file: {basic_fat32_fixture}")
        reject_overlap(basic_fat32_fixture, output)
    # 检查系统启动、Shell 交互与比赛执行所必需的基础用户态程序是否齐全，并校验三类 glibc 补缺资产
    for name in ("bin/busybox", "bin/sh", "sbin/init", "sbin/contest-run", "bin/libc-smoke"):
        if not (userland / name).is_file():
            raise ValueError(f"required installed guest program missing: {userland / name}")
    glibc, assets = select_runtime_assets(architecture, runtime)
    output.parent.mkdir(parents=True, exist_ok=True)
    # 在输出目录所在的父目录下创建临时工作目录，确保最终可以通过同文件系统的 rename 原子切换输出目录
    with tempfile.TemporaryDirectory(prefix="rootfs-", dir=output.parent) as temporary:
        tree = Path(temporary) / "root"
        # 依次叠加客户机程序、系统静态配置、glibc 补缺资产以及比赛脚本配置
        copy_rootfs_entries(userland, tree)
        copy_rootfs_entries(system / "etc", tree / "etc")
        copy_rootfs_entries(system / "sbin", tree / "sbin")
        if (tree / "opt").is_symlink() or (tree / "opt/runtime").is_symlink():
            raise ValueError("installed runtime parent must be a directory, not a symbolic link")
        install_runtime_assets(tree / "opt/runtime/glibc", glibc, assets, output)
        copy_rootfs_entries(contest, tree / "etc/contest")
        if basic_fat32_fixture is not None:
            fixture_directory = tree / "etc/contest/fixtures"
            if fixture_directory.is_symlink():
                raise ValueError("contest fixture directory must not be a symbolic link")
            fixture_directory.mkdir(parents=True, exist_ok=True)
            fixture_destination = fixture_directory / "basic-fat32.img"
            fixture_destination.unlink(missing_ok=True)
            shutil.copy2(basic_fat32_fixture, fixture_destination)
        # 若调用方提供了专用的 init 配置（例如自动化测试负载），用其覆盖默认的 /etc/inittab
        if init_config is not None:
            (tree / "etc/inittab").unlink(missing_ok=True)
            shutil.copy2(init_config, tree / "etc/inittab")
        # 补齐内核挂载点与系统运行所需的标准空目录，并设置根目录及临时目录的粘滞位权限（01777）
        for name in ("dev", "proc", "run", "tmp", "var", "var/tmp", "root", "home",
                     "usr", "usr/bin", "usr/sbin", "mnt", "mnt/extra", "mnt/contest"):
            destination = tree / name
            if destination.is_symlink():
                raise ValueError(f"root filesystem directory must not be a symbolic link: {name}")
            destination.mkdir(parents=True, exist_ok=True)
        tree.chmod(0o755)
        (tree / "tmp").chmod(0o1777)
        (tree / "var/tmp").chmod(0o1777)
        # 先将旧输出目录移入临时目录暂存，再将新目录树重命名就位；若切换失败则恢复旧目录
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
    """解析命令行参数并组装根文件系统目录树；仅消费现成的安装目录，不触发编译。

    参数：
    - `argv`: 可选的命令行参数列表；默认读取 `sys.argv[1:]`。

    返回：
    - 进程退出码：组装成功返回 `0`，参数校验或文件系统操作失败返回 `1`。
    """
    parser = argparse.ArgumentParser(description=__doc__, allow_abbrev=False)
    parser.add_argument("--arch", choices=("riscv64", "loongarch64"), required=True)
    parser.add_argument("--guest-userland-directory", type=Path, required=True)
    parser.add_argument("--runtime-libraries-directory", type=Path, required=True)
    parser.add_argument("--output-directory", type=Path, required=True)
    parser.add_argument("--init-config", type=Path)
    parser.add_argument("--basic-fat32-fixture", type=Path)
    args = parser.parse_args(argv)
    try:
        assemble_rootfs_tree(args.arch, args.guest_userland_directory,
                             args.runtime_libraries_directory, args.output_directory,
                             args.init_config, args.basic_fat32_fixture)
    except (OSError, ValueError) as error:
        print(f"Root filesystem assembly failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
