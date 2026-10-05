"""普通运行和测试共用的 QEMU 块设备槽位与明确启动参数。"""



def disk_arguments(disks, args, names):
    """按实际 MMIO 槽或 PCI BDF 附盘；显式 root 不存在时也原样交给内核。"""
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
    root = args.root
    if root is None:
        slot = next((index for index, (name, _) in enumerate(disks) if name == "root"), None)
        if slot is not None:
            root = f"/dev/vd{chr(ord('a') + slot)}"
    bootargs = []
    if root is not None:
        bootargs.append(f"root={root}")
    if args.extra_root is not None:
        bootargs.append(f"extra={args.extra_root}")
    if bootargs:
        command += ["-append", " ".join(bootargs)]
    return command
