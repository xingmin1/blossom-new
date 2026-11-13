#!/usr/bin/env python3
"""为系统盘准备 glibc 补充资产：校验 GNU 工具链的 libgcc_s.so.1 与仓内 C.utf8 locale 后写出，并记录比赛测试根 libc 与动态链接器的固定身份。"""

import argparse
from dataclasses import dataclass
import hashlib
import os
from pathlib import Path
import re
import shutil
import subprocess
import sys
import tempfile


@dataclass(frozen=True)
class RuntimeProfile:
    """一个目标架构的 GNU 编译器、ELF 约束以及系统盘需要的全部固定身份。"""

    # 查询 libgcc_s.so.1 路径的 GNU 交叉编译器。
    compiler: str
    # ELF 头的 e_machine，以及 e_flags 中浮点调用约定位的掩码与取值。
    machine: int
    abi_mask: int
    abi_value: int
    # 比赛盘测试根的 libc 与动态链接器的 SHA-256；loader 是动态链接器的文件名。
    libc_sha256: str
    loader: str
    loader_sha256: str
    # libgcc_s.so.1 的 SHA-256，locale 清单的 SHA-256 及其在 glibc 目录下的相对路径。
    libgcc_sha256: str
    locale_sha256: str
    locale_path: str


# 两个架构的固定身份，编译器、ELF 约束和全部 SHA-256 以此为唯一来源。
# 浮点 ABI 与测试根加载器名中的 lp64d 一致，均为双精度。
RUNTIME_PROFILES = {
    "riscv64": RuntimeProfile(
        compiler="riscv64-linux-gnu-gcc",
        machine=243,
        abi_mask=0x6,
        abi_value=0x4,
        libc_sha256="81af558241962fadf1d0199171be78dd2bb1e45098b8f7f786b22881b7bab33c",
        loader="ld-linux-riscv64-lp64d.so.1",
        loader_sha256="10ac4a073b52a6b05e383ae6b47be92375436174a857fd6299faea1257495eaa",
        libgcc_sha256="e217643d8315bddf37281cc8bf048ceaf39ad25fd82521a47876164010f05372",
        locale_sha256="a4e5d1d829d11772d53e0a889ee1424c3659eecc712844fb1d5e6ede0e5dd993",
        locale_path="usr/lib/locale/C.utf8",
    ),
    "loongarch64": RuntimeProfile(
        compiler="loongarch64-linux-gnu-gcc",
        machine=258,
        abi_mask=0x7,
        abi_value=0x3,
        libc_sha256="b0622d832bf9531bf9d7ada63a04b084f6082f76673e0d16b3dc058c82d4aeb8",
        loader="ld-linux-loongarch-lp64d.so.1",
        loader_sha256="5201d1507ebcdaebc2ec2e728c68ba2fdca503b9e1e7a95cc177ce576ebdc2bb",
        libgcc_sha256="767f7804e011b0111f542c0ce6b86e5eb78ce440d3540b36f3bb41254ca53743",
        locale_sha256="5360b639bb89d4649bcaabb5ff84dc951c1d325daf943672a67702dddde28794",
        locale_path="usr/lib64/locale/C.utf8",
    ),
}

# ELF 头 e_type 中共享库的取值。
ET_DYN = 3

# glibc 通过 dlopen 载入 libgcc_s 后解析的异常展开入口。
UNWIND_EXPORTS = frozenset((
    "_Unwind_Backtrace", "_Unwind_ForcedUnwind", "_Unwind_GetCFA",
    "_Unwind_Resume", "__gcc_personality_v0",
))

# 旧版工具写入输出目录的 musl 与 glibc 库。第二项为符号链接的旧目标，None 表示普通文件；
# 复用输出目录时只清除仍保持这种形态的条目。
STALE_RUNTIME_ENTRIES = (
    ("musl/lib/libc.so", None),
    ("musl/lib/ld-musl-riscv64-sf.so.1", "libc.so"),
    ("musl/lib/ld-musl-loongarch-lp64d.so.1", "libc.so"),
    ("musl/lib64", "lib"),
    ("glibc/lib/libc.so.6", None),
    ("glibc/lib/libm.so.6", None),
    ("glibc/lib/ld-linux-riscv64-lp64d.so.1", None),
    ("glibc/lib/ld-linux-loongarch-lp64d.so.1", None),
    ("glibc/lib64", "lib"),
)


def toolchain_libgcc_s(compiler):
    """向 GNU 交叉编译器查询 libgcc_s.so.1 的安装路径；编译器没有给出绝对路径时视为库不存在。"""
    result = subprocess.run(
        [compiler, "-print-file-name=libgcc_s.so.1"],
        stdout=subprocess.PIPE,
        text=True,
        check=True,
    )
    reported = result.stdout.strip()
    if not os.path.isabs(reported):
        raise ValueError(f"{compiler} did not report an installed libgcc_s.so.1: {reported!r}")
    return Path(reported)


def readelf_output(path, option):
    """以 C locale 运行 `readelf` 读取一类 ELF 信息并返回文本，工具缺失或读取失败时抛出异常。"""
    result = subprocess.run(
        ["readelf", "-W", option, str(path)],
        stdout=subprocess.PIPE,
        env={**os.environ, "LC_ALL": "C"},
        check=True,
    )
    return result.stdout.decode("utf-8", errors="replace")


def exported_symbols(path):
    """读取共享库对外导出的动态符号名，只计已定义的全局、弱和唯一符号，并去掉版本后缀。"""
    names = set()
    for line in readelf_output(path, "--dyn-syms").splitlines():
        fields = line.split()
        if len(fields) < 8 or not fields[0][:-1].isdigit() or not fields[0].endswith(":"):
            continue
        binding, visibility, index, name = fields[4:8]
        if (index != "UND" and binding in ("GLOBAL", "WEAK", "UNIQUE")
                and visibility in ("DEFAULT", "PROTECTED")):
            names.add(name.partition("@")[0])
    return names


def check_libgcc_s(path, profile):
    """检查 libgcc 是目标架构且浮点 ABI 一致的小端 ELF64 共享库，SONAME、依赖和异常展开导出齐全。"""
    print(f"Checking libgcc: {path}", flush=True)
    with path.open("rb") as stream:
        header = stream.read(64)
    if len(header) != 64 or header[:6] != b"\x7fELF\x02\x01":
        raise ValueError(f"expected little-endian ELF64 libgcc: {path}")
    elf_type = int.from_bytes(header[16:18], "little")
    machine = int.from_bytes(header[18:20], "little")
    flags = int.from_bytes(header[48:52], "little")
    if elf_type != ET_DYN:
        raise ValueError(f"expected ET_DYN libgcc: {path}")
    if machine != profile.machine:
        raise ValueError(f"libgcc ELF machine {machine} does not match {profile.machine}: {path}")
    if (flags & profile.abi_mask) != profile.abi_value:
        raise ValueError(
            f"libgcc floating-point ABI flags {flags & profile.abi_mask:#x} "
            f"do not match {profile.abi_value:#x}: {path}"
        )
    entries = re.findall(
        r"^\s*0x[0-9a-fA-F]+\s+\((SONAME|NEEDED)\)[^\[\r\n]*\[([^\]]+)\]",
        readelf_output(path, "-d"),
        re.MULTILINE,
    )
    sonames = [name for tag, name in entries if tag == "SONAME"]
    needed = [name for tag, name in entries if tag == "NEEDED"]
    if sonames != ["libgcc_s.so.1"]:
        raise ValueError(f"expected libgcc_s.so.1 SONAME: {path}")
    if "libc.so.6" not in needed:
        raise ValueError(f"libgcc must have DT_NEEDED libc.so.6: {path}")
    missing = sorted(UNWIND_EXPORTS - exported_symbols(path))
    if missing:
        raise ValueError(f"libgcc missing unwinder exports: {', '.join(missing)}")


def reject_source_overlap(source, outputs):
    """拒绝输入与任何输出位置相互包含或指向同一文件。"""
    for output in outputs:
        if (source == output or source in output.parents or output in source.parents
                or source.exists() and output.exists() and source.samefile(output)):
            raise ValueError(f"source and output overlap or alias: {source}, {output}")


def builtin_locale_directory(architecture, locale_directory=None):
    """返回清单与固定身份一致的 locale 来源目录，未指定目录时使用仓内数据。

    指定的目录只是同一份数据的另一处存放位置。目录树内只能有目录和普通文件，不得含符号链接或
    特殊文件。清单按相对路径排序，每行写 `文件 SHA-256`、两个空格、相对路径及换行，
    其 SHA-256 必须等于该架构的固定值。
    """
    if locale_directory is None:
        locale_directory = Path(__file__).resolve().parent / "locale" / architecture / "C.utf8"
    source = Path(locale_directory).resolve(strict=True)
    if not source.is_dir():
        raise ValueError(f"locale source is not a directory: {source}")
    files = []
    pending = [source]
    while pending:
        path = pending.pop()
        if path.is_symlink():
            raise ValueError(f"locale contains a symbolic link: {path}")
        if path.is_dir():
            pending.extend(path.iterdir())
        elif path.is_file():
            files.append(path)
        else:
            raise ValueError(f"locale contains a non-regular entry: {path}")
    files.sort(key=lambda path: path.relative_to(source).as_posix())
    manifest = "".join(
        f"{hashlib.sha256(path.read_bytes()).hexdigest()}  {path.relative_to(source).as_posix()}\n"
        for path in files
    )
    try:
        manifest_hash = hashlib.sha256(manifest.encode("ascii")).hexdigest()
    except UnicodeEncodeError as error:
        raise ValueError(f"locale contains a non-ASCII path: {source}") from error
    if manifest_hash != RUNTIME_PROFILES[architecture].locale_sha256:
        raise ValueError(f"locale manifest SHA-256 mismatch: {source}")
    return source


def make_temporary_directories_writable(directory):
    """恢复脚本持有的暂存或备份目录的属主权限，跳过符号链接。"""
    mode = directory.stat().st_mode
    if mode & 0o700 != 0o700:
        directory.chmod(mode | 0o700)
    for child in directory.iterdir():
        if not child.is_symlink() and child.is_dir():
            make_temporary_directories_writable(child)


def publish_locale_directory(source, destination):
    """在同级目录暂存 locale，失败或中断时恢复旧目录。"""
    destination.parent.mkdir(parents=True, exist_ok=True)
    if destination.is_symlink() or (destination.exists() and not destination.is_dir()):
        raise ValueError(f"locale output must be an ordinary directory: {destination}")
    staged = Path(tempfile.mkdtemp(prefix=f".{destination.name}.", dir=destination.parent))
    previous = staged.with_name(f"{staged.name}.previous")
    had_destination = destination.exists()
    previous_available = False
    cleanup_started = False
    try:
        if previous.exists() or previous.is_symlink():
            raise FileExistsError(f"locale backup path already exists: {previous}")
        previous_available = True
        shutil.copytree(source, staged, dirs_exist_ok=True)
        if had_destination:
            destination.rename(previous)
        staged.rename(destination)
        if had_destination:
            cleanup_started = True
            make_temporary_directories_writable(previous)
            shutil.rmtree(previous)
    except BaseException:
        restore_error = None
        if not cleanup_started:
            try:
                if previous_available and previous.exists():
                    if destination.exists() or destination.is_symlink():
                        if staged.exists() or staged.is_symlink():
                            raise FileExistsError(f"locale staging path already exists: {staged}")
                        destination.rename(staged)
                    previous.rename(destination)
                elif not had_destination and not staged.exists() and destination.exists():
                    destination.rename(staged)
            except BaseException as error:
                restore_error = error
                print(f"Failed to restore locale {destination}: {error!r}", file=sys.stderr)
        if previous_available and previous.exists():
            state = "may be partially removed and " if cleanup_started else ""
            print(f"Previous locale {state}retained at {previous}", file=sys.stderr)
        if staged.exists() or staged.is_symlink():
            if restore_error is not None or (previous_available and previous.exists()):
                print(f"Staged locale retained at {staged}", file=sys.stderr)
            else:
                try:
                    make_temporary_directories_writable(staged)
                    shutil.rmtree(staged)
                except BaseException as cleanup_error:
                    print(f"Failed to remove staged locale {staged}: {cleanup_error!r}",
                          file=sys.stderr)
        raise
    print(f"Copied locale: {source} -> {destination}")


def publish_file(destination, content):
    """通过同目录临时文件原子替换目标文件并固定权限为 0644，不跟随目标路径上已有的符号链接。"""
    destination.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.NamedTemporaryFile(
        dir=destination.parent, prefix=f".{destination.name}.", delete=False
    ) as stream:
        temporary_path = Path(stream.name)
    try:
        temporary_path.write_bytes(content)
        temporary_path.chmod(0o644)
        temporary_path.replace(destination)
    finally:
        temporary_path.unlink(missing_ok=True)
    print(f"Wrote: {destination}")


def reject_linked_parents(output_directory, paths):
    """拒绝父级含符号链接或非目录的输出路径，使写入和清理都不会越出输出目录。"""
    for path in paths:
        parent = output_directory
        for name in path.relative_to(output_directory).parts[:-1]:
            parent = parent / name
            if parent.is_symlink() or (parent.exists() and not parent.is_dir()):
                raise ValueError(f"runtime output parent must be an ordinary directory: {parent}")


def is_stale_entry(path, link_target):
    """判断路径是否仍是旧版工具写出的库：link_target 为 None 时要求普通文件，否则要求指向该目标的符号链接。"""
    if link_target is None:
        return path.is_file() and not path.is_symlink()
    return path.is_symlink() and os.readlink(path) == link_target


def read_libgcc_s(profile, libgcc_s_path, outputs):
    """定位并校验 libgcc_s.so.1，返回已核对 SHA-256 的文件内容。

    默认路径取自 GNU 编译器，显式路径只是同一份字节的另一处存放位置；
    来源不能与任何输出位置重叠，身份不符时不会运行 `readelf`。
    """
    if libgcc_s_path is None:
        libgcc_s_path = toolchain_libgcc_s(profile.compiler)
    source = Path(libgcc_s_path).resolve(strict=True)
    if not source.is_file():
        raise ValueError(f"libgcc source is not a regular file: {source}")
    reject_source_overlap(source, outputs)
    content = source.read_bytes()
    actual = hashlib.sha256(content).hexdigest()
    if actual != profile.libgcc_sha256:
        raise ValueError(
            f"libgcc_s.so.1 SHA-256 mismatch: expected {profile.libgcc_sha256}, "
            f"actual {actual}: {source}"
        )
    check_libgcc_s(source, profile)
    return content


def prepare_runtime_libraries(architecture, output_directory, glibc_locale_directory=None,
                              libgcc_s_path=None):
    """校验固定身份的 libgcc 与 locale 后写出 glibc 补充资产，并清除旧版工具遗留的 musl 与 glibc 库。

    输出为 `glibc/lib/libgcc_s.so.1`、`glibc/identity` 和架构对应的 `glibc/usr/lib{,64}/locale/C.utf8`，
    `identity` 一行依次记录测试根 libc 与动态链接器的 SHA-256 及加载器文件名。来源缺失、身份不符和
    输出路径不合规都在第一次写入前报错。写入阶段的多项输出不构成整体事务，调用者以是否抛出异常判断成败。
    """
    if architecture not in RUNTIME_PROFILES:
        raise ValueError(f"unsupported architecture: {architecture}")
    profile = RUNTIME_PROFILES[architecture]
    output_directory = Path(output_directory).absolute()
    libgcc_destination = output_directory / "glibc/lib/libgcc_s.so.1"
    identity_destination = output_directory / "glibc/identity"
    locale_destination = output_directory / "glibc" / profile.locale_path
    destinations = [libgcc_destination, identity_destination, locale_destination]
    stale_entries = [(output_directory / name, target) for name, target in STALE_RUNTIME_ENTRIES]
    outputs = [path.resolve() for path in [output_directory, *destinations]]
    reject_linked_parents(output_directory, destinations + [path for path, _ in stale_entries])
    for destination in (libgcc_destination, identity_destination):
        if destination.is_dir() and not destination.is_symlink():
            raise ValueError(f"runtime output must not be a directory: {destination}")
    if locale_destination.is_symlink() or (
            locale_destination.exists() and not locale_destination.is_dir()):
        raise ValueError(f"locale output must be an ordinary directory: {locale_destination}")
    locale_source = builtin_locale_directory(architecture, glibc_locale_directory)
    reject_source_overlap(locale_source, outputs)
    libgcc = read_libgcc_s(profile, libgcc_s_path, outputs)
    for path, target in stale_entries:
        if is_stale_entry(path, target):
            path.unlink()
            print(f"Removed stale runtime entry: {path}")
    publish_file(libgcc_destination, libgcc)
    publish_locale_directory(locale_source, locale_destination)
    # identity 最后发表，首次准备中途失败的输出目录因缺少它而不完整。
    identity = f"{profile.libc_sha256} {profile.loader_sha256} {profile.loader}\n"
    publish_file(identity_destination, identity.encode("ascii"))
    print(f"Runtime libraries prepared: arch={architecture} output_directory={output_directory}")


def main(argv=None):
    """命令行入口：解析架构、输出目录和可选的 libgcc、locale 重定位路径并执行准备流程，失败时输出错误并返回非零退出码。"""
    parser = argparse.ArgumentParser(description=__doc__, allow_abbrev=False)
    parser.add_argument("--arch", choices=RUNTIME_PROFILES, required=True)
    parser.add_argument("--libgcc-s", type=Path,
                        help="relocate libgcc_s.so.1; its SHA-256 must match the fixed identity")
    parser.add_argument("--glibc-locale-dir", type=Path,
                        help="relocate C.utf8; its manifest SHA-256 must match the fixed identity")
    parser.add_argument("--output-directory", type=Path, required=True)
    args = parser.parse_args(argv)
    try:
        prepare_runtime_libraries(args.arch, args.output_directory,
                                  args.glibc_locale_dir, args.libgcc_s)
    except (OSError, ValueError, RuntimeError, shutil.Error, subprocess.CalledProcessError) as error:
        print(f"Runtime library preparation failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
