# `fdt`

A pure-Rust `#![no_std]` crate for parsing Flattened Devicetrees, with the goal of having a
very ergonomic and idiomatic API.

[![crates.io](https://img.shields.io/crates/v/fdt.svg)](https://crates.io/crates/fdt) [![Documentation](https://docs.rs/fdt/badge.svg)](https://docs.rs/fdt) ![Build](https://github.com/repnop/fdt/actions/workflows/test.yml/badge.svg?branch=master&event=push)

## BLOSSOM 本地修订

源码与许可证来自 crates.io 的 `fdt 0.1.5` 正式包。只修正 `node.rs`：
节点查找、子节点与全树遍历统一跳过固件留下的 `FDT_NOP`，结束标记使用规范值 `9`。
内核在调用库前由 `os/src/platform.rs` 校验 DTB 的块、字符串和节点深度边界；
本库本身不承诺直接接收任意不可信字节。真实 QEMU DTB 的 NOP 回归覆盖在 boot-tests 中。

## License

This crate is licensed under the Mozilla Public License 2.0 (see the LICENSE file).

## Example

```rust
static MY_FDT: &[u8] = include_bytes!("../dtb/test.dtb");

fn main() {
    let fdt = fdt::Fdt::new(MY_FDT).unwrap();

    println!("This is a devicetree representation of a {}", fdt.root().model());
    println!("...which is compatible with at least: {}", fdt.root().compatible().first());
    println!("...and has {} CPU(s)", fdt.cpus().count());
    println!(
        "...and has at least one memory location at: {:#X}\n",
        fdt.memory().regions().next().unwrap().starting_address as usize
    );

    let chosen = fdt.chosen();
    if let Some(bootargs) = chosen.bootargs() {
        println!("The bootargs are: {:?}", bootargs);
    }

    if let Some(stdout) = chosen.stdout() {
        println!("It would write stdout to: {}", stdout.name);
    }

    let soc = fdt.find_node("/soc");
    println!("Does it have a `/soc` node? {}", if soc.is_some() { "yes" } else { "no" });
    if let Some(soc) = soc {
        println!("...and it has the following children:");
        for child in soc.children() {
            println!("    {}", child.name);
        }
    }
}
```