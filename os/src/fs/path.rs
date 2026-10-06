//! 路径保活名字与挂载位置；逐组件查找和 I/O 不跨挂载表短锁。

use alloc::borrow::Cow;
use alloc::sync::Arc;
use alloc::vec::Vec;

use crate::fs::{DentryCache, DentryRef, Ext4Volume, FsError, InodeCache, InodeKind, InodeRef};
use crate::sync::SpinLock;

/// 只控制没有尾随斜杠的最终符号链接，中间链接始终跟随。
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ResolveMode {
    Follow,
    NoFollow,
}

/// 名字对象与挂载位置共同构成身份，私有引用不允许绕过挂载表取得新视图。
#[derive(Clone)]
pub struct Path {
    mount: Arc<Mount>,
    dentry: DentryRef,
}

/// 子挂载只保活父视图，父挂载不反持子挂载；登记表负责向下取得引用。
struct Mount {
    volume: Arc<Ext4Volume>,
    root: DentryRef,
    covered: Option<Path>,
}

/// 一次查找的真实目录边界，不依赖当前 Task 或用户凭据。
#[derive(Clone)]
pub struct FsContext {
    root: Path,
    cwd: Path,
}

/// 名字缓存独立，inode 缓存域和实际卷身份由调用者共享。
/// ponytail: 有限挂载预算内线性查找，预算显著增大时改用有限散列表。
pub struct Vfs {
    dcache: DentryCache,
    inodes: Arc<InodeCache>,
    // 根挂载固定占第一个槽；所有向下穿越与卸载检查共用这把短锁。
    mounts: SpinLock<Vec<Arc<Mount>>>,
    mount_capacity: usize,
}

impl Path {
    /// 借用稳定名字对象，名字缓存淘汰不改变当前路径身份。
    pub fn dentry(&self) -> &DentryRef {
        &self.dentry
    }

    /// 等待真实查找结果并借用共享 inode，不建立另一份文件页索引。
    pub fn inode(&self) -> Result<&InodeRef, FsError> {
        self.dentry.inode()
    }

    /// 同时比较挂载实例和名字实例，同一目录在不同视图中的位置不能混同。
    pub fn ptr_eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.mount, &other.mount) && self.dentry.ptr_eq(&other.dentry)
    }

    /// 借用该挂载保活的实际卷，文件读取继续使用 inode 自己的 mapping。
    pub fn filesystem(&self) -> &Arc<Ext4Volume> {
        &self.mount.volume
    }

    /// 按真实 owner/group/other mode 检查最终访问，写操作始终受只读卷约束。
    pub fn check_access(&self, uid: u32, gid: u32, mask: u16) -> Result<(), FsError> {
        if mask & !7 != 0 {
            return Err(FsError::InvalidOperation);
        }
        if mask & 2 != 0 {
            return Err(FsError::ReadOnly);
        }
        let data = self.inode()?.metadata()?;
        if uid == 0 {
            if mask & 1 != 0 && data.kind != InodeKind::Directory && data.mode & 0o111 == 0 {
                return Err(FsError::Permission);
            }
            return Ok(());
        }
        let shift = if uid == data.uid {
            6
        } else if gid == data.gid {
            3
        } else {
            0
        };
        if (data.mode >> shift) & mask == mask {
            Ok(())
        } else {
            Err(FsError::Permission)
        }
    }

    /// 返回一个真实祖先边；挂载根的上一个位置是父视图覆盖点。
    fn topology_parent(&self) -> Option<Self> {
        if self.dentry.ptr_eq(&self.mount.root) {
            return self.mount.covered.clone();
        }
        self.dentry.parent().map(|parent| Self {
            mount: self.mount.clone(),
            dentry: parent.clone(),
        })
    }

    /// 从挂载根退回覆盖点后再取普通父目录，途中遇到本次 root 就停止。
    fn parent_within(&self, root: &Self) -> Self {
        let mut current = self.clone();
        loop {
            if current.ptr_eq(root) {
                return current;
            }
            let at_mount_root = current.dentry.ptr_eq(&current.mount.root);
            let Some(parent) = current.topology_parent() else {
                return current;
            };
            if !at_mount_root {
                return parent;
            }
            current = parent;
        }
    }
}

impl FsContext {
    /// 验证两个真实目录，并沿名字与挂载的祖先关系确认 cwd 没有越过 root。
    pub fn new(root: Path, cwd: Path) -> Result<Self, FsError> {
        require_directory(&root)?;
        require_directory(&cwd)?;
        let mut ancestor = cwd.clone();
        loop {
            if ancestor.ptr_eq(&root) {
                return Ok(Self { root, cwd });
            }
            ancestor = ancestor
                .topology_parent()
                .ok_or(FsError::InvalidOperation)?;
        }
    }

    /// 借用本次查找的绝对路径与父目录边界。
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 借用相对路径的起始目录。
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }
}

impl Vfs {
    /// 锁外取得 inode 2 和根名字，预留包含根挂载在内的全部有限登记槽。
    pub fn try_new(
        volume: Arc<Ext4Volume>,
        inodes: Arc<InodeCache>,
        dentry_capacity: usize,
        mount_capacity: usize,
    ) -> Result<Self, FsError> {
        if mount_capacity == 0 {
            return Err(FsError::InvalidOperation);
        }
        let dcache = DentryCache::try_new(dentry_capacity)?;
        let mut mounts = Vec::new();
        mounts
            .try_reserve_exact(mount_capacity)
            .map_err(|_| FsError::NoMemory)?;
        let root = DentryRef::try_root(inodes.iget(&volume, 2)?)?;
        let mount = Arc::try_new(Mount {
            volume,
            root,
            covered: None,
        })
        .map_err(|_| FsError::NoMemory)?;
        mounts.push(mount);
        Ok(Self {
            dcache,
            inodes,
            mounts: SpinLock::new(mounts),
            mount_capacity,
        })
    }

    /// 在挂载表短锁中取得根父视图，返回的路径保留这个明确的挂载位置。
    pub fn root(&self) -> Path {
        let mounts = self.mounts.lock();
        let mount = &mounts[0];
        Path {
            mount: mount.clone(),
            dentry: mount.root.clone(),
        }
    }

    /// 借用当前 VFS 的有限名字缓存。
    pub fn dcache(&self) -> &DentryCache {
        &self.dcache
    }

    /// 借用所有卷共用的唯一 inode 缓存域。
    pub fn inodes(&self) -> &Arc<InodeCache> {
        &self.inodes
    }

    /// 内核从 cwd 无凭据行走，与显式 dirfd 查找共用同一逐组件算法。
    pub fn resolve(
        &self,
        context: &FsContext,
        pathname: &[u8],
        mode: ResolveMode,
    ) -> Result<Path, FsError> {
        self.resolve_from(context, context.cwd(), pathname, mode, None)
    }

    /// 绝对路径从 root、相对路径从显式起点行走，每个组件都重新检查目录搜索权限。
    pub fn resolve_from(
        &self,
        context: &FsContext,
        start: &Path,
        pathname: &[u8],
        mode: ResolveMode,
        credentials: Option<(u32, u32)>,
    ) -> Result<Path, FsError> {
        if pathname.is_empty() {
            return Err(FsError::NotFound);
        }
        if pathname.contains(&0) {
            return Err(FsError::InvalidName);
        }
        {
            let mounts = self.mounts.lock();
            if !contains_mount(&mounts, &context.root.mount)
                || (pathname[0] != b'/' && !contains_mount(&mounts, &start.mount))
            {
                return Err(FsError::InvalidOperation);
            }
        }
        let mut current = if pathname[0] == b'/' {
            context.root.clone()
        } else {
            start.clone()
        };
        let mut pending = Cow::Borrowed(pathname);
        let mut offset = 0;
        let mut links = 0;
        while offset < pending.len() {
            // 先验证当前目录，再处理点组件；file/.. 和 missing/.. 不做词法折叠。
            require_directory(&current)?;
            if let Some((uid, gid)) = credentials {
                current.check_access(uid, gid, 1)?;
            }
            while offset < pending.len() && pending[offset] == b'/' {
                offset += 1;
            }
            if offset == pending.len() {
                return Ok(current);
            }
            let end = pending[offset..]
                .iter()
                .position(|byte| *byte == b'/')
                .map_or(pending.len(), |length| offset + length);
            let name = &pending[offset..end];
            if name == b"." {
                offset = end;
                continue;
            }
            if name == b".." {
                current = self.follow_mounts(current.parent_within(&context.root));
                offset = end;
                continue;
            }
            let dentry = self.dcache.lookup(&self.inodes, &current.dentry, name)?;
            let next = self.follow_mounts(Path {
                mount: current.mount.clone(),
                dentry,
            });
            let inode = next.inode()?;
            let follow = inode.metadata()?.kind == InodeKind::Symlink
                && (mode == ResolveMode::Follow || end < pending.len());
            if follow {
                if links == 40 {
                    return Err(FsError::TooManySymlinks);
                }
                links += 1;
                let mut target = inode.read_link()?;
                let absolute = target.first() == Some(&b'/');
                // 保留原斜杠后缀，链接目标或原路径的尾斜杠仍要求真实目录。
                target
                    .try_reserve_exact(pending.len() - end)
                    .map_err(|_| FsError::NoMemory)?;
                target.extend_from_slice(&pending[end..]);
                pending = Cow::Owned(target);
                offset = 0;
                if absolute {
                    current = context.root.clone();
                }
            } else {
                current = next;
                offset = end;
            }
        }
        Ok(current)
    }

    /// 在父视图真实目录下挂入只读卷，准备与 inode I/O 在表锁外，发表时双查。
    pub fn mount(&self, covered: &Path, volume: Arc<Ext4Volume>) -> Result<(), FsError> {
        require_directory(covered)?;
        {
            let mounts = self.mounts.lock();
            self.check_mount(&mounts, covered, &volume)?;
        }
        let root = DentryRef::try_root(self.inodes.iget(&volume, 2)?)?;
        let candidate = Arc::try_new(Mount {
            volume,
            root,
            covered: Some(covered.clone()),
        })
        .map_err(|_| FsError::NoMemory)?;
        // guard 后构造、先析构，双查失败时候选和父视图都在解锁之后释放。
        let mut mounts = self.mounts.lock();
        self.check_mount(&mounts, covered, &candidate.volume)?;
        mounts.push(candidate);
        Ok(())
    }

    /// 以父视图覆盖点卸载；活路径或子挂载返回 Busy，成功摘除后的复杂释放在锁外。
    pub fn unmount(&self, covered: &Path) -> Result<(), FsError> {
        let victim = {
            let mut mounts = self.mounts.lock();
            if !contains_mount(&mounts, &covered.mount) {
                return Err(FsError::InvalidOperation);
            }
            let position = mounts
                .iter()
                .position(|mount| {
                    mount
                        .covered
                        .as_ref()
                        .is_some_and(|path| path.ptr_eq(covered))
                })
                .ok_or(FsError::NotFound)?;
            let mount = &mounts[position];
            let has_children = mounts.iter().any(|child| {
                child
                    .covered
                    .as_ref()
                    .is_some_and(|path| Arc::ptr_eq(&path.mount, mount))
            });
            // 目标只有登记表的一份基数引用；每个活 Path 和子挂载各增加一份。
            // 持锁排除新的向下取得；没有现存 Path 时 Clone 不能凭空制造活引用。
            if has_children || Arc::strong_count(mount) != 1 {
                return Err(FsError::Busy);
            }
            mounts.swap_remove(position)
        };
        let volume = victim.volume.clone();
        drop(victim);
        self.dcache.prune_unused();
        self.inodes.prune_unused();
        volume.prune_unused();
        Ok(())
    }

    /// 只回收无人使用的静止缓存；页与 I/O 自己的拥有者仍保活卷和只读声明。
    pub fn prune_unused(&self) -> usize {
        let mut removed = self.dcache.prune_unused() + self.inodes.prune_unused();
        let count = self.mounts.lock().len();
        for position in 0..count {
            let volume = {
                let mounts = self.mounts.lock();
                mounts.get(position).map(|mount| mount.volume.clone())
            };
            if let Some(volume) = volume {
                removed += volume.prune_unused();
            }
        }
        removed
    }

    /// 同一短表锁检查父挂载、覆盖点、实际 Disk 身份和固定预算。
    fn check_mount(
        &self,
        mounts: &[Arc<Mount>],
        covered: &Path,
        volume: &Arc<Ext4Volume>,
    ) -> Result<(), FsError> {
        if !contains_mount(mounts, &covered.mount) {
            return Err(FsError::InvalidOperation);
        }
        if find_mount(mounts, &covered.mount, &covered.dentry).is_some()
            || mounts.iter().any(|mount| {
                core::ptr::eq(
                    mount.volume.disk().as_ref().get_ref(),
                    volume.disk().as_ref().get_ref(),
                )
            })
        {
            return Err(FsError::Busy);
        }
        if self.mount_capacity <= mounts.len() {
            return Err(FsError::NoMemory);
        }
        Ok(())
    }

    /// 在同一表锁内找到最上层覆盖挂载并取得引用，旧 Path 在锁外释放。
    fn follow_mounts(&self, path: Path) -> Path {
        let crossed = {
            let mounts = self.mounts.lock();
            let mut mount = &path.mount;
            let mut dentry = &path.dentry;
            while let Some(child) = find_mount(&mounts, mount, dentry) {
                mount = child;
                dentry = &child.root;
            }
            if Arc::ptr_eq(mount, &path.mount) {
                None
            } else {
                Some(Path {
                    mount: mount.clone(),
                    dentry: dentry.clone(),
                })
            }
        };
        crossed.unwrap_or(path)
    }
}

/// 只接受实际正目录，缓存命中和点组件都不能绕过类型检查。
fn require_directory(path: &Path) -> Result<(), FsError> {
    if path.inode()?.metadata()?.kind == InodeKind::Directory {
        Ok(())
    } else {
        Err(FsError::NotDirectory)
    }
}

/// 短锁内按私有挂载实例检查路径属于当前视图，不能仅按后端卷号判断。
fn contains_mount(mounts: &[Arc<Mount>], mount: &Arc<Mount>) -> bool {
    mounts.iter().any(|entry| Arc::ptr_eq(entry, mount))
}

/// 借用表内登记找到一个覆盖关系，不在短锁中构造或释放路径。
fn find_mount<'a>(
    mounts: &'a [Arc<Mount>],
    parent: &Arc<Mount>,
    dentry: &DentryRef,
) -> Option<&'a Arc<Mount>> {
    mounts.iter().find(|mount| {
        mount.covered.as_ref().is_some_and(|covered| {
            Arc::ptr_eq(&covered.mount, parent) && covered.dentry.ptr_eq(dentry)
        })
    })
}
