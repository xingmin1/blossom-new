# 开发、测试与比赛执行

本指南面向新增学生和 agent，描述当前源码接口与可照做的操作，不是本轮验收报告。项目为 riscv64、loongarch64 的 Rust `no_std` 宏内核；比赛执行由独立 Rust **std** 程序 `/sbin/contest-run` 完成，普通 BusyBox init 仍是 PID1。命令存在、配置展开、交叉编译和 guest 执行是不同事实；本文不预签新 runner、全部比赛或实体板通过。

## 1. 环境与四种构建职责

除明确标注的 Cargo 命令外，宿主命令都在仓库根执行。公共依赖由 `PATH` 和明确路径提供：Rust/Cargo、GNU Make、Python3、两架构 musl 交叉 GCC；运行需对应 `qemu-system-riscv64` / `qemu-system-loongarch64`；制自有系统盘需 `mke2fs`、`debugfs`、`e2fsck`、`dumpe2fs`，运行库检查需支持目标 ELF 的 `readelf`。Nix 只可作为个人环境，不是 Make 或评测依赖。正式构建不下载、安装或升级工具链，不以旧 ELF 替代失败。

```sh
make help
make build ARCH=riscv64 MODE=debug
make runner ARCH=riscv64
make userland ARCH=riscv64 MUSL_CROSS=riscv64-linux-musl-
```

- `build` **仅构建内核**，默认输出 `build/ARCH/kernel.elf`；内核目标分别为 `riscv64gc-unknown-none-elf`、`loongarch64-unknown-none`。修改普通内核函数不必先运行 all。
- `runner` 仅编译本架构 release runner，输出 `build/contest-run/<Linux target>/release/contest-run`，不安装到已有 SOURCE 或工作盘。
- `userland` 构建并安装 BusyBox、runner、操作工具、C 断言程序及用户脚本到 `USERLAND_ROOT`（默认 `build/userland-ARCH`）；安装的 runner 为 `sbin/contest-run`。内核 `MODE`、`PANIC` 不决定 runner 的 profile。
- `all` 生成两架构普通 abort 内核、两份 userland/runtime 和双根辅助盘，导出固定 **`kernel-rv`、`kernel-la`、`disk.img`、`disk-la.img`**。两份盘是同内容的独立文件，内含 `/riscv64`、`/loongarch64` 两个系统根。构建不读取尚未提供的比赛盘。

### runner 的固定工具链与离线 Cargo

runner 使用 **`nightly-2025-01-18`**，既有版本指纹为 rustc `1.86.0-nightly (6067b3631)`，固定版本记录在根 `rust-toolchain.toml`。与内核相同，Make 直接使用 `PATH` 中已准备的 Cargo/Rust，不传 `+toolchain`、不要求 rustup 代理，也不自动切换编译器；使用 rustup 代理时仍以 `RUSTUP_AUTO_INSTALL=0` 禁止自动安装。在构建前备妥该工具链及其真实目标标准库/sysroot、匹配交叉链接器；不是给任意目录取名 sysroot 即可。两个 Linux 目标分别为 `riscv64gc-unknown-linux-musl`、`loongarch64-unknown-linux-musl`。Make 不安装这些前置。

```sh
make runner ARCH=riscv64 \
  RUNNER_SYSROOT=/srv/rust-std/nightly-2025-01-18 \
  RUNNER_LINKER=riscv64-linux-musl-gcc
make runner ARCH=loongarch64 \
  RUNNER_SYSROOT=/srv/rust-std/nightly-2025-01-18 \
  RUNNER_LINKER=loongarch64-linux-musl-gcc
```

上述 `/srv/rust-std/...` 必须换成已准备、含对应目标组件的实际 sysroot；不指定 `RUNNER_SYSROOT` 时使用所选工具链自己的 sysroot。链接器默认 `$(MUSL_CROSS)gcc`。代码生成固定 `+crt-static`、`relocation-model=static`、`link-self-contained=yes`、`-no-pie`，release `panic=abort`，不额外关闭 LA LSX/LASX。runner 自身静态不意味着所选测试也必须静态；测试可以使用自己的动态 loader 和 DSO。

`userland/contest-run` 是独立 Cargo 工程。当前 `Cargo.lock` 与17个包的独立 `vendor/` 已由 Cargo 正式生成，锁定兼容的 `toml_edit 0.22.20`；`.cargo/config.toml` 将 crates.io 指向本地 vendor，缺少输入直接失败。直接依赖固定 serde 1.0.217、toml 0.8.19、libc 0.2.169、glob 0.3.2。准备依赖属于环境建立，不在 `make all` 内执行。直接 Cargo 操作必须先进入此目录，让 Cargo 读取这份本地配置；不能在仓库根只加 `--manifest-path` 就假定配置发现规则相同。内核和 runner 不共用默认 target。

## 2. 自有系统树、镜像与工作盘

已有完整自有树 `/srv/system-rv`（含匹配架构 `/sbin/init`、`/bin/sh` 和自身配置）时：

```sh
make image ARCH=riscv64 SOURCE=/srv/system-rv IMAGE=build/system-riscv64.img
make run ARCH=riscv64 IMAGE=build/system-riscv64.img ROOTDIR=/
make run ARCH=riscv64 IMAGE=build/system-riscv64.img ROOTDIR=/ RESUME=1
```

`image` 的 `IMAGE` 是**自有系统盘输出**，普通输出须在 `build/` 下且以 `.img` 结尾；`run` 的 `IMAGE` 是已有来源盘输入，run 不隐含 image。不得把官方测试盘、测试工作盘或导出的原测试树作为制盘输出/来源去重制评分输入。

没有 `SOURCE` 且普通 `STAGING` 尚不存在时，image 才准备 userland/runtime 并建立普通树。**显式 SOURCE 从不回填；用户指定的已有 STAGING 也不自动补 runner、配置或库。** 更新这类自有树由调用者负责。项目默认 contest staging 则明确受项目管理，每次 fresh contest 或显式系统刷新用当前 runner、完整配置和普通资产重新生成；自行传 `STAGING=...` 即按普通树规则处理，不把手工树冒充 managed 树。

默认普通根 `/dev/vda`、`ROOTDIR=/`、`INIT=/sbin/init`，挂载 RO；只有 `ROOT_MODE=rw` 才启用正常根写入，extra 固定 RO。`rootdir` 是系统 ext4 卷内真实目录，成为初始进程 root/cwd，与 runner 对测试根的选择无关。明确错误不猜其它设备或目录。

### fresh、RESUME 和 REFRESH_SYSTEM

```sh
make contest ARCH=riscv64 CONTEST_IMAGE=build/sdcard-rv.img
make contest ARCH=riscv64 CONTEST_IMAGE=build/sdcard-rv.img RESUME=1
make contest ARCH=riscv64 CONTEST_IMAGE=build/sdcard-rv.img \
  RESUME=1 REFRESH_SYSTEM=1
```

前提是 `CONTEST_IMAGE` 为实际收到并核对来源的、匹配架构的 raw 整盘 ext4，且运行库与编译环境已经准备。默认比赛工作盘先附为 `/dev/vda`（RW），系统盘第二块 `/dev/vdb`；默认 IMAGE、STAGING、WORK_DIR 分别为 `build/contest-system-ARCH.img`、`build/contest-system-ARCH`、`build/contest-run-ARCH`。

- fresh（默认 `RESUME=0`）：制作系统 image，再由 `tools/run.py` 从来源复制 `contest.raw`、`root.raw` 到 WORK_DIR，只把副本交 QEMU，来源不被 guest 改写。
- `RESUME=1`：跳过 image，严格复用同一目录全部既有工作盘，保留系统与比赛修改；不因源码或 TOML 更新偷偷换盘。缺工作盘报错，不转 fresh；来源文件仍须存在。
- `RESUME=1 REFRESH_SYSTEM=1`：先生成当前系统 image，在原工作锁内只原子替换 **`root.raw`**；保留 `contest.raw` 及其它工作角色、原输入。系统工作盘手工修改会被替换，需要保留它们就只用 RESUME。刷新失败直接结束，不继续用旧系统盘。

启动、复制、刷新与工作清理复用 WORK_DIR 的 `.lock`；锁 FD 继承给实际 QEMU，持有至退出。修改计划但只 RESUME，运行的仍是旧系统工作盘。

```sh
make clean ARCH=riscv64
make work-clean WORK_DIR=build/contest-run-riscv64
# 相同操作的实际 CLI，不需要架构、内核或 image 参数：
python3 tools/run.py --work-clean --work-dir build/contest-run-riscv64
```

clean 仅清明确可重建编译/链接输出，不删除系统镜像、工作盘、SOURCE/STAGING、工具链/sysroot、日志或报告。work-clean 取得同一非阻塞锁，只删 `root.raw`、`contest.raw`、`extra.raw`、`scratch.raw`；保留目录、`.lock` 与无关文件，运行中持锁则拒绝。结束连续实验、确认 QEMU 退出后及时清理工作副本，不为历史复现保留盘备份；保留原命令、摘要与日志。

## 3. 三种测试分别选择

### 宿主纯逻辑

```sh
# 仓库根：Python 的真实 discover，或仅跑一个职责文件。
python3 -m unittest discover -s tools/ktest
python3 -m unittest discover -s tools/ktest -p test_catalog.py

# 独立 Cargo 工程：同源配置/选择逻辑，不是 guest 运行。
cd userland/contest-run
env -u RUSTFLAGS -u CARGO_ENCODED_RUSTFLAGS RUSTUP_AUTO_INSTALL=0 CARGO_TARGET_DIR=../../build/contest-run cargo test --locked --offline --test config
env -u RUSTFLAGS -u CARGO_ENCODED_RUSTFLAGS RUSTUP_AUTO_INSTALL=0 CARGO_TARGET_DIR=../../build/contest-run cargo test --locked --offline --test config multifile_order_globs_exclude_and_locations
```

Cargo host 测试还需当前宿主目标的 std 与宿主链接器。上述直接命令清除交叉 Rust flags，产物写入仓库既有 `build/contest-run`，不在 crate 内创建 `target/`。Python 原测试按 catalog、protocol、resources、UART、boot、lifecycle 拆分，共用帮助在非 `test_` 模块；不要恢复旧 `test_host` 转导壳。宿主测试不证明 guest 调度、设备或 ABI 正确。

### 内核具名 ktest

回到仓库根：

```sh
make ktest-list ARCH=riscv64 CPUS=2 TEST='*final_address'
make ktest ARCH=riscv64 CPUS=2 TEST='*final_address'
```

`TEST` 传给现有 `tools/ktest.py` 的 `--glob`。先 list 取当前 ELF 的实际全名，再用 `KTEST_ARGS='--exact 完整名 ...'` 定向运行；不指定 `--kernel` 时工具调用 ktest-build。需要磁盘的 case 必须提供其真实 `--disk root=PATH` 等输入，需要 UART/网络的 case 按实际资源合同准备。不要把资源缺失当支持性 Skip。参数、min_cpus、真实结束和资源归还仍属原 ktest，不把比赛目录逐项注册成空内核 case。

### 用户 ABI 与比赛测试

客体唯一入口：

```sh
/sbin/contest-run /dev/vda --list
/sbin/contest-run /dev/vda --plan /etc/contest/plan-onsite.toml --list
/sbin/contest-run /dev/vda --plan /etc/contest/plan-onsite.toml
/sbin/contest-run /dev/vda --plan /etc/contest/plan.toml \
  --file sets/basic.toml --file sets/libctest.toml --list
```

CLI 合同为 `DEVICE [--plan FILE] [--file FILE ...] [--list]`；缺省 `/etc/contest/plan.toml`。list 挂载/读取本次真实来源并释放所取挂载，不执行 root.prepare、setup、Hook、测试或补库，也不执行 finish 关机。它不是构建期生成的虚构 catalog。`plan-onsite.toml` 使用项目实际安装的 libc-smoke 和 shell 示例，不需要占位 ELF；仍需真实附盘与完整系统 userland。它设置 `finish="return"`，不代表固定评测默认自然关机已验。

要连续手工操作，应使用保留控制 shell 的自有 SOURCE，不能让默认自动计划先运行并关机：

```sh
make contest ARCH=riscv64 SOURCE=/srv/system-rv \
  IMAGE=build/command-system-rv.img CONTEST_IMAGE=build/sdcard-rv.img
```

## 4. 改配置，而不是改内核启动列表

完整输入目录为 [userland/contest](../userland/contest/)。入口 `files` 按顺序选择 step 文件，`override_files` 选择精确覆盖文件；相对路径始终以**入口文件所在目录**为基准，仅一层引用，无递归 include、变量语言或自动扫描整目录。CLI 的 `--file` 列表只替换 files，仍使用该 plan 的 roots/defaults/override_files；入口本身的 step 先于所引文件。

默认 plan 选择 basic 和固定 pre-2025 libctest，要求真实盘内 `/musl`、`/glibc` 及对应来源；它不自动运行未引用的 `sets/ltp-process.toml`。收到不同盘时先检查实际内容，再改入口，不靠缺文件触发另一来源。

以下为自有现场盘的配置写法。必须先有实际编译的 `/tests/my-test.elf`、原脚本及其依赖；示例不生成这些文件，不能拿空文件或占位 ELF 当实现。

```toml
# userland/contest/plan-local.toml
files = ["onsite/local.toml"]
override_files = ["overrides/local-run.toml"]
finish = "return" # 自动交付需要正常关机时明确选择 poweroff

[defaults]
timeout_sec = 60
repeat = 1
hooks = false

[roots.contest]
path = "/mnt/contest"
cwd = "/"
mounts = ["dev", "proc", "tmp"]
env = { PATH = "/bin:/usr/bin:/tests" }
```

```toml
# userland/contest/onsite/local.toml
[[step]]
kind = "exec"
id = "onsite:memory"
root = "contest"
cwd = "/tests"
program = "./my-test.elf"
args = ["--label", "value with spaces", ""]
arch = ["riscv64", "loongarch64"]

[[step]]
kind = "shell"
id = "onsite:filesystem"
root = "contest"
shell = "/bin/sh"
command = "/tests/run-fs.sh --quick"
```

exec 原样传 argv，包括空字符串、空格、Unicode，不隐式 `sh -c`；只有 shell step 把 command 交给明确存在的 shell。`root.path` 在 runner 系统根解释，cwd 和目标路径在子根解释。父始终不 chroot，子进程沿真实 chroot/chdir/exec；旧 Shell 编排和 C contest-exec 转发链不是正式入口。

需要原 LTP runtest 时，用实际 tag 和清单：

```toml
[[step]]
kind = "ltp"
roots = ["contest"]
suite = "syscalls"
source = "runtest"
catalog = "/ltp/runtest/syscalls"
shell = "/bin/sh"
include = ["fork01", "waitpid*", "execve*"]
exclude = ["execve05"]
arch = ["riscv64", "loongarch64"]
```

runtest 首字段是 tag，尾部命令原文交给指定 shell，保留环境赋值、参数、引号、管道和重定向；不能按空格重新拼 argv。只有 ELF 目录而无 runtest 时明确改为 `source="executables"`、`directory="/tests"`，去掉 catalog/shell，按文件名排序选择真实可执行文件；清单打不开不自动降级为扫目录。普通独立 ELF 也可直接用 exec，不因名称像 LTP 就猜协议。其余 kind 为 basic、libctest，后者明确 `variant="dynamic"` 或 `"static"` 并读取该固定版本原 selector 脚本，不能泛化成任意 Shell parser。

```toml
# userland/contest/overrides/local-run.toml
[[override]]
id = "onsite:memory"
timeout_sec = 120
repeat = 3
env = { MODE = "measure" }
```

顺序为入口 step／files 顺序 → 文件 step → roots → include；模式内遵从原 catalog 顺序。exclude 只影响本 step。精确名不存在、模式无命中、排除后空选择、整计划空、未知字段均报错，不产生空 Pass；`enabled=false` 或架构不适用不选。相同完整 ID 与相同语义保留第一次，冲突要求明确 override 或不同 ID。override 只作用已选 ID，未选 ID 报错，不重选已排除项；可改 root 等运行字段但不改变原选择身份。defaults → root 环境 → step → override，env 按 key 覆盖，args/setup/cleanup 整字段替换；多份 override 同字段冲突报来源。repeat 每轮沿用基本 ID，另记 iteration，不只报告最好一轮。

## 5. 真实解释器、准备与结束责任

默认 **supplied-first**：使用测试根自带 ELF、真实 `PT_INTERP` 路径、loader 和 DSO，不按 root 名叫 musl/glibc 就覆盖文件。`LD_LIBRARY_PATH` 不能弥补缺失 PT_INTERP。静态测试不要求 `/bin/sh`；脚本和 shell step 必须由其 root 提供实际解释器。

确需准备时，`roots.NAME.prepare = [["/bin/sh", "脚本路径", "参数"]]` 在**父系统根**运行，参数明确指向 `/mnt/contest/...`；case 的 setup/cleanup 命令数组在**该 case 根和 cwd**运行。选择旧运行库补充、建立所需 loader 链接或替换文件必须显式，默认不复制系统 `/opt/runtime` 到测试根。直接运行 loader 是另一种明确 exec，参数由该 loader 支持，可能改变 argv/auxv，不宣称与普通 PT_INTERP exec 等价。预检只是诊断，真实 exec errno 单独保留；不能用父根 canonicalize 猜子根绝对链接。

每轮按 setup → 可选 before → spawn/wait/输出收尾 → 成功 before 对应 after → cleanup → 结果结束，每次只执行一个 case。before 成功后，spawn 失败、非零、信号、超时等普通结束路径也必须 after。主结果与 setup/before/after/cleanup 错误分别保存；after 失败不覆盖原结果，也不在未知全局状态继续。同步 Hook 卡死、内核 panic 或硬件卡死不承诺可由用户 runner 强停并恢复。

所有协议都继承 stdin。Plain/LTP 不解析正文，stdout/stderr 通过带 CLOEXEC 的 dup 交给子进程，直接使用原输出端点；不改变借用标准流的共享 OFD 状态，包括可能与 stdin 共用的 `O_NONBLOCK`。只有 Basic/固定 Pre2025 正文协议才捕获输出，以每流有界队列交给一个单线程、fork 后仅作 libc 调用的输出 relay 子进程，原样转发；父只将自己独占的 pipe 端点设为 nonblocking。正常结束要求队列、pipe 和 relay 真正排空并 wait；输出到期则定向 kill/wait 自己的 relay，独立报告 I/O 错误，不以 helper 状态覆盖测试原 wait 结果。不缓存全输出，也不声称恢复两流精确交错。这仍是一个正式 runner，不增加线程池或另一套测试后端。

`timeout_sec` 在启动前建立一次单调绝对 deadline，协调 wait 与必要的输出收尾，新输出不续期；setup/cleanup 命令各有限时。超时终止实际测试进程组，必要时强杀并 wait 直接 child。**进程组不覆盖任意 setsid/脱离的后台树**；测试须按自身约定收子任务，个别后台服务在明确 cleanup 中停稳，不能仅凭主进程退出宣称全系统干净，也不通过通用 `/proc` 扫描猜测回收。

结果保留 exec 失败、程序自己 exit127、非零、信号、超时及原 wait 编码。Plain、LTP、basic、固定 pre-2025 各用原协议：固定 runtest 的 return1 成功需正文配合，basic Completed 不升级成判题器 Pass，LTP 原退出位区分失败、损坏、警告和配置状态；不因一行 FAIL/PASS 字符串改签，也不把纯 TCONF 或缺 loader 当内核支持性 Skip。

### 内核专属 Hook

只需用户态特殊行为时改 runner 的 `custom.rs`；确有内核专属操作才改 [os/src/contest_hooks.rs](../os/src/contest_hooks.rs) 的具体分支，并在对应 case/override 设置 `hooks=true`。合法消息是单次 write 的 `before ID\n` 或 `after ID\n`，上限512字节，完整复制并解析后才产生副作用；拒绝空 ID、内嵌空白/NUL、多命令，短写不得重放。

Hook 在**控制者普通、可睡眠 Task 上下文**运行，不是目标 exec/fault/IRQ 时刻；全局开关也影响同时段后台任务。现有 `runner:trace` 分支直接调用原 trace start/stop，其余合法未知 ID 无特例。新增操作复用原子系统拥有和安全边界，不添加 Profile、任意地址执行、快照回滚或用 case 名伪造 syscall 结果。普通 case 不必增加空分支。操作工具源码在 `userland/tools/{klog-read,ktrace-control}.c`，安装名仍 `/bin/klog-read`、`/bin/ktrace-control`；`tests/user/observation-smoke.c` 仍是断言程序。

## 6. 固定交付与定向验证

运行库来源详见 [sources.md](../userland/runtime/sources.md)。以下路径必须对应实际提取的库、已准备目标 std 与链接器：

```sh
make all \
  MUSL_LIBC_RV=/srv/contest-runtime/riscv64/musl/libc.so \
  GLIBC_LIBDIR_RV=/srv/contest-runtime/riscv64/glibc/lib \
  MUSL_LIBC_LA=/srv/contest-runtime/loongarch64/musl/libc.so \
  GLIBC_LIBDIR_LA=/srv/contest-runtime/loongarch64/glibc/lib \
  RUNNER_SYSROOT_RV=/srv/rust-std/nightly-2025-01-18 \
  RUNNER_SYSROOT_LA=/srv/rust-std/nightly-2025-01-18
```

完整 `userland/contest/` 复制到每个辅助根 `/etc/contest/`。默认 inittab 调 `/sbin/contest-run /dev/vda`；本地 `CONTEST_PLAN=userland/contest/plan-local.toml` 可选树内入口，嵌套入口也**保持原相对位置**，生成的 inittab 选择 `/etc/contest/原相对路径`，不将它重命名后破坏相对引用。评测选择由提交的默认配置决定，不能依赖个人环境变量或要求固定 QEMU 再传 plan。

用本次 all 产物在本地走同一客体计划：

```sh
python3 tools/run.py --arch riscv64 --kernel kernel-rv --cpus 4 --memory 512M \
  --image disk.img --contest-image build/sdcard-rv.img \
  --root /dev/vdb --rootdir /riscv64 --work-dir build/delivery-run-rv
python3 tools/run.py --arch loongarch64 --kernel kernel-la --cpus 4 --memory 768M \
  --image disk-la.img --contest-image build/sdcard-la.img \
  --root /dev/vdb --rootdir /loongarch64 --work-dir build/delivery-run-la
```

这是本地工作副本入口，不替代当次评测机固定命令。固定双盘直接启动不额外传测试名单/plan：第一盘比赛工作内容，第二盘辅助根；内核无显式启动参数时用 `/dev/vdb` 与架构 rootdir 默认值。LA 使用 PCI（前两盘地址0x5、0x9），RV 使用 MMIO，盘槽实现为 `tools/storage.py`，不向 LA 传 RV-only `--modern`。观察实际配置来源、展开 ID、原输出、真实退出及正常同步关机；手动杀 QEMU 不算自然关机。

新增贡献按改动选择最窄验证，不默认重跑历史全矩阵：

- 新名单/现场 ELF：改 `userland/contest` 的入口、step、override；重生项目管理系统盘，先 guest list，再只跑实际选择，核 argv、原输出和结束状态。仅改配置不需 clean 或重编 runner/BusyBox。
- 配置/协议逻辑：改独立 runner 的 model/config/selection/report 及同源 host 测试；host 检查后，用两目标新 runner 在 guest 验真正进程、输出和解释器路径，不能只凭 host Pass。
- 内核缺口：改当前子系统，补一项可定位 ktest 或真实用户 ABI 回归，保错误路径、精确交错和真实资源归还；case 全名以本次 list 为准。
- 写入测试放 `os/src/fs/ktest/user/write/{buffered,mmap,writeback,truncate,reclaim,namespace}.rs`，共用 WriteOwner 在 write.rs；普通 fixture 不默认带网络/串口接收，只有确需的 suite 组合对应 owner。C libc/network 正文在 `tests/user/libc/`、`tests/user/network/`，原二进制与选择 argv 不变，raw ABI 测试不换成会修正输入的高级包装。

当前源码导航见 [runtime 开发导读](../userland/runtime/development.md)。任何新运行只记录实际执行范围；约千项 catalog 展开不等于千项 LTP 功能通过，QEMU 成功不等于 VisionFive 2/2K1000LA 板卡验证。保留原失败身份和未运行项，不扩全部 Linux/std/LTP、分区/VFAT/用户 RTC ABI、PTY/namespace、journal/replay/恢复或恶意坏盘防护；LA NX 暂缓仍非 Pass。
