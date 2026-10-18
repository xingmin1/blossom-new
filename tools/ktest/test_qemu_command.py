"""生产 argv 与测试专属设备布局的纯参数行为回归。"""

from pathlib import Path
import shlex
import unittest

from host_helpers import TOOLS
from qemu_command import (build_block_backend_arguments, build_qemu_command,
                          build_user_network_backend_arguments)
from ktest.resources import build_test_disk_arguments, build_test_network_arguments


def command_for(architecture, **overrides):
    """向生产 builder 明确传入全部字段，覆盖值只改变对应的运行维度。"""
    arguments = dict(architecture=architecture, qemu_binary=f"qemu-system-{architecture}",
                     kernel_elf_path=Path("/selected/kernel.elf"),
                     system_working_disk_path=Path("/selected/root.raw"),
                     contest_working_disk_path=Path("/selected/contest.raw"),
                     cpu_count=3, memory_size="1536M", network_mode="user",
                     host_forward_rule=None, boot_arguments=None, debug_enabled=False,
                     gdb_port=23456)
    arguments.update(overrides)
    return build_qemu_command(**arguments)


def option_values(command, option):
    """只提取被测选项的值，避免把无关 argv 排列钉成整段快照。"""
    return [command[index + 1] for index, value in enumerate(command) if value == option]


class QemuCommandTests(unittest.TestCase):
    """生产设备、明确输入与会话覆盖相互独立。"""

    def test_evaluation_devices_keep_role_order_and_architecture_contract(self):
        """测试块先于网卡，系统块最后；两架构保各自评测设备而无测试硬件属性。"""
        for architecture in ("riscv64", "loongarch64"):
            with self.subTest(architecture=architecture):
                command = command_for(architecture)
                devices = option_values(command, "-device")
                drives = option_values(command, "-drive")
                model = "device" if architecture == "riscv64" else "pci"
                backend_id = "net" if architecture == "riscv64" else "net0"
                self.assertEqual(len(devices), 3)
                self.assertTrue(devices[0].startswith(f"virtio-blk-{model},drive=x0"))
                self.assertEqual(devices[1], f"virtio-net-{model},netdev={backend_id}")
                self.assertTrue(devices[2].startswith(f"virtio-blk-{model},drive=x1"))
                self.assertLess(command.index(drives[0]), command.index(devices[1]))
                self.assertLess(command.index(devices[1]), command.index(drives[1]))
                self.assertIn("id=x0,format=raw,file=/selected/contest.raw", drives[0])
                self.assertIn("id=x1,format=raw,file=/selected/root.raw", drives[1])
                self.assertEqual(option_values(command, "-netdev"), [f"user,id={backend_id}"])
                self.assertEqual(option_values(command, "-rtc"), ["base=utc"])
                self.assertIn("-nographic", command)
                self.assertIn("-no-reboot", command)
                self.assertNotIn("-append", command)
                self.assertNotIn("-gdb", command)
                self.assertNotIn("-cpu", command)
                for device in devices:
                    for attribute in ("mac=", "addr=", "disable-legacy=", "iommu_platform=",
                                      "logical_block_size="):
                        self.assertNotIn(attribute, device)
                if architecture == "riscv64":
                    self.assertEqual(option_values(command, "-machine"), ["virt"])
                    self.assertEqual(option_values(command, "-bios"), ["default"])
                    self.assertIn("bus=virtio-mmio-bus.0", devices[0])
                    self.assertIn("bus=virtio-mmio-bus.1", devices[2])
                else:
                    self.assertNotIn("-machine", command)
                    self.assertNotIn("-bios", command)

    def test_selected_inputs_are_used_without_replacing_other_roles(self):
        """调用者选择的 ELF、工作盘、CPU、内存和网络覆盖直接进入 argv。"""
        command = command_for("riscv64", qemu_binary="/chosen/qemu", cpu_count=5,
                              memory_size="2304M", kernel_elf_path="/chosen/kernel",
                              system_working_disk_path="/chosen/system",
                              contest_working_disk_path="/chosen/contest", network_mode="none")
        self.assertEqual(command[0], "/chosen/qemu")
        self.assertEqual(option_values(command, "-kernel"), ["/chosen/kernel"])
        self.assertEqual(option_values(command, "-smp"), ["5"])
        self.assertEqual(option_values(command, "-m"), ["2304M"])
        self.assertEqual(option_values(command, "-nic"), ["none"])
        self.assertEqual(option_values(command, "-netdev"), [])
        self.assertEqual(option_values(command, "-drive"), [
            "if=none,id=x0,format=raw,file=/chosen/contest",
            "if=none,id=x1,format=raw,file=/chosen/system"])

    def test_single_disk_shell_append_and_debug_are_orthogonal(self):
        """省略比赛盘不推断启动参数，Shell 和 GDB 各只附加自己的明确参数。"""
        for architecture in ("riscv64", "loongarch64"):
            with self.subTest(architecture=architecture):
                base = command_for(architecture)
                shell_arguments = "root=/dev/vda rw blossom.boot=shell"
                shell = command_for(architecture, boot_arguments=shell_arguments)
                self.assertEqual(shell, base + ["-append", shell_arguments])
                self.assertEqual(command_for(architecture, debug_enabled=True),
                                 base + ["-S", "-gdb", "tcp:127.0.0.1:23456"])
                self.assertEqual(command_for(architecture, boot_arguments=shell_arguments,
                                             debug_enabled=True),
                                 shell + ["-S", "-gdb", "tcp:127.0.0.1:23456"])
                single = command_for(architecture, contest_working_disk_path=None)
                self.assertEqual(option_values(single, "-drive"), option_values(base, "-drive")[1:])
                self.assertEqual(option_values(single, "-netdev"), option_values(base, "-netdev"))
                self.assertEqual(option_values(single, "-kernel"), option_values(base, "-kernel"))
                self.assertNotIn("-append", single)
                self.assertNotIn("-gdb", single)
                self.assertEqual(command_for(architecture, boot_arguments="")[-2:], ["-append", ""])

    def test_special_paths_preserve_argv_and_escape_qemu_fields(self):
        """空白、引号和 Unicode 保留原字节语义，字段逗号双写且展示命令可复制。"""
        kernel = "/selected/内核 'quoted', file.elf"
        disk = Path('/selected/工作盘 "quoted", volume.raw')
        forward = "tcp:127.0.0.1:43210-:4321,udp:127.0.0.1:43211-:4322"
        command = command_for("loongarch64", kernel_elf_path=kernel,
                              system_working_disk_path=disk, host_forward_rule=forward)
        self.assertEqual(option_values(command, "-kernel"), [kernel])
        self.assertEqual(option_values(command, "-drive")[1],
                         f"if=none,id=x1,format=raw,file={str(disk).replace(',', ',,')}")
        self.assertEqual(build_block_backend_arguments("chosen", disk, True),
                         ["-drive", f"if=none,id=chosen,format=raw,file={str(disk).replace(',', ',,')},readonly=on"])
        self.assertEqual(build_user_network_backend_arguments("chosen", forward),
                         ["-netdev", "user,id=chosen,hostfwd=" + forward.replace(",", ",,")])
        self.assertEqual(shlex.split(shlex.join(command)), command)

    def test_test_layout_keeps_slots_features_and_actual_peer_ports(self):
        """测试逻辑盘号与 root 字母保持一致，物理槽跳过 NIC，特性只来自明确选择。"""
        disks = [(role, Path(f"/{role}.raw")) for role in ("extra", "root", "scratch", "fourth")]
        for architecture in ("riscv64", "loongarch64"):
            with self.subTest(architecture=architecture):
                command = build_test_disk_arguments(
                    disks, architecture=architecture, read_only_roles={"root"},
                    logical_block_size=4096, virtio_access_platform=True,
                    root_device=None, root_mode="rw", extra_root_device="/dev/vda",
                    network_peer_ports=(32101, 32102))
                drives = option_values(command, "-drive")
                self.assertNotIn("readonly=on", drives[0])
                self.assertIn("readonly=on", drives[1])
                devices = option_values(command, "-device")
                slot = "bus=virtio-mmio-bus.4" if architecture == "riscv64" else "addr=0x15"
                self.assertIn(slot, devices[3])
                for device in devices:
                    self.assertIn("logical_block_size=4096,physical_block_size=4096", device)
                    self.assertIn("iommu_platform=on", device)
                    self.assertEqual("disable-legacy=on" in device, architecture == "loongarch64")
                self.assertEqual(option_values(command, "-append"), [
                    "rw rootdir=/ root=/dev/vdb extra=/dev/vda ktest-net-tcp=32101 ktest-net-udp=32102"])
                network = build_test_network_arguments(architecture, "user", None)
                nic = option_values(network, "-device")[0]
                self.assertIn("mac=52:54:00:12:34:56", nic)
                self.assertIn("bus=virtio-mmio-bus.3" if architecture == "riscv64" else "addr=0x11", nic)
                plain = build_test_disk_arguments(
                    disks[:1], architecture=architecture, read_only_roles=set(),
                    logical_block_size=512, virtio_access_platform=False,
                    root_device="/dev/vdz", root_mode="ro", extra_root_device=None)
                self.assertNotIn("iommu_platform=on", " ".join(plain))
                self.assertEqual(option_values(plain, "-append"), ["ro rootdir=/ root=/dev/vdz"])
