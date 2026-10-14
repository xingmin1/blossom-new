# 当前开发入口与系统盘

本导读描述现行源码合同，不把目录和命令可配置等同于任意盘面或全部 ABI 兼容。项目是一个 Rust `no_std` 宏内核，目标为 riscv64、loongarch64；内核没有比赛专用路径替换。运行库来源与原输入失败事实见 [sources.md](sources.md)。

## 活模块导航

以 [os/src/main.rs](../../os/src/main.rs) 的 `mod` 声明及其子模块为准；磁盘上的文件名或索引命中不代表参与编译。

| 阅读方向 | 当前源码与职责 |
|---|---|
| 启动与硬件 | [boot.rs](../../os/src/boot.rs)、[platform.rs](../../os/src/platform.rs)、[arch.rs](../../os/src/arch.rs) 选择双架构后端；[cpu.rs](../../os/src/cpu.rs)、[irq.rs](../../os/src/irq.rs)、[console.rs](../../os/src/console.rs)、[sync.rs](../../os/src/sync.rs) 管 CPU、中断、UART 和同步。 |
| 物理页与用户地址空间 | [mm.rs](../../os/src/mm.rs) 下的 bootmem/page/buddy/pcp/slub、folio/dma、space/vma/user_table/uaccess；[cache.rs](../../os/src/cache.rs) 管共享元数据缓存。 |
| 执行与服务 | [task.rs](../../os/src/task.rs) 与 [sched.rs](../../os/src/sched.rs) 是唯一任务/调度入口；[wait.rs](../../os/src/wait.rs)、[time.rs](../../os/src/time.rs)、[accounting.rs](../../os/src/accounting.rs)、[timer.rs](../../os/src/timer.rs)、[workqueue.rs](../../os/src/workqueue.rs) 管等待、时间、执行账及延后工作。 |
| 磁盘与文件 | [drivers.rs](../../os/src/drivers.rs) 下的 PCI/VirtIO、[storage.rs](../../os/src/storage.rs)、[block.rs](../../os/src/block.rs) 提供设备与 Bio；[fs.rs](../../os/src/fs.rs) 下的 ext4/inode/dentry/path/file/file_table、writeback/background/readahead 管文件、路径和有限回写。 |
| 用户执行与 ABI | [task/exec.rs](../../os/src/task/exec.rs)、[task/process.rs](../../os/src/task/process.rs)、[task/signal.rs](../../os/src/task/signal.rs)、[task/futex.rs](../../os/src/task/futex.rs) 共用原 Task 与 Mm；[syscall.rs](../../os/src/syscall.rs) 是所选 syscall 分派入口。动态 ELF 使用盘内真实 PT_INTERP 与库文件。 |
| 伪FS、网络与观测 | fs 下的 devfs/procfs/memfs、pipe/poll/epoll、tty 供普通系统使用；[net.rs](../../os/src/net.rs) 下的 core/socket/tcp/udp/unix 与 [drivers/virtio_net.rs](../../os/src/drivers/virtio_net.rs) 提供所选双族网络；[klog.rs](../../os/src/klog.rs)、[trace.rs](../../os/src/trace.rs) 及 fs 的 kmsg/ktrace 提供有限观测。 |
| 系统生命周期与测试 | [fs/system.rs](../../os/src/fs/system.rs) 选择真实根目录，main 执行 PID1、wait/join/reap、停止网络、同步并退休文件系统后执行关机请求；[ktest.rs](../../os/src/ktest.rs) 仅由 `ktest` feature 编译，宿主入口为 [tools/ktest.py](../../tools/ktest.py)。 |

`mm/zram.rs`、`fs/swap.rs`、`lang_items.rs` 没有进入当前模块图，不代表现有 zram、swap 或独立 lang_items 能力；同样不能用未编译的旧 fs/dev、syscall 等源宣称支持 RTC 或全部 ABI。保留旧源不等于恢复旧入口，也不因导读整理机械删文件。

## 最短宿主命令

所有命令在仓库根执行，工具由当前 `PATH` 提供；不自动调用 Nix、下载工具链或重编比赛 ELF。构建需项目 Rust/Cargo，运行需对应 `qemu-system-ARCH`；制盘需 Python3 与 `mke2fs/debugfs/e2fsck/dumpe2fs`。首次生成普通用户树或 `all` 还需匹配的静态 musl 交叉编译器及运行库来源。

已有完整、匹配架构的普通源树 `/srv/system-rv`（含 `/sbin/init`、`/bin/sh` 与自身配置）时：

```sh
make build ARCH=riscv64
make image ARCH=riscv64 SOURCE=/srv/system-rv IMAGE=build/system-riscv64.img
make run ARCH=riscv64 IMAGE=build/system-riscv64.img ROOTDIR=/
make run ARCH=riscv64 IMAGE=build/system-riscv64.img ROOTDIR=/ RESUME=1
```

`loongarch64` 同形使用自己的源树、镜像和工作目录。接口以根 [Makefile](../../Makefile)、[tools/mkext4.py](../../tools/mkext4.py)、[tools/run.py](../../tools/run.py) 为准：

- `build` 只构建内核，默认 `OUTPUT=build/ARCH/kernel.elf`；`run` 构建后启动，不隐含 `image`。
- `image` 的 `IMAGE` 是**自有系统根 FS 的制盘输出**（普通输出须在 `build/` 下且以 `.img` 结尾），会生成并替换该文件；`run` 的 `IMAGE` 是**已有来源盘输入**。禁止把官方测试输入、导出的测试树或测试工作盘传给 `image` / `tools/mkext4.py`；不要把输入盘路径当制盘输出。
- `SOURCE` 是已有目录输入，不是镜像或运行工作盘。未指定 SOURCE 且 STAGING 不存在时，`image` 才构建静态 userland、准备 runtime，并安装普通 bin/sbin/usr、[系统配置](../system/etc/inittab) 和 `/opt/runtime/{musl,glibc}`；库来源可传 `MUSL_LIBC`、`GLIBC_LIBDIR`。已有 SOURCE 或 STAGING 不重填、不自动升级；新内容由调用者修改自有源树或选择新 STAGING。
- fresh（默认 `RESUME=0`）由 run.py 把来源盘复制为 WORK_DIR 中的独立 `root.raw` 等工作盘，只把副本交 QEMU；源盘不被运行改写。`RESUME=1` 只使用同一 WORK_DIR 的已有工作盘，保留当前连续运行的修改，不复制、不制盘；缺盘报错，不自动转 fresh。来源文件仍须存在。锁一直持有到真实 QEMU exec/退出。
- 普通根默认 RO；明确 `ROOT_MODE=rw` 才可正常写盘，extra 固定 RO。当前连续运行恢复时保持原盘角色及 WORK_DIR；不调用 `image` 或 fresh 去“修复”工作状态。运行关闭并确认 QEMU 退出后及时删除工作盘；本轮来源与临时系统盘使用完也删除，不为历史记录或可复现保留镜像备份，保留命令、摘要、日志和原报告即可。

## 普通单根与 all 双根

`rootdir` 是物理 ext4 卷内的真实目录，选中后同时成为初始进程 root/cwd；init 路径在该视图内解析。显式错误不猜其他目录或设备。

| 启动方式 | 系统盘 ROOT | 系统盘内 ROOTDIR | INIT |
|---|---|---|---|
| 普通 `make run` 单根盘 | `/dev/vda` | `/`（Make 默认） | `/sbin/init` |
| `make contest` 默认单根系统盘，比赛盘在前 | `/dev/vdb` | `/`（Make 默认） | `/sbin/init` |
| all 双根盘用于 RV | `/dev/vdb` | `/riscv64` | `/sbin/init` |
| all 双根盘用于 LA | `/dev/vdb` | `/loongarch64` | `/sbin/init` |

`make all` 同时构建两份普通 abort 内核、静态 userland 与 runtime；库来源分别由 `MUSL_LIBC_RV/GLIBC_LIBDIR_RV`、`MUSL_LIBC_LA/GLIBC_LIBDIR_LA` 指定，编译器由 `MUSL_CROSS_RV/MUSL_CROSS_LA` 指定。例如来源已在 sources.md 的 `/srv/contest-runtime` 树中时：

```sh
make all \
  MUSL_LIBC_RV=/srv/contest-runtime/riscv64/musl/libc.so \
  GLIBC_LIBDIR_RV=/srv/contest-runtime/riscv64/glibc/lib \
  MUSL_LIBC_LA=/srv/contest-runtime/loongarch64/musl/libc.so \
  GLIBC_LIBDIR_LA=/srv/contest-runtime/loongarch64/glibc/lib
```

all 使用独立 `build/delivery-ARCH/` 与新双根临时树，制盘一次，导出 `kernel-rv`、`kernel-la`、`disk.img`、`disk-la.img`；两盘是同内容的独立普通文件，不改普通 SOURCE、STAGING 或运行工作盘。双根中的普通 inittab 均运行 `contest-run /dev/vda`。外部无 append 启动才使用内核 `/dev/vdb` 与架构 rootdir 默认值；通过 Make/run.py 使用双根盘必须明确传对应 ROOTDIR。LA 使用 PCI 附盘，前两盘 addr 为0x5、0x9；[tools/storage.py](../../tools/storage.py) 统一盘槽并保留 NIC 槽3，不沿用 PCI/MMIO 混写示例。

## 比赛盘与同一个客体 runner

### 官方测试盘来源

测试盘只使用官方发布资产，自有系统根 FS 才允许制盘。当前选择依据是官方[测试仓库 README](https://github.com/oscomp/testsuits-for-oskernel/blob/pre-2025/README.md) 的整盘 ext4、无分区表与辅助根 FS 说明，以及 [pre-20250615 Release API](https://api.github.com/repos/oscomp/testsuits-for-oskernel/releases/tags/pre-20250615) 的两份资产：

| 架构 | 官方下载 URL | 发布的压缩包 SHA-256 |
|---|---|---|
| riscv64 | https://github.com/oscomp/testsuits-for-oskernel/releases/download/pre-20250615/sdcard-rv.img.xz | `2b59aee4d26681b2d78a8faa615ac216f723a8ea157544e7867bbc8c77aac087` |
| loongarch64 | https://github.com/oscomp/testsuits-for-oskernel/releases/download/pre-20250615/sdcard-la.img.xz | `3cbeae1095d788cd8255e4929bcc6dace96dbe4999b6eaec003ae6fe2cdba42b` |

下载时核资产名、版本与压缩包 SHA-256，解包为对应 `sdcard-rv.img` / `sdcard-la.img` raw 整盘。**解包不是制盘**；`CONTEST_IMAGE` 只接匹配架构、已解包的官方 raw 测试盘，不能传 `.xz`、自制重打包盘或注入版。禁止用 `mkext4` / `mke2fs -d` 重制测试输入，禁止为评分给测试盘或工作副本注入/替换程序、脚本、运行库、链接或断言；测试本身的正常写入保留在当前临时副本。

此前命名为 `original` / `prepared` 的本地重制和注入盘不能改称官方资产；旧日志、SHA 和失败保留原身份，仅作故障线索，不作为本轮官方得分。成功解包不证明原测试、shell、库或决赛用例存在，也不证明已运行通过；以盘内实际目录与原程序/判题器为准。下载源与运行副本只保留至本轮使用结束，不保历史盘或复现备份。

以下示例要求自有 `/srv/system-rv` 保留控制 shell，不自动执行固定 selector；本轮将通过下面明确 root/cwd/argv 的普通命令入口运行原测试，不部署程序或运行库到测试工作根。它仍需原盘自身的 shell、解释器和库；缺失时记录真实输入边界，不通过补装取得“官方盘通过”。


```sh
make contest ARCH=riscv64 SOURCE=/srv/system-rv CONTEST_IMAGE=build/sdcard-rv.img
make contest ARCH=riscv64 SOURCE=/srv/system-rv CONTEST_IMAGE=build/sdcard-rv.img RESUME=1
```

fresh contest 保留 `image` 前置；`RESUME=1` 跳过该前置，仍执行 build 和原参数/缺盘检查，不重新制作系统镜像。默认系统镜像、staging、工作目录分别为 `build/contest-system-ARCH.img`、`build/contest-system-ARCH`、`build/contest-run-ARCH`，可显式改 IMAGE/STAGING/SOURCE/OUTPUT/WORK_DIR。比赛工作副本固定 `/dev/vda` 且 RW，系统副本 `/dev/vdb`；fresh 复制成 `contest.raw` 与 `root.raw`，不把来源比赛盘直接交给 QEMU 写入。只有首次 contest staging 才配置自动 runner；明确 SOURCE 或已有 staging 的配置保持原样。

### 固定 selector

[同一个 /sbin/contest-run](../system/sbin/contest-run) 接口为 `DEVICE [selector...]`；省略 selector 使用原完整列表，接受 basic/busybox/lua、固定表中的 dynamic:NAME/static:NAME，完整校验后依次运行 musl、glibc。例如：

```sh
/sbin/contest-run /dev/vda dynamic:clock_gettime dynamic:stat
```

**当前边界：**固定模式的 `prepare_family` 仍会从系统 `/opt/runtime` 复制库到测试工作根，删除/替换同名库和链接，并设置固定族环境；原脚本、ELF、测试 DSO 与正文保持不变，也不能据此称输入未注入。该模式尚未修改，不能用于本轮未注入官方盘的正式评分。其既有 `runtest` 分类要求原 START/END、`Pass!`、返回1和无对应 FAIL，普通返回0不等于此协议成功；原组内失败、信号、超时及未运行不改签为 Pass，不宣称全部 LTP/libctest 或评分平台兼容。固定模式仍在真实 wait、清理与 sync 后自然 poweroff。

### 明确 root/cwd/argv 的普通命令

精确接口为 `DEVICE --root ROOT --cwd CWD -- COMMAND [ARG...]`，DEVICE 为 `/dev/vd[a-z]`；ROOT、CWD 均须显式为绝对路径，必须有 `--` 和非空命令。ROOT 是**设备卷内**的目录，不含 `..` 组件；CWD 是**换根后**的目录，与宿主启动 ROOTDIR 无关。根在 `/`、任意子目录或只有一个 libc 族都用同一入口，不按目录名猜族。

```sh
/sbin/contest-run /dev/vda --root / --cwd /work -- /bin/probe 'a b' '' '$literal'
/sbin/contest-run /dev/vda --root /musl --cwd / -- ./runtest.exe -w entry-dynamic.exe stat
```

命令、脚本解释器与动态库必须由目标 root 自身提供；只有调用者显式选择 shell 命令或脚本解释器时才需要相应 shell。静态 ELF 不要求目标 root 有 `/bin/sh`。缺命令、解释器或库保留真实失败，不得补装到官方测试盘后当原盘得分。普通命令模式不自动安装 shell、不猜 `/busybox`、不覆盖盘内库。

父进程在系统根挂工作盘，真实子进程先从自有系统根装载静态 `/sbin/contest-exec ROOT CWD COMMAND [ARG...]`，再依次执行 `chroot(ROOT)`、`chdir(CWD)`、`execvp(COMMAND, argv)`；helper 不部署到测试盘。runner 使用 `"$@"`，helper 直接传递原 argv，空字符串、空格、UTF-8 和特殊字符均不拼接、不 eval；CWD 在换根后解析。普通模式不复制两族 runtime、不强设固定族 LD_LIBRARY_PATH、不截取正文做分类；stdout/stderr 继承调用者，非零或信号状态按真实父 wait 保留，不套用 selector 的 Pass/return1 协议。helper 参数不足、换根或换目录失败返回125，exec 缺文件返回127、其它 exec 失败返回126，并输出真实 errno 诊断。

helper 与现有 userland 一同构建，也可单独构建；例如为自有 RV 系统来源树准备它：

```sh
make build/userland-riscv64/sbin/contest-exec ARCH=riscv64 \
  USERLAND_ROOT=build/userland-riscv64 MUSL_CROSS=riscv64-linux-musl-
install -m 755 build/userland-riscv64/sbin/contest-exec /srv/system-rv/sbin/contest-exec
install -m 755 userland/system/sbin/contest-run /srv/system-rv/sbin/contest-run
```

LA 使用 `ARCH=loongarch64`、`USERLAND_ROOT=build/userland-loongarch64`、目标 `build/userland-loongarch64/sbin/contest-exec` 和匹配的 `MUSL_CROSS=loongarch64-linux-musl-`。既有 SOURCE/staging 不会由 `make image` 自动补装 helper，须显式更新自有来源树后再制自有系统盘；官方测试盘保持原文件。

手动命令需要保留控制 shell 的系统配置，不能让默认 contest inittab 先运行固定列表并关机。可用上面的自有普通 SOURCE 启动两盘，配置不会因 contest 被重填：

```sh
make contest ARCH=riscv64 SOURCE=/srv/system-rv \
  IMAGE=build/command-system-rv.img CONTEST_IMAGE=build/sdcard-rv.img
```

两种模式共用 devfs/proc/tmpfs 的取得记录、父等待与反序卸载（tmp/proc/dev，再 ext4），失败也清理并 sync；不强制或 lazy 卸载。命令结果与 cleanup 分别报告：原命令非零优先保留，原命令成功而 cleanup 失败才由 runner 返回非零。普通命令结束**返回调用者，不关机**，可继续下一次调用；最后由控制者执行正常 `sync`/`poweroff`。

参数和 cwd 的最小消费者为 [tests/user/scripts/contest-argv](../../tests/user/scripts/contest-argv)。以下只适用于自有非评分工作根，不能把该脚本放入官方测试输入；将它放入自有工作根的 `/tests` 后，可实际调用：

```sh
/sbin/contest-run /dev/vda --root / --cwd /tests -- \
  /bin/sh ./contest-argv /tests 'two words' '' '*' ';not-eval'
```

它检查真实 cwd 与四个容易被 shell 误拆分的参数，不套评分协议，不要求写系统根；该命令仍需已附盘、盘内 shell 和明确工作副本。

## 新盘与能力边界

同一受支持整盘 ext4 的**自有根 FS**中改变文件、目录、根位置或只保留一个族，只需选择上述目录与程序，不改内核。新自有系统盘优先使用当前 mkext4 的 seed4k 配置；该制盘建议不适用于官方测试输入。既有支持还有所选 legacy1k 等格式，不能据此声称任意 ext4 feature 均受支持。磁盘内的 loader、库和程序必须匹配架构/ABI，不修改原 PT_INTERP、ELF 或 DSO 来掩盖缺失。

当前没有分区表/`/dev/vda2`、VFAT、用户 RTC 设备/ioctl ABI，不把这些失败归咎于 runner 的 root 选择。教学内核按真实盘面处理，普通镜像不创建 journal，不实现 journal 校验/replay、崩溃恢复引擎或恶意坏盘加固；正常 sync/工作副本 RESUME 不等于恢复功能。LA NX 运行暂缓非 Pass，AT_RANDOM 为非加密教学字节；不外推全部 Linux ABI、PTY/namespace、全比赛或线上评测通过。
