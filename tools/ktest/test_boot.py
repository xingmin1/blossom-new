"""宿主 boot 回归；运行 python3 -m unittest discover -s tools/ktest。"""

from pathlib import Path
import struct
import tempfile
import unittest

from host_helpers import case_record
from ktest.boot import elf_layout, verify_trap
from ktest.catalog import HostError


class BootELFTests(unittest.TestCase):
    """实际装载格式的边界检查；合成ELF不充当客体执行或链接证明。"""

    def test_load_contract_rejects_corruption_and_allows_empty_optional_header(self):
        """两架构拒错入口/映射/权限/表界，合法零长eh_frame_hdr不应阻断发现。"""
        for arch in ("riscv64", "loongarch64"):
            with self.subTest(arch=arch), tempfile.TemporaryDirectory() as temporary:
                rv = arch == "riscv64"
                entry = 0x80200000 if rv else 0x9000000090000000
                text = 0xFFFFFFFF80200000 if rv else entry
                physical = 0x80400000 if rv else 0x90000000
                first = 0x2000 if rv else 0x1000
                loads = ([(5, 0x1000, entry, entry, 32, 32)] if rv else []) + [
                    (5, first, text, physical, 32, 32),
                    (4, first + 0x1000, text + 0x1000, physical + 0x1000, 32, 32),
                    (6, first + 0x2000, text + 0x2000, physical + 0x2000, 32, 128)]
                sections = [
                    (b".text", 1, 6, text, first, 32),
                    (b".rodata", 1, 2, text + 0x1000, first + 0x1000, 16),
                    (b".eh_frame_hdr", 1, 2, text + 0x1010, first + 0x1010, 0),
                    (b".data", 1, 3, text + 0x2000, first + 0x2000, 32),
                    (b".boot", 1 if rv else 8, 6 if rv else 3,
                     entry if rv else text + 0x2020, 0x1000 if rv else first + 0x2020, 32),
                    (b".bss", 8, 3, text + 0x2040, first + 0x2040, 32)]
                names = b"\0" + b"\0".join(section[0] for section in sections) + b"\0.shstrtab\0"
                image = bytearray(0x5800)
                struct.pack_into("<16sHHIQQQIHHHHHH", image, 0, b"\x7fELF\x02\x01\x01", 2,
                                 243 if rv else 258, 1, entry, 64, 0x5400, 5 if rv else 0x43,
                                 64, 56, len(loads), 64, len(sections) + 2, len(sections) + 1)
                for index, (flags, offset, address, pa, file_size, memory_size) in enumerate(loads):
                    struct.pack_into("<IIQQQQQQ", image, 64 + index * 56, 1, flags, offset,
                                     address, pa, file_size, memory_size, 0x1000)
                for index, (name, kind, flags, address, offset, length) in enumerate(sections, 1):
                    struct.pack_into("<IIQQQQIIQQ", image, 0x5400 + index * 64,
                                     names.index(name), kind, flags, address, offset, length, 0, 0, 1, 0)
                struct.pack_into("<IIQQQQIIQQ", image, 0x5400 + (len(sections) + 1) * 64,
                                 names.index(b".shstrtab"), 3, 0, 0, 0x5000, len(names), 0, 0, 1, 0)
                image[0x5000:0x5000 + len(names)] = names
                kernel = Path(temporary) / "kernel.elf"
                kernel.write_bytes(image)
                layout = elf_layout(kernel, arch)
                self.assertEqual(layout["entry"], entry)
                self.assertEqual(layout["sections"][".eh_frame_hdr"]["size"], 0)
                self.assertEqual(layout["sections"][".bss"]["flags"], 6)
                corruptions = [
                    ("<H", 16, 3), ("<H", 18, 258 if rv else 243), ("<Q", 24, entry + 4),
                    ("<Q", 40, 1 << 63), ("<I", 64 + 4, 7),
                    ("<Q", 64 + 24, loads[0][3] + 0x1000),
                    ("<Q", 64 + 32, 64), ("<Q", 0x5400 + 64 + 8, 7)]
                for fmt, offset, value in corruptions:
                    with self.subTest(offset=offset, value=value):
                        broken = bytearray(image)
                        struct.pack_into(fmt, broken, offset, value)
                        kernel.write_bytes(broken)
                        with self.assertRaises(HostError):
                            elf_layout(kernel, arch)
                kernel.write_bytes(image[:64])
                with self.assertRaises(HostError):
                    elf_layout(kernel, arch)


class BootTrapTests(unittest.TestCase):
    """真实故障事件之后必须有匹配架构的完整寄存器现场。"""

    def test_boot_fatal_requires_actual_postevent_register_frame(self):
        """正确kind不能洗掉缺现场、零PC/SP、错cause或只在fatal前出现的业务现场。"""
        frames = (
            ("riscv64", "illegal_instruction", "illegal",
             "synchronous trap pc=0xffffffff802f317e sp=0xffffffff80593760 cause=0x2 bad=0x0", "0x2", "0x3"),
            ("loongarch64", "illegal_instruction", "illegal",
             "synchronous trap pc=0x90000000900a4690 sp=0x9000000090604720 cause=0xd0000 bad=0x90000000900a4690", "0xd0000", "0xc0000"),
            ("loongarch64", "unmapped_refill", "refill",
             "refill trap pc=0x90000000900a46b4 sp=0x9000000090604720 cause=0x1 bad=0x1000", "0x1", "0x0"),
        )
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            for arch, name, fatal, frame, cause, wrong_cause in frames:
                case = case_record(cid=7, name="os::ktest::boot_cases::fatal::traps::" + name)
                event = f"  # KTEST fatal 7 {fatal}\n"
                line = "  # diagnostic: " + frame + "\n"
                (directory / "uart.log").write_text(event + line, encoding="utf-8")
                verify_trap(arch, case, directory, fatal)
                for bad in ("", line + line, line.replace("cause=" + cause, "cause=" + wrong_cause),
                            line.replace("pc=" + frame.split("pc=")[1].split()[0], "pc=0x0"),
                            line.replace("sp=" + frame.split("sp=")[1].split()[0], "sp=0x0")):
                    (directory / "uart.log").write_text(line + event + bad, encoding="utf-8")
                    with self.subTest(arch=arch, name=name, bad=bad), self.assertRaises(HostError):
                        verify_trap(arch, case, directory, fatal)


if __name__ == "__main__":
    unittest.main()
