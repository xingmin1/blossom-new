"""宿主 resources 回归；运行 python3 -m unittest discover -s tools/ktest。"""

import errno
import fcntl
import hashlib
import json
from pathlib import Path
import select as select_io
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

from host_helpers import TOOLS, options
from ktest.catalog import HostError
from ktest.resources import aliases, prepare_disks, validate_resources
from ktest.runner import VM
from run import working_disks


class RootModeInputTests(unittest.TestCase):
    """真实宿主入口在资源准备前拒绝非法根挂载模式。"""

    def test_invalid_mode_is_rejected_without_replacing_work_disks(self):
        """命令行和 Make 拒绝未知或多 token 值，已有恢复盘与源文件必须保留。"""
        with tempfile.TemporaryDirectory() as temporary:
            base = Path(temporary)
            source = base / "source.img"
            source.write_bytes(b"original source")
            work = base / "work"
            work.mkdir()
            disk = work / "root.raw"
            disk.write_bytes(b"preserved working disk")
            commands = [
                [sys.executable, str(TOOLS / "run.py"), "--arch", "riscv64",
                 "--kernel", str(base / "missing.elf"), "--image", str(source),
                 "--work-dir", str(work)],
                [sys.executable, str(TOOLS / "ktest.py"), "list", "--arch", "riscv64",
                 "--report-dir", str(base / "report")],
                [sys.executable, str(TOOLS / "ktest.py"), "run", "--arch", "riscv64",
                 "--report-dir", str(base / "report")],
            ]
            for mode in ("", "RO", "rwx", "ro rw", "ro=foo", "rw=foo", "%"):
                for command in commands:
                    with self.subTest(command=command, mode=mode):
                        result = subprocess.run(command + ["--root-mode", mode],
                                                capture_output=True, text=True)
                        self.assertEqual(result.returncode, 2)
                with self.subTest(command="make", mode=mode):
                    result = subprocess.run(["make", "--no-print-directory", "-n", "run",
                                             f"ROOT_MODE={mode}"], cwd=TOOLS.parent,
                                            capture_output=True, text=True)
                    self.assertNotEqual(result.returncode, 0)
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

    def test_ordinary_forward_without_network_rejects_preserved_resume_disk(self):
        """none 与明确转发冲突时返回真实错误，不替换恢复盘或修改源镜像。"""
        with tempfile.TemporaryDirectory() as temporary:
            base = Path(temporary)
            kernel = base / "kernel.elf"
            kernel.write_bytes(b"kernel input")
            source = base / "source.img"
            source.write_bytes(b"source input")
            work = base / "work"
            work.mkdir()
            disk = work / "root.raw"
            disk.write_bytes(b"preserved resumed disk")
            result = subprocess.run([
                sys.executable, str(TOOLS / "run.py"), "--arch", "riscv64",
                "--kernel", str(kernel), "--qemu", sys.executable, "--image", str(source),
                "--work-dir", str(work), "--resume", "--net", "none",
                "--net-forward", "tcp:127.0.0.1:43210-:4321",
            ], capture_output=True, text=True)
            self.assertEqual(result.returncode, 1)
            self.assertIn("host forwarding requires user networking", result.stderr)
            self.assertEqual(source.read_bytes(), b"source input")
            self.assertEqual(disk.read_bytes(), b"preserved resumed disk")
            self.assertEqual(kernel.read_bytes(), b"kernel input")

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
                    vm = VM(args, Path("kernel.elf"), directory, {"root", "extra", "scratch"})
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
                    vm.raw.write(b"UART original log\n")
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
                    VM(args, Path("kernel.elf"), directory, {"root"})
            self.assertIs(caught.exception, no_space)
            self.assertFalse(partial[0].parent.exists())
            self.assertTrue((directory / "uart.log").is_file())
            self.assertTrue((directory / "qemu.log").is_file())
            self.assertEqual((directory / "uart-segment.raw").read_bytes(), b"keep")
            directory = base / "launch-failure"
            directory.mkdir()
            args.qemu = str(base / "missing-executable")
            with self.assertRaises(FileNotFoundError):
                VM(args, Path("kernel.elf"), directory, {"root"})
            command = json.loads((directory / "command.json").read_text())
            working = Path(next(a.split("file=", 1)[1] for a in command if a.startswith("if=none,")))
            self.assertFalse(working.parent.exists())
            self.assertEqual(hashlib.sha256(source.read_bytes()).hexdigest(), source_hash)

    def test_ordinary_fresh_resume_and_system_refresh_preserve_disk_ownership(self):
        """真实副本从 fresh 到 resume，再仅刷新系统盘，比赛数据和源文件保持独立。"""
        with tempfile.TemporaryDirectory() as temporary:
            base = Path(temporary)
            work = base / "work"
            work.mkdir()
            sources = []
            source_contents = {}
            for name in ("contest", "root", "extra", "scratch"):
                source = base / f"{name}.img"
                source_contents[name] = f"source {name}".encode("ascii")
                source.write_bytes(source_contents[name])
                sources.append((name, source))
            with (work / ".lock").open("a+b") as lock:
                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                disks = working_disks(sources, work, False)
                self.assertEqual([name for name, _ in disks],
                                 ["contest", "root", "extra", "scratch"])
                working_contents = {}
                for (name, source), (_, target) in zip(sources, disks):
                    self.assertEqual(target.read_bytes(), source_contents[name])
                    self.assertFalse(target.samefile(source))
                    working_contents[name] = f"guest {name}".encode("ascii")
                    target.write_bytes(working_contents[name])
                for name, source in sources:
                    source_contents[name] = f"updated source {name}".encode("ascii")
                    source.write_bytes(source_contents[name])
                self.assertEqual(working_disks(sources, work, True), disks)
                self.assertEqual({name: path.read_bytes() for name, path in disks},
                                 working_contents)
                self.assertEqual(working_disks(sources, work, True, refresh_system=True), disks)
                working_contents["root"] = source_contents["root"]
                self.assertEqual({name: path.read_bytes() for name, path in disks},
                                 working_contents)
                with self.assertRaises(ValueError):
                    working_disks(sources, work, False, refresh_system=True)
                self.assertEqual({name: path.read_bytes() for name, path in disks},
                                 working_contents)
                self.assertEqual({name: path.read_bytes() for name, path in sources},
                                 source_contents)

    def test_work_clean_cli_refuses_live_lock_then_removes_only_owned_disks(self):
        """持锁子进程存活时清理失败，真实 wait 后只移除四个工作盘并保原锁 inode。"""
        with tempfile.TemporaryDirectory() as temporary:
            base = Path(temporary)
            work = base / "work"
            work.mkdir()
            contents = {}
            for name in ("contest.raw", "root.raw", "extra.raw", "scratch.raw",
                         "unrelated.img", "other.raw", "uart.log"):
                contents[name] = f"preserve {name}".encode("ascii")
                (work / name).write_bytes(contents[name])
            source = base / "source.img"
            source.write_bytes(b"original input")
            script = (
                "import fcntl,sys\n"
                "with open(sys.argv[1],'a+b') as lock:\n"
                " fcntl.flock(lock,fcntl.LOCK_EX|fcntl.LOCK_NB)\n"
                " print('locked',flush=True)\n"
                " sys.stdin.buffer.read(1)\n"
            )
            process = subprocess.Popen(
                [sys.executable, "-c", script, str(work / ".lock")],
                stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
            command = [sys.executable, str(TOOLS / "run.py"),
                       "--work-clean", "--work-dir", str(work)]
            try:
                readable, _, _ = select_io.select([process.stdout], [], [], 2)
                self.assertTrue(readable, "lock holder did not become ready")
                self.assertEqual(process.stdout.readline(), b"locked\n")
                self.assertIsNone(process.poll())
                lock_inode = (work / ".lock").stat().st_ino
                result = subprocess.run(command, capture_output=True, text=True, timeout=2)
                self.assertEqual(result.returncode, 1, result.stderr)
                self.assertEqual({name: (work / name).read_bytes() for name in contents},
                                 contents)
                self.assertIsNone(process.poll())
                self.assertEqual((work / ".lock").stat().st_ino, lock_inode)
                process.stdin.write(b"G")
                process.stdin.flush()
                self.assertEqual(process.wait(timeout=2), 0)
            finally:
                process.stdin.close()
                if process.poll() is None:
                    process.terminate()
                process.wait(timeout=2)
                process.stdout.close()
                process.stderr.close()
            result = subprocess.run(command, capture_output=True, text=True, timeout=2)
            self.assertEqual(result.returncode, 0, result.stderr)
            for name in ("contest.raw", "root.raw", "extra.raw", "scratch.raw"):
                self.assertFalse((work / name).exists())
            for name in ("unrelated.img", "other.raw", "uart.log"):
                self.assertEqual((work / name).read_bytes(), contents[name])
            self.assertEqual((work / ".lock").stat().st_ino, lock_inode)
            self.assertEqual(source.read_bytes(), b"original input")


if __name__ == "__main__":
    unittest.main()
