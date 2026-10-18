#!/usr/bin/env python3
"""从带符号 ELF 生成离线板镜像；不连接板卡、不修改固件或物理介质。"""

import argparse
import os
from pathlib import Path
import re
import shutil
import stat
import struct
import tempfile
import zlib

from ktest.boot import elf_layout
from ktest.catalog import HostError, require

LEGACY = struct.Struct(">7I4B32s")
U32_LIMIT = 1 << 32
CHUNK = 1024 * 1024


def regular(path):
    """包装输入必须是普通文件，不读取设备、目录或管道。"""
    require(stat.S_ISREG(path.stat().st_mode), f"not a regular file: {path}")


def align(address):
    """暂存对象按页分离；此布局不声明目标板的可用 RAM 容量。"""
    return (address + 4095) & ~4095


def physical_entry(layout):
    """通过实际可执行 LOAD 的地址映射取得固件使用的物理入口。"""
    entry = layout["entry"]
    matches = [load for load in layout["loads"] if load["flags"] == 5
               and load["address"] <= entry < load["address"] + load["file_size"]]
    require(len(matches) == 1, "kernel entry lacks a unique executable LOAD")
    load = matches[0]
    return load["physical"] + entry - load["address"]


def raw_kernel(kernel, output, layout):
    """仅复制已验证 LOAD 的文件字节，按物理间隔补零，不把调试节放入 raw。"""
    loads = layout["loads"]
    start = min(load["physical"] for load in loads)
    end = max(load["physical"] + load["file_size"] for load in loads)
    require(0 < end - start < U32_LIMIT, "kernel raw span exceeds legacy size limit")
    with kernel.open("rb") as source, output.open("xb") as target:
        for load in loads:
            source.seek(load["offset"])
            target.seek(load["physical"] - start)
            remaining = load["file_size"]
            while remaining:
                data = source.read(min(CHUNK, remaining))
                require(data, "kernel LOAD became truncated")
                target.write(data)
                remaining -= len(data)
        target.truncate(end - start)
    return start


def legacy_header(size, checksum, arch, kind, load, entry, epoch):
    """生成无压缩 legacy 头；所有长度、地址和时间戳保留真实的 32 位边界错误。"""
    require(all(0 <= value < U32_LIMIT for value in (size, checksum, load, entry, epoch)),
            "legacy image size/address/timestamp exceeds 32 bits")
    name = b"blossom-kernel" if kind == 2 else b"blossom-root"
    fields = (0x27051956, 0, epoch, size, load, entry, checksum, 5, arch, kind, 0,
              name.ljust(32, b"\0"))
    header = LEGACY.pack(*fields)
    return LEGACY.pack(fields[0], zlib.crc32(header), *fields[2:])


def legacy_image(source, output, arch, kind, load, entry, epoch):
    """流式复制实际 payload 并计算 CRC，避免将整个 ext4 卷同时放入宿主内存。"""
    size = source.stat().st_size
    require(0 < size < U32_LIMIT, "legacy image payload is empty or exceeds 32 bits")
    checksum = 0
    copied = 0
    with source.open("rb") as payload, output.open("xb") as image:
        image.write(bytes(LEGACY.size))
        while data := payload.read(CHUNK):
            checksum = zlib.crc32(data, checksum)
            image.write(data)
            copied += len(data)
        require(copied == size, "legacy image input size changed while reading")
        image.seek(0)
        image.write(legacy_header(size, checksum, arch, kind, load, entry, epoch))
    return size + LEGACY.size


def ext4_input(path):
    """RAM 根只包装未压缩 raw ext4；本项目的普通卷不带 journal 或待恢复标记。"""
    regular(path)
    with path.open("rb") as source:
        source.seek(1024)
        superblock = source.read(1024)
    require(len(superblock) == 1024 and superblock[56:58] == b"\x53\xef",
            "ramdisk must be an uncompressed raw ext4 volume")
    compatible, incompatible = struct.unpack_from("<II", superblock, 92)
    require(not compatible & 4 and not incompatible & 4,
            "ramdisk journal/recovery features are not supported")


def boot_environment(args, layout, raw, directory, epoch):
    """以 ELF 完整内存范围和输入长度安排互不重叠的暂存区，输出唯一地址说明。"""
    arch = 26 if args.arch == "riscv64" else 27
    load = min(segment["physical"] for segment in layout["loads"])
    memory_end = max(segment["physical"] + segment["memory_size"]
                     for segment in layout["loads"])
    entry = physical_entry(layout)
    staging = align(memory_end)
    kernel_size = legacy_image(raw, directory / "kernel.uImage", arch, 2, load, entry, epoch)
    spans = [(load, memory_end, "kernel memory"),
             (staging, staging + kernel_size, "kernel container")]
    rows = ["# 仅为离线装载布局；装载前须核对实际 RAM、固件及保留区范围。",
            "# 本次生成未修改固件环境或物理介质。",
            "# OUTPUT 是内核 ELF，IMAGE 是卷输出，SOURCE 是源目录。",
            "# 单架构板卷使用 rootdir=/。",
            f"blossom_platform={args.platform}", "blossom_rootdir=/",
            "kernel_file=kernel.uImage", f"kernel_addr_r={staging:#x}",
            f"kernel_load={load:#x}", f"kernel_entry={entry:#x}",
            f"kernel_elf_entry={layout['entry']:#x}",
            f"kernel_raw_size={raw.stat().st_size:#x}", f"kernel_image_size={kernel_size:#x}",
            f"kernel_mem_end={memory_end:#x}"]
    cursor = align(staging + kernel_size)
    ramdisk_argument = "-"
    if args.ramdisk:
        ext4_input(args.ramdisk)
        payload = cursor + LEGACY.size
        size = legacy_image(args.ramdisk, directory / "root.uInitrd", arch, 3,
                            payload, payload, epoch)
        spans.append((cursor, cursor + size, "ramdisk container"))
        rows += ["ramdisk_file=root.uInitrd", f"ramdisk_addr_r={cursor:#x}",
                 f"ramdisk_image_size={size:#x}", f"initrd_start={payload:#x}",
                 f"initrd_end={cursor + size:#x}", "initrd_high=0xffffffffffffffff"]
        ramdisk_argument = f"{cursor:#x}"
        cursor = align(cursor + size)
    if args.dtb:
        regular(args.dtb)
        with args.dtb.open("rb") as source:
            header = source.read(8)
        require(len(header) == 8 and header[:4] == b"\xd0\x0d\xfe\xed",
                "OS DTB header is invalid")
        total = struct.unpack_from(">I", header, 4)[0]
        size = args.dtb.stat().st_size
        require(40 <= total <= size, "OS DTB size is invalid")
        # bootm 原地增加 chosen 属性需要独立空隙；这部分也计入实际文件和装载范围。
        size += 64 * 1024
        require(size < U32_LIMIT, "padded OS DTB exceeds format bounds")
        target = directory / "board.dtb"
        shutil.copyfile(args.dtb, target)
        with target.open("r+b") as output:
            output.truncate(size)
            output.seek(4)
            output.write(struct.pack(">I", size))
        spans.append((cursor, cursor + size, "OS DTB"))
        rows += ["fdt_file=board.dtb", f"fdt_addr_r={cursor:#x}", f"fdt_size={size:#x}",
                 "fdt_high=0xffffffffffffffff"]
    else:
        rows.append("# 未提供 OS_DTB；启动前须准备实际传给内核的 OS DTB。")
    require(all(0 <= start < end <= U32_LIMIT for start, end, _ in spans),
            "board image ranges exceed legacy physical address bounds")
    ordered = sorted(spans)
    require(all(left[1] <= right[0] for left, right in zip(ordered, ordered[1:])),
            "board image memory ranges overlap")
    if args.root:
        require(re.fullmatch(r"/dev/[A-Za-z0-9]+", args.root), "invalid board root device")
        require(args.root != "/dev/ram0" or args.ramdisk, "RAM root requires --ramdisk")
        require(re.fullmatch(r"/[A-Za-z0-9_./-]+", args.init), "invalid board init path")
        rows.append(f"bootargs=root={args.root} rootdir=/ {args.root_mode} init={args.init}")
        if args.dtb:
            rows.append(f"blossom_boot=bootm {staging:#x} {ramdisk_argument} {cursor:#x}")
    else:
        rows.append("# 未选择 BOARD_ROOT；未生成 bootargs 或启动命令。")
    rows.append("# blossom_boot 仅供手动执行；实际固件、ABI 和 RAM 尚未验证。")
    (directory / "boot.env").write_text("\n".join(rows) + "\n", encoding="utf-8")


def build_images(args):
    """固定本轮 ELF 副本后复用宿主验证器，全部包装成功才发表对应文件。"""
    require((args.arch, args.platform) in (("riscv64", "visionfive2"),
                                         ("loongarch64", "ls2k1000la")),
            "board-image requires a matching board ARCH/PLATFORM")
    regular(args.kernel)
    epoch = int(os.environ.get("SOURCE_DATE_EPOCH", "0"))
    require(0 <= epoch < U32_LIMIT, "SOURCE_DATE_EPOCH exceeds legacy timestamp bounds")
    args.output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix=".board-", dir=args.output) as temporary:
        directory = Path(temporary)
        kernel = directory / "kernel.elf"
        shutil.copyfile(args.kernel, kernel)
        layout = elf_layout(kernel, args.arch, args.platform)
        raw = directory / "kernel.bin"
        raw_kernel(kernel, raw, layout)
        boot_environment(args, layout, raw, directory, epoch)
        names = ["kernel.elf", "kernel.bin", "kernel.uImage", "boot.env"]
        names += ["root.uInitrd"] if args.ramdisk else []
        names += ["board.dtb"] if args.dtb else []
        for name in names:
            os.replace(directory / name, args.output / name)
        for name, supplied in (("root.uInitrd", args.ramdisk), ("board.dtb", args.dtb)):
            if not supplied:
                (args.output / name).unlink(missing_ok=True)


def self_check():
    """独立检查 legacy 头字段及两种 CRC，并覆盖地址溢出拒绝边界。"""
    payload = b"blossom"
    header = legacy_header(len(payload), zlib.crc32(payload), 27, 2, 0x123000, 0x123000, 0)
    fields = LEGACY.unpack(header)
    assert fields[:1] == (0x27051956,)
    assert fields[3:11] == (len(payload), 0x123000, 0x123000, zlib.crc32(payload), 5, 27, 2, 0)
    assert zlib.crc32(header[:4] + bytes(4) + header[8:]) == fields[1]
    try:
        legacy_header(1, 0, 26, 2, U32_LIMIT, 0, 0)
    except HostError:
        pass
    else:
        raise AssertionError("legacy physical address overflow was accepted")


def main(argv=None):
    """仅生成文件；不调用下载器、串口、TFTP、写盘或固件命令。"""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--arch", choices=("riscv64", "loongarch64"))
    parser.add_argument("--platform", choices=("visionfive2", "ls2k1000la"))
    parser.add_argument("--kernel", type=Path)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--ramdisk", type=Path, help="uncompressed raw ext4 input; never modified")
    parser.add_argument("--dtb", type=Path, help="actual OS DTB input; not a firmware control tree")
    parser.add_argument("--root", help="explicit board root device; single-architecture rootdir is /")
    parser.add_argument("--root-mode", choices=("ro", "rw"), default="ro")
    parser.add_argument("--init", default="/sbin/init")
    parser.add_argument("--self-check", action="store_true")
    args = parser.parse_args(argv)
    try:
        if args.self_check:
            self_check()
            return
        require(all((args.arch, args.platform, args.kernel, args.output)),
                "--arch, --platform, --kernel and --output are required")
        build_images(args)
        print(f"Board images generated offline: {args.output}")
    except (OSError, ValueError, HostError, struct.error) as error:
        parser.exit(1, f"board-image: {error}\n")


if __name__ == "__main__":
    main()
