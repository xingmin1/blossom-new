//! `procfs` 进程与系统状态伪文件系统实现。
//!
//! ## 模块职责与架构定位
//! 本模块负责在内存文件系统（[`MemoryFs`]）之上构建只读的 `/proc` 目录树（[`try_new`]），
//! 并在用户或内核打开具体信息节点时即时采样相关子系统状态，生成不可变的文本快照（[`SnapshotFile`]）。
//! 上游调用者为 VFS 挂载与打开路径（`Vfs` 初始化挂载 `/proc`，打开 `MemorySpecial` 节点时调用 [`open`]）；
//! 下游依赖启动信息（[`crate::boot`]）、CPU 与中断统计（[`crate::cpu`]、[`crate::irq`]）、
//! 物理内存统计（[`crate::mm`]）、时间与调度账务（[`crate::time`]、[`crate::accounting`]）、
//! VFS 挂载表以及当前进程所属的网络协议栈（[`crate::net::NetStack`]）。
//!
//! ## `/proc` 目录结构与按打开采样（Snapshot-on-Open）机制
//!
//! ```text
//! /proc (MemoryFsKind::Procfs, 只读)
//!  ├── cmdline     (0444) ──► MemorySpecial::Cmdline    ──► 启动命令行参数
//!  ├── mounts      (0444) ──► MemorySpecial::Mounts     ──► 当前根目录可见的挂载表
//!  ├── meminfo     (0444) ──► MemorySpecial::Meminfo    ──► 物理内存与 Slab 页统计
//!  ├── uptime      (0444) ──► MemorySpecial::Uptime     ──► 系统单调运行时间与累计空闲时间
//!  ├── interrupts  (0444) ──► MemorySpecial::Interrupts ──► 各在线 CPU 的定时器/IPI/外部中断计数
//!  └── net/        (0555)
//!       ├── dev    (0444) ──► MemorySpecial::NetDev     ──► 当前网络命名空间的网卡收发统计
//!       └── route  (0444) ──► MemorySpecial::NetRoute   ──► 当前网络命名空间的 IPv4 路由表
//!
//! 打开与读取生命周期:
//!   open("/proc/...", O_RDONLY)
//!       │
//!       ├─► 1. 从对应内核子系统提取瞬时状态并格式化为 Vec<u8> (SnapshotText)
//!       └─► 2. 封装为 SnapshotFile { path, bytes, cursor: 0, flags }
//!               │
//!               ├─► read / lseek : 在 cursor (SleepMutex) 保护下移动游标并拷贝快照切片
//!               └─► pread        : 无锁按指定 offset 直接读取不可变的 bytes 快照
//! ```
//!
//! 每次调用 `open` 都会生成一份独立的文本快照，后续在该文件描述符上的多次 `read` / `lseek`
//! 始终观察同一份稳定文本，避免分批读取时因系统状态变化导致行内容错位；若需观察最新状态，需重新 `open`。
//!
//! ## 锁获取顺序与并发契约
//! - **快照生成阶段（`open`）**：在获取各子系统（VFS 挂载表、网络协议栈、内存分配器、中断控制器）内部锁时，仅拷贝标量统计数据，随后在锁外使用 [`SnapshotText`] 执行字符串格式化与堆内存扩容，缩短子系统锁的持有时间。
//! - **快照读取阶段（`read_into` / `seek`）**：仅获取 [`SnapshotFile::cursor`]（[`SleepMutex`]）保护游标推进；`SnapshotFile` 不反向持有 `Vfs` 或 `NetStack` 引用。

use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;
use core::fmt::{self, Write};
use core::ops::Range;
use core::sync::atomic::{AtomicU32, Ordering};

use crate::boot::{self, MAX_CPUS};
use crate::fs::file::{require_read, set_nonblocking, set_status_flags, stat_path};
use crate::fs::inode::{InodeMetadata, MemoryContent, MemoryDirectory, MemoryEntry};
use crate::fs::memfs::{MemoryFs, MemorySpecial};
use crate::fs::{
    DirectoryCursor, DirectoryEntry, FileIoContext, FileLike, FileRef, FileStat, FsError,
    InodeKind, InodeRef, Path, ReadBuffer, SeekFrom, Vfs, WriteBuffer, O_ACCMODE, O_RDONLY,
    O_TRUNC,
};
use crate::net::route::{IPV4_ADDRESS_BITS, RTF_GATEWAY, RTF_UP};
use crate::net::{NetStack, INTERFACE_COUNT};
use crate::sync::{SleepMutex, SpinLock};
use crate::task::process::{Pid, ProcessStatus, TaskComm, TaskGraph, MAX_PID};
use crate::task::time::{CpuTime, USER_HZ};
use crate::task::{Policy, State};
use crate::{accounting, cpu, irq, mm, time};

/// 生成 `/proc` 文本快照时预分配的初始缓冲区容量（单位：字节）。
const SNAPSHOT_INITIAL_CAPACITY: usize = 256;
/// 每 KiB 包含的字节数（1024 字节），用于 `/proc/meminfo` 页数到 `kB` 的换算。
const BYTES_PER_KIB: usize = 1024;
/// `/proc/uptime` 小数部分的百分之一秒刻度数（100 ticks/s）。
const UPTIME_TICKS_PER_SEC: u64 = 100;
/// `/proc/uptime` 每个百分之一秒刻度对应的纳秒数（$10^7\text{ ns}$）。
const UPTIME_TICK_NANOS: u64 = time::NSEC_PER_SEC / UPTIME_TICKS_PER_SEC;
/// 固定节点编号之外的进程 inode 编号起点，每个 PID 占四个编号。
const PROCESS_INODE_BASE: u64 = 1 << 32;
/// 根目录中数字进程项的游标起点，避免与固定节点游标重叠。
pub(crate) const PROCESS_COOKIE_BASE: u64 = 1 << 32;
/// 本内核不限制进程常驻内存，stat 将该限额表示为无上限。
const RSS_LIMIT_INFINITY: u64 = u64::MAX;
/// 进程枚举的结束游标，包含最大 PID 之后的一个位置。
pub(crate) const PROCESS_COOKIE_END: u64 = PROCESS_COOKIE_BASE + MAX_PID as u64 + 1;

/// 与一次 PID 身份绑定的动态进程节点，不持有任务图或用户运行资源。
pub(crate) struct ProcessNode {
    /// 原始 PID 对象，号码复用后仍指向旧身份。
    pub(crate) pid: Arc<Pid>,
    /// 目录或信息文件的具体角色。
    pub(crate) role: ProcessRole,
}

/// 进程节点的数据面角色，目录复用共同内存目录格式。
pub(crate) enum ProcessRole {
    /// 仅包含 cmdline 与 stat 的只读目录。
    Directory(SpinLock<MemoryDirectory>),
    /// 按打开时间采集的进程统计文本。
    Stat,
    /// 按打开时间读取的 NUL 分隔用户参数字节。
    Cmdline,
}

/// `/proc` 动态信息节点的打开文件描述（Open File Description）。
///
/// 在 `open` 时一次性生成完整文本并保存在 `bytes` 中；同一打开实例的多个复制描述符共享读取游标 `cursor`，
/// 只有重新打开文件才会重新采样系统状态。
pub struct SnapshotFile {
    /// 打开该节点时的挂载路径与目录项引用，用于 `fstat` 查询属性及维持挂载点生命周期。
    path: Path,
    /// 在 `open` 时刻采样并格式化完成的不可变文本内容。
    bytes: Vec<u8>,
    /// 顺序读写与 `lseek` 共享的字节偏移游标（单位：字节），由可睡眠互斥锁保护。
    cursor: SleepMutex<u64>,
    /// 打开文件状态标志（如 `O_RDONLY`、`O_NONBLOCK`），支持原子读取与更新。
    flags: AtomicU32,
}

/// 支持内存分配失败传播（Fallible Allocation）的快照文本格式化缓冲器。
///
/// 由于 `core::fmt::Write` 接口仅能返回无载荷的 `fmt::Error`，本结构体在扩容失败或长度溢出时
/// 将具体 [`FsError`] 暂存在 `error` 字段中，并在格式化结束调用 [`SnapshotText::finish`] 时还原。
struct SnapshotText {
    /// 累积格式化文本及允许非 UTF-8 的原始字节。
    bytes: Vec<u8>,
    /// 格式化过程中首次遇到的内存分配或长度溢出错误。
    error: Option<FsError>,
}

impl SnapshotText {
    /// 创建快照文本缓冲器并预分配 [`SNAPSHOT_INITIAL_CAPACITY`]（256 字节）的初始容量。
    fn try_new() -> Result<Self, FsError> {
        let mut bytes = Vec::new();
        bytes
            .try_reserve_exact(SNAPSHOT_INITIAL_CAPACITY)
            .map_err(|_| FsError::NoMemory)?;
        Ok(Self { bytes, error: None })
    }

    /// 可失败地追加原始字节，进程名不经过 UTF-8 转换。
    fn append_bytes(&mut self, bytes: &[u8]) -> fmt::Result {
        let result = self
            .bytes
            .len()
            .checked_add(bytes.len())
            .ok_or(FsError::FileTooLarge)
            .and_then(|_| {
                self.bytes
                    .try_reserve(bytes.len())
                    .map_err(|_| FsError::NoMemory)
            });
        if let Err(error) = result {
            self.error = Some(error);
            return Err(fmt::Error);
        }
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }

    /// 结束格式化并提取完整字节数组；若格式化过程中发生错误，则返回暂存的 [`FsError`]。
    fn finish(self, result: fmt::Result) -> Result<Vec<u8>, FsError> {
        match result {
            Ok(()) => Ok(self.bytes),
            Err(_) => Err(self
                .error
                .expect("snapshot formatting failed without source error")),
        }
    }
}

impl Write for SnapshotText {
    /// 在追加字符串片段前先执行长度溢出检查与可失败容量预留（`try_reserve`），失败时记录具体错误并中断格式化。
    fn write_str(&mut self, text: &str) -> fmt::Result {
        self.append_bytes(text.as_bytes())
    }
}

/// 创建并初始化只读的 `procfs` 内存文件系统实例，在 `/proc` 及 `/proc/net` 下安装固定信息节点。
///
/// 各节点仅登记对应的 [`MemorySpecial`] 角色标识，不在挂载阶段提前生成任何快照文本。
pub(crate) fn try_new(root_mode: u16, graph: Weak<TaskGraph>) -> Result<Arc<MemoryFs>, FsError> {
    let fs = MemoryFs::try_procfs(root_mode, graph)?;
    let root = fs.root();
    fs.initialize_special(&root, b"cmdline", MemorySpecial::Cmdline, 0o444, 0)?;
    fs.initialize_special(&root, b"mounts", MemorySpecial::Mounts, 0o444, 0)?;
    fs.initialize_special(&root, b"meminfo", MemorySpecial::Meminfo, 0o444, 0)?;
    fs.initialize_special(&root, b"uptime", MemorySpecial::Uptime, 0o444, 0)?;
    fs.initialize_special(&root, b"interrupts", MemorySpecial::Interrupts, 0o444, 0)?;

    let net = fs.initialize_directory(&root, b"net", 0o555)?;
    fs.initialize_special(&net, b"dev", MemorySpecial::NetDev, 0o444, 0)?;
    fs.initialize_special(&net, b"route", MemorySpecial::NetRoute, 0o444, 0)?;
    Ok(fs)
}

/// 打开 `/proc` 下的动态信息节点并生成当前时刻的文本快照。
///
/// - 调用上下文：普通可睡眠任务上下文。
/// - 参数：
///   - `path`：目标节点的解析路径。
///   - `flags`：打开标志位；必须为只读（`O_RDONLY`）且不含 `O_TRUNC`，否则返回 [`FsError::ReadOnly`]。
///   - `special`：节点对应的动态数据源类型。
///   - `vfs`：全局虚拟文件系统引用，用于生成 `/proc/mounts`。
///   - `caller`：调用方上下文；访问 `/proc/mounts` 时取调用方根目录过滤可见挂载点，访问 `/proc/net/*` 时取调用方所属的网络协议栈。
/// - 返回值：成功返回包含完整快照文本的 [`FileRef`]。
pub fn open(
    path: Path,
    flags: u32,
    special: MemorySpecial,
    vfs: &Vfs,
    caller: FileIoContext<'_>,
) -> Result<FileRef, FsError> {
    if flags & O_ACCMODE != O_RDONLY || flags & O_TRUNC != 0 {
        return Err(FsError::ReadOnly);
    }

    let bytes = match special {
        MemorySpecial::Cmdline => cmdline()?,
        MemorySpecial::Mounts => {
            let root = match caller {
                FileIoContext::Kernel => vfs.root(),
                FileIoContext::User(thread) => {
                    let runtime = thread.runtime().ok_or(FsError::InvalidOperation)?;
                    let fs = runtime.fs.lock();
                    fs.context.root().clone()
                }
            };
            vfs.mounts_text(&root)?
        }
        MemorySpecial::Meminfo => meminfo()?,
        MemorySpecial::Uptime => uptime()?,
        MemorySpecial::Interrupts => interrupts()?,
        MemorySpecial::NetDev | MemorySpecial::NetRoute => {
            let runtime = match caller {
                FileIoContext::User(thread) => thread.runtime().ok_or(FsError::InvalidOperation)?,
                FileIoContext::Kernel => return Err(FsError::InvalidOperation),
            };
            let net = runtime.net.as_ref().ok_or(FsError::NetworkUnreachable)?;
            if special == MemorySpecial::NetDev {
                net_dev(net)?
            } else {
                net_route(net)?
            }
        }
        _ => return Err(FsError::InvalidOperation),
    };

    Ok(Arc::try_new(SnapshotFile {
        path,
        bytes,
        cursor: SleepMutex::new(0),
        flags: AtomicU32::new(flags),
    })
    .map_err(|_| FsError::NoMemory)?)
}

/// 生成 `/proc/cmdline` 快照：原样拷贝固件/引导器传入的启动命令行字节并在末尾追加换行符 `\n`。
fn cmdline() -> Result<Vec<u8>, FsError> {
    let bootargs = boot::info().bootargs();
    let length = bootargs.len().checked_add(1).ok_or(FsError::FileTooLarge)?;
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(length)
        .map_err(|_| FsError::NoMemory)?;
    bytes.extend_from_slice(bootargs);
    bytes.push(b'\n');
    Ok(bytes)
}

/// 生成 `/proc/interrupts` 快照：按当前在线 CPU 列输出本地定时器中断、核间中断（IPI）以及已触发外部中断源的累计次数。
fn interrupts() -> Result<Vec<u8>, FsError> {
    // Step 1: 采样当前在线的 CPU 掩码，固定本次输出的列集合。
    let mut online = [false; MAX_CPUS];
    for (owner, column) in online.iter_mut().enumerate().take(cpu::count()) {
        *column = cpu::get(owner).online.load(Ordering::Acquire);
    }

    // Step 2: 在中断控制器锁外按行格式化表头、本地中断（timer/IPI）与外部中断源计数。
    let mut text = SnapshotText::try_new()?;
    let result = (|| {
        text.write_str("     ")?;
        for (owner, &is_online) in online.iter().enumerate() {
            if is_online {
                write!(text, "CPU{:<8}", owner)?;
            }
        }
        text.write_char('\n')?;

        for (number, label) in [(0, "timer"), (1, "IPI")] {
            write!(text, "{:>3}: ", number)?;
            for (owner, &is_online) in online.iter().enumerate() {
                if is_online {
                    let local = cpu::get(owner);
                    let count = if number == 0 {
                        local.timer_interrupts.load(Ordering::Relaxed)
                    } else {
                        local.ipi_interrupts.load(Ordering::Relaxed)
                    };
                    write!(text, "{:10} ", count)?;
                }
            }
            writeln!(text, "{}", label)?;
        }

        for source in 0..irq::source_count() {
            let Some(snapshot) = irq::source_snapshot(source) else {
                continue;
            };
            if !snapshot.observed {
                continue;
            }
            write!(text, "{:>3}: ", source + 2)?;
            for (owner, &is_online) in online.iter().enumerate() {
                if is_online {
                    write!(text, "{:10} ", snapshot.interrupts[owner])?;
                }
            }
            writeln!(text, "external source {}", source)?;
        }
        Ok(())
    })();
    text.finish(result)
}

/// 生成 `/proc/net/dev` 快照：先从网络栈拷贝各网卡接口的收发统计快照，再格式化为标准 Linux `/proc/net/dev` 表格。
///
/// 当前硬件未单独统计的 FIFO、帧错误、压缩、组播、冲突与载波错误列固定输出 `0`。
fn net_dev(net: &NetStack) -> Result<Vec<u8>, FsError> {
    let interfaces = net.interface_snapshots();
    let mut snapshots = [None; INTERFACE_COUNT];
    for (slot, interface) in snapshots.iter_mut().zip(interfaces.iter()) {
        if let Some(interface) = interface {
            *slot = Some((interface.id, net.statistics(interface.id)?));
        }
    }

    let mut text = SnapshotText::try_new()?;
    let result = (|| {
        text.write_str(concat!(
            "Inter-|   Receive                            ",
            "                    |  Transmit\n",
            " face |bytes    packets errs drop fifo frame ",
            "compressed multicast|bytes    packets errs ",
            "drop fifo colls carrier compressed\n",
        ))?;
        for (interface, stats) in snapshots.iter().flatten() {
            writeln!(
                text,
                "{:>6}: {:7} {:7} {:4} {:4} {:4} {:5} {:10} {:9} {:8} {:7} {:4} {:4} {:4} {:5} {:7} {:10}",
                interface.name(),
                stats.rx_bytes,
                stats.rx_packets,
                stats.rx_errors,
                stats.rx_dropped,
                0, 0, 0, 0,
                stats.tx_bytes,
                stats.tx_packets,
                stats.tx_errors,
                stats.tx_dropped,
                0, 0, 0, 0,
            )?;
        }
        Ok(())
    })();
    text.finish(result)
}

/// 生成 `/proc/net/route` 快照：读取当前网络栈的 IPv4 路由表快照，将目的网络、网关与子网掩码按小端序 8 位十六进制格式输出。
fn net_route(net: &NetStack) -> Result<Vec<u8>, FsError> {
    let routes = net.route_snapshots();
    let mut text = SnapshotText::try_new()?;
    let result = (|| {
        text.write_str(
            "Iface\tDestination\tGateway \tFlags\tRefCnt\tUse\tMetric\tMask\t\tMTU\tWindow\tIRTT\n",
        )?;
        for route in routes.iter().flatten() {
            let mask = if route.prefix_len == 0 {
                0
            } else {
                u32::MAX << (IPV4_ADDRESS_BITS - route.prefix_len)
            };
            let flags = if route.up { RTF_UP } else { 0 }
                | if route.gateway.is_some() {
                    RTF_GATEWAY
                } else {
                    0
                };
            writeln!(
                text,
                "{}\t{:08X}\t{:08X}\t{:04X}\t0\t0\t0\t{:08X}\t0\t0\t0",
                route.interface.name(),
                u32::from_le_bytes(route.destination),
                u32::from_le_bytes(route.gateway.unwrap_or([0; 4])),
                flags,
                u32::from_le_bytes(mask.to_be_bytes()),
            )?;
        }
        Ok(())
    })();
    text.finish(result)
}

/// 生成 `/proc/meminfo` 快照：根据物理页分配器统计计算受管总内存（`MemTotal`）、空闲内存（`MemFree`）及 Slab 占用（`Slab`），单位为 `kB`。
fn meminfo() -> Result<Vec<u8>, FsError> {
    let stats = mm::stats();
    let managed = stats.present_pages - stats.reserved_pages - stats.metadata_pages;
    let free = stats.buddy_free_pages + stats.pcp_cached_pages;
    let kb_per_page = mm::PAGE_SIZE / BYTES_PER_KIB;
    let mut text = SnapshotText::try_new()?;
    let result = write!(
        text,
        "MemTotal: {:8} kB\nMemFree:  {:8} kB\nSlab:     {:8} kB\n",
        managed * kb_per_page,
        free * kb_per_page,
        stats.slab_backing_pages * kb_per_page
    );
    text.finish(result)
}

/// 生成 `/proc/uptime` 快照：输出系统启动以来的单调运行秒数与所有 CPU 累计空闲（idle）秒数（均保留两位小数）。
fn uptime() -> Result<Vec<u8>, FsError> {
    let elapsed = time::now().as_nanos();
    let idle = (0..cpu::count())
        .map(|owner| u128::from(accounting::snapshot(owner).idle_ns))
        .sum::<u128>();
    let mut text = SnapshotText::try_new()?;
    let result = writeln!(
        text,
        "{}.{:02} {}.{:02}",
        elapsed / time::NSEC_PER_SEC,
        elapsed / UPTIME_TICK_NANOS % UPTIME_TICKS_PER_SEC,
        idle / u128::from(time::NSEC_PER_SEC),
        idle / u128::from(UPTIME_TICK_NANOS) % u128::from(UPTIME_TICKS_PER_SEC)
    );
    text.finish(result)
}

impl SnapshotFile {
    /// 从不可变快照 `self.bytes` 的指定偏移 `offset` 处向目标缓冲区拷贝数据。
    ///
    /// 无需持有内容锁；若拷贝途中发生用户态缺页错误，在已成功拷贝部分字节时返回已拷贝长度，在首字节即失败时返回错误。
    fn read_snapshot(&self, offset: u64, target: &mut ReadBuffer<'_>) -> Result<usize, FsError> {
        if self.bytes.len() as u64 <= offset || target.copied() == target.len() {
            return Ok(0);
        }
        let start = target.copied();
        let source = &self.bytes[offset as usize..];
        match target.push(source) {
            Ok(count) => Ok(count),
            Err(error) if target.copied() == start => Err(error),
            Err(_) => Ok(target.copied() - start),
        }
    }
}

impl FileLike for SnapshotFile {
    /// `/proc` 快照文件支持 `pread` 按显式字节偏移读取。
    fn positioned_io(&self) -> bool {
        true
    }

    /// 从当前游标位置顺序读取快照内容，并在成功拷贝后推进共享游标。
    fn read_into(
        &self,
        target: &mut ReadBuffer<'_>,
        _caller: FileIoContext<'_>,
    ) -> Result<usize, FsError> {
        require_read(self.flags())?;
        let mut cursor = self.cursor.lock();
        let count = self.read_snapshot(*cursor, target)?;
        *cursor += count as u64;
        Ok(count)
    }

    /// 从指定字节偏移 `offset` 处读取快照内容（`pread`），不修改共享游标。
    fn read_at_into(
        &self,
        offset: u64,
        target: &mut ReadBuffer<'_>,
        _caller: FileIoContext<'_>,
    ) -> Result<usize, FsError> {
        require_read(self.flags())?;
        self.read_snapshot(offset, target)
    }

    /// 返回打开该节点时的 [`Path`]，用于查询底层 Inode 属性并维持挂载点生命周期。
    fn path(&self) -> Option<&Path> {
        Some(&self.path)
    }

    /// `/proc` 动态快照文件不支持 `mmap` 页缓存映射，返回 `None`。
    fn mapping_inode(&self) -> Option<&InodeRef> {
        None
    }

    /// 原子读取当前打开文件实例的状态标志位。
    fn flags(&self) -> u32 {
        self.flags.load(Ordering::Relaxed)
    }

    /// 更新 `fcntl(F_SETFL)` 允许修改的文件状态标志位，不改变只读访问模式。
    fn set_status_flags(&self, raw: u32) -> Result<(), FsError> {
        set_status_flags(&self.flags, raw)
    }

    /// 原子设置或清除当前打开实例的 `O_NONBLOCK` 非阻塞标志位。
    fn set_nonblocking(&self, enabled: bool) -> Result<(), FsError> {
        set_nonblocking(&self.flags, enabled)
    }

    /// `/proc` 节点为只读快照，不支持截断操作，返回 [`FsError::ReadOnly`]。
    fn truncate(&self, _new_size: u64) -> Result<(), FsError> {
        Err(FsError::ReadOnly)
    }

    /// `/proc` 内存快照无持久化后端，不支持 `fsync` / `fdatasync`，返回 [`FsError::InvalidOperation`]。
    fn sync_range(&self, _range: Range<u64>, _data_only: bool) -> Result<(), FsError> {
        Err(FsError::InvalidOperation)
    }

    /// 根据快照文本字节长度计算并更新读取游标位置；若目标位置为负数或超出 `i64::MAX`，则返回 [`FsError::InvalidOperation`] 且不修改游标。
    fn seek(&self, from: SeekFrom) -> Result<u64, FsError> {
        let mut cursor = self.cursor.lock();
        let position = match from {
            SeekFrom::Start(position) => Some(position),
            SeekFrom::Current(delta) => cursor.checked_add_signed(delta),
            SeekFrom::End(delta) => (self.bytes.len() as u64).checked_add_signed(delta),
        }
        .ok_or(FsError::InvalidOperation)?;
        if (i64::MAX as u64) < position {
            return Err(FsError::InvalidOperation);
        }

        *cursor = position;
        Ok(position)
    }

    /// 查询文件元数据：Inode 身份与权限取自底层 `/proc` 节点，`size` 设为当前打开实例已生成的快照字节长度，`blocks` 为 `0`。
    fn stat(&self) -> Result<FileStat, FsError> {
        let mut stat = stat_path(&self.path)?;
        stat.size = self.bytes.len() as u64;
        stat.blocks = 0;
        Ok(stat)
    }

    /// 快照文件不是目录，拒绝 `readdir` 并返回 [`FsError::NotDirectory`]。
    fn readdir(&self) -> Result<Option<DirectoryEntry>, FsError> {
        Err(FsError::NotDirectory)
    }

    /// 快照文件不是目录，拒绝目录遍历并返回 [`FsError::NotDirectory`]。
    fn for_each_dirent(
        &self,
        _emit: &mut dyn FnMut(&DirectoryEntry) -> Result<bool, FsError>,
    ) -> Result<(), FsError> {
        Err(FsError::NotDirectory)
    }

    /// `/proc` 快照文件固定为只读，任何写入尝试均返回 [`FsError::BadFd`]。
    fn write_from(
        &self,
        _source: &mut WriteBuffer<'_>,
        _caller: FileIoContext<'_>,
    ) -> Result<usize, FsError> {
        Err(FsError::BadFd)
    }

    /// `/proc` 快照文件固定为只读，按偏移写入同样返回 [`FsError::BadFd`]。
    fn write_at_from(
        &self,
        _offset: u64,
        _source: &mut WriteBuffer<'_>,
        _caller: FileIoContext<'_>,
    ) -> Result<usize, FsError> {
        Err(FsError::BadFd)
    }
}

/// 解析规范十进制 PID 名称，拒绝空名、多位前导零、非数字及算术溢出。
fn parse_pid(name: &[u8]) -> Option<usize> {
    if name.is_empty() || (1 < name.len() && name[0] == b'0') {
        return None;
    }
    name.iter()
        .try_fold(0u32, |number, byte| {
            if !byte.is_ascii_digit() || (u32::MAX - 9) / 10 <= number {
                return None;
            }
            Some(number * 10 + u32::from(*byte - b'0'))
        })
        .map(|number| number as usize)
}

/// 按进程编号及节点角色生成稳定 inode 编号，不使用内存文件系统索引额度。
fn process_inode_number(pid: &Pid, offset: u64) -> u64 {
    PROCESS_INODE_BASE + pid.number() as u64 * 4 + offset
}

/// 直接创建绑定 PID 对象的进程节点，节点由目录项和打开引用持有。
fn process_inode(
    fs: &Arc<MemoryFs>,
    pid: Arc<Pid>,
    role: ProcessRole,
) -> Result<InodeRef, FsError> {
    let (kind, mode, links, offset) = match &role {
        ProcessRole::Directory(_) => (InodeKind::Directory, 0o040555, 2, 0),
        ProcessRole::Cmdline => (InodeKind::Regular, 0o100444, 1, 1),
        ProcessRole::Stat => (InodeKind::Regular, 0o100444, 1, 2),
    };
    let mut metadata = InodeMetadata::memory(kind, mode, 0, 0, links, 0);
    metadata.number = process_inode_number(&pid, offset);
    InodeRef::try_memory(
        fs,
        metadata,
        MemoryContent::Process(ProcessNode { pid, role }),
    )
}

/// 在没有文件系统索引锁的上下文中准备一个进程目录的固定子项。
fn process_child(name: &[u8], inode: InodeRef, cookie: u64) -> Result<MemoryEntry, FsError> {
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(name.len())
        .map_err(|_| FsError::NoMemory)?;
    bytes.extend_from_slice(name);
    Ok(MemoryEntry {
        name: Arc::try_new(bytes).map_err(|_| FsError::NoMemory)?,
        inode: inode.memory_arc(),
        kind: InodeKind::Regular,
        cookie,
    })
}

/// 从 proc 根目录按规范 PID 名称查找现存进程，并为原身份创建只读目录。
pub(crate) fn lookup_process(fs: &Arc<MemoryFs>, name: &[u8]) -> Result<Option<InodeRef>, FsError> {
    let Some(number) = parse_pid(name) else {
        return Ok(None);
    };
    let Some(pid) = fs
        .process_graph()
        .and_then(|graph| graph.process_pid(number))
    else {
        return Ok(None);
    };
    let mut entries = Vec::new();
    entries
        .try_reserve_exact(2)
        .map_err(|_| FsError::NoMemory)?;
    entries.push(process_child(
        b"cmdline",
        process_inode(fs, pid.clone(), ProcessRole::Cmdline)?,
        2,
    )?);
    entries.push(process_child(
        b"stat",
        process_inode(fs, pid.clone(), ProcessRole::Stat)?,
        3,
    )?);
    let role = ProcessRole::Directory(SpinLock::new(MemoryDirectory {
        parent: fs.root().number(),
        next_cookie: 4,
        entries,
    }));
    process_inode(fs, pid, role).map(Some)
}

/// 从固定项之后按 TGID 升序枚举进程，以 PID 数值游标续读且不重复前项。
pub(crate) fn next_process(
    fs: &Arc<MemoryFs>,
    cookie: u64,
) -> Result<Option<DirectoryEntry>, FsError> {
    if PROCESS_COOKIE_END <= cookie {
        return Ok(None);
    }
    let start = cookie.saturating_sub(PROCESS_COOKIE_BASE) as usize;
    let Some(pid) = fs
        .process_graph()
        .and_then(|graph| graph.next_process(start))
    else {
        return Ok(None);
    };
    let mut text = SnapshotText::try_new()?;
    let result = write!(text, "{}", pid.number());
    let name = text.finish(result)?;
    let next_cookie = PROCESS_COOKIE_BASE + pid.number() as u64 + 1;
    Ok(Some(DirectoryEntry {
        number: process_inode_number(&pid, 0),
        name,
        kind: Some(InodeKind::Directory),
        next_cookie,
        next_cursor: DirectoryCursor::Memory(next_cookie),
    }))
}

/// 打开进程信息节点并冻结本次读取内容；已回收的原身份返回 ESRCH。
pub(crate) fn open_process(path: Path, flags: u32) -> Result<FileRef, FsError> {
    if flags & O_ACCMODE != O_RDONLY || flags & O_TRUNC != 0 {
        return Err(FsError::ReadOnly);
    }
    let inode = path.inode()?;
    let node = inode
        .process_node()
        .expect("process open has no process node");
    let status = inode
        .memory_fs()?
        .process_graph()
        .and_then(|graph| graph.process_status(&node.pid))
        .ok_or(FsError::NoProcess)?;
    let bytes = match node.role {
        ProcessRole::Stat => format_process_stat(&collect_process_stat(&status))?,
        ProcessRole::Cmdline => process_cmdline(&status)?,
        ProcessRole::Directory(_) => return Err(FsError::IsDirectory),
    };
    Ok(Arc::try_new(SnapshotFile {
        path,
        bytes,
        cursor: SleepMutex::new(0),
        flags: AtomicU32::new(flags),
    })
    .map_err(|_| FsError::NoMemory)?)
}

/// 不持有内核对象或锁的进程统计采样，供纯文本格式化使用。
struct ProcessStat {
    /// 用户可见线程组编号。
    pid: usize,
    /// 最近执行路径生成的原始进程名字节。
    comm: TaskComm,
    /// 单字节运行、睡眠、停止或僵尸状态。
    state: u8,
    /// 父进程线程组编号。
    parent: usize,
    /// 当前进程组编号。
    pgid: usize,
    /// 当前会话编号。
    sid: usize,
    /// 控制终端的编码设备号，无终端时为零。
    tty_nr: u64,
    /// 控制终端前台进程组，无终端时为负一。
    tpgid: i32,
    /// 所有组成员已结算的 CPU 纳秒时间。
    own: CpuTime,
    /// 已回收子代的 CPU 纳秒时间。
    children: CpuTime,
    /// 当前尚未退出的组成员数量。
    members: usize,
    /// 进程身份创建时的单调纳秒数。
    started_ns: u64,
    /// 进程退出通知父进程的信号编号。
    exit_signal: u8,
    /// 组首当前或退出时的调度策略，不含 fork 重置标志。
    policy: Policy,
    /// 组首当前或退出时的实时优先级。
    rt_priority: u8,
}

/// 在任务图锁外分别读取组关系、时间与调度状态，不把各锁的快照当作原子事务。
fn collect_process_stat(status: &ProcessStatus) -> ProcessStat {
    let group = &status.group;
    let (policy, rt_priority) = match status.leader_task.as_ref() {
        Some(task) => {
            let (policy, priority, _) = crate::sched::parameters(task);
            (policy, priority)
        }
        None => (status.leader_policy, status.leader_priority),
    };
    let state = match status.leader_task.as_ref() {
        None => b'Z',
        Some(_) if group.signals().has_stopped_jobs() => b'T',
        Some(task) => match crate::sched::state(task) {
            State::Ready | State::Running(_) | State::SwitchingOut(_, _) => b'R',
            State::Blocked => b'S',
            State::Exited => b'Z',
        },
    };
    let (tty_nr, tpgid) = group.terminal_status();
    ProcessStat {
        pid: group.tgid(),
        comm: group.comm(),
        state,
        parent: status.parent,
        pgid: group.pgid(),
        sid: group.sid(),
        tty_nr,
        tpgid,
        policy,
        rt_priority,
        own: group.time().snapshot(),
        children: group.time().children(),
        members: status.members,
        started_ns: group.started_ns(),
        exit_signal: group.exit_signal(),
    }
}

/// 将纳秒数转换为用户 ABI 的时钟滴答数，以除法避免乘法溢出。
fn process_ticks(ns: u64) -> i128 {
    i128::from(ns / (time::NSEC_PER_SEC / USER_HZ))
}

/// 只根据数值与进程名字节格式化一行五十二字段的 stat 文本，不访问内核状态。
fn format_process_stat(stat: &ProcessStat) -> Result<Vec<u8>, FsError> {
    let mut text = SnapshotText::try_new()?;
    let result = (|| {
        write!(text, "{} (", stat.pid)?;
        text.append_bytes(stat.comm.as_bytes())?;
        write!(text, ") {}", char::from(stat.state))?;
        let fields: [i128; 49] = [
            // 字段 4–8：父进程、进程组、会话与控制终端。
            stat.parent as i128,
            stat.pgid as i128,
            stat.sid as i128,
            i128::from(stat.tty_nr),
            i128::from(stat.tpgid),
            // 字段 9–13：标志与缺页计数。
            0,
            0,
            0,
            0,
            0,
            // 字段 14–22：CPU 时间、调度属性、成员数及创建时间。
            process_ticks(stat.own.user_ns),
            process_ticks(stat.own.kernel_ns),
            process_ticks(stat.children.user_ns),
            process_ticks(stat.children.kernel_ns),
            match stat.policy {
                Policy::Other => 20,
                Policy::Fifo => -(i128::from(stat.rt_priority) + 1),
            },
            0,
            stat.members as i128,
            0,
            process_ticks(stat.started_ns),
            // 字段 23–30：地址空间计数、常驻内存上限与隐藏地址。
            0,
            0,
            i128::from(RSS_LIMIT_INFINITY),
            0,
            0,
            0,
            0,
            0,
            // 字段 31–37：旧信号位图、等待地址与保留值。
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            // 字段 38–44：退出信号、处理器、实时调度属性与扩展计数。
            i128::from(stat.exit_signal),
            0,
            i128::from(stat.rt_priority),
            match stat.policy {
                Policy::Other => 0,
                Policy::Fifo => 1,
            },
            0,
            0,
            0,
            // 字段 45–52：隐藏的程序地址和退出状态。
            0,
            0,
            0,
            0,
            0,
            0,
            0,
            0,
        ];
        for value in fields {
            write!(text, " {}", value)?;
        }
        text.write_str("\n")
    })();
    text.finish(result)
}

/// 从组首线程当前地址空间逐页读取参数区，首个不可读页只截短结果。
fn process_cmdline(status: &ProcessStatus) -> Result<Vec<u8>, FsError> {
    let Some(mm) = status.leader.as_ref().and_then(|leader| leader.mm()) else {
        return Ok(Vec::new());
    };
    let arguments = mm.arguments();
    let mut bytes = Vec::new();
    bytes
        .try_reserve_exact(arguments.len())
        .map_err(|_| FsError::NoMemory)?;
    bytes.resize(arguments.len(), 0);
    // Mm 的读取路径按目标页表取得内核直映地址，mmap 读锁先于页表锁；
    // 缺页在锁外完成，原始指令复制允许目标 CPU 并发修改字节而不构造用户引用。
    let progress = mm.copyin(&mut bytes, arguments.start);
    bytes.truncate(progress.copied);
    Ok(bytes)
}
