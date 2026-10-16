# 当前源码与运行根导读

日常构建、三类测试、多文件 TOML、工作盘恢复/刷新及固定四产物操作统一见 [开发、测试与比赛执行](../../docs/development.md)。运行库具体来源、官方输入与旧失败边界见 [sources.md](sources.md)。这两份公开说明可独立消费，不要求先阅读本地验收流水或外部计划。

项目是 riscv64、loongarch64 的 Rust `no_std` 宏内核。用户比赛执行器是独立 Rust std 包 `userland/contest-run`，安装为 `/sbin/contest-run`；BusyBox init 仍为 PID1。旧 Shell 固定 selector 编排与 C contest-exec 转发不再是正式使用入口。程序能构建、目录能展开、真实 guest 能运行以及实体板验证必须分别判断。

## 活模块导航

以 [os/src/main.rs](../../os/src/main.rs) 的 `mod` 声明及其子模块为准，磁盘上的旧文件或索引命中不等于参与编译。

| 阅读方向 | 当前源码与职责 |
|---|---|
| 启动与硬件 | [boot.rs](../../os/src/boot.rs)、[platform.rs](../../os/src/platform.rs)、[arch.rs](../../os/src/arch.rs) 选择双架构后端；[cpu.rs](../../os/src/cpu.rs)、[irq.rs](../../os/src/irq.rs)、[console.rs](../../os/src/console.rs)、[sync.rs](../../os/src/sync.rs) 管 CPU、中断、UART 与同步。 |
| 物理页与用户地址空间 | [mm.rs](../../os/src/mm.rs) 下的 bootmem/page/buddy/pcp/slub、folio/dma、space/vma/user_table/uaccess；[cache.rs](../../os/src/cache.rs) 管共享元数据缓存。 |
| 执行与服务 | [task.rs](../../os/src/task.rs) 与 [sched.rs](../../os/src/sched.rs) 是唯一任务/调度入口；[wait.rs](../../os/src/wait.rs)、[time.rs](../../os/src/time.rs)、[accounting.rs](../../os/src/accounting.rs)、[timer.rs](../../os/src/timer.rs)、[workqueue.rs](../../os/src/workqueue.rs) 管等待、时间、执行账和延后工作。 |
| 磁盘与文件 | [drivers.rs](../../os/src/drivers.rs) 下的 PCI/VirtIO、[storage.rs](../../os/src/storage.rs)、[block.rs](../../os/src/block.rs) 提供设备和 Bio；[fs.rs](../../os/src/fs.rs) 下的 ext4/inode/dentry/path/file/file_table、writeback/background/readahead 管文件、路径与有限回写。 |
| 用户执行与 ABI | [task/exec.rs](../../os/src/task/exec.rs)、[task/process.rs](../../os/src/task/process.rs)、[task/signal.rs](../../os/src/task/signal.rs)、[task/futex.rs](../../os/src/task/futex.rs) 共用原 Task 与 Mm；[syscall.rs](../../os/src/syscall.rs) 为所选 syscall 分派入口。动态 ELF 使用测试根内真实 PT_INTERP 与库。 |
| 伪 FS、网络与观测 | fs 下的 devfs/procfs/memfs、pipe/poll/epoll、tty 供普通系统使用；[net.rs](../../os/src/net.rs) 下的 core/socket/tcp/udp/unix 和 [drivers/virtio_net.rs](../../os/src/drivers/virtio_net.rs) 提供所选双族网络；[klog.rs](../../os/src/klog.rs)、[trace.rs](../../os/src/trace.rs) 与 fs 的 kmsg/ktrace 提供有限观测。 |
| 系统生命周期 | [fs/system.rs](../../os/src/fs/system.rs) 选择真实根视图，main 执行 PID1、wait/join/reap、停止网络、同步并退休文件系统后执行关机请求。 |
| 内核测试 | [ktest.rs](../../os/src/ktest.rs) 仅由 ktest feature 编译，宿主入口为 [tools/ktest.py](../../tools/ktest.py)。文件写入行为在 [user/write](../../os/src/fs/ktest/user/write/)，共同 WriteOwner 在 write.rs；网络与串口接收 fixture 按真实需要组合。 |
| 比赛选择与执行 | [model.rs](../contest-run/src/model.rs) 拥有具体输入和 ResolvedCase；[config.rs](../contest-run/src/config.rs) 解码并一次校验；[selection.rs](../contest-run/src/selection.rs) 从真实目录/清单展开。[execute.rs](../contest-run/src/execute.rs) 负责子进程、stdio、期限与每轮前后责任；[runtime.rs](../contest-run/src/runtime.rs) 负责实际挂载、系统根准备与有限解释器诊断；[report.rs](../contest-run/src/report.rs) 判定协议，[custom.rs](../contest-run/src/custom.rs) 放具体用户态特殊操作。入口和集合在 [userland/contest](../contest/)。 |
| 内核专属操作 | [contest_hooks.rs](../../os/src/contest_hooks.rs) 由 devfs 的 `/dev/testhook` 完整消息直接分派；Hook 在控制者普通 Task 上下文执行，复用原 trace，不是目标内部时刻或通用回滚平台。 |
| 用户工具与断言 | [userland/tools](../tools/) 放 klog-read、ktrace-control，安装名仍为 `/bin/klog-read`、`/bin/ktrace-control`；[tests/user](../../tests/user/) 的 libc-smoke、network-smoke、observation-smoke 保留测试断言，libc/network 正文按职责分翻译单元。 |

`mm/zram.rs`、`fs/swap.rs`、`lang_items.rs` 未进入当前模块图，不能据此宣称支持 zram、swap 或独立 lang_items；未编译旧 fs/dev 等源也不证明用户 RTC 或全部 ABI 已有实现。普通业务修订沿当前模块，不恢复旧 HAL/TCB/MemorySet/用户缓冲抽象或旧执行入口。

## 明确 root/cwd/argv 的一次命令

需要绕过计划、直接核一个原测试时，接口仍为：

```text
DEVICE --root ROOT --cwd CWD -- COMMAND [ARG...]
```

DEVICE 为 `/dev/vd[a-z]`；ROOT、CWD 都须显式为绝对路径，ROOT 不含 `..` 组件，并有 `--` 与非空命令。ROOT 是设备卷内目录，CWD 是换根后的目录，与系统启动 `ROOTDIR` 无关。例如，实际盘含相应程序时在 guest 执行：

```sh
/sbin/contest-run /dev/vda --root /musl --cwd / -- \
  ./runtest.exe -w entry-dynamic.exe stat
```

这条接口直接构造同一个具体 case 并走正式 Rust 执行函数，不转调 Shell/C helper；它是普通命令协议，不自动套固定 libctest 的 return1/正文判定。要使用对应结果适配，应通过 `kind="libctest"` 的计划选择。

runner 父保持系统根，子进程真实 chroot/chdir/exec。argv 不拼接、不 eval，空参数、空格和特殊字符保持原样；只有显式 `/bin/sh` 或 shell step 才要求目标根有 shell。动态 ELF 必须能在自身根找到真实 PT_INTERP、loader 与库；不自动补装、不猜 `/busybox`、不根据族名覆盖盘内文件。缺 exec 与程序自行 exit127 分别记录。

一次命令使用 Plain 协议：继承 stdin，stdout/stderr 由 CLOEXEC dup 直接交给子进程，不解析正文、不改变借用标准流的 OFD 状态。计划中的 LTP 也走这一直接路径；只有 Basic/固定 Pre2025 才用有界队列及一个仅作 libc 调用的输出 relay 子进程，父只对独占 pipe 使用 nonblocking。relay 必须真实 drain/wait 或到期定向 kill/wait，输出错误独立报告，不覆盖测试原 wait。runner 仍逐 case 单线程控制，不加线程池或通用 `/proc` 后台树扫描；详细期限与进程组边界见[公共操作指南](../../docs/development.md#5-真实解释器准备与结束责任)。

[contest-argv](../../tests/user/scripts/contest-argv) 是仓库已有参数/cwd 消费者。只允许把它放入**自有非评分根** `/tests`，该根还必须实际提供 `/bin/sh`；不是把它注入官方盘后声称原盘通过：

```sh
/sbin/contest-run /dev/vda --root / --cwd /tests -- \
  /bin/sh ./contest-argv /tests 'two words' '' '*' ';not-eval'
```

一次命令结束返回调用者，不默认关机；计划的 finish 明确选择 poweroff 或 return。两者共用真实挂载取得、期限、wait 与逆序卸载，主结果和清理错误分别保留，不强制/lazy 卸载。手动运行前使用保留控制 shell 的完整自有 SOURCE；默认比赛 inittab 可能已经执行计划并关机，不应与它并发运行第二个 runner。

## 学生与 agent 的最短修改路线

1. 只改选择、参数、root/cwd、超时或重复次数：修改 `userland/contest`，先 list 核实际输入，再定向运行。完整树打包，只有入口引用的文件启用。已有工作盘须明确 REFRESH_SYSTEM 才换系统内容，普通 RESUME 不更新它。
2. 新结果协议或特殊用户准备：修改独立 runner 对应具体模块，补同源 host 检查；实际 exec、deadline、PT_INTERP 和输出仍需 guest 验证。setup/cleanup 在 case 根，root.prepare 在系统根，不混淆两个上下文。
3. 新内核 ABI 或真实并发缺口：在当前所属子系统定向修，补最窄 ktest/用户回归；先 `make ktest-list` 取得当前全名，再单选。保留 raw ABI、原错误、精确交错、失败清理和真实停止/资源归还，不借重置统计取得相等。
4. 只需操作日志/追踪：使用 userland/tools；需要断言时留在 tests/user。不要把所有程序迁成 Rust，也不要为每个比赛项加空 ktest 或 Hook 分支。

具体构建、Cargo 工作目录、sysroot/linker 前置、all 四产物、SOURCE/STAGING 与锁内清理命令均在[公共操作指南](../../docs/development.md)，不另建 doctor、能力表或验收账。

## 新盘与能力边界

受支持整盘 ext4 中改变 ELF、目录、参数、根位置或只保留一个 libc 族，通常只改配置与明确准备；新 syscall、文件系统、分区或板载控制器则是对应真实内核工作。新自有系统盘优先用 mkext4 的 seed4k，不把它用于重制官方输入，也不将所选 legacy1k 等支持推广成任意 ext4 feature。

没有分区表/`/dev/vda2`、VFAT、用户 RTC 设备/ioctl ABI时，相关原失败不是 runner root 选择错误。普通镜像不创建 journal，不实现 journal 校验/replay、崩溃恢复引擎或恶意坏盘加固；正常 sync、工作副本 RESUME 和系统盘刷新不等于恢复功能。LA NX 暂缓仍非 Pass，AT_RANDOM 为非加密教学字节；不外推全部 Linux/std/LTP、PTY/namespace、线上成绩或实体板已验证。
