"""普通运行和测试共用的 QEMU 块设备、网络槽位与明确启动参数。"""



def network_arguments(arch, net="none", forward=""):
    """仅按明确网络模式配置固定 NIC 槽位，hostfwd 由调用者原样指定。"""
    if net == "none":
        if forward:
            raise ValueError("host forwarding requires user networking")
        return ["-nic", "none"]
    if net != "user":
        raise ValueError(f"unsupported network mode: {net}")
    backend = "user,id=net0"
    if forward:
        backend += f",hostfwd={forward}"
    if arch == "riscv64":
        device = "virtio-net-device,netdev=net0,bus=virtio-mmio-bus.3"
    elif arch == "loongarch64":
        device = "virtio-net-pci,netdev=net0,addr=0x11,disable-legacy=on"
    else:
        raise ValueError(f"unsupported network architecture: {arch}")
    return ["-netdev", backend, "-device", device + ",mac=52:54:00:12:34:56"]


def disk_arguments(disks, args, names, init=None, network_ports=None, rootdir="/"):
    """磁盘顺序跳过固定 NIC 槽，并把明确的根视图交给同一次 append。"""
    for name, value in (("init", init), ("rootdir", rootdir)):
        if value is not None and any(character in " \t\n\r\v\f\0" for character in value):
            raise ValueError(f"{name} must not contain ASCII whitespace or NUL")
    command = []
    for index, (name, path) in enumerate(disks):
        slot = index + int(3 <= index)
        drive = f"if=none,id=disk{index},format=raw,file={str(path).replace(',', ',,')}"
        if name in args.read_only:
            drive += ",readonly=on"
        command += ["-drive", drive]
        if args.arch == "riscv64":
            device = f"virtio-blk-device,drive=disk{index},bus=virtio-mmio-bus.{slot}"
        else:
            device = f"virtio-blk-pci,drive=disk{index},addr=0x{5 + 4 * slot:x}"
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
    bootargs = [args.root_mode, f"rootdir={rootdir}"]
    if root is not None:
        bootargs.append(f"root={root}")
    if args.extra_root is not None:
        bootargs.append(f"extra={args.extra_root}")
    if init is not None:
        bootargs.append(f"init={init}")
    if network_ports is not None:
        tcp, udp = network_ports
        if not (1 <= tcp <= 65535 and 1 <= udp <= 65535):
            raise ValueError("network peer ports must be in 1..65535")
        bootargs += [f"ktest-net-tcp={tcp}", f"ktest-net-udp={udp}"]
    command += ["-append", " ".join(bootargs)]
    return command
