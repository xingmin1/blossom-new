"""启动的具体外部验收：实际 ELF 装载布局与事件后的真实异常现场。"""

import mmap
import re
import struct

from .catalog import require

TRAP = re.compile(r"  # diagnostic: (synchronous|refill) trap pc=(0x[0-9a-f]+) sp=(0x[0-9a-f]+) cause=(0x[0-9a-f]+) bad=(0x[0-9a-f]+)")


def elf_layout(kernel, arch):
    """有界读取实际ELF表，核入口、ABI、装载地址及段/节权限，不固定历史尺寸。"""
    size = kernel.stat().st_size
    require(size >= 64, "kernel ELF header is truncated")
    with kernel.open("rb") as source, mmap.mmap(source.fileno(), 0, access=mmap.ACCESS_READ) as image:
        (ident, kind, machine, version, entry, phoff, shoff, flags, ehsize,
         phsize, phnum, shsize, shnum, strings_index) = struct.unpack_from("<16sHHIQQQIHHHHHH", image)
        require(ident[:7] == b"\x7fELF\x02\x01\x01" and kind == 2 and version == 1 and ehsize == 64,
                "kernel must be a version-one little-endian ELF64 executable")
        require(machine == (243 if arch == "riscv64" else 258), "ELF architecture mismatch")
        require(flags & 7 == (5 if arch == "riscv64" else 3)
                and (arch == "riscv64" or flags & 0xC0 == 0x40), "kernel ELF ABI mismatch")
        expected_entry = 0x80200000 if arch == "riscv64" else 0x9000000090000000
        require(entry == expected_entry, "kernel ELF entry mismatch")
        require(phsize == 56 and 0 < phnum < 0xFFFF and 64 <= phoff <= size - phnum * phsize,
                "kernel ELF program table is invalid")
        require(shsize == 64 and 0 < strings_index < shnum and 64 <= shoff <= size - shnum * shsize,
                "kernel ELF section table is invalid")
        loads = []
        for index in range(phnum):
            segment = struct.unpack_from("<IIQQQQQQ", image, phoff + index * phsize)
            if segment[0] != 1:
                continue
            _, permission, offset, address, physical, file_size, memory_size, alignment = segment
            require(permission in (4, 5, 6) and 0 < memory_size and file_size <= memory_size
                    and offset <= size - file_size and address + memory_size <= 1 << 64
                    and physical + memory_size <= 1 << 64, "kernel ELF load bounds or permissions invalid")
            require(alignment > 0 and alignment & (alignment - 1) == 0
                    and address % alignment == offset % alignment, "kernel ELF load alignment invalid")
            expected_physical = (address if address == expected_entry else
                                 address - 0xFFFFFFFF80000000 + 0x80200000) if arch == "riscv64" else address - 0x9000000000000000
            require(physical == expected_physical, "kernel ELF virtual/physical load mapping mismatch")
            loads.append(dict(flags=permission, offset=offset, address=address, physical=physical,
                              file_size=file_size, memory_size=memory_size, alignment=alignment))
        ordered = sorted(loads, key=lambda load: load["physical"])
        require(ordered and ordered[0]["physical"] == (0x80200000 if arch == "riscv64" else 0x90000000),
                "kernel ELF physical load base mismatch")
        require(all(left["physical"] + left["memory_size"] <= right["physical"]
                    for left, right in zip(ordered, ordered[1:])), "kernel ELF physical loads overlap")
        require(any(load["flags"] == 5 and load["address"] <= entry < load["address"] + load["file_size"]
                    for load in loads), "kernel ELF entry is not file-backed executable code")
        strings = struct.unpack_from("<IIQQQQIIQQ", image, shoff + strings_index * shsize)
        strings_start, strings_size = strings[4:6]
        require(strings[1] == 3 and strings_start <= size - strings_size,
                "kernel ELF section names are invalid")
        sections = {}
        permissions = {b".text": 5, b".rodata": 4, b".data": 6, b".bss": 6,
                       b".boot": 5 if arch == "riscv64" else 6, b".ktest.suites": 4,
                       b".eh_frame": 4, b".eh_frame_hdr": 4, b".gcc_except_table": 4}
        for index in range(shnum):
            name_offset, section_type, section_flags, address, offset, length, *_ = struct.unpack_from(
                "<IIQQQQIIQQ", image, shoff + index * shsize)
            require(name_offset < strings_size, "kernel ELF section name offset invalid")
            end = image.find(b"\0", strings_start + name_offset, strings_start + strings_size)
            require(end >= 0, "kernel ELF section name is unterminated")
            name = image[strings_start + name_offset:end]
            if name not in permissions:
                continue
            require(name not in sections and section_flags & 2,
                    "kernel ELF required section is duplicate or not allocated")
            expected = permissions[name]
            require(bool(section_flags & 1) == bool(expected & 2)
                    and bool(section_flags & 4) == bool(expected & 1), "kernel ELF section permissions mismatch")
            matching = [load for load in loads if load["flags"] == expected
                        and load["address"] <= address and address + length <= load["address"] + load["memory_size"]]
            require(len(matching) == 1, "kernel ELF section lacks its permitted load segment")
            if section_type != 8:
                load = matching[0]
                require(offset == load["offset"] + address - load["address"]
                        and offset + length <= load["offset"] + load["file_size"],
                        "kernel ELF section file mapping mismatch")
            sections[name.decode("ascii")] = dict(address=address, offset=offset, size=length, flags=expected)
        require(all(name in sections and sections[name]["size"] > 0
                    for name in (".text", ".rodata", ".data", ".bss", ".boot")),
                "kernel ELF required load section is missing or empty")
    return dict(entry=entry, flags=flags, loads=loads, sections=sections)


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
