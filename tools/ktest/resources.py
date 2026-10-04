"""具体 UART 与磁盘资源；原镜像只读输入，每台 VM 使用独占副本。"""

import hashlib
from pathlib import Path
import shutil
import stat
from .catalog import require

RESOURCES = {"uart", "scratch", "root", "extra", "ap-freeze", "virtio-access-platform"}


def aliases(path, target):
    """同时识别符号路径和既有硬链接身份。"""
    path, target = Path(path), Path(target)
    return path.resolve() == target.resolve() or (path.exists() and target.exists() and path.samefile(target))


def regular_file(path):
    """资源输入只能是普通源文件，不接受设备或 FIFO。"""
    require(path.is_file() and stat.S_ISREG(path.stat().st_mode), f"not a regular file: {path}")


def hash_file(path):
    """逐块计算实际内容 SHA256，不把完整 ELF 或镜像留在内存。"""
    digest = hashlib.sha256()
    with path.open("rb") as source:
        while block := source.read(1024 * 1024):
            digest.update(block)
    return digest.hexdigest()


def resource_names(suite, items):
    """只取本次选中实例及 suite 基础资源的并集。"""
    names = set(suite["resources"])
    for item in items:
        names.update(item.case["resources"])
    require(names <= RESOURCES, f"unsupported resources: {sorted(names - RESOURCES)}")
    return names


def crc_table():
    """构造 Castagnoli 反射多项式查表，不依赖错误的 IEEE crc32。"""
    table = []
    for byte in range(256):
        value = byte
        for _ in range(8):
            value = (value >> 1) ^ (0x82F63B78 if value & 1 else 0)
        table.append(value)
    return tuple(table)


CRC_TABLE = crc_table()


def crc32c_update(state, data):
    """接收初值全一的运行状态，末尾取反由消息边界负责。"""
    for byte in data:
        state = CRC_TABLE[(state ^ byte) & 255] ^ (state >> 8)
    return state


def validate_resources(names, args):
    """缺少已声明设备是 Error，不能伪造 Skip 或文件系统。"""
    if "ap-freeze" in names:
        require(args.ap_freeze, "AP freeze resource requires --ap-freeze")
        require(shutil.which(args.gdb or "gdb") is not None, "AP freeze requires an available GDB binary")
    if "virtio-access-platform" in names:
        require(args.iommu_platform, "VirtIO feature fixture requires --iommu-platform")
        require(args.arch != "riscv64" or args.modern, "VirtIO feature fixture requires modern MMIO")
    for name in ("root", "extra"):
        if name in names:
            require(name in args.disks, f"resource {name} requires --disk {name}=PATH")
    for name in names & args.disks.keys():
        regular_file(args.disks[name])
    if "uart" in names:
        require(args.uart_payload is not None or args.uart_expect is not None
                or getattr(args, "execution_uart", False),
                "UART resource requires --uart-payload, --uart-expect or --execution-uart")
    if "uart" in names and args.uart_payload is not None:
        regular_file(args.uart_payload)
    if "uart" in names and args.uart_expect is not None:
        regular_file(args.uart_expect)


def prepare_disks(names, args, directory):
    """按稳定槽位复制普通源盘，空白 scratch 不声称包含 ext4。"""
    validate_resources(names, args)
    disks = []
    for name in ("root", "extra", "scratch"):
        if name not in names:
            continue
        target = directory / f"{name}.raw"
        require(not target.exists() and not target.is_symlink(), "working disk already exists")
        with target.open("xb") as output:
            if name in args.disks:
                with args.disks[name].open("rb") as source:
                    shutil.copyfileobj(source, output, 1024 * 1024)
            else:
                require(name == "scratch", "missing source disk")
                output.truncate(args.scratch_bytes)
        disks.append((name, target))
    return disks


def disk_arguments(disks, args, names):
    """RV 使用实际 MMIO 槽，LA 使用稳定 PCI BDF；没有盘就不附 root。"""
    command = []
    for index, (name, path) in enumerate(disks):
        drive = f"if=none,id=disk{index},format=raw,file={str(path).replace(',', ',,')}"
        if name in args.read_only:
            drive += ",readonly=on"
        command += ["-drive", drive]
        if args.arch == "riscv64":
            device = f"virtio-blk-device,drive=disk{index},bus=virtio-mmio-bus.{index}"
        else:
            device = f"virtio-blk-pci,drive=disk{index},addr=0x{5 + 4 * index:x}"
        device += f",logical_block_size={args.logical_block_size},physical_block_size={args.logical_block_size}"
        if "virtio-access-platform" in names:
            device += ",iommu_platform=on"
            if args.arch == "loongarch64":
                device += ",disable-legacy=on"
        command += ["-device", device]
    root = next((index for index, (name, _) in enumerate(disks) if name == "root"), None)
    if root is not None:
        command += ["-append", f"root=/dev/vd{chr(ord('a') + root)}"]
    return command
