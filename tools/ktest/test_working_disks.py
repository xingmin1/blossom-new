"""生产工作盘的真实文件、来源消费和跨进程独占回归。"""

import errno
import os
from pathlib import Path
import select
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

from host_helpers import TOOLS
from working_disks import clean_working_disks, prepare_working_disks, working_disk_paths


class WorkingDiskTests(unittest.TestCase):
    """按正式 system/contest 两角色核对盘面与锁生命期。"""

    def test_fresh_reuse_and_refresh_preserve_independent_contents(self):
        """fresh 制副本，reuse 保客体修改，refresh 只替换系统盘。"""
        with tempfile.TemporaryDirectory() as temporary:
            base = Path(temporary)
            system, contest = base / "system.img", base / "contest.img"
            system.write_bytes(b"source system")
            contest.write_bytes(b"source contest")
            work = base / "work"
            for action in ("fresh", "reuse", "refresh-system"):
                with prepare_working_disks(work, system, contest, action) as (paths, lock_file):
                    self.assertFalse(lock_file.closed)
                    self.assertEqual(paths, {"system": work / "root.raw", "contest": work / "contest.raw"})
                    expected_system = b"guest system" if action == "reuse" else system.read_bytes()
                    expected_contest = b"source contest" if action == "fresh" else b"guest contest"
                    self.assertEqual(paths["system"].read_bytes(), expected_system)
                    self.assertEqual(paths["contest"].read_bytes(), expected_contest)
                    self.assertFalse(paths["system"].samefile(system))
                    self.assertFalse(paths["contest"].samefile(contest))
                    paths["system"].write_bytes(b"guest system")
                    paths["contest"].write_bytes(b"guest contest")
                self.assertTrue(lock_file.closed)
                system.write_bytes(b"updated system")
            self.assertEqual(system.read_bytes(), b"updated system")
            self.assertEqual(contest.read_bytes(), b"source contest")

    def test_symlink_workspace_parent_keeps_disk_ownership_and_lock_inode(self):
        """普通父目录链接不改变工作盘独占、原来源或清理后保锁的责任。"""
        with tempfile.TemporaryDirectory() as temporary:
            base = Path(temporary)
            workspace = base / "workspace"
            workspace.symlink_to(base, target_is_directory=True)
            source = base / "system.img"
            source.write_bytes(b"source bytes")
            work = workspace / "work"
            with prepare_working_disks(work, source, None, "fresh") as (paths, lock):
                inode = os.fstat(lock.fileno()).st_ino
                self.assertEqual(paths["system"].read_bytes(), b"source bytes")
                self.assertFalse(paths["system"].samefile(source))
                paths["system"].write_bytes(b"guest bytes")
                with self.assertRaises(BlockingIOError):
                    clean_working_disks(base / "work")
                self.assertEqual((base / "work/.lock").stat().st_ino, inode)
            with prepare_working_disks(work, base / "missing.img", None, "reuse") as (paths, lock):
                self.assertEqual(paths["system"].read_bytes(), b"guest bytes")
                self.assertEqual(os.fstat(lock.fileno()).st_ino, inode)
            clean_working_disks(work)
            self.assertFalse((base / "work/root.raw").exists())
            self.assertEqual((base / "work/.lock").stat().st_ino, inode)
            self.assertEqual(source.read_bytes(), b"source bytes")

    def test_unconsumed_sources_are_not_statted_resolved_or_opened(self):
        """来源消失后 reuse 可运行，refresh 不探测或读取比赛来源。"""
        with tempfile.TemporaryDirectory() as temporary:
            base = Path(temporary)
            system, contest = base / "system.img", base / "contest.img"
            system.write_bytes(b"system")
            contest.write_bytes(b"contest")
            work = base / "work"
            with prepare_working_disks(work, system, contest, "fresh"):
                pass
            system.unlink()
            contest.unlink()
            original_stat, original_resolve, original_open = Path.stat, Path.resolve, Path.open
            unconsumed = {system, contest}

            def guarded_stat(path, *args, **kwargs):
                """未消费来源的 metadata 操作立即暴露，其他真实文件照常处理。"""
                if path in unconsumed:
                    raise AssertionError(f"unexpected source stat: {path}")
                return original_stat(path, *args, **kwargs)

            def guarded_resolve(path, *args, **kwargs):
                """未消费来源不能因路径正规化触发实际文件系统解析。"""
                if path in unconsumed:
                    raise AssertionError(f"unexpected source resolve: {path}")
                return original_resolve(path, *args, **kwargs)

            def guarded_open(path, *args, **kwargs):
                """来源缺失时禁止通过摘要或复制隐式重新打开。"""
                if path in unconsumed:
                    raise AssertionError(f"unexpected source open: {path}")
                return original_open(path, *args, **kwargs)

            with patch.object(Path, "stat", guarded_stat), patch.object(Path, "resolve", guarded_resolve), \
                    patch.object(Path, "open", guarded_open):
                with prepare_working_disks(work, system, contest, "reuse") as (paths, _):
                    self.assertEqual(paths["system"].read_bytes(), b"system")
                    self.assertEqual(paths["contest"].read_bytes(), b"contest")
                unconsumed.remove(system)
                system.write_bytes(b"new system")
                with prepare_working_disks(work, system, contest, "refresh-system") as (paths, _):
                    self.assertEqual(paths["system"].read_bytes(), b"new system")
                    self.assertEqual(paths["contest"].read_bytes(), b"contest")
            self.assertFalse(contest.exists())

    def test_explicit_roles_and_missing_work_disks_do_not_select_fresh(self):
        """纯路径计算不创建目录，目录中旧盘不能改变选中角色或动作。"""
        with tempfile.TemporaryDirectory() as temporary:
            base = Path(temporary)
            work = base / "work"
            self.assertEqual(working_disk_paths(work, False), {"system": work / "root.raw"})
            self.assertFalse(work.exists())
            system = base / "system.img"
            system.write_bytes(b"system")
            work.mkdir()
            (work / "contest.raw").write_bytes(b"unselected contest")
            (work / "extra.raw").write_bytes(b"old extra")
            for action in ("reuse", "refresh-system"):
                with self.subTest(action=action), self.assertRaises(ValueError):
                    with prepare_working_disks(work, system, None, action):
                        pass
                self.assertFalse((work / "root.raw").exists())
            with prepare_working_disks(work, system, None, "fresh") as (paths, _):
                self.assertEqual(set(paths), {"system"})
                self.assertEqual(paths["system"].read_bytes(), b"system")
            self.assertEqual((work / "contest.raw").read_bytes(), b"unselected contest")
            self.assertEqual((work / "extra.raw").read_bytes(), b"old extra")
            with self.assertRaises(FileNotFoundError):
                with prepare_working_disks(work, system, base / "missing.img", "fresh"):
                    pass
            self.assertEqual((work / "root.raw").read_bytes(), b"system")
            (work / "contest.raw").unlink()
            for action in ("reuse", "refresh-system"):
                with self.subTest(action=action), self.assertRaises(ValueError):
                    with prepare_working_disks(work, system, base / "missing.img", action):
                        pass
            self.assertFalse((work / "contest.raw").exists())

    def test_fresh_prepares_all_copies_before_replacement_and_cleans_partial_files(self):
        """第二份副本发生真实部分写入后报错，两张旧工作盘都不被发布覆盖。"""
        with tempfile.TemporaryDirectory() as temporary:
            base = Path(temporary)
            work = base / "work"
            work.mkdir()
            system, contest = base / "system.img", base / "contest.img"
            system.write_bytes(b"new system")
            contest.write_bytes(b"new contest")
            (work / "root.raw").write_bytes(b"old system")
            (work / "contest.raw").write_bytes(b"old contest")
            partial = []
            no_space = OSError(errno.ENOSPC, "No space left on device")

            def copy_or_fail(source, target, length):
                """保留真实临时写入，在比赛副本写出前缀后注入 ENOSPC。"""
                target.write(source.read())
                partial.append(Path(target.name))
                if source.name == str(contest):
                    raise no_space

            with patch("working_disks.shutil.copyfileobj", side_effect=copy_or_fail):
                with self.assertRaises(OSError) as caught:
                    with prepare_working_disks(work, system, contest, "fresh"):
                        pass
            self.assertIs(caught.exception, no_space)
            self.assertEqual(len(partial), 2)
            self.assertFalse(partial[0].parent.exists())
            self.assertEqual((work / "root.raw").read_bytes(), b"old system")
            self.assertEqual((work / "contest.raw").read_bytes(), b"old contest")
            with prepare_working_disks(work, system, contest, "reuse"):
                pass
            self.assertEqual(system.read_bytes(), b"new system")
            self.assertEqual(contest.read_bytes(), b"new contest")

    def test_working_files_reject_aliases_and_non_regular_targets(self):
        """符号链接、硬链接、目录和 FIFO 不能作为生产工作盘。"""
        with tempfile.TemporaryDirectory() as temporary:
            base = Path(temporary)
            source = base / "system.img"
            source.write_bytes(b"source")
            for kind in ("symlink", "hardlink", "directory", "fifo"):
                work = base / kind
                work.mkdir()
                target = work / "root.raw"
                if kind == "symlink":
                    target.symlink_to(source)
                elif kind == "hardlink":
                    target.hardlink_to(source)
                elif kind == "directory":
                    target.mkdir()
                else:
                    os.mkfifo(target)
                for action in ("fresh", "reuse", "refresh-system"):
                    with self.subTest(kind=kind, action=action), self.assertRaises(ValueError):
                        with prepare_working_disks(work, source, None, action):
                            pass
                self.assertEqual(source.read_bytes(), b"source")
            work = base / "ordinary"
            work.mkdir()
            target = work / "root.raw"
            target.write_bytes(b"working input")
            alias = base / "source-link.img"
            alias.symlink_to(target)
            for source_path in (target, alias):
                with self.subTest(source=source_path), self.assertRaises(ValueError):
                    with prepare_working_disks(work, source_path, None, "fresh"):
                        pass
                self.assertEqual(target.read_bytes(), b"working input")
            for action in ("reuse", "refresh-system"):
                with self.subTest(action=action), self.assertRaises(ValueError):
                    with prepare_working_disks(work, target, None, action):
                        pass

    def test_non_regular_sources_and_working_directories_fail_without_replacement(self):
        """源设备类文件与非普通工作目录保留真实错误，不转换成可运行副本。"""
        with tempfile.TemporaryDirectory() as temporary:
            base = Path(temporary)
            work = base / "work"
            work.mkdir()
            (work / "root.raw").write_bytes(b"preserved")
            for kind in ("directory", "fifo"):
                source = base / kind
                if kind == "directory":
                    source.mkdir()
                else:
                    os.mkfifo(source)
                with self.subTest(source=source), self.assertRaises(ValueError):
                    with prepare_working_disks(work, source, None, "fresh"):
                        pass
                self.assertEqual((work / "root.raw").read_bytes(), b"preserved")
            source = base / "system.img"
            source.write_bytes(b"source")
            linked = base / "linked-work"
            linked.symlink_to(work, target_is_directory=True)
            ordinary_file = base / "file-work"
            ordinary_file.write_bytes(b"not directory")
            for directory in (linked, ordinary_file):
                with self.subTest(directory=directory), self.assertRaises((ValueError, OSError)):
                    with prepare_working_disks(directory, source, None, "fresh"):
                        pass
                with self.assertRaises(ValueError):
                    clean_working_disks(directory)
            self.assertEqual(source.read_bytes(), b"source")
            self.assertEqual((work / "root.raw").read_bytes(), b"preserved")

    def test_clean_checks_all_targets_before_removing_only_owned_files(self):
        """清理先核两角色再删除；缺目录无动作，实验盘和锁 inode 不删除。"""
        with tempfile.TemporaryDirectory() as temporary:
            base = Path(temporary)
            work = base / "missing"
            clean_working_disks(work)
            self.assertFalse(work.exists())
            work.mkdir()
            root = work / "root.raw"
            root.write_bytes(b"system")
            source = base / "source.img"
            source.write_bytes(b"source")
            contest = work / "contest.raw"
            for symbolic in (True, False):
                if symbolic:
                    contest.symlink_to(source)
                else:
                    contest.hardlink_to(source)
                with self.subTest(symbolic=symbolic), self.assertRaises(ValueError):
                    clean_working_disks(work)
                self.assertEqual(root.read_bytes(), b"system")
                self.assertEqual(source.read_bytes(), b"source")
                contest.unlink()
            contest.write_bytes(b"contest")
            preserved = {name: name.encode() for name in
                         ("extra.raw", "scratch.raw", "unrelated.img", "other.raw", "uart.log")}
            for name, content in preserved.items():
                (work / name).write_bytes(content)
            inode = (work / ".lock").stat().st_ino
            clean_working_disks(work)
            self.assertFalse(root.exists())
            self.assertFalse(contest.exists())
            self.assertTrue(work.is_dir())
            self.assertEqual((work / ".lock").stat().st_ino, inode)
            self.assertEqual({name: (work / name).read_bytes() for name in preserved}, preserved)
            self.assertEqual(source.read_bytes(), b"source")

    def test_lock_covers_copying_and_exec_until_process_wait(self):
        """准备时已有独占锁，继承 FD 的实际用盘进程退出后才允许清理。"""
        with tempfile.TemporaryDirectory() as temporary:
            base = Path(temporary)
            work = base / "work"
            system, contest = base / "system.img", base / "contest.img"
            system.write_bytes(b"system")
            contest.write_bytes(b"contest")
            clean = [sys.executable, str(TOOLS / "working_disks.py"), "clean",
                     "--arch", "riscv64", "--working-disk-dir", str(work)]
            import shutil
            original_copy = shutil.copyfileobj

            def copy_under_lock(source, target, length):
                """独立清理进程在真实复制期间必须因活锁失败。"""
                result = subprocess.run(clean, capture_output=True, timeout=2)
                self.assertEqual(result.returncode, 1, result.stderr)
                original_copy(source, target, length)

            with patch("working_disks.shutil.copyfileobj", side_effect=copy_under_lock):
                with prepare_working_disks(work, system, contest, "fresh"):
                    pass
            script = (
                "import os,sys\n"
                "from pathlib import Path\n"
                "sys.path.insert(0,sys.argv[1])\n"
                "from working_disks import prepare_working_disks\n"
                "with prepare_working_disks(Path(sys.argv[2]),Path(sys.argv[3]),Path(sys.argv[4]),'reuse') as (paths,lock):\n"
                " os.set_inheritable(lock.fileno(),True)\n"
                " os.execv(sys.executable,[sys.executable,'-c',"
                "\"import sys;from pathlib import Path;"
                "p=Path(sys.argv[1]);f=p.open('r+b');"
                "assert f.read()==b'system';f.seek(0);f.write(b'guest!');f.flush();"
                "print('disks opened',flush=True);sys.stdin.buffer.read(1)\",str(paths['system'])])\n"
            )
            process = subprocess.Popen([sys.executable, "-c", script, str(TOOLS), str(work),
                                        str(system), str(contest)], stdin=subprocess.PIPE,
                                       stdout=subprocess.PIPE, stderr=subprocess.PIPE)
            try:
                readable, _, _ = select.select([process.stdout], [], [], 5)
                self.assertTrue(readable, "disk holder did not become ready")
                self.assertEqual(process.stdout.readline(), b"disks opened\n")
                self.assertIsNone(process.poll())
                inode = (work / ".lock").stat().st_ino
                result = subprocess.run(clean, capture_output=True, timeout=2)
                self.assertEqual(result.returncode, 1, result.stderr)
                with self.assertRaises(BlockingIOError):
                    with prepare_working_disks(work, system, contest, "reuse"):
                        pass
                self.assertIsNone(process.poll())
                self.assertEqual((work / "root.raw").read_bytes(), b"guest!")
                self.assertEqual((work / "contest.raw").read_bytes(), b"contest")
                self.assertEqual((work / ".lock").stat().st_ino, inode)
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
            result = subprocess.run(clean, capture_output=True, timeout=2)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertFalse((work / "root.raw").exists())
            self.assertFalse((work / "contest.raw").exists())
            self.assertEqual((work / ".lock").stat().st_ino, inode)
            self.assertEqual(system.read_bytes(), b"system")
            self.assertEqual(contest.read_bytes(), b"contest")

    def test_clean_cli_lexically_normalizes_dotdot_before_directory_access(self):
        """绝对和项目根相对目录的 .. 不访问已消去的缺失组件或符号链接。"""
        with tempfile.TemporaryDirectory() as temporary:
            base = Path(temporary)
            work = base / "work"
            work.mkdir()
            (work / ".lock").write_bytes(b"")
            inode = (work / ".lock").stat().st_ino
            (work / "extra.raw").write_bytes(b"preserved experiment")
            source = base / "source.img"
            source.write_bytes(b"preserved source")
            elsewhere = base / "elsewhere"
            (elsewhere / "child").mkdir(parents=True)
            (elsewhere / "work").mkdir()
            (elsewhere / "work" / "root.raw").write_bytes(b"preserved symlink destination")
            (base / "link").symlink_to(elsewhere / "child", target_is_directory=True)
            command = [sys.executable, str(TOOLS / "working_disks.py"), "clean",
                       "--arch", "riscv64", "--working-disk-dir"]
            for component in ("missing-component", "link"):
                raw_directory = base / component / ".." / "work"
                for directory in (str(raw_directory), os.path.relpath(raw_directory, TOOLS.parent)):
                    with self.subTest(component=component, directory=directory):
                        (work / "root.raw").write_bytes(b"system")
                        (work / "contest.raw").write_bytes(b"contest")
                        result = subprocess.run(command + [directory], cwd=base,
                                                capture_output=True, timeout=2)
                        self.assertEqual(result.returncode, 0, result.stderr)
                        self.assertFalse((work / "root.raw").exists())
                        self.assertFalse((work / "contest.raw").exists())
                        self.assertEqual((work / ".lock").stat().st_ino, inode)
                        self.assertEqual((work / "extra.raw").read_bytes(), b"preserved experiment")
                        self.assertEqual(source.read_bytes(), b"preserved source")
                        self.assertEqual((elsewhere / "work" / "root.raw").read_bytes(),
                                         b"preserved symlink destination")
                        self.assertFalse((base / "missing-component").exists())
            absent = base / "missing-component" / ".." / "absent-work"
            result = subprocess.run(command + [str(absent)], capture_output=True, timeout=2)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertFalse((base / "absent-work").exists())
            self.assertFalse((base / "missing-component").exists())

    def test_clean_cli_rejects_long_option_abbreviations_without_creating_directory(self):
        """清理 CLI 只接受完整参数，解析失败时不创建工作目录。"""
        with tempfile.TemporaryDirectory() as temporary:
            work = Path(temporary) / "work"
            result = subprocess.run([sys.executable, str(TOOLS / "working_disks.py"), "clean",
                                     "--arch", "riscv64", "--working-disk-d", str(work)],
                                    capture_output=True, timeout=2)
            self.assertEqual(result.returncode, 2)
            self.assertFalse(work.exists())


if __name__ == "__main__":
    unittest.main()
