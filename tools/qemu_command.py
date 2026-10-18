"""生产 QEMU 的纯 argv 构造及与测试实际共用的设备后端。"""


def build_block_backend_arguments(drive_id, image_path, read_only=False):
    """为明确工作盘生成 raw 后端；逗号按 QEMU 字段转义，不加入 shell 引号。"""
    image_value = str(image_path).replace(",", ",,")
    backend = f"if=none,id={drive_id},format=raw,file={image_value}"
    if read_only:
        backend += ",readonly=on"
    return ["-drive", backend]


def build_user_network_backend_arguments(backend_id, host_forward_rule=None):
    """生成 user 后端，转发规则由实际调用者明确提供。"""
    backend = f"user,id={backend_id}"
    if host_forward_rule:
        backend += ",hostfwd=" + host_forward_rule.replace(",", ",,")
    return ["-netdev", backend]


def build_qemu_command(architecture, qemu_binary, kernel_elf_path,
                       system_working_disk_path, contest_working_disk_path,
                       cpu_count, memory_size, network_mode, host_forward_rule,
                       boot_arguments, debug_enabled, gdb_port):
    """按评测设备顺序构造 argv；参数均由入口提供，不访问路径或启动进程。"""
    if architecture not in ("riscv64", "loongarch64"):
        raise ValueError(f"unsupported QEMU architecture: {architecture}")
    command = [str(qemu_binary)]
    if architecture == "riscv64":
        command += ["-machine", "virt"]
    command += ["-kernel", str(kernel_elf_path), "-m", memory_size,
                "-nographic", "-smp", str(cpu_count)]
    if architecture == "riscv64":
        command += ["-bios", "default"]
    if contest_working_disk_path is not None:
        command += build_block_backend_arguments("x0", contest_working_disk_path)
        if architecture == "riscv64":
            command += ["-device", "virtio-blk-device,drive=x0,bus=virtio-mmio-bus.0"]
        else:
            command += ["-device", "virtio-blk-pci,drive=x0"]
    command += ["-no-reboot"]
    if network_mode == "none":
        command += ["-nic", "none"]
    elif network_mode == "user":
        backend_id = "net" if architecture == "riscv64" else "net0"
        device = "virtio-net-device" if architecture == "riscv64" else "virtio-net-pci"
        command += ["-device", f"{device},netdev={backend_id}"]
        command += build_user_network_backend_arguments(backend_id, host_forward_rule)
    else:
        raise ValueError(f"unsupported network mode: {network_mode}")
    command += ["-rtc", "base=utc"]
    command += build_block_backend_arguments("x1", system_working_disk_path)
    if architecture == "riscv64":
        command += ["-device", "virtio-blk-device,drive=x1,bus=virtio-mmio-bus.1"]
    else:
        command += ["-device", "virtio-blk-pci,drive=x1"]
    if boot_arguments is not None:
        command += ["-append", boot_arguments]
    if debug_enabled:
        command += ["-S", "-gdb", f"tcp:127.0.0.1:{gdb_port}"]
    return command
