//! 固定描述符槽负责编号与发表，具体文件独立拥有路径和共享位置。

use alloc::vec::Vec;
use core::mem;
use core::sync::atomic::{AtomicUsize, Ordering};

use crate::fs::file::{O_DIRECTORY, O_NOFOLLOW};
use crate::fs::{
    open_path_with_flags, DirectoryEntry, FileIoContext, FileRef, FileStat, FsContext, FsError,
    ResolveMode, SeekFrom, Vfs, WriteBuffer,
};
use crate::sync::SpinLock;

/// 描述符自己的属性，不随共享文件位置传播。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DescriptorFlags {
    pub close_on_exec: bool,
}

/// 只读打开的路径约束与新描述符属性。
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct OpenOptions {
    pub directory: bool,
    pub no_follow: bool,
    pub close_on_exec: bool,
}

/// 槽尚未发表时没有文件引用，只有取得预留责任的对象能完成或取消它。
enum Slot {
    Vacant,
    Reserved,
    Open {
        file: FileRef,
        flags: DescriptorFlags,
    },
}

/// 固定上限的内核描述符表；具体文件调用与最后引用释放均在短锁外。
/// ponytail: 有限槽内线性选择最小空号，容量显著增大时改用空槽位图。
pub struct FileTable {
    slots: SpinLock<Vec<Slot>>,
    capacity: usize,
    users: AtomicUsize,
}

/// 借用原表的单次预留责任；移动后只能发表一次，未发表的析构释放原槽。
pub struct FileReservation<'a> {
    table: &'a FileTable,
    fd: usize,
    active: bool,
}

impl FileTable {
    /// 在锁外一次预留并初始化全部槽位，零容量无可用描述符而返回 InvalidOperation。
    pub fn try_new(capacity: usize) -> Result<Self, FsError> {
        if capacity == 0 {
            return Err(FsError::InvalidOperation);
        }
        let mut slots = Vec::new();
        slots
            .try_reserve_exact(capacity)
            .map_err(|_| FsError::NoMemory)?;
        slots.resize_with(capacity, || Slot::Vacant);
        Ok(Self {
            slots: SpinLock::new(slots),
            capacity,
            users: AtomicUsize::new(0),
        })
    }

    /// 锁外准备完整空表，父锁内仅复制 Open 引用与属性，Reserved 不转移。
    pub fn try_clone(&self) -> Result<Self, FsError> {
        let child = Self::try_new(self.capacity)?;
        {
            let mut target = child.slots.lock();
            let source = self.slots.lock();
            for (source, target) in source.iter().zip(target.iter_mut()) {
                if let Slot::Open { file, flags } = source {
                    *target = Slot::Open {
                        file: file.clone(),
                        flags: *flags,
                    };
                }
            }
        }
        Ok(child)
    }

    /// 增加真实运行绑定责任，普通表引用与诊断拥有不参与计数。
    pub(crate) fn add_user(&self) {
        self.users
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
                count.checked_add(1)
            })
            .expect("file table user count overflow");
    }

    /// 归还真实运行绑定，表与文件的最后引用由调用者在短锁外释放。
    pub(crate) fn remove_user(&self) {
        let previous = self.users.fetch_sub(1, Ordering::AcqRel);
        assert!(previous != 0, "file table user count underflow");
    }

    /// 返回真实运行绑定数，不将诊断引用误判为共享描述符表的执行者。
    pub(crate) fn user_count(&self) -> usize {
        self.users.load(Ordering::Acquire)
    }

    /// 返回构造时固定的槽数，预留和复制描述符都不能扩大它。
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// 同一短锁内选最小空槽并交出独占发表责任，表满不改变已有槽。
    pub fn reserve(&self) -> Result<FileReservation<'_>, FsError> {
        let mut slots = self.slots.lock();
        let fd = slots
            .iter()
            .position(|slot| matches!(slot, Slot::Vacant))
            .ok_or(FsError::TooManyFiles)?;
        slots[fd] = Slot::Reserved;
        Ok(FileReservation {
            table: self,
            fd,
            active: true,
        })
    }

    /// 短锁内取得已发表文件的强引用；调用者随后不重查可复用的整数编号。
    pub fn lookup(&self, fd: usize) -> Result<FileRef, FsError> {
        let slots = self.slots.lock();
        match slots.get(fd) {
            Some(Slot::Open { file, .. }) => Ok(file.clone()),
            _ => Err(FsError::BadFd),
        }
    }

    /// 读取当前描述符的属性，未发表的预留与空槽均不作为打开文件。
    pub fn flags(&self, fd: usize) -> Result<DescriptorFlags, FsError> {
        let slots = self.slots.lock();
        match slots.get(fd) {
            Some(Slot::Open { flags, .. }) => Ok(*flags),
            _ => Err(FsError::BadFd),
        }
    }

    /// 同一次槽锁中替换 descriptor 属性，不改变其共享 OFD。
    pub fn set_flags(&self, fd: usize, replacement: DescriptorFlags) -> Result<(), FsError> {
        let mut slots = self.slots.lock();
        match slots.get_mut(fd) {
            Some(Slot::Open { flags, .. }) => {
                *flags = replacement;
                Ok(())
            }
            _ => Err(FsError::BadFd),
        }
    }

    /// 普通可睡眠 Task 先预留，再解析真实路径并构造完整文件；任何失败由预留析构取消。
    pub fn open(
        &self,
        vfs: &Vfs,
        context: &FsContext,
        pathname: &[u8],
        options: OpenOptions,
    ) -> Result<usize, FsError> {
        let reservation = self.reserve()?;
        let mode = if options.no_follow {
            ResolveMode::NoFollow
        } else {
            ResolveMode::Follow
        };
        let path = vfs.resolve(context, pathname, mode)?;
        let flags = if options.directory { O_DIRECTORY } else { 0 }
            | if options.no_follow { O_NOFOLLOW } else { 0 };
        let file = open_path_with_flags(vfs, path, flags, FileIoContext::Kernel)?;
        Ok(reservation.publish(
            file,
            DescriptorFlags {
                close_on_exec: options.close_on_exec,
            },
        ))
    }

    /// 在表锁外读取原文件并推进其共享位置，关闭或复用 fd 不改变本次文件身份。
    pub fn read(&self, fd: usize, buffer: &mut [u8]) -> Result<usize, FsError> {
        let file = self.lookup(fd)?;
        file.read(buffer)
    }

    /// 在表锁外按 byte offset 读取原文件，不修改其共享位置。
    pub fn read_at(&self, fd: usize, offset: u64, buffer: &mut [u8]) -> Result<usize, FsError> {
        let file = self.lookup(fd)?;
        file.read_at(offset, buffer)
    }

    /// 在表锁外修改具体文件的共享位置，保留它的边界和类型错误。
    pub fn seek(&self, fd: usize, from: SeekFrom) -> Result<u64, FsError> {
        let file = self.lookup(fd)?;
        file.seek(from)
    }

    /// 在表锁外读取真实 inode 元数据，不占用文件位置锁。
    pub fn stat(&self, fd: usize) -> Result<FileStat, FsError> {
        let file = self.lookup(fd)?;
        file.stat()
    }

    /// 在表锁外消费目录的下一项，共享枚举位置由具体目录文件保护。
    pub fn readdir(&self, fd: usize) -> Result<Option<DirectoryEntry>, FsError> {
        let file = self.lookup(fd)?;
        file.readdir()
    }

    /// 内核顺序写只绑定输入，不在描述符表内复制 inode 写循环。
    pub fn write(&self, fd: usize, buffer: &[u8]) -> Result<usize, FsError> {
        self.write_from(fd, &mut WriteBuffer::kernel(buffer))
    }

    /// 内核按位置写经过同一核心，不取得 OFD 共享位置锁。
    pub fn write_at(&self, fd: usize, offset: u64, buffer: &[u8]) -> Result<usize, FsError> {
        self.write_at_from(fd, offset, &mut WriteBuffer::kernel(buffer))
    }

    /// 表短锁外持原 OFD 执行顺序写，关闭复用 fd 不改变本次文件身份。
    pub fn write_from(&self, fd: usize, source: &mut WriteBuffer<'_>) -> Result<usize, FsError> {
        self.lookup(fd)?.write_from(source, FileIoContext::Kernel)
    }

    /// 表短锁外按明确位置写入，实际追加和错误均由原 OFD 提供。
    pub fn write_at_from(
        &self,
        fd: usize,
        offset: u64,
        source: &mut WriteBuffer<'_>,
    ) -> Result<usize, FsError> {
        self.lookup(fd)?
            .write_at_from(offset, source, FileIoContext::Kernel)
    }

    /// 先移出打开槽并使编号立即可复用，解锁后才释放表拥有的文件引用。
    pub fn close(&self, fd: usize) -> Result<(), FsError> {
        let old = {
            let mut slots = self.slots.lock();
            let slot = slots.get_mut(fd).ok_or(FsError::BadFd)?;
            if !matches!(slot, Slot::Open { .. }) {
                return Err(FsError::BadFd);
            }
            mem::replace(slot, Slot::Vacant)
        };
        drop(old);
        Ok(())
    }

    /// 逐槽移出带CLOEXEC的打开文件，锁外最后释放，不分配或改变固定容量。
    pub fn close_on_exec(&self) {
        for fd in 0..self.capacity {
            let old = {
                let mut slots = self.slots.lock();
                let slot = &mut slots[fd];
                if matches!(slot, Slot::Open { flags, .. } if flags.close_on_exec) {
                    Some(mem::replace(slot, Slot::Vacant))
                } else {
                    None
                }
            };
            drop(old);
        }
    }

    /// 内核最小空号复制清除新描述符 CLOEXEC，仍共享原 OFD。
    pub fn dup(&self, fd: usize) -> Result<usize, FsError> {
        self.dup_min(fd, 0, DescriptorFlags::default())
    }

    /// 验证源后选择不小于 min 的最小空槽，固定表不隐式扩容。
    pub fn dup_min(&self, fd: usize, min: usize, flags: DescriptorFlags) -> Result<usize, FsError> {
        let mut slots = self.slots.lock();
        let Some(Slot::Open { file, .. }) = slots.get(fd) else {
            return Err(FsError::BadFd);
        };
        if self.capacity <= min {
            return Err(FsError::InvalidOperation);
        }
        let target = slots
            .iter()
            .enumerate()
            .skip(min)
            .find(|(_, slot)| matches!(slot, Slot::Vacant))
            .map(|(fd, _)| fd)
            .ok_or(FsError::TooManyFiles)?;
        let file = file.clone();
        slots[target] = Slot::Open { file, flags };
        Ok(target)
    }

    /// 内核精确复制允许同号保留原属性，其余复制清除 CLOEXEC。
    pub fn dup_exact(&self, source: usize, target: usize) -> Result<usize, FsError> {
        self.replace_duplicate(source, target, DescriptorFlags::default(), true)
    }

    /// 用户 dup3 原子替换指定槽，同号无效，Reserved 不可由其他责任替换。
    pub fn dup_exact_flags(
        &self,
        source: usize,
        target: usize,
        flags: DescriptorFlags,
    ) -> Result<usize, FsError> {
        self.replace_duplicate(source, target, flags, false)
    }

    /// 同锁验证并移动完整目标拥有值，最后 put 在表锁之外执行。
    fn replace_duplicate(
        &self,
        source: usize,
        target: usize,
        flags: DescriptorFlags,
        allow_same: bool,
    ) -> Result<usize, FsError> {
        if !allow_same && source == target {
            return Err(FsError::InvalidOperation);
        }
        let old = {
            let mut slots = self.slots.lock();
            let Some(Slot::Open { file, .. }) = slots.get(source) else {
                return Err(FsError::BadFd);
            };
            let target_slot = slots.get(target).ok_or(FsError::BadFd)?;
            if matches!(target_slot, Slot::Reserved) {
                return Err(FsError::Busy);
            }
            if source == target {
                return Ok(target);
            }
            let replacement = Slot::Open {
                file: file.clone(),
                flags,
            };
            mem::replace(&mut slots[target], replacement)
        };
        drop(old);
        Ok(target)
    }
}

impl FileReservation<'_> {
    /// 返回自己保留的整数编号，该槽在发表前仍不能查找或关闭。
    pub fn fd(&self) -> usize {
        self.fd
    }

    /// 消费唯一预留责任并移动完整文件入槽，既有固定槽无需分配且不能被外人替换。
    pub fn publish(mut self, file: FileRef, flags: DescriptorFlags) -> usize {
        {
            let mut slots = self.table.slots.lock();
            debug_assert!(matches!(slots[self.fd], Slot::Reserved));
            slots[self.fd] = Slot::Open { file, flags };
            self.active = false;
        }
        self.fd
    }
}

impl Drop for FileReservation<'_> {
    /// 只取消自己尚未发表的原槽；已发表文件的引用由描述符表独立拥有。
    fn drop(&mut self) {
        if self.active {
            let mut slots = self.table.slots.lock();
            debug_assert!(matches!(slots[self.fd], Slot::Reserved));
            slots[self.fd] = Slot::Vacant;
        }
    }
}
