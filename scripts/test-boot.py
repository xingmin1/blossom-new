#!/usr/bin/env python3
"""驱动已构建内核，保留 QEMU 原始输出并逐项核对串口协议。"""

import argparse
import os
from pathlib import Path
import re
import selectors
import subprocess
import sys
import time
from urllib.parse import quote


def run(args):
    """运行一台指定目标，所有正常验收标记齐全才接受关机结果。"""
    command = [f"qemu-system-{args.arch}", "-machine", "virt", "-accel", "tcg",
               "-kernel", str(args.kernel), "-m", args.memory, "-smp", str(args.cpus),
               "-display", "none", "-monitor", "none", "-serial", "stdio",
               "-no-reboot", "-nic", "none"]
    if args.arch == "riscv64":
        command += ["-bios", "default"]
    required = {"TEST READY arch=%s cpus=%s" % (args.arch, args.cpus),
                "PASS boot-input", "PASS uart normal", "PASS uart mask-unmask",
                "PASS uart lifecycle mode=" + ("local" if args.cpus == 1 else "concurrent")}
    for cpu in range(args.cpus):
        required.update({f"cpu-local:{cpu}", f"registers:{cpu}", f"ipi:{cpu}",
                         f"PASS irq-save cpu={cpu}", f"PASS self-call cpu={cpu}"})
    if args.cpus > 1:
        required.update({"PASS smp-call pair", f"PASS smp-call fan-in senders={args.cpus - 1}",
                         "PASS smp-call delayed"})
        required.add("PASS smp-call processing-arrival" if args.cpus == 4
                     else "SCOPE smp-call processing-arrival requires=4")
    else:
        required.update({"PASS smp-call delayed-local", "SCOPE smp-call local-only"})
    prompts = [("UART READY normal", b"U"), ("UART READY masked", b"M"),
               ("UART READY lifecycle", b"L"), ("UART READY unregistered", b"R")]
    seen = set()
    identities = {}
    prompt_index = 0
    passed = False
    fatal = False
    fault_ready = False
    pending = bytearray()
    number = r"(?:0x[0-9a-fA-F]+|[0-9]+)"

    def observe(line):
        """核对真实异常字段、每核身份和有序 UART 协议，不接受缺失或重复证据。"""
        nonlocal passed, fatal, fault_ready, prompt_index
        text = line.decode("utf-8", errors="replace").rstrip("\r")
        if "FAIL" in text or "panicked" in text or re.search(r"\b[Pp][Aa][Nn][Ii][Cc]\b", text):
            raise RuntimeError("guest reported failure: " + text)
        if "FATAL" in text:
            if not args.fault:
                raise RuntimeError("unexpected guest fatal: " + text)
            match = re.fullmatch(rf"FATAL pc=({number}) sp=({number}) cause=({number}) bad=({number})", text)
            if not fault_ready or match is None:
                raise RuntimeError("fault did not report the expected real exception fields")
            if any(int(value, 16 if value.startswith("0x") else 10) == 0
                   for value in match.groups()[:3]):
                raise RuntimeError("fault PC/SP/cause must be nonzero")
            cause = int(match.group(3), 0)
            bad = int(match.group(4), 0)
            if args.fault == "refill":
                if cause != 1 or bad != 0x1000:
                    raise RuntimeError("refill fault must report TLBRERA.IsTLBR and TLBRBADV=0x1000")
            elif ((args.arch == "riscv64" and cause != 2)
                  or (args.arch == "loongarch64" and (cause >> 16) & 0x3F != 13)):
                raise RuntimeError("illegal-instruction fault reported a different hardware cause")
            fatal = True
            return
        if args.fault:
            if text == f"FAULT READY arch={args.arch} cpus={args.cpus}":
                fault_ready = True
            elif text == "STAGE1 PASS":
                raise RuntimeError("fault image ran normal success path")
            return
        if text.startswith("UART READY"):
            if prompt_index >= len(prompts) or text != prompts[prompt_index][0]:
                raise RuntimeError("unexpected UART protocol phase: " + text)
            process.stdin.write(prompts[prompt_index][1])
            process.stdin.flush()
            prompt_index += 1
        key = text
        match = re.fullmatch(rf"PASS cpu-local cpu=([0-9]+) hw=([0-9]+) stack=({number}) local=({number})", text)
        if match:
            cpu, hw, stack, local = (int(value, 16 if value.startswith("0x") else 10)
                                     for value in match.groups())
            if cpu not in range(args.cpus) or stack == 0 or local == 0:
                raise RuntimeError("invalid CPU identity record")
            if any(hw == old[0] or stack == old[1] or local == old[2]
                   for old in identities.values()):
                raise RuntimeError("duplicate hardware CPU, stack, or CpuLocal")
            identities[cpu] = (hw, stack, local)
            key = f"cpu-local:{cpu}"
        match = re.fullmatch(r"PASS (registers|ipi) cpu=([0-9]+) (timer|count)=([0-9]+)", text)
        if match:
            kind, cpu, counter, value = match.groups()
            if int(cpu) not in range(args.cpus) or int(value) <= 0 or counter != {"registers": "timer", "ipi": "count"}[kind]:
                raise RuntimeError("missing real per-CPU interrupt evidence")
            key = f"{kind}:{cpu}"
        if key in required:
            if key in seen:
                raise RuntimeError("duplicate test record: " + key)
            seen.add(key)
        if text == "STAGE1 PASS":
            missing = required - seen
            if missing or prompt_index != len(prompts):
                raise RuntimeError("incomplete stage-one evidence: " + ", ".join(sorted(missing)))
            if passed:
                raise RuntimeError("duplicate final success marker")
            passed = True

    # 合并 stderr 到读取管道，避免某条未读取的错误管道填满后锁死 QEMU。
    process = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                               stderr=subprocess.STDOUT, bufsize=0)
    selector = selectors.DefaultSelector()
    selector.register(process.stdout, selectors.EVENT_READ)
    deadline = time.monotonic() + args.timeout
    try:
        with args.output.open("wb") as output:
            eof = False
            while not eof:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise TimeoutError(f"QEMU timeout after {args.timeout:g}s")
                for key, _ in selector.select(min(remaining, 0.1)):
                    data = os.read(key.fd, 65536)
                    if not data:
                        eof = True
                        break
                    output.write(data)
                    output.flush()
                    sys.stdout.buffer.write(data)
                    sys.stdout.buffer.flush()
                    pending.extend(data)
                    while b"\n" in pending:
                        line, _, rest = pending.partition(b"\n")
                        pending[:] = rest
                        observe(line)
                if args.fault and fatal:
                    # 致命异常本身不是成功退出；观察到完整真实现场后结束整个客体。
                    break
            if pending and not fatal:
                observe(bytes(pending))
            if args.fault:
                if not fatal:
                    raise RuntimeError("QEMU ended without the expected fatal exception")
            else:
                if not passed:
                    raise RuntimeError("QEMU ended without complete stage-one evidence")
                code = process.wait(timeout=max(0.001, deadline - time.monotonic()))
                if code != 0:
                    raise RuntimeError(f"QEMU exited with status {code}")
    finally:
        selector.close()
        if process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=2)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
        process.stdin.close()
        process.stdout.close()


def main():
    """解析唯一运行目标并拒绝日志覆盖输入 ELF，不构建或自动修复宿主环境。"""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--arch", required=True, choices=("riscv64", "loongarch64"))
    parser.add_argument("--cpus", required=True, type=int, choices=(1, 2, 4))
    parser.add_argument("--memory", default="1G", help="QEMU memory specification (default: 1G)")
    parser.add_argument("--kernel", required=True, type=Path)
    parser.add_argument("--fault", choices=("illegal", "refill"), help="expected independently built hardware fault")
    parser.add_argument("--timeout", type=float, default=30, help="whole-machine timeout in seconds")
    parser.add_argument("--output", type=Path, help="raw guest/QEMU output (default: beside ELF)")
    args = parser.parse_args()
    if args.fault == "refill" and args.arch != "loongarch64":
        parser.error("refill fault requires --arch loongarch64")
    if not args.kernel.is_file():
        parser.error("--kernel must name an already built ELF")
    if not 0 < args.timeout < float("inf"):
        parser.error("--timeout must be finite and positive")
    if args.output is None:
        memory_name = quote(args.memory, safe="")
        args.output = args.kernel.with_name(f"{args.kernel.name}.{args.arch}.{args.cpus}.{memory_name}.{args.fault or 'boot'}.log")
    if (args.output.resolve() == args.kernel.resolve()
            or (args.output.exists() and args.output.samefile(args.kernel))):
        parser.error("--output cannot overwrite --kernel")
    try:
        run(args)
    except (OSError, RuntimeError, TimeoutError, subprocess.TimeoutExpired) as error:
        print(f"\nHOST FAIL: {error}; memory={args.memory}; raw output: {args.output}", file=sys.stderr)
        return 1
    print(f"HOST PASS: {args.arch} cpus={args.cpus} memory={args.memory} {args.fault or 'boot'}; raw output: {args.output}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
