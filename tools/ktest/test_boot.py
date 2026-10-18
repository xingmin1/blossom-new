"""宿主 boot 回归；运行 python3 -m unittest discover -s tools/ktest。"""

import contextlib
import io
from pathlib import Path
import struct
import tempfile
import unittest
import zlib

from host_helpers import case_record
from kernel_elf import read_kernel_elf
from ktest.boot import validate_test_kernel_layout, verify_trap
from ktest.catalog import HostError
from package_board_boot_images import LEGACY_HEADER, U32_LIMIT, make_legacy_header, main, package_board_boot_images


def kernel_image(architecture, target_platform="qemu-virt", empty_unwind_header=False):
    """构造含实际 LOAD、普通生产节及公开符号的 ELF 字节，不充当客体链接或执行证明。"""
    rv = architecture == "riscv64"
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
        (b".data", 1, 3, text + 0x2000, first + 0x2000, 32),
        (b".boot", 1 if rv else 8, 6 if rv else 3,
         entry if rv else text + 0x2020, 0x1000 if rv else first + 0x2020, 32),
        (b".bss", 8, 3, text + 0x2040, first + 0x2040, 32)]
    if empty_unwind_header:
        sections.append((b".eh_frame_hdr", 1, 2, text + 0x1010, first + 0x1010, 0))
    symbol_values = {"__blossom_platform": {"qemu-virt": 0, "visionfive2": 1, "ls2k1000la": 2}[target_platform],
                     "__kernel_phys_start": entry if rv else physical,
                     "__kernel_virt_start": 0xFFFFFFFF80000000 if rv else entry,
                     "__kernel_phys_end": physical + 0x2080,
                     "__kernel_virt_end": text + 0x2080,
                     "__direct_base": 0xFFFFFFFF80000000 if rv else 0x9000000000000000,
                     "__phys_ram_base": entry if rv else physical,
                     "ordinary_public_symbol": text + 4}
    symbol_strings = b"\0" + b"\0".join(name.encode() for name in symbol_values) + b"\0"
    sections += [(b".strtab", 3, 0, 0, 0x5300, len(symbol_strings)),
                 (b".symtab", 2, 0, 0, 0x5100, (len(symbol_values) + 1) * 24)]
    names = b"\0" + b"\0".join(section[0] for section in sections) + b"\0.shstrtab\0"
    image = bytearray(0x5800)
    struct.pack_into("<16sHHIQQQIHHHHHH", image, 0, b"\x7fELF\x02\x01\x01", 2,
                     243 if rv else 258, 1, entry, 64, 0x5400, 5 if rv else 0x43,
                     64, 56, len(loads), 64, len(sections) + 2, len(sections) + 1)
    for index, (flags, offset, address, pa, file_size, memory_size) in enumerate(loads):
        struct.pack_into("<IIQQQQQQ", image, 64 + index * 56, 1, flags, offset,
                         address, pa, file_size, memory_size, 0x1000)
    for index, (name, kind, flags, address, offset, length) in enumerate(sections, 1):
        link = next(i for i, section in enumerate(sections, 1) if section[0] == b".strtab") if kind == 2 else 0
        struct.pack_into("<IIQQQQIIQQ", image, 0x5400 + index * 64,
                         names.index(name), kind, flags, address, offset, length, link, 0, 1,
                         24 if kind == 2 else 0)
    struct.pack_into("<IIQQQQIIQQ", image, 0x5400 + (len(sections) + 1) * 64,
                     names.index(b".shstrtab"), 3, 0, 0, 0x5000, len(names), 0, 0, 1, 0)
    image[0x5000:0x5000 + len(names)] = names
    image[0x5300:0x5300 + len(symbol_strings)] = symbol_strings
    for index, (name, value) in enumerate(symbol_values.items(), 1):
        struct.pack_into("<IBBHQQ", image, 0x5100 + index * 24,
                         symbol_strings.index(name.encode()), 0x10, 0, 0xFFF1, value, 0)
    for index, (_, offset, _, _, file_size, _) in enumerate(loads, 1):
        image[offset:offset + file_size] = bytes([index]) * file_size
    return image, entry, loads


class BootELFTests(unittest.TestCase):
    """实际装载格式的边界检查；合成 ELF 不充当客体执行或链接证明。"""

    def test_load_contract_rejects_corruption_and_allows_empty_optional_header(self):
        """两架构拒错入口、映射、权限和表界，合法零长 eh_frame_hdr 不阻断发现。"""
        for architecture in ("riscv64", "loongarch64"):
            with self.subTest(architecture=architecture), tempfile.TemporaryDirectory() as temporary:
                image, entry, loads = kernel_image(architecture, empty_unwind_header=True)
                kernel = Path(temporary) / "kernel.elf"
                kernel.write_bytes(image)
                layout = validate_test_kernel_layout(kernel, architecture)
                self.assertEqual(layout["entry"], entry)
                self.assertEqual(layout["sections"][".eh_frame_hdr"]["size"], 0)
                self.assertEqual(layout["sections"][".bss"]["flags"], 6)
                corruptions = [
                    ("<H", 16, 3), ("<H", 18, 258 if architecture == "riscv64" else 243),
                    ("<Q", 24, entry + 4), ("<Q", 40, 1 << 63), ("<I", 64 + 4, 7),
                    ("<Q", 64 + 24, loads[0][3] + 0x1000),
                    ("<Q", 64 + 32, 64), ("<Q", 0x5400 + 64 + 8, 7)]
                for fmt, offset, value in corruptions:
                    with self.subTest(offset=offset, value=value):
                        broken = bytearray(image)
                        struct.pack_into(fmt, broken, offset, value)
                        kernel.write_bytes(broken)
                        with self.assertRaises(HostError):
                            validate_test_kernel_layout(kernel, architecture)
                kernel.write_bytes(image[:64])
                with self.assertRaises(HostError):
                    validate_test_kernel_layout(kernel, architecture)

    def test_common_reader_accepts_four_production_platforms_without_test_sections(self):
        """共同读取器保留四平台事实和公开符号，不要求测试或展开节。"""
        for architecture, platform in (("riscv64", "qemu-virt"), ("loongarch64", "qemu-virt"),
                                       ("riscv64", "visionfive2"), ("loongarch64", "ls2k1000la")):
            with self.subTest(architecture=architecture, platform=platform), tempfile.TemporaryDirectory() as temporary:
                image, entry, loads = kernel_image(architecture, platform)
                kernel = Path(temporary) / "kernel.elf"
                kernel.write_bytes(image)
                layout = read_kernel_elf(kernel, architecture)
                self.assertEqual(layout["entry"], entry)
                self.assertEqual(len(layout["loads"]), len(loads))
                self.assertEqual(layout["loads"][-1]["memory_size"], 128)
                self.assertEqual(layout["symbols"]["ordinary_public_symbol"], loads[-3][2] + 4)
                self.assertNotIn(".ktest.suites", layout["sections"])
                self.assertNotIn(".eh_frame", layout["sections"])
                self.assertNotIn(".eh_frame_hdr", layout["sections"])

    def test_common_reader_preserves_format_and_io_errors(self):
        """格式错保留 ValueError，文件不存在保留 OSError，测试边界转换为 HostError。"""
        with tempfile.TemporaryDirectory() as temporary:
            kernel = Path(temporary) / "kernel.elf"
            with self.assertRaises(OSError):
                read_kernel_elf(kernel, "riscv64")
            with self.assertRaises(HostError):
                validate_test_kernel_layout(kernel, "riscv64")
            image, _, _ = kernel_image("riscv64")
            corruptions = [("<H", 16, 3), ("<Q", 40, 1 << 63), ("<Q", 64 + 32, 1 << 63),
                           ("<Q", 64 + 24, (1 << 64) - 16), ("<Q", 64 + 48, 3),
                           ("<I", 0x5400 + 64, 1 << 31)]
            for fmt, offset, value in corruptions:
                broken = bytearray(image)
                struct.pack_into(fmt, broken, offset, value)
                kernel.write_bytes(broken)
                with self.subTest(offset=offset), self.assertRaises(ValueError):
                    read_kernel_elf(kernel, "riscv64")


class BoardPackagingTests(unittest.TestCase):
    """验证离线文件内容、发布边界和正式 CLI，不把生成物当作实板运行证据。"""

    def test_production_images_crc_memory_spans_and_optional_inputs(self):
        """两板使用无测试节 ELF，保真实 CRC、完整 BSS 末端及缺输入不生成命令。"""
        for architecture, platform in (("riscv64", "visionfive2"), ("loongarch64", "ls2k1000la")):
            with self.subTest(architecture=architecture), tempfile.TemporaryDirectory() as temporary:
                directory = Path(temporary)
                kernel = directory / "input kernel.elf"
                image, _, loads = kernel_image(architecture, platform)
                kernel.write_bytes(image)
                output = directory / "boot images"
                package_board_boot_images(architecture, platform, kernel, output)
                self.assertEqual((output / "kernel.elf").read_bytes(), image)
                raw = (output / "kernel.bin").read_bytes()
                self.assertEqual(len(raw), loads[-1][3] + loads[-1][4] - loads[0][3])
                for _, offset, _, physical, file_size, _ in loads:
                    start = physical - loads[0][3]
                    self.assertEqual(raw[start:start + file_size], image[offset:offset + file_size])
                container = (output / "kernel.uImage").read_bytes()
                fields = LEGACY_HEADER.unpack(container[:LEGACY_HEADER.size])
                self.assertEqual(fields[0], 0x27051956)
                self.assertEqual(fields[3:7], (len(raw), loads[0][3], loads[0][3], zlib.crc32(raw)))
                self.assertEqual(fields[7:11], (5, 26 if architecture == "riscv64" else 27, 2, 0))
                self.assertEqual(fields[1], zlib.crc32(container[:4] + bytes(4) + container[8:LEGACY_HEADER.size]))
                self.assertEqual(container[LEGACY_HEADER.size:], raw)
                environment = (output / "boot.env").read_text()
                self.assertNotIn("\nblossom_boot=", environment)
                self.assertNotIn("\nbootargs=", environment)
                rows = dict(line.split("=", 1) for line in environment.splitlines() if not line.startswith("#"))
                memory_end = loads[-1][3] + loads[-1][5]
                self.assertEqual(int(rows["kernel_mem_end"], 16), memory_end)
                self.assertLessEqual(memory_end, int(rows["kernel_addr_r"], 16))
                dtb = directory / "OS tree.dtb"
                dtb.write_bytes(struct.pack(">II", 0xD00DFEED, 40) + bytes(32))
                root = directory / "root.ext4"
                root_bytes = bytearray(4096)
                root_bytes[1080:1082] = b"\x53\xef"
                root.write_bytes(root_bytes)
                package_board_boot_images(architecture, platform, kernel, output, os_dtb_path=dtb)
                environment = (output / "boot.env").read_text()
                self.assertNotIn("\nblossom_boot=", environment)
                self.assertNotIn("\nbootargs=", environment)
                package_board_boot_images(architecture, platform, kernel, output,
                                          root, dtb, "/dev/ram0")
                environment = (output / "boot.env").read_text()
                self.assertIn("\nbootargs=root=/dev/ram0 rootdir=/ ro init=/sbin/init\n", environment)
                self.assertIn("\nblossom_boot=bootm ", environment)
                initrd = (output / "root.uInitrd").read_bytes()
                self.assertEqual(initrd[LEGACY_HEADER.size:], root_bytes)
                self.assertEqual(LEGACY_HEADER.unpack(initrd[:LEGACY_HEADER.size])[6], zlib.crc32(root_bytes))
                padded_dtb = (output / "board.dtb").read_bytes()
                self.assertEqual(len(padded_dtb), 40 + 64 * 1024)
                self.assertEqual(struct.unpack_from(">I", padded_dtb, 4)[0], len(padded_dtb))
                self.assertEqual(root.read_bytes(), root_bytes)
                self.assertEqual(dtb.read_bytes(), struct.pack(">II", 0xD00DFEED, 40) + bytes(32))
                package_board_boot_images(architecture, platform, kernel, output, root_device="/dev/sda1")
                self.assertNotIn("\nblossom_boot=", (output / "boot.env").read_text())
                self.assertFalse((output / "root.uInitrd").exists())
                self.assertFalse((output / "board.dtb").exists())

    def test_failed_packaging_leaves_published_files_unchanged(self):
        """错误输入只影响临时目录，先前发表的镜像不被半成品覆盖。"""
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            kernel = directory / "kernel.elf"
            image, _, _ = kernel_image("riscv64", "visionfive2")
            kernel.write_bytes(image)
            output = directory / "output"
            package_board_boot_images("riscv64", "visionfive2", kernel, output)
            published = {path.name: path.read_bytes() for path in output.iterdir()}
            invalid_input = directory / "invalid-input"
            invalid_input.write_bytes(b"invalid")
            for options in ({"root_device": "/dev/ram0"}, {"root_device": "/dev/sda1;bad"},
                            {"root_device": "/dev/sda1", "init_program": "/sbin/init bad"},
                            {"root_ext4_image_path": invalid_input}, {"os_dtb_path": invalid_input}):
                with self.subTest(options=options), self.assertRaises(ValueError):
                    package_board_boot_images("riscv64", "visionfive2", kernel, output, **options)
                self.assertEqual({path.name: path.read_bytes() for path in output.iterdir()}, published)
            with self.assertRaises(ValueError):
                package_board_boot_images("loongarch64", "ls2k1000la", kernel, output)
            with self.assertRaises(ValueError):
                make_legacy_header(1, 0, 26, 2, U32_LIMIT, 0, 0)

    def test_cli_requires_full_names_and_accepts_documented_inputs(self):
        """正式 CLI 拒绝旧参数和缩写，完整参数发布文件且默认根权限为只读。"""
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            kernel = directory / "kernel.elf"
            image, _, _ = kernel_image("riscv64", "visionfive2")
            kernel.write_bytes(image)
            output = directory / "output"
            arguments = ["--arch", "riscv64", "--target-platform", "visionfive2",
                         "--kernel-elf", str(kernel), "--output-directory", str(output)]
            for invalid in (["--platform", "visionfive2"], ["--kernel", str(kernel)],
                            ["--output", str(output)], ["--root", "/dev/sda1"]):
                with self.subTest(invalid=invalid), contextlib.redirect_stderr(io.StringIO()):
                    with self.assertRaises(SystemExit) as error:
                        main(arguments + invalid)
                    self.assertEqual(error.exception.code, 2)
                self.assertFalse(output.exists())
            with contextlib.redirect_stdout(io.StringIO()):
                main(arguments + ["--root-device", "/dev/sda1"])
            self.assertIn("bootargs=root=/dev/sda1 rootdir=/ ro init=/sbin/init\n",
                          (output / "boot.env").read_text())


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
