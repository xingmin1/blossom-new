# 比赛测试执行手册

> 命令执行根已统一：prepare、setup、主命令与 cleanup 各自选择 `system`（执行器自己的系统根）或 `test`（所选测试根）。数组简写恒表示 `test`，`roots.NAME.prepare` 必须写完整对象，旧数组会在加载阶段报迁移错误。完整规则、group 级 setup/cleanup、`--list` 的命令明细和 LA musl 调度兼容见[显式执行根手册](contest-run-roots.md)。

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

默认构建使用仓内匹配的 locale 和官方容器中的运行库；其他本地工具链的覆盖参数见[运行库来源](../userland/runtime/sources.md)。

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
| `fresh` | 重新复制系统盘和比赛盘，源镜像保持不变；此前对工作副本的修改全部丢弃 |
| `reuse` | 直接复用工作目录中已有的副本，不读取源镜像，也不自动更新其中的配置；比赛工作盘里此前的修改原样保留 |
| `refresh-system` | 只用最新系统源镜像替换系统工作盘，保留比赛工作盘的已有数据（含此前的修改） |

例如在交互式会话中明确指定策略：

```sh
make run-qemu ARCH=riscv64 QEMU_ARGS='--working-disk-action fresh'
make run-qemu ARCH=riscv64 QEMU_ARGS='--working-disk-action refresh-system'
```

项目 QEMU 启动工具使用比赛盘工作副本，原始比赛镜像不被写入；默认计划的准备和被测程序可能修改工作副本。runner 本身不隔离写入，自行指定设备或编写 `system` 命令时必须确保目标是工作副本，不能把原镜像直接映射为可写设备。`reuse` 与 `refresh-system` 保留比赛工作盘的改动，`fresh` 才从原始镜像重新开始。默认计划会向 glibc 工作副本补缺失的 libgcc 和 locale；唯一改变已有运行库内容的步骤是 LoongArch musl 的 `cyclictest:musl:script` 兼容 override，见[显式执行根手册](contest-run-roots.md)第 9、10 节。

## 2. 修改哪个文件

| 想做什么 | 修改哪里 |
|---|---|
| 选择启动后运行哪些集合 | `userland/contest/plan.toml` 的 `files` |
| 增减 Basic | `userland/contest/sets/basic.toml` 的 `include` |
| 增减 Libctest selector | `userland/contest/sets/libctest.toml` 的 `names` |
| 选择 LTP 用例 | `userland/contest/sets/ltp.toml` 的 `include`/`exclude` |
| 调整单例的超时、参数、环境或准备命令 | `userland/contest/overrides/local.toml`；`mount`/`umount` 使用 `overrides/basic-mount.toml` |
| 在选中用例之前或之后执行准备、清理命令 | step 或 override 的 `setup`/`cleanup`；整组共用一次的用 group 的 `setup`/`cleanup`。写法见[显式执行根手册](contest-run-roots.md) |
| 启用或关闭 LA musl 调度兼容 | `plan.toml` 的 `override_files` 是否引用 `overrides/la-musl-sched.toml`；改后重新打包，并用 `fresh` 工作盘启动 |
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
    "sets/cyclictest-aligned.toml",
    "sets/ltp.toml",
]
override_files = ["overrides/local.toml", "overrides/basic-mount.toml", "overrides/la-musl-sched.toml"]
finish = "poweroff"
```

从默认计划中删除某个集合文件对应的一行，就停止运行该集合。配置支持 TOML 注释和多行数组，没有 Hook、profile 或“优化名称”之类的开关。

默认计划在两个 libc 根目录各准备一份 64 MiB FAT32 镜像及项目自带 BusyBox。`basic` 的 `mount`、`umount` 使用 `/dev/loop0` 作为原 ELF 的设备参数；每例开始前绑定镜像、结束后解绑，挂载点仍为原来的 `./mnt`。

若目标位置已有同名镜像，准备命令会先截断旧文件，再完整复制新镜像；整条命令仍受默认的 60 秒准备期限约束。

默认计划在比赛根目录的工作副本中补齐缺失的命令和输入：`sh`、`cp`、`sleep`、`true`、`ls` 链接到项目 BusyBox；盘内 lmbench 脚本引用的绝对路径链接同盘 `lmbench_all`；协议名称表提供 `ip`、`icmp`、`tcp`、`udp` 的标准编号。UnixBench 的 `sort.src` 使用[上游固定输入](https://github.com/kdlucas/byte-unixbench/blob/f53eadaa3e628908fa42b2167335459e970e5ebb/UnixBench/testdir/sort.src)，许可证随文件保存在 `userland/contest/fixtures/UNIXBENCH-LICENSE`。已有文件或链接会被保留；这些准备不会修改原始比赛盘、测试程序或脚本。

两个 root 都先复制项目 BusyBox 到 `/.blossom/busybox`，再补缺失的标准 `/bin/sh` 入口。项目 ash 遇到无 shebang 文本时，通过 `/bin/busybox` 重新执行盘内 BusyBox 的 ash；原 `/busybox` 和 `/bin/busybox` 的准备方式不变，显式 `/busybox sh` 仍执行盘内原 shell。已有 `/bin/sh`（包括失效符号链接）不会被替换；复制或链接失败会按准备错误返回。

glibc 根另外通过 `/sbin/prepare-glibc-runtime` 补缺 `lib/libgcc_s.so.1` 和架构对应路径的 `C.utf8` 数据。来源为系统盘 `/opt/runtime/glibc`；需要补入时，测试根的 `libc.so.6` 和加载器 SHA-256 必须匹配系统盘内的身份记录。已有有效目标保留；失效链接，或需要补入时身份记录无效、不匹配，都会使 root 准备失败。该步骤修改比赛工作副本，不修改原盘和原测试 ELF；参赛环境是否允许此准备仍需由赛事规则确认。

更新系统盘中的计划后，使用 `fresh` 才会从原比赛盘重新准备缺失的入口。`reuse` 不更新系统工作盘中的计划，`refresh-system` 虽更新计划但保留比赛工作盘；两者都不会替换比赛工作盘中已有的旧 `/bin/sh` 链接。

默认计划还含一项显式的运行库兼容变更：仅在 LoongArch 上选中 `cyclictest:musl:script` 时，override 的 `setup` 以 `root = "system"` 运行系统盘的 `/sbin/prepare-la-musl-sched`，修补比赛工作副本中已知版本 musl `libc.so` 的 `sched_getparam`、`sched_getscheduler`、`sched_setparam`、`sched_setscheduler` 四个函数。它不属于上面“补缺并保留已有文件”的准备：原比赛镜像不变，但工作副本里该 libc 的字节、语义和 inode 都会改变，同一 musl 根里之后启动的动态程序也会看到新语义；库的版本未知时该例的 setup 失败，不运行测量；`reuse` 与 `refresh-system` 会保留已修补的库，恢复原库须关闭该 override 并使用 `fresh`。赛事是否允许修改工作盘里的运行库尚未确定。详见[显式执行根手册](contest-run-roots.md)第 10 节。

如需只运行 `mount`、`umount`，可显式选择独立计划：

```sh
/sbin/contest-run /dev/vda --plan /etc/contest/mount-plan.toml
```

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

`include`、`exclude` 和 `--only` 支持 `*`、`?`、字符集合等 glob。执行顺序为：`files` 中的文件顺序、组声明顺序、`roots` 顺序、step 顺序。每个 root 内完整执行该组的全部 step，然后切换到下一个 root。每个非空的（组，root）实例开始前执行一次 group 的 `setup`，结束后执行一次 group 的 `cleanup`；用例自己的 `setup`、`cleanup` 则每次执行（含 `repeat` 的每一次）都会运行。同一组内不同 step 的候选不会合并去重。

`repeat` 在 `defaults`、step 或 override 上配置，表示每个用例连续执行的次数；`--repeat` 覆盖本次运行的所有已选用例。

## 4. 原脚本集合与 cyclictest

BusyBox、Lua、Iozone、UnixBench、iperf、libcbench、lmbench、netperf 和 cyclictest 在盘内各有一份原测试脚本。原脚本集合使用单个 `script` 名字，并用盘内 BusyBox 的 `sh` 执行；默认 cyclictest 使用带 `-A` 的同步测量，见[同步采样检查](#62-同步采样检查)。

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
| `cyclictest`（显式原参数） | `/cyclictest_testcode.sh` | 600 |

这些原脚本自带组起止标记与结果行，group 不写 `start`/`end`，避免输出两套标记。默认计划中的 cyclictest 是例外：它运行带 `-A` 的四项测量，并由集合输出组和结果标记。`timeout_sec` 是整个脚本的外层预算，应按脚本实际耗时设置，不能套用普通短用例的 60 秒。

比赛盘内的 RISC-V musl Iozone 3.506 对原脚本的 `-i 11 -i 12` 参数，在 Linux 下也提示 `Selected test not available on the version`，随后输出 initial writers 和 rewriters 测量值。评分器能够读取这两项输出，但这些分数不能证明 pwritev/preadv 模式实际运行。

`iperf` 集合额外配置 `cleanup`：盘内脚本用 `iperf3 -s -D` 启动服务端，守护进程脱离主命令的进程组，脚本结束时也不停止它，主命令收尾的整组 `SIGKILL` 到不了它。`sets/iperf.toml` 直接用 `/busybox sh /iperf_testcode.sh` 原样执行脚本；cleanup 用数组简写，即在同一个测试 root 内用盘内 BusyBox 执行 `killall iperf3`，再用 `pidof` 每秒检查一次，直到服务端退出后才切换 root，否则它打开的 `/dev` 文件会让卸载返回 Busy。

`killall`/`pidof` 依赖内核的 `/proc/<pid>/stat` 与 `/proc/<pid>/cmdline` 查询。系统根 BusyBox 未启用这两个 applet，cleanup 必须使用比赛根内的 `/busybox`。

cleanup 单条命令的超时取 `defaults.timeout_sec`（默认计划为 60 秒），不使用该 step 的 600 秒脚本预算；命令用完整对象写了自己的 `timeout_sec` 时以它为准。超时按 cleanup 错误停止会话。

## 5. 路径相对规则

假设计划中写：

```toml
[roots.musl]
path = "/mnt/contest/musl"
cwd = "/"
mounts = ["dev", "shm", "proc", "tmp"]
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
| `exec = { root = "system", argv = ["/bin/busybox", "true"] }` | 不换根，路径按执行器自己的系统根 `/` 解析 |
| `argv` 里的 `{test_root}`（常用于 `root = "system"` 的命令） | 选择阶段单次展开成 `roots.NAME.path`，仍是普通字符串，不改写、不追踪符号链接 |
| `catalog = { root = "system", path = "/etc/contest/selected.txt" }` | 系统根内的清单；只决定读取位置，不改变命令的执行根，`system` 来源必须写绝对路径 |
| `files = ["sets/basic.toml"]` | 相对主计划文件所在目录，与测试 root 无关 |

有效工作目录的优先级为 step 的 `cwd`、group 的 `cwd`、root 的 `cwd`。环境变量按 root、group、step 的顺序按键覆盖。这是 `test` 命令（数组简写）的默认值；完整对象可用自己的 `cwd`、`env` 再覆盖，`env` 按键合并。`root = "system"` 的命令不继承这些默认值：cwd 默认为 `/`，环境默认为空，需要的变量写在该命令的 `env` 里。`musl`、`glibc` 只是配置中的根名字，程序与库来自 `path`、`exec` 和 `env` 的实际取值。

测试 root 不是写入隔离机制：用例会修改比赛盘中对应目录的文件。

## 6. 根的挂载与准备

`root.mounts` 接受下列显式项目，不自动补项、排序或合并挂载：

| 项目 | 测试根内的挂载点 | 文件系统 |
|---|---|---|
| `dev` | `/dev` | devfs |
| `shm` | `/dev/shm` | 独立 tmpfs |
| `proc` | `/proc` | procfs |
| `tmp` | `/tmp` | 另一份独立 tmpfs |

该 root 激活时，执行器先运行 `root.prepare` 中的命令，再按 `mounts` 的声明顺序挂载。默认 musl、glibc 和显式单命令模式均使用 `["dev", "shm", "proc", "tmp"]`，切换 root 或结束会话时按 `tmp`、`proc`、`shm`、`dev` 的顺序释放。成功挂载才加入现有的 owned 栈；中途失败时，已经成功的挂载仍由该栈逆序释放。卸载遇到 `EBUSY` 立即返回带挂载点的真实错误，保留尚未卸载的所有权，不使用强制或 lazy 卸载，也不将失败记为成功。

`shm` 必须单独声明；`dev` 只挂 devfs，其固定 `/shm` 目录提供挂载位置，不会隐含挂载 tmpfs。需要同时挂载两者时应把 `dev` 放在 `shm` 前，避免后挂的 `/dev` 遮住子挂载。旧配置 `["dev", "proc", "tmp"]` 仍只建立原来三项；省略 `mounts` 或写 `[]` 不建立任何伪文件系统。无需 POSIX 共享内存的配置不必增加 `shm`。

Linux 的 POSIX 共享内存 `shm_open`、`shm_unlink` 使用 `/dev/shm` 中的普通文件，需要用户可见的 tmpfs；SysV 的 `shmget`、`shmat`、`shmctl` 使用内核自己的共享内存对象，不依赖这个挂载。参见 [Linux tmpfs 说明](../ref/linux/Documentation/filesystems/tmpfs.rst)。挂载 `/tmp` 或实现 SysV 共享内存都不能替代 `/dev/shm`；这里不修改测试程序，也不改变其他系统调用的真实返回值。

`mlock` 和 `munlock` 对当前地址空间执行真实锁页与解锁，`RLIMIT_MEMLOCK` 默认软硬限额均为 8 MiB；当前凭据模型以 UID 0 表达锁页特权。`mlockall`、`mlock2` 和独立的 `CAP_IPC_LOCK` 尚未实现，不能将上述支持外推为完整锁页接口。

`prepare` 是显式的环境准备：每条命令都写成完整对象，用 `root` 选择执行根，旧的裸数组会在加载阶段报迁移错误（见[显式执行根手册](contest-run-roots.md)第 7 节）。下列默认计划命令写 `root = "system"`，在父系统根 `/` 中执行，可直接操作挂载在 `/mnt/contest` 的比赛盘工作副本；默认计划的链接与复制只改这份副本，原始比赛镜像保持不变。部分旧版比赛盘的 `musl`、`glibc` 子目录缺少程序 `PT_INTERP` 请求的解释器路径，默认计划用链接和复制补齐（节选）：

```toml
prepare = [
    { root = "system", argv = ["/bin/busybox", "ln", "-sfn", "lib", "/mnt/contest/musl/lib64"] },
    { root = "system", argv = ["/bin/busybox", "ln", "-sf", "libc.so", "/mnt/contest/musl/lib/ld-musl-riscv64.so.1"] },
    { root = "system", argv = ["/bin/busybox", "ln", "-sf", "libc.so", "/mnt/contest/musl/lib/ld-musl-loongarch-lp64d.so.1"] },
]
```

每个准备命令的退出码必须为 0，否则该 root 激活失败并按执行错误处理，后续挂载和用例不再运行。准备命令默认使用 `defaults.timeout_sec`，完整对象可用自己的 `timeout_sec` 覆盖；命令行 `--timeout-sec` 不改变它。挂载失败也立即停止当前会话，并报告实际路径和系统错误。文件、命令与链接的补缺只由 `prepare` 中的明确 argv 执行：第 2 节列出的补缺入口保留已有文件及链接，而解释器链接、loader 和 `/.blossom` 副本仍按各自命令执行。glibc 的 libgcc/locale 安装对失效目标链接直接报错。执行器不自动猜测解释器，不用系统盘的 libc 覆盖比赛盘，也不改写原测试脚本、测试程序或 judge；唯一改变已有比赛盘运行库内容的是被显式选中的 LA musl 兼容 override，见第 2 节与[显式执行根手册](contest-run-roots.md)第 10 节。

### 6.1 定向验证入口

以下是验证命令，不代表已经执行。宿主合同检查使用包内离线依赖：

```sh
(cd userland/contest-run && cargo test --locked --offline root_mounts)
```

该筛选覆盖 shm 重复项和非法路径名称拒绝、空挂载列表不隐式创建目录、准备失败与目标路径错误及其先后顺序。真实 tmpfs 挂卸载及 Busy 检查被显式标为 ignored；在允许用户命名空间的 Linux 宿主运行：

```sh
(cd userland/contest-run && \
  unshare --user --map-root-user --mount --propagation private \
  cargo test --locked --offline --test runtime root_mounts -- --ignored --test-threads=1)
```

这两个用例在独立私有挂载命名空间中使用真实 tmpfs，检查子挂载逆序释放、`EBUSY` 后所有权保留和部分准备失败后的释放；它们用父 tmpfs 检查通用挂载生命周期，不替代客体 devfs 验证。宿主禁止命名空间或缺少挂载权限时命令会真实失败，普通合同测试不会将这两个 ignored 用例算作通过。

重新构建并按第 1 节使用 `fresh` 工作盘进入客体 Shell 后，可以检查显式单命令的可写共享内存目录，再运行两个 libc 根内的原 cyclictest 脚本：

```sh
/sbin/contest-run /dev/vda --root /musl --cwd / -- /busybox sh -c \
  'test -d /dev/shm && printf "shared memory\n" > /dev/shm/runner-check && /busybox cat /dev/shm/runner-check'
/sbin/contest-run /dev/vda --file sets/cyclictest.toml --only 'cyclictest:musl:script' \
  --only 'cyclictest:glibc:script' --finish return
```

前一条只检查文件路径、写入与命令后的正常释放；后一条才执行原程序的 `shm_open` 路径。在 LoongArch 上后一条还会先运行 `cyclictest:musl:script` 的兼容 setup，修改当前工作副本里的 libc；需要从原库开始时使用 `fresh` 工作盘。应保留原始输出与准备、挂载或卸载错误，并交给对应原 judge 判定，不用 runner 退出零代替程序成功或得分。

### 6.2 同步采样检查

默认计划使用 `sets/cyclictest-aligned.toml`，在每个 libc 根下依次运行无压力、压力的 P1 和 P8 四项；每项均使用原测试盘中的 `cyclictest`，在原参数中加入 `-A`。压力项使用原 `hackbench -l 100000000`。输出同时包含评分器可识别的四项结果和采样检查标记；原判题器可计算这些实际测量的分数，但 `-A` 改变了唤醒相位，不能称为原参数的官方成绩，也不能保证固定分数。

同步集合沿用 `cyclictest:{root}:script` 用例 ID 以复用现有准备步骤，不能与原参数的 `sets/cyclictest.toml` 同时作为 `--file` 加载；两种结果应分别运行和保存。要运行原盘脚本时显式选择 `--file sets/cyclictest.toml`。

重新装配系统盘并使用独立的 `fresh` 工作盘，在客体 Shell 中运行：

```sh
/sbin/contest-run /dev/vda --only 'cyclictest:musl:script' --only 'cyclictest:glibc:script' --finish return; status=$?
printf 'CYCLICTEST_ALIGNED SESSION %s\n' "$status"
```

将本次完整串口原文保存为宿主文件后，执行：

```sh
python3 tools/check_cyclictest_aligned.py /path/to/uart.log
```

检查器默认要求 musl 和 glibc 各有完整的无压力、压力 P1/P8 四项：P1 必须有线程 0 的正采样记录，P8 必须包含线程 0–7 各一条正采样记录。它还检查程序退出状态、压力进程的启动等待、信号与回收状态，以及组和测例标记的完整性。`SESSION` 必须在 `contest-run` 完全返回后记录其真实状态，以覆盖用例后通知、卸载和同步错误；默认自动关机不会输出此手动标记。缺样、零采样、非零状态或输出截断返回 1；文件读取或参数错误返回 2。仅使用 `--only 'cyclictest:musl:script'` 运行 musl 时，检查器须同时指定 `--root musl`；glibc 同理。

计划模式的 `contest-run` 退出 0 只表示编排完成，普通非零子进程退出不会自动使它失败；**以检查器的退出码判定采样结果**。`-A` 不能保证在任意负载下每个线程都有样本，超时或采样不完整必须保留为失败。LoongArch musl 仍复用第 2 节的运行库兼容准备。

检查器按一次会话核对：同一 libc 的组及四项测量不得重复。不要对该集合使用 `--repeat`；多次运行时分别保存和检查独立串口日志。

检查器只读取日志，不负责启动或监管 QEMU；还需分别确认本次虚拟机退出和工作盘释放。

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

预览每组输出一行组信息；root 的 `prepare` 与 group 的 `setup`、`cleanup` 按执行位置列出；每个用例先给出一行 `CASE`（完整 ID、重复次数、来源位置），其下列出它的 `setup`、`exec`、`cleanup` 命令。输出形如下例：

```text
GROUP basic / musl path=/mnt/contest/musl
  root.prepare root=system path=/ cwd=/ timeout=60s argv=["/bin/busybox", "ln", "-sfn", "lib", "/mnt/contest/musl/lib64"] env={}
  ……（其余 prepare 命令）
  CASE basic:musl:brk repeat=1 source=/etc/contest/sets/basic.toml group 1 step 1 (.)
    exec root=test path=/mnt/contest/musl cwd=/basic timeout=60s argv=["./brk"] env={"HOME": "/", "LD_LIBRARY_PATH": "/lib", "PATH": "/ltp/testcases/bin:/bin:/usr/bin:/"}
  ……（其他用例）
  CASE basic:musl:getpid repeat=1 source=/etc/contest/sets/basic.toml group 1 step 1 (.)
    exec root=test path=/mnt/contest/musl cwd=/basic timeout=60s argv=["./getpid"] env={"HOME": "/", "LD_LIBRARY_PATH": "/lib", "PATH": "/ltp/testcases/bin:/bin:/usr/bin:/"}
```

每条命令一行，包含执行根（`root=system` 或 `root=test`）、真正执行根的路径 `path`（`system` 为 `/`，`test` 为该测试根的路径）、有效 cwd、超时、未拆分的 argv 数组和环境，`{test_root}` 等模板已展开。`--list` 只挂载设备并解析、校验选择和磁盘文件，显示将要运行的命令，但不运行 `prepare`、setup、cleanup、内核通知或 `finish`，也不执行测试程序。它证明选择结果，不证明程序与内核 ABI 能运行。

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

命令行参数只作用于本次运行，不写回 TOML。`--timeout-sec` 和 `--repeat` 必须为正整数，优先于计划中的对应字段，作用范围为全部已选用例。`--timeout-sec` 覆盖主命令的超时，包括 `exec` 对象自己写的 `timeout_sec`，但不改变 prepare、group 与用例的 setup、cleanup 的超时。`--verbose` 额外打印每次执行的退出状态与耗时、未命中的 override 以及最终状态。

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

可以修改 `exec`、`cwd`、`env`、`timeout_sec`、`repeat`、`setup`、`cleanup`、`before`、`after`；其中 `exec`、`setup`、`cleanup` 是整体替换，不是追加。不能修改用例所属的 root、group 或 ID；命令自己的执行根在 `exec`、`setup`、`cleanup` 里用 `root = "system"` 或 `root = "test"` 的完整对象选择，数组简写表示 `test`。`arch` 非空时只对列出的架构生效：

```toml
[[override]]
id = "ltp:musl:getrusage03"
arch = ["loongarch64"]
timeout_sec = 450
env = { LTP_TIMEOUT_MUL = "14" }
```

覆盖项按声明顺序应用，后者覆盖前者同一字段；`env` 按键合并。覆盖只作用于已被选中的用例，不会把用例重新加入计划。目标未被选中时，`--list` 与 `--verbose` 提示 `override {item} matched no selected case`，运行本身不受影响。`overrides/ltp-environment.toml` 是没有启用的示例。

在选中用例之前运行系统根里的准备命令，同样写在 override 的 `setup` 里。默认计划引用的 `overrides/la-musl-sched.toml` 就是这样写的，它只对 LoongArch 的 `cyclictest:musl:script` 生效，修改的内容和风险见[显式执行根手册](contest-run-roots.md)第 10 节：

```toml
[[override]]
id = "cyclictest:musl:script"
arch = ["loongarch64"]

[[override.setup]]
root = "system"
cwd = "/"
argv = ["/bin/busybox", "sh", "/sbin/prepare-la-musl-sched", "{test_root}/lib/libc.so"]
```

`{test_root}` 展开成该用例所属测试根的路径（这里是 `/mnt/contest/musl`）。setup 普通失败时不运行主脚本、计一次执行错误；进程控制、清理及通知均正常时继续下一例，否则停止会话。

有些用例不能在 tmpfs 中运行，需要真实 ext4 目录，可以用 setup 和 cleanup 准备、删除：

```toml
[[override]]
id = "ltp:musl:sendfile09"
env = { TMPDIR = "/scratch/sendfile09" }
setup = [["/busybox", "mkdir", "-p", "/scratch/sendfile09"]]
cleanup = [["/busybox", "rm", "-rf", "/scratch/sendfile09"]]
```

这两条数组简写在测试根内执行。该写法要求 `/scratch` 已经是容量足够的可写 ext4；`mkdir` 不会把 tmpfs 变成 ext4。需要 `LTP_DEV` 的用例必须使用可格式化的专用测试设备，不能指向承载比赛文件的系统盘。

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

清单每行第一个空白分隔词是名字，其余部分是 `{command}` 原文；`{command}` 整段作为一个参数传入，不按空格重新拆分。此处显式使用盘内 `/busybox sh`，不能假定测试根中存在 `/bin/sh`。清单也可以放在系统根里：`catalog = { root = "system", path = "/etc/contest/ltp-selected.txt" }`，命令仍在测试根执行；选择发生在 prepare 之前，清单必须已经存在。

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

比赛路径不捕获、不解析、不转发测试程序的输出：标准输入、标准输出和标准错误直接继承。每个用例在独立的会话与进程组中运行，没有控制终端，因此 `open("/dev/tty")` 返回 `ENXIO`，`more` 之类依赖终端的命令按非交互方式输出，不会被终端作业控制停住。测试程序与脚本的正文、组标记和结果行经终端发送到串口。执行器按配置向标准输出写入组 `start`/`end` 与用例 `before`/`after` 文本，自身诊断写到标准错误，每行形如：

```text
contest-run: basic:musl:mmap: timed out after 120s
```

终端对 basic `pipe` 的输出有一项特判：同一用户线程对同一终端打开实例单独写入 `cpid: ` 后，前缀和后续十进制数字会暂存在该线程，合计最多 32 字节；遇到非数字写入或超过容量时再连续提交。暂存字节在发送前已由 `write` 报告成功；切换打开实例、`exec` 或线程退出时，尚未发送的暂存内容会被丢弃。`TCFLSH` 和 drain 不包含这部分暂存。该行为偏离 Linux，不表示其他多次 `write` 具有整行原子性；执行器本身不修改输出。

正常结束时没有汇总表或结果摘要；`--list` 与 `--verbose` 才输出面向人的表格和状态。

计划模式退出码：

| 退出码 | 含义 |
|---|---|
| 0 | 编排完成，没有执行错误 |
| 1 | 存在执行错误：启动失败、超时、信号终止、prepare、group 或用例的 setup 失败、清理、挂载或 `finish` 失败等 |
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
| 加载时报 prepare 必须写成 `{ root = "system", argv = [...] }` | 残留旧的 `prepare` 数组；按[显式执行根手册](contest-run-roots.md)第 7 节迁移，退出码 2 |
| LA musl cyclictest 的 setup 失败，主脚本没有运行 | 工作副本里的 libc 不是已核验的原版（`unsupported libc SHA256`）、是软链接，或系统 BusyBox 缺 applet；不会回退成不修补直接运行，也不要删除哈希检查硬套偏移，见[显式执行根手册](contest-run-roots.md)第 10 节 |
| 关掉了 LA musl 兼容，行为仍是修补后的库 | 复用了保留改动的比赛工作盘（`reuse` 或 `refresh-system`）；改用 `fresh` |

## 14. 内核专属处理

每个展开后的用例在执行前自动向执行器所在系统根中的 `/dev/testhook` 写入 `before <case-id>`，执行结束后写入 `after <case-id>`，不需要在配置中开启任何选项。设备缺失或写入失败按执行错误处理并停止会话；进程控制失败导致会话停止时，不写 `after` 通知。

内核在 [contest_hooks.rs](../os/src/contest_hooks.rs) 中按 case ID 匹配专属行为；未匹配的 ID 直接返回成功，不执行任何操作。新增内核特例时在该文件添加普通匹配分支，不改测试选择、TOML 字段或执行器配置。
