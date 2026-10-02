use std::env;

/// 按编译 target 选择唯一链接脚本，并让 Cargo 跟踪其实际布局变化。
fn main() {
    let arch = env::var("CARGO_CFG_TARGET_ARCH").expect("Cargo target architecture is missing");
    let script = match arch.as_str() {
        "riscv64" => "src/arch/riscv64/linker.ld",
        "loongarch64" => "src/arch/loongarch64/linker.ld",
        _ => panic!("unsupported kernel target: {}", arch),
    };
    println!("cargo:rustc-link-arg=-T{}", script);
    println!("cargo:rerun-if-changed={}", script);
}
