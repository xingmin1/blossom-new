# 显式执行根与命令配置手册

本手册说明 `/sbin/contest-run` 的统一命令配置：prepare、setup、主命令（`exec`）和 cleanup 各在哪个根里运行，用什么工作目录、环境和超时，以及怎样在选中的用例之前执行准备命令。读完不必阅读 runner 源码就能写配置。测试盘选择、集合写法和退出码见[比赛测试执行手册](contest-run.md)。

文中的构建、宿主测试、QEMU、trace 与评分检查都是待执行的步骤。本手册不记录它们的结果，也不据此宣称通过；实际结果按第 11 节另行留档。

## 1. 只记住两种根

`system` 是 BLOSSOM 客体里 runner 自己的辅助系统根 `/`，不是运行 QEMU 的 Windows、WSL 或 Linux 宿主机。

`test` 是当前 group 选中的测试根。例如 group 的 `roots = ["musl", "glibc"]` 会分别使用 `/mnt/contest/musl` 和 `/mnt/contest/glibc`（即 `roots.NAME.path`，是 `system` 视图里的绝对目录）。`test` 命令先 `chroot` 到该目录再执行；执行器自己始终留在系统根。

| 项目 | 简写（恒为 test） | 显式写法 |
|---|---|---|
| 一条命令 | `["/busybox", "true"]` | `{ root = "test", argv = ["/busybox", "true"] }` |
| 辅助系统命令 | 没有简写 | `{ root = "system", argv = ["/bin/busybox", "true"] }` |
| runner 读取目录或清单 | `"/ltp/runtest/syscalls"` | `{ root = "test", path = "/ltp/runtest/syscalls" }` |
| 系统盘里的名单 | 没有简写 | `{ root = "system", path = "/etc/contest/selected.txt" }` |

- 数组简写永远表示 `test`，写在 prepare、setup、`exec` 还是 cleanup 都一样；放进 setup 或 cleanup 不会变成 `system`。
- 完整对象只接受 `root`、`argv`、`cwd`、`env`、`timeout_sec`。`root` 必填，只能是 `system` 或 `test`；缺 `root`、别的取值或多余字段都是配置错误。
- `roots.NAME.prepare` 必须写完整对象。旧数组会得到明确的迁移错误，不会悄悄改变含义，见第 7 节。

## 2. 日常只改名单

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
```

新增 `clone` 只改 `include`。被执行的程序不在系统根里找，而在当前 `test` 根的 `/basic` 下找。

## 3. 在一个用例之前执行准备命令

仓库里的真实例子是 `userland/contest/overrides/la-musl-sched.toml`，由 `plan.toml` 的 `override_files` 引用：

```toml
[[override]]
id = "cyclictest:musl:script"
arch = ["loongarch64"]

[[override.setup]]
root = "system"
cwd = "/"
argv = ["/bin/busybox", "sh", "/sbin/prepare-la-musl-sched", "{test_root}/lib/libc.so"]
```

只有选中 LoongArch 的 `cyclictest:musl:script` 才执行这条 setup；RISC-V、LoongArch glibc 和其他用例没有它。显式选择 `sets/cyclictest.toml` 时，主命令是：

```toml
exec = ["/busybox", "sh", "/cyclictest_testcode.sh"]
```

它在 `test` 根里执行，脚本内部原样跑四项，不改脚本也不改 ELF。默认计划则使用同一用例 ID 执行带 `-A` 的四项同步测量，见[同步采样检查](contest-run.md#62-同步采样检查)。两种入口都不必给四项分别配置；用例每重复一次就再执行一次 setup，修补脚本发现目标已是修补后的哈希时安静返回。

- 普通 setup 失败：不运行主脚本；若进程控制仍正常，按既有规则尝试本例的 cleanup 与 after 通知，计一次执行错误（状态 1）；只有清理及通知成功时才继续下一个用例。进程控制失效或清理、通知失败会停止会话；不会输出假的成功。不要把它当成“用例没被选中”。
- `override.setup` 整体替换该用例原来的 setup，不是追加。本例原 setup 为空；给已有 setup 的用例加 override 时，要把需要保留的原命令一并写进去。
- 这项 setup 改的是什么、失败边界和许可状态见第 10 节。

## 4. 一组用例共用一次准备

group 的 `setup` 与 `cleanup` 对每个非空的（组，测试根）实例各执行一次，不是每个用例一次：

```toml
[[group]]
name = "my-tests"
roots = ["musl"]

[[group.setup]]
root = "system"
argv = ["/bin/busybox", "mkdir", "-p", "{test_root}/work"]

[[group.step]]
names = ["a", "b", "c"]
exec = ["/tests/{name}"]
cwd = "/work"

[[group.cleanup]]
root = "system"
argv = ["/bin/busybox", "rm", "-f", "{test_root}/work/result.tmp"]
```

- 组只剩一个用例时同样各执行一次；`--only` 筛选后整组为空则都不执行。`roots` 有多个时按各 root 分别执行；不做跨组去重，也没有永久缓存。
- 一个（组，root）实例的完整顺序：

```text
激活 root：先释放上一个 root 的挂载 → root.prepare（首错即停）→ 按 mounts 挂载
group.setup（首错即停）
组开始文本 start
  每个用例、每次 repeat：
    before 通知 → 用例 setup（首错即停）→ before 文本 → 主命令 → after 文本 → 用例 cleanup → after 通知
组结束文本 end
group.cleanup
切换到下一个 root 或会话结束：按挂载的逆序卸载（默认的挂载顺序下即 tmp、proc、shm、dev）
```

- root 与上一个实例相同时不重新激活。默认计划的各组都按 `musl`、`glibc` 交替，几乎每个实例都会重新激活，所以 prepare 命令必须可以重复执行。
- group.setup 失败：不启动组内任何用例，也不写组开始、结束文本；进程控制没有失效时仍运行 group.cleanup，然后停止整个会话（状态 1）。进程控制已经失效（例如无法确认前台命令的进程组已收尾）时，不运行可能破坏现场的清理。
- group.cleanup 的命令按顺序全部尝试（进程控制失效时立即停止），任一失败都使会话以状态 1 停止。
- 挂载仍由既有的 owned 栈拥有并逆序释放：不强制卸载，不用 lazy 卸载，不把失败记为成功。
- 准备和清理命令都是前台同步命令，没有新增后台服务机制；需要多进程配合的流程留在原主脚本里。

## 5. 每条命令自己的 root、cwd、env 和超时

```toml
[[group]]
name = "demo"
roots = ["musl"]

[[group.step]]
names = ["one"]
cwd = "/tests"
env = { LD_LIBRARY_PATH = "/lib", HOME = "/" }
timeout_sec = 60
exec = { root = "test", argv = ["./one"], timeout_sec = 120 }
setup = [
    { root = "system", argv = ["/bin/busybox", "mkdir", "-p", "{test_root}/tmp/data"] },
    { root = "test", argv = ["/busybox", "chmod", "755", "/tmp/data"] },
]
cleanup = [{ root = "test", argv = ["/busybox", "rm", "-f", "/tmp/data/result"] }]
```

| 命令的根 | 默认 cwd | 默认环境 |
|---|---|---|
| `test`（数组简写或 `root = "test"`） | 有效 cwd：step、group、root 依次取第一个已声明值 | root、group、step 的 `env` 按键覆盖 |
| `system` | `/` | 空；不继承测试根的 `LD_LIBRARY_PATH` 等，需要的变量写在该命令的 `env` 里 |

- 完整对象自己的 `cwd`、`env` 再覆盖默认值，`env` 按键合并。
- `cwd` 永远是该命令执行根内的路径：`root = "test"` 而 `cwd = "/etc"` 指测试根的 `/etc`，不是系统的 `/etc`。命令自己的 `cwd` 必须是不含 `..` 的绝对路径；`system` 命令想把工作目录放进测试根时，写 `cwd = "{test_root}/work"`。
- 命令自己的 `timeout_sec` 优先于它的默认超时。主命令的默认超时是该用例的 `timeout_sec`（step、override 或 `defaults`）；prepare、setup、cleanup 的默认超时是 `defaults.timeout_sec`。上例的 `exec` 自带 120 秒，优先于 step 的 60 秒。
- 命令行 `--timeout-sec` 覆盖主命令的超时，包括 `exec` 对象自己写的 `timeout_sec`；它不改变任何 prepare、setup 或 cleanup 的超时，不会悄悄延长准备命令。
- 不存在的文件、程序或解释器真实报错，不回退到另一个根。

## 6. 路径规则：不猜参数，不自动映射

`argv` 中的每一项始终是字符串。`{test_root}` 只做一次文本展开，例如 `/mnt/contest/musl`（完整对象自己的 `cwd` 与 `env` 值里的 `{test_root}` 同样只展开一次）；它不追踪符号链接、不复制文件，也不突破 chroot。`system` 命令通过该路径操作测试根里的文件：

```toml
[[group.setup]]
root = "system"
argv = ["/bin/busybox", "cp", "/etc/contest/input.dat", "{test_root}/work/input.dat"]
```

这条命令必须写 `root = "system"`：`test` 命令看不到系统盘的 `/etc`，不能因为给参数贴一个“system”标签就获得它。需要的文件先明确复制，或经已有的挂载机制提供。

符号链接的内容也不改写：

```toml
prepare = [{ root = "system", argv = ["/bin/busybox", "ln", "-sfn", "lib", "{test_root}/lib64"] }]
```

这里的 `lib` 就是相对链接内容，不是要解析成绝对路径的文件参数。

Shell 的 `-c`、`PATH`、`LD_LIBRARY_PATH`、ELF 解释器和 shebang 都按所执行程序的真实根解释。不要把不可信的目录字符串拼进 Shell 代码；尽量作为独立 argv 传入，空格不会被 runner 再次拆分。嵌在 Shell 源码里的路径不会被 runner 识别。

| 配置位置 | 规则 |
|---|---|
| `roots.NAME.path` | 系统视图中的绝对目录，是 `test` 的定义位置 |
| `group.roots` | 选择哪些命名测试根，决定组实例与用例 ID |
| prepare、setup、`exec`、cleanup | 统一的命令结构，`root` 选择 `system` 或 `test` |
| `cwd` | 命令自己执行根内的路径 |
| `directory`、`catalog` | runner 读取的来源，可单独指定 `system` 或 `test`，不改变命令的执行根 |
| `files`、`override_files` | 系统视图，相对主计划所在目录；不在测试根里找配置 |
| `mounts` | 仍是所选测试根内 `dev`、`shm`、`proc`、`tmp` 的既有含义 |
| `before`、`after`、`start`、`end` | 输出文本，不是路径，不执行 |

目录或清单的例子：

```toml
[[group.step]]
catalog = { root = "system", path = "/etc/contest/ltp-selected.txt" }
exec = ["/busybox", "sh", "-c", "{command}"]
```

名单来自系统盘，命令在 `test` 根执行。`system` 来源必须写绝对路径；`test` 来源的 `directory` 可以相对有效 cwd，`catalog` 仍要求绝对路径。选择发生在准备之前，名单必须已经存在；不支持“先 setup 生成名单、再反过来重新选择”的依赖。

需要另一个命名根里的文件时，用 `system` 命令和那个根明确的系统路径。本合同不自动切换或激活第三个根，也不建立跨根别名或自动 bind mount。

## 7. root.prepare 的迁移

旧写法（现在在加载阶段被拒绝）：

```toml
prepare = [["/bin/busybox", "mkdir", "-p", "/mnt/contest/musl/bin"]]
```

新写法：

```toml
[roots.musl]
path = "/mnt/contest/musl"
cwd = "/"
mounts = ["dev", "shm", "proc", "tmp"]
prepare = [
    { root = "system", argv = ["/bin/busybox", "mkdir", "-p", "{test_root}/bin"] },
]
```

- 仓库里所有计划文件（`plan.toml`、`mount-plan.toml` 等）和自定义计划都必须迁移；任何残留的旧数组都会在加载阶段报错（退出码 2，提示 prepare 必须写成 `{ root = "system", argv = [...] }` 或显式的 `test`），不挂载比赛盘、不运行任何命令。
- 迁移只改写法：命令的顺序和参数不变，原来的 `/mnt/contest/...` 字面路径可以保留，也可以换成 `{test_root}`。不再有“同一个数组放在不同字段里自动换根”的规则。
- prepare 可以显式使用 `test`，但此时它依赖的解释器必须已经可用，而且 `dev`、`shm`、`proc`、`tmp` 要等 prepare 之后才挂载。需要自举时通常写 `system`。

## 8. 预览、单跑与正式运行

在完整的 BLOSSOM 开发环境里构建，并在 QEMU 宿主终端运行：

```sh
make build-qemu-artifacts ARCH=loongarch64
make run-qemu ARCH=loongarch64 QEMU_ARGS='--shell --working-disk-action fresh --working-disk-dir build/cyclic-la-check'
```

比赛原镜像用工具的默认路径；使用自定义原盘时追加 `--contest-image /实际路径/sdcard-la.img`。`--working-disk-dir` 要指向新的专用目录，不能把源镜像路径当成工作盘路径。

进入客体 Shell 后，第一条预览，第二条真实运行（在 LoongArch 上它会触发第 3 节的 setup，并按第 9、10 节所述修改工作副本）：

```sh
/sbin/contest-run /dev/vda --file sets/cyclictest.toml --only 'cyclictest:musl:script' --list
/sbin/contest-run /dev/vda --file sets/cyclictest.toml --only 'cyclictest:musl:script' --finish return
```

预览每条命令一行，给出执行根（`root=`）、真正执行根的路径（`path=`，`system` 为 `/`，`test` 为测试根的路径）、有效 cwd、超时、未拆分的 argv 和环境，模板（含 `{test_root}`）已展开。输出形如下面的节选：

```text
GROUP cyclictest / musl path=/mnt/contest/musl
  root.prepare root=system path=/ cwd=/ timeout=60s argv=["/bin/busybox", "ln", "-sfn", "lib", "/mnt/contest/musl/lib64"] env={}
  ……（其余 prepare 命令）
  CASE cyclictest:musl:script repeat=1 source=/etc/contest/sets/cyclictest.toml group 1 step 1
    setup root=system path=/ cwd=/ timeout=60s argv=["/bin/busybox", "sh", "/sbin/prepare-la-musl-sched", "/mnt/contest/musl/lib/libc.so"] env={}
    exec root=test path=/mnt/contest/musl cwd=/ timeout=600s argv=["/busybox", "sh", "/cyclictest_testcode.sh"] env={"HOME": "/", "LD_LIBRARY_PATH": "/lib", "PATH": "/ltp/testcases/bin:/bin:/usr/bin:/"}
```

应当看到：setup 是 `root=system`（`path=/`），脚本在 `/sbin`，libc 参数展开为 `/mnt/contest/musl/lib/libc.so`；主命令是 `root=test`（`path` 是该测试根），运行原 `cyclictest_testcode.sh`。预览不运行 prepare、setup、主命令、cleanup、通知或关机；它仍要挂载比赛盘工作副本以读取目录和清单，挂载本身可能更新卷元数据，所以不保证文件系统一个字节都不变。

RISC-V 上预览同一例不应出现这条 setup；LoongArch 只选 glibc 也不应出现。`--only` 只缩小计划已有的选择，不会新增未配置的用例。`--file` 替换主计划的 `files` 列表，仍使用主计划的 `roots`、`defaults`、`override_files` 与 `finish`。

正式自动启动使用打包进系统盘的默认计划，沿用 `make all` 和评测的启动命令，评测机不需要传新参数。默认 cyclictest 使用带 `-A` 的四项同步测量；本节显式选择 `sets/cyclictest.toml`，用于运行原参数脚本。只构建并启动一个架构时用 `make contest ARCH=loongarch64`。只改了宿主上的 TOML、却继续启动旧系统盘，不等于配置已更新：客体读取的是系统盘里的 `/etc/contest/`。

## 9. 工作盘、fresh 与 reuse

下表是项目 QEMU 启动工具的工作盘策略：在该流程中原始比赛镜像不会被写入；默认计划、测试程序及挂载可能修改比赛工作副本。runner 本身并不提供写入隔离：自行提供设备或编写 `system` 命令时，操作者必须确保目标是工作副本，不能把原镜像直接映射成可写设备，也不能让自定义命令写原镜像。

| 策略 | 对比赛工作副本里已有改动的影响 |
|---|---|
| `fresh` | 重新从原始比赛镜像复制，此前所有改动（包括已修补的 libc）全部丢弃 |
| `reuse` | 直接复用工作目录里的副本，不读取原镜像，此前 prepare、setup 与测试写入的内容原样保留 |
| `refresh-system` | 只替换系统工作盘（含最新的计划与脚本），比赛工作副本里已有的改动原样保留 |

- 要从原库开始（baseline），或在关掉兼容后恢复原库，必须用 `fresh`。`reuse` 和 `refresh-system` 都会继续使用已被修补的比赛工作盘。
- 工具在标准错误输出每张盘的真实路径，形如 `run-qemu role=contest source=… source-consumed=… working-disk=…`；记录输入变化时使用这里的真实路径。`--print-command` 只打印配置摘要和转义后的命令行，不创建也不刷新工作盘。
- baseline 与修复轮各用一个全新的 `--working-disk-dir`，不要用 `reuse` 把已修补的盘当 baseline。baseline 把 `overrides/la-musl-sched.toml` 从 `override_files` 里去掉（或用 `--plan` 选择另一份不含它的主计划），修复轮保留；两轮的其他准备操作相同。
- 整盘哈希不能说明哪些输入变了：挂载日志和新 inode 都会改变它。运行前后应从工作副本提取并逐文件比对四套 cyclictest、hackbench、脚本、libc 与 loader。QEMU 退出（锁释放）之后可以在宿主上只读地提取：

```sh
debugfs -R 'stat /musl/lib/libc.so' build/cyclic-la-check/contest.raw
debugfs -R 'dump /musl/lib/libc.so /tmp/libc.after' build/cyclic-la-check/contest.raw
sha256sum /tmp/libc.after
```

- 需要保留的指纹：内核 ELF、系统盘、runner、静态 BusyBox、主计划及全部集合与 override 文件、QEMU 版本与二进制、原比赛盘、judge。
- 不再需要的工作副本用 `make clean-qemu-working-disks ARCH=loongarch64 QEMU_ARGS='--working-disk-dir build/cyclic-la-check'` 清理；它只删除该目录下未被占用的 `root.raw` 与 `contest.raw`。

## 10. LA musl 调度兼容

### 10.1 为什么改，改什么

已知版本的 LoongArch musl `libc.so` 里，`sched_getparam`、`sched_getscheduler`、`sched_setparam`、`sched_setscheduler` 四个公共函数是固定返回 `ENOSYS` 的存根：函数体里没有系统调用指令，只是把 `-ENOSYS` 交给库内的 `__syscall_ret`，所以内核的 118–121 号处理函数收不到这些调用。静态分析表明原 cyclictest 的 `check_privs` 会先后查询策略和参数，`sched_getparam` 返回失败就走 `unable to get scheduler parameters` 分支。

兼容 setup 把这四个函数改成直接发起同号的 Linux 系统调用，仍通过原 `__syscall_ret` 返回，内核给出 `EPERM`、`EINVAL` 等真实错误时照样返回真实错误。Buildroot 的 musl 包补丁把同样四个函数改为 Linux 系统调用，是源码层的同类做法；这里面对的是固定的第三方二进制，所以改字节而不是重编 musl。内核、ELF 加载器、FIFO 与亲和力、判题器和原脚本都不改。

涉及的文件：`userland/contest/overrides/la-musl-sched.toml`（触发条件）、`userland/system/sbin/prepare-la-musl-sched`（安装到系统盘的 `/sbin`，由系统根的静态 BusyBox 执行）、`userland/busybox.config`（脚本需要的 `DD`、`FEATURE_DD_IBS_OBS`、`SHA256SUM`、`MKTEMP` 已在其中启用，其余用到的 `sh`、`cp`、`chmod`、`mv`、`rm`、`printf` 本来就有）。改配置后要重新构建并打包系统盘；系统盘里的 BusyBox 缺 applet 时 setup 失败，同样不会运行主脚本。

### 10.2 工作盘里到底变了什么

这是一项显式的用户态兼容修改，不是“原库字节及语义不变”。按第 9 节使用 QEMU 工作副本时原镜像不动；选中该用例且完成修补后，fresh 工作副本里的 `lib/libc.so` 会改变，脚本、cyclictest 与 hackbench 的 ELF、judge 不变。

- 字节：脚本只接受原始 SHA-256 `816cff1d1abbef3f1423bbce01a56f00c97b6ff8e20d49966c9d4b6d27e5e7be`（长度 1021832 字节），修补后必须是 `09b0fc7cd720c1f5dc25399733c1d203389a2de9ca90f6effe1c394c66414438`。四个 32 字节窗口的文件偏移是 345312（`sched_getparam`，121 号）、345344（`sched_getscheduler`，120 号）、345412（`sched_setparam`，118 号）、345444（`sched_setscheduler`，119 号），窗口内实际有 80 字节不同，窗口外保持原样。
- 语义：这四个函数此前恒返回 -1 和 `ENOSYS`，修补后按内核的真实结果返回。musl 的 `libc.so` 同时是动态加载器，同一个测试根里之后启动、加载这份库的动态程序调用这些函数时也会使用新语义，不只是 cyclictest；这不是每个用例的隔离。
- inode 与元数据：脚本在同一目录创建临时文件，修补并核对完整哈希后设置权限 0755，再用同目录的 `mv -f` 覆盖 `lib/libc.so`。发布出去的是一个新 inode；它的权限（脚本设为 0755）、属主和时间戳取决于新文件的创建者与创建时刻，不继承旧文件，都要计入输入变更。已经映射旧 inode 的活动进程继续使用旧库，不会自动切换；runner 串行执行，setup 在本用例启动前运行，其他仍活跃的旧映射靠既有的收尾处理。
- 失败时的残留：发布之前出错会清除临时文件，原库不变，不会发布半份补丁；`SIGKILL` 或掉电可能留下 `libc.so.sched.*` 临时文件，由 fresh 重置，本机制不提供崩溃事务保证。

输入变化的记录要写明原哈希、目标哈希和这项配置，不得写成“用户态完全未改变”。

### 10.3 未知版本与失败

脚本不识别测试名，不读输出，不伪造返回值，只按完整 SHA-256 判断：

| 工作副本里的 `lib/libc.so` | 脚本的行为 |
|---|---|
| 不存在，或是符号链接 | 报 `expected a regular non-symlink libc: …`，非零退出 |
| 已是修补后的哈希 | 安静返回 0（可重复执行） |
| 是已核验的原始哈希 | 修补并发布 |
| 其他任何哈希（未知版本，含被改过一部分的库） | 报 `unsupported libc SHA256: …`，非零退出，库不动 |
| 复制、写入、校验或发布失败 | 报错并非零退出；发布之前的失败不改原库 |

setup 非零就不运行主脚本，没有“跳过修补照常运行”的回退，也不输出假的成功。遇到新版本的库，不要删除哈希检查去硬套偏移；应重新取证，核对偏移与指令，并生成新的已核验补丁后再放行。

### 10.4 关闭与恢复

从计划的 `override_files` 里删除或注释 `overrides/la-musl-sched.toml`，重新打包系统盘，再用 `fresh` 启动一份新工作副本。仅仅不再选中该用例，不会撤销已经写进工作副本的库；`reuse` 会继续使用旧的已修补工作盘。

只运行 Basic 不会触发这项 setup；但若同一份工作副本之前运行过 cyclictest，Basic 和其他动态程序可能看到已改变的公共接口。这是共享文件的正常后果。

### 10.5 许可状态

用户本轮允许研究最小 hack，不等于赛事组织者已授权修改工作盘里的运行库。赛事是否接受这项兼容修改、线上评测使用的 libc 与 judge 版本，目前都未知。含这项 setup 的运行属于 prepared 输入，不能当作未注入官方盘的成绩。

严格要求连工作盘库的语义也不变时，本问题没有仅靠内核实现系统调用的解法：存根根本不陷入内核。其他做法（换库、重编、预加载四符号、重编 cyclictest、在内核里改写用户态指令）各自都是对用户态输入或行为的另一种改变，需要各自的验证，本合同不采用。更长期的正式修复应由比赛或工具链维护者在固定的 musl 源码里采用 Buildroot 同类实现，成套发布并更新指纹。

## 11. 验证与证据（待执行）

下面是验证入口，不代表已经执行。每一层结论分别取得、分别记录：构建通过不等于测试通过，宿主测试通过不等于客体通过。

### 11.1 构建与宿主合同检查

在项目的原开发环境里：

```sh
(cd userland/contest-run && cargo fmt --all -- --check)
(cd userland/contest-run && cargo test --locked --offline)
make build-qemu-artifacts ARCH=loongarch64
make build-qemu-artifacts ARCH=riscv64
```

`userland/contest-run/tests/command_roots.rs` 的普通宿主测试不挂比赛盘、不写通知设备，不代表任何客体用例通过；它们覆盖命令根语义、旧 prepare 的迁移错误、单遍展开和“来源根与执行根独立”等。实际 CLI 预览用例默认被忽略，需要 `CAP_SYS_ADMIN` 和环境变量 `CONTEST_RUN_TEST_DEVICE`（专用于该测试、可写且未挂载的临时 ext4 块设备，不能是比赛原盘）。普通 `cargo test` 不会把它算作通过，需要时显式运行：

```sh
(cd userland/contest-run && \
  CONTEST_RUN_TEST_DEVICE=/path/to/disposable-ext4-device \
  cargo test --locked --offline --test command_roots -- --ignored --test-threads=1)
```

宿主缺少权限或设备时该命令会真实失败。

### 11.2 预览

按第 8 节在 LoongArch 与 RISC-V 上分别 `--list`，核对 setup 只出现在 LoongArch 的 `cyclictest:musl:script`。

### 11.3 库调用确实进入内核

用材料包 `BLOSSOM-cyclictest-final/tools/` 里的 `sched_libc_probe.c` 做独立诊断。以下从仓库根目录设置材料包的绝对路径，并用项目实际的 LoongArch musl 工具链动态编译，不加 `-static`；`LA_MUSL_CC` 必须是实际编译器的路径。先用 `readelf` 确认 `PT_INTERP` 与 `NEEDED` 对应测试环境，再复制到独立诊断工作副本，在同一个 musl chroot 里执行：

```sh
export CYCLIC_PACKAGE="$PWD/ref/重构计划/BLOSSOM-cyclictest-runner-revised_2026-11-12_03-54/files/BLOSSOM-cyclictest-final"
"$LA_MUSL_CC" -O2 -Wall -Wextra -o sched-libc-probe "$CYCLIC_PACKAGE/tools/sched_libc_probe.c" -ldl -pthread
```

探针打印 `AT_BASE`、`dladdr` 与 `dlsym` 得到的对象和函数位置，比较公共函数与直接系统调用，检查 OTHER、参数、FIFO 99 的设置、回读与恢复、错误参数，以及两个线程的 `errno` 隔离。FIFO 因权限得到 `EPERM` 不记为通过，要记录权限环境。探针自己的绑定不证明原 cyclictest 的绑定：还要观察原 ELF 的实际 GOT/PLT 目标和库映射。

### 11.4 系统调用 trace

用 GDB 硬件断点或临时的完整系统调用 trace 确认原程序调用 `sched_getparam` 之后执行了新封装里的 `syscall 0` 指令，并记录 118–121 号的进入、实参和结果。现有的调度 ktrace 不自动等于系统调用 trace；只比较反汇编不足以断言历史上的某一轮实际如此绑定。同时验证 122/123 号的亲和力和既有的实时自测，不要把 `P:99` 的打印字段当成内核真实策略的回读。

### 11.5 原脚本实测与外部原 judge

先关掉诊断断点和高频 trace，再用原脚本正式测量；它内部保持 `NO_STRESS_P1`、`NO_STRESS_P8`、`STRESS_P1`、`STRESS_P8` 四项和 hackbench 流程不变。原始 UART 日志留档，不修正空白、不重写 Min 与 Max、不补 success。离线用材料包的 `tools/audit_cyclictest.py` 对留档的原固定 judge 核验 SHA-256，提取原始分组正文原样喂入，保存 judge 的输入、输出和独立审计：

```sh
python3 "$CYCLIC_PACKAGE/tools/audit_cyclictest.py" \
  --judge /path/to/original-package/evidence/judge_cyclictest-musl.py \
  --log /path/to/serial.log --group cyclictest-musl --out reports/la-musl
```

独立审计检查 1/8/1/8 条线程行、`C`、异常的最小值、success 与 kill 标记，不修改 judge 或它的分数。整组 kill 成功只说明 Shell 的信号发送命令成功，另需进程列表或 wait 的证据证明压力进程已退出。`C=0`、明显不足的采样、无法回收都要保留为问题，不能拿“有分”当作其他验证都通过。

### 11.6 双架构与既有机制回归

修复前后都要复测 RISC-V musl 与 glibc、LoongArch glibc 的原脚本，确认没有运行兼容 setup、ELF 与库都没有被本脚本改变。RISC-V 的 cyclictest 是静态链接，不能当作 LoongArch 动态库的替代证据。运行既有的 process/realtime、process/affinity、clock、mlock 和动态加载与 libctest 相关用例，确认 FIFO 的真实查询、设置与恢复、线程目标所有权、`errno` 与进程回收仍然保留；库修补之后同根的其他动态程序也受这四个接口语义变化影响，不能只测 cyclictest 一个程序。需要同条件的 Linux 对照时，要有 FIFO 权限，并保证同样的 CPU 数、负载、脚本和库，不要把模拟器宿主的调度差异说成内核的唯一原因。BusyBox 在目标内核上的 `dd`、`mv`、`cp` 文件操作也要在客体里实际验证。

### 11.7 成功要分层报告

- 构建通过：Rust、BusyBox 和双架构产物成功，不等于测试通过。
- 接口修复：实际加载的目标 libc 里，新封装进入了真实系统调用，错误返回也正确。
- 原脚本四项结果：四段完整、有效线程数和非零采样、没有残留的压力进程。
- 固定的本地评分：由原 judge 直接得到的 JSON，并保存输入哈希。
- 线上状态：没有线上真实记录就写未知，不外推排名。

QoS 节点、旧 clock 告警和 `C=0` 各自单独报告。

## 12. 常见报错

| 现象 | 含义与处理 |
|---|---|
| 加载时提示 prepare 必须写成 `{ root = "system", argv = [...] }` | 残留旧的 `prepare` 数组；按第 7 节迁移，退出码 2，没有挂载也没有运行命令 |
| 完整命令报缺 `root`、别的 `root` 取值或未知字段 | 完整对象只接受 `root`（必填，`system` 或 `test`）、`argv`、`cwd`、`env`、`timeout_sec` |
| `--list` 里没有预期的 setup | 不是 LoongArch，或只选了 glibc；`override_files` 没有引用该文件；ID 不匹配；被 `--only` 筛掉；先看 `--list` 末尾“matched no selected case”的提示 |
| 用例报 setup 失败，主脚本没运行 | 看 setup 的诊断：`unsupported libc SHA256`（未知版本）、`expected a regular non-symlink libc`、系统 BusyBox 缺 `dd`、`sha256sum` 或 `mktemp`、或文件操作错误；见 10.3，不要当成“用例没被选中” |
| 关掉了兼容，行为还是修补后的库 | 复用了保留改动的比赛工作盘（`reuse` 或 `refresh-system`）；改用 `fresh` |
| `test` 命令找不到系统盘里的文件 | `test` 看不到系统根；改用 `root = "system"` 的命令和 `{test_root}` 复制，或经已有挂载提供 |

## 13. 不需要做的事

不用配置 hook 名称，不改内核调度函数，不写新的测试 kind，不更改 judge，不复制一份改过的 cyclictest 脚本，不填人为的结果，不引入路径 DSL、插件注册、跨根别名或自动恢复所有系统状态的框架。
