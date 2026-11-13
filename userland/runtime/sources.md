# 运行库与原测试输入来源

关于日常构建、三类测试体系、评测计划配置与工作盘管理命令，请参阅 [公共操作指南](../../docs/development.md)；关于当前源码模块结构与单次命令运行方式，请参阅 [运行根导读](development.md)。本文档专门说明动态运行库资产的准备流程、官方比赛测试盘的版本与哈希身份，以及历史测试输入的已知边界。运行库准备逻辑由同目录下的脚本 [prepare_runtime_libraries.py](prepare_runtime_libraries.py) 实现（注意：在宿主机上成功准备运行库资产，并不等同于客体系统内的测试已经运行通过）。

比赛测试输入以 [oscomp/testsuits-for-oskernel（提交 `8b58dd16d26d30f7c74d48d5832d870d3051b703`）](https://github.com/oscomp/testsuits-for-oskernel/tree/8b58dd16d26d30f7c74d48d5832d870d3051b703) 为基准。系统盘不再复制整套 musl 和 glibc：`prepare_runtime_libraries.py` 从对应架构的 GNU 交叉编译器取得固定身份的 `libgcc_s.so.1`，结合仓内 locale 与测试根运行库身份记录生成辅助资产；不搜索其他版本、不下载库，也不重编或修改原始测试 ELF。

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

`prepare_runtime_libraries.py` 在宿主机侧准备 libgcc、locale 和测试根运行库身份记录，经 `assemble-root-filesystem` 安装到系统根的 `/opt/runtime/glibc`。默认计划在激活 glibc 测试根时运行 `/sbin/prepare-glibc-runtime {test_root}`：仅在工作副本缺少 `lib/libgcc_s.so.1` 或对应架构的 `C.utf8` 数据时补入；补入前将测试根的 libc 和加载器 SHA-256 与系统盘记录的固定身份比较。已有普通文件、目录及分别指向它们的有效链接保留，不校验已有目标的 ABI 或数据内容；失效链接或需要补入时身份不匹配会使准备失败。

区分宿主侧制备与客体内兼容修补：上面的 `prepare_runtime_libraries.py` 只在宿主机上原样复制库文件到系统根的辅助目录，从不触碰比赛盘；LoongArch 的 `cyclictest:musl:script` 例则在客体内由 override 的 `setup`（`root = "system"`，执行系统盘的 `/sbin/prepare-la-musl-sched`）修补比赛盘工作副本里已知版本 musl `libc.so` 的 `sched_getparam`、`sched_getscheduler`、`sched_setparam`、`sched_setscheduler` 四个函数。两者互不调用，后者不属于 supplied-first 的“原样使用盘内库”：它是显式的 prepared 输入变更，只接受原始 SHA-256 `816cff1d1abbef3f1423bbce01a56f00c97b6ff8e20d49966c9d4b6d27e5e7be`、产出 `09b0fc7cd720c1f5dc25399733c1d203389a2de9ca90f6effe1c394c66414438`；原比赛镜像不变，fresh 工作副本中该库的字节、语义和 inode 会变；库版本未知则该例 setup 失败，不运行原脚本。赛事是否允许修改工作盘运行库尚未确定，边界见[显式执行根手册第 10 节](../../docs/contest-run-roots.md)。

在调试动态链接程序时，请注意区分三种不同的准备方式：按 `PT_INTERP` 要求的路径建立符号链接、显式补充缺失的共享库文件、或者直接将动态链接器（loader）作为主程序调用以加载目标二进制。其中，直接调用 loader 启动程序会改变子进程看到的 `argv[0]` 与辅助向量（`auxv`），并不完全等价于内核直接加载动态 ELF。在项目自有的非评分测试根中可以进行上述实验；但对于官方比赛评分盘，切勿向盘内注入或替换程序、脚本、共享库、符号链接或测试断言后再将其当作原始测试盘成绩。凡经过显式环境准备的步骤，其输入状态与运行结果均应明确标注。

## 运行库来源与身份

比赛盘的动态程序使用测试根自己的 libc 和 `PT_INTERP` 加载器。RISC-V musl 程序请求 `/lib/ld-musl-riscv64-sf.so.1`，glibc 程序请求 `/lib/ld-linux-riscv64-lp64d.so.1`；LoongArch 对应 `/lib64/ld-musl-loongarch-lp64d.so.1` 和 `/lib64/ld-linux-loongarch-lp64d.so.1`。系统盘不提供这些 libc、libm 或加载器；依赖原系统盘 `/opt/runtime/musl` 或完整 `/opt/runtime/glibc` 的手动用法不再适用。

| 架构 | 默认 libgcc 来源 | 固定 libgcc SHA-256 |
|---|---|---|
| RISC-V | `riscv64-linux-gnu-gcc -print-file-name=libgcc_s.so.1` | `e217643d8315bddf37281cc8bf048ceaf39ad25fd82521a47876164010f05372` |
| LoongArch | `loongarch64-linux-gnu-gcc -print-file-name=libgcc_s.so.1` | `767f7804e011b0111f542c0ce6b86e5eb78ce440d3540b36f3bb41254ca53743` |

宿主准备入口为 `python3 userland/runtime/prepare_runtime_libraries.py --arch ARCH --output-directory DIR`。可用 `--libgcc-s`、`--glibc-locale-dir` 指定同一组资产的其他路径，但文件或 locale 清单的 SHA-256 必须与固定身份一致；未知版本直接失败。`--musl-libc` 与 `--glibc-libdir` 已删除，不再支持任意 libc 来源。

脚本在写出前核对 libgcc 的 SHA-256、ELF 类型、目标架构、浮点 ABI、SONAME、依赖及 unwind 导出，并核对 locale 的 12 个类别和完整清单。系统盘身份记录将测试根的 libc、加载器与这组资产绑定。此前对任意显式 glibc provider 的逐符号检查不再作为构建入口；新增组合前必须用实际 libc 与加载器重新检查符号并运行客体验证。静态检查不能代替实际的 `setlocale`、动态加载与线程退出验证。

| 架构 | 测试根 libc SHA-256 | 测试根加载器 SHA-256 | locale 清单 SHA-256 |
|---|---|---|---|
| RISC-V | `81af558241962fadf1d0199171be78dd2bb1e45098b8f7f786b22881b7bab33c` | `10ac4a073b52a6b05e383ae6b47be92375436174a857fd6299faea1257495eaa` | `a4e5d1d829d11772d53e0a889ee1424c3659eecc712844fb1d5e6ede0e5dd993` |
| LoongArch | `b0622d832bf9531bf9d7ada63a04b084f6082f76673e0d16b3dc058c82d4aeb8` | `5201d1507ebcdaebc2ec2e728c68ba2fdca503b9e1e7a95cc177ce576ebdc2bb` | `5360b639bb89d4649bcaabb5ff84dc951c1d325daf943672a67702dddde28794` |

locale 清单按相对路径排序，每行由文件 SHA-256、两个空格、相对路径及换行组成，再对完整 ASCII 清单计算 SHA-256。

## locale 数据来源

以下为仓内 locale 数据的历史来源；当前构建不依赖旧镜像。
历史生成输入为官方容器镜像 `docker.educg.net/cg/os-contest:20250624`，当时检查的镜像 digest 为 `sha256:2fd432100bd2bc2cf8647d67b887ef1bb7300f2843f87c7658ce97ac3dac2ad5`。RV 目录直接取该镜像 `/usr/lib/locale/C.utf8`；对应 Ubuntu glibc 源包的 [原始归档](https://launchpad.net/ubuntu/+archive/primary/+sourcefiles/glibc/2.35-0ubuntu3.10/glibc_2.35.orig.tar.xz) SHA-256 为 `5123732f6b67ccd319305efd399971d58592122bcc2a6518a1bd2510dd0cf52e`，[发行版补丁归档](https://launchpad.net/ubuntu/+archive/primary/+sourcefiles/glibc/2.35-0ubuntu3.10/glibc_2.35-0ubuntu3.10.debian.tar.xz) 为 `c6d111273ddb76464035493a1c2cb56d10626ce03053d590cb10af48b543704d`。

LA 数据由 [GCC 13.2.0 工具链归档](https://gitlab.educg.net/wangmingjian/os-contest-2024-image/-/raw/aa2122f118f9d50e6c378000bd2a102915c8b87c/gcc-13.2.0-loongarch64-linux-gnu.tgz)的目标 `localedef` 生成；归档 SHA-256 为 `1acd6c2ae79d6e8c80ea6d5b20d6ee3d6f52944bc473c833047aa717e05a0380`，实际使用的 `localedef` SHA-256 为 `f98a8e29753878dfad3b04aa23fdc36b0e432917e41f1b6d4b2b58fa474e2c5a`。生成环境中的命令为：

下列命令是当时的生成记录；其中的系统加载器路径不再由当前系统盘提供。

```sh
I18NPATH=/opt/locale-tools/i18n /opt/runtime/glibc/lib/ld-linux-loongarch-lp64d.so.1 \
  --library-path /opt/runtime/glibc/lib /opt/locale-tools/localedef \
  --no-archive --little-endian -i C -f /opt/locale-tools/i18n/charmaps/UTF-8 \
  /root/locale-output/C.utf8
```

这条命令在已准备的 LA 客体中实际返回 0；工具链的精确 glibc 补丁源码和独立逐字节重生成尚未确认。源数据中的 Unicode 版本说明存在 14 与 15 的差异，不将其统一写成某个已证实版本。两架构各 12 份 locale 源文件及压缩字符映射位于 `userland/runtime/locale/source/`；`UTF-8.gz` 解压后的 SHA-256 分别为 RV `591deb94b0bea99591001cb74ab8083e557d424e57ee4494ef1a6b2c6a8093b6`、LA `baaeac2ee25a437edaaaf2a2d5a7ae2a62fc021fc2a9d908490ba40114bdad0b`。glibc、Ubuntu 和 Unicode 原始许可材料位于 `userland/runtime/locale/licenses/`；许可证适用范围不由本记录代作法律结论。

## 输出与更新责任

准备脚本输出 `DIR/glibc/lib/libgcc_s.so.1`、`DIR/glibc/identity` 和对应架构的 `DIR/glibc/usr/lib{,64}/locale/C.utf8`。`identity` 一行依次保存测试根 libc SHA-256、加载器 SHA-256 与加载器文件名，由空格分隔。

`assemble-root-filesystem` 将这些资产装入系统根 `/opt/runtime/glibc`；`create-ext4-image` 只负责打包完整根目录，不读取比赛盘。单文件与 locale 更新沿各自的暂存和发布路径；多项输出不构成整体事务，调用者必须以当次退出码判断是否可以继续打包。

## 原输入与现有失败边界

执行器不解析、不改写比赛输出，也不判定通过与否。默认计划对固定 `pre-2025` 工作盘显式准备解释器、同盘 loader、BusyBox 命令，以及 glibc 缺失的 libgcc 和 locale；iperf 直接执行盘内原脚本，cleanup 用比赛根内 BusyBox 的 `killall`/`pidof` 结束 `-D` 守护服务端并等待其退出。此外，LoongArch 的 `cyclictest:musl:script` override 会修补工作副本里已知版本 musl `libc.so` 的四个调度函数，赛事许可未定。原始镜像保持不变，但这些运行使用经过准备的输入，不能作为完全原样运行官方盘的成绩；具体命令见 [比赛执行器手册](../../docs/contest-run.md)，兼容修补的边界见[显式执行根手册](../../docs/contest-run-roots.md)。在已知的测试输入边界中：
- 历史 `basic` 测试集中针对 `vfat` 文件系统与 `/dev/vda2` 分区的挂载请求，与官方提供的无分区表整盘 `ext4` 镜像格式不匹配；`hwclock` 用例则依赖尚未实现的用户态 RTC 设备 ABI，因此这两类用例如实保留原始失败状态（早期 `basic` 中的 `mount`/`umount` 暂缓项同样保持非 Pass 记录）。
- RISC-V 架构下 `gnu` 静态链接的 `clock` 原始测试程序在调用成功后会额外断言 `errno == 0`：在历史运行现场中，虽然 `clock` 相关系统调用本身成功返回了 `0`，但此前 C 运行库初始化阶段调用可选系统调用 `getrandom` 返回的 `ENOSYS` 仍保留在线程局部的 `errno` 中；内核绝不能违反 POSIX 规范在系统调用成功时主动清零用户态 `errno`，也不能返回伪造的硬件随机数去掩盖 `ENOSYS`。这一现象属于原始测试二进制与标准 ABI 交互的真实边界，不会因为更换执行器或重跑测试而自动消失。
- 在网络测试方面，当前宿主机 WSL 环境特殊的 `127.0.0.0/8` 回环路由行为曾导致大体量 UDP 报文未能完整送达宿主机侧对端进程（客体内核协议栈成功发出 6 个合法的 IP 分片，并不等同于宿主机端到端接收成功）。在临时创建的非特权 user/net 命名空间标准 `lo` 接口上，双架构 `17`、`0`、`1472`、`8193` 字节的 UDP 报文全文收发均已实测通过；但该隔离命名空间仅用于定位问题，不作为日常公共运行入口的自动回退方案。
- LoongArch 的不可执行页保护（NX）目前仍属暂缓项（不计为 Pass），`AT_RANDOM` 辅助向量仅为教学用途的非加密字节；当前项目不扩展用户态 RTC `ioctl`、VFAT/MBR 分区挂载、ext4 日志恢复（journal/recovery）、伪终端（PTY）/命名空间（namespace）或其余未列入范围的 Linux ABI。所有未运行项与已知限制均如实记录。
