//! System V 共享内存的 key、ID 和连接生命周期。
//!
//! 每个段持有唯一的 `SharedAnon`，各地址空间的连接凭证显式计数；VMA 快照只保留连接编号，
//! 不承担连接计数。`IPC_RMID` 立即取消 key，已连接的段保留到最后一个连接完成页表失效后退役。
//! 段表的睡眠锁不会调用 Mm；Mm 可以在 mmap 锁内取得段表锁，连接最后释放发生在 TLB 同步之后。

use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::mm::anon::SharedAnon;
use crate::mm::space::VmError;
use crate::mm::PAGE_SIZE;
use crate::sync::SleepMutex;
use crate::task::Credentials;

/// 新建命名段的标志位。
pub(crate) const IPC_CREAT: u32 = 0o1000;
/// 与 IPC_CREAT 同用时拒绝已有 key。
pub(crate) const IPC_EXCL: u32 = 0o2000;
/// 允许同时存在的共享段数量。
const SHMMNI: usize = 4096;
/// 段大小的字节上限，保留页上取整所需的算术余量。
const SHMMAX: usize = usize::MAX - (1 << 24);

/// 共享段操作的领域错误，由系统调用层转换为 errno。
pub(crate) enum ShmError {
    /// ID、大小或参数不合法。
    Invalid,
    /// 指定 key 不存在且没有请求创建。
    NotFound,
    /// 排他创建请求遇到已有 key。
    Exists,
    /// 段对象或段表分配失败。
    NoMemory,
    /// 段数量或 ID 空间耗尽。
    NoSpace,
    /// 请求的访问权限不被段权限允许。
    Permission,
    /// 删除请求不属于段所有者或创建者。
    NotPermitted,
}

/// 一个共享段的稳定身份、真实字节大小和唯一匿名页后备。
pub(crate) struct ShmSegment {
    /// 发布后不复用的非负用户 ID。
    id: i32,
    /// shmget 原始请求的字节数，不按页取整。
    pub(crate) size: usize,
    /// 所有连接和 fork 共同使用的匿名页容器。
    pub(crate) owner: Arc<SharedAnon>,
    /// 创建时的低九位访问权限；当前不提供 IPC_SET。
    mode: u32,
    /// 创建者及所有者的 UID/GID，当前不提供所有者修改。
    credentials: Credentials,
}

/// 由全局段表锁保护的可变命名与连接状态。
struct SegmentEntry {
    /// 活跃 key；删除后改为 IPC_PRIVATE（零）。
    key: i32,
    /// 是否已收到 IPC_RMID。
    removed: bool,
    /// 已建立及正在准备的连接数量，不包含 VMA 或诊断 Arc。
    attachments: usize,
    /// 在最后一个已删除连接退役前始终保有的段对象。
    segment: Arc<ShmSegment>,
}

/// 单个 IPC 名字空间的段表，锁内分配只涉及可失败的控制对象。
struct SegmentTable {
    /// 当前仍可按 ID 查找的段。
    entries: Vec<SegmentEntry>,
    // ponytail: 段表顺序扫描；大量段成为实测热点后再增加 key/ID 索引。
    /// 下一个 ID；不回绕复用，避免旧 ID 指向新段。
    next_id: usize,
}

/// 所有共享段的唯一命名表和连接计数锁。
static SEGMENTS: SleepMutex<SegmentTable> = SleepMutex::new(SegmentTable {
    entries: Vec::new(),
    next_id: 0,
});

impl ShmSegment {
    /// 按所有者、组或其他权限检查读写执行请求；root 保留现有凭据模型的权限豁免。
    fn check_access(&self, credentials: Credentials, requested: u32) -> Result<(), ShmError> {
        let granted = if credentials.uid == self.credentials.uid {
            self.mode >> 6
        } else if credentials.gid == self.credentials.gid {
            self.mode >> 3
        } else {
            self.mode
        };
        if credentials.uid != 0 && requested & !granted & 0o7 != 0 {
            return Err(ShmError::Permission);
        }
        Ok(())
    }
}

/// 查找或创建共享段；IPC_PRIVATE 总是创建新段，已有 key 不改变原大小和后备。
pub(crate) fn get(
    key: i32,
    size: usize,
    flags: u32,
    credentials: Credentials,
) -> Result<i32, ShmError> {
    let mut table = SEGMENTS.lock();
    if key != 0 {
        if let Some(entry) = table.entries.iter().find(|entry| entry.key == key) {
            if flags & (IPC_CREAT | IPC_EXCL) == (IPC_CREAT | IPC_EXCL) {
                return Err(ShmError::Exists);
            }
            if entry.segment.size < size {
                return Err(ShmError::Invalid);
            }
            let requested = (flags >> 6) | (flags >> 3) | flags;
            entry.segment.check_access(credentials, requested)?;
            return Ok(entry.segment.id);
        }
        if flags & IPC_CREAT == 0 {
            return Err(ShmError::NotFound);
        }
    }
    if size == 0 || SHMMAX < size {
        return Err(ShmError::Invalid);
    }
    if SHMMNI <= table.entries.len() || (i32::MAX as usize) < table.next_id {
        return Err(ShmError::NoSpace);
    }

    let id = table.next_id as i32;
    let owner = SharedAnon::try_new().map_err(|_| ShmError::NoMemory)?;
    let segment = Arc::try_new(ShmSegment {
        id,
        size,
        owner,
        mode: flags & 0o777,
        credentials,
    })
    .map_err(|_| ShmError::NoMemory)?;
    table
        .entries
        .try_reserve(1)
        .map_err(|_| ShmError::NoMemory)?;
    table.entries.push(SegmentEntry {
        key,
        removed: false,
        attachments: 0,
        segment,
    });
    table.next_id += 1;
    Ok(id)
}

/// 删除 key 并标记销毁；已有连接仍可访问，也允许在末连接消失前按 ID 再连接。
pub(crate) fn remove(id: i32, credentials: Credentials) -> Result<(), ShmError> {
    let mut table = SEGMENTS.lock();
    let index = table
        .entries
        .iter()
        .position(|entry| entry.segment.id == id)
        .ok_or(ShmError::Invalid)?;
    let entry = &mut table.entries[index];
    if credentials.uid != 0 && credentials.uid != entry.segment.credentials.uid {
        return Err(ShmError::NotPermitted);
    }
    entry.key = 0;
    entry.removed = true;
    let retired = (entry.attachments == 0).then(|| table.entries.swap_remove(index));
    let storage = table
        .entries
        .is_empty()
        .then(|| core::mem::take(&mut table.entries));
    drop(table);
    drop(retired);
    drop(storage);
    Ok(())
}

/// 一个地址空间内一次独立 shmat 的连接凭证；切分 VMA 不复制该凭证。
pub(crate) struct ShmAttachment {
    /// 所属 Mm 内不复用的连接编号，零只用于尚未映射的预留连接。
    pub(crate) identity: usize,
    /// shmat 最初返回的页对齐地址，部分 munmap 后仍保持不变。
    pub(crate) base: usize,
    /// 稳定段身份及其真实 SharedAnon 后备。
    pub(crate) segment: Arc<ShmSegment>,
}

impl ShmAttachment {
    /// 在段表锁下预留连接，避免 shmat 准备期间与 IPC_RMID 竞争销毁；失败由 Drop 回滚。
    pub(crate) fn acquire(
        id: i32,
        credentials: Credentials,
        permissions: u32,
    ) -> Result<Self, ShmError> {
        let mut table = SEGMENTS.lock();
        let entry = table
            .entries
            .iter_mut()
            .find(|entry| entry.segment.id == id)
            .ok_or(ShmError::Invalid)?;
        entry.segment.check_access(credentials, permissions)?;
        entry.attachments = entry.attachments.checked_add(1).ok_or(ShmError::NoMemory)?;
        Ok(Self {
            identity: 0,
            base: 0,
            segment: Arc::clone(&entry.segment),
        })
    }

    /// fork 为子 Mm 创建独立连接；父连接保证已删除的段在此期间仍在段表中。
    pub(crate) fn try_fork(&self) -> Result<Self, VmError> {
        let mut table = SEGMENTS.lock();
        let entry = table
            .entries
            .iter_mut()
            .find(|entry| entry.segment.id == self.segment.id)
            .expect("fork lost a live shared segment");
        assert!(Arc::ptr_eq(&entry.segment, &self.segment));
        entry.attachments = entry.attachments.checked_add(1).ok_or(VmError::NoMemory)?;
        Ok(Self {
            identity: self.identity,
            base: self.base,
            segment: Arc::clone(&self.segment),
        })
    }

    /// 返回映射占用的页上取整字节数；创建时的 SHMMAX 已保证加法不溢出。
    pub(crate) fn length(&self) -> usize {
        (self.segment.size + PAGE_SIZE - 1) & !(PAGE_SIZE - 1)
    }
}

impl Drop for ShmAttachment {
    /// 释放一笔真实连接；调用方须先撤除该连接全部 PTE 并完成全核 TLB 同步。
    fn drop(&mut self) {
        let mut table = SEGMENTS.lock();
        let index = table
            .entries
            .iter()
            .position(|entry| entry.segment.id == self.segment.id)
            .expect("detach lost a live shared segment");
        let entry = &mut table.entries[index];
        assert!(Arc::ptr_eq(&entry.segment, &self.segment));
        assert!(0 < entry.attachments, "shared attachment count underflow");
        entry.attachments -= 1;
        let destroy = entry.removed && entry.attachments == 0;
        let retired = destroy.then(|| table.entries.swap_remove(index));
        let storage = table
            .entries
            .is_empty()
            .then(|| core::mem::take(&mut table.entries));
        drop(table);
        drop(retired);
        drop(storage);
    }
}
