# 运行库与原测试输入来源

关于日常构建、三类测试体系、评测计划配置与工作盘管理命令，请参阅 [公共操作指南](../../docs/development.md)；关于当前源码模块结构与单次命令运行方式，请参阅 [运行根导读](development.md)。本文档专门说明动态运行库资产的准备流程、官方比赛测试盘的版本与哈希身份，以及历史测试输入的已知边界。运行库准备逻辑由同目录下的脚本 [prepare_runtime_libraries.py](prepare_runtime_libraries.py) 实现（注意：在宿主机上成功准备运行库资产，并不等同于客体系统内的测试已经运行通过）。

运行库与原始测试程序固定采用官方仓库 [oscomp/testsuits-for-oskernel（提交 `8b58dd16d26d30f7c74d48d5832d870d3051b703`）](https://github.com/oscomp/testsuits-for-oskernel/tree/8b58dd16d26d30f7c74d48d5832d870d3051b703) 以及官方评测容器 `docker.educg.net/cg/os-contest:20250624`。`prepare_runtime_libraries.py` 从固定容器路径或显式指定的本地路径复制库，默认使用仓内按 libc 身份绑定的 locale；不会搜索其他版本、联网下载或安装工具链，也不会重新编译或修改原始测试的 ELF、`PT_INTERP` 或共享库。

正式的比赛执行器 `/sbin/contest-run` 是由 `userland/contest-run` 编译生成的 Rust `std` 静态链接程序，固定使用 `nightly-2025-01-18` 工具链；Makefile 直接调用 `PATH` 中已就绪的 `cargo`/`rustc`（不传 `+toolchain` 参数，也不启用自动安装），因此对应架构的 Linux musl 目标标准库（sysroot）与交叉链接器必须事先准备就绪。`userland/contest-run` 目录下的 `Cargo.lock` 与包含 17 个依赖包的 `vendor/` 目录是由 Cargo 正式生成的离线输入（其中锁定了 `toml_edit 0.22.20`）。**请特别注意：本文下方准备的客体动态运行库输出目录并非 `RUNNER_SYSROOT`，不能用来替代编译 `contest-run` 所需的 Rust 目标标准库。** `contest-run` 自身采用静态链接，与被测程序采用动态链接完全互不冲突；内核的裸机编译 target 与用户态 Linux target 各司其职，日常 Make 构建也不依赖 Nix 环境。

## 官方测试盘与身份

官方 [测试仓库说明文档](https://github.com/oscomp/testsuits-for-oskernel/blob/pre-2025/README.md) 描述了整盘无分区表的 `ext4` 比赛镜像结构。当前项目固定选用 [pre-20250615 Release](https://api.github.com/repos/oscomp/testsuits-for-oskernel/releases/tags/pre-20250615) 发布的两张镜像资产：

| 架构 | 官方发布资产 | 发布压缩包 SHA-256 摘要 |
|---|---|---|
| `riscv64` | [sdcard-rv.img.xz](https://github.com/oscomp/testsuits-for-oskernel/releases/download/pre-20250615/sdcard-rv.img.xz) | `2b59aee4d26681b2d78a8faa615ac216f723a8ea157544e7867bbc8c77aac087` |
| `loongarch64` | [sdcard-la.img.xz](https://github.com/oscomp/testsuits-for-oskernel/releases/download/pre-20250615/sdcard-la.img.xz) | `3cbeae1095d788cd8255e4929bcc6dace96dbe4999b6eaec003ae6fe2cdba42b` |

下载后请先校验 Release 版本、文件名及 `.xz` 压缩包的 SHA-256 摘要，随后解压得到未压缩的 raw 整盘镜像；QEMU 启动脚本的 `--contest-image` 参数只接受解压后的匹配架构 raw 镜像，不直接接受 `.xz` 压缩文件。注意：解压官方镜像并非重新制盘，严禁使用 `create-ext4-image` 或 `mke2fs -d` 重新打包生成比赛评分盘。镜像内部实际包含哪些测试 ELF、`runtest`、Shell 解释器、动态链接器（loader）或共享库（DSO），均以挂载后的真实盘面内容为准（不能仅凭 Release 名称或默认测试计划就假定某个文件必然存在）。

宿主机 QEMU 启动入口默认使用 `fresh`：将原始比赛盘复制为工作目录中的工作副本（`contest.raw`）再交给 QEMU 挂载；`reuse` 和 `refresh-system` 保留已有比赛工作副本。原镜像不被该流程写入，但 runner 不提供写入隔离，不能把原镜像直接映射成可写设备。早期调试阶段曾使用过的名为 `original` 或 `prepared` 的本地重制盘或注入盘，仅保留其各自的哈希与历史日志作为排障参考，不作为官方原始测试盘统计成绩。

## supplied-first 与明确准备

当前的 Rust 比赛执行器默认遵循“优先使用测试盘自带环境（supplied-first）”原则：直接使用所选子根目录（root）内自带的原始 ELF、`PT_INTERP` 解释器路径、动态链接器（loader）、共享库（DSO）与测试脚本，不再像早期 Shell 脚本那样自动执行 `prepare_family`，也不会仅因为子目录名为 `musl` 或 `glibc` 就去覆盖盘内已有的库文件。需要注意，当 ELF 的 `PT_INTERP` 动态链接器本身缺失时，单纯设置 `LD_LIBRARY_PATH` 环境变量并不能让程序启动；此时执行器会如实记录 `exec` 启动失败，而不会将其归类为支持性跳过（Skip）。

`prepare_runtime_libraries.py` 在宿主机侧整理运行库和 locale 资产：由 `make prepare-guest-runtime-libraries` 通过 `RUNTIME_LIBRARIES_OUTPUT_DIR` 指定输出目录，再由 `assemble-root-filesystem` 安装到系统根的 `/opt/runtime/{musl,glibc}`。这一步不修改比赛盘。默认计划在激活 glibc 测试根时，显式运行系统根的 `/sbin/prepare-glibc-runtime {test_root}`：仅在工作副本缺少 `lib/libgcc_s.so.1` 或对应架构的 `C.utf8` 数据时补入，并在需要补入时比较系统根与测试根的 `libc.so.6` SHA-256。已有普通文件、目录及分别指向它们的有效链接保留，不校验已有目标的 ABI 或数据内容；失效链接或补入时的 libc 不匹配会使准备失败。

区分宿主侧制备与客体内兼容修补：上面的 `prepare_runtime_libraries.py` 只在宿主机上原样复制库文件到系统根的辅助目录，从不触碰比赛盘；LoongArch 的 `cyclictest:musl:script` 例则在客体内由 override 的 `setup`（`root = "system"`，执行系统盘的 `/sbin/prepare-la-musl-sched`）修补比赛盘工作副本里已知版本 musl `libc.so` 的 `sched_getparam`、`sched_getscheduler`、`sched_setparam`、`sched_setscheduler` 四个函数。两者互不调用，后者不属于 supplied-first 的“原样使用盘内库”：它是显式的 prepared 输入变更，只接受原始 SHA-256 `816cff1d1abbef3f1423bbce01a56f00c97b6ff8e20d49966c9d4b6d27e5e7be`、产出 `09b0fc7cd720c1f5dc25399733c1d203389a2de9ca90f6effe1c394c66414438`；原比赛镜像不变，fresh 工作副本中该库的字节、语义和 inode 会变；库版本未知则该例 setup 失败，不运行原脚本。赛事是否允许修改工作盘运行库尚未确定，边界见[显式执行根手册第 10 节](../../docs/contest-run-roots.md)。

在调试动态链接程序时，请注意区分三种不同的准备方式：按 `PT_INTERP` 要求的路径建立符号链接、显式补充缺失的共享库文件、或者直接将动态链接器（loader）作为主程序调用以加载目标二进制。其中，直接调用 loader 启动程序会改变子进程看到的 `argv[0]` 与辅助向量（`auxv`），并不完全等价于内核直接加载动态 ELF。在项目自有的非评分测试根中可以进行上述实验；但对于官方比赛评分盘，切勿向盘内注入或替换程序、脚本、共享库、符号链接或测试断言后再将其当作原始测试盘成绩。凡经过显式环境准备的步骤，其输入状态与运行结果均应明确标注。

## 四组固定路径与真实请求

下表列出了官方评测容器内的四组运行库路径（同时也是 `prepare_runtime_libraries.py` 在未传入显式路径参数时按架构查找的默认源路径），以及比赛盘内原始动态 ELF 程序头（`PT_INTERP`）所请求的动态链接器路径：

| 架构与 libc 族 | 官方容器内源文件路径 | 原始动态 ELF 请求的 `PT_INTERP` 解释器路径 |
|---|---|---|
| RV `musl` | `/opt/riscv64--musl--bleeding-edge-2020.08-1/riscv64-buildroot-linux-musl/sysroot/lib/libc.so` | `/lib/ld-musl-riscv64-sf.so.1` |
| RV `glibc` | `/usr/riscv64-linux-gnu/lib` | `/lib/ld-linux-riscv64-lp64d.so.1` |
| LA `musl` | `/opt/loongarch64-linux-musl-cross/loongarch64-linux-musl/lib/libc.so` | `/lib64/ld-musl-loongarch-lp64d.so.1` |
| LA `glibc` | `/opt/gcc-13.2.0-loongarch64-linux-gnu/sysroot/usr/lib64` | `/lib64/ld-linux-loongarch-lp64d.so.1` |

在 `musl` 族中，原始动态程序依赖 `libc.so`，且 `PT_INTERP` 解释器符号链接直接指向同一份 `libc.so`。在 `glibc` 族中，辅助资产包括 `libc.so.6`、`libm.so.6`、对应架构的 `ld-linux-*.so.1`、`libgcc_s.so.1` 和完整的 `C.utf8` 目录。libgcc 与 locale 必须来自与所选 glibc 兼容的工具链或运行环境；脚本原样复制，不修改共享库字节。RISC-V 原始动态程序还在动态段中显式声明了对 glibc loader 的依赖。

## 显式本地来源

若不使用官方容器中的固定运行库，可显式指定本地来源。内置 `C.utf8` 只接受下方记录的官方 libc SHA-256；其他 libc 必须显式提供兼容的 locale。以下路径是示例：

```sh
python3 userland/runtime/prepare_runtime_libraries.py --arch riscv64 \
  --musl-libc /srv/contest-runtime/riscv64/musl/libc.so \
  --glibc-libdir /srv/contest-runtime/riscv64/glibc/lib \
  --libgcc-s /srv/contest-runtime/riscv64/glibc/libgcc_s.so.1 \
  --glibc-locale-dir /srv/contest-runtime/riscv64/glibc/C.utf8 \
  --output-directory build/runtime-riscv64
python3 userland/runtime/prepare_runtime_libraries.py --arch loongarch64 \
  --musl-libc /srv/contest-runtime/loongarch64/musl/libc.so \
  --glibc-libdir /srv/contest-runtime/loongarch64/glibc/lib \
  --libgcc-s /srv/contest-runtime/loongarch64/glibc/libgcc_s.so.1 \
  --glibc-locale-dir /srv/contest-runtime/loongarch64/glibc/C.utf8 \
  --output-directory build/runtime-loongarch64
```

命令行中的 `--arch` 与 `--output-directory` 必填，所有长选项均不接受缩写。省略 `--musl-libc` 或 `--glibc-libdir` 时使用上一节固定容器路径；未显式指定 glibc 目录和 libgcc 时，libgcc 使用各架构的固定官方路径，其中 LoongArch 为 `/opt/gcc-13.2.0-loongarch64-linux-gnu/loongarch64-linux-gnu/lib64/libgcc_s.so.1`。显式指定 glibc 目录而不指定 libgcc 时，只读取该目录下的 `libgcc_s.so.1`，不搜索其他工具链。省略 `--glibc-locale-dir` 时，脚本按架构及选中 libc 的完整 SHA-256 选择仓内 locale，并校验 12 个文件的清单散列；未知 libc 或内置数据损坏时在写出前失败。显式 locale 仍需由调用者确认兼容性。

`PATH` 中必须提供支持两架构的 `readelf`。脚本在写出前检查文件、ELF 架构与类型、libgcc 浮点 ABI、SONAME、依赖、必需的 unwind 导出和版本化 glibc 符号，并检查 12 类 locale 数据的头部和索引。来源不匹配、文件缺失或工具失败时返回非零。静态检查不能证明 locale 与 glibc 运行时兼容，也不能替代客体中的 `setlocale`、动态加载与线程退出验证。

本次定向验证使用官方评测容器内对应架构的 `libgcc_s.so.1`：RISC-V SHA-256 为 `d5c9791ca8b75d2cfaf3861f162e8406c243c192db51add9b583bae7662b108f`，LoongArch 为 `767f7804e011b0111f542c0ce6b86e5eb78ce440d3540b36f3bb41254ca53743`。内置 RV locale 来自容器 Ubuntu glibc `2.35-0ubuntu3.10` 的预制目录，原盘 libc 为 `2.35-0ubuntu3`；两者修订不同。内置 LA locale 用官方工具链 glibc 2.38 的目标 `localedef` 和字符集源数据在客体生成，并非独立重新生成验证。两次运行分别与测试根的 libc 字节核对，12 类 locale 与安装源逐字节相同。详细身份及实测边界见 `.trellis/spec/os/backend/userland.md` 的“比赛工作副本的 glibc 运行环境”。

内置数据的选择与完整性绑定如下。locale 清单按相对路径排序，每行是 `文件 SHA-256`、两个空格、相对路径及换行，再对完整 ASCII 清单计算 SHA-256：

| 架构 | 对应 libc SHA-256 | locale 清单 SHA-256 |
|---|---|---|
| RISC-V | `81af558241962fadf1d0199171be78dd2bb1e45098b8f7f786b22881b7bab33c` | `a4e5d1d829d11772d53e0a889ee1424c3659eecc712844fb1d5e6ede0e5dd993` |
| LoongArch | `b0622d832bf9531bf9d7ada63a04b084f6082f76673e0d16b3dc058c82d4aeb8` | `5360b639bb89d4649bcaabb5ff84dc951c1d325daf943672a67702dddde28794` |

两套数据的固定输入为官方容器镜像 `docker.educg.net/cg/os-contest:20250624`，本次检查的镜像 digest 为 `sha256:2fd432100bd2bc2cf8647d67b887ef1bb7300f2843f87c7658ce97ac3dac2ad5`。RV 目录直接取该镜像 `/usr/lib/locale/C.utf8`；对应 Ubuntu glibc 源包的 [原始归档](https://launchpad.net/ubuntu/+archive/primary/+sourcefiles/glibc/2.35-0ubuntu3.10/glibc_2.35.orig.tar.xz) SHA-256 为 `5123732f6b67ccd319305efd399971d58592122bcc2a6518a1bd2510dd0cf52e`，[发行版补丁归档](https://launchpad.net/ubuntu/+archive/primary/+sourcefiles/glibc/2.35-0ubuntu3.10/glibc_2.35-0ubuntu3.10.debian.tar.xz) 为 `c6d111273ddb76464035493a1c2cb56d10626ce03053d590cb10af48b543704d`。

LA 数据由 [GCC 13.2.0 工具链归档](https://gitlab.educg.net/wangmingjian/os-contest-2024-image/-/raw/aa2122f118f9d50e6c378000bd2a102915c8b87c/gcc-13.2.0-loongarch64-linux-gnu.tgz)的目标 `localedef` 生成；归档 SHA-256 为 `1acd6c2ae79d6e8c80ea6d5b20d6ee3d6f52944bc473c833047aa717e05a0380`，实际使用的 `localedef` SHA-256 为 `f98a8e29753878dfad3b04aa23fdc36b0e432917e41f1b6d4b2b58fa474e2c5a`。生成环境中的命令为：

```sh
I18NPATH=/opt/locale-tools/i18n /opt/runtime/glibc/lib/ld-linux-loongarch-lp64d.so.1 \
  --library-path /opt/runtime/glibc/lib /opt/locale-tools/localedef \
  --no-archive --little-endian -i C -f /opt/locale-tools/i18n/charmaps/UTF-8 \
  /root/locale-output/C.utf8
```

这条命令在已准备的 LA 客体中实际返回 0；工具链的精确 glibc 补丁源码和独立逐字节重生成尚未确认。源数据中的 Unicode 版本说明存在 14 与 15 的差异，不将其统一写成某个已证实版本。两架构各 12 份 locale 源文件及压缩字符映射位于 `userland/runtime/locale/source/`；`UTF-8.gz` 解压后的 SHA-256 分别为 RV `591deb94b0bea99591001cb74ab8083e557d424e57ee4494ef1a6b2c6a8093b6`、LA `baaeac2ee25a437edaaaf2a2d5a7ae2a62fc021fc2a9d908490ba40114bdad0b`。glibc、Ubuntu 和 Unicode 原始许可材料位于 `userland/runtime/locale/licenses/`；许可证适用范围不由本记录代作法律结论。

## 输出与更新责任

脚本生成的目录结构为标准用户态文件树：
- `DIR/musl/lib/libc.so`，以及同目录下指向上表 `musl` 解释器名称的相对符号链接；
- `DIR/glibc/lib/{libc.so.6,libm.so.6,libgcc_s.so.1,对应架构的 glibc loader}`；
- `DIR/glibc/usr/lib/locale/C.utf8`（RISC-V）或 `DIR/glibc/usr/lib64/locale/C.utf8`（LoongArch）；
- 在 `loongarch64` 架构下，`musl` 与 `glibc` 子目录内还会分别创建 `lib64 -> lib` 符号链接，使原始程序请求的 `/lib64/...` 路径能够解析到 `lib/` 目录。

后续执行 `assemble-root-filesystem` 时，会将这两个运行库目录组装进项目管理的系统根文件树；而 `create-ext4-image` 仅负责将组装好的完整根文件树打包为镜像，不会触发运行库准备，也不会在内核中添加硬编码路径重写逻辑。

重复运行时，脚本更新五个具名共享库、当前架构的解释器链接和对应的 locale 目录，不递归删除输出目录中的其他文件。共享库使用同目录临时文件后替换；locale 先在同文件系统暂存，发布失败时尝试恢复旧目录，恢复也失败时保留旧目录并报告位置。多项输出不是整体事务，调用者必须以当次退出码判断成功与否，不能凭旧文件存在继续打包。

## 原输入与现有失败边界

执行器不解析、不改写比赛输出，也不判定通过与否。默认计划对固定 `pre-2025` 工作盘显式准备解释器、同盘 loader、BusyBox 命令，以及 glibc 缺失的 libgcc 和 locale；iperf 直接执行盘内原脚本，cleanup 用比赛根内 BusyBox 的 `killall`/`pidof` 结束 `-D` 守护服务端并等待其退出。此外，LoongArch 的 `cyclictest:musl:script` override 会修补工作副本里已知版本 musl `libc.so` 的四个调度函数，赛事许可未定。原始镜像保持不变，但这些运行使用经过准备的输入，不能作为完全原样运行官方盘的成绩；具体命令见 [比赛执行器手册](../../docs/contest-run.md)，兼容修补的边界见[显式执行根手册](../../docs/contest-run-roots.md)。在已知的测试输入边界中：
- 历史 `basic` 测试集中针对 `vfat` 文件系统与 `/dev/vda2` 分区的挂载请求，与官方提供的无分区表整盘 `ext4` 镜像格式不匹配；`hwclock` 用例则依赖尚未实现的用户态 RTC 设备 ABI，因此这两类用例如实保留原始失败状态（早期 `basic` 中的 `mount`/`umount` 暂缓项同样保持非 Pass 记录）。
- RISC-V 架构下 `gnu` 静态链接的 `clock` 原始测试程序在调用成功后会额外断言 `errno == 0`：在历史运行现场中，虽然 `clock` 相关系统调用本身成功返回了 `0`，但此前 C 运行库初始化阶段调用可选系统调用 `getrandom` 返回的 `ENOSYS` 仍保留在线程局部的 `errno` 中；内核绝不能违反 POSIX 规范在系统调用成功时主动清零用户态 `errno`，也不能返回伪造的硬件随机数去掩盖 `ENOSYS`。这一现象属于原始测试二进制与标准 ABI 交互的真实边界，不会因为更换执行器或重跑测试而自动消失。
- 在网络测试方面，当前宿主机 WSL 环境特殊的 `127.0.0.0/8` 回环路由行为曾导致大体量 UDP 报文未能完整送达宿主机侧对端进程（客体内核协议栈成功发出 6 个合法的 IP 分片，并不等同于宿主机端到端接收成功）。在临时创建的非特权 user/net 命名空间标准 `lo` 接口上，双架构 `17`、`0`、`1472`、`8193` 字节的 UDP 报文全文收发均已实测通过；但该隔离命名空间仅用于定位问题，不作为日常公共运行入口的自动回退方案。
- LoongArch 的不可执行页保护（NX）目前仍属暂缓项（不计为 Pass），`AT_RANDOM` 辅助向量仅为教学用途的非加密字节；当前项目不扩展用户态 RTC `ioctl`、VFAT/MBR 分区挂载、ext4 日志恢复（journal/recovery）、伪终端（PTY）/命名空间（namespace）或其余未列入范围的 Linux ABI。所有未运行项与已知限制均如实记录。
