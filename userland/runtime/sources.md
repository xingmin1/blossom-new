# 比赛运行库来源与准备

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

这里只准备运行库资产。CLI 成功、来源 ELF 检查和普通链接成立均不代表动态程序已在 guest 运行通过；双架构双 libc 的实际执行由后续内核与原程序验证记录说明。

## 当前宿主入口

`make build` 只构建普通内核；`make runtime` 只准备本架构运行库；`make image` 从明确的 `SOURCE` 或普通 staging 制盘；`make run` 构建内核并运行独占工作副本。宿主工具均由当前 `PATH` 提供，不调用 Nix、下载或比赛源码构建。

```sh
make runtime ARCH=riscv64 \
  MUSL_LIBC=build/stage9-input/original-tree-riscv64/musl/lib/libc.so \
  GLIBC_LIBDIR=build/stage9-input/original-tree-riscv64/glibc/lib \
  RUNTIME_OUTPUT=build/runtime-riscv64
make runtime ARCH=loongarch64 \
  MUSL_LIBC=build/stage9-input/original-tree-loongarch64/musl/lib/libc.so \
  GLIBC_LIBDIR=build/stage9-input/original-tree-loongarch64/glibc/lib \
  RUNTIME_OUTPUT=build/runtime-loongarch64
```

`MUSL_LIBC`、`GLIBC_LIBDIR` 留空时由 `prepare.py` 唯一定义的架构默认来源决定，缺少来源直接失败，不能靠旧输出继续。`RUNTIME_OUTPUT` 默认 `build/runtime-$(ARCH)`。首次 `make image` 且没有 `SOURCE` 或已有 staging 时，先执行 runtime 与静态 userland 构建，再复制普通 `bin/sbin/usr`、系统配置和运行库；脚本安装为 `/sbin/contest-run`，运行库为 `/opt/runtime/{musl,glibc}`，比赛挂载点为 `/mnt/contest`。已有 `SOURCE` 或 staging 不安装、不重填；需要新内容时显式选择一个新的 `STAGING`，或自行修改自己的来源树。

普通开发启动明确使用物理盘根 `ROOTDIR=/`，默认 `ROOT=/dev/vda`、`INIT=/sbin/init`，例如 `make run ARCH=riscv64 IMAGE=build/system-riscv64.img ROOTDIR=/`。要从新普通树制盘，向 `make image` 传上面的本架构来源参数。

```sh
make contest ARCH=riscv64 CPUS=1 \
  CONTEST_IMAGE=build/stage9-input/contest-original-rv.img \
  MUSL_LIBC=build/stage9-input/original-tree-riscv64/musl/lib/libc.so \
  GLIBC_LIBDIR=build/stage9-input/original-tree-riscv64/glibc/lib
make contest ARCH=riscv64 CPUS=1 \
  CONTEST_IMAGE=build/stage9-input/contest-original-rv.img RESUME=1
```

`contest` 仍由同一个 `tools/run.py` 启动，比赛工作盘固定第一块 `/dev/vda`，系统盘第二块 `/dev/vdb`，普通 PID1 保持 `/sbin/init`。默认系统镜像、staging 和工作目录分别为 `build/contest-system-ARCH.img`、`build/contest-system-ARCH`、`build/contest-run-ARCH`，与普通 run 分开；`IMAGE`、`STAGING`、`SOURCE`、`OUTPUT`、`WORK_DIR` 均可显式指定。新 contest staging 的普通 inittab 执行 `/sbin/contest-run /dev/vda`；明确 `SOURCE` 或已有 staging 的配置原样保留。`tools/run.py --contest-image PATH --image SYSTEM ...` 提供相同盘顺序，不替调用者修改系统配置。fresh 将来源复制为独立的 `contest.raw` 与 `root.raw`，只有 `RESUME=1` 或 `--resume` 使用现存副本；锁一直保留到真实 QEMU exec/退出。原比赛盘从不直接交给 QEMU 写入。

```sh
make all \
  MUSL_LIBC_RV=build/stage9-input/original-tree-riscv64/musl/lib/libc.so \
  GLIBC_LIBDIR_RV=build/stage9-input/original-tree-riscv64/glibc/lib \
  MUSL_LIBC_LA=build/stage9-input/original-tree-loongarch64/musl/lib/libc.so \
  GLIBC_LIBDIR_LA=build/stage9-input/original-tree-loongarch64/glibc/lib
```

`all` 真实依赖两份普通 abort 内核、两份静态 userland 和两架构 runtime，交叉编译器分别通过 `MUSL_CROSS_RV`、`MUSL_CROSS_LA` 指定。两份 userland 使用独立 `build/delivery-ARCH/` 构建目录，不使用普通 `SOURCE`、staging 或运行工作盘。全部依赖成功后，`mkext4.py` 在新临时树内调用同一个 `system_staging` 建 `/riscv64` 与 `/loongarch64`，两根配置均执行 `contest-run /dev/vda`；随后同一个 `system_image` 只制盘一次。成功才从本次产物导出普通 ELF `kernel-rv`、`kernel-la` 和两个独立普通文件 `disk.img`、`disk-la.img`，两盘逐字节同内容；制盘或复制失败不继续导出旧产物。

不带 append 的外部启动使用内核默认系统 `/dev/vdb`，根目录按架构取 `/riscv64` 或 `/loongarch64`；第一块为对应比赛工作副本，第二块为同一双根盘的独立工作副本。两架构已按此布局真实启动。LA 使用 PCI：前两盘分别 `addr=0x5`、`addr=0x9`，不采用上游 README 的 PCI/MMIO 总线混写示例；该局部验证不等于线上评测命令已核实。

## 客体入口与当前限制

普通 `/sbin/contest-run DEVICE [项目...]` 接受 `basic`、`busybox`、`lua`、`dynamic:NAME` 和固定表实际存在的 `static:NAME`，先校验完整列表，再按给定顺序运行 musl、glibc 两族。例如修改自己的 inittab 为 `/sbin/contest-run /dev/vda dynamic:clock_gettime dynamic:stat`，只运行这两个 selector。它保留原脚本/ELF/DSO字节、cwd、族标记和正文；`runtest` 的 `Pass!` 加返回1才是成功，BusyBox 内部 fail、失败/信号/超时和未运行保持原分类。最后真实 wait、反序卸载、sync、自然 poweroff，不实现全部 LTP/libctest 或评分平台。

默认完整列表在两架构均实际执行并自然结束：两族十个动态 selector，包括 dlopen、TLS 与 pthread；静态对照按原表。原 basic 的 vfat/`/dev/vda2` 请求与整盘 ext4 不符、hwclock 缺用户 RTC 设备 ABI，仍真失败。RV GNU static clock 原测试要求成功后 `errno == 0`；实际 clock syscall 返回0，先前可选 getrandom 的 ENOSYS保留，不能由内核清 errno 或伪造随机成功。

普通根默认RO，文件系统正常写入只通过明确RW；比赛工作盘必须RW，extra仍RO。四盘加NIC时共享宿主规则保留槽3：前三盘位置不变，第四盘RV bus.4／LA addr0x15，设备字母仍按原逻辑盘顺序。源盘、普通SOURCE与已运行工作盘不会因all被改写；恢复必须明确RESUME，当前普通与比赛同盘marker写入/同步/再次恢复均有实据。

当前WSL特殊127路由会使大UDP未交到宿主peer；六合法出站分片不等于端到端成功。验证复用临时非特权user/net namespace中的标准lo后，两架构真实17/0/1472/8193全文收发通过；这不是公共run的fallback或宿主/QEMU整改。LA NX运行仍暂缓非Pass，AT_RANDOM仅教学非加密字节；不扩RTC ioctl、VFAT/分区、journal/recovery、PTY/namespace或全部ABI。完整原命令、SHA、失败、资源与独审沿[阶段九记录](../../.docs/阶段九记录.md)。

