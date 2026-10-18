"""生产 QEMU 入口的真实子进程参数、只读展示与失败边界。"""

import fcntl
import os
from pathlib import Path
import shlex
import subprocess
import sys
import tempfile
import unittest

from test_boot import kernel_image

TOOLS = Path(__file__).resolve().parents[1]
REPOSITORY_ROOT = TOOLS.parent


class RunQemuTests(unittest.TestCase):
    """只经过公开 CLI 检查行为，不检查源码、导入关系或 mock 转发。"""

    def invoke(self, arguments, cwd=None):
        """从任意宿主目录启动真实 Python 入口并保留退出状态和原文输出。"""
        return subprocess.run([sys.executable, str(TOOLS / "run_qemu.py"), *arguments],
                              cwd=cwd, capture_output=True, text=True, timeout=10)

    def printed_command(self, arguments, cwd=None):
        """把展示文本按 shell 展示规则还原成 argv，供实际参数行为断言。"""
        result = self.invoke([*arguments, "--print-command"], cwd)
        self.assertEqual(result.returncode, 0, result.stderr)
        return shlex.split(result.stdout), result.stderr

    def test_help_and_print_do_not_touch_selected_paths(self):
        """缺失输入、锁和工作目录不影响 help 或展示，也不会触发创建和制盘。"""
        with tempfile.TemporaryDirectory() as temporary:
            base = Path(temporary)
            marker = base / "preserved"
            marker.write_bytes(b"untouched")
            common = ["--arch", "riscv64", "--kernel-elf", str(base / "missing.elf"),
                      "--system-image", str(base / "missing-system.img"),
                      "--contest-image", str(base / "missing-contest.img"),
                      "--working-disk-dir", str(base / "missing-work"),
                      "--qemu-binary", str(base / "missing-qemu")]
            before = sorted(base.iterdir())
            for tail in (["--help"], ["--print-command"],
                         ["--working-disk-action", "reuse", "--print-command"],
                         ["--working-disk-action", "refresh-system", "--print-command"]):
                with self.subTest(tail=tail):
                    result = self.invoke([*common, *tail], base)
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertEqual(sorted(base.iterdir()), before)
                    self.assertEqual(marker.read_bytes(), b"untouched")
            result = self.invoke(["--help"], base)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(sorted(base.iterdir()), before)

    def test_default_paths_and_machine_options_are_architecture_specific(self):
        """两 ISA 使用固定默认角色来源，生产默认没有 append 或调试暂停。"""
        with tempfile.TemporaryDirectory() as temporary:
            for architecture, suffix, system in (("riscv64", "rv", "disk.img"),
                                                 ("loongarch64", "la", "disk-la.img")):
                with self.subTest(architecture=architecture):
                    command, summary = self.printed_command(["--arch", architecture], temporary)
                    self.assertEqual(command[0], f"qemu-system-{architecture}")
                    self.assertEqual(command[command.index("-kernel") + 1], str(REPOSITORY_ROOT / f"kernel-{suffix}"))
                    self.assertEqual(command[command.index("-smp") + 1], "1")
                    self.assertEqual(command[command.index("-m") + 1], "1G")
                    self.assertNotIn("-append", command)
                    self.assertNotIn("-S", command)
                    self.assertNotIn("-gdb", command)
                    self.assertIn(f"source={REPOSITORY_ROOT / system}", summary)
                    self.assertIn(f"source={REPOSITORY_ROOT / f'sdcard-{suffix}.img'}", summary)
                    self.assertEqual(command.count("-drive"), 2)
                    self.assertIn("user,id=", command[command.index("-netdev") + 1])

    def test_print_does_not_take_an_existing_active_disk_lock(self):
        """展示分支可面对正在使用的目录，只读保留工作盘和锁 inode。"""
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            disk = directory / "root.raw"
            disk.write_bytes(b"active working disk")
            with (directory / ".lock").open("a+b") as lock:
                fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
                inode = os.fstat(lock.fileno()).st_ino
                self.printed_command(["--arch", "riscv64", "--working-disk-dir", str(directory)])
                self.assertEqual((directory / ".lock").stat().st_ino, inode)
                self.assertEqual(disk.read_bytes(), b"active working disk")
                self.assertEqual(sorted(path.name for path in directory.iterdir()), [".lock", "root.raw"])

    def test_orthogonal_overrides_only_add_explicit_boot_arguments(self):
        """Shell、根权限、单盘和 GDB 不替换 ELF、来源、CPU、内存或网络。"""
        base = ["--arch", "riscv64", "--kernel-elf", "chosen/kernel.elf",
                "--system-image", "chosen/system.img", "--contest-image", "chosen/contest.img"]
        default, _ = self.printed_command(base)
        shell, _ = self.printed_command([*base, "--shell"])
        debug, _ = self.printed_command([*base, "--debug"])
        both, _ = self.printed_command([*base, "--shell", "--debug"])
        self.assertEqual(shell, default + ["-append", "blossom.boot=shell"])
        self.assertEqual(debug, default + ["-S", "-gdb", "tcp:127.0.0.1:1234"])
        self.assertEqual(both, shell + ["-S", "-gdb", "tcp:127.0.0.1:1234"])
        single, summary = self.printed_command([*base, "--system-only", "--shell", "--root-mode", "rw"])
        self.assertEqual(single.count("-drive"), 1)
        self.assertEqual(single[single.index("-append") + 1], "root=/dev/vda rw blossom.boot=shell")
        self.assertNotIn("role=contest", summary)
        self.assertIn("-netdev", single)
        init, _ = self.printed_command([*base, "--system-only", "--init-program", "/bin/probe"])
        self.assertEqual(init[init.index("-append") + 1], "root=/dev/vda init=/bin/probe")
        root_mode, _ = self.printed_command([*base, "--root-mode", "ro"])
        self.assertEqual(root_mode[root_mode.index("-append") + 1], "ro")

    def test_invalid_options_fail_before_any_file_change(self):
        """必填项、组合、缩写和数值错误均由入口拒绝，不读取或替换盘。"""
        invalid = [[], ["--arch", "mips"], ["--arc", "riscv64"],
                   ["--arch", "riscv64", "--system-only"],
                   ["--arch", "riscv64", "--shell", "--init-program", "/bin/sh"],
                   ["--arch", "riscv64", "--network", "none", "--host-forward", "tcp::1234-:80"],
                   ["--arch", "riscv64", "--gdb-port", "1234"]]
        invalid += [["--arch", "riscv64", "--debug", "--gdb-port", value]
                    for value in ("0", "65536", "not-a-port")]
        invalid += [["--arch", "riscv64", "--cpus", value] for value in ("0", "-1", "one")]
        invalid += [["--arch", "riscv64", "--init-program", value]
                    for value in ("", "/bin/a b", "/bin/a\tb", "/bin/a\nb", "/bin/a\rb", "/bin/a\vb", "/bin/a\fb")]
        invalid += [["--arch", "riscv64", "--root-mode", value] for value in ("", "RO", "ro rw")]
        invalid += [["--arch", "riscv64", option] for option in ("--single", "--interactive", "--boot-mode", "--rootdir")]
        with tempfile.TemporaryDirectory() as temporary:
            base = Path(temporary)
            work = base / "work"
            work.mkdir()
            disk = work / "root.raw"
            disk.write_bytes(b"preserved")
            for arguments in invalid:
                with self.subTest(arguments=arguments):
                    result = self.invoke([*arguments, "--working-disk-dir", str(work), "--print-command"], base)
                    self.assertEqual(result.returncode, 2, result.stderr)
                    self.assertEqual(sorted(path.name for path in work.iterdir()), ["root.raw"])
                    self.assertEqual(disk.read_bytes(), b"preserved")

    def test_valid_numeric_edges_and_memory_are_forwarded_without_extra_policy(self):
        """CPU 接受所有正整数；内存语法留给 QEMU，GDB 接受端口两端边界。"""
        for port in (1, 65535):
            with self.subTest(port=port):
                command, _ = self.printed_command(["--arch", "loongarch64", "--cpus", "5",
                                                  "--memory", "QEMU-validates-this", "--debug",
                                                  "--gdb-port", str(port), "--network", "none"])
                self.assertEqual(command[command.index("-smp") + 1], "5")
                self.assertEqual(command[command.index("-m") + 1], "QEMU-validates-this")
                self.assertEqual(command[command.index("-gdb") + 1], f"tcp:127.0.0.1:{port}")
                self.assertNotIn("-netdev", command)

    def test_special_paths_are_repository_relative_and_round_trip_argv(self):
        """空格、逗号和引号在真实 argv 中保留，路径不受调用者 cwd 影响。"""
        with tempfile.TemporaryDirectory() as temporary:
            relative = Path(os.path.relpath(
                Path(temporary) / "missing parent" / "lexical" / ".." / "chosen , 'quoted'", REPOSITORY_ROOT))
            absolute = Path(os.path.abspath(REPOSITORY_ROOT / relative))
            command, summary = self.printed_command([
                "--arch", "riscv64", "--kernel-elf", str(relative / "kernel.elf"),
                "--system-image", str(relative / "system.img"),
                "--contest-image", str(relative / "contest.img"),
                "--working-disk-dir", str(relative / "work , 'dir'"),
                "--qemu-binary", str(relative / "qemu binary"),
            ], temporary)
            self.assertEqual(command[0], str(absolute / "qemu binary"))
            self.assertEqual(command[command.index("-kernel") + 1], str(absolute / "kernel.elf"))
            self.assertIn(f"source={absolute / 'system.img'}", summary)
            self.assertIn(f"source={absolute / 'contest.img'}", summary)
            for value in (value for value in command if value.startswith("if=none,")):
                self.assertIn(str(absolute).replace(",", ",,"), value)
            self.assertEqual(list(Path(temporary).iterdir()), [])
            self.assertFalse(absolute.exists())

    def test_source_consumption_summary_follows_action(self):
        """reuse 不声明消费来源，refresh-system 只声明消费系统来源。"""
        for action, system, contest in (("fresh", "true", "true"), ("reuse", "false", "false"),
                                        ("refresh-system", "true", "false")):
            with self.subTest(action=action):
                _, summary = self.printed_command(["--arch", "riscv64", "--working-disk-action", action])
                roles = {line.split("role=", 1)[1].split()[0]: line
                         for line in summary.splitlines() if "role=" in line}
                self.assertIn(f"source-consumed={system}", roles["system"])
                self.assertIn(f"source-consumed={contest}", roles["contest"])

    def test_runtime_validates_elf_and_executable_before_preparing_disks(self):
        """真正启动时缺文件、错 ISA、坏 ELF 与不可执行二进制保留真实错误。"""
        with tempfile.TemporaryDirectory() as temporary:
            base = Path(temporary)
            kernel = base / "kernel.elf"
            work = base / "work"
            binary = base / "not-executable"
            binary.write_bytes(b"not an executable")
            common = ["--arch", "riscv64", "--kernel-elf", str(kernel),
                      "--qemu-binary", str(binary), "--working-disk-dir", str(work)]
            for image in (None, b"not ELF", bytes(kernel_image("loongarch64")[0]),
                          bytes(kernel_image("riscv64")[0])):
                with self.subTest(image=None if image is None else len(image)):
                    if image is not None:
                        kernel.write_bytes(image)
                    result = self.invoke(common, base)
                    self.assertEqual(result.returncode, 1, result.stderr)
                    self.assertFalse(work.exists())

    def test_exec_inherits_lock_and_exit_status_and_reuse_ignores_missing_sources(self):
        """真实 exec 子进程持有目录锁并返回原状态；reuse 不要求来源继续存在。"""
        with tempfile.TemporaryDirectory() as temporary:
            base = Path(temporary)
            kernel = base / "kernel.elf"
            kernel.write_bytes(kernel_image("riscv64")[0])
            system = base / "system.img"
            contest = base / "contest.img"
            system.write_bytes(b"system input")
            contest.write_bytes(b"contest input")
            work = base / "work"
            executable = base / "lock-observer"
            executable.write_text(
                f"#!{sys.executable}\n"
                "import fcntl,sys\n"
                f"with open({str(work / '.lock')!r}, 'a+b') as lock:\n"
                "    try:\n"
                "        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)\n"
                "    except BlockingIOError:\n"
                "        print('inherited lock held', flush=True)\n"
                "        sys.exit(37)\n"
                "sys.exit(0)\n", encoding="utf-8")
            executable.chmod(0o700)
            arguments = ["--arch", "riscv64", "--kernel-elf", str(kernel),
                         "--system-image", str(system), "--contest-image", str(contest),
                         "--working-disk-dir", str(work), "--qemu-binary", str(executable)]
            fresh = self.invoke(arguments, base)
            self.assertEqual(fresh.returncode, 37, fresh.stderr)
            self.assertIn("inherited lock held", fresh.stdout)
            self.assertEqual(system.read_bytes(), b"system input")
            self.assertEqual(contest.read_bytes(), b"contest input")
            (work / "root.raw").write_bytes(b"preserved system work")
            (work / "contest.raw").write_bytes(b"preserved contest work")
            system.unlink()
            contest.unlink()
            reused = self.invoke([*arguments, "--working-disk-action", "reuse"], base)
            self.assertEqual(reused.returncode, 37, reused.stderr)
            self.assertEqual((work / "root.raw").read_bytes(), b"preserved system work")
            self.assertEqual((work / "contest.raw").read_bytes(), b"preserved contest work")


if __name__ == "__main__":
    unittest.main()
