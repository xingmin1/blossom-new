#!/usr/bin/env python3
"""在 build 输出域用普通文件、mke2fs/debugfs/e2fsck 创建只读 ext4 镜像。"""

import argparse
import os
from pathlib import Path
import re
import shutil
import subprocess
import tempfile

ROOT = Path(__file__).resolve().parents[1]
PROFILES = {
    "legacy1k": (1024, 128, "none,ext_attr,resize_inode,dir_index,filetype,sparse_super,large_file,uninit_bg"),
    "metadata2k": (2048, 256, "none,ext_attr,resize_inode,dir_index,filetype,sparse_super,large_file,extent,metadata_csum,extra_isize"),
    "seed4k": (4096, 256, "none,ext_attr,resize_inode,dir_index,filetype,sparse_super,large_file,extent,64bit,metadata_csum,metadata_csum_seed,extra_isize"),
}
PATTERN_SIZE = 65573
DIRECTORY_ENTRIES = 12000
DEEP_EXTENTS = 1500


def output_path(value, replace=False):
    """仅允许 build 内普通输出；生产镜像可替换，夹具仍要求全新目录。"""
    path = Path(os.path.abspath(value))
    base = ROOT / "build"
    if (base not in path.parents or path.is_symlink()
            or (path.exists() and (not replace or not path.is_file()))):
        raise ValueError("--output must be a regular path below repository build/")
    log = path.with_suffix(".log")
    if log.is_symlink() or (log.exists() and (not replace or not log.is_file())):
        raise ValueError("output log must be a regular file")
    if any(parent.is_symlink() for parent in path.parents):
        raise ValueError("output ancestors must not be symbolic links")
    path.parent.mkdir(parents=True, exist_ok=True)
    return path


def command(arguments, log, accepted=(0,)):
    """保留工具实际输出，只有显式允许的工具状态才继续制盘。"""
    result = subprocess.run([str(item) for item in arguments], check=False,
                            stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                            env={**os.environ, "LC_ALL": "C", "E2FSPROGS_FAKE_TIME": "1700000000"})
    with log.open("ab") as stream:
        stream.write(("$ " + " ".join(map(str, arguments)) + "\n").encode())
        stream.write(result.stdout)
    if result.returncode not in accepted:
        raise subprocess.CalledProcessError(result.returncode, arguments, output=result.stdout)
    return result.stdout.decode("ascii", errors="replace")


def debug_result(text, request):
    """debugfs 部分命令用零退出报告错误，保留原文本并拒绝明确错误诊断。"""
    if re.search(r"(?:Command not found|Usage:|Invalid field|Unknown feature|Filesystem not open|File not found|while (?:opening|setting|allocating|punching))", text, re.IGNORECASE):
        raise ValueError(f"debugfs rejected request: {request}")
    return text


def debug(image, request):
    """只修改本次生成的输出镜像，读取查询也保留 debugfs 原始证据。"""
    return debug_result(command(["debugfs", "-w", "-R", request, image],
                                image.with_suffix(".log")), request)


def punch_holes(image, block_size):
    """子块 hole 用工具明确 punch，避免依赖宿主 4KiB SEEK_HOLE 的粒度。"""
    with tempfile.NamedTemporaryFile(mode="w", encoding="ascii", prefix="holes-",
                                     suffix=".commands", dir=image.parent) as requests:
        for logical in range(1, (DEEP_EXTENTS - 1) * 2, 2):
            requests.write(f"punch /deep {logical} {logical}\n")
        requests.write(f"punch /sparse {4096 // block_size} {12288 // block_size - 1}\n")
        requests.flush()
        text = command(["debugfs", "-w", "-f", requests.name, image], image.with_suffix(".log"))
        debug_result(text, "punch sparse logical blocks")


def pattern(offset, length):
    """生成文件逻辑偏移对应的非零周期图样，内核按同一外部数据合同核验。"""
    return bytes((37 + (position // 512) * 17 + position % 512) & 255
                 for position in range(offset, offset + length))


def write_pattern(path, length):
    """分段写普通 staging 文件，避免把整盘数据保留在宿主内存。"""
    with path.open("xb") as stream:
        for offset in range(0, length, 65536):
            stream.write(pattern(offset, min(65536, length - offset)))



def system_staging(directory, userland_root):
    """首次复制已构建 bin/sbin/usr 和普通系统配置；已有 staging 不读取或安装任何输入。"""
    if directory.exists():
        if not directory.is_dir():
            raise ValueError("--staging must be a directory")
        return directory
    for name in ("bin", "sbin", "usr"):
        if not (userland_root / name).is_dir():
            raise ValueError(f"built userland directory missing: {userland_root / name}")
    for name in ("bin/busybox", "bin/sh", "sbin/init", "bin/libc-smoke"):
        if not (userland_root / name).is_file():
            raise ValueError(f"build the ordinary userland first: {userland_root / name}")
    directory.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="system-staging-", dir=directory.parent) as temporary:
        tree = Path(temporary) / "root"
        tree.mkdir(mode=0o755)
        tree.chmod(0o755)
        for name in ("bin", "sbin", "usr"):
            shutil.copytree(userland_root / name, tree / name, symlinks=True)
        shutil.copytree(ROOT / "userland" / "system" / "etc", tree / "etc", symlinks=True)
        for name in ("dev", "proc", "run", "tmp", "var", "var/tmp", "root", "home",
                     "usr/bin", "usr/sbin", "mnt", "mnt/extra"):
            (tree / name).mkdir(parents=True, exist_ok=True)
            (tree / name).chmod(0o755)
        (tree / "tmp").chmod(0o1777)
        (tree / "var" / "tmp").chmod(0o1777)
        os.rename(tree, directory)
    return directory


def check_system_image(image, profile):
    """核实际超级块几何，并保留正式只读工具的格式与目录检查结果。"""
    with image.open("rb") as stream:
        stream.seek(1024)
        superblock = stream.read(1024)
    block_size, inode_size, _ = PROFILES[profile]
    if (len(superblock) != 1024 or superblock[56:58] != b"\x53\xef"
            or int.from_bytes(superblock[24:28], "little") != block_size.bit_length() - 11
            or int.from_bytes(superblock[88:90], "little") != inode_size):
        raise ValueError("generated superblock does not match requested geometry")
    log = image.with_suffix(".log")
    command(["dumpe2fs", "-h", image], log)
    debug_result(command(["debugfs", "-R", "stat /", image], log), "stat /")
    command(["e2fsck", "-fn", image], log)


def system_image(source, output, profile, size_mib, write_fixture=False, nearly_full=False):
    """新镜像完整制盘并检查后才原子替换目标，普通写夹具也使用同一制盘工具。"""
    if source == output.parent or source in output.parents:
        raise ValueError("output must not be inside the source tree")
    with tempfile.TemporaryDirectory(prefix="system-image-", dir=output.parent) as temporary:
        image = Path(temporary) / "system.img"
        try:
            create_image(source, image, profile, size_mib)
            if write_fixture:
                block_size = PROFILES[profile][0]
                preallocated = () if profile == "legacy1k" else (
                    ("write-unwritten", 16384 // block_size), ("write-merge", 64),
                )
                for name, blocks in preallocated:
                    debug(image, f"fallocate /test/{name} 0 {blocks - 1}")
                    for logical in range(blocks):
                        physical = physical_block(image, f"test/{name}", logical)
                        with image.open("r+b") as stream:
                            stream.seek(physical * block_size)
                            stream.write(b"\xa7" * block_size)
                    debug(image, f"set_inode_field /test/{name} size {blocks * block_size}")
                if nearly_full:
                    with image.open("rb") as stream:
                        stream.seek(1024 + 12)
                        free = int.from_bytes(stream.read(4), "little")
                    if free <= 256:
                        raise ValueError("ordinary ENOSPC fixture has insufficient initial free blocks")
                    debug(image, f"fallocate /test/write-reserve 0 {free - 129}")
                    debug(image, f"set_inode_field /test/write-reserve size {(free - 128) * block_size}")
            check_system_image(image, profile)
        except (ValueError, OSError, subprocess.CalledProcessError):
            log = image.with_suffix(".log")
            if log.exists():
                os.replace(log, output.with_suffix(".log"))
            raise
        os.replace(image.with_suffix(".log"), output.with_suffix(".log"))
        os.replace(image, output)


def write_staging(directory, arch):
    """普通可编辑 source 保留新编译目标 ELF，独立文件供每个写消费者使用。"""
    program = ROOT / "build" / f"user-{arch}" / "user-write"
    if not program.is_file():
        raise ValueError(f"build the ordinary user program first: {program}")
    test = directory / "test"
    (test / "bin").mkdir(parents=True, exist_ok=True)
    target = test / "bin" / "user-write"
    if target.exists():
        raise ValueError("write fixture requires a fresh source copy")
    shutil.copy2(program, target)
    shutil.copy2(program, test / "bin" / "user-write-next")
    for name in ("write-kernel", "write-user", "write-append", "write-append-kernel",
                 "write-oom", "write-unwritten", "write-merge", "write-tree", "write-rollback", "write-full", "write-reserve"):
        (test / name).touch(exist_ok=False)
    (test / "write-map").write_bytes(bytes(range(256)) * 32)
    (test / "write-map").chmod(0o755)

def staging(directory, block_size):
    """从普通文件构造链接、字节名、稀疏文件和真实多层映射的输入树。"""
    directory.mkdir()
    write_pattern(directory / "pattern", PATTERN_SIZE)
    os.link(directory / "pattern", directory / "hardlink")
    (directory / "empty").touch()
    (directory / "unwritten").touch()
    os.symlink("pattern", directory / "fastlink")
    os.symlink("t" * 180, directory / "blocklink")
    raw = os.fsencode(directory) + b"/name-\xff\xfe"
    with open(raw, "xb") as stream:
        stream.write(b"raw-name\n")
    walk = directory / "walk"
    walk.mkdir()
    (walk / "sub").mkdir()
    (directory / "mnt").mkdir()
    (directory / "mnt" / "extra").mkdir()
    for name, target in (
        ("relative", "../pattern"),
        ("absolute", "/pattern"),
        ("directory", "sub"),
        ("loop", "loop"),
    ):
        os.symlink(target, walk / name)
    for index in range(1, 42):
        target = f"chain{index + 1:02d}" if index < 41 else "../pattern"
        os.symlink(target, walk / f"chain{index:02d}")
    with (directory / "sparse").open("xb") as stream:
        stream.write(pattern(0, 4096))
        stream.seek(12288)
        stream.write(pattern(12288, 37))
    with (directory / "deep").open("xb") as stream:
        for index in range(DEEP_EXTENTS):
            offset = index * 2 * block_size
            stream.seek(offset)
            stream.write(pattern(offset, block_size))
    if block_size == 1024:
        with (directory / "indirect").open("xb") as stream:
            for logical in (0, 12, 268, 65804):
                offset = logical * block_size
                stream.seek(offset)
                stream.write(pattern(offset, block_size))
    plain = directory / "plain"
    plain.mkdir()
    for index in range(40):
        (plain / f"item-{index:04d}").touch()
    indexed = directory / "indexed"
    indexed.mkdir()
    for index in range(DIRECTORY_ENTRIES):
        (indexed / (f"item-{index:05d}-" + "n" * 180)).touch()


def create_image(source, output, profile, size_mib=256):
    """生产制盘入口只读取 staging，独占新输出文件，不需要挂载或特权。"""
    block_size, inode_size, features = PROFILES[profile]
    with output.open("xb") as stream:
        stream.truncate(size_mib * 1024 * 1024)
    log = output.with_suffix(".log")
    command(["mke2fs", "-q", "-F", "-t", "ext4", "-b", block_size,
             "-I", inode_size, "-N", 24000, "-O", features,
             "-E", "lazy_itable_init=0", "-L", profile,
             "-U", "ae649880-7392-4424-a21c-392fdc0d1357", "-d", source, output], log)


def checked_image(image):
    """目录优化使用正式工具生成 dx 及 CRC，第二次只读检查必须干净退出。"""
    command(["e2fsck", "-fyD", image], image.with_suffix(".log"), accepted=(0, 1))
    command(["e2fsck", "-fn", image], image.with_suffix(".log"))
    command(["dumpe2fs", "-h", image], image.with_suffix(".log"))
    for name in ("pattern", "deep", "indexed", "fastlink", "blocklink", "unwritten"):
        debug(image, f"stat /{name}")
    debug(image, "htree_dump /indexed")
    if "legacy1k" not in image.name:
        debug(image, "dump_extents /deep")


def fixture_image(output, profile, size_mib):
    """每种几何独立制盘，预分配区由 debugfs 建立且保持 unwritten 状态。"""
    block_size = PROFILES[profile][0]
    with tempfile.TemporaryDirectory(prefix="staging-", dir=output.parent) as temporary:
        source = Path(temporary) / "root"
        staging(source, block_size)
        create_image(source, output, profile, size_mib)
    punch_holes(output, block_size)
    if profile != "legacy1k":
        debug(output, f"fallocate /unwritten 0 {16384 // block_size - 1}")
    debug(output, "set_inode_field /unwritten size 16384")
    checked_image(output)
    if profile != "legacy1k":
        stat = debug(output, "stat /deep")
        # ETB1 是实际二层树节点；格式来自 debugfs，不实现内核 extent 解码。
        if "(ETB1)" not in stat:
            raise ValueError("debugfs did not produce a depth-two extent tree")
    tree = debug(output, "htree_dump /indexed")
    if not re.search(r"Indirect levels:\s*[1-9]", tree):
        raise ValueError("e2fsck did not produce a deeper indexed directory")


def duplicate(source, name):
    """破坏副本只从本次正盘复制到全新输出文件，原正盘始终保留。"""
    target = source.with_name(name + ".img")
    with source.open("rb") as input_file, target.open("xb") as output:
        shutil.copyfileobj(input_file, output, 1024 * 1024)
    return target


def flip(image, offset):
    """翻转指定校验或内容字节，故意保留旧 CRC，不复制格式校验算法。"""
    with image.open("r+b") as stream:
        stream.seek(offset)
        original = stream.read(1)
        if len(original) != 1:
            raise ValueError("corruption offset outside generated image")
        stream.seek(offset)
        stream.write(bytes([original[0] ^ 1]))
    with image.with_suffix(".log").open("ab") as stream:
        stream.write(f"corruption byte-offset={offset} old={original[0]:02x} new={original[0] ^ 1:02x} CRC-retained=1\n".encode())


def inode_offset(image, name, block_size):
    """inode 的实际位置由 debugfs imap 给出，不自行重算组布局。"""
    text = debug(image, f"imap /{name}")
    found = re.search(r"located at block\s+(\d+),\s+offset\s+(0x[0-9a-fA-F]+)", text)
    if found is None:
        raise ValueError("debugfs imap did not report inode position")
    return int(found[1]) * block_size + int(found[2], 16)


def physical_block(image, name, logical):
    """实际物理地址只消费 bmap 的完整结果，unwritten 的合法标记不改变地址。"""
    text = debug(image, f"bmap /{name} {logical}")
    numbers = re.findall(r"^([0-9]+)(?: \(uninit\))?$", text, re.MULTILINE)
    if len(numbers) != 1 or int(numbers[0]) == 0:
        raise ValueError("debugfs bmap did not report one mapped block")
    return int(numbers[0])


def overwrite(image, offset, data):
    """有界覆盖本次独立副本的格式字段，旧内容与写入范围保留在工具日志。"""
    with image.open("r+b") as stream:
        stream.seek(offset)
        old = stream.read(len(data))
        if len(old) != len(data):
            raise ValueError("mutation range outside generated image")
        stream.seek(offset)
        stream.write(data)
    with image.with_suffix(".log").open("ab") as stream:
        stream.write(f"mutation byte-offset={offset} length={len(data)} old-prefix={old[:16].hex()} new-prefix={data[:16].hex()} CRC-retained=1\n".encode())


def corrupt_indirect_data(image):
    """只破坏合法 IND 节点的首个 data 指针，inode root 与前三文件页保持有效。"""
    text = debug(image, "stat /pattern")
    found = re.search(r"\(IND\):([0-9]+)", text)
    if found is None:
        raise ValueError("debugfs did not report pattern single-indirect node")
    overwrite(image, int(found[1]) * 1024, (0xffffffff).to_bytes(4, "little"))


def damaged_images(directory):
    """各错误类型独立镜像保留证据，结构修改由 debugfs 重算所属 inode/super CRC。"""
    source = directory / "seed4k.img"
    block_size = 4096
    flip(duplicate(source, "seed4k-bad-super"), 1024 + 1020)
    flip(duplicate(source, "seed4k-bad-group"), block_size + 30)
    inode = duplicate(source, "seed4k-bad-inode")
    flip(inode, inode_offset(inode, "pattern", block_size) + 124)
    extent = duplicate(source, "seed4k-bad-extent")
    tree = debug(extent, "stat /deep")
    node = re.search(r"\(ETB[01]\):([0-9]+)", tree)
    if node is None:
        raise ValueError("debugfs did not report an external extent block")
    flip(extent, int(node[1]) * block_size + block_size - 4)
    directory_image = duplicate(source, "seed4k-bad-dir")
    flip(directory_image, physical_block(directory_image, "plain", 0) * block_size + block_size - 4)
    dx = duplicate(source, "seed4k-bad-dx")
    flip(dx, physical_block(dx, "indexed", 0) * block_size + block_size - 4)
    layout = duplicate(source, "seed4k-bad-layout")
    debug(layout, "set_super_value inode_size 192")
    feature = duplicate(source, "seed4k-bad-features")
    debug(feature, "feature encrypt")
    for field in ("compat", "ro_compat"):
        unknown = duplicate(source, f"seed4k-unknown-{field}")
        command(["dumpe2fs", "-h", unknown], unknown.with_suffix(".log"))
        # ssv 保留正盘已知位，同时加入只读必须允许的未知位；CRC 由工具更新。
        offset = 92 if field == "compat" else 100
        with unknown.open("rb") as stream:
            stream.seek(1024 + offset)
            value = int.from_bytes(stream.read(4), "little")
        debug(unknown, f"set_super_value feature_{field} {value | 0x80000000}")
    legacy = directory / "legacy1k.img"
    flip(duplicate(legacy, "legacy1k-bad-group"), 2 * 1024 + 30)
    malformed = duplicate(legacy, "legacy1k-bad-layout")
    corrupt_indirect_data(malformed)
    prefix = duplicate(legacy, "legacy1k-bad-prefix")
    corrupt_indirect_data(prefix)
    malformed_extent = duplicate(source, "seed4k-bad-extent-layout")
    debug(malformed_extent, "set_inode_field /pattern block[0] 0")
    malformed_dir = duplicate(legacy, "legacy1k-bad-dir-layout")
    with malformed_dir.open("r+b") as stream:
        stream.seek(physical_block(malformed_dir, "plain", 0) * 1024 + 4)
        stream.write(b"\x00\x00")
    malformed_dx = duplicate(legacy, "legacy1k-bad-dx-layout")
    with malformed_dx.open("r+b") as stream:
        stream.seek(physical_block(malformed_dx, "indexed", 0) * 1024 + 34)
        stream.write(b"\x00\x00")
    for image in sorted(directory.glob("*-bad-*.img")):
        print(f"corrupt root={image} hardware-EIO-covered=0")


def main(argv=None):
    """普通系统盘只在无 source 的首次 staging 安装用户树，既有夹具保持独立生成。"""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", required=True)
    parser.add_argument("--profile", choices=PROFILES, default="seed4k")
    parser.add_argument("--source", type=Path)
    parser.add_argument("--arch", choices=("riscv64", "loongarch64"), default="riscv64")
    parser.add_argument("--staging", type=Path)
    parser.add_argument("--userland-root", type=Path,
                        help="built bin/sbin/usr input for a new ordinary staging tree")
    parser.add_argument("--identity", choices=("root", "extra"), default="root")
    parser.add_argument("--size-mib", type=int, default=256)
    parser.add_argument("--fixtures", action="store_true")
    parser.add_argument("--write-fixture", action="store_true",
                        help="add ordinary writable files and the freshly built user-write ELF")
    parser.add_argument("--nearly-full", action="store_true",
                        help="reserve real ordinary data blocks, leaving at most 128 free")
    args = parser.parse_args(argv)
    if args.size_mib < 128:
        parser.error("--size-mib must be at least 128")
    if args.write_fixture and args.fixtures:
        parser.error("--write-fixture uses a normal single source image, not damaged fixtures")
    if args.nearly_full and not args.write_fixture:
        parser.error("--nearly-full requires --write-fixture")
    if args.fixtures and (args.source is not None or args.staging is not None):
        parser.error("--fixtures creates its own ordinary staging tree")
    if args.source is not None and args.staging is not None:
        parser.error("--source and --staging are mutually exclusive")
    if not args.fixtures and Path(args.output).suffix != ".img":
        parser.error("production --output must end in .img")
    try:
        output = output_path(args.output, replace=not args.fixtures)
        for binary in ("mke2fs", "debugfs", "e2fsck", "dumpe2fs"):
            if shutil.which(binary) is None:
                raise ValueError(f"required tool missing: {binary}")
        if args.fixtures:
            output.mkdir()
            for profile in PROFILES:
                image = output / f"{profile}.img"
                fixture_image(image, profile, args.size_mib)
                print(f"valid root={image} profile={profile}")
            damaged_images(output)
        else:
            source = args.source
            if source is None:
                directory = args.staging or ROOT / "build" / f"system-{args.arch}-{args.identity}"
                userland_root = args.userland_root or ROOT / "build" / f"userland-{args.arch}"
                source = system_staging(directory, userland_root)
            if not source.is_dir():
                raise ValueError("--source must be an existing directory")
            if args.write_fixture:
                write_staging(source, args.arch)
            system_image(source.resolve(), output, args.profile, args.size_mib,
                         args.write_fixture, args.nearly_full)
            print(f"valid root={output} profile={args.profile} source={source.resolve()}")
    except (ValueError, OSError, subprocess.CalledProcessError) as error:
        parser.exit(1, f"mkext4: {error}\n")


if __name__ == "__main__":
    main()
