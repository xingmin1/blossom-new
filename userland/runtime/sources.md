# 比赛运行库来源与准备

当前架构、根布局、build/image/run/RESUME 与客体命令入口见[开发导读](development.md)。本文只说明运行库来源、准备责任和原输入的失败边界；实现为同目录的 [prepare.py](prepare.py)。

运行库与原测试固定采用 [oscomp/testsuits-for-oskernel 的 8b58dd16d26d30f7c74d48d5832d870d3051b703](https://github.com/oscomp/testsuits-for-oskernel/tree/8b58dd16d26d30f7c74d48d5832d870d3051b703) 和 `docker.educg.net/cg/os-contest:20250624`。`prepare.py` 只复制明确的本地来源，不搜索其他版本、不下载或安装工具链，也不重编或修改原测试 ELF、PT_INTERP、DSO。

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
python3 userland/runtime/prepare.py --arch riscv64 \
  --musl-libc /srv/contest-runtime/riscv64/musl/libc.so \
  --glibc-libdir /srv/contest-runtime/riscv64/glibc/lib \
  --output build/runtime-riscv64
python3 userland/runtime/prepare.py --arch loongarch64 \
  --musl-libc /srv/contest-runtime/loongarch64/musl/libc.so \
  --glibc-libdir /srv/contest-runtime/loongarch64/glibc/lib \
  --output build/runtime-loongarch64
```

`--arch` 和 `--output` 必须显式给出；省略两个来源选项时只使用表中对应路径，缺失直接失败。宿主 `PATH` 必须提供支持两架构 ELF 的 `readelf`。脚本先检查全部必需来源，再对每个来源实际运行 `readelf -h -l`，要求机器为 `RISC-V` 或 `LoongArch`、类型为 `DYN`；stdout/stderr 保留工具原始输出。机器错误、工具失败、缺库或普通复制/链接错误均返回非零状态，旧输出不能替代本次来源检查。

## 输出与更新责任

输出为普通用户树：`DIR/musl/lib/libc.so` 及表中 musl loader 相对链接，`DIR/glibc/lib/{libc.so.6,libm.so.6,对应 glibc loader}`。LA 两族各有 `lib64 -> lib`，让原 `/lib64/...` 请求沿普通文件路径解析。后续单一 Make/staging 消费这两族目录，不增加内核路径替换或第二个运行库管理器。

重复执行只替换上述四个具名库文件、当前架构的 musl loader 链接和 LA 的两个 `lib64` 链接；不递归删除输出目录、不清理其他文件。库复制先写同目录临时文件再替换目标，失败清理该临时文件。具名链接位置若已有目录则失败并保留目录；输出中断后可能保留部分本次文件，调用者必须以本次退出状态判定成功，不能凭残留文件继续 staging。

这里只准备运行库资产。CLI 成功、来源 ELF 检查和普通链接成立均不代表动态程序已在 guest 运行通过，不将资产准备结果当作原程序结果。

## 原输入与现有失败边界

固定 selector 保留原脚本、ELF、DSO 与结果协议，不修补测试以取得成功。原 basic 的 vfat/`/dev/vda2` 请求与整盘 ext4 不符，hwclock 缺用户 RTC 设备 ABI，仍真失败。RV GNU static clock 原测试要求成功后 `errno == 0`；实际 clock syscall 返回0，先前可选 getrandom 的 ENOSYS 保留，不能由内核清 errno 或伪造随机成功。

当前 WSL 特殊127路由曾使大 UDP 未交到宿主 peer；六合法出站分片不等于端到端成功。临时非特权 user/net namespace 的标准 lo 上，双架构17/0/1472/8193字节全文收发已有成功实据；该环境不属于公共 run 的 fallback 或宿主/QEMU 整改。LA NX 运行仍暂缓非 Pass，AT_RANDOM 仅教学非加密字节；不扩 RTC ioctl、VFAT/分区、journal/recovery、PTY/namespace 或全部 ABI。
