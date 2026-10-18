"""一次用户自测根树的真实文件、只读输入与输出失败边界。"""

from contextlib import redirect_stderr
import io
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest
from unittest.mock import patch

from host_helpers import TOOLS
from prepare_user_selftest_rootfs import main, prepare_user_selftest_rootfs
from test_rootfs import tree_contents


HAS_E2FSPROGS = all(shutil.which(name) for name in ("mke2fs", "debugfs", "e2fsck", "dumpe2fs"))
RUN_ID = "0123456789abcdef0123456789abcdef"


class UserSelftestRootfsTests(unittest.TestCase):
    """只消费安装产物，不模拟 Guest 的 PID1 或 TAP 判定。"""

    def setUp(self):
        """准备带特殊路径的独立安装源和可检查的真实字节。"""
        temporary = tempfile.TemporaryDirectory(prefix="selftest-rootfs-")
        self.addCleanup(temporary.cleanup)
        self.base = Path(temporary.name)
        self.userland = self.base / "installed user ' $ files"
        self.runtime = self.base / "installed libraries"
        self.output = self.base / "owned root ' ; $(not-a-command)"
        self.image = self.base / "owned image ' ; $(not-a-command).ext4"
        for name in ("bin/busybox", "bin/libc-smoke", "sbin/contest-run", "sbin/user-selftests",
                     "test/selftests/custom/probe", "test/selftests/custom/helper"):
            path = self.userland / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(b"installed:" + name.encode())
            path.chmod(0o755)
        (self.userland / "bin/sh").symlink_to("busybox")
        (self.userland / "sbin/init").symlink_to("../bin/busybox")
        self.manifest = self.userland / "test/selftests/kselftest-list.txt"
        self.manifest.write_text("custom:probe\n", encoding="utf-8")
        for relative in ("musl/lib/libc.so", "glibc/lib/libc.so.6", "glibc/lib/libm.so.6"):
            path = self.runtime / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(b"runtime:" + relative.encode())

    def inputs(self):
        """记录所有安装源和正常系统配置，写入它们会使本例失败。"""
        return tuple(tree_contents(path) for path in (
            self.userland, self.runtime, TOOLS.parent / "userland/system"))

    def prepare(self, **kwargs):
        """调用公开准备接口，几何限定为最小合法的真实 ext4。"""
        return prepare_user_selftest_rootfs(
            "riscv64", self.userland, self.runtime, self.output, self.image,
            image_size_mib=128, run_id=RUN_ID, **kwargs)

    @unittest.skipUnless(HAS_E2FSPROGS, "requires e2fsprogs")
    def test_preparation_removes_installed_stale_results_from_tree_and_volume(self):
        """安装源保留原字节，但复制出的树和实际新卷都不得携带旧结果。"""
        stale = self.userland / "test/selftests/exit-status"
        stale.write_bytes(b"run=ffffffffffffffffffffffffffffffff status=0\n")
        before = self.inputs()
        self.prepare(collections=["custom"], programs=["custom:probe"],
                     program_timeout_seconds=7)
        self.assertEqual(self.inputs(), before)
        self.assertFalse((self.output / "test/selftests/exit-status").exists())
        self.assertTrue(os.access(self.output / "sbin/run-user-selftests", os.X_OK))
        result = subprocess.run(
            ["debugfs", "-R", "cat /test/selftests/exit-status", str(self.image)],
            capture_output=True, check=True)
        self.assertEqual(result.stdout, b"")

    @unittest.skipUnless(HAS_E2FSPROGS, "requires e2fsprogs")
    def test_repreparation_replaces_only_owned_tree_and_reflects_installed_deletion(self):
        """再次显式准备反映安装变化，不保留旧输出或生成第二份程序表。"""
        self.prepare()
        (self.output / "stale-output").write_bytes(b"obsolete")
        (self.userland / "test/selftests/custom/helper").unlink()
        self.manifest.write_text("custom:replacement\n", encoding="utf-8")
        before = self.inputs()
        self.prepare()
        self.assertEqual(self.inputs(), before)
        self.assertFalse((self.output / "stale-output").exists())
        self.assertFalse((self.output / "test/selftests/custom/helper").exists())
        self.assertEqual((self.output / "test/selftests/kselftest-list.txt").read_bytes(),
                         b"custom:replacement\n")

    def test_missing_installed_runner_or_manifest_preserves_existing_outputs(self):
        """缺少真正消费的安装文件时，在装配或制卷之前失败。"""
        self.output.mkdir()
        (self.output / "preserved").write_bytes(b"old tree")
        self.image.write_bytes(b"old volume")
        before = tree_contents(self.output)
        for relative in ("sbin/user-selftests", "test/selftests/kselftest-list.txt"):
            path = self.userland / relative
            original = path.read_bytes()
            path.unlink()
            with self.subTest(relative=relative), self.assertRaises(ValueError):
                self.prepare()
            self.assertEqual(tree_contents(self.output), before)
            self.assertEqual(self.image.read_bytes(), b"old volume")
            path.write_bytes(original)

    def test_input_overlap_symlinks_and_invalid_selection_do_not_write_inputs(self):
        """输出不得覆盖安装输入、沿外部链接写入或把 NUL 放入 Shell argv。"""
        before = self.inputs()
        for output, image in ((self.userland, self.image),
                              (self.userland / "nested", self.image),
                              (self.output, self.runtime / "new.ext4"),
                              (self.output, self.output / "nested.ext4")):
            with self.subTest(output=output, image=image), self.assertRaises(ValueError):
                prepare_user_selftest_rootfs("riscv64", self.userland, self.runtime,
                                             output, image, image_size_mib=128, run_id=RUN_ID)
        self.output.symlink_to(self.userland, target_is_directory=True)
        with self.assertRaises(ValueError):
            self.prepare()
        self.output.unlink()
        ancestor = self.base / "aliased-parent"
        ancestor.symlink_to(self.runtime, target_is_directory=True)
        with self.assertRaises(ValueError):
            prepare_user_selftest_rootfs("riscv64", self.userland, self.runtime,
                                         ancestor / "root", self.image, run_id=RUN_ID)
        for selection in ("", "custom:\0probe"):
            with self.subTest(selection=selection), self.assertRaises(ValueError):
                self.prepare(programs=[selection])
        for timeout in (0, -1, 1.5):
            with self.subTest(timeout=timeout), self.assertRaises(ValueError):
                self.prepare(program_timeout_seconds=timeout)
        self.assertEqual(self.inputs(), before)
        self.assertFalse(self.output.exists())
        self.assertFalse(self.image.exists())

    def test_real_tool_discovery_failure_keeps_previous_volume_and_input_bytes(self):
        """宿主工具缺失保留原卷及明确错误，不伪造制卷成功。"""
        self.image.write_bytes(b"previous volume")
        before = self.inputs()
        with patch.dict(os.environ, {"PATH": ""}), self.assertRaises(ValueError):
            self.prepare()
        self.assertEqual(self.image.read_bytes(), b"previous volume")
        self.assertEqual(self.inputs(), before)

    def test_cli_reports_preparation_error_without_creating_outputs(self):
        """CLI 的输入错误形成非零返回和 stderr，正常系统配置不受影响。"""
        self.manifest.unlink()
        before = self.inputs()
        error = io.StringIO()
        with redirect_stderr(error):
            status = main(["--arch", "loongarch64", "--guest-userland-directory", str(self.userland),
                           "--runtime-libraries-directory", str(self.runtime),
                           "--output-directory", str(self.output), "--output-image", str(self.image),
                           "--run-id", RUN_ID])
        self.assertEqual(status, 1)
        self.assertTrue(error.getvalue())
        self.assertEqual(self.inputs(), before)
        self.assertFalse(self.output.exists())
        self.assertFalse(self.image.exists())


if __name__ == "__main__":
    unittest.main()
