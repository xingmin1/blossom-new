# 比赛测试执行手册

本手册说明如何在比赛测试盘上选择、预览和执行用例，以及如何阅读输出文本与退出码。执行器为客体系统中的 `/sbin/contest-run`，测试计划与集合文件位于仓库的 `userland/contest/`。增减程序、修改参数或运行环境都不需要改动 Rust 代码。

## 1. 第一次运行

准备对应架构的比赛测试盘：RISC-V 使用 `sdcard-rv.img`，LoongArch 使用 `sdcard-la.img`。这两张盘是比赛测试盘，不是构建生成的系统辅助盘，二者不能互换。

```sh
# 构建双架构交付物，然后运行 RISC-V。
make all
make run-qemu ARCH=riscv64

# 运行 LoongArch。
make run-qemu ARCH=loongarch64
```

系统启动后由 `init` 调用 `/sbin/contest-run /dev/vda`；执行器默认读取 `/etc/contest/plan.toml`，执行计划中选中的用例，最后关机。

只调试一种架构时使用便捷目标，它依次执行该架构的打包和 QEMU 启动：

```sh
make contest ARCH=riscv64
make contest ARCH=loongarch64
```

等价的显式命令为：

```sh
make build-qemu-artifacts ARCH=riscv64
make run-qemu ARCH=riscv64
```

客体读取的是系统盘中的 `/etc/contest/` 配置，不是宿主机的源码文件。只改测试名单也要重新打包，否则启动的仍是旧配置。

QEMU 默认使用 `fresh` 工作盘：每次把系统源镜像和比赛源镜像重新复制到工作目录。三种策略的行为如下：

| 策略 | 行为 |
|---|---|
| `fresh` | 重新复制系统盘和比赛盘，源镜像保持不变 |
| `reuse` | 直接复用工作目录中已有的副本，不读取源镜像，也不自动更新其中的配置 |
| `refresh-system` | 只用最新系统源镜像替换系统工作盘，保留比赛工作盘的已有数据 |

例如在交互式会话中明确指定策略：

```sh
make run-qemu ARCH=riscv64 QEMU_ARGS='--working-disk-action fresh'
make run-qemu ARCH=riscv64 QEMU_ARGS='--working-disk-action refresh-system'
```

`roots.prepare` 修改的是比赛盘的工作副本；原始比赛镜像不会被写入。

## 2. 修改哪个文件

| 想做什么 | 修改哪里 |
|---|---|
| 选择启动后运行哪些集合 | `userland/contest/plan.toml` 的 `files` |
| 增减 Basic | `userland/contest/sets/basic.toml` 的 `include` |
| 增减 Libctest selector | `userland/contest/sets/libctest.toml` 的 `names` |
| 选择 LTP 用例 | `userland/contest/sets/ltp.toml` 的 `include`/`exclude` |
| 调整单例的超时、参数、环境或准备命令 | `userland/contest/overrides/local.toml` |
| 适配现场测试盘布局 | `plan.toml` 中的 `roots` 及对应集合文件 |

`files` 中没有列出的集合文件不会运行。磁盘或仓库中存在某个 TOML 不代表它已启用。

默认计划启用 12 个集合：`basic`、`busybox`、`lua`、`libctest`、`iozone`、`unixbench`、`iperf`、`libcbench`、`lmbench`、`netperf`、`cyclictest` 和 `ltp`。`ltp` 只运行 `sets/ltp.toml` 的 `include` 所匹配的部分用例，不是全量：

```toml
files = [
    "sets/basic.toml",
    "sets/busybox.toml",
    "sets/lua.toml",
    "sets/libctest.toml",
    "sets/iozone.toml",
    "sets/unixbench.toml",
    "sets/iperf.toml",
    "sets/libcbench.toml",
    "sets/lmbench.toml",
    "sets/netperf.toml",
    "sets/cyclictest.toml",
    "sets/ltp.toml",
]
override_files = ["overrides/local.toml"]
finish = "poweroff"
```

从默认计划中删除某个集合文件对应的一行，就停止运行该集合。配置支持 TOML 注释和多行数组，没有 Hook、profile 或“优化名称”之类的开关。

## 3. 集合的写法

### 3.1 Basic

默认 `sets/basic.toml` 按盘内 `basic/run-all.sh` 的顺序列出 32 个名称，下面只保留 5 个作为写法示例：

```toml
[[group]]
name = "basic"
roots = ["musl", "glibc"]
cwd = "/basic"
start = "#### OS COMP TEST GROUP START basic-{root} ####\n"
end = "#### OS COMP TEST GROUP END basic-{root} ####\n"

[[group.step]]
directory = "."
include = ["brk", "getpid", "mmap", "pipe", "read"]
exec = ["./{name}"]
before = "Testing {name} :\n"
```

`directory` 列出 `cwd` 中具有执行位的普通文件作为候选；`include` 按名字或 glob 选择。增加程序只需在 `include` 中加一项，例如 `"clone"`。只测一个根时把 `roots` 改为 `["musl"]`。

| 字段 | 含义 |
|---|---|
| `name` | 组名，用于 ID 前缀和输出分组，不触发专属判定 |
| `roots` | 使用 `plan.toml` 中声明过的执行根 |
| `cwd` | 用例在测试根内的工作目录 |
| `directory` | 候选文件所在目录；`.` 表示 `cwd` |
| `include`/`exclude` | 按名字或 glob 选择、排除候选 |
| `exec` | 实际执行的 argv 模板，`{name}` 替换为候选名 |
| `start`/`end`/`before`/`after` | 输出文本，不是评分条件 |

`include` 省略等价于 `["*"]`。同一个 step 内重叠的匹配只执行一次，需要重复时使用 `repeat`。`exclude` 没有匹配项不报错；某个 `include` 一个候选都没匹配时执行器报错，并指出该 step 的来源文件与条目。

### 3.2 Libctest

`names` 是 selector，不要求盘内存在与 selector 同名的 ELF；下面只列出部分 selector：

```toml
[[group.step]]
id = "libctest:{root}:static:{name}"
names = ["argv", "env", "clock_gettime", "stat"]
exec = ["./runtest.exe", "-w", "entry-static.exe", "{name}"]
```

dynamic 与 static 两个 step 放在同一个 group 内，组标记只打印一次。把两个 step 拆成同名 group 会使外部评分器重复看到同名分组。

### 3.3 LTP

目录模式直接执行盘内的 ELF：

```toml
[[group]]
name = "ltp"
roots = ["musl", "glibc"]
cwd = "/"
env = { LTPROOT = "/ltp", TMPDIR = "/tmp" }
start = "#### OS COMP TEST GROUP START ltp-{root} ####\n"
end = "#### OS COMP TEST GROUP END ltp-{root} ####\n"

[[group.step]]
directory = "/ltp/testcases/bin"
include = ["open*", "close*", "mmap01"]
exclude = ["open_by_handle_at*"]
exec = ["/ltp/testcases/bin/{name}"]
before = "RUN LTP CASE {name}\n"
after = "FAIL LTP CASE {name} : {status}\n"
```

需要官方参数、别名或 Shell 操作的条目改用 runtest 清单，见第 9 节。

### 3.4 通配符、顺序与重复

`include`、`exclude` 和 `--only` 支持 `*`、`?`、字符集合等 glob。执行顺序为：`files` 中的文件顺序、组声明顺序、`roots` 顺序、step 顺序。每个 root 内完整执行该组的全部 step，然后切换到下一个 root。同一组内不同 step 的候选不会合并去重。

`repeat` 在 `defaults`、step 或 override 上配置，表示每个用例连续执行的次数；`--repeat` 覆盖本次运行的所有已选用例。

## 4. 仓库自带的 9 个原脚本集合

BusyBox、Lua、Iozone、UnixBench、iperf、libcbench、lmbench、netperf 和 cyclictest 在盘内各有一份原测试脚本。集合文件用单个 `script` 名字选中它，并用盘内 BusyBox 的 `sh` 执行：

```toml
# 脚本自己输出组标记，group 不写 start/end。
[[group]]
name = "busybox"
roots = ["musl", "glibc"]

[[group.step]]
names = ["script"]
exec = ["/busybox", "sh", "/busybox_testcode.sh"]
timeout_sec = 600
```

| 集合 | 脚本 | 外层超时（秒） |
|---|---|---|
| `busybox` | `/busybox_testcode.sh` | 600 |
| `lua` | `/lua_testcode.sh` | 600 |
| `iozone` | `/iozone_testcode.sh` | 1800 |
| `unixbench` | `/unixbench_testcode.sh` | 3600 |
| `iperf` | `/iperf_testcode.sh` | 600 |
| `libcbench` | `/libcbench_testcode.sh` | 1800 |
| `lmbench` | `/lmbench_testcode.sh` | 1800 |
| `netperf` | `/netperf_testcode.sh` | 600 |
| `cyclictest` | `/cyclictest_testcode.sh` | 600 |

这些脚本自带组起止标记与结果行，group 不写 `start`/`end`，避免输出两套标记。`timeout_sec` 是整个脚本的外层预算，应按脚本实际耗时设置，不能套用普通短用例的 60 秒。

`iperf` 集合额外配置 `cleanup`：盘内脚本用 `iperf3 -s -D` 启动服务端，守护进程脱离主命令的进程组，脚本结束时也不停止它，主命令收尾的整组 `SIGKILL` 到不了它。`sets/iperf.toml` 直接用 `/busybox sh /iperf_testcode.sh` 原样执行脚本；cleanup 在同一个 root 内用盘内 BusyBox 执行 `killall iperf3`，再用 `pidof` 每秒检查一次，直到服务端退出后才切换 root，否则它打开的 `/dev` 文件会让卸载返回 Busy。

`killall`/`pidof` 依赖内核的 `/proc/<pid>/stat` 与 `/proc/<pid>/cmdline` 查询。系统根 BusyBox 未启用这两个 applet，cleanup 必须使用比赛根内的 `/busybox`。

cleanup 单条命令的超时取 `defaults.timeout_sec`（默认计划为 60 秒），不使用该 step 的 600 秒脚本预算；超时按 cleanup 错误停止会话。

## 5. 路径相对规则

假设计划中写：

```toml
[roots.musl]
path = "/mnt/contest/musl"
cwd = "/"
mounts = ["dev", "proc", "tmp"]
env = { PATH = "/bin:/usr/bin:/", LD_LIBRARY_PATH = "/lib" }
```

| 写在配置里的路径 | 实际含义 |
|---|---|
| `root.path = "/mnt/contest/musl"` | 执行器看到的比赛盘子目录 |
| `cwd = "/basic"` | 切换根之后测试程序看到的 `/basic` |
| `exec = ["./getpid"]` | 相对当前 `cwd` 执行 |
| `exec = ["/basic/getpid"]` | 相对测试 root 的绝对路径，不是系统辅助盘的 `/basic` |
| `directory = "/ltp/testcases/bin"` | 绝对路径按测试 root 解析；相对路径按有效 `cwd` 解析 |
| `catalog = "/ltp/runtest/syscalls"` | 测试 root 内的清单文件 |
| `files = ["sets/basic.toml"]` | 相对主计划文件所在目录，与测试 root 无关 |

有效工作目录的优先级为 step 的 `cwd`、group 的 `cwd`、root 的 `cwd`。环境变量按 root、group、step 的顺序按键覆盖。`musl`、`glibc` 只是配置中的根名字，程序与库来自 `path`、`exec` 和 `env` 的实际取值。

测试 root 不是写入隔离机制：用例会修改比赛盘中对应目录的文件。

## 6. 根的挂载与准备

`root.mounts` 只接受 `dev`、`proc`、`tmp`，分别在测试根内挂载 devfs、procfs 与 tmpfs。该 root 激活时，执行器先运行 `root.prepare` 中的命令，再挂载这些目录；切换到下一个 root 或结束会话时按相反顺序卸载。

`prepare` 是显式的环境准备，在父系统根 `/` 中执行，可直接操作挂载在 `/mnt/contest` 的比赛盘工作副本；默认计划的链接与复制只改这份副本，原始比赛镜像保持不变。部分旧版比赛盘的 `musl`、`glibc` 子目录缺少程序 `PT_INTERP` 请求的解释器路径，默认计划用链接和复制补齐（节选）：

```toml
prepare = [
    ["/bin/busybox", "ln", "-sfn", "lib", "/mnt/contest/musl/lib64"],
    ["/bin/busybox", "ln", "-sf", "libc.so", "/mnt/contest/musl/lib/ld-musl-riscv64.so.1"],
    ["/bin/busybox", "ln", "-sf", "libc.so", "/mnt/contest/musl/lib/ld-musl-loongarch-lp64d.so.1"],
]
```

每个准备命令的退出码必须为 0，否则该 root 激活失败并按执行错误处理。准备超时使用 `defaults.timeout_sec`。路径以实际比赛盘为准；执行器不会自动猜测解释器、复制系统盘中的库或改写盘内文件。

## 7. 预览与临时筛选

```text
contest-run DEVICE [--plan FILE] [--file FILE]... [--list] [--only PATTERN]... [--timeout-sec N] [--repeat N] [--verbose] [--finish poweroff|return]
contest-run DEVICE --root ROOT --cwd CWD -- COMMAND [ARG...]
```

`DEVICE` 是承载比赛文件的块设备，例如 `/dev/vda`。进入客体 Shell 后先预览：

```sh
make run-qemu ARCH=riscv64 QEMU_ARGS='--shell'
# 客体 Shell：
/sbin/contest-run /dev/vda --list
```

预览每组输出一行组信息，组内每个用例输出一行：

```text
GROUP basic / musl path=/mnt/contest/musl
basic:musl:brk cwd=/basic timeout=60s repeat=1 argv=["./brk"] source=sets/basic.toml group 1 step 1
basic:musl:getpid cwd=/basic timeout=60s repeat=1 argv=["./getpid"] source=sets/basic.toml group 1 step 1
```

每行包含完整 ID、工作目录、超时、重复次数、未拆分的 argv 数组和来源位置。`--list` 只挂载设备并解析、校验选择和磁盘文件，不运行 `prepare`、setup、cleanup、内核通知或 `finish`，也不执行测试程序。它证明选择结果，不证明程序与内核 ABI 能运行。

只运行一例：

```sh
/sbin/contest-run /dev/vda --only 'basic:musl:getpid' --finish return
```

临时运行一组 LTP（该集合必须已在计划中启用）：

```sh
/sbin/contest-run /dev/vda --only 'ltp:musl:open*' --finish return
```

`--only` 匹配完整 ID，可以重复给出，并且只缩小当前计划，不会把未选中的用例加回来。模式加引号，避免 Shell 自行展开 `*`。

临时调整超时与重复并查看诊断：

```sh
/sbin/contest-run /dev/vda \
  --only 'basic:musl:mmap' \
  --timeout-sec 120 --repeat 3 --verbose --finish return
```

命令行参数只作用于本次运行，不写回 TOML。`--timeout-sec` 和 `--repeat` 必须为正整数，优先于计划中的对应字段，作用范围为全部已选用例，不改变 setup、cleanup 与 prepare 的超时。`--verbose` 额外打印每次执行的退出状态与耗时、未命中的 override 以及最终状态。

`--plan` 指定另一份主计划；`--file` 替换主计划的 `files`，不是追加，多个 `--file` 按给出顺序加载，主计划的 `roots`、`defaults` 和 `finish` 保留：

```sh
/sbin/contest-run /dev/vda --plan /etc/contest/onsite-plan.toml --list
/sbin/contest-run /dev/vda --file sets/ltp.toml --list
```

`--finish return` 跳过关机，执行完直接返回；`--finish poweroff` 覆盖计划并在结束时关机。正式运行使用计划中的 `finish`。

## 8. 单个用例的覆盖

先用 `--list` 找到完整 ID，再在 `overrides/local.toml` 中按 ID 覆盖：

```toml
[[override]]
id = "ltp:musl:some_case"
exec = ["/ltp/testcases/bin/some_case", "-n", "100"]
timeout_sec = 120
env = { LTP_TIMEOUT_MUL = "4" }
```

可以修改 `exec`、`cwd`、`env`、`timeout_sec`、`repeat`、`setup`、`cleanup`、`before`、`after`。不能修改 root、group 或 ID。`arch` 非空时只对列出的架构生效：

```toml
[[override]]
id = "ltp:musl:getrusage03"
arch = ["loongarch64"]
timeout_sec = 450
env = { LTP_TIMEOUT_MUL = "14" }
```

覆盖项按声明顺序应用，后者覆盖前者同一字段；`env` 按键合并。覆盖只作用于已被选中的用例，不会把用例重新加入计划。目标未被选中时，`--list` 与 `--verbose` 提示 `override {item} matched no selected case`，运行本身不受影响。`overrides/ltp-environment.toml` 是没有启用的示例。

有些用例不能在 tmpfs 中运行，需要真实 ext4 目录，可以用 setup 和 cleanup 准备、删除：

```toml
[[override]]
id = "ltp:musl:sendfile09"
env = { TMPDIR = "/scratch/sendfile09" }
setup = [["/busybox", "mkdir", "-p", "/scratch/sendfile09"]]
cleanup = [["/busybox", "rm", "-rf", "/scratch/sendfile09"]]
```

该写法要求 `/scratch` 已经是容量足够的可写 ext4；`mkdir` 不会把 tmpfs 变成 ext4。需要 `LTP_DEV` 的用例必须使用可格式化的专用测试设备，不能指向承载比赛文件的系统盘。

外层超时与 LTP 自身的内部超时无关。只改 `timeout_sec` 不会修改 LTP 的内部时限，需要时同时设置 `LTP_TIMEOUT_MUL`。

## 9. LTP 目录模式与清单模式

目录模式见 3.3 节，适合可以直接执行的 ELF。需要官方参数、别名或 Shell 操作的条目使用已有 runtest 清单：

```toml
[[group.step]]
catalog = "/ltp/runtest/syscalls"
include = ["open*", "fork*", "waitpid*"]
exec = ["/busybox", "sh", "-c", "{command}"]
before = "RUN LTP CASE {name}\n"
after = "FAIL LTP CASE {name} : {status}\n"
```

清单每行第一个空白分隔词是名字，其余部分是 `{command}` 原文；`{command}` 整段作为一个参数传入，不按空格重新拆分。此处显式使用盘内 `/busybox sh`，不能假定测试根中存在 `/bin/sh`。

`sets/ltp.toml` 与 `sets/ltp-catalog.toml` 是两种替代方式，不能同时启用而生成同一个 ltp/root 分组。选择清单模式前先确认镜像内确实有该清单文件；不存在时执行器报错，不会回退去扫描目录。

## 10. 现场新盘与显式 loader

现场盘只有几个 ELF 时，新建一份计划与其集合。例如盘内为 `/tests/task_a.elf`、`/tests/task_b.elf` 和 `/runtime/lib`：

```toml
files = ["sets/onsite.toml"]
finish = "poweroff"

[defaults]
timeout_sec = 60
repeat = 1

[roots.onsite]
path = "/mnt/contest"
cwd = "/tests"
mounts = ["dev", "proc", "tmp"]
env = { PATH = "/bin:/usr/bin", LD_LIBRARY_PATH = "/runtime/lib", HOME = "/" }
```

```toml
[[group]]
name = "onsite"
roots = ["onsite"]

[[group.step]]
names = ["task_a.elf", "task_b.elf"]
exec = ["./{name}"]
```

没有给定评测包装格式时不要编造 `start`/`end`；正式评测要求什么文本就填什么文本，ELF 自己输出的内容不要重复。

正常动态 ELF 按自身的 `PT_INTERP` 路径加载，`LD_LIBRARY_PATH` 只是共享库搜索路径，不替代解释器。盘内提供了解释器时可以显式运行它：

```toml
[[group.step]]
arch = ["riscv64"]
names = ["task_c.elf"]
exec = [
  "/runtime/ld-linux-riscv64-lp64d.so.1",
  "--library-path", "/runtime/lib",
  "/tests/{name}", "--size", "64"
]
```

loader 的路径与选项以实际镜像为准。显式 loader 启动与普通 ELF 启动的辅助向量等细节不完全相同，只在确实需要时使用。找不到解释器时核对 root 内实际路径，不要用系统盘的 libc 覆盖比赛盘。

用本地镜像试新盘：

```sh
make contest ARCH=riscv64 QEMU_ARGS='--contest-image /path/to/new.img'
```

随包系统盘中的默认计划必须已经选择该现场配置。临时 `--plan` 只影响当次手动运行；正式提交的入口以 `userland/contest/plan.toml` 为准。

## 11. 外部原脚本

仓库外的原脚本按整段工作接入。以下按完整 Debian 根与 `/scripts` 位置举例，实际发布盘的目录布局需要自行核对：

```toml
[[group]]
name = "cagent"
roots = ["final"]

[[group.step]]
names = ["script"]
exec = ["/bin/bash", "/scripts/cagent_testcode.sh"]
timeout_sec = 120
```

CAgent 的参考脚本使用 Bash 数组，不能随意改成 BusyBox `sh`。工作目录应让脚本中的 `./busybox`、`./agent_lite` 等相对路径能够找到程序。

BuildStorm 的参考脚本自行设置环境、挂载、构建、计时并检查产物，需要整体执行，外层 `timeout_sec` 应覆盖整段工作：

```toml
[[group]]
name = "buildstorm"
roots = ["debian"]

[[group.step]]
names = ["script"]
exec = ["/bin/sh", "/scripts/buildstorm_testcode.sh"]
timeout_sec = 18000
cleanup = [["/bin/sh", "-c", "for p in /sys /dev /proc; do if mountpoint -q \"$p\"; then umount \"$p\" || exit; fi; done"]]
```

脚本自行挂载时，对应的 root 不要再声明相同挂载，cleanup 只清理脚本创建的挂载点。脚本输出的标记与结果行由脚本自己负责，执行器不改写。

## 12. 输出、退出码与结果

比赛路径不捕获、不解析、不转发测试程序的输出：标准输入、标准输出和标准错误直接继承。每个用例在独立的会话与进程组中运行，没有控制终端，因此 `open("/dev/tty")` 返回 `ENXIO`，`more` 之类依赖终端的命令按非交互方式输出，不会被终端作业控制停住。测试程序与脚本打印的正文、组标记和结果行原样出现在串口。执行器按配置向标准输出写入组 `start`/`end` 与用例 `before`/`after` 文本，自身诊断写到标准错误，每行形如：

```text
contest-run: basic:musl:mmap: timed out after 120s
```

正常结束时没有汇总表或结果摘要；`--list` 与 `--verbose` 才输出面向人的表格和状态。

计划模式退出码：

| 退出码 | 含义 |
|---|---|
| 0 | 编排完成，没有执行错误 |
| 1 | 存在执行错误：启动失败、超时、信号终止、setup 失败、清理、挂载或 `finish` 失败等 |
| 2 | 命令行、配置或选择错误 |

单命令模式透传主命令的 shell 风格状态：正常退出取原退出码，信号为 `128 + signal`，超时为 124，命令不存在为 127，权限或格式错误为 126，其他执行错误为 125。

退出码 0 只表示调度执行完成，不是成绩。普通非零退出不是执行错误，也不由执行器判定测试失败；测试是否通过由对应的外部 judge 根据原始输出判断。`FAIL LTP CASE open01 : 0` 同样是原脚本要求的结束行：其中 `0` 是进程状态，不是执行器给出的失败结论。

## 13. 错误排查

| 现象 | 先检查什么 |
|---|---|
| 改了名单却仍运行旧测试 | 是否重新打包；是否显式使用了 `reuse` |
| `--only` 没有匹配 | 先 `--list`；ID 不是单纯程序名；该集合是否在 `files` 中 |
| `include` 找不到文件 | `root.path`、`cwd`、`directory` 与当前架构是否正确 |
| 文件存在却启动失败 | 测试 root 内的解释器、共享库、执行权限；不要只看宿主路径 |
| Libctest 退出 1 | 不据此判断失败；看原始输出与外部 judge |
| LTP 提前超时 | 是否为 LTP 内部时限；需要时调整 `LTP_TIMEOUT_MUL` |
| 测试做完不退出 | 是否有后台服务或脱离进程组的守护进程；脚本与 cleanup 是否等待并结束它 |
| 分数被覆盖或少了一半 | 同一 group/root 是否打印了多次同名分组，例如拆开的 dynamic/static |
| 编排返回 0 | 只说明调度完成，不代表所有测试通过 |

## 14. 内核专属处理

每个展开后的用例在执行前自动向执行器所在系统根中的 `/dev/testhook` 写入 `before <case-id>`，执行结束后写入 `after <case-id>`，不需要在配置中开启任何选项。设备缺失或写入失败按执行错误处理并停止会话；进程控制失败导致会话停止时，不写 `after` 通知。

内核在 [contest_hooks.rs](../os/src/contest_hooks.rs) 中按 case ID 匹配专属行为；未匹配的 ID 直接返回成功，不执行任何操作。新增内核特例时在该文件添加普通匹配分支，不改测试选择、TOML 字段或执行器配置。
