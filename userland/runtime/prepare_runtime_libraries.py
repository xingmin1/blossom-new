#!/usr/bin/env python3
"""从宿主机交叉工具链提取并校验 musl 与 glibc 动态运行库，安装到目标运行库目录并建立加载器符号链接。"""

import argparse
import hashlib
import os
from pathlib import Path
import re
import shutil
import struct
import subprocess
import sys
import tempfile


# 各架构默认的交叉工具链运行库路径、期望的 readelf Machine 标识及动态链接器文件名。
RUNTIME_LIBRARY_SOURCES = {
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

# 默认工具链中的 libgcc 与内置 locale 分别绑定官方 libc 和数据清单。
DEFAULT_LIBGCC_S = {
    "riscv64": "/usr/riscv64-linux-gnu/lib/libgcc_s.so.1",
    "loongarch64": (
        "/opt/gcc-13.2.0-loongarch64-linux-gnu/"
        "loongarch64-linux-gnu/lib64/libgcc_s.so.1"
    ),
}
BUILTIN_LOCALE_HASHES = {
    "riscv64": (
        "81af558241962fadf1d0199171be78dd2bb1e45098b8f7f786b22881b7bab33c",
        "a4e5d1d829d11772d53e0a889ee1424c3659eecc712844fb1d5e6ede0e5dd993",
    ),
    "loongarch64": (
        "b0622d832bf9531bf9d7ada63a04b084f6082f76673e0d16b3dc058c82d4aeb8",
        "5360b639bb89d4649bcaabb5ff84dc951c1d325daf943672a67702dddde28794",
    ),
}

# LC_ALL 不是独立的 locale 数据类别。
LOCALE_CATEGORIES = (
    ("LC_CTYPE", 0), ("LC_NUMERIC", 1), ("LC_TIME", 2), ("LC_COLLATE", 3),
    ("LC_MONETARY", 4), ("LC_MESSAGES", 5), ("LC_PAPER", 7), ("LC_NAME", 8),
    ("LC_ADDRESS", 9), ("LC_TELEPHONE", 10), ("LC_MEASUREMENT", 11),
    ("LC_IDENTIFICATION", 12),
)


def check_runtime_library_elf(source_path, expected_machine):
    """调用 `readelf` 检查源库文件并透传原始输出，验证其为匹配目标架构的 `ET_DYN` 共享库。"""
    print(f"Checking ELF source: {source_path}", flush=True)
    result = subprocess.run(
        ["readelf", "-h", "-l", str(source_path)],
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
        raise ValueError(f"expected ET_DYN runtime library: {source_path}")
    if actual_machine is None or actual_machine.group(1).strip() != expected_machine:
        raise ValueError(f"expected {expected_machine} runtime library: {source_path}")


def read_dynamic_symbols(source_path):
    """读取动态符号的名称、版本、绑定、可见性及定义状态。"""
    print(f"Checking dynamic symbols: {source_path}", flush=True)
    result = subprocess.run(
        ["readelf", "--dyn-syms", "--wide", str(source_path)],
        stdout=subprocess.PIPE,
        env={**os.environ, "LC_ALL": "C"},
        check=False,
    )
    result.check_returncode()
    symbols = []
    for line in result.stdout.decode("utf-8", errors="replace").splitlines():
        fields = line.split()
        if len(fields) < 8 or not fields[0].endswith(":") or not fields[0][:-1].isdigit():
            continue
        name, _, version = fields[7].partition("@")
        symbols.append((name, version.lstrip("@"), fields[4], fields[5], fields[6]))
    if not symbols:
        raise ValueError(f"no readable dynamic symbols: {source_path}")
    return symbols


def check_libgcc_s(source_path, libc_path, loader_path, architecture):
    """检查 libgcc 的动态链接身份、浮点 ABI 和必需的动态符号。"""
    def floating_abi(path):
        """读取目标小端 ELF64 文件头中的架构浮点调用约定。"""
        with path.open("rb") as stream:
            header = stream.read(64)
        if len(header) != 64 or header[:6] != b"\x7fELF\x02\x01":
            raise ValueError(f"expected little-endian ELF64 runtime library: {path}")
        flags = int.from_bytes(header[48:52], "little")
        abi = flags & (0x6 if architecture == "riscv64" else 0x7)
        if architecture == "loongarch64" and abi not in (1, 2, 3):
            raise ValueError(f"unknown LoongArch floating-point ABI: {path}")
        return abi

    if floating_abi(source_path) != floating_abi(libc_path):
        raise ValueError(f"libgcc floating-point ABI does not match glibc: {source_path}")
    result = subprocess.run(
        ["readelf", "-W", "-d", str(source_path)],
        stdout=subprocess.PIPE,
        env={**os.environ, "LC_ALL": "C"},
        check=False,
    )
    sys.stdout.buffer.write(result.stdout)
    sys.stdout.buffer.flush()
    result.check_returncode()
    entries = re.findall(
        r"^\s*0x[0-9a-fA-F]+\s+\((SONAME|NEEDED)\)[^\[\r\n]*\[([^\]]+)\]",
        result.stdout.decode("utf-8", errors="replace"),
        re.MULTILINE,
    )
    sonames = [name for tag, name in entries if tag == "SONAME"]
    needed = [name for tag, name in entries if tag == "NEEDED"]
    if sonames != ["libgcc_s.so.1"]:
        raise ValueError(f"expected libgcc_s.so.1 SONAME: {source_path}")
    if "libc.so.6" not in needed:
        raise ValueError(f"libgcc must have DT_NEEDED libc.so.6: {source_path}")
    libgcc_symbols = read_dynamic_symbols(source_path)
    provider_symbols = read_dynamic_symbols(libc_path) + read_dynamic_symbols(loader_path)
    provided = {
        (name, version) for name, version, binding, visibility, index in provider_symbols
        if index != "UND" and binding in ("GLOBAL", "WEAK", "UNIQUE")
        and visibility in ("DEFAULT", "PROTECTED")
    }
    required = {
        (name, version) for name, version, binding, _, index in libgcc_symbols
        if index == "UND" and binding != "WEAK" and version.startswith("GLIBC_")
    }
    missing = sorted(required - provided)
    if missing:
        names = ", ".join(f"{name}@{version}" for name, version in missing)
        raise ValueError(
            f"libgcc requires unavailable glibc symbols: {names}; "
            f"checked {libc_path} and {loader_path}"
        )
    unwind_exports = {
        name for name, _, binding, visibility, index in libgcc_symbols
        if index != "UND" and binding in ("GLOBAL", "WEAK", "UNIQUE")
        and visibility in ("DEFAULT", "PROTECTED")
    }
    missing_unwind = sorted({
        "_Unwind_Backtrace", "_Unwind_ForcedUnwind", "_Unwind_GetCFA",
        "_Unwind_Resume", "__gcc_personality_v0",
    } - unwind_exports)
    if missing_unwind:
        raise ValueError(f"libgcc missing unwinder exports: {', '.join(missing_unwind)}")


def reject_source_overlap(source, outputs):
    """拒绝输入与任何输出位置相互包含或指向同一文件。"""
    for output in outputs:
        if (source == output or source in output.parents or output in source.parents
                or source.exists() and output.exists() and source.samefile(output)):
            raise ValueError(f"source and output overlap or alias: {source}, {output}")


def file_sha256(path):
    """流式计算源文件的 SHA-256，不修改源文件。"""
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        while block := stream.read(1024 * 1024):
            digest.update(block)
    return digest.hexdigest()


def builtin_locale_directory(architecture, libc_path):
    """确认内置 locale 的文件清单及其绑定的官方 libc 身份。"""
    libc_hash, locale_hash = BUILTIN_LOCALE_HASHES[architecture]
    if file_sha256(libc_path) != libc_hash:
        raise ValueError(
            f"built-in {architecture} locale requires the matching official libc: {libc_path}; "
            "specify --glibc-locale-dir for a different libc"
        )
    source = Path(__file__).resolve().parent
    for component in ("locale", architecture, "C.utf8"):
        source = source / component
        if source.is_symlink() or not source.is_dir():
            raise ValueError(f"built-in locale path must be an ordinary directory: {source}")
    files = []
    pending = [source]
    while pending:
        path = pending.pop()
        if path.is_symlink():
            raise ValueError(f"built-in locale contains a symbolic link: {path}")
        if path.is_dir():
            pending.extend(path.iterdir())
        elif path.is_file():
            files.append(path)
        else:
            raise ValueError(f"built-in locale contains a non-regular entry: {path}")
    if len(files) != len(LOCALE_CATEGORIES):
        raise ValueError(f"built-in locale must contain 12 category files: {source}")
    files.sort(key=lambda path: path.relative_to(source).as_posix())
    manifest = "".join(
        f"{file_sha256(path)}  {path.relative_to(source).as_posix()}\n" for path in files
    )
    try:
        manifest_hash = hashlib.sha256(manifest.encode("ascii")).hexdigest()
    except UnicodeEncodeError as error:
        raise ValueError(f"built-in locale contains a non-ASCII path: {source}") from error
    if manifest_hash != locale_hash:
        raise ValueError(f"built-in locale manifest SHA-256 mismatch: {source}")
    return source


def check_locale_tree(source, outputs, ancestors):
    """检查待复制树及链接目标，拒绝循环、特殊文件和输入输出重叠。"""
    source = source.resolve(strict=True)
    reject_source_overlap(source, outputs)
    if source.is_dir():
        if source in ancestors:
            raise ValueError(f"locale directory contains a symbolic-link cycle: {source}")
        for entry in source.iterdir():
            check_locale_tree(entry, outputs, ancestors | {source})
    elif source.is_file():
        with source.open("rb") as stream:
            stream.read(1)
    else:
        raise ValueError(f"locale source is not a regular file or directory: {source}")


def check_locale_directory(source, outputs):
    """验证 glibc locale 的十二个类别及可复制的完整目录树。"""
    source = source.resolve(strict=True)
    if not source.is_dir():
        raise ValueError(f"locale source is not a directory: {source}")
    check_locale_tree(source, outputs, set())
    for name, category in LOCALE_CATEGORIES:
        path = source / name
        if name == "LC_MESSAGES":
            if not path.is_dir():
                raise ValueError(f"locale category must be a directory: {path}")
            path = path / "SYS_LC_MESSAGES"
        if not path.is_file():
            raise ValueError(f"locale category is not a regular file: {path}")
        with path.open("rb") as stream:
            header = stream.read(8)
            size = os.fstat(stream.fileno()).st_size
            if len(header) != 8:
                raise ValueError(f"invalid locale category header: {path}")
            magic, count = struct.unpack("<II", header)
            base = 0x20090720 if category == 0 else (
                0x20051014 if category == 3 else 0x20031115
            )
            if magic != base ^ category or count == 0 or size <= 8 + count * 4:
                raise ValueError(f"invalid locale category header: {path}")
            indexes = stream.read(count * 4)
            if (len(indexes) != count * 4
                    or any(size < offset for (offset,) in struct.iter_unpack("<I", indexes))):
                raise ValueError(f"invalid locale category indexes: {path}")
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


def copy_runtime_library(source_path, destination_path):
    """通过同目录临时文件复制并原子替换目标运行库，避免跟随目标路径上可能已有的符号链接。"""
    with tempfile.NamedTemporaryFile(
        dir=destination_path.parent, prefix=f".{destination_path.name}.", delete=False
    ) as stream:
        temporary_path = Path(stream.name)
    try:
        shutil.copy2(source_path, temporary_path)
        temporary_path.replace(destination_path)
    finally:
        temporary_path.unlink(missing_ok=True)
    print(f"Copied: {source_path} -> {destination_path}")


def install_loader_link(link_path, target):
    """在 `link_path` 创建指向 `target` 的符号链接；若原路径为普通文件或符号链接则先移除。"""
    if link_path.is_symlink() or link_path.is_file():
        link_path.unlink()
    link_path.symlink_to(target)
    print(f"Linked: {link_path} -> {target}")


def prepare_runtime_libraries(architecture, musl_libc_path, glibc_library_directory,
                              output_directory, glibc_locale_directory=None, libgcc_s_path=None):
    """校验指定来源并安装动态库及 locale，默认 locale 仅匹配固定身份的官方 libc。"""
    if architecture not in RUNTIME_LIBRARY_SOURCES:
        raise ValueError(f"unsupported architecture: {architecture}")
    _, _, expected_machine, musl_loader, glibc_loader = RUNTIME_LIBRARY_SOURCES[architecture]
    glibc_library_directory = Path(glibc_library_directory).resolve(strict=True)
    if not glibc_library_directory.is_dir():
        raise ValueError(f"glibc source is not a directory: {glibc_library_directory}")
    if libgcc_s_path is None:
        libgcc_s_path = glibc_library_directory / "libgcc_s.so.1"
    output_directory = Path(output_directory).absolute()
    locale_lib = "lib" if architecture == "riscv64" else "lib64"
    locale_destination = output_directory / "glibc/usr" / locale_lib / "locale/C.utf8"
    libraries = [
        (musl_libc_path, output_directory / "musl/lib/libc.so"),
        (glibc_library_directory / "libc.so.6", output_directory / "glibc/lib/libc.so.6"),
        (glibc_library_directory / "libm.so.6", output_directory / "glibc/lib/libm.so.6"),
        (glibc_library_directory / glibc_loader, output_directory / "glibc/lib" / glibc_loader),
        (libgcc_s_path, output_directory / "glibc/lib/libgcc_s.so.1"),
    ]
    links = [output_directory / "musl/lib" / musl_loader]
    if architecture == "loongarch64":
        links.extend((output_directory / "musl/lib64", output_directory / "glibc/lib64"))
    outputs = [path.resolve() for path in (
        [output_directory, locale_destination] + [destination for _, destination in libraries] + links
    )]
    reject_source_overlap(glibc_library_directory, outputs)
    resolved_libraries = []
    for source_path, destination_path in libraries:
        source_path = Path(source_path).resolve(strict=True)
        if not source_path.is_file():
            raise ValueError(f"required source is not a regular file: {source_path}")
        reject_source_overlap(source_path, outputs)
        resolved_libraries.append((source_path, destination_path))
    if glibc_locale_directory is None:
        glibc_locale_directory = builtin_locale_directory(architecture, resolved_libraries[1][0])
    locale_source = check_locale_directory(Path(glibc_locale_directory), outputs)
    if locale_destination.is_symlink() or (
            locale_destination.exists() and not locale_destination.is_dir()):
        raise ValueError(f"locale output must be an ordinary directory: {locale_destination}")
    # 所有源文件和 locale 均在写出前完成校验。
    for source_path, _ in resolved_libraries:
        check_runtime_library_elf(source_path, expected_machine)
    check_libgcc_s(resolved_libraries[4][0], resolved_libraries[1][0],
                   resolved_libraries[3][0], architecture)
    print("Static symbol checks do not prove guest dynamic loading or locale compatibility; "
          "guest dlopen and setlocale are required.")
    for source_path, destination_path in resolved_libraries:
        destination_path.parent.mkdir(parents=True, exist_ok=True)
        copy_runtime_library(source_path, destination_path)
    install_loader_link(output_directory / "musl/lib" / musl_loader, "libc.so")
    if architecture == "loongarch64":
        install_loader_link(output_directory / "musl/lib64", "lib")
        install_loader_link(output_directory / "glibc/lib64", "lib")
    publish_locale_directory(locale_source, locale_destination)
    print(f"Runtime libraries prepared: arch={architecture} output_directory={output_directory}")


def main(argv=None):
    """命令行入口：解析架构与运行库路径参数并执行准备流程，失败时输出错误并返回非零退出码。"""
    parser = argparse.ArgumentParser(description=__doc__, allow_abbrev=False)
    parser.add_argument("--arch", choices=RUNTIME_LIBRARY_SOURCES, required=True)
    parser.add_argument("--musl-libc", type=Path)
    parser.add_argument("--glibc-libdir", type=Path)
    parser.add_argument("--libgcc-s", type=Path)
    parser.add_argument("--glibc-locale-dir", type=Path)
    parser.add_argument("--output-directory", type=Path, required=True)
    args = parser.parse_args(argv)
    default_musl, default_glibc, _, _, _ = RUNTIME_LIBRARY_SOURCES[args.arch]
    libgcc_s_path = args.libgcc_s
    if libgcc_s_path is None and args.glibc_libdir is None:
        libgcc_s_path = Path(DEFAULT_LIBGCC_S[args.arch])
    try:
        prepare_runtime_libraries(
            args.arch,
            args.musl_libc if args.musl_libc is not None else Path(default_musl),
            args.glibc_libdir if args.glibc_libdir is not None else Path(default_glibc),
            args.output_directory,
            args.glibc_locale_dir,
            libgcc_s_path,
        )
    except (OSError, ValueError, RuntimeError, shutil.Error, subprocess.CalledProcessError) as error:
        print(f"Runtime library preparation failed: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
