"""启动的具体外部验收：实际 ELF 装载布局与事件后的真实异常现场。"""

import re

from kernel_elf import read_kernel_elf
from .catalog import HostError, require

TRAP = re.compile(r"  # diagnostic: (synchronous|refill) trap pc=(0x[0-9a-f]+) sp=(0x[0-9a-f]+) cause=(0x[0-9a-f]+) bad=(0x[0-9a-f]+)")


def validate_test_kernel_layout(kernel_elf_path, architecture):
    """在共同 ELF 事实上检查 QEMU 测试内核的映射及所需装载节。"""
    try:
        layout = read_kernel_elf(kernel_elf_path, architecture)
    except (OSError, ValueError) as error:
        raise HostError(str(error)) from error
    physical_base = 0x80200000 if architecture == "riscv64" else 0x90000000
    virtual_base = 0xFFFFFFFF80000000 if architecture == "riscv64" else 0x9000000090000000
    expected_entry = physical_base if architecture == "riscv64" else virtual_base
    require(layout["entry"] == expected_entry, "kernel ELF entry mismatch")
    loads = layout["loads"]
    for load in loads:
        require(load["flags"] in (4, 5, 6), "kernel ELF load permissions invalid")
        address = load["address"]
        physical = (address if architecture == "riscv64" and address == expected_entry else
                    address - virtual_base + physical_base)
        require(load["physical"] == physical, "kernel ELF virtual/physical load mapping mismatch")
    require(min(load["physical"] for load in loads) == physical_base,
            "kernel ELF physical load base mismatch")
    sections = layout["sections"]
    permissions = {".text": 5, ".rodata": 4, ".data": 6, ".bss": 6,
                   ".boot": 5 if architecture == "riscv64" else 6, ".ktest.suites": 4,
                   ".eh_frame": 4, ".eh_frame_hdr": 4, ".gcc_except_table": 4}
    for name, expected in permissions.items():
        if name not in sections:
            continue
        section = sections[name]
        require(section["section_flags"] & 2, "kernel ELF required section is not allocated")
        require(section["flags"] == expected, "kernel ELF section permissions mismatch")
        address, length = section["address"], section["size"]
        matching = [load for load in loads if load["flags"] == expected
                    and load["address"] <= address
                    and address + length <= load["address"] + load["memory_size"]]
        require(len(matching) == 1, "kernel ELF section lacks its permitted load segment")
        if section["type"] != 8:
            load = matching[0]
            require(section["offset"] == load["offset"] + address - load["address"]
                    and section["offset"] + length <= load["offset"] + load["file_size"],
                    "kernel ELF section file mapping mismatch")
    require(all(name in sections and 0 < sections[name]["size"]
                for name in (".text", ".rodata", ".data", ".bss", ".boot")),
            "kernel ELF required load section is missing or empty")
    return layout


def verify_trap(arch, case, directory, fatal):
    """仅真实fatal事件后的唯一完整现场可证明非法指令或refill，不借先前业务诊断。"""
    prefix = "os::ktest::boot_cases::fatal::traps::"
    if not case["name"].startswith(prefix):
        return
    name = case["name"][len(prefix):]
    require(name in ("illegal_instruction", "unmapped_refill"), "unknown concrete Boot trap fixture")
    event = f"  # KTEST fatal {case['case']} {fatal}\n"
    text = (directory / "uart.log").read_text(encoding="utf-8")
    require(text.count(event) == 1, "Boot trap requires one actual fatal event")
    rows = [match.groups() for line in text.split(event, 1)[1].splitlines()
            if (match := TRAP.fullmatch(line))]
    require(len(rows) == 1, "Boot trap requires one complete postevent register frame")
    kind, *values = rows[0]
    pc, sp, cause, bad = (int(value, 16) for value in values)
    require(0 < pc < 1 << 64 and 0 < sp < 1 << 64 and sp % 16 == 0,
            "Boot trap PC/SP invalid")
    if name == "unmapped_refill":
        require(arch == "loongarch64" and fatal == "refill" and kind == "refill"
                and cause == 1 and bad == 0x1000, "Boot refill register frame mismatch")
    else:
        actual_cause = cause if arch == "riscv64" else (cause >> 16) & 0x3F
        require(fatal == "illegal" and kind == "synchronous"
                and actual_cause == (2 if arch == "riscv64" else 13),
                "Boot illegal-instruction register frame mismatch")
