use std::env;
use std::fs;
use std::path::PathBuf;

/// 按编译 target 选择唯一链接脚本，并让 Cargo 跟踪其实际布局变化。
fn main() {
    let arch = env::var("CARGO_CFG_TARGET_ARCH").expect("Cargo target architecture is missing");
    let script = match arch.as_str() {
        "riscv64" => "src/arch/riscv64/linker.ld",
        "loongarch64" => "src/arch/loongarch64/linker.ld",
        _ => panic!("unsupported kernel target: {}", arch),
    };
    let output = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo output directory is missing"));
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
