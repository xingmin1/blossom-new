Unwinding library in Rust and for Rust
======================================

[![crates.io](https://img.shields.io/crates/v/unwinding.svg)](https://crates.io/crates/unwinding)
[![docs.rs](https://docs.rs/unwinding/badge.svg)](https://docs.rs/unwinding)
[![license](https://img.shields.io/crates/l/unwinding.svg)](https://crates.io/crates/unwinding)

This library serves two purposes:
1. Provide a pure Rust alternative to libgcc_eh or libunwind.
2. Provide easier unwinding support for `#![no_std]` targets.

Currently supports x86_64, x86, RV64, RV32 and AArch64.

## Unwinder

The unwinder can be enabled with `unwinder` feature. Here are the feature gates related to the unwinder:

| Feature              | Default | Description |
|--------------------- |---------|-|
| unwinder             | Yes     | The primary feature gate to enable the unwinder |
| fde-phdr-dl          | Yes     | Use `dl_iterator_phdr` to retrieve frame unwind table. Depends on libc. |
| fde-phdr-aux         | No      | Use ELF auxiliary vector to retrieve frame unwind table. Depends on libc. |
| fde-registry         | Yes     | Provide `__register__frame` and others for dynamic registration. Requires either `libc` or `spin` for a mutex implementation. |
| fde-gnu-eh-frame-hdr | No      | Use `__executable_start`, `__etext` and `__GNU_EH_FRAME_HDR` to retrieve frame unwind table. The former two symbols are usually provided by the linker, while the last one is provided if GNU LD is used and --eh-frame-hdr option is enabled. |
| fde-static           | No      | Use `__executable_start`, `__etext` and `__eh_frame` to retrieve frame unwind table. The former two symbols are usually provided by the linker, while the last one would need to be provided by the user via linker script.  |
| fde-custom           | No      | Allow the program to provide a custom means of retrieving frame unwind table at runtime via the `set_custom_eh_frame_finder` function. |
| dwarf-expr           | Yes     | Enable the dwarf expression evaluator. Usually not necessary for Rust |
| hide-trace           | Yes     | Hide unwinder frames in back trace |

If you want to use the unwinder for other Rust (C++, or any programs that utilize the unwinder), you can build the [`unwinding_dyn`](cdylib) crate provided, and use `LD_PRELOAD` to replace the system unwinder with it.
```sh
cd cdylib
cargo build --release
# Test the unwinder using rustc. Why not :)
LD_PRELOAD=`../target/release/libunwinding_dyn.so` rustc +nightly -Ztreat-err-as-bug
```

If you want to link to the unwinder in a Rust binary, simply add
```rust
extern crate unwinding;
```

## Personality and other utilities

The library also provides Rust personality function. This can work with the unwinder described above or with a different unwinder. This can be handy if you are working on a `#![no_std]` binary/staticlib/cdylib and you still want unwinding support.

Here are the feature gates related:

| Feature       | Default | Description |
|---------------|---------|-|
| personality   | No      | Provides `#[lang = eh_personality]` |
| print         | No      | Provides `(e)?print(ln)?`. This is really only here because panic handler needs to print things. Depends on libc. |
| panicking     | No      | Provides a generic `begin_panic` and `catch_unwind`. Only stack unwinding functionality is provided, memory allocation and panic handling is left to the user. |
| panic         | No      | Provides Rust `begin_panic` and `catch_unwind`. Only stack unwinding functionality is provided and no printing is done, because this feature does not depend on libc. |
| panic-handler | No      | Provides `#[panic_handler]`. Provides similar behaviour on panic to std, with `RUST_BACKTRACE` support as well. Stack trace won't have symbols though. Depends on libc. |
| system-alloc  | No      | Provides a global allocator which calls `malloc` and friends. Provided for convience. |

If you are writing a `#![no_std]` program, simply enable `personality`, `panic-handler` and `system-alloc` in addition to the defaults, you instantly obtains the ability to do unwinding! An example is given in the [`example/` folder](example).

## Baremetal

To use this library for baremetal projects, disable default features and enable `unwinder`, `fde-static`, `personality`, `panic`. `dwarf-expr` and `hide-trace` are optional. Modify the linker script by
```ld
/* Inserting these two lines */
. = ALIGN(8);
PROVIDE(__eh_frame = .);
/* before .eh_frame rule */
.eh_frame : { KEEP (*(.eh_frame)) *(.eh_frame.*) }
```

And that's it! After you ensured that the global allocator is functional, you can use `unwinding::panic::begin_panic` to initiate an unwing and catch using `unwinding::panic::catch_unwind`, as if you have a `std`.

If your linker supports `--eh-frame-hdr` you can also try to use `fde-gnu-eh-frame-hdr` instead of `fde-static`. GNU LD will provides a `__GNU_EH_FRAME_HDR` magic symbol so you don't have to provide `__eh_frame` through linker script.

If you have your own version of `thread_local` and `println!` working, you can port [`panic_handler.rs`](src/panic_handler.rs) for double-panic protection and stack traces!

## BLOSSOM 本地适配

基础固定为 `181889db4b4c0f39021506e7a753036528cf8c73`（0.2.5），LoongArch64
寄存器定义和真实保存/恢复来自 `5fd5fdcdd2d6b674db1af6cdd547771fcf670a72`。
来源、许可证与本地改动范围见 `ORIGIN.toml`；不跟随 trunk。

内核使用 `default-features = false`，只启用 `unwinder`、`panicking`、
`personality`、`fde-static`、`dwarf-expr` 和 `kernel-percpu`。不启用
`panic`、`alloc`、`panic-handler`、`fde-registry` 或 libc finder。异常头由
内核的 pin 捕获边界保存，库只负责真实两阶段展开和 landing pad 清理。

本地 `kernel-percpu` 不从旧现场恢复 RV/LA 的 tp；LA 的 KS0 从不参与库的寄存器恢复。
RV/LA 保存入口先初始化完整 Context，再写入 ABI callee-saved 现场，避免克隆未初始化槽。
LoongArch CFI 使用数字寄存器 `1`，对应 RA；这是对 LLVM 19 未实现 CFI 名字解析的
限定适配，不是删除 LA 捕获或更换目标浮点 ABI。`#[naked]` 使用 0.2.5 的旧 nightly 形式。

静态 finder 还要求 `__eh_frame_end`，启用 `kernel-percpu` 的 personality 要求
`__gcc_except_table_start` 和 `__gcc_except_table_end`，所有符号必须使用永久映射的
执行 VMA。保留完整 `.eh_frame*`、`.gcc_except_table*`，链接脚本在 eh_frame 末尾
提供零终止项。这里的范围长度替代了内核路径上的无限长切片。

上游 LLVM 20 要求的具体来源是 LA CFI 名字解析：
https://github.com/nbdd0121/unwinding/pull/50#issuecomment-3764196497 和
https://github.com/llvm/llvm-project/pull/117120 。数字 CFI 消除了该名字解析依赖；
固定 nightly/LLVM 19 的 landing pad、CFI/LSDA 和真实 Drop 是否可用仍须双架构实际构建、
检查 ELF 并运行 Boot/Task 捕获，本文不声称该组合已经验证。
