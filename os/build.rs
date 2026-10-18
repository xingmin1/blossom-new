use std::env;
use std::fs;
use std::path::PathBuf;

/// 校验架构与平台组合，生成唯一装载常量及链接段并登记 Cargo 重建条件。
fn main() {
    let arch = env::var("CARGO_CFG_TARGET_ARCH").expect("Cargo target architecture is missing");
    let platform = env::var("BLOSSOM_PLATFORM").unwrap_or_else(|_| "qemu-virt".into());
    let (identity, physical, virtual_start, direct, mmio, ram_base): (u8, u64, u64, u64, u64, u64) =
        match (arch.as_str(), platform.as_str()) {
            ("riscv64", "qemu-virt") => (
                0,
                0x80200000,
                0xffffffff80000000,
                0xffffffd600000000,
                0xffffffc000000000,
                0x80000000,
            ),
            ("riscv64", "visionfive2") => (
                1,
                0x40200000,
                0xffffffff80000000,
                0xffffffd600000000,
                0xffffffc000000000,
                0x40000000,
            ),
            ("loongarch64", "qemu-virt") => (
                0,
                0x90000000,
                0x9000000090000000,
                0x9000000000000000,
                0x8000000000000000,
                0,
            ),
            ("loongarch64", "ls2k1000la") => (
                2,
                0x00200000,
                0x9000000000200000,
                0x9000000000000000,
                0x8000000000000000,
                0,
            ),
            _ => panic!("unsupported kernel target/platform: {}/{}", arch, platform),
        };
    println!("cargo:rerun-if-env-changed=BLOSSOM_PLATFORM");
    println!("cargo:rustc-check-cfg=cfg(blossom_platform, values(\"qemu-virt\", \"visionfive2\", \"ls2k1000la\"))");
    println!("cargo:rustc-cfg=blossom_platform=\"{platform}\"");
    let script = match arch.as_str() {
        "riscv64" => "src/arch/riscv64/linker.ld",
        "loongarch64" => "src/arch/loongarch64/linker.ld",
        _ => panic!("unsupported kernel target: {}", arch),
    };
    let output = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo output directory is missing"));
    let layout = format!(
        "__blossom_platform = {identity};\n\
         __kernel_phys_start = {physical:#x};\n\
         __kernel_virt_start = {virtual_start:#x};\n\
         __direct_base = {direct:#x};\n\
         __mmio_base = {mmio:#x};\n\
         __phys_ram_base = {ram_base:#x};\n"
    );
    fs::write(output.join("platform-layout.ld"), layout)
        .expect("platform linker layout cannot be written");
    let testing = env::var_os("CARGO_FEATURE_KTEST").is_some();
    let sections = if testing {
        r#"
  . = ALIGN(8);
  .ktest.suites : AT(ADDR(.ktest.suites) - __kernel_virt_start + __kernel_phys_start) {
    __ktest_suites_start = .;
    KEEP(*(.ktest.suites))
    __ktest_suites_end = .;
  } :rodata
  . = ALIGN(8);
  .eh_frame : AT(ADDR(.eh_frame) - __kernel_virt_start + __kernel_phys_start) {
    __eh_frame = .;
    KEEP(*(.eh_frame .eh_frame.*))
    LONG(0)
    __eh_frame_end = .;
  } :rodata
  .eh_frame_hdr : AT(ADDR(.eh_frame_hdr) - __kernel_virt_start + __kernel_phys_start) {
    KEEP(*(.eh_frame_hdr .eh_frame_hdr.*))
  } :rodata
  .gcc_except_table : AT(ADDR(.gcc_except_table) - __kernel_virt_start + __kernel_phys_start) {
    __gcc_except_table_start = .;
    KEEP(*(.gcc_except_table .gcc_except_table.*))
    __gcc_except_table_end = .;
  } :rodata
"#
    } else {
        "/DISCARD/ : { *(.eh_frame .eh_frame.* .eh_frame_hdr .eh_frame_hdr.* .gcc_except_table .gcc_except_table.*) }\n"
    };
    fs::write(output.join("ktest-sections.ld"), sections)
        .expect("linker sections cannot be written");
    println!("cargo:rustc-link-arg=-L{}", output.display());
    if testing {
        println!("cargo:rustc-link-arg=--undefined-glob=__ktest_register__*");
    }
    println!("cargo:rustc-link-arg=-T{}", script);
    println!("cargo:rerun-if-changed={}", script);
}
