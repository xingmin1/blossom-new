"""普通运行和测试共用的 QEMU 块设备槽位与明确启动参数。"""



def disk_arguments(disks, args, names, init=None):
    """按实际槽位附盘并传根挂载模式；显式 init 保留空值，只拒绝启动 token 分隔符。"""
    if init is not None and any(character in " \t\n\r\v\f\0" for character in init):
        raise ValueError("init must not contain ASCII whitespace or NUL")
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
    bootargs = [args.root_mode]
    if root is not None:
        bootargs.append(f"root={root}")
    if args.extra_root is not None:
        bootargs.append(f"extra={args.extra_root}")
    if init is not None:
        bootargs.append(f"init={init}")
    command += ["-append", " ".join(bootargs)]
    return command
