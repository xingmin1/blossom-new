"""宿主 resources 回归；运行 python3 -m unittest discover -s tools/ktest。"""

import errno
import hashlib
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

from host_helpers import TOOLS, options
from ktest.catalog import HostError
from ktest.resources import aliases, prepare_disks, validate_resources
from ktest.qemu_session import QemuTestSession


class RootModeInputTests(unittest.TestCase):
    """真实宿主入口在资源准备前拒绝非法根挂载模式。"""

    def test_invalid_mode_is_rejected_without_replacing_work_disks(self):
        """ktest CLI 拒绝未知或多 token 值，已有工作盘与源文件保持不变。"""
        with tempfile.TemporaryDirectory() as temporary:
            base = Path(temporary)
            source = base / "source.img"
            source.write_bytes(b"original source")
            work = base / "work"
            work.mkdir()
            disk = work / "root.raw"
            disk.write_bytes(b"preserved working disk")
            commands = [
                [sys.executable, str(TOOLS / "run_kernel_tests.py"), "list", "--arch", "riscv64",
                 "--report-dir", str(base / "report")],
                [sys.executable, str(TOOLS / "run_kernel_tests.py"), "run", "--arch", "riscv64",
                 "--report-dir", str(base / "report")],
            ]
            for mode in ("", "RO", "rwx", "ro rw", "ro=foo", "rw=foo", "%"):
                for command in commands:
                    with self.subTest(command=command, mode=mode):
                        result = subprocess.run(command + ["--root-mode", mode],
                                                capture_output=True, text=True)
                        self.assertEqual(result.returncode, 2)
                self.assertEqual(source.read_bytes(), b"original source")
                self.assertEqual(disk.read_bytes(), b"preserved working disk")
                self.assertFalse((base / "report").exists())


class ResourceTests(unittest.TestCase):
    """工作盘独占副本、真实进程拥有和失败清理。"""

    def test_missing_declared_root_is_error(self):
        """声明根盘资源后必须提供真实输入镜像。"""
        with self.assertRaises(HostError):
            validate_resources({"root"}, options())

    def test_work_copy_does_not_modify_source_and_rejects_alias(self):
        """硬链接和符号链接不能成为可写工作盘。"""
        with tempfile.TemporaryDirectory() as temporary:
            base = Path(temporary)
            source = base / "source.raw"
            source.write_bytes(b"original disk")
            work = base / "work"
            work.mkdir()
            args = options(disks={"root": source})
            disks = prepare_disks({"root"}, args, work)
            disks[0][1].write_bytes(b"changed work disk")
            self.assertEqual(source.read_bytes(), b"original disk")
            for symbolic in (False, True):
                alias_dir = base / ("symbolic" if symbolic else "hard")
                alias_dir.mkdir()
                target = alias_dir / "root.raw"
                if symbolic:
                    target.symlink_to(source)
                else:
                    target.hardlink_to(source)
                self.assertTrue(aliases(target, source))
                with self.assertRaises(HostError):
                    prepare_disks({"root"}, args, alias_dir)
                self.assertEqual(source.read_bytes(), b"original disk")

    def test_vm_disks_live_until_process_wait_and_cleanup_partial_construction(self):
        """真实进程退出前保盘，隔离并行实例并清掉半复制，保留输入及原文日志。"""
        with tempfile.TemporaryDirectory() as temporary:
            base = Path(temporary)
            source = base / "source.raw"
            source.write_bytes(b"original disk")
            source_hash = hashlib.sha256(source.read_bytes()).hexdigest()
            executable = base / "disk-user"
            executable.write_text(
                f"#!{sys.executable}\n"
                "import json,signal,subprocess,sys,time\n"
                "from pathlib import Path\n"
                "child_mode=sys.argv[1:2]==['--child']\n"
                "paths=[Path(p) for p in sys.argv[2:]] if child_mode else "
                "[Path(a.split('file=',1)[1].split(',readonly=',1)[0]) "
                "for a in sys.argv[1:] if a.startswith('if=none,')]\n"
                "report=paths[0].parent.parent\n"
                "handles=[p.open('r+b') for p in paths]\n"
                "child=None if child_mode else subprocess.Popen("
                "[sys.executable,__file__,'--child',*[str(p) for p in paths]])\n"
                "def stop(*unused):\n"
                "    if child is not None: child.wait(timeout=1)\n"
                "    (report/('child-stopped' if child_mode else 'stopped')).write_text("
                "json.dumps([p.exists() for p in paths]))\n"
                "    raise SystemExit(0)\n"
                "signal.signal(signal.SIGTERM,stop)\n"
                "if child_mode:\n"
                "    (report/'child-ready').write_text('ready')\n"
                "else:\n"
                "    while not (report/'child-ready').exists(): time.sleep(0.001)\n"
                "    print('working disks opened',flush=True)\n"
                "    (report/'ready').write_text('ready')\n"
                "while True: time.sleep(1)\n", encoding="utf-8")
            executable.chmod(0o700)
            args = options(qemu=str(executable), memory="128M", disks={"root": source, "extra": source})
            vms = []
            working_disks = []
            try:
                for index in range(2):
                    directory = base / str(index)
                    directory.mkdir()
                    (directory / "uart-segment.raw").write_bytes(b"raw UART evidence")
                    (directory / "root.raw").write_bytes(b"unowned report evidence")
                    vm = QemuTestSession(args, Path("kernel.elf"), directory, {"root", "extra", "scratch"})
                    vms.append(vm)
                    deadline = time.monotonic() + 5
                    while not (directory / "ready").exists() and time.monotonic() < deadline:
                        time.sleep(0.01)
                    self.assertTrue((directory / "ready").exists())
                    self.assertIsNone(vm.process.poll())
                    command = json.loads((directory / "command.json").read_text())
                    paths = [Path(a.split("file=", 1)[1]) for a in command if a.startswith("if=none,")]
                    working_disks.append(paths)
                    for path in paths:
                        self.assertTrue(path.is_file())
                    vm.uart.raw.write(b"UART original log\n")
                self.assertTrue(set(working_disks[0]).isdisjoint(working_disks[1]))
                working_disks[0][0].write_bytes(b"guest mutation")
                for index, vm in enumerate(vms):
                    vm.close()
                    self.assertEqual(vm.process.returncode, 0)
                    for path in working_disks[index]:
                        self.assertFalse(path.exists())
                    self.assertEqual(json.loads((vm.directory / "stopped").read_text()), [True] * 3)
                    self.assertEqual(json.loads((vm.directory / "child-stopped").read_text()), [True] * 3)
                    self.assertEqual((vm.directory / "uart.log").read_bytes(), b"UART original log\n")
                    self.assertEqual((vm.directory / "qemu.log").read_bytes(), b"working disks opened\n")
                    self.assertTrue((vm.directory / "command.json").is_file())
                    self.assertEqual((vm.directory / "uart-segment.raw").read_bytes(), b"raw UART evidence")
                    self.assertEqual((vm.directory / "root.raw").read_bytes(), b"unowned report evidence")
                    if index == 0:
                        for path in working_disks[1]:
                            self.assertTrue(path.is_file())
                        self.assertIsNone(vms[1].process.poll())
            finally:
                for vm in vms:
                    vm.close()
            partial = []
            no_space = OSError(errno.ENOSPC, "No space left on device")

            def fail_copy(source_file, target_file, length):
                """已实际写出半盘时注入原始宿主 ENOSPC。"""
                target_file.write(source_file.read(3))
                partial.append(Path(target_file.name))
                raise no_space

            directory = base / "partial"
            directory.mkdir()
            (directory / "uart-segment.raw").write_bytes(b"keep")
            with patch("ktest.resources.shutil.copyfileobj", side_effect=fail_copy):
                with self.assertRaises(OSError) as caught:
                    QemuTestSession(args, Path("kernel.elf"), directory, {"root"})
            self.assertIs(caught.exception, no_space)
            self.assertFalse(partial[0].parent.exists())
            self.assertTrue((directory / "uart.log").is_file())
            self.assertTrue((directory / "qemu.log").is_file())
            self.assertEqual((directory / "uart-segment.raw").read_bytes(), b"keep")
            directory = base / "launch-failure"
            directory.mkdir()
            args.qemu = str(base / "missing-executable")
            with self.assertRaises(FileNotFoundError):
                QemuTestSession(args, Path("kernel.elf"), directory, {"root"})
            command = json.loads((directory / "command.json").read_text())
            working = Path(next(a.split("file=", 1)[1] for a in command if a.startswith("if=none,")))
            self.assertFalse(working.parent.exists())
            self.assertEqual(hashlib.sha256(source.read_bytes()).hexdigest(), source_hash)


if __name__ == "__main__":
    unittest.main()
