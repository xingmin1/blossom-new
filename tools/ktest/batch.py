"""固定目录与批次执行：完整 KTAP、绝对期限和真实退出共同决定结果。"""

from concurrent.futures import ThreadPoolExecutor, as_completed
import json
import subprocess
import time

from . import boot
from .catalog import Catalog, HostError, require, run_command
from .protocol import CaseResult, KtapParser
from .qemu_session import CANCELLED, QemuTestSession
from .uart_channel import DeadlineExpired, VMExited
from .resources import resource_names, validate_resources


def config(args):
    """发现与运行共享同一个实际配置。"""
    return dict(arch=args.arch, cpus=args.cpus, mode=args.mode, panic=args.panic)


def discover(args, kernel, directory):
    """从实际 QEMU UART 读取注册 JSONL，目录不执行 fixture。"""
    catalog = Catalog(config(args))
    vm = QemuTestSession(args, kernel, directory, set())
    deadline = time.monotonic() + args.startup_timeout
    try:
        vm.handshake(config(args), b"catalog\n", deadline)
        while not catalog.done:
            line = vm.uart.line(deadline)
            if line.startswith("KTEST READY"):
                raise HostError("duplicate READY")
            if line.startswith("{"):
                catalog.feed(json.loads(line))
            else:
                require(not line.strip() or line.lstrip().startswith("#"), "unexpected catalog output")
        finish_vm(vm, time.monotonic() + args.cleanup_timeout, catalog=True)
        return catalog
    finally:
        vm.close()


def looks_control(value):
    """EOF 的半条框架记录必须拒绝，普通未换行业务日志仍保留。"""
    value = value.lstrip()
    if not value:
        return False
    markers = ("KTAP version", "KTEST READY", "# KTEST", "# Subtest:", "ok ", "not ok ", "1..")
    if any(value.startswith(marker) or marker.startswith(value) for marker in markers):
        return True
    if value.startswith("{"):
        kinds = ("catalog", "suite", "case", "catalog_end")
        try:
            row = json.loads(value)
            return isinstance(row, dict) and row.get("type") in kinds
        except json.JSONDecodeError:
            compact = "".join(value.split())
            prefixes = [f'{{"type":"{kind}' for kind in kinds]
            return any(compact.startswith(prefix) or prefix.startswith(compact) for prefix in prefixes)
    return False


def finish_vm(vm, deadline, parser=None, catalog=False):
    """final 后继续解析尾文，并在原绝对期限内确认 QEMU 自行退出成功。"""
    while True:
        try:
            line = vm.uart.line(deadline)
        except VMExited:
            fragment = bytes(vm.uart.buffer).decode("utf-8", errors="replace").rstrip("\r")
            require(not looks_control(fragment), "partial control record at EOF")
            if parser is not None and parser.case_abort is not None:
                require(not fragment.strip(), "residual UART bytes after case-abort")
            while vm.process.poll() is None:
                vm.check(deadline)
                time.sleep(min(0.01, max(0, deadline - time.monotonic())))
            vm.check(deadline)
            require(vm.process.returncode == 0, f"QEMU failed after final: {vm.process.returncode}")
            vm.verify_resources(deadline)
            return
        if catalog:
            require(not looks_control(line), "control record after catalog_end")
        else:
            require(not (line.lstrip().startswith("{") and looks_control(line)),
                    "catalog control record after run complete")
            parser.feed(line)




def result_row(item, status, reason, directory, case_abort=None):
    """报告记录真实身份；中止账本也进入同次 JUnit 的错误原因。"""
    case = item.case
    row = dict(name=case["name"], suite=case["suite"], case=case["case"], parameter=case["parameter"],
               source=case["source"], line=case["line"], repeat=item.repeat, seed=item.seed,
               order=item.order, status=status, reason=reason, log=str(directory / "uart.log"))
    if case_abort is not None:
        row["case_abort"] = dict(case_abort)
        row["reason"] += "\ncase-abort: " + json.dumps(case_abort, ensure_ascii=False, sort_keys=True)
    return row


def suite_error_row(suite, item, error, directory):
    """独立记录 suite 基础设施错误，不能冒充未开始的用例结果。"""
    return dict(name=f"{suite['name']} [SuiteError:{error['phase']}]", suite=suite["suite"],
                case=None, parameter=None, source=None, line=None, repeat=item.repeat,
                seed=item.seed, order=item.order, status="Error", kind="SuiteError",
                reason=f"{error['phase']}: {error['reason']}", suite_error=dict(error),
                log=str(directory / "uart.log"))


def finish_case(vm, event, case_deadline, deadline):
    """先完成两个外部观察，再按结果携带的真实 ID 发送 END。"""
    errors = []
    try:
        vm.uart.execution_uart.finish(event.status)
    except (HostError, OSError, ValueError) as error:
        errors.append("UART observation: " + str(error))
    if vm.net_user is not None:
        try:
            vm.net_user.finish(event.status, case_deadline)
        except (HostError, OSError, ValueError) as error:
            errors.append("network observation: " + str(error))
        vm.uart.send(f"KTEST PEER END {event.case_id}\n".encode("ascii"), deadline)
    vm.check(deadline)
    require(not errors, f"Guest {event.status}; " + "; ".join(errors))


def execute_batch(args, kernel, catalog, items, directory):
    """保持原 KTAP、fatal 和绝对期限，peer 消费边界完成后才回执放行本例或后继。"""
    output = []
    pending = list(items)
    attempt = 0
    while pending:
        if CANCELLED.is_set():
            output.extend(result_row(item, "NotRun", "host run cancelled before VM start", directory)
                          for item in pending)
            break
        current = directory / f"part-{attempt}"
        current.mkdir()
        attempt += 1
        suite = catalog.suites[pending[0].case["suite"]]
        parser = KtapParser(suite, pending)
        vm = None
        infrastructure = None
        external = None
        safe_suffix = False
        deadline = time.monotonic() + args.startup_timeout
        setup_deadline = case_deadline = cleanup_deadline = None
        try:
            names = resource_names(suite, pending)
            validate_resources(names, args)
            vm = QemuTestSession(args, kernel, current, names)
            vm.handshake(config(args), run_command(pending, args.seed), deadline)
            while not parser.complete:
                effective = min(d for d in (deadline, setup_deadline, case_deadline, cleanup_deadline) if d is not None)
                event = parser.feed(vm.uart.line(effective))
                now = time.monotonic()
                if event == "suite-setup":
                    setup_deadline = now + args.setup_timeout
                elif event == "case-abort":
                    cleanup_deadline = effective
                    setup_deadline = case_deadline = None
                    deadline = effective
                elif event == "suite-error":
                    setup_deadline = case_deadline = None
                    if cleanup_deadline is None:
                        cleanup_deadline = now + args.cleanup_timeout
                    deadline = cleanup_deadline
                elif event == "begin":
                    item = pending[len(parser.results)]
                    vm.uart.execution_uart.begin(item.case)
                    case_deadline = now + min(item.case["timeout_ms"] / 1000, args.case_timeout)
                    peer = vm.net_user
                    if peer is not None:
                        peer.begin(item.case, case_deadline)
                    setup_deadline = now + args.setup_timeout
                    cleanup_deadline = None
                    deadline = case_deadline
                    if peer is not None:
                        vm.uart.send(f"KTEST PEER BEGIN {item.case['case']}\n".encode("ascii"),
                                min(case_deadline, setup_deadline))
                elif event == "body":
                    setup_deadline = None
                    vm.uart.execution_uart.body = True
                elif event in ("quiesce", "join", "cleanup"):
                    setup_deadline = None
                    if cleanup_deadline is None:
                        cleanup_deadline = now + args.cleanup_timeout
                elif isinstance(event, dict):
                    vm.uart.raw_io(event, effective, names)
                elif isinstance(event, CaseResult):
                    try:
                        finish_case(vm, event, case_deadline, effective)
                    except (HostError, OSError, ValueError, DeadlineExpired, VMExited) as error:
                        result = parser.results[event.case_id]
                        result["status"] = "Error"
                        result["reason"] = f"Guest {event.status}; external completion failed: {error}"
                        raise
                    setup_deadline = case_deadline = None
                    if cleanup_deadline is None:
                        cleanup_deadline = now + args.cleanup_timeout
                    deadline = cleanup_deadline
            finish_vm(vm, cleanup_deadline or time.monotonic() + args.cleanup_timeout, parser)
            if parser.case_abort is not None:
                # 完整 outer/complete 与 QEMU 真实退出后才确认明确的中止 Error。
                parser.results[parser.aborted_case_id] = dict(
                    status="Error", reason=parser.case_abort["reason"], case_abort=parser.case_abort)
                parser.active = None
                safe_suffix = True
        except DeadlineExpired as error:
            external = ("Timeout", str(error))
        except VMExited as error:
            terminated = vm is not None and vm.process.poll() is not None
            if (parser.active is not None and parser.case_abort is None
                    and parser.fatal is not None and not terminated):
                try:
                    vm.process.wait(timeout=max(0, min(args.cleanup_timeout, effective - time.monotonic())))
                    terminated = True
                except subprocess.TimeoutExpired:
                    pass
            case = pending[len(parser.results)].case if parser.active is not None else None
            fragment = bytes(vm.uart.buffer).decode("utf-8", errors="replace").rstrip("\r")
            if looks_control(fragment):
                infrastructure = "partial control record at EOF"
            elif case is not None and effective <= time.monotonic():
                external = ("Timeout", "absolute host deadline expired")
            else:
                # fatal 的不可返回终点必须由 QEMU 真实自行退出提供；
                # close()、UART EOF 和宿主超时都不能证明后继尚未开始。
                fatal_matches = (case is not None and parser.phase == "body"
                                 and parser.fatal is not None
                                 and parser.armed == parser.fatal == case["expected_fatal"]
                                 and (case["fatal_message"] is None or parser.fatal_message_seen))
                safe_suffix = (case is not None and parser.case_abort is None
                               and parser.fatal is not None and terminated
                               and vm.process.returncode == 0
                               and not vm.uart.buffer.strip())
                if safe_suffix:
                    try:
                        vm.verify_resources(effective)
                        if fatal_matches:
                            boot.verify_trap(args.arch, case, vm.directory, parser.fatal)
                    except DeadlineExpired as resource_error:
                        external = ("Timeout", str(resource_error))
                        safe_suffix = False
                    except (HostError, OSError, ValueError, KeyError) as resource_error:
                        infrastructure = str(resource_error)
                        safe_suffix = False
                if safe_suffix and fatal_matches:
                    parser.results[parser.active] = dict(
                        status="Pass", reason=f"expected fatal: {parser.fatal}; QEMU={vm.process.returncode}")
                    parser.active = None
                else:
                    external = external or ("Crash", str(error))
        except (HostError, OSError, ValueError, UnicodeError) as error:
            infrastructure = str(error)
        finally:
            if vm is not None:
                vm.close()
        for item in pending:
            cid = item.case["case"]
            if cid in parser.results:
                result = parser.results[cid]
                peer = vm.net_user if vm is not None else None
                peer_error = peer.failures.get(cid) if peer is not None else None
                if result["status"] == "Pass" and peer_error:
                    result = dict(result, status="Error", reason=peer_error)
                output.append(result_row(item, result["status"], result["reason"], current,
                                         result.get("case_abort")))
            elif cid == parser.active:
                status, reason = ("Error", infrastructure) if infrastructure else external or ("Error", "missing final result")
                output.append(result_row(item, status, reason, current, parser.case_abort))
        unstarted = [item for item in pending if item.case["case"] not in parser.started]
        if parser.case_abort is not None:
            if safe_suffix and not CANCELLED.is_set():
                pending = unstarted
                continue
            for item in unstarted:
                output.append(result_row(
                    item, "NotRun", "Host did not confirm case start; incomplete abort protocol", current))
            if infrastructure or external:
                aborted = next(row for row in output if row["case"] == parser.aborted_case_id)
                aborted["infrastructure_error"] = infrastructure or (
                    "VM failed case-abort protocol/shutdown completion: " + external[1])
            break
        suite_error = parser.suite_error
        if (suite_error is None and parser.suite_started and not parser.started
                and (infrastructure or external)):
            suite_error = dict(phase="setup", reason=infrastructure or external[1])
        elif (suite_error is None and external and parser.active is None
              and len(parser.results) == len(pending)):
            suite_error = dict(phase="cleanup", reason=external[1])
        if suite_error is not None:
            for item in pending:
                if item.case["case"] not in parser.started:
                    output.append(result_row(item, "NotRun",
                                             f"suite {suite_error['phase']}: {suite_error['reason']}", current))
            row = suite_error_row(suite, pending[-1], suite_error, current)
            if infrastructure:
                row["infrastructure_error"] = infrastructure
            if external:
                row["infrastructure_error"] = "VM failed protocol/shutdown completion: " + external[1]
            output.append(row)
            break
        if external and parser.active is None and parser.results and output:
            output[-1]["infrastructure_error"] = "VM failed protocol/shutdown completion: " + external[1]
        if safe_suffix and not CANCELLED.is_set():
            pending = unstarted
            continue
        if infrastructure or (external and not parser.started):
            reason = infrastructure or external[1]
            # 无 begin 的崩溃无法证明执行命令已经生效，不自动重放整批。
            for item in unstarted:
                output.append(result_row(item, "NotRun",
                                         "Host did not confirm case start; incomplete protocol: " + reason,
                                         current))
            if not parser.active and not parser.results and output:
                output[-len(unstarted)]["infrastructure_error"] = reason
            elif infrastructure and output:
                output[-1]["infrastructure_error"] = infrastructure
            break
        if external:
            # begin/final 可能仍在串口或缓冲区；缺少记录不意味着尚未执行。
            for item in unstarted:
                output.append(result_row(item, "NotRun",
                                         "Host did not confirm case start; incomplete protocol: " + external[1],
                                         current))
            break
        if unstarted:
            for item in unstarted:
                output.append(result_row(item, "NotRun",
                                         "Host did not confirm case start; incomplete protocol", current))
        break
    return output


def execute(args, kernel, catalog, groups, directory):
    """并行只作用于 VM；每个 suite 内的顶层 case 顺序不变。"""
    completed = {}
    pool = ThreadPoolExecutor(max_workers=args.jobs)
    futures = {}
    interrupted = False
    try:
        for index, group in enumerate(groups):
            target = directory / f"vm-{index:05d}"
            target.mkdir()
            futures[pool.submit(execute_batch, args, kernel, catalog, group, target)] = index
        for future in as_completed(futures):
            completed[futures[future]] = future.result()
    except KeyboardInterrupt:
        interrupted = True
        CANCELLED.set()
        for future in futures:
            future.cancel()
    except BaseException:
        CANCELLED.set()
        for future in futures:
            future.cancel()
        raise
    finally:
        pool.shutdown(wait=True, cancel_futures=True)
    if interrupted:
        started_groups = set()
        for future, index in futures.items():
            if not future.cancelled():
                started_groups.add(index)
                if index not in completed:
                    completed[index] = future.result()
        for index, group in enumerate(groups):
            if index not in started_groups:
                target = directory / f"vm-{index:05d}"
                completed[index] = [result_row(item, "NotRun", "host interrupted before VM start", target)
                                    for item in group]
        results = [row for rows in completed.values() for row in rows]
        if results:
            results[0]["infrastructure_error"] = "host run interrupted"
    results = [row for rows in completed.values() for row in rows]
    return sorted(results, key=lambda row: row["order"])
