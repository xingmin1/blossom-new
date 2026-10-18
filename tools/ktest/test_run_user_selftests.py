"""自动自测的真实盘面协议、子进程期限和工作盘锁生命周期。"""

import fcntl
import os
from pathlib import Path
import shutil
import signal
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

import test_user_selftest_rootfs
from host_helpers import TOOLS
from run_qemu import configure_qemu, parse_arguments, prepare_qemu
from run_user_selftests import (HOST_CANCELLATION_SIGNALS, host_deadline, read_result,
                               run_host_process, stop_and_wait, wait_for_qemu)
from test_boot import kernel_image

RUN_ID = "0123456789abcdef0123456789abcdef"
HAS_E2FSPROGS = all(shutil.which(name) for name in ("mke2fs", "debugfs"))


class UserSelftestHostTests(unittest.TestCase):
    """小卷只验证离线协议，真实宿主进程只验证等待；不模拟目标 ISA 的测试成功。"""

    def setUp(self):
        """每例拥有独立输入、工作目录和 UART，结束后删除临时产物。"""
        temporary = tempfile.TemporaryDirectory(prefix="selftest-host-")
        self.addCleanup(temporary.cleanup)
        self.base = Path(temporary.name)
        self.image = self.base / "source.ext4"

    def make_volume(self):
        """使用真实 e2fsprogs 建立仅含结果目录的小卷，不调用生产容量策略。"""
        tree = self.base / "tree"
        (tree / "test/selftests").mkdir(parents=True)
        with self.image.open("wb") as stream:
            stream.truncate(8 * 1024 * 1024)
        subprocess.run(["mke2fs", "-q", "-F", "-t", "ext4", "-O", "^has_journal",
                        "-d", str(tree), str(self.image)], capture_output=True, check=True)

    def replace_result(self, image, content):
        """通过真实 debugfs 改变指定卷的结果，源卷和工作卷始终是不同文件。"""
        subprocess.run(["debugfs", "-w", "-R", "rm /test/selftests/exit-status", str(image)],
                       capture_output=True, check=True)
        if content is not None:
            payload = self.base / "result.bytes"
            payload.write_bytes(content)
            subprocess.run(["debugfs", "-w", "-R",
                            f"write {payload} /test/selftests/exit-status", str(image)],
                           capture_output=True, check=True)

    def wait_for_path(self, path, process):
        """等待受控真实进程发表阶段标记，子进程提前退出或停滞都直接失败。"""
        deadline = time.monotonic() + 5
        while not path.exists():
            self.assertIsNone(process.poll(), "host exited before reaching the controlled stage")
            self.assertLess(time.monotonic(), deadline, f"missing lifecycle marker: {path}")
            time.sleep(0.01)

    @unittest.skipUnless(HAS_E2FSPROGS, "requires e2fsprogs")
    def test_complete_current_record_preserves_status_and_rejects_other_records(self):
        """四种完整本轮结果原样传播；旧整数、旧轮次、截断和额外字节均拒绝。"""
        self.make_volume()
        for status in (0, 1, 2, 4):
            with self.subTest(status=status):
                self.replace_result(self.image, f"run={RUN_ID} status={status}\n".encode())
                self.assertEqual(read_result(self.image, RUN_ID, time.monotonic() + 5), status)
        valid = f"run={RUN_ID} status=0\n".encode()
        invalid = [None, b"", b"0\n", valid[:-1], valid + b"\n", valid + valid,
                   valid.replace(b"status=0", b"status=3"), valid.replace(b"\n", b"\r\n"),
                   valid.replace(RUN_ID.encode(), b"f" * 32),
                   valid.replace(RUN_ID.encode(), RUN_ID.upper().encode())]
        for record in invalid:
            with self.subTest(record=record):
                self.replace_result(self.image, record)
                with self.assertRaises(ValueError):
                    read_result(self.image, RUN_ID, time.monotonic() + 5)
        self.image.write_bytes(b"not an ext4 volume")
        with self.assertRaises((ValueError, subprocess.CalledProcessError)):
            read_result(self.image, RUN_ID, time.monotonic() + 5)

    @unittest.skipUnless(HAS_E2FSPROGS, "requires e2fsprogs")
    def test_production_context_keeps_lock_through_wait_and_working_volume_read(self):
        """真实子进程退出后锁仍在；只读工作卷结果，不读已改成另一状态的来源卷。"""
        self.make_volume()
        kernel = self.base / "kernel.elf"
        kernel.write_bytes(kernel_image("riscv64")[0])
        work = self.base / "work"
        _, arguments = parse_arguments([
            "--arch", "riscv64", "--kernel-elf", str(kernel),
            "--system-image", str(self.image), "--working-disk-dir", str(work),
            "--system-only", "--init-program", "/sbin/init", "--qemu-binary", sys.executable])
        configuration = configure_qemu(arguments)
        with prepare_qemu(configuration) as (_, environment, disks, _):
            self.replace_result(disks["system"], f"run={RUN_ID} status=4\n".encode())
            self.replace_result(self.image, f"run={RUN_ID} status=0\n".encode())
            child = (
                "import fcntl,sys\n"
                f"with open({str(work / '.lock')!r}, 'r+b') as lock:\n"
                "    try:\n"
                "        fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)\n"
                "    except BlockingIOError:\n"
                "        sys.exit(0)\n"
                "sys.exit(1)\n")
            self.assertEqual(wait_for_qemu([sys.executable, "-c", child], environment,
                                          work / "child.uart", time.monotonic() + 5), 0)
            with (work / ".lock").open("r+b") as competitor:
                with self.assertRaises(BlockingIOError):
                    fcntl.flock(competitor, fcntl.LOCK_EX | fcntl.LOCK_NB)
            self.assertEqual(read_result(disks["system"], RUN_ID, time.monotonic() + 5), 4)
        with (work / ".lock").open("r+b") as competitor:
            fcntl.flock(competitor, fcntl.LOCK_EX | fcntl.LOCK_NB)

    def test_output_does_not_renew_deadline_and_timeout_reaps_child(self):
        """日志不续期，超时终止整个组；父子真实 wait 后才返回并保留完整日志。"""
        uart = self.base / "flood.uart"
        child = (
            "import os,signal,subprocess,sys,time\n"
            "sleeper=subprocess.Popen([sys.executable,'-c','import time; time.sleep(30)'])\n"
            "def stop(*args):\n"
            "    sleeper.wait(timeout=2)\n"
            "    os.write(1,b'STOPPED\\n')\n"
            "    sys.exit(0)\n"
            "signal.signal(signal.SIGTERM,stop)\n"
            "print(os.getpid(),flush=True)\n"
            "while True:\n"
            "    os.write(1,b'progress\\n')\n"
            "    time.sleep(0.01)\n")
        with self.assertRaises(subprocess.TimeoutExpired):
            wait_for_qemu([sys.executable, "-c", child], os.environ.copy(), uart,
                          time.monotonic() + 0.5)
        output = uart.read_bytes()
        pid = int(output.splitlines()[0])
        self.assertIn(b"progress\n", output)
        self.assertTrue(output.endswith(b"STOPPED\n"))
        with self.assertRaises(ChildProcessError):
            os.waitpid(pid, os.WNOHANG)

    def test_creation_cancellation_is_delivered_only_after_child_is_owned_and_reaped(self):
        """真实 Popen 已创建子进程但包装尚未返回时取消，先停止并回收再向调用者递交。"""
        ready = self.base / "created-child-ready"
        mask = sorted(int(number) for number in signal.pthread_sigmask(signal.SIG_BLOCK, []))
        handlers = {number: signal.getsignal(number) for number in HOST_CANCELLATION_SIGNALS}
        child = (
            "import signal,time\n"
            "from pathlib import Path\n"
            "blocked=sorted(int(n) for n in signal.pthread_sigmask(signal.SIG_BLOCK, []))\n"
            f"assert blocked=={mask!r}\n"
            f"Path({str(ready)!r}).touch()\n"
            "time.sleep(30)\n")
        original_popen = subprocess.Popen
        created = []
        creation_returned = []

        def interrupt_after_creation(*args, **kwargs):
            """包装真实构造，仅在活子进程确认正常继承信号掩码后递交 Host SIGTERM。"""
            process = original_popen(*args, **kwargs)
            created.append(process)
            self.wait_for_path(ready, process)
            self.assertIsNone(process.poll())
            os.kill(os.getpid(), signal.SIGTERM)
            creation_returned.append(True)
            return process

        deadline = time.monotonic() + 10
        try:
            with (self.base / "creation.uart").open("xb") as output:
                with self.assertRaisesRegex(KeyboardInterrupt, "SIGTERM"), host_deadline(deadline):
                    with patch("run_user_selftests.subprocess.Popen", new=interrupt_after_creation):
                        run_host_process([sys.executable, "-c", child],
                                         os.environ.copy(), output, deadline)
            self.assertEqual(creation_returned, [True])
            self.assertEqual(len(created), 1)
            self.assertEqual(created[0].returncode, -signal.SIGTERM)
            with self.assertRaises(ChildProcessError):
                os.waitpid(created[0].pid, os.WNOHANG)
            self.assertEqual(
                sorted(int(number) for number in signal.pthread_sigmask(signal.SIG_BLOCK, [])), mask)
            self.assertEqual(
                {number: signal.getsignal(number) for number in HOST_CANCELLATION_SIGNALS}, handlers)
            self.assertEqual(signal.getitimer(signal.ITIMER_REAL), (0.0, 0.0))
        finally:
            for process in created:
                stop_and_wait(process)

    def test_deadline_scope_restores_handlers_and_disarms_timer(self):
        """整轮计时在同步阻塞中触发，正常结束和取消后均恢复调用者的信号处理。"""
        handlers = {number: signal.getsignal(number) for number in HOST_CANCELLATION_SIGNALS}
        with host_deadline(time.monotonic() + 5):
            pass
        with self.assertRaises(TimeoutError):
            with host_deadline(time.monotonic() + 0.05):
                time.sleep(5)
        for number in (signal.SIGINT, signal.SIGTERM):
            with self.subTest(signal=number), self.assertRaises(KeyboardInterrupt):
                with host_deadline(time.monotonic() + 5):
                    os.kill(os.getpid(), number)
        self.assertEqual(signal.getitimer(signal.ITIMER_REAL), (0.0, 0.0))
        self.assertEqual(
            {number: signal.getsignal(number) for number in HOST_CANCELLATION_SIGNALS}, handlers)

    @unittest.skipUnless(HAS_E2FSPROGS, "requires e2fsprogs")
    def test_cancellation_reaps_live_process_before_releasing_work_lock(self):
        """SIGINT、SIGTERM 及清理期重复取消均先回收真实宿主进程，不模拟 Guest 通过。"""
        self.make_volume()
        kernel = self.base / "kernel.elf"
        kernel.write_bytes(kernel_image("riscv64")[0])
        for number in (signal.SIGINT, signal.SIGTERM):
            with self.subTest(signal=number):
                work = self.base / f"work-{number}"
                ready = self.base / f"ready-{number}"
                stopping = self.base / f"stopping-{number}"
                held = self.base / f"held-{number}"
                arguments = [
                    "--arch", "riscv64", "--kernel-elf", str(kernel),
                    "--system-image", str(self.image), "--working-disk-dir", str(work),
                    "--system-only", "--init-program", "/sbin/init",
                    "--qemu-binary", sys.executable]
                child = (
                    "import fcntl,os,signal,sys,time\n"
                    "from pathlib import Path\n"
                    "def stop(*args):\n"
                    "    \"\"\"延迟正常退出期间检查 Host 仍持有工作盘锁。\"\"\"\n"
                    f"    Path({str(stopping)!r}).touch()\n"
                    "    time.sleep(0.3)\n"
                    f"    with open({str(work / '.lock')!r},'r+b') as lock:\n"
                    "        try:\n"
                    "            fcntl.flock(lock,fcntl.LOCK_EX|fcntl.LOCK_NB)\n"
                    "        except BlockingIOError:\n"
                    f"            Path({str(held)!r}).touch()\n"
                    "        else:\n"
                    "            sys.exit(1)\n"
                    "    sys.exit(0)\n"
                    "signal.signal(signal.SIGTERM,stop)\n"
                    f"ready=Path({str(ready)!r})\n"
                    "pending=ready.with_suffix('.pending')\n"
                    "pending.write_text(str(os.getpid()))\n"
                    "pending.replace(ready)\n"
                    "while True:\n"
                    "    time.sleep(1)\n")
                bootstrap = (
                    "import os,signal,sys,time\n"
                    "from pathlib import Path\n"
                    f"sys.path.insert(0,{str(TOOLS)!r})\n"
                    "from run_qemu import configure_qemu,parse_arguments,prepare_qemu\n"
                    "from run_user_selftests import HOST_CANCELLATION_SIGNALS,host_deadline,wait_for_qemu\n"
                    f"_,args=parse_arguments({arguments!r})\n"
                    "handlers={n:signal.getsignal(n) for n in HOST_CANCELLATION_SIGNALS}\n"
                    "deadline=time.monotonic()+5\n"
                    "try:\n"
                    "    with host_deadline(deadline):\n"
                    "        with prepare_qemu(configure_qemu(args)) as (_,env,disks,lock):\n"
                    f"            wait_for_qemu([sys.executable,'-c',{child!r}],env,\n"
                    f"                          {str(work / 'child.uart')!r},deadline)\n"
                    "except KeyboardInterrupt:\n"
                    f"    pid=int(Path({str(ready)!r}).read_text())\n"
                    "    try:\n"
                    "        os.waitpid(pid,os.WNOHANG)\n"
                    "    except ChildProcessError:\n"
                    "        pass\n"
                    "    else:\n"
                    "        raise AssertionError('child was not reaped')\n"
                    "    assert signal.getitimer(signal.ITIMER_REAL)==(0.0,0.0)\n"
                    "    assert {n:signal.getsignal(n) for n in HOST_CANCELLATION_SIGNALS}==handlers\n"
                    "    sys.exit(2)\n"
                    "sys.exit(1)\n")
                host = subprocess.Popen([sys.executable, "-c", bootstrap], start_new_session=True,
                                        stdout=subprocess.PIPE, stderr=subprocess.PIPE)
                pid = None
                try:
                    self.wait_for_path(ready, host)
                    pid = int(ready.read_text())
                    with (work / ".lock").open("r+b") as competitor:
                        with self.assertRaises(BlockingIOError):
                            fcntl.flock(competitor, fcntl.LOCK_EX | fcntl.LOCK_NB)
                    host.send_signal(number)
                    self.wait_for_path(stopping, host)
                    host.send_signal(signal.SIGINT)
                    host.send_signal(signal.SIGTERM)
                    _, error = host.communicate(timeout=2)
                    self.assertEqual(host.returncode, 2, error.decode(errors="replace"))
                    self.assertTrue(held.exists(), "work lock was released while child was alive")
                    with self.assertRaises(ProcessLookupError):
                        os.kill(pid, 0)
                    with (work / ".lock").open("r+b") as competitor:
                        fcntl.flock(competitor, fcntl.LOCK_EX | fcntl.LOCK_NB)
                finally:
                    if host.poll() is None:
                        host.kill()
                    host.communicate()
                    if pid is not None:
                        try:
                            os.killpg(pid, signal.SIGKILL)
                        except ProcessLookupError:
                            pass

    @unittest.skipUnless(test_user_selftest_rootfs.HAS_E2FSPROGS, "requires e2fsprogs")
    def test_main_deadline_interrupts_real_copy_without_publishing_work_volume(self):
        """真实准备器后慢速复制仍受整轮期限控制，取消前的旧工作卷保留且锁已释放。"""
        fixture = test_user_selftest_rootfs.UserSelftestRootfsTests()
        fixture.setUp()
        self.addCleanup(fixture.doCleanups)
        self.make_volume()
        self.replace_result(self.image, f"run={RUN_ID} status=4\n".encode())
        work = self.base / "work"
        work.mkdir()
        disk = work / "root.raw"
        shutil.copyfile(self.image, disk)
        previous = disk.read_bytes()
        kernel = self.base / "kernel.elf"
        kernel.write_bytes(kernel_image("riscv64")[0])
        marker = self.base / "copy-started"
        bootstrap = (
            "import shutil,sys,time\n"
            "from pathlib import Path\n"
            f"sys.path.insert(0,{str(TOOLS)!r})\n"
            "import run_user_selftests\n"
            "original=shutil.copyfileobj\n"
            "def slow_copy(source,target,length):\n"
            "    \"\"\"由原复制实现读取真实文件，只在每次读取后加入受控延迟。\"\"\"\n"
            "    class SlowSource:\n"
            "        def read(self,size):\n"
            "            \"\"\"保留真实字节和 EOF，延迟发生在正式工作盘发布之前。\"\"\"\n"
            "            data=source.read(size)\n"
            "            if data:\n"
            f"                Path({str(marker)!r}).touch()\n"
            "                time.sleep(0.1)\n"
            "            return data\n"
            "    return original(SlowSource(),target,length)\n"
            "shutil.copyfileobj=slow_copy\n"
            "sys.exit(run_user_selftests.main(sys.argv[1:]))\n")
        budget = 3
        command = [
            sys.executable, "-c", bootstrap, "--arch", "riscv64",
            "--guest-userland-directory", str(fixture.userland),
            "--runtime-libraries-directory", str(fixture.runtime),
            "--output-directory", str(fixture.output), "--system-image", str(fixture.image),
            "--size-mib", "128", "--kernel-elf", str(kernel),
            "--working-disk-dir", str(work), "--qemu-binary", sys.executable,
            "--host-timeout-seconds", str(budget)]
        started = time.monotonic()
        host = subprocess.Popen(command, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        try:
            _, error = host.communicate(timeout=budget + 2)
            self.assertLess(time.monotonic() - started, budget + 1)
            self.assertEqual(host.returncode, 2, error.decode(errors="replace"))
            self.assertIn(b"host selftest deadline expired", error)
            self.assertTrue(marker.exists(), "deadline did not reach the real working-volume copy")
            self.assertEqual(disk.read_bytes(), previous)
            self.assertEqual(sorted(path.name for path in work.iterdir()), [".lock", "root.raw"])
            with (work / ".lock").open("r+b") as competitor:
                fcntl.flock(competitor, fcntl.LOCK_EX | fcntl.LOCK_NB)
        finally:
            if host.poll() is None:
                host.kill()
            host.communicate()

    def test_cli_rejects_unsafe_outputs_reuse_and_invalid_deadlines_before_preparation(self):
        """输出重叠、自动 reuse 和非法期限在缺失安装输入之前失败，不生成输出。"""
        common = [sys.executable, str(TOOLS / "run_user_selftests.py"), "--arch", "riscv64",
                  "--guest-userland-directory", str(self.base / "missing-userland"),
                  "--runtime-libraries-directory", str(self.base / "missing-runtime"),
                  "--output-directory", str(self.base / "output"),
                  "--system-image", str(self.image), "--working-disk-dir", str(self.base / "work")]
        invalid = [["--working-disk-action", "reuse"]]
        invalid += [["--host-timeout-seconds", value] for value in ("0", "-1", "nan", "inf", "bad")]
        invalid += [["--output-directory", str(path)] for path in
                    (self.base, self.base / "work", self.base / "work/nested")]
        invalid += [["--system-image", str(path)] for path in
                    (self.base / "work", self.base / "work/root.raw")]
        invalid += [["--system-image", str(self.base / "work.img"),
                     "--working-disk-dir", str(self.base / "work.log")]]
        for arguments in invalid:
            with self.subTest(arguments=arguments):
                result = subprocess.run([*common, *arguments], capture_output=True, timeout=5)
                self.assertEqual(result.returncode, 2)
                self.assertEqual(list(self.base.iterdir()), [])


if __name__ == "__main__":
    unittest.main()
