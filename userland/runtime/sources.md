# 运行库与原测试输入来源

构建、三类测试、配置和工作盘命令见[公共操作指南](../../docs/development.md)，当前源码阅读见[运行根导读](development.md)。本文说明运行库资产准备、官方盘身份与原失败边界；运行库准备实现为同目录 [prepare_runtime_libraries.py](prepare_runtime_libraries.py)。资产准备成功不等于 guest 测试通过。

运行库与原测试固定采用 [oscomp/testsuits-for-oskernel 的 8b58dd16d26d30f7c74d48d5832d870d3051b703](https://github.com/oscomp/testsuits-for-oskernel/tree/8b58dd16d26d30f7c74d48d5832d870d3051b703) 和 `docker.educg.net/cg/os-contest:20250624`。`prepare_runtime_libraries.py` 只复制明确的本地来源，不搜索其他版本、不下载或安装工具链，也不重编或修改原测试 ELF、PT_INTERP、DSO。

正式 `/sbin/contest-run` 是 `userland/contest-run` 的 Rust std 静态程序，固定 `nightly-2025-01-18`；Make 使用 PATH 中已准备的 Cargo/Rust，不传 `+toolchain`、不自动安装，真实 Linux musl 目标标准库/sysroot 和交叉链接器须事先具备。当前 Cargo 正式生成的 Cargo.lock 与17包 vendor 是独立离线输入，其中锁定 `toml_edit 0.22.20`。**下面的动态运行库输出不是 RUNNER_SYSROOT，也不用于替代 runner 的目标 std。** runner 自身静态与测试自身动态可以同时成立；内核裸机工具链和用户 Linux target 不混用，普通 Make 不依赖 Nix。依赖固定与资产准备不代表 P01–P18 已验收。

## 官方测试盘与身份

官方[测试仓库说明](https://github.com/oscomp/testsuits-for-oskernel/blob/pre-2025/README.md)描述整盘 ext4、无分区表及辅助根 FS。当前选用的 [pre-20250615 Release](https://api.github.com/repos/oscomp/testsuits-for-oskernel/releases/tags/pre-20250615) 资产为：

| 架构 | 官方资产 | 发布的压缩包 SHA-256 |
|---|---|---|
| riscv64 | [sdcard-rv.img.xz](https://github.com/oscomp/testsuits-for-oskernel/releases/download/pre-20250615/sdcard-rv.img.xz) | `2b59aee4d26681b2d78a8faa615ac216f723a8ea157544e7867bbc8c77aac087` |
| loongarch64 | [sdcard-la.img.xz](https://github.com/oscomp/testsuits-for-oskernel/releases/download/pre-20250615/sdcard-la.img.xz) | `3cbeae1095d788cd8255e4929bcc6dace96dbe4999b6eaec003ae6fe2cdba42b` |

核版本、资产名与压缩包摘要后解包为 raw 整盘；运行 CLI 的 `--contest-image` 接匹配架构的 raw，不接 `.xz`。解包不是制盘，不使用 `create-ext4-image`/`mke2fs -d` 重制评分输入。是否包含某个 ELF、runtest、Shell、loader 或 DSO，必须读实际盘面；Release 名称和当前默认 plan 不证明它们一定存在或能运行。

本地宿主入口只把原盘复制为当前工作盘再交 QEMU。此前名为 original/prepared 的本地重制或注入盘仍保原身份、SHA、日志和失败，只作线索，不改称官方输入或重签成绩。固定评测如何提供可写盘按其真实合同，不把本地副本保护外推成 guest 能保护任意宿主输入。

## supplied-first 与明确准备

当前 Rust runner 默认使用所选 root 自带的原 ELF、PT_INTERP 路径、loader、DSO 和脚本，不自动执行旧 Shell 的 prepare_family，不因名字为 musl/glibc 就覆盖文件。仅设置 LD_LIBRARY_PATH 不能使缺失 PT_INTERP 可用；缺失保留真实 exec 错误，不改成支持性 Skip。

`prepare_runtime_libraries.py` 只在宿主准备下面的辅助运行库资产，`make prepare-guest-runtime-libraries` 的 `RUNTIME_LIBRARIES_OUTPUT_DIR` 指定输出，`assemble-root-filesystem` 将其装入系统根 `/opt/runtime/{musl,glibc}`，**不因此自动补进比赛根**。确需补充旧输入时，在明确选择的 `roots.NAME.prepare` 中处理；该命令运行于父系统根，参数明确指向 `/mnt/contest/...`。case.setup/cleanup 则在该 case 根执行。新输入已有匹配 loader/DSO 时直接使用，不隐式替换。

建立 PT_INTERP 要求的路径/链接、明确补库或明确 exec loader 是不同准备方式。直接 loader 可能改变 argv/auxv，不称为透明等价；替换已有内容必须显式说明。对自有非评分根可作这些实验；评分输入不得注入/替换程序、脚本、库、链接或断言后仍称原盘得分。显式准备后的身份与结果应单独说明。


## 四组固定路径与真实请求

以下容器内路径也是脚本按架构选择的默认来源：

| 架构与族 | 容器内来源 | 原动态 ELF 请求的解释器 |
|---|---|---|
| RV musl | `/opt/riscv64--musl--bleeding-edge-2020.08-1/riscv64-buildroot-linux-musl/sysroot/lib/libc.so` | `/lib/ld-musl-riscv64-sf.so.1` |
| RV glibc | `/usr/riscv64-linux-gnu/lib` | `/lib/ld-linux-riscv64-lp64d.so.1` |
| LA musl | `/opt/loongarch64-linux-musl-cross/loongarch64-linux-musl/lib/libc.so` | `/lib64/ld-musl-loongarch-lp64d.so.1` |
| LA glibc | `/opt/gcc-13.2.0-loongarch64-linux-gnu/sysroot/usr/lib64` | `/lib64/ld-linux-loongarch-lp64d.so.1` |

musl 的原程序请求 `libc.so`；其解释器链接直接指向同一份 `libc.so`。glibc 必需来源为 `libc.so.6`、`libm.so.6` 和该架构表中同名的 `ld-linux-*.so.1`；RV 原程序还显式依赖该 loader。库文件按来源字节复制，不改 musl `libc.so` 内容，不用静态库替代动态库。

## 显式本地来源

宿主先从上述固定容器提取本架构的库，再把实际提取路径交给脚本。每个架构使用独立输出目录。例如，本地提取树为 `/srv/contest-runtime/<arch>/musl/libc.so` 和 `/srv/contest-runtime/<arch>/glibc/lib/` 时：

```sh
python3 userland/runtime/prepare_runtime_libraries.py --arch riscv64 \
  --musl-libc /srv/contest-runtime/riscv64/musl/libc.so \
  --glibc-libdir /srv/contest-runtime/riscv64/glibc/lib \
  --output-directory build/runtime-riscv64
python3 userland/runtime/prepare_runtime_libraries.py --arch loongarch64 \
  --musl-libc /srv/contest-runtime/loongarch64/musl/libc.so \
  --glibc-libdir /srv/contest-runtime/loongarch64/glibc/lib \
  --output-directory build/runtime-loongarch64
```

`--arch` 和 `--output-directory` 必须显式给出，长参数不接受缩写；省略两个来源选项时只使用表中对应路径，缺失直接失败。宿主 `PATH` 必须提供支持两架构 ELF 的 `readelf`。脚本先检查全部必需来源，再对每个来源实际运行 `readelf -h -l`，要求机器为 `RISC-V` 或 `LoongArch`、类型为 `DYN`；stdout/stderr 保留工具原始输出。机器错误、工具失败、缺库或普通复制/链接错误均返回非零状态，旧输出不能替代本次来源检查。

## 输出与更新责任

输出为普通用户树：`DIR/musl/lib/libc.so` 及表中 musl loader 相对链接，`DIR/glibc/lib/{libc.so.6,libm.so.6,对应 glibc loader}`。LA 两族各有 `lib64 -> lib`，让原 `/lib64/...` 请求沿普通文件路径解析。`assemble-root-filesystem` 消费这两族目录并重新装配项目拥有的根树；`create-ext4-image` 只消费完整根树，不准备运行库，不增加内核路径替换或第二个运行库管理器。

重复执行只替换上述四个具名库文件、当前架构的 musl loader 链接和 LA 的两个 `lib64` 链接；不递归删除输出目录、不清理其他文件。库复制先写同目录临时文件再替换目标，失败清理该临时文件。具名链接位置若已有目录则失败并保留目录；输出中断后可能保留部分本次文件，调用者必须以本次退出状态判定成功，不能凭残留文件继续 staging。

这里只准备运行库资产。CLI 成功、来源 ELF 检查和普通链接成立均不代表动态程序已在 guest 运行通过，不将资产准备结果当作原程序结果。

## 原输入与现有失败边界

固定 pre-2025 的结果适配保留原脚本、ELF、DSO 与结果协议，不修测试来取得成功；原 Shell selector 不是当前正式入口。历史 basic 的 vfat/`/dev/vda2` 请求与整盘 ext4 不符，hwclock 缺用户 RTC 设备 ABI，保留原失败；此前 basic mount/umount 暂缓项仍非 Pass。RV GNU static clock 原测试要求成功后 `errno == 0`；历史现场 clock syscall 返回0，先前可选 getrandom 的 ENOSYS 留在 errno，不能由内核清 errno 或伪造随机成功。这是原输入/现场事实，不因新 runner 或重跑自动覆盖。

当前 WSL 特殊127路由曾使大 UDP 未交到宿主 peer；六合法出站分片不等于端到端成功。临时非特权 user/net namespace 的标准 lo 上，双架构17/0/1472/8193字节全文收发已有成功实据；该环境不属于公共 run 的 fallback 或宿主/QEMU 整改。LA NX 运行仍暂缓非 Pass，AT_RANDOM 仅教学非加密字节；不扩 RTC ioctl、VFAT/分区、journal/recovery、PTY/namespace 或全部 ABI。保留原失败与未运行项，不杜撰新测试全部通过或实体板已经验证。
