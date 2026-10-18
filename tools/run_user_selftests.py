#!/usr/bin/env python3
"""准备本轮自测；可捕获取消和整次 POSIX 期限均先回收子进程，再释放工作盘锁。"""

import sys

sys.dont_write_bytecode = True

import argparse
from contextlib import contextmanager
import math
import os
from pathlib import Path
import re
import secrets
import signal
import subprocess
import time

from create_ext4_image import PROFILES
from rootfs_paths import output_path, reject_overlap
from run_qemu import configure_qemu, parse_arguments, prepare_qemu, repository_path

DEFAULT_HOST_TIMEOUT_SECONDS = 900
HOST_CANCELLATION_SIGNALS = (signal.SIGINT, signal.SIGTERM, signal.SIGALRM)


def positive_seconds(value):
    """Host 总期限接受正有限秒数，不与 Guest 每程序预算共用默认值。"""
    try:
        seconds = float(value)
    except ValueError:
        raise argparse.ArgumentTypeError("host timeout must be a positive finite number") from None
    if not math.isfinite(seconds) or seconds <= 0:
        raise argparse.ArgumentTypeError("host timeout must be a positive finite number")
    return seconds


def remaining_seconds(deadline):
    """所有阻塞等待使用同一单调绝对期限，UART 输出不会延长它。"""
    remaining = deadline - time.monotonic()
    if remaining <= 0:
        raise TimeoutError("host selftest deadline expired")
    return remaining


@contextmanager
def host_deadline(deadline):
    """仅自动入口安装一次实时时钟期限；首次取消关闭计时并让正常 finally 完成清理。"""
    handlers = {number: signal.getsignal(number) for number in HOST_CANCELLATION_SIGNALS}

    def cancel(number, _):
        """首次信号取消整轮执行，后续信号不再打断停止、等待和释放锁。"""
        signal.setitimer(signal.ITIMER_REAL, 0)
        for pending in HOST_CANCELLATION_SIGNALS:
            signal.signal(pending, signal.SIG_IGN)
        if number == signal.SIGALRM:
            raise TimeoutError("host selftest deadline expired")
        raise KeyboardInterrupt(f"host selftest cancelled by {signal.Signals(number).name}")

    try:
        for number in HOST_CANCELLATION_SIGNALS:
            signal.signal(number, cancel)
        signal.setitimer(signal.ITIMER_REAL, remaining_seconds(deadline))
        yield
    finally:
        signal.setitimer(signal.ITIMER_REAL, 0)
        for number, handler in handlers.items():
            signal.signal(number, handler)


def stop_and_wait(process):
    """终止独立进程组并真实回收；等待自身先超时时，待处理的取消也须等清理结束。"""
    mask = signal.pthread_sigmask(signal.SIG_BLOCK, HOST_CANCELLATION_SIGNALS)
    try:
        if process.poll() is None:
            try:
                os.killpg(process.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                pass
            finally:
                try:
                    os.killpg(process.pid, signal.SIGKILL)
                except ProcessLookupError:
                    pass
        process.wait()
    finally:
        signal.pthread_sigmask(signal.SIG_SETMASK, mask)


@contextmanager
def _defer_host_cancellation():
    """创建交接期间只记首次取消；恢复 handler 后递交，让已登记的 finally 负责回收。"""
    handlers = {number: signal.getsignal(number) for number in HOST_CANCELLATION_SIGNALS}
    pending = None

    def record(number, _):
        """保存首个取消信号，不改变将由子进程继承的信号掩码。"""
        nonlocal pending
        if pending is None:
            pending = number

    try:
        for number in HOST_CANCELLATION_SIGNALS:
            signal.signal(number, record)
        yield
    finally:
        for number, handler in handlers.items():
            signal.signal(number, handler)
        if pending is not None:
            signal.raise_signal(pending)


def run_host_process(command, environment, output, deadline):
    """创建和清理责任登记期间暂缓取消，之后用同一绝对期限等待并真实回收。"""
    remaining_seconds(deadline)
    process = None
    try:
        with _defer_host_cancellation():
            process = subprocess.Popen(
                command, env=environment, stdin=subprocess.DEVNULL,
                stdout=output, stderr=subprocess.STDOUT, start_new_session=True)
        return process.wait(timeout=remaining_seconds(deadline))
    finally:
        if process is not None:
            stop_and_wait(process)


def wait_for_qemu(command, environment, uart_path, deadline):
    """保存完整合并 UART，不把持续输出当成成功或延长期限的依据。"""
    with Path(uart_path).open("xb") as uart:
        return run_host_process(command, environment, uart, deadline)


def read_result(system_working_disk, run_id, deadline):
    """保留 debugfs 读取失败的原始诊断，分别拒绝畸形记录和其他轮次的完整结果。"""
    result = subprocess.run(
        ["debugfs", "-R", "cat /test/selftests/exit-status", str(system_working_disk)],
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, check=False,
        timeout=remaining_seconds(deadline))
    if result.returncode != 0 or not result.stdout:
        diagnostic = result.stderr.decode("utf-8", errors="replace").strip()
        raise ValueError(
            f"selftest result read failed in {system_working_disk} "
            f"(debugfs status={result.returncode}): {diagnostic}")
    match = re.fullmatch(rb"run=([0-9a-f]{32}) status=([0124])\n", result.stdout)
    if match is None:
        raise ValueError(f"malformed selftest result in {system_working_disk}")
    if match[1].decode("ascii") != run_id:
        raise ValueError(f"stale selftest result in {system_working_disk}")
    remaining_seconds(deadline)
    return int(match[2])


def run_selftest_qemu(configuration, run_id, uart_path, deadline):
    """沿唯一生产准备上下文持锁到 QEMU 回收和实际系统工作盘结果读取完成。"""
    with prepare_qemu(configuration) as (command, environment, disk_paths, _):
        print(f"user-selftests run={run_id} uart={uart_path} "
              f"working-disk={disk_paths['system']}", file=sys.stderr, flush=True)
        status = wait_for_qemu(command, environment, uart_path, deadline)
        if status != 0:
            raise ValueError(f"QEMU exited with status {status}")
        return read_result(disk_paths["system"], run_id, deadline)


def prepare_run_input(args, configuration, run_id, deadline):
    """只调用唯一准备器；所有来源、输出、选择和显式程序期限都作为明确 argv 传入。"""
    command = [
        sys.executable, str(Path(__file__).with_name("prepare_user_selftest_rootfs.py")),
        "--arch", args.arch,
        "--guest-userland-directory", str(args.guest_userland_directory),
        "--runtime-libraries-directory", str(args.runtime_libraries_directory),
        "--output-directory", str(args.output_directory),
        "--output-image", str(configuration.system_image_path),
        "--profile", args.profile, "--size-mib", str(args.size_mib), "--run-id", run_id]
    for option, values in (("--collection", args.collection), ("--program", args.program)):
        for value in values:
            command.extend((option, value))
    if args.program_timeout_seconds is not None:
        command.extend(("--program-timeout-seconds", str(args.program_timeout_seconds)))
    status = run_host_process(command, None, sys.stderr, deadline)
    if status != 0:
        raise ValueError(f"selftest preparation exited with status {status}")


def check_preparation_outputs(args, configuration):
    """准备输出按实际父目录核对，不得替换工作目录、锁或其父树。"""
    directory = output_path(configuration.working_disk_directory, directory=True)
    image = configuration.system_image_path
    for output in (
            output_path(args.output_directory, directory=True),
            output_path(image),
            output_path(image.with_suffix(".log"))):
        reject_overlap(directory, output)


def main(argv=None):
    """每次明确准备新输入；Host 错误统一返回2，正常 QEMU 才传播 Guest 状态。"""
    parser = argparse.ArgumentParser(description=__doc__, allow_abbrev=False)
    parser.add_argument("--guest-userland-directory", type=repository_path, required=True)
    parser.add_argument("--runtime-libraries-directory", type=repository_path, required=True)
    parser.add_argument("--output-directory", type=repository_path, required=True)
    parser.add_argument("--collection", action="append", default=[])
    parser.add_argument("--program", action="append", default=[])
    parser.add_argument("--program-timeout-seconds", type=int)
    parser.add_argument("--host-timeout-seconds", type=positive_seconds,
                        default=DEFAULT_HOST_TIMEOUT_SECONDS,
                        help="absolute Host deadline in seconds (default: %(default)s)")
    parser.add_argument("--profile", choices=PROFILES, default="seed4k")
    parser.add_argument("--size-mib", type=int, default=256)
    parser.set_defaults(system_only=True, init_program="/sbin/init", root_mode="rw")
    _, args = parse_arguments(argv, parser)
    if args.system_image is None:
        parser.error("--system-image is required for the prepared selftest image")
    if args.shell or args.init_program != "/sbin/init" or args.root_mode != "rw":
        parser.error("automatic selftests require ordinary /sbin/init and a writable root")
    if args.print_command:
        parser.error("--print-command belongs to the manual run-qemu entry")
    if args.working_disk_action == "reuse":
        parser.error("automatic selftests require fresh or refresh-system to consume this run's input")
    configuration = configure_qemu(args)
    try:
        check_preparation_outputs(args, configuration)
    except (OSError, ValueError) as error:
        parser.error(str(error))
    run_id = secrets.token_hex(16)
    deadline = time.monotonic() + args.host_timeout_seconds
    uart_path = configuration.working_disk_directory / f"selftests-{run_id}.uart.log"
    print(f"user-selftests run={run_id} host-timeout-seconds={args.host_timeout_seconds} "
          f"uart={uart_path}", file=sys.stderr, flush=True)
    try:
        with host_deadline(deadline):
            prepare_run_input(args, configuration, run_id, deadline)
            remaining_seconds(deadline)
            status = run_selftest_qemu(configuration, run_id, uart_path, deadline)
    except (OSError, ValueError, subprocess.SubprocessError, TimeoutError, KeyboardInterrupt) as error:
        print(f"user-selftests: {error}", file=sys.stderr, flush=True)
        return 2
    print(f"user-selftests run={run_id} status={status} uart={uart_path}",
          file=sys.stderr, flush=True)
    return status


if __name__ == "__main__":
    sys.exit(main())
