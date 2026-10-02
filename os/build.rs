use std::env;

fn main() {
    let arch = env::var("CARGO_CFG_TARGET_ARCH").unwrap();
    let script = match arch.as_str() {
        "riscv64" => "src/arch/riscv64/linker.ld",
        "loongarch64" => "src/arch/loongarch64/linker.ld",
        _ => panic!("不支持的内核目标：{}", arch),
    };
    println!("cargo:rustc-link-arg=-T{}", script);
    println!("cargo:rerun-if-changed={}", script);
}
