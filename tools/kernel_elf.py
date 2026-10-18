"""读取内核 ELF 的实际装载、节和公开符号；平台及测试要求由消费者检查。"""

import mmap
import os
from pathlib import Path
import struct


def read_string(image, start, size, offset, label):
    """只在已验证的字符串表范围内读取一个终止字符串。"""
    if not 0 <= offset < size:
        raise ValueError(f"kernel ELF {label} offset is invalid")
    end = image.find(b"\0", start + offset, start + size)
    if end < 0:
        raise ValueError(f"kernel ELF {label} is unterminated")
    return image[start + offset:end].decode("utf-8")


def read_kernel_elf(kernel_elf_path, architecture):
    """一次映射 ELF64，返回有界的 entry、flags、LOAD、节及已定义公开符号事实。"""
    if architecture not in ("riscv64", "loongarch64"):
        raise ValueError("unsupported ELF architecture")
    with Path(kernel_elf_path).open("rb") as source:
        size = os.fstat(source.fileno()).st_size
        if size < 64:
            raise ValueError("kernel ELF header is truncated")
        with mmap.mmap(source.fileno(), 0, access=mmap.ACCESS_READ) as image:
            (ident, kind, machine, version, entry, phoff, shoff, flags, ehsize,
             phsize, phnum, shsize, shnum, strings_index) = struct.unpack_from(
                 "<16sHHIQQQIHHHHHH", image)
            if not (ident[:7] == b"\x7fELF\x02\x01\x01" and kind == 2
                    and version == 1 and ehsize == 64):
                raise ValueError("kernel must be a version-one little-endian ELF64 executable")
            if machine != (243 if architecture == "riscv64" else 258):
                raise ValueError("ELF architecture mismatch")
            if not (flags & 7 == (5 if architecture == "riscv64" else 3)
                    and (architecture == "riscv64" or flags & 0xC0 == 0x40)):
                raise ValueError("kernel ELF ABI mismatch")
            if not (phsize == 56 and 0 < phnum < 0xFFFF
                    and 64 <= phoff <= size - phnum * phsize):
                raise ValueError("kernel ELF program table is invalid")
            if not (shsize == 64 and 0 < strings_index < shnum
                    and 64 <= shoff <= size - shnum * shsize):
                raise ValueError("kernel ELF section table is invalid")
            loads = []
            for index in range(phnum):
                segment = struct.unpack_from("<IIQQQQQQ", image, phoff + index * phsize)
                if segment[0] != 1:
                    continue
                _, permission, offset, address, physical, file_size, memory_size, alignment = segment
                if not (permission < 8 and 0 < memory_size and file_size <= memory_size
                        and offset <= size - file_size and address + memory_size <= 1 << 64
                        and physical + memory_size <= 1 << 64):
                    raise ValueError("kernel ELF load bounds or permissions invalid")
                if not (0 < alignment and alignment & (alignment - 1) == 0
                        and address % alignment == offset % alignment):
                    raise ValueError("kernel ELF load alignment invalid")
                loads.append(dict(flags=permission, offset=offset, address=address, physical=physical,
                                  file_size=file_size, memory_size=memory_size, alignment=alignment))
            if not loads:
                raise ValueError("kernel ELF has no LOAD segments")
            ordered = sorted(loads, key=lambda load: load["physical"])
            if not all(left["physical"] + left["memory_size"] <= right["physical"]
                       for left, right in zip(ordered, ordered[1:])):
                raise ValueError("kernel ELF physical loads overlap")
            if not any(load["flags"] & 1 and load["address"] <= entry
                       < load["address"] + load["file_size"] for load in loads):
                raise ValueError("kernel ELF entry is not file-backed executable code")
            headers = [struct.unpack_from("<IIQQQQIIQQ", image, shoff + index * shsize)
                       for index in range(shnum)]
            strings = headers[strings_index]
            strings_start, strings_size = strings[4:6]
            if not (strings[1] == 3 and strings_start <= size - strings_size):
                raise ValueError("kernel ELF section names are invalid")
            sections = {}
            for header in headers:
                name_offset, section_type, section_flags, address, offset, length, *_ = header
                if (1 << 64) < address + length or (section_type != 8 and size - length < offset):
                    raise ValueError("kernel ELF section bounds are invalid")
                name = read_string(image, strings_start, strings_size, name_offset, "section name")
                if not name:
                    continue
                if name in sections:
                    raise ValueError("kernel ELF section name is duplicate")
                permission = 4 | (2 if section_flags & 1 else 0) | (1 if section_flags & 4 else 0)
                sections[name] = dict(address=address, offset=offset, size=length, flags=permission,
                                      type=section_type, section_flags=section_flags)
            symbols = {}
            for header in headers:
                if header[1] != 2:
                    continue
                offset, length, link, _, _, entsize = header[4:]
                if not (entsize == 24 and length % entsize == 0 and link < shnum):
                    raise ValueError("kernel ELF symbol table is invalid")
                strings = headers[link]
                start, length_strings = strings[4:6]
                if not (strings[1] == 3 and start <= size - length_strings):
                    raise ValueError("kernel ELF symbol strings are invalid")
                for at in range(offset, offset + length, entsize):
                    name_offset, info, _, defined, value, _ = struct.unpack_from("<IBBHQQ", image, at)
                    name = read_string(image, start, length_strings, name_offset, "symbol name")
                    if not defined or info >> 4 not in (1, 2) or not name:
                        continue
                    if name in symbols:
                        raise ValueError("kernel ELF public symbol is duplicate")
                    symbols[name] = value
    return dict(entry=entry, flags=flags, loads=loads, sections=sections, symbols=symbols)
